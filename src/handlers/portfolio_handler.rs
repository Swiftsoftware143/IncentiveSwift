//! Portfolio company handlers — CRUD for portfolio_companies table.

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

/// A portfolio company record.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct PortfolioCompany {
    pub id: Uuid,
    pub account_id: Uuid,
    pub name: String,
    pub slug: String,
    pub settings: Value,
    pub email: Option<String>,
    pub description: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// Input for creating a portfolio company.
#[derive(Deserialize)]
pub struct CreatePortfolioCompanyInput {
    pub name: String,
    pub slug: Option<String>,
    pub settings: Option<Value>,
    pub email: Option<String>,
    pub description: Option<String>,
}

/// Input for updating a portfolio company.
#[derive(Deserialize)]
pub struct UpdatePortfolioCompanyInput {
    pub name: Option<String>,
    pub slug: Option<String>,
    pub settings: Option<Value>,
    pub email: Option<String>,
    pub description: Option<String>,
}

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

/// GET /api/v1/portfolio-companies
pub async fn list_portfolio_companies(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let companies = sqlx::query_as::<_, PortfolioCompany>(
        r#"SELECT id, account_id, name, slug, settings, email, description, created_at, updated_at
           FROM portfolio_companies
           ORDER BY name"#
    )
    .fetch_all(&state.db)
    .await?;

    Ok(Json(json!({ "companies": companies })))
}

/// POST /api/v1/portfolio-companies
pub async fn create_portfolio_company(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Json(body): Json<CreatePortfolioCompanyInput>,
) -> Result<Json<Value>, AppError> {
    let id = Uuid::new_v4();
    let slug = body.slug.unwrap_or_else(|| generate_slug(&body.name));
    let settings = body.settings.unwrap_or_else(|| json!({}));

    sqlx::query(
        r#"INSERT INTO portfolio_companies (id, account_id, name, slug, settings, email, description)
           VALUES ($1, $2, $3, $4, $5, $6, $7)"#
    )
    .bind(id)
    .bind(Uuid::parse_str(&_user.account_id).map_err(|_| AppError::BadRequest("Invalid user ID".to_string()))?)
    .bind(&body.name)
    .bind(&slug)
    .bind(&settings)
    .bind(&body.email)
    .bind(&body.description)
    .execute(&state.db)
    .await?;

    let company = sqlx::query_as::<_, PortfolioCompany>(
        r#"SELECT id, account_id, name, slug, settings, email, description, created_at, updated_at
           FROM portfolio_companies WHERE id = $1"#
    )
    .bind(id)
    .fetch_one(&state.db)
    .await?;

    Ok(Json(json!({ "company": company })))
}

/// GET /api/v1/portfolio-companies/{id}
pub async fn get_portfolio_company(
    State(state): State<AppState>,
    Path(id): Path<String>,
    _user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let company_id = Uuid::parse_str(&id)
        .map_err(|_| AppError::BadRequest("Invalid company ID".to_string()))?;

    let company = sqlx::query_as::<_, PortfolioCompany>(
        r#"SELECT id, account_id, name, slug, settings, email, description, created_at, updated_at
           FROM portfolio_companies WHERE id = $1"#
    )
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Portfolio company not found".to_string()))?;

    Ok(Json(json!({ "company": company })))
}

/// PUT /api/v1/portfolio-companies/{id}
pub async fn update_portfolio_company(
    State(state): State<AppState>,
    Path(id): Path<String>,
    _user: AuthenticatedUser,
    Json(body): Json<UpdatePortfolioCompanyInput>,
) -> Result<Json<Value>, AppError> {
    let company_id = Uuid::parse_str(&id)
        .map_err(|_| AppError::BadRequest("Invalid company ID".to_string()))?;

    let existing = sqlx::query(
        r#"SELECT name, slug, settings, email, description
           FROM portfolio_companies WHERE id = $1"#
    )
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Portfolio company not found".to_string()))?;

    let name = body.name.unwrap_or_else(|| existing.get("name"));
    let slug = body.slug.unwrap_or_else(|| existing.get("slug"));
    let settings = body.settings.unwrap_or_else(|| existing.get("settings"));
    let email: Option<String> = body.email.or_else(|| existing.get("email"));
    let description: Option<String> = body.description.or_else(|| existing.get("description"));

    sqlx::query(
        r#"UPDATE portfolio_companies SET
               name = $1, slug = $2, settings = $3, email = $4, description = $5, updated_at = now()
           WHERE id = $6"#
    )
    .bind(&name)
    .bind(&slug)
    .bind(&settings)
    .bind(&email)
    .bind(&description)
    .bind(company_id)
    .execute(&state.db)
    .await?;

    let company = sqlx::query_as::<_, PortfolioCompany>(
        r#"SELECT id, account_id, name, slug, settings, email, description, created_at, updated_at
           FROM portfolio_companies WHERE id = $1"#
    )
    .bind(company_id)
    .fetch_one(&state.db)
    .await?;

    Ok(Json(json!({ "company": company })))
}

/// DELETE /api/v1/portfolio-companies/{id}
pub async fn delete_portfolio_company(
    State(state): State<AppState>,
    Path(id): Path<String>,
    _user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let company_id = Uuid::parse_str(&id)
        .map_err(|_| AppError::BadRequest("Invalid company ID".to_string()))?;

    let result = sqlx::query("DELETE FROM portfolio_companies WHERE id = $1")
        .bind(company_id)
        .execute(&state.db)
        .await?;

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound("Portfolio company not found".to_string()));
    }

    Ok(Json(json!({ "status": "deleted", "id": id })))
}
