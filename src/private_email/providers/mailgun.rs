//! Mailgun email provider — send via REST API, receive via webhook.

use crate::private_email::providers::{
    EmailProvider, InboundEmail, InlineImage, ProviderConfig, SendResult,
};
use async_trait::async_trait;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;

pub struct MailgunProvider;

/// Assemble a `multipart/form-data` body carrying the text fields plus ONE inline image.
///
/// Hand-built rather than via reqwest's `multipart` feature so the part NAME (`inline`), the FILENAME
/// (which becomes the Content-ID) and the `Content-Type` are exactly the three things Mailgun's API
/// documents for an inline image — and so no extra crate feature is compiled in for one call site.
fn multipart_body(fields: &[(String, String)], image: &InlineImage, boundary: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, value) in fields {
        out.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n")
                .as_bytes(),
        );
        out.extend_from_slice(value.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"inline\"; filename=\"{}\"\r\n\
             Content-Type: {}\r\n\r\n",
            image.content_id, image.content_type
        )
        .as_bytes(),
    );
    out.extend_from_slice(&image.bytes);
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    out
}

#[async_trait]
impl EmailProvider for MailgunProvider {
    fn name(&self) -> &'static str {
        "mailgun"
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
        let api_key = match &config.encrypted_api_key {
            // `secret_box::open` is the app-wide reader (enc:v1:, the older prefixless ciphertext,
            // or a legacy plaintext row) and never fails, so an empty result means "this deployment
            // cannot read the stored credential". That must be REPORTED — never sent as an empty
            // Basic-auth password (t_45772522).
            Some(k) => match crate::secret_box::open(config.tenant_id, k) {
                key if key.trim().is_empty() => {
                    return SendResult {
                        success: false,
                        provider_message_id: None,
                        error: Some(format!(
                            "Stored API key for domain {} cannot be read by this deployment — \
                             re-add the domain with a valid key",
                            config.domain_id
                        )),
                    }
                }
                key => key,
            },
            None => {
                return SendResult {
                    success: false,
                    provider_message_id: None,
                    error: Some("Mailgun API key not configured".into()),
                }
            }
        };

        let base_url = if config.region.as_deref() == Some("eu") {
            "https://api.eu.mailgun.net"
        } else {
            "https://api.mailgun.net"
        };

        let mut form: Vec<(String, String)> = vec![
            ("from".into(), from.to_string()),
            ("to".into(), to.to_string()),
            ("subject".into(), subject.to_string()),
            ("text".into(), body_text.to_string()),
            ("html".into(), body_html.to_string()),
        ];

        if let Some(ref reply_to) = in_reply_to {
            form.push(("h:In-Reply-To".into(), reply_to.to_string()));
        }
        // Cc is visible to everyone; Bcc is sent as Mailgun's own `bcc` field so those addresses
        // never appear in the delivered headers.
        if !cc.is_empty() {
            form.push(("cc".into(), cc.join(",")));
        }
        if !bcc.is_empty() {
            form.push(("bcc".into(), bcc.join(",")));
        }

        let client = reqwest::Client::new();
        let url = format!("{}/v3/{}/messages", base_url, config.domain);

        // NO inline image: the form-urlencoded body is what this provider has always taken.
        // WITH one: a multipart body is required, and Mailgun publishes the part under
        // `inline` with the Content-ID derived from the FILENAME — which is why the HTML's
        // `cid:` reference and this part's filename are the same string. The body is assembled by
        // hand instead of via reqwest's `multipart` feature: it is 20 lines, needs no extra crate
        // feature, and pins the exact part name, filename and Content-Type Mailgun documents.
        let request = match inline {
            None => client
                .post(&url)
                .basic_auth("api", Some(&api_key))
                .form(&form),
            Some(img) => {
                let boundary = format!("cs-{}", Uuid::new_v4().simple());
                let body = multipart_body(&form, img, &boundary);
                client
                    .post(&url)
                    .basic_auth("api", Some(&api_key))
                    .header(
                        reqwest::header::CONTENT_TYPE,
                        format!("multipart/form-data; boundary={boundary}"),
                    )
                    .body(body)
            }
        };

