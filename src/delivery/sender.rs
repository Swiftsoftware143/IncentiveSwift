//! SMTP email sender — tenant-aware, reads SMTP config from tenant_settings
//!
//! Each tenant can configure their own SMTP server (host, port, username, password, from address).
//! This falls back to system-wide Mailgun SMTP if no tenant config is set.

use lettre::message::header::ContentType;
use lettre::{
    transport::smtp::authentication::Credentials, AsyncSmtpTransport, AsyncTransport, Message,
    Tokio1Executor,
};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

/// SMTP configuration for a tenant
#[derive(Debug, Clone)]
pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub from_email: String,
    pub from_name: Option<String>,
}

/// Load SMTP config for a specific tenant account
pub async fn load_smtp_config(pool: &PgPool, account_id: Uuid) -> Option<SmtpConfig> {
    let rows = sqlx::query_as::<_, (String, Value)>(
        "SELECT key, value FROM tenant_settings WHERE tenant_id = $1 AND key LIKE 'smtp_%'",
    )
    .bind(account_id)
    .fetch_all(pool)
    .await
    .ok()?;

    let mut config = std::collections::HashMap::new();
    for (key, value) in rows {
        config.insert(key, value);
    }

    let host = config.get("smtp_host")?.as_str()?.to_string();
    let username = config.get("smtp_username")?.as_str()?.to_string();
    let password = config.get("smtp_password")?.as_str()?.to_string();
    let from_email = config.get("smtp_from_email")?.as_str()?.to_string();
    let port = config
        .get("smtp_port")
        .and_then(|v| v.as_i64())
        .unwrap_or(587) as u16;
    let from_name = config
        .get("smtp_from_name")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    Some(SmtpConfig {
        host,
        port,
        username,
        password,
        from_email,
        from_name,
    })
}

/// Try to load system-level Mailgun SMTP fallback from provider_keys
pub async fn load_system_smtp_fallback(pool: &PgPool) -> Option<SmtpConfig> {
    let row = sqlx::query_as::<_, (String,)>(
        "SELECT api_key FROM provider_keys WHERE provider = 'mailgun' AND (account_id IS NULL OR scope = 'account') AND is_active = true LIMIT 1"
    )
    .fetch_optional(pool)
    .await
    .ok()?;

    let (key,) = row?;

    // Mailgun SMTP: username = 'postmaster@domain', password = API key, host = smtp.mailgun.org
    Some(SmtpConfig {
        host: "smtp.mailgun.org".to_string(),
        port: 587,
        username: "postmaster@mail.incentiveswift.com".to_string(),
        password: key,
        from_email: "notifications@mail.incentiveswift.com".to_string(),
        from_name: Some("IncentiveSwift".to_string()),
    })
}

/// Send an email using the tenant's SMTP config, falling back to system Mailgun
pub async fn send_email(
    pool: &PgPool,
    account_id: Uuid,
    to: &str,
    subject: &str,
    body_html: &str,
) -> Result<(), String> {
    // Try tenant SMTP config first, fallback to system Mailgun SMTP
    let config = match load_smtp_config(pool, account_id).await {
        Some(c) => Some(c),
        None => load_system_smtp_fallback(pool).await,
    };

    let config = config.ok_or_else(|| {
        "No SMTP configuration found. Configure SMTP in Settings or add Mailgun API key."
            .to_string()
    })?;

    // Build the email
    let from_name = config
        .from_name
        .clone()
        .unwrap_or_else(|| "IncentiveSwift".to_string());
    let email = Message::builder()
        .from(
            format!("{} <{}>", from_name, config.from_email)
                .parse()
                .map_err(|e: lettre::address::AddressError| {
                    format!("Invalid from address: {}", e)
                })?,
        )
        .to(to
            .parse()
            .map_err(|e: lettre::address::AddressError| format!("Invalid to address: {}", e))?)
        .subject(subject)
        .header(ContentType::TEXT_HTML)
        .body(body_html.to_string())
        .map_err(|e| format!("Failed to build email: {}", e))?;

    // Connect via STARTTLS
    let creds = Credentials::new(config.username.clone(), config.password.clone());

    let mailer = AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&config.host)
        .map_err(|e| format!("Invalid SMTP host: {}", e))?
        .port(config.port)
        .credentials(creds)
        .build();

    // Send
    mailer
        .send(email)
        .await
        .map_err(|e| format!("SMTP send failed: {}", e))?;

    Ok(())
}

/// Test SMTP config by sending a test email to the account owner
pub async fn test_smtp_config(
    pool: &PgPool,
    account_id: Uuid,
    to_email: &str,
) -> Result<(), String> {
    send_email(pool, account_id, to_email, "Test Email from IncentiveSwift", 
        "<h2>✅ SMTP Configuration Works!</h2><p>Your SMTP settings are correct. This email was sent using your configured SMTP server.</p><p>— IncentiveSwift</p>"
    ).await
}

/// Render {{key}} placeholders from a vars object.
pub fn render_template(template: &str, vars: &serde_json::Value) -> String {
    let mut result = template.to_string();
    if let Some(obj) = vars.as_object() {
        for (key, value) in obj {
            let placeholder = format!("{{{{{}}}}}", key);
            let replacement = match value {
                serde_json::Value::String(sv) => sv.clone(),
                other => other.to_string(),
            };
            result = result.replace(&placeholder, &replacement);
        }
    }
    result
}

/// Load an email template by type for the given account (account override first,
/// then global default), render vars, and send via the tenant's SMTP.
/// Returns Err if no template exists for that type OR no SMTP is configured.
pub async fn send_template_by_type(
    pool: &PgPool,
    account_id: Uuid,
    to: &str,
    template_type: &str,
    vars: &serde_json::Value,
) -> Result<(), String> {
    let row = sqlx::query_as::<_, (Option<String>, Option<String>, Option<String>)>(
        "SELECT subject, body, html_body FROM email_templates
         WHERE template_type = $1 AND (aid = $2 OR (aid IS NULL AND is_default = true))
         ORDER BY (aid = $2) DESC, is_default DESC, created_at DESC
         LIMIT 1",
    )
    .bind(template_type)
    .bind(account_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| format!("DB error loading template: {e}"))?
    .ok_or_else(|| format!("No email template found for type '{template_type}'"))?;

    let subject = row
        .0
        .unwrap_or_else(|| format!("IncentiveSwift: {}", template_type));
    // Prefer html_body, fall back to body
    let body = row.2.or(row.1).unwrap_or_default();
    let subject = render_template(&subject, vars);
    let body = render_template(&body, vars);

    send_email(pool, account_id, to, &subject, &body).await
}
