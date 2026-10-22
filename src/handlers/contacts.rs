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

    // The tags column (migration zz_is21). ONE extra query for the whole page, not one per row, and
    // scoped to the same account, so a shared contact never shows another tenant's label.
    let ids: Vec<Uuid> = contact_list.iter().map(|c| c.id).collect();
    let mut tags = contacts::tags_for_contacts(&state.db, &account_id, &ids).await?;
    let rows: Vec<Value> = contact_list
        .iter()
        .map(|c| {
            let mut row = serde_json::to_value(c).unwrap_or_else(|_| json!({}));
            if let Some(obj) = row.as_object_mut() {
                obj.insert(
                    "tags".to_string(),
                    serde_json::to_value(tags.remove(&c.id).unwrap_or_default())
                        .unwrap_or_else(|_| json!([])),
                );
            }
            row
        })
        .collect();

    Ok(Json(json!({
        "contacts": rows,
        "count": rows.len(),
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

    // `contacts` carries `CHECK (email IS NOT NULL OR phone IS NOT NULL)`. A new-contact form with
    // neither reached the INSERT and answered a bare 500 (measured LIVE 2026-10-17), which reads as
    // the server breaking rather than as a field the operator still has to fill in.
    let has_email = input
        .email
        .as_deref()
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false);
    let has_phone = input
        .phone
        .as_deref()
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false);
    if !has_email && !has_phone {
        return Err(AppError::UnprocessableEntity(
            "email or phone: a contact needs at least one of them".to_string(),
        ));
    }

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

// ---- Contact tags (migration zz_is21, cron 2026-10-22) -----------------------------------------
//
// The Tags screen has always listed "Apply a tag to contacts" as an action and the Loyalty tier
// editor promises the reward tag is what the member is tagged with — but `tags` had no link to a
// person, so neither could actually happen. These three arms are that link, scoped exactly like
// every other contact reader: the contact must be linked to the caller's account.

/// Body for `POST /api/v1/contacts/:id/tags`. Exactly one of `tag_id` / `name` is used: `name` is
/// the console's "type the tag you want" path and is created in the caller's own library if new.
#[derive(Deserialize)]
pub struct AssignTagBody {
    pub tag_id: Option<String>,
    pub name: Option<String>,
}

/// GET /api/v1/contacts/:id/tags — the tags this account has on one contact.
pub async fn list_contact_tags(
    State(state): State<AppState>,
    Path(id): Path<String>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let account_id = caller_account(&user)?;
    let contact_id =
        Uuid::parse_str(&id).map_err(|_| AppError::BadRequest("Invalid contact ID".to_string()))?;

    let tags = contacts::list_contact_tags(&state.db, &account_id, &contact_id).await?;
    Ok(Json(json!({ "tags": tags, "count": tags.len() })))
}

/// POST /api/v1/contacts/:id/tags — put a tag on a contact (idempotent).
pub async fn assign_contact_tag(
    State(state): State<AppState>,
    Path(id): Path<String>,
    user: AuthenticatedUser,
    Json(body): Json<AssignTagBody>,
) -> Result<Json<Value>, AppError> {
    let account_id = caller_account(&user)?;
    let contact_id =
        Uuid::parse_str(&id).map_err(|_| AppError::BadRequest("Invalid contact ID".to_string()))?;

    // A name is resolved (or created) in the caller's own library first; a tag_id is taken as-is and
    // then verified to belong to the caller inside `assign_contact_tag`.
    let tag_id = match (&body.tag_id, &body.name) {
        (Some(raw), _) => {
            Uuid::parse_str(raw).map_err(|_| AppError::BadRequest("Invalid tag ID".to_string()))?
        }
        (None, Some(name)) => contacts::ensure_tag_named(&state.db, &account_id, name).await?,
        (None, None) => {
            return Err(AppError::BadRequest(
                "Send either a tag_id or a name".to_string(),
            ))
        }
    };

    let created =
        contacts::assign_contact_tag(&state.db, &account_id, &contact_id, &tag_id, "manual")
            .await?;

    // Answer with the contact's tags as they now stand, so the screen renders the truth instead of
    // assuming its own POST worked.
    let tags = contacts::list_contact_tags(&state.db, &account_id, &contact_id).await?;
    Ok(Json(json!({
        "status": "ok",
        "created": created,
        "tag_id": tag_id,
        "tags": tags,
    })))
}

/// DELETE /api/v1/contacts/:id/tags/:tag_id — take a tag off a contact.
pub async fn remove_contact_tag(
    State(state): State<AppState>,
    Path((id, tag_id)): Path<(String, String)>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let account_id = caller_account(&user)?;
    let contact_id =
        Uuid::parse_str(&id).map_err(|_| AppError::BadRequest("Invalid contact ID".to_string()))?;
    let tag_id =
        Uuid::parse_str(&tag_id).map_err(|_| AppError::BadRequest("Invalid tag ID".to_string()))?;

    let removed =
        contacts::remove_contact_tag(&state.db, &account_id, &contact_id, &tag_id).await?;
    let tags = contacts::list_contact_tags(&state.db, &account_id, &contact_id).await?;
    Ok(Json(
        json!({ "status": "ok", "removed": removed, "tags": tags }),
    ))
}
