//! The float rule — three independent conditions, in one place, with plain-English outcomes.
//!
//! David, 2026-10-04 (kanban t_69e6598e): *"Based on the calculation of what's there for the loyalty
//! what would the float rule be. I'm a layman in this. You come up with best industry practice float
//! rule. But again that should be able to be configured also by the Admin in the admin panel."* — and
//! the rules must be published to the businesses as *"what is expected of the local businesses to
//! maintain the float"*.
//!
//! WHY NOT ONE NUMBER: the guard used to compare exactly one flat figure (`available >=
//! minimum_float`). One number cannot adapt — a programme with a hundred points outstanding and one
//! with a million need the same *relationship* held, not the same dollar amount.
//!
//! THE THREE CONDITIONS, and why each exists:
//!
//! | rule     | condition                                                  | view        |
//! |----------|------------------------------------------------------------|-------------|
//! | coverage | `available >= coverage_pct% x outstanding_liability`        | accounting  |
//! | burn     | `available >= burn_months  x trailing-30-day redemption $`  | operational |
//! | floor    | `available >= minimum_float`                                | risk        |
//!
//! * **Coverage** — never redeem points nobody funded.
//! * **Burn** — a programme can be fully covered on paper and still fail when several members redeem
//!   at once; the float has to be sized against what actually gets paid out.
//! * **Floor** — a brand-new programme has near-zero liability *and* near-zero burn, so it would pass
//!   the other two while holding nothing. The floor is the absolute minimum every programme keeps.
//!
//! ACTION ON BREACH IS `hold`: points stay valid, redemptions resume on top-up, nothing is confiscated.
//!
//! A MALFORMED SETTING FALLS BACK TO THE CONSERVATIVE DEFAULT (100% cover, 1 month burn, whatever
//! floor the row carries) — a bad value fails safe, never open. A setting column that is text in a
//! future schema, or a zero/negative number, is treated as absent.

use rust_decimal::Decimal;
use serde::Serialize;
use std::str::FromStr;

/// Best-practice defaults, as strings so they share the parser with the stored settings.
pub const DEFAULT_COVERAGE_PCT: &str = "100";
pub const DEFAULT_BURN_MONTHS: &str = "1";

/// The three thresholds an admin configures. No other state — the candidate position is passed to
/// `evaluate`, so the same rule judges the live treasury AND a hypothetical payout.
#[derive(Debug, Clone, Copy)]
pub struct FloatRule {
    pub coverage_pct: Decimal,
    pub burn_months: Decimal,
    pub floor: Decimal,
}

/// One condition's outcome, carrying the numbers that decided it — because "breach" teaches a business
/// owner nothing.
#[derive(Debug, Clone, Serialize)]
pub struct RuleOutcome {
    /// Stable machine key: "coverage" | "burn" | "floor".
    pub key: &'static str,
    /// Short human label for a panel.
    pub label: &'static str,
    /// What the rule needs the float to be at.
    pub required: Decimal,
    /// Did the position meet it?
    pub passed: bool,
    /// The whole condition in plain words, with the actual figures.
    pub sentence: String,
}

/// The verdict for one position: safe only when EVERY condition passes.
#[derive(Debug, Clone, Serialize)]
pub struct FloatVerdict {
    pub safe: bool,
    pub available: Decimal,
    pub outcomes: Vec<RuleOutcome>,
}

impl FloatVerdict {
    /// The conditions that are short, in rule order.
    pub fn failures(&self) -> Vec<&RuleOutcome> {
        self.outcomes.iter().filter(|o| !o.passed).collect()
    }

