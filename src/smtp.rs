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
        .to(to.parse().map_err(|e| {
            format!(
                "{} {}",
                crate::delivery::sender::INVALID_RECIPIENT_PREFIX,
                e
            )
        })?)
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

    // Credentials are attached ONLY when a username is actually configured.
    //
    // Measured 2026-10-01: with an empty username the transport still NEGOTIATES auth, and a relay that
    // wants none answers "No compatible authentication mechanism was found" — which is exactly how the
    // credential email died the first time this path was exercised end to end. An unauthenticated relay
    // is an ordinary mail server (an internal one, or a local sink), so asking it to authenticate makes
    // the panel's "SMTP (any mail server)" label false. Same fix as FunnelSwift's smtp.rs.
    let has_auth = !config.username.trim().is_empty();
    let creds = Credentials::new(config.username.clone(), config.password.clone());

    let base = match config.encryption.as_deref() {
        Some("tls") => AsyncSmtpTransport::<Tokio1Executor>::relay(&config.host)
            .map_err(|e| format!("Failed to create TLS SMTP transport: {}", e))?,
        // PLAIN SMTP, no TLS at all. Added 2026-10-01 for the same reason FunnelSwift needed it: the
        // panel offers this field as "SMTP (any mail server)", and an internal relay — or a local
        // sink — without TLS is a perfectly ordinary mail server. Refusing it made that label false,
        // and it left the credential proof unable to capture what the app actually sent.
        Some("none") => AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&config.host),
        _ => {
            // STARTTLS (default)
            AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&config.host)
                .map_err(|e| format!("Failed to create STARTTLS SMTP transport: {}", e))?
        }
    };

    // Every arm above yields the same builder type, so the port and the OPTIONAL credentials are
    // applied once, here.
    let base = base.port(config.port);
    let base = if has_auth {
        base.credentials(creds)
    } else {
        base
    };
    let mailer = base.build();

    mailer
        .send(email)
        .await
        .map_err(|e| format!("Failed to send email: {}", e))?;

    Ok(())
}
