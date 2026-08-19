//! Calendar events handlers — schedule/CRUD for calendar_events table.
use crate::error::AppError;
use crate::security::auth::AuthenticatedUser;
use crate::state::AppState;
use axum::{
    extract::{Path, Query, State},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct CalendarEvent {
    pub id: Uuid,
    pub tenant_id: Option<Uuid>,
    pub campaign_id: Option<Uuid>,
    pub contact_id: Option<Uuid>,
    pub title: String,
    pub description: Option<String>,
    pub location: Option<String>,
    pub event_type: String,
    pub starts_at: chrono::DateTime<chrono::Utc>,
    pub ends_at: Option<chrono::DateTime<chrono::Utc>>,
    pub all_day: bool,
    pub status: String,
    pub color: Option<String>,
    pub created_by: Option<Uuid>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Deserialize)]
pub struct CreateEventInput {
    pub campaign_id: Option<Uuid>,
    pub contact_id: Option<Uuid>,
    pub title: String,
    pub description: Option<String>,
    pub location: Option<String>,
    pub event_type: Option<String>,
    pub starts_at: chrono::DateTime<chrono::Utc>,
    pub ends_at: Option<chrono::DateTime<chrono::Utc>>,
    pub all_day: Option<bool>,
    pub color: Option<String>,
}

#[derive(Deserialize)]
pub struct UpdateEventInput {
    pub title: Option<String>,
    pub description: Option<String>,
    pub location: Option<String>,
    pub event_type: Option<String>,
    pub starts_at: Option<chrono::DateTime<chrono::Utc>>,
    pub ends_at: Option<chrono::DateTime<chrono::Utc>>,
    pub all_day: Option<bool>,
    pub status: Option<String>,
    pub color: Option<String>,
}

#[derive(Deserialize)]
pub struct RangeQuery {
    pub from: Option<String>,
    pub to: Option<String>,
}

const CE_COLS: &str = "id, tenant_id, campaign_id, contact_id, title, description, location, event_type, starts_at, ends_at, all_day, status, color, created_by, created_at, updated_at";

/// GET /api/v1/calendar-events?from=&to=
pub async fn list_events(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Query(q): Query<RangeQuery>,
) -> Result<Json<Value>, AppError> {
    let account = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;
    let from_dt: Option<chrono::DateTime<chrono::Utc>> = match q.from {
        Some(s) => Some(
            chrono::DateTime::parse_from_rfc3339(&s)
                .map_err(|_| AppError::BadRequest(format!("Invalid `from`: {}", s)))?
                .with_timezone(&chrono::Utc),
        ),
        None => None,
    };
    let to_dt: Option<chrono::DateTime<chrono::Utc>> = match q.to {
        Some(s) => Some(
            chrono::DateTime::parse_from_rfc3339(&s)
                .map_err(|_| AppError::BadRequest(format!("Invalid `to`: {}", s)))?
                .with_timezone(&chrono::Utc),
        ),
        None => None,
    };
    let mut sql = format!(
        "SELECT {CE_COLS} FROM calendar_events WHERE tenant_id = $1"
    );
    let mut next: usize = 2;
    if from_dt.is_some() {
        sql.push_str(&format!(" AND starts_at >= ${}", next));
        next += 1;
    }
    if to_dt.is_some() {
        sql.push_str(&format!(" AND starts_at <= ${}", next));
    }
    sql.push_str(" ORDER BY starts_at ASC");

    let mut qb = sqlx::query_as::<_, CalendarEvent>(&sql).bind(account);
    if let Some(from) = from_dt {
        qb = qb.bind(from);
    }
    if let Some(to) = to_dt {
        qb = qb.bind(to);
    }
    let rows = qb.fetch_all(&state.db).await?;
    Ok(Json(json!({ "events": rows })))
}

/// POST /api/v1/calendar-events
pub async fn create_event(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<CreateEventInput>,
) -> Result<Json<Value>, AppError> {
    let account = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;
    let id = Uuid::new_v4();
    let event_type = body.event_type.unwrap_or_else(|| "event".to_string());
    let all_day = body.all_day.unwrap_or(false);
    sqlx::query(
        r#"INSERT INTO calendar_events
             (id, tenant_id, campaign_id, contact_id, title, description, location, event_type,
              starts_at, ends_at, all_day, color, created_by)
           VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)"#,
    )
    .bind(id)
    .bind(account)
    .bind(body.campaign_id)
    .bind(body.contact_id)
    .bind(&body.title)
    .bind(&body.description)
    .bind(&body.location)
    .bind(&event_type)
    .bind(body.starts_at)
    .bind(body.ends_at)
    .bind(all_day)
    .bind(&body.color)
    .bind(account)
    .execute(&state.db)
    .await?;
    let row = sqlx::query_as::<_, CalendarEvent>(&format!("SELECT {CE_COLS} FROM calendar_events WHERE id = $1"))
        .bind(id)
        .fetch_one(&state.db)
        .await?;
    Ok(Json(json!({ "event": row })))
}

/// PUT /api/v1/calendar-events/:id
pub async fn update_event(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateEventInput>,
) -> Result<Json<Value>, AppError> {
    let account = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;
    let exists = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM calendar_events WHERE id = $1 AND tenant_id = $2",
    )
    .bind(id)
    .bind(account)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Event not found".to_string()))?;
    let _ = exists;
    sqlx::query(
        r#"UPDATE calendar_events SET
             title = COALESCE($2, title),
             description = COALESCE($3, description),
             location = COALESCE($4, location),
             event_type = COALESCE($5, event_type),
             starts_at = COALESCE($6, starts_at),
             ends_at = COALESCE($7, ends_at),
             all_day = COALESCE($8, all_day),
             status = COALESCE($9, status),
             color = COALESCE($10, color),
             updated_at = now()
           WHERE id = $1"#,
    )
    .bind(id)
    .bind(&body.title)
    .bind(&body.description)
    .bind(&body.location)
    .bind(&body.event_type)
    .bind(body.starts_at)
    .bind(body.ends_at)
    .bind(body.all_day)
    .bind(&body.status)
    .bind(&body.color)
    .execute(&state.db)
    .await?;
    let row = sqlx::query_as::<_, CalendarEvent>(&format!("SELECT {CE_COLS} FROM calendar_events WHERE id = $1"))
        .bind(id)
        .fetch_one(&state.db)
        .await?;
    Ok(Json(json!({ "event": row })))
}

/// DELETE /api/v1/calendar-events/:id
pub async fn delete_event(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, AppError> {
    let account = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;
    sqlx::query("DELETE FROM calendar_events WHERE id = $1 AND tenant_id = $2")
        .bind(id)
        .bind(account)
        .execute(&state.db)
        .await?;
    Ok(Json(json!({ "deleted": true })))
}
