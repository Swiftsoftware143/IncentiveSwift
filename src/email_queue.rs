//! Pending email queue — scheduled/delayed email sends (follow-ups, reminders).
//!
//! `schedule_email` inserts a row into `pending_emails`; a background ticker
//! (`process_due_emails` loop) flushes due rows via the tenant-aware SMTP sender.
//!
//! `status` vocabulary (kanban t_9d711589, extended by t_44a990da):
//!   `pending`  — queued and due to be flushed by the ticker. The ONLY status the ticker reads.
//!                `attempts = 0` means "never tried"; `attempts > 0` means a prior attempt FAILED
//!                and the row has been RE-ARMED by pushing `send_at` forward — `send_at` is then
//!                the next attempt time, not the original one. See the POLICY block below.
//!   `sent`     — delivered (`sent_at` set).
//!   `failed`   — the send raised and this row has used up `MAX_SEND_ATTEMPTS` attempts.
//!                **Terminal**: the ticker never reads a `failed` row, so this is a DEAD LETTER.
//!                Recorded with `attempts` + `last_error`, logged at ERROR, and listed on
//!                `GET /api/v1/admin/email-queue` so a human can see it.
//!   `retired`  — deliberately withdrawn (never sent, never will be); the reason lives in
//!                `last_error`. Used to clear probe/fixture residue out of the dead-letter set.
//!   `cancelled`— reserved; no writer in this tree.
//!
//! A failure used to be visible ONLY in `last_error` — no log line, no panel — and 18 dead rows sat
//! unnoticed for 12 days (measured 2026-10-02, kanban t_9d711589). Hence the ERROR line below and
//! the admin read route that lists the dead letters.
//!
//! A timed-out tenant dial is NOT a separate state (kanban t_05b6efa2): `sender::deliver_via`
//! bounds ONE dial at `sender::TENANT_SMTP_DEADLINE` (10 s), so a tenant mail server that accepts
//! the connection and stays silent now fails in 10 s with `last_error` naming the bound instead of
//! parking this loop. The loop is SEQUENTIAL (LIMIT 100), so one unreachable tenant server can
//! still delay the rows queued behind it by 10 s per row — bounded, not indefinite.
//!
//! # POLICY (kanban t_44a990da): a failed send is retried a BOUNDED number of times
//!
//! DECISION: **arm A — bounded retry.** Arm B ("keep it terminal") was rejected because the errors
//! this arm actually receives are *transient by construction*: the queue's own transport report is
//! at worst `SMTP send failed: <host:port> did not answer within 10s` / a connection reset /
//! `Mailgun returned 5xx` — a mail server restarting, a provider blip, a TCP timeout. SMTP itself
//! answers a temporary refusal with a **4xx** by definition (RFC 5321 §4.2.1: "transient negative
//! completion reply" — the verb is *try again later*), and nothing else in this app retries an
//! outbound mail: the ticker is the last hop. Making the FIRST error final therefore means silently
//! losing that recipient's email, which is the defect this card exists to close.
//!
//! SHAPE (chosen for the smallest honest change):
//!   * `MAX_SEND_ATTEMPTS = 3` **total** sends (1 initial + 2 retries) — the same budget the app's
//!     own webhook sender uses (`delivery/webhook.rs`, "retry (3 attempts, exponential backoff)").
//!   * the retry is DEFERRED, not a `sleep` inside this loop: the failure arm pushes `send_at`
//!     forward by `RETRY_BACKOFF` (5 min, then 30 min — a mail server that is restarting needs
//!     minutes, and this loop must not park, see t_05b6efa2). Because the flush predicate is
//!     `status = 'pending' AND send_at <= NOW()`, re-arming `send_at` *is* the backoff: no new
//!     column, no new status, no migration, and `idx_pending_emails_due` still covers the row.
//!   * the row is written `failed` — TERMINAL — only when the 3rd attempt also raises, so the
//!     dead-letter set stays honest and small.
//!
//! DEDUPE IS UNCHANGED. `lifecycle_emails::already_emailed` counts `pending_emails` rows for
//! (account, to_email, template_type) created in the last 24 h **regardless of status**. A retry
//! UPDATEs the SAME row: no row is inserted, `created_at` is not touched, so the count and this
//! function's answer are identical before and after a retry. (Proven: the retried row keeps its id
//! and `created_at` — see the card's audit.)
//!
//! NOT resurrected (and deliberately not a goal): rows already sitting `failed` from before this
//! change stay terminal, and `failed` is NOT added to the ticker's predicate. The live table holds
//! 0 such rows (measured 2026-10-02), so nothing is stranded by that choice.

