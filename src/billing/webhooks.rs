//! Webhook handlers for Stripe and PayPal.
//!
//! Endpoints:
//!   POST /api/v1/webhooks/stripe  — Stripe webhook receiver (the ONE Stripe address)
//!   POST /api/v1/webhooks/paypal  — PayPal webhook receiver
//!
//! The ONE Stripe address (kanban t_b474bd74)
//! -----------------------------------------
//! This app had TWO Stripe receivers: this one and `POST /api/v1/loyalty/webhook/stripe`
//! (`handlers::stripe_webhook`). Both authenticated against the same rows — and the console (and
//! the table: `UNIQUE (account_id, provider_type)`) stores exactly ONE signing secret per account,
//! while Stripe issues a DISTINCT signing secret per registered endpoint. One secret therefore
//! authenticated at most one of the two addresses, so an operator who pasted the key the console
//! asked for still had one flow silently dead. The two-receiver model is reduced to ONE address:
//! `POST /api/v1/webhooks/stripe` completes EVERY Stripe flow for the account — the platform-plan
//! checkout (`checkout_sessions`, `grant_plan_session` below), the loyalty-plan checkout
//! (`stripe_checkout_sessions`, `handlers::stripe_webhook::complete_loyalty_session`) and a
//! loyalty subscription's cancellation (`cancel_loyalty_subscription`). One endpoint, one signing
//! secret, both purchases.
//!
//! Multi-account verification (kanban t_3c98f2b6)
//! ---------------------------------------------
//! These two endpoints are ONE shared URL each, while `payment_providers` is per-account (unique
//! `(account_id, provider_type)`, all CRUD arms account-scoped, checkout resolves the account's own
//! row). A delivery therefore cannot be attributed to an account before it is authenticated, so the
//! receiver authenticates it against EVERY active signing secret the app holds for that provider
//! and accepts it when ANY one of them verifies. The account the event acts on is never taken from
//! whichever secret matched — `handle_stripe_checkout_completed` resolves it from the event's own
//! provider session id against `stripe_checkout_sessions`, exactly as before. The account whose
//! configuration verified the delivery is logged (`verified_account`), so an operator can tell
//! which workspace's key authenticated a delivery without guessing.
//!
//! The header itself (`t=…,v1=…`, HMAC over `"{t}.{body}"`) is parsed and compared by
//! `crate::billing::stripe_signature`, shared with the loyalty-plan receiver (`handlers::
//! stripe_webhook`) so the two doors cannot disagree about it. Until kanban t_1b8d96f6 this file
//! read the header as a bare `v1=` value, so a genuine Stripe delivery — which begins with `t=` —
//! was answered `400 Missing v1= prefix in signature header`.

use crate::email;
use crate::error::AppError;
use crate::state::AppState;
use axum::{extract::State, http::HeaderMap, Json};
use rand::Rng;
use serde_json::{json, Value};
use sqlx::Row;
use tracing;
use uuid::Uuid;

// Re-export these for use outside billing (e.g. admin handler)
pub use crate::billing::providers::{lookup_webhook_secrets, WebhookSecretRow};

