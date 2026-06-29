//! Loyalty database operations — member management, checkins, rewards, thresholds.

use crate::error::AppError;
use sqlx::{PgPool, Row};
use uuid::Uuid;

/// Find an existing loyalty member or create a new one.
pub async fn find_or_create_member(
    pool: &PgPool,
    program_id: &Uuid,
    contact_id: &Uuid,
) -> Result<Uuid, AppError> {
    // Try to find existing member
    let existing: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM loyalty_members WHERE program_id = $1 AND contact_id = $2"
    )
    .bind(program_id)
    .bind(contact_id)
    .fetch_optional(pool)
    .await?;

    if let Some(id) = existing {
        return Ok(id);
    }

    // Create new member
    let id = Uuid::new_v4();
    sqlx::query(
        r#"INSERT INTO loyalty_members (id, program_id, contact_id, points_balance, lifetime_points)
           VALUES ($1, $2, $3, 0, 0)"#
    )
    .bind(id)
    .bind(program_id)
    .bind(contact_id)
    .execute(pool)
    .await?;

    Ok(id)
}

/// Count today's checkins for a member.
pub async fn count_daily_checkins(
    pool: &PgPool,
    member_id: &Uuid,
) -> Result<i64, AppError> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM loyalty_checkins WHERE member_id = $1 AND checked_in_at::date = CURRENT_DATE"
    )
    .bind(member_id)
    .fetch_one(pool)
    .await?;

    Ok(count)
}

/// Record a checkin and update points balances.
pub async fn record_checkin(
    pool: &PgPool,
    member_id: &Uuid,
    points: i32,
    method: &str,
    entry_id: &Uuid,
) -> Result<(), AppError> {
    let checkin_id = Uuid::new_v4();
    sqlx::query(
        r#"INSERT INTO loyalty_checkins (id, member_id, points_awarded, method, entry_id)
           VALUES ($1, $2, $3, $4, $5)"#
    )
    .bind(checkin_id)
    .bind(member_id)
    .bind(points)
    .bind(method)
    .bind(entry_id)
    .execute(pool)
    .await?;

    // Update points balance and lifetime points
    sqlx::query(
        r#"UPDATE loyalty_members
           SET points_balance = points_balance + $1,
               lifetime_points = lifetime_points + $1,
               last_checkin_at = now()
           WHERE id = $2"#
    )
    .bind(points)
    .bind(member_id)
    .execute(pool)
    .await?;

    Ok(())
}

