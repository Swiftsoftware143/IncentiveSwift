//! Webhook delivery — push payload to webhook URL with retry and logging.

use crate::delivery::payload::DeliveryPayload;
use crate::error::AppError;
use serde_json::json;
use uuid::Uuid;

/// Push payload to a webhook URL with retry (3 attempts, exponential backoff).
/// Logs each delivery attempt to delivery_log.
pub async fn push_to_webhook(
    client: &reqwest::Client,
    url: &str,
    payload: &DeliveryPayload,
    pool: &sqlx::PgPool,
    entry_id: &Uuid,
) -> Result<(), AppError> {
    let entry_id_str = entry_id.to_string();
    let payload_json = serde_json::to_value(payload)
        .map_err(|e| AppError::Internal(format!("Failed to serialize payload: {}", e)))?;

    let retry_delays = [1u64, 2, 4];
    let mut last_error = None;

    for (attempt, delay_secs) in retry_delays.iter().enumerate() {
        match client
            .post(url)
            .json(&payload_json)
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
        {
            Ok(response) => {
                let status = response.status();
                let status_code = status.as_u16() as i32;
                let body = response.text().await.unwrap_or_default();

                // Log the delivery attempt
                let success = status.is_success();
                let _ = crate::db::delivery_log::log_delivery(
                    pool,
                    entry_id,
                    "webhook",
                    url,
                    success,
                    Some(status_code),
                    Some(body.clone()),
                ).await;

                if success {
                    // Update entry delivery status
                    let _ = crate::db::entries::record_delivery(
                        pool,
                        entry_id,
                        true,
                        "webhook",
                        url,
                    ).await;
                    return Ok(());
                }

                last_error = Some(AppError::Internal(format!(
                    "Webhook returned status {}: {}",
                    status_code, body
                )));

                // Don't retry 4xx errors
                if status_code >= 400 && status_code < 500 {
                    break;
                }
            }
            Err(e) => {
                last_error = Some(AppError::Internal(format!(
                    "Webhook request failed: {}",
                    e
                )));
            }
        }

        // Wait before retry
        if attempt < retry_delays.len() - 1 {
            tokio::time::sleep(std::time::Duration::from_secs(*delay_secs)).await;
        }
    }

    // Log final failure
    let _ = crate::db::delivery_log::log_delivery(
        pool,
        entry_id,
        "webhook",
        url,
        false,
        None,
        Some(last_error.as_ref().map(|e| e.to_string()).unwrap_or_default()),
    ).await;

    // Don't fail the request — delivery is best-effort
    tracing::warn!(
        "Webhook delivery failed for entry {} after retries: {:?}",
        entry_id_str,
        last_error
    );

    Ok(())
}
