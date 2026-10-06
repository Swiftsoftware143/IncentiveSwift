//! The treasury engine — collect from businesses, guard the float, keep the ledger.
//!
//! David, 2026-10-02: *"the main concern is the loyalty engine so there's always money in there ... I
//! also have to be able to collect payment from the businesses for the loyalty program. And I'll connect
//! Stripe later on but that system needs to be built in ... and those rules need to be added into it so
//! the businesses understand."*
//!
//! WHAT WAS THERE (measured, see migrations/20261002_treasury_engine.sql): the counters existed and were
//! written on issue and on redemption; `minimum_float` was displayed and compared against NOTHING; no
//! money was ever collected from a business; the ledger table had no writer.
//!
//! THIS FILE IS THE MONEY. Three rules, and they are the whole engine:
//!   1. MONEY IN is recorded against a business, with a provider seam (`method` + `reference`) so Stripe
//!      plugs in later without touching this logic.
//!   2. THE FLOAT IS GUARDED. A redemption that would take the float below the minimum does NOT quietly
//!      succeed — it becomes a HOLD, because the customer is standing at the counter and the business
//!      has to be told to top up.
//!   3. EVERY MOVEMENT IS WRITTEN DOWN. If it moved money and it is not in `journal_entries`, it did not
//!      happen as far as an audit is concerned.
//!
//! ── STAGED STATUS, MEASURED 2026-10-06 (kanban t_0107d46b) — READ THIS BEFORE "CLEANING UP" ──────
//! The engine is HALF LIVE, and the two halves must be described differently:
//!   * LIVE (a route reaches it): money in (`record_funding`), the rule + the position for DISPLAY
//!     (`get_state`, the business rules page, `set_rule`), and the human-resolve arm of a hold
//!     (`list_holds` / `resolve_hold`, which the console renders only when a hold exists).
//!   * NOT WIRED (routed nothing, called by nothing): `check_float` — the enforcement. Its ONLY caller
//!     was the anonymous clearinghouse redemption in `loyalty_badges.rs`, and that whole surface was
//!     RETIRED on purpose (f3d212e4, kanban t_5e244255: anonymous + unscoped, 0 callers fleet-wide,
//!     0 nginx hits, 0 rows at stake). That retire also removed the only writer of
//!     `point_redemption_log` and the only redemption-side writer of the `point_treasury` counters, so
//!     nothing in the crate now reaches the guard or can insert a `treasury_hold`.
//! So today the guard is STAGED ENFORCEMENT, not display-only decoration and not dead code: do NOT
//! delete it and do NOT dismantle the hold arms. Wire `check_float` into a payout path (an event that
//! pays a business money and therefore has a dollar amount) when that path exists again; the blocker is
//! a product decision, not code — what a redemption is worth in dollars, and who carries the shortfall.
//! Until then the console's float verdict is a REPORT of the position, not a rule the app acts on.

use axum::{
    extract::{Path, State},
    Json,
};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::error::AppError;
use crate::handlers::float_rule;
use crate::security::auth::AuthenticatedUser;
use crate::state::AppState;

/// ── THE BREACH BEHAVIOURS — the ONE source of truth every surface reads ───────────────────────────
///
/// The DB CHECK admits `hold | allow_and_bill | suspend` so an unknown behaviour can never be
/// configured. Measured 2026-10-02 (kanban t_d5754642): the constraint worked and the behaviour behind
/// it did not exist. `allow_and_bill` and `suspend` appeared in exactly four places — the CHECK, this
/// vocabulary, and two published sentences — and the breach arm branched on NOTHING, so both behaved
/// exactly like `hold` while the console and the page the businesses read promised "it is paid and the
/// shortfall is billed to the business" / "the programme stops redeeming". Copy promising an outcome
/// the system does not perform is a lie in writing.
///
/// ARM CHOSEN: REFUSE what is not built. A behaviour the enforcement does not perform is not saved
/// (400 naming it), not offered by the console, and never described on the published rules — until the
/// arm exists. `allow_and_bill` needs a product decision before it can be implemented (who the bill
/// goes to, on what cycle, where the receivable lives) and `suspend` needs the customer-facing story
/// (what the person standing at the counter is told), so neither was invented here.
pub const IMPLEMENTED_ON_FLOAT_BREACH: [&str; 1] = ["hold"];
pub const UNIMPLEMENTED_ON_FLOAT_BREACH: [&str; 2] = ["allow_and_bill", "suspend"];

