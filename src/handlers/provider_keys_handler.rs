//! Provider Keys handler - manage per-account third-party API keys.
//!
//! Endpoints:
//!   GET  /api/v1/provider-keys          - list keys for current account
//!   POST /api/v1/provider-keys          - upsert a key
//!   DELETE /api/v1/provider-keys/:provider - delete a key by provider name
//!   GET  /api/v1/available-providers    - list all available provider types

use crate::error::AppError;
use crate::state::AppState;
use crate::security::auth::AuthenticatedUser;
use axum::{
    extract::{Path, State},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

/// Mask a key showing only first 3 and last 3 characters.
fn mask_key(key: &str) -> String {
    if key.len() <= 6 {
        return "***".to_string();
    }
    let first = &key[..3];
    let last = &key[key.len()-3..];
    format!("{}...{}", first, last)
}

/// GET /api/v1/provider-keys
pub async fn list_provider_keys(
    State(state): State<AppState>,
    auth: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&auth.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;

    let rows = sqlx::query(
        r#"
        SELECT pk.id, pk.account_id, pk.provider, pk.api_key, pk.base_url,
               pk.metadata, pk.is_active, pk.scope, pk.created_at, pk.updated_at,
               ap.name AS provider_name, ap.description AS provider_description
        FROM provider_keys pk
        LEFT JOIN available_providers ap ON ap.key = pk.provider
        WHERE pk.account_id = $1
        ORDER BY pk.provider
        "#
    )
    .bind(account_id)
    .fetch_all(&state.db)
    .await?;

    let items: Vec<Value> = rows.iter().map(|row| {
        let raw_key: String = row.get("api_key");
        json!({
            "id": row.get::<Uuid, _>("id"),
            "account_id": row.get::<Uuid, _>("account_id"),
            "provider": row.get::<String, _>("provider"),
            "api_key_masked": mask_key(&raw_key),
            "base_url": row.get::<Option<String>, _>("base_url"),
            "metadata": row.get::<Option<serde_json::Value>, _>("metadata"),
            "is_active": row.get::<bool, _>("is_active"),
            "scope": row.get::<String, _>("scope"),
            "provider_name": row.get::<Option<String>, _>("provider_name"),
            "provider_description": row.get::<Option<String>, _>("provider_description"),
            "created_at": row.get::<chrono::DateTime<chrono::Utc>, _>("created_at"),
            "updated_at": row.get::<chrono::DateTime<chrono::Utc>, _>("updated_at"),
        })
    }).collect();

    Ok(Json(json!({ "items": items, "count": items.len() })))
}

/// POST /api/v1/provider-keys
pub async fn upsert_provider_key(
    State(state): State<AppState>,
    auth: AuthenticatedUser,
    Json(body): Json<UpsertInput>,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&auth.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;

    // Validate provider exists
    let provider_exists = sqlx::query(
        "SELECT 1 FROM available_providers WHERE key = $1"
    )
    .bind(&body.provider)
    .fetch_optional(&state.db)
    .await?;

    if provider_exists.is_none() {
        return Err(AppError::BadRequest(format!(
            "Unknown provider: '{}'. Must be one of the available providers.",
            body.provider
        )));
    }

    // Upsert using EXCLUDED pattern
    let row = sqlx::query(
        r#"
        INSERT INTO provider_keys (account_id, provider, api_key, base_url, metadata, is_active, scope)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        ON CONFLICT (account_id, provider)
        DO UPDATE SET
            api_key = EXCLUDED.api_key,
            base_url = COALESCE(EXCLUDED.base_url, provider_keys.base_url),
            metadata = CASE
                WHEN EXCLUDED.metadata IS NOT NULL AND EXCLUDED.metadata != '{}'::jsonb
                THEN EXCLUDED.metadata
                ELSE provider_keys.metadata
            END,
            is_active = EXCLUDED.is_active,
            scope = EXCLUDED.scope,
            updated_at = now()
        RETURNING id, account_id, provider, api_key, base_url, metadata, is_active, scope, created_at, updated_at
        "#
    )
    .bind(account_id)
    .bind(&body.provider)
    .bind(&body.api_key)
    .bind(&body.base_url)
    .bind(&body.metadata)
    .bind(body.is_active.unwrap_or(true))
    .bind(body.scope.as_deref().unwrap_or("account"))
    .fetch_one(&state.db)
    .await?;

    let raw_key: String = row.get("api_key");
    let item = json!({
        "id": row.get::<Uuid, _>("id"),
        "account_id": row.get::<Uuid, _>("account_id"),
        "provider": row.get::<String, _>("provider"),
        "api_key_masked": mask_key(&raw_key),
        "base_url": row.get::<Option<String>, _>("base_url"),
        "metadata": row.get::<Option<serde_json::Value>, _>("metadata"),
        "is_active": row.get::<bool, _>("is_active"),
        "scope": row.get::<String, _>("scope"),
        "created_at": row.get::<chrono::DateTime<chrono::Utc>, _>("created_at"),
        "updated_at": row.get::<chrono::DateTime<chrono::Utc>, _>("updated_at"),
    });

    Ok(Json(json!({ "item": item })))
}

/// DELETE /api/v1/provider-keys/:provider
pub async fn delete_provider_key(
    State(state): State<AppState>,
    auth: AuthenticatedUser,
    Path(provider): Path<String>,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&auth.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;

    let result = sqlx::query(
        "DELETE FROM provider_keys WHERE account_id = $1 AND provider = $2"
    )
    .bind(account_id)
    .bind(&provider)
    .execute(&state.db)
    .await?;

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound(format!(
            "Provider key not found for provider '{}'",
            provider
        )));
    }

    Ok(Json(json!({ "status": "deleted", "provider": provider })))
}

/// GET /api/v1/available-providers
pub async fn list_available_providers(
    State(state): State<AppState>,
) -> Result<Json<Value>, AppError> {
    let rows = sqlx::query(
        "SELECT key, name, description, requires_base_url, requires_metadata, icon FROM available_providers ORDER BY name"
    )
    .fetch_all(&state.db)
    .await?;

    let items: Vec<Value> = rows.iter().map(|row| {
        json!({
            "key": row.get::<String, _>("key"),
            "name": row.get::<String, _>("name"),
            "description": row.get::<Option<String>, _>("description"),
            "requires_base_url": row.get::<bool, _>("requires_base_url"),
            "requires_metadata": row.get::<serde_json::Value, _>("requires_metadata"),
            "icon": row.get::<Option<String>, _>("icon"),
        })
    }).collect();

    Ok(Json(json!({ "items": items, "count": items.len() })))
}

// ---- Input types ----

#[derive(Deserialize)]
pub struct UpsertInput {
    pub provider: String,
    pub api_key: String,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
    #[serde(default)]
    pub is_active: Option<bool>,
    #[serde(default)]
    pub scope: Option<String>,
}
