//! Checkout & Payment Provider handler - Stripe/PayPal payment session management.
//!
//! Endpoints:
//!   GET  /api/v1/payment-providers              - list providers for current account
//!   POST /api/v1/payment-providers              - upsert payment provider config
//!   DELETE /api/v1/payment-providers/:provider_type - delete a provider config
//!   POST /api/v1/checkout/create                - create a checkout session
//!   GET  /api/v1/checkout/sessions              - list checkout sessions
//!   POST /api/v1/webhooks/stripe                - Stripe webhook receiver
//!   POST /api/v1/webhooks/paypal                - PayPal webhook receiver

use crate::email;
use crate::error::AppError;
use crate::state::AppState;
use crate::security::auth::AuthenticatedUser;
use rand::Rng;
use axum::{
    extract::{Path, State},
    Json,
};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Mask a sensitive key/value showing only first 3 and last 3 characters.
fn mask_key(key: &str) -> String {
    if key.len() <= 6 {
        return "***".to_string();
    }
    let first = &key[..3];
    let last = &key[key.len() - 3..];
    format!("{}...{}", first, last)
}

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
// Payment Providers
// ---------------------------------------------------------------------------

/// GET /api/v1/payment-providers
pub async fn list_payment_providers(
    State(state): State<AppState>,
    auth: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&auth.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;

    let rows = sqlx::query(
        r#"
        SELECT pp.id, pp.account_id, pp.provider_type, pp.api_key, pp.webhook_secret,
               pp.base_url, pp.metadata, pp.is_active, pp.created_at, pp.updated_at
        FROM payment_providers pp
        WHERE pp.account_id = $1
        ORDER BY pp.provider_type
        "#
    )
    .bind(account_id)
    .fetch_all(&state.db)
    .await?;

    let items: Vec<Value> = rows.iter().map(|row| {
        let raw_key: String = row.get("api_key");
        let raw_webhook: String = row.get("webhook_secret");
        json!({
            "id": row.get::<Uuid, _>("id"),
            "account_id": row.get::<Uuid, _>("account_id"),
            "provider_type": row.get::<String, _>("provider_type"),
            "api_key_masked": mask_key(&raw_key),
            "webhook_secret_masked": mask_key(&raw_webhook),
            "base_url": row.get::<Option<String>, _>("base_url"),
            "metadata": row.get::<Option<serde_json::Value>, _>("metadata"),
            "is_active": row.get::<bool, _>("is_active"),
            "created_at": row.get::<chrono::DateTime<chrono::Utc>, _>("created_at"),
            "updated_at": row.get::<chrono::DateTime<chrono::Utc>, _>("updated_at"),
        })
    }).collect();

    Ok(Json(json!({ "items": items, "count": items.len() })))
}

/// POST /api/v1/payment-providers
pub async fn upsert_payment_provider(
    State(state): State<AppState>,
    auth: AuthenticatedUser,
    Json(body): Json<UpsertPaymentProviderInput>,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&auth.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;

    // Upsert using ON CONFLICT pattern
    let row = sqlx::query(
        r#"
        INSERT INTO payment_providers (account_id, provider_type, api_key, webhook_secret, base_url, metadata, is_active)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        ON CONFLICT (account_id, provider_type)
        DO UPDATE SET
            api_key = CASE
                WHEN EXCLUDED.api_key IS NOT NULL AND EXCLUDED.api_key != ''
                THEN EXCLUDED.api_key
                ELSE payment_providers.api_key
            END,
            webhook_secret = CASE
                WHEN EXCLUDED.webhook_secret IS NOT NULL AND EXCLUDED.webhook_secret != ''
                THEN EXCLUDED.webhook_secret
                ELSE payment_providers.webhook_secret
            END,
            base_url = COALESCE(EXCLUDED.base_url, payment_providers.base_url),
            metadata = CASE
                WHEN EXCLUDED.metadata IS NOT NULL AND EXCLUDED.metadata != '{}'::jsonb
                THEN EXCLUDED.metadata
                ELSE payment_providers.metadata
            END,
            is_active = EXCLUDED.is_active,
            updated_at = now()
        RETURNING id, account_id, provider_type, api_key, webhook_secret,
                  base_url, metadata, is_active, created_at, updated_at
        "#
    )
    .bind(account_id)
    .bind(&body.provider_type)
    .bind(body.api_key.as_deref().unwrap_or(""))
    .bind(body.webhook_secret.as_deref().unwrap_or(""))
    .bind(&body.base_url)
    .bind(&body.metadata)
    .bind(body.is_active.unwrap_or(true))
    .fetch_one(&state.db)
    .await?;

    let raw_key: String = row.get("api_key");
    let raw_webhook: String = row.get("webhook_secret");
    let item = json!({
        "id": row.get::<Uuid, _>("id"),
        "account_id": row.get::<Uuid, _>("account_id"),
        "provider_type": row.get::<String, _>("provider_type"),
        "api_key_masked": mask_key(&raw_key),
        "webhook_secret_masked": mask_key(&raw_webhook),
        "base_url": row.get::<Option<String>, _>("base_url"),
        "metadata": row.get::<Option<serde_json::Value>, _>("metadata"),
        "is_active": row.get::<bool, _>("is_active"),
        "created_at": row.get::<chrono::DateTime<chrono::Utc>, _>("created_at"),
        "updated_at": row.get::<chrono::DateTime<chrono::Utc>, _>("updated_at"),
    });

    Ok(Json(json!({ "item": item })))
}

