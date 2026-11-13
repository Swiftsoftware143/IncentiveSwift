//! The `AuthUser` extractor the fleet-canonical `integration_schema` module reads.
//!
//! ADMIN ONLY, two arms (an admin session OR the admin's API key):
//!   * a session JWT whose `role` claim is `admin`/`super_admin` AND whose `sub` still names an
//!     `accounts` row that grants that role (a demoted admin's old token must not pass — the same
//!     live-row rule `security::auth::admin_guard` enforces); or
//!   * an issued API key whose OWNER account's role is `admin`/`super_admin`.
//!
//! A missing credential is refused by the app's default-deny boundary (`security::route_policy`)
//! before this extractor runs; a credential that is present but not admin resolves to
//! `is_admin == false` and the handler answers 403.

use axum::async_trait;
use axum::extract::{FromRef, FromRequestParts};
use axum::http::request::Parts;

use crate::error::AppError;
use crate::state::AppState;

#[derive(Debug, Clone)]
pub struct AuthUser {
    pub is_admin: bool,
}

#[async_trait]
impl<S> FromRequestParts<S> for AuthUser
where
    S: Send + Sync,
    AppState: FromRef<S>,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let app_state = AppState::from_ref(state);
        let token = parts
            .headers
            .get("Authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or_else(|| {
                AppError::Unauthorized("Missing or invalid Authorization header".to_string())
            })?;

        let is_admin = resolve_is_admin(&app_state, token).await?;
        Ok(AuthUser { is_admin })
    }
}

async fn resolve_is_admin(state: &AppState, token: &str) -> Result<bool, AppError> {
    // Arm 1: an issued API key. Its OWNER account's live role decides admin (an API key resolves to
    // the literal `api_key` role, so the account row is the only honest source of authority).
    if let Some(user) = crate::security::auth::validate_api_key(state, token).await? {
        return account_is_platform_admin(&state.pool, &user.account_id).await;
    }

    // Arm 2: a session JWT.
    let claims = crate::security::jwt::verify_jwt(token, &state.config.jwt_secret)?;
    let role = claims.role.clone().unwrap_or_default();
    if !crate::security::auth::is_platform_admin(&role) {
        return Ok(false);
    }
    match claims.sub.as_deref() {
        Some(sub) => account_is_platform_admin(&state.pool, sub).await,
        None => Ok(false),
    }
}

/// Does `account_id` name an `accounts` row whose role is still a platform admin? `false` (never an
/// error) for an unparseable id or a missing/demoted row — fail closed.
async fn account_is_platform_admin(
    pool: &sqlx::PgPool,
    account_id: &str,
) -> Result<bool, AppError> {
    let Ok(id) = uuid::Uuid::parse_str(account_id) else {
        return Ok(false);
    };
    let role: Option<String> = sqlx::query_scalar("SELECT role FROM accounts WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(role
        .as_deref()
        .is_some_and(crate::security::auth::is_platform_admin))
}