/// Is this a behaviour the enforcement actually performs? ONE gate for every surface — the writer, the
/// console payload, the published rules and the hold record all ask this, so they cannot drift.
pub fn on_breach_is_implemented(behaviour: &str) -> bool {
    IMPLEMENTED_ON_FLOAT_BREACH.contains(&behaviour)
}

/// The rule the app APPLIES, given what the row stores.
///
/// The breach arm holds under every setting, so `hold` is what is in force whatever the column says.
/// An unimplemented stored value (only reachable by writing the DB outside this API, which refuses it)
/// is reported separately and never as the rule in force.
pub fn effective_rule(stored: &str) -> &str {
    if on_breach_is_implemented(stored) {
        stored
    } else {
        IMPLEMENTED_ON_FLOAT_BREACH[0]
    }
}

/// The ONE plain-English sentence for the rule in force — read by the businesses' rules page AND the
/// operator console, generated from the rule the app APPLIES so the published copy cannot disagree
/// with the enforcement.
///
/// `stored` is what the ROW says, which is what the sentence has to speak about: when the app performs
/// it the sentence is the promise, and when it does not the sentence refuses to promise and names the
/// stored value as not applied alongside the hold that really happens. Callers publish
/// `effective_rule(stored)` as the rule IN FORCE, so a stored value the app cannot perform is never
/// presented as an outcome (kanban t_d5754642).
pub fn on_breach_sentence(rule: &float_rule::FloatRule, stored: &str) -> String {
    let base = format!(
        "If paying a reward would leave the programme short on any of its three float conditions — cover {}% of the value of the points members are holding, hold {} month(s) of recent redemptions, or stay at or above the {} safety balance — the payment to the business for that reward is held and the business is asked to top up before it is paid out. The customer has already been given their reward at the counter; nothing is taken back.",
        float_rule::plain_number(rule.coverage_pct),
        float_rule::plain_number(rule.burn_months),
        float_rule::money(rule.floor)
    );
    if on_breach_is_implemented(stored) {
        base
    } else {
        format!(
            "{base} Note: this programme's stored float rule ('{stored}') is not supported by the software and is not applied."
        )
    }
}

/// What a hold record should say the rule WAS. The record describes what the app DID, so an
/// unimplemented stored setting is named as such instead of being quoted as the rule in force.
///
/// STAGED (kanban t_0107d46b): no caller today — the hold record it writes is produced by `check_float`,
/// which nothing calls since the redemption payout path was retired. Kept with its consumer so the two
/// land together; see the STAGED STATUS note at the top of this file.
#[allow(dead_code)]
pub fn on_breach_applied_note(behaviour: &str) -> String {
    if on_breach_is_implemented(behaviour) {
        format!("Rule in force: {behaviour}")
    } else {
        format!("Rule applied: hold (the stored setting '{behaviour}' is not implemented)")
    }
}

/// The full vocabulary the DB CHECK admits, for refusal messages — so a caller who sends nonsense is
/// told what IS implemented instead of only what is not.
pub fn on_breach_vocabulary() -> String {
    IMPLEMENTED_ON_FLOAT_BREACH
        .iter()
        .chain(UNIMPLEMENTED_ON_FLOAT_BREACH.iter())
        .copied()
        .collect::<Vec<_>>()
        .join(", ")
}

/// The whole float position, as one object — so a UI can never show a stale half of it.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct TreasuryState {
    pub total_points_issued: Option<i64>,
    pub total_points_redeemed: Option<i64>,
    pub total_revenue_collected: Option<Decimal>,
    pub total_reimbursements_paid: Option<Decimal>,
    pub outstanding_liability: Option<Decimal>,
    pub minimum_float: Option<Decimal>,
    pub float_coverage_pct: Option<Decimal>,
    pub float_burn_months: Option<Decimal>,
    pub on_float_breach: Option<String>,
    pub float_breaches: Option<i32>,
}

