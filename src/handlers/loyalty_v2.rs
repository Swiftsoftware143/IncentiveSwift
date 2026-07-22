//! Loyalty verification & voucher handlers
//! PIN generation, receipt verification, rotating vouchers, business pledges

use axum::{
    extract::{Path, State},
    response::IntoResponse,
    Json,
};
use rand::Rng;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::state::AppState;
use crate::error::AppError;
use crate::handlers::campaign_integrations;
use crate::security::auth::AuthenticatedUser;

// ── Purchase Verification ──

#[derive(Debug, Deserialize)]
pub struct GeneratePinRequest {
    pub campaign_slug: String,
    pub business_id: Uuid,
    pub business_name: Option<String>,
    pub purchase_amount: Option<rust_decimal::Decimal>,
}

/// POST /api/v1/loyalty/generate-pin — business generates a 4-digit PIN for a customer purchase
pub async fn generate_pin(
    State(s): State<AppState>,
    Json(req): Json<GeneratePinRequest>,
) -> Result<impl IntoResponse, AppError> {
    let campaign = sqlx::query_as::<_, (Uuid,)>(
        "SELECT id FROM campaigns WHERE slug = $1 AND status = 'active' LIMIT 1"
    )
    .bind(&req.campaign_slug)
    .fetch_optional(&s.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Campaign not found".into()))?;

    let pin: String = rand::thread_rng()
        .sample_iter(&rand::distributions::Alphanumeric)
        .take(4)
        .map(char::from)
        .collect::<String>()
        .to_uppercase();

    let verification_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO purchase_verifications (id, campaign_id, business_id, business_name, verification_type, pin_code, purchase_amount, status)
         VALUES ($1, $2, $3, $4, 'pin', $5, $6, 'pending')"
    )
    .bind(verification_id)
    .bind(campaign.0)
    .bind(req.business_id)
    .bind(&req.business_name)
    .bind(&pin)
    .bind(req.purchase_amount)
    .execute(&s.db)
    .await?;

    Ok(Json(json!({
        "pin": pin,
        "expires_in": "30 minutes",
        "verification_id": verification_id
    })))
}

#[derive(Debug, Deserialize)]
pub struct VerifyPurchaseRequest {
    pub pin_code: String,
    pub contact_id: Uuid,
}

/// POST /api/v1/loyalty/verify-purchase — consumer enters PIN to verify purchase
pub async fn verify_purchase(
    State(s): State<AppState>,
    auth: AuthenticatedUser,
    Json(req): Json<VerifyPurchaseRequest>,
) -> Result<impl IntoResponse, AppError> {
    let account_id = Uuid::parse_str(&auth.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".into()))?;

    let verification = sqlx::query_as::<_, (Uuid, Uuid, Uuid, String, Option<String>, String, Option<rust_decimal::Decimal>)>(
        "SELECT pv.id, pv.campaign_id, pv.business_id, pv.business_name, pv.pin_code, pv.status, pv.purchase_amount
         FROM purchase_verifications pv
         WHERE pv.pin_code = $1 AND pv.status = 'pending'
         LIMIT 1"
    )
    .bind(&req.pin_code.to_uppercase())
    .fetch_optional(&s.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Invalid or expired PIN".into()))?;

    // Check if contact exists before linking
    let contact_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM contacts WHERE id = $1)"
    )
    .bind(req.contact_id)
    .fetch_one(&s.db)
    .await
    .unwrap_or(false);

    if contact_exists {
        sqlx::query(
            "UPDATE purchase_verifications SET status = 'verified', contact_id = $1, verified_at = NOW() WHERE id = $2"
        )
        .bind(req.contact_id)
        .bind(verification.0)
        .execute(&s.db)
        .await?;
    } else {
        sqlx::query(
            "UPDATE purchase_verifications SET status = 'verified', verified_at = NOW() WHERE id = $1"
        )
        .bind(verification.0)
        .execute(&s.db)
        .await?;
    }

    let business_name = if verification.3.is_empty() { "Business".to_string() } else { verification.3.clone() };

    // Auto-issue a rotating voucher from cross-promotion group (only if contact exists)
    let voucher = if contact_exists {
        issue_rotation_voucher(
            &s.db,
            &verification.1,
            &req.contact_id,
            &verification.2,
        ).await?
    } else {
        None
    };

    // Auto-credit the customer based on purchase amount
    let mut credit_amount = 0i32;
    let mut credit_message = String::new();
    if let Some(amount) = verification.6 {
        // Convert purchase amount to credits: $1 = 10 credits
        let scaled = (amount * rust_decimal::Decimal::new(10, 0)).round();
        if let Ok(int_val) = i32::try_from(scaled) {
            if int_val > 0 {
                credit_amount = int_val;
                // Get current balance
                let cur_balance: i32 = sqlx::query_scalar(
                    "SELECT credits_balance FROM accounts WHERE id = $1"
                )
                .bind(account_id)
                .fetch_optional(&s.db)
                .await?
                .unwrap_or(0);

                let new_balance = cur_balance + credit_amount;

                // Update balance
                sqlx::query(
                    "UPDATE accounts SET credits_balance = $1 WHERE id = $2"
                )
                .bind(new_balance)
                .bind(account_id)
                .execute(&s.db)
                .await?;

                // Log transaction
                sqlx::query(
                    "INSERT INTO credit_transactions (account_id, amount, balance_after, action, reference_type, reference_id, description)
                     VALUES ($1, $2, $3, 'purchase', 'purchase_verification', $4, $5)"
                )
                .bind(account_id)
                .bind(credit_amount)
                .bind(new_balance)
                .bind(verification.0.to_string())
                .bind(format!("Purchase at {} — {} credits earned", business_name, credit_amount))
                .execute(&s.db)
                .await?;

                credit_message = format!("You earned {} loyalty credits!", credit_amount);
            }
        }
    }

    let mut response = json!({
        "status": "verified",
        "business_name": business_name,
        "credits_earned": credit_amount,
        "message": format!("Purchase at {} verified! {}", business_name, credit_message)
    });

    if let Some(ref v) = voucher {
        response["voucher"] = v.clone();
        if let Some(biz) = v.get("business_name").and_then(|b| b.as_str()) {
            response["reward_message"] = json!(format!("🎉 You earned a reward at {}!", biz));
        }
    }

    Ok(Json(response))
}

