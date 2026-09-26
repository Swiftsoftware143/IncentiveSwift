//! Secret Codes Admin Handlers — manage and verify loyalty secret codes.
//! Admins create codes (e.g., "FBGROUP2026") that members enter on the
//! check-in page to earn points. Great for Facebook groups, newsletters, social posts.

use crate::error::AppError;
use crate::security::auth::AuthenticatedUser;
use crate::state::AppState;
use axum::{
    extract::{Path, State},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

/// Secret code record from DB
#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct SecretCode {
    pub id: Uuid,
    pub program_id: Uuid,
    pub code: String,
    pub description: Option<String>,
    pub points_reward: i32,
    pub max_uses: i32,
    pub uses_so_far: i32,
    pub starts_at: chrono::DateTime<chrono::Utc>,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    pub is_active: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Input for creating/updating a secret code
#[derive(Deserialize)]
pub struct SecretCodeInput {
    pub program_id: Option<String>,
    pub code: String,
    pub description: Option<String>,
    pub points_reward: Option<i32>,
    pub max_uses: Option<i32>,
    pub expires_at: Option<String>,
    pub is_active: Option<bool>,
}

/// GET /api/v1/loyalty/secret-codes — list all secret codes for the admin's program
pub async fn list_secret_codes(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let codes = sqlx::query_as::<_, SecretCode>(
        "SELECT id, program_id, code, description, points_reward, max_uses, uses_so_far,
                starts_at, expires_at, is_active, created_at
         FROM loyalty_secret_codes
         ORDER BY created_at DESC",
    )
    .fetch_all(&state.db)
    .await?;

    Ok(Json(json!({ "codes": codes })))
}

/// POST /api/v1/loyalty/secret-codes — create a new secret code
pub async fn create_secret_code(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<SecretCodeInput>,
) -> Result<Json<Value>, AppError> {
    let code_upper = body.code.trim().to_uppercase();
    if code_upper.is_empty() {
        return Err(AppError::BadRequest("Code is required".to_string()));
    }
    if code_upper.len() < 3 {
        return Err(AppError::BadRequest(
            "Code must be at least 3 characters".to_string(),
        ));
    }

    // Find the first program (admin-level, so for now use first active program)
    let program_id = if let Some(pid) = &body.program_id {
        Uuid::parse_str(pid).map_err(|_| AppError::BadRequest("Invalid program_id".to_string()))?
    } else {
        // Fall back to the first active program
        let row = sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM loyalty_programs WHERE is_active = true ORDER BY created_at ASC LIMIT 1"
        )
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("No active loyalty program found. Create one first.".to_string()))?;
        row
    };

    // Check for duplicate code in this program
    let existing: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM loyalty_secret_codes WHERE program_id = $1 AND UPPER(code) = $2",
    )
    .bind(program_id)
    .bind(&code_upper)
    .fetch_optional(&state.db)
    .await?;

    if existing.is_some() {
        return Err(AppError::BadRequest(
            "A code with this name already exists in this program".to_string(),
        ));
    }

    // Parse expires_at if provided
    let expires_at = match &body.expires_at {
        Some(dt) if !dt.is_empty() => {
            let parsed = chrono::DateTime::parse_from_rfc3339(dt).map_err(|_| {
                AppError::BadRequest(
                    "Invalid expires_at format. Use ISO 8601 (e.g., 2026-12-31T23:59:59Z)"
                        .to_string(),
                )
            })?;
            Some(parsed.with_timezone(&chrono::Utc))
        }
        _ => None,
    };

    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO loyalty_secret_codes (id, program_id, code, description, points_reward, max_uses, expires_at, is_active)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)"
    )
    .bind(id)
    .bind(program_id)
    .bind(&code_upper)
    .bind(&body.description)
    .bind(body.points_reward.unwrap_or(25))
    .bind(body.max_uses.unwrap_or(0))
    .bind(expires_at)
    .bind(body.is_active.unwrap_or(true))
    .execute(&state.db)
    .await?;

    Ok(Json(json!({
        "status": "created",
        "id": id,
        "code": code_upper
    })))
}

/// DELETE /api/v1/loyalty/secret-codes/:id — delete a secret code
pub async fn delete_secret_code(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, AppError> {
    let result = sqlx::query("DELETE FROM loyalty_secret_codes WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await?;

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound("Secret code not found".to_string()));
    }

    Ok(Json(json!({ "status": "deleted" })))
}