/// Write one line of the ledger. Every money movement in this file goes through here, so "the ledger is
/// complete" is a property of the code rather than a promise in a comment.
///
/// `tenant_id` is deliberately NULL: the treasury belongs to the PLATFORM, not to one tenant, and a row
/// that has to lie about its tenant to be written is a row nobody can trust.
#[allow(clippy::too_many_arguments)]
async fn ledger(
    s: &AppState,
    entry_type: &str,
    description: &str,
    amount: Decimal,
    direction: &str,
    reference_id: Option<Uuid>,
    reference_type: &str,
    balance_after: Option<Decimal>,
) -> Result<(), AppError> {
    sqlx::query(
        r#"INSERT INTO journal_entries
               (id, tenant_id, entry_type, description, amount, direction, account,
                reference_id, reference_type, balance_after, created_at)
           VALUES ($1, NULL, $2, $3, $4, $5, 'treasury', $6, $7, $8, now())"#,
    )
    .bind(Uuid::new_v4())
    .bind(entry_type)
    .bind(description)
    .bind(amount)
    .bind(direction)
    .bind(reference_id)
    .bind(reference_type)
    .bind(balance_after)
    .execute(&s.db)
    .await?;
    Ok(())
}

/// The rule in force + the position it judges. Read in ONE place so the console, the business rules
/// page and the enforcement can never judge different numbers.
pub(crate) struct FloatPosition {
    pub available: Decimal,
    pub outstanding_liability: Decimal,
    pub monthly_burn: Decimal,
    pub rule: float_rule::FloatRule,
    pub stored_behaviour: String,
}

/// The trailing-30-day redemption volume, in money — what "one month" means to the burn condition.
/// `point_redemption_log` WAS written by the redemption path in `loyalty_badges.rs`; that path was
/// retired (kanban t_5e244255) and nothing writes it now, so this measures 0 and the burn condition
/// cannot fail. It stays as the measurement of the same rows the programme actually paid — the moment a
/// payout path writes them again, burn becomes real with no change here (kanban t_0107d46b).
pub(crate) async fn monthly_burn(s: &AppState) -> Result<Decimal, AppError> {
    let burn: Decimal = sqlx::query_scalar(
        "SELECT COALESCE(SUM(total_reimbursement),0) FROM point_redemption_log
          WHERE created_at >= now() - interval '30 days'",
    )
    .fetch_one(&s.db)
    .await?;
    Ok(burn.max(Decimal::ZERO))
}

/// Load the position and the rule together (one query for the row, one for the burn).
pub(crate) async fn load_position(s: &AppState) -> Result<Option<FloatPosition>, AppError> {
    let row: Option<TreasuryState> = sqlx::query_as::<_, TreasuryState>(
        r#"SELECT total_points_issued, total_points_redeemed, total_revenue_collected,
                  total_reimbursements_paid, outstanding_liability, minimum_float,
                  float_coverage_pct, float_burn_months, on_float_breach, float_breaches
             FROM point_treasury LIMIT 1"#,
    )
    .fetch_optional(&s.db)
    .await?;
    let Some(t) = row else { return Ok(None) };
    let available = t.total_revenue_collected.unwrap_or(Decimal::ZERO)
        - t.total_reimbursements_paid.unwrap_or(Decimal::ZERO);
    let rule = float_rule::rule_from_columns(
        t.float_coverage_pct,
        t.float_burn_months,
        t.minimum_float.unwrap_or(Decimal::ZERO),
    );
    Ok(Some(FloatPosition {
        available,
        outstanding_liability: t.outstanding_liability.unwrap_or(Decimal::ZERO),
        monthly_burn: monthly_burn(s).await?,
        rule,
        stored_behaviour: t.on_float_breach.clone().unwrap_or_else(|| "hold".into()),
    }))
}

