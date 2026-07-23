//! Plan management handlers — CRUD for plans table and admin plan assignment.

use crate::error::AppError;
use crate::state::AppState;
use crate::security::auth::AuthenticatedUser;
use axum::{
    extract::{Path, State},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;
use reqwest::Client as HttpClient;

/// Notify the FunnelSwift affiliate system about a plan change.
/// Calls FunnelSwift's conversion webhook endpoint to sync plan data.
async fn sync_to_funnelswift_affiliates(
    action: &str,
    plan_name: &str,
    plan_price_monthly: f64,
    plan_id: &str,
    is_active: bool,
) {
    let payload = json!({
        "action": action,
        "plan_name": plan_name,
        "plan_price": plan_price_monthly,
        "plan_id": plan_id,
        "source_app": "incentiveswift",
        "is_active": is_active,
        "owner_name": "SwiftSoftware",
        "product_type": "software"
    });

    // Try local FunnelSwift webhook; silently ignore if unavailable
    if let Ok(client) = HttpClient::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
    {
        let _ = client
            .post("http://localhost:8080/api/v1/internal/sync-affiliate-plan")
            .json(&payload)
            .send()
            .await;
    }
}

/// Fire-and-forget sync of a plan to FunnelSwift's affiliate_products
/// using the configured funnelswift_url and internal_sync_key.
async fn sync_plan_to_affiliate(
    config: &crate::config::AppConfig,
    action: &str,
    plan_name: &str,
    plan_price_monthly: f64,
    is_active: bool,
) {
    let url = format!("{}/api/v1/internal/sync-affiliate-plan", config.funnelswift_url.trim_end_matches('/'));
    let api_key = config.internal_sync_key.clone();

    let action_owned = action.to_string();
    let plan_name_owned = plan_name.to_string();

    let payload = json!({
        "action": &action_owned,
        "plan_name": &plan_name_owned,
        "plan_price": plan_price_monthly,
        "source_app": "incentiveswift",
        "is_active": is_active,
        "owner_name": "SwiftSoftware",
        "product_type": "software",
        "api_key": &api_key,
    });

    tokio::spawn(async move {
        match reqwest::Client::new()
            .post(&url)
            .json(&payload)
            .send()
            .await
        {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    tracing::info!("sync-affiliate-plan {} {}: {}", action_owned, plan_name_owned, status);
                } else {
                    let body = resp.text().await.unwrap_or_default();
                    tracing::warn!("sync-affiliate-plan {} {} failed: {} - {}", action_owned, plan_name_owned, status, body);
                }
            }
            Err(e) => tracing::warn!("sync-affiliate-plan {} {} error: {}", action_owned, plan_name_owned, e),
        }
    });
}

/// A plan record.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Plan {
    pub id: Uuid,
    pub name: String,
    pub slug: String,
    pub description: Option<String>,
    pub price_monthly: f64,
    pub price_yearly: f64,
    pub features: Value,
    pub is_active: bool,
    pub sort_order: i32,
    pub payment_provider: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// Input for creating a plan.
#[derive(Deserialize)]
pub struct CreatePlanInput {
    pub name: String,
    pub slug: Option<String>,
    pub description: Option<String>,
    pub price_monthly: Option<f64>,
    pub price_yearly: Option<f64>,
    pub features: Option<Value>,
    pub is_active: Option<bool>,
    pub sort_order: Option<i32>,
    pub payment_provider: Option<String>,
}

/// Input for updating a plan.
#[derive(Deserialize)]
pub struct UpdatePlanInput {
    pub name: Option<String>,
    pub slug: Option<String>,
    pub description: Option<String>,
    pub price_monthly: Option<f64>,
    pub price_yearly: Option<f64>,
    pub features: Option<Value>,
    pub is_active: Option<bool>,
    pub sort_order: Option<i32>,
    pub payment_provider: Option<String>,
}

/// Input for admin plan assignment.
#[derive(Deserialize)]
pub struct AssignPlanInput {
    pub plan_id: String,
    pub account_id: String,
}

/// Input for updating plan features JSONB only.
#[derive(Deserialize)]
pub struct UpdateFeaturesInput {
    pub features: Value,
}

/// Generate a URL-safe slug.
fn generate_slug(name: &str) -> String {
    let slug: String = name
        .to_lowercase()
        .chars()
        .map(|c| match c {
            'a'..='z' | '0'..='9' | '-' => c,
            ' ' | '_' => '-',
            _ => '-',
        })
        .collect();

    let slug: String = slug
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");

    if slug.is_empty() {
        Uuid::new_v4().to_string()
    } else {
        slug
    }
}

/// GET /api/v1/admin/plans
pub async fn list_plans(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let plans = sqlx::query_as::<_, Plan>(
        r#"SELECT id, name, slug, description, price_monthly, price_yearly,
                  features, is_active, sort_order, payment_provider, created_at, updated_at
           FROM plans
           WHERE is_active = true
           ORDER BY sort_order, name"#
    )
    .fetch_all(&state.db)
    .await?;

    Ok(Json(json!({ "plans": plans })))
}

