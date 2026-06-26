//! GoHighLevel direct API delivery — push contact with custom fields.

use crate::delivery::payload::DeliveryPayload;
use crate::error::AppError;

/// Push a delivery payload to GoHighLevel API.
/// Creates or updates a contact with custom field values for Q&A.
pub async fn push_to_gohighlevel(
    client: &reqwest::Client,
    api_key: &str,
    payload: &DeliveryPayload,
) -> Result<(), AppError> {
    let url = "https://rest.gohighlevel.com/v1/contacts/";

    let response = client
        .post(url)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "firstName": payload.contact.first_name,
            "lastName": payload.contact.last_name,
            "email": payload.contact.email,
            "phone": payload.contact.phone,
            "companyName": payload.contact.business_name,
            "customFields": build_custom_fields(payload),
            "tags": payload.tags_applied,
        }))
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("GoHighLevel API request failed: {}", e)))?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(AppError::Internal(format!(
            "GoHighLevel API returned {}: {}",
            status.as_u16(),
            body
        )));
    }

    tracing::info!("GoHighLevel contact push successful for {}", payload.contact.email.as_deref().unwrap_or("unknown"));
    Ok(())
}

/// Build GoHighLevel custom fields from Q&A.
fn build_custom_fields(payload: &DeliveryPayload) -> Vec<serde_json::Value> {
    let mut fields = vec![];

    for (i, qa) in payload.questions_and_answers.iter().enumerate() {
        fields.push(serde_json::json!({
            "key": format!("incentive_q_{}", i + 1),
            "value": qa.answer,
        }));
        fields.push(serde_json::json!({
            "key": format!("incentive_q_{}_label", i + 1),
            "value": qa.question,
        }));
    }

    fields.push(serde_json::json!({
        "key": "campaign_name",
        "value": payload.campaign.name,
    }));

    fields.push(serde_json::json!({
        "key": "campaign_type",
        "value": payload.campaign.campaign_type,
    }));

    fields.push(serde_json::json!({
        "key": "outcome",
        "value": payload.outcome,
    }));

    fields
}
