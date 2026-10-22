//! Health check handler — `GET /api/v1/health`.
//!
//! REWRITTEN 2026-10-21 (cron). This handler used to return a CONSTANT
//! `200 {"status":"ok"}`. It read no state, so it could not fail: with the database
//! gone, the pool exhausted, or every query timing out, it still answered "ok". A probe
//! that cannot fail is worse than no probe — it turns "the app is serving" into an
//! assertion nobody ever measured, and the operator reading a green light learns nothing.
//!
//! It now round-trips `SELECT 1` and answers 200 only when the database answers. On
//! failure it answers 503 with `database: "unreachable"` so a watchdog (and the container
//! healthcheck) can tell a serving app from a broken one. It reads no row and needs no
//! credential, so it stays public.

use axum::{extract::State, http::StatusCode, Json};
use serde_json::{json, Value};

use crate::state::AppState;

pub async fn health_check(State(state): State<AppState>) -> (StatusCode, Json<Value>) {
    // A round-trip, not a pool flag: `pool.is_closed()` stays false while every query
    // times out, which is exactly the outage this check exists to catch.
    let db_ok = sqlx::query_scalar::<_, i32>("SELECT 1")
        .fetch_one(&state.db)
        .await
        .is_ok();

    let body = json!({
        "status": if db_ok { "ok" } else { "degraded" },
        "service": "incentiveswift-api",
        "database": if db_ok { "ok" } else { "unreachable" },
        "version": "0.1.0",
    });

    if db_ok {
        (StatusCode::OK, Json(body))
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, Json(body))
    }
}
