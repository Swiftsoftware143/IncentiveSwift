//! Long-Form Qualifier handler — multi-question logic-based scoring.
//!
//! Branch/rule config lives in `config.rules`:
//!   [{ "question": "q1", "answer": "yes", "score": 10, "outcome": "qualified", "tag": "qualified" }]
//! Matched rules sum a score; the outcome is resolved from `config.outcomes`
//! score thresholds (fallback: the last matched rule). The outcome tag is
//! persisted to the entry's tags_applied AND to the contact (notes2).

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

/// Request body for a long-form qualifier submission.
#[derive(Debug, Deserialize)]
pub struct LongFormBody {
    pub contact: MechanicContact,
    /// Map of question key/id -> answer value (string).
    pub answers: Value,
    pub utm_source: Option<String>,
    pub utm_medium: Option<String>,
    pub utm_campaign: Option<String>,
    pub referrer_url: Option<String>,
    pub page_url: Option<String>,
}

/// Resolve outcome label + tag from a total score.
/// `config.outcomes` = [{ "min_score": 0, "label": "...", "tag": "..." }]
fn resolve_outcome(score: i32, config: &Value) -> (String, String) {
    if let Some(outcomes) = config.get("outcomes").and_then(|o| o.as_array()) {
        let mut best = (String::new(), String::new());
        for o in outcomes {
            let min = o.get("min_score").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
            if score >= min {
                let label = o
                    .get("label")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let tag = o
                    .get("tag")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                best = (label, tag);
            }
        }
        if !best.0.is_empty() {
            return best;
        }
    }
    ("unqualified".to_string(), "unqualified".to_string())
}

/// POST /api/v1/campaigns/:slug/long-form-qualifier
pub async fn long_form_qualifier(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    headers: HeaderMap,
    Json(body): Json<LongFormBody>,
) -> Result<Json<Value>, AppError> {
    let campaign = campaigns::get_campaign_by_slug(&state.db, &slug).await?;
    if campaign.status != "active" {
        return Err(AppError::Forbidden("Campaign is not active".to_string()));
    }
    if campaign.r#type != "long_form_qualifier" {
        return Err(AppError::BadRequest(format!(
            "Campaign '{}' is type '{}', not 'long_form_qualifier'",
            campaign.slug, campaign.r#type
        )));
    }

    gate_mechanic(&state, &campaign.account_id, "long_form_qualifier").await?;

    let contact_id = resolve_contact(&state, &body.contact).await?;

    let rules = campaign
        .config
        .get("rules")
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default();

    let mut total_score = 0i32;
    let mut last_outcome: Option<String> = None;
    let mut last_tag: Option<String> = None;

    for rule in &rules {
        let question = rule.get("question").and_then(|v| v.as_str()).unwrap_or("");
        let expected = rule.get("answer").and_then(|v| v.as_str()).unwrap_or("");
        let score = rule.get("score").and_then(|v| v.as_i64()).unwrap_or(0) as i32;

        let user_answer = body
            .answers
            .get(question)
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if !question.is_empty()
            && user_answer.trim().to_lowercase() == expected.trim().to_lowercase()
        {
            total_score += score;
            last_outcome = rule
                .get("outcome")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            last_tag = rule
                .get("tag")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
        }
    }

    // Resolve outcome: config.outcomes thresholds take priority over last rule.
    let (threshold_outcome, threshold_tag) = resolve_outcome(total_score, &campaign.config);
    let (outcome, tag) = if !threshold_outcome.is_empty() && threshold_outcome != "unqualified" {
        (threshold_outcome, threshold_tag)
    } else {
        (
            last_outcome.unwrap_or_else(|| "unqualified".to_string()),
            last_tag.unwrap_or_else(|| "unqualified".to_string()),
        )
    };

    let full_tag = if tag.is_empty() { outcome.clone() } else { tag };
    let namespaced_tag = format!("{}_{}", campaign.tag_namespace, full_tag);

    // Persist outcome tag to the contact (notes2, comma-joined).
    let _ = sqlx::query(
        r#"UPDATE contacts SET notes2 = CASE
             WHEN notes2 IS NULL OR notes2 = '' THEN $2
             ELSE notes2 || ',' || $2
           END
           WHERE id = $1"#,
    )
    .bind(contact_id)
    .bind(&namespaced_tag)
    .execute(&state.db)
    .await;

    let (user_agent, ip_address) = extract_source(&headers);
    let entry_id = entries::create_entry(
        &state.db,
        &entries::CreateEntryInput {
            contact_id,
            campaign_id: campaign.id,
            answers: json!({
                "answers": body.answers,
                "total_score": total_score,
                "outcome": outcome,
            }),
            score: Some(total_score),
            outcome: Some(outcome.clone()),
            tags_applied: Some(vec![namespaced_tag.clone()]),
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
        "score": total_score,
        "outcome": outcome,
        "tag": namespaced_tag,
    })))
}