/// POST /api/v1/loyalty/secret-codes/:id/toggle — toggle active/inactive
pub async fn toggle_secret_code(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, AppError> {
    let row = sqlx::query_scalar::<_, bool>(
        "UPDATE loyalty_secret_codes SET is_active = NOT is_active WHERE id = $1 RETURNING is_active"
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Secret code not found".to_string()))?;

    Ok(Json(json!({ "status": "toggled", "is_active": row })))
}

/// POST /api/v1/loyalty/secret-code/verify — verify a secret code and award points to a member
/// This is called from the check-in page when someone enters a secret code.
/// Body: { program_slug: string, contact: ContactBody, secret_code: string }
pub async fn verify_secret_code(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, AppError> {
    let program_slug = body
        .get("program_slug")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AppError::BadRequest("program_slug is required".to_string()))?;

    let secret_code = body
        .get("secret_code")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AppError::BadRequest("secret_code is required".to_string()))?;

    let code_upper = secret_code.trim().to_uppercase();
    if code_upper.is_empty() {
        return Err(AppError::BadRequest(
            "Secret code cannot be empty".to_string(),
        ));
    }

    // Get program by slug (resolves campaign → program).
    //
    // `program_slug` is a *campaign* slug, and `db::loyalty::get_program`
    // resolves `WHERE id = $1` — a PROGRAM id — so handing it `campaign.id`
    // could never resolve and answered 404 to every caller. `campaigns`
    // carries the canonical forward link (`loyalty_program_id`) and
    // `loyalty_programs.campaign_id` is the reverse pointer `create_program`
    // can also set; the public check-in route accepts either
    // (handlers/loyalty.rs:51-54), so this does too.
    let campaign = crate::db::campaigns::get_campaign_by_slug(&state.db, program_slug).await?;
    let program = match campaign.loyalty_program_id {
        Some(program_id) => crate::db::loyalty::get_program(&state.db, &program_id).await?,
        None => crate::db::loyalty::get_program_by_campaign(&state.db, &campaign.id).await?,
    };

    // Look up the secret code in the loyalty_secret_codes table
    let code_record = sqlx::query_as::<_, SecretCode>(
        "SELECT id, program_id, code, description, points_reward, max_uses, uses_so_far,
                starts_at, expires_at, is_active, created_at
         FROM loyalty_secret_codes
         WHERE program_id = $1 AND UPPER(code) = $2 AND is_active = true
         LIMIT 1",
    )
    .bind(program.id)
    .bind(&code_upper)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::BadRequest("Invalid or expired secret code".to_string()))?;

    // Check if code has expired
    if let Some(expires) = code_record.expires_at {
        if chrono::Utc::now() > expires {
            return Err(AppError::BadRequest("This code has expired".to_string()));
        }
    }

    // Check if code hasn't started yet
    if chrono::Utc::now() < code_record.starts_at {
        return Err(AppError::BadRequest(
            "This code is not active yet".to_string(),
        ));
    }

    // Check max uses
    if code_record.max_uses > 0 && code_record.uses_so_far >= code_record.max_uses {
        return Err(AppError::BadRequest(
            "This code has reached its maximum uses".to_string(),
        ));
    }

    // Upsert contact
    let contact_data = body
        .get("contact")
        .ok_or_else(|| AppError::BadRequest("contact is required".to_string()))?;
    let contact_input = crate::db::contacts::ContactInput {
        first_name: contact_data
            .get("first_name")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        last_name: contact_data
            .get("last_name")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        email: contact_data
            .get("email")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        phone: contact_data
            .get("phone")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        website: contact_data
            .get("website")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        business_name: contact_data
            .get("business_name")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
    };
    let contact_id = crate::db::contacts::upsert_contact(&state.db, &contact_input).await?;

    // Find or create loyalty member
    let member_id =
        crate::db::loyalty::find_or_create_member(&state.db, &program.id, &contact_id).await?;

    // Check if this member already redeemed this code. The UNIQUE
    // (code_id, member_id) constraint below is the real gate — this SELECT is
    // only the friendly path, so a concurrent double-submit cannot slip past.
    let already_redeemed: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM loyalty_secret_code_redemptions WHERE code_id = $1 AND member_id = $2",
    )
    .bind(code_record.id)
    .bind(member_id)
    .fetch_optional(&state.db)
    .await?;

    if already_redeemed.is_some() {
        return Err(AppError::BadRequest(
            "You have already redeemed this code".to_string(),
        ));
    }

    // Award the code's points.
    //
    // What a redemption MEANS: the code's own `points_reward` credited to the
    // member and recorded in the ledgers every other non-check-in award path in
    // this app writes — `loyalty_online_actions` (handlers/loyalty.rs
    // daily_visit/social_share, auth_handler.rs referral_signup) plus the
    // member-facing `loyalty_activity` audit row. The `loyalty_online_actions`
    // row is not optional: it is one of the three earn ledgers
    // `POST /admin/treasury/expire-points` recomputes a balance from, and that
    // sweep SETs `loyalty_members.points_balance` to the recomputed sum, so an
    // award written to the balance alone is DELETED by the next sweep
    // (point_expiry_handler.rs:27-41).
    //
    // The two inserts this replaces were placeholders that could never succeed,
    // so they are deleted rather than fixtured:
    //   * `db::loyalty::record_checkin` bound `Uuid::new_v4()` as
    //     `loyalty_checkins.entry_id`, whose FK is -> entries(id): 23503 for
    //     every caller. A secret code is not a check-in either — a
    //     loyalty_checkins row with entry_id NULL would eat the member's daily
    //     check-in quota and is REFUSED by loyalty_checkins_daily_cap_guard
    //     once they sit at max_checkins_per_day.
    //   * `db::loyalty::create_reward` passed the MEMBER id as `tier_id`, whose
    //     FK is -> loyalty_reward_tiers(id): 23503 for every caller. A code
    //     crosses no reward tier, so there is no honest tier_id to write, and
    //     no other non-check-in award path grants tiers.
    // The award and the redemption claim are one transaction: a crash mid-flow
    // can neither double-award nor burn the code.
    let points = code_record.points_reward;
    let mut tx = state.db.begin().await?;

    let new_balance: i32 = sqlx::query_scalar(
        "UPDATE loyalty_members
            SET points_balance = COALESCE(points_balance, 0) + $1,
                lifetime_points = COALESCE(lifetime_points, 0) + $1
          WHERE id = $2
          RETURNING COALESCE(points_balance, 0) AS points_balance",
    )
    .bind(points)
    .bind(member_id)
    .fetch_one(&mut *tx)
    .await?;

    let metadata = json!({
        "code": code_record.code,
        "code_id": code_record.id,
        "points_reward": points,
    });
    sqlx::query(
        r#"INSERT INTO loyalty_online_actions (member_id, action_type, points_earned, metadata)
           VALUES ($1, 'secret_code', $2, $3::jsonb)"#,
    )
    .bind(member_id)
    .bind(points as i64)
    .bind(metadata.to_string())
    .execute(&mut *tx)
    .await?;

    // Claim the redemption: the unique constraint, not the SELECT above, is the
    // idempotency gate, so two simultaneous submissions of one code by one
    // member cannot both award.
    match sqlx::query(
        "INSERT INTO loyalty_secret_code_redemptions (code_id, member_id) VALUES ($1, $2)",
    )
    .bind(code_record.id)
    .bind(member_id)
    .execute(&mut *tx)
    .await
    {
        Ok(_) => {}
        Err(sqlx::Error::Database(ref d))
            if d.constraint() == Some("loyalty_secret_code_redemptions_code_id_member_id_key") =>
        {
            return Err(AppError::BadRequest(
                "You have already redeemed this code".to_string(),
            ));
        }
        Err(e) => return Err(e.into()),
    }

    // Increment uses counter
    sqlx::query("UPDATE loyalty_secret_codes SET uses_so_far = uses_so_far + 1 WHERE id = $1")
        .bind(code_record.id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;

    // Member-facing audit trail, written next to the award like every other
    // award path does (mechanics/loyalty_checkin.rs::record_activity).
    let _ = crate::mechanics::loyalty_checkin::record_activity(
        &state,
        &member_id.to_string(),
        "secret_code",
        &format!("secret_code {} = {} points", code_record.code, points),
        points,
    )
    .await;

    Ok(Json(json!({
        "status": "ok",
        "points_earned": points,
        "total_points": new_balance,
        "message": "Secret code redeemed! +".to_string() + &points.to_string() + " points",
        "code": code_record.code
    })))
}
