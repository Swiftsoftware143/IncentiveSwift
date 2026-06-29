//! Entry handler — the core capture endpoint.

use crate::error::AppError;
use crate::state::AppState;
use crate::db::{contacts, entries, campaigns, delivery_log};
use crate::delivery::{payload::DeliveryPayload, webhook, payload::ContactPayload};
use axum::{extract::State, Json};
use serde::Deserialize;
use serde_json::{json, Value};

/// Request body for creating an entry.
#[derive(Deserialize)]
pub struct CreateEntryBody {
    pub contact: ContactBody,
    pub campaign_slug: String,
    pub answers: Option<Value>,
    pub score: Option<i32>,
}

#[derive(Deserialize)]
pub struct ContactBody {
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub business_name: Option<String>,
}

/// POST /api/v1/entries — create entry (public, rate-limited).
/// Flow: upsert contact -> find campaign -> create entry -> build payload -> trigger delivery -> return.
pub async fn create_entry(
    State(state): State<AppState>,
    Json(body): Json<CreateEntryBody>,
) -> Result<Json<Value>, AppError> {
    // 1. Upsert contact
    let contact_input = contacts::ContactInput {
        first_name: body.contact.first_name.clone(),
        last_name: body.contact.last_name.clone(),
        email: body.contact.email.clone(),
        phone: body.contact.phone.clone(),
        business_name: body.contact.business_name.clone(),
    };
    let contact_id = contacts::upsert_contact(&state.db, &contact_input).await?;

    // 2. Find campaign by slug
    let campaign = campaigns::get_campaign_by_slug(&state.db, &body.campaign_slug).await?;

    // 3. Determine outcome and tags
    let (outcome, tags) = determine_outcome(&campaign, body.score);
    let tags_applied = tags.clone();

    // 4. Create entry
    let answers_json = body.answers.clone().unwrap_or_else(|| json!({}));
    let entry_input = entries::CreateEntryInput {
        contact_id,
        campaign_id: campaign.id,
        answers: answers_json,
        score: body.score,
        outcome: Some(outcome.clone()),
        tags_applied: Some(tags_applied.clone()),
    };
    let entry_id = entries::create_entry(&state.db, &entry_input).await?;

    // 5. Build delivery payload from normalized Q&A
    let qa_pairs = if let Some(ref answers) = body.answers {
        // Extract Q&A from the JSONB for delivery payload
        extract_qa_from_jsonb(answers, &[])
    } else {
        vec![]
    };

    let payload = DeliveryPayload::build(
        ContactPayload {
            first_name: body.contact.first_name,
            last_name: body.contact.last_name,
            email: body.contact.email,
            phone: body.contact.phone,
            business_name: body.contact.business_name,
        },
        crate::delivery::payload::CampaignPayload {
            name: campaign.name.clone(),
            campaign_type: campaign.r#type.clone(),
            tag_namespace: campaign.tag_namespace.clone(),
        },
        outcome,
        tags_applied,
        body.score,
        qa_pairs,
        entry_id.to_string(),
    );

    // 6. Trigger delivery based on campaign delivery method
    match campaign.delivery_method.as_str() {
        "direct_api" => {
            // Direct API delivery — determine which API from delivery_config
            let api_type = campaign.delivery_config.get("api_type")
                .and_then(|v| v.as_str())
                .unwrap_or("webhook");
            let api_key = campaign.delivery_config.get("api_key")
                .and_then(|v| v.as_str())
                .unwrap_or("");

            match api_type {
                "hubspot" => {
                    crate::delivery::direct_api::hubspot::push_to_hubspot(&state.http_client, api_key, &payload).await?;
                }
                "activecampaign" => {
                    crate::delivery::direct_api::activecampaign::push_to_activecampaign(&state.http_client, api_key, &payload).await?;
                }
                "gohighlevel" => {
                    crate::delivery::direct_api::gohighlevel::push_to_gohighlevel(&state.http_client, api_key, &payload).await?;
                }
                _ => {
                    // Fallback: try webhook
                    let url = campaign.delivery_config.get("webhook_url")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if !url.is_empty() {
                        webhook::push_to_webhook(&state.http_client, url, &payload, &state.db, &entry_id).await?;
                    }
                }
            }
        }
        "webhook" | _ => {
            // Webhook delivery
            let url = campaign.delivery_config.get("webhook_url")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if !url.is_empty() {
                webhook::push_to_webhook(&state.http_client, url, &payload, &state.db, &entry_id).await?;
            }
        }
    }

    // 7. Return result
    Ok(Json(json!({
        "entry_id": entry_id,
        "contact_id": contact_id,
        "outcome": payload.outcome,
        "tags_applied": payload.tags_applied,
    })))
}

/// Determine outcome and tags based on campaign config and score.
fn determine_outcome(campaign: &crate::db::campaigns::Campaign, score: Option<i32>) -> (String, Vec<String>) {
    let default_outcome = "entrant".to_string();
    let default_tags = vec![format!("{}_entrant", campaign.tag_namespace)];

    // If no score, return default
    let score = match score {
        Some(s) => s,
        None => return (default_outcome, default_tags),
    };

    // Try to get outcome tags from campaign config
    let tag_namespace = &campaign.tag_namespace;
    let outcome_tags = &campaign.outcome_tags;

    // Check for winner outcome
    if let Some(threshold) = outcome_tags.get("winner_threshold").and_then(|v| v.as_i64()) {
        if score >= threshold as i32 {
            let tag = outcome_tags.get("winner")
                .and_then(|v| v.as_str())
                .unwrap_or(&format!("{}_winner", tag_namespace))
                .to_string();
            return ("winner".to_string(), vec![tag]);
        }
    }

    if let Some(threshold) = outcome_tags.get("runner_up_threshold").and_then(|v| v.as_i64()) {
        if score >= threshold as i32 {
            let tag = outcome_tags.get("runner_up")
                .and_then(|v| v.as_str())
                .unwrap_or(&format!("{}_runner_up", tag_namespace))
                .to_string();
            return ("runner_up".to_string(), vec![tag]);
        }
    }

    // Default entrant
    (default_outcome, default_tags)
}

/// Extract Q&A pairs from JSONB answers for the delivery payload.
fn extract_qa_from_jsonb(answers: &Value, _questions: &[crate::db::questions_answers::QuestionAnswerPair]) -> Vec<crate::delivery::payload::QuestionAnswerPair> {
    let mut pairs = vec![];

    if let Some(obj) = answers.as_object() {
        for (key, value) in obj {
            let question_text = key.clone();
            let answer_text = match value {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => b.to_string(),
                Value::Array(arr) => arr.iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                _ => value.to_string(),
            };
            pairs.push(crate::delivery::payload::QuestionAnswerPair {
                question: question_text,
                answer: answer_text,
            });
        }
    }

    pairs
}
