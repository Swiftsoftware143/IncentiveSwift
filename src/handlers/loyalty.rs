//! Loyalty handlers — checkin, approve reward, deny reward.

use crate::error::AppError;
use crate::state::AppState;
use crate::security::auth::AuthenticatedUser;
use crate::db::{contacts, loyalty};
use crate::mechanics::loyalty_checkin;
use axum::{
    extract::{Path, State},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

/// Body for loyalty checkin.
#[derive(Deserialize)]
pub struct CheckinBody {
    pub program_slug: String,
    pub contact: super::entries::ContactBody,
    pub method: Option<String>,
}

/// POST /api/v1/loyalty/checkin — public-but-scoped.
/// Full flow: upsert contact -> find/create member -> check daily cap -> create entry
/// -> award points -> check thresholds -> auto-approve or pending -> push delivery.
pub async fn checkin(
    State(state): State<AppState>,
    Json(body): Json<CheckinBody>,
) -> Result<Json<Value>, AppError> {
    // 1. Upsert contact
    let contact_input = contacts::ContactInput {
        first_name: body.contact.first_name.clone(),
        last_name: body.contact.last_name.clone(),
        email: body.contact.email.clone(),
        phone: body.contact.phone.clone(),
        business_name: body.contact.business_name.clone(),
    };
    let contact_id = contacts::upsert_contact(&state.db, &contact_input).await?;

    // 2. Get loyalty program by slug — lookup from campaign slug
    let campaign = crate::db::campaigns::get_campaign_by_slug(&state.db, &body.program_slug).await?;
    let program = loyalty::get_program(&state.db, &campaign.id).await?;

    // 3. Process checkin
    let result = loyalty_checkin::process_checkin(
        &state,
        &program.id.to_string(),
        &contact_id.to_string(),
        body.method.as_deref().unwrap_or("web"),
    ).await?;

    // 4. Return result
    match result {
        loyalty_checkin::CheckinResult::Success { points_awarded, new_balance, rewards_awarded } => {
            Ok(Json(json!({
                "status": "ok",
                "points_awarded": points_awarded,
                "new_balance": new_balance,
                "rewards_awarded": rewards_awarded.iter().map(|r| json!({
                    "id": r.id,
                    "name": r.name,
                    "status": r.status,
                })).collect::<Vec<_>>(),
            })))
        }
        loyalty_checkin::CheckinResult::DailyCapReached { message } => {
            Ok(Json(json!({
                "status": "daily_cap_reached",
                "message": message,
            })))
        }
    }
}

/// Body for approving a reward.
#[derive(Deserialize)]
pub struct ApproveBody {
    pub approved_by: Option<String>,
}

/// POST /api/v1/loyalty/rewards/:id/approve — authenticated.
pub async fn approve_reward(
    State(state): State<AppState>,
    Path(id): Path<String>,
    _user: AuthenticatedUser,
    Json(body): Json<ApproveBody>,
) -> Result<Json<Value>, AppError> {
    let reward_id = Uuid::parse_str(&id)
        .map_err(|_| AppError::BadRequest("Invalid reward ID".to_string()))?;

    // Get reward
    let reward = loyalty::get_reward(&state.db, &reward_id).await?;

    if reward.status != "pending" {
        return Err(AppError::BadRequest(format!(
            "Reward is already {}", reward.status
        )));
    }

    let approved_by = body.approved_by
        .and_then(|s| Uuid::parse_str(&s).ok());

    // Update to approved
    loyalty::update_reward_status(
        &state.db,
        &reward_id,
        "approved",
        approved_by.as_ref(),
    ).await?;

    // Get tier info for tag
    let tier = loyalty::get_reward_tier(&state.db, &reward.tier_id).await?;

    // Get member to find contact_id
    let member = loyalty::get_member(&state.db, &reward.member_id).await?;

    // Apply reward tag to contact
    loyalty::apply_reward_tag(&state.db, &member.contact_id, &tier.reward_tag).await?;

    Ok(Json(json!({
        "status": "approved",
        "reward_id": id,
        "reward_tag": tier.reward_tag,
        "message": "Reward approved and tag applied"
    })))
}

/// POST /api/v1/loyalty/rewards/:id/deny — authenticated.
pub async fn deny_reward(
    State(state): State<AppState>,
    Path(id): Path<String>,
    _user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let reward_id = Uuid::parse_str(&id)
        .map_err(|_| AppError::BadRequest("Invalid reward ID".to_string()))?;

    // Get reward
    let reward = loyalty::get_reward(&state.db, &reward_id).await?;

    if reward.status != "pending" {
        return Err(AppError::BadRequest(format!(
            "Reward is already {}", reward.status
        )));
    }

    // Update to denied
    loyalty::update_reward_status(
        &state.db,
        &reward_id,
        "denied",
        None,
    ).await?;

    Ok(Json(json!({
        "status": "denied",
        "reward_id": id,
        "message": "Reward denied"
    })))
}
