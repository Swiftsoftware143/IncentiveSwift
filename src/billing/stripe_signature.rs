//! Stripe's `Stripe-Signature` header — ONE parse, shared by every Stripe receiver in this app.
//!
//! What a delivery actually carries (kanban t_1b8d96f6)
//! ---------------------------------------------------
//! Stripe sends
//!
//! ```text
//! Stripe-Signature: t=1492774577,v1=5257a869...,v1=<older, while a secret is rotated>
//! ```
//!
//! and the HMAC-SHA256 it signs is over `"{t}.{raw body}"` under the endpoint's signing secret.
//! The `t=` stamp is PART OF THE SIGNED BYTES, not decoration: a verifier that HMACs the bare body
//! cannot accept a genuine delivery (the bytes do not match), and a verifier that accepts a header
//! with no `t=` is not comparing the bytes Stripe signed.
//!
//! Both receivers used to read the header as if it were nothing but a bare `v1=<hex>` value — the
//! sibling payment receiver did `strip_prefix("v1=")` and its own unit test asserted that
//! `t=1,v0=deadbeef` is refused — so EVERY genuine Stripe delivery was answered `400 Missing v1=
//! prefix in signature header`. Neither door could accept a real delivery. Both doors now call this
//! one parse and this one comparison, so they cannot disagree about the header or about the bytes.
//!
//! The verdict is per credential (the receiver's own loop), never per header: see
//! [`verify_stripe_signature_any`].

use crate::billing::providers::WebhookSecretRow;
use crate::error::AppError;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use uuid::Uuid;

/// The parts of a `Stripe-Signature` header that decide a delivery.
#[derive(Debug, Default)]
pub struct StripeSignature<'a> {
    /// The `t=` value VERBATIM. It is part of the signed bytes, so it is never parsed to an int or
    /// re-formatted: any change to these characters would change the HMAC input.
    pub timestamp: Option<&'a str>,
    /// Every `v1=` value. Stripe sends more than one while a signing secret is being rotated, and
    /// the delivery is authentic when ANY of them matches.
    pub v1: Vec<&'a str>,
}

impl<'a> StripeSignature<'a> {
    /// Parse a `Stripe-Signature` header. Unknown parts (`v0=`, future schemes) are ignored.
    pub fn parse(header: &'a str) -> Self {
        let mut parsed = StripeSignature::default();
        for part in header.split(',') {
            let part = part.trim();
            if let Some(value) = part.strip_prefix("t=") {
                if parsed.timestamp.is_none() {
                    parsed.timestamp = Some(value);
                }
            } else if let Some(value) = part.strip_prefix("v1=") {
                parsed.v1.push(value);
            }
        }
        parsed
    }

    /// The exact bytes Stripe signs: `"{t}.{body}"`.
    ///
    /// `None` when the header carries no `t=`: there is then no signed byte string this app can
    /// reproduce, so the delivery cannot be verified — refusing is the only honest verdict, and it
    /// is what keeps a bare `v1=` (the shape the old code demanded, which Stripe never sends) from
    /// being accepted as if it were a signed delivery.
    pub fn signed_payload(&self, body: &str) -> Option<String> {
        self.timestamp.map(|t| format!("{}.{}", t, body))
    }
}

/// Hex HMAC-SHA256 of `payload` under `secret` — the value Stripe puts in `v1=`.
pub fn hmac_hex(payload: &str, secret: &str) -> Result<String, AppError> {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .map_err(|_| AppError::Internal("HMAC key initialization failed".to_string()))?;
    mac.update(payload.as_bytes());
    Ok(hex::encode(mac.finalize().into_bytes()))
}

