//! Plan tier feature management — `tier_features` CRUD + `plan_tiers` editing.
//!
//! Canonical feature model: `plan_tiers` (the tier) -> `tier_features`
//! (tier_id, feature_id, enabled, limit_value) -> `features` (key, label,
//! category, description). The runtime gate (`access::feature_gate`) reads
//! exactly this. The legacy `plans.features` JSONB column is not the source
//! of truth and is not touched by these endpoints.

use crate::error::AppError;
use crate::security::auth::AuthenticatedUser;
use crate::state::AppState;
use axum::{
    extract::{Path, State},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

/// A plan tier as listed for the admin UI.
#[derive(Debug, Serialize)]
pub struct TierListItem {
    pub id: Uuid,
    pub slug: String,
    pub name: String,
    pub price_monthly: f64,
    pub price_annual: f64,
    pub is_active: bool,
    pub sort_order: i32,
    pub max_campaigns: Option<i32>,
    pub max_entries_per_month: Option<i32>,
}

/// A feature joined with its per-tier state.
#[derive(Debug, Serialize)]
pub struct TierFeatureItem {
    pub feature_key: String,
    pub label: String,
    pub category: Option<String>,
    pub description: Option<String>,
    pub enabled: bool,
    pub limit_value: Option<i32>,
}

/// Body for PUT /admin/tiers/:tier_id/features/:feature_key.
#[derive(Debug, Deserialize)]
pub struct TierFeatureInput {
    pub enabled: bool,
    #[serde(default)]
    pub limit_value: Option<i32>,
}

/// A single feature upsert within the batch POST body.
#[derive(Debug, Deserialize)]
pub struct FeatureUpsert {
    pub feature_key: String,
    pub enabled: bool,
    #[serde(default)]
    pub limit_value: Option<i32>,
}

/// Body for POST /admin/plans/:id/features (batch upsert keyed by tier).
#[derive(Debug, Deserialize)]
pub struct PlanFeaturesInput {
    pub features: Vec<FeatureUpsert>,
}

/// Body for PUT /admin/tiers/:tier_id (edit plan_tiers fields).
#[derive(Debug, Deserialize)]
pub struct UpdateTierInput {
    pub name: Option<String>,
    pub slug: Option<String>,
    pub price_monthly: Option<f64>,
    pub price_annual: Option<f64>,
    pub is_active: Option<bool>,
    pub sort_order: Option<i32>,
    pub max_campaigns: Option<i32>,
    pub max_entries_per_month: Option<i32>,
}

/// Resolve a tier id string to a `Uuid` (400 on malformed).
fn parse_tier_id(id: &str) -> Result<Uuid, AppError> {
    Uuid::parse_str(id).map_err(|_| AppError::BadRequest("Invalid tier ID".to_string()))
}

/// Ensure a plan tier exists (404 otherwise).
async fn ensure_tier_exists(state: &AppState, tier_id: Uuid) -> Result<(), AppError> {
    let exists: Option<Uuid> = sqlx::query_scalar("SELECT id FROM plan_tiers WHERE id = $1")
        .bind(tier_id)
        .fetch_optional(&state.db)
        .await?
        .flatten();

    if exists.is_some() {
        Ok(())
    } else {
        Err(AppError::NotFound("Plan tier not found".to_string()))
    }
}

/// GET /api/v1/admin/tiers — list plan_tiers.
pub async fn list_tiers(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let rows = sqlx::query(
        r#"SELECT id, slug, name,
                  COALESCE(price_monthly::float8, 0.0) AS price_monthly,
                  COALESCE(price_annual::float8, 0.0) AS price_annual,
                  is_active, sort_order, max_campaigns, max_entries_per_month
           FROM plan_tiers
           ORDER BY sort_order, name"#,
    )
    .fetch_all(&state.db)
    .await?;

    let tiers: Vec<TierListItem> = rows
        .iter()
        .map(|r| TierListItem {
            id: r.get("id"),
            slug: r.get("slug"),
            name: r.get("name"),
            price_monthly: r.get("price_monthly"),
            price_annual: r.get("price_annual"),
            is_active: r.get("is_active"),
            sort_order: r.get("sort_order"),
            max_campaigns: r.get("max_campaigns"),
            max_entries_per_month: r.get("max_entries_per_month"),
        })
        .collect();

    Ok(Json(json!({ "tiers": tiers })))
}

/// GET /api/v1/admin/tiers/:tier_id/features — list ALL registry features
/// joined with this tier's tier_features state (missing row -> enabled=false).
pub async fn get_tier_features(
    State(state): State<AppState>,
    Path(tier_id): Path<String>,
    _user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let tier_id = parse_tier_id(&tier_id)?;
    ensure_tier_exists(&state, tier_id).await?;

    let rows = sqlx::query(
        r#"SELECT f.key AS feature_key, f.label, f.category, f.description,
                  COALESCE(tf.enabled, false) AS enabled, tf.limit_value
           FROM features f
           LEFT JOIN tier_features tf
             ON tf.feature_id = f.id AND tf.tier_id = $1
           ORDER BY f.category, f.key"#,
    )
    .bind(tier_id)
    .fetch_all(&state.db)
    .await?;

    let features: Vec<TierFeatureItem> = rows
        .iter()
        .map(|r| TierFeatureItem {
            feature_key: r.get("feature_key"),
            label: r.get("label"),
            category: r.get("category"),
            description: r.get("description"),
            enabled: r.get("enabled"),
            limit_value: r.get("limit_value"),
        })
        .collect();

    Ok(Json(json!({
        "tier_id": tier_id,
        "features": features,
    })))
}

/// PUT /api/v1/admin/tiers/:tier_id/features/:feature_key — UPSERT into
/// tier_features. ADD = enabled:true, SUBTRACT = enabled:false. The runtime
/// gate reflects this immediately.
pub async fn update_tier_feature(
    State(state): State<AppState>,
    Path((tier_id, feature_key)): Path<(String, String)>,
    _user: AuthenticatedUser,
    Json(body): Json<TierFeatureInput>,
) -> Result<Json<Value>, AppError> {
    let tier_id = parse_tier_id(&tier_id)?;
    ensure_tier_exists(&state, tier_id).await?;

    let feature_id: Option<Uuid> = sqlx::query_scalar("SELECT id FROM features WHERE key = $1")
        .bind(&feature_key)
        .fetch_optional(&state.db)
        .await?
        .flatten();

    let feature_id = feature_id
        .ok_or_else(|| AppError::NotFound(format!("Feature '{}' not found", feature_key)))?;

    sqlx::query(
        r#"INSERT INTO tier_features (tier_id, feature_id, enabled, limit_value)
           VALUES ($1, $2, $3, $4)
           ON CONFLICT (tier_id, feature_id)
           DO UPDATE SET enabled = EXCLUDED.enabled, limit_value = EXCLUDED.limit_value"#,
    )
    .bind(tier_id)
    .bind(feature_id)
    .bind(body.enabled)
    .bind(body.limit_value)
    .execute(&state.db)
    .await?;

    Ok(Json(json!({
        "status": "ok",
        "tier_id": tier_id,
        "feature_key": feature_key,
        "enabled": body.enabled,
        "limit_value": body.limit_value,
    })))
}

/// PUT /api/v1/admin/tiers/:tier_id — edit plan_tiers fields.
pub async fn update_tier(
    State(state): State<AppState>,
    Path(tier_id): Path<String>,
    _user: AuthenticatedUser,
    Json(body): Json<UpdateTierInput>,
) -> Result<Json<Value>, AppError> {
    let tier_id = parse_tier_id(&tier_id)?;

    let existing = sqlx::query(
        r#"SELECT name, slug, price_monthly::float8, price_annual::float8,
                  is_active, sort_order, max_campaigns, max_entries_per_month
           FROM plan_tiers WHERE id = $1"#,
    )
    .bind(tier_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Plan tier not found".to_string()))?;

    let name: String = body.name.unwrap_or_else(|| existing.get("name"));
    let slug: String = body.slug.unwrap_or_else(|| existing.get("slug"));
    let price_monthly: Option<f64> = body.price_monthly.or_else(|| existing.get("price_monthly"));
    let price_annual: Option<f64> = body.price_annual.or_else(|| existing.get("price_annual"));
    let is_active: bool = body.is_active.unwrap_or_else(|| existing.get("is_active"));
    let sort_order: i32 = body
        .sort_order
        .unwrap_or_else(|| existing.get("sort_order"));
    let max_campaigns: Option<i32> = body.max_campaigns.or_else(|| existing.get("max_campaigns"));
    let max_entries_per_month: Option<i32> = body
        .max_entries_per_month
        .or_else(|| existing.get("max_entries_per_month"));

    sqlx::query(
        r#"UPDATE plan_tiers SET
               name = $1, slug = $2, price_monthly = $3, price_annual = $4,
               is_active = $5, sort_order = $6,
               max_campaigns = $7, max_entries_per_month = $8
           WHERE id = $9"#,
    )
    .bind(&name)
    .bind(&slug)
    .bind(price_monthly)
    .bind(price_annual)
    .bind(is_active)
    .bind(sort_order)
    .bind(max_campaigns)
    .bind(max_entries_per_month)
    .bind(tier_id)
    .execute(&state.db)
    .await?;

    Ok(Json(json!({
        "status": "ok",
        "tier_id": tier_id,
        "tier": {
            "id": tier_id,
            "name": name,
            "slug": slug,
            "price_monthly": price_monthly,
            "price_annual": price_annual,
            "is_active": is_active,
            "sort_order": sort_order,
            "max_campaigns": max_campaigns,
            "max_entries_per_month": max_entries_per_month,
        },
    })))
}

/// POST /api/v1/admin/plans/:id/features — batch upsert into `tier_features`,
/// keyed by the tier that matches the legacy plan by slug. This is the
/// canonical write path (NOT the `plans.features` JSONB column).
pub async fn post_plan_features(
    State(state): State<AppState>,
    Path(plan_id): Path<String>,
    _user: AuthenticatedUser,
    Json(body): Json<PlanFeaturesInput>,
) -> Result<Json<Value>, AppError> {
    let plan_id = Uuid::parse_str(&plan_id)
        .map_err(|_| AppError::BadRequest("Invalid plan ID".to_string()))?;

    // Resolve the legacy plans row to its canonical plan_tiers row via slug.
    let tier_id: Option<Uuid> = sqlx::query_scalar(
        r#"SELECT pt.id
           FROM plan_tiers pt
           JOIN plans p ON p.slug = pt.slug
           WHERE p.id = $1"#,
    )
    .bind(plan_id)
    .fetch_optional(&state.db)
    .await?
    .flatten();

    let tier_id = tier_id.ok_or_else(|| {
        AppError::NotFound("No plan tier matches this plan (no shared slug)".to_string())
    })?;

    let mut updated: Vec<String> = Vec::new();
    for item in &body.features {
        let feature_id: Option<Uuid> = sqlx::query_scalar("SELECT id FROM features WHERE key = $1")
            .bind(&item.feature_key)
            .fetch_optional(&state.db)
            .await?
            .flatten();

        let Some(feature_id) = feature_id else {
            continue; // skip unknown feature keys rather than failing the batch
        };

        sqlx::query(
            r#"INSERT INTO tier_features (tier_id, feature_id, enabled, limit_value)
               VALUES ($1, $2, $3, $4)
               ON CONFLICT (tier_id, feature_id)
               DO UPDATE SET enabled = EXCLUDED.enabled, limit_value = EXCLUDED.limit_value"#,
        )
        .bind(tier_id)
        .bind(feature_id)
        .bind(item.enabled)
        .bind(item.limit_value)
        .execute(&state.db)
        .await?;

        updated.push(item.feature_key.clone());
    }

    Ok(Json(json!({
        "status": "ok",
        "tier_id": tier_id,
        "updated": updated,
    })))
}
