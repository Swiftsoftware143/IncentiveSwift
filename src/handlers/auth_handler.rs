//! Auth handlers — login, me, change password, forgot/reset password.

use crate::error::AppError;
use crate::state::AppState;
use crate::security::auth::AuthenticatedUser;
use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use axum::{
    extract::State,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

/// Login request body.
#[derive(Deserialize)]
pub struct LoginInput {
    pub email: String,
    pub password: String,
}

/// Password change request body.
#[derive(Deserialize)]
pub struct ChangePasswordInput {
    pub current_password: String,
    pub new_password: String,
}

/// Forgot password request body.
#[derive(Deserialize)]
pub struct ForgotPasswordInput {
    pub email: String,
}

/// Reset password request body.
#[derive(Deserialize)]
pub struct ResetPasswordInput {
    pub token: String,
    pub new_password: String,
}

/// Create a signed JWT token for an authenticated user.
fn create_jwt(account_id: &str, email: &str, role: &str, secret: &str) -> Result<String, AppError> {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    use base64::Engine;

    type HmacSha256 = Hmac<Sha256>;

    let header = json!({
        "alg": "HS256",
        "typ": "JWT",
    });

    let now = chrono::Utc::now().timestamp();
    let payload = json!({
        "sub": account_id,
        "email": email,
        "role": role,
        "iat": now,
        "exp": now + 86400, // 24 hours
    });

    let header_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_string(&header).unwrap().as_bytes());
    let payload_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_string(&payload).unwrap().as_bytes());

    let message = format!("{}.{}", header_b64, payload_b64);

    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .map_err(|_| AppError::Internal("Failed to create HMAC".to_string()))?;
    mac.update(message.as_bytes());
    let sig = mac.finalize().into_bytes();

    let sig_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sig);

    Ok(format!("{}.{}", message, sig_b64))
}

/// Verify a password against a stored hash (argon2 or bcrypt).
fn verify_password(password: &str, hash: &str) -> bool {
    // Try argon2 first
    if let Ok(parsed) = PasswordHash::new(hash) {
        if Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok() {
            return true;
        }
    }
    // Fallback: try bcrypt for backward compatibility
    bcrypt::verify(password, hash).unwrap_or(false)
}

/// Hash a password with argon2.
fn hash_password(password: &str) -> Result<String, AppError> {
    let salt = SaltString::generate(&mut OsRng);
    let argon2 = Argon2::default();
    argon2
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| AppError::Internal(format!("Failed to hash password: {}", e)))
}

/// POST /api/v1/auth/login
pub async fn login(
    State(state): State<AppState>,
    Json(body): Json<LoginInput>,
) -> Result<Json<Value>, AppError> {
    // Look up account by email
    let row = sqlx::query(
        r#"SELECT id, email, password_hash, role
           FROM accounts WHERE email = $1"#
    )
    .bind(&body.email)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::Unauthorized("Invalid email or password".to_string()))?;

    let account_id: Uuid = row.get("id");
    let email: String = row.get("email");
    let role: String = row.get("role");
    let password_hash: Option<String> = row.get("password_hash");

    // Verify password
    match password_hash {
        Some(ref hash) => {
            if !verify_password(&body.password, hash) {
                return Err(AppError::Unauthorized("Invalid email or password".to_string()));
            }
        }
        None => {
            return Err(AppError::Unauthorized("Invalid email or password".to_string()));
        }
    }

    // Generate JWT
    let token = create_jwt(
        &account_id.to_string(),
        &email,
        &role,
        &state.config.jwt_secret,
    )?;

    Ok(Json(json!({
        "token": token,
        "user": {
            "id": account_id,
            "email": email,
            "role": role,
        }
    })))
}

/// GET /api/v1/auth/me
pub async fn me(
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    Ok(Json(json!({
        "user": {
            "id": user.account_id,
            "email": user.email,
            "role": user.role,
        }
    })))
}

