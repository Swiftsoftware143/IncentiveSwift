//! Loyalty verification & voucher handlers
//! Voucher claim/redeem, business pledges
//!
//! RETIRED from this module: `generate_pin` and `issue_voucher` (kanban t_b209d263 — both
//! anonymous and unscoped), then `verify_purchase` and `issue_rotation_voucher` (kanban
//! t_7a16bf0b — the guarded reader of `purchase_verifications` and its private voucher
//! issuer, left writer-less by t_b209d263), then the rotation-group CRUD family (kanban
//! t_8e9d3a52 — the last writers of `rotation_configs` / `rotation_group_members`), then the
//! anonymous `survey_response` $50 voucher mint (kanban t_3bde2e27 — `vouchers`' last writer), then
//! the remaining voucher read surface (`get_vouchers` / `list_my_vouchers` / `claim_voucher`) with
//! the `vouchers` table itself (kanban t_30dfc98c — 0 writers fleet-wide, 0 rows ever).
//! See the retirement notes at each section.

use axum::{
    extract::{Path, State},
    response::IntoResponse,
    Json,
};
use rand::Rng;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::error::AppError;
use crate::security::auth::AuthenticatedUser;
use crate::state::AppState;

// ── Purchase Verification ──
//
// Both arms that lived here are RETIRED:
//   * `generate_pin` (kanban t_b209d263) — an anonymous 4-character PIN minter that inserted a
//     `pending` row into `purchase_verifications` for ANY active campaign with a caller-chosen
//     `business_id`/`business_name`/`purchase_amount`, and the SOLE writer of that table.
//   * `verify_purchase` + `VerifyPurchaseRequest` (kanban t_7a16bf0b) — the
//     `AuthenticatedUser`-guarded reader of `purchase_verifications`. It was correctly guarded
//     (it scoped the contact through `contact_tenants`), but with its only producer gone it could
//     never find a `pending` row: dead for every caller, `404 {"error":"Invalid or expired PIN"}`
//     forever. It was also `issue_rotation_voucher`'s only caller.
//   Removed rather than repurposed: re-adding a PIN generator would re-add the anonymous mint
//   t_b209d263 retired, and a contact cannot authenticate in this app, so there is no credential
//   to scope such a generator to.
//
// The live purchase-verification flow is `purchase_verify` below, which validates the caller's
// OWN `accounts.purchase_pin` — what the tenant console and the served guide describe.

// ── Voucher Engine — RETIRED ──
//
// `issue_voucher` (and the `IssueVoucherRequest` it took) was RETIRED here (kanban
// t_b209d263): an anonymous, unscoped value-mint — the caller chose the campaign slug, the
// recipient contact AND the discount value, and an uncredentialed request inserted a live
// `vouchers` row. Its only fleet caller was MultiDirectory's
// `tag_automation.rs::execute_voucher_action`, and that call path is retired (MD retired the
// IncentiveSwift loyalty integration on 2026-09-23: `service="incentiveswift"` answers 400;
// `tag_rules` has 0 rows ever and the MD console's tag-rule form does not offer the action).
// `verify_purchase` and its private issuer `issue_rotation_voucher` were RETIRED in kanban
// t_7a16bf0b (see the notes above).
//
// The REMAINING voucher surface — the `claim_voucher` writer (keyed by a claim code, a bearer
// secret), the `AuthenticatedUser`-scoped `list_my_vouchers` and `get_vouchers` readers — was
// RETIRED in kanban t_30dfc98c, and `vouchers` was dropped in
// migrations/20261004_drop_vouchers.sql. Measured on the deployed binary 2f06f423413bc2a9: the
// last producer (`survey_response`) went in t_3bde2e27, no `vouchers` writer survives in the
// 8-app fleet, the table held 0 rows EVER, and the surface was advertised only by the console's
// own Vouchers tab (the single browser hit these paths ever saw) plus the served guides. README
// for the decision: /opt/swift/audits/t_30dfc98c/EVIDENCE.md.

// ── Business Pledges ──
#[derive(Debug, Deserialize)]
pub struct ApprovePledgeRequest {
    pub status: String, // "approved" or "rejected"
    pub admin_id: Option<Uuid>,
}

/// POST /api/v1/admin/pledges/:id/review — approve or reject a pledge
pub async fn review_pledge(
    State(s): State<AppState>,
    Path(pledge_id): Path<Uuid>,
    Json(req): Json<ApprovePledgeRequest>,
) -> Result<impl IntoResponse, AppError> {
    let valid_statuses = ["approved", "rejected"];
    if !valid_statuses.contains(&req.status.as_str()) {
        return Err(AppError::BadRequest(
            "Status must be 'approved' or 'rejected'".into(),
        ));
    }

    sqlx::query(
        "UPDATE business_pledges SET status = $1, reviewed_by = $2, reviewed_at = NOW() WHERE id = $3 AND status = 'pending'"
    )
    .bind(&req.status)
    .bind(req.admin_id)
    .bind(pledge_id)
    .execute(&s.db)
    .await?;

    Ok(Json(json!({"status": req.status})))
}

/// GET /api/v1/admin/pledges — list pending pledges
pub async fn list_pending_pledges(
    State(s): State<AppState>,
) -> Result<impl IntoResponse, AppError> {
    let pledges = sqlx::query_as::<_, (Uuid, String, String, String, String, String, String)>(
        "SELECT id, business_name, offer_type, offer_value, offer_description, business_phone, status
         FROM business_pledges WHERE status = 'pending' ORDER BY created_at ASC"
    )
    .fetch_all(&s.db)
    .await?;

    let result: Vec<serde_json::Value> = pledges
        .into_iter()
        .map(|p| {
            json!({
                "id": p.0, "business": p.1, "offer_type": p.2, "offer_value": p.3,
                "description": p.4, "phone": p.5, "status": p.6
            })
        })
        .collect();

    Ok(Json(json!({"pending_pledges": result})))
}

