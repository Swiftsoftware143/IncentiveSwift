//! Feedback — idea / bug / complaint from any signed-in user (kanban B209, t_2b4e36f9).
//!
//! A "Submit Feedback" button in the console sends a message that is NEVER anonymous: the row
//! records the account (`tenant_id` from the session's `aid`), the person (`user_id` from the
//! session's `sub`), the app (a compile-time constant) and the screen the caller named. The read
//! half is platform-operator only (`is_platform_admin`), so it is a real queue the team reads,
//! not a write-and-drop.

use axum::{extract::State, routing, Extension, Json, Router};
use serde::Deserialize;
use serde_json::json;
use sqlx::Row;
use uuid::Uuid;

use crate::access::claims::Claims;
use crate::error::{ApiResult, AppError};
use crate::state::AppState;

/// The app's own name, captured on every row so a message can never be mis-attributed when the
/// team reads it in any one console.
const APP_NAME: &str = "IncentiveSwift";

#[derive(Deserialize)]
pub struct SubmitFeedback {
    pub kind: String,
    pub screen: String,
    pub message: String,
}

fn clean_text(raw: &str, max: usize, label: &str) -> Result<String, AppError> {
    let t = raw.trim();
    if t.is_empty() {
        return Err(AppError::Validation(format!("{label} is required.")));
    }
    if t.chars().count() > max {
        return Err(AppError::Validation(format!(
            "{label} is too long ({max} characters maximum)."
        )));
    }
    Ok(t.to_string())
}

/// POST /api/v1/feedback — capture an idea / bug / complaint with account, person, app and screen.
pub async fn submit_feedback(
    State(state): State<AppState>,
    Extension(claims): Extension<Claims>,
    Json(req): Json<SubmitFeedback>,
) -> ApiResult<Json<serde_json::Value>> {
    let tenant_id = claims.account_uuid()?;
    let user_id = Uuid::parse_str(&claims.sub)
        .map_err(|_| AppError::Unauthorized("Invalid session".to_string()))?;
    let kind = clean_text(&req.kind, 20, "Type")?;
    let screen = clean_text(&req.screen, 120, "Screen")?;
    let message = clean_text(&req.message, 4000, "Message")?;

    let row = sqlx::query(
        "INSERT INTO feedback (tenant_id, user_id, app, screen, kind, message) \
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING id",
    )
    .bind(tenant_id)
    .bind(user_id)
    .bind(APP_NAME)
    .bind(&screen)
    .bind(&kind)
    .bind(&message)
    .fetch_one(&state.db)
    .await
    .map_err(crate::error::db_error)?;

    let id: Uuid = row.get("id");
    Ok(Json(json!({ "ok": true, "id": id })))
}

/// GET /api/v1/feedback — the team-side queue, platform operator only.
pub async fn list_feedback(
    State(state): State<AppState>,
    Extension(claims): Extension<Claims>,
) -> ApiResult<Json<serde_json::Value>> {
    // Fail closed for anyone who is not a platform operator: feedback is the customer talking to
    // US, so the reader is the operator, never a tenant browsing its own (or another's) messages.
    if !crate::security::auth::is_platform_admin(&claims.role) {
        return Err(AppError::Forbidden("Admin role required".to_string()));
    }

    let rows = sqlx::query(
        "SELECT f.id, f.tenant_id, f.app, f.screen, f.kind, f.message, f.status, \
                a.email AS email, a.name AS name, \
                to_char(f.created_at, 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created_at \
           FROM feedback f \
           LEFT JOIN accounts a ON a.id = f.user_id \
          ORDER BY f.created_at DESC \
          LIMIT 200",
    )
    .fetch_all(&state.db)
    .await
    .map_err(crate::error::db_error)?;

    let items: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.get::<Uuid, _>("id"),
                "account_id": r.get::<Uuid, _>("tenant_id"),
                "email": r.get::<Option<String>, _>("email"),
                "name": r.get::<Option<String>, _>("name"),
                "app": r.get::<String, _>("app"),
                "screen": r.get::<String, _>("screen"),
                "kind": r.get::<String, _>("kind"),
                "message": r.get::<String, _>("message"),
                "status": r.get::<String, _>("status"),
                "created_at": r.get::<String, _>("created_at"),
            })
        })
        .collect();

    Ok(Json(json!({ "count": items.len(), "feedback": items })))
}

pub fn router(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/", routing::post(submit_feedback).get(list_feedback))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::access::claims::claims_middleware,
        ))
}
