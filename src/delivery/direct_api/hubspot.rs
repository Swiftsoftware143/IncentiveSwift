//! HubSpot direct API delivery — push contact with Q&A as custom properties.

use crate::delivery::payload::DeliveryPayload;
use crate::error::AppError;

/// Push a delivery payload to HubSpot contacts API.
/// Creates or updates a contact with Q&A as custom properties.
pub async fn push_to_hubspot(
    client: &reqwest::Client,
    api_key: &str,
    payload: &DeliveryPayload,
) -> Result<(), AppError> {
    let url = "https://api.hubapi.com/crm/v3/objects/contacts";

    // Build properties from payload
    let mut properties = serde_json::json!({
        "firstname": payload.contact.first_name,
        "lastname": payload.contact.last_name,
        "email": payload.contact.email,
        "phone": payload.contact.phone,
        "hs_lead_status": payload.outcome,
    });

    // Add Q&A as custom properties
    if let Some(obj) = properties.as_object_mut() {
        for (i, qa) in payload.questions_and_answers.iter().enumerate() {
            let key = format!("incentive_q_{}", i + 1);
            obj.insert(key, serde_json::Value::String(qa.answer.clone()));
            let key_text = format!("incentive_q_{}_text", i + 1);
            obj.insert(key_text, serde_json::Value::String(qa.question.clone()));
        }
    }

    let response = client
        .post(url)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "properties": properties
        }))
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("HubSpot API request failed: {}", e)))?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(AppError::Internal(format!(
            "HubSpot API returned {}: {}",
            status.as_u16(),
            body
        )));
    }

    tracing::info!("HubSpot contact push successful for {}", payload.contact.email.as_deref().unwrap_or("unknown"));
    Ok(())
}
