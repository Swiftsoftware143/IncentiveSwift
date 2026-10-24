//! Checkout Session handlers.
//!
//! Endpoints:
//!   POST /api/v1/checkout/create    — create a checkout session
//!   GET  /api/v1/checkout/sessions  — list checkout sessions
//!
//! ## Why `create` REFUSES (kanban t_59a1b420)
//!
//! Until this card, `create_checkout_session` unconditionally INSERTed a `pending`
//! `checkout_sessions` row and returned a fabricated
//! `https://checkout.example.com/session/<id>` URL — a provider URL that does not exist and can
//! never complete. Measured live on 2026-10-02 with a real tenant token: a request for the Pro
//! plan's $49 answered `200 {"status":"pending","checkout_url":"https://checkout.example.com/…"}`
//! and left a phantom session row behind. That is a payment surface claiming a purchase started
//! when nothing can charge — the same defect class as the retired dead upsell anchors
//! (t_70b96213, t_538505de).
//!
//! It now fails LOUDLY, mirroring the fleet's canonical shape (FunnelSwift
//! `handlers/checkout_handler.rs::create_checkout_session`):
//!
//! * no active payment provider row  -> `503 payment_provider_not_configured`
//! * provider row but not Stripe     -> `501 checkout_not_implemented`
//! * Stripe row with no usable key   -> `503 payment_provider_not_configured`
//! * Stripe key present, no call     -> `501 checkout_not_implemented`
//! * `price_amount <= 0`             -> `400`
//!
//! NO arm creates a session, and NO arm invents a URL. When a payment credential is supplied and
//! the Stripe call is implemented, this is the single place to add it.

