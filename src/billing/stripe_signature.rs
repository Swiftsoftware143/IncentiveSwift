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
//!
//! The `t=` stamp is also a CLOCK (kanban t_d9fb9e1b)
//! ------------------------------------------------
//! Signing the stamp fixes WHICH bytes were signed; it does not make them expire. A captured
//! `Stripe-Signature` header plus the body it signs is therefore a bearer credential that verifies
//! FOR EVER unless the receiver also compares the stamp against its own clock — that comparison is
//! Stripe's own replay defence, and neither receiver in this app did it. [`verify_stripe_signature_any`]
//! now measures the age of an AUTHENTICATED delivery and refuses one whose stamp is further than the
//! tolerance in force from `now`, so the arms read CONFIG -> PRESENCE -> AUTHENTICITY -> FRESHNESS:
//! an ancient stamp on a signature that does NOT verify is still `Invalid webhook signature`, and a
//! stamp that is not a readable clock is refused rather than treated as fresh.

use crate::billing::providers::WebhookSecretRow;
use crate::error::AppError;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use uuid::Uuid;

/// How far a delivery's `t=` stamp may be from this receiver's clock, in seconds, before the
/// delivery is refused. Stripe's own default, and the value used when
/// `STRIPE_WEBHOOK_TOLERANCE_SECS` is unset or unreadable.
pub const DEFAULT_STRIPE_SIGNATURE_TOLERANCE_SECS: i64 = 300;

/// The floor the configured tolerance is clamped to: below this a receiver would refuse its own
/// genuine deliveries over clock drift.
pub const MIN_STRIPE_SIGNATURE_TOLERANCE_SECS: i64 = 30;

/// The ceiling the configured tolerance is clamped to: above this the stamp stops being a replay
/// bound in any useful sense (a day).
pub const MAX_STRIPE_SIGNATURE_TOLERANCE_SECS: i64 = 86_400;

/// The refusal for an AUTHENTICATED delivery whose stamp is not a readable clock, or whose distance
/// from this receiver's clock exceeds the tolerance in force. Deliberately NOT
/// `Invalid webhook signature`: only a delivery this app has authenticated is judged on its clock,
/// so this verdict means "the signature is genuine but stale", which an operator must be able to
/// tell apart from a forgery (and from a host clock that has drifted).
pub const STALE_SIGNATURE_ERROR: &str = "Webhook signature timestamp out of tolerance";

/// This receiver's own clock, in whole seconds since the Unix epoch.
///
/// `0` when the system clock is before the epoch — every stamp is then far in the future and every
/// delivery is refused, which is the honest verdict for a host whose clock is broken (the refusal
/// logs the measured age, so that is the line an operator sees).
pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        .unwrap_or(0)
}

/// The parts of a `Stripe-Signature` header that decide a delivery.
#[derive(Debug, Default)]
pub struct StripeSignature<'a> {
    /// The `t=` value VERBATIM. It is part of the signed bytes, so it is never parsed to an int or
    /// re-formatted: any change to these characters would change the HMAC input.
    pub timestamp: Option<&'a str>,
    /// The same `t=` value read as a CLOCK — `None` when the header carries no `t=`, or carries one
    /// that is not a whole number of seconds. An unmeasurable stamp is refused (kanban t_d9fb9e1b):
    /// "no age I can measure" is never "fresh".
    pub timestamp_secs: Option<i64>,
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
                    // The verbatim slice is what the signature covers; the clock is read from the
                    // same characters. `t=1.5` or `t=nope` parses to `None` -> refused below.
                    parsed.timestamp_secs = value.parse::<i64>().ok();
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
///
/// The stamp's AGE is judged last, against this host's clock and `tolerance_secs` in force
/// (kanban t_d9fb9e1b): a delivery that verified but whose stamp is older — or further in the
/// future — than the tolerance is refused, so a captured header plus body is not a bearer
/// credential. The arms are CONFIG -> PRESENCE -> AUTHENTICITY -> FRESHNESS, so freshness can only
/// ever judge a delivery this app has authenticated.
pub fn verify_stripe_signature_any(
    body: &str,
    header: &str,
    credentials: &[WebhookSecretRow],
    tolerance_secs: i64,
) -> Result<Uuid, AppError> {
    verify_stripe_signature_any_at(body, header, credentials, unix_now(), tolerance_secs)
}