// ── Voucher Engine ──

#[derive(Debug, Deserialize)]
pub struct IssueVoucherRequest {
    pub campaign_slug: String,
    pub contact_id: Uuid,
    pub source_business_id: Uuid,
    pub target_business_id: Uuid,
    pub discount_value: String,
    pub voucher_type: Option<String>,
    pub expires_in_days: Option<i32>,
}

/// POST /api/v1/loyalty/issue-voucher (internal, called after purchase verification)
pub async fn issue_voucher(
    State(s): State<AppState>,
    Json(req): Json<IssueVoucherRequest>,
) -> Result<impl IntoResponse, AppError> {
    let campaign = sqlx::query_as::<_, (Uuid,)>(
        "SELECT id FROM campaigns WHERE slug = $1 AND status = 'active' LIMIT 1"
    )
    .bind(&req.campaign_slug)
    .fetch_optional(&s.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Campaign not found".into()))?;

    let code: String = rand::thread_rng()
        .sample_iter(&rand::distributions::Alphanumeric)
        .take(8)
        .map(char::from)
        .collect::<String>()
        .to_uppercase();

    let days = req.expires_in_days.unwrap_or(30);
    let voucher_id = Uuid::new_v4();

    sqlx::query(
        "INSERT INTO vouchers (id, campaign_id, issued_to_contact_id, source_business_id, target_business_id,
         voucher_type, discount_value, redemption_code, expires_at, status)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, NOW() + make_interval(days => $9), 'active')"
    )
    .bind(voucher_id)
    .bind(campaign.0)
    .bind(req.contact_id)
    .bind(req.source_business_id)
    .bind(req.target_business_id)
    .bind(req.voucher_type.clone().unwrap_or_else(|| "discount".to_string()))
    .bind(&req.discount_value)
    .bind(&code)
    .bind(days)
    .execute(&s.db)
    .await?;

    // Look up contact info for Marketing Boost payload
    let mb_contact_lookup = sqlx::query_as::<_, (Option<String>, Option<String>, Option<String>)>(
        "SELECT email, first_name, last_name FROM contacts WHERE id = $1"
    )
    .bind(req.contact_id)
    .fetch_optional(&s.db)
    .await
    .ok()
    .flatten();

    let (mb_email, mb_first_name, mb_last_name) = mb_contact_lookup.unwrap_or((None, None, None));

    // Fire Marketing Boost webhook if configured
    let mb_payload = json!({
        "voucher_id": voucher_id,
        "code": code,
        "discount_value": req.discount_value,
        "voucher_type": req.voucher_type,
        "contact_id": req.contact_id,
        "source_business_id": req.source_business_id,
        "target_business_id": req.target_business_id,
        "expires_in_days": days,
        "email": mb_email,
        "first_name": mb_first_name,
        "last_name": mb_last_name,
    });
    campaign_integrations::fire_marketing_boost(
        &s,
        &campaign.0,
        "voucher_issued",
        &mb_payload,
    ).await;

    Ok(Json(json!({
        "voucher_id": voucher_id,
        "code": code,
        "discount": req.discount_value,
        "expires_at": format!("{} days", days)
    })))
}

