//! Health check handler.

use axum::{Json, http::StatusCode};
use serde_json::{json, Value};

pub async fn health_check() -> (StatusCode, Json<Value>) {
    (
        StatusCode::OK,
        Json(json!({
            "status": "ok",
            "service": "incentiveswift-api",
            "version": "0.1.0",
        })),
    )
}
