//! Checkout Session handlers.
//!
//! Endpoints:
//!   POST /api/v1/checkout/create    — create a checkout session
//!   GET  /api/v1/checkout/sessions  — list checkout sessions

use crate::error::AppError;
use crate::security::auth::AuthenticatedUser;
use crate::state::AppState;
use axum::{extract::State, Json};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Input types
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

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

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
    let mut payment_provider = body
        .payment_provider
        .clone()
        .unwrap_or_else(|| String::from("stripe"));
    if payment_provider == "stripe" {
        if let Some(ref pid) = body.plan_id {
            if let Ok(uuid) = Uuid::parse_str(pid) {
                if let Ok(pp) = sqlx::query_scalar::<_, Option<String>>(
                    "SELECT payment_provider FROM plans WHERE id = $1",
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
            sqlx::query_scalar::<_, Option<String>>("SELECT thank_you_url FROM plans WHERE id = $1")
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
        "#,
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