/// The ONE parse-and-verify for a `Stripe-Signature` header. The loyalty-plan arms that used to sit
/// behind their own door are called from this receiver now, so there is no second verdict on the
/// header anywhere in the app to disagree with this one. See the module for the header shape and
/// for why the bare-body HMAC that used to live here could never accept a genuine delivery
/// (kanban t_1b8d96f6).
use crate::billing::stripe_signature::verify_stripe_signature_any;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// POST /api/v1/webhooks/stripe — the ONE Stripe address for this app (kanban t_b474bd74).
///
/// Authenticates the delivery once, then hands the event to the arm that owns the session row its
/// own id resolves to. One endpoint completes every Stripe flow, because Stripe's own model is one
/// signing secret per registered endpoint while this app stores one signing secret per account: the
/// platform-plan checkout, the loyalty-plan checkout, and a loyalty subscription's cancellation.
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

    // Authenticate the delivery against EVERY active "stripe" signing secret this app holds — the
    // endpoints are one shared URL while the provider rows are per-account (kanban t_3c98f2b6), and
    // the account acted on is resolved from the event's own session id below, never from the secret
    // that matched.
    let credentials = lookup_webhook_secrets(&state, "stripe").await?;

    // Verify the signature — and that the delivery is not a CAPTURED one replayed later: the `t=`
    // stamp's age is judged against this host's clock under the configured tolerance (t_d9fb9e1b).
    let verified_for = verify_stripe_signature_any(
        &body,
        sig_header,
        &credentials,
        state.config.stripe_webhook_tolerance_secs,
    )?;
    tracing::info!(
        verified_account = %verified_for,
        active_secrets = credentials.len(),
        "Stripe webhook signature verified"
    );

    // Parse the event payload
    let event: Value = serde_json::from_str(&body)
        .map_err(|e| AppError::BadRequest(format!("Invalid webhook payload: {}", e)))?;

    let event_type = event["type"].as_str().unwrap_or("unknown");

    tracing::info!("Received Stripe webhook: type={}", event_type);

    // Handle the event. Which arm owns it is decided by the session row its own id resolves to —
    // never by anything in the body — so ONE address can complete both checkouts this app creates
    // without either arm being able to move a workspace the other one owns.
    let outcome = match event_type {
        "checkout.session.completed" => handle_stripe_checkout_completed(&state, &event).await?,
        // The loyalty-plan arms live beside their sibling (`handlers::stripe_webhook`): the
        // cancellation resolves its workspace from the signed event's own subscription id.
        "customer.subscription.deleted" => {
            crate::handlers::stripe_webhook::cancel_loyalty_subscription(&state, &event).await?
        }
        "payment_intent.succeeded" => {
            handle_stripe_payment_succeeded(&state, &event).await?;
            "accepted"
        }
        "payment_intent.payment_failed" => {
            handle_stripe_payment_failed(&state, &event).await?;
            "accepted"
        }
        _ => {
            tracing::debug!("Unhandled Stripe event type: {}", event_type);
            "ignored_unhandled_event_type"
        }
    };

    // 200 for anything the signature authenticated, INCLUDING the no-op outcomes: the provider
    // presents a real delivery and must not be told to retry a body that can never change. The
    // outcome names the arm that ran, so the effect of a delivery is observable without reading the
    // log (the loyalty receiver used to answer exactly this way, kanban t_1b8d96f6).
    Ok(Json(json!({ "status": "received", "outcome": outcome })))
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

    // The receiver needs an active "paypal" row to exist before it will look at the event; the
    // verification itself is the stub below. Loading EVERY active row (not one arbitrary row) keeps
    // the presence check account-independent (kanban t_3c98f2b6).
    let _secrets = lookup_webhook_secrets(&state, "paypal").await?;

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
    const CHARSET: &[u8] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789!@#$%^&*()";
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
    // ── Address boundary (kanban t_d7ef4a88) ─────────────────────────────────────
    // This path MINTS an account and mails credentials to whatever `email` the provider metadata
    // carried, with no validation at all. Normalise before the first SELECT so the INSERT below
    // can never store an unreachable login identity.
    let email = crate::security::email_addr::normalize(email).map_err(AppError::BadRequest)?;

    // Look for existing account by email
    let existing_account =
        sqlx::query("SELECT id, password_hash, name FROM accounts WHERE lower(email) = $1")
            .bind(&email)
            .fetch_optional(&state.db)
            .await?;

    if let Some(account_row) = existing_account {
        let account_id: Uuid = account_row.try_get("id")?;
        let existing_hash: Option<String> = account_row.try_get("password_hash")?;
        let existing_name: String = account_row.try_get("name")?;

        match existing_hash {
            Some(hash) if !hash.is_empty() => {
                // Account exists with password → send purchase confirmed
                if let Err(e) = email::send_purchase_confirmed_email(
                    &state.db,
                    account_id, // the account this mail is FOR (tenant-scoped lookup, t_0fb81177)
                    &email,
                    &existing_name,
                    plan_name,
                )
                .await
                {
                    tracing::warn!(
                        "Failed to send purchase confirmed email to {}: {}",
                        email,
                        e
                    );
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

                if let Err(e) = email::send_welcome_email(
                    &state.db,
                    account_id,
                    &email,
                    &existing_name,
                    &temp_password,
                )
                .await
                {
                    tracing::warn!("Failed to send welcome email to {}: {}", email, e);
                }
            }
        }
    } else {
        // No account found → create one
        let slug = format!(
            "acct-{}",
            Uuid::new_v4()
                .to_string()
                .split('-')
                .next()
                .unwrap_or("new")
        );
        let temp_password = generate_temp_password();
        let hash = hash_password(&temp_password)?;
        let account_id = Uuid::new_v4();

        // Insert account with tenant_id = self (standalone tenant).
        // NOT marked with `probe_harness` on purpose (kanban t_4a744754): this is the paid-checkout
        // credential path (a provider-signed payment webhook), not the served, unauthenticated
        // signup — the only door the `X-Swift-Harness` marker belongs to.
        sqlx::query(
            "INSERT INTO accounts (id, name, email, password_hash, slug, role, tenant_id, purchase_pin)
             VALUES ($1, $2, $3, $4, $5, 'company_admin', $6, '0000')"
        )
        .bind(account_id)
        .bind(customer_name)
        .bind(&email)
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

        if let Err(e) =
            email::send_welcome_email(&state.db, account_id, &email, customer_name, &temp_password)
                .await
        {
            tracing::warn!("Failed to send welcome email to {}: {}", email, e);
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Webhook event handlers
// ---------------------------------------------------------------------------

/// Grant the plan behind a PLAN checkout session this app created, if this event names one.
///
/// Returns `true` when the event named one of OUR plan sessions (so the loyalty arm must not also
/// run), `false` otherwise. Nothing here trusts the event's metadata: the account and the plan come
/// from `checkout_sessions` — the row WE wrote when the session was created — and the plan is applied
/// through the same `plans.slug -> plan_tiers.slug -> accounts.plan_tier_id` mapping the operator's
/// Assign-plan arm uses (`handlers::plans_handler::admin_assign_plan`), because `accounts.plan_tier_id`
/// is what `/auth/me` and every feature gate read.
///
/// `status = 'pending'` in the lookup makes a replayed delivery a no-op: the first one flips the row
/// to `completed`, so a second event finds nothing and falls through as if it were not ours.
async fn grant_plan_session(state: &AppState, provider_session_id: &str) -> Result<bool, AppError> {
    let row = sqlx::query(
        "SELECT account_id, COALESCE(metadata->>'plan_slug', '') AS plan_slug \
           FROM checkout_sessions \
          WHERE provider_session_id = $1 AND payment_provider = 'stripe' \
            AND COALESCE(metadata->>'kind', '') = 'plan' \
            AND status = 'pending'",
    )
    .bind(provider_session_id)
    .fetch_optional(&state.db)
    .await?;
    let Some(row) = row else {
        return Ok(false);
    };

    let account_id: Uuid = row.try_get("account_id")?;
    let plan_slug: String = row.try_get("plan_slug")?;

    // No tier of that slug: grant nothing rather than leave a half-applied plan, and say so.
    let tier_id: Option<Uuid> = sqlx::query_scalar("SELECT id FROM plan_tiers WHERE slug = $1")
        .bind(&plan_slug)
        .fetch_optional(&state.db)
        .await?;
    let Some(tier_id) = tier_id else {
        tracing::error!(
            session = %provider_session_id, plan = %plan_slug,
            "Stripe plan checkout completed but NO plan_tiers row carries that slug — nothing granted"
        );
        return Ok(true);
    };

    sqlx::query("UPDATE accounts SET plan_tier_id = $1, updated_at = NOW() WHERE id = $2")
        .bind(tier_id)
        .bind(account_id)
        .execute(&state.db)
        .await?;
    sqlx::query(
        "UPDATE checkout_sessions SET status = 'completed', updated_at = NOW() \
          WHERE provider_session_id = $1 AND status = 'pending'",
    )
    .bind(provider_session_id)
    .execute(&state.db)
    .await?;
    tracing::info!(
        session = %provider_session_id, %account_id, plan = %plan_slug,
        "Stripe plan checkout completed — the workspace's plan tier was granted"
    );

    // The referral credit the loyalty door already carries, so a plan bought through this path is
    // attributed exactly like one the operator granted by hand.
    if let Some(plan_id) = sqlx::query_scalar::<_, Uuid>("SELECT id FROM plans WHERE slug = $1")
        .bind(&plan_slug)
        .fetch_optional(&state.db)
        .await?
    {
        crate::handlers::plans_handler::attribute_plan_upgrade(state, account_id, plan_id).await;
    }
    Ok(true)
}

/// `checkout.session.completed`, dispatched to the arm that owns the session the event names.
///
/// ONE Stripe address (kanban t_b474bd74) means a single event may belong to either checkout this
/// app creates, so the arms are tried in order and the FIRST one that recognizes the session id
/// owns the event:
///
/// 1. `grant_plan_session` — a PLATFORM PLAN purchase (`checkout_sessions`, `kind = 'plan'`);
/// 2. `handlers::stripe_webhook::complete_loyalty_session` — a LOYALTY-PLAN purchase
///    (`stripe_checkout_sessions`);
/// 3. the generic `checkout_sessions` update plus credential delivery (the arm that predates both).
///
/// Each of them reads the workspace and the plan from OUR OWN row named by the signature-verified
/// session id, never from the event's metadata, so no arm can be steered by the request body. The
/// returned outcome names the arm that ran (or why nothing moved) and is echoed to the caller.
/// `None` from the loyalty arm means "not one of ours", so the generic arm still gets its turn —
/// an `ignored_*` outcome from it is OURS and must not be reinterpreted by a later arm.
async fn handle_stripe_checkout_completed(
    state: &AppState,
    event: &Value,
) -> Result<&'static str, AppError> {
    tracing::info!("Stripe checkout.session.completed — processing...");

    let session = &event["data"]["object"];
    let provider_session_id = session["id"].as_str().unwrap_or("");
    if provider_session_id.is_empty() {
        tracing::warn!("Stripe checkout.session.completed missing session ID");
        return Ok("ignored_missing_session_id");
    }

    // A PLATFORM PLAN purchase, created by THIS app's own checkout (`billing::checkout`): the account
    // that pays and the plan to grant are read from OUR `checkout_sessions` row, identified by the
    // Stripe session id in the signature-verified event — never from the event's own metadata. That is
    // the same trust boundary the loyalty arm documents (`handlers::stripe_webhook`), and it is why a
    // plan session is not silently swallowed by the loyalty arm below, which looks the session up in
    // a DIFFERENT table.
    if grant_plan_session(state, provider_session_id).await? {
        return Ok("plan_granted");
    }

    // A LOYALTY-PLAN purchase, created by `handlers::loyalty_plans::subscribe` — the flow that used
    // to be delivered to its own Stripe address and was silently DROPPED here when the operator
    // pointed their one Stripe endpoint at this receiver (kanban t_b474bd74).
    if let Some(outcome) =
        crate::handlers::stripe_webhook::complete_loyalty_session(state, event).await?
    {
        return Ok(outcome);
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
        let customer_name = session["customer_details"]["name"]
            .as_str()
            .or_else(|| email.and_then(|e| e.split('@').next()))
            .unwrap_or("Customer");

        // Get account info from stripe_checkout_sessions
        let session_info = sqlx::query(
            "SELECT account_id, credits FROM stripe_checkout_sessions WHERE stripe_session_id = $1",
        )
        .bind(provider_session_id)
        .fetch_optional(&state.db)
        .await?;

        let account_id = session_info
            .as_ref()
            .and_then(|r| r.try_get::<Uuid, _>("account_id").ok())
            .unwrap_or_else(Uuid::new_v4);

        let metadata = session.get("metadata");
        let is_loyalty_sub = metadata
            .and_then(|m| m.get("loyalty_subscription"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            == "true";
        let plan_name = if is_loyalty_sub {
            metadata
                .and_then(|m| m.get("plan_name"))
                .and_then(|v| v.as_str())
                .unwrap_or("Plan")
        } else {
            "Plan"
        };

        if let Some(email_str) = email {
            if let Err(e) =
                deliver_credentials(state, email_str, customer_name, account_id, plan_name).await
            {
                tracing::warn!("Credential delivery failed for {}: {}", email_str, e);
            }
        }
        return Ok("session_completed");
    }

    tracing::debug!(
        session = %provider_session_id,
        "Stripe checkout.session.completed names no session of ours — nothing moved"
    );
    Ok("ignored_unknown_session")
}

async fn handle_stripe_payment_succeeded(
    _state: &AppState,
    _event: &Value,
) -> Result<(), AppError> {
    tracing::info!("Stripe payment_intent.succeeded — processing...");
    Ok(())
}

async fn handle_stripe_payment_failed(_state: &AppState, _event: &Value) -> Result<(), AppError> {
    tracing::info!("Stripe payment_intent.payment_failed — processing...");
    Ok(())
}

async fn handle_paypal_payment_completed(state: &AppState, event: &Value) -> Result<(), AppError> {
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
        let customer_name = resource["payer"]["name"]["given_name"]
            .as_str()
            .or_else(|| email.and_then(|e| e.split('@').next()))
            .unwrap_or("Customer");

        let account_id = Uuid::new_v4();
        let plan_name = "Plan";

        if let Some(email_str) = email {
            if let Err(e) =
                deliver_credentials(state, email_str, customer_name, account_id, plan_name).await
            {
                tracing::warn!("Credential delivery failed for {}: {}", email_str, e);
            }
        }
    }

    Ok(())
}

async fn handle_paypal_payment_failed(_state: &AppState, _event: &Value) -> Result<(), AppError> {
    tracing::info!("PayPal payment failed/refunded — processing...");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── multi-account signature verification (kanban t_3c98f2b6, header shape t_1b8d96f6) ────────
    //
    // The endpoints are ONE shared URL while `payment_providers` is per-account, so the receiver
    // must accept a delivery signed with ANY account's active secret, must resolve the account from
    // the event rather than from which secret matched, and must still refuse a delivery that no
    // active secret signed. These properties are what a later refactor must not break.
    //
    // The signature is a GENUINE Stripe one: the header carries `t=`, and the HMAC is over
    // `"{t}.{body}"`. The previous fixture signed the bare body and sent a bare `v1=` header, which
    // is why it could not tell that the receiver this file used to ship refused every real delivery.

    /// A `t=` stamp inside the tolerance in force. The fixture used to pin a 2017 stamp; the freshness
    /// arm (kanban t_d9fb9e1b) now refuses that, so the tests below carry a live stamp — they are about
    /// the HEADER shape and the per-account verdict, not about the clock.
    fn live_stamp() -> i64 {
        crate::billing::stripe_signature::unix_now() - 1
    }

    fn tolerance() -> i64 {
        crate::billing::stripe_signature::DEFAULT_STRIPE_SIGNATURE_TOLERANCE_SECS
    }

    fn hmac(payload: &str, secret: &str) -> String {
        crate::billing::stripe_signature::hmac_hex(payload, secret).unwrap()
    }

    fn sign(payload: &str, secret: &str) -> String {
        let stamp = live_stamp();
        format!(
            "t={},v1={}",
            stamp,
            hmac(&format!("{}.{}", stamp, payload), secret)
        )
    }

    fn credential(secret: &str) -> WebhookSecretRow {
        WebhookSecretRow {
            account_id: Uuid::new_v4(),
            secret: secret.to_string(),
        }
    }

    #[test]
    fn accepts_a_signature_from_any_active_account() {
        let payload = r#"{"id":"evt_1","type":"checkout.session.completed"}"#;
        let a = credential("whsec_account_a");
        let b = credential("whsec_account_b");
        let credentials = vec![a.clone(), b.clone()];

        // The SECOND account's secret verifies: before t_3c98f2b6 only whichever row the planner
        // returned first was used, so this delivery was answered 400 "Invalid webhook signature".
        let verified = verify_stripe_signature_any(
            payload,
            &sign(payload, &b.secret),
            &credentials,
            tolerance(),
        )
        .expect("account B's own secret must verify");
        assert_eq!(verified, b.account_id);

        // The first account's secret still verifies (the owner control).
        let verified = verify_stripe_signature_any(
            payload,
            &sign(payload, &a.secret),
            &credentials,
            tolerance(),
        )
        .expect("account A's own secret must verify");
        assert_eq!(verified, a.account_id);
    }

    #[test]
    fn refuses_a_signature_no_active_account_signed() {
        let payload = r#"{"id":"evt_2"}"#;
        let credentials = vec![credential("whsec_account_a"), credential("whsec_account_b")];

        // A secret that belongs to NO active row (a rotated/removed account) is refused.
        let err = verify_stripe_signature_any(
            payload,
            &sign(payload, "whsec_unknown"),
            &credentials,
            tolerance(),
        )
        .expect_err("a foreign secret must not verify");
        assert!(matches!(err, AppError::BadRequest(ref m) if m == "Invalid webhook signature"));

        // A signature over DIFFERENT bytes is refused too.
        let err = verify_stripe_signature_any(
            "{}.",
            &sign(payload, "whsec_account_a"),
            &credentials,
            tolerance(),
        )
        .expect_err("a signature over other bytes must not verify");
        assert!(matches!(err, AppError::BadRequest(_)));
    }

    #[test]
    fn refuses_a_header_that_is_not_a_signed_delivery() {
        let credentials = vec![credential("whsec_account_a")];

        // What Stripe never sends: a bare `v1=` with no `t=`. There are no signed bytes this app
        // can reproduce, so it must not be accepted (the old receiver demanded exactly this shape,
        // which is the inverse of what a real delivery looks like).
        let err = verify_stripe_signature_any("{}", "v1=deadbeef", &credentials, tolerance())
            .expect_err("a stamp-less header is not a signed delivery");
        assert!(matches!(err, AppError::BadRequest(ref m) if m.starts_with("Missing t=")));

        // A stamp with no signature value at all is not a delivery either.
        let err = verify_stripe_signature_any(
            "{}",
            &format!("t={}", live_stamp()),
            &credentials,
            tolerance(),
        )
        .expect_err("a header with no v1= carries no signature");
        assert!(matches!(err, AppError::BadRequest(ref m) if m.starts_with("Missing v1=")));
    }

    #[test]
    fn single_account_behaviour_is_unchanged() {
        let payload = r#"{"id":"evt_3"}"#;
        let only = credential("whsec_only_account");
        let credentials = vec![only.clone()];

        assert_eq!(
            verify_stripe_signature_any(
                payload,
                &sign(payload, &only.secret),
                &credentials,
                tolerance()
            )
            .unwrap(),
            only.account_id
        );
        assert!(verify_stripe_signature_any(
            payload,
            &sign(payload, "other"),
            &credentials,
            tolerance()
        )
        .is_err());
    }
}
