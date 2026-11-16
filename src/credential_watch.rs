//! `credential_watch` — tell the OPERATOR when a customer's first password never reached them.
//!
//! WHY THIS EXISTS (measured on the sibling app ADASwift 2026-11-13..15): a real customer signed
//! up, the mail carrying their first password never left the box, and the account sat unusable for
//! two days. Every piece of the state already existed here — the signup door latches
//! `accounts.password_undelivered` when the credentials mail does not go
//! (`handlers::auth_handler::register`), the admin console badges that row, and
//! `POST /api/v1/admin/accounts/:id/reset-password` is the door that sets a first password — but a
//! badge is only read by somebody who happens to open that screen, and NOBODY WAS EVER TOLD.
//! IncentiveSwift mailed the operator for a new workspace, a plan request and a support request and
//! had no arm at all for the one event that leaves a customer unable to use the product.
//!
//! Two rules decide the design, both learned the hard way on MissedCall Respondr (commit 273e092)
//! and FunnelSwift:
//!
//! * **"Already reported" means the operator was actually MAILED**, not that a row exists. A send
//!   the transport withholds (a reserved test address, an unconfigured provider) writes a row too,
//!   and treating that as a report retires the account for ever. So the dedupe names the operator
//!   address AND requires status `sent`, and a pass whose own row comes back `withheld` leaves the
//!   account unreported so the next pass retries.
//! * **Machinery must never ring a human.** A harness signup always lands with the latch set — its
//!   credentials mail is suppressed by construction — so without a gate this watch would mail the
//!   operator once per probe signup. The gate is on the REQUESTER (the customer identity and the
//!   workspace name), never on a route: the same door serves real customers.
//!
//! This app differs from the siblings in three ways, all visible below: the table is `accounts`
//! (there is no separate `users`), there is no raw send (only `email::send_template_email`, so the
//! wording comes from the `undelivered_credential_operator_alert` template row seeded by
//! `migrations/zz_is32_undelivered_credential_alert_template.sql`), and `email_send_log` carries
//! only `to_email/subject/status/detail`, so the account's own id is what the subject carries and
//! the dedupe reads it back from there.

use sqlx::PgPool;
use std::time::Duration;
use uuid::Uuid;

/// How often the watch looks. Fifteen minutes bounds how long a locked-out customer waits for a
/// human to be told, and costs one cheap indexed read when there is nothing to report.
pub const WATCH_INTERVAL_SECS: u64 = 15 * 60;

/// Grace period. A signup whose mail is still in flight, or a mail layer that is momentarily slow,
/// must not page the operator for a customer who is about to receive their password anyway.
pub const MIN_AGE_MINUTES: i64 = 30;

/// How far back the watch looks. A latch set longer ago than this belongs to an account the
/// operator has already dealt with by other means; two weeks is the window in which the arrival is
/// still news.
pub const LOOKBACK_DAYS: i64 = 14;

/// Accounts per pass, so one pass never sends an unbounded burst of mail.
pub const BATCH: i64 = 5;

/// Boot-time worker, started from `main` next to the probe sweeper and the plan seater. A failure
/// inside one pass is logged and the loop continues — an alerter must never take the app down.
pub fn spawn_credential_watch(pool: PgPool) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(WATCH_INTERVAL_SECS));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            // The first tick fires immediately, so a deploy reports the existing backlog instead of
            // waiting a quarter of an hour.
            ticker.tick().await;
            match alert_once(&pool).await {
                Ok(0) => {}
                Ok(n) => tracing::info!(reported = n, "undelivered-credential alert"),
                Err(e) => tracing::warn!(error = %e, "undelivered-credential watch failed"),
            }
        }
    });
}

