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

/// Register request body.
#[derive(Deserialize)]
pub struct RegisterInput {
    pub email: String,
    pub password: String,
    pub name: Option<String>,
    pub referral_code: Option<String>,
    pub industry_slug: Option<String>,
}

/// POST /api/v1/auth/register — self-service signup, assigns Free plan.
pub async fn register(
    State(state): State<AppState>,
    Json(body): Json<RegisterInput>,
) -> Result<Json<Value>, AppError> {
    if body.email.is_empty() || body.password.is_empty() {
        return Err(AppError::BadRequest("Email and password are required".to_string()));
    }
    if body.password.len() < 6 {
        return Err(AppError::BadRequest("Password must be at least 6 characters".to_string()));
    }

    // Check if account already exists
    let existing: Option<Uuid> = sqlx::query_scalar("SELECT id FROM accounts WHERE email = $1")
        .bind(&body.email)
        .fetch_optional(&state.db)
        .await?
        .flatten();
    if existing.is_some() {
        return Err(AppError::BadRequest("An account with this email already exists".to_string()));
    }

    // Get the Free plan tier id
    let free_plan_id: Uuid = sqlx::query_scalar(
        "SELECT id FROM plans WHERE slug = 'free' LIMIT 1"
    )
    .fetch_one(&state.db)
    .await
    .map_err(|_| AppError::Internal("Free plan not configured".to_string()))?;

    // Generate account id first (so we can use as tenant_id)
    let account_id = Uuid::new_v4();
    let password_hash = hash_password(&body.password)?;
    let name = body.name.unwrap_or_else(|| {
        body.email.split('@').next().unwrap_or("User").to_string()
    });

    // Generate a unique slug from email
    let slug_base = body.email.split('@').next().unwrap_or("user");
    let slug = format!("{}-{}", slug_base, &account_id.to_string()[..8]);

    // Insert account with tenant_id = self (standalone tenant)
    sqlx::query(
        r#"INSERT INTO accounts (id, name, email, password_hash, role, plan_tier_id, tenant_id, slug)
           VALUES ($1, $2, $3, $4, 'company_admin', $5, $6, $7)"#
    )
    .bind(account_id)
    .bind(&name)
    .bind(&body.email)
    .bind(&password_hash)
    .bind(free_plan_id)
    .bind(account_id) // tenant_id = self
    .bind(&slug)
    .execute(&state.db)
    .await?;

    // Assign industry if provided; fall back to 'general'
    let industry_slug = body.industry_slug.as_deref().unwrap_or("general");
    let industry_id: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM industries WHERE slug = $1 AND is_active = true"
    )
    .bind(industry_slug)
    .fetch_optional(&state.db)
    .await?
    .flatten();

    if let Some(ind_id) = industry_id {
        sqlx::query(
            r#"INSERT INTO account_industries (account_id, industry_id, is_primary)
               VALUES ($1, $2, true)
               ON CONFLICT (account_id, industry_id) DO NOTHING"#
        )
        .bind(account_id)
        .bind(ind_id)
        .execute(&state.db)
        .await?;
    }

    // Generate JWT
    let token = create_jwt(
        &account_id.to_string(),
        &body.email,
        "company_admin",
        &state.config.jwt_secret,
    )?;

    // Award referral bonus if referral_code provided
    if let Some(ref ref_code) = body.referral_code {
        // Look up referrer by their referral code
        let referrer = sqlx::query_as::<_, crate::db::loyalty::LoyaltyMember>(
            r#"SELECT id, program_id, contact_id, points_balance, lifetime_points,
                      member_since, last_checkin_at
               FROM loyalty_members WHERE referral_code = $1"#
        )
        .bind(ref_code)
        .fetch_optional(&state.db)
        .await?;

        if let Some(program_member) = referrer {
            // Get program for referral_bonus amount
            let program = sqlx::query_as::<_, crate::db::loyalty::LoyaltyProgram>(
                r#"SELECT id, campaign_id, name, recognition_method,
                          points_per_checkin, max_checkins_per_day,
                          point_decay_days, is_active, created_at,
                          tiers_enabled, milestones_enabled, streak_enabled,
                          streak_bonus, streak_days, referral_bonus, birthday_bonus,
                          points_expire_days, social_share_points, points_per_visit
                   FROM loyalty_programs WHERE id = $1 AND is_active = true"#
            )
            .bind(&program_member.program_id)
            .fetch_optional(&state.db)
            .await?;

            if let Some(ref prog) = program {
                let bonus = if prog.referral_bonus > 0 { prog.referral_bonus as i64 } else { 50i64 };

                // Award points to referrer
                sqlx::query(
                    r#"UPDATE loyalty_members
                       SET points_balance = points_balance + $1,
                           lifetime_points = lifetime_points + $1
                       WHERE id = $2"#
                )
                .bind(bonus)
                .bind(&program_member.id)
                .execute(&state.db)
                .await?;

                // Record the referral action
                let metadata = serde_json::json!({
                    "type": "signup_referral",
                    "referee_email": body.email,
                    "bonus": bonus
                });
                sqlx::query(
                    r#"INSERT INTO loyalty_online_actions (member_id, action_type, points_earned, metadata)
                       VALUES ($1, 'referral_signup', $2, $3::jsonb)"#
                )
                .bind(&program_member.id)
                .bind(bonus)
                .bind(metadata.to_string())
                .execute(&state.db)
                .await?;

                // Award points to referee (new user) — create a member record for them
                // First find or create a contact for this new user
                let contact_id = sqlx::query_scalar::<_, Uuid>(
                    r#"SELECT id FROM contacts WHERE email = $1 AND tenant_id = $2 LIMIT 1"#
                )
                .bind(&body.email)
                .bind(account_id)
                .fetch_optional(&state.db)
                .await?;

                if let Some(cid) = contact_id {
                    // Check if they already have a membership in this program
                    let existing_member = sqlx::query_scalar::<_, i64>(
                        r#"SELECT COUNT(*) FROM loyalty_members WHERE contact_id = $1 AND program_id = $2"#
                    )
                    .bind(cid)
                    .bind(&program_member.program_id)
                    .fetch_one(&state.db)
                    .await?;

                    if existing_member == 0 {
                        // Create member record for referee with welcome bonus
                        sqlx::query(
                            r#"INSERT INTO loyalty_members (program_id, contact_id, points_balance, lifetime_points, referral_code)
                               VALUES ($1, $2, $3, $3, $4)"#
                        )
                        .bind(&program_member.program_id)
                        .bind(cid)
                        .bind(bonus / 2) // Referee gets half the bonus
                        .bind(&format!("ref-{}", &Uuid::new_v4().to_string()[..8]))
                        .execute(&state.db)
                        .await?;
                    }
                }
            }
        }
    }

    Ok(Json(json!({
        "token": token,
        "user": {
            "id": account_id,
            "email": body.email,
            "name": name,
            "role": "company_admin",
            "plan": "free",
        }
    })))
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
#[derive(Deserialize)]
pub struct UpdateProfileInput {
    pub name: Option<String>,
    pub industry_slug: Option<String>,
}

