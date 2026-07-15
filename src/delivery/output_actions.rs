// Output Action Engine — Executes configured actions on campaign entry
//
// Each campaign has an "output_actions" JSONB array in its config field.
// Each action has:
//   - action_type: "webhook" | "coreswift_contact" | "coreswift_list" | "coreswift_tag" | "email" | "sms"
//   - config: JSON object with type-specific fields
//   - enabled: bool
//   - name: string (for display)
//
// On every entry, the Output Engine iterates the list and dispatches each
// enabled action.

use crate::state::AppState;
use reqwest::Client;
use serde_json::{json, Value};
use uuid::Uuid;

/// Fire all output actions configured on a campaign for this entry.
/// Runs in background — does not block the entry response.
#[allow(clippy::too_many_arguments)]
pub async fn execute_output_actions(
    state: &AppState,
    campaign_id: &Uuid,
    campaign_name: &str,
    campaign_slug: &str,
    campaign_type: &str,
    campaign_config: &Value,
    contact_id: &Uuid,
    contact_first_name: &str,
    contact_last_name: &str,
    contact_email: &str,
    contact_phone: &str,
    contact_website: &str,
    contact_business_name: &str,
    account_id: &Uuid,
    outcome: &str,
    tags: &[String],
    score: Option<f64>,
    answers: Option<&Value>,
    utm_source: Option<&str>,
    utm_medium: Option<&str>,
    utm_campaign: Option<&str>,
    referrer_url: Option<&str>,
    page_url: Option<&str>,
) {
    // Read output_actions from campaign config
    let actions = campaign_config
        .get("output_actions")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    if actions.is_empty() {
        // Fall back to legacy config: check for entry_webhook_url
        let webhook_url = campaign_config
            .get("entry_webhook_url")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if !webhook_url.is_empty() {
            let payload = build_entry_payload(
                campaign_name, campaign_slug, campaign_type,
                contact_first_name, contact_last_name, contact_email, contact_phone,
                contact_website, contact_business_name,
                outcome, tags, score, answers,
                utm_source, utm_medium, utm_campaign, referrer_url, page_url,
            );
            let _ = fire_webhook(&state.http_client, webhook_url, &payload).await;
        }

        // Also fire legacy CoreSwift sync
        let _ = crate::delivery::coreswift_sync::sync_entry_to_coreswift(
            state, campaign_id, campaign_name, campaign_slug,
            contact_id,
            &Some(contact_first_name.to_string()),
            &Some(contact_last_name.to_string()),
            &Some(contact_email.to_string()),
            &Some(contact_phone.to_string()),
            &Some(contact_website.to_string()),
            &Some(contact_business_name.to_string()),
            account_id, outcome, answers, utm_source,
        ).await;

        return;
    }

    // Execute each action
    for action in &actions {
        let enabled = action.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
        if !enabled { continue; }

        let action_type = action.get("action_type").and_then(|v| v.as_str()).unwrap_or("");
        let action_config = action.get("config").and_then(|v| v.as_object()).cloned().unwrap_or_default();
        let action_config_val = action.get("config").cloned().unwrap_or_default();

        let payload = build_entry_payload(
            campaign_name, campaign_slug, campaign_type,
            contact_first_name, contact_last_name, contact_email, contact_phone,
            contact_website, contact_business_name,
            outcome, tags, score, answers,
            utm_source, utm_medium, utm_campaign, referrer_url, page_url,
        );

        match action_type {
            "webhook" => {
                let url = action_config.get("url").and_then(|v| v.as_str()).unwrap_or("");
                if !url.is_empty() {
                    let method = action_config.get("method").and_then(|v| v.as_str()).unwrap_or("POST");
                    fire_webhook_with_method(&state.http_client, url, method, &payload).await;
                }
            }
            "coreswift_contact" => {
                let _ = crate::delivery::coreswift_sync::sync_entry_to_coreswift(
                    state, campaign_id, campaign_name, campaign_slug,
                    contact_id,
                    &Some(contact_first_name.to_string()),
                    &Some(contact_last_name.to_string()),
                    &Some(contact_email.to_string()),
                    &Some(contact_phone.to_string()),
                    &Some(contact_website.to_string()),
                    &Some(contact_business_name.to_string()),
                    account_id, outcome, answers, utm_source,
                ).await;

                // Also add to list if configured
                let list_id = action_config.get("list_id").and_then(|v| v.as_str()).unwrap_or("");
                if !list_id.is_empty() {
                    add_contact_to_coreswift_list(state, account_id, contact_id, list_id).await;
                }

                // Also apply tag if configured
                let tag_name = action_config.get("tag").and_then(|v| v.as_str()).unwrap_or("");
                if !tag_name.is_empty() {
                    apply_tag_to_coreswift_contact(state, account_id, contact_id, tag_name).await;
                }
            }
            "email" => {
                let subject = action_config.get("subject").and_then(|v| v.as_str()).unwrap_or("New campaign entry");
                let body = action_config.get("body").and_then(|v| v.as_str()).unwrap_or("A new entry was received.");
                let to = action_config.get("to").and_then(|v| v.as_str()).unwrap_or(contact_email);
                let from_name = action_config.get("from_name").and_then(|v| v.as_str()).unwrap_or("IncentiveSwift");

                // Render body with template variables
                let rendered_body = render_template(body, &payload);
                let rendered_subject = render_template(subject, &payload);

                let _ = crate::delivery::sender::send_email(
                    &state.db,
                    *account_id,
                    to,
                    &rendered_subject,
                    &rendered_body,
                ).await;
            }
            "sms" => {
                let message = action_config.get("message").and_then(|v| v.as_str()).unwrap_or("You have a new campaign entry!");
                let phone = action_config.get("to").and_then(|v| v.as_str()).unwrap_or(contact_phone);
                if !phone.is_empty() {
                    let rendered = render_template(message, &payload);
                    let _ = send_sms_via_telnyx(state, account_id, phone, &rendered).await;
                }
            }
            "n8n" => {
                let webhook_url = action_config.get("webhook_url").and_then(|v| v.as_str()).unwrap_or("");
                if !webhook_url.is_empty() {
                    let mut req = state.http_client.post(webhook_url).json(&payload);
                    if let Some(api_key) = action_config.get("n8n_api_key").and_then(|v| v.as_str()).filter(|k| !k.is_empty()) {
                        req = req.header("X-API-Key", api_key);
                    }
                    let _ = req
                        .header("User-Agent", "IncentiveSwift-n8nOutputAction/1.0")
                        .timeout(std::time::Duration::from_secs(15))
                        .send()
                        .await;
                }
            }
            _ => {
                tracing::warn!("Unknown output action type: {}", action_type);
            }
        }
    }
}

