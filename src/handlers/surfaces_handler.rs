//! Surfaces handler — REST endpoints for surfaces.
//! Auto-generated during endpoint restoration.

use crate::error::AppError;
use crate::security::auth::AuthenticatedUser;
use crate::state::AppState;
use axum::{
    extract::{Path, Query, State},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Surfaces {
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

/// The caller's own account id, from the verified session.
fn account_uuid(user: &AuthenticatedUser) -> Result<Uuid, AppError> {
    Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid user ID value".to_string()))
}

/// GET /api/v1/surfaces — the CALLER's own rows only.
///
/// SECURITY (kanban t_373f10c0). This statement used to have NO `WHERE` clause at all:
/// `SELECT … FROM surfaces ORDER BY name LIMIT $1 OFFSET $2`. Measured live 2026-10-02 on the
/// pre-fix binary, a throwaway account that owned one surface received every account's rows
/// (id + account_id + name), and a fresh account that owned none received all of them. The route
/// is an Operator-Console `ops`-panel arm and its three siblings below matched on `id` alone, so
/// the same caller could also read, rename and DELETE another account's surface.
///
/// Scoped UNCONDITIONALLY to `account_id` — no `role == "admin"` bypass. Decided by measurement,
/// not by taste: this app has NO operator view of `surfaces` (`grep -n "surfaces" src/main.rs` ->
/// these two routes only; there is no `/api/v1/admin/surfaces`), the `ops` panel is a generic API
/// exerciser rather than a catalogue (`{"id": "surfaces-kiosk", "list": "/surfaces"}`), and the
/// sibling ops arm the card names — `/api/v1/provider-keys` — is likewise `WHERE account_id = $1`
/// with no role branch. This mirrors `integration_target_handler::list_integration_targets`
/// (t_305a0549), which made the same call for the same reason; a cross-account listing, if ever
/// wanted, belongs on an `admin_guard`-protected `/api/v1/admin/*` route, not here.
///
/// The query error is no longer swallowed: `unwrap_or_else(..Default::default())` turned any SQL
/// failure into `200 {"items":[]}`, which is exactly how a scoping mistake stays invisible.
pub async fn list(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Query(query): Query<ListQuery>,
) -> Result<Json<Value>, AppError> {
    let account_id = account_uuid(&user)?;
    let limit = query.limit.unwrap_or(50).min(100);
    let offset = query.offset.unwrap_or(0);
    let items = sqlx::query_as::<_, Surfaces>(
        "SELECT id, account_id, name, created_at, updated_at FROM surfaces WHERE account_id = $1 ORDER BY name LIMIT $2 OFFSET $3",
    )
    .bind(account_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(
        json!({ "items": items, "count": items.len(), "limit": limit, "offset": offset }),
    ))
}

/// POST /api/v1/surfaces
pub async fn create(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<CreateInput>,
) -> Result<Json<Value>, AppError> {
    let id = Uuid::new_v4();
    let account_id = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid user ID value".to_string()))?;
    sqlx::query("INSERT INTO surfaces (id, account_id, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(account_id)
        .bind(&body.name)
        .execute(&state.db)
        .await?;
    let item = sqlx::query_as::<_, Surfaces>(
        "SELECT id, account_id, name, created_at, updated_at FROM surfaces WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&state.db)
    .await?;
    Ok(Json(json!({ "item": item })))
}

/// GET /api/v1/surfaces/{id} — the caller's OWN surface, or 404.
///
/// SECURITY (kanban t_373f10c0): used to match on the id alone.
pub async fn get(
    State(state): State<AppState>,
    Path(id): Path<String>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let account_id = account_uuid(&user)?;
    let item_id = Uuid::parse_str(&id)
        .map_err(|_| AppError::BadRequest("Invalid item ID value".to_string()))?;
    let item = sqlx::query_as::<_, Surfaces>(
        "SELECT id, account_id, name, created_at, updated_at FROM surfaces WHERE id = $1 AND account_id = $2",
    )
    .bind(item_id)
    .bind(account_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound(format!("Item not found for id: {}", id)))?;
    Ok(Json(json!({ "item": item })))
}

/// PUT /api/v1/surfaces/{id} — the caller's OWN surface, or 404.
///
/// SECURITY (kanban t_373f10c0): every statement below used to match on the row id alone, so any
/// authenticated account could rename another account's surface. The ownership predicate is on the
/// *read* as well as the *write*: this handler is read-then-write, so scoping only the UPDATE would
/// 404 on the id and then write the body back anyway. 404 (not 403) is this app's convention — a
/// 403 would confirm the id exists somewhere (same rule as `integration_target_handler`, t_305a0549).
pub async fn update(
    State(state): State<AppState>,
    Path(id): Path<String>,
    user: AuthenticatedUser,
    Json(body): Json<UpdateInput>,
) -> Result<Json<Value>, AppError> {
    let account_id = account_uuid(&user)?;
    let item_id =
        Uuid::parse_str(&id).map_err(|_| AppError::BadRequest("Invalid item ID".to_string()))?;
    let row = sqlx::query("SELECT name FROM surfaces WHERE id = $1 AND account_id = $2")
        .bind(item_id)
        .bind(account_id)
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Item not found: {}", id)))?;
    let new_name = body.name.unwrap_or_else(|| row.get("name"));
    let updated = sqlx::query(
        "UPDATE surfaces SET name = $1, updated_at = now() WHERE id = $2 AND account_id = $3",
    )
    .bind(&new_name)
    .bind(item_id)
    .bind(account_id)
    .execute(&state.db)
    .await?;
    if updated.rows_affected() == 0 {
        return Err(AppError::NotFound(format!("Item not found: {}", id)));
    }
    let item = sqlx::query_as::<_, Surfaces>(
        "SELECT id, account_id, name, created_at, updated_at FROM surfaces WHERE id = $1 AND account_id = $2",
    )
    .bind(item_id)
    .bind(account_id)
    .fetch_one(&state.db)
    .await?;
    Ok(Json(json!({ "item": item })))
}

/// DELETE /api/v1/surfaces/{id} — the caller's OWN surface, or 404.
///
/// SECURITY (kanban t_373f10c0): this used to be `DELETE … WHERE id = $1`, i.e. any account could
/// delete any other account's surface. Measured live before the fix.
pub async fn delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let account_id = account_uuid(&user)?;
    let item_id =
        Uuid::parse_str(&id).map_err(|_| AppError::BadRequest("Invalid item ID".to_string()))?;
    let result = sqlx::query("DELETE FROM surfaces WHERE id = $1 AND account_id = $2")
        .bind(item_id)
        .bind(account_id)
        .execute(&state.db)
        .await?;
    if result.rows_affected() == 0 {
        return Err(AppError::NotFound(format!("Item not found: {}", id)));
    }
    Ok(Json(json!({ "status": "deleted" })))
}