/// [`verify_stripe_signature_any`] against a clock the caller supplies.
///
/// Split out so a test can pin `now` and assert an exact age (and so a caller that has already read
/// the clock can use the SAME reading for its own logging, instead of a second read that reports an
/// age one second off what was enforced).
pub fn verify_stripe_signature_any_at(
    body: &str,
    header: &str,
    credentials: &[WebhookSecretRow],
    now: i64,
    tolerance_secs: i64,
) -> Result<Uuid, AppError> {
    let parts = StripeSignature::parse(header);

    // ── PRESENCE ────────────────────────────────────────────────────────────────────────────
    let signed = parts.signed_payload(body).ok_or_else(|| {
        AppError::BadRequest("Missing t= timestamp in signature header".to_string())
    })?;

    if parts.v1.is_empty() {
        return Err(AppError::BadRequest(
            "Missing v1= signature in header".to_string(),
        ));
    }

    // ── AUTHENTICITY ────────────────────────────────────────────────────────────────────────
    let mut verified_for: Option<Uuid> = None;
    for credential in credentials {
        let computed = hmac_hex(&signed, &credential.secret)?;
        if parts.v1.iter().any(|value| *value == computed) && verified_for.is_none() {
            verified_for = Some(credential.account_id);
        }
    }

    let account_id = verified_for
        .ok_or_else(|| AppError::BadRequest("Invalid webhook signature".to_string()))?;

    // ── FRESHNESS ───────────────────────────────────────────────────────────────────────────
    // Only now, on a delivery this app has authenticated: an ancient stamp on a signature that does
    // not verify is still `Invalid webhook signature`. `None` means the stamp is no clock at all —
    // refused, never assumed fresh.
    let age_secs = parts.timestamp_secs.map(|stamp| now.saturating_sub(stamp));
    if !matches!(age_secs, Some(age) if age.saturating_abs() <= tolerance_secs) {
        tracing::warn!(
            stamp = parts.timestamp.unwrap_or("<none>"),
            age_secs = ?age_secs,
            tolerance_secs,
            "Stripe webhook signature timestamp out of tolerance"
        );
        return Err(AppError::BadRequest(STALE_SIGNATURE_ERROR.to_string()));
    }

    Ok(account_id)
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

    /// A `t=` stamp inside any sane tolerance of this host's clock — what a live delivery carries.
    fn live_stamp() -> i64 {
        unix_now() - 1
    }

    /// A genuine Stripe header: the signature is over `"{t}.{body}"` and the stamp is in the header.
    fn genuine(payload: &str, secret: &str) -> String {
        genuine_at(payload, secret, live_stamp())
    }

    fn genuine_at(payload: &str, secret: &str, stamp: i64) -> String {
        format!(
            "t={},v1={}",
            stamp,
            hmac_hex(&format!("{}.{}", stamp, payload), secret).unwrap()
        )
    }

    /// A genuine header whose `t=` is not a whole number of seconds. Stripe never sends one, but the
    /// app must refuse it rather than treat an unreadable clock as fresh.
    fn genuine_unreadable_stamp(payload: &str, secret: &str, stamp: &str) -> String {
        format!(
            "t={},v1={}",
            stamp,
            hmac_hex(&format!("{}.{}", stamp, payload), secret).unwrap()
        )
    }

    /// Verify under the SHIPPED tolerance.
    fn verify(
        body: &str,
        header: &str,
        credentials: &[WebhookSecretRow],
    ) -> Result<Uuid, AppError> {
        verify_stripe_signature_any(
            body,
            header,
            credentials,
            DEFAULT_STRIPE_SIGNATURE_TOLERANCE_SECS,
        )
    }

    fn is_stale(err: &AppError) -> bool {
        matches!(err, AppError::BadRequest(m) if m.as_str() == STALE_SIGNATURE_ERROR)
    }

    // ── the header and the signed bytes ─────────────────────────────────────────────────────────

    #[test]
    fn parses_a_genuine_stripe_header() {
        let parts = StripeSignature::parse("t=1492774577,v1=aaa,v1=bbb");
        assert_eq!(parts.timestamp, Some("1492774577"));
        assert_eq!(parts.timestamp_secs, Some(1492774577));
        assert_eq!(parts.v1, vec!["aaa", "bbb"]);
        assert_eq!(
            parts.signed_payload("{}"),
            Some("1492774577.{}".to_string())
        );

        // No stamp -> no reproducible signed bytes, whatever else the header carries.
        assert_eq!(StripeSignature::parse("v1=aaa").signed_payload("{}"), None);
        assert_eq!(StripeSignature::parse("v1=aaa").timestamp_secs, None);

        // A stamp that is not a whole number of seconds is kept VERBATIM (it is signed) and reads as
        // no clock at all.
        let odd = StripeSignature::parse("t=1492774577.5,v1=aaa");
        assert_eq!(odd.timestamp, Some("1492774577.5"));
        assert_eq!(odd.timestamp_secs, None);
        assert_eq!(StripeSignature::parse("t=nope,v1=aaa").timestamp_secs, None);
    }

    #[test]
    fn accepts_a_genuine_delivery_and_names_the_authenticating_account() {
        let payload = r#"{"id":"evt_1","type":"checkout.session.completed"}"#;
        let a = credential("whsec_account_a");
        let credentials = vec![a.clone(), credential("whsec_account_b")];

        assert_eq!(
            verify(payload, &genuine(payload, &a.secret), &credentials)
                .expect("a genuine delivery must verify"),
            a.account_id
        );
    }

    #[test]
    fn accepts_any_of_the_rotating_v1_values() {
        let payload = r#"{"id":"evt_rotate"}"#;
        let a = credential("whsec_account_a");
        let credentials = vec![a.clone()];
        let stamp = live_stamp();
        let fresh = hmac_hex(&format!("{}.{}", stamp, payload), &a.secret).unwrap();
        // The older value is listed first, the current one second.
        let header = format!("t={},v1=deadbeef,v1={}", stamp, fresh);
        assert_eq!(
            verify(payload, &header, &credentials).unwrap(),
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
        let err = verify(
            payload,
            &format!("t={},v1={}", live_stamp(), bare),
            &credentials,
        )
        .expect_err("a signature that is not over {t}.{body} must not verify");
        assert!(matches!(err, AppError::BadRequest(ref m) if m == "Invalid webhook signature"));
    }

    #[test]
    fn refuses_a_stamp_only_header() {
        // `t=` with no v1= is not a signature at all.
        let credentials = vec![credential("whsec_account_a")];
        let err = verify("{}", &format!("t={}", live_stamp()), &credentials)
            .expect_err("a header with no v1= is not a signature");
        assert!(matches!(err, AppError::BadRequest(ref m) if m.starts_with("Missing v1=")));

        // …and a header with no t= is refused too: the stamp is part of the signed bytes.
        let err = verify("{}", "v1=deadbeef", &credentials)
            .expect_err("a header with no t= cannot reproduce the signed bytes");
        assert!(matches!(err, AppError::BadRequest(ref m) if m.starts_with("Missing t=")));
    }

    #[test]
    fn refuses_a_signature_no_active_row_signed() {
        let payload = r#"{"id":"evt_3"}"#;
        let credentials = vec![credential("whsec_account_a")];
        let err = verify(
            payload,
            &genuine(payload, "whsec_rotated_away"),
            &credentials,
        )
        .expect_err("a secret no active row holds must not verify");
        assert!(matches!(err, AppError::BadRequest(ref m) if m == "Invalid webhook signature"));
    }

    // ── the stamp as a clock (kanban t_d9fb9e1b) ────────────────────────────────────────────────

    /// The defect this replaced: the stamp was part of the signed bytes but its AGE was never
    /// checked, so a captured header + body verified FOR EVER.
    #[test]
    fn refuses_a_delivery_whose_stamp_is_older_than_the_tolerance() {
        let payload = r#"{"id":"evt_stale"}"#;
        let a = credential("whsec_account_a");
        let credentials = vec![a.clone()];
        let now: i64 = 1_800_000_000;
        let header = genuine_at(payload, &a.secret, now - 3_600);

        let err = verify_stripe_signature_any_at(payload, &header, &credentials, now, 300)
            .expect_err("an hour-old stamp must not verify under a 300 s tolerance");
        assert!(is_stale(&err), "expected the freshness arm, got {:?}", err);

        // The same pair one second inside the bound IS accepted — the arm is a bound, not a
        // tripwire that swallows genuine deliveries.
        let inside = genuine_at(payload, &a.secret, now - 300);
        assert_eq!(
            verify_stripe_signature_any_at(payload, &inside, &credentials, now, 300).unwrap(),
            a.account_id
        );

        // …and the very next second outside it is refused.
        let outside = genuine_at(payload, &a.secret, now - 301);
        assert!(is_stale(
            &verify_stripe_signature_any_at(payload, &outside, &credentials, now, 300)
                .expect_err("301 s old is outside a 300 s tolerance")
        ));
    }

    /// A clock ahead of ours is as suspect as one behind: Stripe's own check is `abs(now - t)`.
    #[test]
    fn refuses_a_delivery_whose_stamp_is_far_in_the_future() {
        let payload = r#"{"id":"evt_future"}"#;
        let a = credential("whsec_account_a");
        let credentials = vec![a.clone()];
        let now: i64 = 1_800_000_000;

        let skew = genuine_at(payload, &a.secret, now + 300);
        assert_eq!(
            verify_stripe_signature_any_at(payload, &skew, &credentials, now, 300).unwrap(),
            a.account_id,
            "a 300 s forward skew is inside the bound"
        );

        let far = genuine_at(payload, &a.secret, now + 86_400);
        assert!(is_stale(
            &verify_stripe_signature_any_at(payload, &far, &credentials, now, 300)
                .expect_err("a day in the future must be refused")
        ));
    }

    /// The tolerance in force is the only thing that decides the bound, so a misconfigured value is
    /// observable in the verdict.
    #[test]
    fn the_bound_follows_the_tolerance_in_force() {
        let payload = r#"{"id":"evt_knob"}"#;
        let a = credential("whsec_account_a");
        let credentials = vec![a.clone()];
        let now: i64 = 1_800_000_000;
        let header = genuine_at(payload, &a.secret, now - 600);

        assert!(
            is_stale(
                &verify_stripe_signature_any_at(&payload[..], &header, &credentials, now, 300)
                    .expect_err("600 s old is outside a 300 s tolerance")
            ),
            "300 s tolerance must refuse a 600 s old stamp"
        );
        assert_eq!(
            verify_stripe_signature_any_at(payload, &header, &credentials, now, 900).unwrap(),
            a.account_id,
            "the same delivery is inside a 900 s tolerance"
        );
    }

    #[test]
    fn refuses_a_stamp_that_is_not_a_clock_even_though_the_signature_is_genuine() {
        let payload = r#"{"id":"evt_noclk"}"#;
        let a = credential("whsec_account_a");
        let credentials = vec![a.clone()];

        for stamp in ["nope", "", "1492774577.5", "1e9", "1492774577s"] {
            let header = genuine_unreadable_stamp(payload, &a.secret, stamp);
            let err = verify(payload, &header, &credentials)
                .expect_err(&format!("t={} is not a clock and must be refused", stamp));
            assert!(
                is_stale(&err),
                "t={:?} must be refused by the freshness arm, got {:?}",
                stamp,
                err
            );
        }
    }

    /// The ordering contract: freshness may only judge a delivery this app AUTHENTICATED.
    #[test]
    fn an_ancient_stamp_on_a_forged_signature_is_still_an_invalid_signature() {
        let payload = r#"{"id":"evt_order"}"#;
        let credentials = vec![credential("whsec_account_a")];
        let now: i64 = 1_800_000_000;
        // A genuine header for a secret no active row holds, stamped 30 days ago.
        let stale_forgery = genuine_at(payload, "whsec_someone_elses", now - 2_592_000);
        let err = verify_stripe_signature_any_at(payload, &stale_forgery, &credentials, now, 300)
            .expect_err("a foreign secret must not verify, however old the stamp is");
        assert!(
            matches!(err, AppError::BadRequest(ref m) if m == "Invalid webhook signature"),
            "freshness must not pre-empt authenticity, got {:?}",
            err
        );
    }

    /// The same signature with an ancient stamp, walked through every arm in order.
    #[test]
    fn the_arms_read_presence_then_authenticity_then_freshness() {
        let payload = r#"{"id":"evt_arms"}"#;
        let a = credential("whsec_account_a");
        let credentials = vec![a.clone()];
        let now: i64 = 1_800_000_000;
        let ancient = now - 2_592_000;

        // PRESENCE: no `t=` at all, and no `v1=` at all.
        let err = verify_stripe_signature_any_at(payload, "v1=deadbeef", &credentials, now, 300)
            .expect_err("no stamp is no signed bytes");
        assert!(matches!(err, AppError::BadRequest(ref m) if m.starts_with("Missing t=")));
        let err = verify_stripe_signature_any_at(
            payload,
            &format!("t={}", ancient),
            &credentials,
            now,
            300,
        )
        .expect_err("no v1= is no signature");
        assert!(matches!(err, AppError::BadRequest(ref m) if m.starts_with("Missing v1=")));

        // AUTHENTICITY: an ancient stamp on a signature nothing signed.
        let err = verify_stripe_signature_any_at(
            payload,
            &genuine_at(payload, "whsec_nobody", ancient),
            &credentials,
            now,
            300,
        )
        .expect_err("a foreign secret must not verify");
        assert!(matches!(err, AppError::BadRequest(ref m) if m == "Invalid webhook signature"));

        // FRESHNESS: a genuine, ancient delivery.
        let err = verify_stripe_signature_any_at(
            payload,
            &genuine_at(payload, &a.secret, ancient),
            &credentials,
            now,
            300,
        )
        .expect_err("a genuine but ancient delivery must be refused");
        assert!(is_stale(&err));
    }

    /// A refusal is never "no verdict": every arm above is reached and answers one, and a fresh
    /// genuine delivery is the control that keeps the freshness arm from being a blanket refusal.
    #[test]
    fn a_fresh_genuine_delivery_is_answered_with_the_account() {
        let payload = r#"{"id":"evt_control"}"#;
        let a = credential("whsec_account_a");
        let credentials = vec![a.clone()];
        let now = unix_now();
        for age in [0, 1, 30, 299, 300] {
            let header = genuine_at(payload, &a.secret, now - age);
            assert_eq!(
                verify_stripe_signature_any_at(
                    payload,
                    &header,
                    &credentials,
                    now,
                    DEFAULT_STRIPE_SIGNATURE_TOLERANCE_SECS
                )
                .unwrap(),
                a.account_id,
                "an {} s old genuine delivery must verify",
                age
            );
        }
    }
}
