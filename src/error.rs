//! Centralized error type for the application.
//! All errors implement IntoResponse so they never panic on the request path.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("Not found: {0}")]
    NotFound(String),

    #[error("Bad request: {0}")]
    BadRequest(String),

    #[error("Unauthorized: {0}")]
    Unauthorized(String),

    #[error("Forbidden: {0}")]
    Forbidden(String),

    /// A write that is well-formed but refused because it would break an invariant the rest of the
    /// system relies on — today: attaching a survey to a campaign when another campaign already
    /// runs it (kanban t_6c8d8e40, one funnel <=> one campaign). Distinct from BadRequest so the
    /// caller can tell "you typed something wrong" from "the thing you asked for is taken".
    #[error("Conflict: {0}")]
    Conflict(String),

    #[error("Rate limited")]
    RateLimited,

    /// A route the public can drive that spends a trial-and-error budget (e.g. guessing a
    /// promo code) answers this once the caller's window is burned. Distinct from RateLimited so
    /// the customer-facing message is actionable instead of "Rate limit exceeded".
    #[error("Too many requests: {0}")]
    TooManyRequests(String),

    /// 429 — too many FAILED sign-ins from one client address (`crate::login_throttle`). Its
    /// own variant so the answer can carry `Retry-After` as well as a sentence naming how long
    /// to wait; `TooManyRequests` above is the promo-code budget, which has no window to name.
    #[error("Too many failed sign-ins")]
    TooManyAttempts { retry_after_secs: u64 },
    #[error("Upgrade required: {0}")]
    UpgradeRequired(String),

    /// A body over the envelope a route accepts (2 MB on the image routes). Its own variant so the
    /// answer is `413`, not `400`: the payload was not malformed, it was too big. Added by the
    /// fleet-wide upload normalisation (2026-10-11) so the sentence a customer reads here is the
    /// same one the other five apps answer.
    #[error("Payload too large: {0}")]
    PayloadTooLarge(String),

    /// 422 — the body parsed and the caller is allowed to write, but a FIELD cannot be stored as
    /// sent, so the write is refused with the field and its width named.
    ///
    /// WHY NOT 400: the fleet's newer writers (FunnelSwift `handlers/lead_handler.rs`, ADASwift
    /// `clients`) answer 422 for exactly this, and the served consoles distinguish "I mistyped"
    /// from "that value is too long". WHY NOT 500: before this variant an over-long value reached
    /// the INSERT and [`AppError::Database`] answered `500 {"code":500,"error":"Internal server
    /// error"}` — measured LIVE on this app 2026-10-17 on a fresh probe account for an IQS funnel
    /// name/slug, an API-key label, a knowledge-base title, and a contact with neither an email nor
    /// a phone. Nothing the operator could act on, and every one of them looked like the app was
    /// broken.
    #[error("Unprocessable: {0}")]
    UnprocessableEntity(String),

    /// 424 — an integration PARTNER was reached and refused (or could not be reached).
    ///
    /// WHY 424 AND NOT 500: `GET /api/v1/integrations/coreswift/lists` mapped the hub's refusal
    /// onto [`AppError::Internal`], whose arm LOGS the reason and answers the caller a bare
    /// "Internal server error" — measured live 2026-10-15: a tenant whose stored key CoreSwift
    /// refuses clicked "Test — fetch lists" and got `{"code":500,"error":"Internal server
    /// error"}`, indistinguishable from our own server breaking and impossible to act on. 424
    /// Failed Dependency means exactly "the action depended on another action and that action
    /// failed", it carries the partner's own words, and being a 4xx Cloudflare passes the body
    /// through verbatim (CF REPLACES an origin 502/504 body with its own page). Same class fixed
    /// in FunnelSwift (fbf7753) and missedcallrespondr (5d1d858).
    #[error("Upstream refused: {0}")]
    UpstreamRefused(String),

    /// 503 — the route is fine and the request is fine, but a CONFIGURATION this surface needs is
    /// absent, so there is nothing to authenticate or act with.
    ///
    /// WHY NOT [`AppError::Internal`]: the receiver's config arm used to return `Internal`, whose
    /// arm LOGS the reason and answers the caller a bare "Internal server error" — measured live
    /// 2026-10-15 on `POST /api/v1/loyalty/webhook/stripe`: an unconfigured install answered a
    /// bogus `Stripe-Signature` with `500 {"code":500,"error":"Internal server error"}`. A payment
    /// provider cannot act on that (it is not "our server is broken", it is "nothing here can
    /// verify you", and Stripe retries a 5xx forever), and the operator is told nothing. This
    /// variant carries the REASON to the caller, and the status class is the one every sibling
    /// fleet receiver answers the same condition with: FunnelSwift
    /// `handlers/checkout_handler.rs::stripe_rejection` returns
    /// `503 {"error":"stripe_webhook_secret_not_configured"}` (kanban t_b474bd74).
    ///
    /// The message is a REASON SLUG, never a configured value, so echoing it is safe.
    #[error("Service unavailable: {0}")]
    ServiceUnavailable(String),

    #[error("Internal error: {0}")]
    Internal(String),

    #[error("Database error: {0}")]
    Database(String),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        // The paused-sign-in answer carries its retry budget in a header as well as in the
        // sentence, so a client (and a log reader) can obey the pause without parsing prose.
        let retry_after_secs = match &self {
            AppError::TooManyAttempts { retry_after_secs } => Some(*retry_after_secs),
            _ => None,
        };
        let (status, message) = match &self {
            AppError::NotFound(msg) => (StatusCode::NOT_FOUND, msg.clone()),
            AppError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg.clone()),
            AppError::Unauthorized(msg) => (StatusCode::UNAUTHORIZED, msg.clone()),
            AppError::Forbidden(msg) => (StatusCode::FORBIDDEN, msg.clone()),
            AppError::Conflict(msg) => (StatusCode::CONFLICT, msg.clone()),
            AppError::UpgradeRequired(msg) => (StatusCode::PAYMENT_REQUIRED, msg.clone()),
            AppError::PayloadTooLarge(msg) => (StatusCode::PAYLOAD_TOO_LARGE, msg.clone()),
            AppError::UnprocessableEntity(msg) => (StatusCode::UNPROCESSABLE_ENTITY, msg.clone()),
            // 503, and the REASON travels to the caller: this is a configuration refusal, not a
            // defect, and the answer must name what is missing (kanban t_b474bd74).
            AppError::ServiceUnavailable(msg) => {
                tracing::warn!("Service unavailable: {}", msg);
                (StatusCode::SERVICE_UNAVAILABLE, msg.clone())
            }
            // 424, and the provider's own sentence is the body — see the variant's doc.
            AppError::UpstreamRefused(msg) => (StatusCode::FAILED_DEPENDENCY, msg.clone()),
            AppError::RateLimited => (
                StatusCode::TOO_MANY_REQUESTS,
                "Rate limit exceeded".to_string(),
            ),
            AppError::TooManyRequests(msg) => (StatusCode::TOO_MANY_REQUESTS, msg.clone()),
            AppError::TooManyAttempts { retry_after_secs } => {
                let minutes = retry_after_secs.div_ceil(60).max(1);
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    format!(
                        "Too many failed sign-in attempts from this address. Try again in about \
                         {minutes} minute{}. If you have forgotten your password, use \
                         \"Forgot your password?\".",
                        if minutes == 1 { "" } else { "s" }
                    ),
                )
            }
            AppError::Internal(msg) => {
                tracing::error!("Internal error: {}", msg);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Internal server error".to_string(),
                )
            }
            AppError::Database(msg) => {
                tracing::error!("Database error: {}", msg);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Internal server error".to_string(),
                )
            }
        };

        let body = Json(json!({
            "error": message,
            "code": status.as_u16(),
        }));

        let mut response = (status, body).into_response();
        if let Some(value) = retry_after_secs
            .and_then(|secs| axum::http::HeaderValue::from_str(&secs.to_string()).ok())
        {
            response
                .headers_mut()
                .insert(axum::http::header::RETRY_AFTER, value);
        }
        response
    }
}

impl From<sqlx::Error> for AppError {
    fn from(err: sqlx::Error) -> Self {
        AppError::Database(err.to_string())
    }
}

impl From<anyhow::Error> for AppError {
    fn from(err: anyhow::Error) -> Self {
        AppError::Internal(err.to_string())
    }
}
