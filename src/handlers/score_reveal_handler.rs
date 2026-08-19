//! Score Reveal handler — reveal a derived score + tier message.
//!
//! The animated reveal is handled client-side; the server computes the score,
//! maps it to a tier from campaign config, stores an entry, and returns the
//! score + tier message.

use crate::db::campaigns;
use crate::db::entries;
use crate::error::AppError;
use crate::handlers::mechanic_common::{
    extract_source, gate_mechanic, resolve_contact, MechanicContact,
};
use crate::mechanics::scoring;
use crate::state::AppState;
use axum::{
    extract::{Path, State},
    http::HeaderMap,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

/// Request body for a score reveal.
#[derive(Debug, Deserialize)]
pub struct ScoreRevealBody {
    pub contact: MechanicContact,
    /// Explicit score override; otherwise derived from `answers`.
    pub score: Option<i32>,
    /// Question/answer map used to derive a score when `score` is omitted.
    pub answers: Option<Value>,
    pub utm_source: Option<String>,
    pub utm_medium: Option<String>,
    pub utm_campaign: Option<String>,
    pub referrer_url: Option<String>,
    pub page_url: Option<String>,
}

/// Map a score to a tier label + message from campaign config.
/// Config shape: `config.tiers` = [{ "min": 0, "max": 59, "label": "...", "message": "..." }]
fn score_tier(score: i32, config: &Value) -> (String, String) {
    if let Some(tiers) = config.get("tiers").and_then(|t| t.as_array()) {
        for tier in tiers {
            let min = tier.get("min").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
            let max = tier
                .get("max")
                .and_then(|v| v.as_i64())
                .unwrap_or(i32::MAX as i64) as i32;
            if score >= min && score <= max {
                let label = tier
                    .get("label")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Tier")
                    .to_string();
                let message = tier
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                return (label, message);
            }
        }
    }
    ("Score".to_string(), String::new())
}

/// POST /api/v1/campaigns/:slug/score-reveal
pub async fn score_reveal(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    headers: HeaderMap,
    Json(body): Json<ScoreRevealBody>,
) -> Result<Json<Value>, AppError> {
    let campaign = campaigns::get_campaign_by_slug(&state.db, &slug).await?;
    if campaign.status != "active" {
        return Err(AppError::Forbidden("Campaign is not active".to_string()));
    }
    if campaign.r#type != "score_reveal" {
        return Err(AppError::BadRequest(format!(
            "Campaign '{}' is type '{}', not 'score_reveal'",
            campaign.slug, campaign.r#type
        )));
    }

    gate_mechanic(&state, &campaign.account_id, "score_reveal").await?;

    let contact_id = resolve_contact(&state, &body.contact).await?;

    // Derive score: explicit override, else compute from answers.
    let answers = body.answers.clone().unwrap_or_else(|| json!({}));
    let score = match body.score {
        Some(s) => s,
        None => scoring::calculate_score("score_reveal", &answers),
    };

    let (tier_label, tier_message) = score_tier(score, &campaign.config);

    let (user_agent, ip_address) = extract_source(&headers);
    let entry_id = entries::create_entry(
        &state.db,
        &entries::CreateEntryInput {
            contact_id,
            campaign_id: campaign.id,
            answers: json!({
                "score": score,
                "tier": tier_label,
                "answers": answers,
            }),
            score: Some(score),
            outcome: Some(tier_label.clone()),
            tags_applied: Some(vec![format!("{}_{}", campaign.tag_namespace, tier_label)]),
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
        "score": score,
        "tier": tier_label,
        "message": tier_message,
    })))
}
