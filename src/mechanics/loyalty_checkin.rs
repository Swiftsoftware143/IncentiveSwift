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

    // Fund the award from the workspace's monthly points credit before it is recorded. This is
    // the metre the dashboard promises; see `spend_credit_pool`.
    if !spend_credit_pool(state, program_id, award.awarded).await? {
        return Ok(CheckinResult::CreditPoolExhausted {
            message: "This business has used up its monthly points credit, so no points could be added right now. Please ask them to top up."
                .to_string(),
        });
    }

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
            let _ = push_reward_notification(state, program_id, contact_id, &tier.name).await;

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
    /// The workspace's paid loyalty plan has no points credit left this month, so this award
    /// cannot be funded. Distinct from `DailyCapReached`, which is the MEMBER's daily allowance;
    /// this is the BUSINESS's monthly budget.
    CreditPoolExhausted {
        message: String,
    },
}

/// Spend `points` from the workspace's monthly points credit pool — the number the customer
/// dashboard shows as "the points credit left this month" and sells as "points credits a month
/// to spend on customers' check-ins".
///
/// THE METRE WAS MISSING (measured 2026-11-08). The pool gate lived only in
/// `handlers::loyalty_v2::purchase_verify`; neither award path in this module ever read it. So a
/// workspace that bought a plan saw that figure never move while check-ins awarded without limit,
/// and a workspace whose pool was already at zero kept awarding (measured live: 10 points credited
/// against a pool of 0, `loyalty_plan_status='inactive'`).
///
/// Only an ACTIVE plan is metered: a workspace with no loyalty plan keeps exactly the behaviour it
/// had before, because whether an unpaid workspace may award at all is a pricing decision and not a
/// defect to guess at.
///
/// Returns `Ok(false)` when the pool cannot fund the award. The decrement is a single conditional
/// statement, so two concurrent check-ins can never both spend the last credit.
pub(crate) async fn spend_credit_pool(
    state: &AppState,
    program_id: &str,
    points: i32,
) -> Result<bool, AppError> {
    if points <= 0 {
        return Ok(true);
    }

    let owner = sqlx::query(
        "SELECT p.account_id::text AS account_id, a.loyalty_plan_status AS plan_status \
           FROM loyalty_programs p JOIN accounts a ON a.id = p.account_id \
          WHERE p.id = $1::uuid",
    )
    .bind(program_id)
    .fetch_optional(&state.db)
    .await?;

    let owner = match owner {
        Some(row) => row,
        None => return Ok(true),
    };

    let plan_status: Option<String> = owner.get("plan_status");
    if plan_status.as_deref() != Some("active") {
        return Ok(true);
    }

    let account_id: String = owner.get("account_id");
    let spent = sqlx::query(
        "UPDATE accounts SET zc_pool_remaining = zc_pool_remaining - $1 \
          WHERE id = $2::uuid AND loyalty_plan_status = 'active' \
            AND zc_pool_remaining >= $1",
    )
    .bind(points)
    .bind(&account_id)
    .execute(&state.db)
    .await?;

    Ok(spent.rows_affected() == 1)
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

/// A short, unambiguous referral handle for a loyalty member.
///
/// This is the handle every online-loyalty endpoint resolves a member by —
/// `POST /loyalty/online/share`, `/online/referral-click` and `GET /online/stats/:code` all do
/// `WHERE referral_code = $1` — so a member minted without one is invisible to the entire referral
/// engine. Measured 2026-11-08: `find_or_create_member` inserted rows with no `referral_code` at all,
/// which is why the whole share/referral surface had no caller that could ever work.
///
/// The alphabet drops `I`, `L`, `O`, `0` and `1` so a code read off a phone screen cannot be mistyped
/// into a different member's handle. 32^8 ≈ 1.1e12 handles: collisions are not a practical concern,
/// and the partial unique index `loyalty_members_referral_code_uniq` makes them impossible rather
/// than merely unlikely.
///
/// The RNG is created and dropped INSIDE this synchronous helper on purpose: `rand::rngs::ThreadRng`
/// is `!Send`, and the caller is an axum handler, so holding one across an `.await` would make the
/// handler's future `!Send` and surface as `Handler<_, _> is not satisfied` on the route instead of
/// as an error here (the same trap `db/viral.rs` documents).
fn member_referral_code() -> String {
    use rand::Rng;
    const ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTVWXYZ23456789";
    let mut rng = rand::thread_rng();
    (0..8)
        .map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char)
        .collect()
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

    // Create new member WITH a referral code (2026-11-08). The code is what the member's own page
    // builds their sharing link from, so it must exist from the first check-in — not be back-filled
    // later by some other screen that does not exist.
    //
    // Uniqueness is enforced by the partial unique index, not by a SELECT-then-INSERT check: two
    // concurrent first check-ins can both pass a check, but only one can win the insert. A losing
    // insert answers 23505 and we mint another handle; any other error is the real answer.
    for _ in 0..8 {
        let code = member_referral_code();
        match sqlx::query(
            "INSERT INTO loyalty_members (program_id, contact_id, points_balance, lifetime_points, member_since, referral_code)
             VALUES ($1::uuid, $2::uuid, 0, 0, now(), $3)
             RETURNING id::text",
        )
        .bind(program_id)
        .bind(contact_id)
        .bind(&code)
        .fetch_one(&state.db)
        .await
        {
            Ok(row) => return Ok(row.get("id")),
            Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("23505") => continue,
            Err(e) => return Err(e.into()),
        }
    }

    Err(AppError::Internal(
        "could not mint a unique loyalty referral code after 8 attempts".to_string(),
    ))
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
         SET points_balance = COALESCE(points_balance, 0) + $1,
             lifetime_points = COALESCE(lifetime_points, 0) + $1,
             last_checkin_at = now()
         WHERE id = $2::uuid
         RETURNING COALESCE(points_balance, 0) AS points_balance",
    )
    .bind(points)
    .bind(member_id)
    .fetch_one(&state.db)
    .await?;

    Ok(row.try_get::<i32, _>("points_balance")?)
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
    // ONE implementation (cron 2026-10-22). This local copy used to append to `contacts.notes` only,
    // so the AUTO-APPROVED path tagged nobody while the manual-approval path (db::loyalty) applied
    // the tag — the tier editor's promise held only half the time. Both now write the real
    // `contact_tags` row through db::loyalty::apply_reward_tag.
    let contact_uuid = Uuid::parse_str(contact_id)
        .map_err(|_| AppError::BadRequest("Invalid contact ID".to_string()))?;
    crate::db::loyalty::apply_reward_tag(&state.db, &contact_uuid, tag).await
}