use crate::delivery::sender;
use crate::state::AppState;
use serde_json::Value;
use sqlx::PgPool;
use std::time::Duration;
use uuid::Uuid;

/// How many send attempts a queued row may receive before it becomes a TERMINAL dead letter
/// (`status = 'failed'`). Three TOTAL sends — one initial plus two retries — the same budget the
/// app's own webhook sender uses (`delivery/webhook.rs`). See the module's POLICY block.
pub const MAX_SEND_ATTEMPTS: i32 = 3;

/// How long to wait before the NEXT attempt, indexed by the number of failures already recorded:
/// `RETRY_BACKOFF[0]` is the wait after the 1st failure, `RETRY_BACKOFF[1]` after the 2nd.
///
/// 5 min then 30 min, not seconds: the failure this exists for is a mail server being restarted or
/// a provider blip, and a retry that lands back in the same second is the same failure again. The
/// wait is applied by moving `send_at` forward, so the ticker (30 s) simply skips the row until
/// then — it never sleeps on it (see t_05b6efa2).
pub const RETRY_BACKOFF: [Duration; (MAX_SEND_ATTEMPTS - 1) as usize] =
    [Duration::from_secs(5 * 60), Duration::from_secs(30 * 60)];

/// What to do with a row whose `failures`-th send attempt just raised (kanban t_44a990da).
#[derive(Debug, PartialEq, Eq)]
pub enum Disposition {
    /// Not the last attempt: re-arm the row and try again after the backoff.
    Retry(Duration),
    /// `MAX_SEND_ATTEMPTS` used up: write `failed` and stop. The row is now a dead letter.
    DeadLetter,
}

/// THE retry rule — pure, so it can be unit-tested against the pre-change behaviour where the
/// first failure was final. `failures` is the count INCLUDING the attempt that just failed.
pub fn disposition_after_failure(failures: i32) -> Disposition {
    if failures < MAX_SEND_ATTEMPTS {
        // `failures` is >= 1 on every call; clamp so a malformed row cannot panic the ticker.
        let idx = (failures.max(1) as usize - 1).min(RETRY_BACKOFF.len() - 1);
        Disposition::Retry(RETRY_BACKOFF[idx])
    } else {
        Disposition::DeadLetter
    }
}

/// Queue an email to be sent at `send_at`.
pub async fn schedule_email(
    pool: &PgPool,
    account_id: Uuid,
    to_email: &str,
    template_type: &str,
    vars: &Value,
    send_at: chrono::DateTime<chrono::Utc>,
) -> Result<Uuid, String> {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO pending_emails (id, account_id, to_email, template_type, vars, send_at, status)
         VALUES ($1, $2, $3, $4, $5, $6, 'pending')",
    )
    .bind(id)
    .bind(account_id)
    .bind(to_email)
    .bind(template_type)
    .bind(vars)
    .bind(send_at)
    .execute(pool)
    .await
    .map_err(|e| format!("Failed to queue email: {e}"))?;
    Ok(id)
}