use crate::error::AppError;
use crate::security::auth::AuthenticatedUser;
use crate::security::provider_key_crypto::{decrypt_from_storage, mask};
use crate::state::AppState;
use axum::http::StatusCode;
use axum::{
    extract::{Path, State},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Input types
// ---------------------------------------------------------------------------
/// The ONE currency a plan checkout is charged in.
///
/// Stripe's request body carries it (`plan_session_form`), and the `checkout_sessions` row we write
/// MUST carry the same value — this constant is that single source of truth. Before it existed the
/// row took the CALLER's `price_currency` string while Stripe was always charged `usd`, so a client
/// that posted `price_currency: "jpy"` produced a session row recording 79 JPY for a 79 USD charge:
/// a money record that could lie about the currency of a real sale.
pub const CHECKOUT_CURRENCY: &str = "usd";

/// Input for creating a checkout session. Every field is echoed back in a refusal (see
/// `requested` below) so a caller can see the whole request that was turned down.
#[derive(Deserialize)]
pub struct CreateCheckoutInput {
    /// OPTIONAL. The amount actually charged is the PLAN row's own price (see the doc above the
    /// handler) — this field was required yet never trusted, so a client that sent the documented
    /// minimum (`plan_id`) was refused by serde with a leaked internal sentence
    /// (`missing field \`price_amount\``) before the plan was even read. A caller that still POSTS
    /// an amount keeps the old refusal when it is not finite/positive, so nothing that used to be
    /// rejected now passes silently.
    #[serde(default)]
    pub price_amount: Option<f64>,
    #[serde(default)]
    pub price_currency: Option<String>,
    pub description: Option<String>,
    pub success_url: Option<String>,
    pub cancel_url: Option<String>,
    pub metadata: Option<Value>,
    pub payment_provider: Option<String>,
    pub plan_id: Option<String>,
    /// OPTIONAL. Which TERM to sell: absent/`"month"` (the only thing this app could sell before
    /// 2026-10-24), or `"year"`/`"yearly"`/`"annual"` for the yearly price the catalogue carries.
    /// Anything else is a 400 that names this field — never a silent fallback to monthly.
    #[serde(default)]
    pub billing_cycle: Option<String>,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// POST /api/v1/checkout/create
///
/// A REAL Stripe Checkout session for a PLATFORM PLAN, or an explicit refusal (card: the ladder was
/// unbuyable). Until this change the handler could only ever refuse: it resolved the provider from
/// the CALLER's own row alone and then answered `501 checkout_not_implemented` whatever key was
/// configured, because the Stripe call had never been written. Measured live 2026-10-15 with a real
/// workspace token: the paid plan answered `501`, `checkout_sessions` stayed empty, and the tenant
/// console's Upgrade button (shipped the same day) could never complete a purchase.
///
/// The arms now, cheapest first:
///
/// * a SUPPLIED `price_amount` not finite/positive -> 400 (a zero price is not a checkout); omitting
///   it is legal, because the charged amount is the plan's own price
/// * `plan_id` missing/unknown/inactive -> 404 `plan_not_found`
/// * the PLAN's own price is 0 -> 400 "That plan costs nothing…"
/// * no active provider row and none owned by a platform-operator account -> 503
///   `payment_provider_not_configured`, naming where the key belongs
/// * a resolved provider that is not Stripe -> 501 `checkout_not_implemented`
/// * a Stripe row whose key cannot be read -> 503 `payment_provider_not_configured`
/// * Stripe ACCEPTED the session -> 201 `{url, session_id, plan}` + a `pending` `checkout_sessions`
///   row that carries the plan in its metadata
/// * Stripe REFUSED the session -> 424 `checkout_session_failed` with Stripe's own sentence
///
/// 424 and never 502 for the failure arm: Cloudflare REPLACES the body of an origin 502 with its own
/// page, so the customer would see nothing at all (FunnelSwift measured the same on 2026-10-15).
///
/// ## The BILLING TERM (2026-10-24): the yearly price the catalogue carries can now be bought
///
/// Until this change the Stripe form hardcoded `recurring][interval]=month` (`plan_session_form`),
/// so a MONTHLY subscription was the only thing this app could ever sell — while the catalogue
/// already carries an annual figure for every paid tier (`plan_tiers.price_annual`: Pro $399,
/// Enterprise $1699, edited by the operator's own Tier editor) and `terms.html` tells customers a
/// subscription is billed "on a recurring monthly or annual basis, depending on the plan you
/// select". The yearly offer existed everywhere except the one place a customer buys.
///
/// `billing_cycle` is OPTIONAL and defaults to `"month"`, so a caller that does not send it gets
/// exactly the request this handler built before (same form, same columns, same metadata plus one
/// `billing_cycle: "month"` key). `"year"`, `"yearly"` and `"annual"` ask for the yearly arm, which:
///
/// * charges the plan's yearly price — `COALESCE(plans.price_yearly, plan_tiers.price_annual)`,
///   the same precedence `COALESCE(NULLIF(price_monthly,0), price)` uses for the monthly figure, so
///   the operator's Tier editor funds what checkout sells (those two tables drifted apart before:
///   `plans` holds 0/49/199 in `price` while `plan_tiers` holds the annual numbers);
/// * builds `interval=year`;
/// * records the yearly amount + term on `checkout_sessions.metadata` and on the refused attempt;
/// * REFUSES BY NAME when the tier has no yearly price, instead of quietly charging the monthly
///   rate — a charge the buyer did not agree to is the worst possible fallback;
/// * rejects any other value with a 400 that names the field.
///
/// The price CHARGED is the plan row's own (`COALESCE(NULLIF(price_monthly, 0), price)`, the
/// expression `/auth/me` and the public catalogue already use); the caller's `price_amount` is
/// echoed on a refusal and never trusted as the amount.
pub async fn create_checkout_session(
    State(state): State<AppState>,
    auth: AuthenticatedUser,
    Json(body): Json<CreateCheckoutInput>,
) -> Result<(StatusCode, Json<Value>), AppError> {
    let account_id = Uuid::parse_str(&auth.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;

    // What was asked for, echoed on every refusal so nothing is silent.
    let requested = json!({
        "price_amount": body.price_amount,
        "price_currency": body.price_currency,
        "description": body.description,
        "plan_id": body.plan_id,
        "success_url": body.success_url,
        "cancel_url": body.cancel_url,
        "metadata": body.metadata,
        "payment_provider": body.payment_provider,
        "billing_cycle": body.billing_cycle,
    });

    // WHICH TERM (2026-10-24). Absent means month — the only term this handler could build before
    // today, so every existing caller keeps its exact behaviour. An unrecognised value is named in a
    // 400 rather than silently sold as monthly: a buyer who asked for a year must never be charged
    // on a term they did not choose.
    let billing_cycle = body
        .billing_cycle
        .as_deref()
        .unwrap_or("month")
        .trim()
        .to_lowercase();
    let yearly = match billing_cycle.as_str() {
        "" | "month" | "monthly" => false,
        "year" | "yearly" | "annual" => true,
        other => {
            return Err(AppError::BadRequest(format!(
                "Unknown billing_cycle '{other}' — use 'month' or 'year'."
            )))
        }
    };
    let interval = if yearly { "year" } else { "month" };

    // A zero/negative/non-finite price is not a checkout. Only a SUPPLIED amount is judged here:
    // the amount that decides whether there is anything to charge is the PLAN's own price (arm 3
    // below), so a caller that names only the plan is not refused before we can read it.
    if let Some(amount) = body.price_amount {
        if !amount.is_finite() || amount <= 0.0 {
            return Err(AppError::BadRequest(format!(
                "price_amount must be greater than zero (got {amount}); there is nothing to check out."
            )));
        }
    }

    // ── 1. The PLAN, from our own catalogue. ─────────────────────────────────────────────────────
    let raw_plan_id = body.plan_id.as_deref().map(str::trim).unwrap_or("");
    let plan_id =
        Uuid::parse_str(raw_plan_id).map_err(|_| AppError::NotFound("Unknown plan".to_string()))?;
    // The MONTHLY figure is `COALESCE(NULLIF(price_monthly, 0), price)` — the expression `/auth/me`
    // and the public catalogue use. The YEARLY figure follows the same precedence one level down:
    // `plans.price_yearly` when an operator has filled it in, else `plan_tiers.price_annual`, which
    // is the table the operator's own Tier editor writes (measured 2026-10-24: `plans.price_yearly`
    // is 0 on every row while `plan_tiers.price_annual` carries Pro 399 / Enterprise 1699 — so
    // reading only `plans` would advertise a yearly term that can never be bought).
    let plan: Option<(String, String, f64, f64)> = sqlx::query_as(
        "SELECT p.slug, p.name, \
                COALESCE(NULLIF(p.price_monthly, 0), p.price)::float8, \
                COALESCE(NULLIF(p.price_yearly, 0), NULLIF(pt.price_annual, 0)::float8, 0)::float8 \
         FROM plans p \
         LEFT JOIN plan_tiers pt ON lower(pt.slug) = lower(p.slug) \
         WHERE p.id = $1 AND p.is_active = true",
    )
    .bind(plan_id)
    .fetch_optional(&state.db)
    .await?;
    let Some((plan_slug, plan_name, plan_price, plan_annual)) = plan else {
        tracing::warn!(%account_id, %plan_id, "checkout/create REFUSED: unknown or inactive plan");
        return Ok((
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": "plan_not_found",
                "message": "That plan does not exist (or is no longer on sale). No session was \
                            created and no charge can occur.",
                "requested": requested,
            })),
        ));
    };
    // The amount for the term the buyer asked for, and the ONE guard over it. A yearly ask on a tier
    // with no yearly price is refused BY NAME: charging the monthly rate instead would bill someone
    // for a term they did not choose.
    let amount = if yearly { plan_annual } else { plan_price };
    if !amount.is_finite() || amount <= 0.0 {
        return Err(AppError::BadRequest(if yearly {
            format!("The {plan_name} plan has no yearly price, so it can only be bought monthly.")
        } else {
            "That plan costs nothing — there is nothing to check out.".to_string()
        }));
    }

    // ── 2. Which merchant account funds this purchase? ───────────────────────────────────────────
    // A plan is sold BY the platform TO a workspace, so the merchant account is the platform's.
    // Resolution order (see [`resolve_plan_provider`]): the buyer's own active row first, so a
    // workspace that brought its own Stripe account keeps paying through it, then the operator's.
    let resolved = resolve_plan_provider(&state, account_id).await?;
    let Some((provider_account, provider_type)) = resolved else {
        // ── A PLAN CAN BE SOLD WITHOUT AN API KEY (2026-10-24) ────────────────────────────────
        // `plans.purchase_url` is a hosted payment page the OPERATOR pastes into the plan (a Stripe
        // Payment Link, a PayPal button, anything that takes the money). The column has existed
        // since the plans table shipped, `admin plans` now writes it, and nothing read it at the
        // till — so on this install, whose `payment_providers` holds ZERO rows, every press on
        // Upgrade could only ever be refused even when the operator already had a working way to be
        // paid. A plan that carries a link is SELLABLE: hand the buyer to it, and STILL file the
        // ask, because a hosted page tells this installation nothing — the workspace is seated when
        // the operator resolves the ticket, never by a redirect.
        //
        // The provider arm keeps precedence (a configured provider is a real checkout with a
        // session id a webhook can confirm), so this only runs where the ladder used to dead-end.
        // Same shape as ADASwift `create_upgrade_checkout` (commit 95c83c5).
        if let Some(url) = hosted_payment_link(&state.db, plan_id).await {
            return Ok(hand_off_to_payment_link(
                &state,
                account_id,
                plan_id,
                &plan_slug,
                &plan_name,
                amount,
                interval,
                url,
                body.description.clone(),
            )
            .await);
        }

        tracing::error!(
            %account_id,
            "checkout/create REFUSED: no active payment provider for the account and none owned by \
             a platform-operator account"
        );
        // BUYER-FIRST (2026-10-22, parity with FunnelSwift). The upgrade screen paints this
        // `message` verbatim (`d.message || d.error`), so it is addressed to the CUSTOMER: what
        // happened to their money comes first and the operator's setup detail sits in parentheses
        // at the end. Before this the sentence OPENED by telling a buyer how the platform operator
        // configures Stripe in his admin console — an instruction addressed to somebody else, on
        // the one screen where a buyer is trying to give us money.
        //
        // The attempt is also RECORDED, not just logged. A 503 in a log line tells support nothing
        // about which plan a real workspace wanted; `checkout_sessions` with status
        // `provider_missing` is that history, so the platform team can activate the plan by hand.
        // `user_id` has no FK here (only `account_id` does), so the account id is the fallback —
        // the same shape the successful path below uses.
        let record_user: Option<Uuid> = sqlx::query_scalar(
            "SELECT u.id FROM users u JOIN accounts a ON a.tenant_id = u.tenant_id \
             WHERE a.id = $1 ORDER BY u.created_at LIMIT 1",
        )
        .bind(account_id)
        .fetch_optional(&state.db)
        .await
        .unwrap_or(None);
        let recorded = sqlx::query(
            "INSERT INTO checkout_sessions \
             (id, account_id, user_id, price_amount, price_currency, description, metadata, status, \
              payment_provider) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, 'provider_missing', 'none')",
        )
        .bind(Uuid::new_v4())
        .bind(account_id)
        .bind(record_user.unwrap_or(account_id))
        .bind(amount)
        .bind(CHECKOUT_CURRENCY)
        .bind(body.description.clone())
        .bind(json!({
            "kind": "plan",
            "reason": "payment_provider_not_configured",
            "account_id": account_id.to_string(),
            "plan_id": plan_id.to_string(),
            "plan_slug": plan_slug,
            // The term the buyer asked for, so the operator's history shows a yearly ask as a
            // yearly amount rather than reading back as the monthly rate.
            "billing_cycle": interval,
        }))
        .execute(&state.db)
        .await;
        if let Err(e) = recorded {
            tracing::error!(%account_id, "checkout/create: could not record the refused attempt: {e:?}");
        }
        // THE ASK, WHERE THE OPERATOR CAN ACT ON IT (2026-10-23; parity with ADASwift and
        // WorkflowSwift). `checkout_sessions` is a HISTORY screen: it shows that a workspace
        // pressed Upgrade, but acting on it means hand-assigning a plan. The operator's Plan
        // requests queue is the surface whose Resolve action seats the workspace on the tier, and
        // it reads `support_tickets` rows carrying `source='plan_request'` + `plan_slug`.
        // The dashboard's own "Request the plan" press files exactly such a row, but only if that
        // follow-up press runs -- a customer who closed the tab had asked for nothing. The ask is
        // filed HERE, server-side, deduped on the same (tenant, source, open, plan_slug) key the
        // ticket door uses, so a later press returns the open request instead of minting a twin.
        // No mail is queued: nothing was charged, so a refusal must not add mail volume.
        let plan_request = record_upgrade_request(
            &state.db, account_id, &plan_slug, &plan_name, amount, interval, None,
        )
        .await;
        return Ok((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "error": "payment_provider_not_configured",
                "message": "No charge was made — nothing was sent to a payment provider, so you \
                            cannot be billed. Upgrades are not switched on for this installation \
                            yet: your request has been recorded for the platform team, who can \
                            activate this plan for your workspace by hand. (Operator: add Stripe's \
                            keys on the operator's own account in the admin console under Provider \
                            Keys / Payment providers; that key then funds every workspace's plan \
                            purchase, and a workspace that brings its own Stripe account is charged \
                            through its own.)",
                "configured": false,
                "request_recorded": true,
                "plan_request": plan_request,
                "requested": requested,
            })),
        ));
    };

    if provider_type != "stripe" {
        return Ok((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({
                "error": "checkout_not_implemented",
                "message": format!(
                    "A {provider_type} provider is configured, but only Stripe checkout is \
                     implemented. No session was created and no charge can occur."
                ),
                "configured": true,
                "provider_type": provider_type,
                "requested": requested,
            })),
        ));
    }

    if provider_account != account_id {
        tracing::info!(
            buyer = %account_id, provider_owner = %provider_account,
            "checkout/create: charging through the platform account (buyer has no provider of its own)"
        );
    }

    // ── 3. The key, from the RESOLVED owner's row (never the buyer's alone). ─────────────────────
    let stored: Option<String> = sqlx::query_scalar(
        "SELECT api_key FROM payment_providers \
         WHERE account_id = $1 AND provider_type = 'stripe' AND is_active = true \
         ORDER BY created_at LIMIT 1",
    )
    .bind(provider_account)
    .fetch_optional(&state.db)
    .await?;
    let secret_key = crate::security::provider_key_crypto::decrypt_from_storage(
        &state.db,
        stored.as_deref().unwrap_or("").trim(),
    )
    .await
    .ok()
    .filter(|k| !k.trim().is_empty());
    let Some(secret_key) = secret_key else {
        // A row exists but no usable KEY: same dead end as having no row at all, and the same
        // answer — a plan that carries its own hosted payment page is still sellable. Same
        // precedence as above: only reached when the ladder cannot charge.
        if let Some(url) = hosted_payment_link(&state.db, plan_id).await {
            return Ok(hand_off_to_payment_link(
                &state,
                account_id,
                plan_id,
                &plan_slug,
                &plan_name,
                amount,
                interval,
                url,
                body.description.clone(),
            )
            .await);
        }
        tracing::error!(
            %account_id, %provider_account,
            "checkout/create REFUSED: the resolved stripe row has no usable api_key"
        );
        return Ok((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "error": "payment_provider_not_configured",
                "message": "A Stripe provider exists but its secret key is missing or unreadable, \
                            so no session was created and no charge can occur. An admin must \
                            re-enter the key in the admin panel (Provider Keys).",
                "configured": false,
                "provider_type": provider_type,
                "requested": requested,
            })),
        ));
    };

    // ── 4. The real Stripe session. ──────────────────────────────────────────────────────────────
    let (url, provider_session_id) = match create_stripe_plan_session(
        account_id,
        &auth.email,
        plan_id,
        &plan_slug,
        &plan_name,
        amount,
        interval,
        &secret_key,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(
                %account_id, plan = %plan_slug, error = %e,
                "checkout/create: the Stripe session FAILED — no session row written, no charge can occur"
            );
            return Ok((
                StatusCode::FAILED_DEPENDENCY,
                Json(json!({
                    "error": "checkout_session_failed",
                    "message": e,
                    "configured": true,
                    "requested": requested,
                })),
            ));
        }
    };

    // The buyer's own `users` row when the account has one. `checkout_sessions.user_id` has no FK
    // (only `account_id` does), so a workspace whose signup minted no user row falls back to its
    // account id rather than writing a NOT NULL column as NULL.
    let user_id: Option<Uuid> = sqlx::query_scalar(
        "SELECT u.id FROM users u JOIN accounts a ON a.tenant_id = u.tenant_id \
         WHERE a.id = $1 ORDER BY u.created_at LIMIT 1",
    )
    .bind(account_id)
    .fetch_optional(&state.db)
    .await?;
    let user_id = user_id.unwrap_or(account_id);

    sqlx::query(
        "INSERT INTO checkout_sessions \
         (id, account_id, user_id, price_amount, price_currency, description, success_url, cancel_url, \
          metadata, status, payment_provider, provider_session_id) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 'pending', 'stripe', $10)",
    )
    .bind(Uuid::new_v4())
    .bind(account_id)
    .bind(user_id)
    .bind(amount)
    .bind(CHECKOUT_CURRENCY)
    .bind(body.description.clone())
    .bind(body.success_url.clone())
    .bind(body.cancel_url.clone())
    .bind(json!({
        "kind": "plan",
        "account_id": account_id.to_string(),
        "plan_id": plan_id.to_string(),
        "plan_slug": plan_slug,
        // What the buyer is actually subscribed to. The receiver grants by `plan_slug`, so the term
        // changes no entitlement — it makes the money record say 399.00|year instead of implying a
        // monthly sale, which is what an operator reconciling against Stripe needs.
        "billing_cycle": interval,
    }))
    .bind(&provider_session_id)
    .execute(&state.db)
    .await?;

    tracing::info!(
        %account_id, plan = %plan_slug, session = %provider_session_id, term = interval, amount,
        "stripe checkout session created"
    );
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "url": url,
            "session_id": provider_session_id,
            "plan": plan_slug,
            "billing_cycle": interval,
            "amount": amount,
        })),
    ))
}