pub async fn me(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let account_uuid = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account id".to_string()))?;

    let row = sqlx::query(
        r#"SELECT a.name, a.email, a.role, a.plan_tier_id,
                  p.name as plan_name,
                  p.slug as plan_slug,
                  COALESCE(p.features::text, '{}')::jsonb as features,
                  p.max_campaigns,
                  p.max_entries_per_month,
                  p.price_monthly,
                  p.price_annual
           FROM accounts a
           LEFT JOIN plans p ON a.plan_tier_id = p.id
           WHERE a.id = $1"#
    )
    .bind(&account_uuid)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Account not found".to_string()))?;

    let name: Option<String> = row.get("name");
    let email: String = row.get("email");
    let role: String = row.get("role");
    let plan_name: Option<String> = row.get("plan_name");
    let plan_slug: Option<String> = row.get("plan_slug");
    let features: serde_json::Value = row.get("features");
    let plan_tier_id: Option<uuid::Uuid> = row.get("plan_tier_id");
    let max_campaigns: Option<i32> = row.get("max_campaigns");
    let max_entries: Option<i32> = row.get("max_entries_per_month");

    // Fetch account industries
    let industries: Vec<Value> = sqlx::query(
        r#"SELECT i.id, i.name, i.slug, i.icon, ai.is_primary
           FROM account_industries ai
           JOIN industries i ON i.id = ai.industry_id
           WHERE ai.account_id = $1
           ORDER BY ai.is_primary DESC, i.sort_order"#
    )
    .bind(&account_uuid)
    .fetch_all(&state.db)
    .await?
    .iter()
    .map(|r| {
        json!({
            "id": r.get::<Uuid, _>("id"),
            "name": r.get::<String, _>("name"),
            "slug": r.get::<String, _>("slug"),
            "icon": r.get::<Option<String>, _>("icon"),
            "is_primary": r.get::<bool, _>("is_primary"),
        })
    })
    .collect();

    // Industry limit from plan features
    let industry_limit: i32 = features.get("industry_limit").and_then(|v| v.as_i64()).unwrap_or(1) as i32;

    Ok(Json(json!({
        "user": {
            "id": user.account_id,
            "email": email,
            "name": name.unwrap_or_default(),
            "role": role,
            "plan_tier_id": plan_tier_id,
            "plan": {
                "name": plan_name,
                "slug": plan_slug,
                "features": features,
                "max_campaigns": max_campaigns.unwrap_or(0),
                "max_entries_per_month": max_entries.unwrap_or(0),
            },
            "industries": industries,
            "industry_limit": industry_limit,
            "impersonating": user.impersonating
        }
    })))
}