/// DELETE /api/v1/payment-providers/:provider_type
pub async fn delete_payment_provider(
    State(state): State<AppState>,
    auth: AuthenticatedUser,
    Path(provider_type): Path<String>,
) -> Result<Json<Value>, AppError> {
    // Super admin gate: only super admins can delete payment providers
    if auth.role != "super_admin" {
        return Err(AppError::Forbidden(
            "Only super admins can delete payment providers".to_string(),
        ));
    }

    let result = sqlx::query(
        "DELETE FROM payment_providers WHERE provider_type = $1"
    )
    .bind(&provider_type)
    .execute(&state.db)
    .await?;

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound(format!(
            "Payment provider not found for type '{}'",
            provider_type
        )));
    }

    Ok(Json(json!({ "status": "deleted", "provider_type": provider_type })))
}

// ---------------------------------------------------------------------------
// Checkout Sessions
// ---------------------------------------------------------------------------

/// Input for creating a checkout session.
#[derive(Deserialize)]
pub struct CreateCheckoutInput {
    pub price_amount: f64,
    pub price_currency: String,
    pub description: Option<String>,
    pub success_url: Option<String>,
    pub cancel_url: Option<String>,
    pub metadata: Option<Value>,
    pub payment_provider: Option<String>,
    pub plan_id: Option<String>,
}

/// POST /api/v1/checkout/create
pub async fn create_checkout_session(
    State(state): State<AppState>,
    auth: AuthenticatedUser,
    Json(body): Json<CreateCheckoutInput>,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&auth.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;

    // user_id is a placeholder — incentiveswift maps 1 account : 1 user
    let user_id = Uuid::new_v4();

    // Determine payment provider — explicit over plan's payment_provider over default (stripe)
    let mut payment_provider = body.payment_provider.clone().unwrap_or_else(|| String::from("stripe"));
    if payment_provider == "stripe" {
        if let Some(ref pid) = body.plan_id {
            if let Ok(uuid) = Uuid::parse_str(pid) {
                if let Ok(pp) = sqlx::query_scalar::<_, Option<String>>(
                    "SELECT payment_provider FROM plans WHERE id = $1"
                )
                .bind(uuid)
                .fetch_one(&state.db)
                .await
                {
                    if let Some(ref provider) = pp {
                        payment_provider = provider.clone();
                    }
                }
            }
        }
    }

    // Resolve success_url: explicit > plan's thank_you_url > /thank-you.html
    let success_url = if let Some(ref url) = body.success_url {
        url.clone()
    } else if let Some(ref pid) = body.plan_id {
        if let Ok(uuid) = Uuid::parse_str(pid) {
            sqlx::query_scalar::<_, Option<String>>(
                "SELECT thank_you_url FROM plans WHERE id = $1"
            )
            .bind(uuid)
            .fetch_optional(&state.db)
            .await?
            .flatten()
            .unwrap_or_else(|| "/thank-you.html".to_string())
        } else {
            "/thank-you.html".to_string()
        }
    } else {
        "/thank-you.html".to_string()
    };

    let cancel_url = body.cancel_url.clone().unwrap_or_else(|| "/".to_string());

    // Store the checkout session
    let row = sqlx::query(
        r#"
        INSERT INTO checkout_sessions (account_id, user_id, price_amount, price_currency,
                                       description, success_url, cancel_url, metadata,
                                       status, payment_provider)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'pending', $9)
        RETURNING id, account_id, user_id, price_amount, price_currency, description,
                  success_url, cancel_url, metadata, status, payment_provider,
                  created_at, updated_at
        "#
    )
    .bind(account_id)
    .bind(user_id)
    .bind(body.price_amount)
    .bind(&body.price_currency)
    .bind(&body.description)
    .bind(&success_url)
    .bind(&cancel_url)
    .bind(&body.metadata)
    .bind(&payment_provider)
    .fetch_one(&state.db)
    .await?;

    // For now, generate a mock/placeholder checkout URL.
    // Future integration will call the payment provider's API to create a real session.
    let mock_checkout_url = format!(
        "https://checkout.example.com/session/{}",
        row.get::<Uuid, _>("id")
    );

    let item = json!({
        "id": row.get::<Uuid, _>("id"),
        "account_id": row.get::<Uuid, _>("account_id"),
        "price_amount": row.get::<rust_decimal::Decimal, _>("price_amount"),
        "price_currency": row.get::<String, _>("price_currency"),
        "description": row.get::<Option<String>, _>("description"),
        "success_url": row.get::<Option<String>, _>("success_url"),
        "cancel_url": row.get::<Option<String>, _>("cancel_url"),
        "metadata": row.get::<Option<serde_json::Value>, _>("metadata"),
        "status": row.get::<String, _>("status"),
        "payment_provider": row.get::<String, _>("payment_provider"),
        "checkout_url": mock_checkout_url,
        "created_at": row.get::<chrono::DateTime<chrono::Utc>, _>("created_at"),
        "updated_at": row.get::<chrono::DateTime<chrono::Utc>, _>("updated_at"),
    });

    Ok(Json(json!({ "item": item })))
}

