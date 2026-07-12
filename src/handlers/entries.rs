//! Entry handler — the core capture endpoint.

use crate::error::AppError;
use crate::state::AppState;
use crate::db::{contacts, entries, campaigns};
use crate::delivery::{payload::DeliveryPayload, webhook, payload::ContactPayload};
use axum::{
    extract::State,
    http::HeaderMap,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

/// Request body for creating an entry.
#[derive(Deserialize)]
pub struct CreateEntryBody {
    pub contact: ContactBody,
    pub campaign_slug: String,
    pub answers: Option<Value>,
    pub score: Option<i32>,
    pub utm_source: Option<String>,
    pub utm_medium: Option<String>,
    pub utm_campaign: Option<String>,
    pub referrer_url: Option<String>,
    pub page_url: Option<String>,
}

#[derive(Deserialize)]
pub struct ContactBody {
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub website: Option<String>,
    pub business_name: Option<String>,
}

/// POST /api/v1/entries — create entry (public, rate-limited).
/// Flow: upsert contact -> find campaign -> check daily limit -> apply pity timer -> create entry -> build payload -> trigger delivery -> return.
/// Extract user agent and IP from request headers.
fn extract_source_headers(headers: &HeaderMap) -> (Option<String>, Option<String>) {
    let user_agent = headers
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let ip_address = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .or_else(|| {
            headers
                .get("x-real-ip")
                .and_then(|v| v.to_str().ok())
        })
        .map(|s| s.to_string());
    (user_agent, ip_address)
}

pub async fn create_entry(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateEntryBody>,
) -> Result<Json<Value>, AppError> {
    // 1. Upsert contact
    let contact_input = contacts::ContactInput {
        first_name: body.contact.first_name.clone(),
        last_name: body.contact.last_name.clone(),
        email: body.contact.email.clone(),
        phone: body.contact.phone.clone(),
        website: body.contact.website.clone(),
        business_name: body.contact.business_name.clone(),
    };
    let contact_id = contacts::upsert_contact(&state.db, &contact_input).await?;

    // 2. Find campaign by slug
    let campaign = campaigns::get_campaign_by_slug(&state.db, &body.campaign_slug).await?;

    // 3. Check daily spin limit (before creating entry)
    crate::mechanics::pity_timer::check_daily_limit(
        &state.db, &campaign.id, &contact_id, &campaign.config
    ).await?;

    // 4. Determine outcome and tags
    let (mut outcome, mut tags) = determine_outcome(&campaign, body.score);

    // 5. Apply pity timer — may override outcome to force a win
    let (pity_triggered, pity_outcome, pity_tags) = crate::mechanics::pity_timer::apply_pity_timer(
        &state.db,
        &campaign.id,
        &contact_id,
        &campaign.config,
        &campaign.tag_namespace,
        &outcome,
        &tags,
    ).await?;
    if pity_triggered {
        outcome = pity_outcome;
        tags = pity_tags;
    }

    let tags_applied = tags.clone();

    // 6. Create entry
    let (user_agent, ip_address) = extract_source_headers(&headers);

    let answers_json = body.answers.clone().unwrap_or_else(|| json!({}));
    let entry_input = entries::CreateEntryInput {
        contact_id,
        campaign_id: campaign.id,
        answers: answers_json,
        score: body.score,
        outcome: Some(outcome.clone()),
        tags_applied: Some(tags_applied.clone()),
        utm_source: body.utm_source.clone(),
        utm_medium: body.utm_medium.clone(),
        utm_campaign: body.utm_campaign.clone(),
        referrer_url: body.referrer_url.clone(),
        page_url: body.page_url.clone(),
        user_agent,
        ip_address,
    };
    let entry_id = entries::create_entry(&state.db, &entry_input).await?;

    // 7. Record daily spin count
    crate::mechanics::pity_timer::record_daily_spin(&state.db, &campaign.id, &contact_id).await?;

    // 7.5. Loyalty bridge — auto-enroll and award points if campaign is linked to a loyalty program
    if campaign.auto_enroll_loyalty {
        if let Some(program_id) = campaign.loyalty_program_id {
            let points = campaign.loyalty_points_per_play;
            // Use the loyalty checkin mechanics to process the loyalty enrollment
            let _ = crate::mechanics::loyalty_checkin::process_checkin_from_entry(
                &state,
                &program_id.to_string(),
                &contact_id.to_string(),
                &entry_id.to_string(),
                &body.campaign_slug,
                points,
            ).await;
            // Best-effort: don't fail the entry if loyalty checkin fails
        }
    }

    // 8. If winning outcome and auto-email configured, trigger prize email via n8n
    //    Do this BEFORE consuming contact fields in the delivery payload.
    let is_win = outcome == "winner" || outcome == "grand_prize";
    if is_win && campaign.config.get("email_prize").and_then(|v| v.as_bool()).unwrap_or(false) {
        let email_payload = json!({
            "event": "prize.won",
            "contact": {
                "first_name": body.contact.first_name.as_deref(),
                "last_name": body.contact.last_name.as_deref(),
                "email": body.contact.email.as_deref(),
                "phone": body.contact.phone.as_deref(),
            },
            "campaign": {
                "name": campaign.name,
                "type": campaign.r#type,
                "tag_namespace": campaign.tag_namespace,
            },
            "prize": campaign.config.get("prize_name"),
            "entry_id": entry_id.to_string(),
            "captured_at": chrono::Utc::now().to_rfc3339(),
        });

        let n8n_url = format!("{}/api/prize-email", state.config.workflowswift_url.trim_end_matches('/'));
        let _ = state.http_client
            .post(&n8n_url)
            .json(&email_payload)
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await;
        // Best-effort: don't fail the entry if email fails
    }

    // 9. Build delivery payload from normalized Q&A
    let qa_pairs = if let Some(ref answers) = body.answers {
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
            website: body.contact.website,
            business_name: body.contact.business_name,
        },
        crate::delivery::payload::CampaignPayload {
            name: campaign.name.clone(),
            campaign_type: campaign.r#type.clone(),
            tag_namespace: campaign.tag_namespace.clone(),
        },
        outcome.clone(),
        tags_applied,
        body.score,
        qa_pairs,
        entry_id.to_string(),
    );

    // 10. Execute campaign integrations — pushes to WorkflowSwift
    //     WorkflowSwift handles routing to API targets using stored keys
    dispatch_integrations(
        &state.http_client,
        &state.config.workflowswift_url,
        &campaign.delivery_config,
        &payload,
        &state.db,
        &entry_id,
    ).await?;

    // 11. Return result
    Ok(Json(json!({
        "entry_id": entry_id,
        "contact_id": contact_id,
        "outcome": payload.outcome,
        "tags_applied": payload.tags_applied,
    })))
}

