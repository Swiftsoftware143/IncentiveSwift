//! Marketing Boost configuration & send handler.
//!
//! This module manages the per-campaign Marketing Boost configuration stored in
//! `campaigns.config['marketing_boost']` and provides the API to:
//!   - Fetch destination list from Marketing Boost API
//!   - Send incentives (dining voucher, hotel savings card, vacation incentive)
//!     when a contact wins or redeems a prize
//!
//! Routes:
//!   GET  /api/v1/marketing-boost/destinations
//!   PUT  /api/v1/campaigns/:slug/marketing-boost   (in campaign_integrations.rs)
//!   GET  /api/v1/campaigns/:slug/marketing-boost   (in campaign_integrations.rs)

use crate::error::AppError;
use crate::state::AppState;
use axum::{
    extract::State,
    Json,
};
use serde_json::{json, Value};

/// GET /api/v1/marketing-boost/destinations
/// Fetch the destination list from Marketing Boost API.
/// Requires `MB_API_KEY` and `MB_SENDER` environment variables or
/// the first configured campaign's marketing_boost config credentials.
/// The results are cached in memory for 1 hour.
pub async fn get_destinations(
    State(state): State<AppState>,
) -> Result<Json<Value>, AppError> {
    tracing::info!("get_destinations called");

    // Try to get API key from environment first, then fall back to DB
    let api_key = match std::env::var("MB_API_KEY") {
        Ok(k) => k,
        Err(_) => {
            get_marketing_boost_credentials_from_db(&state).await
                .map(|(k, _)| k)
                .map_err(|_| AppError::Internal(
                    "Marketing Boost API key not configured. Set MB_API_KEY env var or configure on a campaign first.".to_string()
                ))?
        }
    };

    let sender = std::env::var("MB_SENDER")
        .unwrap_or_else(|_| "3822-4706".to_string());

    let client = &state.http_client;
    let destinations = crate::delivery::direct_api::marketing_boost::fetch_destinations(
        client,
        &api_key,
        &sender,
    )
    .await?;

    Ok(Json(json!({
        "sender": sender,
        "destinations": destinations,
    })))
}

/// Helper: fetch the first campaign's Marketing Boost credentials from the database.
async fn get_marketing_boost_credentials_from_db(
    state: &AppState,
) -> Result<(String, String), AppError> {
    let row = sqlx::query_scalar::<_, serde_json::Value>(
        r#"SELECT config FROM campaigns WHERE config ? 'marketing_boost' LIMIT 1"#
    )
    .fetch_optional(&state.db)
    .await
    .map_err(|e| AppError::Internal(format!("DB error: {}", e)))?;

    match row {
        Some(config) => {
            let boost = config.get("marketing_boost")
                .and_then(|v| v.as_object())
                .ok_or_else(|| AppError::Internal("No marketing_boost config found".to_string()))?;

            let api_key = boost.get("api_key")
                .and_then(|v| v.as_str())
                .ok_or_else(|| AppError::Internal("api_key not found in marketing_boost config".to_string()))?
                .to_string();

            let sender = boost.get("sender")
                .and_then(|v| v.as_str())
                .unwrap_or("3822-4706")
                .to_string();

            Ok((api_key, sender))
        }
        None => Err(AppError::Internal(
            "No campaign with Marketing Boost configuration found".to_string()
        )),
    }
}

/// Send a Marketing Boost incentive based on campaign config.
/// Called from win/redeem flows in loyalty_v2 and spin_handler.
/// This is fire-and-forget — errors are logged but not returned to the caller.
pub async fn send_marketing_boost_incentive(
    state: &AppState,
    campaign_id: &uuid::Uuid,
    campaign_name: &str,
    first_name: &str,
    last_name: &str,
    email: &str,
    phone: Option<String>,
    countrycode: Option<String>,
) {
    // Fetch campaign config
    let row = sqlx::query_scalar::<_, serde_json::Value>(
        "SELECT config FROM campaigns WHERE id = $1"
    )
    .bind(campaign_id)
    .fetch_optional(&state.db)
    .await;

    let config = match row {
        Ok(Some(c)) => c,
        _ => {
            tracing::debug!("Marketing Boost: no campaign config found for {}", campaign_id);
            return;
        }
    };

    let boost = match config.get("marketing_boost") {
        Some(Value::Object(m)) => m.clone(),
        _ => {
            tracing::debug!("Marketing Boost: no marketing_boost config on campaign {}", campaign_id);
            return;
        }
    };

    // Check if enabled
    if boost.get("enabled").and_then(|v| v.as_bool()) != Some(true) {
        tracing::debug!("Marketing Boost: disabled on campaign {}", campaign_id);
        return;
    }

    // Extract config fields
    let api_key = match boost.get("api_key").and_then(|v| v.as_str()) {
        Some(k) => k.to_string(),
        None => {
            tracing::warn!("Marketing Boost: missing api_key on campaign {}", campaign_id);
            return;
        }
    };

    let sender = boost.get("sender")
        .and_then(|v| v.as_str())
        .unwrap_or("3822-4706")
        .to_string();

    let business = boost.get("business")
        .and_then(|v| v.as_str())
        .unwrap_or("6111")
        .to_string();

    let incentive_type = match boost.get("incentive_type").and_then(|v| v.as_str()) {
        Some(t) => t.to_string(),
        None => {
            tracing::warn!("Marketing Boost: missing incentive_type on campaign {}", campaign_id);
            return;
        }
    };

    let amount = boost.get("amount").and_then(|v| v.as_u64()).map(|v| v as u32);
    let destination = boost.get("destination").and_then(|v| v.as_u64()).map(|v| v as u32);

    tracing::info!(
        "Marketing Boost: sending {} incentive for campaign {} (contact: {})",
        incentive_type, campaign_name, email
    );

    let client = &state.http_client;

    let result = match incentive_type.as_str() {
        "dining_voucher" => {
            let amt = amount.unwrap_or(50);
            crate::delivery::direct_api::marketing_boost::send_dining_voucher(
                client, &api_key, &sender, &business,
                first_name, last_name, email, amt, campaign_name,
            )
            .await
        }
        "hotel_savings_card" => {
            let amt = amount.unwrap_or(200);
            crate::delivery::direct_api::marketing_boost::send_hotel_savings_card(
                client, &api_key, &sender, &business,
                first_name, last_name, email, amt, campaign_name,
            )
            .await
        }
        "vacation_incentive" => {
            let dest = destination.unwrap_or(41);
            crate::delivery::direct_api::marketing_boost::send_vacation_incentive(
                client, &api_key, &sender, &business,
                first_name, last_name, email, phone, countrycode, dest, campaign_name,
            )
            .await
        }
        other => {
            tracing::warn!("Marketing Boost: unknown incentive_type: {}", other);
            return;
        }
    };

    match result {
        Ok(resp) => {
            tracing::info!(
                "Marketing Boost {} incentive sent successfully for {}: {:?}",
                incentive_type, email, resp
            );
        }
        Err(e) => {
            tracing::warn!(
                "Marketing Boost {} incentive failed for {}: {}",
                incentive_type, email, e
            );
        }
    }
}
