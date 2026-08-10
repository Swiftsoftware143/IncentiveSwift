//! Contacts handlers — list and get contacts.

use crate::db::{contacts, entries, questions_answers};
use crate::error::AppError;
use crate::security::auth::AuthenticatedUser;
use crate::state::AppState;
use axum::{
    extract::{Path, Query, State},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

/// Query parameters for listing contacts.
#[derive(Deserialize)]
pub struct ListContactsQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub search: Option<String>,
}

/// GET /api/v1/contacts — authenticated, paginated with search.
pub async fn list_contacts(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Query(query): Query<ListContactsQuery>,
) -> Result<Json<Value>, AppError> {
    let limit = query.limit.unwrap_or(50).min(100);
    let offset = query.offset.unwrap_or(0);
    let search = query.search.as_deref();

    let contact_list = contacts::list_contacts(&state.db, limit, offset, search).await?;

    Ok(Json(json!({
        "contacts": contact_list,
        "count": contact_list.len(),
        "limit": limit,
        "offset": offset,
    })))
}

/// GET /api/v1/contacts/:id — authenticated, returns full contact with entry history + Q&A.
pub async fn get_contact(
    State(state): State<AppState>,
    Path(id): Path<String>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let contact_id =
        Uuid::parse_str(&id).map_err(|_| AppError::BadRequest("Invalid contact ID".to_string()))?;

    // Get contact
    let contact = contacts::get_contact(&state.db, &contact_id).await?;

    // Get entry history
    let entry_history = entries::get_entries_for_contact(&state.db, &contact_id).await?;

    // For each entry, get Q&A history
    let mut entries_with_qa: Vec<Value> = Vec::new();
    for entry in &entry_history {
        let qa = questions_answers::get_questions_with_answers(&state.db, &entry.id)
            .await
            .unwrap_or_default();
        entries_with_qa.push(json!({
            "entry": entry,
            "questions_and_answers": qa,
        }));
    }

    Ok(Json(json!({
        "contact": contact,
        "entries": entries_with_qa,
    })))
}

/// Input for creating/updating a contact via REST.
#[derive(Deserialize)]
pub struct ContactBody {
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub website: Option<String>,
    pub business_name: Option<String>,
    pub name: Option<String>,
}

/// Helper to convert ContactBody to ContactInput, splitting full name if needed.
fn body_to_input(body: ContactBody) -> contacts::ContactInput {
    let (first_name, last_name) = if let Some(name) = body.name {
        let mut parts = name.splitn(2, ' ');
        let first = parts.next().map(|s| s.to_string());
        let last = parts.next().map(|s| s.to_string());
        (first, last)
    } else {
        (body.first_name, body.last_name)
    };

    contacts::ContactInput {
        first_name,
        last_name,
        email: body.email,
        phone: body.phone,
        website: body.website,
        business_name: body.business_name,
    }
}

/// POST /api/v1/contacts — create contact (authenticated).
pub async fn create_contact(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<ContactBody>,
) -> Result<Json<Value>, AppError> {
    let input = body_to_input(body);
    let contact = contacts::create_contact(&state.db, &input).await?;
    Ok(Json(json!({
        "contact": contact,
        "created": true
    })))
}

/// PUT /api/v1/contacts/:id — update contact (authenticated).
pub async fn update_contact(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(id): Path<String>,
    Json(body): Json<ContactBody>,
) -> Result<Json<Value>, AppError> {
    let contact_id =
        Uuid::parse_str(&id).map_err(|_| AppError::BadRequest("Invalid contact ID".to_string()))?;

    let input = body_to_input(body);
    let contact = contacts::update_contact(&state.db, &contact_id, &input).await?;
    Ok(Json(json!({
        "contact": contact,
        "updated": true
    })))
}

/// DELETE /api/v1/contacts/:id — delete contact (authenticated).
pub async fn delete_contact(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    let contact_id =
        Uuid::parse_str(&id).map_err(|_| AppError::BadRequest("Invalid contact ID".to_string()))?;

    let deleted = contacts::delete_contact(&state.db, &contact_id).await?;
    if !deleted {
        return Err(AppError::NotFound("Contact not found".to_string()));
    }

    Ok(Json(json!({
        "status": "deleted"
    })))
}
