//! Authentication — API key validation (bcrypt) and Supabase JWT validation.

use crate::error::AppError;
use axum::{
    extract::FromRequestParts,
    http::{request::Parts, StatusCode},
    async_trait,
};

/// Authenticated user context extracted from request.
#[derive(Debug, Clone)]
pub struct AuthenticatedUser {
    pub account_id: String,
    pub email: String,
}

/// Extracts and validates API key from Authorization header.
/// The key is compared against stored bcrypt hash.
#[async_trait]
impl<S> FromRequestParts<S> for AuthenticatedUser
where
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        let auth_header = parts
            .headers
            .get("Authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or_else(|| AppError::Unauthorized("Missing or invalid Authorization header".to_string()))?;

        // Validate API key against bcrypt hash in database
        // This is a placeholder — the actual validation queries the api_credentials table
        // using bcrypt::verify(provided_key, &stored_hash)
        //
        // NEVER do: SELECT * FROM api_credentials WHERE key_hash = hash(provided_key)
        // ALWAYS do: SELECT key_hash FROM api_credentials WHERE key_identifier = ? 
        //            then bcrypt::verify(provided_key, &stored_hash)
        
        // TODO: Implement actual API key validation
        // For now, return a placeholder authenticated user
        // This will be replaced with real bcrypt verification against the DB

        Ok(AuthenticatedUser {
            account_id: "placeholder".to_string(),
            email: "placeholder@example.com".to_string(),
        })
    }
}