        match request
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await
        {
            Ok(resp) => {
                if resp.status().is_success() {
                    // Try to extract message ID from response
                    let body = resp.text().await.unwrap_or_default();
                    let msg_id = serde_json::from_str::<serde_json::Value>(&body)
                        .ok()
                        .and_then(|v| {
                            v.get("id")
                                .and_then(|id| id.as_str())
                                .map(|s| s.to_string())
                        });
                    SendResult {
                        success: true,
                        provider_message_id: msg_id,
                        error: None,
                    }
                } else {
                    let body = resp.text().await.unwrap_or_default();
                    SendResult {
                        success: false,
                        provider_message_id: None,
                        error: Some(format!("Mailgun returned error: {}", body)),
                    }
                }
            }
            Err(e) => SendResult {
                success: false,
                provider_message_id: None,
                error: Some(format!("Mailgun request failed: {}", e)),
            },
        }
    }

    fn accept_inbound(&self, config: &ProviderConfig, body: &[u8]) -> Option<InboundEmail> {
        // Parse form-urlencoded Mailgun webhook
        let body_str = std::str::from_utf8(body).ok()?;
        let params: Vec<(String, String)> = url::form_urlencoded::parse(body_str.as_bytes())
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();

        let get = |key: &str| -> Option<String> {
            params
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
        };

        // The signature is this receiver's ONLY credential (kanban t_7f134e77): a domain with no
        // signing key configured cannot authenticate a delivery, so NOTHING is accepted from it.
        // The previous `if let Some(..)` read "no key" as "nothing to check" and returned the parsed
        // message, which made an unarmed domain an anonymous writer of contacts/tickets/events —
        // reproduced live 2026-10-11 (HTTP 200 with one row in each of the three tables, no
        // credential presented). Fail CLOSED here for the missing key, exactly as for a bad
        // signature or a key that is configured but cannot be opened by this deployment
        // (t_45772522 fixed that second half).
        let encrypted_key = match config.encrypted_webhook_key.as_deref() {
            Some(k) if !k.is_empty() => k,
            _ => {
                tracing::warn!(
                    tenant = %config.tenant_id,
                    "inbound webhook refused: no signing key is configured for this domain"
                );
                return None;
            }
        };
        let signing_key = crate::secret_box::open(config.tenant_id, encrypted_key);
        if signing_key.trim().is_empty() {
            tracing::error!(
                tenant = %config.tenant_id,
                "inbound webhook signing key cannot be read — the signature cannot be validated"
            );
            return None;
        }
        let token = get("token").unwrap_or_default();
        let timestamp = get("timestamp").unwrap_or_default();
        let signature = get("signature").unwrap_or_default();

        // Validate: HMAC-SHA256(signing_key, timestamp + token) == signature
        let mut mac = HmacSha256::new_from_slice(signing_key.as_bytes()).ok()?;
        mac.update(format!("{}{}", timestamp, token).as_bytes());
        let expected = hex::encode(mac.finalize().into_bytes());
        if !constant_time_eq(expected.as_bytes(), signature.as_bytes()) {
            return None; // Signature mismatch
        }

        let sender = get("sender").unwrap_or_default();
        let recipient = get("recipient").unwrap_or_default();
        let subject = get("subject").unwrap_or_default();
        let body_plain = get("body-plain")
            .or_else(|| get("stripped-text"))
            .unwrap_or_default();
        let body_html = get("body-html");
        let message_id = get("Message-Id");
        let in_reply_to = get("In-Reply-To");

        let from = extract_email_address(&sender);
        let to = extract_email_address(&recipient);

        if from.is_empty() || to.is_empty() {
            return None;
        }

        Some(InboundEmail {
            from,
            to,
            subject,
            body_plain,
            body_html,
            message_id,
            in_reply_to,
            provider_message_id: get("Message-Id"),
        })
    }
}

/// Extract bare email address from "Name <email>" format.
fn extract_email_address(raw: &str) -> String {
    if let Some(start) = raw.find('<') {
        if let Some(end) = raw.find('>') {
            return raw[start + 1..end].trim().to_lowercase();
        }
    }
    raw.trim().to_lowercase()
}

/// Constant-time comparison for signature validation.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b.iter()).fold(0, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image() -> InlineImage {
        InlineImage {
            content_id: "coreswift-logo.png".into(),
            content_type: "image/png".into(),
            bytes: vec![0x89, b'P', b'N', b'G', 1, 2, 3],
        }
    }

    #[test]
    fn the_inline_part_carries_the_filename_that_becomes_the_content_id() {
        let fields = vec![(
            "html".to_string(),
            "<img src=\"cid:coreswift-logo.png\">".into(),
        )];
        let body = multipart_body(&fields, &image(), "cs-boundary");
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("name=\"inline\"; filename=\"coreswift-logo.png\""));
        assert!(text.contains("Content-Type: image/png"));
        // The bytes are carried verbatim, after the part headers.
        assert!(body
            .windows(7)
            .any(|w| w == [0x89, b'P', b'N', b'G', 1, 2, 3]));
        // The text fields are present and the body is terminated.
        assert!(text.contains("name=\"html\""));
        assert!(text.trim_end().ends_with("--cs-boundary--"));
    }
}
