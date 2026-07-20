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

// ── Purchase Verification ──

#[derive(Debug, Deserialize)]
pub struct GeneratePinRequest {
    pub campaign_slug: String,
    pub business_id: Uuid,
    pub business_name: Option<String>,
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
        "INSERT INTO purchase_verifications (id, campaign_id, business_id, business_name, verification_type, pin_code, status)
         VALUES ($1, $2, $3, $4, 'pin', $5, 'pending')"
    )
    .bind(verification_id)
    .bind(campaign.0)
    .bind(req.business_id)
    .bind(&req.business_name)
    .bind(&pin)
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
    Json(req): Json<VerifyPurchaseRequest>,
) -> Result<impl IntoResponse, AppError> {
    let verification = sqlx::query_as::<_, (Uuid, Uuid, Uuid, String, Option<String>, String)>(
        "SELECT pv.id, pv.campaign_id, pv.business_id, pv.business_name, pv.pin_code, pv.status
         FROM purchase_verifications pv
         WHERE pv.pin_code = $1 AND pv.status = 'pending'
         LIMIT 1"
    )
    .bind(&req.pin_code.to_uppercase())
    .fetch_optional(&s.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Invalid or expired PIN".into()))?;

    // Mark verified
    sqlx::query(
        "UPDATE purchase_verifications SET status = 'verified', contact_id = $1, verified_at = NOW() WHERE id = $2"
    )
    .bind(req.contact_id)
    .bind(verification.0)
    .execute(&s.db)
    .await?;

    let business_name = if verification.3.is_empty() { "Business".to_string() } else { verification.3.clone() };

    // Auto-issue a rotating voucher from cross-promotion group
    let voucher = issue_rotation_voucher(
        &s.db,
        &verification.1,
        &req.contact_id,
        &verification.2,
    ).await?;

    let mut response = json!({
        "status": "verified",
        "business_name": business_name,
        "message": format!("Purchase at {} verified!", business_name)
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

    let tier = sqlx::query_as::<_, (Uuid, String, i32, bool, Option<String>, Option<Value>)>(
        "SELECT id, name, points_required, requires_approval, reward_tag, redeem_action_config
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

    // Fire Marketing Boost webhook if configured
    let mb_payload = json!({
        "reward": tier.1,
        "reward_tag": tier.4,
        "points_spent": tier.2,
        "contact_id": req.contact_id,
        "campaign_slug": req.campaign_slug,
    });
    campaign_integrations::fire_marketing_boost(
        &s,
        &campaign.0,
        "reward_redeemed",
        &mb_payload,
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
