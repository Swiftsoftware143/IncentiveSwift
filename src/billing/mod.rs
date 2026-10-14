//! Billing module — payment providers, checkout sessions, and webhooks.
//!
//! Routes:
//!   GET  /api/v1/payment-providers              -> providers::list
//!   POST /api/v1/payment-providers              -> providers::upsert
//!   DELETE /api/v1/payment-providers/:type       -> providers::delete
//!   POST /api/v1/checkout/create                -> checkout::create
//!   GET  /api/v1/checkout/sessions              -> checkout::list
//!   POST /api/v1/webhooks/stripe                -> webhooks::stripe
//!   POST /api/v1/webhooks/paypal                -> webhooks::paypal
//!
//! `stripe_signature` is the ONE parse-and-verify for a `Stripe-Signature` header, shared by both
//! Stripe receivers in this app: `webhooks::stripe_webhook` (the payment receiver) and
//! `handlers::stripe_webhook::stripe_webhook` (the loyalty-plan receiver). Two receivers must not
//! be able to disagree about the header or about the bytes Stripe signed (kanban t_1b8d96f6).

use crate::state::AppState;
use axum::{
    routing::{delete, get, post},
    Router,
};

pub mod checkout;
pub mod providers;
pub mod stripe_signature;
pub mod webhooks;

/// Build a router that nests all billing sub-routes under a shared state.
/// These are mounted at the top level (not under a `/billing` prefix) to
/// preserve existing path compatibility:
///   /api/v1/payment-providers, /api/v1/checkout/*, /api/v1/webhooks/*
pub fn router(state: AppState) -> Router<AppState> {
    Router::new()
        // Payment Providers
        .route(
            "/api/v1/payment-providers",
            get(providers::list_payment_providers).post(providers::upsert_payment_provider),
        )
        .route(
            "/api/v1/payment-providers/:provider_type",
            delete(providers::delete_payment_provider),
        )
        // Checkout Sessions
        .route(
            "/api/v1/checkout/create",
            post(checkout::create_checkout_session),
        )
        .route(
            "/api/v1/checkout/sessions",
            get(checkout::list_checkout_sessions),
        )
        // Webhooks (public — no auth; signature verification in handler)
        .route("/api/v1/webhooks/stripe", post(webhooks::stripe_webhook))
        .route("/api/v1/webhooks/paypal", post(webhooks::paypal_webhook))
        .with_state(state)
}
