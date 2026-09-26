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
//!
//! NOTE (kanban t_0fc42946): a second, older sender lived in this module —
//! `send_marketing_boost_incentive(state, campaign_id, campaign_name, first/last name, email,
//! phone, countrycode)`. It had ZERO callers (`grep -rn` across `src/` returned only its own
//! definition) and was a strict SUBSET of
//! `campaign_integrations::fire_marketing_boost_with_override`, which is what the win/redeem
//! flows actually call (handlers/loyalty_v2.rs:274/887/1759, handlers/spin_handler.rs:517).
//! The live path also carries the per-prize `per_item_boost` override, `trigger_events`
//! filtering, stored `provider_keys` credential resolution and legacy webhook mode. The dead
//! duplicate was DELETED rather than wired: two senders for one incentive is how the config a
//! prize actually carries stops being the config that fires.

use crate::error::AppError;
use crate::state::AppState;
use axum::{extract::State, Json};
use serde_json::{json, Value};

/// GET /api/v1/marketing-boost/destinations
/// Fetch the destination list from Marketing Boost API.
/// Requires `MB_API_KEY` and `MB_SENDER` environment variables or
/// the first configured campaign's marketing_boost config credentials.
/// The results are cached in memory for 1 hour.
pub async fn get_destinations(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
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

    let sender = std::env::var("MB_SENDER").unwrap_or_else(|_| "3822-4706".to_string());

    let client = &state.http_client;
    let destinations =
        crate::delivery::direct_api::marketing_boost::fetch_destinations(client, &api_key, &sender)
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
        r#"SELECT COALESCE(config, '{}'::jsonb) FROM campaigns WHERE config ? 'marketing_boost' LIMIT 1"#,
    )
    .fetch_optional(&state.db)
    .await
    .map_err(|e| AppError::Internal(format!("DB error: {}", e)))?;

    match row {
        Some(config) => {
            let boost = config
                .get("marketing_boost")
                .and_then(|v| v.as_object())
                .ok_or_else(|| AppError::Internal("No marketing_boost config found".to_string()))?;

            let api_key = boost
                .get("api_key")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    AppError::Internal("api_key not found in marketing_boost config".to_string())
                })?
                .to_string();

            let sender = boost
                .get("sender")
                .and_then(|v| v.as_str())
                .unwrap_or("3822-4706")
                .to_string();

            Ok((api_key, sender))
        }
        None => Err(AppError::Internal(
            "No campaign with Marketing Boost configuration found".to_string(),
        )),
    }
}
