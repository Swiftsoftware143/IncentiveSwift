//! System email provider settings (admin).
//!
//! The provider is a stored choice (`smtp | mailgun | sendgrid | sendiio`) held in
//! `admin_settings` key `email`. Credentials live in the database and are entered in the
//! admin panel — nothing is read from the process environment.
//!
//! Routes:
//!   GET  /api/v1/admin/email-settings       — current config, secrets masked
//!   PUT  /api/v1/admin/email-settings       — save (a masked secret never clobbers the stored one)
//!   POST /api/v1/admin/email-settings/test  — send a real test message

use crate::email_provider;
use crate::error::AppError;
use crate::security::auth::AuthenticatedUser;
use crate::state::AppState;
use axum::{extract::State, Json};
use serde_json::{json, Value};

/// The one string a masked secret is replaced with in responses.
const MASK: &str = "••••••••";

fn is_masked(v: &str) -> bool {
    v.is_empty() || v.chars().all(|c| c == '•' || c == '*')
}

async fn load_row(state: &AppState) -> Option<Value> {
    sqlx::query_scalar::<_, Value>(
        "SELECT COALESCE(value, '{}'::jsonb) FROM admin_settings WHERE key = 'email'",
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten()
}

/// GET /api/v1/admin/email-settings
pub async fn get_email_settings(
    State(state): State<AppState>,
    _auth: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let value = load_row(&state).await;
    let cfg = value
        .as_ref()
        .map(|v| email_provider::EmailConfig::from_json(v, "smtp"));

    let mut out = value.unwrap_or_else(|| json!({}));
    if let Some(obj) = out.as_object_mut() {
        if obj.contains_key("api_key") {
            let set = cfg.as_ref().map(|c| !c.api_key.is_empty()).unwrap_or(false);
            obj.insert("api_key".into(), json!(if set { MASK } else { "" }));
            obj.insert("api_key_set".into(), json!(set));
        }
        if obj.contains_key("smtp_password") {
            let set = cfg
                .as_ref()
                .map(|c| !c.smtp_password.is_empty())
                .unwrap_or(false);
            obj.insert("smtp_password".into(), json!(if set { MASK } else { "" }));
            obj.insert("smtp_password_set".into(), json!(set));
        }
    }

    Ok(Json(json!({
        "config": out,
        "configured": cfg.as_ref().map(|c| c.is_configured()).unwrap_or(false),
        "provider": cfg.as_ref().map(|c| c.provider.clone()).unwrap_or_default(),
        "providers": email_provider::available(),
    })))
}

/// PUT /api/v1/admin/email-settings
pub async fn update_email_settings(
    State(state): State<AppState>,
    _auth: AuthenticatedUser,
    Json(mut body): Json<Value>,
) -> Result<Json<Value>, AppError> {
    let existing = load_row(&state).await.unwrap_or_else(|| json!({}));

    let obj = body
        .as_object_mut()
        .ok_or_else(|| AppError::BadRequest("Expected a JSON object".to_string()))?;

    for secret in ["api_key", "smtp_password"] {
        let incoming = obj.get(secret).and_then(|v| v.as_str()).unwrap_or("");
        if is_masked(incoming) {
            let kept = existing.get(secret).cloned().unwrap_or(json!(""));
            obj.insert(secret.to_string(), kept);
        }
    }
    obj.remove("api_key_set");
    obj.remove("smtp_password_set");

    if let Some(p) = body.get("provider").and_then(|v| v.as_str()) {
        let valid = email_provider::available()
            .iter()
            .any(|v| v.get("value").and_then(|x| x.as_str()) == Some(p));
        if !valid {
            return Err(AppError::BadRequest(format!(
                "Unknown email provider '{p}'. Use one of the values served by GET /api/v1/admin/email-settings."
            )));
        }
    }

    sqlx::query(
        "INSERT INTO admin_settings (key, value, description, updated_at)
         VALUES ('email', $1::jsonb, 'Global system email provider (admin-editable)', NOW())
         ON CONFLICT (key) DO UPDATE SET value = $1::jsonb, updated_at = NOW()",
    )
    .bind(&body)
    .execute(&state.db)
    .await
    .map_err(|e| AppError::Internal(format!("Failed to save email settings: {e}")))?;

    Ok(Json(json!({ "success": true })))
}

/// POST /api/v1/admin/email-settings/test — real send, returns the provider's true answer.
pub async fn test_email_settings(
    State(state): State<AppState>,
    auth: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let Some(cfg) = email_provider::resolve(&state.db, None).await else {
        return Ok(Json(json!({
            "success": false,
            "detail": "Global email provider not configured — save provider + credentials first."
        })));
    };

    let to = if auth.email.trim().is_empty() {
        "swiftsoftware143@yahoo.com".to_string()
    } else {
        auth.email.clone()
    };

    match email_provider::deliver(
        &cfg,
        &to,
        "IncentiveSwift System Email Test",
        "This is a test of the IncentiveSwift system email provider.\n\nIf you received it, sending works.\n\n- IncentiveSwift",
        None,
    )
    .await
    {
        Ok(()) => Ok(Json(json!({
            "success": true,
            "provider": cfg.provider,
            "to": to,
            "detail": format!("{} accepted the message", cfg.provider)
        }))),
        Err(e) => Ok(Json(json!({
            "success": false,
            "provider": cfg.provider,
            "to": to,
            "detail": e
        }))),
    }
}
