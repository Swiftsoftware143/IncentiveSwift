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
            if let Some(v) = row(
                pool,
                "SELECT value FROM tenant_settings WHERE tenant_id = $1 AND key = $2",
                &[&tid.to_string(), key],
            )
            .await
            {
                let cfg = EmailConfig::from_json(&v, default_provider);
                if cfg.is_configured() {
                    return Some(cfg);
                }
            }
        }
    }

    if let Some(v) = row(
        pool,
        "SELECT value FROM admin_settings WHERE key = 'email'",
        &[],
    )
    .await
    {
        let cfg = EmailConfig::from_json(&v, "smtp");
        if cfg.is_configured() {
            return Some(cfg);
        }
    }

    None
}

/// Deliver a message through the configured provider.
pub async fn deliver(
    cfg: &EmailConfig,
    to: &str,
    subject: &str,
    text: &str,
    html: Option<&str>,
) -> Result<(), String> {
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

    let resp = reqwest::Client::new()
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

    let resp = reqwest::Client::new()
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

    let resp = reqwest::Client::new()
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