    /// The machine keys of the short conditions (empty when safe).
    pub fn failed_keys(&self) -> Vec<&'static str> {
        self.outcomes
            .iter()
            .filter(|o| !o.passed)
            .map(|o| o.key)
            .collect()
    }

    /// How much more money the float needs to pass every condition (0 when safe). This is the number
    /// a business is asked to top up by.
    pub fn shortfall(&self) -> Decimal {
        self.outcomes
            .iter()
            .filter(|o| !o.passed)
            .map(|o| o.required - self.available)
            .max()
            .unwrap_or(Decimal::ZERO)
            .max(Decimal::ZERO)
    }

    /// ONE plain-English sentence naming WHICH condition is short and by how much — what the operator
    /// sees on the console and what the business is told. When every condition passes it says so.
    pub fn sentence(&self) -> String {
        match self.failures().first() {
            None => format!(
                "The programme holds {} and passes all three conditions, so rewards are being paid.",
                money(self.available)
            ),
            Some(first) => {
                let shortfall = self.shortfall();
                let extra = self.failures().len().saturating_sub(1);
                let also = if extra > 0 {
                    format!(
                        " {} more condition{} also short.",
                        extra,
                        if extra == 1 { " is" } else { "s are" }
                    )
                } else {
                    String::new()
                };
                format!(
                    "Redemptions are held: {}{} The programme needs {} more before the business is reimbursed again.",
                    first.sentence,
                    also,
                    money(shortfall)
                )
            }
        }
    }

    /// The three conditions as business-facing sentences (the published rules page is generated from
    /// these, so the copy and the enforcement cannot drift).
    pub fn conditions_plain_english(&self) -> Vec<String> {
        self.outcomes.iter().map(|o| o.sentence.clone()).collect()
    }
}

/// The largest value the `numeric(6,2)` setting columns can hold. A write outside `(0, MAX_SETTING]` is
/// refused in the API with a 400 naming the range — otherwise it dies on the column as a 500 (measured
/// 2026-10-04: `float_burn_months = 100000` answered `500 {"code":500,"error":"Internal server error"}`).
pub const MAX_SETTING: &str = "9999.99";

fn max_setting() -> Decimal {
    Decimal::from_str(MAX_SETTING).unwrap_or_else(|_| Decimal::new(999999, 2))
}

/// Why a proposed coverage/burn setting is refused, in plain words. `None` means it is acceptable, and
/// this is the ONE place both writers ask, so the rule and the clearinghouse config cannot disagree.
pub fn setting_refusal(field: &str, v: Decimal) -> Option<String> {
    if v <= Decimal::ZERO {
        Some(format!(
            "{field} must be more than zero — a programme protecting nothing is not protected at all"
        ))
    } else if v > max_setting() {
        Some(format!(
            "{field} must be at most {} (the largest value the setting can hold)",
            MAX_SETTING
        ))
    } else {
        None
    }
}

/// Parse one stored setting into a positive decimal, falling back to the conservative default for a
/// missing, blank, unparseable, zero or negative value. A bad setting fails SAFE (toward holding).
fn setting_or_default(raw: Option<&str>, default: &str) -> Decimal {
    let parsed = raw
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| Decimal::from_str(s).ok());
    match parsed {
        Some(d) if d > Decimal::ZERO => d,
        _ => Decimal::from_str(default).unwrap_or(Decimal::ONE),
    }
}

/// Build the rule an admin has configured. `coverage` and `burn` arrive as strings so a malformed
/// setting (or a future text column) is tolerated exactly as the design requires.
pub fn rule_from_settings(coverage: Option<&str>, burn: Option<&str>, floor: Decimal) -> FloatRule {
    FloatRule {
        coverage_pct: setting_or_default(coverage, DEFAULT_COVERAGE_PCT),
        burn_months: setting_or_default(burn, DEFAULT_BURN_MONTHS),
        floor: if floor < Decimal::ZERO {
            Decimal::ZERO
        } else {
            floor
        },
    }
}

/// Convenience for callers that read the columns as numeric (the common case).
pub fn rule_from_columns(
    coverage: Option<Decimal>,
    burn: Option<Decimal>,
    floor: Decimal,
) -> FloatRule {
    rule_from_settings(
        coverage.map(|d| d.to_string()).as_deref(),
        burn.map(|d| d.to_string()).as_deref(),
        floor,
    )
}

