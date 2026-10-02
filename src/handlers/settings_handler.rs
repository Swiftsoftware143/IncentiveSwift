//! Tenant settings handler — manage per-account settings (SEO, branding, etc.)
//!
//! Endpoints:
//!   GET  /api/v1/settings          — list all settings for current account
//!   PUT  /api/v1/settings          — upsert settings
//!
//! Two mail-credential contracts live on this route (kanban t_123b886b):
//!
//! * the bare `smtp_password` scalar IS a credential — it is what
//!   `delivery::sender::load_smtp_config` (a LIVE send path: the pending-email ticker, lifecycle
//!   emails, entry emails, output actions) hands to lettre. It is SEALED here with the same
//!   `enc:v1:` envelope the rest of the app uses, MASKED on the way back out, and a masked
//!   round-trip keeps the stored value instead of storing the mask;
//! * the tenant mail-config OBJECT keys (`email_config` / `mailgun_config` / `smtp_config`) are
//!   RETIRED and REFUSED here. Nothing read them: `email_provider::resolve` takes no tenant, no
//!   shipped screen writes them, and the live database held 0 rows. The tenant's own mail server is
//!   the bare `smtp_*` family above (see `email_provider::retired_tenant_mail_key`).

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
/// email-settings surface uses, so the two credential surfaces answer in one shape — and the SAME
/// marker [`is_masked`] recognises on the way back in, so a panel round-trip can never store it.
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

        if key == crate::delivery::sender::SMTP_PASSWORD_KEY {
            // The credential is ciphertext at rest; this surface must never ship the envelope (a
            // client cannot tell `enc:v1:…` from a masked key) nor the plaintext. OPEN it FIRST:
            // the mask is reported only when the value can actually be read, so a row this
            // deployment cannot open reads as "not set" instead of looking healthy.
            let stored = value.as_str().unwrap_or("").to_string();
            let set = match crate::delivery::sender::open_smtp_password(&state.db, &stored).await {
                Ok(p) => !p.is_empty(),
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        "tenant smtp_password cannot be opened — reporting it as unset"
                    );
                    false
                }
            };
            value = json!(if set { MASK } else { "" });
        }

        settings.push(json!({ "key": key, "value": value }));
    }

    Ok(Json(json!({ "settings": settings })))
}

/// The `smtp_password` write half.
///
/// * a masked (or empty) incoming value keeps the STORED credential — the shape `get_settings`
///   just handed the caller, so a panel round-trip cannot store the mask as the password;
/// * anything else is SEALED before it reaches the database;
/// * a non-string is refused: `load_smtp_config` would ignore it, leaving a credential-shaped
///   value in the column that no reader can use.
async fn seal_smtp_password(
    state: &AppState,
    account_id: Uuid,
    value: Value,
) -> Result<Value, AppError> {
    let key = crate::delivery::sender::SMTP_PASSWORD_KEY;
    let Some(raw) = value.as_str() else {
        return Err(AppError::BadRequest(format!("{} must be a string", key)));
    };
    if raw.is_empty() {
        return Ok(value);
    }
    if is_masked(raw) {
        let stored: Option<Value> = sqlx::query_scalar(
            "SELECT value FROM tenant_settings WHERE tenant_id = $1 AND key = $2",
        )
        .bind(account_id)
        .bind(key)
        .fetch_optional(&state.db)
        .await
        .map_err(|e| AppError::Internal(format!("DB error: {}", e)))?;
        return Ok(stored.unwrap_or_else(|| json!("")));
    }
    if crate::security::provider_key_crypto::is_encrypted(raw) {
        // Already sealed — a caller carrying the STORED ciphertext back (an older client that read
        // the row itself, a future panel that does not mask) must not be sealed a SECOND time: the
        // outer envelope would open to the inner one and the SMTP server would be handed an
        // envelope as the password instead of the credential. Same skip the config-object writer
        // makes, and what keeps a ciphertext round-trip from double-wrapping.
        return Ok(value);
    }
    let sealed = crate::security::provider_key_crypto::encrypt_for_storage(&state.db, raw)
        .await
        .map_err(|e| AppError::Internal(format!("Failed to seal {}: {}", key, e)))?;
    Ok(json!(sealed))
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

        // A RETIRED mail-config key is refused where it would be STORED, so it can never sit at
        // rest as a credential nothing reads (kanban t_123b886b).
        if let Some(reason) = crate::email_provider::retired_tenant_mail_key(&entry.key) {
            return Err(AppError::BadRequest(format!(
                "{} is refused: {}",
                entry.key, reason
            )));
        }

        if entry.key == "smtp_host" {
            // The host is the socket target `delivery::sender::send_email` will open, so it is
            // gate-checked where it is stored — a private destination is refused here instead of
            // reaching the socket later (kanban t_f3c75b2a).
            crate::security::webhook_security::gate_provider_endpoint_host(
                &state.db,
                "smtp",
                value.as_str().unwrap_or(""),
            )
            .await
            .map_err(|reason| {
                AppError::BadRequest(format!("smtp_host refused by security policy: {}", reason))
            })?;
        } else if entry.key == crate::delivery::sender::SMTP_PASSWORD_KEY {
            value = seal_smtp_password(&state, account_id, value).await?;
        }

        sqlx::query(
            r#"INSERT INTO tenant_settings (tenant_id, key, value)
               VALUES ($1, $2, $3::jsonb)
               ON CONFLICT (tenant_id, key)
               DO UPDATE SET value = $3::jsonb, updated_at = NOW()"#,
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

#[cfg(test)]
mod tests {
    use super::{is_masked, MASK};

    /// The RETIRE decision (kanban t_123b886b) must stay visible and stay KEY-SCOPED: those three
    /// config-object keys can never be stored again because nothing reads them, while the LIVE
    /// `smtp_*` family and ordinary tenant settings must keep working. Re-adding a reader for a
    /// retired key means deleting it from this list deliberately, which is a visible change here.
    #[test]
    fn retired_tenant_mail_keys_are_refused_and_the_live_family_is_not() {
        for k in ["email_config", "mailgun_config", "smtp_config"] {
            assert!(
                crate::email_provider::retired_tenant_mail_key(k).is_some(),
                "{k} must stay retired"
            );
        }
        for k in [
            "smtp_password",
            "smtp_host",
            "smtp_port",
            "smtp_username",
            "smtp_from_email",
            "company_name",
            "support_email",
            // a prefix twin of a retired key is NOT the retired key
            "smtp_configs",
            "email_config_backup",
        ] {
            assert!(
                crate::email_provider::retired_tenant_mail_key(k).is_none(),
                "{k} must stay accepted"
            );
        }
    }

    /// Only the all-bullets marker counts as "the caller echoed the mask back". A DERIVED mask
    /// (`abc...xyz`) must NOT be recognised: storing it back would silently replace the credential
    /// with its own mask — which is exactly why this route answers with the fixed bullet mask.
    #[test]
    fn only_the_all_bullets_mask_counts_as_masked() {
        assert!(is_masked(MASK));
        assert!(is_masked("********"));
        assert!(is_masked(""));
        assert!(!is_masked("t12...9f4"));
        assert!(!is_masked("hunter2"));
        assert!(!is_masked("•partial"));
    }
}
