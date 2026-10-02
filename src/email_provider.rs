//! Email provider configuration + delivery — **DB ONLY** (no env-var credentials).
//!
//! Nothing here reads the process environment. Credentials come from the database and are
//! entered in the admin panel (Admin > Settings > Email Provider).
//!
//! Resolution order for a tenant:
//!   1. `tenant_settings` key `email_config`   (explicit `provider`: smtp|mailgun|sendgrid|sendiio)
//!   2. `tenant_settings` key `mailgun_config` (legacy row → provider "mailgun")
//!   3. `tenant_settings` key `smtp_config`    (legacy row → provider "smtp")
//!   4. `admin_settings`  key `email`          (global system mail, admin-editable)
//!
//! Unconfigured resolves to `None`; every caller logs and skips (never panics, never
//! silently falls back to a server-wide env var).

use crate::security::provider_key_crypto;
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct EmailConfig {
    pub provider: String,
    pub api_url: String,
    pub api_key: String,
    pub domain: String,
    pub from_address: String,
    pub from_name: String,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_username: String,
    pub smtp_password: String,
    pub smtp_encryption: String,
}

fn s(cfg: &Value, key: &str) -> String {
    cfg.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string()
}

/// Accept both the new generic names and the legacy Mailgun/SMTP row names.
fn first(cfg: &Value, keys: &[&str]) -> String {
    for k in keys {
        let v = s(cfg, k);
        if !v.is_empty() {
            return v;
        }
    }
    String::new()
}

impl EmailConfig {
    pub fn from_json(cfg: &Value, default_provider: &str) -> EmailConfig {
        let provider = {
            let p = s(cfg, "provider").to_ascii_lowercase();
            if p.is_empty() {
                default_provider.to_string()
            } else {
                p
            }
        };
        let from_address = first(cfg, &["from_address", "from_email"]);
        let from_name = {
            let n = first(cfg, &["from_name"]);
            if n.is_empty() {
                "IncentiveSwift".to_string()
            } else {
                n
            }
        };
        EmailConfig {
            provider,
            api_url: first(cfg, &["api_url", "base_url"]),
            api_key: first(cfg, &["api_key"]),
            domain: first(cfg, &["domain", "mailgun_domain"]),
            from_address,
            from_name,
            smtp_host: first(cfg, &["smtp_host", "host"]),
            smtp_port: cfg
                .get("smtp_port")
                .or_else(|| cfg.get("port"))
                .and_then(|v| v.as_u64())
                .unwrap_or(587) as u16,
            smtp_username: first(cfg, &["smtp_username", "username", "user"]),
            smtp_password: first(cfg, &["smtp_password", "password", "pass"]),
            smtp_encryption: first(cfg, &["smtp_encryption", "encryption"]),
        }
    }

    /// True when the stored row actually carries what its transport needs.
    pub fn is_configured(&self) -> bool {
        match self.provider.as_str() {
            "smtp" => !self.smtp_host.is_empty() && !self.from_address.is_empty(),
            _ => {
                (!self.api_key.is_empty() || !self.api_url.is_empty())
                    && !self.from_address.is_empty()
            }
        }
    }

    pub fn sender(&self) -> String {
        if self.from_name.is_empty() {
            self.from_address.clone()
        } else {
            format!("{} <{}>", self.from_name, self.from_address)
        }
    }
}

/// The credential fields carried inside an `admin_settings.email` (or tenant `email_config`)
/// object. They are sealed with the SAME `enc:v1:` envelope this app already uses for
/// `provider_keys`/`payment_providers` — this config was the one credential path that stored
/// its value in the clear (kanban t_a794cb09), so a dump or a backup yielded a usable Mailgun
/// private key for the whole fleet.
pub const CONFIG_SECRET_FIELDS: [&str; 2] = ["api_key", "smtp_password"];

/// Seal the credential fields of an email-config object IN PLACE, before it is stored.
///
/// * empty stays empty — a blank field is "no credential", never a ciphertext of nothing;
/// * an already-sealed value is left exactly as it is: that is what the panel's masked
///   round-trip carries back, and re-encrypting it would destroy the stored credential;
/// * a missing master key makes this FAIL — a plaintext credential is never a fallback.
pub async fn seal_config_secrets(
    pool: &PgPool,
    cfg: &mut Value,
) -> Result<(), provider_key_crypto::CryptoError> {
    let Some(obj) = cfg.as_object_mut() else {
        return Ok(());
    };
    for field in CONFIG_SECRET_FIELDS {
        let current = obj
            .get(field)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if current.is_empty() || provider_key_crypto::is_encrypted(&current) {
            continue;
        }
        let sealed = provider_key_crypto::encrypt_for_storage(pool, &current).await?;
        obj.insert(field.to_string(), Value::String(sealed));
    }
    Ok(())
}

