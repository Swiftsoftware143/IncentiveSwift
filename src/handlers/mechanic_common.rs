//! Shared helpers for mechanic play handlers.
//!
//! Each mechanic handler (score_reveal, scratch_card, mystery, countdown, poll,
//! chat, long_form_qualifier) resolves a contact the same way and enforces the
//! same plan-tier feature gate. Keeping that here avoids duplication.

use crate::db::contacts;
use crate::error::AppError;
use crate::state::AppState;
use axum::http::HeaderMap;
use serde::Deserialize;
use uuid::Uuid;

/// Contact descriptor embedded in every mechanic play request body.
#[derive(Debug, Clone, Deserialize)]
pub struct MechanicContact {
    pub contact_id: Option<Uuid>,
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub website: Option<String>,
    pub business_name: Option<String>,
}

/// Resolve (or create) a contact from the request body.
/// Prefers an explicit `contact_id`, then upserts by email, then phone.
pub async fn resolve_contact(
    state: &AppState,
    contact: &MechanicContact,
) -> Result<Uuid, AppError> {
    if let Some(cid) = contact.contact_id {
        contacts::get_contact(&state.db, &cid).await?;
        return Ok(cid);
    }

    if contact.email.is_none() && contact.phone.is_none() {
        return Err(AppError::BadRequest(
            "Either contact_id, email, or phone is required".to_string(),
        ));
    }

    let input = contacts::ContactInput {
        first_name: contact.first_name.clone(),
        last_name: contact.last_name.clone(),
        email: contact.email.clone(),
        phone: contact.phone.clone(),
        website: contact.website.clone(),
        business_name: contact.business_name.clone(),
    };
    contacts::upsert_contact(&state.db, &input).await
}

/// Extract (user_agent, ip_address) from request headers for source tracking.
pub fn extract_source(headers: &HeaderMap) -> (Option<String>, Option<String>) {
    let user_agent = headers
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let ip_address = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .or_else(|| headers.get("x-real-ip").and_then(|v| v.to_str().ok()))
        .map(|s| s.to_string());
    (user_agent, ip_address)
}

/// Gate play on the campaign owner's plan tier. Free-tier accounts are blocked
/// with 402 (UpgradeRequired); pro/enterprise pass.
pub async fn gate_mechanic(
    state: &AppState,
    account_id: &Uuid,
    mechanic_type: &str,
) -> Result<(), AppError> {
    crate::access::feature_gate::enforce_mechanic_feature(
        state,
        &account_id.to_string(),
        mechanic_type,
    )
    .await
}