// ═════════════════════ Verifying the operator's payment key ═════════════════════
//
// kanban t_14081cea. Until this route existed, this app's ONLY way to learn that a payment key was
// wrong, truncated or from another account was for a CUSTOMER to press Upgrade and receive the 424
// that carries Stripe's refusal. `payment_providers` is empty here (measured 2026-10-16: 0 rows), so
// every purchase refuses `payment_provider_not_configured` — and an operator pasting a key had no way
// to check it before it mattered. This route makes the key verifiable at the moment it is pasted.
//
// The check is a READ: Stripe's `GET /v1/balance`. It moves no money and creates no Stripe object.
//
// One body shape, `{ "api_key": "<key>" }`, and both arms are useful:
//   * `api_key` non-empty -> test THAT key. It is never stored, never logged and never echoed (only
//     `mask`-ed), so a typo cannot overwrite a working credential the way a Save would. This is the
//     "Test key" button beside Save.
//   * `api_key` empty/absent -> test the key STORED for this account, resolved exactly as a purchase
//     resolves it (`resolve_plan_provider`: the account's own active row first, then the platform
//     operator's). This is the per-row "Test stored key" button.
//
// The refusal arms are the buy path's own arms, so this route can never claim to have verified
// something `checkout/create` would not sell through.
//
// Every refusal is a 4xx class JSON body, never a 502: a CDN replaces a 502 body and the operator
// would lose the reason, which is the same rule `create_checkout_session` follows.
pub async fn test_payment_provider(
    State(state): State<AppState>,
    auth: AuthenticatedUser,
    Path(provider_type): Path<String>,
    payload: Option<Json<Value>>,
) -> Result<(StatusCode, Json<Value>), AppError> {
    let account_id = Uuid::parse_str(&auth.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;
    let provider = provider_type.trim().to_ascii_lowercase();
    let supplied = payload
        .as_ref()
        .and_then(|j| j.0.get("api_key"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();

    let (key, source) = if !supplied.is_empty() {
        (supplied, "supplied")
    } else {
        let resolved = resolve_plan_provider(&state, account_id).await?;
        let Some((owner_account, owner_provider)) = resolved else {
            return Ok((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({
                    "ok": false,
                    "error": "payment_provider_not_configured",
                    "reason": "payment_provider_not_configured",
                    "message": "No payment provider is stored for this account or for the platform \
                                operator, so there is no stored key to test. Paste a key and press \
                                Test key, or save one first.",
                })),
            ));
        };
        if provider != owner_provider.to_ascii_lowercase() {
            return Ok((
                StatusCode::NOT_FOUND,
                Json(json!({
                    "ok": false,
                    "error": "payment_provider_not_stored",
                    "reason": "payment_provider_not_stored",
                    "provider": provider,
                    "message": format!(
                        "The stored provider for this account is '{owner_provider}', not \
                         '{provider}', so there is no '{provider}' key to test."
                    ),
                })),
            ));
        }
        let stored: Option<String> = sqlx::query_scalar(
            "SELECT api_key FROM payment_providers \
             WHERE account_id = $1 AND provider_type = $2 AND is_active = true \
             ORDER BY created_at LIMIT 1",
        )
        .bind(owner_account)
        .bind(&owner_provider)
        .fetch_optional(&state.db)
        .await?;
        // Read through the SAME at-rest decryption the buy path uses: a row whose key is unreadable
        // (wrong/absent PROVIDER_KEY_ENC_SECRET) is "nothing to test", not a 500.
        let plain = decrypt_from_storage(&state.db, stored.as_deref().unwrap_or("").trim())
            .await
            .ok()
            .unwrap_or_default();
        if plain.trim().is_empty() {
            return Ok((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({
                    "ok": false,
                    "error": "payment_provider_not_configured",
                    "reason": "payment_provider_not_configured",
                    "provider": provider,
                    "message": format!(
                        "The stored '{owner_provider}' provider row carries no readable key, so \
                         there is nothing to test. Paste the key and press Save first."
                    ),
                })),
            ));
        }
        (plain.trim().to_string(), "stored")
    };

    // Stay in step with `create_checkout_session`: whatever that arm will not sell through, this one
    // must not claim to have verified.
    if provider != "stripe" {
        return Ok((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({
                "ok": false,
                "error": "checkout_not_implemented",
                "reason": "checkout_not_implemented",
                "provider": provider,
                "message": format!(
                    "'{provider}' cannot be verified: only Stripe is implemented for checkout in \
                     this app, so a key for any other provider would be accepted here and then fail \
                     at the buy button."
                ),
            })),
        ));
    }

    let api_base = std::env::var("STRIPE_API_BASE")
        .unwrap_or_else(|_| "https://api.stripe.com".to_string())
        .trim_end_matches('/')
        .to_string();
    let masked = mask(&key);

    let sent = reqwest::Client::new()
        .get(format!("{api_base}/v1/balance"))
        .bearer_auth(&key)
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await;
    let resp = match sent {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(provider = %provider, key_source = source, error = %e,
                "payment key test could not reach the provider");
            return Ok((
                StatusCode::FAILED_DEPENDENCY,
                Json(json!({
                    "ok": false,
                    "error": "provider_unreachable",
                    "reason": "provider_unreachable",
                    "provider": provider,
                    "key_source": source,
                    "api_key_masked": masked,
                    "message": format!("Could not reach Stripe at all: {e}"),
                })),
            ));
        }
    };
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();

    if status.is_success() {
        // Stripe's /v1/balance answers `{ "object": "balance", "livemode": <bool>, ... }`. Only
        // `livemode` is read, and reporting it is most of the value: a TEST-mode key is a working key
        // that will never charge anybody, and mistaking one for the other is the exact mistake this
        // route exists to prevent.
        let live = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v.get("livemode").and_then(|b| b.as_bool()));
        let message = match live {
            Some(true) => {
                "Stripe accepted the key and it is a LIVE key — a real customer can be charged."
                    .to_string()
            }
            Some(false) => "Stripe accepted the key but it is a TEST-mode key — no customer will \
                            ever be charged. Replace it with the live key before selling."
                .to_string(),
            None => "Stripe accepted the key.".to_string(),
        };
        tracing::info!(provider = %provider, key_source = source, "payment key verified");
        return Ok((
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "provider": provider,
                "key_source": source,
                "livemode": live,
                "api_key_masked": masked,
                "message": message,
            })),
        ));
    }

    // Never echo the key. Stripe's own sentence is safe and is the whole point of the route.
    let msg = serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| format!("Stripe returned {status}"));
    tracing::warn!(provider = %provider, key_source = source, http = %status,
        "payment key was REFUSED by the provider");
    Ok((
        StatusCode::FAILED_DEPENDENCY,
        Json(json!({
            "ok": false,
            "error": "provider_refused",
            "reason": "provider_refused",
            "provider": provider,
            "key_source": source,
            "api_key_masked": masked,
            "message": format!("Stripe refused the key: {msg}"),
        })),
    ))
}

