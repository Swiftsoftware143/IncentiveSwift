//! External integration handlers — allows authorized external services to:
//!   - Register loyalty members (MultiDirectory signups)
//!   - Grant credits (MultiDirectory referrals)
//!   - Query program info
//! Authenticated via X-API-Key header (system API key stored in provider_keys).

use axum::{extract::State, Json};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing;
use uuid::Uuid;

use crate::error::AppError;
use crate::handlers::credits_handler::add_credits_internal;
use crate::state::AppState;

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

    // 2. Validate input — the address (normalised below) is the login identity this call may
    // MINT an account for, so it is refused here when it is not an address (kanban t_d7ef4a88).
    let email = crate::security::email_addr::normalize(&req.email).map_err(AppError::BadRequest)?;
    if req.amount <= 0 {
        return Err(AppError::BadRequest("amount must be positive".to_string()));
    }
    if req.reason.trim().is_empty() {
        return Err(AppError::BadRequest("reason is required".to_string()));
    }

    // 3. Find or create account by email
    let account_id = find_or_create_account(&state.db, &email).await?;

    // 4. Grant credits
    let credit_description = format!(
        "{} — {}: {} credits",
        req.reason,
        req.program.as_deref().unwrap_or("default"),
        req.amount
    );
    let new_balance = add_credits_internal(
        &state.db,
        account_id,
        req.amount,
        "referral_reward",
        Some("referral"),
        None,
        &Some(credit_description),
    )
    .await
    .map_err(AppError::Internal)?;

    tracing::info!(
        "Granted {} credits to account {} (email: {}) for reason: {}",
        req.amount,
        account_id,
        email,
        req.reason
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
async fn find_or_create_account(db: &sqlx::PgPool, email: &str) -> Result<Uuid, AppError> {
    // ── Address boundary (kanban t_d7ef4a88) ─────────────────────────────────────
    // `accounts.email` is a login identity and the only address credentials mail can reach. This
    // function MINTS an account when the address is unknown and it had NO guard at all, so any
    // string from the referral/credit callers became a real account. Normalise BEFORE the first
    // SELECT and use the normalised value for the lookup and the INSERT.
    let email = crate::security::email_addr::normalize(email).map_err(AppError::BadRequest)?;

    // First try to find existing account
    let existing = sqlx::query_scalar::<_, Uuid>("SELECT id FROM accounts WHERE lower(email) = $1")
        .bind(&email)
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
    .bind(&email)
    .bind(format!("Referral User: {}", email))
    .bind(now)
    .bind(now)
    .execute(db)
    .await?;

    Ok(new_id)
}

/// GET /api/v1/loyalty/external/program/{id} — get program info for external systems
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

    let program_id =
        Uuid::parse_str(&id).map_err(|_| AppError::BadRequest("Invalid program ID".to_string()))?;

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
async fn validate_system_api_key(state: &AppState, key: &str) -> Result<(), AppError> {
    // The stored key is CIPHERTEXT at rest, so the comparison happens in Rust against the
    // DECRYPTED value — a SQL `api_key = $1` can never match once the column is encrypted.
    let candidates = sqlx::query_scalar::<_, String>(
        r#"SELECT api_key FROM provider_keys
           WHERE provider = 'system_api_key'
             AND is_active = true
             AND (scope = 'internal' OR scope = 'external')"#,
    )
    .fetch_all(&state.db)
    .await
    .map_err(|_| AppError::Internal("DB error validating API key".to_string()))?;

    let mut valid = 0i64;
    for stored in candidates {
        // A row that cannot be decrypted is skipped, never treated as a match.
        if let Ok(plain) =
            crate::security::provider_key_crypto::decrypt_from_storage(&state.db, stored.trim())
                .await
        {
            if plain == key {
                valid = 1;
                break;
            }
        }
    }

    if valid == 0 {
        return Err(AppError::Unauthorized(
            "Invalid or inactive API key".to_string(),
        ));
    }

    Ok(())
}

// ── Register Member ──────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct RegisterMemberRequest {
    pub email: String,
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    pub phone: Option<String>,
    pub member_type: String, // "visitor", "supplier", "business_owner"
    pub business_type: Option<String>, // supplier subtype: farm, wholesaler, etc.
    pub directory_slug: Option<String>, // which city directory (optional for network-wide)
    /// WHICH loyalty programme to enrol in, named by the CALLER.
    ///
    /// This replaces a hardcoded default that resolved to ONE particular directory's programmes.
    /// IncentiveSwift is the engine every app and every business runs on; it must not contain the name
    /// business runs on; it must not contain the name of a single one of them. The callers are the ones
    /// who know whose points these are — the businesses dictate what loyalty goes out.
    pub program_slug: Option<String>,
    pub tags: Option<Vec<String>>,
}

