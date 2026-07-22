//! Offers CRUD — businesses create redemption offers for their loyalty credits.
//!
//! Offers define how credits can be redeemed at a business:
//!   - discount_percent: e.g. 25 means 25% off
//!   - cap_dollars: maximum dollar value per redemption (e.g. $6)
//!
//! Offers are tenant-scoped. company_admin can manage their own; super_admin/admin can manage any.

use axum::{
    extract::{Path, State},
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;
use chrono::{DateTime, Utc};

use crate::state::AppState;
use crate::error::AppError;
use crate::security::auth::AuthenticatedUser;

// ── Data Models ──

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct Offer {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub name: String,
    pub discount_percent: i32,
    pub cap_dollars: i32,
    pub active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
pub struct CreateOfferRequest {
    pub name: String,
    pub discount_percent: i32,
    pub cap_dollars: i32,
}

#[derive(Debug, Deserialize)]
pub struct UpdateOfferRequest {
    pub name: Option<String>,
    pub discount_percent: Option<i32>,
    pub cap_dollars: Option<i32>,
    pub active: Option<bool>,
}

// ── Auth Helper ──

/// Resolve the tenant_id for the authenticated user.
/// - super_admin / admin: read from path param (they specify tenant_id)
/// - company_admin: derive from their own account
fn resolve_tenant_id(user: &AuthenticatedUser) -> Result<String, AppError> {
    if user.role == "super_admin" || user.role == "admin" {
        // For super_admin/admin, they pass tenant_id as query param or we leave it open
        // We'll handle differently — they can see all, but for CRUD they act on their own tenant
        // unless they pass a tenant_id query param
        Ok(user.account_id.clone())
    } else {
        // company_admin — derive from their account
        Ok(user.account_id.clone())
    }
}

/// Look up the actual tenant UUID from the authenticated user's account.
async fn get_tenant_id(
    state: &AppState,
    user: &AuthenticatedUser,
) -> Result<Uuid, AppError> {
    let user_uuid = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid user account ID".into()))?;

    let tenant_id: Option<Uuid> = sqlx::query_scalar(
        "SELECT tenant_id FROM accounts WHERE id = $1"
    )
    .bind(user_uuid)
    .fetch_optional(&state.db)
    .await?;

    match tenant_id {
        Some(tid) => Ok(tid),
        None => {
            // For super_admin without a tenant, use their own account_id
            if user.role == "super_admin" || user.role == "admin" {
                Ok(user_uuid)
            } else {
                Err(AppError::NotFound("Tenant not found for user".into()))
            }
        }
    }
}

// ── Handler ──

/// GET /api/v1/admin/offers — list all offers for the authenticated tenant
/// super_admin/admin can optionally pass ?tenant_id= to list another tenant's offers
pub async fn list_offers(
    State(s): State<AppState>,
    user: AuthenticatedUser,
) -> Result<impl IntoResponse, AppError> {
    let tenant_id = get_tenant_id(&s, &user).await?;

    let offers = sqlx::query_as::<_, Offer>(
        "SELECT id, tenant_id, name, discount_percent, cap_dollars, active, created_at, updated_at
         FROM offers WHERE tenant_id = $1 ORDER BY created_at DESC"
    )
    .bind(tenant_id)
    .fetch_all(&s.db)
    .await?;

    Ok(Json(json!({ "offers": offers })))
}

/// POST /api/v1/admin/offers — create a new offer for the authenticated tenant
pub async fn create_offer(
    State(s): State<AppState>,
    user: AuthenticatedUser,
    Json(req): Json<CreateOfferRequest>,
) -> Result<impl IntoResponse, AppError> {
    let tenant_id = get_tenant_id(&s, &user).await?;

    // Validate input
    if req.name.trim().is_empty() {
        return Err(AppError::BadRequest("Offer name is required".into()));
    }
    if req.discount_percent <= 0 || req.discount_percent > 100 {
        return Err(AppError::BadRequest("Discount percent must be between 1 and 100".into()));
    }
    if req.cap_dollars <= 0 {
        return Err(AppError::BadRequest("Cap dollars must be greater than 0".into()));
    }

    let offer_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO offers (id, tenant_id, name, discount_percent, cap_dollars)
         VALUES ($1, $2, $3, $4, $5)"
    )
    .bind(offer_id)
    .bind(tenant_id)
    .bind(req.name.trim())
    .bind(req.discount_percent)
    .bind(req.cap_dollars)
    .execute(&s.db)
    .await?;

    let offer = sqlx::query_as::<_, Offer>(
        "SELECT id, tenant_id, name, discount_percent, cap_dollars, active, created_at, updated_at
         FROM offers WHERE id = $1"
    )
    .bind(offer_id)
    .fetch_one(&s.db)
    .await?;

    Ok(Json(json!({ "offer": offer, "message": "Offer created successfully" })))
}

/// GET /api/v1/admin/offers/:id — get one offer
pub async fn get_offer(
    State(s): State<AppState>,
    user: AuthenticatedUser,
    Path(offer_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let tenant_id = get_tenant_id(&s, &user).await?;

    let offer = sqlx::query_as::<_, Offer>(
        "SELECT id, tenant_id, name, discount_percent, cap_dollars, active, created_at, updated_at
         FROM offers WHERE id = $1 AND tenant_id = $2"
    )
    .bind(offer_id)
    .bind(tenant_id)
    .fetch_optional(&s.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Offer not found".into()))?;

    Ok(Json(json!({ "offer": offer })))
}

/// PUT /api/v1/admin/offers/:id — update an offer
pub async fn update_offer(
    State(s): State<AppState>,
    user: AuthenticatedUser,
    Path(offer_id): Path<Uuid>,
    Json(req): Json<UpdateOfferRequest>,
) -> Result<impl IntoResponse, AppError> {
    let tenant_id = get_tenant_id(&s, &user).await?;

    // Verify offer exists and belongs to tenant
    let existing = sqlx::query_as::<_, (Uuid,)>(
        "SELECT id FROM offers WHERE id = $1 AND tenant_id = $2"
    )
    .bind(offer_id)
    .bind(tenant_id)
    .fetch_optional(&s.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Offer not found".into()))?;

    // Build dynamic update
    if let Some(ref name) = req.name {
        if name.trim().is_empty() {
            return Err(AppError::BadRequest("Offer name cannot be empty".into()));
        }
        sqlx::query("UPDATE offers SET name = $1 WHERE id = $2")
            .bind(name.trim())
            .bind(offer_id)
            .execute(&s.db)
            .await?;
    }

    if let Some(dp) = req.discount_percent {
        if dp <= 0 || dp > 100 {
            return Err(AppError::BadRequest("Discount percent must be between 1 and 100".into()));
        }
        sqlx::query("UPDATE offers SET discount_percent = $1 WHERE id = $2")
            .bind(dp)
            .bind(offer_id)
            .execute(&s.db)
            .await?;
    }

    if let Some(cap) = req.cap_dollars {
        if cap <= 0 {
            return Err(AppError::BadRequest("Cap dollars must be greater than 0".into()));
        }
        sqlx::query("UPDATE offers SET cap_dollars = $1 WHERE id = $2")
            .bind(cap)
            .bind(offer_id)
            .execute(&s.db)
            .await?;
    }

    if let Some(active) = req.active {
        sqlx::query("UPDATE offers SET active = $1 WHERE id = $2")
            .bind(active)
            .bind(offer_id)
            .execute(&s.db)
            .await?;
    }

    // Touch updated_at
    sqlx::query("UPDATE offers SET updated_at = NOW() WHERE id = $1")
        .bind(offer_id)
        .execute(&s.db)
        .await?;

    let offer = sqlx::query_as::<_, Offer>(
        "SELECT id, tenant_id, name, discount_percent, cap_dollars, active, created_at, updated_at
         FROM offers WHERE id = $1"
    )
    .bind(offer_id)
    .fetch_one(&s.db)
    .await?;

    Ok(Json(json!({ "offer": offer, "message": "Offer updated successfully" })))
}

/// DELETE /api/v1/admin/offers/:id — deactivate an offer (soft delete)
pub async fn delete_offer(
    State(s): State<AppState>,
    user: AuthenticatedUser,
    Path(offer_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let tenant_id = get_tenant_id(&s, &user).await?;

    let existing = sqlx::query_as::<_, (Uuid,)>(
        "SELECT id FROM offers WHERE id = $1 AND tenant_id = $2"
    )
    .bind(offer_id)
    .bind(tenant_id)
    .fetch_optional(&s.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Offer not found".into()))?;

    // Soft delete — set active = false
    sqlx::query("UPDATE offers SET active = false, updated_at = NOW() WHERE id = $1")
        .bind(offer_id)
        .execute(&s.db)
        .await?;

    Ok(Json(json!({ "message": "Offer deactivated successfully" })))
}