/// Verify a Stripe delivery against EVERY active signing secret, and return the account whose
/// configuration authenticated it.
///
/// The endpoints are one shared URL while `payment_providers` is per-account, so a delivery is
/// accepted as soon as ANY account's own secret verifies it (kanban t_3c98f2b6). The verdict is
/// computed for every credential, so the work done does not vary with which one matched. No
/// credential matching is one refusal for every credential: `400 Invalid webhook signature`.
///
/// The returned account is the AUTHENTICATED one, never the one an event acts on — the event acts
/// on whatever its own provider identity resolves to in the database.
pub fn verify_stripe_signature_any(
    body: &str,
    header: &str,
    credentials: &[WebhookSecretRow],
) -> Result<Uuid, AppError> {
    let parts = StripeSignature::parse(header);

    let signed = parts.signed_payload(body).ok_or_else(|| {
        AppError::BadRequest("Missing t= timestamp in signature header".to_string())
    })?;

    if parts.v1.is_empty() {
        return Err(AppError::BadRequest(
            "Missing v1= signature in header".to_string(),
        ));
    }

    let mut verified_for: Option<Uuid> = None;
    for credential in credentials {
        let computed = hmac_hex(&signed, &credential.secret)?;
        if parts.v1.iter().any(|value| *value == computed) && verified_for.is_none() {
            verified_for = Some(credential.account_id);
        }
    }

    verified_for.ok_or_else(|| AppError::BadRequest("Invalid webhook signature".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credential(secret: &str) -> WebhookSecretRow {
        WebhookSecretRow {
            account_id: Uuid::new_v4(),
            secret: secret.to_string(),
        }
    }

    /// A genuine Stripe header: the signature is over `"{t}.{body}"` and the stamp is in the header.
    fn genuine(payload: &str, secret: &str) -> String {
        let stamp = "1492774577";
        format!(
            "t={},v1={}",
            stamp,
            hmac_hex(&format!("{}.{}", stamp, payload), secret).unwrap()
        )
    }

    #[test]
    fn parses_a_genuine_stripe_header() {
        let parts = StripeSignature::parse("t=1492774577,v1=aaa,v1=bbb");
        assert_eq!(parts.timestamp, Some("1492774577"));
        assert_eq!(parts.v1, vec!["aaa", "bbb"]);
        assert_eq!(
            parts.signed_payload("{}"),
            Some("1492774577.{}".to_string())
        );

        // No stamp -> no reproducible signed bytes, whatever else the header carries.
        assert_eq!(StripeSignature::parse("v1=aaa").signed_payload("{}"), None);
    }

    #[test]
    fn accepts_a_genuine_delivery_and_names_the_authenticating_account() {
        let payload = r#"{"id":"evt_1","type":"checkout.session.completed"}"#;
        let a = credential("whsec_account_a");
        let credentials = vec![a.clone(), credential("whsec_account_b")];

        assert_eq!(
            verify_stripe_signature_any(payload, &genuine(payload, &a.secret), &credentials)
                .expect("a genuine delivery must verify"),
            a.account_id
        );
    }

    #[test]
    fn accepts_any_of_the_rotating_v1_values() {
        let payload = r#"{"id":"evt_rotate"}"#;
        let a = credential("whsec_account_a");
        let credentials = vec![a.clone()];
        let stamp = "1492774577";
        let fresh = hmac_hex(&format!("{}.{}", stamp, payload), &a.secret).unwrap();
        // The older value is listed first, the current one second.
        let header = format!("t={},v1=deadbeef,v1={}", stamp, fresh);
        assert_eq!(
            verify_stripe_signature_any(payload, &header, &credentials).unwrap(),
            a.account_id
        );
    }

    #[test]
    fn refuses_a_signature_over_the_bare_body() {
        // The shape the old code demanded (and Stripe never sends): HMAC over the raw body only.
        let payload = r#"{"id":"evt_2"}"#;
        let a = credential("whsec_account_a");
        let credentials = vec![a.clone()];
        let bare = hmac_hex(payload, &a.secret).unwrap();
        let err = verify_stripe_signature_any(
            payload,
            &format!("t=1492774577,v1={}", bare),
            &credentials,
        )
        .expect_err("a signature that is not over {t}.{body} must not verify");
        assert!(matches!(err, AppError::BadRequest(ref m) if m == "Invalid webhook signature"));
    }

    #[test]
    fn refuses_a_stamp_only_header() {
        // `t=` with no v1= is not a signature at all.
        let credentials = vec![credential("whsec_account_a")];
        let err = verify_stripe_signature_any("{}", "t=1492774577", &credentials)
            .expect_err("a header with no v1= is not a signature");
        assert!(matches!(err, AppError::BadRequest(ref m) if m.starts_with("Missing v1=")));

        // …and a header with no t= is refused too: the stamp is part of the signed bytes.
        let err = verify_stripe_signature_any("{}", "v1=deadbeef", &credentials)
            .expect_err("a header with no t= cannot reproduce the signed bytes");
        assert!(matches!(err, AppError::BadRequest(ref m) if m.starts_with("Missing t=")));
    }

    #[test]
    fn refuses_a_signature_no_active_row_signed() {
        let payload = r#"{"id":"evt_3"}"#;
        let credentials = vec![credential("whsec_account_a")];
        let err = verify_stripe_signature_any(
            payload,
            &genuine(payload, "whsec_rotated_away"),
            &credentials,
        )
        .expect_err("a secret no active row holds must not verify");
        assert!(matches!(err, AppError::BadRequest(ref m) if m == "Invalid webhook signature"));
    }
}
