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
    #[error("Upgrade required: {0}")]
    UpgradeRequired(String),

    /// A body over the envelope a route accepts (2 MB on the image routes). Its own variant so the
    /// answer is `413`, not `400`: the payload was not malformed, it was too big. Added by the
    /// fleet-wide upload normalisation (2026-10-11) so the sentence a customer reads here is the
    /// same one the other five apps answer.
    #[error("Payload too large: {0}")]
    PayloadTooLarge(String),

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

    #[error("Internal error: {0}")]
    Internal(String),

    #[error("Database error: {0}")]
    Database(String),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            AppError::NotFound(msg) => (StatusCode::NOT_FOUND, msg.clone()),
            AppError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg.clone()),
            AppError::Unauthorized(msg) => (StatusCode::UNAUTHORIZED, msg.clone()),
            AppError::Forbidden(msg) => (StatusCode::FORBIDDEN, msg.clone()),
            AppError::Conflict(msg) => (StatusCode::CONFLICT, msg.clone()),
            AppError::UpgradeRequired(msg) => (StatusCode::PAYMENT_REQUIRED, msg.clone()),
            AppError::PayloadTooLarge(msg) => (StatusCode::PAYLOAD_TOO_LARGE, msg.clone()),
            // 424, and the provider's own sentence is the body — see the variant's doc.
            AppError::UpstreamRefused(msg) => (StatusCode::FAILED_DEPENDENCY, msg.clone()),
            AppError::RateLimited => (
                StatusCode::TOO_MANY_REQUESTS,
                "Rate limit exceeded".to_string(),
            ),
            AppError::TooManyRequests(msg) => (StatusCode::TOO_MANY_REQUESTS, msg.clone()),
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

        (status, body).into_response()
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
