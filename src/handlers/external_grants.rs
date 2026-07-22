//! External Grant handler — allows authorized external services to grant credits.
//!
//! POST /api/v1/loyalty/external/grant-credits
//! Authenticated via X-API-Key header (system API key stored in provider_keys).

use axum::{
    extract::State,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;
use tracing;

use crate::error::AppError;
use crate::state::AppState;
use crate::handlers::credits_handler::add_credits_internal;

#[derive(Debug, Deserialize)]
pub struct GrantCreditsRequest {
    pub email: String,
    pub amount: i32,
    pub reason: String,
    pub program: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct GrantCreditsResponse {
    pub success: bool,
    pub account_id: Option<String>,
    pub new_balance: Option<i32>,
    pub amount: i32,
    pub message: String,
}

/// POST /api/v1/loyalty/external/grant-credits
///
/// Grants credits to a user identified by email.
/// Authenticated via X-API-Key header.
/// Creates an account if one doesn't exist for that email.
/// Used by MultiDirectory's referral system to award Zaarcash.
pub async fn grant_credits(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(req): Json<GrantCreditsRequest>,
) -> Result<Json<Value>, AppError> {
    // 1. Validate API key
    let api_key = headers
        .get("X-API-Key")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| AppError::Unauthorized("Missing X-API-Key header".to_string()))?;

    validate_system_api_key(&state, api_key).await?;

    // 2. Validate input
    if req.email.trim().is_empty() {
        return Err(AppError::BadRequest("email is required".to_string()));
    }
    if req.amount <= 0 {
        return Err(AppError::BadRequest("amount must be positive".to_string()));
    }
    if req.reason.trim().is_empty() {
        return Err(AppError::BadRequest("reason is required".to_string()));
    }

    // 3. Find or create account by email
    let account_id = find_or_create_account(&state.db, &req.email).await?;

    // 4. Grant credits
    let credit_description = format!("{} — {}: {} credits", req.reason, req.program.as_deref().unwrap_or("zaarhub"), req.amount);
    let new_balance = add_credits_internal(
        &state.db,
        account_id,
        req.amount,
        "referral_reward",
        Some("referral"),
        None,
        &Some(credit_description),
    ).await.map_err(|e| AppError::Internal(e))?;

    tracing::info!(
        "Granted {} credits to account {} (email: {}) for reason: {}",
        req.amount, account_id, req.email, req.reason
    );

    Ok(Json(json!({
        "success": true,
        "account_id": account_id.to_string(),
        "new_balance": new_balance,
        "amount": req.amount,
        "message": format!("{} credits granted successfully", req.amount),
    })))
}

/// Find an account by email, or create a minimal one if it doesn't exist.
/// Returns the account UUID.
async fn find_or_create_account(
    db: &sqlx::PgPool,
    email: &str,
) -> Result<Uuid, AppError> {
    // First try to find existing account
    let existing = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM accounts WHERE email = $1"
    )
    .bind(email)
    .fetch_optional(db)
    .await?;

    if let Some(id) = existing {
        return Ok(id);
    }

    // Create a minimal account (no tenant — just for credit holding)
    let new_id = Uuid::new_v4();
    let now = chrono::Utc::now();

    sqlx::query(
        r#"INSERT INTO accounts (id, email, name, role, credits_balance, credits_lifetime_used, created_at, updated_at)
           VALUES ($1, $2, $3, 'authenticated', 0, 0, $4, $5)
           ON CONFLICT (email) DO UPDATE SET updated_at = NOW()
           RETURNING id"#
    )
    .bind(new_id)
    .bind(email)
    .bind(format!("Referral User: {}", email))
    .bind(now)
    .bind(now)
    .execute(db)
    .await?;

    Ok(new_id)
}

/// GET /api/v1/loyalty/external/program/{id} — get program info for external systems
///
/// Returns program details including currency_name, currency_icon, currency_color.
pub async fn get_external_program(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
) -> Result<Json<Value>, AppError> {
    let api_key = headers
        .get("X-API-Key")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| AppError::Unauthorized("Missing X-API-Key header".to_string()))?;

    validate_system_api_key(&state, api_key).await?;

    let program_id = Uuid::parse_str(&id)
        .map_err(|_| AppError::BadRequest("Invalid program ID".to_string()))?;

    let program = sqlx::query_as::<_, crate::db::loyalty::LoyaltyProgram>(
        r#"SELECT id, campaign_id, name, recognition_method,
                  points_per_checkin, max_checkins_per_day,
                  point_decay_days, is_active, created_at,
                  tiers_enabled, milestones_enabled, streak_enabled,
                  streak_bonus, streak_days, referral_bonus, birthday_bonus,
                  points_expire_days, social_share_points, points_per_visit,
                  currency_name, currency_icon, currency_color
           FROM loyalty_programs WHERE id = $1"#,
    )
    .bind(program_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Loyalty program not found".to_string()))?;

    Ok(Json(json!({
        "success": true,
        "program": {
            "id": program.id,
            "name": program.name,
            "is_active": program.is_active,
            "currency_name": program.currency_name,
            "currency_icon": program.currency_icon,
            "currency_color": program.currency_color,
            "points_per_checkin": program.points_per_checkin,
            "max_checkins_per_day": program.max_checkins_per_day,
            "tiers_enabled": program.tiers_enabled,
            "milestones_enabled": program.milestones_enabled,
            "streak_enabled": program.streak_enabled,
        }
    })))
}

/// Validate a system API key against provider_keys table.
async fn validate_system_api_key(
    state: &AppState,
    key: &str,
) -> Result<(), AppError> {
    let valid = sqlx::query_scalar::<_, i64>(
        r#"SELECT COUNT(*) FROM provider_keys
           WHERE provider = 'system_api_key'
             AND api_key = $1
             AND is_active = true
             AND (scope = 'internal' OR scope = 'external')"#
    )
    .bind(key)
    .fetch_one(&state.db)
    .await
    .map_err(|_| AppError::Internal("DB error validating API key".to_string()))?;

    if valid == 0 {
        return Err(AppError::Unauthorized("Invalid or inactive API key".to_string()));
    }

    Ok(())
}
