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
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use crate::error::AppError;
use crate::security::auth::AuthenticatedUser;
use crate::state::AppState;

/// `offers.name` is `character varying(255)` (live `information_schema`, 2026-10-17).
const OFFER_NAME_CHARS: usize = 255;

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

/// The ACCOUNT id every offer is scoped to.
///
/// `offers.tenant_id` is a foreign key to **`accounts(id)`** (`offers_tenant_id_fkey`, live
/// `pg_get_constraintdef` 2026-10-17), not to `tenants(id)`. This function used to resolve
/// `accounts.tenant_id` — a `tenants.id` — and bind THAT into the column, so every write failed the
/// foreign key and every read filtered on a value no row can hold. Measured LIVE on this app with a
/// real operator credential (2026-10-17):
///
///   POST /api/v1/admin/offers {"name":"Probe control two",…}  -> 500, log:
///       insert or update on table "offers" violates foreign key constraint "offers_tenant_id_fkey"
///   GET  /api/v1/admin/offers                                 -> 200 {"offers":[]} (a row can never match)
///
/// The redemption path already treats the column as an account id
/// (`loyalty_v2::redeem` → `WHERE id = $1 AND tenant_id = $2` bound to `business_account_id`), so the
/// caller's own account id is the value the rest of the app means by this column.
fn get_tenant_id(_state: &AppState, user: &AuthenticatedUser) -> Result<Uuid, AppError> {
    Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid user account ID".into()))
}

/// Roles that may CHANGE the account's offers.
///
/// `company_admin` is the role a self-service signup receives
/// (`handlers::auth_handler::register`), i.e. the business owner the offers belong to;
/// `admin` / `super_admin` are the platform operators. A plain `user` / `business_user`
/// member of the account may READ its offer list (it is their own business's offers) but
/// not rewrite them.
///
/// Why this lives in the handler and not in a router layer (measured 2026-10-17): these
/// endpoints were reachable only under `/api/v1/admin/*`, which
/// `security::auth::admin_guard` refuses to a `company_admin` with 403 — so the console's
/// Offers screen failed for EVERY real customer while the handlers themselves had no role
/// logic at all. The tenant-scoped paths below fix the reachability; this guard keeps the
/// write side to the owner.
const OFFER_WRITER_ROLES: [&str; 3] = ["company_admin", "admin", "super_admin"];

fn require_offer_writer(user: &AuthenticatedUser) -> Result<(), AppError> {
    if OFFER_WRITER_ROLES.contains(&user.role.as_str()) {
        Ok(())
    } else {
        Err(AppError::Forbidden(
            "Only the account owner can change this account's offers".into(),
        ))
    }
}

// ── Handler ──

/// GET /api/v1/admin/offers — list all offers for the authenticated tenant
/// super_admin/admin can optionally pass ?tenant_id= to list another tenant's offers
pub async fn list_offers(
    State(s): State<AppState>,
    user: AuthenticatedUser,
) -> Result<impl IntoResponse, AppError> {
    let tenant_id = get_tenant_id(&s, &user)?;

    let offers = sqlx::query_as::<_, Offer>(
        "SELECT id, tenant_id, name, discount_percent, cap_dollars, active, created_at, updated_at
         FROM offers WHERE tenant_id = $1 ORDER BY created_at DESC",
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
    require_offer_writer(&user)?;
    let tenant_id = get_tenant_id(&s, &user)?;

    // Validate input
    if req.name.trim().is_empty() {
        return Err(AppError::BadRequest("Offer name is required".into()));
    }
    // `offers.name` is varchar(255): without this the INSERT came back 22001 and the caller saw a
    // bare 500 "Internal server error" (measured live 2026-10-17 with a 400-character name).
    crate::security::display_name::check_len(req.name.trim(), OFFER_NAME_CHARS, "name")
        .map_err(AppError::UnprocessableEntity)?;
    if req.discount_percent <= 0 || req.discount_percent > 100 {
        return Err(AppError::BadRequest(
            "Discount percent must be between 1 and 100".into(),
        ));
    }
    if req.cap_dollars <= 0 {
        return Err(AppError::BadRequest(
            "Cap dollars must be greater than 0".into(),
        ));
    }

    let offer_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO offers (id, tenant_id, name, discount_percent, cap_dollars)
         VALUES ($1, $2, $3, $4, $5)",
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
         FROM offers WHERE id = $1",
    )
    .bind(offer_id)
    .fetch_one(&s.db)
    .await?;

    Ok(Json(
        json!({ "offer": offer, "message": "Offer created successfully" }),
    ))
}

/// GET /api/v1/admin/offers/:id — get one offer
pub async fn get_offer(
    State(s): State<AppState>,
    user: AuthenticatedUser,
    Path(offer_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let tenant_id = get_tenant_id(&s, &user)?;

    let offer = sqlx::query_as::<_, Offer>(
        "SELECT id, tenant_id, name, discount_percent, cap_dollars, active, created_at, updated_at
         FROM offers WHERE id = $1 AND tenant_id = $2",
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
    require_offer_writer(&user)?;
    let tenant_id = get_tenant_id(&s, &user)?;

    // Verify offer exists and belongs to tenant
    let existing =
        sqlx::query_as::<_, (Uuid,)>("SELECT id FROM offers WHERE id = $1 AND tenant_id = $2")
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
        crate::security::display_name::check_len(name.trim(), OFFER_NAME_CHARS, "name")
            .map_err(AppError::UnprocessableEntity)?;
        sqlx::query("UPDATE offers SET name = $1 WHERE id = $2")
            .bind(name.trim())
            .bind(offer_id)
            .execute(&s.db)
            .await?;
    }

    if let Some(dp) = req.discount_percent {
        if dp <= 0 || dp > 100 {
            return Err(AppError::BadRequest(
                "Discount percent must be between 1 and 100".into(),
            ));
        }
        sqlx::query("UPDATE offers SET discount_percent = $1 WHERE id = $2")
            .bind(dp)
            .bind(offer_id)
            .execute(&s.db)
            .await?;
    }

    if let Some(cap) = req.cap_dollars {
        if cap <= 0 {
            return Err(AppError::BadRequest(
                "Cap dollars must be greater than 0".into(),
            ));
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
         FROM offers WHERE id = $1",
    )
    .bind(offer_id)
    .fetch_one(&s.db)
    .await?;

    Ok(Json(
        json!({ "offer": offer, "message": "Offer updated successfully" }),
    ))
}

/// DELETE /api/v1/admin/offers/:id — deactivate an offer (soft delete)
pub async fn delete_offer(
    State(s): State<AppState>,
    user: AuthenticatedUser,
    Path(offer_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    require_offer_writer(&user)?;
    let tenant_id = get_tenant_id(&s, &user)?;

    let existing =
        sqlx::query_as::<_, (Uuid,)>("SELECT id FROM offers WHERE id = $1 AND tenant_id = $2")
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
