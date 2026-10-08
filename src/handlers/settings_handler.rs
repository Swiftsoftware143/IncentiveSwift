//! Tenant settings handler — manage per-account settings (SEO, branding, etc.)
//!
//! Endpoints:
//!   GET  /api/v1/settings            — list all settings for current account
//!   PUT  /api/v1/settings            — upsert settings
//!   POST /api/v1/settings/email/test — send a real message through THIS account's own mail
//!                                      server (the `smtp_*` family below), no platform fallback
//!
//! The tenant's mail server is what the shipped console's Settings → Email pane drives
//! (kanban t_ba200ddf): the pane reads this route, writes the `smtp_*` family through the PUT
//! above and calls the test route. Before that card the whole family was API-only.
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

use crate::delivery::sender::SmtpConfig;
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

/// The one answer shape of `POST /api/v1/settings/email/test` (kanban t_ba200ddf). Pure, so the
/// rule is testable with no database and no transport:
///
/// * `success` is the SERVER's verdict on the TENANT's own mail server. The config reported is the
///   tenant's (`host`/`port`/`username`/`from_email`) because the send never falls back to the
///   platform provider — a fallback would report on a mailer the panel is not showing;
/// * a failure carries the transport's own text, so the operator reads what the server said.
fn test_email_answer(to: &str, sent: &Result<SmtpConfig, String>) -> Value {
    match sent {
        Ok(cfg) => json!({
            "success": true,
            "host": cfg.host,
            "port": cfg.port,
            "username": cfg.username,
            "from_email": cfg.from_email,
            "to": to,
            "detail": format!("{} accepted the test message", cfg.host),
        }),
        Err(detail) => json!({
            "success": false,
            "to": to,
            "detail": detail,
        }),
    }
}