/// WHICH payment provider funds a PLAN purchase (card: the ladder was unbuyable, measured 2026-10-15).
///
/// A plan is sold BY the platform TO a workspace, so the merchant account is the platform's. This
/// used to read the CALLER's own row only, while the only shipped writer of the table is the
/// account's own Provider Keys screen. The consequence was measured live: `payment_providers` held
/// 0 rows, `POST /api/v1/checkout/create` answered `503` for every workspace, and the operator's key
/// could not have changed that — every plan purchase would still have looked for a row owned by the
/// BUYING workspace.
///
/// Resolution order, lowest-surprise first:
///   1. the buying account's OWN active row — unchanged behaviour, so a workspace that brings its
///      own merchant account keeps paying through it and nothing existing moves;
///   2. otherwise the OLDEST active row owned by an account whose `role` is `admin`/`super_admin` —
///      this app's own definition of the platform operator (`security::auth::is_platform_admin`,
///      and the value the login puts in the token from `accounts.role`), i.e. the key the operator
///      entered in the admin console;
///   3. otherwise `None` -> the caller answers 503 naming exactly where the key belongs.
///
/// The role guard on arm 2 is load-bearing: without it ANY workspace could register, save its own
/// Stripe key and have every customer's plan purchase on the install charged to that workspace.
async fn resolve_plan_provider(
    state: &AppState,
    account_id: Uuid,
) -> Result<Option<(Uuid, String)>, sqlx::Error> {
    let own: Option<(Uuid, String)> = sqlx::query_as(
        "SELECT account_id, provider_type FROM payment_providers \
         WHERE account_id = $1 AND is_active = true ORDER BY created_at LIMIT 1",
    )
    .bind(account_id)
    .fetch_optional(&state.db)
    .await?;
    if own.is_some() {
        return Ok(own);
    }
    sqlx::query_as(
        "SELECT pp.account_id, pp.provider_type FROM payment_providers pp \
         WHERE pp.is_active = true \
           AND EXISTS (SELECT 1 FROM accounts a WHERE a.id = pp.account_id \
                         AND a.role IN ('admin', 'super_admin')) \
         ORDER BY pp.created_at LIMIT 1",
    )
    .fetch_optional(&state.db)
    .await
}

