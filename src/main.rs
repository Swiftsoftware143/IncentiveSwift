//! IncentiveSwift — Multi-tenant Engagement & Capture Engine
//!
//! REST API server providing gamified incentive mechanics, raffle/giveaway system,
//! long-form qualifier, and loyalty program module.

mod config;
mod state;
mod error;
mod db;
pub mod handlers;
pub mod delivery;
pub mod mechanics;
pub mod access;
pub mod security;

use axum::{
    routing::{get, post},
    Router,
    middleware,
};
use tokio::signal;
use tower_http::{
    cors::CorsLayer,
    trace::TraceLayer,
    compression::CompressionLayer,
    timeout::TimeoutLayer,
};
use tracing_subscriber::EnvFilter;
use std::time::Duration;
use std::sync::Arc;
use governor::clock::QuantaClock;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(true)
        .with_thread_ids(true)
        .init();

    // Load configuration
    let config = config::AppConfig::from_env()?;
    let config = Arc::new(config);

    // Build shared state
    let state = state::AppState::new(&config).await?;

    // Shared rate limiter for public endpoints (20 req/min per IP tracked in middleware)
    let public_limiter = Arc::new(governor::RateLimiter::direct(
        governor::Quota::per_minute(
            std::num::NonZeroU32::new(20).unwrap(),
        ),
    ));

    // Build router
    let app = Router::new()
        // Public routes
        .route("/api/v1/health", get(handlers::health::health_check))
        .route("/api/v1/campaigns/{slug}", get(handlers::campaigns::get_campaign))
        .route("/api/v1/entries", post(handlers::entries::create_entry))
        .route("/api/v1/raffles/{slug}/enter", post(handlers::raffles::enter_raffle))
        .route("/api/v1/loyalty/checkin", post(handlers::loyalty::checkin))
        // Authenticated routes
        .route("/api/v1/campaigns", get(handlers::campaigns::list_campaigns).post(handlers::campaigns::create_campaign))
        .route("/api/v1/raffles/{slug}/draw", post(handlers::raffles::draw))
        .route("/api/v1/raffles/{slug}/redraw", post(handlers::raffles::redraw))
        .route("/api/v1/loyalty/rewards/{id}/approve", post(handlers::loyalty::approve_reward))
        .route("/api/v1/loyalty/rewards/{id}/deny", post(handlers::loyalty::deny_reward))
        .route("/api/v1/delivery/resend", post(handlers::delivery::resend))
        .route("/api/v1/contacts", get(handlers::contacts::list_contacts))
        .route("/api/v1/contacts/{id}", get(handlers::contacts::get_contact))
        // Middleware — order matters: outer layers wrap inner
        .layer(middleware::from_fn(security::rate_limit::rate_limit_middleware))
        .layer(middleware::from_fn(security::headers::add_security_headers))
        .layer(TimeoutLayer::new(Duration::from_secs(30)))
        .layer(CompressionLayer::new())
        .layer(TraceLayer::new_for_http())
        .layer(
            CorsLayer::new()
                .allow_origin(tower_http::cors::Any)
                .allow_methods(tower_http::cors::Any)
                .allow_headers(tower_http::cors::Any),
        )
        .with_state(state);

    // Start server
    let addr = format!("{}:{}", config.host, config.port);
    tracing::info!("Starting IncentiveSwift API on {}", addr);

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    tracing::info!("Shutdown signal received, starting graceful shutdown...");
}
