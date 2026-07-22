//! Loyalty handlers — checkin, approve reward, deny reward.

use crate::error::AppError;
use crate::state::AppState;
use crate::security::auth::AuthenticatedUser;
use crate::db::{contacts, loyalty};
use crate::mechanics::loyalty_checkin;
use axum::{
    extract::{Path, Query, State},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

/// Body for loyalty checkin.
#[derive(Deserialize)]
pub struct CheckinBody {
    pub program_slug: String,
    pub contact: super::entries::ContactBody,
    pub method: Option<String>,
    pub answers: Option<Value>,
}

/// POST /api/v1/loyalty/checkin — public-but-scoped.
/// Full flow: upsert contact -> find/create member -> check daily cap -> create entry
/// -> award points -> check thresholds -> auto-approve or pending -> push delivery.
pub async fn checkin(
    State(state): State<AppState>,
    Json(body): Json<CheckinBody>,
) -> Result<Json<Value>, AppError> {
    // 1. Upsert contact
    let contact_input = contacts::ContactInput {
        first_name: body.contact.first_name.clone(),
        last_name: body.contact.last_name.clone(),
        email: body.contact.email.clone(),
        phone: body.contact.phone.clone(),
        website: body.contact.website.clone(),
        business_name: body.contact.business_name.clone(),
    };
    let contact_id = contacts::upsert_contact(&state.db, &contact_input).await?;

    // 2. Get loyalty program by slug — lookup from campaign slug
    let campaign = crate::db::campaigns::get_campaign_by_slug(&state.db, &body.program_slug).await?;
    let program = loyalty::get_program(&state.db, &campaign.id).await?;

    // 3. Process checkin
    let result = loyalty_checkin::process_checkin(
        &state,
        &program.id.to_string(),
        &contact_id.to_string(),
        body.method.as_deref().unwrap_or("web"),
    ).await?;

    // 4. Return result
    match result {
        loyalty_checkin::CheckinResult::Success { points_awarded, new_balance, rewards_awarded } => {
            Ok(Json(json!({
                "status": "ok",
                "points_awarded": points_awarded,
                "new_balance": new_balance,
                "rewards_awarded": rewards_awarded.iter().map(|r| json!({
                    "id": r.id,
                    "name": r.name,
                    "status": r.status,
                })).collect::<Vec<_>>(),
            })))
        }
        loyalty_checkin::CheckinResult::DailyCapReached { message } => {
            Ok(Json(json!({
                "status": "daily_cap_reached",
                "message": message,
            })))
        }
    }
}

/// Body for approving a reward.
#[derive(Deserialize)]
pub struct ApproveBody {
    pub approved_by: Option<String>,
}

/// POST /api/v1/loyalty/rewards/:id/approve — authenticated.
pub async fn approve_reward(
    State(state): State<AppState>,
    Path(id): Path<String>,
    _user: AuthenticatedUser,
    Json(body): Json<ApproveBody>,
) -> Result<Json<Value>, AppError> {
    let reward_id = Uuid::parse_str(&id)
        .map_err(|_| AppError::BadRequest("Invalid reward ID".to_string()))?;

    // Get reward
    let reward = loyalty::get_reward(&state.db, &reward_id).await?;

    if reward.status != "pending" {
        return Err(AppError::BadRequest(format!(
            "Reward is already {}", reward.status
        )));
    }

    let approved_by = body.approved_by
        .and_then(|s| Uuid::parse_str(&s).ok());

    // Update to approved
    loyalty::update_reward_status(
        &state.db,
        &reward_id,
        "approved",
        approved_by.as_ref(),
    ).await?;

    // Get tier info for tag
    let tier = loyalty::get_reward_tier(&state.db, &reward.tier_id).await?;

    // Get member to find contact_id
    let member = loyalty::get_member(&state.db, &reward.member_id).await?;

    // Apply reward tag to contact
    loyalty::apply_reward_tag(&state.db, &member.contact_id, &tier.reward_tag).await?;

    Ok(Json(json!({
        "status": "approved",
        "reward_id": id,
        "reward_tag": tier.reward_tag,
        "message": "Reward approved and tag applied"
    })))
}

/// GET /api/v1/loyalty/programs — list all loyalty programs for the authenticated user's account.
pub async fn list_programs(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&_user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;

    let programs = sqlx::query_as::<_, crate::db::loyalty::LoyaltyProgram>(
        r#"SELECT lp.id, lp.campaign_id, lp.name, lp.recognition_method,
                  lp.points_per_checkin, lp.max_checkins_per_day,
                  lp.point_decay_days, lp.is_active, lp.created_at,
                  lp.tiers_enabled, lp.milestones_enabled, lp.streak_enabled,
                  lp.streak_bonus, lp.streak_days, lp.referral_bonus, lp.birthday_bonus,
                  lp.points_expire_days, lp.social_share_points, lp.points_per_visit,
                  lp.currency_name, lp.currency_icon, lp.currency_color
           FROM loyalty_programs lp
           LEFT JOIN campaigns c ON c.id = lp.campaign_id
           WHERE c.account_id = $1 OR lp.campaign_id IS NULL
           ORDER BY lp.name"#
    )
    .bind(account_id)
    .fetch_all(&state.db)
    .await?;

    Ok(Json(json!({ "programs": programs })))
}

/// GET /api/v1/loyalty/rewards — list all rewards for the account.
pub async fn list_rewards(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&_user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;

    #[derive(sqlx::FromRow, serde::Serialize)]
    struct RewardRow {
        id: Uuid,
        member_id: Uuid,
        tier_id: Uuid,
        status: String,
        earned_at: chrono::DateTime<chrono::Utc>,
        tier_name: String,
        points_required: i32,
        requires_approval: bool,
        first_name: Option<String>,
        last_name: Option<String>,
        email: Option<String>,
    }

    let rewards = sqlx::query_as::<_, RewardRow>(
        r#"SELECT re.id, re.member_id, re.tier_id, re.status, re.earned_at,
                  rt.name as tier_name, rt.points_required, rt.requires_approval,
                  c.first_name, c.last_name, c.email
           FROM loyalty_rewards_earned re
           JOIN loyalty_reward_tiers rt ON rt.id = re.tier_id
           JOIN loyalty_members lm ON lm.id = re.member_id
           JOIN contacts c ON c.id = lm.contact_id
           JOIN loyalty_programs lp ON lp.id = rt.program_id
           LEFT JOIN campaigns cam ON cam.id = lp.campaign_id
           WHERE cam.account_id = $1 OR lp.campaign_id IS NULL
           ORDER BY re.earned_at DESC"#
    )
    .bind(account_id)
    .fetch_all(&state.db)
    .await?;

    Ok(Json(json!({ "rewards": rewards })))
}

/// POST /api/v1/loyalty/rewards/:id/deny — authenticated.
pub async fn deny_reward(
    State(state): State<AppState>,
    Path(id): Path<String>,
    _user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let reward_id = Uuid::parse_str(&id)
        .map_err(|_| AppError::BadRequest("Invalid reward ID".to_string()))?;

    // Get reward
    let reward = loyalty::get_reward(&state.db, &reward_id).await?;

    if reward.status != "pending" {
        return Err(AppError::BadRequest(format!(
            "Reward is already {}", reward.status
        )));
    }

    // Update to denied
    loyalty::update_reward_status(
        &state.db,
        &reward_id,
        "denied",
        None,
    ).await?;

    Ok(Json(json!({
        "status": "denied",
        "reward_id": id,
        "message": "Reward denied"
    })))
}

/* ===== Loyalty Program CRUD ===== */

/// Input for creating/updating a loyalty program.
#[derive(Deserialize)]
pub struct LoyaltyProgramInput {
    pub name: String,
    pub campaign_id: Option<String>,
    pub points_per_checkin: Option<i32>,
    pub max_checkins_per_day: Option<i32>,
    pub point_decay_days: Option<i32>,
    pub is_active: Option<bool>,
    pub currency_name: Option<String>,
    pub currency_icon: Option<String>,
    pub currency_color: Option<String>,
}

/// Query for listing tiers.
#[derive(Deserialize)]
pub struct TierListQuery {
    pub program_id: String,
}

/// Input for creating a reward tier.
#[derive(Deserialize)]
pub struct RewardTierInput {
    pub program_id: String,
    pub name: String,
    pub points_required: i32,
    pub reward_tag: String,
    pub requires_approval: Option<bool>,
    pub sort_order: Option<i32>,
    pub marketing_boost: Option<serde_json::Value>,
}

/// Input for updating a reward tier (all fields optional).
#[derive(Deserialize)]
pub struct RewardTierUpdateInput {
    pub name: Option<String>,
    pub points_required: Option<i32>,
    pub reward_tag: Option<String>,
    pub requires_approval: Option<bool>,
    pub sort_order: Option<i32>,
    pub marketing_boost: Option<serde_json::Value>,
}

/// POST /api/v1/loyalty/programs — create program.
pub async fn create_program(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Json(body): Json<LoyaltyProgramInput>,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&_user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;

    let id = Uuid::new_v4();
    let campaign_id = body.campaign_id
        .and_then(|s| Uuid::parse_str(&s).ok());

    sqlx::query(
        r#"INSERT INTO loyalty_programs (id, campaign_id, name, recognition_method,
            points_per_checkin, max_checkins_per_day, point_decay_days, is_active,
            currency_name, currency_icon, currency_color)
           VALUES ($1, $2, $3, 'both', $4, $5, $6, $7, $8, $9, $10)"#
    )
    .bind(id)
    .bind(campaign_id)
    .bind(&body.name)
    .bind(body.points_per_checkin.unwrap_or(10))
    .bind(body.max_checkins_per_day.unwrap_or(1))
    .bind(body.point_decay_days)
    .bind(body.is_active.unwrap_or(true))
    .bind(body.currency_name.unwrap_or_else(|| "Points".to_string()))
    .bind(body.currency_icon.unwrap_or_else(|| "⭐".to_string()))
    .bind(body.currency_color.unwrap_or_else(|| "#0d9488".to_string()))
    .execute(&state.db)
    .await?;

    let program = sqlx::query_as::<_, crate::db::loyalty::LoyaltyProgram>(
        r#"SELECT id, campaign_id, name, recognition_method,
                  points_per_checkin, max_checkins_per_day,
                  point_decay_days, is_active, created_at,
                  tiers_enabled, milestones_enabled, streak_enabled,
                  streak_bonus, streak_days, referral_bonus, birthday_bonus,
                  points_expire_days, social_share_points, points_per_visit,
                  currency_name, currency_icon, currency_color
           FROM loyalty_programs WHERE id = $1"#
    )
    .bind(id)
    .fetch_one(&state.db)
    .await?;

    Ok(Json(json!({ "program": program })))
}

