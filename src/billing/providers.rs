//! Payment Provider CRUD handlers.
//!
//! Endpoints:
//!   GET    /api/v1/payment-providers              — list providers
//!   POST   /api/v1/payment-providers              — upsert provider config
//!   DELETE /api/v1/payment-providers/:provider_type — delete a provider

use crate::error::AppError;
use crate::security::auth::AuthenticatedUser;
use crate::state::AppState;
use axum::{
    extract::{Path, State},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Mask a sensitive key/value showing only first 3 and last 3 characters.
fn mask_key(key: &str) -> String {
    if key.len() <= 6 {
        return "***".to_string();
    }
    let first = &key[..3];
    let last = &key[key.len() - 3..];
    format!("{}...{}", first, last)
}

// ---------------------------------------------------------------------------
// Input types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct UpsertPaymentProviderInput {
    pub provider_type: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub webhook_secret: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub metadata: Option<Value>,
    #[serde(default)]
    pub is_active: Option<bool>,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// GET /api/v1/payment-providers
pub async fn list_payment_providers(
    State(state): State<AppState>,
    auth: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&auth.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;

    let rows = sqlx::query(
        r#"
        SELECT pp.id, pp.account_id, pp.provider_type, pp.api_key, pp.webhook_secret,
               pp.base_url, pp.metadata, pp.is_active, pp.created_at, pp.updated_at
        FROM payment_providers pp
        WHERE pp.account_id = $1
        ORDER BY pp.provider_type
        "#,
    )
    .bind(account_id)
    .fetch_all(&state.db)
    .await?;

    let items: Vec<Value> = rows
        .iter()
        .map(|row| {
            let raw_key: String = row.get("api_key");
            let raw_webhook: String = row.get("webhook_secret");
            json!({
                "id": row.get::<Uuid, _>("id"),
                "account_id": row.get::<Uuid, _>("account_id"),
                "provider_type": row.get::<String, _>("provider_type"),
                "api_key_masked": mask_key(&raw_key),
                "webhook_secret_masked": mask_key(&raw_webhook),
                "base_url": row.get::<Option<String>, _>("base_url"),
                "metadata": row.get::<Option<serde_json::Value>, _>("metadata"),
                "is_active": row.get::<bool, _>("is_active"),
                "created_at": row.get::<chrono::DateTime<chrono::Utc>, _>("created_at"),
                "updated_at": row.get::<chrono::DateTime<chrono::Utc>, _>("updated_at"),
            })
        })
        .collect();

    Ok(Json(json!({ "items": items, "count": items.len() })))
}

/// POST /api/v1/payment-providers
pub async fn upsert_payment_provider(
    State(state): State<AppState>,
    auth: AuthenticatedUser,
    Json(body): Json<UpsertPaymentProviderInput>,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&auth.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;

    // Upsert using ON CONFLICT pattern
    let row = sqlx::query(
        r#"
        INSERT INTO payment_providers (account_id, provider_type, api_key, webhook_secret, base_url, metadata, is_active)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        ON CONFLICT (account_id, provider_type)
        DO UPDATE SET
            api_key = CASE
                WHEN EXCLUDED.api_key IS NOT NULL AND EXCLUDED.api_key != ''
                THEN EXCLUDED.api_key
                ELSE payment_providers.api_key
            END,
            webhook_secret = CASE
                WHEN EXCLUDED.webhook_secret IS NOT NULL AND EXCLUDED.webhook_secret != ''
                THEN EXCLUDED.webhook_secret
                ELSE payment_providers.webhook_secret
            END,
            base_url = COALESCE(EXCLUDED.base_url, payment_providers.base_url),
            metadata = CASE
                WHEN EXCLUDED.metadata IS NOT NULL AND EXCLUDED.metadata != '{}'::jsonb
                THEN EXCLUDED.metadata
                ELSE payment_providers.metadata
            END,
            is_active = EXCLUDED.is_active,
            updated_at = now()
        RETURNING id, account_id, provider_type, api_key, webhook_secret,
                  base_url, metadata, is_active, created_at, updated_at
        "#
    )
    .bind(account_id)
    .bind(&body.provider_type)
    .bind(body.api_key.as_deref().unwrap_or(""))
    .bind(body.webhook_secret.as_deref().unwrap_or(""))
    .bind(&body.base_url)
    .bind(&body.metadata)
    .bind(body.is_active.unwrap_or(true))
    .fetch_one(&state.db)
    .await?;

    let raw_key: String = row.get("api_key");
    let raw_webhook: String = row.get("webhook_secret");
    let item = json!({
        "id": row.get::<Uuid, _>("id"),
        "account_id": row.get::<Uuid, _>("account_id"),
        "provider_type": row.get::<String, _>("provider_type"),
        "api_key_masked": mask_key(&raw_key),
        "webhook_secret_masked": mask_key(&raw_webhook),
        "base_url": row.get::<Option<String>, _>("base_url"),
        "metadata": row.get::<Option<serde_json::Value>, _>("metadata"),
        "is_active": row.get::<bool, _>("is_active"),
        "created_at": row.get::<chrono::DateTime<chrono::Utc>, _>("created_at"),
        "updated_at": row.get::<chrono::DateTime<chrono::Utc>, _>("updated_at"),
    });

    Ok(Json(json!({ "item": item })))
}

/// DELETE /api/v1/payment-providers/:provider_type
pub async fn delete_payment_provider(
    State(state): State<AppState>,
    auth: AuthenticatedUser,
    Path(provider_type): Path<String>,
) -> Result<Json<Value>, AppError> {
    // Super admin gate: only super admins can delete payment providers
    if auth.role != "super_admin" {
        return Err(AppError::Forbidden(
            "Only super admins can delete payment providers".to_string(),
        ));
    }

    let result = sqlx::query("DELETE FROM payment_providers WHERE provider_type = $1")
        .bind(&provider_type)
        .execute(&state.db)
        .await?;

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound(format!(
            "Payment provider not found for type '{}'",
            provider_type
        )));
    }

    Ok(Json(
        json!({ "status": "deleted", "provider_type": provider_type }),
    ))
}

/// Look up a payment provider's webhook secret from the database by provider type.
/// Returns the first active provider's webhook_secret.
/// Re-exported from `billing` so webhooks can share this lookup.
pub async fn lookup_webhook_secret(
    state: &AppState,
    provider_type: &str,
) -> Result<String, AppError> {
    let row = sqlx::query(
        "SELECT pp.webhook_secret FROM payment_providers pp WHERE pp.provider_type = $1 AND pp.is_active = true LIMIT 1"
    )
    .bind(provider_type)
    .fetch_optional(&state.db)
    .await?;

    match row {
        Some(r) => {
            let secret: String = r.get("webhook_secret");
            if secret.is_empty() {
                Err(AppError::Internal(format!(
                    "{} webhook secret is configured but empty",
                    provider_type
                )))
            } else {
                Ok(secret)
            }
        }
        None => Err(AppError::Internal(format!(
            "No active {} payment provider configured",
            provider_type
        ))),
    }
}