/// GET /api/v1/loyalty/my-vouchers — list active vouchers for a contact
pub async fn list_my_vouchers(
    State(s): State<AppState>,
    Path(contact_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let vouchers = sqlx::query_as::<_, (Uuid, String, String, String, String, String, Option<chrono::DateTime<chrono::Utc>>)>(
        r#"SELECT v.id, v.discount_value, v.voucher_type, v.redemption_code, v.status,
                  COALESCE(b.name, '') as business_name, v.expires_at
           FROM vouchers v
           LEFT JOIN businesses b ON b.id = v.target_business_id
           WHERE v.issued_to_contact_id = $1
           ORDER BY v.created_at DESC"#
    )
    .bind(contact_id)
    .fetch_all(&s.db)
    .await?;

    let result: Vec<serde_json::Value> = vouchers.into_iter().map(|v| json!({
        "id": v.0, "discount": v.1, "type": v.2, "code": v.3, "status": v.4,
        "business": v.5, "expires_at": v.6
    })).collect();

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
    .bind(&req.code.to_uppercase())
    .bind(req.contact_id)
    .fetch_optional(&s.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Voucher not found".into()))?;

    if voucher.2 != "active" {
        return Err(AppError::BadRequest("Voucher already used or expired".into()));
    }

    sqlx::query(
        "UPDATE vouchers SET status = 'used', used_at = NOW() WHERE id = $1"
    )
    .bind(voucher.0)
    .execute(&s.db)
    .await?;

    Ok(Json(json!({"status": "claimed", "discount": voucher.1})))
}

/// POST /api/v1/loyalty/expire-vouchers — expire old vouchers (cron)
pub async fn expire_vouchers(
    State(s): State<AppState>,
) -> Result<impl IntoResponse, AppError> {
    let result = sqlx::query(
        "UPDATE vouchers SET status = 'expired' WHERE status = 'active' AND expires_at < NOW()"
    )
    .execute(&s.db)
    .await?;

    Ok(Json(json!({"expired": result.rows_affected()})))
}

// ── Business Pledges ──

#[derive(Debug, Deserialize)]
pub struct CreatePledgeRequest {
    pub campaign_slug: String,
    pub business_id: Uuid,
    pub business_name: String,
    pub offer_type: String,
    pub offer_value: String,
    pub offer_description: Option<String>,
    pub min_purchase: Option<String>,
    pub valid_until: Option<chrono::DateTime<chrono::Utc>>,
}

/// POST /api/v1/business/pledge — business submits a reward pledge
pub async fn create_pledge(
    State(s): State<AppState>,
    Json(req): Json<CreatePledgeRequest>,
) -> Result<impl IntoResponse, AppError> {
    let campaign = sqlx::query_as::<_, (Uuid,)>(
        "SELECT id FROM campaigns WHERE slug = $1 LIMIT 1"
    )
    .bind(&req.campaign_slug)
    .fetch_optional(&s.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Campaign not found".into()))?;

    let pledge_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO business_pledges (id, campaign_id, business_id, business_name, offer_type, offer_value,
         offer_description, min_purchase, status, valid_until)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'pending', $9)"
    )
    .bind(pledge_id)
    .bind(campaign.0)
    .bind(req.business_id)
    .bind(&req.business_name)
    .bind(&req.offer_type)
    .bind(&req.offer_value)
    .bind(&req.offer_description)
    .bind(&req.min_purchase)
    .bind(req.valid_until)
    .execute(&s.db)
    .await?;

    Ok(Json(json!({"id": pledge_id, "status": "pending", "message": "Your pledge is under review. The directory team will approve it shortly."})))
}

/// GET /api/v1/business/pledges — list pledges for a business
pub async fn list_business_pledges(
    State(s): State<AppState>,
    Path(business_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let pledges = sqlx::query_as::<_, (Uuid, String, String, String, String)>(
        "SELECT id, offer_type, offer_value, offer_description, status FROM business_pledges WHERE business_id = $1 ORDER BY created_at DESC"
    )
    .bind(business_id)
    .fetch_all(&s.db)
    .await?;

    let result: Vec<serde_json::Value> = pledges.into_iter().map(|p| json!({
        "id": p.0, "offer_type": p.1, "offer_value": p.2,
        "description": p.3, "status": p.4
    })).collect();

    Ok(Json(json!({"pledges": result})))
}

// ── Admin: Pledge Approval ──

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
        return Err(AppError::BadRequest("Status must be 'approved' or 'rejected'".into()));
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

    let result: Vec<serde_json::Value> = pledges.into_iter().map(|p| json!({
        "id": p.0, "business": p.1, "offer_type": p.2, "offer_value": p.3,
        "description": p.4, "phone": p.5, "status": p.6
    })).collect();

    Ok(Json(json!({"pending_pledges": result})))
}

// ── Rotation Engine ──────────────────────────────────────────────────

/// When a purchase is verified, automatically issue a rotating voucher
/// from a non-competing business in the same rotation group.
pub async fn issue_rotation_voucher(
    pool: &sqlx::PgPool,
    campaign_id: &Uuid,
    contact_id: &Uuid,
    source_business_id: &Uuid,
) -> Result<Option<serde_json::Value>, AppError> {
    // Find active rotation configs for this campaign
    let configs = sqlx::query_as::<_, (Uuid, String, i32, String, i32)>(
        "SELECT id, name, group_size, rotation_frequency, max_vouchers_per_rotation
         FROM rotation_configs WHERE campaign_id = $1 AND is_active = true LIMIT 5"
    )
    .bind(campaign_id)
    .fetch_all(pool)
    .await?;

    for (config_id, config_name, group_size, frequency, max_vouchers) in &configs {
        // Find businesses in this rotation group, excluding the source business
        let targets = sqlx::query_as::<_, (Uuid, String, i32)>(
            "SELECT rgm.business_id, rgm.business_name, rgm.rotation_order
             FROM rotation_group_members rgm
             JOIN rotation_configs rc ON rc.id = rgm.rotation_config_id
             WHERE rgm.rotation_config_id = $1 AND rgm.is_active = true
             AND rgm.business_id != $2
             ORDER BY rgm.rotation_order ASC"
        )
        .bind(config_id)
        .bind(source_business_id)
        .fetch_all(pool)
        .await?;

        if targets.is_empty() {
            continue;
        }

        // Pick the next business in rotation order
        let target_idx = rand::thread_rng().gen_range(0..targets.len());
        let (target_id, target_name, _order) = &targets[target_idx];

        // Check if they have an active pledge
        let pledge = sqlx::query_as::<_, (String, String, Option<String>)>(
            "SELECT offer_type, offer_value, offer_description FROM business_pledges
             WHERE business_id = $1 AND status = 'active' AND is_active = true
             LIMIT 1"
        )
        .bind(target_id)
        .fetch_optional(pool)
        .await?;

        if let Some((offer_type, offer_value, offer_desc)) = pledge {
            // Generate voucher code
            let code: String = rand::thread_rng()
                .sample_iter(&rand::distributions::Alphanumeric)
                .take(8)
                .map(char::from)
                .collect::<String>()
                .to_uppercase();

            let voucher_id = Uuid::new_v4();
            let validity_days = 30;

            sqlx::query(
                "INSERT INTO vouchers (id, campaign_id, issued_to_contact_id, source_business_id,
                 target_business_id, voucher_type, discount_value, redemption_code, expires_at, status)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, NOW() + make_interval(days => $9), 'active')"
            )
            .bind(voucher_id)
            .bind(campaign_id)
            .bind(contact_id)
            .bind(source_business_id)
            .bind(target_id)
            .bind(&offer_type)
            .bind(&offer_value)
            .bind(&code)
            .bind(validity_days)
            .execute(pool)
            .await?;

            // Track the rotation
            sqlx::query(
                "UPDATE purchase_verifications SET voucher_id = $1 WHERE issued_to_contact_id = (SELECT id FROM purchase_verifications WHERE contact_id = $2 ORDER BY created_at DESC LIMIT 1)"
            )
            .bind(voucher_id)
            .bind(contact_id)
            .execute(pool)
            .await.ok();

            return Ok(Some(json!({
                "voucher_id": voucher_id,
                "code": code,
                "business_name": target_name,
                "offer": format!("{} {}", offer_value, offer_type),
                "expires_in_days": validity_days
            })));
        }
    }

    Ok(None)
}

