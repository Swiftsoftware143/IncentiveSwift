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
use uuid::Uuid;

/// Process a loyalty check-in for a contact in a given program.
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

    // Award points: resolve the member's tier multiplier, then record the
    // checkin with BOTH the base points and the multiplied points.
    let balance_before = member_balance(state, &member_id).await?;
    let award = resolve_award(
        state,
        program_id,
        balance_before,
        program.points_per_checkin,
        program.tiers_enabled,
    )
    .await?;

    record_checkin(state, &member_id, &award, method).await?;

    // Update points_balance and lifetime_points with the multiplied amount
    let new_balance = update_points_balance(state, &member_id, award.awarded).await?;

    // Keep the member's denormalised tier_id in sync with their points.
    let _ = sync_member_tier(
        state,
        program_id,
        &member_id,
        new_balance as i64,
        program.tiers_enabled,
    )
    .await;

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
        base_points: award.base_points,
        points_awarded: award.awarded,
        multiplier: award.multiplier,
        tier_name: award.tier.as_ref().map(|t| t.name.clone()),
        new_balance,
        rewards_awarded,
        milestones_triggered: fire_milestones(
            state,
            program_id,
            contact_id,
            new_balance,
            program.milestones_enabled,
        )
        .await,
    })
}

#[derive(Debug)]
pub struct ProgramInfo {
    pub id: String,
    pub points_per_checkin: i32,
    pub max_checkins_per_day: i32,
    pub tiers_enabled: bool,
    /// loyalty_programs.milestones_enabled — gates the campaign-milestone hook.
    pub milestones_enabled: bool,
}

/// The tier a member currently holds, and the multiplier it earns at.
#[derive(Debug, Clone)]
pub struct TierInfo {
    pub id: String,
    pub name: String,
    pub multiplier: f64,
}

/// A resolved award: what the program config said (base) vs what actually
/// landed in the member's balance (awarded) once the tier multiplier applied.
#[derive(Debug, Clone)]
pub struct Award {
    pub base_points: i32,
    pub awarded: i32,
    pub multiplier: f64,
    pub tier: Option<TierInfo>,
}

impl Award {
    /// One-line arithmetic record, written to loyalty_activity so the ledger
    /// stays auditable: base, multiplier, tier and the final awarded amount.
    pub fn audit_note(&self, source: &str) -> String {
        match &self.tier {
            Some(t) => format!(
                "{} base={} x multiplier={} (tier {}) = {}",
                source, self.base_points, self.multiplier, t.name, self.awarded
            ),
            None => format!(
                "{} base={} x multiplier={} (no tier) = {}",
                source, self.base_points, self.multiplier, self.awarded
            ),
        }
    }
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
        /// Points the program config specified, before the tier multiplier.
        base_points: i32,
        /// Points actually credited (base x tier multiplier).
        points_awarded: i32,
        /// The tier multiplier applied to this award.
        multiplier: f64,
        /// Name of the tier the member held when earning, if any.
        tier_name: Option<String>,
        new_balance: i32,
        rewards_awarded: Vec<RewardInfo>,
        /// Campaign milestones fired by this award, as (name, action_type).
        milestones_triggered: Vec<(String, String)>,
    },
    DailyCapReached {
        message: String,
    },
}