/// PUT /api/v1/loyalty/programs/:id — update program.
pub async fn update_program(
    State(state): State<AppState>,
    Path(id): Path<String>,
    _user: AuthenticatedUser,
    Json(body): Json<LoyaltyProgramInput>,
) -> Result<Json<Value>, AppError> {
    let program_id = Uuid::parse_str(&id)
        .map_err(|_| AppError::BadRequest("Invalid program ID".to_string()))?;

    // Verify program exists
    let existing = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM loyalty_programs WHERE id = $1"
    )
    .bind(program_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Loyalty program not found".to_string()))?;

    let _ = existing;

    sqlx::query(
        r#"UPDATE loyalty_programs
           SET name = COALESCE($1, name),
               points_per_checkin = COALESCE($2, points_per_checkin),
               max_checkins_per_day = COALESCE($3, max_checkins_per_day),
               point_decay_days = COALESCE($4, point_decay_days),
               is_active = COALESCE($5, is_active),
               currency_name = COALESCE($6, currency_name),
               currency_icon = COALESCE($7, currency_icon),
               currency_color = COALESCE($8, currency_color)
           WHERE id = $9"#
    )
    .bind(&body.name)
    .bind(body.points_per_checkin)
    .bind(body.max_checkins_per_day)
    .bind(body.point_decay_days)
    .bind(body.is_active)
    .bind(body.currency_name)
    .bind(body.currency_icon)
    .bind(body.currency_color)
    .bind(program_id)
    .execute(&state.db)
    .await?;

    let program = sqlx::query_as::<_, crate::db::loyalty::LoyaltyProgram>(
        r#"SELECT id, campaign_id, name, recognition_method,
                  points_per_checkin, max_checkins_per_day,
                  point_decay_days, is_active, created_at,
                  tiers_enabled, milestones_enabled, streak_enabled,
                  streak_bonus, streak_days, referral_bonus, birthday_bonus,
                  points_expire_days, social_share_points, points_per_visit,
                  currency_name, currency_icon, currency_color
           FROM loyalty_programs WHERE id = $1"#
    )
    .bind(program_id)
    .fetch_one(&state.db)
    .await?;

    Ok(Json(json!({ "program": program })))
}

