// SMTP sender — credentials come from the DB-backed email provider config
// (Admin > Settings > Email Provider). Nothing here reads the process environment.

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub from_email: String,
    pub from_name: Option<String>,
    pub encryption: Option<String>, // "tls" or "starttls"
}

/// Send an email via SMTP using the provided config. Returns Ok(()) on success.
pub async fn send_via_smtp(
    config: &SmtpConfig,
    to: &str,
    subject: &str,
    body: &str,
    html: Option<&str>,
) -> Result<(), String> {
    use lettre::message::{MultiPart, SinglePart};
    use lettre::{
        transport::smtp::authentication::Credentials, AsyncSmtpTransport, AsyncTransport, Message,
        Tokio1Executor,
    };

    let from_name = config.from_name.as_deref().unwrap_or("IncentiveSwift");
    let from_addr = format!("{} <{}>", from_name, config.from_email);

    let builder = Message::builder()
        .from(
            from_addr
                .parse()
                .map_err(|e| format!("Invalid from address: {}", e))?,
        )
        .to(to
            .parse()
            .map_err(|e| format!("Invalid to address: {}", e))?)
        .subject(subject);

    let email = match html.filter(|h| !h.is_empty()) {
        Some(h) => builder
            .multipart(
                MultiPart::alternative()
                    .singlepart(SinglePart::plain(body.to_string()))
                    .singlepart(SinglePart::html(h.to_string())),
            )
            .map_err(|e| format!("Failed to build email: {}", e))?,
        None => builder
            .multipart(MultiPart::alternative().singlepart(SinglePart::plain(body.to_string())))
            .map_err(|e| format!("Failed to build email: {}", e))?,
    };

    let creds = Credentials::new(config.username.clone(), config.password.clone());

    let mailer = match config.encryption.as_deref() {
        Some("tls") => AsyncSmtpTransport::<Tokio1Executor>::relay(&config.host)
            .map_err(|e| format!("Failed to create TLS SMTP transport: {}", e))?
            .port(config.port)
            .credentials(creds)
            .build(),
        _ => {
            // STARTTLS (default)
            AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&config.host)
                .map_err(|e| format!("Failed to create STARTTLS SMTP transport: {}", e))?
                .port(config.port)
                .credentials(creds)
                .build()
        }
    };

    mailer
        .send(email)
        .await
        .map_err(|e| format!("Failed to send email: {}", e))?;

    Ok(())
}
