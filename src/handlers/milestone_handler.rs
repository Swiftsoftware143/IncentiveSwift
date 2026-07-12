//! Phase 2: Admin handlers for Campaign Milestones CRUD

use crate::error::AppError;
use crate::state::AppState;
use crate::security::auth::AuthenticatedUser;
use crate::mechanics::milestone_engine;
use axum::{
    extract::{Path, State},
    Json,
};
use serde_json::{json, Value};
use uuid::Uuid;

/// GET /api/v1/campaigns/:slug/milestones ??? list milestones for a campaign
pub async fn list_milestones(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(slug): Path<String>,
) -> Result<Json<Value>, AppError> {
    let campaign = crate::db::campaigns::get_campaign_by_slug(&state.db, &slug).await?;

    let milestones = milestone_engine::list_milestones(&state.db, &campaign.id).await?;

    Ok(Json(json!({
        "milestones": milestones,
        "count": milestones.len(),
    })))
}

/// POST /api/v1/campaigns/:slug/milestones ??? create a milestone
pub async fn create_milestone(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(slug): Path<String>,
    Json(body): Json<milestone_engine::CreateMilestoneInput>,
) -> Result<Json<Value>, AppError> {
    let campaign = crate::db::campaigns::get_campaign_by_slug(&state.db, &slug).await?;

    let milestone = milestone_engine::create_milestone(&state.db, &campaign.id, &body).await?;

    Ok(Json(json!({ "milestone": milestone })))
}

/// PUT /api/v1/campaigns/:slug/milestones/:milestone_id ??? update a milestone
pub async fn update_milestone(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path((slug, milestone_id)): Path<(String, Uuid)>,
    Json(body): Json<milestone_engine::UpdateMilestoneInput>,
) -> Result<Json<Value>, AppError> {
    let _campaign = crate::db::campaigns::get_campaign_by_slug(&state.db, &slug).await?;

    let milestone = milestone_engine::update_milestone(&state.db, &milestone_id, &body).await?;

    Ok(Json(json!({ "milestone": milestone })))
}

/// DELETE /api/v1/campaigns/:slug/milestones/:milestone_id ??? delete a milestone
pub async fn delete_milestone(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path((slug, milestone_id)): Path<(String, Uuid)>,
) -> Result<Json<Value>, AppError> {
    let _campaign = crate::db::campaigns::get_campaign_by_slug(&state.db, &slug).await?;

    milestone_engine::delete_milestone(&state.db, &milestone_id).await?;

    Ok(Json(json!({ "deleted": true })))
}

/// GET /api/v1/campaigns/:slug/milestones/achieved ??? list achieved milestones
pub async fn list_achieved_milestones(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(slug): Path<String>,
) -> Result<Json<Value>, AppError> {
    let campaign = crate::db::campaigns::get_campaign_by_slug(&state.db, &slug).await?;

    let achieved = sqlx::query_as::<_, milestone_engine::MilestoneAchieved>(
        r#"SELECT ma.id, ma.milestone_id, ma.campaign_id, ma.contact_id,
                  ma.action_executed, ma.action_result, ma.achieved_at
           FROM campaign_milestones_achieved ma
           WHERE ma.campaign_id = $1
           ORDER BY ma.achieved_at DESC
           LIMIT 100"#
    )
    .bind(campaign.id)
    .fetch_all(&state.db)
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;

    Ok(Json(json!({
        "achieved": achieved,
        "count": achieved.len(),
    })))
}