/// Open the credential fields of an email-config object IN PLACE after a DB read, so what
/// reaches a provider is the credential and never the envelope. A value without the envelope
/// is a legacy plaintext row and is passed through unchanged.
pub async fn open_config_secrets(
    pool: &PgPool,
    cfg: &mut Value,
) -> Result<(), provider_key_crypto::CryptoError> {
    let Some(obj) = cfg.as_object_mut() else {
        return Ok(());
    };
    for field in CONFIG_SECRET_FIELDS {
        let current = obj
            .get(field)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if current.is_empty() || !provider_key_crypto::is_encrypted(&current) {
            continue;
        }
        let opened = provider_key_crypto::decrypt_from_storage(pool, &current).await?;
        obj.insert(field.to_string(), Value::String(opened));
    }
    Ok(())
}

/// Seal every credential still sitting in the clear in the `admin_settings.email` row.
///
/// Both write paths seal before they store, but this row can also arrive plaintext from a
/// database restored out of a dump taken before the change, or from a writer added later that
/// forgets. Idempotent; returns the number of rows it had to rewrite.
pub async fn seal_legacy_config_secrets(
    pool: &PgPool,
) -> Result<u64, provider_key_crypto::CryptoError> {
    let mut value: Option<Value> =
        sqlx::query_scalar("SELECT value FROM admin_settings WHERE key = 'email'")
            .fetch_optional(pool)
            .await?;
    let Some(mut value) = value.take() else {
        return Ok(0);
    };
    if !value.is_object() {
        return Ok(0);
    }
    let before = value.clone();
    seal_config_secrets(pool, &mut value).await?;
    if value == before {
        return Ok(0);
    }
    sqlx::query(
        "UPDATE admin_settings SET value = $1::jsonb, updated_at = NOW() WHERE key = 'email'",
    )
    .bind(&value)
    .execute(pool)
    .await?;
    Ok(1)
}

/// The three TENANT-side mail-config keys `resolve` reads (resolution order 1-3) and the one
/// tenant route may write (`PUT /api/v1/settings`). Their `api_key` / `smtp_password` fields are
/// credentials and carry the SAME `enc:v1:` envelope as the admin `admin_settings.email` row
/// (kanban t_a794cb09) — this was the sibling write path that stored a tenant's provider key in
/// the clear (kanban t_a65483ff).
pub const TENANT_CONFIG_KEYS: [&str; 3] = ["email_config", "mailgun_config", "smtp_config"];

/// Seal every credential still sitting in the clear in a TENANT's own mail-config rows
/// (`tenant_settings` keys [`TENANT_CONFIG_KEYS`]).
///
/// The tenant writer seals before it stores, exactly as the admin writer does, but these rows can
/// also arrive plaintext from a database restored out of an older dump — or from a writer added
/// later that forgets. This is the boot half that converges them. Idempotent; returns the number
/// of rows it had to rewrite.
pub async fn seal_legacy_tenant_config_secrets(
    pool: &PgPool,
) -> Result<u64, provider_key_crypto::CryptoError> {
    let mut sealed = 0u64;
    for key in TENANT_CONFIG_KEYS {
        let rows: Vec<(Uuid, Value)> =
            sqlx::query_as("SELECT tenant_id, value FROM tenant_settings WHERE key = $1")
                .bind(key)
                .fetch_all(pool)
                .await?;
        for (tenant_id, mut value) in rows {
            if !value.is_object() {
                continue;
            }
            let before = value.clone();
            seal_config_secrets(pool, &mut value).await?;
            if value == before {
                continue;
            }
            sqlx::query(
                "UPDATE tenant_settings SET value = $1::jsonb, updated_at = NOW()
                  WHERE tenant_id = $2 AND key = $3",
            )
            .bind(&value)
            .bind(tenant_id)
            .bind(key)
            .execute(pool)
            .await?;
            sealed += 1;
        }
    }
    Ok(sealed)
}

async fn row(pool: &PgPool, sql: &str, binds: &[&str]) -> Option<Value> {
    let mut q = sqlx::query_scalar::<_, Value>(sql);
    for b in binds {
        q = q.bind(*b);
    }
    q.fetch_optional(pool).await.ok().flatten()
}