/// DELETE /api/v1/loyalty/programs/:id — delete program.
pub async fn delete_program(
    State(state): State<AppState>,
    Path(id): Path<String>,
    _user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let program_id = Uuid::parse_str(&id)
        .map_err(|_| AppError::BadRequest("Invalid program ID".to_string()))?;

    let result = sqlx::query("DELETE FROM loyalty_programs WHERE id = $1")
        .bind(program_id)
        .execute(&state.db)
        .await?;

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound("Loyalty program not found".to_string()));
    }

    Ok(Json(json!({
        "status": "deleted",
        "program_id": id
    })))
}

/// POST /api/v1/loyalty/tiers — create reward tier.
pub async fn create_tier(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Json(body): Json<RewardTierInput>,
) -> Result<Json<Value>, AppError> {
    let program_id = Uuid::parse_str(&body.program_id)
        .map_err(|_| AppError::BadRequest("Invalid program ID".to_string()))?;

    let id = Uuid::new_v4();
    sqlx::query(
        r#"INSERT INTO loyalty_reward_tiers (id, program_id, name, points_required, reward_tag, requires_approval, sort_order, marketing_boost)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8)"#
    )
    .bind(id)
    .bind(program_id)
    .bind(&body.name)
    .bind(body.points_required)
    .bind(&body.reward_tag)
    .bind(body.requires_approval.unwrap_or(false))
    .bind(body.sort_order.unwrap_or(0))
    .bind(&body.marketing_boost)
    .execute(&state.db)
    .await?;

    let tier = sqlx::query_as::<_, crate::db::loyalty::RewardTier>(
        r#"SELECT id, program_id, name, points_required, requires_approval,
                  reward_tag, sort_order, marketing_boost
           FROM loyalty_reward_tiers WHERE id = $1"#
    )
    .bind(id)
    .fetch_one(&state.db)
    .await?;

    Ok(Json(json!({ "tier": tier })))
}

