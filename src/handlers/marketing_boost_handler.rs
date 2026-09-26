//! Marketing Boost configuration & send handler.
//!
//! This module manages the per-campaign Marketing Boost configuration stored in
//! `campaigns.config['marketing_boost']` and provides the API to:
//!   - Fetch destination list from Marketing Boost API
//!   - Send incentives (dining voucher, hotel savings card, vacation incentive)
//!     when a contact wins or redeems a prize
//!
//! Routes:
//!   GET  /api/v1/marketing-boost/destinations      (AUTHENTICATED; per-tenant credential — the
//!                                                  destination catalogue of the CALLER'S account,
//!                                                  resolved by `account_credentials` below)
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
use crate::security::auth::AuthenticatedUser;
use crate::state::AppState;
use axum::{extract::State, Json};
use serde_json::{json, Value};
use uuid::Uuid;

/// Sender id used when the tenant's own stored metadata names none.
const DEFAULT_SENDER: &str = "3822-4706";

/// GET /api/v1/marketing-boost/destinations
/// Fetch the destination list from the Marketing Boost API for the CALLING ACCOUNT.
///
/// The credential is the caller's own. This route used to read a server-wide `MB_API_KEY`
/// environment variable and fall back to "the first campaign in the whole table that carries a
/// marketing_boost config" (`LIMIT 1`, no tenant predicate) — gate rule 5c (class 8), kanban
/// t_017517a9. MEASURED at HEAD: it answered **200 to an anonymous caller** (the handler took no
/// `AuthenticatedUser`) while spending the fleet's vendor key. It is authenticated now and the key
/// comes from the caller's own Integration Center row / own campaign config; there is no
/// server-wide fallback. `fetch_destinations` still caches the vendor response for 1 hour.
pub async fn get_destinations(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    tracing::info!("get_destinations called");

    let account_id = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;

    let (api_key, sender) = account_credentials(&state, account_id).await?;

    let client = &state.http_client;
    let destinations =
        crate::delivery::direct_api::marketing_boost::fetch_destinations(client, &api_key, &sender)
            .await?;

    Ok(Json(json!({
        "sender": sender,
        "destinations": destinations,
    })))
}

/// Resolve ONE account's Marketing Boost credential from that account's own two stores, in order:
///   1. its Integration Center row — `provider_keys(provider='marketing_boost')`, ciphertext at
///      rest, decrypted here (the same store the live win/redeem send path resolves);
///   2. a `marketing_boost` config on one of the account's OWN campaigns
///      (`campaigns.config['marketing_boost']`, written by PUT /api/v1/campaigns/:slug/marketing-boost).
/// An account with neither is told to configure one instead of silently borrowing another tenant's
/// key or a fleet-owned env var.
async fn account_credentials(
    state: &AppState,
    account_id: Uuid,
) -> Result<(String, String), AppError> {
    let stored = sqlx::query_as::<_, (String, Option<Value>)>(
        "SELECT api_key, metadata FROM provider_keys WHERE account_id = $1 AND provider = 'marketing_boost' AND is_active = true LIMIT 1",
    )
    .bind(account_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| AppError::Internal(format!("DB error: {}", e)))?;

    if let Some((ciphertext, metadata)) = stored {
        // provider_keys.api_key is CIPHERTEXT at rest (src/security/provider_key_crypto.rs), so the
        // value handed to the vendor is the DECRYPTED one; a row that cannot be decrypted is a hard
        // error rather than an empty key sent upstream.
        let plain = crate::security::provider_key_crypto::decrypt_from_storage(
            &state.db,
            ciphertext.trim(),
        )
        .await?;
        if !plain.trim().is_empty() {
            let sender = metadata
                .as_ref()
                .and_then(|m| m.get("sender"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(DEFAULT_SENDER)
                .to_string();
            return Ok((plain, sender));
        }
    }

    let row = sqlx::query_scalar::<_, Value>(
        "SELECT COALESCE(config, '{}'::jsonb) FROM campaigns WHERE account_id = $1 AND config ? 'marketing_boost' LIMIT 1",
    )
    .bind(account_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| AppError::Internal(format!("DB error: {}", e)))?;

    let boost = row.as_ref().and_then(|c| c.get("marketing_boost"));
    let api_key = boost
        .and_then(|b| b.get("api_key"))
        .and_then(|v| v.as_str())
        .filter(|k| !k.trim().is_empty());
    if let Some(api_key) = api_key {
        let sender = boost
            .and_then(|b| b.get("sender"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(DEFAULT_SENDER)
            .to_string();
        return Ok((api_key.to_string(), sender));
    }

    Err(AppError::BadRequest(
        "Marketing Boost is not configured for this account. Add your Marketing Boost API key in \
         Settings -> Integration Center (provider `marketing_boost`), or on a campaign's Marketing \
         Boost tab. It is a per-tenant credential: there is no server-wide fallback."
            .to_string(),
    ))
}
