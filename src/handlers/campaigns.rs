//! Campaign handlers — list, get by slug, create.

use crate::error::AppError;
use crate::state::AppState;
use crate::security::auth::AuthenticatedUser;
use crate::access::feature_gate;
use crate::db::campaigns;
use axum::{
    extract::{Path, State},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

/// GET /api/v1/campaigns — list all campaigns (authenticated).
pub async fn list_campaigns(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let campaigns = campaigns::list_campaigns(&state.db).await?;
    Ok(Json(json!({ "campaigns": campaigns })))
}

/// GET /api/v1/campaigns/:slug — public, cacheable.
pub async fn get_campaign(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> Result<Json<Value>, AppError> {
    let campaign = campaigns::get_campaign_by_slug(&state.db, &slug).await?;
    Ok(Json(json!({ "campaign": campaign })))
}

/// Input for creating a campaign.
#[derive(Deserialize)]
pub struct CreateCampaignBody {
    pub name: String,
    pub r#type: String,
    pub tag_namespace: String,
    pub config: Option<Value>,
    pub outcome_tags: Option<Value>,
    pub delivery_method: Option<String>,
    pub delivery_config: Option<Value>,
}

/// POST /api/v1/campaigns — create campaign (authenticated + feature-gated).
pub async fn create_campaign(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<CreateCampaignBody>,
) -> Result<Json<Value>, AppError> {
    // Feature gate: check if account can create campaigns
    let account_id = uuid::Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;

    let feature_key = format!("mechanic_{}", body.r#type);
    let has_access = feature_gate::has_feature_access(&state, &user.account_id, &feature_key).await?;
    if !has_access {
        return Err(AppError::Forbidden(format!(
            "Your plan does not include the '{}' mechanic. Upgrade to access this feature.",
            body.r#type
        )));
    }

    let input = campaigns::CreateCampaignInput {
        name: body.name,
        r#type: body.r#type,
        tag_namespace: body.tag_namespace,
        config: body.config,
        outcome_tags: body.outcome_tags,
        delivery_method: body.delivery_method,
        delivery_config: body.delivery_config,
        account_id,
    };

    let campaign = campaigns::create_campaign(&state.db, &input).await?;
    Ok(Json(json!({ "campaign": campaign })))
}
