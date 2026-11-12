//! SendGrid Inbound Parse — the SECOND inbound receiver (kanban t_04db9228).
//!
//! Why this exists: sending has been provider-agnostic for a while (`mailgun` over its API, `smtp`
//! for Amazon SES / SendGrid / Postmark / Google Workspace / a host's own server), but RECEIVING
//! had exactly one door — `POST /api/v1/webhooks/mailgun/inbound` — so a customer who wanted to
//! keep their own sender was still told to move to Mailgun if they wanted an inbox with us. This is
//! the smallest second receiver that removes that objection: SendGrid Inbound Parse posts the parsed
//! message to a URL the customer configures, in the same shape Mailgun's webhook already parses
//! (sender, recipient, subject, text, html, Message-Id).
//!
//! ## The credential
//! SendGrid Inbound Parse signs NOTHING — there is no HMAC, no shared secret, no signature header,
//! and a "Send Raw" post is still unsigned. So the only credential the delivery can carry is the
//! URL it was told to POST to. Each domain therefore gets its own unguessable path segment:
//!
//! ```text
//! POST /api/v1/webhooks/sendgrid/inbound/<domain's inbound_token>
//! ```
//!
//! The handler resolves the domain BY that token (a unique index), so a delivery to an unknown or
//! wrong token finds no domain and is refused before a single row is written. That is the same
//! shape this app already uses for `POST /api/webhook/{token}/{action}`, where the token IS the
//! credential — and it is strictly stronger than the alternative SendGrid offers (nothing): a
//! Mailgun-style signature cannot be conjured for a provider that does not sign.
//!
//! ## What arrives
//! `multipart/form-data` with text fields `from`, `to`, `subject`, `text`, `html`, `envelope`
//! (JSON `{"to":[...],"from":"..."}`), `headers` (the raw header block), `attachment-count`, and
//! one file part per attachment. Attachments are COUNTED, not stored: the message body is what the
//! inbox renders, and the file bytes are ignored exactly as the Mailgun receiver ignores them.

use crate::private_email::providers::{InboundEmail, ProviderConfig};

/// One Inbound Parse delivery, reduced to the text fields worth keeping.
#[derive(Debug, Clone, Default)]
pub struct SendGridInbound {
    fields: Vec<(String, String)>,
    /// How many file parts arrived. A number, never the bytes: the inbox stores the message, and the
    /// receiver must not buffer an attachment it will not keep.
    pub attachment_parts: u32,
}

impl SendGridInbound {
    pub fn new(fields: Vec<(String, String)>, attachment_parts: u32) -> Self {
        Self {
            fields,
            attachment_parts,
        }
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
    }

    /// `true` when SendGrid's own spam check (enabled per account) scored the message as spam.
    ///
    /// Filed in Spam, never dropped: the message is stored either way, which is the same contract
    /// the Mailgun receiver keeps with its S-flag. A delivery that cannot be scored (the field is
    /// absent on accounts without spam checking) is not spam — never invent a verdict.
    pub fn is_spam(&self) -> bool {
        let raw = match self.get("spam_score").or_else(|| self.get("spam-score")) {
            Some(v) => v.trim().trim_matches('"'),
            None => return false,
        };
        // SendGrid sends either a bare number ("0.0") or a JSON object ({"score": 5.2}). Both are
        // accepted; anything unparseable is NOT spam, because a fabricated verdict filed real mail
        // in the spam folder.
        let score = match raw.parse::<f64>().ok().or_else(|| {
            serde_json::from_str::<serde_json::Value>(raw)
                .ok()
                .and_then(|v| v.get("score").and_then(|s| s.as_f64()))
        }) {
            Some(score) => score,
            None => return false,
        };
        score >= 5.0
    }

    pub fn has_attachments(&self) -> bool {
        if self.attachment_parts > 0 {
            return true;
        }
        self.get("attachment-count")
            .or_else(|| self.get("attachments"))
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(0)
            > 0
    }

