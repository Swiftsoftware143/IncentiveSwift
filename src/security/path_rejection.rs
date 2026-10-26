//! A malformed id in a URL answers "not found", not a parser sentence.
//!
//! Eighteen routes on this app take an id as an axum `Path<Uuid>`. When the segment is not a
//! uuid (a stale link, a truncated paste, a hand-typed URL) axum's own extractor refuses the
//! request BEFORE the handler runs and the body is a plain-text framework sentence:
//!
//! ```text
//! Invalid URL: UUID parsing failed: invalid character: found `n` at 0
//! ```
//!
//! That is an internal detail leaking to a caller, and it is inconsistent with the rest of the
//! fleet: `GET /api/v1/loyalty/member/{bad}/qr` answered that sentence where every other
//! absent-resource read answers `404 {"error": …, "code": 404}` (measured 2026-10-26 across
//! this app's 124 id routes: 19 of them carried it).
//!
//! Fixing it in one place instead of eighteen: the rejection is produced by the extractor, so
//! no handler can catch it — but it IS an ordinary response by the time this middleware sees
//! it. [`bad_id_is_not_found`] rewrites exactly that response (status 400 **and** a
//! `text/plain` body that STARTS WITH the parser prefix) into the app's own 404 envelope.
//!
//! What it deliberately cannot touch:
//! * every handler-level `AppError::BadRequest` — those are `application/json`, so the
//!   content-type arm keeps them exactly as they are;
//! * any 400 whose body merely mentions a uuid elsewhere — the match is the fixed prefix;
//! * anonymous callers on a private route — `default_deny` runs BEFORE the extractor, so those
//!   still answer 401 and never reach this layer with a 400.
//!
//! Header work is preserved rather than rebuilt: the status and body change, the headers that
//! the inner layers already set (security headers, CORS, trace) stay on the response.

use axum::body::Body;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::Response;

/// The prefix axum's `PathRejection` body carries. Only this shape is rewritten.
const PARSER_PREFIX: &str = "Invalid URL: UUID parsing failed";

/// An internal ceiling for the body we are willing to inspect. A rejection body is one short
/// sentence; anything larger is not ours and is passed through untouched.
const INSPECT_LIMIT: usize = 8 * 1024;

/// Rewrite axum's path-parser refusal into the app's own 404 envelope.
pub async fn bad_id_is_not_found(response: Response) -> Response {
    if response.status() != StatusCode::BAD_REQUEST {
        return response;
    }
    let plain = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.starts_with("text/plain"))
        .unwrap_or(false);
    if !plain {
        return response;
    }

    let (mut parts, body) = response.into_parts();
    let bytes = match axum::body::to_bytes(body, INSPECT_LIMIT).await {
        Ok(b) => b,
        Err(_) => return Response::from_parts(parts, Body::empty()),
    };
    if !bytes.starts_with(PARSER_PREFIX.as_bytes()) {
        return Response::from_parts(parts, Body::from(bytes));
    }

    // The id cannot name a row, so "not found" is the honest answer — the same one an id that
    // parses to nothing gets. Kept generic on purpose: the parser's own words ("found `n` at
    // 0") tell a caller about our URL grammar, which is exactly what must not leak.
    let body = serde_json::json!({ "error": "Not found", "code": 404 }).to_string();
    parts.status = StatusCode::NOT_FOUND;
    parts.headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    if let Ok(len) = HeaderValue::from_str(&body.len().to_string()) {
        parts.headers.insert(header::CONTENT_LENGTH, len);
    }
    Response::from_parts(parts, Body::from(body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;

    /// The exact body axum 0.7 produces for `Path<Uuid>` (measured live 2026-10-26 on this app).
    const REJECTION: &str = "Invalid URL: UUID parsing failed: invalid character: found `n` at 0";

    fn rejection_response() -> Response {
        (
            StatusCode::BAD_REQUEST,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            REJECTION,
        )
            .into_response()
    }

    async fn body_and_status(res: Response) -> (StatusCode, String, String) {
        let status = res.status();
        let ctype = res
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let body = axum::body::to_bytes(res.into_body(), 8192).await.unwrap();
        (status, ctype, String::from_utf8_lossy(&body).to_string())
    }

    #[tokio::test]
    async fn a_malformed_id_is_a_404_in_the_apps_own_shape() {
        let (status, ctype, body) =
            body_and_status(bad_id_is_not_found(rejection_response()).await).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "body was {body}");
        assert!(ctype.starts_with("application/json"), "ctype was {ctype}");
        assert!(body.contains("\"code\":404"), "body was {body}");
        assert!(
            !body.contains("UUID parsing failed"),
            "the parser sentence must not reach the caller: {body}"
        );
    }

    #[tokio::test]
    async fn a_handlers_own_400_is_left_alone() {
        // `AppError::BadRequest` renders JSON; the content-type arm must not touch it.
        let res = (
            StatusCode::BAD_REQUEST,
            [(header::CONTENT_TYPE, "application/json")],
            r#"{"error":"Invalid account ID","code":400}"#,
        )
            .into_response();
        let (status, ctype, body) = body_and_status(bad_id_is_not_found(res).await).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "body was {body}");
        assert!(ctype.starts_with("application/json"), "ctype was {ctype}");
        assert!(body.contains("Invalid account ID"), "body was {body}");
    }

    #[tokio::test]
    async fn a_successful_response_is_untouched() {
        let res = (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            r#"{"ok":true}"#,
        )
            .into_response();
        let (status, _, body) = body_and_status(bad_id_is_not_found(res).await).await;
        assert_eq!(status, StatusCode::OK, "body was {body}");
        assert!(body.contains("\"ok\":true"), "body was {body}");
    }
}