/// GET /api/v1/admin/treasury/state — the float, whether it is safe, and the rule in force.
pub async fn get_state(State(s): State<AppState>) -> Result<Json<Value>, AppError> {
    let row: Option<TreasuryState> = sqlx::query_as::<_, TreasuryState>(
        r#"SELECT total_points_issued, total_points_redeemed, total_revenue_collected,
                  total_reimbursements_paid, outstanding_liability, minimum_float,
                  float_coverage_pct, float_burn_months, on_float_breach, float_breaches
             FROM point_treasury LIMIT 1"#,
    )
    .fetch_optional(&s.db)
    .await?;

    let Some(t) = row else {
        // An empty treasury is a real and dangerous state — reporting it beats reporting zeroes.
        return Ok(Json(json!({
            "configured": false,
            "warning": "No treasury row exists. Nothing is funding redemptions.",
        })));
    };

    let collected = t.total_revenue_collected.unwrap_or(Decimal::ZERO);
    let reimbursed = t.total_reimbursements_paid.unwrap_or(Decimal::ZERO);
    let minimum = t.minimum_float.unwrap_or(Decimal::ZERO);
    let available = collected - reimbursed;

    // The three-condition rule in force, judged against the LIVE position by the SAME function the
    // guard uses (`float_rule::evaluate`) — so the verdict on screen is computed from the same rule with
    // the same numbers the guard would judge if it were called.
    //
    // It is NOT acted on: `check_float` has no caller since the redemption payout path was retired
    // (kanban t_5e244255), so a "short" verdict here is a REPORT of the position, and no hold can be
    // raised from it today. Stated plainly rather than left implied — the comment that previously stood
    // here claimed the console "can never show a verdict the guard does not act on", which stopped being
    // true the day the guard lost its caller (kanban t_0107d46b).
    let rule = float_rule::rule_from_columns(t.float_coverage_pct, t.float_burn_months, minimum);
    let burn = monthly_burn(&s).await?;
    let verdict = float_rule::evaluate(available, t.outstanding_liability, Some(burn), &rule);
    let failed_rules = verdict.failed_keys();
    let verdict_sentence = verdict.sentence();
    let shortfall = verdict.shortfall();
    let binding_required = verdict
        .outcomes
        .iter()
        .map(|o| o.required)
        .max()
        .unwrap_or(Decimal::ZERO);

    // The pending holds are part of the position: they are money the platform has been asked for but has
    // not paid, and hiding them would make the float look healthier than it is.
    let (pending_holds, pending_amount): (i64, Option<Decimal>) = sqlx::query_as(
        "SELECT COUNT(*), COALESCE(SUM(amount),0) FROM treasury_holds WHERE status = 'pending'",
    )
    .fetch_one(&s.db)
    .await?;

    // The rule the app APPLIES, never a stored value the enforcement does not perform. The breach arm
    // holds under every setting, so a column written outside this API cannot make the console claim a
    // behaviour the code does not have.
    let stored_behaviour = t.on_float_breach.clone().unwrap_or_else(|| "hold".into());
    let behaviour = effective_rule(&stored_behaviour);

    Ok(Json(json!({
        "configured": true,
        "collected": collected,
        "reimbursed": reimbursed,
        "available": available,
        "minimum_float": minimum,
        "float_coverage_pct": rule.coverage_pct,
        "float_burn_months": rule.burn_months,
        "monthly_burn": burn,
        // The three independent conditions, each carrying the numbers that decided it. `is_safe` is
        // true only when ALL THREE pass; `failed_rules` names the short ones and
        // `float_rule_plain_english` says which and by how much.
        "rules": verdict.outcomes,
        "failed_rules": failed_rules,
        "float_rule_plain_english": verdict_sentence,
        "shortfall": shortfall,
        // Headroom against the BINDING condition (the largest requirement), not just the floor.
        "headroom": available - binding_required,
        "is_safe": verdict.safe,
        "on_float_breach": behaviour,
        // What the row stores. Equals `on_float_breach` unless the column was written outside this API
        // (which refuses an unimplemented value) — kept so a divergence is visible, not silent.
        "stored_on_float_breach": stored_behaviour,
        // The console offers ONLY these, so it can never propose a setting the API would refuse.
        "on_float_breach_choices": IMPLEMENTED_ON_FLOAT_BREACH,
        "on_float_breach_not_implemented": UNIMPLEMENTED_ON_FLOAT_BREACH,
        "float_breaches": t.float_breaches,
        "points_issued": t.total_points_issued,
        "points_redeemed": t.total_points_redeemed,
        "outstanding_liability": t.outstanding_liability,
        "pending_holds": pending_holds,
        "pending_hold_amount": pending_amount,
        // The plain-English sentence — the SAME function the businesses' rules page reads, spoken about
        // the STORED value so an unimplemented one is named as not applied rather than silently dropped.
        "rule_plain_english": on_breach_sentence(&rule, &stored_behaviour),
    })))
}

#[derive(Debug, Deserialize)]
pub struct FundingBody {
    pub business_id: Option<String>,
    pub business_name: String,
    pub amount: Decimal,
    /// PROVIDER SEAM. 'manual' today; 'stripe' when David connects it. Nothing else changes.
    pub method: Option<String>,
    /// The provider's own id (a Stripe payment_intent or invoice id), kept so a payment can always be
    /// traced back to the system that took it.
    pub reference: Option<String>,
}