/// Tell the member they earned a reward — a real message, not a log line.
///
/// Until this card the whole body of this function was `tracing::info!("Reward notification would
/// be sent for: …")`. Both auto-approve call sites in this module call it, so a member who crossed
/// a tier's points threshold got a `loyalty_rewards_earned` row, a tag on the contact, and silence:
/// the console showed the reward and the person who earned it never heard. `handlers::loyalty::
/// approve_reward` settles a manually-approved reward and did not notify either; it calls this now.
///
/// The message goes through the app's ONE queued/template path
/// ([`crate::delivery::sender::send_template_by_type`]) so it carries the account's own logo and name
/// exactly like every other customer-facing mail, and a fleet-harness recipient is WITHHELD at that
/// path's choke point instead of being mailed. The template row is `loyalty_reward_earned`
/// (`migrations/20261105_loyalty_reward_earned_template.sql`).
///
/// Best-effort by design, and this is why it returns `Result` it never fails on a send: a member
/// with no address, or an account with no provider configured, must never fail the check-in that
/// earned the reward. Every failure is logged with its cause.
///
/// The `Ok` value answers "did this reach the member", so a caller that reports it (the console's
/// approve action does) can never claim a send that did not happen: `true` means the message was
/// handed to the mail path (or recorded as withheld for a probe address), `false` means it was
/// skipped or refused.
pub async fn push_reward_notification(
    state: &AppState,
    program_id: &str,
    contact_id: &str,
    reward_name: &str,
) -> Result<bool, AppError> {
    // The contact row carries the address; the programme row carries the account the mail is sent
    // FOR (branding + mail server are per-account). `contacts` has no account column of its own, so
    // the programme is what scopes this send — the check-in already knows which programme it is.
    let row = sqlx::query(
        "SELECT c.email, c.first_name, p.account_id::text AS account_id, p.name AS program_name
           FROM contacts c
           JOIN loyalty_programs p ON p.id = $2::uuid
          WHERE c.id = $1::uuid",
    )
    .bind(contact_id)
    .bind(program_id)
    .fetch_optional(&state.db)
    .await?;

    let Some(row) = row else {
        tracing::warn!(
            contact = %contact_id,
            program = %program_id,
            "reward notification skipped: no contact/programme row"
        );
        return Ok(false);
    };

    let email: Option<String> = row.get("email");
    let Some(to) = email.filter(|e| !e.trim().is_empty()) else {
        tracing::info!(
            contact = %contact_id,
            reward = %reward_name,
            "reward notification skipped: the member has no email address"
        );
        return Ok(false);
    };
    let first_name: Option<String> = row.get("first_name");
    let program_name: Option<String> = row.get("program_name");
    let account_id = match Uuid::parse_str(row.get::<String, _>("account_id").as_str()) {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(contact = %contact_id, "reward notification skipped: bad account id: {e}");
            return Ok(false);
        }
    };

    let vars = serde_json::json!({
        "first_name": first_name.unwrap_or_default(),
        "reward_name": reward_name,
        "program_name": program_name.unwrap_or_default(),
    });

    match crate::delivery::sender::send_template_by_type(
        &state.db,
        account_id,
        &to,
        "loyalty_reward_earned",
        &vars,
    )
    .await
    {
        Ok(()) => {
            tracing::info!(
                to = %to,
                reward = %reward_name,
                "reward notification sent to the member who earned it"
            );
            Ok(true)
        }
        Err(e) => {
            tracing::warn!(
                to = %to,
                reward = %reward_name,
                "reward notification not sent: {e}"
            );
            Ok(false)
        }
    }
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

    // Same metre as the counter check-in: campaign-entry awards spend the same monthly credit.
    if !spend_credit_pool(state, program_id, award.awarded).await? {
        return Ok(CheckinResult::CreditPoolExhausted {
            message: "This business has used up its monthly points credit, so no points could be added right now. Please ask them to top up."
                .to_string(),
        });
    }

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
            let _ = push_reward_notification(state, program_id, contact_id, &tier.name).await;

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

