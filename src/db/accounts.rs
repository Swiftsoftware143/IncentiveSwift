//! Accounts database operations.

use crate::error::AppError;
use sqlx::PgPool;
use uuid::Uuid;

/// An account record.
#[derive(Debug, Clone, serde::Serialize)]
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
    let account = sqlx::query_as!(
        Account,
        r#"SELECT id, name, email, plan_tier_id, created_at
           FROM accounts WHERE id = $1"#,
        account_id
    )
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::NotFound("Account not found".to_string()))?;

    Ok(account)
}

/// Get account by email.
pub async fn get_account_by_email(
    pool: &PgPool,
    email: &str,
) -> Result<Option<Account>, AppError> {
    let account = sqlx::query_as!(
        Account,
        r#"SELECT id, name, email, plan_tier_id, created_at
           FROM accounts WHERE email = $1"#,
        email
    )
    .fetch_optional(pool)
    .await?;

    Ok(account)
}
