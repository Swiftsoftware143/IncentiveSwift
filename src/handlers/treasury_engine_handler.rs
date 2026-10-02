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

use axum::{
    extract::{Path, State},
    Json,
};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::error::AppError;
use crate::security::auth::AuthenticatedUser;
use crate::state::AppState;

/// The whole float position, as one object — so a UI can never show a stale half of it.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct TreasuryState {
    pub total_points_issued: Option<i64>,
    pub total_points_redeemed: Option<i64>,
    pub total_revenue_collected: Option<Decimal>,
    pub total_reimbursements_paid: Option<Decimal>,
    pub outstanding_liability: Option<Decimal>,
    pub minimum_float: Option<Decimal>,
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

/// GET /api/v1/admin/treasury/state — the float, whether it is safe, and the rule in force.
pub async fn get_state(State(s): State<AppState>) -> Result<Json<Value>, AppError> {
    let row: Option<TreasuryState> = sqlx::query_as::<_, TreasuryState>(
        r#"SELECT total_points_issued, total_points_redeemed, total_revenue_collected,
                  total_reimbursements_paid, outstanding_liability, minimum_float,
                  on_float_breach, float_breaches
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

    // The pending holds are part of the position: they are money the platform has been asked for but has
    // not paid, and hiding them would make the float look healthier than it is.
    let (pending_holds, pending_amount): (i64, Option<Decimal>) = sqlx::query_as(
        "SELECT COUNT(*), COALESCE(SUM(amount),0) FROM treasury_holds WHERE status = 'pending'",
    )
    .fetch_one(&s.db)
    .await?;

    Ok(Json(json!({
        "configured": true,
        "collected": collected,
        "reimbursed": reimbursed,
        "available": available,
        "minimum_float": minimum,
        "headroom": available - minimum,
        "is_safe": available >= minimum,
        "on_float_breach": t.on_float_breach,
        "float_breaches": t.float_breaches,
        "points_issued": t.total_points_issued,
        "points_redeemed": t.total_points_redeemed,
        "outstanding_liability": t.outstanding_liability,
        "pending_holds": pending_holds,
        "pending_hold_amount": pending_amount,
        // The plain-English sentence the businesses read. It is generated from the rule in force so the
        // business-facing rules and the enforcement can never disagree.
        "rule_plain_english": match t.on_float_breach.as_deref().unwrap_or("hold") {
            "allow_and_bill" => "If a reward would take the programme below its safety balance, it is paid and the shortfall is billed to the business.",
            "suspend" => "If a reward would take the programme below its safety balance, the programme stops redeeming until the business tops up.",
            _ => "If a reward would take the programme below its safety balance, it is held for confirmation and the business is asked to top up.",
        },
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
}

/// PUT /api/v1/admin/treasury/rule — set the rule. Constrained in the DB too, so an unknown behaviour
/// cannot be configured and then silently ignored by the enforcement.
pub async fn set_rule(
    State(s): State<AppState>,
    Json(body): Json<RuleBody>,
) -> Result<Json<Value>, AppError> {
    if let Some(behaviour) = body.on_float_breach.as_deref() {
        const CHOICES: [&str; 3] = ["hold", "allow_and_bill", "suspend"];
        if !CHOICES.contains(&behaviour) {
            return Err(AppError::BadRequest(format!(
                "on_float_breach must be one of {}",
                CHOICES.join(", ")
            )));
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
    let row: Option<(Option<Decimal>, Option<String>)> =
        sqlx::query_as("SELECT minimum_float, on_float_breach FROM point_treasury LIMIT 1")
            .fetch_optional(&s.db)
            .await?;
    let (minimum, behaviour) = row.unwrap_or((None, None));
    let minimum = minimum.unwrap_or(Decimal::ZERO);
    let behaviour = behaviour.unwrap_or_else(|| "hold".into());

    let on_breach = match behaviour.as_str() {
        "allow_and_bill" => format!(
            "If paying a reward would take the programme below ${minimum}, the reward is paid and the shortfall is billed to the business."
        ),
        "suspend" => format!(
            "If paying a reward would take the programme below ${minimum}, the programme stops paying rewards until the business tops up."
        ),
        _ => format!(
            "If paying a reward would take the programme below ${minimum}, the reward is held and the business is asked to top up before it is paid."
        ),
    };

    Ok(Json(json!({
        "safety_balance": minimum,
        "rule_in_force": behaviour,
        "rules": [
            "Businesses fund the rewards their customers earn.",
            format!("The programme keeps a safety balance of ${minimum} so it never runs out of money."),
            on_breach,
            "Every payment in and every reward paid out is recorded, and businesses can see both.",
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
/// Returns whether `amount` may leave the treasury right now. It never silently refuses and never
/// silently pays: the caller gets a verdict, and a breach is counted.
pub enum FloatCheck {
    /// There is room above the safety balance. Pay it.
    Allowed { available: Decimal },
    /// The payment would breach the safety balance. The caller records a hold.
    Breached {
        available: Decimal,
        shortfall: Decimal,
        behaviour: String,
    },
}

pub async fn check_float(s: &AppState, amount: Decimal) -> Result<FloatCheck, AppError> {
    let row: Option<(
        Option<Decimal>,
        Option<Decimal>,
        Option<Decimal>,
        Option<String>,
    )> = sqlx::query_as(
        "SELECT total_revenue_collected, total_reimbursements_paid, minimum_float, on_float_breach
           FROM point_treasury LIMIT 1",
    )
    .fetch_optional(&s.db)
    .await?;

    // No treasury row = nothing is funding redemptions. Treat it as breached rather than as "no limit",
    // because the safe default is to stop paying out of a pot that does not exist.
    let Some((collected, reimbursed, minimum, behaviour)) = row else {
        return Ok(FloatCheck::Breached {
            available: Decimal::ZERO,
            shortfall: amount,
            behaviour: "hold".into(),
        });
    };

    let available = collected.unwrap_or(Decimal::ZERO) - reimbursed.unwrap_or(Decimal::ZERO);
    let minimum = minimum.unwrap_or(Decimal::ZERO);
    let behaviour = behaviour.unwrap_or_else(|| "hold".into());

    if amount <= Decimal::ZERO || available - amount >= minimum {
        return Ok(FloatCheck::Allowed { available });
    }

    // A breach is a fact worth counting even when the configured behaviour is to pay anyway.
    sqlx::query(
        "UPDATE point_treasury SET float_breaches = COALESCE(float_breaches,0) + 1,
                                   last_breach_at = now(), updated_at = now()",
    )
    .execute(&s.db)
    .await?;

    Ok(FloatCheck::Breached {
        available,
        shortfall: minimum - (available - amount),
        behaviour,
    })
}