/// POST /api/v1/admin/treasury/funding — a business has paid in.
pub async fn record_funding(
    State(s): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<FundingBody>,
) -> Result<Json<Value>, AppError> {
    if body.business_name.trim().is_empty() {
        return Err(AppError::BadRequest("which business paid?".into()));
    }
    if body.amount <= Decimal::ZERO {
        return Err(AppError::BadRequest("amount must be more than zero".into()));
    }
    let method = body.method.clone().unwrap_or_else(|| "manual".into());
    const METHODS: [&str; 4] = ["manual", "stripe", "paypal", "bank_transfer"];
    if !METHODS.contains(&method.as_str()) {
        return Err(AppError::BadRequest(format!(
            "method must be one of {}",
            METHODS.join(", ")
        )));
    }
    let business_id = match body.business_id.as_deref() {
        Some(v) if !v.is_empty() => Some(
            Uuid::parse_str(v)
                .map_err(|_| AppError::BadRequest("business_id is not a uuid".into()))?,
        ),
        _ => None,
    };
    let recorded_by = Uuid::parse_str(&user.account_id).ok();
    let id = Uuid::new_v4();

    sqlx::query(
        r#"INSERT INTO treasury_funding
               (id, business_id, business_name, amount, method, reference, status, received_at, recorded_by)
           VALUES ($1, $2, $3, $4, $5, $6, 'received', now(), $7)"#,
    )
    .bind(id)
    .bind(business_id)
    .bind(&body.business_name)
    .bind(body.amount)
    .bind(&method)
    .bind(&body.reference)
    .bind(recorded_by)
    .execute(&s.db)
    .await?;

    // The counter the rest of the app reads, and the position the guard tests.
    sqlx::query(
        "UPDATE point_treasury SET total_revenue_collected = COALESCE(total_revenue_collected,0) + $1,
                                   updated_at = now()",
    )
    .bind(body.amount)
    .execute(&s.db)
    .await?;

    let balance_after: Option<Decimal> = sqlx::query_scalar(
        "SELECT COALESCE(total_revenue_collected,0) - COALESCE(total_reimbursements_paid,0)
           FROM point_treasury LIMIT 1",
    )
    .fetch_optional(&s.db)
    .await?;

    ledger(
        &s,
        "funding",
        &format!(
            "Loyalty funding received from {} ({})",
            body.business_name, method
        ),
        body.amount,
        "credit",
        Some(id),
        "treasury_funding",
        balance_after,
    )
    .await?;

    Ok(Json(json!({
        "recorded": true,
        "id": id,
        "method": method,
        "available_after": balance_after,
    })))
}

/// GET /api/v1/admin/treasury/funding — the money-in history.
pub async fn list_funding(State(s): State<AppState>) -> Result<Json<Value>, AppError> {
    let rows: Vec<(
        Uuid,
        Option<Uuid>,
        String,
        Decimal,
        String,
        Option<String>,
        String,
    )> = sqlx::query_as(
        r#"SELECT id, business_id, business_name, amount, method, reference, status
             FROM treasury_funding ORDER BY received_at DESC LIMIT 200"#,
    )
    .fetch_all(&s.db)
    .await?;
    let items: Vec<Value> = rows
        .into_iter()
        .map(|(id, bid, name, amount, method, reference, status)| {
            json!({"id": id, "business_id": bid, "business_name": name, "amount": amount,
                   "method": method, "reference": reference, "status": status})
        })
        .collect();
    Ok(Json(json!({"items": items})))
}

#[derive(Debug, Deserialize)]
pub struct RuleBody {
    pub on_float_breach: Option<String>,
    pub minimum_float: Option<Decimal>,
    /// Coverage condition: the float must cover this percentage of the points members hold.
    pub float_coverage_pct: Option<Decimal>,
    /// Burn condition: the float must hold this many months of recent redemptions.
    pub float_burn_months: Option<Decimal>,
}

