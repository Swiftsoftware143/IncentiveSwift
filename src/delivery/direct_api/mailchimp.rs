//! Mailchimp direct API delivery — upsert one list member per entry.
//!
//! Until this module existed, `entries::dispatch_integrations` answered a `"mailchimp"` integration
//! with `tracing::info!("… pushing to list {list_id}")` and **made no request at all**: the operator
//! configured a Mailchimp list, the log said it was pushed, and nothing ever arrived. A log line is
//! not a delivery, so the arm now performs the real call and records the outcome in `delivery_log`
//! (the same table `push_to_webhook` writes) — a failure is visible in the console instead of being
//! reported as success.
//!
//! Mailchimp addresses a member by the MD5 of its lower-cased address, and an upsert is
//! `PUT /3.0/lists/{list_id}/members/{subscriber_hash}` (create-or-update in one call, so a repeat
//! customer does not produce a `400 Member Exists`). Only FNAME / LNAME / PHONE are written: those
//! are the three merge tags every Mailchimp audience ships with, so a real list cannot reject the
//! body for naming a merge field it does not have.

use md5::{Digest, Md5};
use uuid::Uuid;

use crate::delivery::payload::DeliveryPayload;
use crate::error::AppError;

/// Mailchimp encodes the datacentre in the API key's suffix (`abc123-us14`). A key with no usable
/// suffix cannot address one, so it falls back to `us1` — and because the suffix is authoritative,
/// a key minted for another datacentre still reaches the right host instead of being sent to `us1`
/// and refused with "the API key provided is linked to a different datacenter".
const DEFAULT_SERVER_PREFIX: &str = "us1";

/// The datacentre prefix the key names, or the default when it names none.
pub fn server_prefix(api_key: &str) -> &str {
    match api_key.rsplit_once('-') {
        Some((_, dc)) if dc.len() >= 3 && dc.starts_with("us") => dc,
        _ => DEFAULT_SERVER_PREFIX,
    }
}

/// Mailchimp's member id: the lower-cased address, MD5-hex.
pub fn subscriber_hash(email: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(email.trim().to_ascii_lowercase().as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The member body. `status_if_new` only applies when the member does not exist yet; an existing
/// subscriber keeps its current status, so a push never silently re-subscribes someone who opted out
/// of the audience through Mailchimp itself.
pub fn member_body(payload: &DeliveryPayload) -> serde_json::Value {
    let mut merge = serde_json::Map::new();
    if let Some(v) = payload
        .contact
        .first_name
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        merge.insert("FNAME".into(), serde_json::Value::String(v.to_string()));
    }
    if let Some(v) = payload
        .contact
        .last_name
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        merge.insert("LNAME".into(), serde_json::Value::String(v.to_string()));
    }
    if let Some(v) = payload
        .contact
        .phone
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        merge.insert("PHONE".into(), serde_json::Value::String(v.to_string()));
    }
    serde_json::json!({
        "email_address": payload.contact.email.as_deref().unwrap_or(""),
        "status_if_new": "subscribed",
        "merge_fields": serde_json::Value::Object(merge),
    })
}

