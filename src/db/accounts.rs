//! Accounts database operations.

use crate::error::AppError;
use sqlx::{PgPool, Row};
use uuid::Uuid;

/// An account record.
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct Account {
    pub id: uuid::Uuid,
    pub name: Option<String>,
    pub email: String,
    pub plan_tier_id: Option<uuid::Uuid>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Get a single account by ID.
pub async fn get_account(
    pool: &PgPool,
    account_id: &Uuid,
) -> Result<Account, AppError> {
    let row = sqlx::query(
        r#"SELECT id, name, email, plan_tier_id, created_at
           FROM accounts WHERE id = $1"#
    )
    .bind(account_id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::NotFound("Account not found".to_string()))?;

    Ok(Account {
        id: row.get("id"),
        name: row.get("name"),
        email: row.get("email"),
        plan_tier_id: row.get("plan_tier_id"),
        created_at: row.get("created_at"),
    })
}

/// Get account by email.
pub async fn get_account_by_email(
    pool: &PgPool,
    email: &str,
) -> Result<Option<Account>, AppError> {
    let row = sqlx::query(
        r#"SELECT id, name, email, plan_tier_id, created_at
           FROM accounts WHERE email = $1"#
    )
    .bind(email)
    .fetch_optional(pool)
    .await?;

    match row {
        Some(r) => Ok(Some(Account {
            id: r.get("id"),
            name: r.get("name"),
            email: r.get("email"),
            plan_tier_id: r.get("plan_tier_id"),
            created_at: r.get("created_at"),
        })),
        None => Ok(None),
    }
}