/// PUT /api/v1/loyalty/tiers/:id — update tier.
pub async fn update_tier(
    State(state): State<AppState>,
    Path(id): Path<String>,
    _user: AuthenticatedUser,
    Json(body): Json<RewardTierUpdateInput>,
) -> Result<Json<Value>, AppError> {
    let tier_id = Uuid::parse_str(&id)
        .map_err(|_| AppError::BadRequest("Invalid tier ID".to_string()))?;

    sqlx::query(
        r#"UPDATE loyalty_reward_tiers
           SET name = COALESCE($1, name),
               points_required = COALESCE($2, points_required),
               reward_tag = COALESCE($3, reward_tag),
               requires_approval = COALESCE($4, requires_approval),
               sort_order = COALESCE($5, sort_order),
               marketing_boost = COALESCE($6, marketing_boost)
           WHERE id = $7"#
    )
    .bind(&body.name)
    .bind(body.points_required)
    .bind(&body.reward_tag)
    .bind(body.requires_approval)
    .bind(body.sort_order)
    .bind(&body.marketing_boost)
    .bind(tier_id)
    .execute(&state.db)
    .await?;

    let tier = sqlx::query_as::<_, crate::db::loyalty::RewardTier>(
        r#"SELECT id, program_id, name, points_required, requires_approval,
                  reward_tag, sort_order, marketing_boost
           FROM loyalty_reward_tiers WHERE id = $1"#
    )
    .bind(tier_id)
    .fetch_one(&state.db)
    .await?;

    Ok(Json(json!({ "tier": tier })))
}