// ── Rotation Group API ────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CreateRotationConfigRequest {
    pub campaign_slug: String,
    pub name: String,
    pub description: Option<String>,
    pub group_size: Option<i32>,
    pub rotation_frequency: Option<String>,
    pub voucher_validity_days: Option<i32>,
}

/// POST /api/v1/admin/rotation-configs — create a rotation config
pub async fn create_rotation_config(
    State(s): State<AppState>,
    Json(req): Json<CreateRotationConfigRequest>,
) -> Result<Json<Value>, AppError> {
    let campaign = sqlx::query_as::<_, (Uuid,)>(
        "SELECT id FROM campaigns WHERE slug = $1 LIMIT 1"
    )
    .bind(&req.campaign_slug)
    .fetch_optional(&s.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Campaign not found".into()))?;

    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO rotation_configs (id, campaign_id, name, description, group_size, rotation_frequency, voucher_validity_days)
         VALUES ($1, $2, $3, $4, $5, $6, $7)"
    )
    .bind(id)
    .bind(campaign.0)
    .bind(&req.name)
    .bind(&req.description)
    .bind(req.group_size.unwrap_or(4))
    .bind(req.rotation_frequency.as_deref().unwrap_or("weekly"))
    .bind(req.voucher_validity_days.unwrap_or(30))
    .execute(&s.db)
    .await?;

    Ok(Json(json!({"id": id, "status": "created"})))
}

#[derive(Debug, Deserialize)]
pub struct AddToRotationRequest {
    pub rotation_config_id: Uuid,
    pub business_id: Uuid,
    pub business_name: String,
    pub business_category: Option<String>,
    pub rotation_order: Option<i32>,
}

/// POST /api/v1/admin/rotation-members — add a business to a rotation group
pub async fn add_rotation_member(
    State(s): State<AppState>,
    Json(req): Json<AddToRotationRequest>,
) -> Result<Json<Value>, AppError> {
    sqlx::query(
        "INSERT INTO rotation_group_members (id, rotation_config_id, business_id, business_name, business_category, rotation_order)
         VALUES ($1, $2, $3, $4, $5, $6)
         ON CONFLICT (rotation_config_id, business_id) DO UPDATE SET is_active = true"
    )
    .bind(Uuid::new_v4())
    .bind(req.rotation_config_id)
    .bind(req.business_id)
    .bind(&req.business_name)
    .bind(&req.business_category)
    .bind(req.rotation_order.unwrap_or(0))
    .execute(&s.db)
    .await?;

    Ok(Json(json!({"status": "added"})))
}

