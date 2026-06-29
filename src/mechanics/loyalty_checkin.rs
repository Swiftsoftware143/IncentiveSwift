//! Loyalty check-in processing logic.
//!
//! Full flow as specified in the architecture:
//! 1. Upsert contact
//! 2. Find or create loyalty_member row
//! 3. Count today's check-ins — enforce daily cap
//! 4. Award points
//! 5. Check reward tier thresholds
//! 6. Auto-approve or mark pending
//! 7. Push delivery for auto-approved rewards

use crate::error::AppError;
use crate::state::AppState;
use sqlx::Row;

/// Process a loyalty check-in for a contact in a given program.
///
/// Returns a result indicating success or an error explaining why the check-in was rejected.
#[allow(dead_code)]
pub async fn process_checkin(
    state: &AppState,
    program_id: &str,
    contact_id: &str,
    method: &str,
) -> Result<CheckinResult, AppError> {
    // Get program config
    let program = get_program(state, program_id).await?;

    // Find or create loyalty member
    let member_id = find_or_create_member(state, program_id, contact_id).await?;

    // Check daily cap
    let today_count = count_daily_checkins(state, &member_id).await?;
    if today_count >= program.max_checkins_per_day {
        return Ok(CheckinResult::DailyCapReached {
            message: "Come back tomorrow! You've reached your daily check-in limit.".to_string(),
        });
    }

    // Award points: create an entry and checkin record
    let checkin_points = program.points_per_checkin;
    record_checkin(state, &member_id, checkin_points, method).await?;

    // Update points_balance and lifetime_points
    let new_balance = update_points_balance(state, &member_id, checkin_points).await?;

    // Check reward tier thresholds
    let newly_crossed = check_threshold_crossed(state, program_id, new_balance, &member_id).await?;

    let mut rewards_awarded = Vec::new();

    for tier in newly_crossed {
        if !tier.requires_approval {
            // Auto-approve
            let reward_id = create_reward(state, &member_id, &tier.id, "approved").await?;
            apply_reward_tag(state, contact_id, &tier.reward_tag).await?;

            // Push delivery
            let _ = push_reward_notification(state, contact_id, &tier.name).await;

            rewards_awarded.push(RewardInfo {
                id: reward_id,
                name: tier.name.clone(),
                status: "approved".to_string(),
            });
        } else {
            // Requires manual approval
            let reward_id = create_reward(state, &member_id, &tier.id, "pending").await?;
            rewards_awarded.push(RewardInfo {
                id: reward_id,
                name: tier.name.clone(),
                status: "pending".to_string(),
            });
        }
    }

    Ok(CheckinResult::Success {
        points_awarded: checkin_points,
        new_balance,
        rewards_awarded,
    })
}

#[derive(Debug)]
pub struct ProgramInfo {
    pub id: String,
    pub points_per_checkin: i32,
    pub max_checkins_per_day: i32,
}

#[derive(Debug)]
pub struct RewardTierInfo {
    pub id: String,
    pub name: String,
    pub points_required: i32,
    pub requires_approval: bool,
    pub reward_tag: String,
}

#[derive(Debug)]
pub struct RewardInfo {
    pub id: String,
    pub name: String,
    pub status: String,
}

#[derive(Debug)]
pub enum CheckinResult {
    Success {
        points_awarded: i32,
        new_balance: i32,
        rewards_awarded: Vec<RewardInfo>,
    },
    DailyCapReached {
        message: String,
    },
}