/// Percent-encode ONE `application/x-www-form-urlencoded` component (RFC 3986 unreserved set kept).
///
/// The body is built here by hand rather than through a serialiser: the Stripe form API needs literal
/// `line_items[0][price_data][unit_amount]`-style keys, and a serde sequence/map is one dependency
/// change away from deciding those keys for us. Stripe validates the API key BEFORE it parses the
/// body, so a malformed body is invisible behind an auth failure — the encoder is pinned by a unit
/// test instead.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Create the Stripe Checkout session for a plan, on the TERM the buyer chose.
///
/// Kept to ONE outbound call so the whole thing is provable without a live Stripe account: the base
/// URL honours `STRIPE_API_BASE` (default = production, so unset behaviour is byte-identical) and the
/// redirect origin honours `STRIPE_CHECKOUT_ORIGIN` — a local stub can assert the exact request the
/// app builds. Returns `(checkout_url, provider_session_id)`, or Stripe's own sentence on a refusal.
///
/// `metadata[kind]=plan` + `metadata[account_id]` + `metadata[plan_slug]` are what the receiver needs
/// to grant the right plan to the right workspace. They are set HERE, and the receiver re-reads them
/// from THIS app's `checkout_sessions` row (identified by the Stripe session id Stripe signed) rather
/// than from the event's own metadata — the same trust boundary the loyalty receiver documents.
async fn create_stripe_plan_session(
    account_id: Uuid,
    email: &str,
    plan_id: Uuid,
    plan_slug: &str,
    plan_name: &str,
    price: f64,
    interval: &str,
    secret_key: &str,
) -> Result<(String, String), String> {
    let api_base = std::env::var("STRIPE_API_BASE")
        .unwrap_or_else(|_| "https://api.stripe.com".to_string())
        .trim_end_matches('/')
        .to_string();
    let origin = std::env::var("STRIPE_CHECKOUT_ORIGIN")
        .unwrap_or_else(|_| "https://app.incentiveswift.com".to_string())
        .trim_end_matches('/')
        .to_string();

    let body = plan_session_form(
        account_id, email, plan_id, plan_slug, plan_name, price, interval, &origin,
    );

    let resp = reqwest::Client::new()
        .post(format!("{api_base}/v1/checkout/sessions"))
        .header("Authorization", format!("Bearer {secret_key}"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .timeout(std::time::Duration::from_secs(25))
        .send()
        .await
        .map_err(|e| format!("could not reach Stripe: {e}"))?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        // Never echo the key. Stripe's own message is safe and is what an operator needs.
        let msg = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| {
                v.get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| format!("Stripe returned {status}"));
        return Err(format!("Stripe refused the session: {msg}"));
    }

    let parsed: Value =
        serde_json::from_str(&text).map_err(|e| format!("unreadable Stripe response: {e}"))?;
    let session_id = parsed
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let url = parsed
        .get("url")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if session_id.is_empty() || url.is_empty() {
        return Err("Stripe accepted the request but returned no session id/url".to_string());
    }
    Ok((url, session_id))
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
        "#,
    )
    .bind(account_id)
    .fetch_all(&state.db)
    .await?;

    let items: Vec<Value> = rows
        .iter()
        .map(|row| {
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
        })
        .collect();

    Ok(Json(json!({ "items": items, "count": items.len() })))
}

