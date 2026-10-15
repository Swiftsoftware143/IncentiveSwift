//! The LOYALTY-PLAN arms of this app's ONE Stripe receiver.
//!
//! These two functions used to be the second Stripe door, `POST /api/v1/loyalty/webhook/stripe`,
//! registered beside `POST /api/v1/webhooks/stripe` (`billing::webhooks`). Both doors called the
//! same `lookup_webhook_secrets(&s, "stripe")` over the same rows — but the console (and the
//! database: `payment_providers_account_provider_type_key UNIQUE (account_id, provider_type)`)
//! stores exactly ONE webhook signing secret per account, while Stripe issues a DISTINCT signing
//! secret per registered endpoint. Two Stripe endpoints on one account could therefore never both
//! be verified by the one secret the operator can paste, and the address that did not hold it
//! refused every real delivery (kanban t_b474bd74).
//!
//! DECISION (kanban t_b474bd74): the two-receiver model is reduced to ONE address. Stripe's model
//! is one endpoint per URL and one signing secret per endpoint, and this app's model is one signing
//! secret per account — so the app supports one Stripe endpoint per account, and that ONE endpoint
//! must complete every Stripe flow: the platform-plan checkout (`checkout_sessions`, arm in
//! `billing::webhooks`), the loyalty-plan checkout (`stripe_checkout_sessions`, the arms below) and
//! a loyalty subscription's cancellation. `POST /api/v1/loyalty/webhook/stripe` is retired; its
//! address is not served at all, so a Stripe endpoint pointed at it is refused at the edge instead
//! of silently dropping purchases.
//!
//! What did NOT change, and must not: the door authenticates the delivery once, before any arm
//! runs (`billing::stripe_signature`, the one shared parser), and every arm resolves the workspace
//! it acts on from the SIGNED event's own provider identity — never from `metadata.account_id` in
//! the request body, which any anonymous caller can write. That was the defect fixed by kanban
//! t_1b8d96f6 (an anonymous POST naming a real account id ran all the way to the money INSERT) and
//! its properties are carried through the single door unchanged:
//!
//! * the workspace comes from the SIGNED event's own session id resolved against
//!   `stripe_checkout_sessions`, and the plan slug and pool size come from that row and
//!   `accounts.loyalty_plan`, so the request body can only ever name a session;
//! * the session is claimed exactly once (`... AND status = 'pending'`), so a duplicate or
//!   replayed delivery cannot credit a pool twice;
//! * a cancellation resolves its workspace from the SIGNED event's own subscription id against
//!   `accounts.subscription_id`, the column the completion arm writes.

use serde_json::Value;
use sqlx::Row;
use uuid::Uuid;

use crate::error::AppError;
use crate::state::AppState;

/// The loyalty-plan arm of `checkout.session.completed`.
///
/// `None` means the event does NOT name a loyalty checkout of ours — no `stripe_checkout_sessions`
/// row carries that session id — so the caller must continue with its other arms. `Some(outcome)`
/// means this IS one of our loyalty sessions and the arm has reported on it: `completed` when the
/// workspace was activated, or one of the `ignored_*` reasons (missing session id, no plan recorded
/// against the session, already completed). Those are ours and must not be reinterpreted by a later
/// arm — `ignored_already_completed` is what makes a replayed delivery a no-op.
pub async fn complete_loyalty_session(
    state: &AppState,
    payload: &Value,
) -> Result<Option<&'static str>, AppError> {
    let session = &payload["data"]["object"];
    let session_id = session["id"].as_str().unwrap_or("").trim();

    if session_id.is_empty() {
        tracing::warn!("Loyalty webhook: checkout.session.completed without a session id");
        return Ok(Some("ignored_missing_session_id"));
    }

    // ── Resolve the workspace from the DATABASE, never from the request body ────────────────────
    // The session row was written by this app's own checkout before the customer paid, so it is the
    // only trustworthy statement of who bought what.
    let row = sqlx::query(
        "SELECT s.account_id, s.credits, a.loyalty_plan \
         FROM stripe_checkout_sessions s JOIN accounts a ON a.id = s.account_id \
         WHERE s.stripe_session_id = $1",
    )
    .bind(session_id)
    .fetch_optional(&state.db)
    .await?;

    let Some(row) = row else {
        // Not a loyalty session at all: another arm owns this event (or nothing does).
        return Ok(None);
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
        return Ok(Some("ignored_no_plan"));
    };

    // ── Claim the session exactly once, then activate ───────────────────────────────────────────
    let claimed = sqlx::query(
        "UPDATE stripe_checkout_sessions SET status = 'completed', completed_at = NOW(), webhook_raw = $1 \
         WHERE stripe_session_id = $2 AND status = 'pending'",
    )
    .bind(payload)
    .bind(session_id)
    .execute(&state.db)
    .await?;

    if claimed.rows_affected() == 0 {
        tracing::info!(
            %account_id,
            stripe_session_id = %session_id,
            "Loyalty webhook: session was already completed — no second credit"
        );
        return Ok(Some("ignored_already_completed"));
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
    .execute(&state.db)
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
    .execute(&state.db)
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
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()
    {
        crate::handlers::plans_handler::attribute_plan_upgrade(state, account_id, plan_id).await;
    }

    Ok(Some("completed"))
}

/// The loyalty-plan arm of `customer.subscription.deleted`.
pub async fn cancel_loyalty_subscription(
    state: &AppState,
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
    .execute(&state.db)
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The subscription id is read from the SIGNED event object and never invented: a blank or
    /// absent one leaves the stored value alone (`COALESCE($4, subscription_id)`).
    #[test]
    fn reads_the_subscription_id_from_the_event_object_only() {
        assert_eq!(
            subscription_of(&json!({"id": "cs_1", "subscription": "sub_123"})),
            Some("sub_123")
        );
        // Padded values are trimmed, and a blank one is "no subscription" rather than an empty id.
        assert_eq!(
            subscription_of(&json!({"subscription": "  sub_456  "})),
            Some("sub_456")
        );
        assert_eq!(subscription_of(&json!({"subscription": "   "})), None);
        assert_eq!(subscription_of(&json!({"id": "cs_1"})), None);
        assert_eq!(subscription_of(&json!({"subscription": 42})), None);
    }
}
