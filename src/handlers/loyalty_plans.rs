//! Loyalty plans — subscription tiers for business loyalty program

use axum::{extract::State, Json};
use serde::Serialize;
use uuid::Uuid;

use crate::billing::checkout::resolve_plan_provider;
use crate::error::AppError;
use crate::security::auth::AuthenticatedUser;
use crate::state::AppState;

#[derive(Serialize, sqlx::FromRow)]
pub struct LoyaltyPlan {
    pub id: Uuid,
    pub name: String,
    pub slug: String,
    pub monthly_price: i32,
    pub monthly_zc_pool: i32,
    pub features: Option<serde_json::Value>,
    pub description: Option<String>,
    pub how_it_works: Option<String>,
}

#[derive(Serialize)]
pub struct PlanStatusResponse {
    pub enrolled: bool,
    pub plan: Option<String>,
    pub status: String,
    pub zc_pool_remaining: i32,
    pub zc_pool_total: i32,
    pub pool_reset_date: Option<String>,
}

/// GET /api/v1/loyalty/plans
/// Returns all active loyalty subscription plans available for business enrollment.
pub async fn list_plans(State(s): State<AppState>) -> Result<Json<Vec<LoyaltyPlan>>, AppError> {
    let plans = sqlx::query_as::<_, LoyaltyPlan>(
        "SELECT id, name, slug, monthly_price, monthly_zc_pool, features, description, how_it_works FROM loyalty_plans WHERE is_active = true ORDER BY monthly_price ASC"
    )
    .fetch_all(&s.db)
    .await?;
    Ok(Json(plans))
}

/// GET /api/v1/loyalty/plan/status
/// Returns the authenticated business account's current loyalty plan enrollment status.
pub async fn plan_status(
    State(s): State<AppState>,
    auth: AuthenticatedUser,
) -> Result<Json<PlanStatusResponse>, AppError> {
    let account_id = auth.account_id;

    // Every column in this tuple is NULLABLE in `accounts` (loyalty_plan_status DEFAULT
    // 'inactive', zc_pool_remaining / zc_pool_total DEFAULT 0). Decode them all as `Option`
    // so one NULL cannot fail the whole tuple: the error propagated as a 500 for that
    // account and took the REAL pool values down with it.
    let row = sqlx::query_as::<
        _,
        (
            Option<String>,
            Option<String>,
            Option<i32>,
            Option<i32>,
            Option<chrono::NaiveDate>,
        ),
    >(
        "SELECT loyalty_plan, loyalty_plan_status, zc_pool_remaining, zc_pool_total, pool_reset_date FROM accounts WHERE id = $1::uuid"
    )
    .bind(&account_id)
    .fetch_optional(&s.db)
    .await?
    .unwrap_or((None, None, None, None, None));

    // A NULL status means "no plan state recorded"; the column DEFAULT is 'inactive', so
    // report that deliberately — and make the NULL observable — instead of inventing it
    // silently or 500-ing the whole read.
    let status = row.1.unwrap_or_else(|| {
        tracing::warn!(
            account_id = %account_id,
            "loyalty plan status is NULL in accounts — reporting 'inactive' (column DEFAULT)"
        );
        "inactive".to_string()
    });
    if row.2.is_none() || row.3.is_none() {
        tracing::warn!(
            account_id = %account_id,
            zc_pool_remaining = ?row.2,
            zc_pool_total = ?row.3,
            "zc pool columns are NULL in accounts (DEFAULT 0) — reporting 0"
        );
    }

    Ok(Json(PlanStatusResponse {
        enrolled: row.0.is_some() && status == "active",
        plan: row.0,
        status,
        zc_pool_remaining: row.2.unwrap_or(0),
        zc_pool_total: row.3.unwrap_or(0),
        pool_reset_date: row.4.map(|d| d.format("%Y-%m-%d").to_string()),
    }))
}

// ── Subscribe (Stripe Checkout) ────────────────────────────────────────────