    /// The message, normalized into the shape `messages_handler::store_inbound` takes.
    ///
    /// `None` when the delivery does not name a sender and a recipient — the one thing a stored
    /// message cannot be without. The token has already proven this delivery belongs to a real
    /// domain of ours by the time this is called; this is a shape check, not the credential check.
    pub fn accept(&self, config: &ProviderConfig) -> Option<InboundEmail> {
        let (from_raw, to_raw) = self.envelope_or_headers();
        let from = extract_email_address(&from_raw);
        let to = extract_email_address(&to_raw);
        if from.is_empty() || to.is_empty() {
            return None;
        }

        let body_plain = self
            .get("text")
            .filter(|t| !t.trim().is_empty())
            .map(|t| t.to_string())
            .unwrap_or_default();
        let body_html = self
            .get("html")
            .filter(|h| !h.trim().is_empty())
            .map(|h| h.to_string());
        if body_plain.is_empty() && body_html.is_none() {
            // A message with no body at all is still a message (a header-only notification), so this
            // is not a refusal — it stores with an empty body rather than being dropped.
            tracing::debug!(
                tenant = %config.tenant_id,
                "SendGrid inbound delivery carries no text and no html part"
            );
        }

        let message_id = self
            .header("Message-Id")
            .or_else(|| self.get("message-id").map(str::to_string));
        let in_reply_to = self
            .header("In-Reply-To")
            .or_else(|| self.get("in-reply-to").map(str::to_string));

        Some(InboundEmail {
            from,
            to,
            subject: self.get("subject").unwrap_or_default().to_string(),
            body_plain,
            body_html,
            provider_message_id: message_id.clone(),
            message_id,
            in_reply_to,
        })
    }

    /// `envelope` is SendGrid's own JSON of the SMTP conversation — the most reliable source, because
    /// `from`/`to` are the display headers and may carry a name or several recipients. Falls back to
    /// those headers when the envelope is absent or unparseable.
    fn envelope_or_headers(&self) -> (String, String) {
        if let Some(raw) = self.get("envelope") {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(raw.trim()) {
                let from = v
                    .get("from")
                    .and_then(|f| f.as_str())
                    .unwrap_or_default()
                    .to_string();
                let to = v
                    .get("to")
                    .and_then(|t| t.as_array())
                    .and_then(|a| a.first())
                    .and_then(|t| t.as_str())
                    .or_else(|| v.get("to").and_then(|t| t.as_str()))
                    .unwrap_or_default()
                    .to_string();
                if !from.is_empty() && !to.is_empty() {
                    return (from, to);
                }
            }
        }
        (
            self.get("from").unwrap_or_default().to_string(),
            self.get("to").unwrap_or_default().to_string(),
        )
    }

    /// One header out of the raw block SendGrid forwards, tolerating folded continuation lines.
    fn header(&self, name: &str) -> Option<String> {
        let block = self.get("headers")?;
        let mut out: Option<String> = None;
        for line in block.lines() {
            if line.starts_with(' ') || line.starts_with('\t') {
                if let Some(v) = out.as_mut() {
                    v.push(' ');
                    v.push_str(line.trim());
                }
                continue;
            }
            // A new header line: the one being collected is complete.
            if out.is_some() {
                break;
            }
            if let Some((k, v)) = line.split_once(':') {
                if k.trim().eq_ignore_ascii_case(name) {
                    out = Some(v.trim().to_string());
                }
            }
        }
        out.map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
    }
}