// ── Rotation Engine (retired) ──
//
// `issue_rotation_voucher` was RETIRED here (kanban t_7a16bf0b): the private issuer that picked
// the next non-competing business in a rotation group and INSERTed a live `vouchers` row. Its ONLY
// caller was `verify_purchase` (retired in the same pass) and it had no route of its own, so it was
// live code with no way in.
//
// Its INSERT was one of `vouchers`'s two writers. The other, `survey_response`, was itself
// RETIRED in kanban t_3bde2e27 (see the note at the foot of this module) — `vouchers` now has
// three mounted readers and no writer.
//
// The rotation-group CRUD below was RETIRED in kanban t_8e9d3a52 (the five /admin/rotation-configs
// and /admin/rotation-members arms + `CreateRotationConfigRequest` / `AddToRotationRequest`). They
// were the last writers of `rotation_configs` / `rotation_group_members` (0 rows ever), nothing
// consumed what they wrote, and one arm could never have worked: `add_rotation_member`'s
// `ON CONFLICT (rotation_config_id, business_id)` has no matching unique constraint on the live
// table, so it answered 500 for every caller. The tables are now orphaned; their drop is a
// separate card. `admin_guard` (path-based over /api/v1/admin/*) covered all five — they were NOT
// anonymous, contrary to the card. Proof: /opt/swift/audits/t_8e9d3a52/proof.py.