async fn get_program(state: &AppState, program_id: &str) -> Result<ProgramInfo, AppError> {
    let row = sqlx::query(
        "SELECT id, points_per_checkin, max_checkins_per_day FROM loyalty_programs WHERE id = $1 AND is_active = true"
    )
    .bind(program_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Loyalty program not found or inactive".to_string()))?;

    Ok(ProgramInfo {
        id: row.get("id"),
        points_per_checkin: row.get("points_per_checkin"),
        max_checkins_per_day: row.get("max_checkins_per_day"),
    })
}

async fn find_or_create_member(
    state: &AppState,
    program_id: &str,
    contact_id: &str,
) -> Result<String, AppError> {
    // Try to find existing member
    let existing: Option<String> = sqlx::query_scalar(
        "SELECT id FROM loyalty_members WHERE program_id = $1 AND contact_id = $2"
    )
    .bind(program_id)
    .bind(contact_id)
    .fetch_optional(&state.db)
    .await?;

    if let Some(id) = existing {
        return Ok(id);
    }

    // Create new member
    let row = sqlx::query(
        "INSERT INTO loyalty_members (program_id, contact_id, points_balance, lifetime_points, member_since)
         VALUES ($1, $2, 0, 0, now())
         RETURNING id"
    )
    .bind(program_id)
    .bind(contact_id)
    .fetch_one(&state.db)
    .await?;

    Ok(row.get("id"))
}

async fn count_daily_checkins(state: &AppState, member_id: &str) -> Result<i32, AppError> {
    let count: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM loyalty_checkins
         WHERE member_id = $1 AND checked_in_at::date = CURRENT_DATE"
    )
    .bind(member_id)
    .fetch_one(&state.db)
    .await?;

    Ok(count.0 as i32)
}

async fn record_checkin(
    state: &AppState,
    member_id: &str,
    points_awarded: i32,
    method: &str,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO loyalty_checkins (member_id, points_awarded, method, checked_in_at)
         VALUES ($1, $2, $3, now())"
    )
    .bind(member_id)
    .bind(points_awarded)
    .bind(method)
    .execute(&state.db)
    .await?;

    Ok(())
}

async fn update_points_balance(
    state: &AppState,
    member_id: &str,
    points: i32,
) -> Result<i32, AppError> {
    let row = sqlx::query(
        "UPDATE loyalty_members
         SET points_balance = points_balance + $1,
             lifetime_points = lifetime_points + $1,
             last_checkin_at = now()
         WHERE id = $2
         RETURNING points_balance"
    )
    .bind(points)
    .bind(member_id)
    .fetch_one(&state.db)
    .await?;

    Ok(row.get("points_balance"))
}

async fn check_threshold_crossed(
    state: &AppState,
    program_id: &str,
    new_balance: i32,
    member_id: &str,
) -> Result<Vec<RewardTierInfo>, AppError> {
    // Find tiers where points_required <= new_balance AND no existing reward for this member+tier
    let rows = sqlx::query(
        "SELECT rt.id, rt.name, rt.points_required, rt.requires_approval, rt.reward_tag
         FROM loyalty_reward_tiers rt
         WHERE rt.program_id = $1
           AND rt.points_required <= $2
           AND NOT EXISTS (
               SELECT 1 FROM loyalty_rewards_earned re
               WHERE re.member_id = $3 AND re.tier_id = rt.id
           )
         ORDER BY rt.points_required ASC"
    )
    .bind(program_id)
    .bind(new_balance)
    .bind(member_id)
    .fetch_all(&state.db)
    .await?;

    Ok(rows.iter().map(|row| RewardTierInfo {
        id: row.get("id"),
        name: row.get("name"),
        points_required: row.get("points_required"),
        requires_approval: row.get("requires_approval"),
        reward_tag: row.get("reward_tag"),
    }).collect())
}

async fn create_reward(
    state: &AppState,
    member_id: &str,
    tier_id: &str,
    status: &str,
) -> Result<String, AppError> {
    let row = sqlx::query(
        "INSERT INTO loyalty_rewards_earned (member_id, tier_id, status, earned_at)
         VALUES ($1, $2, $3, now())
         RETURNING id"
    )
    .bind(member_id)
    .bind(tier_id)
    .bind(status)
    .fetch_one(&state.db)
    .await?;

    Ok(row.get("id"))
}

async fn apply_reward_tag(
    state: &AppState,
    contact_id: &str,
    tag: &str,
) -> Result<(), AppError> {
    // Update the contact's tags_applied or similar field
    // This depends on schema — here we update a hypothetical tags field
    sqlx::query(
        "UPDATE contacts SET notes = COALESCE(notes, '') || $1 WHERE id = $2"
    )
    .bind(format!("\n[Reward Tag: {}]", tag))
    .bind(contact_id)
    .execute(&state.db)
    .await?;

    Ok(())
}

async fn push_reward_notification(
    _state: &AppState,
    _contact_id: &str,
    _reward_name: &str,
) -> Result<(), AppError> {
    // In production, push notification via delivery system
    // For now, this is a placeholder
    tracing::info!("Reward notification would be sent for: {} (contact: {})", _reward_name, _contact_id);
    Ok(())
}