/// Judge one position against the rule.
///
/// * a negative `outstanding_liability` (reachable with test data, or after a rate change) is treated
///   as zero — the coverage rule must not demand a positive float from a negative liability;
/// * `monthly_burn` of `None` or `0` means there is nothing to size a month against, so the burn
///   condition cannot fail.
pub fn evaluate(
    available: Decimal,
    outstanding_liability: Option<Decimal>,
    monthly_burn: Option<Decimal>,
    rule: &FloatRule,
) -> FloatVerdict {
    let liability = outstanding_liability
        .unwrap_or(Decimal::ZERO)
        .max(Decimal::ZERO);
    let burn = monthly_burn.unwrap_or(Decimal::ZERO).max(Decimal::ZERO);

    // ── coverage ─────────────────────────────────────────────────────────────────────────────────
    let coverage_required = (rule.coverage_pct / Decimal::from(100u32) * liability).round_dp(2);
    let coverage_passed = available >= coverage_required;
    let coverage_sentence = if coverage_passed {
        format!(
            "Coverage: the programme holds {} against member points worth {} — that meets the {}% cover rule (needs {}).",
            money(available),
            money(liability),
            trim_pct(rule.coverage_pct),
            money(coverage_required)
        )
    } else {
        format!(
            "Coverage: the programme holds {} but members are holding points worth {}, and the {}% cover rule needs {}.",
            money(available),
            money(liability),
            trim_pct(rule.coverage_pct),
            money(coverage_required)
        )
    };
    let coverage = RuleOutcome {
        key: "coverage",
        label: "Coverage",
        required: coverage_required,
        passed: coverage_passed,
        sentence: coverage_sentence,
    };

    // ── burn ─────────────────────────────────────────────────────────────────────────────────────
    let burn_required = (rule.burn_months * burn).round_dp(2);
    let burn_passed = available >= burn_required;
    let burn_sentence = if burn <= Decimal::ZERO {
        format!(
            "Burn: there is not enough redemption history yet to measure a month, so this condition cannot fail. The programme holds {}.",
            money(available)
        )
    } else if burn_passed {
        format!(
            "Burn: the programme holds {} against {}, which is {} month(s) of recent redemptions ({} a month).",
            money(available),
            money(burn_required),
            trim_pct(rule.burn_months),
            money(burn)
        )
    } else {
        format!(
            "Burn: the programme holds {} but needs {} to cover {} month(s) of recent redemptions ({} a month).",
            money(available),
            money(burn_required),
            trim_pct(rule.burn_months),
            money(burn)
        )
    };
    let burn_outcome = RuleOutcome {
        key: "burn",
        label: "Burn",
        required: burn_required,
        passed: burn_passed,
        sentence: burn_sentence,
    };

    // ── floor ────────────────────────────────────────────────────────────────────────────────────
    let floor_passed = available >= rule.floor;
    let floor_sentence = if floor_passed {
        format!(
            "Floor: the programme holds {}, at or above the {} safety balance every programme keeps.",
            money(available),
            money(rule.floor)
        )
    } else {
        format!(
            "Floor: the programme holds {}, below the {} safety balance every programme keeps.",
            money(available),
            money(rule.floor)
        )
    };
    let floor_outcome = RuleOutcome {
        key: "floor",
        label: "Floor",
        required: rule.floor,
        passed: floor_passed,
        sentence: floor_sentence,
    };

    let outcomes = vec![coverage, burn_outcome, floor_outcome];
    let safe = outcomes.iter().all(|o| o.passed);
    FloatVerdict {
        safe,
        available,
        outcomes,
    }
}

/// Money as a person reads it: two decimal places, no exponent.
pub fn money(d: Decimal) -> String {
    format!("${}", d.round_dp(2))
}

/// A percentage/months figure without a trailing `.00` — `100`, `1.5`, `1`.
pub fn plain_number(d: Decimal) -> String {
    d.round_dp(2).normalize().to_string()
}

fn trim_pct(d: Decimal) -> String {
    plain_number(d)
}

