//! Loyalty-plan Stripe receiver — activates a workspace's loyalty subscription.
//! POST /api/v1/loyalty/webhook/stripe
//!
//! The second Stripe door in this app. It is the receiver for the plan checkout created by
//! `handlers::loyalty_plans::subscribe` (a real Stripe Checkout session, stored in
//! `stripe_checkout_sessions` as `pending`), and it is listed as public in
//! `src/security/route_policy.rs` because a payment provider presents no credential of ours.
//!
//! It verified NOTHING before kanban t_1b8d96f6: it read no signature header at all and decided
//! which workspace to credit from `data.object.metadata.account_id` — a value the CALLER put in the
//! request body. Measured on the deployed binary 2026-10-14: an anonymous POST naming an account id
//! ran all the way to the money INSERT (`credit_transactions` FK error for an id that did not
//! exist), so with a real account id — which is not a secret, it is in every JWT this app issues —
//! any caller could hand a workspace a loyalty plan and a ZC pool with no credential of any kind.
//!
//! The door is now authenticated the same way as the payment receiver:
//!
//! * the credential is the workspace's own stored webhook signing secret, looked up across every
//!   active `stripe` row (`billing::providers::lookup_webhook_secrets`) and verified over the bytes
//!   Stripe signed by the one shared parser (`billing::stripe_signature`); no active secret signed
//!   the delivery -> `400` and nothing is read;
//! * the workspace acted on comes from the SIGNED event's own provider session id resolved against
//!   `stripe_checkout_sessions`, never from `metadata.account_id`, and the plan slug and the pool
//!   size come from that row and `accounts.loyalty_plan` — so the request body can only ever name a
//!   session, and only the row that session belongs to can move;
//! * the session is claimed exactly once (`... AND status = 'pending'`), so a duplicate or replayed
//!   delivery cannot credit a pool twice.

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use serde_json::Value;
use sqlx::Row;
use uuid::Uuid;

use crate::billing::providers::lookup_webhook_secrets;
use crate::billing::stripe_signature::verify_stripe_signature_any;
use crate::error::AppError;
use crate::state::AppState;

pub async fn stripe_webhook(
    State(s): State<AppState>,
    headers: HeaderMap,
    body: String,
) -> Result<impl IntoResponse, AppError> {
    // ── 1. Authenticate the delivery ────────────────────────────────────────────────────────────
    // The raw body is verified BEFORE it is parsed, because the signature covers these exact bytes.
    let sig_header = headers
        .get("stripe-signature")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| AppError::BadRequest("Missing stripe-signature header".to_string()))?;

    let credentials = lookup_webhook_secrets(&s, "stripe").await?;
    // Genuine AND not a captured delivery replayed later: the `t=` stamp's age is judged against this
    // host's clock under the configured tolerance, so a recorded header + body stops being a bearer
    // credential (kanban t_d9fb9e1b).
    let verified_for = verify_stripe_signature_any(
        &body,
        sig_header,
        &credentials,
        s.config.stripe_webhook_tolerance_secs,
    )?;
    tracing::info!(
        verified_account = %verified_for,
        active_secrets = credentials.len(),
        "Loyalty Stripe webhook signature verified"
    );

    let payload: Value = serde_json::from_str(&body)
        .map_err(|e| AppError::BadRequest(format!("Invalid JSON: {}", e)))?;

    let event_type = payload["type"].as_str().unwrap_or("");

    let outcome = match event_type {
        "checkout.session.completed" => handle_checkout_completed(&s, &payload).await?,
        "customer.subscription.deleted" => handle_subscription_cancelled(&s, &payload).await?,
        _ => {
            tracing::info!("Unhandled Stripe event: {}", event_type);
            "ignored_unhandled_event_type"
        }
    };

    // 200 for anything the signature authenticated, INCLUDING the no-op outcomes: the provider
    // presents a real delivery and must not be told to retry a body that can never change.
    Ok((
        StatusCode::OK,
        Json(serde_json::json!({"received": true, "outcome": outcome})),
    ))
}