async fn get_program(state: &AppState, program_id: &str) -> Result<ProgramInfo, AppError> {
    let row = sqlx::query(
        "SELECT id::text AS id, points_per_checkin, max_checkins_per_day, tiers_enabled, milestones_enabled FROM loyalty_programs WHERE id = $1::uuid AND is_active = true"
    )
    .bind(program_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Loyalty program not found or inactive".to_string()))?;

    Ok(ProgramInfo {
        id: row.get("id"),
        points_per_checkin: row.get("points_per_checkin"),
        max_checkins_per_day: row.get("max_checkins_per_day"),
        tiers_enabled: row.get("tiers_enabled"),
        milestones_enabled: row.get("milestones_enabled"),
    })
}

/// The member's current points balance, used to resolve the tier they hold.
pub async fn member_balance(state: &AppState, member_id: &str) -> Result<i64, AppError> {
    let balance: i32 = sqlx::query_scalar(
        "SELECT COALESCE(points_balance, 0) FROM loyalty_members WHERE id = $1::uuid",
    )
    .bind(member_id)
    .fetch_one(&state.db)
    .await?;
    Ok(balance as i64)
}

/// Resolve the member's tier from their points: the tier with the highest
/// min_points that is still <= the member's current balance.
pub async fn resolve_tier(
    state: &AppState,
    program_id: &str,
    points: i64,
) -> Result<Option<TierInfo>, AppError> {
    let row = sqlx::query(
        "SELECT id::text AS id, name, multiplier::float8 AS multiplier
           FROM loyalty_tiers
          WHERE loyalty_program_id = $1::uuid AND min_points <= $2
          ORDER BY min_points DESC
          LIMIT 1",
    )
    .bind(program_id)
    .bind(points)
    .fetch_optional(&state.db)
    .await?;

    Ok(row.map(|r| TierInfo {
        id: r.get("id"),
        name: r.get("name"),
        multiplier: r.get("multiplier"),
    }))
}

/// Apply a tier multiplier to a base award. Rounds to the nearest whole point;
/// a missing/unset/<=1.0 multiplier is a pass-through.
pub fn apply_multiplier(base: i32, multiplier: f64) -> i32 {
    if !multiplier.is_finite() || multiplier <= 1.0 {
        return base;
    }
    (base as f64 * multiplier).round() as i32
}

/// Resolve a full award for a member: tier (from the balance they hold *before*
/// this award), multiplier, base points and the multiplied points that should
/// actually be credited. Resolving pre-award is deliberate — you earn at the
/// tier you held when you earned, so crossing a threshold takes effect on the
/// member's next award.
pub async fn resolve_award(
    state: &AppState,
    program_id: &str,
    current_points: i64,
    base_points: i32,
    tiers_enabled: bool,
) -> Result<Award, AppError> {
    let tier = if tiers_enabled {
        resolve_tier(state, program_id, current_points).await?
    } else {
        None
    };
    let multiplier = tier.as_ref().map(|t| t.multiplier).unwrap_or(1.0);
    Ok(Award {
        base_points,
        awarded: apply_multiplier(base_points, multiplier),
        multiplier,
        tier,
    })
}

/// Write a loyalty_activity row. This is the member-facing audit trail and the
/// only durable record of base-vs-multiplied arithmetic (loyalty_checkins has
/// no such column), so every award path writes one.
pub async fn record_activity(
    state: &AppState,
    member_id: &str,
    activity_type: &str,
    description: &str,
    points_earned: i32,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO loyalty_activity (member_id, activity_type, description, points_earned)
         VALUES ($1::uuid, $2, $3, $4)",
    )
    .bind(member_id)
    .bind(activity_type)
    .bind(description)
    .bind(points_earned as i64)
    .execute(&state.db)
    .await?;
    Ok(())
}

/// Keep loyalty_members.tier_id in sync with the member's *current* standing —
/// resolved from their balance AFTER the award, so the column answers "what tier
/// is this member in right now" instead of lagging an award behind. The
/// multiplier applied to an award still comes from the tier held at earn time.
pub async fn sync_member_tier(
    state: &AppState,
    program_id: &str,
    member_id: &str,
    current_points: i64,
    tiers_enabled: bool,
) -> Result<(), AppError> {
    let tier_id: Option<String> = if tiers_enabled {
        resolve_tier(state, program_id, current_points)
            .await?
            .map(|t| t.id)
    } else {
        None
    };

    sqlx::query("UPDATE loyalty_members SET tier_id = $1::uuid WHERE id = $2::uuid")
        .bind(tier_id)
        .bind(member_id)
        .execute(&state.db)
        .await?;
    Ok(())
}