/// Dispatch to all integrations configured in a campaign's delivery_config.
///
/// The primary dispatch route is to WorkflowSwift's incoming webhook, which
/// handles all routing using stored API keys, workflow steps, and n8n triggers.
/// Users configure everything in WorkflowSwift — this is the hands-off layer.
///
/// For backwards compatibility, if `integrations` is not empty, those direct
/// integrations will also be dispatched (legacy path). New campaigns should
/// configure everything via WorkflowSwift and leave `integrations` empty.
pub(crate) async fn dispatch_integrations(
    client: &reqwest::Client,
    workflowswift_url: &str,
    delivery_config: &serde_json::Value,
    payload: &DeliveryPayload,
    db: &sqlx::PgPool,
    entry_id: &Uuid,
) -> Result<(), AppError> {
    // PRIMARY: push to WorkflowSwift for orchestrated routing
    // This is the recommended path — WorkflowSwift handles all integrations
    crate::delivery::coreswift::push_to_workflowswift(client, workflowswift_url, payload).await?;

    // LEGACY: also do any direct integrations specified in the campaign config
    // These are kept for backwards compat with existing campaigns
    if let Some(integrations) = delivery_config.get("integrations").and_then(|v| v.as_array()) {
        for integration in integrations {
            let int_type = integration.get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let int_config = integration.get("config")
                .cloned()
                .unwrap_or_else(|| json!({}));

            match int_type {
                "core_swift" => {
                    // Already handled by WorkflowSwift, skip
                }
                "mailchimp" => {
                    let _api_key = int_config.get("api_key")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let _server_prefix = int_config.get("server_prefix")
                        .and_then(|v| v.as_str())
                        .unwrap_or("us1");
                    let list_id = int_config.get("list_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    // TODO: add Mailchimp direct push module
                    tracing::info!("Mailchimp integration configured for {} — pushing to list {}",
                        payload.contact.email.as_deref().unwrap_or("unknown"), list_id);
                }
                "webhook" => {
                    let url = int_config.get("url")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if !url.is_empty() {
                        webhook::push_to_webhook(client, url, payload, db, entry_id).await?;
                    }
                }
                "hubspot" => {
                    let api_key = int_config.get("api_key")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    crate::delivery::direct_api::hubspot::push_to_hubspot(client, api_key, payload).await?;
                }
                "activecampaign" => {
                    let api_key = int_config.get("api_key")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    crate::delivery::direct_api::activecampaign::push_to_activecampaign(client, api_key, payload).await?;
                }
                "gohighlevel" => {
                    let api_key = int_config.get("api_key")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    crate::delivery::direct_api::gohighlevel::push_to_gohighlevel(client, api_key, payload).await?;
                }
                "n8n" => {
                    let url = int_config.get("url")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if !url.is_empty() {
                        webhook::push_to_webhook(client, url, payload, db, entry_id).await?;
                    }
                }
                _ => {
                    tracing::warn!("Unknown integration type: {}", int_type);
                }
            }
        }
    }

    // Fallback: legacy flat delivery_config pattern
    let delivery_method = delivery_config.get("_method")
        .and_then(|v| v.as_str())
        .unwrap_or("webhook");

    match delivery_method {
        "direct_api" => {
            let api_type = delivery_config.get("api_type")
                .and_then(|v| v.as_str())
                .unwrap_or("webhook");
            let api_key = delivery_config.get("api_key")
                .and_then(|v| v.as_str())
                .unwrap_or("");

            match api_type {
                "hubspot" => {
                    crate::delivery::direct_api::hubspot::push_to_hubspot(client, api_key, payload).await?;
                }
                "activecampaign" => {
                    crate::delivery::direct_api::activecampaign::push_to_activecampaign(client, api_key, payload).await?;
                }
                "gohighlevel" => {
                    crate::delivery::direct_api::gohighlevel::push_to_gohighlevel(client, api_key, payload).await?;
                }
                _ => {
                    let url = delivery_config.get("webhook_url")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if !url.is_empty() {
                        webhook::push_to_webhook(client, url, payload, db, entry_id).await?;
                    }
                }
            }
        }
        _ => {
            let url = delivery_config.get("webhook_url")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if !url.is_empty() {
                webhook::push_to_webhook(client, url, payload, db, entry_id).await?;
            }
        }
    }

    Ok(())
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