/// POST /api/v1/admin/plans
pub async fn create_plan(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Json(body): Json<CreatePlanInput>,
) -> Result<Json<Value>, AppError> {
    let slug = body.slug.unwrap_or_else(|| generate_slug(&body.name));
    let id = Uuid::new_v4();
    let features = body.features.unwrap_or_else(|| json!({}));

    sqlx::query(
        r#"INSERT INTO plans (id, name, slug, description, price_monthly, price_yearly, features, is_active, sort_order, payment_provider)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)"#
    )
    .bind(id)
    .bind(&body.name)
    .bind(&slug)
    .bind(&body.description)
    .bind(body.price_monthly.unwrap_or(0.0))
    .bind(body.price_yearly.unwrap_or(0.0))
    .bind(&features)
    .bind(body.is_active.unwrap_or(true))
    .bind(body.sort_order.unwrap_or(0))
    .bind(&body.payment_provider)
    .execute(&state.db)
    .await?;

    let plan = sqlx::query_as::<_, Plan>(
        r#"SELECT id, name, slug, description, price_monthly, price_yearly,
                  features, is_active, sort_order, payment_provider, created_at, updated_at
           FROM plans WHERE id = $1"#
    )
    .bind(id)
    .fetch_one(&state.db)
    .await?;

    // Notify FunnelSwift affiliate system
    let _ = sync_to_funnelswift_affiliates(
        "create",
        &body.name,
        body.price_monthly.unwrap_or(0.0),
        &id.to_string(),
        true,
    ).await;
    // Fire-and-forget to FunnelSwift with configured URL + API key
    let plan_name2 = body.name.clone();
    let plan_price2 = body.price_monthly.unwrap_or(0.0);
    let config2 = state.config.clone();
    tokio::spawn(async move {
        sync_plan_to_affiliate(&config2, "create", &plan_name2, plan_price2, true).await;
    });

    Ok(Json(json!({ "plan": plan })))
}

/// POST /api/v1/admin/plans/assign
pub async fn admin_assign_plan(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Json(body): Json<AssignPlanInput>,
) -> Result<Json<Value>, AppError> {
    let plan_id = Uuid::parse_str(&body.plan_id)
        .map_err(|_| AppError::BadRequest("Invalid plan_id".to_string()))?;
    let account_id = Uuid::parse_str(&body.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account_id".to_string()))?;

    // Verify plan exists
    let _plan = sqlx::query_scalar::<_, Uuid>("SELECT id FROM plans WHERE id = $1")
        .bind(plan_id)
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Plan not found".to_string()))?;

    // Verify account exists
    let _account = sqlx::query_scalar::<_, Uuid>("SELECT id FROM accounts WHERE id = $1")
        .bind(account_id)
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Account not found".to_string()))?;

    // Find the plan by slug
    
    let plan_info = sqlx::query("SELECT slug FROM plans WHERE id = $1")
        .bind(plan_id)
        .fetch_one(&state.db)
        .await?;
    let plan_slug: String = plan_info.get("slug");

    // Find plan by slug
    let tier_id = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM plans WHERE slug = $1"
    )
    .bind(&plan_slug)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound(format!(
        "No plan found matching plan slug '{}' — create the plan first", plan_slug
    )))?;

    sqlx::query("UPDATE accounts SET plan_tier_id = $1 WHERE id = $2")
        .bind(tier_id)
        .bind(account_id)
        .execute(&state.db)
        .await?;

    Ok(Json(json!({
        "status": "assigned",
        "plan_id": plan_id.to_string(),
        "account_id": account_id.to_string(),
        "plan_tier_id": tier_id.to_string(),
    })))
}

