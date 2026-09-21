//! Authentication — API key validation (bcrypt) and JWT validation.
//!
//! SECURITY RULES:
//! - API keys: NEVER compare via direct hash equality. ALWAYS use bcrypt::verify.
//! - JWTs: Decode header+payload, verify HMAC-SHA256 signature with the JWT secret.

use crate::error::AppError;
use axum::extract::FromRef;
use axum::{async_trait, extract::FromRequestParts, http::request::Parts};
use sqlx::Row;

/// Authenticated user context extracted from request.
#[derive(Debug, Clone)]
pub struct AuthenticatedUser {
    pub account_id: String,
    pub email: String,
    pub role: String,
    pub impersonating: Option<String>,
}

/// Extracts and validates Bearer token from Authorization header.
/// Supports both API keys (bcrypt-verified) and JWTs.
#[async_trait]
impl<S> FromRequestParts<S> for AuthenticatedUser
where
    S: Send + Sync,
    crate::state::AppState: FromRef<S>,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let token = parts
            .headers
            .get("Authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or_else(|| {
                AppError::Unauthorized("Missing or invalid Authorization header".to_string())
            })?;

        let app_state = crate::state::AppState::from_ref(state);

        // Try API key validation first (bcrypt verify)
        if let Some(user) = validate_api_key(&app_state, token).await? {
            return Ok(user);
        }

        // Fall back to local JWT validation
        let claims = crate::security::jwt::verify_jwt(token, &app_state.config.jwt_secret)?;

        Ok(AuthenticatedUser {
            account_id: claims.sub.clone().unwrap_or_default(),
            email: claims.email.clone().unwrap_or_default(),
            role: claims
                .role
                .clone()
                .unwrap_or_else(|| "authenticated".to_string()),
            impersonating: claims.impersonating.clone(),
        })
    }
}

/// Resolve a bearer token that claims to be an IncentiveSwift API key.
///
/// Two credential stores exist and both are consulted, `api_keys` first:
///
/// 1. `api_keys` — what `POST /api/v1/api-keys` writes (the admin + tenant
///    "API Keys" screens). Keys are `is_key_<48 random alphanumerics>`; the stored
///    `prefix` is the first 8 characters of the random part and `key_hash` is a
///    bcrypt hash of the WHOLE key.
/// 2. `api_credentials` — legacy rows (`key_identifier` + bcrypt `key_hash`).
///
/// Returns `Ok(None)` when the token is not an API key at all, so the caller can
/// fall back to JWT validation; `Err(Unauthorized)` when it looks like one but the
/// secret does not match (or the key is deactivated/expired).
///
/// This is the single resolver behind BOTH `AuthenticatedUser` and
/// `POST /api/v1/api-keys/verify`, which is what makes the verify endpoint honest:
/// a key that verifies is a key that really authenticates.
pub(crate) async fn validate_api_key(
    state: &crate::state::AppState,
    token: &str,
) -> Result<Option<AuthenticatedUser>, AppError> {
    if let Some(user) = verify_issued_api_key(state, token).await? {
        return Ok(Some(user));
    }
    verify_legacy_api_credential(state, token).await
}

/// Issued keys are `is_key_<48 random alphanumerics>`; `api_keys.prefix` stores the
/// first 8 characters of the random part.
const ISSUED_KEY_PREFIX: &str = "is_key_";
const KEY_PREFIX_LEN: usize = 8;

/// The `api_keys.prefix` value for a presented token, or `None` when the token is not
/// an issued key at all.
///
/// Byte-safe on purpose: `str::get` refuses to slice inside a multi-byte character, so
/// a hand-crafted token (`is_key_aéééé`) cannot panic the request — a real panic here
/// was reachable unauthenticated on `POST /api/v1/api-keys/verify`. Such a token simply
/// is not a key; anything non-ASCII can never match a stored prefix (they are alnum).
fn issued_key_prefix(token: &str) -> Option<&str> {
    token
        .strip_prefix(ISSUED_KEY_PREFIX)
        .and_then(|secret| secret.get(..KEY_PREFIX_LEN))
}

/// Look a key up in `api_keys` (the store the CRUD API and the UI write).
///
/// `Ok(None)` = prefix unknown, not an issued key (caller may try the legacy store).
/// `Err(Unauthorized)` = the key exists but is inactive, expired, or the secret is wrong.
async fn verify_issued_api_key(
    state: &crate::state::AppState,
    token: &str,
) -> Result<Option<AuthenticatedUser>, AppError> {
    let Some(prefix) = issued_key_prefix(token) else {
        return Ok(None);
    };

    // The column has no unique constraint, so verify against every row that shares
    // the prefix rather than assuming the first one is the key.
    let rows = sqlx::query(
        "SELECT ak.key_hash, ak.user_id::text AS user_id, ak.is_active, ak.expires_at,
                COALESCE(a.email, '') AS email
           FROM api_keys ak
           LEFT JOIN accounts a ON a.id = ak.user_id
          WHERE ak.prefix = $1",
    )
    .bind(prefix)
    .fetch_all(&state.db)
    .await?;

    if rows.is_empty() {
        return Ok(None);
    }

    for r in &rows {
        let is_active: bool = r.try_get("is_active").unwrap_or(false);
        if !is_active {
            continue;
        }
        let expires_at: Option<chrono::DateTime<chrono::Utc>> = r.try_get("expires_at")?;
        if expires_at.is_some_and(|e| e <= chrono::Utc::now()) {
            continue;
        }

        let stored_hash: String = r.get("key_hash");
        // bcrypt::verify is the only correct comparison for a stored key hash.
        if bcrypt::verify(token, &stored_hash).unwrap_or(false) {
            return Ok(Some(AuthenticatedUser {
                account_id: r.get("user_id"),
                email: r.get("email"),
                role: "api_key".to_string(),
                impersonating: None,
            }));
        }
    }

    Err(AppError::Unauthorized("Invalid API key".to_string()))
}

