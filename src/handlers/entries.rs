//! Entry handler — the core capture endpoint.

use crate::db::{campaigns, contacts, entries};
use crate::delivery::{payload::ContactPayload, payload::DeliveryPayload, webhook};
use crate::error::AppError;
use crate::state::AppState;
use axum::{extract::State, http::HeaderMap, Json};
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
        .or_else(|| headers.get("x-real-ip").and_then(|v| v.to_str().ok()))
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

    // Play-time gate for the calculator mechanic. Calculator has no dedicated
    // handler — it plays through this generic entry-capture endpoint and evaluates
    // its formula client-side — so gate on the campaign owner's tier here.
    if campaign.r#type == "calculator" {
        crate::access::feature_gate::enforce_mechanic_feature(
            &state,
            &campaign.account_id.to_string(),
            "calculator",
        )
        .await?;
    }

    // 3. Check daily spin limit (before creating entry)
    crate::mechanics::pity_timer::check_daily_limit(
        &state.db,
        &campaign.id,
        &contact_id,
        &campaign.config,
    )
    .await?;

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
    )
    .await?;
    if pity_triggered {
        outcome = pity_outcome;
        tags = pity_tags;
    }

    // 5.5. Auto-add Newsletter tag for directory (b2b_loyalty) campaign entries
    if campaign.r#type == "b2b_loyalty" && !campaign.name.is_empty() {
        let city_newsletter_tag = format!("{} - Newsletter", campaign.name);
        if !tags.contains(&city_newsletter_tag) {
            tags.push(city_newsletter_tag);
        }
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
            )
            .await;
            // Best-effort: don't fail the entry if loyalty checkin fails
        }
    }

    // 8. If winning outcome and auto-email configured, trigger prize email via n8n
    //    Do this BEFORE consuming contact fields in the delivery payload.
    let is_win = outcome == "winner" || outcome == "grand_prize";
    if is_win
        && campaign
            .config
            .get("email_prize")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    {
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

        let n8n_url = format!(
            "{}/api/prize-email",
            state.config.workflowswift_url.trim_end_matches('/')
        );
        let _ = state
            .http_client
            .post(&n8n_url)
            .json(&email_payload)
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await;
        // Best-effort: don't fail the entry if email fails
    }

    // 8.5. Push tags to CoreSwift for directory campaign entries
    if campaign.r#type == "b2b_loyalty" {
        let push_contact_id = contact_id;
        let push_account_id = campaign.account_id;
        let push_tags: Vec<String> = tags_applied.iter().map(|t| t.to_string()).collect();
        let push_added: Vec<String> = tags_applied
            .iter()
            .filter(|t| t.contains(" - Newsletter"))
            .cloned()
            .collect();
        let state_clone = state.clone();
        tokio::spawn(async move {
            crate::delivery::coreswift_push::push_contact_to_coreswift(
                &state_clone,
                &push_contact_id,
                &push_account_id,
                &push_tags,
                &push_added,
                &[],
                "entry",
            )
            .await;
        });
    }

    // 8.6. Execute output actions (webhook, CoreSwift sync, email, SMS)
    let oa_tags: Vec<String> = tags_applied.iter().map(|t| t.to_string()).collect();
    let oa_answers = body.answers.clone();
    let oa_utm_source = body.utm_source.clone();
    let oa_utm_medium = body.utm_medium.clone();
    let oa_utm_campaign = body.utm_campaign.clone();
    let oa_referrer_url = body.referrer_url.clone();
    let oa_page_url = body.page_url.clone();
    let first_name = body.contact.first_name.as_deref().unwrap_or("");
    let last_name = body.contact.last_name.as_deref().unwrap_or("");
    let email = body.contact.email.as_deref().unwrap_or("");
    let phone = body.contact.phone.as_deref().unwrap_or("");
    let website = body.contact.website.as_deref().unwrap_or("");
    let business_name = body.contact.business_name.as_deref().unwrap_or("");

    tokio::spawn({
        let state = state.clone();
        let campaign_id = campaign.id;
        let campaign_name = campaign.name.clone();
        let campaign_slug = campaign.slug.clone();
        let campaign_type = campaign.r#type.clone();
        let campaign_config = campaign.config.clone();
        let contact_id = contact_id;
        let account_id = campaign.account_id;
        let outcome = outcome.clone();
        let tags = oa_tags.clone();
        let score = body.score;
        let answers = oa_answers.clone();
        let utm_source = oa_utm_source.clone();
        let utm_medium = oa_utm_medium.clone();
        let utm_campaign = oa_utm_campaign.clone();
        let referrer_url = oa_referrer_url.clone();
        let page_url = oa_page_url.clone();
        let fn1 = first_name.to_string();
        let ln1 = last_name.to_string();
        let em1 = email.to_string();
        let ph1 = phone.to_string();
        let ws1 = website.to_string();
        let bn1 = business_name.to_string();
        async move {
            crate::delivery::output_actions::execute_output_actions(
                &state,
                &campaign_id,
                &campaign_name,
                &campaign_slug,
                &campaign_type,
                &campaign_config,
                &contact_id,
                &fn1,
                &ln1,
                &em1,
                &ph1,
                &ws1,
                &bn1,
                &account_id,
                &outcome,
                &tags,
                score.map(|s| s as f64),
                answers.as_ref(),
                utm_source.as_deref(),
                utm_medium.as_deref(),
                utm_campaign.as_deref(),
                referrer_url.as_deref(),
                page_url.as_deref(),
            )
            .await;
        }
    });

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
    )
    .await?;

    // 11. Return result
    Ok(Json(json!({
        "entry_id": entry_id,
        "contact_id": contact_id,
        "outcome": payload.outcome,
        "tags_applied": payload.tags_applied,
    })))
}

