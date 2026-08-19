//! Mystery Reveal handler — one-time locked reward unlock per contact.
//!
//! A contact may reveal the mystery reward only once. The redemption is stored
//! in `entries`; a repeat reveal for the same contact returns 409.

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
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

/// Request body for a mystery reveal.
#[derive(Debug, Deserialize)]
pub struct MysteryBody {
    pub contact: MechanicContact,
    pub utm_source: Option<String>,
    pub utm_medium: Option<String>,
    pub utm_campaign: Option<String>,
    pub referrer_url: Option<String>,
    pub page_url: Option<String>,
}

/// POST /api/v1/campaigns/:slug/mystery
pub async fn mystery(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    headers: HeaderMap,
    Json(body): Json<MysteryBody>,
) -> Result<Json<Value>, AppError> {
    let campaign = campaigns::get_campaign_by_slug(&state.db, &slug).await?;
    if campaign.status != "active" {
        return Err(AppError::Forbidden("Campaign is not active".to_string()));
    }
    if campaign.r#type != "mystery" {
        return Err(AppError::BadRequest(format!(
            "Campaign '{}' is type '{}', not 'mystery'",
            campaign.slug, campaign.r#type
        )));
    }

    gate_mechanic(&state, &campaign.account_id, "mystery").await?;

    let contact_id = resolve_contact(&state, &body.contact).await?;

    // One-time reveal: reject if this contact already redeemed.
    let already: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM entries WHERE contact_id = $1 AND campaign_id = $2 AND outcome = 'redeemed' LIMIT 1",
    )
    .bind(contact_id)
    .bind(campaign.id)
    .fetch_optional(&state.db)
    .await?;

    if already.is_some() {
        return Err(AppError::Forbidden(
            "This mystery reward has already been revealed for this contact.".to_string(),
        ));
    }

    // Reward comes from campaign config.
    let reward = campaign
        .config
        .get("reward")
        .cloned()
        .unwrap_or_else(|| json!({ "label": "Mystery Reward" }));
    let reward_label = reward
        .get("label")
        .and_then(|v| v.as_str())
        .unwrap_or("Mystery Reward")
        .to_string();
    let code = format!("MYSTERY-{:08x}", rand::random::<u32>());

    let (user_agent, ip_address) = extract_source(&headers);
    let entry_id = entries::create_entry(
        &state.db,
        &entries::CreateEntryInput {
            contact_id,
            campaign_id: campaign.id,
            answers: json!({
                "reward": reward,
                "redemption_code": code,
            }),
            score: None,
            outcome: Some("redeemed".to_string()),
            tags_applied: Some(vec![format!("{}_redeemed", campaign.tag_namespace)]),
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

    Ok(Json(json!({
        "entry_id": entry_id,
        "contact_id": contact_id,
        "reward": reward,
        "reward_label": reward_label,
        "redemption_code": code,
    })))
}