/// Flush all due pending emails. Called by the background ticker.
///
/// The predicate reads `status = 'pending'` ONLY, and is UNCHANGED by the retry policy: a row that
/// failed and was re-armed is still `pending` with a future `send_at`, so it comes back through
/// this same query when its backoff elapses. `attempts` rides along so the failure arm can decide
/// retry-vs-dead-letter without a second read.
pub async fn process_due_emails(state: &AppState) -> usize {
    let due: Vec<(Uuid, Uuid, String, String, Value, i32)> = sqlx::query_as(
        "SELECT id, account_id, to_email, template_type, vars, attempts FROM pending_emails
         WHERE status = 'pending' AND send_at <= NOW()
         ORDER BY send_at ASC
         LIMIT 100",
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_else(|e| {
        tracing::error!(error = %e, "pending_emails fetch failed — skipping this run");
        Default::default()
    });

    let mut sent = 0;
    for (id, account_id, to, template_type, vars, attempts) in due {
        let result =
            sender::send_template_by_type(&state.db, account_id, &to, &template_type, &vars).await;

        match result {
            Ok(_) => {
                let _ = sqlx::query(
                    "UPDATE pending_emails SET status = 'sent', sent_at = NOW() WHERE id = $1",
                )
                .bind(id)
                .execute(&state.db)
                .await;
                sent += 1;
            }
            Err(e) => {
                // VISIBILITY (kanban t_9d711589). Before that line the row's ONLY record of the
                // failure was `last_error` — nothing logged, nothing listed — which is how 18 dead
                // letters sat unnoticed from 2026-09-20. The recipient address is deliberately NOT
                // logged (this app's convention); the id joins to the row an operator reads on
                // GET /api/v1/admin/email-queue, which does show it.
                //
                // POLICY (kanban t_44a990da): a first failure is no longer final. The row is
                // RE-ARMED (same row, same `created_at` — so `already_emailed` is unaffected) by
                // pushing `send_at` past the backoff, and only the LAST attempt writes the terminal
                // `failed` dead letter.
                let failures = attempts + 1;
                match disposition_after_failure(failures) {
                    Disposition::Retry(backoff) => {
                        let next_attempt_at = chrono::Utc::now()
                            + chrono::Duration::seconds(backoff.as_secs() as i64);
                        tracing::warn!(
                            email_id = %id,
                            account_id = %account_id,
                            template_type = %template_type,
                            attempt = failures,
                            of = MAX_SEND_ATTEMPTS,
                            retry_in_secs = backoff.as_secs(),
                            "queued email could not be sent — re-armed for a bounded retry: {e}"
                        );
                        let _ = sqlx::query(
                            "UPDATE pending_emails SET attempts = $2, last_error = $3, send_at = $4 \
                             WHERE id = $1",
                        )
                        .bind(id)
                        .bind(failures)
                        .bind(&e)
                        .bind(next_attempt_at)
                        .execute(&state.db)
                        .await;
                    }
                    Disposition::DeadLetter => {
                        tracing::error!(
                            email_id = %id,
                            account_id = %account_id,
                            template_type = %template_type,
                            attempts = failures,
                            "queued email could not be sent — DEAD LETTER (status='failed' after \
                             {failures} attempts, the ticker does not retry a failed row); see \
                             GET /api/v1/admin/email-queue: {e}"
                        );
                        let _ = sqlx::query(
                            "UPDATE pending_emails SET status = 'failed', attempts = $2, \
                             last_error = $3 WHERE id = $1",
                        )
                        .bind(id)
                        .bind(failures)
                        .bind(&e)
                        .execute(&state.db)
                        .await;
                    }
                }
            }
        }
    }
    sent
}

/// Background ticker — flush due emails every 30s.
pub fn spawn_email_ticker(state: AppState) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        // first tick immediately
        interval.tick().await;
        loop {
            interval.tick().await;
            let sent = process_due_emails(&state).await;
            if sent > 0 {
                tracing::info!("Email ticker sent {sent} queued email(s)");
            }
        }
    });
}

#[cfg(test)]
mod retry_policy_tests {
    use super::*;

    /// THE defect this policy exists to close (kanban t_44a990da): before it, the failure arm
    /// wrote `failed` on the FIRST error and nothing ever read `failed` again, so one transient
    /// SMTP refusal lost the recipient's email for ever. A first failure must NOT be terminal.
    /// This test fails on the pre-change rule (where disposition was unconditionally DeadLetter).
    #[test]
    fn a_first_failure_is_retried_not_terminal() {
        match disposition_after_failure(1) {
            Disposition::Retry(backoff) => assert_eq!(
                backoff,
                Duration::from_secs(300),
                "the first retry is 5 min out"
            ),
            Disposition::DeadLetter => panic!(
                "a first send failure must be re-armed, not written off — that is the silent-loss \
                 defect (t_44a990da)"
            ),
        }
    }

    /// The retry is BOUNDED: the wait grows, and the last allowed attempt is terminal — otherwise
    /// a permanently bad row (no such template, refused host) would be re-tried for ever.
    #[test]
    fn retries_are_bounded_and_the_last_attempt_is_terminal() {
        assert_eq!(
            disposition_after_failure(2),
            Disposition::Retry(Duration::from_secs(1800)),
            "the second retry is 30 min out"
        );
        assert_eq!(
            disposition_after_failure(MAX_SEND_ATTEMPTS),
            Disposition::DeadLetter,
            "attempt {MAX_SEND_ATTEMPTS} is the LAST one — it must be terminal"
        );
        assert_eq!(
            disposition_after_failure(0),
            Disposition::Retry(Duration::from_secs(300))
        );
    }

    /// The vocabulary the console and `already_emailed` rely on: `MAX_SEND_ATTEMPTS` is exactly
    /// `RETRY_BACKOFF.len() + 1`, i.e. one initial send plus one backoff slot per retry.
    #[test]
    fn the_backoff_table_covers_every_retry() {
        assert_eq!(RETRY_BACKOFF.len() as i32, MAX_SEND_ATTEMPTS - 1);
        assert!(
            MAX_SEND_ATTEMPTS >= 2,
            "a one-attempt budget is the old defect"
        );
    }
}
