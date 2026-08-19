//! Countdown handler — urgency gate returning a target timestamp + locked state.
//!
//! No draw is performed. The server reads the deadline from campaign config and
//! reports locked/unlocked. `POST` also records an entry so urgency views are
//! captured for analytics.

use crate::db::campaigns;
use crate::db::entries;
use crate::error::AppError;
use crate::handlers::mechanic_common::{
    extract_source, gate_mechanic, resolve_contact, MechanicContact,
};
use crate::state::AppState;
use axum::{
    extract::{Path, State},
    http::HeaderMap,
    Json,
};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};

/// Resolve the target timestamp from campaign config (RFC3339).
/// Looks at `config.target_at`, then `config.deadline`, then `config.countdown.ends_at`.
fn target_at(config: &Value) -> Option<chrono::DateTime<Utc>> {
    for key in ["target_at", "deadline"] {
        if let Some(s) = config.get(key).and_then(|v| v.as_str()) {
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
                return Some(dt.with_timezone(&Utc));
            }
        }
    }
    config
        .get("countdown")
        .and_then(|c| c.get("ends_at"))
        .and_then(|v| v.as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc))
}

/// Build the countdown status payload.
fn countdown_status(config: &Value) -> (Option<chrono::DateTime<Utc>>, bool, i64) {
    match target_at(config) {
        Some(target) => {
            let now = Utc::now();
            let locked = now < target;
            let seconds = (target - now).num_seconds().max(0);
            (Some(target), locked, seconds)
        }
        None => (None, false, 0),
    }
}

/// GET /api/v1/campaigns/:slug/countdown
pub async fn countdown_get(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> Result<Json<Value>, AppError> {
    let campaign = campaigns::get_campaign_by_slug(&state.db, &slug).await?;
    if campaign.r#type != "countdown" {
        return Err(AppError::BadRequest(format!(
            "Campaign '{}' is type '{}', not 'countdown'",
            campaign.slug, campaign.r#type
        )));
    }
    let (target, locked, seconds) = countdown_status(&campaign.config);
    Ok(Json(json!({
        "campaign_id": campaign.id,
        "target_at": target.map(|t| t.to_rfc3339()),
        "locked": locked,
        "unlocked": !locked,
        "seconds_remaining": seconds,
    })))
}

/// Request body for a countdown view capture.
#[derive(Debug, Deserialize)]
pub struct CountdownBody {
    pub contact: Option<MechanicContact>,
    pub utm_source: Option<String>,
    pub utm_medium: Option<String>,
    pub utm_campaign: Option<String>,
    pub referrer_url: Option<String>,
    pub page_url: Option<String>,
}

/// POST /api/v1/campaigns/:slug/countdown
pub async fn countdown_post(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    headers: HeaderMap,
    Json(body): Json<CountdownBody>,
) -> Result<Json<Value>, AppError> {
    let campaign = campaigns::get_campaign_by_slug(&state.db, &slug).await?;
    if campaign.status != "active" {
        return Err(AppError::Forbidden("Campaign is not active".to_string()));
    }
    if campaign.r#type != "countdown" {
        return Err(AppError::BadRequest(format!(
            "Campaign '{}' is type '{}', not 'countdown'",
            campaign.slug, campaign.r#type
        )));
    }

    gate_mechanic(&state, &campaign.account_id, "countdown").await?;

    let (target, locked, seconds) = countdown_status(&campaign.config);

    // Record an entry when a contact is provided; otherwise skip persistence.
    let mut entry_id: Option<uuid::Uuid> = None;
    let mut contact_id: Option<uuid::Uuid> = None;
    if let Some(contact) = body.contact.as_ref() {
        if contact.contact_id.is_some() || contact.email.is_some() || contact.phone.is_some() {
            let cid = resolve_contact(&state, contact).await?;
            let (user_agent, ip_address) = extract_source(&headers);
            let eid = entries::create_entry(
                &state.db,
                &entries::CreateEntryInput {
                    contact_id: cid,
                    campaign_id: campaign.id,
                    answers: json!({
                        "target_at": target.map(|t| t.to_rfc3339()),
                        "locked": locked,
                        "seconds_remaining": seconds,
                    }),
                    score: None,
                    outcome: Some(if locked { "locked" } else { "unlocked" }.to_string()),
                    tags_applied: Some(vec![]),
                    utm_source: body.utm_source.clone(),
                    utm_medium: body.utm_medium.clone(),
                    utm_campaign: body.utm_campaign.clone(),
                    referrer_url: body.referrer_url.clone(),
                    page_url: body.page_url.clone(),
                    user_agent,
                    ip_address,
                },
            )
            .await?;
            entry_id = Some(eid);
            contact_id = Some(cid);
        }
    }

    Ok(Json(json!({
        "campaign_id": campaign.id,
        "target_at": target.map(|t| t.to_rfc3339()),
        "locked": locked,
        "unlocked": !locked,
        "seconds_remaining": seconds,
        "entry_id": entry_id,
        "contact_id": contact_id,
    })))
}