#[derive(Debug, Serialize)]
pub struct RegisterMemberResponse {
    pub success: bool,
    pub contact_id: Option<String>,
    pub member_id: Option<String>,
    pub loyalty_program_id: Option<String>,
    pub loyalty_program_name: Option<String>,
    pub already_existed: bool,
    pub message: String,
}

/// POST /api/v1/loyalty/external/register-member
/// Called by MultiDirectory on every member signup (visitor, supplier, business owner).
/// Creates/finds the IS contact and enrols it in the programme the CALLER names.
/// This is the single entry point for every directory's members into the loyalty system.
pub async fn register_member(
    State(state): State<AppState>,
    Json(req): Json<RegisterMemberRequest>,
) -> Result<Json<Value>, AppError> {
    // NOTE: This endpoint is internal-only (called by MultiDirectory on localhost).
    // No API key required — same pattern as survey-response endpoint.

    // 1. Validate input — the address is checked-and-normalised BEFORE the first SELECT/INSERT,
    // because this endpoint can MINT an accounts row for it (kanban t_d7ef4a88).
    let email = crate::security::email_addr::normalize(&req.email).map_err(AppError::BadRequest)?;

    let valid_types = ["visitor", "supplier", "business_owner"];
    if !valid_types.contains(&req.member_type.as_str()) {
        return Err(AppError::BadRequest(format!(
            "Invalid member_type '{}'. Must be one of: visitor, supplier, business_owner",
            req.member_type
        )));
    }

    // 3. WHICH programme? The caller names it. Nothing here may assume a particular directory.
    //    Resolution order:
    //      (a) `program_slug` — the caller says exactly whose points these are. This is the contract.
    //      (b) `directory-<directory_slug>` — the per-directory convention already used by the
    //          directory visitor path, for callers that think in directories rather than programmes.
    //      (c) nothing: refuse, and NAME what is missing. There is deliberately NO built-in default,
    //          because a silent default is how one tenant's name ends up inside the engine.
    //    A refusal here is visible to the caller at signup; a silent fallback would not be.
    let resolved_slug: Option<String> =
        match (req.program_slug.as_deref(), req.directory_slug.as_deref()) {
            (Some(slug), _) if !slug.trim().is_empty() => Some(slug.trim().to_string()),
            (_, Some(dir)) if !dir.trim().is_empty() => Some(format!("directory-{}", dir.trim())),
            _ => None,
        };

    let lookup_slug = resolved_slug.ok_or_else(|| {
        AppError::BadRequest(
            "program_slug is required: name the loyalty programme this member belongs to              (or send directory_slug and the `directory-<slug>` programme is used).              IncentiveSwift does not pick a programme for you."
                .to_string(),
        )
    })?;

    // 4. Look up loyalty program by slug
    let program = sqlx::query_as::<_, (Uuid, String, bool)>(
        "SELECT id, name, is_active FROM loyalty_programs WHERE slug = $1 AND is_active = true LIMIT 1"
    )
    .bind(&lookup_slug)
    .fetch_optional(&state.db)
    .await?;

    let (program_id, program_name, _active) = match program {
        Some(p) => p,
        None => {
            return Err(AppError::NotFound(format!(
                "Loyalty programme '{}' not found on this account. Create it first.",
                lookup_slug
            )));
        }
    };

    // 5. Upsert contact (dedup by email)
    let contact_id = crate::db::contacts::upsert_contact(
        &state.db,
        &crate::db::contacts::ContactInput {
            first_name: req.first_name.clone(),
            last_name: req.last_name.clone(),
            email: Some(email.clone()),
            phone: req.phone.clone(),
            business_name: None,
            website: None,
        },
    )
    .await?;

    // 6. Check if already a loyalty member in this program
    let existing_member: Option<(Uuid, bool)> = sqlx::query_as(
        "SELECT id, true FROM loyalty_members WHERE program_id = $1 AND contact_id = $2 LIMIT 1",
    )
    .bind(program_id)
    .bind(contact_id)
    .fetch_optional(&state.db)
    .await?;

    let (member_id, already_existed) = if let Some((mid, _)) = existing_member {
        (mid, true)
    } else {
        // Enroll as loyalty member
        let mid =
            crate::db::loyalty::find_or_create_member(&state.db, &program_id, &contact_id).await?;
        (mid, false)
    };

    // 7. If directory_slug provided, also enroll in the city directory campaign
    //    The city campaigns use campaign_points_balance for ZaarCash points
    if let Some(ref dir_slug) = req.directory_slug {
        let campaign_slug = format!("directory-{}", dir_slug);
        let campaign_info: Option<(Uuid, Option<Uuid>)> = sqlx::query_as(
            "SELECT id, loyalty_program_id FROM campaigns WHERE slug = $1 AND status = 'active' LIMIT 1"
        )
        .bind(&campaign_slug)
        .fetch_optional(&state.db)
        .await?;

        if let Some((campaign_id, campaign_program_id)) = campaign_info {
            // Also enroll in the campaign's loyalty program if different from the main one
            if let Some(cp_id) = campaign_program_id {
                if cp_id != program_id {
                    let _ =
                        crate::db::loyalty::find_or_create_member(&state.db, &cp_id, &contact_id)
                            .await;
                }
            }
        }
    }

    // 8. Apply tags if provided (store in contact notes2 for segmentation)
    if let Some(ref tags) = req.tags {
        if !tags.is_empty() {
            let tag_string = tags.join(", ");
            sqlx::query("UPDATE contacts SET notes2 = $1 WHERE id = $2")
                .bind(&tag_string)
                .bind(contact_id)
                .execute(&state.db)
                .await?;
        }
    }

    // 9. If business_owner, ensure an IS accounts entry exists for credit tracking
    let _account_id: Option<Uuid> = if req.member_type == "business_owner" {
        let existing_acct: Option<Uuid> =
            sqlx::query_scalar("SELECT id FROM accounts WHERE lower(email) = $1 LIMIT 1")
                .bind(&email)
                .fetch_optional(&state.db)
                .await?;

        match existing_acct {
            Some(id) => Some(id),
            None => {
                let new_id = Uuid::new_v4();
                let now = chrono::Utc::now();
                let name = format!(
                    "{} {}",
                    req.first_name.as_deref().unwrap_or(""),
                    req.last_name.as_deref().unwrap_or("")
                )
                .trim()
                .to_string();
                let display_name = if name.is_empty() { email.clone() } else { name };

                sqlx::query(
                    r#"INSERT INTO accounts (id, email, name, role, credits_balance, credits_lifetime_used, created_at, updated_at)
                       VALUES ($1, $2, $3, 'company_admin', 0, 0, $4, $5)
                       ON CONFLICT (email) DO UPDATE SET updated_at = NOW()"#
                )
                .bind(new_id)
                .bind(&email)
                .bind(&display_name)
                .bind(now)
                .bind(now)
                .execute(&state.db)
                .await?;
                Some(new_id)
            }
        }
    } else {
        None
    };

    tracing::info!(
        "[register-member] {} {} enrolled in {} (program={}) contact={} member={} existed={}",
        req.member_type,
        email,
        program_name,
        lookup_slug,
        contact_id,
        member_id,
        already_existed
    );

    Ok(Json(json!({
        "success": true,
        "contact_id": contact_id.to_string(),
        "member_id": member_id.to_string(),
        "loyalty_program_id": program_id.to_string(),
        "loyalty_program_name": program_name,
        "already_existed": already_existed,
        "message": if already_existed {
            format!("{} was already enrolled in {}", email, program_name)
        } else {
            format!("{} enrolled in {}", email, program_name)
        },
    })))
}
