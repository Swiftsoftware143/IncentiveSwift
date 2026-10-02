//! Pending email queue — scheduled/delayed email sends (follow-ups, reminders).
//!
//! `schedule_email` inserts a row into `pending_emails`; a background ticker
//! (`process_due_emails` loop) flushes due rows via the tenant-aware SMTP sender.
//!
//! `status` vocabulary (kanban t_9d711589):
//!   `pending`  — queued and due to be flushed by the ticker. The ONLY status the ticker reads.
//!   `sent`     — delivered (`sent_at` set).
//!   `failed`   — the send raised; recorded with `attempts` + `last_error`. **Terminal**: the
//!                ticker never re-reads a failed row, so this is a DEAD LETTER. Logged at ERROR and
//!                listed on `GET /api/v1/admin/email-queue` so a human can see it.
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
//! still delay the rows queued behind it by 10 s per row — bounded, not indefinite. The row is
//! written `failed` exactly like any other send error, i.e. TERMINAL today; whether a `failed` row
//! should be retried is the policy card t_44a990da and is deliberately not decided here.

use crate::delivery::sender;
use crate::state::AppState;
use serde_json::Value;
use sqlx::PgPool;
use std::time::Duration;
use uuid::Uuid;

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
pub async fn process_due_emails(state: &AppState) -> usize {
    let due: Vec<(Uuid, Uuid, String, String, Value)> = sqlx::query_as(
        "SELECT id, account_id, to_email, template_type, vars FROM pending_emails
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
    for (id, account_id, to, template_type, vars) in due {
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
                // VISIBILITY (kanban t_9d711589). Before this line the row's ONLY record of the
                // failure was `last_error` — nothing logged, nothing listed — which is how 18 dead
                // letters sat unnoticed from 2026-09-20. The recipient address is deliberately NOT
                // logged (this app's convention); the id joins to the row an operator reads on
                // GET /api/v1/admin/email-queue, which does show it.
                tracing::error!(
                    email_id = %id,
                    account_id = %account_id,
                    template_type = %template_type,
                    "queued email could not be sent — DEAD LETTER (status='failed', the ticker never \
                     retries a failed row); see GET /api/v1/admin/email-queue: {e}"
                );
                let _ = sqlx::query(
                    "UPDATE pending_emails SET status = 'failed', attempts = attempts + 1, last_error = $2 WHERE id = $1",
                )
                .bind(id)
                .bind(&e)
                .execute(&state.db)
                .await;
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
