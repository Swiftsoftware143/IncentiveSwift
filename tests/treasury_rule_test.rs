//! The float-rule vocabulary — a setting the app does not perform must never be saveable, offered, or
//! described.
//!
//! Measured 2026-10-02 (kanban t_d5754642): `point_treasury.on_float_breach` was CHECK-constrained to
//! `hold | allow_and_bill | suspend`, selectable in the console, and DESCRIBED value-by-value on the
//! page the businesses read — while the breach arm branched on NOTHING and held the redemption under
//! every setting. The console and the published copy promised "it is paid and the shortfall is billed
//! to the business" / "the programme stops redeeming"; the app did neither.
//!
//! ARM CHOSEN: REFUSE what is not built (the API answers 400 naming it, the console offers only what
//! is enforced, the published rules describe only what happens). These tests pin the ONE source of
//! truth every surface reads, so a future edit cannot reintroduce a published promise the enforcement
//! does not keep.

use incentiveswift_api::handlers::treasury_engine_handler::{
    effective_rule, on_breach_applied_note, on_breach_is_implemented, on_breach_sentence,
    on_breach_vocabulary, IMPLEMENTED_ON_FLOAT_BREACH, UNIMPLEMENTED_ON_FLOAT_BREACH,
};
use rust_decimal::Decimal;

/// The vocabulary the migration's CHECK constraint admits
/// (`migrations/20261002_treasury_engine.sql`). Pinned here so removing a value from BOTH lists — which
/// would leave a stored value unclassified and undescribable — fails the suite.
const DB_CHECK_VOCABULARY: [&str; 3] = ["hold", "allow_and_bill", "suspend"];

fn hundred() -> Decimal {
    Decimal::new(10000, 2)
}

#[test]
fn every_check_legal_value_is_classified_exactly_once() {
    for v in DB_CHECK_VOCABULARY {
        let implemented = IMPLEMENTED_ON_FLOAT_BREACH.contains(&v);
        let unimplemented = UNIMPLEMENTED_ON_FLOAT_BREACH.contains(&v);
        assert!(
            implemented ^ unimplemented,
            "{v} must be in exactly one list (implemented xor unimplemented)"
        );
        assert_eq!(implemented, on_breach_is_implemented(v), "{v}");
    }
    // The live enforcement holds and only holds: this list is what the API will accept.
    assert_eq!(IMPLEMENTED_ON_FLOAT_BREACH, ["hold"]);
    assert_eq!(UNIMPLEMENTED_ON_FLOAT_BREACH, ["allow_and_bill", "suspend"]);
    assert_eq!(on_breach_vocabulary(), "hold, allow_and_bill, suspend");
}

#[test]
fn an_unimplemented_setting_is_never_reported_as_the_rule_in_force() {
    for v in UNIMPLEMENTED_ON_FLOAT_BREACH {
        assert_eq!(
            effective_rule(v),
            "hold",
            "{v} is not performed; hold is in force"
        );
    }
    assert_eq!(effective_rule("hold"), "hold");
    // A value that is in NEITHER list (a direct DB write of something nonsense) also resolves to the
    // behaviour the code actually has, rather than being echoed back as if it were in force.
    assert_eq!(effective_rule("pay_everything"), "hold");
}

#[test]
fn the_published_sentence_never_promises_billing_or_suspension() {
    for v in UNIMPLEMENTED_ON_FLOAT_BREACH {
        let s = on_breach_sentence(hundred(), v);
        assert!(!s.contains("billed"), "{v} must not promise billing: {s}");
        assert!(!s.contains("paid and"), "{v} must not promise payment: {s}");
        assert!(!s.contains("stops"), "{v} must not promise suspension: {s}");
        assert!(
            s.contains("$100.00") && s.contains("held") && s.contains("top up"),
            "{v} must describe the hold that really happens: {s}"
        );
        assert!(
            s.contains("not applied"),
            "{v} must say the stored rule is not applied: {s}"
        );
    }
}

#[test]
fn the_hold_sentence_is_the_one_promise_and_names_the_balance() {
    let s = on_breach_sentence(hundred(), "hold");
    assert!(s.contains("$100.00"), "{s}");
    assert!(s.contains("held") && s.contains("top up"), "{s}");
    assert!(!s.contains("billed") && !s.contains("not applied"), "{s}");
    // The console and the businesses' rules page read the SAME function, so the live (hold) case is one
    // sentence, not two that can drift.
    assert!(s.starts_with(
        "If paying a reward would take the programme below $100.00, the reward is held"
    ));
}

#[test]
fn a_hold_record_says_what_the_app_did() {
    assert_eq!(on_breach_applied_note("hold"), "Rule in force: hold");
    for v in UNIMPLEMENTED_ON_FLOAT_BREACH {
        let note = on_breach_applied_note(v);
        assert!(
            note.contains("Rule applied: hold"),
            "{v}: the record must say what happened: {note}"
        );
        assert!(
            !note.contains("Rule in force"),
            "{v}: never quoted as the rule in force: {note}"
        );
    }
}
