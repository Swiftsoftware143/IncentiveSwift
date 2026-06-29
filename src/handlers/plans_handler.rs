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
                  features, is_active, sort_order, created_at, updated_at
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
        r#"INSERT INTO plans (id, name, slug, description, price_monthly, price_yearly, features, is_active, sort_order)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)"#
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
    .execute(&state.db)
    .await?;

    let plan = sqlx::query_as::<_, Plan>(
        r#"SELECT id, name, slug, description, price_monthly, price_yearly,
                  features, is_active, sort_order, created_at, updated_at
           FROM plans WHERE id = $1"#
    )
    .bind(id)
    .fetch_one(&state.db)
    .await?;

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

    // The plans table doesn't have a FK to accounts, so we need to map plan->plan_tier
    // Use the plan's slug to find the matching plan_tier
    let plan_info = sqlx::query("SELECT slug FROM plans WHERE id = $1")
        .bind(plan_id)
        .fetch_one(&state.db)
        .await?;
    let plan_slug: String = plan_info.get("slug");

    // Find plan_tier by slug
    let tier_id = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM plan_tiers WHERE slug = $1"
    )
    .bind(&plan_slug)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound(format!(
        "No plan_tier found matching plan slug '{}' — create the plan_tier first", plan_slug
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
                  features, is_active, sort_order, created_at, updated_at
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
                  features, is_active, sort_order
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

    sqlx::query(
        r#"UPDATE plans SET
               name = $1, slug = $2, description = $3,
               price_monthly = $4, price_yearly = $5,
               features = $6, is_active = $7, sort_order = $8,
               updated_at = now()
           WHERE id = $9"#
    )
    .bind(&name)
    .bind(&slug)
    .bind(&description)
    .bind(price_monthly)
    .bind(price_yearly)
    .bind(&features)
    .bind(is_active)
    .bind(sort_order)
    .bind(plan_id)
    .execute(&state.db)
    .await?;

    let plan = sqlx::query_as::<_, Plan>(
        r#"SELECT id, name, slug, description, price_monthly, price_yearly,
                  features, is_active, sort_order, created_at, updated_at
           FROM plans WHERE id = $1"#
    )
    .bind(plan_id)
    .fetch_one(&state.db)
    .await?;

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

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound("Plan not found or already inactive".to_string()));
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
                  features, is_active, sort_order, created_at, updated_at
           FROM plans WHERE id = $1"#
    )
    .bind(plan_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Plan not found".to_string()))?;

    Ok(Json(json!({ "plan": plan })))
}
