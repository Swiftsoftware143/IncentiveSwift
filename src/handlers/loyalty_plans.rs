//! Loyalty plans — subscription tiers for business loyalty program

use axum::{
    extract::State,
    Json,
};
use serde::Serialize;
use uuid::Uuid;

use crate::state::AppState;
use crate::error::AppError;
use crate::security::auth::AuthenticatedUser;

#[derive(Serialize, sqlx::FromRow)]
pub struct LoyaltyPlan {
    pub id: Uuid,
    pub name: String,
    pub slug: String,
    pub monthly_price: i32,
    pub monthly_zc_pool: i32,
    pub features: Option<Vec<String>>,
}

#[derive(Serialize)]
pub struct PlanStatusResponse {
    pub enrolled: bool,
    pub plan: Option<String>,
    pub status: String,
    pub zc_pool_remaining: i32,
    pub zc_pool_total: i32,
    pub pool_reset_date: Option<String>,
}

/// GET /api/v1/loyalty/plans
/// Returns all active loyalty subscription plans available for business enrollment.
pub async fn list_plans(
    State(s): State<AppState>,
) -> Result<Json<Vec<LoyaltyPlan>>, AppError> {
    let plans = sqlx::query_as::<_, LoyaltyPlan>(
        "SELECT id, name, slug, monthly_price, monthly_zc_pool, features FROM loyalty_plans WHERE is_active = true ORDER BY monthly_price ASC"
    )
    .fetch_all(&s.db)
    .await?;
    Ok(Json(plans))
}

/// GET /api/v1/loyalty/plan/status
/// Returns the authenticated business account's current loyalty plan enrollment status.
pub async fn plan_status(
    State(s): State<AppState>,
    auth: AuthenticatedUser,
) -> Result<Json<PlanStatusResponse>, AppError> {
    let account_id = auth.account_id;

    let row = sqlx::query_as::<_, (Option<String>, String, i32, i32, Option<chrono::NaiveDate>)>(
        "SELECT loyalty_plan, loyalty_plan_status, zc_pool_remaining, zc_pool_total, pool_reset_date FROM accounts WHERE id = $1::uuid"
    )
    .bind(&account_id)
    .fetch_optional(&s.db)
    .await?
    .unwrap_or((None, "inactive".to_string(), 0, 0, None));

    Ok(Json(PlanStatusResponse {
        enrolled: row.0.is_some() && row.1 == "active",
        plan: row.0,
        status: row.1,
        zc_pool_remaining: row.2,
        zc_pool_total: row.3,
        pool_reset_date: row.4.map(|d| d.format("%Y-%m-%d").to_string()),
    }))
}