/// A public check-in identifier resolved to everything the two public doors need.
pub struct PublicProgram {
    /// The programme the check-in awards (active — a paused programme is refused here).
    pub program: crate::db::loyalty::LoyaltyProgram,
    /// The account the visitor's contact is linked to and the lead belongs to.
    pub account_id: Uuid,
    /// The campaign that serves this programme, when one does: `(id, slug, name)`.
    ///
    /// A programme created from the console's "+ Add Program" modal has no campaign at all — its
    /// own `account_id` is its owner and it is a complete check-in target on its own (kanban
    /// t_b2a052c9). A campaign is an optional wrapper that adds delivery and milestones to the
    /// same points; it was never required to award them.
    pub campaign: Option<(Uuid, String, String)>,
}

/// Resolve the identifier a public check-in page carries to the programme it awards and the
/// account that owns it.
///
/// One resolver, used by every door the check-in page calls (`GET /loyalty/public/program/:slug`,
/// `POST /loyalty/checkin`, `POST /loyalty/secret-code/verify`). The page used to resolve one way
/// and the award doors another, which is exactly how a programme the console created came to render
/// its QR link as 404 "not linked to a campaign" while the campaign-linked shape worked.
///
/// Accepted forms, in order:
///   1. a CAMPAIGN slug (`campaigns.slug`, what every existing printed QR posts);
///   2. a PROGRAMME slug (`loyalty_programs.slug`, UNIQUE, the identifier `create_program` derives
///      from the programme's name and the only one a campaign-less programme has);
///   3. the LEGACY name-derived slug old QRs carry, resolved ONLY when it matches exactly one
///      programme — an ambiguous string carries no tenant, so it must resolve to nothing rather
///      than guess a winner (kanban t_25e9f950 measured one tenant's QR crediting another's
///      campaign).
pub async fn resolve_public_program(
    state: &AppState,
    identifier: &str,
) -> Result<PublicProgram, AppError> {
    // 1. A campaign slug: the campaign carries the canonical forward link to its programme
    //    (`campaigns.loyalty_program_id`) and `loyalty_programs.campaign_id` is the reverse pointer
    //    `create_program` can also set. Accept either wiring, as every other loyalty path does.
    match crate::db::campaigns::get_campaign_by_slug(&state.db, identifier).await {
        Ok(campaign) => {
            let program = match campaign.loyalty_program_id {
                Some(program_id) => crate::db::loyalty::get_program(&state.db, &program_id).await?,
                None => {
                    crate::db::loyalty::get_program_by_campaign(&state.db, &campaign.id).await?
                }
            };
            return Ok(PublicProgram {
                program,
                account_id: campaign.account_id,
                campaign: Some((campaign.id, campaign.slug, campaign.name)),
            });
        }
        Err(AppError::NotFound(_)) => {}
        Err(e) => return Err(e),
    }

    // 2. A programme's own slug — the identifier the printed counter QR carries for a programme
    //    created from the console (no campaign).
    let program = match crate::db::loyalty::find_program_by_slug(&state.db, identifier).await? {
        Some(p) => Some(p),
        None => {
            // 3. LEGACY name-derived slug, resolved only when unambiguous.
            let matches: i64 = sqlx::query_scalar(
                r#"SELECT count(*) FROM loyalty_programs
                    WHERE lower(replace(name, ' ', '-')) = lower($1)"#,
            )
            .bind(identifier)
            .fetch_one(&state.db)
            .await?;

            if matches > 1 {
                return Err(AppError::NotFound(
                    "That check-in link matches more than one business — ask the business to reprint its QR code."
                        .to_string(),
                ));
            }
            if matches == 0 {
                return Err(AppError::NotFound(
                    "No loyalty program or campaign for that link".to_string(),
                ));
            }
            let id: Uuid = sqlx::query_scalar(
                r#"SELECT id FROM loyalty_programs
                    WHERE lower(replace(name, ' ', '-')) = lower($1)
                    LIMIT 1"#,
            )
            .bind(identifier)
            .fetch_one(&state.db)
            .await?;
            crate::db::loyalty::find_program_by_id_any_state(&state.db, &id).await?
        }
    };

    let program = program.ok_or_else(|| {
        AppError::NotFound("No loyalty program or campaign for that link".to_string())
    })?;

    // A paused programme is not taking check-ins. Say so, rather than "no such link" — the business
    // paused it on purpose and the person at the counter needs to know that.
    if !program.is_active {
        return Err(AppError::NotFound(
            "This loyalty programme is paused, so it is not taking check-ins right now."
                .to_string(),
        ));
    }

    let account_id = crate::db::loyalty::program_owner_account(&state.db, &program.id)
        .await?
        .ok_or_else(|| {
            AppError::NotFound(
                "That loyalty programme is not attached to a business account yet — save it again from the console to give it an owner."
                    .to_string(),
            )
        })?;

    // The campaign that serves it, when one does: the forward pointer first (canonical), resolved
    // to its row so the check-in page can post that campaign's slug and keep delivery/milestone
    // attribution.
    let campaign = match program_campaign_id(state, &program.id.to_string()).await? {
        Some(campaign_id) => {
            let c = crate::db::campaigns::get_campaign_by_id(&state.db, &campaign_id).await?;
            Some((c.id, c.slug, c.name))
        }
        None => None,
    };

    Ok(PublicProgram {
        program,
        account_id,
        campaign,
    })
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