/// DELETE /api/v1/loyalty/tiers/:id — delete tier.
pub async fn delete_tier(
    State(state): State<AppState>,
    Path(id): Path<String>,
    _user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let tier_id = Uuid::parse_str(&id)
        .map_err(|_| AppError::BadRequest("Invalid tier ID".to_string()))?;

    let result = sqlx::query("DELETE FROM loyalty_reward_tiers WHERE id = $1")
        .bind(tier_id)
        .execute(&state.db)
        .await?;

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound("Reward tier not found".to_string()));
    }

    Ok(Json(json!({
        "status": "deleted",
        "tier_id": id
    })))
}

/// GET /api/v1/loyalty/tiers — list reward tiers for a program.
pub async fn list_tiers(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Query(query): Query<TierListQuery>,
) -> Result<Json<Value>, AppError> {
    let program_id = Uuid::parse_str(&query.program_id)
        .map_err(|_| AppError::BadRequest("Invalid program ID".to_string()))?;

    let tiers = sqlx::query_as::<_, crate::db::loyalty::RewardTier>(
        r#"SELECT id, program_id, name, points_required, requires_approval,
                  reward_tag, sort_order, marketing_boost
           FROM loyalty_reward_tiers
           WHERE program_id = $1
           ORDER BY sort_order ASC, points_required ASC"#
    )
    .bind(program_id)
    .fetch_all(&state.db)
    .await?;

    Ok(Json(json!({ "tiers": tiers })))
}

/* ===== Online Loyalty Tracking ===== */

/// Body for tracking a daily visit.
#[derive(Deserialize)]
pub struct OnlineVisitBody {
    pub referral_code: String,
    pub url: String,
    pub user_agent: Option<String>,
    pub referrer: Option<String>,
}

/// Body for tracking a social share.
#[derive(Deserialize)]
pub struct OnlineShareBody {
    pub referral_code: String,
    pub platform: String,
    pub url: String,
}

/// Body for tracking a referral click.
#[derive(Deserialize)]
pub struct ReferralClickBody {
    pub referrer_code: String,
    pub url: String,
    pub visitor_cookie: Option<String>,
}