/// Build the entry payload dict for use in templates
fn build_entry_payload<'a>(
    campaign_name: &'a str,
    campaign_slug: &'a str,
    campaign_type: &'a str,
    contact_first_name: &'a str,
    contact_last_name: &'a str,
    contact_email: &'a str,
    contact_phone: &'a str,
    contact_website: &'a str,
    contact_business_name: &'a str,
    outcome: &'a str,
    tags: &'a [String],
    score: Option<f64>,
    answers: Option<&'a Value>,
    utm_source: Option<&'a str>,
    utm_medium: Option<&'a str>,
    utm_campaign: Option<&'a str>,
    referrer_url: Option<&'a str>,
    page_url: Option<&'a str>,
) -> Value {
    json!({
        "campaign": {
            "name": campaign_name,
            "slug": campaign_slug,
            "type": campaign_type,
        },
        "contact": {
            "first_name": contact_first_name,
            "last_name": contact_last_name,
            "email": contact_email,
            "phone": contact_phone,
            "website": contact_website,
            "business_name": contact_business_name,
        },
        "outcome": outcome,
        "tags": tags,
        "score": score,
        "answers": answers,
        "source": {
            "utm_source": utm_source,
            "utm_medium": utm_medium,
            "utm_campaign": utm_campaign,
            "referrer_url": referrer_url,
            "page_url": page_url,
        },
        "timestamp": chrono::Utc::now().to_rfc3339(),
    })
}

/// Simple template rendering: replace {{key}} with values from payload
fn render_template(template: &str, payload: &Value) -> String {
    let mut result = template.to_string();

    // Flatten the payload for easy access: {{ campaign.name }}, {{ contact.email }}, etc.
    let flat = flatten_json(payload, "");

    for (key, val) in &flat {
        let placeholder = format!("{{{{{}}}}}", key);
        result = result.replace(&placeholder, val);
    }

    result
}

/// Flatten JSON to dot-notation key:value pairs
fn flatten_json(value: &Value, prefix: &str) -> Vec<(String, String)> {
    let mut result = Vec::new();
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                let key = if prefix.is_empty() { k.clone() } else { format!("{}.{}", prefix, k) };
                result.extend(flatten_json(v, &key));
            }
        }
        Value::String(s) => {
            result.push((prefix.to_string(), s.clone()));
        }
        Value::Number(n) => {
            result.push((prefix.to_string(), n.to_string()));
        }
        Value::Bool(b) => {
            result.push((prefix.to_string(), b.to_string()));
        }
        Value::Array(arr) => {
            let items: Vec<String> = arr.iter().map(|v| {
                match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                }
            }).collect();
            result.push((prefix.to_string(), items.join(", ")));
        }
        Value::Null => {
            result.push((prefix.to_string(), String::new()));
        }
    }
    result
}