/// PUT /api/v1/auth/password
pub async fn change_password(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<ChangePasswordInput>,
) -> Result<Json<Value>, AppError> {
    // Verify current password
    let row = sqlx::query(
        r#"SELECT password_hash FROM accounts WHERE id = $1"#
    )
    .bind(&user.account_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Account not found".to_string()))?;

    let password_hash: Option<String> = row.get("password_hash");

    match password_hash {
        Some(ref hash) => {
            if !verify_password(&body.current_password, hash) {
                return Err(AppError::Forbidden("Current password is incorrect".to_string()));
            }
        }
        None => {
            return Err(AppError::Forbidden("No password set for this account".to_string()));
        }
    }

    let new_hash = hash_password(&body.new_password)?;

    sqlx::query("UPDATE accounts SET password_hash = $1 WHERE id = $2")
        .bind(&new_hash)
        .bind(&user.account_id)
        .execute(&state.db)
        .await?;

    Ok(Json(json!({ "status": "password_updated" })))
}

/// POST /api/v1/auth/forgot-password
pub async fn forgot_password(
    State(state): State<AppState>,
    Json(body): Json<ForgotPasswordInput>,
) -> Result<Json<Value>, AppError> {
    // Check if account exists
    let account_id: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM accounts WHERE email = $1"
    )
    .bind(&body.email)
    .fetch_optional(&state.db)
    .await?
    .flatten();

    // Always return success to prevent email enumeration
    if account_id.is_none() {
        return Ok(Json(json!({
            "status": "ok",
            "message": "If the email exists, a reset link has been sent",
        })));
    }

    let account_id = account_id.unwrap();
    let token = Uuid::new_v4().to_string();
    let expires_at = chrono::Utc::now() + chrono::Duration::hours(1);

    sqlx::query(
        r#"INSERT INTO password_resets (account_id, token, expires_at)
           VALUES ($1, $2, $3)"#
    )
    .bind(account_id)
    .bind(&token)
    .bind(expires_at)
    .execute(&state.db)
    .await?;

    // Try to send email, but log the token to server logs as fallback
    tracing::info!(
        "Password reset token for {}: {} (expires at {})",
        body.email, token, expires_at
    );

    // Attempt to send the email via configured provider
    let email_sent = crate::email::send_reset_email(&body.email, &token).await;
    if let Err(e) = email_sent {
        tracing::warn!("Failed to send password reset email: {}", e);
    }

    Ok(Json(json!({
        "status": "ok",
        "message": "If the email exists, a reset link has been sent",
        // Include token in response for development convenience
        "reset_token": token,
    })))
}

/// POST /api/v1/auth/reset-password
pub async fn reset_password(
    State(state): State<AppState>,
    Json(body): Json<ResetPasswordInput>,
) -> Result<Json<Value>, AppError> {
    // Look up the reset token
    let row = sqlx::query(
        r#"SELECT account_id, expires_at, used
           FROM password_resets WHERE token = $1"#
    )
    .bind(&body.token)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::BadRequest("Invalid or expired reset token".to_string()))?;

    let account_id: Uuid = row.get("account_id");
    let expires_at: chrono::DateTime<chrono::Utc> = row.get("expires_at");
    let used: bool = row.get("used");

    if used {
        return Err(AppError::BadRequest("Reset token has already been used".to_string()));
    }

    if chrono::Utc::now() > expires_at {
        return Err(AppError::BadRequest("Reset token has expired".to_string()));
    }

    let new_hash = hash_password(&body.new_password)?;

    // Update account password and mark token as used
    sqlx::query("UPDATE accounts SET password_hash = $1 WHERE id = $2")
        .bind(&new_hash)
        .bind(account_id)
        .execute(&state.db)
        .await?;

    sqlx::query("UPDATE password_resets SET used = true WHERE token = $1")
        .bind(&body.token)
        .execute(&state.db)
        .await?;

    Ok(Json(json!({ "status": "password_reset" })))
}
