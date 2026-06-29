//! Integration target handlers — CRUD for integration_targets table.

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

/// An integration target record.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct IntegrationTarget {
    pub id: Uuid,
    pub account_id: Uuid,
    pub portfolio_company_id: Option<Uuid>,
    pub name: String,
    pub provider: String,
    pub webhook_url: String,
    pub api_key: Option<String>,
    pub events: Vec<String>,
    pub is_active: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// Input for creating an integration target.
#[derive(Deserialize)]
pub struct CreateIntegrationTargetInput {
    pub name: String,
    pub portfolio_company_id: Option<String>,
    pub provider: Option<String>,
    pub webhook_url: String,
    pub api_key: Option<String>,
    pub events: Option<Vec<String>>,
    pub is_active: Option<bool>,
}

/// Input for updating an integration target.
#[derive(Deserialize)]
pub struct UpdateIntegrationTargetInput {
    pub name: Option<String>,
    pub portfolio_company_id: Option<String>,
    pub provider: Option<String>,
    pub webhook_url: Option<String>,
    pub api_key: Option<String>,
    pub events: Option<Vec<String>>,
    pub is_active: Option<bool>,
}

/// GET /api/v1/integration-targets
pub async fn list_integration_targets(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let targets = sqlx::query_as::<_, IntegrationTarget>(
        r#"SELECT id, account_id, portfolio_company_id, name, provider, webhook_url,
                  api_key, events, is_active, created_at, updated_at
           FROM integration_targets
           ORDER BY name"#
    )
    .fetch_all(&state.db)
    .await?;

    Ok(Json(json!({ "targets": targets })))
}

/// POST /api/v1/integration-targets
pub async fn create_integration_target(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Json(body): Json<CreateIntegrationTargetInput>,
) -> Result<Json<Value>, AppError> {
    let id = Uuid::new_v4();
    let account_id = Uuid::parse_str(&_user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid user ID".to_string()))?;
    let portfolio_company_id = body.portfolio_company_id
        .and_then(|s| Uuid::parse_str(&s).ok());
    let provider = body.provider.unwrap_or_else(|| "webhook".to_string());
    let events = body.events.unwrap_or_default();

    sqlx::query(
        r#"INSERT INTO integration_targets
               (id, account_id, portfolio_company_id, name, provider, webhook_url, api_key, events, is_active)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)"#
    )
    .bind(id)
    .bind(account_id)
    .bind(portfolio_company_id)
    .bind(&body.name)
    .bind(&provider)
    .bind(&body.webhook_url)
    .bind(&body.api_key)
    .bind(&events)
    .bind(body.is_active.unwrap_or(true))
    .execute(&state.db)
    .await?;

    let target = sqlx::query_as::<_, IntegrationTarget>(
        r#"SELECT id, account_id, portfolio_company_id, name, provider, webhook_url,
                  api_key, events, is_active, created_at, updated_at
           FROM integration_targets WHERE id = $1"#
    )
    .bind(id)
    .fetch_one(&state.db)
    .await?;

    Ok(Json(json!({ "target": target })))
}

/// PUT /api/v1/integration-targets/{id}
pub async fn update_integration_target(
    State(state): State<AppState>,
    Path(id): Path<String>,
    _user: AuthenticatedUser,
    Json(body): Json<UpdateIntegrationTargetInput>,
) -> Result<Json<Value>, AppError> {
    let target_id = Uuid::parse_str(&id)
        .map_err(|_| AppError::BadRequest("Invalid target ID".to_string()))?;

    let existing = sqlx::query(
        r#"SELECT name, provider, webhook_url, api_key, events, is_active
           FROM integration_targets WHERE id = $1"#
    )
    .bind(target_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Integration target not found".to_string()))?;

    let name = body.name.unwrap_or_else(|| existing.get("name"));
    let provider = body.provider.unwrap_or_else(|| existing.get("provider"));
    let webhook_url = body.webhook_url.unwrap_or_else(|| existing.get("webhook_url"));
    let api_key: Option<String> = body.api_key.or_else(|| existing.get("api_key"));
    let events: Vec<String> = body.events.unwrap_or_else(|| existing.get("events"));
    let is_active = body.is_active.unwrap_or_else(|| existing.get("is_active"));

    let portfolio_company_id: Option<Uuid> = if let Some(ref pcid) = body.portfolio_company_id {
        Some(Uuid::parse_str(pcid).map_err(|_| AppError::BadRequest("Invalid portfolio_company_id".to_string()))?)
    } else {
        sqlx::query_scalar("SELECT portfolio_company_id FROM integration_targets WHERE id = $1")
            .bind(target_id)
            .fetch_optional(&state.db)
            .await?
            .flatten()
    };

    sqlx::query(
        r#"UPDATE integration_targets SET
               name = $1, provider = $2, webhook_url = $3, api_key = $4,
               events = $5, is_active = $6, portfolio_company_id = $7, updated_at = now()
           WHERE id = $8"#
    )
    .bind(&name)
    .bind(&provider)
    .bind(&webhook_url)
    .bind(&api_key)
    .bind(&events)
    .bind(is_active)
    .bind(portfolio_company_id)
    .bind(target_id)
    .execute(&state.db)
    .await?;

    let target = sqlx::query_as::<_, IntegrationTarget>(
        r#"SELECT id, account_id, portfolio_company_id, name, provider, webhook_url,
                  api_key, events, is_active, created_at, updated_at
           FROM integration_targets WHERE id = $1"#
    )
    .bind(target_id)
    .fetch_one(&state.db)
    .await?;

    Ok(Json(json!({ "target": target })))
}

/// DELETE /api/v1/integration-targets/{id}
pub async fn delete_integration_target(
    State(state): State<AppState>,
    Path(id): Path<String>,
    _user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let target_id = Uuid::parse_str(&id)
        .map_err(|_| AppError::BadRequest("Invalid target ID".to_string()))?;

    let result = sqlx::query("DELETE FROM integration_targets WHERE id = $1")
        .bind(target_id)
        .execute(&state.db)
        .await?;

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound("Integration target not found".to_string()));
    }

    Ok(Json(json!({ "status": "deleted", "id": id })))
}
