//! Authentication — API key validation (bcrypt) and Supabase JWT validation.
//!
//! SECURITY RULES:
//! - API keys: NEVER compare via direct hash equality. ALWAYS use bcrypt::verify.
//! - JWTs: Decode header+payload, verify HMAC-SHA256 signature with Supabase anon key.

use crate::error::AppError;
use axum::{
    extract::FromRequestParts,
    http::{request::Parts},
    async_trait,
    RequestPartsExt,
};
use axum::extract::FromRef;
use sqlx::Row;

/// Authenticated user context extracted from request.
#[derive(Debug, Clone)]
pub struct AuthenticatedUser {
    pub account_id: String,
    pub email: String,
    pub role: String,
}

/// Extracts and validates Bearer token from Authorization header.
/// Supports both API keys (bcrypt-verified) and Supabase JWTs.
#[async_trait]
impl<S> FromRequestParts<S> for AuthenticatedUser
where
    S: Send + Sync,
    crate::state::AppState: FromRef<S>,
{
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        let token = parts
            .headers
            .get("Authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or_else(|| AppError::Unauthorized("Missing or invalid Authorization header".to_string()))?;

        let app_state = crate::state::AppState::from_ref(state);

        // Try API key validation first (bcrypt verify)
        if let Some(user) = validate_api_key(&app_state, token).await? {
            return Ok(user);
        }

        // Fall back to Supabase JWT validation
        let claims = crate::security::jwt::verify_supabase_jwt(
            token,
            &app_state.config.supabase_service_key,
        )?;

        Ok(AuthenticatedUser {
            account_id: claims.sub.clone().unwrap_or_default(),
            email: claims.email.clone().unwrap_or_default(),
            role: claims.role.clone().unwrap_or_else(|| "authenticated".to_string()),
        })
    }
}

/// Validate an API key by extracting its identifier prefix, looking up the hash,
/// and verifying with bcrypt::verify.
async fn validate_api_key(
    state: &crate::state::AppState,
    token: &str,
) -> Result<Option<AuthenticatedUser>, AppError> {
    // Extract key identifier (first 8 chars of the key, or use a prefix scheme)
    // API keys should be formatted as: "is_key_<identifier>_<secret>"
    let parts: Vec<&str> = token.split('_').collect();
    if parts.len() < 3 || parts[0] != "is" || parts[1] != "key" {
        return Ok(None);
    }

    let identifier = parts[1..parts.len()-1].join("_");
    let _secret = parts.last().unwrap_or(&"");

    // Look up the stored hash by identifier
    let row = sqlx::query(
        "SELECT ac.key_hash, ac.account_id::text, a.email
         FROM api_credentials ac
         JOIN accounts a ON a.id = ac.account_id
         WHERE ac.key_identifier = $1"
    )
    .bind(&identifier)
    .fetch_optional(&state.db)
    .await?;

    match row {
        Some(r) => {
            let stored_hash: String = r.get("key_hash");
            let account_id: String = r.get("account_id");
            let email: String = r.get("email");

            // Verify with bcrypt — this is the correct way
            // bcrypt::verify is intentionally slow and salted
            match bcrypt::verify(token, &stored_hash) {
                Ok(true) => Ok(Some(AuthenticatedUser {
                    account_id,
                    email,
                    role: "api_key".to_string(),
                })),
                _ => Err(AppError::Unauthorized("Invalid API key".to_string())),
            }
        }
        None => Ok(None), // Not an API key, try JWT validation
    }
}