async fn find_or_create_member(
    state: &AppState,
    program_id: &str,
    contact_id: &str,
) -> Result<String, AppError> {
    // Try to find existing member
    let existing: Option<String> = sqlx::query_scalar(
        "SELECT id::text FROM loyalty_members WHERE program_id = $1::uuid AND contact_id = $2::uuid",
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
         VALUES ($1::uuid, $2::uuid, 0, 0, now())
         RETURNING id::text"
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
         WHERE member_id = $1::uuid AND checked_in_at::date = CURRENT_DATE",
    )
    .bind(member_id)
    .fetch_one(&state.db)
    .await?;

    Ok(count.0 as i32)
}

async fn record_checkin(
    state: &AppState,
    member_id: &str,
    award: &Award,
    method: &str,
) -> Result<(), AppError> {
    // points_awarded is the amount actually credited (base x tier multiplier).
    // loyalty_checkins has no column for the base value, so the arithmetic is
    // preserved in the loyalty_activity row written alongside it.
    sqlx::query(
        "INSERT INTO loyalty_checkins (member_id, points_awarded, method, checked_in_at)
         VALUES ($1::uuid, $2, $3, now())",
    )
    .bind(member_id)
    .bind(award.awarded)
    .bind(method)
    .execute(&state.db)
    .await?;

    record_activity(
        state,
        member_id,
        "checkin",
        &award.audit_note("checkin"),
        award.awarded,
    )
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
         WHERE id = $2::uuid
         RETURNING points_balance",
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
        "SELECT rt.id::text AS id, rt.name, rt.points_required, rt.requires_approval, rt.reward_tag
         FROM loyalty_reward_tiers rt
         WHERE rt.program_id = $1::uuid
           AND rt.points_required <= $2
           AND NOT EXISTS (
               SELECT 1 FROM loyalty_rewards_earned re
               WHERE re.member_id = $3::uuid AND re.tier_id = rt.id
           )
         ORDER BY rt.points_required ASC",
    )
    .bind(program_id)
    .bind(new_balance)
    .bind(member_id)
    .fetch_all(&state.db)
    .await?;

    Ok(rows
        .iter()
        .map(|row| RewardTierInfo {
            id: row.get("id"),
            name: row.get("name"),
            points_required: row.get("points_required"),
            requires_approval: row.get("requires_approval"),
            reward_tag: row.get("reward_tag"),
        })
        .collect())
}

async fn create_reward(
    state: &AppState,
    member_id: &str,
    tier_id: &str,
    status: &str,
) -> Result<String, AppError> {
    let row = sqlx::query(
        "INSERT INTO loyalty_rewards_earned (member_id, tier_id, status, earned_at)
         VALUES ($1::uuid, $2::uuid, $3, now())
         RETURNING id::text",
    )
    .bind(member_id)
    .bind(tier_id)
    .bind(status)
    .fetch_one(&state.db)
    .await?;

    Ok(row.get("id"))
}

async fn apply_reward_tag(state: &AppState, contact_id: &str, tag: &str) -> Result<(), AppError> {
    // Update the contact's tags_applied or similar field
    // This depends on schema — here we update a hypothetical tags field
    sqlx::query("UPDATE contacts SET notes = COALESCE(notes, '') || $1 WHERE id = $2::uuid")
        .bind(format!("\n[Reward Tag: {}]", tag))
        .bind(contact_id)
        .execute(&state.db)
        .await?;

    Ok(())
}

async fn push_reward_notification(
    _state: &AppState,
    contact_id: &str,
    _reward_name: &str,
) -> Result<(), AppError> {
    // In production, push notification via delivery system
    // For now, this is a placeholder
    tracing::info!(
        "Reward notification would be sent for: {} (contact: {})",
        _reward_name,
        contact_id
    );
    Ok(())
}

