//! Supabase JWT validation.
//!
//! Decodes and verifies Supabase JWTs using HMAC-SHA256.
//! The anon key (JWT secret) is used to verify the signature.

use crate::error::AppError;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use serde::Deserialize;
use base64::Engine;

type HmacSha256 = Hmac<Sha256>;

/// Claims extracted from a verified Supabase JWT.
#[derive(Debug, Deserialize)]
pub struct SupabaseClaims {
    pub sub: Option<String>,
    pub email: Option<String>,
    pub aud: Option<String>,
    pub role: Option<String>,
    pub iat: Option<i64>,
    pub exp: Option<i64>,
}

/// Decode and verify a Supabase JWT.
///
/// # Arguments
/// * `token` - The JWT string (without "Bearer " prefix)
/// * `jwt_secret` - The Supabase anon key (used as HMAC secret)
///
/// # Returns
/// The parsed claims if the token is valid, or an error.
pub fn verify_jwt(token: &str, jwt_secret: &str) -> Result<SupabaseClaims, AppError> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return Err(AppError::Unauthorized("Invalid JWT format".to_string()));
    }

    let (header_b64, payload_b64, signature_b64) = (parts[0], parts[1], parts[2]);

    // Verify signature using HMAC-SHA256
    let message = format!("{}.{}", header_b64, payload_b64);

    let mut mac = HmacSha256::new_from_slice(jwt_secret.as_bytes())
        .map_err(|_| AppError::Internal("Failed to create HMAC".to_string()))?;

    mac.update(message.as_bytes());

    let expected_sig = mac.finalize().into_bytes();
    let provided_sig = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(signature_b64)
        .map_err(|_| AppError::Unauthorized("Invalid JWT signature encoding".to_string()))?;

    // Constant-time comparison to prevent timing attacks
    if expected_sig.as_slice() != provided_sig.as_slice() {
        return Err(AppError::Unauthorized("Invalid JWT signature".to_string()));
    }

    // Decode payload
    let payload_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|_| AppError::Unauthorized("Invalid JWT payload encoding".to_string()))?;

    let claims: SupabaseClaims = serde_json::from_slice(&payload_bytes)
        .map_err(|e| AppError::Unauthorized(format!("Invalid JWT payload: {}", e)))?;

    // Check expiration
    if let Some(exp) = claims.exp {
        let now = chrono::Utc::now().timestamp();
        if now > exp {
            return Err(AppError::Unauthorized("JWT expired".to_string()));
        }
    }

    Ok(claims)
}