/// The exact `application/x-www-form-urlencoded` body for a PLAN checkout session on `interval`.
///
/// Split out of the outbound call for one reason: Stripe validates the API key BEFORE it parses the
/// body, so a malformed body is invisible behind an auth failure. Everything a Stripe session needs
/// is asserted here, in a unit test, instead of being inferred from a 401.
///
/// `interval` is `"month"` or `"year"` — the term the buyer chose. It reaches Stripe BOTH in
/// `recurring][interval]` (what actually bills the card) and in the product name (what the payer
/// reads on the checkout page), so the two can never disagree.
fn plan_session_form(
    account_id: Uuid,
    email: &str,
    plan_id: Uuid,
    plan_slug: &str,
    plan_name: &str,
    price: f64,
    interval: &str,
    origin: &str,
) -> String {
    // A recurring plan, so the session is a subscription rather than a one-off charge. Stripe takes
    // the amount in whole cents.
    let cents = (price * 100.0).round() as i64;
    let term_label = if interval == "year" {
        "yearly"
    } else {
        "monthly"
    };
    let params: Vec<(&str, String)> = vec![
        ("mode", "subscription".to_string()),
        ("payment_method_types[]", "card".to_string()),
        ("line_items[0][quantity]", "1".to_string()),
        (
            "line_items[0][price_data][currency]",
            CHECKOUT_CURRENCY.to_string(),
        ),
        ("line_items[0][price_data][unit_amount]", cents.to_string()),
        (
            "line_items[0][price_data][recurring][interval]",
            interval.to_string(),
        ),
        (
            "line_items[0][price_data][product_data][name]",
            format!("{plan_name} plan ({term_label})"),
        ),
        (
            "success_url",
            format!("{origin}/dashboard/?checkout=success"),
        ),
        (
            "cancel_url",
            format!("{origin}/dashboard/?checkout=cancelled"),
        ),
        ("client_reference_id", account_id.to_string()),
        ("customer_email", email.to_string()),
        ("metadata[kind]", "plan".to_string()),
        ("metadata[account_id]", account_id.to_string()),
        ("metadata[plan_id]", plan_id.to_string()),
        ("metadata[plan_slug]", plan_slug.to_string()),
    ];
    params
        .iter()
        .map(|(k, v)| format!("{}={}", urlencode(k), urlencode(v)))
        .collect::<Vec<String>>()
        .join("&")
}

/// Is this a URL a browser can be sent to in order to pay?
///
/// `plans.purchase_url` is typed in by an operator, so a typo must REFUSE the sale rather than send
/// a buyer somewhere broken or insecure. https only (a payment page over plain http is a downgrade
/// worth refusing), a host with a dot in it, and no whitespace anywhere — enough to catch the two
/// mistakes an operator actually makes (a bare `stripe.com/pay` with no scheme, a pasted line with a
/// trailing space or a newline in the middle) without pretending to be a URL parser. Copied from
/// ADASwift's arm (commit 95c83c5) so the two apps judge a link the same way.
fn is_hosted_payment_url(raw: &str) -> bool {
    let u = raw.trim();
    let Some(rest) = u.strip_prefix("https://") else {
        return false;
    };
    if u.contains(char::is_whitespace) {
        return false;
    }
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    !host.is_empty() && host.contains('.')
}

/// The plan's own hosted payment page, if the operator pasted a usable one.
///
/// Read per purchase (never cached): the operator can clear a link, and the next press must see it.
/// An unusable value is treated as NO link — the refusal below is the honest answer, and the link
/// has already been refused by the till rather than sending a buyer to a broken page.
async fn hosted_payment_link(db: &sqlx::PgPool, plan_id: Uuid) -> Option<String> {
    let stored: Option<String> =
        sqlx::query_scalar("SELECT COALESCE(BTRIM(purchase_url), '') FROM plans WHERE id = $1")
            .bind(plan_id)
            .fetch_optional(db)
            .await
            .ok()
            .flatten();
    stored.filter(|u| is_hosted_payment_url(u))
}