/// DELETE /api/v1/admin/rotation-members/:config_id/:business_id — remove from rotation
pub async fn remove_rotation_member(
    State(s): State<AppState>,
    Path((config_id, business_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<Value>, AppError> {
    sqlx::query(
        "UPDATE rotation_group_members SET is_active = false WHERE rotation_config_id = $1 AND business_id = $2"
    )
    .bind(config_id)
    .bind(business_id)
    .execute(&s.db)
    .await?;

    Ok(Json(json!({"status": "removed"})))
}

/// GET /api/v1/admin/rotation-configs/:campaign_slug — list rotation configs for a campaign
pub async fn list_rotation_configs(
    State(s): State<AppState>,
    Path(campaign_slug): Path<String>,
) -> Result<Json<Value>, AppError> {
    let configs = sqlx::query_as::<_, (Uuid, String, Option<String>, i32, String, i32, bool)>(
        "SELECT rc.id, rc.name, rc.description, rc.group_size, rc.rotation_frequency,
                rc.voucher_validity_days, rc.is_active
         FROM rotation_configs rc
         JOIN campaigns c ON c.id = rc.campaign_id
         WHERE c.slug = $1
         ORDER BY rc.created_at DESC"
    )
    .bind(&campaign_slug)
    .fetch_all(&s.db)
    .await?;

    let result: Vec<Value> = configs.into_iter().map(|c| json!({
        "id": c.0, "name": c.1, "description": c.2, "group_size": c.3,
        "frequency": c.4, "validity_days": c.5, "is_active": c.6
    })).collect();

    Ok(Json(json!({"rotation_configs": result})))
}

/// GET /api/v1/admin/rotation-members/:config_id — list members of a rotation group
pub async fn list_rotation_members(
    State(s): State<AppState>,
    Path(config_id): Path<Uuid>,
) -> Result<Json<Value>, AppError> {
    let members = sqlx::query_as::<_, (Uuid, String, Option<String>, i32, bool)>(
        "SELECT business_id, business_name, business_category, rotation_order, is_active
         FROM rotation_group_members WHERE rotation_config_id = $1
         ORDER BY rotation_order ASC"
    )
    .bind(config_id)
    .fetch_all(&s.db)
    .await?;

    let result: Vec<Value> = members.into_iter().map(|m| json!({
        "id": m.0, "name": m.1, "category": m.2, "order": m.3, "active": m.4
    })).collect();

    Ok(Json(json!({"members": result})))
}

// ── Reward Redemption + Webhook Delivery ─────────────────────────────

#[derive(Debug, Deserialize)]
pub struct RedeemRewardRequest {
    pub contact_id: Uuid,
    pub reward_tier_id: Uuid,
    pub campaign_slug: String,
}

/// POST /api/v1/loyalty/redeem-reward — redeem ZaarCash/Pro Credits for a reward
/// Fires a webhook to the tenant's configured endpoint when redeemed.
pub async fn redeem_reward(
    State(s): State<AppState>,
    Json(req): Json<RedeemRewardRequest>,
) -> Result<Json<Value>, AppError> {
    // Get the campaign and reward tier
    let campaign = sqlx::query_as::<_, (Uuid, String, Option<String>, Option<Value>, Option<String>)>(
        "SELECT c.id, c.name, c.config->>'entry_webhook_url', c.config->'output_actions', c.delivery_method
         FROM campaigns c WHERE c.slug = $1 LIMIT 1"
    )
    .bind(&req.campaign_slug)
    .fetch_optional(&s.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Campaign not found".into()))?;

    let tier = sqlx::query_as::<_, (Uuid, String, i32, bool, Option<String>, Option<Value>, Option<Value>)>(
        "SELECT id, name, points_required, requires_approval, reward_tag, redeem_action_config, marketing_boost
         FROM loyalty_reward_tiers WHERE id = $1 LIMIT 1"
    )
    .bind(req.reward_tier_id)
    .fetch_optional(&s.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Reward tier not found".into()))?;

    // Check points balance
    let balance = sqlx::query_scalar::<_, i32>(
        "SELECT points_balance FROM campaign_points_balance WHERE campaign_id = $1 AND contact_id = $2"
    )
    .bind(campaign.0)
    .bind(req.contact_id)
    .fetch_optional(&s.db)
    .await?
    .unwrap_or(0);

    if balance < tier.2 {
        return Err(AppError::BadRequest(format!(
            "Insufficient points. You have {} but need {}.", balance, tier.2
        )));
    }

    // Deduct points
    sqlx::query(
        "UPDATE campaign_points_balance SET points_balance = points_balance - $1, updated_at = NOW()
         WHERE campaign_id = $2 AND contact_id = $3"
    )
    .bind(tier.2)
    .bind(campaign.0)
    .bind(req.contact_id)
    .execute(&s.db)
    .await?;

    // Record the reward
    sqlx::query(
        "INSERT INTO loyalty_rewards_earned (id, campaign_id, contact_id, reward_tier_id, points_spent, status)
         VALUES ($1, $2, $3, $4, $5, 'active')"
    )
    .bind(Uuid::new_v4())
    .bind(campaign.0)
    .bind(req.contact_id)
    .bind(tier.0)
    .bind(tier.2)
    .execute(&s.db)
    .await?;

    // Fire webhook to tenant (legacy entry_webhook_url config)
    let webhook_url = campaign.2.clone().unwrap_or_default();
    if !webhook_url.is_empty() {
        let payload = json!({
            "event": "reward_redeemed",
            "campaign": campaign.1,
            "reward": tier.1,
            "reward_tag": tier.4,
            "points_spent": tier.2,
            "contact_id": req.contact_id,
            "timestamp": chrono::Utc::now().to_rfc3339()
        });
        let client = reqwest::Client::new();
        let _ = client.post(&webhook_url)
            .json(&payload)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await;
    }

    // Look up contact info for Marketing Boost payload
    let mb_contact_lookup = sqlx::query_as::<_, (Option<String>, Option<String>, Option<String>)>(
        "SELECT email, first_name, last_name FROM contacts WHERE id = $1"
    )
    .bind(req.contact_id)
    .fetch_optional(&s.db)
    .await
    .ok()
    .flatten();

    let (mb_email, mb_first_name, mb_last_name) = mb_contact_lookup.unwrap_or((None, None, None));

    // Fire Marketing Boost webhook if configured
    // Per-reward marketing_boost (on the tier) takes priority over campaign-level config
    let mb_payload = json!({
        "reward": tier.1,
        "reward_tag": tier.4,
        "points_spent": tier.2,
        "contact_id": req.contact_id,
        "campaign_slug": req.campaign_slug,
        "email": mb_email,
        "first_name": mb_first_name,
        "last_name": mb_last_name,
    });
    campaign_integrations::fire_marketing_boost_with_override(
        &s,
        &campaign.0,
        "reward_redeemed",
        &mb_payload,
        tier.6.as_ref(),  // tier.6 = marketing_boost column
    ).await;

    Ok(Json(json!({
        "status": "redeemed",
        "reward": tier.1,
        "points_spent": tier.2,
        "needs_approval": tier.3
    })))
}

/// GET /api/v1/loyalty/rewards-earned/:contact_id — list rewards earned by a contact
pub async fn list_rewards_earned(
    State(s): State<AppState>,
    Path(contact_id): Path<Uuid>,
) -> Result<Json<Value>, AppError> {
    let rewards = sqlx::query_as::<_, (Uuid, String, i32, String, Option<chrono::DateTime<chrono::Utc>>)>(
        r#"SELECT lre.id, lrt.name, lre.points_spent, lre.status, lre.created_at
           FROM loyalty_rewards_earned lre
           JOIN loyalty_reward_tiers lrt ON lrt.id = lre.reward_tier_id
           WHERE lre.contact_id = $1
           ORDER BY lre.created_at DESC LIMIT 50"#
    )
    .bind(contact_id)
    .fetch_all(&s.db)
    .await?;

    let result: Vec<Value> = rewards.into_iter().map(|r| json!({
        "id": r.0, "reward": r.1, "points": r.2, "status": r.3, "earned_at": r.4
    })).collect();

    Ok(Json(json!({"rewards": result})))
}

// ── Cross-Platform Tag Sync (MultiDirectory → IncentiveSwift) ──────────────

/// POST /api/v1/loyalty/external/tag-contact
/// Receives tag events from MultiDirectory or CoreSwift.
/// Creates/updates a contact with the given tags.
pub async fn external_tag_contact(
    State(s): State<AppState>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, AppError> {
    let email = body.get("email").and_then(|v| v.as_str()).unwrap_or("");
    let first_name = body.get("first_name").and_then(|v| v.as_str()).unwrap_or("");
    let last_name = body.get("last_name").and_then(|v| v.as_str()).unwrap_or("");
    let phone = body.get("phone").and_then(|v| v.as_str()).unwrap_or("");
    let tags: Vec<String> = body.get("tags")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|t| t.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let source = body.get("source").and_then(|v| v.as_str()).unwrap_or("external");

    if email.is_empty() && phone.is_empty() && first_name.is_empty() && last_name.is_empty() {
        return Err(AppError::BadRequest("At least one contact identifier required".into()));
    }

    // Upsert the contact — find by email first, then by phone, or create
    let contact_id = crate::db::contacts::upsert_contact(&s.db, &crate::db::contacts::ContactInput {
        first_name: if first_name.is_empty() { None } else { Some(first_name.to_string()) },
        last_name: if last_name.is_empty() { None } else { Some(last_name.to_string()) },
        email: if email.is_empty() { None } else { Some(email.to_string()) },
        phone: if phone.is_empty() { None } else { Some(phone.to_string()) },
        business_name: None,
        website: None,
    }).await?;

    // Apply tags as notes2 (comma-separated, deduplicated)
    if !tags.is_empty() {
        let existing_notes: Option<String> = sqlx::query_scalar(
            "SELECT notes2 FROM contacts WHERE id = $1"
        )
        .bind(contact_id)
        .fetch_optional(&s.db)
        .await
        .ok()
        .flatten();

        let mut all_tags: Vec<String> = existing_notes
            .as_deref()
            .unwrap_or("")
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        for tag in &tags {
            let tag_trimmed = tag.trim().to_string();
            if !tag_trimmed.is_empty() && !all_tags.contains(&tag_trimmed) {
                all_tags.push(tag_trimmed);
            }
        }

        let notes2 = all_tags.join(", ");
        let _ = sqlx::query("UPDATE contacts SET notes2 = $1 WHERE id = $2")
            .bind(&notes2)
            .bind(contact_id)
            .execute(&s.db)
            .await;
    }

    tracing::info!(
        "[tag-sync] IncentiveSwift tag-contact: contact={} email={} tags={:?} source={}",
        contact_id, email, tags, source
    );

    Ok(Json(json!({
        "status": "ok",
        "contact_id": contact_id.to_string(),
        "tags_applied": tags.len(),
        "source": source,
    })))
}

// ── Purchase Verify (business-scanner auto-credit) ────────────────────────

#[derive(Debug, Deserialize)]
pub struct PurchaseVerifyRequest {
    pub contact_id: Uuid,
    pub amount: f64,
    pub pin: String,
    pub offer_id: Option<Uuid>,
}

/// POST /api/v1/loyalty/purchase/verify
/// Business scans customer QR code, enters PIN, and this endpoint
/// verifies the purchase and auto-credits the customer.
/// Credits awarded: amount * credit_rate (configurable per tenant).
/// If an offer_id is provided, redemption credits are deducted per offer cap.
pub async fn purchase_verify(
    State(s): State<AppState>,
    auth: AuthenticatedUser,
    Json(req): Json<PurchaseVerifyRequest>,
) -> Result<impl IntoResponse, AppError> {
    let business_account_id = Uuid::parse_str(&auth.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".into()))?;

    // Get the account's tenant_id and purchase_pin
    let account_info = sqlx::query_as::<_, (Option<Uuid>, String)>(
        "SELECT tenant_id, purchase_pin FROM accounts WHERE id = $1"
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

    // Read credit_rate from accounts (tenant) table
    let credit_rate: i32 = if let Some(tid) = tenant_id {
        sqlx::query_scalar("SELECT credit_rate FROM accounts WHERE id = $1")
            .bind(tid)
            .fetch_optional(&s.db)
            .await?
            .unwrap_or(10)
    } else {
        10 // default
    };

    // Verify contact exists
    let contact_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM contacts WHERE id = $1)"
    )
    .bind(req.contact_id)
    .fetch_one(&s.db)
    .await
    .unwrap_or(false);

    if !contact_exists {
        return Err(AppError::NotFound("Customer contact not found".into()));
    }

    // If an offer_id is provided, look up the offer and calculate redemption cap
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

        // Calculate max discount credits: min(cap_dollars * credit_rate, balance)
        // discount_percent is informational — the actual cap is in dollars
        let max_discount_credits = (cap_dollars as i32) * credit_rate;

        // Get customer's current balance
        let customer_balance: i32 = sqlx::query_scalar(
            "SELECT credits_balance FROM accounts WHERE id = $1"
        )
        .bind(req.contact_id)
        .fetch_optional(&s.db)
        .await?
        .unwrap_or(0);

        // Redeemed = min(max_discount_credits, customer_balance)
        redeemed_credits = max_discount_credits.min(customer_balance);
    }

    // Calculate earned credits using configurable credit_rate: amount * credit_rate
    let credit_amount = ((req.amount * credit_rate as f64).floor() as i32).max(1);

    // Update the contact's account credits
    // First check if contact has an account by same UUID
    let account_credits: Option<i32> = sqlx::query_scalar(
        "SELECT credits_balance FROM accounts WHERE id = $1"
    )
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

        let new_balance = (balance as i32 + net_change).max(0);
        sqlx::query(
            "UPDATE accounts SET credits_balance = $1 WHERE id = $2"
        )
        .bind(new_balance)
        .bind(req.contact_id)
        .execute(&s.db)
        .await?;

        // Log transaction(s)
        let tx_id = Uuid::new_v4();
        let mut desc = format!("Purchase verified -- {} credits earned (${:.2} purchase)", credit_amount, req.amount);

        if redeemed_credits > 0 {
            desc = format!(
                "{} credits earned, {} redeemed via '{}' offer (${:.2} purchase)",
                credit_amount, redeemed_credits,
                offer_name.as_deref().unwrap_or("Offer"),
                req.amount
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
        resp.insert("new_balance".to_string(), json!(new_balance));
        resp.insert("purchase_amount".to_string(), json!(req.amount));

        if redeemed_credits > 0 {
            resp.insert("credits_redeemed".to_string(), json!(redeemed_credits));
            resp.insert("offer_applied".to_string(), json!(offer_name));
        }

        resp.insert("message".to_string(), json!(format!("Purchase verified! You earned {} credits{}",
            credit_amount,
            if redeemed_credits > 0 { format!(" and redeemed {} credits", redeemed_credits) } else { String::new() }
        )));

        Ok(Json(Value::Object(resp)))
    } else {
        // Contact exists but no account with that UUID — return success with no credits
        Ok(Json(json!({
            "status": "contact_linked",
            "contact_id": req.contact_id,
            "credits_earned": 0,
            "purchase_amount": req.amount,
            "credits_eligible": credit_amount,
            "message": "Contact linked. Credits available after account registration.".to_string()
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
    let code_row = sqlx::query_scalar::<_, Option<String>>(
        "SELECT referrer_code FROM accounts WHERE id = $1"
    )
    .bind(account_id)
    .fetch_optional(&s.db)
    .await?
    .flatten();

    // Count referrals across ALL campaigns that reference this account's contact
    let referral_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM campaign_referrals WHERE referrer_contact_id = $1::uuid"
    )
    .bind(account_id)
    .fetch_optional(&s.db)
    .await?
    .unwrap_or(0);

    // Get the actual referral records
    let referrals = sqlx::query_as::<_, (Uuid, Uuid, Option<Uuid>, Option<Uuid>, String, String, bool, Option<chrono::DateTime<chrono::Utc>>, i32, i32, chrono::DateTime<chrono::Utc>)>(
        "SELECT id, campaign_id, referrer_contact_id, referee_contact_id, referral_code, source, converted, converted_at, click_count, points_earned, created_at
         FROM campaign_referrals WHERE referrer_contact_id = $1::uuid
         ORDER BY created_at DESC LIMIT 50"
    )
    .bind(account_id)
    .fetch_all(&s.db)
    .await?;

    let referral_list: Vec<Value> = referrals.into_iter().map(|r| json!({
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
    })).collect();

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
    
    let existing = sqlx::query_scalar::<_, Option<String>>(
        "SELECT referrer_code FROM accounts WHERE id = $1"
    )
    .bind(account_id)
    .fetch_optional(&s.db)
    .await?
    .flatten();
    
    if let Some(code) = existing {
        return Ok(Json(json!({"code": code, "message": "exists"})));
    }
    
    let code = format!("REF{:06}", rand::thread_rng().gen_range(0..999999));
    
    sqlx::query(
        "UPDATE accounts SET referrer_code = $1 WHERE id = $2"
    )
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
           ORDER BY lrt.points_required ASC"#
    )
    .bind(account_id)
    .fetch_all(&s.db)
    .await?;

    let rewards_list: Vec<Value> = tiers.into_iter().map(|t| json!({
        "id": t.0,
        "program_name": t.1,
        "name": t.2,
        "cost": t.3,
        "requires_approval": t.4,
    })).collect();

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
    let vouchers = sqlx::query_as::<_, (Uuid, String, String, String, String, Option<chrono::DateTime<chrono::Utc>>)>(
        r#"SELECT v.id, v.discount_value, v.voucher_type, v.redemption_code, v.status, v.expires_at
           FROM vouchers v
           WHERE v.issued_to_contact_id = $1
           ORDER BY v.created_at DESC LIMIT 50"#
    )
    .bind(account_id)
    .fetch_all(&s.db)
    .await?;

    let voucher_list: Vec<Value> = vouchers.into_iter().map(|v| json!({
        "id": v.0,
        "discount": v.1,
        "type": v.2,
        "code": v.3,
        "status": v.4,
        "expires_at": v.5,
    })).collect();

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
    let campaign = sqlx::query_as::<_, (Uuid, String)>(
        "SELECT id, name FROM campaigns WHERE slug = $1 AND status = 'active' LIMIT 1"
    )
    .bind(&campaign_slug)
    .fetch_optional(&s.db)
    .await?
    .ok_or_else(|| AppError::NotFound(
        format!("Campaign not found for slug: {}", campaign_slug)
    ))?;

    let campaign_id = campaign.0;
    let campaign_name = campaign.1;

    // If we have a visitor email, find or create the contact
    let contact_id = if let Some(ref email) = payload.visitor_email {
        // Try to find existing contact
        let existing = sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM contacts WHERE email = $1 LIMIT 1"
        )
        .bind(email)
        .fetch_optional(&s.db)
        .await?;

        match existing {
            Some(cid) => cid,
            None => {
                // Create new contact
                let new_id = Uuid::new_v4();
                sqlx::query(
                    "INSERT INTO contacts (id, email, notes2) VALUES ($1, $2, $3)"
                )
                .bind(new_id)
                .bind(email)
                .bind(&payload.applied_tags.as_ref().map(|t| t.join(", ")))
                .execute(&s.db)
                .await?;
                new_id
            }
        }
    } else {
        return Err(AppError::BadRequest("Visitor email is required".into()));
    };

    // Award 100 Zaarcash — upsert campaign_points_balance
    let existing_balance = sqlx::query_scalar::<_, i32>(
        "SELECT points_balance FROM campaign_points_balance 
         WHERE campaign_id = $1 AND contact_id = $2"
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
             WHERE campaign_id = $1 AND contact_id = $2"
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
        (0..8).map(|_| {
            let idx = rng.gen_range(0..CHARSET.len());
            CHARSET[idx] as char
        }).collect()
    };

    let thirty_days = chrono::Duration::days(30);
    let expires_at = chrono::Utc::now() + thirty_days;

    sqlx::query(
        "INSERT INTO vouchers (id, campaign_id, issued_to_contact_id, voucher_type, 
         discount_value, redemption_code, expires_at, status)
         VALUES ($1, $2, $3, 'restaurant_card', '$50.00', $4, $5, 'active')"
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
        "SELECT first_name, last_name FROM contacts WHERE id = $1"
    )
    .bind(contact_id)
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
    ).await;

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
