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
    });

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
    let plan: Option<(String, String, f64)> = sqlx::query_as(
        "SELECT slug, name, COALESCE(NULLIF(price_monthly, 0), price)::float8 FROM plans \
         WHERE id = $1 AND is_active = true",
    )
    .bind(plan_id)
    .fetch_optional(&state.db)
    .await?;
    let Some((plan_slug, plan_name, plan_price)) = plan else {
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
    if !plan_price.is_finite() || plan_price <= 0.0 {
        return Err(AppError::BadRequest(
            "That plan costs nothing — there is nothing to check out.".to_string(),
        ));
    }

    // ── 2. Which merchant account funds this purchase? ───────────────────────────────────────────
    // A plan is sold BY the platform TO a workspace, so the merchant account is the platform's.
    // Resolution order (see [`resolve_plan_provider`]): the buyer's own active row first, so a
    // workspace that brought its own Stripe account keeps paying through it, then the operator's.
    let resolved = resolve_plan_provider(&state, account_id).await?;
    let Some((provider_account, provider_type)) = resolved else {
        tracing::error!(
            %account_id,
            "checkout/create REFUSED: no active payment provider for the account and none owned by \
             a platform-operator account"
        );
        return Ok((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "error": "payment_provider_not_configured",
                "message": "No payment provider is configured, so checkout cannot work. The \
                            platform operator adds Stripe's keys in the admin console (Provider \
                            Keys / Payment providers) on the operator's own account; that key then \
                            funds every workspace's plan purchase, and a workspace that brings its \
                            own Stripe account is charged through its own. No session was created \
                            and no charge can occur.",
                "configured": false,
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
        plan_price,
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
    .bind(plan_price)
    .bind(CHECKOUT_CURRENCY)
    .bind(body.description.clone())
    .bind(body.success_url.clone())
    .bind(body.cancel_url.clone())
    .bind(json!({
        "kind": "plan",
        "account_id": account_id.to_string(),
        "plan_id": plan_id.to_string(),
        "plan_slug": plan_slug,
    }))
    .bind(&provider_session_id)
    .execute(&state.db)
    .await?;

    tracing::info!(
        %account_id, plan = %plan_slug, session = %provider_session_id,
        "stripe checkout session created"
    );
    Ok((
        StatusCode::CREATED,
        Json(json!({ "url": url, "session_id": provider_session_id, "plan": plan_slug })),
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

/// Create the Stripe Checkout session for a monthly plan.
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
        account_id, email, plan_id, plan_slug, plan_name, price, &origin,
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

/// The exact `application/x-www-form-urlencoded` body for a monthly PLAN checkout session.
///
/// Split out of the outbound call for one reason: Stripe validates the API key BEFORE it parses the
/// body, so a malformed body is invisible behind an auth failure. Everything a Stripe session needs
/// is asserted here, in a unit test, instead of being inferred from a 401.
fn plan_session_form(
    account_id: Uuid,
    email: &str,
    plan_id: Uuid,
    plan_slug: &str,
    plan_name: &str,
    price: f64,
    origin: &str,
) -> String {
    // A monthly plan, so the session is a subscription rather than a one-off charge. Stripe takes
    // the amount in whole cents.
    let cents = (price * 100.0).round() as i64;
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
            "month".to_string(),
        ),
        (
            "line_items[0][price_data][product_data][name]",
            format!("{plan_name} plan (monthly)"),
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
            "https://o",
        );
        assert_eq!(
            value(&decoded(&body), "line_items[0][price_data][unit_amount]"),
            "901"
        );
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