/// GET /api/v1/loyalty/rewards-earned/:contact_id — list rewards earned by a contact
pub async fn list_rewards_earned(
    State(s): State<AppState>,
    auth: AuthenticatedUser,
    Path(contact_id): Path<Uuid>,
) -> Result<Json<Value>, AppError> {
    // SECURITY (kanban t_f08d32e7): this arm took no `AuthenticatedUser` at all, so any anonymous
    // caller who named a contact id got that contact's reward history. It is now scoped to the
    // caller: the caller is linked to the contact through the `contact_tenants` boundary
    // (kanban t_369cb159), or owns the loyalty programme the rewards were earned in
    // (`loyalty_programs -> campaigns.account_id`). No role bypass is invented.
    let account_id = Uuid::parse_str(&auth.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".into()))?;
    if !caller_may_read_contact_rewards(&s.db, &contact_id, &account_id).await? {
        return Err(AppError::NotFound("Contact not found".into()));
    }

    let rewards = sqlx::query_as::<
        _,
        (
            Uuid,
            String,
            i32,
            String,
            Option<chrono::DateTime<chrono::Utc>>,
        ),
    >(
        // Canonical shape (member_id, tier_id, earned_at); the points reported for a reward are
        // the tier's own point cost. The previous statement named four columns this table never
        // had (plain-statement drift, kanban t_cf7469bb) and 500'd on every call.
        r#"SELECT lre.id, lrt.name, lrt.points_required, COALESCE(lre.status, 'pending') AS status, lre.earned_at
           FROM loyalty_rewards_earned lre
           JOIN loyalty_reward_tiers lrt ON lrt.id = lre.tier_id
           JOIN loyalty_members lm ON lm.id = lre.member_id
           WHERE lm.contact_id = $1
           ORDER BY lre.earned_at DESC LIMIT 50"#,
    )
    .bind(contact_id)
    .fetch_all(&s.db)
    .await?;

    let result: Vec<Value> = rewards
        .into_iter()
        .map(|r| {
            json!({
                "id": r.0, "reward": r.1, "points": r.2, "status": r.3, "earned_at": r.4
            })
        })
        .collect();

    Ok(Json(json!({"rewards": result})))
}

/// May `account_id` read `contact_id`'s reward history? True when the account is linked to the
/// contact (the `contact_tenants` visibility boundary, kanban t_369cb159) or owns a loyalty
/// programme the contact is a member of (programme -> campaign -> account). The second arm keeps
/// the programme owner's own console working even when a member was enrolled by a path that did
/// not write a link row.
async fn caller_may_read_contact_rewards(
    pool: &sqlx::PgPool,
    contact_id: &Uuid,
    account_id: &Uuid,
) -> Result<bool, AppError> {
    if crate::db::contacts::contact_visible_to(pool, contact_id, account_id).await? {
        return Ok(true);
    }
    let owns: bool = sqlx::query_scalar(
        // The SAME three-arm ownership predicate the rest of this app uses (see
        // `loyalty_badges::ensure_member_in_caller_programme`): `lp.account_id` is the explicit
        // owner, `lp.campaign_id` is the reverse pointer the console's enrol path may leave NULL,
        // and `campaigns.loyalty_program_id` is the canonical FORWARD link `POST /loyalty/checkin`
        // resolves FIRST. Joining only the reverse pointer answered this read empty-handed for a
        // member of a forward-linked programme (fixed 2026-11-09; the same defect the member card
        // carried until 2026-10-19).
        "SELECT EXISTS (SELECT 1 FROM loyalty_members lm \
           JOIN loyalty_programs lp ON lp.id = lm.program_id \
          WHERE lm.contact_id = $1 \
            AND (lp.account_id = $2 \
                 OR EXISTS (SELECT 1 FROM campaigns c \
                             WHERE c.id = lp.campaign_id AND c.account_id = $2) \
                 OR EXISTS (SELECT 1 FROM campaigns fwd \
                             WHERE fwd.loyalty_program_id = lp.id AND fwd.account_id = $2)))",
    )
    .bind(contact_id)
    .bind(account_id)
    .fetch_one(pool)
    .await?;
    Ok(owns)
}

// ── Purchase Verify (business-scanner auto-credit) ────────────────────────

#[derive(Debug, Deserialize)]
pub struct PurchaseVerifyRequest {
    pub contact_id: Uuid,
    pub amount: f64, // Total receipt amount (for logging/display)
    pub pin: String,
    pub offer_id: Option<Uuid>,
    pub subtotal_amount: Option<f64>, // Portion eligible for earning (excludes deal items)
    pub deal_description: Option<String>, // Human-readable: "Free dessert — rest of meal earns"
    #[serde(default)]
    pub transaction_category: Option<String>, // b2c, b2b_supplies, b2b_services, events
}

/// POST /api/v1/loyalty/purchase/verify
/// Business scans customer QR code, enters PIN, and this endpoint
/// verifies the purchase and auto-credits the customer.
///
/// Credits awarded:
///   - If `subtotal_amount` is set: earned = subtotal_amount × credit_rate
///   - Otherwise: earned = amount × credit_rate (full receipt)
///
/// This allows businesses to offer deals ("free dessert", "10% off item")
/// while still giving customers ZaarCash on the portion they actually paid for.
///
/// If offer_id is provided: deduction is calculated from the offer's cap,
/// applied against the customer's balance.
pub async fn purchase_verify(
    State(s): State<AppState>,
    auth: AuthenticatedUser,
    Json(req): Json<PurchaseVerifyRequest>,
) -> Result<impl IntoResponse, AppError> {
    let business_account_id = Uuid::parse_str(&auth.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".into()))?;

    // Get the account's tenant_id and purchase_pin
    let account_info = sqlx::query_as::<_, (Option<Uuid>, String)>(
        "SELECT tenant_id, purchase_pin FROM accounts WHERE id = $1",
    )
    .bind(business_account_id)
    .fetch_optional(&s.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Business account not found".into()))?;

    let (tenant_id, purchase_pin) = account_info;

    // Validate PIN against the business tenant's stored purchase_pin
    if req.pin.trim() != purchase_pin.as_str() {
        return Err(AppError::BadRequest("Invalid PIN".into()));
    }

    // Check loyalty plan status for pool gating
    let loyalty_plan_status: Option<String> =
        sqlx::query_scalar("SELECT loyalty_plan_status FROM accounts WHERE id = $1")
            .bind(business_account_id)
            .fetch_optional(&s.db)
            .await?
            .flatten();

    // If business has a loyalty plan but it's not active, block purchase verification
    let loyalty_plan: Option<String> =
        sqlx::query_scalar("SELECT loyalty_plan FROM accounts WHERE id = $1")
            .bind(business_account_id)
            .fetch_optional(&s.db)
            .await?
            .flatten();

    if loyalty_plan.is_some() && loyalty_plan_status.as_deref() != Some("active") {
        return Err(AppError::BadRequest(
            "Business loyalty plan is not active. Please subscribe to continue earning ZaarCash."
                .into(),
        ));
    }

    // Read credit_rate from accounts (tenant) table.
    // credit_rate = credits earned per $1 spent (default 1 credit/dollar).
    // e.g. rate=2 means $25 purchase earns 50 credits.
    let credit_rate: i32 = if let Some(tid) = tenant_id {
        sqlx::query_scalar("SELECT credit_rate FROM accounts WHERE id = $1")
            .bind(tid)
            .fetch_optional(&s.db)
            .await?
            .unwrap_or(1)
    } else {
        1 // default: 1 credit per $1
    };

    // Verify the contact belongs to the calling business (contact_tenants, kanban t_369cb159).
    let contact_exists: bool =
        crate::db::contacts::contact_visible_to(&s.db, &req.contact_id, &business_account_id)
            .await?;

    if !contact_exists {
        return Err(AppError::NotFound("Customer contact not found".into()));
    }

    // ── Read tenant-level ZaarCash guardrails ───────────────────────────
    let (tenant_redemption_cap_pct, tenant_min_redemption): (i32, i32) =
        if let Some(tid) = tenant_id {
            sqlx::query_as::<_, (i32, i32)>(
                "SELECT redemption_cap_pct, min_redemption_credits FROM accounts WHERE id = $1",
            )
            .bind(tid)
            .fetch_optional(&s.db)
            .await?
            .unwrap_or((10, 100))
        } else {
            (10, 100) // defaults: 10% cap, 100 ZC min to redeem
        };

    // ZC value: 100 ZaarCash = $1 (1 cent per credit)
    const ZC_PER_DOLLAR: i32 = 100;

    // ── The owner's two redemption guardrails, ENFORCED ─────────────
    // `redemption_cap_pct` and `min_redemption_credits` are what the owner sets on the loyalty
    // screen, and that screen tells them verbatim: "redemptions now follow N% of the bill,
    // minimum M points", "the rest of the bill must be paid in money", "0 stops point redemption
    // without stopping earning", and "a member cannot redeem fewer points than this". Until now
    // both numbers were only DISPLAYED (`max_redeemable_this_visit`) and the floor was bound at
    // the top of this function and never read, so an offer could redeem any amount of a bill and
    // the visible number and the enforced one were different again. The line below is the most
    // points may pay on THIS bill.
    let cap_credits_this_bill: i32 =
        ((req.amount.max(0.0) * tenant_redemption_cap_pct as f64 / 100.0) * ZC_PER_DOLLAR as f64)
            .floor() as i32;

    // ── Handle offer-based redemption (business applies a named offer) ──
    let mut redeemed_credits: i32 = 0;
    let mut offer_name: Option<String> = None;
    // Set when the offer cannot be redeemed under the owner's guardrails. The PURCHASE still
    // completes and still earns (the screen promises "0 stops point redemption without stopping
    // earning"); only the redemption is withheld, and the answer says why.
    let mut offer_refused: Option<String> = None;

    if let Some(oid) = req.offer_id {
        let offer = sqlx::query_as::<_, (String, i32, i32, bool)>(
            "SELECT name, discount_percent, cap_dollars, active FROM offers WHERE id = $1 AND tenant_id = $2"
        )
        .bind(oid)
        .bind(tenant_id.unwrap_or(business_account_id))
        .fetch_optional(&s.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Offer not found".into()))?;

        let (o_name, discount_pct, cap_dollars, active) = offer;
        if !active {
            return Err(AppError::BadRequest("Offer is no longer active".into()));
        }

        offer_name = Some(o_name);

        // Cap in ZC: cap_dollars converted to credits (e.g. $5 off = 500 ZC)
        let max_discount_credits = cap_dollars * ZC_PER_DOLLAR;

        let customer_balance: i32 =
            sqlx::query_scalar("SELECT credits_balance FROM accounts WHERE id = $1")
                .bind(req.contact_id)
                .fetch_optional(&s.db)
                .await?
                .unwrap_or(0);

        // Every reason to withhold the redemption is handled in ONE chain: the customer's own
        // balance first, then the owner's two guardrails. Each refusal withholds the REDEMPTION
        // only -- the purchase still completes and still earns, which is exactly what the loyalty
        // screen promises when it says "0 stops point redemption without stopping earning". The
        // balance arm used to return 400 and fail the whole sale, so a customer a few points short
        // cost the business the earned credits as well.
        if customer_balance < max_discount_credits {
            offer_refused = Some(format!(
                "The customer has {} points, not enough for this offer ({} points). The points were NOT taken and the rest of the bill must be paid in money.",
                customer_balance, max_discount_credits
            ));
        } else if max_discount_credits > cap_credits_this_bill {
            offer_refused = Some(format!(
                "This offer would take ${:.2} off in points, more than this business allows on one bill: {}% of a ${:.2} bill is ${:.2}. The points were NOT taken and the rest of the bill must be paid in money.",
                max_discount_credits as f64 / ZC_PER_DOLLAR as f64,
                tenant_redemption_cap_pct,
                req.amount,
                cap_credits_this_bill as f64 / ZC_PER_DOLLAR as f64
            ));
        } else if max_discount_credits < tenant_min_redemption {
            offer_refused = Some(format!(
                "This offer redeems {} points, below the smallest redemption this business allows ({} points). The points were NOT taken.",
                max_discount_credits, tenant_min_redemption
            ));
        } else {
            redeemed_credits = max_discount_credits;
        }
    }

    // Calculate earned credits using configurable credit_rate.
    // Use subtotal_amount if provided (portion eligible for earning minus deal items).
    // Otherwise, use the full amount.
    let earnable_amount = req.subtotal_amount.unwrap_or(req.amount);
    let credit_amount = ((earnable_amount.max(0.0) * credit_rate as f64).floor() as i32).max(0);
    // If no earnable amount (e.g. fully redeemed deal), still award minimum 1 point
    let credit_amount = if credit_amount == 0 && earnable_amount > 0.0 {
        1
    } else {
        credit_amount
    };
    let deal_discount = req.amount - earnable_amount;

    // ── The loyalty-member ledger (2026-11-19) ─────────────────────────────
    // A member's points live in `loyalty_members` (the check-in engine writes them) and a member's
    // `contact_id` is a `contacts` row — NOT an `accounts` row. Measured on live: 1 member, 0 with an
    // `accounts` row at that id. Until this block the only ledger this route knew was
    // `accounts.credits_balance`, so a scanned purchase for a real loyalty member answered 200 with
    // `status:"contact_linked"`, `credits_earned:0` and "Register to earn!" — and credited NOTHING
    // while the member's balance stayed put. Credit the member's own ledger first; the account arm
    // below still serves a contact that really is an account.
    let member_row: Option<(Uuid, Uuid, bool)> = sqlx::query_as(
        "SELECT lm.id, lm.program_id, lp.tiers_enabled \
           FROM loyalty_members lm \
           JOIN loyalty_programs lp ON lp.id = lm.program_id \
          WHERE lm.contact_id = $1 \
            AND (lp.account_id = $2 \
                 OR EXISTS (SELECT 1 FROM campaigns c WHERE c.id = lp.campaign_id AND c.account_id = $2) \
                 OR EXISTS (SELECT 1 FROM campaigns f WHERE f.loyalty_program_id = lp.id AND f.account_id = $2)) \
          ORDER BY lm.member_since DESC NULLS LAST \
          LIMIT 1",
    )
    .bind(req.contact_id)
    .bind(business_account_id)
    .fetch_optional(&s.db)
    .await?;

    if let Some((member_id, program_id, tiers_enabled)) = member_row {
        // The same ZaarCash pool gate the account arm applies, so a member credit cannot outrun the
        // business's plan allowance.
        let zc_pool: i32 =
            sqlx::query_scalar("SELECT COALESCE(zc_pool_remaining, 0) FROM accounts WHERE id = $1")
                .bind(business_account_id)
                .fetch_optional(&s.db)
                .await?
                .unwrap_or(0);
        if zc_pool > 0 && zc_pool < credit_amount {
            return Err(AppError::BadRequest(format!(
                "Business ZC pool too low. Pool has {} ZC remaining — this transaction awards {} ZC.",
                zc_pool, credit_amount
            )));
        }
        if loyalty_plan_status.as_deref() == Some("active") {
            let _ = sqlx::query(
                "UPDATE accounts SET zc_pool_remaining = zc_pool_remaining - $1 WHERE id = $2 AND zc_pool_remaining >= $1",
            )
            .bind(credit_amount)
            .bind(business_account_id)
            .execute(&s.db)
            .await;
        }

        let new_points: i32 = sqlx::query_scalar(
            "UPDATE loyalty_members \
                SET points_balance = COALESCE(points_balance, 0) + $1, \
                    lifetime_points = COALESCE(lifetime_points, 0) + $1, \
                    last_activity_date = now() \
              WHERE id = $2 \
          RETURNING COALESCE(points_balance, 0)",
        )
        .bind(credit_amount)
        .bind(member_id)
        .fetch_one(&s.db)
        .await?;

        // The member-facing audit trail: the roster and the member's own activity list read this row.
        let description = if deal_discount > 0.01 {
            format!(
                "Purchase verified — {} points on ${:.2} spend (${:.2} deal excluded{})",
                credit_amount,
                earnable_amount,
                deal_discount,
                req.deal_description
                    .as_deref()
                    .map(|n| format!(": {}", n))
                    .unwrap_or_default()
            )
        } else {
            format!(
                "Purchase verified — {} points on ${:.2} spend",
                credit_amount, earnable_amount
            )
        };
        crate::mechanics::loyalty_checkin::record_activity(
            &s,
            &member_id.to_string(),
            "purchase",
            &description,
            credit_amount,
        )
        .await?;
        // Keep the roster's tier column honest after the credit, by the check-in engine's own rule.
        let _ = crate::mechanics::loyalty_checkin::sync_member_tier(
            &s,
            &program_id.to_string(),
            &member_id.to_string(),
            new_points as i64,
            tiers_enabled,
        )
        .await;

        let zc_value = credit_amount as f64 / ZC_PER_DOLLAR as f64;
        return Ok(Json(json!({
            "status": "verified",
            "credited_to": "loyalty_member",
            "member_id": member_id,
            "contact_id": req.contact_id,
            "points_earned": credit_amount,
            "credits_earned": credit_amount,
            "credit_rate": credit_rate,
            "new_points_balance": new_points,
            "purchase_amount": req.amount,
            "earnable_amount": earnable_amount,
            "deal_discount": deal_discount,
            "zc_per_dollar": ZC_PER_DOLLAR,
            "zc_value_display": format!("{} ZC = $1", ZC_PER_DOLLAR),
            "zc_value": format!("${:.2}", zc_value),
            "message": format!(
                "{} points credited to the member — their balance is now {}.",
                credit_amount, new_points
            ),
        })));
    }

    // Update the contact's account credits
    // First check if contact has an account by same UUID
    let account_credits: Option<i32> =
        sqlx::query_scalar("SELECT credits_balance FROM accounts WHERE id = $1")
            .bind(req.contact_id)
            .fetch_optional(&s.db)
            .await?;

    if let Some(balance) = account_credits {
        // Contact UUID matches an account
        // Step 1: Deduct redeemed credits (if offer applied)
        let mut net_change = credit_amount; // earned
        if redeemed_credits > 0 {
            net_change = credit_amount - redeemed_credits;
        }

        let new_balance = (balance + net_change).max(0);

        // Check ZC pool for business loyalty plan gating
        let zc_pool: i32 =
            sqlx::query_scalar("SELECT COALESCE(zc_pool_remaining, 0) FROM accounts WHERE id = $1")
                .bind(business_account_id)
                .fetch_optional(&s.db)
                .await?
                .unwrap_or(0);

        if zc_pool > 0 && zc_pool < credit_amount {
            return Err(AppError::BadRequest(
                format!("Business ZC pool too low. Pool has {} ZC remaining — this transaction awards {} ZC.", zc_pool, credit_amount)
            ));
        }

        // Deduct from pool (only if on a paid plan)
        if loyalty_plan_status.as_deref() == Some("active") {
            let _ = sqlx::query("UPDATE accounts SET zc_pool_remaining = zc_pool_remaining - $1 WHERE id = $2 AND zc_pool_remaining >= $1")
                .bind(credit_amount)
                .bind(business_account_id)
                .execute(&s.db)
                .await;
        }

        sqlx::query("UPDATE accounts SET credits_balance = $1 WHERE id = $2")
            .bind(new_balance)
            .bind(req.contact_id)
            .execute(&s.db)
            .await?;

        // Log transaction(s)
        let tx_id = Uuid::new_v4();
        let mut desc = format!(
            "Purchase verified -- {} credits earned (${:.2} total, ${:.2} earnable)",
            credit_amount, req.amount, earnable_amount
        );

        if deal_discount > 0.01 {
            desc = format!("{}, ${:.2} deal discount excluded", desc, deal_discount);
            if let Some(ref deal_note) = req.deal_description {
                desc = format!("{} — {}", desc, deal_note);
            }
        }

        if redeemed_credits > 0 {
            desc = format!(
                "{} credits earned, {} redeemed via '{}' offer (${:.2} total, ${:.2} earnable)",
                credit_amount,
                redeemed_credits,
                offer_name.as_deref().unwrap_or("Offer"),
                req.amount,
                earnable_amount
            );
        }

        sqlx::query(
            "INSERT INTO credit_transactions (id, account_id, amount, balance_after, action, reference_type, reference_id, description)
             VALUES ($1, $2, $3, $4, 'purchase', 'purchase_verify', $5, $6)"
        )
        .bind(tx_id)
        .bind(req.contact_id)
        .bind(net_change)
        .bind(new_balance)
        .bind(req.contact_id.to_string())
        .bind(&desc)
        .execute(&s.db)
        .await?;

        let mut resp = serde_json::Map::new();
        resp.insert("status".to_string(), json!("verified"));
        resp.insert("contact_id".to_string(), json!(req.contact_id));
        resp.insert("credits_earned".to_string(), json!(credit_amount));
        resp.insert("credit_rate".to_string(), json!(credit_rate));
        resp.insert("new_balance".to_string(), json!(new_balance));
        resp.insert("purchase_amount".to_string(), json!(req.amount));
        resp.insert("earnable_amount".to_string(), json!(earnable_amount));
        resp.insert("deal_discount".to_string(), json!(deal_discount));
        if let Some(ref deal_note) = req.deal_description {
            resp.insert("deal_description".to_string(), json!(deal_note));
        }

        // ZC value info for UI display
        resp.insert("zc_per_dollar".to_string(), json!(ZC_PER_DOLLAR));
        resp.insert(
            "zc_value_display".to_string(),
            json!(format!("{} ZC = $1", ZC_PER_DOLLAR)),
        );
        resp.insert(
            "redemption_cap_pct".to_string(),
            json!(tenant_redemption_cap_pct),
        );
        resp.insert(
            "max_redeemable_this_visit".to_string(),
            json!(
                ((req.amount * tenant_redemption_cap_pct as f64 / 100.0) * ZC_PER_DOLLAR as f64)
                    .floor() as i32
            ),
        );

        if redeemed_credits > 0 {
            let zc_dollars = redeemed_credits as f64 / ZC_PER_DOLLAR as f64;
            resp.insert("credits_redeemed".to_string(), json!(redeemed_credits));
            resp.insert("offer_applied".to_string(), json!(offer_name));
            resp.insert(
                "zc_redeemed_value".to_string(),
                json!(format!("${:.2}", zc_dollars)),
            );
        }
        if let Some(ref why) = offer_refused {
            resp.insert("offer_refused".to_string(), json!(true));
            resp.insert("offer_refused_reason".to_string(), json!(why));
        }

        let zc_value = credit_amount as f64 / ZC_PER_DOLLAR as f64;
        let mut msg = if deal_discount > 0.01 {
            format!(
                "${:.2} deal excluded! Earned {} ZaarCash (worth ${:.2}) on ${:.2} spend{}",
                deal_discount,
                credit_amount,
                zc_value,
                earnable_amount,
                if let Some(ref n) = req.deal_description {
                    format!(" ({})", n)
                } else {
                    String::new()
                }
            )
        } else {
            format!(
                "Earned {} ZaarCash (worth ${:.2}) on ${:.2} spend",
                credit_amount, zc_value, earnable_amount
            )
        };
        if let Some(ref why) = offer_refused {
            msg = format!("{} {}", why, msg);
        }
        resp.insert("message".to_string(), json!(msg));

        Ok(Json(Value::Object(resp)))
    } else {
        // Contact exists but no account with that UUID — return success with no credits
        let zc_value = credit_amount as f64 / ZC_PER_DOLLAR as f64;
        Ok(Json(json!({
            "status": "contact_linked",
            "contact_id": req.contact_id,
            "credits_earned": 0,
            "purchase_amount": req.amount,
            "credits_eligible": credit_amount,
            "zc_eligible_value": format!("${:.2}", zc_value),
            "zc_per_dollar": ZC_PER_DOLLAR,
            "message": format!("Register to earn! This visit would be worth {} ZaarCash (${:.2} value)", credit_amount, zc_value)
        })))
    }
}

/// GET /api/v1/loyalty/purchase/pin — the caller's OWN counter PIN.
///
/// `POST /loyalty/purchase/verify` refuses a wrong PIN, and the counter screen needs to show the
/// business the value it is meant to type. The only existing reader is
/// `GET /admin/tenants/:id/purchase-pin`, which is admin-gated (`check_tenant_access`), so a plain
/// business owner got a 403 and the screen was unusable for exactly the person who owns the shop.
/// No path parameter and no role: the answer is the caller's own account row, nothing else.
pub async fn purchase_pin(
    State(s): State<AppState>,
    auth: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&auth.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".into()))?;
    let pin: Option<String> = sqlx::query_scalar("SELECT purchase_pin FROM accounts WHERE id = $1")
        .bind(account_id)
        .fetch_optional(&s.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Account not found".into()))?;
    Ok(Json(json!({ "pin": pin })))
}

#[derive(Debug, Deserialize)]
pub struct SetPurchasePinRequest {
    pub pin: String,
}

/// A counter PIN is the only thing between "anyone signed in" and crediting points onto a member's
/// balance, so it must be short enough to type at a till and long enough not to be guessed. This
/// accepts 4–8 ASCII letters or digits and upper-cases them, which covers both shapes the product
/// already uses: the `Z100` an account creation mints and the `4821` an owner picks.
fn checked_purchase_pin(raw: &str) -> Result<String, AppError> {
    let pin = raw.trim().to_ascii_uppercase();
    let ok = (4..=8).contains(&pin.len()) && pin.bytes().all(|b| b.is_ascii_alphanumeric());
    if !ok {
        return Err(AppError::BadRequest(
            "Choose a counter PIN of 4 to 8 letters or digits — it is the code your counter staff \
             type before a purchase is credited."
                .to_string(),
        ));
    }
    Ok(pin)
}

/// PUT /api/v1/loyalty/purchase/pin — change the caller's OWN counter PIN.
///
/// UNTIL THIS HANDLER THE PIN WAS WRITE-ONCE. `accounts.purchase_pin` was set at account creation
/// (`account_mint`, format `Z100`…, plus the `'0000'` column default for accounts minted before it)
/// and the only writers anywhere were those two doors; the admin note in `admin_handler.rs` said so
/// verbatim ("Only read endpoint is exposed") and the admin guide told a business a compromised PIN
/// needed "a manual database operation performed by a super admin". So a business that believed its
/// counter PIN had leaked could NOT rotate it — which makes a shared-secret worthless exactly when
/// it matters. This closes that: the answer is the caller's own account row, nothing else, and the
/// old PIN stops working the moment it returns.
pub async fn set_purchase_pin(
    State(s): State<AppState>,
    auth: AuthenticatedUser,
    Json(req): Json<SetPurchasePinRequest>,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&auth.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".into()))?;
    let pin = checked_purchase_pin(&req.pin)?;
    let updated =
        sqlx::query("UPDATE accounts SET purchase_pin = $1, updated_at = now() WHERE id = $2")
            .bind(&pin)
            .bind(account_id)
            .execute(&s.db)
            .await?;
    if updated.rows_affected() == 0 {
        return Err(AppError::NotFound("Account not found".into()));
    }
    Ok(Json(json!({
        "pin": pin,
        "message": "Counter PIN changed. Give it to the staff who credit purchases — the old PIN no longer works."
    })))
}

#[cfg(test)]
mod purchase_pin_tests {
    use super::*;

    #[test]
    fn accepts_the_shapes_the_product_uses() {
        assert_eq!(checked_purchase_pin("4821").unwrap(), "4821");
        assert_eq!(checked_purchase_pin(" z100 ").unwrap(), "Z100");
        assert_eq!(checked_purchase_pin("abc12").unwrap(), "ABC12");
    }

    #[test]
    fn refuses_short_long_and_non_alphanumeric() {
        assert!(checked_purchase_pin("123").is_err());
        assert!(checked_purchase_pin("123456789").is_err());
        assert!(checked_purchase_pin("12 34").is_err());
        assert!(checked_purchase_pin("12-34").is_err());
        assert!(checked_purchase_pin("").is_err());
    }
}

// ── Survey Response Handler ────────────────────────────────────────────────

// ── Account-Level Loyalty Routes (via auth) ──────────────────────────────

/// GET /api/v1/loyalty/referrals — get account referral code + referral count
pub async fn get_referrals(
    State(s): State<AppState>,
    auth: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&auth.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".into()))?;

    // Get account's referrer_code
    let code_row =
        sqlx::query_scalar::<_, Option<String>>("SELECT referrer_code FROM accounts WHERE id = $1")
            .bind(account_id)
            .fetch_optional(&s.db)
            .await?
            .flatten();

    // Referrals are CAMPAIGN-scoped: `campaign_referrals.referrer_contact_id` holds a `contacts.id`
    // (see viral::ensure_campaign_referral), while this route is ACCOUNT-scoped. The account's
    // referrals are therefore the referrals recorded in the account's OWN campaigns — the old
    // `WHERE referrer_contact_id = <accounts.id>` could never match anything, because accounts.id
    // is not a contacts.id and `accounts` has no contact row at all (kanban t_ad98b6ab).
    let referral_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM campaign_referrals cr
         JOIN campaigns c ON c.id = cr.campaign_id
         WHERE c.account_id = $1",
    )
    .bind(account_id)
    .fetch_optional(&s.db)
    .await?
    .unwrap_or(0);

    // Get the actual referral records. The campaign NAME rides along so the console tab can say
    // which campaign a code belongs to — the account's referrals span all of its campaigns
    // (kanban t_ad98b6ab).
    let referrals = sqlx::query_as::<_, (Uuid, Uuid, Option<Uuid>, Option<Uuid>, String, String, bool, Option<chrono::DateTime<chrono::Utc>>, i32, i32, chrono::DateTime<chrono::Utc>, String)>(
        "SELECT cr.id, cr.campaign_id, cr.referrer_contact_id, cr.referee_contact_id, cr.referral_code, cr.source, cr.converted, cr.converted_at, cr.click_count, cr.points_earned, cr.created_at, c.name
         FROM campaign_referrals cr
         JOIN campaigns c ON c.id = cr.campaign_id
         WHERE c.account_id = $1
         ORDER BY cr.created_at DESC LIMIT 50"
    )
    .bind(account_id)
    .fetch_all(&s.db)
    .await?;

    let referral_list: Vec<Value> = referrals
        .into_iter()
        .map(|r| {
            json!({
                "id": r.0,
                "campaign_id": r.1,
                "referrer_contact_id": r.2,
                "referee_contact_id": r.3,
                "referral_code": r.4,
                "source": r.5,
                "converted": r.6,
                "converted_at": r.7,
                "click_count": r.8,
                "points_earned": r.9,
                "created_at": r.10,
                "campaign_name": r.11,
            })
        })
        .collect();

    Ok(Json(json!({
        "code": code_row,
        "referrals": referral_list,
        "referral_count": referral_count,
    })))
}

/// POST /api/v1/loyalty/referrals/create — generate referral code for account
pub async fn account_create_referral(
    State(s): State<AppState>,
    auth: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&auth.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".into()))?;

    let existing =
        sqlx::query_scalar::<_, Option<String>>("SELECT referrer_code FROM accounts WHERE id = $1")
            .bind(account_id)
            .fetch_optional(&s.db)
            .await?
            .flatten();

    if let Some(code) = existing {
        return Ok(Json(json!({"code": code, "message": "exists"})));
    }

    let code = format!("REF{:06}", rand::thread_rng().gen_range(0..999999));

    sqlx::query("UPDATE accounts SET referrer_code = $1 WHERE id = $2")
        .bind(&code)
        .bind(account_id)
        .execute(&s.db)
        .await?;

    Ok(Json(json!({"code": code, "message": "created"})))
}

/// GET /api/v1/loyalty/rewards — list all reward tiers for account
pub async fn get_rewards(
    State(s): State<AppState>,
    auth: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&auth.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".into()))?;

    let tiers = sqlx::query_as::<_, (Uuid, String, String, i32, bool)>(
        r#"SELECT lrt.id, lp.name, lrt.name, lrt.points_required, lrt.requires_approval
           FROM loyalty_reward_tiers lrt
           JOIN loyalty_programs lp ON lp.id = lrt.program_id
           WHERE lp.is_active = true
           ORDER BY lrt.points_required ASC"#,
    )
    .bind(account_id)
    .fetch_all(&s.db)
    .await?;

    let rewards_list: Vec<Value> = tiers
        .into_iter()
        .map(|t| {
            json!({
                "id": t.0,
                "program_name": t.1,
                "name": t.2,
                "cost": t.3,
                "requires_approval": t.4,
            })
        })
        .collect();

    Ok(Json(json!({
        "rewards": rewards_list,
        "count": rewards_list.len(),
    })))
}

/// GET /api/v1/loyalty/rewards/earned — the caller's OWN redemptions, pending first.
///
/// Why this exists (kanban t_ae0f4ef2): the operator console's "Rewards Earned" tab read
/// `GET /loyalty/rewards`, which returns the reward-TIER catalogue. Every row therefore showed
/// `pending` (a tier has no status), the member column showed `?` (a tier has no member) and the
/// Earned column showed "Invalid Date" — and a real redemption could be neither approved nor
/// denied from the panel, even though `POST /loyalty/rewards/:id/{approve,deny}` both existed.
/// Scoped exactly like `list_rewards_earned`: the programme must belong to the caller's account,
/// directly or through its campaign. No role bypass is invented.
pub async fn list_account_rewards_earned(
    State(s): State<AppState>,
    auth: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&auth.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".into()))?;

    #[derive(sqlx::FromRow)]
    struct EarnedRow {
        id: Uuid,
        member_name: Option<String>,
        member_email: Option<String>,
        tier_name: String,
        points: i32,
        status: String,
        earned_at: Option<chrono::DateTime<chrono::Utc>>,
        requires_approval: bool,
        program_name: String,
    }

    let rows = sqlx::query_as::<_, EarnedRow>(
        r#"SELECT lre.id,
                  NULLIF(TRIM(CONCAT_WS(' ', c.first_name, c.last_name)), '') AS member_name,
                  c.email AS member_email,
                  lrt.name AS tier_name,
                  lrt.points_required AS points,
                  COALESCE(lre.status, 'pending') AS status,
                  lre.earned_at,
                  COALESCE(lrt.requires_approval, false) AS requires_approval,
                  lp.name AS program_name
             FROM loyalty_rewards_earned lre
             JOIN loyalty_reward_tiers lrt ON lrt.id = lre.tier_id
             JOIN loyalty_programs lp ON lp.id = lrt.program_id
             LEFT JOIN loyalty_members lm ON lm.id = lre.member_id
             LEFT JOIN contacts c ON c.id = lm.contact_id
            WHERE lp.account_id = $1
               OR EXISTS (SELECT 1 FROM campaigns camp
                           WHERE camp.id = lp.campaign_id AND camp.account_id = $1)
               OR EXISTS (SELECT 1 FROM campaigns fwd
                           WHERE fwd.loyalty_program_id = lp.id AND fwd.account_id = $1)
            ORDER BY (COALESCE(lre.status, 'pending') = 'pending') DESC,
                     lre.earned_at DESC NULLS LAST
            LIMIT 200"#,
    )
    .bind(account_id)
    .fetch_all(&s.db)
    .await?;

    let pending = rows.iter().filter(|r| r.status == "pending").count();
    let list: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "id": r.id,
                "member_name": r.member_name,
                "member_email": r.member_email,
                "tier_name": r.tier_name,
                "program_name": r.program_name,
                "points": r.points,
                "status": r.status,
                "earned_at": r.earned_at,
                "requires_approval": r.requires_approval,
            })
        })
        .collect();

    Ok(Json(json!({
        "earned": list,
        "count": list.len(),
        "pending": pending,
    })))
}

