//! Campaign handlers — list, get by slug, create.

use crate::access::feature_gate;
use crate::db::campaigns;
use crate::error::AppError;
use crate::security::auth::AuthenticatedUser;
use crate::state::AppState;
use axum::{
    extract::{Path, State},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

/// GET /api/v1/campaigns — list campaigns scoped to authenticated user's account.
pub async fn list_campaigns(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let account_id = uuid::Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;
    let campaigns = campaigns::list_campaigns(&state.db, &account_id).await?;
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

/// GET /api/v1/campaigns/by-subdomain/:slug — public campaigns for a tenant subdomain.
pub async fn get_campaigns_by_subdomain(
    State(state): State<AppState>,
    Path(t_slug): Path<String>,
) -> Result<Json<Value>, AppError> {
    let account_id = campaigns::get_account_by_slug(&state.db, &t_slug).await?;
    let campaigns = campaigns::list_campaigns(&state.db, &account_id).await?;
    Ok(Json(json!({ "campaigns": campaigns })))
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
    pub loyalty_program_id: Option<uuid::Uuid>,
    pub loyalty_points_per_play: Option<i32>,
    pub auto_enroll_loyalty: Option<bool>,
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
    let mut has_access =
        feature_gate::has_feature_access(&state, &user.account_id, &feature_key).await?;
    // Also check the catch-all 'all_mechanics' feature
    if !has_access {
        has_access =
            feature_gate::has_feature_access(&state, &user.account_id, "all_mechanics").await?;
    }
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
        loyalty_program_id: body.loyalty_program_id,
        loyalty_points_per_play: body.loyalty_points_per_play,
        auto_enroll_loyalty: body.auto_enroll_loyalty,
    };

    let campaign = campaigns::create_campaign(&state.db, &input).await?;
    Ok(Json(json!({ "campaign": campaign })))
}

/// Input for updating a campaign.
#[derive(Deserialize)]
pub struct UpdateCampaignBody {
    pub name: Option<String>,
    pub config: Option<Value>,
    pub outcome_tags: Option<Value>,
    pub delivery_method: Option<String>,
    pub delivery_config: Option<Value>,
    pub branding: Option<Value>,
    pub loyalty_program_id: Option<Option<uuid::Uuid>>,
    pub loyalty_points_per_play: Option<i32>,
    pub auto_enroll_loyalty: Option<bool>,
}

/// PUT /api/v1/campaigns/:slug — update campaign (authenticated).
pub async fn update_campaign(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(slug): Path<String>,
    Json(body): Json<UpdateCampaignBody>,
) -> Result<Json<Value>, AppError> {
    // Resolve campaign by slug or UUID
    let campaign = if let Ok(id) = uuid::Uuid::parse_str(&slug) {
        campaigns::get_campaign_by_id(&state.db, &id).await
    } else {
        campaigns::get_campaign_by_slug(&state.db, &slug).await
    };
    let campaign = campaign?;

    // Merge branding into existing campaign config if provided
    let config: Option<Value> = if let Some(ref branding) = body.branding {
        let mut merged = body.config.clone().unwrap_or(campaign.config);
        if let Some(obj) = merged.as_object_mut() {
            obj.insert("branding".to_string(), branding.clone());
        }
        Some(merged)
    } else {
        body.config.clone()
    };

    let campaign = campaigns::update_campaign(
        &state.db,
        &campaign.id,
        body.name.as_deref(),
        config.as_ref(),
        body.outcome_tags.as_ref(),
        body.delivery_method.as_deref(),
        body.delivery_config.as_ref(),
        body.loyalty_program_id,
        body.loyalty_points_per_play,
        body.auto_enroll_loyalty,
    )
    .await?;

    Ok(Json(json!({ "campaign": campaign })))
}

/// DELETE /api/v1/campaigns/:slug — delete campaign by slug (authenticated).
pub async fn delete_campaign_by_id(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(slug): Path<String>,
) -> Result<Json<Value>, AppError> {
    // Try as UUID first, then as slug
    let campaign = if let Ok(id) = uuid::Uuid::parse_str(&slug) {
        campaigns::get_campaign_by_id(&state.db, &id).await
    } else {
        campaigns::get_campaign_by_slug(&state.db, &slug).await
    };

    let campaign = campaign?;
    let deleted = campaigns::delete_campaign(&state.db, &campaign.id).await?;
    if !deleted {
        return Err(AppError::NotFound("Campaign not found".to_string()));
    }

    Ok(Json(json!({ "status": "deleted" })))
}

/// POST /api/v1/campaigns/:slug/clone — Clone a campaign with all config
pub async fn clone_campaign(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(slug): Path<String>,
) -> Result<Json<Value>, AppError> {
    let original = campaigns::get_campaign_by_slug(&state.db, &slug).await?;
    let new_name = format!("{} (Copy)", original.name);
    let new_slug = campaigns::generate_clone_slug(&original.name);
    let account_id = uuid::Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;
    let new_id = uuid::Uuid::new_v4();

    sqlx::query(
        r#"INSERT INTO campaigns (id, account_id, name, slug, type, status, config, tag_namespace, outcome_tags, delivery_method, delivery_config, loyalty_program_id, loyalty_points_per_play, auto_enroll_loyalty)
           VALUES ($1, $2, $3, $4, $5, 'draft', $6, $7, $8, $9, $10, $11, $12, $13)"#
    )
    .bind(new_id)
    .bind(account_id)
    .bind(&new_name)
    .bind(&new_slug)
    .bind(&original.r#type)
    .bind(&original.config)
    .bind(&original.tag_namespace)
    .bind(&original.outcome_tags)
    .bind(&original.delivery_method)
    .bind(&original.delivery_config)
    .bind(original.loyalty_program_id)
    .bind(original.loyalty_points_per_play)
    .bind(original.auto_enroll_loyalty)
    .execute(&state.db)
    .await?;

    let cloned = campaigns::get_campaign_by_slug(&state.db, &new_slug).await?;
    Ok(Json(json!({ "campaign": cloned })))
}