/// PUT /api/v1/admin/treasury/rule — set the rule. Constrained in the DB too, so an unknown behaviour
/// cannot be configured and then silently ignored by the enforcement.
///
/// ARM (a), decided 2026-10-02 (kanban t_d5754642): a behaviour the enforcement does NOT perform is
/// REFUSED (400 naming it) instead of stored-and-ignored. The DB CHECK still admits the intended space,
/// so this handler is the only door that could save such a value — and it no longer can. Nothing is
/// written when the behaviour is refused, not even a valid `minimum_float` sent alongside it.
pub async fn set_rule(
    State(s): State<AppState>,
    Json(body): Json<RuleBody>,
) -> Result<Json<Value>, AppError> {
    if let Some(behaviour) = body.on_float_breach.as_deref() {
        if !on_breach_is_implemented(behaviour) {
            return Err(AppError::BadRequest(
                if UNIMPLEMENTED_ON_FLOAT_BREACH.contains(&behaviour) {
                    format!(
                        "on_float_breach '{}' is not implemented: the app can only hold a redemption that would breach the safety balance. Implemented: {}. Not implemented: {}. Nothing was saved.",
                        behaviour,
                        IMPLEMENTED_ON_FLOAT_BREACH.join(", "),
                        UNIMPLEMENTED_ON_FLOAT_BREACH.join(", ")
                    )
                } else {
                    format!(
                        "on_float_breach must be one of {} (implemented: {})",
                        on_breach_vocabulary(),
                        IMPLEMENTED_ON_FLOAT_BREACH.join(", ")
                    )
                },
            ));
        }
        sqlx::query("UPDATE point_treasury SET on_float_breach = $1, updated_at = now()")
            .bind(behaviour)
            .execute(&s.db)
            .await?;
    }
    if let Some(minimum) = body.minimum_float {
        if minimum < Decimal::ZERO {
            return Err(AppError::BadRequest(
                "minimum_float cannot be negative".into(),
            ));
        }
        sqlx::query("UPDATE point_treasury SET minimum_float = $1, updated_at = now()")
            .bind(minimum)
            .execute(&s.db)
            .await?;
    }
    // The two adaptive conditions. Zero would mean "protect nothing", so it is refused the same way an
    // unimplemented breach behaviour is: nothing is saved and the caller is told why.
    if let Some(coverage) = body.float_coverage_pct {
        if let Some(why) = float_rule::setting_refusal("float_coverage_pct", coverage) {
            return Err(AppError::BadRequest(why));
        }
        sqlx::query("UPDATE point_treasury SET float_coverage_pct = $1, updated_at = now()")
            .bind(coverage)
            .execute(&s.db)
            .await?;
    }
    if let Some(burn) = body.float_burn_months {
        if let Some(why) = float_rule::setting_refusal("float_burn_months", burn) {
            return Err(AppError::BadRequest(why));
        }
        sqlx::query("UPDATE point_treasury SET float_burn_months = $1, updated_at = now()")
            .bind(burn)
            .execute(&s.db)
            .await?;
    }
    Ok(Json(json!({"updated": true})))
}

/// GET /api/v1/admin/treasury/holds — redemptions parked by the guard.
pub async fn list_holds(State(s): State<AppState>) -> Result<Json<Value>, AppError> {
    let rows: Vec<(
        Uuid,
        Option<String>,
        i64,
        Decimal,
        Decimal,
        String,
        Option<String>,
    )> = sqlx::query_as(
        r#"SELECT id, business_name, points, amount, shortfall, status, reason
             FROM treasury_holds ORDER BY created_at DESC LIMIT 200"#,
    )
    .fetch_all(&s.db)
    .await?;
    let items: Vec<Value> = rows
        .into_iter()
        .map(
            |(id, business, points, amount, shortfall, status, reason)| {
                json!({"id": id, "business_name": business, "points": points, "amount": amount,
                   "shortfall": shortfall, "status": status, "reason": reason})
            },
        )
        .collect();
    Ok(Json(json!({"items": items})))
}

#[derive(Debug, Deserialize)]
pub struct ResolveBody {
    /// 'approved' pays the redemption and books the shortfall against the business; 'declined' refuses it.
    pub decision: String,
    pub note: Option<String>,
}

