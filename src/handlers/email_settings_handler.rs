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
use axum::{extract::Query, extract::State, Json};
use serde::Deserialize;
use serde_json::{json, Value};

/// The one string a masked secret is replaced with in responses.
const MASK: &str = "••••••••";

fn is_masked(v: &str) -> bool {
    v.is_empty() || v.chars().all(|c| c == '•' || c == '*')
}

/// The non-empty credential fields a stored mail row can carry: the two the panel writes, plus the
/// legacy names this app's own DB CHECK constraint also recognises.
const CREDENTIAL_FIELDS: [&str; 4] = ["api_key", "smtp_password", "password", "pass"];

/// `(credential_present, sealed)` for a STORED `admin_settings.email` row, read from the row's own
/// bytes instead of from the decrypted config.
///
/// `configured` says a provider is stored. It does NOT say the copy in the database is encrypted:
/// the row can be restored from a dump taken before sealing, or written by hand, and this app only
/// seals such a row on its next start. The console printed "Secrets are stored encrypted" as a flat
/// claim until 2026-10-16; both facts are now MEASURED here, through the same predicate the storage
/// layer uses, so the panel cannot disagree with the database. Fleet contract: WorkflowSwift's
/// `stored_credential_present`/`stored_credential_sealed` and CoreSwift's `stored`/`sealed`.
fn at_rest_posture(raw: &Value) -> (bool, bool) {
    let mut present = false;
    let mut all_sealed = true;
    for field in CREDENTIAL_FIELDS {
        let Some(v) = raw.get(field).and_then(Value::as_str) else {
            continue;
        };
        let v = v.trim();
        if v.is_empty() {
            continue;
        }
        present = true;
        if !crate::security::provider_key_crypto::is_encrypted(v) {
            all_sealed = false;
        }
    }
    // Nothing saved is not "sealed" either — there is no copy to describe.
    (present, present && all_sealed)
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

    let last_send: Option<Value> = sqlx::query_scalar::<_, Value>(
        "SELECT value FROM admin_settings WHERE key = 'email_last_send'",
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();

    // Measured BEFORE the masking below rewrites the credential fields: the posture has to be read
    // off the stored row, and the mask is what replaces it in the response.
    let stored = value.clone().unwrap_or_else(|| json!({}));
    let (credential_present, sealed) = at_rest_posture(&stored);

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
        "last_send": last_send,
        // At-rest posture of the SAVED row, PRINTED rather than asserted (kanban t_d1fcda7e).
        "credential_present": credential_present,
        "sealed": sealed,
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

    // The endpoint in this body is used as the FULL request URL for mailgun/sendgrid/sendiio (and
    // the smtp_host as the socket target), by any authenticated caller of this route — gate it at
    // the source as well as at send time (kanban t_f3c75b2a).
    email_provider::gate_config_json(&state.db, &body, "smtp")
        .await
        .map_err(|reason| {
            AppError::BadRequest(format!(
                "Email endpoint refused by security policy: {}",
                reason
            ))
        })?;

    // The credential is SEALED before it reaches the database (kanban t_a794cb09). This row holds
    // the fleet-wide Mailgun private key and used to be written in the clear, so a dump or a
    // read-only psql handed out a working key for every fleet domain.
    email_provider::seal_config_secrets(&state.db, &mut body)
        .await
        .map_err(|e| AppError::Internal(format!("Failed to seal email credentials: {}", e)))?;

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
/// Optional explicit recipient for the "Send test email" button: `POST …/email-settings/test?to=<addr>`.
///
/// Absent, the test goes to the signed-in admin's own address — behaviour unchanged. Added
/// 2026-10-10 (test-mail-hygiene): an automated fleet probe must be able to aim the test at a
/// disposable mailbox, because otherwise every probe run mails whatever inbox the admin owns.
#[derive(Deserialize)]
pub struct TestRecipientQuery {
    pub to: Option<String>,
}

/// The recipient of an admin test send: an explicit `?to=`, else the caller's OWN sign-in address.
///
/// A literal address is NEVER a fallback (test-mail hygiene): an empty sign-in address used to fall
/// back to a hardcoded inbox, so a test press could mail an address the fleet does not own. It is a
/// refusal the operator can act on instead, and it says how to aim the test explicitly.
fn test_recipient(explicit: Option<&str>, own: &str) -> Result<String, AppError> {
    let picked = explicit
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| own.trim());
    if picked.is_empty() {
        return Err(AppError::BadRequest(
            "No inbox to aim the test at: your sign-in address could not be read, so no message was \
             sent. Sign in again, or name a recipient explicitly with ?to=<address>."
                .to_string(),
        ));
    }
    if !picked.contains('@') {
        return Err(AppError::BadRequest(
            "That is not a usable email address, so nothing was sent.".to_string(),
        ));
    }
    Ok(picked.to_string())
}

pub async fn test_email_settings(
    State(state): State<AppState>,
    Query(q): Query<TestRecipientQuery>,
    auth: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    // Reads the SAME row this route's GET and PUT use (the fleet-wide `admin_settings.email`): a
    // test-send that silently used a tenant override would report on a config the panel is not
    // showing. The tenant override was retired (kanban t_123b886b, see `email_provider`).
    let Some(cfg) = email_provider::resolve(&state.db).await else {
        return Ok(Json(json!({
            "success": false,
            "detail": "Global email provider not configured — save provider + credentials first."
        })));
    };

    let to = test_recipient(q.to.as_deref(), &auth.email)?;

    // The SAME record the customer send funnel writes, so a test press and a real customer mail are
    // read off one line. A recipient that cannot receive mail is WITHHELD by `deliver` (it answers
    // Ok), so the withheld case is decided here and reported as its own outcome — the test-mail-hygiene
    // rule: never hand a test address to the provider, and never call a withheld send a delivery.
    let subject = "IncentiveSwift System Email Test";
    let held = crate::email::recipient_is_undeliverable(&to);
    match email_provider::deliver(
        &state.db,
        &cfg,
        &to,
        subject,
        "This is a test of the IncentiveSwift system email provider.\n\nIf you received it, sending works.\n\n- IncentiveSwift",
        None,
    )
    .await
    {
        Ok(()) if held => {
            crate::email::record_last_send(
                &state.db,
                &to,
                subject,
                false,
                "withheld: recipient cannot receive mail (no send attempted)",
            )
            .await;
            Ok(Json(json!({
                "success": false,
                "withheld": true,
                "provider": cfg.provider,
                "to": to,
                "detail": "Withheld: this address cannot receive mail, so no message was sent. Point the test at a real inbox to prove delivery."
            })))
        }
        Ok(()) => {
            crate::email::record_last_send(&state.db, &to, subject, true, "").await;
            Ok(Json(json!({
                "success": true,
                "provider": cfg.provider,
                "to": to,
                "detail": format!("{} accepted the message", cfg.provider)
            })))
        }
        Err(e) => {
            crate::email::record_last_send(&state.db, &to, subject, false, &e).await;
            Ok(Json(json!({
                "success": false,
                "provider": cfg.provider,
                "to": to,
                "detail": e
            })))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_test_send_never_falls_back_to_a_literal_inbox() {
        // The live defect: an empty sign-in address fell back to a hardcoded inbox.
        assert!(test_recipient(None, "").is_err());
        assert!(test_recipient(None, "   ").is_err());
        assert!(test_recipient(None, "not-an-address").is_err());
        assert!(test_recipient(Some("   "), "").is_err());
        // The caller's own address is the recipient when nothing is named.
        assert_eq!(
            test_recipient(None, " probe@example.invalid ").unwrap(),
            "probe@example.invalid"
        );
        // An explicit recipient wins over the caller's own address.
        assert_eq!(
            test_recipient(Some("aim@example.invalid"), "me@example.invalid").unwrap(),
            "aim@example.invalid"
        );
    }

    #[test]
    fn at_rest_posture_reads_the_stored_row_and_can_say_no() {
        // Nothing saved: no credential, so not sealed either.
        assert_eq!(at_rest_posture(&json!({})), (false, false));
        assert_eq!(
            at_rest_posture(&json!({"api_key": "", "smtp_password": "   "})),
            (false, false)
        );
        // A legacy plaintext row: present and NOT sealed — the arm the old panel could not state.
        assert_eq!(
            at_rest_posture(&json!({"api_key": "SG.plaintext"})),
            (true, false)
        );
        // A sealed row: present and sealed.
        assert_eq!(
            at_rest_posture(&json!({"api_key": "enc:v1:AAAA"})),
            (true, true)
        );
        // One plaintext field beside a sealed one makes the row NOT sealed.
        assert_eq!(
            at_rest_posture(&json!({"api_key": "enc:v1:AAAA", "smtp_password": "hunter2"})),
            (true, false)
        );
        // The legacy field names are credentials too.
        assert_eq!(
            at_rest_posture(&json!({"password": "hunter2"})),
            (true, false)
        );
    }
}