/// Process a loyalty check-in triggered from a campaign entry (spin, raffle, etc).
/// This is the campaign-to-loyalty bridge — called by `create_entry()` when a campaign
/// has `auto_enroll_loyalty = true` and a `loyalty_program_id` set.
/// Unlike the standalone `process_checkin()`, this:
/// - Does NOT enforce daily cap (entry is already gated)
/// - Records the entry_id on the checkin for traceability
/// - Awards the campaign's configured points_per_play (not the loyalty program's default)
pub async fn process_checkin_from_entry(
    state: &AppState,
    program_id: &str,
    contact_id: &str,
    entry_id: &str,
    _source_campaign_slug: &str,
    points_to_award: i32,
) -> Result<CheckinResult, AppError> {
    // Get program config (just to validate it exists)
    let program = get_program(state, program_id).await?;

    // Find or create loyalty member
    let member_id = find_or_create_member(state, program_id, contact_id).await?;

    // Award points through the tier multiplier, then record checkin with the entry_id
    let balance_before = member_balance(state, &member_id).await?;
    let award = resolve_award(
        state,
        program_id,
        balance_before,
        points_to_award,
        program.tiers_enabled,
    )
    .await?;

    record_entry_checkin(state, &member_id, &award, "entry", entry_id).await?;

    // Update points_balance and lifetime_points with the multiplied amount
    let new_balance = update_points_balance(state, &member_id, award.awarded).await?;

    let _ = sync_member_tier(
        state,
        program_id,
        &member_id,
        new_balance as i64,
        program.tiers_enabled,
    )
    .await;

    // Check reward tier thresholds
    let newly_crossed = check_threshold_crossed(state, program_id, new_balance, &member_id).await?;

    let mut rewards_awarded = Vec::new();

    for tier in newly_crossed {
        if !tier.requires_approval {
            // Auto-approve
            let reward_id = create_reward(state, &member_id, &tier.id, "approved").await?;
            apply_reward_tag(state, contact_id, &tier.reward_tag).await?;
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
        base_points: award.base_points,
        points_awarded: award.awarded,
        multiplier: award.multiplier,
        tier_name: award.tier.as_ref().map(|t| t.name.clone()),
        new_balance,
        rewards_awarded: if !rewards_awarded.is_empty() {
            rewards_awarded
        } else {
            // Use the program's default points_per_checkin
            vec![]
        },
        milestones_triggered: fire_milestones(
            state,
            program_id,
            contact_id,
            new_balance,
            program.milestones_enabled,
        )
        .await,
    })
}

/// The campaign a loyalty program reports to.
///
/// `campaigns.loyalty_program_id` is the canonical pointer (the same one the
/// check-in handler resolves a program from); `loyalty_programs.campaign_id` is
/// the historical forward link and is NULL on the live program, so it is only a
/// fallback. Milestones are campaign-scoped, so without a campaign there is
/// nothing to evaluate.
pub async fn program_campaign_id(
    state: &AppState,
    program_id: &str,
) -> Result<Option<Uuid>, AppError> {
    let by_campaign: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM campaigns WHERE loyalty_program_id = $1::uuid \
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(program_id)
    .fetch_optional(&state.db)
    .await?;
    if by_campaign.is_some() {
        return Ok(by_campaign);
    }

    let forward: Option<Option<Uuid>> =
        sqlx::query_scalar("SELECT campaign_id FROM loyalty_programs WHERE id = $1::uuid")
            .bind(program_id)
            .fetch_optional(&state.db)
            .await?;
    Ok(forward.flatten())
}