/// Dispatch to all integrations configured in a campaign's delivery_config.
/// The primary dispatch route is to WorkflowSwift's incoming webhook, which
/// handles all routing using stored API keys, workflow steps, and n8n triggers.
/// Users configure everything in WorkflowSwift — this is the hands-off layer.
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
    if let Some(integrations) = delivery_config
        .get("integrations")
        .and_then(|v| v.as_array())
    {
        for integration in integrations {
            let int_type = integration
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let int_config = integration
                .get("config")
                .cloned()
                .unwrap_or_else(|| json!({}));

            match int_type {
                "core_swift" => {
                    // Already handled by WorkflowSwift, skip
                }
                "mailchimp" => {
                    let _api_key = int_config
                        .get("api_key")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let _server_prefix = int_config
                        .get("server_prefix")
                        .and_then(|v| v.as_str())
                        .unwrap_or("us1");
                    let list_id = int_config
                        .get("list_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    // TODO: add Mailchimp direct push module
                    tracing::info!(
                        "Mailchimp integration configured for {} — pushing to list {}",
                        payload.contact.email.as_deref().unwrap_or("unknown"),
                        list_id
                    );
                }
                "webhook" => {
                    let url = int_config.get("url").and_then(|v| v.as_str()).unwrap_or("");
                    if !url.is_empty() {
                        webhook::push_to_webhook(client, url, payload, db, entry_id).await?;
                    }
                }
                "hubspot" => {
                    let api_key = int_config
                        .get("api_key")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    crate::delivery::direct_api::hubspot::push_to_hubspot(client, api_key, payload)
                        .await?;
                }
                "activecampaign" => {
                    let api_key = int_config
                        .get("api_key")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    crate::delivery::direct_api::activecampaign::push_to_activecampaign(
                        client, api_key, payload,
                    )
                    .await?;
                }
                "gohighlevel" => {
                    let api_key = int_config
                        .get("api_key")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    crate::delivery::direct_api::gohighlevel::push_to_gohighlevel(
                        client, api_key, payload,
                    )
                    .await?;
                }
                "n8n" => {
                    let url = int_config.get("url").and_then(|v| v.as_str()).unwrap_or("");
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
    let delivery_method = delivery_config
        .get("_method")
        .and_then(|v| v.as_str())
        .unwrap_or("webhook");

    match delivery_method {
        "direct_api" => {
            let api_type = delivery_config
                .get("api_type")
                .and_then(|v| v.as_str())
                .unwrap_or("webhook");
            let api_key = delivery_config
                .get("api_key")
                .and_then(|v| v.as_str())
                .unwrap_or("");

            match api_type {
                "hubspot" => {
                    crate::delivery::direct_api::hubspot::push_to_hubspot(client, api_key, payload)
                        .await?;
                }
                "activecampaign" => {
                    crate::delivery::direct_api::activecampaign::push_to_activecampaign(
                        client, api_key, payload,
                    )
                    .await?;
                }
                "gohighlevel" => {
                    crate::delivery::direct_api::gohighlevel::push_to_gohighlevel(
                        client, api_key, payload,
                    )
                    .await?;
                }
                _ => {
                    let url = delivery_config
                        .get("webhook_url")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if !url.is_empty() {
                        webhook::push_to_webhook(client, url, payload, db, entry_id).await?;
                    }
                }
            }
        }
        _ => {
            let url = delivery_config
                .get("webhook_url")
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
fn determine_outcome(
    campaign: &crate::db::campaigns::Campaign,
    score: Option<i32>,
) -> (String, Vec<String>) {
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
    if let Some(threshold) = outcome_tags
        .get("winner_threshold")
        .and_then(|v| v.as_i64())
    {
        if score >= threshold as i32 {
            let tag = outcome_tags
                .get("winner")
                .and_then(|v| v.as_str())
                .unwrap_or(&format!("{}_winner", tag_namespace))
                .to_string();
            return ("winner".to_string(), vec![tag]);
        }
    }

    if let Some(threshold) = outcome_tags
        .get("runner_up_threshold")
        .and_then(|v| v.as_i64())
    {
        if score >= threshold as i32 {
            let tag = outcome_tags
                .get("runner_up")
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
fn extract_qa_from_jsonb(
    answers: &Value,
    _questions: &[crate::db::questions_answers::QuestionAnswerPair],
) -> Vec<crate::delivery::payload::QuestionAnswerPair> {
    let mut pairs = vec![];

    if let Some(obj) = answers.as_object() {
        for (key, value) in obj {
            let question_text = key.clone();
            let answer_text = match value {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => b.to_string(),
                Value::Array(arr) => arr
                    .iter()
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

/// POST /api/v1/campaigns/test-webhook — Send a test webhook POST to a URL
pub async fn test_entry_webhook(Json(body): Json<Value>) -> Result<Json<Value>, AppError> {
    let webhook_url = body
        .get("webhook_url")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AppError::BadRequest("webhook_url is required".to_string()))?;

    let contact = body.get("contact").cloned().unwrap_or_else(
        || json!({"first_name":"Test","last_name":"User","email":"test@example.com"}),
    );

    let payload = json!({
        "event": "entry.created",
        "test": true,
        "entry_id": "00000000-0000-0000-0000-000000000000",
        "campaign": {
            "id": "00000000-0000-0000-0000-000000000000",
            "name": "Test Campaign",
            "slug": "test-campaign",
            "type": "spin_wheel",
        },
        "contact": contact,
        "outcome": "winner",
        "tags": ["test"],
        "score": null,
        "answers": {"test_question": "test_answer"},
        "source": {
            "utm_source": "test",
            "utm_medium": null,
            "utm_campaign": null,
            "referrer_url": "https://test.com",
            "page_url": "https://test.com/campaign",
        },
        "timestamp": chrono::Utc::now().to_rfc3339(),
    });

    let client = reqwest::Client::new();
    let result = client
        .post(webhook_url)
        .json(&payload)
        .timeout(std::time::Duration::from_secs(10))
        .header("User-Agent", "IncentiveSwift-EntryWebhook/1.0")
        .send()
        .await;

    match result {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let body_text = resp
                .text()
                .await
                .unwrap_or_default()
                .chars()
                .take(500)
                .collect::<String>();
            Ok(Json(json!({
                "success": (200..300).contains(&status),
                "status": status,
                "response": body_text,
            })))
        }
        Err(e) => Ok(Json(json!({
            "success": false,
            "error": e.to_string(),
        }))),
    }
}