/// POST /api/v1/settings/email/test — send a real message through the TENANT's own mail server.
///
/// The recipient is the CALLER's own address and is deliberately not caller-settable (no body):
/// this route can never be used as a relay. The send goes through
/// [`crate::delivery::sender::test_tenant_smtp_config`], which opens the sealed `smtp_password` at
/// the send site and AUTHs for real — so "the credential the panel saved actually works" is
/// answered by the mail server, not by a form check.
pub async fn test_settings_email(
    State(state): State<AppState>,
    auth: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&auth.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;
    let to = auth.email.trim().to_string();
    if to.is_empty() {
        return Err(AppError::BadRequest(
            "This account has no email address to send the test to".to_string(),
        ));
    }

    let sent = crate::delivery::sender::test_tenant_smtp_config(&state.db, account_id, &to).await;
    Ok(Json(test_email_answer(&to, &sent)))
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
///   just handed the caller, so a panel round-trip cannot store the mask as the password, and an
///   emptied box cannot destroy it either; when there is nothing stored to keep it is REFUSED
///   (kanban t_2e9117a5) instead of storing a credential-shaped blank no reader can use;
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
    if raw.is_empty() || is_masked(raw) {
        // An empty box means exactly what the mask means: KEEP the stored credential. Storing
        // the empty string instead (the pre-t_2e9117a5 behaviour) replaced a working credential
        // with a value no reader can use — silently, because `is_masked` already answers true
        // for "". With nothing stored there is nothing to keep, so the write is refused.
        let stored: Option<Value> = sqlx::query_scalar(
            "SELECT value FROM tenant_settings WHERE tenant_id = $1 AND key = $2",
        )
        .bind(account_id)
        .bind(key)
        .fetch_optional(&state.db)
        .await
        .map_err(|e| AppError::Internal(format!("DB error: {}", e)))?;
        return stored.ok_or_else(|| {
            AppError::BadRequest(format!(
                "{} is empty and no password is saved yet — type the mail server password, or \
                 press Remove mail server in Settings → Email",
                key
            ))
        });
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

/// The check→write race backstop for the request-entry account guard (kanban t_a7b7b5b9).
///
/// `AuthenticatedUser` now refuses a state-changing request whose token names an account that does
/// not exist (`security::auth::guard_account_exists`), so this writer only reaches the INSERT for a
/// real account — unless the row is deleted in the window between that lookup and this statement.
/// Then the FK fires `23503` and the same request would be a 500 again. Translated here for EXACTLY
/// this constraint, so the table's other foreign keys (and every other SQLSTATE) keep their generic
/// 500: a blanket `23503` translation would hide real integrity defects behind a 4xx.
fn is_unknown_account_fk(code: Option<&str>, constraint: Option<&str>) -> bool {
    code == Some("23503") && constraint == Some("tenant_settings_tenant_id_fkey")
}

/// The write's error arm: the account-gone FK becomes the app's own 4xx, everything else stays a
/// 500 with the database's own message.
fn write_err(e: sqlx::Error) -> AppError {
    if let sqlx::Error::Database(db) = &e {
        if is_unknown_account_fk(db.code().as_deref(), db.constraint()) {
            return crate::security::auth::unknown_account();
        }
    }
    AppError::Internal(format!("DB error: {}", e))
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

        // A BLANK scalar in the mail family is refused where it would be STORED (kanban
        // t_2e9117a5): a stored `""` is a PRESENT config that lettre fails on, so the tenant's
        // mail stopped silently instead of falling back to the platform mail service.
        if let Some(reason) = mail_blank_refusal(&entry.key, &value) {
            return Err(AppError::BadRequest(reason));
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
        } else if entry.key == crate::branding::SETTINGS_KEY {
            // Account email branding (kanban t_feab8aff). The two halves are asymmetric on purpose:
            //  * `brand_name` / `brand_color` are validated here (length, control chars, hex colour)
            //    so the renderer never has to defend against what this write let through;
            //  * `logo_url` is owned by the logo endpoints, so when the caller omits the key the
            //    STORED value is inherited. The console echoes the document it was given, and a stale
            //    echo must not un-reference a logo that is still stored. An explicit `""` still
            //    clears it (the panel's own "remove", alongside DELETE /settings/branding/logo).
            crate::branding::validate_value(&value).map_err(AppError::BadRequest)?;
            if let Some(obj) = value.as_object_mut() {
                if !obj.contains_key("logo_url") {
                    let stored: Option<serde_json::Value> = sqlx::query_scalar(
                        "SELECT value->'logo_url' FROM tenant_settings WHERE tenant_id = $1 AND key = $2",
                    )
                    .bind(account_id)
                    .bind(&entry.key)
                    .fetch_optional(&state.db)
                    .await
                    .ok()
                    .flatten()
                    .flatten();
                    if let Some(u) = stored {
                        obj.insert("logo_url".to_string(), u);
                    }
                }
            }
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
        .map_err(write_err)?;
    }

    Ok(Json(json!({ "message": "Settings updated" })))
}

/// Why a value may not be STORED for a key in the tenant mail family (kanban t_2e9117a5).
///
/// The pane requires host/username/from/password to be non-empty, but the API did not: a stored
/// `""` is a PRESENT config (`load_smtp_config` needs the key and an `as_str()`-able value), the
/// `smtp_host` write gate explicitly ALLOWS an empty value, and lettre then fails on it — the
/// tenant's mail silently stopped instead of falling back to the platform mailer. Refusing the
/// blank where it would be stored, and naming the control that really means "I have no mail
/// server", is what keeps that from ever being reachable again. Optional keys (from-name) and
/// the numeric port are deliberately not covered: a blank from-name is legitimate and a blank
/// port falls back to 587 in the reader.
fn mail_blank_refusal(key: &str, value: &Value) -> Option<String> {
    let raw = value.as_str()?;
    if !raw.trim().is_empty() {
        return None;
    }
    match key {
        "smtp_host" | "smtp_username" | "smtp_from_email" => Some(format!(
            "{} cannot be blank — press Remove mail server in Settings → Email (DELETE \
             /api/v1/settings/email) to go back to the platform mail service",
            key
        )),
        _ => None,
    }
}

/// DELETE /api/v1/settings/email — remove THIS account's own mail server.
///
/// The console could configure and UPDATE a tenant mail server but never take one away (kanban
/// t_2e9117a5): `PUT` had no delete arm and a blank host is refused, so an account that had ever
/// saved a server was stuck with it. This deletes exactly the caller's OWN rows under
/// [`crate::delivery::sender::MAIL_KEY_PREFIX`] — the family `load_smtp_config` reads and nothing
/// else — which is what "go back to the platform mail service" means. The prefix is bound as a
/// parameter and the tenant id comes from the token, so the arm cannot touch another account or
/// an unrelated setting. Idempotent: an account with no mail server answers 200 with an empty
/// list, so the panel's button is safe to press twice.
pub async fn delete_settings_mail(
    State(state): State<AppState>,
    auth: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&auth.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;

    let removed: Vec<String> = sqlx::query_scalar(
        "DELETE FROM tenant_settings\n          WHERE tenant_id = $1 AND left(key, length($2)) = $2\n      RETURNING key",
    )
    .bind(account_id)
    .bind(crate::delivery::sender::MAIL_KEY_PREFIX)
    .fetch_all(&state.db)
    .await
    .map_err(write_err)?;

    Ok(Json(json!({
        "message": "Mail server removed — this account now uses the platform mail service.",
        "removed": removed,
    })))
}

#[cfg(test)]
mod tests {
    use super::{is_masked, is_unknown_account_fk, test_email_answer, MASK};
    use crate::delivery::sender::{SmtpConfig, NO_TENANT_SMTP};

    fn cfg() -> SmtpConfig {
        SmtpConfig {
            host: "smtp.acme.example".to_string(),
            port: 587,
            username: "postmaster@acme.example".to_string(),
            password: "secret".to_string(),
            from_email: "noreply@acme.example".to_string(),
            from_name: None,
        }
    }

    /// The panel's Test send (kanban t_ba200ddf) must report the TENANT's OWN server, and only the
    /// server's own acknowledgement may turn that into `success`: the config named in a success
    /// answer is the one the transport actually used.
    #[test]
    fn a_successful_tenant_test_names_the_tenants_own_server() {
        let ok = test_email_answer("op@acme.example", &Ok(cfg()));
        assert_eq!(ok["success"], serde_json::json!(true));
        assert_eq!(ok["host"], serde_json::json!("smtp.acme.example"));
        assert_eq!(ok["port"], serde_json::json!(587));
        assert_eq!(ok["from_email"], serde_json::json!("noreply@acme.example"));
        assert_eq!(ok["to"], serde_json::json!("op@acme.example"));
        assert!(ok["detail"].as_str().unwrap().contains("smtp.acme.example"));
        // the credential NEVER travels back to the caller
        assert!(!ok.to_string().contains("secret"));
    }

    /// The no-fallback rule, as data: an account with no mail server of its own must get a
    /// failure that points at the panel, and must never be told a platform provider accepted
    /// anything — a test that silently fell back would report on a mailer the panel is not showing.
    #[test]
    fn the_tenant_test_never_falls_back_to_the_platform_provider() {
        let no = test_email_answer("op@acme.example", &Err(NO_TENANT_SMTP.to_string()));
        assert_eq!(no["success"], serde_json::json!(false));
        assert!(no["detail"].as_str().unwrap().contains("Settings"));
        let lowered = no.to_string().to_lowercase();
        for forbidden in [
            "mailgun",
            "sendgrid",
            "sendiio",
            "fallback",
            "platform provider",
        ] {
            assert!(
                !lowered.contains(forbidden),
                "the tenant test must not talk about {forbidden}: {no}"
            );
        }
    }

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

    /// The check→write race backstop (kanban t_a7b7b5b9) must stay EXACT: only a 23503 raised by
    /// `tenant_settings_tenant_id_fkey` becomes the app's "unknown account" 4xx. Another foreign key
    /// on the same table, another SQLSTATE, or a driver that does not name the constraint at all
    /// must keep the generic 500 — otherwise a real integrity defect would be reported to the caller
    /// as their own bad input.
    #[test]
    fn only_the_account_gone_fk_is_translated() {
        assert!(is_unknown_account_fk(
            Some("23503"),
            Some("tenant_settings_tenant_id_fkey")
        ));
        assert!(!is_unknown_account_fk(
            Some("23503"),
            Some("tenant_settings_pkey")
        ));
        assert!(!is_unknown_account_fk(
            Some("23505"),
            Some("tenant_settings_tenant_id_fkey")
        ));
        assert!(!is_unknown_account_fk(Some("23503"), None));
        assert!(!is_unknown_account_fk(
            None,
            Some("tenant_settings_tenant_id_fkey")
        ));
    }

    /// A blank scalar in the mail family is refused, and the refusal must name the control that
    /// really means "no mail server" — while the optional keys and every ordinary setting stay
    /// accepted (kanban t_2e9117a5).
    #[test]
    fn a_blank_mail_scalar_is_refused_and_names_the_remove_control() {
        use super::mail_blank_refusal;
        use serde_json::json;
        for k in ["smtp_host", "smtp_username", "smtp_from_email"] {
            let reason = mail_blank_refusal(k, &json!("")).expect("a blank must be refused");
            assert!(reason.contains("Remove mail server"), "{k}: {reason}");
            assert!(reason.contains("/api/v1/settings/email"), "{k}: {reason}");
            assert!(mail_blank_refusal(k, &json!("smtp.acme.example")).is_none());
            // whitespace is blank too — it is exactly the value lettre would sign in with
            assert!(
                mail_blank_refusal(k, &json!("   ")).is_some(),
                "{k} whitespace"
            );
            assert!(
                mail_blank_refusal(k, &json!(587)).is_none(),
                "{k} non-string"
            );
        }
        for k in [
            "smtp_from_name",
            "smtp_port",
            "company_name",
            "support_email",
        ] {
            assert!(
                mail_blank_refusal(k, &json!("")).is_none(),
                "{k} must stay accepted"
            );
        }
    }
}