/// Fire a simple POST webhook with entry payload
async fn fire_webhook(client: &Client, url: &str, payload: &Value) -> Result<(), String> {
    let resp = client
        .post(url)
        .json(payload)
        .timeout(std::time::Duration::from_secs(15))
        .header("User-Agent", "IncentiveSwift-OutputAction/1.0")
        .send()
        .await
        .map_err(|e| format!("Webhook request failed: {}", e))?;

    let status = resp.status().as_u16();
    tracing::debug!("Webhook {} returned status {}", url, status);
    Ok(())
}

/// Fire webhook with configurable method
async fn fire_webhook_with_method(client: &Client, url: &str, method: &str, payload: &Value) {
    let req = match method {
        "GET" => client.get(url),
        _ => client.post(url).json(payload),
    };

    let resp = req
        .timeout(std::time::Duration::from_secs(15))
        .header("User-Agent", "IncentiveSwift-OutputAction/1.0")
        .send()
        .await;

    match resp {
        Ok(r) => tracing::debug!("Webhook {} returned {}", url, r.status()),
        Err(e) => tracing::warn!("Webhook {} failed: {}", url, e),
    }
}

/// Add a contact to a CoreSwift list via the API
async fn add_contact_to_coreswift_list(
    state: &AppState,
    account_id: &Uuid,
    contact_id: &Uuid,
    list_id: &str,
) {
    // Get CoreSwift credentials
    let creds = sqlx::query_as::<_, (String, Option<String>)>(
        r#"SELECT api_key, base_url FROM provider_keys
           WHERE provider = 'coreswift' AND account_id = $1 AND is_active = true
           LIMIT 1"#
    )
    .bind(account_id)
    .fetch_optional(&state.db)
    .await;

    let Ok(Some((jwt, base_url))) = creds else { return; };
    if jwt.is_empty() { return; }

    let base = base_url.unwrap_or_else(|| "https://coreswiftcrm.com".to_string());

    let resp = state.http_client
        .post(format!("{}/api/lists/{}/members", base.trim_end_matches('/'), list_id))
        .header("Authorization", format!("Bearer {}", jwt))
        .json(&json!({"contact_id": contact_id}))
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await;

    match resp {
        Ok(r) => tracing::info!("CoreSwift list add: {} (status={})", list_id, r.status()),
        Err(e) => tracing::warn!("CoreSwift list add failed: {}", e),
    }
}

/// Apply a tag to a CoreSwift contact
/// Note: CoreSwift tags contacts via PATCH on the contact itself or a dedicated tag endpoint.
/// We assume a simple approach: tags are appended to contact notes.
async fn apply_tag_to_coreswift_contact(
    state: &AppState,
    account_id: &Uuid,
    contact_id: &Uuid,
    tag_name: &str,
) {
    let creds = sqlx::query_as::<_, (String, Option<String>)>(
        r#"SELECT api_key, base_url FROM provider_keys
           WHERE provider = 'coreswift' AND account_id = $1 AND is_active = true
           LIMIT 1"#
    )
    .bind(account_id)
    .fetch_optional(&state.db)
    .await;

    let Ok(Some((jwt, base_url))) = creds else { return; };
    if jwt.is_empty() { return; }

    let base = base_url.unwrap_or_else(|| "https://coreswiftcrm.com".to_string());

    // Try: PATCH /api/contacts/:id with tag in metadata
    let resp = state.http_client
        .patch(format!("{}/api/contacts/{}", base.trim_end_matches('/'), contact_id))
        .header("Authorization", format!("Bearer {}", jwt))
        .json(&json!({"tags": [tag_name]}))
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await;

    match resp {
        Ok(r) => tracing::info!("CoreSwift tag '{}' applied to contact {} (status={})", tag_name, contact_id, r.status()),
        Err(e) => tracing::warn!("CoreSwift tag apply failed: {}", e),
    }
}

/// Send an SMS via Telnyx (reuses the existing SMS handler logic)
async fn send_sms_via_telnyx(
    state: &AppState,
    account_id: &Uuid,
    to: &str,
    message: &str,
) {
    // Get Telnyx credentials
    let creds = sqlx::query_as::<_, (String, Option<String>, Option<Value>)>(
        r#"SELECT api_key, base_url, metadata
           FROM provider_keys
           WHERE provider = 'telnyx_sms' AND account_id = $1 AND is_active = true
           LIMIT 1"#
    )
    .bind(account_id)
    .fetch_optional(&state.db)
    .await;

    let Ok(Some((api_key, _base_url, meta))) = creds else { return; };
    if api_key.is_empty() { return; }

    // Get the from number from metadata or settings
    let from_number = meta.as_ref()
        .and_then(|m| m.get("from_number"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if from_number.is_empty() { return; }

    let telnyx_payload = json!({
        "from": from_number,
        "to": to,
        "text": message,
    });

    let resp = state.http_client
        .post("https://api.telnyx.com/v2/messages")
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&telnyx_payload)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await;

    match resp {
        Ok(r) => tracing::info!("Telnyx SMS sent to {} (status={})", to, r.status()),
        Err(e) => tracing::warn!("Telnyx SMS failed: {}", e),
    }
}