/// Extract bare email address from `Name <email>` format, or the raw string.
fn extract_email_address(raw: &str) -> String {
    if let Some(start) = raw.find('<') {
        if let Some(end) = raw[start..].find('>') {
            return raw[start + 1..start + end].trim().to_lowercase();
        }
    }
    raw.trim().to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn config() -> ProviderConfig {
        ProviderConfig {
            tenant_id: Uuid::nil(),
            domain_id: Uuid::nil(),
            domain: "example.com".into(),
            provider_type: "smtp".into(),
            encrypted_api_key: None,
            region: None,
            smtp_host: Some("smtp.example.com".into()),
            smtp_port: Some(587),
            smtp_username: None,
            encrypted_smtp_password: None,
            smtp_tls: true,
            inbound_mode: "webhook".into(),
            encrypted_webhook_key: None,
            inbound_provider: "sendgrid".into(),
        }
    }

    fn parsed(pairs: &[(&str, &str)]) -> SendGridInbound {
        SendGridInbound::new(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            0,
        )
    }

    #[test]
    fn the_envelope_wins_over_the_headers() {
        let p = parsed(&[
            ("from", "\"A Customer\" <Customer@Example.COM>"),
            ("to", "info@example.com"),
            (
                "envelope",
                r#"{"to":["support@example.com"],"from":"customer@example.com"}"#,
            ),
            ("subject", "Where is my order"),
            ("text", "Hello"),
        ]);
        let m = p.accept(&config()).expect("a complete delivery parses");
        assert_eq!(m.from, "customer@example.com");
        assert_eq!(m.to, "support@example.com");
        assert_eq!(m.subject, "Where is my order");
        assert_eq!(m.body_plain, "Hello");
        assert!(m.body_html.is_none());
    }

    #[test]
    fn the_display_headers_are_the_fallback() {
        let p = parsed(&[
            ("from", "\"A Customer\" <Customer@Example.COM>"),
            ("to", "info@example.com"),
            ("subject", "hi"),
        ]);
        let m = p
            .accept(&config())
            .expect("parses from the display headers");
        assert_eq!(m.from, "customer@example.com");
        assert_eq!(m.to, "info@example.com");
    }

    #[test]
    fn a_delivery_without_a_sender_or_recipient_is_refused() {
        let p = parsed(&[("subject", "hi"), ("text", "hello")]);
        assert!(p.accept(&config()).is_none());
    }

    #[test]
    fn message_id_and_reply_threading_come_out_of_the_header_block() {
        let p = parsed(&[
            ("from", "a@example.com"),
            ("to", "b@example.com"),
            (
                "headers",
                "MIME-Version: 1.0\r\nMessage-Id: <abc123@mail.example.com>\r\nIn-Reply-To: <parent@x>\r\nSubject: ignored\r\n",
            ),
        ]);
        let m = p.accept(&config()).expect("parses");
        assert_eq!(m.message_id.as_deref(), Some("<abc123@mail.example.com>"));
        assert_eq!(m.in_reply_to.as_deref(), Some("<parent@x>"));
        assert_eq!(
            m.provider_message_id.as_deref(),
            Some("<abc123@mail.example.com>")
        );
    }

    #[test]
    fn a_folded_header_keeps_its_continuation() {
        let p = parsed(&[
            ("from", "a@example.com"),
            ("to", "b@example.com"),
            ("headers", "Message-Id: <part1\r\n .part2@x>\r\n"),
        ]);
        let m = p.accept(&config()).expect("parses");
        assert_eq!(m.message_id.as_deref(), Some("<part1 .part2@x>"));
    }

    #[test]
    fn spam_is_the_providers_score_not_a_guess() {
        // Absent score is NOT spam: a fabricated verdict would file real mail in the spam folder.
        assert!(!parsed(&[("subject", "x")]).is_spam());
        assert!(!parsed(&[("spam_score", "1.2")]).is_spam());
        assert!(parsed(&[("spam_score", "8.4")]).is_spam());
        assert!(parsed(&[("spam_score", r#"{"score": 6.0}"#)]).is_spam());
        assert!(!parsed(&[("spam_score", "not-a-number")]).is_spam());
    }

    #[test]
    fn attachments_are_counted_not_stored() {
        assert!(!parsed(&[("subject", "x")]).has_attachments());
        assert!(parsed(&[("attachment-count", "2")]).has_attachments());
        assert!(SendGridInbound::new(vec![], 1).has_attachments());
    }
}