/// Fire campaign milestones for a completed points award; returns the milestones
/// that triggered as (name, action_type).
///
/// No-op when the program has `milestones_enabled = false` or is not attached to
/// a campaign. Never fails the caller: the award is already persisted, so a
/// milestone problem must not roll it back or 500 the request — it is logged.
pub async fn fire_milestones(
    state: &AppState,
    program_id: &str,
    contact_id: &str,
    new_balance: i32,
    enabled: bool,
) -> Vec<(String, String)> {
    if !enabled {
        return Vec::new();
    }

    let campaign_id = match program_campaign_id(state, program_id).await {
        Ok(Some(id)) => id,
        Ok(None) => return Vec::new(),
        Err(e) => {
            tracing::warn!("milestone check skipped (campaign lookup failed): {}", e);
            return Vec::new();
        }
    };

    let contact_uuid = match Uuid::parse_str(contact_id) {
        Ok(u) => u,
        Err(_) => return Vec::new(),
    };

    match crate::mechanics::milestone_engine::check_milestones(
        state,
        &campaign_id,
        &contact_uuid,
        new_balance,
    )
    .await
    {
        Ok(triggered) => triggered,
        Err(e) => {
            tracing::warn!("milestone check failed: {}", e);
            Vec::new()
        }
    }
}

/// Record a checkin for an entry-based loyalty award.
async fn record_entry_checkin(
    state: &AppState,
    member_id: &str,
    award: &Award,
    method: &str,
    entry_id: &str,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO loyalty_checkins (member_id, points_awarded, method, entry_id, checked_in_at)
         VALUES ($1::uuid, $2, $3, $4, now())",
    )
    .bind(member_id)
    .bind(award.awarded)
    .bind(method)
    .bind(entry_id.parse::<uuid::Uuid>().ok())
    .execute(&state.db)
    .await?;

    record_activity(
        state,
        member_id,
        "campaign_entry",
        &award.audit_note("campaign_entry"),
        award.awarded,
    )
    .await?;

    Ok(())
}
/// Award loyalty points from an external action (earn click, referral credit, etc.).
#[allow(dead_code)]
pub async fn award_points_from_action(
    state: &AppState,
    program_id: &str,
    contact_id: &str,
    points_to_award: i32,
    action_type: &str,
    _channel_code: &str,
) -> Result<(), AppError> {
    let program = get_program(state, program_id).await?;
    let member_id = find_or_create_member(state, program_id, contact_id).await?;

    // Resolve the member's tier multiplier and credit the multiplied amount.
    let balance_before = member_balance(state, &member_id).await?;
    let award = resolve_award(
        state,
        program_id,
        balance_before,
        points_to_award,
        program.tiers_enabled,
    )
    .await?;

    // loyalty_checkins has no `notes` column — the channel code and the
    // base-vs-multiplied arithmetic go to loyalty_activity instead.
    sqlx::query(
        "INSERT INTO loyalty_checkins (member_id, points_awarded, method, checked_in_at)
         VALUES ($1::uuid, $2, $3, now())",
    )
    .bind(&member_id)
    .bind(award.awarded)
    .bind(action_type)
    .execute(&state.db)
    .await?;

    let _new_balance = update_points_balance(state, &member_id, award.awarded).await?;

    let channel_note = format!("{} channel={}", award.audit_note("earn"), _channel_code);
    let _ = record_activity(state, &member_id, "earn", &channel_note, award.awarded).await;

    let _ = sync_member_tier(
        state,
        program_id,
        &member_id,
        _new_balance as i64,
        program.tiers_enabled,
    )
    .await;

    if let Ok(newly_crossed) =
        check_threshold_crossed(state, program_id, _new_balance, &member_id).await
    {
        for tier in newly_crossed {
            if !tier.requires_approval {
                let _ = create_reward(state, &member_id, &tier.id, "approved").await;
                let _ = apply_reward_tag(state, contact_id, &tier.reward_tag).await;
            } else {
                let _ = create_reward(state, &member_id, &tier.id, "pending").await;
            }
        }
    }

    // Same hook as the check-in paths: an earn click must be able to cross a
    // milestone threshold, not just a reward-tier threshold.
    let _ = fire_milestones(
        state,
        program_id,
        contact_id,
        _new_balance,
        program.milestones_enabled,
    )
    .await;

    Ok(())
}
