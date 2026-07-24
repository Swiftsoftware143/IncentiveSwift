// Entry webhook — fires POST to campaign's configured entry_webhook_url
// when a user submits an entry (spin, form, quiz, etc.)

use reqwest::Client;
use serde_json::{json, Value};
use uuid::Uuid;

/// Fire entry webhook if campaign has entry_webhook_url configured in config.
/// Best-effort: does not fail the entry if webhook fails.
pub async fn fire_entry_webhook(
    http_client: &Client,
    campaign_config: &Value,
    campaign_id: &Uuid,
    campaign_name: &str,
    campaign_slug: &str,
    campaign_type: &str,
    contact: &Value,
    entry_id: &Uuid,
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
    let webhook_url = campaign_config
        .get("entry_webhook_url")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let Some(url) = webhook_url else {
        return; // No webhook configured
    };

    let payload = json!({
        "event": "entry.created",
        "entry_id": entry_id.to_string(),
        "campaign": {
            "id": campaign_id.to_string(),
            "name": campaign_name,
            "slug": campaign_slug,
            "type": campaign_type,
        },
        "contact": contact,
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
    });

    let _ = http_client
        .post(&url)
        .json(&payload)
        .timeout(std::time::Duration::from_secs(10))
        .header("User-Agent", "IncentiveSwift-EntryWebhook/1.0")
        .send()
        .await;
}