/// GET /api/v1/admin/plans/{id}
pub async fn get_plan(
    State(state): State<AppState>,
    Path(id): Path<String>,
    _user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let plan_id = Uuid::parse_str(&id)
        .map_err(|_| AppError::BadRequest("Invalid plan ID".to_string()))?;

    let plan = sqlx::query_as::<_, Plan>(
        r#"SELECT id, name, slug, description, price_monthly, price_yearly,
                  features, is_active, sort_order, payment_provider, created_at, updated_at
           FROM plans WHERE id = $1"#
    )
    .bind(plan_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Plan not found".to_string()))?;

    Ok(Json(json!({ "plan": plan })))
}

/// PUT /api/v1/admin/plans/{id}
pub async fn update_plan(
    State(state): State<AppState>,
    Path(id): Path<String>,
    _user: AuthenticatedUser,
    Json(body): Json<UpdatePlanInput>,
) -> Result<Json<Value>, AppError> {
    let plan_id = Uuid::parse_str(&id)
        .map_err(|_| AppError::BadRequest("Invalid plan ID".to_string()))?;

    // Get existing plan
    let existing = sqlx::query(
        r#"SELECT name, slug, description, price_monthly, price_yearly,
                  features, is_active, sort_order, payment_provider
           FROM plans WHERE id = $1"#
    )
    .bind(plan_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Plan not found".to_string()))?;

    let name: String = body.name.unwrap_or_else(|| existing.get("name"));
    let slug = body.slug.unwrap_or_else(|| generate_slug(&name));
    let description: Option<String> = body.description.or_else(|| existing.get("description"));
    let price_monthly: f64 = body.price_monthly.unwrap_or_else(|| existing.get("price_monthly"));
    let price_yearly: f64 = body.price_yearly.unwrap_or_else(|| existing.get("price_yearly"));
    let features: Value = body.features.unwrap_or_else(|| existing.get("features"));
    let is_active: bool = body.is_active.unwrap_or_else(|| existing.get("is_active"));
    let sort_order: i32 = body.sort_order.unwrap_or_else(|| existing.get("sort_order"));
    let payment_provider: Option<String> = existing.get("payment_provider");
    let payment_provider: Option<String> = body.payment_provider.or(payment_provider);

    sqlx::query(
        r#"UPDATE plans SET
               name = $1, slug = $2, description = $3,
               price_monthly = $4, price_yearly = $5,
               features = $6, is_active = $7, sort_order = $8,
               payment_provider = $9, updated_at = now()
           WHERE id = $10"#
    )
    .bind(&name)
    .bind(&slug)
    .bind(&description)
    .bind(price_monthly)
    .bind(price_yearly)
    .bind(&features)
    .bind(is_active)
    .bind(sort_order)
    .bind(&payment_provider)
    .bind(plan_id)
    .execute(&state.db)
    .await?;

    let plan = sqlx::query_as::<_, Plan>(
        r#"SELECT id, name, slug, description, price_monthly, price_yearly,
                  features, is_active, sort_order, payment_provider, created_at, updated_at
           FROM plans WHERE id = $1"#
    )
    .bind(plan_id)
    .fetch_one(&state.db)
    .await?;

    // Notify FunnelSwift affiliate system about the update
    let _ = sync_to_funnelswift_affiliates(
        "update",
        &name,
        price_monthly,
        &plan_id.to_string(),
        is_active,
    ).await;
    // Fire-and-forget to FunnelSwift with configured URL + API key
    let plan_name2 = name.clone();
    let config2 = state.config.clone();
    tokio::spawn(async move {
        sync_plan_to_affiliate(&config2, "update", &plan_name2, price_monthly, is_active).await;
    });

    Ok(Json(json!({ "plan": plan })))
}

/// DELETE /api/v1/admin/plans/{id} — soft delete via is_active = false
pub async fn delete_plan(
    State(state): State<AppState>,
    Path(id): Path<String>,
    _user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let plan_id = Uuid::parse_str(&id)
        .map_err(|_| AppError::BadRequest("Invalid plan ID".to_string()))?;

    let result = sqlx::query(
        "UPDATE plans SET is_active = false, updated_at = now() WHERE id = $1 AND is_active = true"
    )
    .bind(plan_id)
    .execute(&state.db)
    .await?;

    // Get plan name before soft-delete for affiliate sync
    let plan_name: Option<String> = sqlx::query_scalar(
        "SELECT name FROM plans WHERE id = $1"
    )
    .bind(plan_id)
    .fetch_optional(&state.db)
    .await?
    .flatten();

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound("Plan not found or already inactive".to_string()));
    }

    // Notify FunnelSwift affiliate system about the deletion
    if let Some(name) = plan_name {
        let _ = sync_to_funnelswift_affiliates(
            "deactivate",
            &name,
            0.0,
            &plan_id.to_string(),
            false,
        ).await;
        // Fire-and-forget to FunnelSwift with configured URL + API key
        let plan_name2 = name.clone();
        let config2 = state.config.clone();
        tokio::spawn(async move {
            sync_plan_to_affiliate(&config2, "deactivate", &plan_name2, 0.0, false).await;
        });
    }

    Ok(Json(json!({ "status": "deleted", "id": id })))
}

/// PUT /api/v1/admin/plans/{id}/features
pub async fn admin_update_plan_features(
    State(state): State<AppState>,
    Path(id): Path<String>,
    _user: AuthenticatedUser,
    Json(body): Json<UpdateFeaturesInput>,
) -> Result<Json<Value>, AppError> {
    let plan_id = Uuid::parse_str(&id)
        .map_err(|_| AppError::BadRequest("Invalid plan ID".to_string()))?;

    sqlx::query(
        "UPDATE plans SET features = $1, updated_at = now() WHERE id = $2"
    )
    .bind(&body.features)
    .bind(plan_id)
    .execute(&state.db)
    .await?;

    let plan = sqlx::query_as::<_, Plan>(
        r#"SELECT id, name, slug, description, price_monthly, price_yearly,
                  features, is_active, sort_order, payment_provider, created_at, updated_at
           FROM plans WHERE id = $1"#
    )
    .bind(plan_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Plan not found".to_string()))?;

    Ok(Json(json!({ "plan": plan })))
}