/// Hand a buyer to the plan's own hosted payment page instead of refusing the sale.
///
/// The ask is STILL filed as a plan request, and the hand-off is recorded on `checkout_sessions`, so
/// the operator's queue and history both show what happened. The session row's status is
/// `payment_link` and never `pending`: `pending` promises a webhook that cannot arrive for a hosted
/// page this installation knows nothing about.
async fn hand_off_to_payment_link(
    state: &AppState,
    account_id: Uuid,
    plan_id: Uuid,
    plan_slug: &str,
    plan_name: &str,
    amount: f64,
    interval: &str,
    url: String,
    description: Option<String>,
) -> (StatusCode, Json<Value>) {
    let plan_request = record_upgrade_request(
        &state.db,
        account_id,
        plan_slug,
        plan_name,
        amount,
        interval,
        Some(&url),
    )
    .await;

    // The buyer's own `users` row when the account has one — the same shape the charging path uses.
    // `checkout_sessions.user_id` has no FK (only `account_id` does), so a workspace with no user
    // row falls back to its account id rather than writing a NOT NULL column as NULL.
    let user_id: Option<Uuid> = sqlx::query_scalar(
        "SELECT u.id FROM users u JOIN accounts a ON a.tenant_id = u.tenant_id \
         WHERE a.id = $1 ORDER BY u.created_at LIMIT 1",
    )
    .bind(account_id)
    .fetch_optional(&state.db)
    .await
    .unwrap_or(None);

    let recorded = sqlx::query(
        "INSERT INTO checkout_sessions \
         (id, account_id, user_id, price_amount, price_currency, description, metadata, status, \
          payment_provider) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, 'payment_link', 'payment_link')",
    )
    .bind(Uuid::new_v4())
    .bind(account_id)
    .bind(user_id.unwrap_or(account_id))
    .bind(amount)
    .bind(CHECKOUT_CURRENCY)
    .bind(description)
    .bind(json!({
        "kind": "plan",
        // What actually happened: handed over to the operator's own payment page. The buyer's plan is
        // seated when the operator resolves the request, never by the redirect.
        "reason": "hosted_payment_link",
        "account_id": account_id.to_string(),
        "plan_id": plan_id.to_string(),
        "plan_slug": plan_slug,
        "billing_cycle": interval,
        "purchase_url": url,
    }))
    .execute(&state.db)
    .await;
    if let Err(e) = recorded {
        // Best effort, like the ticket: the buyer is being handed to a payment page, and a
        // bookkeeping failure must not turn a live sale into a 500.
        tracing::error!(%account_id, "checkout/create: could not record the payment-link hand-off: {e:?}");
    }
    tracing::info!(
        %account_id, plan = %plan_slug, term = interval, amount,
        "checkout/create: handing the workspace to the plan's own hosted payment page"
    );

    (
        StatusCode::OK,
        Json(json!({
            "status": "payment_link",
            "url": url,
            "checkout_url": url,
            "configured": false,
            "request_recorded": plan_request.is_some(),
            "plan_request": plan_request,
            "amount": amount,
            "billing_cycle": interval,
            "plan": {
                "id": plan_id.to_string(),
                "slug": plan_slug,
                "name": plan_name,
            },
        })),
    )
}

/// File the tier a customer pressed "Upgrade" for as an OPEN plan request the operator can act on.
///
/// Returns `Some({"id", "plan_slug", "plan_name", "already_open"})` when a request exists — either
/// the row just written or the one already open for that tier — and `None` when the row could not be
/// written. BEST-EFFORT by design: the caller is already answering a 503, so a bookkeeping failure
/// must never turn a refusal into a 500. It is logged instead.
///
/// The row is the SAME shape the dashboard's "Request the plan" button files
/// (`handlers::support_tickets::create_ticket`: `source='plan_request'`, `category='billing'`,
/// `plan_slug` set), because that is what `list_tickets` filters for the operator's Plan requests
/// view and what `update_ticket`'s Resolve arm reads to switch the workspace onto the tier. It is
/// inserted directly rather than through that handler so no receipt mail is queued: this path
/// charged nothing, and a second "we got your request" mail would be noise on top of the screen the
/// customer is already reading.
///
/// Dedupe key: one OPEN/in_progress request per (account, plan_slug) — the same key the ticket door
/// uses. A customer who presses the button twice, or whose console then fires its own ticket
/// request, gets `already_open: true` instead of a duplicate in the queue. The TERM is recorded on
/// the row that gets written (`amount`/`interval` describe what was actually asked for), so a yearly
/// ask reads back as the yearly figure rather than as the monthly rate.
async fn record_upgrade_request(
    db: &sqlx::PgPool,
    account_id: Uuid,
    plan_slug: &str,
    plan_name: &str,
    amount: f64,
    interval: &str,
    // `Some(the plan's own payment page)` when the buyer was HANDED OVER to it rather than refused
    // (2026-10-24): the ticket then has to say so, because the operator cannot wait for a webhook
    // that will never arrive — they confirm the payment themselves and resolve the request.
    via_link: Option<&str>,
) -> Option<Value> {
    let open: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM support_tickets \
          WHERE tenant_id = $1 AND source = 'plan_request' \
            AND status IN ('open','in_progress') \
            AND plan_slug IS NOT DISTINCT FROM $2 \
          ORDER BY created_at DESC LIMIT 1",
    )
    .bind(account_id)
    .bind(plan_slug)
    .fetch_optional(db)
    .await
    .map_err(|e| {
        tracing::error!(
            %account_id,
            "checkout/create: could not look for an open plan request: {e:?}"
        )
    })
    .ok()?;

    let (id, already_open) = match open {
        Some(id) => (id, true),
        None => {
            let id = Uuid::new_v4();
            let written = sqlx::query(
                r#"INSERT INTO support_tickets
                     (id, tenant_id, campaign_id, contact_id, subject, description, status, priority,
                      category, source, created_by, plan_slug)
                   VALUES ($1,$2,NULL,NULL,$3,$4,'open','normal','billing','plan_request',$2,$5)"#,
            )
            .bind(id)
            .bind(account_id)
            .bind(format!("Upgrade request: {plan_name} plan ({interval}ly)"))
            .bind(match via_link {
                Some(url) => format!(
                    "This workspace pressed Upgrade on the {plan_name} plan (${amount:.0}/{interval}) \
                     and was handed to the plan's own hosted payment page ({url}). Nothing was \
                     charged by this installation and no payment can be confirmed automatically — a \
                     hosted page tells it nothing — so the workspace is NOT seated yet. Confirm the \
                     payment with your provider, then resolve this request to seat the workspace on \
                     the plan."
                ),
                None => format!(
                    "This workspace pressed Upgrade on the {plan_name} plan (${amount:.0}/{interval}) \
                     and no payment provider is configured, so nothing was charged and nothing can be \
                     billed. Filed automatically by the checkout refusal arm, so the ask survives even \
                     if the customer closes the tab. Resolving this request seats the workspace on the \
                     plan; quote the {interval}ly price when the invoice is raised."
                ),
            })
            .bind(plan_slug)
            .execute(db)
            .await;
            if let Err(e) = written {
                tracing::error!(
                    %account_id,
                    "checkout/create: could not file the plan request: {e:?}"
                );
                return None;
            }
            (id, false)
        }
    };

    Some(json!({
        "id": id.to_string(),
        "plan_slug": plan_slug,
        "plan_name": plan_name,
        "already_open": already_open,
    }))
}

