//! Calendar Events handler — REST endpoints for calendar_events.
//! Auto-generated during endpoint restoration.

use crate::error::AppError;
use crate::state::AppState;
use crate::security::auth::AuthenticatedUser;
use axum::{
    extract::{Path, Query, State},
    Json,
};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use serde_json::{json, Value};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct CalendarEvents {
    pub id: Uuid,
    pub account_id: Option<Uuid>,
    pub name: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Deserialize)]
pub struct ListQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Deserialize)]
pub struct CreateInput {
    pub name: String,
}

#[derive(Deserialize)]
pub struct UpdateInput {
    pub name: Option<String>,
}

/// GET /api/v1/calendar-events
pub async fn list(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Query(query): Query<ListQuery>,
) -> Result<Json<Value>, AppError> {
    let limit = query.limit.unwrap_or(50).min(100);
    let offset = query.offset.unwrap_or(0);
    let sql = "SELECT id, account_id, name, created_at, updated_at FROM calendar_events ORDER BY name LIMIT $1 OFFSET $2".to_string();
    let items = sqlx::query_as::<_, CalendarEvents>(&sql)
        .bind(limit)
        .bind(offset)
        .fetch_all(&state.db)
        .await
        .unwrap_or_default();
    Ok(Json(json!({ "items": items, "count": items.len(), "limit": limit, "offset": offset })))
}

/// POST /api/v1/calendar-events
pub async fn create(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Json(body): Json<CreateInput>,
) -> Result<Json<Value>, AppError> {
    let id = Uuid::new_v4();
    let account_id = Uuid::parse_str(&_user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid user ID value".to_string()))?;
    sqlx::query("INSERT INTO calendar_events (id, account_id, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(account_id)
        .bind(&body.name)
        .execute(&state.db)
        .await?;
    let item = sqlx::query_as::<_, CalendarEvents>(
        "SELECT id, account_id, name, created_at, updated_at FROM calendar_events WHERE id = $1"
    )
    .bind(id)
    .fetch_one(&state.db)
    .await?;
    Ok(Json(json!({ "item": item })))
}

/// GET /api/v1/calendar-events/{id}
pub async fn get(
    State(state): State<AppState>,
    Path(id): Path<String>,
    _user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let item_id = Uuid::parse_str(&id)
        .map_err(|_| AppError::BadRequest("Invalid item ID value".to_string()))?;
    let item = sqlx::query_as::<_, CalendarEvents>(
        "SELECT id, account_id, name, created_at, updated_at FROM calendar_events WHERE id = $1"
    )
    .bind(item_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound(format!("Item not found for id: {}", id)))?;
    Ok(Json(json!({ "item": item })))
}

/// PUT /api/v1/calendar-events/{id}
pub async fn update(
    State(state): State<AppState>,
    Path(id): Path<String>,
    _user: AuthenticatedUser,
    Json(body): Json<UpdateInput>,
) -> Result<Json<Value>, AppError> {
    let item_id = Uuid::parse_str(&id)
        .map_err(|_| AppError::BadRequest("Invalid item ID".to_string()))?;
    let row = sqlx::query("SELECT name FROM calendar_events WHERE id = $1")
        .bind(item_id)
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Item not found: {}", id)))?;
    let new_name = body.name.unwrap_or_else(|| row.get("name"));
    sqlx::query("UPDATE calendar_events SET name = $1, updated_at = now() WHERE id = $2")
        .bind(&new_name)
        .bind(item_id)
        .execute(&state.db)
        .await?;
    let item = sqlx::query_as::<_, CalendarEvents>(
        "SELECT id, account_id, name, created_at, updated_at FROM calendar_events WHERE id = $1"
    )
    .bind(item_id)
    .fetch_one(&state.db)
    .await?;
    Ok(Json(json!({ "item": item })))
}

/// DELETE /api/v1/calendar-events/{id}
pub async fn delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
    _user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let item_id = Uuid::parse_str(&id)
        .map_err(|_| AppError::BadRequest("Invalid item ID".to_string()))?;
    let result = sqlx::query("DELETE FROM calendar_events WHERE id = $1")
        .bind(item_id)
        .execute(&state.db)
        .await?;
    if result.rows_affected() == 0 {
        return Err(AppError::NotFound(format!("Item not found: {}", id)));
    }
    Ok(Json(json!({ "status": "deleted" })))
}