/// The three conditions stated as the RULE (thresholds only — no live balance). This is what the
/// published page reads, so the copy and the enforcement come from one rule and cannot drift, and a
/// public page never leaks the programme's real position.
pub fn rule_conditions_plain_english(rule: &FloatRule) -> Vec<String> {
    vec![
        format!(
            "Coverage: the float must always cover at least {}% of the value of the points members are holding.",
            plain_number(rule.coverage_pct)
        ),
        format!(
            "Burn: the float must always hold at least {} month(s) of recent redemptions, so several customers redeeming at once is still covered.",
            plain_number(rule.burn_months)
        ),
        format!(
            "Floor: the float must never fall below the {} safety balance.",
            money(rule.floor)
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn dec(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    #[test]
    fn malformed_settings_fall_back_to_the_conservative_default() {
        for bad in [
            None,
            Some(""),
            Some("  "),
            Some("abc"),
            Some("-5"),
            Some("0"),
        ] {
            let r = rule_from_settings(bad, bad, dec("100"));
            assert_eq!(r.coverage_pct, dec("100"));
            assert_eq!(r.burn_months, dec("1"));
        }
        let good = rule_from_settings(Some("120.5"), Some("3"), dec("250"));
        assert_eq!(good.coverage_pct, dec("120.5"));
        assert_eq!(good.burn_months, dec("3"));
        assert_eq!(good.floor, dec("250"));
    }

    #[test]
    fn a_write_outside_the_column_range_is_refused_with_a_reason() {
        // 0 and negative would mean "protect nothing"; more than the numeric(6,2) range would die on the
        // column as a 500 (measured 2026-10-04 with float_burn_months = 100000).
        for bad in ["0", "-1", "100000"] {
            let why = setting_refusal("float_burn_months", dec(bad));
            assert!(why.is_some(), "{bad} must be refused");
            assert!(why.unwrap().contains("float_burn_months"));
        }
        assert!(setting_refusal("float_burn_months", dec(MAX_SETTING)).is_none());
        assert!(setting_refusal("float_coverage_pct", dec("100")).is_none());
    }

    #[test]
    fn coverage_uses_the_liability_relationship_not_a_flat_number() {
        let rule = rule_from_settings(Some("100"), Some("1"), dec("100"));
        // covered: 1000 held against 800 liability => needs 800 => passes coverage
        let v = evaluate(dec("1000"), Some(dec("800")), Some(dec("0")), &rule);
        assert!(v.safe, "{}", v.sentence());
        // underwater: 1000 held against 5000 liability => needs 5000 => coverage short
        let v = evaluate(dec("1000"), Some(dec("5000")), Some(dec("0")), &rule);
        assert!(!v.safe);
        assert_eq!(v.failed_keys(), vec!["coverage"]);
        assert!(v.sentence().contains("Coverage"));
        assert_eq!(v.shortfall(), dec("4000"));
    }

    #[test]
    fn a_negative_liability_never_demands_a_positive_float() {
        let rule = rule_from_settings(Some("200"), Some("0"), dec("10"));
        let v = evaluate(dec("10"), Some(dec("-5000")), Some(dec("0")), &rule);
        assert!(v.safe, "{}", v.sentence());
    }

    #[test]
    fn burn_sizes_the_float_against_a_month_of_redemptions() {
        // 100% cover and floor are trivially met; the burn is what bites.
        let rule = rule_from_settings(Some("10"), Some("2"), dec("10"));
        let v = evaluate(dec("500"), Some(dec("100")), Some(dec("400")), &rule);
        assert!(!v.safe);
        assert_eq!(v.failed_keys(), vec!["burn"]);
        assert_eq!(v.shortfall(), dec("300")); // needs 800, holds 500
                                               // no redemption history => burn cannot fail
        let v = evaluate(dec("500"), Some(dec("100")), None, &rule);
        assert!(v.safe, "{}", v.sentence());
        let v = evaluate(dec("500"), Some(dec("100")), Some(dec("0")), &rule);
        assert!(v.safe, "{}", v.sentence());
    }

    #[test]
    fn a_brand_new_programme_is_caught_by_the_floor() {
        // near-zero liability and near-zero burn would pass the adaptive rules; the floor is why a
        // programme that holds nothing cannot pay.
        let rule = rule_from_settings(Some("100"), Some("1"), dec("100"));
        let v = evaluate(dec("0"), Some(dec("0")), Some(dec("0")), &rule);
        assert!(!v.safe);
        assert_eq!(v.failed_keys(), vec!["floor"]);
        assert_eq!(v.shortfall(), dec("100"));
    }

    #[test]
    fn the_sentence_names_the_condition_and_the_shortfall() {
        let rule = rule_from_settings(Some("100"), Some("1"), dec("50"));
        let v = evaluate(dec("40"), Some(dec("200")), Some(dec("0")), &rule);
        let s = v.sentence();
        assert!(s.contains("Coverage"), "{s}");
        assert!(s.contains("$160"), "{s}"); // needs 200, holds 40
        assert!(v.conditions_plain_english().len() == 3);
    }
}