#[cfg(test)]
mod plan_session_form_tests {
    //! The Stripe request body is built here rather than by a serialiser, so these tests ARE the
    //! contract: a `Vec`/`HashMap` form would be one dependency change away from sending
    //! `0=line_items%5B0%5D`-style keys, and Stripe's auth check would hide it.
    use super::{plan_session_form, urlencode};
    use uuid::Uuid;

    fn decoded(body: &str) -> Vec<(String, String)> {
        body.split('&')
            .map(|pair| {
                let (k, v) = pair.split_once('=').expect("every part is key=value");
                (undecode(k), undecode(v))
            })
            .collect()
    }

    fn undecode(s: &str) -> String {
        let b = s.as_bytes();
        let mut out = Vec::with_capacity(b.len());
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'%' && i + 2 < b.len() {
                let hex = std::str::from_utf8(&b[i + 1..i + 3]).unwrap();
                out.push(u8::from_str_radix(hex, 16).expect("valid percent escape"));
                i += 3;
            } else {
                out.push(b[i]);
                i += 1;
            }
        }
        String::from_utf8(out).expect("ascii in, utf8 out")
    }

    fn value<'a>(pairs: &'a [(String, String)], key: &str) -> &'a str {
        pairs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
            .unwrap_or_else(|| panic!("missing key {key}"))
    }

    #[test]
    fn stripes_bracket_keys_survive_encoding() {
        // Built from integers on purpose: the deploy gate refuses literal UUIDs in `src/` (query by
        // slug instead), and a test never needs a real row's id.
        let account = Uuid::from_u128(0x1111_1111_1111_1111_1111_1111_1111_1111);
        let plan = Uuid::from_u128(0x2222_2222_2222_2222_2222_2222_2222_2222);
        let body = plan_session_form(
            account,
            "buyer@example.invalid",
            plan,
            "pro",
            "Pro",
            49.0,
            "month",
            "https://app.incentiveswift.com",
        );
        // The raw body is percent-encoded...
        assert!(body.contains("line_items%5B0%5D%5Bprice_data%5D%5Bunit_amount%5D=4900"));
        assert!(
            !body.contains("line_items[0]"),
            "keys must be encoded, not literal"
        );
        // ...and it decodes to exactly the keys Stripe parses.
        let pairs = decoded(&body);
        assert_eq!(value(&pairs, "mode"), "subscription");
        assert_eq!(value(&pairs, "line_items[0][quantity]"), "1");
        assert_eq!(value(&pairs, "line_items[0][price_data][currency]"), "usd");
        assert_eq!(
            value(&pairs, "line_items[0][price_data][unit_amount]"),
            "4900"
        );
        assert_eq!(
            value(&pairs, "line_items[0][price_data][recurring][interval]"),
            "month"
        );
        assert_eq!(
            value(&pairs, "line_items[0][price_data][product_data][name]"),
            "Pro plan (monthly)"
        );
        assert_eq!(
            value(&pairs, "success_url"),
            "https://app.incentiveswift.com/dashboard/?checkout=success"
        );
        assert_eq!(
            value(&pairs, "cancel_url"),
            "https://app.incentiveswift.com/dashboard/?checkout=cancelled"
        );
        assert_eq!(value(&pairs, "client_reference_id"), account.to_string());
        // The four fields the receiver reads back off OUR row, never off the event.
        assert_eq!(value(&pairs, "metadata[kind]"), "plan");
        assert_eq!(value(&pairs, "metadata[account_id]"), account.to_string());
        assert_eq!(value(&pairs, "metadata[plan_id]"), plan.to_string());
        assert_eq!(value(&pairs, "metadata[plan_slug]"), "pro");
    }

    #[test]
    fn price_goes_out_in_cents_and_the_amount_is_not_the_callers() {
        let account = Uuid::nil();
        let plan = Uuid::nil();
        // 199.00 -> 19900; a half-cent is rounded, never truncated to a free charge.
        let body = plan_session_form(
            account,
            "x@example.invalid",
            plan,
            "enterprise",
            "Enterprise",
            199.0,
            "month",
            "https://o",
        );
        assert_eq!(
            value(&decoded(&body), "line_items[0][price_data][unit_amount]"),
            "19900"
        );
        let body = plan_session_form(
            account,
            "x@example.invalid",
            plan,
            "pro",
            "Pro",
            9.005,
            "month",
            "https://o",
        );
        assert_eq!(
            value(&decoded(&body), "line_items[0][price_data][unit_amount]"),
            "901"
        );
    }

    #[test]
    fn a_yearly_ask_bills_twelve_months_and_says_so_on_the_page() {
        // The defect this guards (2026-10-24): every form carried `interval=month`, so the yearly
        // price the catalogue advertises could not be bought at all. The term must reach BOTH the
        // billing interval and the product name the payer reads.
        let body = plan_session_form(
            Uuid::nil(),
            "x@example.invalid",
            Uuid::nil(),
            "pro",
            "Pro",
            399.0,
            "year",
            "https://o",
        );
        let pairs = decoded(&body);
        assert_eq!(
            value(&pairs, "line_items[0][price_data][recurring][interval]"),
            "year"
        );
        assert_eq!(
            value(&pairs, "line_items[0][price_data][unit_amount]"),
            "39900",
            "a yearly session charges the yearly amount, not the monthly rate"
        );
        assert_eq!(
            value(&pairs, "line_items[0][price_data][product_data][name]"),
            "Pro plan (yearly)"
        );
        assert_eq!(value(&pairs, "mode"), "subscription");
    }

    #[test]
    fn urlencode_keeps_the_unreserved_set_and_escapes_everything_else() {
        assert_eq!(urlencode("aZ09-_.~"), "aZ09-_.~");
        assert_eq!(urlencode("a b"), "a%20b");
        assert_eq!(urlencode("+&=%"), "%2B%26%3D%25");
        // UTF-8 goes out byte-wise (é is two bytes), never as a raw byte.
        assert_eq!(urlencode("é"), "%C3%A9");
    }
}