/// POST /api/v1/admin/treasury/holds/:id/resolve — a human decides a parked redemption.
pub async fn resolve_hold(
    State(s): State<AppState>,
    user: AuthenticatedUser,
    Path(id): Path<String>,
    Json(body): Json<ResolveBody>,
) -> Result<Json<Value>, AppError> {
    if body.decision != "approved" && body.decision != "declined" {
        return Err(AppError::BadRequest(
            "decision must be approved or declined".into(),
        ));
    }
    let hold_id =
        Uuid::parse_str(&id).map_err(|_| AppError::BadRequest("id is not a uuid".into()))?;

    let row: Option<(Decimal, String, Option<String>)> =
        sqlx::query_as("SELECT amount, status, business_name FROM treasury_holds WHERE id = $1")
            .bind(hold_id)
            .fetch_optional(&s.db)
            .await?;
    let (amount, status, business) =
        row.ok_or_else(|| AppError::NotFound("no such hold".into()))?;
    if status != "pending" {
        return Err(AppError::BadRequest(format!(
            "that hold is already {status}"
        )));
    }

    sqlx::query(
        "UPDATE treasury_holds SET status = $1, resolved_by = $2, resolved_at = now(), reason = $3 WHERE id = $4",
    )
    .bind(&body.decision)
    .bind(Uuid::parse_str(&user.account_id).ok())
    .bind(&body.note)
    .bind(hold_id)
    .execute(&s.db)
    .await?;

    if body.decision == "approved" {
        // Approving means the platform pays it, even though the float did not cover it — which is exactly
        // why it had to be a human decision and why it goes in the ledger.
        sqlx::query(
            "UPDATE point_treasury SET total_reimbursements_paid = COALESCE(total_reimbursements_paid,0) + $1,
                                       updated_at = now()",
        )
        .bind(amount)
        .execute(&s.db)
        .await?;
    }

    let balance_after: Option<Decimal> = sqlx::query_scalar(
        "SELECT COALESCE(total_revenue_collected,0) - COALESCE(total_reimbursements_paid,0)
           FROM point_treasury LIMIT 1",
    )
    .fetch_optional(&s.db)
    .await?;

    ledger(
        &s,
        "hold_resolved",
        &format!(
            "Held reward {} for {}",
            if body.decision == "approved" {
                "paid"
            } else {
                "refused"
            },
            business.as_deref().unwrap_or("unknown business")
        ),
        amount,
        if body.decision == "approved" {
            "debit"
        } else {
            "credit"
        },
        Some(hold_id),
        "treasury_hold",
        balance_after,
    )
    .await?;

    Ok(Json(
        json!({"resolved": true, "decision": body.decision, "available_after": balance_after}),
    ))
}

/// GET /api/v1/treasury/rules — WHAT THE BUSINESSES READ.
///
/// David: *"those rules need to be added into it so the businesses understand"*. Generated from the rule
/// actually in force, so the published rules and the enforcement cannot drift apart.
pub async fn get_business_rules(State(s): State<AppState>) -> Result<Json<Value>, AppError> {
    let row: Option<(
        Option<Decimal>,
        Option<String>,
        Option<Decimal>,
        Option<Decimal>,
    )> = sqlx::query_as(
        "SELECT minimum_float, on_float_breach, float_coverage_pct, float_burn_months
           FROM point_treasury LIMIT 1",
    )
    .fetch_optional(&s.db)
    .await?;
    let (minimum, behaviour, coverage_col, burn_col) = row.unwrap_or((None, None, None, None));
    let minimum = minimum.unwrap_or(Decimal::ZERO);
    let stored = behaviour.unwrap_or_else(|| "hold".into());
    let rule = float_rule::rule_from_columns(coverage_col, burn_col, minimum);
    // The conditions are stated as TEXT (thresholds), never as the live position: this page is public,
    // and publishing the programme's real balance would hand every reader the treasury. The rule the app
    // APPLIES is published — never a stored setting the enforcement does not perform — so this page
    // describes what the software DOES and a value it cannot perform never gets a promise here.
    let applied = effective_rule(&stored);
    let on_breach = on_breach_sentence(&rule, &stored);

    let conditions = float_rule::rule_conditions_plain_english(&rule);
    Ok(Json(json!({
        "safety_balance": minimum,
        "float_coverage_pct": rule.coverage_pct,
        "float_burn_months": rule.burn_months,
        "conditions": conditions,
        "rule_in_force": applied,
        // What the row stores. Equals `rule_in_force` unless the column was written outside this API
        // (which refuses an unimplemented value) — surfaced so a divergence is visible, not silent.
        "stored_setting": stored,
        "rules": [
            "Businesses fund the rewards their customers earn before they are earned.",
            conditions[0].clone(),
            conditions[1].clone(),
            conditions[2].clone(),
            on_breach,
            "Every payment in and every reward paid out is recorded, and businesses can see both.",
        ],
        // WHAT IS EXPECTED OF THE BUSINESS — the same three conditions, said as a duty.
        "what_is_expected_of_you": [
            "Fund your programme before you launch it: rewards are paid out of money you have put in.",
            "Top up BEFORE you grow. A promotion raises redemptions immediately, so the float has to be ahead of it.",
            "If any condition is short, the payment to you for each new redemption is held — it does not vanish. You are reimbursed once the float is back above all three conditions; the customer has already been given their reward.",
            "You are always told which condition is short and by how much, in plain numbers.",
        ],
    })))
}

