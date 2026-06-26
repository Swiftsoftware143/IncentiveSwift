//! Contacts handlers — list and get contacts.

use crate::error::AppError;
use crate::state::AppState;
use crate::security::auth::AuthenticatedUser;
use crate::db::{contacts, entries, questions_answers};
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
    _user: AuthenticatedUser,
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
    _user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let contact_id = Uuid::parse_str(&id)
        .map_err(|_| AppError::BadRequest("Invalid contact ID".to_string()))?;

    // Get contact
    let contact = contacts::get_contact(&state.db, &contact_id).await?;

    // Get entry history
    let entry_history = entries::get_entries_for_contact(&state.db, &contact_id).await?;

    // For each entry, get Q&A history
    let mut entries_with_qa: Vec<Value> = Vec::new();
    for entry in &entry_history {
        let qa = questions_answers::get_questions_with_answers(&state.db, &entry.id).await.unwrap_or_default();
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
