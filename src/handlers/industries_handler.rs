//! Industries handler — CRUD for industry registry + public list.
//!
//! Industry = Dashboard = Template Category.
//! Admins create industries; they auto-sync with template categories by slug.
//! Users get X industry dashboards based on their plan's `industry_limit`.
//!
//! Endpoints:
//!   GET  /api/v1/industries              — list active industries (authenticated)
//!   POST /api/v1/admin/industries        — create industry (admin)
//!   PUT  /api/v1/admin/industries/:id    — update industry (admin)
//!   DELETE /api/v1/admin/industries/:id  — soft-delete industry (admin)
//!   GET  /api/v1/admin/industries        — list all industries incl. inactive (admin)

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

/// A single industry record.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Industry {
    pub id: Uuid,
    pub name: String,
    pub slug: String,
    pub description: Option<String>,
    pub icon: Option<String>,
    pub is_active: bool,
    pub sort_order: i32,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// Input for creating an industry.
#[derive(Deserialize)]
pub struct CreateIndustryInput {
    pub name: String,
    pub slug: Option<String>,
    pub description: Option<String>,
    pub icon: Option<String>,
    pub sort_order: Option<i32>,
}

/// Input for updating an industry.
#[derive(Deserialize)]
pub struct UpdateIndustryInput {
    pub name: Option<String>,
    pub slug: Option<String>,
    pub description: Option<String>,
    pub icon: Option<String>,
    pub is_active: Option<bool>,
    pub sort_order: Option<i32>,
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

/// GET /api/v1/industries — list active industries (user-facing)
pub async fn list_active_industries(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let industries = sqlx::query_as::<_, Industry>(
        r#"SELECT id, name, slug, description, icon, is_active, sort_order, created_at, updated_at
           FROM industries
           WHERE is_active = true
           ORDER BY sort_order, name"#
    )
    .fetch_all(&state.db)
    .await?;

    Ok(Json(json!({ "industries": industries })))
}

/// GET /api/v1/admin/industries — list all industries (admin)
pub async fn admin_list_industries(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let industries = sqlx::query_as::<_, Industry>(
        r#"SELECT id, name, slug, description, icon, is_active, sort_order, created_at, updated_at
           FROM industries
           ORDER BY sort_order, name"#
    )
    .fetch_all(&state.db)
    .await?;

    Ok(Json(json!({ "industries": industries })))
}

/// POST /api/v1/admin/industries
pub async fn admin_create_industry(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Json(body): Json<CreateIndustryInput>,
) -> Result<Json<Value>, AppError> {
    let slug = body.slug.unwrap_or_else(|| generate_slug(&body.name));
    let id = Uuid::new_v4();

    sqlx::query(
        r#"INSERT INTO industries (id, name, slug, description, icon, sort_order)
           VALUES ($1, $2, $3, $4, $5, $6)"#
    )
    .bind(id)
    .bind(&body.name)
    .bind(&slug)
    .bind(&body.description)
    .bind(&body.icon)
    .bind(body.sort_order.unwrap_or(0))
    .execute(&state.db)
    .await?;

    let industry = sqlx::query_as::<_, Industry>(
        r#"SELECT id, name, slug, description, icon, is_active, sort_order, created_at, updated_at
           FROM industries WHERE id = $1"#
    )
    .bind(id)
    .fetch_one(&state.db)
    .await?;

    Ok(Json(json!({ "industry": industry })))
}

/// PUT /api/v1/admin/industries/:id
pub async fn admin_update_industry(
    State(state): State<AppState>,
    Path(id): Path<String>,
    _user: AuthenticatedUser,
    Json(body): Json<UpdateIndustryInput>,
) -> Result<Json<Value>, AppError> {
    let industry_id = Uuid::parse_str(&id)
        .map_err(|_| AppError::BadRequest("Invalid industry ID".to_string()))?;

    // Get existing
    let existing = sqlx::query(
        r#"SELECT name, slug, description, icon, is_active, sort_order
           FROM industries WHERE id = $1"#
    )
    .bind(industry_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Industry not found".to_string()))?;

    let name: String = body.name.unwrap_or_else(|| existing.get("name"));
    let slug: String = body.slug.unwrap_or_else(|| generate_slug(&name));
    let description: Option<String> = body.description.or_else(|| existing.get("description"));
    let icon: Option<String> = body.icon.or_else(|| existing.get("icon"));
    let is_active: bool = body.is_active.unwrap_or_else(|| existing.get("is_active"));
    let sort_order: i32 = body.sort_order.unwrap_or_else(|| existing.get("sort_order"));

    sqlx::query(
        r#"UPDATE industries SET
               name = $1, slug = $2, description = $3, icon = $4,
               is_active = $5, sort_order = $6, updated_at = now()
           WHERE id = $7"#
    )
    .bind(&name)
    .bind(&slug)
    .bind(&description)
    .bind(&icon)
    .bind(is_active)
    .bind(sort_order)
    .bind(industry_id)
    .execute(&state.db)
    .await?;

    let industry = sqlx::query_as::<_, Industry>(
        r#"SELECT id, name, slug, description, icon, is_active, sort_order, created_at, updated_at
           FROM industries WHERE id = $1"#
    )
    .bind(industry_id)
    .fetch_one(&state.db)
    .await?;

    Ok(Json(json!({ "industry": industry })))
}

/// DELETE /api/v1/admin/industries/:id — soft delete
pub async fn admin_delete_industry(
    State(state): State<AppState>,
    Path(id): Path<String>,
    _user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let industry_id = Uuid::parse_str(&id)
        .map_err(|_| AppError::BadRequest("Invalid industry ID".to_string()))?;

    let result = sqlx::query(
        "UPDATE industries SET is_active = false, updated_at = now() WHERE id = $1 AND is_active = true"
    )
    .bind(industry_id)
    .execute(&state.db)
    .await?;

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound("Industry not found or already inactive".to_string()));
    }

    Ok(Json(json!({ "status": "deleted", "id": id })))
}