use serde::Deserialize;

#[derive(Deserialize)]
pub struct SubscribeRequest {
    pub plan_slug: String,
    pub email: String,
    pub success_url: Option<String>,
    pub cancel_url: Option<String>,
}

#[derive(Serialize)]
pub struct SubscribeResponse {
    pub checkout_url: Option<String>,
    pub error: Option<String>,
    pub plan: Option<String>,
    pub monthly_price: Option<i32>,
    pub monthly_zc_pool: Option<i32>,
}

/// POST /api/v1/loyalty/subscribe
/// Creates a Stripe checkout session for a loyalty plan subscription.
pub async fn subscribe(
    State(s): State<AppState>,
    auth: AuthenticatedUser,
    Json(req): Json<SubscribeRequest>,
) -> Result<Json<SubscribeResponse>, AppError> {
    let account_id = Uuid::parse_str(&auth.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;

    // Look up the plan
    let plan = sqlx::query_as::<_, LoyaltyPlan>(
        "SELECT id, name, slug, monthly_price, monthly_zc_pool, features, description, how_it_works FROM loyalty_plans WHERE slug = $1 AND is_active = true"
    )
    .bind(&req.plan_slug)
    .fetch_optional(&s.db)
    .await?
    .ok_or_else(|| AppError::NotFound(format!("Plan '{}' not found", req.plan_slug)))?;

    // Check if already has an active plan
    // `fetch_optional` makes the ANNOTATION the carrier, so sqlx decodes `O` here: the
    // `flatten()` forces `O = Option<String>`, so a NULL status is `None`, not a 500.
    let existing: Option<String> =
        sqlx::query_scalar("SELECT loyalty_plan_status FROM accounts WHERE id = $1")
            .bind(&account_id)
            .fetch_optional(&s.db)
            .await?
            .flatten();

    if existing.as_deref() == Some("active") {
        return Ok(Json(SubscribeResponse {
            checkout_url: None,
            error: Some("You already have an active loyalty plan".into()),
            plan: None,
            monthly_price: None,
            monthly_zc_pool: None,
        }));
    }

    // ── Which merchant account funds this purchase? ──────────────────────────────────────────────
    // A loyalty plan is sold BY the platform TO a workspace, exactly like a platform plan, so it
    // resolves the merchant account the SAME way the plan till does (see
    // [`crate::billing::checkout::resolve_plan_provider`]): the BUYER's own active row first — a
    // workspace that brought its own Stripe account keeps paying through it — and then a PLATFORM
    // OPERATOR's row.
    //
    // This handler used to read the caller's own row alone, so on an installation with no
    // per-customer Stripe key every press of the dashboard's "Choose <plan>" button could only ever
    // answer "add your Stripe secret key on the Payment Providers screen": an instruction addressed
    // to the buyer, on the one screen where they are trying to give us money, and a dead end even
    // when the platform itself had a working merchant account. Selling a PLATFORM plan already
    // worked that way; selling a loyalty plan did not, on the same dashboard.
    let resolved = resolve_plan_provider(&s, account_id).await?;
    let Some((provider_account, provider_type)) = resolved else {
        tracing::error!(
            %account_id,
            plan_slug = %plan.slug,
            "loyalty/subscribe REFUSED: no active payment provider for the account and none owned \
             by a platform-operator account. OPERATOR: add a provider key on the operator's own \
             account (Provider Keys / Payment providers); that key then funds every workspace's \
             loyalty purchase."
        );
        // Addressed to the BUYER: what happened to their money comes first. The operator's setup
        // detail belongs in the log line above, not on the customer's screen.
        return Ok(Json(SubscribeResponse {
            checkout_url: None,
            error: Some(
                "No charge was made — nothing was sent to a payment provider, so you cannot be \
                 billed. Loyalty plans are not switched on for this installation yet; please \
                 contact support and the team that runs it can activate this plan for your \
                 workspace."
                    .into(),
            ),
            plan: None,
            monthly_price: None,
            monthly_zc_pool: None,
        }));
    };

    if provider_type != "stripe" {
        return Ok(Json(SubscribeResponse {
            checkout_url: None,
            error: Some(format!(
                "A {provider_type} provider is configured, but only Stripe checkout is \
                 implemented. No session was created and no charge can occur."
            )),
            plan: None,
            monthly_price: None,
            monthly_zc_pool: None,
        }));
    }

    if provider_account != account_id {
        tracing::info!(
            buyer = %account_id, provider_owner = %provider_account,
            "loyalty/subscribe: charging through the platform account (buyer has no provider of its own)"
        );
    }

    // The key, from the RESOLVED owner's row (never the buyer's alone). `payment_providers.api_key`
    // is ciphertext at rest, so it is decrypted before use.
    let stored: Option<String> = sqlx::query_scalar(
        "SELECT api_key FROM payment_providers \
         WHERE account_id = $1 AND provider_type = 'stripe' AND is_active = true \
         ORDER BY created_at, id LIMIT 1",
    )
    .bind(provider_account)
    .fetch_optional(&s.db)
    .await?;

    let stripe_key = match crate::security::provider_key_crypto::decrypt_from_storage(
        &s.db,
        stored.as_deref().unwrap_or("").trim(),
    )
    .await
    .ok()
    .filter(|k| !k.trim().is_empty())
    {
        Some(k) => k,
        None => {
            tracing::error!(
                %account_id,
                provider_owner = %provider_account,
                "loyalty/subscribe REFUSED: the resolved provider row's key could not be decrypted"
            );
            return Ok(Json(SubscribeResponse {
                checkout_url: None,
                error: Some(
                    "No charge was made — the payment key on file could not be read, so this \
                     installation cannot take the payment. Please contact support so the team that \
                     runs it can activate this plan for your workspace."
                        .into(),
                ),
                plan: None,
                monthly_price: None,
                monthly_zc_pool: None,
            }));
        }
    };

    let base_url = std::env::var("APP_BASE_URL")
        .unwrap_or_else(|_| "https://app.incentiveswift.com".to_string());
    let success_url = req
        .success_url
        .unwrap_or_else(|| format!("{}/business-portal?loyalty=success", base_url));
    let cancel_url = req
        .cancel_url
        .unwrap_or_else(|| format!("{}/business-portal?loyalty=cancelled", base_url));

    let plan_name = plan.name.clone();
    let monthly_price = plan.monthly_price;
    let monthly_zc_pool = plan.monthly_zc_pool;

    let client = reqwest::Client::new();
    let params = [
        ("mode", "subscription"),
        ("payment_method_types[]", "card"),
        ("line_items[0][price_data][currency]", "usd"),
        (
            "line_items[0][price_data][unit_amount]",
            &monthly_price.to_string(),
        ),
        ("line_items[0][price_data][recurring][interval]", "month"),
        (
            "line_items[0][price_data][product_data][name]",
            // Neutral on purpose. This line item is what a business sees on a receipt when it buys a
            // loyalty plan, and the engine must not print one customer's brand onto everyone's invoice.
            // The business's own points name belongs here once it is set on its programme.
            &format!("Loyalty — {} ({} points/mo)", plan_name, monthly_zc_pool),
        ),
        ("line_items[0][quantity]", "1"),
        ("success_url", &success_url),
        ("cancel_url", &cancel_url),
        ("metadata[account_id]", &account_id.to_string()),
        ("metadata[plan_slug]", &req.plan_slug),
        ("metadata[monthly_zc_pool]", &monthly_zc_pool.to_string()),
        ("metadata[loyalty_subscription]", "true"),
    ];

    let api_base =
        std::env::var("STRIPE_API_BASE").unwrap_or_else(|_| "https://api.stripe.com".to_string());

    let resp = client
        .post(format!("{api_base}/v1/checkout/sessions"))
        .header("Authorization", format!("Bearer {}", stripe_key))
        .form(&params)
        .send()
        .await;

    match resp {
        Ok(r) if r.status().is_success() => {
            let json: serde_json::Value = r
                .json()
                .await
                .map_err(|e| AppError::Internal(e.to_string()))?;
            let stripe_session_id = json["id"].as_str().unwrap_or("").to_string();
            let checkout_url = json["url"].as_str().unwrap_or("").to_string();

            if !stripe_session_id.is_empty() {
                sqlx::query(
                    "INSERT INTO stripe_checkout_sessions (account_id, stripe_session_id, amount, credits, status)
                     VALUES ($1, $2, $3, $4, 'pending')"
                )
                .bind(&account_id)
                .bind(&stripe_session_id)
                .bind(monthly_price)
                .bind(monthly_zc_pool)
                .execute(&s.db)
                .await?;

                sqlx::query(
                    "UPDATE accounts SET loyalty_plan = $1, loyalty_plan_status = 'pending', subscription_id = $2, updated_at = NOW() WHERE id = $3"
                )
                .bind(&req.plan_slug)
                .bind(&stripe_session_id)
                .bind(&account_id)
                .execute(&s.db)
                .await?;
            }

            Ok(Json(SubscribeResponse {
                checkout_url: Some(checkout_url),
                error: None,
                plan: Some(plan_name),
                monthly_price: Some(monthly_price),
                monthly_zc_pool: Some(monthly_zc_pool),
            }))
        }
        Ok(r) => {
            // The provider's own refusal body is for the LOG, not for the customer. This arm used to
            // paint Stripe's raw JSON into the dashboard's status line — masked key fragment
            // included — on the one screen where a buyer is trying to give us money. The buyer is
            // told what happened to their money (nothing was charged); the detail goes where an
            // operator reads it.
            let provider_status = r.status();
            let err_body = r.text().await.unwrap_or_default();
            tracing::error!(
                %provider_status,
                %account_id,
                "loyalty/subscribe: the payment provider refused the checkout request: {err_body}"
            );
            Ok(Json(SubscribeResponse {
                checkout_url: None,
                error: Some(
                    "No charge was made — the payment provider refused the request before any \
                     money moved, so you have not been billed. Please contact support and the team \
                     that runs this installation will look into it."
                        .into(),
                ),
                plan: None,
                monthly_price: None,
                monthly_zc_pool: None,
            }))
        }
        Err(e) => {
            tracing::error!(%account_id, "loyalty/subscribe: could not reach the payment provider: {e}");
            Ok(Json(SubscribeResponse {
                checkout_url: None,
                error: Some(
                    "No charge was made — the payment provider could not be reached, so you have \
                     not been billed. Please try again in a moment; if it keeps failing, contact \
                     support."
                        .into(),
                ),
                plan: None,
                monthly_price: None,
                monthly_zc_pool: None,
            }))
        }
    }
}

/// Input for the operator's "switch this loyalty plan on" action.
///
/// The operator names the REQUEST they are honouring (`ticket_id`) — the request row already knows
/// which workspace filed it, so the console never has to carry another tenant's account id around.
/// `account_id` stays for a direct operator call that has no ticket in front of it.
#[derive(serde::Deserialize)]
pub struct OperatorActivateInput {
    #[serde(default)]
    pub ticket_id: Option<String>,
    #[serde(default)]
    pub account_id: Option<String>,
    pub plan_slug: String,
}

/// POST /api/v1/admin/loyalty/activate — the operator switches a workspace's loyalty plan on.
///
/// WHY THIS EXISTS (cron 2026-11-08, measured live): a customer on an installation with no card
/// payment provider presses "Choose <plan>" on the loyalty catalogue; the dashboard files a
/// `loyalty_plan_request` support ticket (by design nothing switches a plan on by itself), the
/// operator reads it in the queue — and there was NO control anywhere in the product that turns
/// that plan on. The only writer of `accounts.loyalty_plan_status = 'active'` was the Stripe
/// webhook, which by definition never fires when no provider is stored, and the queue's Resolve
/// action only moves WORKSPACE plans (`plans`), so a loyalty request resolved green and switched
/// nothing. The buying request was a dead end.
///
/// The operator surface (`/api/v1/admin/*`) already refuses every non-platform-admin in
/// `security::auth::is_admin_surface`, so this handler carries no second role check. It writes
/// exactly the columns the paid webhook writes, so a hand-switched workspace is
/// indistinguishable from a paid one to every reader (`plan_status`, `loyalty_v2`'s gates).
pub async fn operator_activate(
    State(s): State<AppState>,
    Json(body): Json<OperatorActivateInput>,
) -> Result<Json<serde_json::Value>, AppError> {
    let slug = body.plan_slug.trim().to_string();
    if slug.is_empty() {
        return Err(AppError::BadRequest("plan_slug is required".to_string()));
    }

    // The workspace: from the request the operator is honouring, else from an explicit id.
    let account_id: Uuid = if let Some(tid) = body.ticket_id.as_deref().filter(|t| !t.is_empty()) {
        let ticket = Uuid::parse_str(tid)
            .map_err(|_| AppError::BadRequest("Invalid ticket_id".to_string()))?;
        sqlx::query_scalar::<_, Uuid>("SELECT tenant_id FROM support_tickets WHERE id = $1")
            .bind(ticket)
            .fetch_optional(&s.db)
            .await?
            .ok_or_else(|| AppError::NotFound("No such request.".to_string()))?
    } else if let Some(aid) = body.account_id.as_deref().filter(|a| !a.is_empty()) {
        Uuid::parse_str(aid).map_err(|_| AppError::BadRequest("Invalid account_id".to_string()))?
    } else {
        return Err(AppError::BadRequest(
            "Name the request (ticket_id) or the workspace (account_id).".to_string(),
        ));
    };

    // The plan must exist AND still be on sale: seating a retired loyalty plan is never intended,
    // and refusing it by name is more useful than writing a tier the catalogue cannot show.
    let plan: Option<(String, i32)> = sqlx::query_as(
        "SELECT name, monthly_zc_pool FROM loyalty_plans WHERE slug = $1 AND is_active = true",
    )
    .bind(&slug)
    .fetch_optional(&s.db)
    .await?;
    let (plan_name, pool) = plan.ok_or_else(|| {
        AppError::BadRequest(format!("No loyalty plan called '{slug}' is on sale."))
    })?;

    let updated = sqlx::query(
        "UPDATE accounts SET loyalty_plan = $1, loyalty_plan_status = 'active', \
         zc_pool_remaining = $2, zc_pool_total = $2, \
         pool_reset_date = (NOW()::date + INTERVAL '30 days'), updated_at = NOW() \
         WHERE id = $3",
    )
    .bind(&slug)
    .bind(pool)
    .bind(account_id)
    .execute(&s.db)
    .await?;
    if updated.rows_affected() == 0 {
        return Err(AppError::NotFound("No such workspace.".to_string()));
    }

    // The credit ledger records the allocation, exactly as the paid path does.
    sqlx::query(
        "INSERT INTO credit_transactions (account_id, amount, action, description, balance_after, created_at) \
         VALUES ($1, $2, 'operator_activation', $3, $2, NOW())",
    )
    .bind(account_id)
    .bind(pool)
    .bind(format!(
        "{plan_name} loyalty plan switched on by the operator"
    ))
    .execute(&s.db)
    .await?;

    Ok(Json(serde_json::json!({
        "account_id": account_id,
        "plan": plan_name,
        "slug": slug,
        "zc_pool": pool,
        "status": "active",
        "message": format!(
            "{plan_name} is now on for this workspace, with {pool} credits in its pool."
        )
    })))
}
