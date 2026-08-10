//! Clearinghouse config handler — CRUD for economic parameters
//! GET/PUT /api/v1/admin/clearinghouse/config
//! GET/PUT /api/v1/admin/clearinghouse/caps
//! GET /api/v1/admin/clearinghouse/supplier-config

use axum::{extract::State, Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use rust_decimal::Decimal;

use crate::state::AppState;
use crate::error::AppError;

// ── Treasury Config ──

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct TreasuryConfig {
    pub issuance_rate: Decimal,
    pub redemption_rate: Decimal,
    pub platform_spread_percent: Decimal,
    pub minimum_float: Decimal,
    pub default_monthly_expiry_months: i32,
}

/// GET /api/v1/admin/clearinghouse/config
pub async fn get_treasury_config(State(s): State<AppState>) -> Result<Json<Value>, AppError> {
    let row = sqlx::query_as::<_, (Decimal, Decimal, Decimal)>(
        "SELECT COALESCE(total_revenue_collected / NULLIF(total_points_issued,0) * 100, 1.00) as rate,
                COALESCE(total_reimbursements_paid / NULLIF(total_points_redeemed,0) * 100, 0.80) as redeem_rate,
                COALESCE(minimum_float, 100.00) as min_float
         FROM point_treasury LIMIT 1"
    )
    .fetch_optional(&s.db)
    .await?
    .unwrap_or((Decimal::new(100, 2), Decimal::new(80, 2), Decimal::new(10000, 2)));

    Ok(Json(json!({
        "issuance_rate": 0.01,
        "redemption_rate": 0.008,
        "platform_spread_percent": 20.0,
        "minimum_float": row.2,
        "default_monthly_expiry_months": 12
    })))
}

/// PUT /api/v1/admin/clearinghouse/config
#[derive(Debug, Deserialize)]
pub struct UpdateTreasuryConfig {
    pub minimum_float: Option<Decimal>,
}

pub async fn update_treasury_config(
    State(s): State<AppState>,
    Json(req): Json<UpdateTreasuryConfig>,
) -> Result<Json<Value>, AppError> {
    if let Some(min_float) = req.minimum_float {
        sqlx::query("UPDATE point_treasury SET minimum_float = $1, updated_at = NOW()")
            .bind(min_float)
            .execute(&s.db)
            .await?;
    }
    Ok(Json(json!({"success": true})))
}

// ── Category Caps ──

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct CategoryCap {
    pub category_name: String,
    pub max_redeem_percent: Decimal,
    pub description: Option<String>,
}

/// GET /api/v1/admin/clearinghouse/caps
pub async fn get_category_caps(State(s): State<AppState>) -> Result<Json<Vec<CategoryCap>>, AppError> {
    let caps = sqlx::query_as::<_, CategoryCap>(
        "SELECT category_name as category_name, max_redeem_percent, description FROM category_redeem_caps ORDER BY category_name"
    )
    .fetch_all(&s.db)
    .await?;
    Ok(Json(caps))
}

/// PUT /api/v1/admin/clearinghouse/caps
#[derive(Debug, Deserialize)]
pub struct UpdateCap {
    pub category_name: String,
    pub max_redeem_percent: Option<Decimal>,
    pub description: Option<String>,
}

pub async fn update_category_cap(
    State(s): State<AppState>,
    Json(req): Json<UpdateCap>,
) -> Result<Json<Value>, AppError> {
    let mut set_clauses = vec![];
    let mut params: Vec<&(dyn sqlx::Encode<'_, sqlx::Postgres> + Sync + 'static)> = vec![];

    if let Some(pct) = &req.max_redeem_percent {
        set_clauses.push("max_redeem_percent");
    }
    if req.description.is_some() {
        set_clauses.push("description");
    }

    if set_clauses.is_empty() {
        return Err(AppError::BadRequest("No fields to update".into()));
    }

    // Simple approach — execute update with direct bindings
    sqlx::query(
        "UPDATE category_redeem_caps SET max_redeem_percent = COALESCE($1, max_redeem_percent), description = COALESCE($2, description) WHERE category_name = $3"
    )
    .bind(req.max_redeem_percent)
    .bind(&req.description)
    .bind(&req.category_name)
    .execute(&s.db)
    .await?;

    Ok(Json(json!({"success": true, "category": req.category_name})))
}

// ── Supplier Tier Config ──

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct SupplierTierConfigRow {
    pub id: uuid::Uuid,
    pub program_id: uuid::Uuid,
    pub contract_sign_points: i32,
    pub verified_review_points: i32,
    pub supplier_referral_points: i32,
    pub onboarding_points: i32,
    pub community_contribution_points: i32,
    pub max_monthly_earn: i32,
    pub is_active: bool,
}

/// GET /api/v1/admin/clearinghouse/supplier-config
pub async fn get_supplier_config(State(s): State<AppState>) -> Result<Json<Vec<SupplierTierConfigRow>>, AppError> {
    let rows = sqlx::query_as::<_, SupplierTierConfigRow>(
        "SELECT id, program_id, contract_sign_points, verified_review_points, supplier_referral_points, onboarding_points, community_contribution_points, max_monthly_earn, is_active FROM supplier_tier_config"
    )
    .fetch_all(&s.db)
    .await?;
    Ok(Json(rows))
}

/// PUT /api/v1/admin/clearinghouse/supplier-config/:id
#[derive(Debug, Deserialize)]
pub struct UpdateSupplierConfig {
    pub contract_sign_points: Option<i32>,
    pub verified_review_points: Option<i32>,
    pub supplier_referral_points: Option<i32>,
    pub onboarding_points: Option<i32>,
    pub community_contribution_points: Option<i32>,
    pub max_monthly_earn: Option<i32>,
    pub is_active: Option<bool>,
}

pub async fn update_supplier_config(
    State(s): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<uuid::Uuid>,
    Json(req): Json<UpdateSupplierConfig>,
) -> Result<Json<Value>, AppError> {
    sqlx::query(
        "UPDATE supplier_tier_config SET
            contract_sign_points = COALESCE($1, contract_sign_points),
            verified_review_points = COALESCE($2, verified_review_points),
            supplier_referral_points = COALESCE($3, supplier_referral_points),
            onboarding_points = COALESCE($4, onboarding_points),
            community_contribution_points = COALESCE($5, community_contribution_points),
            max_monthly_earn = COALESCE($6, max_monthly_earn),
            is_active = COALESCE($7, is_active),
            updated_at = NOW()
         WHERE id = $8"
    )
    .bind(req.contract_sign_points)
    .bind(req.verified_review_points)
    .bind(req.supplier_referral_points)
    .bind(req.onboarding_points)
    .bind(req.community_contribution_points)
    .bind(req.max_monthly_earn)
    .bind(req.is_active)
    .bind(id)
    .execute(&s.db)
    .await?;

    Ok(Json(json!({"success": true})))
}
