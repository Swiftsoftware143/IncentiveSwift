//! Background purge task for email data retention.
//!
//! RETENTION IS A FAR BACKSTOP, NOT THE PRODUCT RULE (B206, David 2026-11-11 revised). The product
//! rule is the STORAGE QUOTA (`storage.rs`): mail is kept while the account has room, the owner is
//! warned at 50/75/99% and mail is NEVER deleted to make room. What survives here is the database's
//! own bound — a window that defaults to TEN YEARS (migration 142 raised it from 365 days) and is
//! stated on the inbox screen with the account's real number. An operator can still set a shorter
//! window deliberately through the admin retention control (30..3650 days).
//!
//! Deletes, per tenant, rows older than that window:
//!   * `private_email_messages` — stored mail (this is the backstop that must be stated on screen);
//!   * `outbound_messages`      — the delivery log;
//!   * email `events`           — the thin auditing stream.

use sqlx::PgPool;
use uuid::Uuid;

use super::models::PurgeSummaryResponse;

/// The window used when an account has no explicit one: ten years, the backstop.
pub const DEFAULT_RETENTION_DAYS: i32 = 3650;

/// Purge email data for all tenants (or a single tenant) based on their
/// retention policy. Each tenant's `retention_days` in `tenant_email_limits`
/// determines the cutoff; falls back to 3650 days (the backstop) if no override exists.
pub async fn purge_expired_emails(
    pool: &PgPool,
    target_tenant: Option<Uuid>,
) -> PurgeSummaryResponse {
    let mut summary = PurgeSummaryResponse::new();

    // ── 1. Determine which tenants to process ──
    let tenants: Vec<Uuid> = match get_purge_targets(pool, target_tenant).await {
        Ok(t) => {
            summary.tenants_checked = t.len() as i64;
            t
        }
        Err(e) => {
            summary
                .errors
                .push(format!("Failed to list tenants: {}", e));
            return summary;
        }
    };

    if tenants.is_empty() {
        return summary;
    }

    // ── 2. For each tenant, run the purge ──
    for tenant_id in &tenants {
        match purge_single_tenant(pool, *tenant_id).await {
            Ok((msgs, inbox_msgs, evts)) => {
                if msgs > 0 || inbox_msgs > 0 || evts > 0 {
                    summary.tenants_purged += 1;
                    summary.messages_deleted += msgs;
                    summary.inbox_messages_deleted += inbox_msgs;
                    summary.events_deleted += evts;
                    tracing::info!(
                        tenant = %tenant_id,
                        messages = msgs,
                        inbox_messages = inbox_msgs,
                        events = evts,
                        "Purged data past the retention backstop"
                    );

                    // Update last_purged_at
                    let _ = sqlx::query(
                        r#"
                        UPDATE tenant_email_limits
                        SET last_purged_at = NOW()
                        WHERE tenant_id = $1 AND EXISTS (
                            SELECT 1 FROM tenant_email_limits WHERE tenant_id = $1
                        )
                        "#,
                    )
                    .bind(tenant_id)
                    .execute(pool)
                    .await;
                }
            }
            Err(e) => {
                let msg = format!("Tenant {} purge error: {}", tenant_id, e);
                tracing::warn!("{}", msg);
                summary.errors.push(msg);
            }
        }
    }

    summary
}

/// Returns the list of tenant UUIDs to purge.
/// If `target_tenant` is provided, returns just that one.
async fn get_purge_targets(
    pool: &PgPool,
    target_tenant: Option<Uuid>,
) -> Result<Vec<Uuid>, sqlx::Error> {
    if let Some(tid) = target_tenant {
        return Ok(vec![tid]);
    }

    // Every account that holds stored mail OR a delivery log. The stored-mail arm matters: an
    // account whose mail predates the backstop must be swept even if it never queued an outbound
    // message (before this, it was not a purge target at all).
    // A row with a NULL tenant_id has no tenant whose retention policy could own it, so it is not a
    // purge target at all: the predicate says that instead of letting the decode 500 the sweep
    // (t_d6eeea96). 0 rows carry a NULL today, so the result set is unchanged.
    let rows: Vec<(Uuid,)> = sqlx::query_as(
        "SELECT DISTINCT tenant_id FROM outbound_messages WHERE tenant_id IS NOT NULL
         UNION
         SELECT DISTINCT tenant_id FROM private_email_messages WHERE tenant_id IS NOT NULL",
    )
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().map(|(id,)| id).collect())
}

/// Purge data past the retention BACKSTOP for a single tenant.
/// Returns (outbound_messages_deleted, inbox_messages_deleted, events_deleted).
async fn purge_single_tenant(
    pool: &PgPool,
    tenant_id: Uuid,
) -> Result<(i64, i64, i64), sqlx::Error> {
    // Determine retention window for this tenant
    let retention_days = get_retention_days(pool, tenant_id).await;

    // Delete outbound_messages past the window (the delivery log)
    let msg_result = sqlx::query(
        r#"
        DELETE FROM outbound_messages
        WHERE tenant_id = $1
          AND created_at < NOW() - ($2 || ' days')::INTERVAL
        "#,
    )
    .bind(tenant_id)
    .bind(retention_days.to_string())
    .execute(pool)
    .await?;
    let messages_deleted = msg_result.rows_affected() as i64;

    // Delete STORED MAIL past the window. This is the backstop the inbox screen states, and it is
    // the only path in the app that removes a customer's mail. Ten years by default, so it removes
    // nothing in normal operation; it never runs to make room for new mail (the storage quota is
    // what handles a full mailbox, and it refuses new attachments instead of deleting anything).
    let inbox_result = sqlx::query(
        r#"
        DELETE FROM private_email_messages
        WHERE tenant_id = $1
          AND received_at < NOW() - ($2 || ' days')::INTERVAL
        "#,
    )
    .bind(tenant_id)
    .bind(retention_days.to_string())
    .execute(pool)
    .await?;
    let inbox_messages_deleted = inbox_result.rows_affected() as i64;

    // Delete expired email-related events (source = 'private_email')
    let evt_result = sqlx::query(
        r#"
        DELETE FROM events
        WHERE tenant_id = $1
          AND source = 'private_email'
          AND created_at < NOW() - ($2 || ' days')::INTERVAL
        "#,
    )
    .bind(tenant_id)
    .bind(retention_days.to_string())
    .execute(pool)
    .await?;
    let events_deleted = evt_result.rows_affected() as i64;

    Ok((messages_deleted, inbox_messages_deleted, events_deleted))
}

/// Look up the retention window for a tenant.
/// Falls back to the BACKSTOP (3650 days) when no override is set — not 365: the clock is no longer
/// the product rule, and an account that never set a window must not start losing mail at a year.
async fn get_retention_days(pool: &PgPool, tenant_id: Uuid) -> i32 {
    let row: Option<(Option<i32>,)> =
        sqlx::query_as("SELECT retention_days FROM tenant_email_limits WHERE tenant_id = $1")
            .bind(tenant_id)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();

    row.and_then(|(days,)| days)
        .unwrap_or(DEFAULT_RETENTION_DAYS)
}
