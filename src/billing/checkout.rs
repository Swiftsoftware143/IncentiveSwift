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
use crate::state::AppState;
use axum::http::StatusCode;
use axum::{extract::State, Json};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Input types
// ---------------------------------------------------------------------------

/// Input for creating a checkout session. Every field is echoed back in a refusal (see
/// `requested` below) so a caller can see the whole request that was turned down.
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
///
/// Refuses, honestly and explicitly, unless a payment provider is configured *and* this app
/// implements that provider's checkout call. See the module docs for the arm table.
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

    // A zero/negative/non-finite price is not a checkout.
    if !body.price_amount.is_finite() || body.price_amount <= 0.0 {
        return Err(AppError::BadRequest(format!(
            "price_amount must be greater than zero (got {}); there is nothing to check out.",
            body.price_amount
        )));
    }

    // Resolve the account's own ACTIVE payment provider row.
    let provider: Option<(String, String)> = sqlx::query_as(
        "SELECT provider_type, api_key FROM payment_providers \
         WHERE account_id = $1 AND is_active = true ORDER BY created_at LIMIT 1",
    )
    .bind(account_id)
    .fetch_optional(&state.db)
    .await?;

    let Some((provider_type, stored_key)) = provider else {
        tracing::error!(
            %account_id,
            "checkout/create REFUSED: no active payment provider for account"
        );
        return Ok((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "error": "payment_provider_not_configured",
                "message": "No payment provider is configured for this account, so checkout cannot \
                            work. Add the provider's keys first (Integrations -> Provider Keys). No \
                            session was created and no charge can occur.",
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

    // provider_keys / payment_providers store the secret as ciphertext at rest; a row whose key
    // cannot be decrypted is the same as no key at all.
    let secret_key =
        crate::security::provider_key_crypto::decrypt_from_storage(&state.db, stored_key.trim())
            .await
            .ok()
            .filter(|k| !k.trim().is_empty());

    let Some(_secret_key) = secret_key else {
        tracing::error!(
            %account_id,
            "checkout/create REFUSED: stripe provider row has no usable api_key"
        );
        return Ok((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "error": "payment_provider_not_configured",
                "message": "A Stripe provider exists but its secret key is missing or unreadable, \
                            so no session was created and no charge can occur. Re-enter the key in \
                            Provider Keys.",
                "configured": false,
                "provider_type": provider_type,
                "requested": requested,
            })),
        ));
    };

    // A usable Stripe key IS configured — but this app has no Stripe checkout call yet. Answer
    // that truth instead of inventing a session: the only arm that ever existed here fabricated a
    // `checkout.example.com` URL. Wiring the real call is the next step, blocked on the credential.
    tracing::error!(
        %account_id,
        "checkout/create REFUSED: stripe configured but no Stripe checkout call implemented"
    );
    Ok((
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({
            "error": "checkout_not_implemented",
            "message": "A Stripe provider is configured, but this app does not implement the Stripe \
                        checkout call yet. No session was created and no charge can occur.",
            "configured": true,
            "provider_type": provider_type,
            "requested": requested,
        })),
    ))
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
