//! SMTP email provider — send via standard SMTP with TLS.
//! No inbound webhook support (SMTP is send-only via this provider).
//! Inbound for SMTP domains would use IMAP polling (not yet implemented).

use crate::private_email::providers::{
    EmailProvider, InboundEmail, InlineImage, ProviderConfig, SendResult,
};
use async_trait::async_trait;
use lettre::{
    message::{
        header::{ContentDisposition, ContentId, ContentType},
        MultiPart, SinglePart,
    },
    transport::smtp::authentication::Credentials,
    AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
};

pub struct SmtpProvider;

#[async_trait]
impl EmailProvider for SmtpProvider {
    fn name(&self) -> &'static str {
        "smtp"
    }

    #[allow(clippy::too_many_arguments)]
    async fn send(
        &self,
        config: &ProviderConfig,
        from: &str,
        to: &str,
        cc: &[String],
        bcc: &[String],
        subject: &str,
        body_text: &str,
        body_html: &str,
        inline: Option<&InlineImage>,
        in_reply_to: Option<&str>,
    ) -> SendResult {
        let host = match &config.smtp_host {
            Some(h) => h.clone(),
            None => {
                return SendResult {
                    success: false,
                    provider_message_id: None,
                    error: Some("SMTP host not configured".into()),
                }
            }
        };

        let port = config.smtp_port.unwrap_or(587) as u16;
        let username = match &config.smtp_username {
            Some(u) => u.clone(),
            None => {
                return SendResult {
                    success: false,
                    provider_message_id: None,
                    error: Some("SMTP username not configured".into()),
                }
            }
        };

        let password =
            match &config.encrypted_smtp_password {
                // App-wide reader (see the module note): never fails, so empty means unreadable here.
                Some(p) => match crate::secret_box::open(config.tenant_id, p) {
                    pw if pw.trim().is_empty() => return SendResult {
                        success: false,
                        provider_message_id: None,
                        error: Some(
                            "Stored SMTP password cannot be read by this deployment — re-add the \
                             domain with a valid password"
                                .into(),
                        ),
                    },
                    pw => pw,
                },
                None => {
                    return SendResult {
                        success: false,
                        provider_message_id: None,
                        error: Some("SMTP password not configured".into()),
                    }
                }
            };

        // Build the email
        let mut msg_builder = Message::builder()
            .from(from.parse().unwrap_or_else(|_| {
                format!("{} <{}>", from.split('@').next().unwrap_or("user"), from)
                    .parse()
                    .unwrap()
            }))
            .to(to.parse().unwrap_or_else(|_| {
                format!("{} <{}>", to.split('@').next().unwrap_or("user"), to)
                    .parse()
                    .unwrap()
            }))
            .subject(subject.to_string());

        if let Some(ref reply_to) = in_reply_to {
            msg_builder = msg_builder.in_reply_to(reply_to.to_string());
        }
        for addr in cc {
            if let Ok(mailbox) = addr.parse() {
                msg_builder = msg_builder.cc(mailbox);
            }
        }
        for addr in bcc {
            if let Ok(mailbox) = addr.parse() {
                msg_builder = msg_builder.bcc(mailbox);
            }
        }

        // The body is multipart/alternative (a plain-text part and the HTML part) and, when a logo
        // travels with the message, the HTML alternative is itself multipart/related with the image
        // as an inline part addressed by Content-ID. That is what makes the signature logo render
        // without a remote fetch and keep rendering in mail that was already sent.
        let text_part = SinglePart::plain(body_text.to_string());
        let html_part = SinglePart::builder()
            .header(ContentType::TEXT_HTML)
            .body(body_html.to_string());
        let alternative = match inline {
            None => MultiPart::alternative()
                .singlepart(text_part)
                .singlepart(html_part),
            Some(img) => {
                let image_part = SinglePart::builder()
                    .header(
                        ContentType::parse(&img.content_type).unwrap_or(ContentType::TEXT_PLAIN),
                    )
                    .header(ContentDisposition::inline())
                    .header(ContentId::from(format!("<{}>", img.content_id)))
                    .body(img.bytes.clone());
                MultiPart::alternative().singlepart(text_part).multipart(
                    MultiPart::related()
                        .singlepart(html_part)
                        .singlepart(image_part),
                )
            }
        };

        let msg = match msg_builder.multipart(alternative) {
            Ok(m) => m,
            Err(e) => {
                return SendResult {
                    success: false,
                    provider_message_id: None,
                    error: Some(format!("Failed to build email: {}", e)),
                }
            }
        };

        // Build transport
        let creds = Credentials::new(username, password);
        // `smtp_tls` is the customer's own choice on the domain form: on = STARTTLS, off = a plain
        // relay (an internal mail server, or a local sink). The off-arm used to call lettre's
        // `relay()`, which is NOT plain — it wraps the socket in TLS (SUBMISSIONS_PORT, 465) — so a
        // domain saved as "no TLS" still opened a TLS handshake and died with
        // `ssl3_get_record:wrong version number` against any plain server (measured 2026-11-11 on
        // the proof sink). `builder_dangerous` is lettre's actual no-TLS builder; the port comes
        // from the domain row either way.
        let builder = if config.smtp_tls {
            match AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&host) {
                Ok(b) => b,
                Err(e) => {
                    return SendResult {
                        success: false,
                        provider_message_id: None,
                        error: Some(format!("Failed to build SMTP transport: {}", e)),
                    }
                }
            }
        } else {
            AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&host)
        };

        let transport = builder.port(port).credentials(creds).build();

        match transport.send(msg).await {
            Ok(response) => {
                let msg_id = response.message().collect::<Vec<_>>().join(" ");
                SendResult {
                    success: true,
                    provider_message_id: if msg_id.is_empty() {
                        None
                    } else {
                        Some(msg_id)
                    },
                    error: None,
                }
            }
            Err(e) => SendResult {
                success: false,
                provider_message_id: None,
                error: Some(format!("SMTP send failed: {}", e)),
            },
        }
    }

    fn accept_inbound(&self, _config: &ProviderConfig, _body: &[u8]) -> Option<InboundEmail> {
        // SMTP provider doesn't support webhook inbound.
        // Inbound would come via IMAP polling (future feature).
        None
    }
}