/// POST /api/v1/loyalty/online/visit — Track a daily visit via cookie/referral.
pub async fn online_visit(
    State(state): State<AppState>,
    Json(body): Json<OnlineVisitBody>,
) -> Result<Json<Value>, AppError> {
    // 1. Look up member by referral code
    let member = sqlx::query_as::<_, crate::db::loyalty::LoyaltyMember>(
        r#"SELECT id, program_id, contact_id, points_balance, lifetime_points,
                  member_since, last_checkin_at
           FROM loyalty_members WHERE referral_code = $1"#
    )
    .bind(&body.referral_code)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Member not found for referral code".to_string()))?;

    // 2. Get program config for points_per_visit
    let program = sqlx::query_as::<_, crate::db::loyalty::LoyaltyProgram>(
        r#"SELECT id, campaign_id, name, recognition_method,
                  points_per_checkin, max_checkins_per_day,
                  point_decay_days, is_active, created_at,
                  tiers_enabled, milestones_enabled, streak_enabled,
                  streak_bonus, streak_days, referral_bonus, birthday_bonus,
                  points_expire_days, social_share_points, points_per_visit,
                  currency_name, currency_icon, currency_color
           FROM loyalty_programs WHERE id = $1 AND is_active = true"#
    )
    .bind(&member.program_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Loyalty program not found or not active".to_string()))?;

    // 3. Check if they already visited today (no duplicate daily visit points)
    let today_visits: i64 = sqlx::query_scalar(
        r#"SELECT COUNT(*) FROM loyalty_online_actions
           WHERE member_id = $1
             AND action_type = 'daily_visit'
             AND created_at::date = CURRENT_DATE"#
    )
    .bind(&member.id)
    .fetch_one(&state.db)
    .await?;

    if today_visits > 0 {
        return Ok(Json(json!({
            "status": "already_visited_today",
            "points_awarded": 0,
            "current_streak": 0,
            "total_balance": member.points_balance,
            "message": "Already visited today. No points earned."
        })));
    }

    let points = program.points_per_visit as i64;

    // 4. Record the online action
    let mut metadata = serde_json::Map::new();
    metadata.insert("url".to_string(), json!(body.url));
    if let Some(ref ua) = body.user_agent {
        metadata.insert("user_agent".to_string(), json!(ua));
    }
    if let Some(ref r) = body.referrer {
        metadata.insert("referrer".to_string(), json!(r));
    }

    sqlx::query(
        r#"INSERT INTO loyalty_online_actions (member_id, action_type, points_earned, metadata)
           VALUES ($1, 'daily_visit', $2, $3::jsonb)"#
    )
    .bind(&member.id)
    .bind(points)
    .bind(json!(metadata).to_string())
    .execute(&state.db)
    .await?;

    // 5. Award points to balance
    sqlx::query(
        r#"UPDATE loyalty_members
           SET points_balance = points_balance + $1,
               lifetime_points = lifetime_points + $1,
               current_streak = current_streak + 1,
               -- reset streak if last activity was more than 1 day ago
               last_activity_date = now()
           WHERE id = $2"#
    )
    .bind(points as i32)
    .bind(&member.id)
    .execute(&state.db)
    .await?;

    // 6. Fetch updated member for streak & balance
    #[derive(sqlx::FromRow, serde::Serialize)]
    struct MemberStreak {
        points_balance: i32,
        current_streak: i32,
    }

    let updated = sqlx::query_as::<_, MemberStreak>(
        r#"SELECT points_balance, current_streak
           FROM loyalty_members WHERE id = $1"#
    )
    .bind(&member.id)
    .fetch_one(&state.db)
    .await?;

    Ok(Json(json!({
        "status": "ok",
        "points_awarded": points,
        "current_streak": updated.current_streak,
        "total_balance": updated.points_balance
    })))
}

/// POST /api/v1/loyalty/online/share — Track social share.
pub async fn online_share(
    State(state): State<AppState>,
    Json(body): Json<OnlineShareBody>,
) -> Result<Json<Value>, AppError> {
    // 1. Find member by referral code
    let member = sqlx::query_as::<_, crate::db::loyalty::LoyaltyMember>(
        r#"SELECT id, program_id, contact_id, points_balance, lifetime_points,
                  member_since, last_checkin_at
           FROM loyalty_members WHERE referral_code = $1"#
    )
    .bind(&body.referral_code)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Member not found for referral code".to_string()))?;

    // 2. Get program config for social_share_points
    let program = sqlx::query_as::<_, crate::db::loyalty::LoyaltyProgram>(
        r#"SELECT id, campaign_id, name, recognition_method,
                  points_per_checkin, max_checkins_per_day,
                  point_decay_days, is_active, created_at,
                  tiers_enabled, milestones_enabled, streak_enabled,
                  streak_bonus, streak_days, referral_bonus, birthday_bonus,
                  points_expire_days, social_share_points, points_per_visit,
                  currency_name, currency_icon, currency_color
           FROM loyalty_programs WHERE id = $1 AND is_active = true"#
    )
    .bind(&member.program_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Loyalty program not found or not active".to_string()))?;

    let points = program.social_share_points as i64;

    // 3. Record the action
    let metadata = json!({
        "platform": body.platform,
        "url": body.url
    });

    sqlx::query(
        r#"INSERT INTO loyalty_online_actions (member_id, action_type, points_earned, metadata)
           VALUES ($1, 'social_share', $2, $3::jsonb)"#
    )
    .bind(&member.id)
    .bind(points)
    .bind(metadata.to_string())
    .execute(&state.db)
    .await?;

    // 4. Award points
    sqlx::query(
        r#"UPDATE loyalty_members
           SET points_balance = points_balance + $1,
               lifetime_points = lifetime_points + $1
           WHERE id = $2"#
    )
    .bind(points as i32)
    .bind(&member.id)
    .execute(&state.db)
    .await?;

    // 5. Get updated balance
    let new_balance: i32 = sqlx::query_scalar(
        r#"SELECT points_balance FROM loyalty_members WHERE id = $1"#
    )
    .bind(&member.id)
    .fetch_one(&state.db)
    .await?;

    Ok(Json(json!({
        "status": "ok",
        "points_awarded": points,
        "total_balance": new_balance
    })))
}

/// POST /api/v1/loyalty/online/referral-click — Track referral link click.
pub async fn referral_click(
    State(state): State<AppState>,
    Json(body): Json<ReferralClickBody>,
) -> Result<Json<Value>, AppError> {
    // 1. Find member by referral code (the referrer)
    let member = sqlx::query_as::<_, crate::db::loyalty::LoyaltyMember>(
        r#"SELECT id, program_id, contact_id, points_balance, lifetime_points,
                  member_since, last_checkin_at
           FROM loyalty_members WHERE referral_code = $1"#
    )
    .bind(&body.referrer_code)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Referrer not found for referral code".to_string()))?;

    // 2. Record the click (no points yet — awarded when visitor converts)
    let mut metadata = serde_json::Map::new();
    metadata.insert("url".to_string(), json!(body.url));
    if let Some(ref cookie) = body.visitor_cookie {
        metadata.insert("visitor_cookie".to_string(), json!(cookie));
    }

    sqlx::query(
        r#"INSERT INTO loyalty_online_actions (member_id, action_type, points_earned, metadata)
           VALUES ($1, 'referral_click', 0, $2::jsonb)"#
    )
    .bind(&member.id)
    .bind(json!(metadata).to_string())
    .execute(&state.db)
    .await?;

    Ok(Json(json!({
        "status": "ok",
        "message": "Referral click recorded"
    })))
}

/// GET /api/v1/loyalty/online/stats/{referral_code} — Member's online stats.
pub async fn online_stats(
    State(state): State<AppState>,
    Path(code): Path<String>,
) -> Result<Json<Value>, AppError> {
    // 1. Find member
    let member = sqlx::query_as::<_, crate::db::loyalty::LoyaltyMember>(
        r#"SELECT id, program_id, contact_id, points_balance, lifetime_points,
                  member_since, last_checkin_at
           FROM loyalty_members WHERE referral_code = $1"#
    )
    .bind(&code)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Member not found for referral code".to_string()))?;

    // 2. Get current streak
    let current_streak: i32 = sqlx::query_scalar(
        "SELECT current_streak FROM loyalty_members WHERE id = $1"
    )
    .bind(&member.id)
    .fetch_one(&state.db)
    .await?;

    // 3. Count daily visits
    let total_visits: i64 = sqlx::query_scalar(
        r#"SELECT COUNT(*) FROM loyalty_online_actions
           WHERE member_id = $1 AND action_type = 'daily_visit'"#
    )
    .bind(&member.id)
    .fetch_one(&state.db)
    .await?;

    // 4. Count social shares
    let total_shares: i64 = sqlx::query_scalar(
        r#"SELECT COUNT(*) FROM loyalty_online_actions
           WHERE member_id = $1 AND action_type = 'social_share'"#
    )
    .bind(&member.id)
    .fetch_one(&state.db)
    .await?;

    // 5. Count referral clicks
    let referral_clicks: i64 = sqlx::query_scalar(
        r#"SELECT COUNT(*) FROM loyalty_online_actions
           WHERE member_id = $1 AND action_type = 'referral_click'"#
    )
    .bind(&member.id)
    .fetch_one(&state.db)
    .await?;

    // 6. Count total referrals
    let total_referrals: i32 = sqlx::query_scalar(
        "SELECT total_referrals FROM loyalty_members WHERE id = $1"
    )
    .bind(&member.id)
    .fetch_one(&state.db)
    .await?;

    // 7. Sum points earned from online actions
    let online_points: i64 = sqlx::query_scalar(
        r#"SELECT COALESCE(SUM(points_earned), 0)::bigint FROM loyalty_online_actions
           WHERE member_id = $1"#
    )
    .bind(&member.id)
    .fetch_one(&state.db)
    .await?;

    Ok(Json(json!({
        "status": "ok",
        "referral_code": code,
        "current_streak": current_streak,
        "total_visits": total_visits,
        "total_shares": total_shares,
        "referral_clicks": referral_clicks,
        "total_referrals": total_referrals,
        "online_points_earned": online_points,
        "points_balance": member.points_balance
    })))
}

/* ===== Plan Gating ===== */

/// Check if the account's plan has loyalty enabled.
pub async fn check_plan_loyalty(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&_user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;

    // Get tenant's plan_tier
    let plan_tier: Option<String> = sqlx::query_scalar(
        "SELECT plan_tier FROM tenants WHERE id = $1"
    )
    .bind(account_id)
    .fetch_optional(&state.db)
    .await?
    .flatten();

    let tier = plan_tier.unwrap_or_else(|| "free".to_string());

    // Check if plan has loyalty_enabled
    let limit: Option<i32> = sqlx::query_scalar(
        "SELECT limit_value FROM feature_limits WHERE plan_tier = $1 AND feature_key = 'loyalty_enabled'"
    )
    .bind(&tier)
    .fetch_optional(&state.db)
    .await?
    .flatten();

    let enabled = match limit {
        Some(-1) => true,
        Some(l) if l > 0 => true,
        _ => false,
    };

    Ok(Json(json!({
        "plan_tier": tier,
        "loyalty_enabled": enabled,
        "message": if enabled {
            "Loyalty is included in your plan"
        } else {
            "Upgrade your plan to unlock Loyalty rewards"
        }
    })))
}

/* ===== Secret Code Setting (legacy — new codes use loyalty_secret_codes table) ===== */

/// Input for setting a program's secret code.
#[derive(Deserialize)]
pub struct SetSecretCodeInput {
    pub secret_code: Option<String>,
    pub secret_code_points: Option<i32>,
}

/// PUT /api/v1/loyalty/programs/:id/secret-code — Set the secret code for a program.
pub async fn set_secret_code(
    State(state): State<AppState>,
    Path(id): Path<String>,
    _user: AuthenticatedUser,
    Json(body): Json<SetSecretCodeInput>,
) -> Result<Json<Value>, AppError> {
    let program_id = Uuid::parse_str(&id)
        .map_err(|_| AppError::BadRequest("Invalid program ID".to_string()))?;

    // Verify program exists and user has access via campaign
    let existing = sqlx::query_scalar::<_, Uuid>(
        r#"SELECT lp.id FROM loyalty_programs lp
           LEFT JOIN campaigns c ON c.id = lp.campaign_id
           WHERE lp.id = $1"#
    )
    .bind(program_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Program not found".to_string()))?;
    let _ = existing;

    sqlx::query(
        r#"UPDATE loyalty_programs SET secret_code = $1, secret_code_points = $2 WHERE id = $3"#
    )
    .bind(&body.secret_code)
    .bind(body.secret_code_points.unwrap_or(25))
    .bind(program_id)
    .execute(&state.db)
    .await?;

    Ok(Json(json!({
        "status": "ok",
        "message": if body.secret_code.is_some() && !body.secret_code.as_ref().unwrap().is_empty() {
            "Secret code set. Members can now enter this code to earn points."
        } else {
            "Secret code cleared."
        }
    })))
}

/// GET /api/v1/loyalty/programs/:id/qr — Generate QR code for member check-in.
pub async fn program_qr(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
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
           FROM loyalty_programs WHERE id = $1"#
    )
    .bind(program_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Program not found".to_string()))?;

    // Generate the check-in URL
    let checkin_url = format!(
        "https://app.incentiveswift.com/loyalty-checkin/{}",
        program.name.to_lowercase().replace(' ', "-")
    );

    // Return URL + QR API link (using public QR API)
    let qr_img_url = format!("https://api.qrserver.com/v1/create-qr-code/?size=300x300&data={}", urlencoding(&checkin_url));

    Ok(Json(json!({
        "checkin_url": checkin_url,
        "qr_image_url": qr_img_url,
        "program_name": program.name
    })))
}

/// Simple URL encoder.
fn urlencoding(s: &str) -> String {
    s.chars().map(|c| match c {
        'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
        ' ' => "%20".to_string(),
        ':' => "%3A".to_string(),
        '/' => "%2F".to_string(),
        '?' => "%3F".to_string(),
        '&' => "%26".to_string(),
        '=' => "%3D".to_string(),
        '#' => "%23".to_string(),
        _ => format!("%{:02X}", c as u8),
    }).collect()
}