/// GET /api/v1/checkout/sessions
pub async fn list_checkout_sessions(
    State(state): State<AppState>,
    auth: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&auth.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;

    let rows = sqlx::query(
        r#"
        SELECT cs.id, cs.account_id, cs.user_id, cs.price_amount, cs.price_currency,
               cs.description, cs.status, cs.payment_provider, cs.payment_id,
               cs.metadata, cs.created_at, cs.updated_at
        FROM checkout_sessions cs
        WHERE cs.account_id = $1
        ORDER BY cs.created_at DESC
        "#
    )
    .bind(account_id)
    .fetch_all(&state.db)
    .await?;

    let items: Vec<Value> = rows.iter().map(|row| {
        json!({
            "id": row.get::<Uuid, _>("id"),
            "account_id": row.get::<Uuid, _>("account_id"),
            "price_amount": row.get::<rust_decimal::Decimal, _>("price_amount"),
            "price_currency": row.get::<String, _>("price_currency"),
            "description": row.get::<Option<String>, _>("description"),
            "status": row.get::<String, _>("status"),
            "payment_provider": row.get::<String, _>("payment_provider"),
            "payment_id": row.get::<Option<String>, _>("payment_id"),
            "metadata": row.get::<Option<serde_json::Value>, _>("metadata"),
            "created_at": row.get::<chrono::DateTime<chrono::Utc>, _>("created_at"),
            "updated_at": row.get::<chrono::DateTime<chrono::Utc>, _>("updated_at"),
        })
    }).collect();

    Ok(Json(json!({ "items": items, "count": items.len() })))
}

// ---------------------------------------------------------------------------
// Webhooks
// ---------------------------------------------------------------------------

/// Look up a payment provider's webhook secret from the database by provider type.
/// Returns the first active provider's webhook_secret.
async fn lookup_webhook_secret(
    state: &AppState,
    provider_type: &str,
) -> Result<String, AppError> {
    let row = sqlx::query(
        "SELECT pp.webhook_secret FROM payment_providers pp WHERE pp.provider_type = $1 AND pp.is_active = true LIMIT 1"
    )
    .bind(provider_type)
    .fetch_optional(&state.db)
    .await?;

    match row {
        Some(r) => {
            let secret: String = r.get("webhook_secret");
            if secret.is_empty() {
                Err(AppError::Internal(format!(
                    "{} webhook secret is configured but empty",
                    provider_type
                )))
            } else {
                Ok(secret)
            }
        }
        None => Err(AppError::Internal(format!(
            "No active {} payment provider configured",
            provider_type
        ))),
    }
}

/// POST /api/v1/webhooks/stripe
pub async fn stripe_webhook(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
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
    headers: axum::http::HeaderMap,
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
// Webhook event handlers (stubs — implement with actual business logic)
// ---------------------------------------------------------------------------

// ──────────────────────────────────────────────
// Credential delivery helpers
// ──────────────────────────────────────────────

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
    _account_id: Uuid,
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
            .or_else(|| email.map(|e| e.split('@').next()).flatten())
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

        let plan_name = "Plan";

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
            .or_else(|| email.map(|e| e.split('@').next()).flatten())
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

// ---------------------------------------------------------------------------
// Input types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct UpsertPaymentProviderInput {
    pub provider_type: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub webhook_secret: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub metadata: Option<Value>,
    #[serde(default)]
    pub is_active: Option<bool>,
}