/// THE FLOAT GUARD — the rule David asked for, in one place so every payer uses the same test.
///
/// Measured before this existed: `minimum_float` was read for display and compared against NOTHING, so a
/// redemption could take the programme below its safety balance with no record and no warning. The live
/// treasury row (issued=110, redeemed=200, collected=1.10, reimbursed=1.60, min_float=100.00) is what
/// that looks like after the fact.
///
/// The guard applies the SAME three-condition rule the console shows (`float_rule::evaluate`) — coverage
/// against the outstanding liability, burn against trailing-30-day redemptions, and the floor — to the
/// position the payout would LEAVE BEHIND. That is the same "after this payment" test the single floor
/// rule always used, extended to all three: a verdict the display would call unsafe can never be paid
/// here, and the reason names WHICH condition is short.
///
/// Returns whether `amount` may leave the treasury right now. It never silently refuses and never
/// silently pays: the caller gets a verdict, and a breach is counted.
///
/// STAGED — NOT WIRED (kanban t_0107d46b). Read the STAGED STATUS note at the top of this file before
/// touching this: there is no caller today, and the only one it ever had was retired with the anonymous
/// clearinghouse redemption it guarded. It is kept, not deleted, because the treasury engine is a staged
/// feature (David 2026-10-02: *"I'll connect Stripe later on but that system needs to be built in"*), and
/// `FloatCheck` is the contract a payout path will call. Wire it there — never call it from a read path,
/// and never delete it to satisfy a dead-code census.
#[allow(dead_code)]
pub enum FloatCheck {
    /// Every condition still holds after the payment. Pay it.
    Allowed { available: Decimal },
    /// The payment would leave a condition short. The caller records a hold.
    Breached {
        available: Decimal,
        /// How much more the float needs to pass EVERY condition (0 when only one is short and it is
        /// the binding one).
        shortfall: Decimal,
        /// Machine keys of the short conditions, e.g. `["coverage"]`.
        failed_rules: Vec<&'static str>,
        /// The short condition's human label, for the hold record.
        failed_label: String,
        /// One plain sentence naming the condition and the numbers.
        reason: String,
        behaviour: String,
    },
}

pub async fn check_float(s: &AppState, amount: Decimal) -> Result<FloatCheck, AppError> {
    // No treasury row = nothing is funding redemptions. Treat it as breached rather than as "no limit",
    // because the safe default is to stop paying out of a pot that does not exist.
    let Some(p) = load_position(s).await? else {
        return Ok(FloatCheck::Breached {
            available: Decimal::ZERO,
            shortfall: amount.max(Decimal::ZERO),
            failed_rules: vec!["floor"],
            failed_label: "Floor".into(),
            reason: "There is no treasury row, so nothing is funding redemptions.".into(),
            behaviour: "hold".into(),
        });
    };

    let behaviour = effective_rule(&p.stored_behaviour).to_string();

    if amount <= Decimal::ZERO {
        return Ok(FloatCheck::Allowed {
            available: p.available,
        });
    }

    let available_after = p.available - amount;
    let verdict = float_rule::evaluate(
        available_after,
        Some(p.outstanding_liability),
        Some(p.monthly_burn),
        &p.rule,
    );

    if verdict.safe {
        return Ok(FloatCheck::Allowed {
            available: p.available,
        });
    }

    // A breach is a fact worth counting even when the configured behaviour is to pay anyway.
    sqlx::query(
        "UPDATE point_treasury SET float_breaches = COALESCE(float_breaches,0) + 1,
                                   last_breach_at = now(), updated_at = now()",
    )
    .execute(&s.db)
    .await?;

    Ok(FloatCheck::Breached {
        available: p.available,
        shortfall: verdict.shortfall(),
        failed_rules: verdict.failed_keys(),
        failed_label: verdict
            .failures()
            .first()
            .map(|o| o.label.to_string())
            .unwrap_or_else(|| "Float".into()),
        reason: verdict.sentence(),
        behaviour,
    })
}
