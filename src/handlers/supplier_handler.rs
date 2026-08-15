//! B2B Supplier milestone rewards
//! POST /api/v1/loyalty/supplier/milestone
//! GET  /api/v1/loyalty/supplier/milestones/:business_id

use axum::{
    extract::{Path, State},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::error::AppError;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct MilestoneRequest {
    pub business_id: Uuid,
    pub milestone_type: String,
    pub description: Option<String>,
    pub contract_value: Option<f64>,
    pub contract_partner: Option<String>,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct SupplierMilestone {
    pub id: Uuid,
    pub business_id: Uuid,
    pub milestone_type: String,
    pub description: Option<String>,
    pub points_awarded: i32,
    pub awarded_at: chrono::DateTime<chrono::Utc>,
    pub contract_value: Option<rust_decimal::Decimal>,
    pub contract_partner: Option<String>,
}

/// POST /api/v1/loyalty/supplier/milestone
/// Business reports a B2B milestone (contract sign, review, referral, onboarding, community contribution)
pub async fn record_milestone(
    State(s): State<AppState>,
    Json(req): Json<MilestoneRequest>,
) -> Result<Json<Value>, AppError> {
    let valid_types = [
        "contract_sign",
        "verified_review",
        "supplier_referral",
        "onboarding",
        "community_contribution",
    ];
    if !valid_types.contains(&req.milestone_type.as_str()) {
        return Err(AppError::BadRequest(format!(
            "Invalid milestone_type. Must be one of: {}",
            valid_types.join(", ")
        )));
    }

    // Get points config from supplier_tier_config
    let config = {
        let row = sqlx::query_as::<_, (i32,i32,i32,i32,i32)>(
            "SELECT COALESCE(contract_sign_points,500), COALESCE(verified_review_points,250), COALESCE(supplier_referral_points,1000), COALESCE(onboarding_points,100), COALESCE(community_contribution_points,150) FROM supplier_tier_config WHERE is_active = true LIMIT 1"
        )
        .fetch_optional(&s.db)
        .await?
        .unwrap_or((500,250,1000,100,150));

        match req.milestone_type.as_str() {
            "contract_sign" => row.0,
            "verified_review" => row.1,
            "supplier_referral" => row.2,
            "onboarding" => row.3,
            _ => row.4,
        }
    };

    // Check monthly cap
    let month_key = chrono::Utc::now().format("%Y-%m").to_string();
    let earned_this_month: i32 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(points_awarded),0)::int4 FROM supplier_milestones WHERE business_id = $1 AND TO_CHAR(awarded_at, 'YYYY-MM') = $2"
    )
    .bind(&req.business_id)
    .bind(&month_key)
    .fetch_one(&s.db)
    .await?;

    let max_monthly: i32 = sqlx::query_scalar(
        "SELECT COALESCE(max_monthly_earn, 5000) FROM supplier_tier_config WHERE is_active = true LIMIT 1"
    )
    .fetch_one(&s.db)
    .await?;

    if earned_this_month + config > max_monthly {
        return Err(AppError::BadRequest(format!(
            "Monthly cap of {} ZC exceeded. Already earned {} ZC this month.",
            max_monthly, earned_this_month
        )));
    }

    // Insert milestone
    let milestone_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO supplier_milestones (id, business_id, milestone_type, description, points_awarded, contract_value, contract_partner) VALUES ($1, $2, $3, $4, $5, $6, $7)"
    )
    .bind(milestone_id)
    .bind(&req.business_id)
    .bind(&req.milestone_type)
    .bind(&req.description)
    .bind(config)
    .bind(req.contract_value.map(|v| rust_decimal::Decimal::try_from(v).ok()).flatten())
    .bind(&req.contract_partner)
    .execute(&s.db)
    .await?;

    tracing::info!(
        "B2B milestone: business={} type={} points={}",
        req.business_id,
        req.milestone_type,
        config
    );

    Ok(Json(json!({
        "success": true,
        "milestone_id": milestone_id,
        "points_awarded": config,
        "milestone_type": req.milestone_type
    })))
}

/// GET /api/v1/loyalty/supplier/milestones/:business_id
pub async fn get_milestones(
    State(s): State<AppState>,
    Path(business_id): Path<Uuid>,
) -> Result<Json<Vec<SupplierMilestone>>, AppError> {
    let milestones = sqlx::query_as::<_, SupplierMilestone>(
        "SELECT id, business_id, milestone_type, description, points_awarded, awarded_at, contract_value, contract_partner FROM supplier_milestones WHERE business_id = $1 ORDER BY awarded_at DESC LIMIT 100"
    )
    .bind(business_id)
    .fetch_all(&s.db)
    .await?;

    Ok(Json(milestones))
}
