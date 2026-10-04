//! Contacts handlers — list and get contacts.
//!
//! Every arm here passes the caller's `account_id` down to `db::contacts`, which is the one place
//! that decides visibility (a `contact_tenants` link row). kanban t_369cb159.

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

/// Parse the authenticated account id once, per arm.
fn caller_account(user: &AuthenticatedUser) -> Result<Uuid, AppError> {
    Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))
}

/// GET /api/v1/contacts — authenticated, paginated with search.
/// Only contacts linked to the calling account are returned.
pub async fn list_contacts(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Query(query): Query<ListContactsQuery>,
) -> Result<Json<Value>, AppError> {
    let account_id = caller_account(&user)?;
    let limit = query.limit.unwrap_or(50).min(100);
    let offset = query.offset.unwrap_or(0);
    let search = query.search.as_deref();

    let contact_list =
        contacts::list_contacts(&state.db, &account_id, limit, offset, search).await?;

    Ok(Json(json!({
        "contacts": contact_list,
        "count": contact_list.len(),
        "limit": limit,
        "offset": offset,
    })))
}

/// GET /api/v1/contacts/:id — authenticated, returns full contact with entry history + Q&A.
/// A contact the caller cannot see answers 404, the same as one that does not exist.
pub async fn get_contact(
    State(state): State<AppState>,
    Path(id): Path<String>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let account_id = caller_account(&user)?;
    let contact_id =
        Uuid::parse_str(&id).map_err(|_| AppError::BadRequest("Invalid contact ID".to_string()))?;

    // Get contact (scoped to the caller's account)
    let contact = contacts::get_contact(&state.db, &account_id, &contact_id).await?;

    // Get entry history
    let entry_history = entries::get_entries_for_contact(&state.db, &contact_id).await?;

    // For each entry, get Q&A history
    let mut entries_with_qa: Vec<Value> = Vec::new();
    for entry in &entry_history {
        let qa = questions_answers::get_questions_with_answers(&state.db, &entry.id)
            .await
            .unwrap_or_else(|e| {
                tracing::error!(error = %e, entry_id = %entry.id, "Q&A history decode failed — this entry renders with an empty question list");
                Vec::new()
            });
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
/// Creates the shared identity row if new and always links it to the calling account, so the
/// account sees it immediately (this is the console's "new contact" and a CSV import row).
pub async fn create_contact(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<ContactBody>,
) -> Result<Json<Value>, AppError> {
    let account_id = caller_account(&user)?;
    let input = body_to_input(body);
    let contact = contacts::create_contact(&state.db, &account_id, &input, "console").await?;
    Ok(Json(json!({
        "contact": contact,
        "created": true
    })))
}

/// PUT /api/v1/contacts/:id — update contact (authenticated). 404 on a contact the caller does
/// not own; the write itself carries the tenancy predicate.
pub async fn update_contact(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(id): Path<String>,
    Json(body): Json<ContactBody>,
) -> Result<Json<Value>, AppError> {
    let account_id = caller_account(&user)?;
    let contact_id =
        Uuid::parse_str(&id).map_err(|_| AppError::BadRequest("Invalid contact ID".to_string()))?;

    let input = body_to_input(body);
    let contact = contacts::update_contact(&state.db, &account_id, &contact_id, &input).await?;
    Ok(Json(json!({
        "contact": contact,
        "updated": true
    })))
}

/// DELETE /api/v1/contacts/:id — delete contact (authenticated).
/// Unlinks the contact from the calling account and drops the shared identity row only when no
/// other account is linked to it any more.
pub async fn delete_contact(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    let account_id = caller_account(&user)?;
    let contact_id =
        Uuid::parse_str(&id).map_err(|_| AppError::BadRequest("Invalid contact ID".to_string()))?;

    let deleted = contacts::delete_contact(&state.db, &account_id, &contact_id).await?;
    if !deleted {
        return Err(AppError::NotFound("Contact not found".to_string()));
    }

    Ok(Json(json!({
        "status": "deleted"
    })))
}
