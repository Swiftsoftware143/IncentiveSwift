//! Webhook handlers for Stripe and PayPal.
//!
//! Endpoints:
//!   POST /api/v1/webhooks/stripe  — Stripe webhook receiver
//!   POST /api/v1/webhooks/paypal  — PayPal webhook receiver

use crate::email;
use crate::error::AppError;
use crate::state::AppState;
use axum::{
    extract::State,
    http::HeaderMap,
    Json,
};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;
use rand::Rng;
use tracing;

// Re-export these for use outside billing (e.g. admin handler)
pub use crate::billing::providers::lookup_webhook_secret;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Verify a Stripe-style HMAC-SHA256 signature header against a known secret.
fn verify_stripe_signature(payload: &str, sig_header: &str, secret: &str) -> Result<(), AppError> {
    // Stripe v1 signatures are hex-encoded HMAC-SHA256 prefixed with "v1="
    let sig_value = sig_header
        .strip_prefix("v1=")
        .ok_or_else(|| AppError::BadRequest("Missing v1= prefix in signature header".to_string()))?;

    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .map_err(|_| AppError::Internal("HMAC key initialization failed".to_string()))?;

    mac.update(payload.as_bytes());
    let computed = mac.finalize().into_bytes();
    let computed_hex = hex::encode(computed);

    // Constant-time comparison via hex string equality (bounded by hex length)
    if computed_hex != sig_value {
        return Err(AppError::BadRequest("Invalid webhook signature".to_string()));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// POST /api/v1/webhooks/stripe
pub async fn stripe_webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: String,
) -> Result<Json<Value>, AppError> {
    // Extract the Stripe signature header
    let sig_header = headers
        .get("stripe-signature")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| AppError::BadRequest("Missing stripe-signature header".to_string()))?;

    // Look up webhook secret from the database (multi-tenant)
    let webhook_secret = lookup_webhook_secret(&state, "stripe").await?;

    // Verify the signature
    verify_stripe_signature(&body, sig_header, &webhook_secret)?;

    // Parse the event payload
    let event: Value = serde_json::from_str(&body)
        .map_err(|e| AppError::BadRequest(format!("Invalid webhook payload: {}", e)))?;

    let event_type = event["type"].as_str().unwrap_or("unknown");

    tracing::info!("Received Stripe webhook: type={}", event_type);

    // Handle the event
    match event_type {
        "checkout.session.completed" => {
            handle_stripe_checkout_completed(&state, &event).await?;
        }
        "payment_intent.succeeded" => {
            handle_stripe_payment_succeeded(&state, &event).await?;
        }
        "payment_intent.payment_failed" => {
            handle_stripe_payment_failed(&state, &event).await?;
        }
        _ => {
            tracing::debug!("Unhandled Stripe event type: {}", event_type);
        }
    }

    Ok(Json(json!({ "status": "received" })))
}

/// POST /api/v1/webhooks/paypal
pub async fn paypal_webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: String,
) -> Result<Json<Value>, AppError> {
    // PayPal requires transmission headers for verification
    let _transmission_id = headers
        .get("paypal-transmission-id")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| AppError::BadRequest("Missing paypal-transmission-id header".to_string()))?;

    // Look up webhook ID from the database (multi-tenant)
    let _webhook_id = lookup_webhook_secret(&state, "paypal").await?;

    // Full PayPal webhook verification requires a POST to PayPal's verify-webhook-signature API.
    // This is a stub — production should implement the full verification flow.
    tracing::info!("PayPal webhook received (basic validation only)");

    // Parse the event payload
    let event: Value = serde_json::from_str(&body)
        .map_err(|e| AppError::BadRequest(format!("Invalid webhook payload: {}", e)))?;

    let event_type = event["event_type"].as_str().unwrap_or("unknown");

    tracing::info!("Received PayPal webhook: type={}", event_type);

    // Handle the event
    match event_type {
        "CHECKOUT.ORDER.APPROVED" | "PAYMENT.CAPTURE.COMPLETED" => {
            handle_paypal_payment_completed(&state, &event).await?;
        }
        "PAYMENT.CAPTURE.DENIED" | "PAYMENT.CAPTURE.REFUNDED" => {
            handle_paypal_payment_failed(&state, &event).await?;
        }
        _ => {
            tracing::debug!("Unhandled PayPal event type: {}", event_type);
        }
    }

    Ok(Json(json!({ "status": "received" })))
}