/// Resolve the email configuration for a tenant, falling back to the global
/// `admin_settings.email` row (system mail). Returns `None` when nothing is configured.
pub async fn resolve(pool: &PgPool, tenant_id: Option<Uuid>) -> Option<EmailConfig> {
    if let Some(tid) = tenant_id {
        let candidates: [(&str, &str); 3] = [
            ("email_config", "smtp"),
            ("mailgun_config", "mailgun"),
            ("smtp_config", "smtp"),
        ];
        for (key, default_provider) in candidates {
            if let Some(mut v) = row(
                pool,
                "SELECT value FROM tenant_settings WHERE tenant_id = $1 AND key = $2",
                &[&tid.to_string(), key],
            )
            .await
            {
                // The credential is ciphertext at rest; the provider must see the plaintext
                // (an envelope sent as a password is a guaranteed 401, not a send).
                if let Err(e) = open_config_secrets(pool, &mut v).await {
                    tracing::error!(error = %e, key, "tenant email config credential cannot be opened — skipped");
                    continue;
                }
                let cfg = EmailConfig::from_json(&v, default_provider);
                if cfg.is_configured() {
                    return Some(cfg);
                }
            }
        }
    }

    if let Some(mut v) = row(
        pool,
        "SELECT value FROM admin_settings WHERE key = 'email'",
        &[],
    )
    .await
    {
        if let Err(e) = open_config_secrets(pool, &mut v).await {
            tracing::error!(
                error = %e,
                "admin_settings.email credential cannot be opened — treating system mail as \
                 unconfigured (PROVIDER_KEY_ENC_SECRET mismatch?)"
            );
            return None;
        }
        let cfg = EmailConfig::from_json(&v, "smtp");
        if cfg.is_configured() {
            return Some(cfg);
        }
    }

    None
}

/// Gate the DESTINATIONS inside a candidate email-config JSON at WRITE time, so an endpoint that
/// would be refused at send time is refused where it is entered instead of being quietly stored
/// (kanban t_f3c75b2a). The two writers of these rows — the admin email-settings route and the
/// tenant `PUT /api/v1/settings` route — both call this. Secrets in the body are not touched.
pub async fn gate_config_json(
    pool: &PgPool,
    cfg: &Value,
    default_provider: &str,
) -> Result<(), String> {
    let c = EmailConfig::from_json(cfg, default_provider);
    if !c.api_url.is_empty() {
        crate::security::webhook_security::gate_provider_endpoint(pool, &c.provider, &c.api_url)
            .await?;
    }
    if c.provider == "smtp" && !c.smtp_host.is_empty() {
        crate::security::webhook_security::gate_provider_endpoint_host(pool, "smtp", &c.smtp_host)
            .await?;
    }
    Ok(())
}

/// Deliver a message through the configured provider.
///
/// The destination is admin- or TENANT-settable (`admin_settings.email`, or a tenant's own
/// `tenant_settings.email_config` via `PUT /api/v1/settings`), so it passes the provider-endpoint
/// gate before the first request is made (kanban t_f3c75b2a): a private/reserved destination is
/// refused — the only exception is the platform's own preset endpoint for that provider.
pub async fn deliver(
    pool: &PgPool,
    cfg: &EmailConfig,
    to: &str,
    subject: &str,
    text: &str,
    html: Option<&str>,
) -> Result<(), String> {
    if !cfg.api_url.is_empty() {
        crate::security::webhook_security::gate_provider_endpoint(
            pool,
            &cfg.provider,
            &cfg.api_url,
        )
        .await
        .map_err(|reason| format!("Email endpoint refused by security policy: {}", reason))?;
    }
    if cfg.provider == "smtp" && !cfg.smtp_host.is_empty() {
        crate::security::webhook_security::gate_provider_endpoint_host(
            pool,
            "smtp",
            &cfg.smtp_host,
        )
        .await
        .map_err(|reason| format!("Email host refused by security policy: {}", reason))?;
    }

    match cfg.provider.as_str() {
        "smtp" => {
            let sc = crate::smtp::SmtpConfig {
                host: cfg.smtp_host.clone(),
                port: cfg.smtp_port,
                username: cfg.smtp_username.clone(),
                password: cfg.smtp_password.clone(),
                from_email: cfg.from_address.clone(),
                from_name: Some(cfg.from_name.clone()),
                encryption: if cfg.smtp_encryption.is_empty() {
                    None
                } else {
                    Some(cfg.smtp_encryption.clone())
                },
            };
            crate::smtp::send_via_smtp(&sc, to, subject, text, html).await
        }
        "sendgrid" => send_sendgrid(cfg, to, subject, text, html).await,
        "sendiio" => send_sendiio(cfg, to, subject, text, html).await,
        // "mailgun" — and any unknown value, so a pre-provider row keeps working.
        _ => send_mailgun(cfg, to, subject, text, html).await,
    }
}

