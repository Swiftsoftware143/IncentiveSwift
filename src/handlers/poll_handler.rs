//! Poll handler — single-question vote with unique-voter dedup + results.
//!
//! Votes are stored in `entries` (outcome = "poll_vote", answers.option = choice).
//! A contact may vote only once; a repeat vote returns 409.

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
use sqlx::Row;

/// Request body for a poll vote.
#[derive(Debug, Deserialize)]
pub struct PollVoteBody {
    pub contact: MechanicContact,
    pub option: String,
    pub utm_source: Option<String>,
    pub utm_medium: Option<String>,
    pub utm_campaign: Option<String>,
    pub referrer_url: Option<String>,
    pub page_url: Option<String>,
}

/// POST /api/v1/campaigns/:slug/poll
pub async fn poll_vote(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    headers: HeaderMap,
    Json(body): Json<PollVoteBody>,
) -> Result<Json<Value>, AppError> {
    let campaign = campaigns::get_campaign_by_slug(&state.db, &slug).await?;
    if campaign.status != "active" {
        return Err(AppError::Forbidden("Campaign is not active".to_string()));
    }
    if campaign.r#type != "poll" {
        return Err(AppError::BadRequest(format!(
            "Campaign '{}' is type '{}', not 'poll'",
            campaign.slug, campaign.r#type
        )));
    }

    gate_mechanic(&state, &campaign.account_id, "poll").await?;

    let option = body.option.trim().to_string();
    if option.is_empty() {
        return Err(AppError::BadRequest("option is required".to_string()));
    }

    // Validate the option is one of the configured choices (if options provided).
    if let Some(options) = campaign.config.get("options").and_then(|o| o.as_array()) {
        let valid = options
            .iter()
            .any(|o| o.as_str().map(|s| s == option).unwrap_or(false));
        if !valid {
            return Err(AppError::BadRequest(format!(
                "Invalid option '{}'. Must be one of the configured poll options.",
                option
            )));
        }
    }

    let contact_id = resolve_contact(&state, &body.contact).await?;

    // Unique-voter dedup.
    let existing: Option<uuid::Uuid> = sqlx::query_scalar(
        "SELECT id FROM entries WHERE contact_id = $1 AND campaign_id = $2 AND outcome = 'poll_vote' LIMIT 1",
    )
    .bind(contact_id)
    .bind(campaign.id)
    .fetch_optional(&state.db)
    .await?;

    if existing.is_some() {
        return Err(AppError::Forbidden(
            "This contact has already voted in this poll.".to_string(),
        ));
    }

    let (user_agent, ip_address) = extract_source(&headers);
    let entry_id = entries::create_entry(
        &state.db,
        &entries::CreateEntryInput {
            contact_id,
            campaign_id: campaign.id,
            answers: json!({
                "kind": "poll_vote",
                "option": option,
            }),
            score: None,
            outcome: Some("poll_vote".to_string()),
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

    Ok(Json(json!({
        "entry_id": entry_id,
        "contact_id": contact_id,
        "option": option,
    })))
}

/// GET /api/v1/campaigns/:slug/poll/results
pub async fn poll_results(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> Result<Json<Value>, AppError> {
    let campaign = campaigns::get_campaign_by_slug(&state.db, &slug).await?;
    if campaign.r#type != "poll" {
        return Err(AppError::BadRequest(format!(
            "Campaign '{}' is type '{}', not 'poll'",
            campaign.slug, campaign.r#type
        )));
    }

    let question = campaign
        .config
        .get("question")
        .and_then(|v| v.as_str())
        .unwrap_or("Poll");

    let rows = sqlx::query(
        r#"SELECT answers->>'option' AS option, COUNT(*) AS votes
           FROM entries
           WHERE campaign_id = $1 AND outcome = 'poll_vote'
           GROUP BY answers->>'option'
           ORDER BY votes DESC"#,
    )
    .bind(campaign.id)
    .fetch_all(&state.db)
    .await?;

    let results: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "option": r.get::<Option<String>, _>("option").unwrap_or_default(),
                "votes": r.get::<i64, _>("votes"),
            })
        })
        .collect();

    let total_votes: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM entries WHERE campaign_id = $1 AND outcome = 'poll_vote'",
    )
    .bind(campaign.id)
    .fetch_one(&state.db)
    .await?;

    Ok(Json(json!({
        "campaign_id": campaign.id,
        "question": question,
        "total_votes": total_votes,
        "results": results,
    })))
}
