//! Loyalty plans — subscription tiers for business loyalty program

use axum::{extract::State, Json};
use serde::Serialize;
use uuid::Uuid;

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

    let row = sqlx::query_as::<_, (Option<String>, String, i32, i32, Option<chrono::NaiveDate>)>(
        "SELECT loyalty_plan, loyalty_plan_status, zc_pool_remaining, zc_pool_total, pool_reset_date FROM accounts WHERE id = $1::uuid"
    )
    .bind(&account_id)
    .fetch_optional(&s.db)
    .await?
    .unwrap_or((None, "inactive".to_string(), 0, 0, None));

    Ok(Json(PlanStatusResponse {
        enrolled: row.0.is_some() && row.1 == "active",
        plan: row.0,
        status: row.1,
        zc_pool_remaining: row.2,
        zc_pool_total: row.3,
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
    let existing: Option<String> =
        sqlx::query_scalar("SELECT loyalty_plan_status FROM accounts WHERE id = $1")
            .bind(&account_id)
            .fetch_optional(&s.db)
            .await?;

    if existing.as_deref() == Some("active") {
        return Ok(Json(SubscribeResponse {
            checkout_url: None,
            error: Some("You already have an active loyalty plan".into()),
            plan: None,
            monthly_price: None,
            monthly_zc_pool: None,
        }));
    }

    // Get Stripe key
    let stripe_key: Option<String> = sqlx::query_scalar(
        "SELECT api_key FROM provider_keys WHERE provider = 'stripe' AND is_active = true LIMIT 1",
    )
    .fetch_optional(&s.db)
    .await?;

    let stripe_key = match stripe_key {
        Some(k) if !k.is_empty() => k,
        _ => {
            return Ok(Json(SubscribeResponse {
                checkout_url: None,
                error: Some("Stripe not configured. Add your Stripe key in Provider Keys.".into()),
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
            &format!(
                "ZaarHub Loyalty — {} ({} ZC/mo)",
                plan_name, monthly_zc_pool
            ),
        ),
        ("line_items[0][quantity]", "1"),
        ("success_url", &success_url),
        ("cancel_url", &cancel_url),
        ("metadata[account_id]", &account_id.to_string()),
        ("metadata[plan_slug]", &req.plan_slug),
        ("metadata[monthly_zc_pool]", &monthly_zc_pool.to_string()),
        ("metadata[loyalty_subscription]", "true"),
    ];

    let resp = client
        .post("https://api.stripe.com/v1/checkout/sessions")
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
            let err_body = r.text().await.unwrap_or_default();
            tracing::error!("Stripe checkout creation failed: {}", err_body);
            Ok(Json(SubscribeResponse {
                checkout_url: None,
                error: Some(format!("Stripe error: {}", err_body)),
                plan: None,
                monthly_price: None,
                monthly_zc_pool: None,
            }))
        }
        Err(e) => {
            tracing::error!("Stripe request failed: {}", e);
            Ok(Json(SubscribeResponse {
                checkout_url: None,
                error: Some(format!("Could not reach Stripe: {}", e)),
                plan: None,
                monthly_price: None,
                monthly_zc_pool: None,
            }))
        }
    }
}
