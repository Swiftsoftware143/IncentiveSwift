//! Pending email queue — scheduled/delayed email sends (follow-ups, reminders).
//!
//! `schedule_email` inserts a row into `pending_emails`; a background ticker
//! (`process_due_emails` loop) flushes due rows via the tenant-aware SMTP sender.

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
    .unwrap_or_default();

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
