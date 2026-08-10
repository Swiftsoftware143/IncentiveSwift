//! Marketing Boost direct API — send vouchers, hotel savings cards, and vacation incentives.
//!
//! Endpoints:
//!   POST https://members.marketingboost.com/api/restaurants_api/send        — Dining Voucher
//!   POST https://members.marketingboost.com/api/hotel_saving_api/send        — Hotel Savings Card
//!   POST https://members.marketingboost.com/api/vacation-incentives/send     — Vacation Incentive
//!   GET  https://members.marketingboost.com/api/all-destination-list/{sender} — List destinations

use crate::error::AppError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use std::time::Instant;
use tokio::sync::Mutex;

// ---------------------------------------------------------------------------
// Destination cache per account (TTL 1 hour)
// ---------------------------------------------------------------------------

/// Cached destination list for a sender.
#[derive(Debug, Clone)]
struct DestinationCache {
    /// The serialized list of destinations
    destinations: Value,
    /// When the cache was populated
    fetched_at: Instant,
    /// The sender this cache is keyed on
    sender: String,
}

impl DestinationCache {
    fn is_fresh(&self) -> bool {
        self.fetched_at.elapsed().as_secs() < 3600 // 1 hour TTL
    }
}

static DESTINATION_CACHE: LazyLock<Arc<Mutex<HashMap<String, DestinationCache>>>> =
    LazyLock::new(|| Arc::new(Mutex::new(HashMap::new())));

// ---------------------------------------------------------------------------
// Request/Response types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct DiningVoucherRequest {
    pub sender: String,
    pub full_name: String,
    pub email: String,
    pub amount: u32,
    pub business: String,
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct HotelSavingsCardRequest {
    pub sender: String,
    pub full_name: String,
    pub email: String,
    pub amount: u32,
    pub business: String,
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct VacationIncentiveRequest {
    pub sender: String,
    pub business: String,
    pub destination: u32,
    pub name: String,
    pub email: String,
    pub countrycode: Option<String>,
    pub phone: Option<String>,
    pub message: String,
}

#[derive(Debug, Deserialize)]
pub struct Destination {
    pub id: u32,
    pub name: String,
    pub country: Option<String>,
}

// ---------------------------------------------------------------------------
// API helpers
// ---------------------------------------------------------------------------

const BASE_URL: &str = "https://members.marketingboost.com/api";

/// Build the common headers for Marketing Boost API requests.
fn common_headers(api_key: &str) -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        "X-Api-Key",
        reqwest::header::HeaderValue::from_str(api_key).unwrap(),
    );
    headers.insert(
        "Content-Type",
        reqwest::header::HeaderValue::from_static("application/json"),
    );
    headers
}

/// Send a Dining Voucher via Marketing Boost.
pub async fn send_dining_voucher(
    client: &reqwest::Client,
    api_key: &str,
    sender: &str,
    business: &str,
    first_name: &str,
    last_name: &str,
    email: &str,
    amount: u32,
    campaign_name: &str,
) -> Result<Value, AppError> {
    // Validate amount
    let valid_amounts = [25, 50, 100, 200];
    if !valid_amounts.contains(&amount) {
        return Err(AppError::BadRequest(format!(
            "Invalid dining voucher amount: {}. Valid amounts: {:?}",
            amount, valid_amounts
        )));
    }

    let body = DiningVoucherRequest {
        sender: sender.to_string(),
        full_name: format!("{} {}", first_name, last_name),
        email: email.to_string(),
        amount,
        business: business.to_string(),
        message: campaign_name.to_string(),
    };

    let url = format!("{}/restaurants_api/send", BASE_URL);
    let resp = client
        .post(&url)
        .headers(common_headers(api_key))
        .json(&body)
        .send()
        .await
        .map_err(|e| {
            AppError::Internal(format!(
                "Marketing Boost dining voucher request failed: {}",
                e
            ))
        })?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();

    if status.is_success() {
        tracing::info!("Marketing Boost dining voucher sent for {}", email);
        serde_json::from_str(&text).map_err(|e| {
            AppError::Internal(format!("Failed to parse Marketing Boost response: {}", e))
        })
    } else {
        Err(AppError::Internal(format!(
            "Marketing Boost dining voucher returned {}: {}",
            status, text
        )))
    }
}

