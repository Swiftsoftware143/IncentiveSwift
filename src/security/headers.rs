//! Security headers middleware applied globally via Tower.
//! CSP, HSTS, X-Frame-Options, X-Content-Type-Options, Referrer-Policy.

use axum::{
    body::Body,
    http::{Request, HeaderValue, StatusCode},
    middleware::Next,
    response::Response,
};

pub async fn add_security_headers(
    req: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    let mut response = next.run(req).await;

    let headers = response.headers_mut();

    headers.insert(
        "X-Content-Type-Options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        "X-Frame-Options",
        HeaderValue::from_static("DENY"),
    );
    headers.insert(
        "Referrer-Policy",
        HeaderValue::from_static("strict-origin-when-cross-origin"),
    );
    headers.insert(
        "X-XSS-Protection",
        HeaderValue::from_static("0"),
    );
    headers.insert(
        "Strict-Transport-Security",
        HeaderValue::from_static("max-age=63072000; includeSubDomains; preload"),
    );
    headers.insert(
        "Content-Security-Policy",
        HeaderValue::from_static(
            "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
             connect-src 'self' https://*.supabase.co; frame-ancestors 'none'; form-action 'self'",
        ),
    );

    Ok(response)
}