/// Push the entry's contact to a Mailchimp audience. `server_prefix` overrides the datacentre the
/// key names (the console's field is optional; the key's suffix is the default).
pub async fn push_to_mailchimp(
    client: &reqwest::Client,
    pool: &sqlx::PgPool,
    entry_id: &Uuid,
    api_key: &str,
    server_prefix_override: Option<&str>,
    list_id: &str,
    payload: &DeliveryPayload,
) -> Result<(), AppError> {
    let email = payload
        .contact
        .email
        .as_deref()
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .ok_or_else(|| {
            AppError::BadRequest("Mailchimp: the entry has no contact email to push".into())
        })?;
    if api_key.trim().is_empty() {
        return Err(AppError::BadRequest(
            "Mailchimp: no API key is configured for this integration".into(),
        ));
    }
    if list_id.trim().is_empty() {
        return Err(AppError::BadRequest(
            "Mailchimp: no audience (list) id is configured for this integration".into(),
        ));
    }

    let dc = match server_prefix_override
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(explicit) => explicit.to_string(),
        None => server_prefix(api_key).to_string(),
    };
    let url = format!(
        "https://{dc}.api.mailchimp.com/3.0/lists/{}/members/{}",
        list_id.trim(),
        subscriber_hash(email)
    );

    let response = client
        .put(&url)
        .basic_auth("swiftsw", Some(api_key.trim()))
        .header("Content-Type", "application/json")
        .json(&member_body(payload))
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await;

    let (success, code, body) = match response {
        Ok(resp) => {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            let snippet: String = text.chars().take(400).collect();
            (status.is_success(), Some(status.as_u16() as i32), snippet)
        }
        Err(e) => (false, None, format!("request failed: {e}")),
    };

    let _ = crate::db::delivery_log::log_delivery(
        pool,
        entry_id,
        "mailchimp",
        &url,
        success,
        code,
        Some(body.clone()),
    )
    .await;

    if success {
        tracing::info!("Mailchimp member upserted for {email} (list {list_id})");
        Ok(())
    } else {
        Err(AppError::Internal(format!(
            "Mailchimp API refused the push ({}) : {}",
            code.map(|c| c.to_string())
                .unwrap_or_else(|| "no response".into()),
            body.chars().take(300).collect::<String>()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delivery::payload::{
        CampaignPayload, ContactPayload, DeliveryPayload, QuestionAnswerPair,
    };

    fn payload_with(
        email: Option<&str>,
        first: Option<&str>,
        phone: Option<&str>,
    ) -> DeliveryPayload {
        DeliveryPayload {
            event: "entry.captured",
            contact: ContactPayload {
                first_name: first.map(str::to_string),
                last_name: None,
                email: email.map(str::to_string),
                phone: phone.map(str::to_string),
                website: None,
                business_name: None,
            },
            campaign: CampaignPayload {
                name: "Probe".into(),
                campaign_type: "quiz".into(),
                tag_namespace: "probe".into(),
            },
            outcome: "won".into(),
            tags_applied: vec![],
            score: None,
            questions_and_answers: Vec::<QuestionAnswerPair>::new(),
            entry_id: uuid::Uuid::nil().to_string(),
            captured_at: "2026-11-07T00:00:00Z".into(),
        }
    }

    #[test]
    fn datacentre_comes_from_the_key_suffix() {
        assert_eq!(server_prefix("abc123-us14"), "us14");
        assert_eq!(server_prefix("abc123"), DEFAULT_SERVER_PREFIX);
        assert_eq!(server_prefix("abc123-"), DEFAULT_SERVER_PREFIX);
        assert_eq!(server_prefix(""), DEFAULT_SERVER_PREFIX);
    }

    #[test]
    fn subscriber_hash_is_md5_of_the_lowercased_address() {
        // RFC 1321 vector, then the shape Mailchimp documents for a member id.
        assert_eq!(subscriber_hash("abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            subscriber_hash("  Probe@Example.INVALID "),
            subscriber_hash("probe@example.invalid")
        );
        assert_eq!(subscriber_hash("probe@example.invalid").len(), 32);
    }

    #[test]
    fn member_body_carries_only_the_default_merge_tags_and_never_resets_status() {
        let body = member_body(&payload_with(
            Some("probe@example.invalid"),
            Some("Ada"),
            Some("+1"),
        ));
        assert_eq!(body["email_address"], "probe@example.invalid");
        assert_eq!(body["status_if_new"], "subscribed");
        assert_eq!(body["merge_fields"]["FNAME"], "Ada");
        assert_eq!(body["merge_fields"]["PHONE"], "+1");
        assert!(
            body.get("status").is_none(),
            "a plain `status` would overwrite an opt-out"
        );
    }

    #[test]
    fn empty_contact_fields_are_omitted_rather_than_sent_blank() {
        let body = member_body(&payload_with(
            Some("probe@example.invalid"),
            Some("  "),
            None,
        ));
        assert_eq!(body["merge_fields"], serde_json::json!({}));
    }
}
