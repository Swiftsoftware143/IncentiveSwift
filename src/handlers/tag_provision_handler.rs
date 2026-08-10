//! Tag provision webhook — receives FunnelSwift system tag assignments.

use axum::response::IntoResponse;
use axum::{extract::State, http::HeaderMap, Json};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::error::AppError;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct TagProvisionRequest {
    pub contact: TagProvisionContact,
    pub tag: TagProvisionTag,
    pub source: String,
    pub timestamp: String,
}

#[derive(Debug, Deserialize)]
pub struct TagProvisionContact {
    pub id: Option<String>,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    pub company: Option<String>,
    pub custom_fields: Option<Value>,
}

#[derive(Debug, Deserialize)]
pub struct TagProvisionTag {
    pub name: String,
    pub campaign_id: Option<String>,
    pub metadata: Option<Value>,
}

pub async fn handle_tag_provision(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<TagProvisionRequest>,
) -> Result<impl IntoResponse, AppError> {
    let key = headers
        .get("x-internal-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let expected = state.config.internal_sync_key.as_str();
    if key != expected {
        return Err(AppError::Unauthorized("Invalid internal key".into()));
    }

    let email = req
        .contact
        .email
        .as_deref()
        .unwrap_or("")
        .trim()
        .to_lowercase();
    let first_name = req
        .contact
        .first_name
        .as_deref()
        .unwrap_or("")
        .trim()
        .to_string();
    let last_name = req
        .contact
        .last_name
        .as_deref()
        .unwrap_or("")
        .trim()
        .to_string();
    let company_name = req
        .contact
        .company
        .as_deref()
        .unwrap_or("")
        .trim()
        .to_string();
    let phone = req
        .contact
        .phone
        .as_deref()
        .unwrap_or("")
        .trim()
        .to_string();

    if !email.is_empty() {
        let existing: Option<(Uuid,)> =
            sqlx::query_as(r#"SELECT id FROM contacts WHERE email = $1 LIMIT 1"#)
                .bind(&email)
                .fetch_optional(&state.db)
                .await
                .map_err(|e| AppError::Database(e.to_string()))?;

        if let Some((contact_id,)) = existing {
            return Ok((
                axum::http::StatusCode::OK,
                Json(json!({
                    "status": "already_exists",
                    "contact_id": contact_id.to_string(),
                })),
            ));
        }
    }

    let contact_id = Uuid::new_v4();
    let notes = format!("funnelswift:{}:{}", req.source, req.tag.name);

    sqlx::query(
        r#"INSERT INTO contacts (id, first_name, last_name, email, phone, business_name, notes, created_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, NOW())"#
    )
    .bind(contact_id)
    .bind(&first_name)
    .bind(&last_name)
    .bind(if email.is_empty() { None } else { Some(&email) })
    .bind(if phone.is_empty() { None } else { Some(&phone) })
    .bind(if company_name.is_empty() { None } else { Some(&company_name) })
    .bind(&notes)
    .execute(&state.db)
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;

    tracing::info!(
        "tag_provision: created contact {} ({})",
        contact_id,
        first_name
    );

    Ok((
        axum::http::StatusCode::CREATED,
        Json(json!({
            "status": "provisioned",
            "contact_id": contact_id.to_string(),
        })),
    ))
}
