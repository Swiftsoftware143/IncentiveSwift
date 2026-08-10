//! Stripe webhook handler — activates loyalty subscriptions
//! POST /api/v1/loyalty/webhook/stripe
//! Called by Stripe when checkout.session.completed fires

use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde_json::Value;

use crate::state::AppState; use crate::error::AppError;

pub async fn stripe_webhook(
    State(s): State<AppState>,
    body: String,
) -> Result<impl IntoResponse, AppError> {
    let payload: Value = serde_json::from_str(&body)
        .map_err(|e| AppError::BadRequest(format!("Invalid JSON: {}", e)))?;

    let event_type = payload["type"].as_str().unwrap_or("");

    match event_type {
        "checkout.session.completed" => handle_checkout_completed(&s, &payload).await?,
        "customer.subscription.deleted" => handle_subscription_cancelled(&s, &payload).await?,
        _ => {
            tracing::info!("Unhandled Stripe event: {}", event_type);
        }
    }

    Ok((StatusCode::OK, Json(serde_json::json!({"received": true}))))
}

async fn handle_checkout_completed(s: &AppState, payload: &Value) -> Result<(), AppError> {
    let session = &payload["data"]["object"];
    let session_id = session["id"].as_str().unwrap_or("");
    let metadata = &session["metadata"];
    let account_id = metadata["account_id"].as_str().unwrap_or("");
    let plan_slug = metadata["plan_slug"].as_str().unwrap_or("");
    let monthly_zc_pool: i32 = metadata["monthly_zc_pool"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    if account_id.is_empty() || session_id.is_empty() {
        tracing::warn!("Webhook missing account_id or session_id");
        return Ok(());
    }

    // Mark checkout session as completed
    sqlx::query(
        "UPDATE stripe_checkout_sessions SET status = 'completed', completed_at = NOW(), webhook_raw = $1 WHERE stripe_session_id = $2"
    )
    .bind(payload)
    .bind(session_id)
    .execute(&s.db)
    .await?;

    // Activate the loyalty plan
    sqlx::query(
        "UPDATE accounts SET loyalty_plan = $1, loyalty_plan_status = 'active', zc_pool_remaining = $2, zc_pool_total = $2, pool_reset_date = (NOW()::date + INTERVAL '30 days'), updated_at = NOW() WHERE id = $3::uuid"
    )
    .bind(plan_slug)
    .bind(monthly_zc_pool)
    .bind(account_id)
    .execute(&s.db)
    .await?;

    // Log the credit allocation
    sqlx::query(
        "INSERT INTO credit_transactions (account_id, amount, transaction_type, description, balance_after, created_at)
         VALUES ($1::uuid, $2, 'subscription_renewal', $3, $2, NOW())"
    )
    .bind(account_id)
    .bind(monthly_zc_pool)
    .bind(format!("Monthly ZC pool credited for {} plan", plan_slug))
    .execute(&s.db)
    .await?;

    tracing::info!(
        "Loyalty plan activated: account={} plan={} zc_pool={}",
        account_id, plan_slug, monthly_zc_pool
    );

    Ok(())
}

async fn handle_subscription_cancelled(s: &AppState, payload: &Value) -> Result<(), AppError> {
    let subscription = &payload["data"]["object"];
    let subscription_id = subscription["id"].as_str().unwrap_or("");

    if subscription_id.is_empty() {
        return Ok(());
    }

    // Deactivate the loyalty plan
    sqlx::query(
        "UPDATE accounts SET loyalty_plan_status = 'cancelled', updated_at = NOW() WHERE subscription_id = $1"
    )
    .bind(subscription_id)
    .execute(&s.db)
    .await?;

    tracing::info!("Loyalty plan cancelled: subscription={}", subscription_id);
    Ok(())
}