/// PUT /api/v1/auth/profile
pub async fn update_profile(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<UpdateProfileInput>,
) -> Result<Json<Value>, AppError> {
    let account_uuid = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account id".to_string()))?;

    if let Some(ref name) = body.name {
        sqlx::query("UPDATE accounts SET name = $1 WHERE id = $2")
            .bind(name)
            .bind(&account_uuid)
            .execute(&state.db)
            .await?;
    }

    // Industry change: set as primary, swapping out previous primary if at limit
    if let Some(ref industry_slug) = body.industry_slug {
        // Get the industry ID
        let industry_id: Option<Uuid> = sqlx::query_scalar(
            "SELECT id FROM industries WHERE slug = $1 AND is_active = true"
        )
        .bind(industry_slug)
        .fetch_optional(&state.db)
        .await?
        .flatten();

        if let Some(ind_id) = industry_id {
            // Check if already assigned — if so just make it primary
            let already_assigned: bool = sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM account_industries WHERE account_id = $1 AND industry_id = $2"
            )
            .bind(&account_uuid)
            .bind(ind_id)
            .fetch_one(&state.db)
            .await?
            > 0;

            if !already_assigned {
                // Check plan limit
                let plan_features: serde_json::Value = sqlx::query_scalar(
                    r#"SELECT COALESCE(p.features::text, '{}')::jsonb
                       FROM accounts a
                       LEFT JOIN plans p ON a.plan_tier_id = p.id
                       WHERE a.id = $1"#
                )
                .bind(&account_uuid)
                .fetch_optional(&state.db)
                .await?
                .unwrap_or(serde_json::json!({}));

                let industry_limit: i64 = plan_features.get("industry_limit")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(1);

                let current_count: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM account_industries WHERE account_id = $1"
                )
                .bind(&account_uuid)
                .fetch_one(&state.db)
                .await?
                .unwrap_or(0);

                if current_count >= industry_limit {
                    return Err(AppError::BadRequest(format!(
                        "Plan limit reached: maximum {} industries. Upgrade your plan to add more.",
                        industry_limit
                    )));
                }

                sqlx::query(
                    r#"INSERT INTO account_industries (account_id, industry_id, is_primary)
                       VALUES ($1, $2, true)
                       ON CONFLICT (account_id, industry_id) DO UPDATE SET is_primary = true"#
                )
                .bind(&account_uuid)
                .bind(ind_id)
                .execute(&state.db)
                .await?;
            } else {
                // Already assigned — just set as primary
                sqlx::query(
                    "UPDATE account_industries SET is_primary = true WHERE account_id = $1 AND industry_id = $2"
                )
                .bind(&account_uuid)
                .bind(ind_id)
                .execute(&state.db)
                .await?;
            }

            // Unset other primaries
            sqlx::query(
                r#"UPDATE account_industries SET is_primary = false
                   WHERE account_id = $1 AND industry_id != $2 AND is_primary = true"#
            )
            .bind(&account_uuid)
            .bind(ind_id)
            .execute(&state.db)
            .await?;
        }
    }

    // Return full profile (same shape as /me for consistency)
    let row = sqlx::query(
        r#"SELECT a.name, a.email, a.role, a.plan_tier_id,
                  p.name as plan_name,
                  p.slug as plan_slug,
                  COALESCE(p.features::text, '{}')::jsonb as features,
                  p.max_campaigns,
                  p.max_entries_per_month,
                  p.price_monthly,
                  p.price_annual
           FROM accounts a
           LEFT JOIN plans p ON a.plan_tier_id = p.id
           WHERE a.id = $1"#
    )
    .bind(&account_uuid)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Account not found".to_string()))?;

    let name: Option<String> = row.get("name");
    let email: String = row.get("email");
    let role: String = row.get("role");
    let features: serde_json::Value = row.get("features");

    let industries: Vec<Value> = sqlx::query(
        r#"SELECT i.id, i.name, i.slug, i.icon, ai.is_primary
           FROM account_industries ai
           JOIN industries i ON i.id = ai.industry_id
           WHERE ai.account_id = $1
           ORDER BY ai.is_primary DESC, i.sort_order"#
    )
    .bind(&account_uuid)
    .fetch_all(&state.db)
    .await?
    .iter()
    .map(|r| {
        json!({
            "id": r.get::<Uuid, _>("id"),
            "name": r.get::<String, _>("name"),
            "slug": r.get::<String, _>("slug"),
            "icon": r.get::<Option<String>, _>("icon"),
            "is_primary": r.get::<bool, _>("is_primary"),
        })
    })
    .collect();

    let industry_limit: i32 = features.get("industry_limit").and_then(|v| v.as_i64()).unwrap_or(1) as i32;

    let plan_name: Option<String> = row.get("plan_name");
    let plan_slug: Option<String> = row.get("plan_slug");
    let plan_tier_id: Option<uuid::Uuid> = row.get("plan_tier_id");
    let max_campaigns: Option<i32> = row.get("max_campaigns");
    let max_entries: Option<i32> = row.get("max_entries_per_month");

    Ok(Json(json!({
        "user": {
            "id": user.account_id,
            "email": email,
            "name": name.unwrap_or_default(),
            "role": role,
            "plan_tier_id": plan_tier_id,
            "plan": {
                "name": plan_name,
                "slug": plan_slug,
                "features": features,
                "max_campaigns": max_campaigns.unwrap_or(0),
                "max_entries_per_month": max_entries.unwrap_or(0),
            },
            "industries": industries,
            "industry_limit": industry_limit,
        }
    })))
}

/// PUT /api/v1/auth/password
pub async fn change_password(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<ChangePasswordInput>,
) -> Result<Json<Value>, AppError> {
    let account_uuid = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account id".to_string()))?;

    // Verify current password
    let row = sqlx::query(
        r#"SELECT password_hash FROM accounts WHERE id = $1"#
    )
    .bind(&account_uuid)
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
        .bind(&account_uuid)
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