async fn send_mailgun(
    cfg: &EmailConfig,
    to: &str,
    subject: &str,
    text: &str,
    html: Option<&str>,
) -> Result<(), String> {
    let url = if !cfg.api_url.is_empty() {
        cfg.api_url.clone()
    } else if !cfg.domain.is_empty() {
        format!("https://api.mailgun.net/v3/{}/messages", cfg.domain)
    } else {
        return Err("Mailgun api_url/domain not configured".to_string());
    };

    let from = cfg.sender();
    let mut params: Vec<(&str, String)> = vec![
        ("from", from),
        ("to", to.to_string()),
        ("subject", subject.to_string()),
        ("text", text.to_string()),
    ];
    if let Some(h) = html.filter(|h| !h.is_empty()) {
        params.push(("html", h.to_string()));
    }

    // Never `reqwest::Client::new()`: it follows up to 10 redirects, so a public endpoint that
    // answers `302 Location: http://127.0.0.1:…` would be a free hop past the gate above.
    let Some(client) = crate::security::webhook_security::delivery_client() else {
        return Err("outbound email client unavailable — send skipped".to_string());
    };
    let resp = client
        .post(&url)
        .basic_auth("api", Some(&cfg.api_key))
        .form(&params)
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| format!("Mailgun request failed: {}", e))?;

    if resp.status().is_success() {
        Ok(())
    } else {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        Err(format!("Mailgun returned {}: {}", status, body))
    }
}

async fn send_sendgrid(
    cfg: &EmailConfig,
    to: &str,
    subject: &str,
    text: &str,
    html: Option<&str>,
) -> Result<(), String> {
    let url = if cfg.api_url.is_empty() {
        "https://api.sendgrid.com/v3/mail/send".to_string()
    } else {
        cfg.api_url.clone()
    };

    let mut content = vec![json!({ "type": "text/plain", "value": text })];
    if let Some(h) = html.filter(|h| !h.is_empty()) {
        content.push(json!({ "type": "text/html", "value": h }));
    }

    let payload = json!({
        "personalizations": [{ "to": [{ "email": to }] }],
        "from": { "email": cfg.from_address, "name": cfg.from_name },
        "subject": subject,
        "content": content,
    });

    // Never `reqwest::Client::new()`: it follows up to 10 redirects, so a public endpoint that
    // answers `302 Location: http://127.0.0.1:…` would be a free hop past the gate above.
    let Some(client) = crate::security::webhook_security::delivery_client() else {
        return Err("outbound email client unavailable — send skipped".to_string());
    };
    let resp = client
        .post(&url)
        .bearer_auth(&cfg.api_key)
        .json(&payload)
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| format!("SendGrid request failed: {}", e))?;

    if resp.status().is_success() {
        Ok(())
    } else {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        Err(format!("SendGrid returned {}: {}", status, body))
    }
}

async fn send_sendiio(
    cfg: &EmailConfig,
    to: &str,
    subject: &str,
    text: &str,
    html: Option<&str>,
) -> Result<(), String> {
    let url = if cfg.api_url.is_empty() {
        "https://sendiio.com/api/v1/smtp/send".to_string()
    } else {
        cfg.api_url.clone()
    };

    let payload = json!({
        "api_key": cfg.api_key,
        "from_email": cfg.from_address,
        "from_name": cfg.from_name,
        "to_email": to,
        "subject": subject,
        "text": text,
        "html": html.unwrap_or(""),
    });

    // Never `reqwest::Client::new()`: it follows up to 10 redirects, so a public endpoint that
    // answers `302 Location: http://127.0.0.1:…` would be a free hop past the gate above.
    let Some(client) = crate::security::webhook_security::delivery_client() else {
        return Err("outbound email client unavailable — send skipped".to_string());
    };
    let resp = client
        .post(&url)
        .bearer_auth(&cfg.api_key)
        .json(&payload)
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| format!("Sendiio request failed: {}", e))?;

    if resp.status().is_success() {
        Ok(())
    } else {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        Err(format!("Sendiio returned {}: {}", status, body))
    }
}

/// Providers the admin can pick — served to the admin UI so the dropdown is not
/// hardcoded in the browser either.
pub fn available() -> Vec<Value> {
    vec![
        json!({"value":"smtp","label":"SMTP (any mail server)"}),
        json!({"value":"mailgun","label":"Mailgun"}),
        json!({"value":"sendgrid","label":"SendGrid"}),
        json!({"value":"sendiio","label":"Sendiio"}),
    ]
}