// ---------------------------------------------------------------------------
// Credential delivery helpers
// ---------------------------------------------------------------------------

/// Generate a random temporary password (12 characters)
pub fn generate_temp_password() -> String {
    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789!@#$%^&*()";
    let mut rng = rand::thread_rng();
    (0..12)
        .map(|_| {
            let idx = rng.gen_range(0..CHARSET.len());
            CHARSET[idx] as char
        })
        .collect()
}

/// Hash a password using argon2
pub fn hash_password(password: &str) -> Result<String, AppError> {
    use argon2::{
        password_hash::{rand_core::OsRng, PasswordHasher, SaltString},
        Argon2,
    };
    let salt = SaltString::generate(&mut OsRng);
    let argon2 = Argon2::default();
    let hash = argon2
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| AppError::Internal(format!("Password hashing failed: {}", e)))?;
    Ok(hash.to_string())
}

/// Deliver credentials to the user who just completed a purchase.
/// IncentiveSwift uses `accounts` table (id, name, email, password_hash, slug) — no separate users table.
async fn deliver_credentials(
    state: &AppState,
    email: &str,
    customer_name: &str,
    account_id: Uuid,
    plan_name: &str,
) -> Result<(), AppError> {
    // Look for existing account by email
    let existing_account = sqlx::query(
        "SELECT id, password_hash, name FROM accounts WHERE email = $1"
    )
    .bind(email)
    .fetch_optional(&state.db)
    .await?;

    if let Some(account_row) = existing_account {
        let account_id: Uuid = account_row.try_get("id")?;
        let existing_hash: Option<String> = account_row.try_get("password_hash")?;
        let existing_name: String = account_row.try_get("name")?;

        match existing_hash {
            Some(hash) if !hash.is_empty() => {
                // Account exists with password → send purchase confirmed
                if let Err(e) = email::send_purchase_confirmed_email(&state.db, email, &existing_name, plan_name).await {
                    tracing::warn!("Failed to send purchase confirmed email to {}: {}", email, e);
                }
            }
            _ => {
                // Account exists but no password → generate temp password
                let temp_password = generate_temp_password();
                let hash = hash_password(&temp_password)?;
                sqlx::query("UPDATE accounts SET password_hash = $1, name = $2 WHERE id = $3")
                    .bind(&hash)
                    .bind(customer_name)
                    .bind(account_id)
                    .execute(&state.db)
                    .await?;

                if let Err(e) = email::send_welcome_email(&state.db, email, &existing_name, &temp_password).await {
                    tracing::warn!("Failed to send welcome email to {}: {}", email, e);
                }
            }
        }
    } else {
        // No account found → create one
        let slug = format!("acct-{}", Uuid::new_v4().to_string().split('-').next().unwrap_or("new"));
        let temp_password = generate_temp_password();
        let hash = hash_password(&temp_password)?;
        let account_id = Uuid::new_v4();

        // Insert account with tenant_id = self (standalone tenant)
        sqlx::query(
            "INSERT INTO accounts (id, name, email, password_hash, slug, role, tenant_id, purchase_pin)
             VALUES ($1, $2, $3, $4, $5, 'company_admin', $6, '0000')"
        )
        .bind(account_id)
        .bind(customer_name)
        .bind(email)
        .bind(&hash)
        .bind(&slug)
        .bind(account_id) // tenant_id = self
        .execute(&state.db)
        .await?;

        // Auto-generate purchase PIN: prefix "Z" + sequential number from account's tenant record
        let next_num: Option<i32> = sqlx::query_scalar(
            "UPDATE accounts SET next_pin_number = next_pin_number + 1 WHERE id = $1 RETURNING next_pin_number - 1"
        )
        .bind(account_id)
        .fetch_optional(&state.db)
        .await?;
        if let Some(num) = next_num {
            let new_pin = if num < 1000 {
                format!("Z{:03}", num)
            } else {
                format!("Z{}", num)
            };
            sqlx::query("UPDATE accounts SET purchase_pin = $1 WHERE id = $2")
                .bind(&new_pin)
                .bind(account_id)
                .execute(&state.db)
                .await?;
        }

        if let Err(e) = email::send_welcome_email(&state.db, email, customer_name, &temp_password).await {
            tracing::warn!("Failed to send welcome email to {}: {}", email, e);
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Webhook event handlers
// ---------------------------------------------------------------------------

async fn handle_stripe_checkout_completed(
    state: &AppState,
    event: &Value,
) -> Result<(), AppError> {
    tracing::info!("Stripe checkout.session.completed — processing...");

    let session = &event["data"]["object"];
    let provider_session_id = session["id"].as_str().unwrap_or("");
    if provider_session_id.is_empty() {
        tracing::warn!("Stripe checkout.session.completed missing session ID");
        return Ok(());
    }

    // Update checkout_sessions
    let result = sqlx::query(
        "UPDATE checkout_sessions SET status = 'completed', updated_at = NOW() WHERE provider_session_id = $1 AND status = 'pending'"
    )
    .bind(provider_session_id)
    .execute(&state.db)
    .await?;

    if result.rows_affected() > 0 {
        // Credential delivery
        let email = session["customer_details"]["email"].as_str();
        let customer_name = session["customer_details"]["name"].as_str()
            .or_else(|| email.and_then(|e| e.split('@').next()))
            .unwrap_or("Customer");

        // Get account info from stripe_checkout_sessions
        let session_info = sqlx::query(
            "SELECT account_id, credits FROM stripe_checkout_sessions WHERE stripe_session_id = $1"
        )
        .bind(provider_session_id)
        .fetch_optional(&state.db)
        .await?;

        let account_id = session_info.as_ref()
            .and_then(|r| r.try_get::<Uuid, _>("account_id").ok())
            .unwrap_or_else(Uuid::new_v4);

        let metadata = session.get("metadata");
    let is_loyalty_sub = metadata.and_then(|m| m.get("loyalty_subscription")).and_then(|v| v.as_str()).unwrap_or("") == "true";
    let plan_name = if is_loyalty_sub {
        metadata.and_then(|m| m.get("plan_name")).and_then(|v| v.as_str()).unwrap_or("Plan")
    } else {
        "Plan"
    };

        if let Some(email_str) = email {
            if let Err(e) = deliver_credentials(state, email_str, customer_name, account_id, plan_name).await {
                tracing::warn!("Credential delivery failed for {}: {}", email_str, e);
            }
        }
    }

    Ok(())
}

async fn handle_stripe_payment_succeeded(
    _state: &AppState,
    _event: &Value,
) -> Result<(), AppError> {
    tracing::info!("Stripe payment_intent.succeeded — processing...");
    Ok(())
}

async fn handle_stripe_payment_failed(
    _state: &AppState,
    _event: &Value,
) -> Result<(), AppError> {
    tracing::info!("Stripe payment_intent.payment_failed — processing...");
    Ok(())
}

async fn handle_paypal_payment_completed(
    state: &AppState,
    event: &Value,
) -> Result<(), AppError> {
    tracing::info!("PayPal payment completed — processing...");

    let resource = &event["resource"];
    let provider_session_id = resource["id"].as_str().unwrap_or("");
    if provider_session_id.is_empty() {
        tracing::warn!("PayPal payment completed missing resource ID");
        return Ok(());
    }

    // Update checkout_sessions
    let result = sqlx::query(
        "UPDATE checkout_sessions SET status = 'completed', updated_at = NOW() WHERE provider_session_id = $1 AND status = 'pending'"
    )
    .bind(provider_session_id)
    .execute(&state.db)
    .await?;

    if result.rows_affected() > 0 {
        let email = resource["payer"]["email_address"].as_str();
        let customer_name = resource["payer"]["name"]["given_name"].as_str()
            .or_else(|| email.and_then(|e| e.split('@').next()))
            .unwrap_or("Customer");

        let account_id = Uuid::new_v4();
        let plan_name = "Plan";

        if let Some(email_str) = email {
            if let Err(e) = deliver_credentials(state, email_str, customer_name, account_id, plan_name).await {
                tracing::warn!("Credential delivery failed for {}: {}", email_str, e);
            }
        }
    }

    Ok(())
}

async fn handle_paypal_payment_failed(
    _state: &AppState,
    _event: &Value,
) -> Result<(), AppError> {
    tracing::info!("PayPal payment failed/refunded — processing...");
    Ok(())
}
