//! ActiveCampaign direct API delivery — push contact with custom fields.

use crate::delivery::payload::DeliveryPayload;
use crate::error::AppError;

/// Push a delivery payload to ActiveCampaign API.
/// Creates or updates a contact with custom field values for Q&A.
pub async fn push_to_activecampaign(
    client: &reqwest::Client,
    api_key: &str,
    payload: &DeliveryPayload,
) -> Result<(), AppError> {
    let base_url = "https://{}.api-us1.com/api/3";

    // ActiveCampaign uses API key as a header
    // The base URL contains the account name, which we extract from the API key context
    // For simplicity, we use a constructed URL pattern
    let response = client
        .post("https://api.activecampaign.com/api/3/contact/sync")
        .header("Api-Token", api_key)
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "contact": {
                "email": payload.contact.email,
                "firstName": payload.contact.first_name,
                "lastName": payload.contact.last_name,
                "phone": payload.contact.phone,
                "fieldValues": build_field_values(payload),
            }
        }))
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("ActiveCampaign API request failed: {}", e)))?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(AppError::Internal(format!(
            "ActiveCampaign API returned {}: {}",
            status.as_u16(),
            body
        )));
    }

    tracing::info!("ActiveCampaign contact push successful for {}", payload.contact.email.as_deref().unwrap_or("unknown"));
    Ok(())
}

/// Build ActiveCampaign field values from Q&A.
fn build_field_values(payload: &DeliveryPayload) -> Vec<serde_json::Value> {
    let mut fields = vec![];

    for (i, qa) in payload.questions_and_answers.iter().enumerate() {
        fields.push(serde_json::json!({
            "field": format!("%INCENTIVE_Q_{}%", i + 1),
            "value": qa.answer,
        }));
    }

    // Add campaign info
    fields.push(serde_json::json!({
        "field": "%INCENTIVE_CAMPAIGN%",
        "value": payload.campaign.name,
    }));

    fields.push(serde_json::json!({
        "field": "%INCENTIVE_OUTCOME%",
        "value": payload.outcome,
    }));

    fields
}
