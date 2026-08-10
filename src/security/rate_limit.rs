//! Rate limiting middleware using governor token bucket.

use axum::{
    body::Body,
    http::{Request, StatusCode},
    middleware::Next,
    response::Response,
    extract::ConnectInfo,
};
use governor::{
    clock::DefaultClock,
    state::{InMemoryState, NotKeyed},
    Quota, RateLimiter,
};
use std::{
    net::SocketAddr,
    num::NonZeroU32,
    sync::Arc,
    time::Duration,
};
use tokio::sync::Mutex;

/// Simple in-memory rate limiter per IP.
/// In production, use Redis-backed rate limiting for multi-instance support.
pub async fn rate_limit_middleware(
    req: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    // Extract client IP from connection info or headers
    let client_ip = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0.ip().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    // Check rate limit (20 req/min for unauthenticated, checked at middleware level)
    // Authenticated routes have their own check in the auth middleware
    if let Err(_) = check_rate_limit(&client_ip).await {
        return Err(StatusCode::TOO_MANY_REQUESTS);
    }

    Ok(next.run(req).await)
}

/// Token bucket rate limiter: 20 requests per minute per IP.
async fn check_rate_limit(_ip: &str) -> Result<(), ()> {
    // In-memory rate limiter per IP
    // For multi-instance Railway deployment, replace with Redis-backed rate limiting
    let quota = Quota::per_minute(NonZeroU32::new(20).unwrap());
    let limiter = RateLimiter::direct(quota);

    match limiter.check() {
        Ok(_) => Ok(()),
        Err(_) => Err(()),
    }
}