// ── Account voucher listing (retired) ──
//
// `get_vouchers` (GET /api/v1/loyalty/vouchers) was RETIRED here (kanban t_30dfc98c), together
// with its two siblings and the `vouchers` table. Its ONLY live caller was the served admin
// console's Vouchers tab (`www-admin/index.html` -> `loadVouchers()`), which is retired in the
// same pass; the tab could never show a row again, and the guides that advertised the rest of the
// flow are corrected too. Proof: /opt/swift/audits/t_30dfc98c/proof.py.

// ── External survey-response mint (retired) ──
//
// `survey_response` + `SurveyResponsePayload` (POST /api/v1/campaigns/external/survey-response)
// were RETIRED here (kanban t_3bde2e27). Measured on the deployed binary 98ab2c0012ed4396, the
// handler took NO credential, and an ANONYMOUS caller who named a live `directory-*` campaign
// slug minted an ACTIVE $50 `restaurant_card` voucher (30-day expiry, its own 8-char code),
// find-or-created a `contacts` row from the caller-supplied `visitor_email`, linked it into the
// campaign owner's account via `contact_tenants`, awarded 100 Zaarcash into
// `campaign_points_balance`, and fired the `voucher_issued` Marketing Boost webhook. Repeat calls
// minted again — unbounded. Same class as t_b209d263 (anonymous, unscoped value mint with a
// caller-chosen recipient).
//
// Its only named caller was MultiDirectory ("when a visitor completes the onboarding survey"),
// and MultiDirectory RETIRED the IncentiveSwift loyalty integration on 2026-09-23
// (`service="incentiveswift"` answers 400; its `onboarding_survey.rs` states "There is NO
// IncentiveSwift call here any more" — rewards are credited by MD's native engine). Census:
// 0 nginx hits over 692,556 access-log lines (instrument validated — 2,975 `/api/v1/health` hits
// in the same window), 0 fleet callers, 0 served-root call sites.
//
// Not scoped instead: there is no caller to scope to, and a contact cannot authenticate in this
// app, so there is no credential to bind. The app's host-to-host seam (`x-internal-key`
// /api/v1/internal/*) was not invented onto a route nothing calls.
//
// This was `vouchers`' LAST writer. The three mounted READERS it left behind
// (`get_vouchers`, `list_my_vouchers`, `claim_voucher`) were RETIRED and the table dropped in
// kanban t_30dfc98c — the decision this note used to defer.
//
// Proof: /opt/swift/audits/t_3bde2e27/proof.py (before 11/11, after 12/12, residue 0).
