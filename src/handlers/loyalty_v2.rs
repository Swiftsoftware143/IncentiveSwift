//! Loyalty verification & voucher handlers
//! PIN generation, receipt verification, rotating vouchers, business pledges

use axum::{
    extract::{Path, State, Query},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use rand::Rng;
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use crate::state::AppState;
use crate::error::AppError;

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

    Ok(Json(json!({
        "status": "verified",
        "business_name": business_name,
        "message": format!("Purchase at {} verified!", business_name)
    })))
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
    .bind(req.voucher_type.unwrap_or_else(|| "discount".to_string()))
    .bind(&req.discount_value)
    .bind(&code)
    .bind(days)
    .execute(&s.db)
    .await?;

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