/// The operator address this install is told at (`admin_settings.email` key
/// `operator_alert_email`, set from the console's Email Provider card). `None` means nothing is
/// configured, which is a normal state — this code never invents a mailbox.
async fn operator_alert_email(pool: &PgPool) -> Option<String> {
    let row: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT value FROM admin_settings WHERE key = 'email'")
            .fetch_optional(pool)
            .await
            .ok()?;
    row.as_ref()
        .and_then(|v| v.get("operator_alert_email"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Report at most [`BATCH`] unreported accounts whose first password never reached them.
///
/// Returns how many alerts actually reached the operator. A withheld send does not count, so the
/// number is "a human was told", not "a function ran".
pub async fn alert_once(pool: &PgPool) -> Result<usize, String> {
    // WHO the alert goes to is resolved BEFORE the selector, because "already reported" has to mean
    // "delivered to THIS operator".
    let Some(to) = operator_alert_email(pool).await else {
        return Ok(0);
    };

    // The SELECT names the two identities the alert prints (kanban t_0f8bf1c0). It used to put the
    // owner's NAME in the `workspace` column and the OPTIONAL `username` in the `owner_name` column,
    // so the operator alert read "Workspace: <person>" and — because `username` is usually empty —
    // its subject fell back to the bare address, which is the defect: the notification list could
    // not say WHO was locked out. `accounts.name` is what the signup's "your name" field stores
    // (auth_handler::register), so that is the OWNER name; the workspace comes from the account's
    // own default workspace row, and only falls back to the company/account name when the tenant
    // has no workspace row at all.
    let rows: Vec<(Uuid, String, String, String, String)> = sqlx::query_as(
        r#"
        SELECT a.id,
               a.email,
               COALESCE(NULLIF(btrim(w.name), ''), NULLIF(btrim(a.company), ''), NULLIF(btrim(a.name), ''), a.email),
               COALESCE(NULLIF(btrim(a.name), ''), a.email),
               a.created_at::date::text
          FROM accounts a
          LEFT JOIN LATERAL (
                SELECT name FROM workspaces
                 WHERE tenant_id = a.tenant_id
                 ORDER BY is_default DESC, created_at ASC
                 LIMIT 1
               ) w ON true
         WHERE a.password_undelivered
           AND a.closed_at IS NULL
           AND a.created_at < now() - make_interval(mins => $1::int)
           AND a.created_at > now() - make_interval(days => $2::int)
           -- already reported: a row that names this account AND actually reached the operator we
           -- are about to mail. A withheld row (reserved recipient, unconfigured provider) is not a
           -- report, and neither is a row addressed to anyone else.
           AND NOT EXISTS (
                 SELECT 1 FROM email_send_log l
                  WHERE l.subject LIKE '%' || a.id::text || '%'
                    AND l.to_email = $4
                    AND l.status = 'sent')
         ORDER BY a.created_at ASC
         LIMIT $3
        "#,
    )
    .bind(MIN_AGE_MINUTES)
    .bind(LOOKBACK_DAYS)
    .bind(BATCH)
    .bind(&to)
    .fetch_all(pool)
    .await
    .map_err(|e| format!("select undelivered credentials: {e}"))?;

    if rows.is_empty() {
        return Ok(0);
    }

    let mut reported = 0usize;
    for (account_id, customer_email, workspace, owner_name, signed_up) in rows {
        // A probe signup always carries the latch (its mail cannot be delivered by construction),
        // so this gate is what stops the watch mailing a human once per harness run. Requester, not
        // route: the same door serves real customers. Both the customer identity and the workspace
        // name are judged, because a harness workspace can carry a clean-looking address.
        let synthetic =
            crate::security::probe_addr::synthetic_requester(&customer_email, &owner_name)
                .or_else(|| crate::security::probe_addr::synthetic_requester("", &workspace));
        if let Some(reason) = synthetic {
            tracing::info!(
                account = %account_id,
                reason = %reason,
                "undelivered-credential alert WITHHELD — the account is a synthetic identity"
            );
            continue;
        }

        match crate::email::send_undelivered_credential_operator_alert(
            pool,
            account_id,
            &customer_email,
            &workspace,
            &owner_name,
            &signed_up,
            &to,
        )
        .await
        {
            Ok(status) if status == "withheld" => {
                // The send funnel answers Ok for a recipient the transport withholds. The account
                // stays UNREPORTED so a later pass retries once the operator address is a real
                // mailbox, instead of being silently retired for ever.
                tracing::warn!(
                    account = %account_id,
                    to = %to,
                    "undelivered-credential alert WITHHELD — the operator address cannot receive \
                     mail, so this account stays unreported and the next pass retries"
                );
            }
            Ok(status) if status == "sent" => {
                tracing::info!(
                    account = %account_id,
                    "reported an undelivered credential to the operator"
                );
                reported += 1;
            }
            Ok(status) => {
                tracing::warn!(
                    account = %account_id,
                    status = %status,
                    "undelivered-credential alert did not reach the operator; the next pass retries"
                );
            }
            Err(e) => {
                tracing::warn!(account = %account_id, error = %e, "undelivered-credential alert failed");
            }
        }
    }
    Ok(reported)
}
