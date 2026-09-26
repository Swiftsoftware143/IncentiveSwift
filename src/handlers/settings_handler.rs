//! Tenant settings handler — manage per-account settings (SEO, branding, etc.)
//!
//! Endpoints:
//!   GET  /api/v1/settings          — list all settings for current account
//!   PUT  /api/v1/settings          — upsert settings

use crate::error::AppError;
use crate::security::auth::AuthenticatedUser;
use crate::state::AppState;
use axum::{extract::State, Json};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

/// A single setting entry.
#[derive(Debug, Serialize, Deserialize)]
pub struct SettingEntry {
    pub key: String,
    pub value: serde_json::Value,
}

/// Request body for PUT /api/v1/settings.
#[derive(Debug, Deserialize)]
pub struct UpdateSettingsRequest {
    pub settings: Vec<SettingEntry>,
}

/// GET /api/v1/settings
pub async fn get_settings(
    State(state): State<AppState>,
    auth: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&auth.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;

    let rows =
        sqlx::query(r#"SELECT key, value FROM tenant_settings WHERE tenant_id = $1 ORDER BY key"#)
            .bind(account_id)
            .fetch_all(&state.db)
            .await
            .map_err(|e| AppError::Internal(format!("DB error: {}", e)))?;

    let settings: Vec<Value> = rows
        .iter()
        .map(|row| {
            let key: String = row.get("key");
            let value: serde_json::Value = row.get("value");
            json!({ "key": key, "value": value })
        })
        .collect();

    Ok(Json(json!({ "settings": settings })))
}

/// PUT /api/v1/settings
pub async fn update_settings(
    State(state): State<AppState>,
    auth: AuthenticatedUser,
    Json(req): Json<UpdateSettingsRequest>,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&auth.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;

    for entry in req.settings {
        // These keys are the TENANT-side writers of the mail endpoints `email_provider` and
        // `delivery::sender` contact, so they are gate-checked where they are stored — a private
        // destination is refused here instead of reaching the socket later (kanban t_f3c75b2a).
        if entry.key == "smtp_host" {
            crate::security::webhook_security::gate_provider_endpoint_host(
                &state.db,
                "smtp",
                entry.value.as_str().unwrap_or(""),
            )
            .await
            .map_err(|reason| {
                AppError::BadRequest(format!("smtp_host refused by security policy: {}", reason))
            })?;
        } else if matches!(
            entry.key.as_str(),
            "email_config" | "mailgun_config" | "smtp_config"
        ) {
            crate::email_provider::gate_config_json(&state.db, &entry.value, "smtp")
                .await
                .map_err(|reason| {
                    AppError::BadRequest(format!(
                        "{} refused by security policy: {}",
                        entry.key, reason
                    ))
                })?;
        }

        sqlx::query(
            r#"INSERT INTO tenant_settings (tenant_id, key, value)
               VALUES ($1, $2, $3::jsonb)
               ON CONFLICT (tenant_id, key)
               DO UPDATE SET value = $3::jsonb"#,
        )
        .bind(account_id)
        .bind(&entry.key)
        .bind(entry.value.to_string())
        .execute(&state.db)
        .await
        .map_err(|e| AppError::Internal(format!("DB error: {}", e)))?;
    }

    Ok(Json(json!({ "message": "Settings updated" })))
}