/// The subscription that this loyalty plan belongs to, when the signed session carries one.
///
/// Stripe's subscription-mode sessions carry the `sub_…` id here. It is what a later
/// `customer.subscription.deleted` names, so it is stored from the SIGNED event only. `None` (a
/// one-off session) leaves whatever the checkout wrote in place.
fn subscription_of(session: &Value) -> Option<&str> {
    session["subscription"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

async fn handle_checkout_completed(
    s: &AppState,
    payload: &Value,
) -> Result<&'static str, AppError> {
    let session = &payload["data"]["object"];
    let session_id = session["id"].as_str().unwrap_or("").trim();

    if session_id.is_empty() {
        tracing::warn!("Loyalty webhook: checkout.session.completed without a session id");
        return Ok("ignored_missing_session_id");
    }

    // ── 2. Resolve the workspace from the DATABASE, never from the request body ─────────────────
    // The session row was written by this app's own checkout before the customer paid, so it is the
    // only trustworthy statement of who bought what.
    let row = sqlx::query(
        "SELECT s.account_id, s.credits, a.loyalty_plan \
         FROM stripe_checkout_sessions s JOIN accounts a ON a.id = s.account_id \
         WHERE s.stripe_session_id = $1",
    )
    .bind(session_id)
    .fetch_optional(&s.db)
    .await?;

    let Some(row) = row else {
        tracing::warn!(
            stripe_session_id = %session_id,
            "Loyalty webhook: session is not one of our checkout rows — nothing to activate"
        );
        return Ok("ignored_unknown_session");
    };

    let account_id: Uuid = row.try_get("account_id")?;
    // The pool size the checkout recorded for THIS session (column `credits`, written from the
    // plan's `monthly_zc_pool`). The body's own `monthly_zc_pool` is never read.
    let monthly_zc_pool: i32 = row.try_get("credits")?;
    let plan_slug: Option<String> = row.try_get("loyalty_plan")?;

    let Some(plan_slug) = plan_slug.filter(|slug| !slug.trim().is_empty()) else {
        tracing::warn!(
            %account_id,
            stripe_session_id = %session_id,
            "Loyalty webhook: the workspace has no loyalty plan recorded against this session"
        );
        return Ok("ignored_no_plan");
    };

    // ── 3. Claim the session exactly once, then activate ────────────────────────────────────────
    let claimed = sqlx::query(
        "UPDATE stripe_checkout_sessions SET status = 'completed', completed_at = NOW(), webhook_raw = $1 \
         WHERE stripe_session_id = $2 AND status = 'pending'",
    )
    .bind(payload)
    .bind(session_id)
    .execute(&s.db)
    .await?;

    if claimed.rows_affected() == 0 {
        tracing::info!(
            %account_id,
            stripe_session_id = %session_id,
            "Loyalty webhook: session was already completed — no second credit"
        );
        return Ok("ignored_already_completed");
    }

    let subscription_id = subscription_of(session);

    sqlx::query(
        "UPDATE accounts SET loyalty_plan = $1, loyalty_plan_status = 'active', \
         zc_pool_remaining = $2, zc_pool_total = $2, \
         pool_reset_date = (NOW()::date + INTERVAL '30 days'), \
         subscription_id = COALESCE($4, subscription_id), updated_at = NOW() WHERE id = $3::uuid",
    )
    .bind(&plan_slug)
    .bind(monthly_zc_pool)
    .bind(account_id)
    .bind(subscription_id)
    .execute(&s.db)
    .await?;

    // Log the credit allocation.
    sqlx::query(
        // credit_transactions records the kind of movement in `action`; there is no
        // `transaction_type` column (plain-statement drift, kanban t_cf7469bb).
        "INSERT INTO credit_transactions (account_id, amount, action, description, balance_after, created_at)
         VALUES ($1::uuid, $2, 'subscription_renewal', $3, $2, NOW())",
    )
    .bind(account_id)
    .bind(monthly_zc_pool)
    .bind(format!(
        "Monthly ZC pool credited for {} plan",
        plan_slug
    ))
    .execute(&s.db)
    .await?;

    tracing::info!(
        account_id = %account_id,
        stripe_session_id = %session_id,
        plan = %plan_slug,
        zc_pool = monthly_zc_pool,
        "Loyalty plan activated"
    );

    // Credit the referring affiliate for the paid-plan upgrade (permanent, no expiry).
    if let Some(plan_id) = sqlx::query_scalar::<_, Uuid>("SELECT id FROM plans WHERE slug = $1")
        .bind(&plan_slug)
        .fetch_optional(&s.db)
        .await
        .ok()
        .flatten()
    {
        crate::handlers::plans_handler::attribute_plan_upgrade(s, account_id, plan_id).await;
    }

    Ok("completed")
}

async fn handle_subscription_cancelled(
    s: &AppState,
    payload: &Value,
) -> Result<&'static str, AppError> {
    let subscription = &payload["data"]["object"];
    let subscription_id = subscription["id"].as_str().unwrap_or("").trim();

    if subscription_id.is_empty() {
        tracing::warn!("Loyalty webhook: subscription.deleted without a subscription id");
        return Ok("ignored_missing_subscription_id");
    }

    // The workspace is resolved from the signed event's own subscription id against the column the
    // completion arm above writes, never from a body-supplied account id.
    let result = sqlx::query(
        "UPDATE accounts SET loyalty_plan_status = 'cancelled', updated_at = NOW() \
         WHERE subscription_id = $1",
    )
    .bind(subscription_id)
    .execute(&s.db)
    .await?;

    if result.rows_affected() == 0 {
        tracing::warn!(
            subscription_id = %subscription_id,
            "Loyalty webhook: no workspace holds this subscription id — nothing to cancel"
        );
        return Ok("ignored_unknown_subscription");
    }

    tracing::info!(subscription_id = %subscription_id, "Loyalty plan cancelled");
    Ok("cancelled")
}
