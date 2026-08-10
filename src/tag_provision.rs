use axum::{extract::State, http::HeaderMap, Json};
use axum::response::IntoResponse;
use uuid::Uuid;

use crate::AppState;
use crate::error::{ApiResult, AppError};

/// POST /api/v1/internal/tag-provision
pub async fn tag_provision(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<serde_json::Value>,
) -> ApiResult<impl IntoResponse> {
    let key = headers.get("x-internal-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if key != state.config.internal_sync_key {
        return Err(AppError::Unauthorized(axum::http::StatusCode::UNAUTHORIZED, "Invalid internal key".into()));
    }

    let contact = payload.get("contact");
    let tag = payload.get("tag");

    let email = contact
        .and_then(|c| c.get("email"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if email.is_empty() {
        return Err(AppError::BadRequest("contact.email is required".into()));
    }

    let name = contact
        .and_then(|c| c.get("first_name"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let tag_name = tag
        .and_then(|t| t.get("name"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    // Idempotency
    let existing: Option<(String,)> = sqlx::query_as(
        "SELECT id::text FROM campaigns WHERE id IN (SELECT id FROM contacts WHERE email = $1) LIMIT 1"
    )
    .bind(&email)
    .fetch_optional(&state.db)
    .await?;

    if existing.is_some() {
        return Ok(Json(serde_json::json!({"status": "exists", "email": email})));
    }

    // Create a contact entry for this lead
    let contact_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO contacts (id, first_name, email, notes, created_at) VALUES ($1, $2, $3, $4, NOW())"
    )
    .bind(contact_id)
    .bind(&name)
    .bind(&email)
    .bind(format!("Auto-provisioned via FunnelSwift tag: {}", tag_name))
    .execute(&state.db)
    .await?;

    tracing::info!(
        contact_id = %contact_id,
        email = %email,
        tag = %tag_name,
        "tag_provision: IncentiveSwift contact created"
    );

    Ok(Json(serde_json::json!({
        "status": "created",
        "contact_id": contact_id.to_string(),
        "email": email
    })))
}