/// Legacy store: `api_credentials.key_identifier` + bcrypt hash. Unchanged
/// behaviour — kept so any credential seeded outside `api_keys` keeps working.
async fn verify_legacy_api_credential(
    state: &crate::state::AppState,
    token: &str,
) -> Result<Option<AuthenticatedUser>, AppError> {
    // Extract key identifier (first 8 chars of the key, or use a prefix scheme)
    // API keys should be formatted as: "is_key_<identifier>_<secret>"
    let parts: Vec<&str> = token.split('_').collect();
    if parts.len() < 3 || parts[0] != "is" || parts[1] != "key" {
        return Ok(None);
    }

    let identifier = parts[1..parts.len() - 1].join("_");
    let _secret = parts.last().unwrap_or(&"");

    // Look up the stored hash by identifier
    let row = sqlx::query(
        "SELECT ac.key_hash, ac.account_id::text, a.email
         FROM api_credentials ac
         JOIN accounts a ON a.id = ac.account_id
         WHERE ac.key_identifier = $1",
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
                    impersonating: None,
                })),
                _ => Err(AppError::Unauthorized("Invalid API key".to_string())),
            }
        }
        None => Ok(None), // Not an API key, try JWT validation
    }
}

/// Fleet guard for `/api/v1/admin/*`.
///
/// SECURITY (2026-09-20): these routes previously answered 2xx to *anonymous*
/// callers (12 of them, including the state-changing
/// `POST /api/v1/admin/treasury/expire-points`). This middleware is applied
/// globally but only inspects admin paths, so non-admin traffic is untouched.
///
/// Allowed callers:
/// - a valid JWT or API key whose role is `admin` / `super_admin`;
/// - a sibling service presenting the shared `X-Internal-Sync-Key`.
pub async fn admin_guard(
    axum::extract::State(state): axum::extract::State<crate::state::AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<axum::response::Response, AppError> {
    let path = req.uri().path().to_string();
    if !path.starts_with("/api/v1/admin") {
        return Ok(next.run(req).await);
    }

    // Sibling-bot / cron bypass using the shared internal sync key.
    let sync_key = state.config.internal_sync_key.clone();
    if !sync_key.is_empty() {
        let presented = req
            .headers()
            .get("X-Internal-Sync-Key")
            .and_then(|v| v.to_str().ok());
        if presented == Some(sync_key.as_str()) {
            return Ok(next.run(req).await);
        }
    }

    let token = req
        .headers()
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string)
        .ok_or_else(|| AppError::Unauthorized("Admin authentication required".to_string()))?;

    let role = match validate_api_key(&state, &token).await? {
        Some(user) => user.role,
        None => crate::security::jwt::verify_jwt(&token, &state.config.jwt_secret)?
            .role
            .clone()
            .unwrap_or_else(|| "authenticated".to_string()),
    };

    if role != "admin" && role != "super_admin" {
        return Err(AppError::Forbidden("Admin role required".to_string()));
    }

    Ok(next.run(req).await)
}

#[cfg(test)]
mod tests {
    use super::{issued_key_prefix, KEY_PREFIX_LEN};

    /// The column `POST /api/v1/api-keys` fills: first 8 chars of the random part.
    #[test]
    fn prefix_is_the_first_eight_of_the_random_part() {
        let key = format!("is_key_{}", "aB3xY7zQ".to_owned() + &"k".repeat(40));
        assert_eq!(issued_key_prefix(&key), Some("aB3xY7zQ"));
        assert_eq!(issued_key_prefix(&key).unwrap().len(), KEY_PREFIX_LEN);
    }

    #[test]
    fn short_and_foreign_tokens_are_not_keys() {
        assert_eq!(issued_key_prefix("is_key_abc"), None);
        assert_eq!(issued_key_prefix("is_key_"), None);
        assert_eq!(issued_key_prefix(""), None);
        assert_eq!(issued_key_prefix("eyJhbGciOiJIUzI1NiJ9.e30.sig"), None);
        assert_eq!(issued_key_prefix("IS_KEY_abcdefgh"), None);
    }

    /// Regression: `&secret[..8]` panicked here ("byte index 8 is not a char boundary"),
    /// reachable unauthenticated on POST /api/v1/api-keys/verify (live 2026-09-21).
    #[test]
    fn multibyte_token_is_rejected_without_panicking() {
        // 'a' + 4x 'é': byte 8 falls inside a character — exactly the live panic
        assert_eq!(issued_key_prefix("is_key_aéééé"), None);
        // 8 bytes of multi-byte characters: index 8 is the end of the string, a valid
        // boundary, so this is a (never-matching) value rather than a panic
        assert_eq!(issued_key_prefix("is_key_éééé"), Some("éééé"));
        // mixed: 3x 'é' is 6 bytes, so the 8-byte window closes on boundaries
        assert_eq!(issued_key_prefix("is_key_éééab"), Some("éééab"));
    }
}
