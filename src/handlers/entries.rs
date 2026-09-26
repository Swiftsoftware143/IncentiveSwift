//! Entry handler — the core capture endpoint.

use crate::db::{campaigns, contacts, entries};
use crate::delivery::{payload::ContactPayload, payload::DeliveryPayload, webhook};
use crate::error::AppError;
use crate::security::auth::AuthenticatedUser;
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

/// POST /api/v1/entries — create entry (public). Bounded by the campaign's daily entry cap
/// (`check_daily_limit` below) and, at the edge, by nginx's per-visitor `limit_req` on `/api/`.
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

    // 5.5. Loyalty campaigns: tags are driven by campaign config (tag_namespace),
    // NOT hardcoded business-specific assumptions (no forced "Newsletter" tag).

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

    // 7.6. Lifecycle email trigger (stage 1 immediate + stage 3 scheduled 24h).
    // Industry-standard dedupe: once per campaign per email. Best-effort, non-blocking.
    if let Some(ref email) = body.contact.email {
        if !email.trim().is_empty() {
            let already = crate::lifecycle_emails::already_emailed(
                &state.db,
                campaign.account_id,
                email.trim(),
                &campaign.r#type,
            )
            .await;
            if !already {
                let state_clone = state.clone();
                let acct = campaign.account_id;
                let em = email.trim().to_string();
                let ctype = campaign.r#type.clone();
                let cname = campaign.name.clone();
                let fname = body.contact.first_name.clone();
                let lname = body.contact.last_name.clone();
                tokio::spawn(async move {
                    crate::lifecycle_emails::trigger_entry_lifecycle(
                        &state_clone,
                        acct,
                        &em,
                        &ctype,
                        &cname,
                        fname.as_deref(),
                        lname.as_deref(),
                    )
                    .await;
                });
            }
        }
    }

    // 7.8. CoreSwift external push (per-user connection + per-campaign list + field mapping).
    //      Best-effort: fire-and-forget — never fails the entry.
    {
        let state_clone = state.clone();
        let push_contact_id = contact_id;
        let push_campaign_id = campaign.id;
        let push_entry_id = entry_id;
        let push_tags: Vec<String> = tags_applied.iter().map(|t| t.to_string()).collect();
        tokio::spawn(async move {
            crate::delivery::coreswift_external::push_entry_to_coreswift(
                &state_clone,
                &push_contact_id,
                &push_campaign_id,
                &push_entry_id,
                &push_tags,
            )
            .await;
        });
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
        let to_email = body.contact.email.as_deref().unwrap_or("");
        if !to_email.is_empty() {
            // Per-account SMTP + DB template (fallback template_type = winner)
            let template_type = format!("{}_winner", campaign.r#type);
            let vars = json!({
                "first_name": body.contact.first_name.as_deref().unwrap_or(""),
                "last_name": body.contact.last_name.as_deref().unwrap_or(""),
                "email": to_email,
                "phone": body.contact.phone.as_deref().unwrap_or(""),
                "campaign_name": campaign.name,
                "campaign_type": campaign.r#type,
                "prize_name": campaign.config.get("prize_name").and_then(|v| v.as_str()).unwrap_or(""),
                "entry_id": entry_id.to_string(),
            });
            // Try per-type template first, then generic "winner"
            let mut res = crate::delivery::sender::send_template_by_type(
                &state.db,
                campaign.account_id,
                to_email,
                &template_type,
                &vars,
            )
            .await;
            if res.is_err() {
                res = crate::delivery::sender::send_template_by_type(
                    &state.db,
                    campaign.account_id,
                    to_email,
                    "winner",
                    &vars,
                )
                .await;
            }
            if let Err(e) = res {
                tracing::warn!("Prize email send failed: {e}");
            }
            // Best-effort: don't fail the entry if email fails
        }
    }

    // 8.5. (removed) The legacy X-Internal-Key/tag-sync push lived here. There is now
    //      exactly ONE CoreSwift path: step 7.8 above (coreswift_external, per-tenant
    //      BYOK key -> hub /api/external/contacts), which already carries tags_applied.

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

    // 10. Execute DIRECT campaign integrations (webhook/Mailchimp/HubSpot/etc)
    dispatch_integrations(
        &state.http_client,
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

/// Dispatch to all DIRECT integrations configured in a campaign's delivery_config
/// (webhook, Mailchimp, HubSpot, ActiveCampaign, GoHighLevel, n8n). CoreSwift's
/// per-user/per-campaign push is handled separately in `create_entry` step 7.8.
///
/// NOTE: IncentiveSwift never routes outbound data through WorkflowSwift here —
/// WorkflowSwift only receives data that originates within WorkflowSwift itself.
pub(crate) async fn dispatch_integrations(
    client: &reqwest::Client,
    delivery_config: &serde_json::Value,
    payload: &DeliveryPayload,
    db: &sqlx::PgPool,
    entry_id: &Uuid,
) -> Result<(), AppError> {
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
                    // Handled by the per-user CoreSwift push in create_entry step 7.8
                    // (provider_keys + delivery_config.coreswift.list_id + field mapping).
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

/// POST /api/v1/campaigns/test-webhook — fire a sample entry payload at the CALLER'S OWN campaign
/// webhook and report the delivery status.
///
/// SECURITY (kanban t_016c839c). This route used to be ANONYMOUS, POSTed to a CALLER-SUPPLIED URL
/// and returned up to 500 bytes of the target's response body — an outbound-request + read
/// primitive any unauthenticated caller could aim at 169.254.169.254, `127.0.0.1:8083` itself, the
/// docker network, n8n or the postgres port. It now:
///   * requires `AuthenticatedUser` (anon is 401),
///   * takes only a `campaign_id` and sends to that campaign's OWN `config.entry_webhook_url` —
///     the same field `delivery::entry_webhook::fire_entry_webhook` reads in production — resolved
///     from a campaign the caller's account owns (another tenant's campaign answers 404),
///   * passes the platform's outbound-webhook gate (`security::webhook_security`), which refuses
///     loopback/link-local/private addresses after DNS resolution and enforces the matching
///     integration target's domain allowlist and daily cap,
///   * follows no redirects (a validated host that 302s to 169.254.169.254 is not a delivery),
///   * returns the STATUS only, never the target's body.
pub async fn test_entry_webhook(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<Value>,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::Unauthorized("Authenticated account is not a uuid".to_string()))?;

    let campaign_id = body
        .get("campaign_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AppError::BadRequest("campaign_id is required".to_string()))?;
    let campaign_id = Uuid::parse_str(campaign_id)
        .map_err(|_| AppError::BadRequest("campaign_id must be a uuid".to_string()))?;

    // Tenant scope: the caller may only test a campaign its own account owns, so another tenant's
    // campaign id is a 404 (never a delivery) and the destination can never be attacker-chosen.
    let campaign: Option<(String, String, Value)> = sqlx::query_as(
        "SELECT name, slug, COALESCE(config, '{}'::jsonb) FROM campaigns \
         WHERE id = $1 AND account_id = $2",
    )
    .bind(campaign_id)
    .bind(account_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;

    let Some((name, slug, config)) = campaign else {
        return Err(AppError::NotFound("Campaign not found".to_string()));
    };

    let webhook_url = config
        .get("entry_webhook_url")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            AppError::BadRequest(
                "This campaign has no entry_webhook_url in its config — set one in the campaign \
                 editor before testing"
                    .to_string(),
            )
        })?;

    // The platform's outbound-webhook gate: every delivery must pass it, and this route was the
    // one place that skipped it. A matching integration target also brings its own allowlist and
    // daily cap along.
    let target: Option<(Uuid, Vec<String>, i32)> = sqlx::query_as(
        "SELECT id, COALESCE(allowed_domains, '{}'), daily_limit FROM integration_targets \
         WHERE account_id = $1 AND webhook_url = $2 AND is_active = true LIMIT 1",
    )
    .bind(account_id)
    .bind(webhook_url)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;

    match target {
        Some((target_id, allowed_domains, daily_limit)) => {
            crate::security::webhook_security::check_webhook_security(
                &state.db,
                &target_id,
                webhook_url,
                &allowed_domains,
                daily_limit,
            )
            .await?
        }
        None => crate::security::webhook_security::validate_webhook_url(webhook_url, &[])
            .await
            .map_err(|msg| {
                AppError::Forbidden(format!("Webhook blocked by security policy: {}", msg))
            })?,
    }

    let contact = body.get("contact").cloned().unwrap_or_else(
        || json!({"first_name":"Test","last_name":"User","email":"test@example.com"}),
    );

    // No entry exists for a test, so `entry_id` is the nil UUID built at run time (it names no
    // row). The campaign block carries the REAL campaign being tested.
    let sample_id = Uuid::nil().to_string();
    let payload = json!({
        "event": "entry.created",
        "test": true,
        "entry_id": sample_id,
        "campaign": {
            "id": campaign_id.to_string(),
            "name": name,
            "slug": slug,
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

    // Redirects are NOT followed: the gate validated this host, and a 30x would otherwise be a
    // free hop to an address the gate exists to refuse.
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent("IncentiveSwift-EntryWebhook/1.0")
        .build()
        .map_err(|e| AppError::Internal(format!("http client: {}", e)))?;

    let result = client.post(webhook_url).json(&payload).send().await;

    match result {
        Ok(resp) => {
            let status = resp.status().as_u16();
            // The status only. The target's body is NOT echoed back to the caller.
            Ok(Json(json!({
                "success": (200..300).contains(&status),
                "status": status,
            })))
        }
        Err(e) => Ok(Json(json!({
            "success": false,
            "error": e.to_string().chars().take(200).collect::<String>(),
        }))),
    }
}