/// Send a Hotel Savings Card via Marketing Boost.
pub async fn send_hotel_savings_card(
    client: &reqwest::Client,
    api_key: &str,
    sender: &str,
    business: &str,
    first_name: &str,
    last_name: &str,
    email: &str,
    amount: u32,
    campaign_name: &str,
) -> Result<Value, AppError> {
    // Validate amount
    let valid_amounts = [100, 200, 300, 500];
    if !valid_amounts.contains(&amount) {
        return Err(AppError::BadRequest(format!(
            "Invalid hotel savings card amount: {}. Valid amounts: {:?}",
            amount, valid_amounts
        )));
    }

    let body = HotelSavingsCardRequest {
        sender: sender.to_string(),
        full_name: format!("{} {}", first_name, last_name),
        email: email.to_string(),
        amount,
        business: business.to_string(),
        message: campaign_name.to_string(),
    };

    let url = format!("{}/hotel_saving_api/send", BASE_URL);
    let resp = client
        .post(&url)
        .headers(common_headers(api_key))
        .json(&body)
        .send()
        .await
        .map_err(|e| {
            AppError::Internal(format!(
                "Marketing Boost hotel savings card request failed: {}",
                e
            ))
        })?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();

    if status.is_success() {
        tracing::info!("Marketing Boost hotel savings card sent for {}", email);
        serde_json::from_str(&text).map_err(|e| {
            AppError::Internal(format!("Failed to parse Marketing Boost response: {}", e))
        })
    } else {
        Err(AppError::Internal(format!(
            "Marketing Boost hotel savings card returned {}: {}",
            status, text
        )))
    }
}

/// Send a Vacation Incentive via Marketing Boost.
pub async fn send_vacation_incentive(
    client: &reqwest::Client,
    api_key: &str,
    sender: &str,
    business: &str,
    first_name: &str,
    last_name: &str,
    email: &str,
    phone: Option<String>,
    countrycode: Option<String>,
    destination_id: u32,
    campaign_name: &str,
) -> Result<Value, AppError> {
    let body = VacationIncentiveRequest {
        sender: sender.to_string(),
        business: business.to_string(),
        destination: destination_id,
        name: format!("{} {}", first_name, last_name),
        email: email.to_string(),
        countrycode,
        phone,
        message: campaign_name.to_string(),
    };

    let url = format!("{}/vacation-incentives/send", BASE_URL);
    let resp = client
        .post(&url)
        .headers(common_headers(api_key))
        .json(&body)
        .send()
        .await
        .map_err(|e| {
            AppError::Internal(format!(
                "Marketing Boost vacation incentive request failed: {}",
                e
            ))
        })?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();

    if status.is_success() {
        tracing::info!("Marketing Boost vacation incentive sent for {}", email);
        serde_json::from_str(&text).map_err(|e| {
            AppError::Internal(format!("Failed to parse Marketing Boost response: {}", e))
        })
    } else {
        Err(AppError::Internal(format!(
            "Marketing Boost vacation incentive returned {}: {}",
            status, text
        )))
    }
}

/// Fetch the destination list from Marketing Boost.
/// Results are cached in memory per sender for 1 hour.
pub async fn fetch_destinations(
    client: &reqwest::Client,
    api_key: &str,
    sender: &str,
) -> Result<Value, AppError> {
    // Check cache first
    {
        let cache = DESTINATION_CACHE.lock().await;
        if let Some(entry) = cache.get(sender) {
            if entry.is_fresh() {
                tracing::debug!(
                    "Returning cached Marketing Boost destinations for sender {}",
                    sender
                );
                return Ok(entry.destinations.clone());
            }
        }
    }

    // Fetch from API
    let url = format!("{}/all-destination-list/{}", BASE_URL, sender);
    let resp = client
        .get(&url)
        .headers(common_headers(api_key))
        .send()
        .await
        .map_err(|e| {
            AppError::Internal(format!(
                "Marketing Boost destination list request failed: {}",
                e
            ))
        })?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();

    if !status.is_success() {
        return Err(AppError::Internal(format!(
            "Marketing Boost destination list returned {}: {}",
            status, text
        )));
    }

    let destinations: Value = serde_json::from_str(&text).map_err(|e| {
        AppError::Internal(format!(
            "Failed to parse Marketing Boost destination list: {}",
            e
        ))
    })?;

    // Update cache
    {
        let mut cache = DESTINATION_CACHE.lock().await;
        cache.insert(
            sender.to_string(),
            DestinationCache {
                destinations: destinations.clone(),
                fetched_at: Instant::now(),
                sender: sender.to_string(),
            },
        );
    }

    tracing::info!("Fetched Marketing Boost destinations for sender {}", sender);
    Ok(destinations)
}

/// Invalidate the destination cache for a sender (useful for testing/admin).
pub async fn invalidate_destination_cache(sender: &str) {
    let mut cache = DESTINATION_CACHE.lock().await;
    cache.remove(sender);
    tracing::debug!(
        "Invalidated Marketing Boost destination cache for sender {}",
        sender
    );
}
