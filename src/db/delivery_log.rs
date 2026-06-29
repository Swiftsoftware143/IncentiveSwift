//! Delivery log database operations — audit trail for webhook/API pushes.

use crate::error::AppError;
use sqlx::PgPool;
use uuid::Uuid;

/// A delivery log entry.
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct DeliveryLogEntry {
    pub id: uuid::Uuid,
    pub entry_id: uuid::Uuid,
    pub method: String,
    pub target: String,
    pub success: bool,
    pub response_code: Option<i32>,
    pub response_body: Option<String>,
    pub attempted_at: chrono::DateTime<chrono::Utc>,
}

/// Log a delivery attempt.
pub async fn log_delivery(
    pool: &PgPool,
    entry_id: &Uuid,
    method: &str,
    target: &str,
    success: bool,
    response_code: Option<i32>,
    response_body: Option<String>,
) -> Result<(), AppError> {
    let id = Uuid::new_v4();
    sqlx::query(
        r#"INSERT INTO delivery_log (id, entry_id, method, target, success, response_code, response_body)
           VALUES ($1, $2, $3, $4, $5, $6, $7)"#
    )
    .bind(id)
    .bind(entry_id)
    .bind(method)
    .bind(target)
    .bind(success)
    .bind(response_code)
    .bind(&response_body)
    .execute(pool)
    .await?;

    Ok(())
}

/// Get delivery log entries for an entry.
pub async fn get_delivery_log(
    pool: &PgPool,
    entry_id: &Uuid,
) -> Result<Vec<DeliveryLogEntry>, AppError> {
    let log = sqlx::query_as::<_, DeliveryLogEntry>(
        r#"SELECT id, entry_id, method, target, success, response_code, response_body, attempted_at
           FROM delivery_log WHERE entry_id = $1
           ORDER BY attempted_at DESC"#,
    )
    .bind(entry_id)
    .fetch_all(pool)
    .await?;

    Ok(log)
}
