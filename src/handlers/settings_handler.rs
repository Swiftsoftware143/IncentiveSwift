//! Tenant settings handler — manage per-account settings (SEO, branding, etc.)
//!
//! Endpoints:
//!   GET  /api/v1/settings          — list all settings for current account
//!   PUT  /api/v1/settings          — upsert settings
//!
//! This route is also a WRITER of the tenant's own mail credential: `email_provider::resolve`
//! reads `tenant_settings` keys `email_config` / `mailgun_config` / `smtp_config` (resolution
//! order 1-3) for a tenant, and `PUT /api/v1/settings` used to store their `api_key` /
//! `smtp_password` fields in the CLEAR — the sibling of the fleet-wide `admin_settings.email`
//! defect (kanban t_a794cb09). Both halves are closed here (kanban t_a65483ff): the write path
//! seals with the same `enc:v1:` envelope before it stores, the read path opens and masks the
//! DECRYPTED value, and a masked round-trip never clobbers the stored credential.

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

/// The one string a masked secret is replaced with in responses. Same marker the app's admin
/// email-settings surface uses, so the two credential surfaces answer in one shape.
const MASK: &str = "••••••••";

/// True when the caller sent the mask back (or nothing at all) instead of a credential: such a
/// field must keep the STORED value, never overwrite it with the literal mask.
fn is_masked(v: &str) -> bool {
    v.is_empty() || v.chars().all(|c| c == '•' || c == '*')
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

    let mut settings: Vec<Value> = Vec::with_capacity(rows.len());
    for row in rows.iter() {
        let key: String = row.get("key");
        let mut value: serde_json::Value = row.get("value");

        if crate::email_provider::TENANT_CONFIG_KEYS.contains(&key.as_str()) {
            // The credential is ciphertext at rest; this surface must never be handed the
            // envelope (a client cannot tell `enc:v1:…` from a masked key, and the ciphertext
            // would ship to the browser). Open the DECRYPTED value FIRST, then mask the secret
            // fields — a mask derived from the ciphertext is itself the defect.
            let opened = crate::email_provider::open_config_secrets(&state.db, &mut value)
                .await
                .is_ok();
            if !opened {
                tracing::error!(
                    key,
                    "tenant email config credential cannot be opened — reporting it as unset"
                );
            }
            if let Some(obj) = value.as_object_mut() {
                for field in crate::email_provider::CONFIG_SECRET_FIELDS {
                    let set = opened
                        && obj
                            .get(field)
                            .and_then(|v| v.as_str())
                            .map(|s| !s.is_empty())
                            .unwrap_or(false);
                    if obj.contains_key(field) {
                        obj.insert(field.to_string(), json!(if set { MASK } else { "" }));
                    }
                    obj.insert(format!("{}_set", field), json!(set));
                }
            }
        }

        settings.push(json!({ "key": key, "value": value }));
    }

    Ok(Json(json!({ "settings": settings })))
}

/// Restore the stored credential for any secret field the caller returned MASKED — the shape
/// `get_settings` answers with, so a panel round-trip (GET → edit → PUT) keeps the stored key
/// instead of storing the mask over it. Mirrors the admin email-settings writer.
async fn restore_masked_secrets(
    state: &AppState,
    account_id: Uuid,
    key: &str,
    value: &mut Value,
) -> Result<(), AppError> {
    let Some(obj) = value.as_object_mut() else {
        return Ok(());
    };
    let any_masked = crate::email_provider::CONFIG_SECRET_FIELDS.iter().any(|f| {
        obj.get(*f)
            .and_then(|v| v.as_str())
            .map(is_masked)
            .unwrap_or(false)
    });
    if !any_masked {
        return Ok(());
    }

    let stored: Option<Value> =
        sqlx::query_scalar("SELECT value FROM tenant_settings WHERE tenant_id = $1 AND key = $2")
            .bind(account_id)
            .bind(key)
            .fetch_optional(&state.db)
            .await
            .map_err(|e| AppError::Internal(format!("DB error: {}", e)))?;
    let stored = stored
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();

    for field in crate::email_provider::CONFIG_SECRET_FIELDS {
        let incoming = obj.get(field).and_then(|v| v.as_str()).unwrap_or("");
        if is_masked(incoming) {
            let kept = stored.get(field).cloned().unwrap_or(json!(""));
            obj.insert(field.to_string(), kept);
        }
    }
    Ok(())
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
        let mut value = entry.value;

        // These keys are the TENANT-side writers of the mail endpoints `email_provider` and
        // `delivery::sender` contact, so they are gate-checked where they are stored — a private
        // destination is refused here instead of reaching the socket later (kanban t_f3c75b2a).
        if entry.key == "smtp_host" {
            crate::security::webhook_security::gate_provider_endpoint_host(
                &state.db,
                "smtp",
                value.as_str().unwrap_or(""),
            )
            .await
            .map_err(|reason| {
                AppError::BadRequest(format!("smtp_host refused by security policy: {}", reason))
            })?;
        } else if crate::email_provider::TENANT_CONFIG_KEYS.contains(&entry.key.as_str()) {
            crate::email_provider::gate_config_json(&state.db, &value, "smtp")
                .await
                .map_err(|reason| {
                    AppError::BadRequest(format!(
                        "{} refused by security policy: {}",
                        entry.key, reason
                    ))
                })?;

            // A masked round-trip (what GET just handed back) must not be stored as the
            // credential, and then the credential the caller DID send is SEALED before it
            // reaches the database — this write path stored a tenant's provider key in the
            // clear (kanban t_a65483ff, sibling of t_a794cb09).
            restore_masked_secrets(&state, account_id, &entry.key, &mut value).await?;
            crate::email_provider::seal_config_secrets(&state.db, &mut value)
                .await
                .map_err(|e| {
                    AppError::Internal(format!("Failed to seal email credentials: {}", e))
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
        .bind(value.to_string())
        .execute(&state.db)
        .await
        .map_err(|e| AppError::Internal(format!("DB error: {}", e)))?;
    }

    Ok(Json(json!({ "message": "Settings updated" })))
}
