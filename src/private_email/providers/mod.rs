//! Email provider abstraction — Mailgun, SMTP, SendGrid (inbound), SES, Postmark.
//!
//! Each provider implements the `EmailProvider` trait for SENDING. The send layer calls the trait,
//! never a specific provider.
//!
//! ## Sending and receiving are TWO choices (kanban t_04db9228)
//! `provider_type` answers how a domain SENDS (`mailgun` over its API, `smtp` for Amazon SES /
//! SendGrid / Postmark / Google Workspace / any SMTP server). `inbound_provider` answers how it
//! RECEIVES (`mailgun` or `sendgrid` — the two receivers that exist — or `none`). They are
//! independent on purpose, so a customer never has to move their outbound mail in order to receive
//! with us.
//!
//! ## Inbound model
//! - **Push (webhook)**: the provider POSTs to one of our receivers. `mailgun` signs the delivery
//!   (HMAC-SHA256 over timestamp+token, per-domain signing key). `sendgrid` signs nothing — its
//!   Inbound Parse has no signature mechanism at all — so the per-domain token in the URL path is
//!   the credential (`providers::sendgrid`).

pub mod mailgun;
pub mod sendgrid;
pub mod smtp;

use async_trait::async_trait;
use uuid::Uuid;

/// Result of a send attempt.
#[derive(Debug)]
pub struct SendResult {
    pub success: bool,
    pub provider_message_id: Option<String>,
    pub error: Option<String>,
}

/// Parsed inbound email from any provider's webhook.
#[derive(Debug, Clone)]
pub struct InboundEmail {
    pub from: String,
    pub to: String,
    pub subject: String,
    pub body_plain: String,
    pub body_html: Option<String>,
    pub message_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub provider_message_id: Option<String>,
}

/// Configuration for a domain's email provider, loaded from the database.
#[derive(Debug, Clone)]
pub struct ProviderConfig {
    pub tenant_id: Uuid,
    pub domain_id: Uuid,
    pub domain: String,
    pub provider_type: String, // "mailgun", "smtp", "ses", "postmark"
    pub encrypted_api_key: Option<String>,
    pub region: Option<String>,
    // SMTP-specific
    pub smtp_host: Option<String>,
    pub smtp_port: Option<i32>,
    pub smtp_username: Option<String>,
    pub encrypted_smtp_password: Option<String>,
    pub smtp_tls: bool,
    // Inbound
    pub inbound_mode: String,
    pub encrypted_webhook_key: Option<String>,
    /// HOW THIS DOMAIN RECEIVES: `mailgun` | `sendgrid` | `none` (kanban t_04db9228). The receiver
    /// a delivery must be authenticated by, independent of how the domain SENDS.
    pub inbound_provider: String,
}

impl ProviderConfig {
    /// The receiver that answers for this domain, or `None` for a sending-only domain.
    ///
    /// `Some("mailgun")` / `Some("sendgrid")` name real receivers in this binary; `none` (and any
    /// value a hand-edited row could carry) means no delivery can be authenticated for it, so both
    /// receivers refuse rather than one of them silently accepting.
    pub fn receiver(&self) -> Option<&'static str> {
        match self.inbound_provider.trim().to_ascii_lowercase().as_str() {
            "mailgun" => Some("mailgun"),
            "sendgrid" => Some("sendgrid"),
            _ => None,
        }
    }
}

/// One image that belongs INSIDE the message body (kanban t_b671a0f7).
///
/// The company logo travels with the message instead of being fetched from a URL: the HTML points at
/// `cid:<content_id>` and this part carries the bytes. That is what makes "deleting or updating the
/// logo never breaks already-sent mail" true — a mailbox holding last month's message still draws
/// last month's bytes — and it renders even where remote images are blocked until the reader allows
/// them. `content_id` is also the part's FILENAME because Mailgun derives the Content-ID it publishes
/// from the filename, so both providers resolve the same `cid:` reference.
#[derive(Debug, Clone)]
pub struct InlineImage {
    pub content_id: String,
    pub content_type: String,
    pub bytes: Vec<u8>,
}

/// The unified email provider trait. All providers implement this.
#[async_trait]
#[async_trait::async_trait]
pub trait EmailProvider: Send + Sync {
    /// Human-readable name for logging/debugging.
    fn name(&self) -> &'static str;

    /// Send an outbound email. Returns result with optional provider message ID.
    ///
    /// `cc` and `bcc` are separate from `to` on purpose: folding a blind copy into the visible
    /// recipient list would publish the very addresses the sender hid. Both are honoured by every
    /// provider this app ships (Mailgun's own `cc`/`bcc` fields, lettre's `.cc()`/`.bcc()`).
    ///
    /// `body_text` is the plain-text alternative. It is not optional decoration: a message with only
    /// an `html` part scores worse with spam filters and is unreadable in a text-only client, and the
    /// old code passed the typed plain text AS the html part.
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
    ) -> SendResult;

    /// Validate and parse an inbound webhook payload into a normalized InboundEmail.
    /// Returns None if signature validation fails or the payload isn't a valid inbound message.
    fn accept_inbound(&self, config: &ProviderConfig, body: &[u8]) -> Option<InboundEmail>;
}

/// Build the correct provider from a config.
pub fn provider_for(config: &ProviderConfig) -> Box<dyn EmailProvider> {
    match config.provider_type.as_str() {
        "smtp" => Box::new(smtp::SmtpProvider),
        _ => Box::new(mailgun::MailgunProvider),
    }
}

/// The parser that answers a delivery on the MAILGUN receiver route.
///
/// Receiving is its OWN choice (kanban t_04db9228), so this must never be [`provider_for`]: that
/// picks by `provider_type`, which is how the domain SENDS. A customer who keeps Amazon SES (or any
/// SMTP service) for outbound and lets Mailgun deliver inbound — the combination the receiving
/// selector exists for — was handed the SMTP provider here, whose `accept_inbound` always answers
/// `None`; every delivery was refused and the log blamed the signing key for a key that was fine.
/// Measured live on ADASwift 2026-11-12 and ported fleet-wide (kanban t_5f71b8ff).
pub fn inbound_provider_for(_config: &ProviderConfig) -> Box<dyn EmailProvider> {
    // This route IS Mailgun's receiver: every domain that reaches it was checked by the handler
    // (`receiver()` must answer "mailgun"), and a SendGrid domain is refused with a sentence that
    // names its own door. So the parser is Mailgun's, whatever the domain uses to SEND.
    Box::new(mailgun::MailgunProvider)
}
