//! Phase 2: Admin handlers for Campaign Milestones CRUD

use crate::error::AppError;
use crate::handlers::campaigns::campaign_for_caller;
use crate::mechanics::milestone_engine;
use crate::security::auth::AuthenticatedUser;
use crate::state::AppState;
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
    let campaign = campaign_for_caller(&state, &slug, &user).await?;

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
    let campaign = campaign_for_caller(&state, &slug, &user).await?;

    let milestone = milestone_engine::create_milestone(&state.db, &campaign.id, &body).await?;

    Ok(Json(json!({ "milestone": milestone })))
}

/// PUT /api/v1/campaigns/:slug/milestones/:milestone_id — update a milestone
///
/// SECURITY (kanban t_27e3e083): the campaign is part of the write, not just a gate in front of it.
/// This handler resolved the campaign under the caller's account and discarded it, so the UPDATE
/// bound only the milestone id — a tenant could name its own campaign and rewrite another tenant's
/// milestone. No row matched -> 404.
pub async fn update_milestone(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path((slug, milestone_id)): Path<(String, Uuid)>,
    Json(body): Json<milestone_engine::UpdateMilestoneInput>,
) -> Result<Json<Value>, AppError> {
    let campaign = campaign_for_caller(&state, &slug, &user).await?;

    let milestone =
        milestone_engine::update_milestone(&state.db, &campaign.id, &milestone_id, &body).await?;

    match milestone {
        Some(milestone) => Ok(Json(json!({ "milestone": milestone }))),
        None => Err(AppError::NotFound("Milestone not found".to_string())),
    }
}

/// DELETE /api/v1/campaigns/:slug/milestones/:milestone_id — delete a milestone
///
/// SECURITY (kanban t_27e3e083): the DELETE binds the campaign resolved under the caller's account,
/// so a foreign milestone id deletes nothing and the route answers 404.
pub async fn delete_milestone(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path((slug, milestone_id)): Path<(String, Uuid)>,
) -> Result<Json<Value>, AppError> {
    let campaign = campaign_for_caller(&state, &slug, &user).await?;

    let deleted =
        milestone_engine::delete_milestone(&state.db, &campaign.id, &milestone_id).await?;

    if !deleted {
        return Err(AppError::NotFound("Milestone not found".to_string()));
    }
    Ok(Json(json!({ "deleted": true })))
}

/// GET /api/v1/campaigns/:slug/milestones/achieved ??? list achieved milestones
pub async fn list_achieved_milestones(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(slug): Path<String>,
) -> Result<Json<Value>, AppError> {
    let campaign = campaign_for_caller(&state, &slug, &user).await?;

    let achieved = sqlx::query_as::<_, milestone_engine::MilestoneAchieved>(
        r#"SELECT ma.id, ma.milestone_id, ma.campaign_id, ma.contact_id,
                  ma.action_executed, ma.action_result, ma.achieved_at
           FROM campaign_milestones_achieved ma
           WHERE ma.campaign_id = $1
           ORDER BY ma.achieved_at DESC
           LIMIT 100"#,
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