/// Get program config including max_checkins_per_day and points_per_checkin.
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct LoyaltyProgram {
    pub id: uuid::Uuid,
    pub campaign_id: uuid::Uuid,
    pub name: String,
    pub recognition_method: String,
    pub points_per_checkin: i32,
    pub max_checkins_per_day: i32,
    pub point_decay_days: Option<i32>,
    pub is_active: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Get a loyalty program by ID or slug.
pub async fn get_program(
    pool: &PgPool,
    program_id: &Uuid,
) -> Result<LoyaltyProgram, AppError> {
    let program = sqlx::query_as::<_, LoyaltyProgram>(
        r#"SELECT id, campaign_id, name, recognition_method,
                  points_per_checkin, max_checkins_per_day,
                  point_decay_days, is_active, created_at
           FROM loyalty_programs WHERE id = $1 AND is_active = true"#,
    )
    .bind(program_id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::NotFound("Loyalty program not found or not active".to_string()))?;

    Ok(program)
}

/// Check if a reward threshold has just been crossed.
/// Returns the reward tier that was crossed (if any).
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct RewardTier {
    pub id: uuid::Uuid,
    pub program_id: uuid::Uuid,
    pub name: String,
    pub points_required: i32,
    pub requires_approval: bool,
    pub reward_tag: String,
    pub sort_order: i32,
}

pub async fn check_threshold_crossed(
    pool: &PgPool,
    program_id: &Uuid,
    member_id: &Uuid,
    new_balance: i32,
) -> Result<Option<RewardTier>, AppError> {
    // Find tiers where points_required <= new_balance AND no existing reward for this member+tier
    let tier = sqlx::query_as::<_, RewardTier>(
        r#"SELECT t.id, t.program_id, t.name, t.points_required, t.requires_approval,
                  t.reward_tag, t.sort_order
           FROM loyalty_reward_tiers t
           WHERE t.program_id = $1
             AND t.points_required <= $2
             AND NOT EXISTS (
                 SELECT 1 FROM loyalty_rewards_earned re
                 WHERE re.member_id = $3 AND re.tier_id = t.id
             )
           ORDER BY t.points_required DESC
           LIMIT 1"#,
    )
    .bind(program_id)
    .bind(new_balance)
    .bind(member_id)
    .fetch_optional(pool)
    .await?;

    Ok(tier)
}

/// Create a new reward earned record.
pub async fn create_reward(
    pool: &PgPool,
    member_id: &Uuid,
    tier_id: &Uuid,
    status: &str,
) -> Result<Uuid, AppError> {
    let id = Uuid::new_v4();
    sqlx::query(
        r#"INSERT INTO loyalty_rewards_earned (id, member_id, tier_id, status)
           VALUES ($1, $2, $3, $4)"#
    )
    .bind(id)
    .bind(member_id)
    .bind(tier_id)
    .bind(status)
    .execute(pool)
    .await?;

    Ok(id)
}

/// Apply a reward tag to a contact (stored in contact notes).
pub async fn apply_reward_tag(
    pool: &PgPool,
    contact_id: &Uuid,
    tag: &str,
) -> Result<(), AppError> {
    sqlx::query(
        r#"UPDATE contacts
           SET notes = COALESCE(notes || E'\n', '') || $1 || ' earned at ' || now()::text
           WHERE id = $2"#
    )
    .bind(tag)
    .bind(contact_id)
    .execute(pool)
    .await?;

    Ok(())
}

/// Get a reward earned record by ID.
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct RewardEarned {
    pub id: uuid::Uuid,
    pub member_id: uuid::Uuid,
    pub tier_id: uuid::Uuid,
    pub status: String,
    pub earned_at: chrono::DateTime<chrono::Utc>,
    pub approved_by: Option<uuid::Uuid>,
    pub fulfilled_at: Option<chrono::DateTime<chrono::Utc>>,
}

pub async fn get_reward(
    pool: &PgPool,
    reward_id: &Uuid,
) -> Result<RewardEarned, AppError> {
    let reward = sqlx::query_as::<_, RewardEarned>(
        r#"SELECT id, member_id, tier_id, status, earned_at, approved_by, fulfilled_at
           FROM loyalty_rewards_earned WHERE id = $1"#,
    )
    .bind(reward_id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::NotFound("Reward not found".to_string()))?;

    Ok(reward)
}

/// Update reward status (approve or deny).
pub async fn update_reward_status(
    pool: &PgPool,
    reward_id: &Uuid,
    status: &str,
    approved_by: Option<&Uuid>,
) -> Result<(), AppError> {
    if status == "approved" {
        sqlx::query(
            r#"UPDATE loyalty_rewards_earned
               SET status = $1, approved_by = $2, fulfilled_at = now()
               WHERE id = $3"#
        )
        .bind(status)
        .bind(approved_by)
        .bind(reward_id)
        .execute(pool)
        .await?;
    } else {
        sqlx::query(
            "UPDATE loyalty_rewards_earned SET status = $1 WHERE id = $2"
        )
        .bind(status)
        .bind(reward_id)
        .execute(pool)
        .await?;
    }

    Ok(())
}

/// Get reward tier by ID.
pub async fn get_reward_tier(
    pool: &PgPool,
    tier_id: &Uuid,
) -> Result<RewardTier, AppError> {
    let tier = sqlx::query_as::<_, RewardTier>(
        r#"SELECT id, program_id, name, points_required, requires_approval,
                  reward_tag, sort_order
           FROM loyalty_reward_tiers WHERE id = $1"#,
    )
    .bind(tier_id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::NotFound("Reward tier not found".to_string()))?;

    Ok(tier)
}

/// Get member by ID.
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct LoyaltyMember {
    pub id: uuid::Uuid,
    pub program_id: uuid::Uuid,
    pub contact_id: uuid::Uuid,
    pub points_balance: i32,
    pub lifetime_points: i32,
    pub member_since: chrono::DateTime<chrono::Utc>,
    pub last_checkin_at: Option<chrono::DateTime<chrono::Utc>>,
}

pub async fn get_member(
    pool: &PgPool,
    member_id: &Uuid,
) -> Result<LoyaltyMember, AppError> {
    let member = sqlx::query_as::<_, LoyaltyMember>(
        r#"SELECT id, program_id, contact_id, points_balance, lifetime_points,
                  member_since, last_checkin_at
           FROM loyalty_members WHERE id = $1"#,
    )
    .bind(member_id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::NotFound("Loyalty member not found".to_string()))?;

    Ok(member)
}
