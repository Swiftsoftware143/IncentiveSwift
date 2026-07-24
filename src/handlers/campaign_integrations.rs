//! Campaign Integration binding handlers.
//!
//! Links integration_targets to campaigns with trigger event configuration.
//! Endpoints:
//!   GET    /api/v1/campaigns/{slug}/integrations
//!   POST   /api/v1/campaigns/{slug}/integrations
//!   DELETE /api/v1/campaigns/{slug}/integrations/{integration_id}

use crate::error::AppError;
use crate::state::AppState;
use crate::db::campaigns;
use crate::security::auth::AuthenticatedUser;
use axum::{
    extract::{Path, State},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;
use chrono::{DateTime, Utc};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct CampaignIntegration {
    pub id: Uuid,
    pub campaign_id: Uuid,
    pub integration_id: Uuid,
    pub trigger_events: Vec<String>,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Extended view with integration target details.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct CampaignIntegrationWithTarget {
    pub id: Uuid,
    pub campaign_id: Uuid,
    pub integration_id: Uuid,
    pub trigger_events: Vec<String>,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub name: String,
    pub provider: String,
    pub webhook_url: String,
    pub target_events: Vec<String>,
    pub is_active: bool,
}

#[derive(Deserialize)]
pub struct LinkIntegrationInput {
    pub integration_id: String,
    #[serde(default)]
    pub trigger_events: Option<Vec<String>>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool { true }

// ---------------------------------------------------------------------------
// Upsert conflict: fetch by campaign+integration ID
async fn get_by_campaign_and_integration(
    db: &sqlx::PgPool,
    campaign_id: &Uuid,
    integration_id: &Uuid,
) -> Result<CampaignIntegrationWithTarget, AppError> {
    sqlx::query_as::<_, CampaignIntegrationWithTarget>(
        r#"SELECT ci.id, ci.campaign_id, ci.integration_id, ci.trigger_events,
                  ci.enabled, ci.created_at, ci.updated_at,
                  it.name, it.provider, it.webhook_url, it.events as target_events,
                  it.is_active
           FROM campaign_integrations ci
           JOIN integration_targets it ON it.id = ci.integration_id
           WHERE ci.campaign_id = $1 AND ci.integration_id = $2"#
    )
    .bind(campaign_id)
    .bind(integration_id)
    .fetch_one(db)
    .await
    .map_err(|e| AppError::Internal(format!("Failed to fetch integration: {}", e)))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// GET /api/v1/campaigns/{slug}/integrations
/// List all integrations linked to a campaign.
pub async fn list_campaign_integrations(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> Result<Json<Value>, AppError> {
    let campaign = campaigns::get_campaign_by_slug(&state.db, &slug).await?;

    let integrations = sqlx::query_as::<_, CampaignIntegrationWithTarget>(
        r#"SELECT ci.id, ci.campaign_id, ci.integration_id, ci.trigger_events,
                  ci.enabled, ci.created_at, ci.updated_at,
                  it.name, it.provider, it.webhook_url, it.events as target_events,
                  it.is_active
           FROM campaign_integrations ci
           JOIN integration_targets it ON it.id = ci.integration_id
           WHERE ci.campaign_id = $1
           ORDER BY it.name"#
    )
    .bind(campaign.id)
    .fetch_all(&state.db)
    .await?;

    Ok(Json(json!({
        "integrations": integrations,
        "campaign_id": campaign.id,
        "campaign_slug": slug,
    })))
}

/// POST /api/v1/campaigns/{slug}/integrations
/// Link an integration target to a campaign.
pub async fn link_campaign_integration(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    Json(body): Json<LinkIntegrationInput>,
) -> Result<Json<Value>, AppError> {
    let campaign = campaigns::get_campaign_by_slug(&state.db, &slug).await?;

    let integration_id = Uuid::parse_str(&body.integration_id)
        .map_err(|_| AppError::BadRequest("Invalid integration_id format".to_string()))?;

    // Verify integration target exists
    let target_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM integration_targets WHERE id = $1)"
    )
    .bind(integration_id)
    .fetch_one(&state.db)
    .await?;

    if !target_exists {
        return Err(AppError::NotFound("Integration target not found".to_string()));
    }

    let trigger_events = body.trigger_events.unwrap_or_else(|| {
        vec!["on_win".to_string()]
    });

    let id = Uuid::new_v4();

    // Upsert: insert or update existing binding
    sqlx::query(
        r#"INSERT INTO campaign_integrations (id, campaign_id, integration_id, trigger_events, enabled)
           VALUES ($1, $2, $3, $4, $5)
           ON CONFLICT (campaign_id, integration_id)
           DO UPDATE SET trigger_events = EXCLUDED.trigger_events,
                         enabled = EXCLUDED.enabled,
                         updated_at = now()"#
    )
    .bind(id)
    .bind(campaign.id)
    .bind(integration_id)
    .bind(&trigger_events)
    .bind(body.enabled)
    .execute(&state.db)
    .await?;

    // Return the integration with target details
    // Try fetching by the new id first; on conflict (upsert) the insert id won't match,
    // so fall back to campaign+integration lookup
    let integration = match sqlx::query_as::<_, CampaignIntegrationWithTarget>(
        r#"SELECT ci.id, ci.campaign_id, ci.integration_id, ci.trigger_events,
                  ci.enabled, ci.created_at, ci.updated_at,
                  it.name, it.provider, it.webhook_url, it.events as target_events,
                  it.is_active
           FROM campaign_integrations ci
           JOIN integration_targets it ON it.id = ci.integration_id
           WHERE ci.id = $1"#
    )
    .bind(id)
    .fetch_one(&state.db)
    .await
    {
        Ok(ci) => ci,
        Err(_) => {
            // Upsert conflict — the row is keyed by (campaign_id, integration_id) so the
            // ON CONFLICT DO UPDATE means the inserted id may not match. Fetch the existing link.
            get_by_campaign_and_integration(&state.db, &campaign.id, &integration_id).await?
        }
    };

    Ok(Json(json!({ "integration": integration })))
}

/// DELETE /api/v1/campaigns/{slug}/integrations/{integration_id}
/// Unlink an integration from a campaign.
pub async fn unlink_campaign_integration(
    State(state): State<AppState>,
    Path((slug, integration_id)): Path<(String, String)>,
) -> Result<Json<Value>, AppError> {
    let campaign = campaigns::get_campaign_by_slug(&state.db, &slug).await?;

    let int_id = Uuid::parse_str(&integration_id)
        .map_err(|_| AppError::BadRequest("Invalid integration_id format".to_string()))?;

    let result = sqlx::query(
        r#"DELETE FROM campaign_integrations
           WHERE campaign_id = $1 AND integration_id = $2"#
    )
    .bind(campaign.id)
    .bind(int_id)
    .execute(&state.db)
    .await?;

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound("Integration link not found".to_string()));
    }

    Ok(Json(json!({ "status": "unlinked", "campaign_slug": slug, "integration_id": integration_id })))
}

// ---------------------------------------------------------------------------
// Marketing Boost configuration (per-campaign webhook for external marketing systems)
// ---------------------------------------------------------------------------

/// Marketing Boost config stored in campaigns.config['marketing_boost'].
/// Supports two modes:
///   1. Legacy webhook mode: fires webhook on events
///   2. Direct API mode: sends vouchers/cards/incentives via Marketing Boost API
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketingBoostConfig {
    pub enabled: bool,
    // Legacy webhook fields
    pub webhook_url: Option<String>,
    pub auth_header_name: Option<String>,
    pub auth_header_value: Option<String>,
    pub events: Option<Vec<String>>,
    // Direct API fields
    pub api_key: Option<String>,
    pub sender: Option<String>,
    pub business: Option<String>,
    pub incentive_type: Option<String>,
    pub amount: Option<u32>,
    pub destination: Option<u32>,
    pub trigger_events: Option<Vec<String>>,
    pub label: Option<String>,
}

/// Input for setting up Marketing Boost.
#[derive(Debug, Deserialize)]
pub struct SetMarketingBoostInput {
    pub enabled: bool,
    // Legacy webhook fields
    pub webhook_url: Option<String>,
    pub auth_header_name: Option<String>,
    pub auth_header_value: Option<String>,
    pub events: Option<Vec<String>>,
    // Direct API fields
    pub api_key: Option<String>,
    pub sender: Option<String>,
    pub business: Option<String>,
    pub incentive_type: Option<String>,
    pub amount: Option<u32>,
    pub destination: Option<u32>,
    pub trigger_events: Option<Vec<String>>,
    pub label: Option<String>,
}

/// PUT /api/v1/campaigns/{slug}/marketing-boost
/// Set or clear Marketing Boost config on a campaign.
/// Supports both legacy webhook mode and direct API mode (with incentive_type).
pub async fn set_marketing_boost(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    user: AuthenticatedUser,
    Json(body): Json<SetMarketingBoostInput>,
) -> Result<Json<Value>, AppError> {
    let campaign = campaigns::get_campaign_by_slug(&state.db, &slug).await?;

    // Build the marketing_boost block
    let boost = if body.enabled {
        let mut m = serde_json::Map::new();
        m.insert("enabled".to_string(), json!(true));

        // Direct API fields
        if let Some(ref v) = body.api_key { m.insert("api_key".to_string(), json!(v)); }
        if let Some(ref v) = body.sender { m.insert("sender".to_string(), json!(v)); }
        if let Some(ref v) = body.business { m.insert("business".to_string(), json!(v)); }
        if let Some(ref v) = body.incentive_type { m.insert("incentive_type".to_string(), json!(v)); }
        if let Some(v) = body.amount { m.insert("amount".to_string(), json!(v)); }
        if let Some(v) = body.destination { m.insert("destination".to_string(), json!(v)); }
        if let Some(ref v) = body.trigger_events { m.insert("trigger_events".to_string(), json!(v)); }

        // Legacy webhook fields
        if let Some(ref v) = body.webhook_url { m.insert("webhook_url".to_string(), json!(v)); }
        if let Some(ref v) = body.auth_header_name { m.insert("auth_header_name".to_string(), json!(v)); }
        if let Some(ref v) = body.auth_header_value { m.insert("auth_header_value".to_string(), json!(v)); }
        if let Some(ref v) = body.events { m.insert("events".to_string(), json!(v)); }

        // Label
        let label = body.label.unwrap_or_else(|| "Marketing Boost".to_string());
        m.insert("label".to_string(), json!(label));

        json!(m)
    } else {
        // Disabled => remove from config
        json!(null)
    };

    // Merge into existing config
    let config = campaign.config.clone();
    let mut new_map = config.as_object()
        .cloned()
        .unwrap_or_else(serde_json::Map::new);
    if body.enabled {
        new_map.insert("marketing_boost".to_string(), boost);
    } else {
        new_map.remove("marketing_boost");
    }
    let config = json!(new_map);

    sqlx::query("UPDATE campaigns SET config = $1 WHERE id = $2")
        .bind(&config)
        .bind(campaign.id)
        .execute(&state.db)
        .await?;

    Ok(Json(json!({
        "status": if body.enabled { "configured" } else { "disabled" },
        "campaign_slug": slug,
        "marketing_boost": if body.enabled {
            config.get("marketing_boost").cloned()
        } else {
            None::<Value>
        }
    })))
}

/// GET /api/v1/campaigns/{slug}/marketing-boost
/// Read the current Marketing Boost config.
pub async fn get_marketing_boost(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let campaign = campaigns::get_campaign_by_slug(&state.db, &slug).await?;
    let boost = campaign.config.get("marketing_boost");
    match boost {
        Some(Value::Object(_)) => Ok(Json(json!({
            "campaign_slug": slug,
            "marketing_boost": boost
        }))),
        _ => Ok(Json(json!({
            "campaign_slug": slug,
            "marketing_boost": null,
            "message": "Marketing Boost is not configured for this campaign."
        }))),
    }
}

/// Fire Marketing Boost for a given event, with optional per-reward/per-prize override.
/// Strategy:
///   1. If `per_item_boost` is `Some`, use that config (per-reward/per-prize).
///   2. Otherwise, fall back to campaign-level `campaigns.config['marketing_boost']`.
/// Supports two modes:
///   1. Legacy webhook mode: fires HTTP webhook on events
///   2. Direct API mode: sends voucher/card/incentive via Marketing Boost API
/// Returns Ok(()) regardless — fire-and-forget.
pub async fn fire_marketing_boost(
    state: &AppState,
    campaign_id: &Uuid,
    event: &str,
    payload: &Value,
) {
    fire_marketing_boost_with_override(state, campaign_id, event, payload, None).await;
}

/// Like `fire_marketing_boost`, but accepts an optional per-item boost config.
/// When `per_item_boost` is Some, it takes priority over campaign-level config.
pub async fn fire_marketing_boost_with_override(
    state: &AppState,
    campaign_id: &Uuid,
    event: &str,
    payload: &Value,
    per_item_boost: Option<&serde_json::Value>,
) {
    // Fetch campaign config fresh
    let row = sqlx::query_scalar::<_, serde_json::Value>(
        "SELECT config FROM campaigns WHERE id = $1"
    )
    .bind(campaign_id)
    .fetch_optional(&state.db)
    .await;

    let config = match row {
        Ok(Some(c)) => c,
        _ => return,
    };

    // Determine which boost config to use: per-item override takes priority
    let boost = if let Some(override_val) = per_item_boost {
        match override_val.as_object() {
            Some(obj) => obj.clone(),
            None => {
                // Override is present but not an object (e.g. null) — fall back to campaign
                match config.get("marketing_boost") {
                    Some(Value::Object(m)) => m.clone(),
                    _ => return,
                }
            }
        }
    } else {
        match config.get("marketing_boost") {
            Some(Value::Object(m)) => m.clone(),
            _ => return,
        }
    };

    if boost.get("enabled").and_then(|v| v.as_bool()) != Some(true) {
        return;
    }

    // Check if this is direct API mode (has incentive_type) or legacy webhook mode
    if let Some(incentive_type) = boost.get("incentive_type").and_then(|v| v.as_str()) {
        // Direct API mode — fire the marketing boost incentive send
        let trigger_events = boost.get("trigger_events")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect::<Vec<_>>())
            .unwrap_or_else(|| vec!["on_win".to_string(), "on_redeem".to_string()]);

        if !trigger_events.iter().any(|e| e == event) {
            tracing::debug!("Marketing Boost: event '{}' not in trigger_events {:?}", event, trigger_events);
            return;
        }

        // Extract contact info from payload
        let first_name = payload.get("first_name")
            .or_else(|| payload.get("contact_id")).and_then(|v| v.as_str())
            .unwrap_or("");
        let last_name = payload.get("last_name")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let email = payload.get("email")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let phone = payload.get("phone").and_then(|v| v.as_str()).map(String::from);
        let countrycode = payload.get("countrycode").and_then(|v| v.as_str()).map(String::from);
        let campaign_name = payload.get("campaign_name")
            .or_else(|| payload.get("campaign_slug"))
            .and_then(|v| v.as_str())
            .unwrap_or("Campaign");

        // Check that we have enough info to send
        if email.is_empty() {
            tracing::warn!("Marketing Boost: cannot send {} incentive without email", incentive_type);
            return;
        }

        // Resolve credentials: per-boost config -> stored provider key -> env defaults
        let api_key = if let Some(k) = boost.get("api_key").and_then(|v| v.as_str()).filter(|k| !k.is_empty()) {
            k.to_string()
        } else {
            sqlx::query_scalar::<_, String>(
                "SELECT api_key FROM provider_keys WHERE account_id = (SELECT account_id FROM campaigns WHERE id = $1) AND provider = 'marketing_boost' AND is_active = true LIMIT 1"
            )
            .bind(campaign_id)
            .fetch_optional(&state.db)
            .await
            .ok()
            .flatten()
            .unwrap_or_default()
        };
        let sender = if let Some(s) = boost.get("sender").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
            s.to_string()
        } else {
            sqlx::query_scalar::<_, String>(
                "SELECT COALESCE(metadata->>'sender', '3822-4706') FROM provider_keys WHERE account_id = (SELECT account_id FROM campaigns WHERE id = $1) AND provider = 'marketing_boost' AND is_active = true LIMIT 1"
            )
            .bind(campaign_id)
            .fetch_optional(&state.db)
            .await
            .ok()
            .flatten()
            .unwrap_or_else(|| "3822-4706".to_string())
        };
        let business = if let Some(b) = boost.get("business").and_then(|v| v.as_str()).filter(|b| !b.is_empty()) {
            b.to_string()
        } else {
            sqlx::query_scalar::<_, String>(
                "SELECT COALESCE(metadata->>'business', '6111') FROM provider_keys WHERE account_id = (SELECT account_id FROM campaigns WHERE id = $1) AND provider = 'marketing_boost' AND is_active = true LIMIT 1"
            )
            .bind(campaign_id)
            .fetch_optional(&state.db)
            .await
            .ok()
            .flatten()
            .unwrap_or_else(|| "6111".to_string())
        };
        let amount = boost.get("amount").and_then(|v| v.as_u64()).map(|v| v as u32);
        let destination = boost.get("destination").and_then(|v| v.as_u64()).map(|v| v as u32);

        let client = &state.http_client;

        let result = match incentive_type {
            "dining_voucher" => {
                let amt = amount.unwrap_or(50);
                crate::delivery::direct_api::marketing_boost::send_dining_voucher(
                    client, &api_key, &sender, &business,
                    first_name, last_name, email, amt, campaign_name,
                ).await
            }
            "hotel_savings_card" => {
                let amt = amount.unwrap_or(200);
                crate::delivery::direct_api::marketing_boost::send_hotel_savings_card(
                    client, &api_key, &sender, &business,
                    first_name, last_name, email, amt, campaign_name,
                ).await
            }
            "vacation_incentive" => {
                let dest = destination.unwrap_or(41);
                crate::delivery::direct_api::marketing_boost::send_vacation_incentive(
                    client, &api_key, &sender, &business,
                    first_name, last_name, email, phone, countrycode, dest, campaign_name,
                ).await
            }
            other => {
                tracing::warn!("Marketing Boost: unknown incentive_type: {}", other);
                return;
            }
        };

        match result {
            Ok(resp) => tracing::info!("Marketing Boost {} incentive sent: {:?}", incentive_type, resp),
            Err(e) => tracing::warn!("Marketing Boost {} incentive failed: {}", incentive_type, e),
        }
    } else {
        // Legacy webhook mode
        let webhook_url = match boost.get("webhook_url") {
            Some(Value::String(s)) => s.clone(),
            _ => return,
        };

        // Check if this event is in the configured list
        let allowed_events = boost.get("events").and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter().filter_map(|v| v.as_str().map(String::from)).collect::<Vec<_>>()
            })
            .unwrap_or_else(|| vec!["voucher_issued".to_string(), "reward_redeemed".to_string()]);

        if !allowed_events.iter().any(|e| e == event) {
            tracing::debug!("Marketing Boost: event '{}' not in allowed list {:?}", event, allowed_events);
            return;
        }

        // Build the full webhook payload
        let auth_header_name = boost.get("auth_header_name").and_then(|v| v.as_str()).map(String::from);
        let auth_header_value = boost.get("auth_header_value").and_then(|v| v.as_str()).map(String::from);

        let full_payload = json!({
            "event": event,
            "campaign_id": campaign_id,
            "timestamp": chrono::Utc::now().to_rfc3339(),
            "data": payload,
        });

        let mut req = state.http_client.post(&webhook_url)
            .json(&full_payload)
            .timeout(std::time::Duration::from_secs(10));

        if let Some(ref name) = auth_header_name {
            if let Some(ref val) = auth_header_value {
                req = req.header(name.as_str(), val.as_str());
            }
        }

        match req.send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    tracing::info!("Marketing Boost webhook sent for event '{}': {}", event, status);
                } else {
                    tracing::warn!("Marketing Boost webhook returned {} for event '{}'", status, event);
                }
            }
            Err(e) => {
                tracing::warn!("Marketing Boost webhook failed for event '{}': {}", event, e);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Integration firing logic (called from spin handler)
// ---------------------------------------------------------------------------

/// Payload sent to integrations when a prize is won.
#[derive(Debug, Serialize)]
pub struct IntegrationEventPayload {
    pub event: String,
    pub contact: ContactInfo,
    pub campaign: CampaignInfo,
    pub prize: PrizeInfo,
    pub was_pity: bool,
    pub streak: i32,
    pub total_spins: i32,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContactInfo {
    pub email: Option<String>,
    pub phone: Option<String>,
    pub first_name: Option<String>,
    pub last_name: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CampaignInfo {
    pub name: String,
    pub slug: String,
    #[serde(rename = "type")]
    pub campaign_type: String,
}

#[derive(Debug, Serialize)]
pub struct PrizeInfo {
    pub id: String,
    pub label: String,
    #[serde(rename = "type")]
    pub prize_type: String,
    pub redemption_code: Option<String>,
}

/// Fire all enabled integrations for a campaign on a trigger event.
pub async fn fire_campaign_integrations(
    state: &AppState,
    campaign_slug: &str,
    campaign_name: &str,
    campaign_type: &str,
    campaign_id: &Uuid,
    event_type: &str,
    contact: ContactInfo,
    prize: PrizeInfo,
    was_pity: bool,
    streak: i32,
    total_spins: i32,
) {
    // Look up all enabled campaign integrations matching this event
    let integrations = sqlx::query_as::<_, CampaignIntegrationWithTarget>(
        r#"SELECT ci.id, ci.campaign_id, ci.integration_id, ci.trigger_events,
                  ci.enabled, ci.created_at, ci.updated_at,
                  it.name, it.provider, it.webhook_url, it.events as target_events,
                  it.is_active
           FROM campaign_integrations ci
           JOIN integration_targets it ON it.id = ci.integration_id
           WHERE ci.campaign_id = $1
             AND ci.enabled = true
             AND it.is_active = true
             AND $2 = ANY(ci.trigger_events)"#
    )
    .bind(campaign_id)
    .bind(event_type)
    .fetch_all(&state.db)
    .await;

    let integrations = match integrations {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!("Failed to fetch campaign integrations: {}", e);
            return;
        }
    };

    if integrations.is_empty() {
        tracing::debug!("No enabled integrations for campaign {} on event {}", campaign_slug, event_type);
        return;
    }

    let contact_for_payload = contact.clone();
    let payload = IntegrationEventPayload {
        event: event_type.to_string(),
        contact: contact_for_payload,
        campaign: CampaignInfo {
            name: campaign_name.to_string(),
            slug: campaign_slug.to_string(),
            campaign_type: campaign_type.to_string(),
        },
        prize,
        was_pity,
        streak,
        total_spins,
    };

    let _payload_json = serde_json::to_value(&payload)
        .unwrap_or_else(|_| serde_json::json!({}));

    let client = &state.http_client;

    for integration in &integrations {
        let url = &integration.webhook_url;
        let integration_name = &integration.name;

        tracing::info!(
            "Firing integration '{}' ({}) for campaign {} event {}",
            integration_name, url, campaign_slug, event_type
        );

        // Build a delivery-friendly payload compatible with the webhook delivery system
        let delivery_payload = crate::delivery::payload::DeliveryPayload::build(
            crate::delivery::payload::ContactPayload {
                first_name: contact.first_name.clone(),
                last_name: contact.last_name.clone(),
                email: contact.email.clone(),
                phone: contact.phone.clone(),
                website: None,
                business_name: None,
            },
            crate::delivery::payload::CampaignPayload {
                name: campaign_name.to_string(),
                campaign_type: campaign_type.to_string(),
                tag_namespace: String::new(),
            },
            "winner".to_string(),
            vec!["Prize_Winner".to_string()],
            None,
            vec![],
            Uuid::new_v4().to_string(),
        );

        // Use the existing webhook delivery system with retries
        let entry_id = Uuid::new_v4();
        if let Err(e) = crate::delivery::webhook::push_to_webhook(
            client,
            url,
            &delivery_payload,
            &state.db,
            &entry_id,
        )
        .await
        {
            tracing::warn!(
                "Integration '{}' webhook delivery failed: {}",
                integration_name, e
            );
        }
    }
}
