//! Loyalty verification & voucher handlers
//! Voucher claim/redeem, business pledges
//!
//! RETIRED from this module: `generate_pin` and `issue_voucher` (kanban t_b209d263 — both
//! anonymous and unscoped), then `verify_purchase` and `issue_rotation_voucher` (kanban
//! t_7a16bf0b — the guarded reader of `purchase_verifications` and its private voucher
//! issuer, left writer-less by t_b209d263), then the rotation-group CRUD family (kanban
//! t_8e9d3a52 — the last writers of `rotation_configs` / `rotation_group_members`).
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

// ── Voucher Engine ──
//
// `issue_voucher` (and the `IssueVoucherRequest` it took) was RETIRED here (kanban
// t_b209d263): an anonymous, unscoped value-mint — the caller chose the campaign slug, the
// recipient contact AND the discount value, and an uncredentialed request inserted a live
// `vouchers` row. Its only fleet caller was MultiDirectory's
// `tag_automation.rs::execute_voucher_action`, and that call path is retired (MD retired the
// IncentiveSwift loyalty integration on 2026-09-23: `service="incentiveswift"` answers 400;
// `tag_rules` has 0 rows ever and the MD console's tag-rule form does not offer the action).
// `verify_purchase` and its private issuer `issue_rotation_voucher` were RETIRED in kanban
// t_7a16bf0b (see the notes above). The voucher lifecycle that remains is `claim_voucher`
// (keyed by a claim code, a bearer secret) and the AuthenticatedUser-scoped
// `list_my_vouchers`. NOTE (measured, t_7a16bf0b): `vouchers` is NOT orphaned —
// `survey_response` below still INSERTs a live `$50` restaurant-card voucher, anonymously.
// That arm is its own card.
/// GET /api/v1/loyalty/my-vouchers — list active vouchers for a contact
pub async fn list_my_vouchers(
    State(s): State<AppState>,
    auth: AuthenticatedUser,
    Path(contact_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    // SECURITY (kanban t_f08d32e7): this arm took no `AuthenticatedUser`, so any caller who named
    // a contact id got that contact's vouchers - redemption codes included. The caller must now be
    // linked to the contact through the `contact_tenants` boundary (kanban t_369cb159); anything
    // else answers the same 404 an absent contact does.
    let account_id = Uuid::parse_str(&auth.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".into()))?;
    if !crate::db::contacts::contact_visible_to(&s.db, &contact_id, &account_id).await? {
        return Err(AppError::NotFound("Contact not found".into()));
    }

    let vouchers = sqlx::query_as::<
        _,
        (
            Uuid,
            String,
            String,
            String,
            String,
            String,
            Option<chrono::DateTime<chrono::Utc>>,
        ),
    >(
        r#"SELECT v.id, v.discount_value, v.voucher_type, v.redemption_code, v.status,
                  COALESCE(b.name, '') as business_name, v.expires_at
           FROM vouchers v
           LEFT JOIN portfolio_companies b ON b.id = v.target_business_id
           WHERE v.issued_to_contact_id = $1
           ORDER BY v.created_at DESC"#,
    )
    .bind(contact_id)
    .fetch_all(&s.db)
    .await?;

    let result: Vec<serde_json::Value> = vouchers
        .into_iter()
        .map(|v| {
            json!({
                "id": v.0, "discount": v.1, "type": v.2, "code": v.3, "status": v.4,
                "business": v.5, "expires_at": v.6
            })
        })
        .collect();

    Ok(Json(json!({"vouchers": result})))
}

/// POST /api/v1/loyalty/claim-voucher — redeem a voucher by code
#[derive(Debug, Deserialize)]
pub struct ClaimVoucherRequest {
    pub code: String,
    pub contact_id: Uuid,
}

pub async fn claim_voucher(
    State(s): State<AppState>,
    Json(req): Json<ClaimVoucherRequest>,
) -> Result<impl IntoResponse, AppError> {
    let voucher = sqlx::query_as::<_, (Uuid, String, String)>(
        "SELECT id, discount_value, status FROM vouchers WHERE redemption_code = $1 AND issued_to_contact_id = $2 LIMIT 1"
    )
    .bind(req.code.to_uppercase())
    .bind(req.contact_id)
    .fetch_optional(&s.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Voucher not found".into()))?;

    if voucher.2 != "active" {
        return Err(AppError::BadRequest(
            "Voucher already used or expired".into(),
        ));
    }

    sqlx::query("UPDATE vouchers SET status = 'used', used_at = NOW() WHERE id = $1")
        .bind(voucher.0)
        .execute(&s.db)
        .await?;

    Ok(Json(json!({"status": "claimed", "discount": voucher.1})))
}

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
// Its INSERT was one of `vouchers`'s two writers. The other, `survey_response` below, is STILL
// LIVE and anonymous, so `vouchers` is NOT orphaned — contrary to the card's premise (carded).
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
        "SELECT EXISTS (SELECT 1 FROM loyalty_members lm \
           JOIN loyalty_programs lp ON lp.id = lm.program_id \
           JOIN campaigns c ON c.id = lp.campaign_id \
          WHERE lm.contact_id = $1 AND c.account_id = $2)",
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

    // ── Handle offer-based redemption (business applies a named offer) ──
    let mut redeemed_credits: i32 = 0;
    let mut offer_name: Option<String> = None;

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

        // Customer must have enough ZC to redeem the offer
        if customer_balance < max_discount_credits {
            return Err(AppError::BadRequest(format!(
                "Insufficient ZaarCash. Offer needs {} ZC but you have {}.",
                max_discount_credits, customer_balance
            )));
        }

        redeemed_credits = max_discount_credits;
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

        let zc_value = credit_amount as f64 / ZC_PER_DOLLAR as f64;
        let msg = if deal_discount > 0.01 {
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

/// GET /api/v1/loyalty/vouchers — list vouchers for account (uses account_id as fallback contact_id)
pub async fn get_vouchers(
    State(s): State<AppState>,
    auth: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&auth.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".into()))?;

    // Try account_id as contact_id (vouchers are issued to contacts)
    let vouchers = sqlx::query_as::<
        _,
        (
            Uuid,
            String,
            String,
            String,
            String,
            Option<chrono::DateTime<chrono::Utc>>,
        ),
    >(
        r#"SELECT v.id, v.discount_value, v.voucher_type, v.redemption_code, v.status, v.expires_at
           FROM vouchers v
           WHERE v.issued_to_contact_id = $1
           ORDER BY v.created_at DESC LIMIT 50"#,
    )
    .bind(account_id)
    .fetch_all(&s.db)
    .await?;

    let voucher_list: Vec<Value> = vouchers
        .into_iter()
        .map(|v| {
            json!({
                "id": v.0,
                "discount": v.1,
                "discount_value": v.1,
                "type": v.2,
                "voucher_type": v.2,
                "code": v.3,
                "redemption_code": v.3,
                "status": v.4,
                "expires_at": v.5,
            })
        })
        .collect();

    Ok(Json(json!({
        "vouchers": voucher_list,
        "count": voucher_list.len(),
    })))
}

/// POST /api/v1/campaigns/external/survey-response
/// Called by MultiDirectory when a visitor completes the onboarding survey.
/// Awards 100 Zaarcash + issues $50 restaurant card voucher.
#[derive(Debug, Deserialize)]
pub struct SurveyResponsePayload {
    pub directory_slug: String,
    pub visitor_account_id: Option<Uuid>,
    pub visitor_email: Option<String>,
    pub survey_id: Option<Uuid>,
    pub answers: Option<Value>,
    pub applied_tags: Option<Vec<String>>,
}

pub async fn survey_response(
    State(s): State<AppState>,
    Json(payload): Json<SurveyResponsePayload>,
) -> Result<impl IntoResponse, AppError> {
    // Build campaign slug from directory slug
    // Directory slug format: "palm-coast" -> campaign slug: "directory-palm-coast"
    let campaign_slug = format!("directory-{}", payload.directory_slug);

    // Find the campaign
    let campaign = sqlx::query_as::<_, (Uuid, String, Uuid)>(
        "SELECT id, name, account_id FROM campaigns WHERE slug = $1 AND status = 'active' LIMIT 1",
    )
    .bind(&campaign_slug)
    .fetch_optional(&s.db)
    .await?
    .ok_or_else(|| AppError::NotFound(format!("Campaign not found for slug: {}", campaign_slug)))?;

    let campaign_id = campaign.0;
    let campaign_name = campaign.1;
    // The directory campaign's owner is whose lead this visitor is (contact_tenants, t_369cb159).
    let campaign_account = campaign.2;

    // If we have a visitor email, find or create the contact
    let contact_id = if let Some(ref email) = payload.visitor_email {
        // Try to find existing contact
        let existing =
            sqlx::query_scalar::<_, Uuid>("SELECT id FROM contacts WHERE email = $1 LIMIT 1")
                .bind(email)
                .fetch_optional(&s.db)
                .await?;

        let cid = match existing {
            Some(cid) => cid,
            None => {
                // Create new contact
                let new_id = Uuid::new_v4();
                sqlx::query("INSERT INTO contacts (id, email, notes2) VALUES ($1, $2, $3)")
                    .bind(new_id)
                    .bind(email)
                    .bind(payload.applied_tags.as_ref().map(|t| t.join(", ")))
                    .execute(&s.db)
                    .await?;
                new_id
            }
        };
        crate::db::contacts::link_contact(&s.db, &cid, &campaign_account, "survey").await?;
        cid
    } else {
        return Err(AppError::BadRequest("Visitor email is required".into()));
    };

    // Enrol in the programme for THIS directory, resolved from the payload rather than from a name
    // baked into the engine. There used to be an extra hardcoded enrolment above this one, pointing at
    // a single directory's programme; it is gone. Whose points a visitor earns is the caller's answer,
    // which is why the slug is built from `directory_slug` and not from a literal.
    let city_program_slug = format!("directory-{}", payload.directory_slug);
    let city_program: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM loyalty_programs WHERE slug = $1 LIMIT 1")
            .bind(&city_program_slug)
            .fetch_optional(&s.db)
            .await?;

    if let Some(program_id) = city_program {
        let _ = crate::db::loyalty::find_or_create_member(&s.db, &program_id, &contact_id).await;
    }

    // Award 100 Zaarcash — upsert campaign_points_balance
    let existing_balance = sqlx::query_scalar::<_, i32>(
        "SELECT points_balance FROM campaign_points_balance 
         WHERE campaign_id = $1 AND contact_id = $2",
    )
    .bind(campaign_id)
    .bind(contact_id)
    .fetch_optional(&s.db)
    .await?
    .unwrap_or(0);

    if existing_balance == 0 {
        // First time — insert
        sqlx::query(
            "INSERT INTO campaign_points_balance (campaign_id, contact_id, points_balance, lifetime_points)
             VALUES ($1, $2, 100, 100)"
        )
        .bind(campaign_id)
        .bind(contact_id)
        .execute(&s.db)
        .await?;
    } else {
        // Existing — add 100 points
        sqlx::query(
            "UPDATE campaign_points_balance 
             SET points_balance = points_balance + 100, 
                 lifetime_points = lifetime_points + 100,
                 updated_at = NOW()
             WHERE campaign_id = $1 AND contact_id = $2",
        )
        .bind(campaign_id)
        .bind(contact_id)
        .execute(&s.db)
        .await?;
    }

    // No loyalty_transactions table exists — skipping history record

    // Issue $50 restaurant card voucher
    let voucher_id = Uuid::new_v4();
    let code: String = {
        use rand::Rng;
        const CHARSET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
        let mut rng = rand::thread_rng();
        (0..8)
            .map(|_| {
                let idx = rng.gen_range(0..CHARSET.len());
                CHARSET[idx] as char
            })
            .collect()
    };

    let thirty_days = chrono::Duration::days(30);
    let expires_at = chrono::Utc::now() + thirty_days;

    sqlx::query(
        "INSERT INTO vouchers (id, campaign_id, issued_to_contact_id, voucher_type, 
         discount_value, redemption_code, expires_at, status)
         VALUES ($1, $2, $3, 'restaurant_card', '$50.00', $4, $5, 'active')",
    )
    .bind(voucher_id)
    .bind(campaign_id)
    .bind(contact_id)
    .bind(&code)
    .bind(expires_at)
    .execute(&s.db)
    .await?;

    // Look up contact name for Marketing Boost payload
    let mb_contact_lookup = sqlx::query_as::<_, (Option<String>, Option<String>)>(
        "SELECT first_name, last_name FROM contacts WHERE id = $1 \
         AND EXISTS (SELECT 1 FROM contact_tenants ct \
                     WHERE ct.contact_id = contacts.id AND ct.account_id = $2)",
    )
    .bind(contact_id)
    .bind(campaign_account)
    .fetch_optional(&s.db)
    .await
    .ok()
    .flatten();

    let (mb_first_name, mb_last_name) = mb_contact_lookup.unwrap_or((None, None));

    // Fire Marketing Boost webhook for the voucher (handles the $50 card fulfillment)
    let mb_payload = serde_json::json!({
        "voucher_id": voucher_id,
        "code": code,
        "discount_value": "$50.00",
        "voucher_type": "restaurant_card",
        "contact_id": contact_id,
        "email": payload.visitor_email,
        "first_name": mb_first_name,
        "last_name": mb_last_name,
        "campaign_name": campaign_name,
        "campaign_slug": campaign_slug,
        "source": "onboarding_survey",
    });
    crate::handlers::campaign_integrations::fire_marketing_boost(
        &s,
        &campaign_id,
        "voucher_issued",
        &mb_payload,
    )
    .await;

    Ok(Json(json!({
        "status": "ok",
        "contact_id": contact_id,
        "voucher_id": voucher_id,
        "code": code,
        "zaarcash_awarded": 100,
        "voucher_type": "restaurant_card",
        "campaign": campaign_name,
    })))
}
