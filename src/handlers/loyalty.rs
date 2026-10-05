//! Loyalty handlers — checkin, approve reward, deny reward.

use crate::db::{contacts, loyalty};
use crate::error::AppError;
use crate::mechanics::loyalty_checkin;
use crate::security::auth::AuthenticatedUser;
use crate::state::AppState;
use axum::{
    extract::{Path, Query, State},
    Json,
};
use serde::Deserialize;
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
    // 1. Resolve the campaign FIRST — the contact captured at check-in belongs to the campaign's
    //    owner, and the `contact_tenants` link the upsert writes below is what makes them visible
    //    to that owner (kanban t_369cb159).
    let campaign =
        crate::db::campaigns::get_campaign_by_slug(&state.db, &body.program_slug).await?;

    // 2. Upsert contact (shared identity row) and link it to the campaign's account.
    let contact_input = contacts::ContactInput {
        first_name: body.contact.first_name.clone(),
        last_name: body.contact.last_name.clone(),
        email: body.contact.email.clone(),
        phone: body.contact.phone.clone(),
        website: body.contact.website.clone(),
        business_name: body.contact.business_name.clone(),
    };
    let contact_id = contacts::upsert_contact(
        &state.db,
        &contact_input,
        Some(campaign.account_id),
        "checkin",
    )
    .await?;

    // 3. Resolve the loyalty program wired to this campaign. `program_slug` is
    //    a *campaign* slug: the campaigns row carries the canonical forward
    //    link (loyalty_program_id), and loyalty_programs.campaign_id is the
    //    reverse pointer, which create_program can also set. Accept either —
    //    passing the campaign id straight to get_program() (which looks up by
    //    program id) could never resolve.
    let program = match campaign.loyalty_program_id {
        Some(program_id) => loyalty::get_program(&state.db, &program_id).await?,
        None => loyalty::get_program_by_campaign(&state.db, &campaign.id).await?,
    };

    // 3. Process checkin
    let result = loyalty_checkin::process_checkin(
        &state,
        &program.id.to_string(),
        &contact_id.to_string(),
        body.method.as_deref().unwrap_or("web"),
    )
    .await?;

    // 4. Return result
    match result {
        loyalty_checkin::CheckinResult::Success {
            base_points,
            points_awarded,
            multiplier,
            tier_name,
            new_balance,
            rewards_awarded,
            milestones_triggered,
        } => Ok(Json(json!({
            "status": "ok",
            "base_points": base_points,
            "multiplier": multiplier,
            "tier": tier_name,
            "points_awarded": points_awarded,
            "new_balance": new_balance,
            "rewards_awarded": rewards_awarded.iter().map(|r| json!({
                "id": r.id,
                "name": r.name,
                "status": r.status,
            })).collect::<Vec<_>>(),
            "milestones_triggered": milestones_triggered.iter().map(|(name, action_type)| json!({
                "name": name,
                "action_type": action_type,
            })).collect::<Vec<_>>(),
        }))),
        loyalty_checkin::CheckinResult::DailyCapReached { message } => Ok(Json(json!({
            "status": "daily_cap_reached",
            "message": message,
        }))),
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
    user: AuthenticatedUser,
    Json(body): Json<ApproveBody>,
) -> Result<Json<Value>, AppError> {
    let reward_id =
        Uuid::parse_str(&id).map_err(|_| AppError::BadRequest("Invalid reward ID".to_string()))?;

    // Get reward
    let reward = loyalty::get_reward(&state.db, &reward_id).await?;

    if reward.status != "pending" {
        return Err(AppError::BadRequest(format!(
            "Reward is already {}",
            reward.status
        )));
    }

    let approved_by = body.approved_by.and_then(|s| Uuid::parse_str(&s).ok());

    // Update to approved
    loyalty::update_reward_status(&state.db, &reward_id, "approved", approved_by.as_ref()).await?;

    // Get tier info for tag
    let tier = loyalty::get_reward_tier(&state.db, &reward.tier_id).await?;

    // Get member to find contact_id.
    //
    // `reward.member_id` is NULLABLE by design (loyalty_rewards_earned.member_id — a redemption can
    // be recorded for a contact that never enrolled, see db::loyalty::RewardEarned and kanban
    // t_d6e55678), so this is a real branch and not a missing check: with no member row there is no
    // contact to tag. Before, the whole get_reward decode failed for such a row, so this route
    // answered 500; now the reward is approved with the tag skipped and the answer says so.
    let member = match reward.member_id {
        Some(member_id) => Some(loyalty::get_member(&state.db, &member_id).await?),
        None => None,
    };

    // Apply reward tag to contact
    if let Some(member) = &member {
        loyalty::apply_reward_tag(&state.db, &member.contact_id, &tier.reward_tag).await?;
    }

    Ok(Json(json!({
        "status": "approved",
        "reward_id": id,
        "reward_tag": tier.reward_tag,
        "message": if member.is_some() {
            "Reward approved and tag applied".to_string()
        } else {
            "Reward approved; the redemption has no loyalty member, so no tag was applied".to_string()
        }
    })))
}

/// GET /api/v1/loyalty/programs — list all loyalty programs for the authenticated user's account.
///
/// What changed (kanban t_e8faac56): the response now carries each programme's `slug` — the
/// identifier its printed counter QR encodes (`https://app.incentiveswift.com/loyalty-checkin/<slug>`,
/// kanban t_25e9f950) — so a tenant can actually obtain/reprint the link. Because the slug is the
/// tenant's own key, the list must be scoped to that tenant: the old
/// `WHERE c.account_id = $1 OR lp.campaign_id IS NULL` arm handed EVERY authenticated account every
/// campaign-less programme (measured on the live app: the ZaarHub programme, campaign_id NULL, was
/// listed for all 16 accounts). Ownership is now explicit — `loyalty_programs.account_id`, written
/// by `create_program` and backfilled by migration 20261005_loyalty_program_account_owner.sql — so
/// a programme the console creates (no campaign_id) is still listed for its creator and for nobody
/// else. The two campaign pointers stay as well, because that is how a programme is wired for its
/// check-in link (`campaigns.loyalty_program_id` first, the canonical one every loyalty path reads)
/// and because a row predating the owner column can still be matched through them.
pub async fn list_programs(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;

    let programs = sqlx::query_as::<_, crate::db::loyalty::LoyaltyProgram>(
        r#"SELECT lp.id, lp.campaign_id, lp.name, lp.slug, lp.recognition_method,
                  lp.points_per_checkin, lp.max_checkins_per_day,
                  lp.point_decay_days, lp.is_active, lp.created_at,
                  lp.tiers_enabled, lp.milestones_enabled, lp.streak_enabled,
                  lp.streak_bonus, lp.streak_days, lp.referral_bonus, lp.birthday_bonus,
                  lp.points_expire_days, lp.social_share_points, lp.points_per_visit,
                  lp.currency_name, lp.currency_icon, lp.currency_color
           FROM loyalty_programs lp
           LEFT JOIN campaigns c ON c.id = lp.campaign_id
           WHERE lp.account_id = $1
              OR c.account_id = $1
              OR EXISTS (SELECT 1 FROM campaigns fwd
                          WHERE fwd.loyalty_program_id = lp.id AND fwd.account_id = $1)
           ORDER BY lp.name"#,
    )
    .bind(account_id)
    .fetch_all(&state.db)
    .await?;

    Ok(Json(json!({ "programs": programs })))
}

/// GET /api/v1/loyalty/rewards — list all rewards for the account.
pub async fn list_rewards(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&user.account_id)
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
           ORDER BY re.earned_at DESC"#,
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
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let reward_id =
        Uuid::parse_str(&id).map_err(|_| AppError::BadRequest("Invalid reward ID".to_string()))?;

    // Get reward
    let reward = loyalty::get_reward(&state.db, &reward_id).await?;

    if reward.status != "pending" {
        return Err(AppError::BadRequest(format!(
            "Reward is already {}",
            reward.status
        )));
    }

    // Update to denied
    loyalty::update_reward_status(&state.db, &reward_id, "denied", None).await?;

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

/// The identifier a printed counter QR carries for a programme: the programme's name,
/// slugified. UNIQUE by migration (`loyalty_programs_slug_uidx`) and STABLE — written once by
/// `create_program` and never changed by `update_program`, because a QR already on a counter
/// encodes this exact string (see `public_program`, kanban t_25e9f950). Non-alphanumeric runs
/// collapse to a single `-`, so "Bob's Bar" and "Bob  Bar" both derive `bob-s-bar` and the
/// second programme takes the next free suffixed candidate.
fn slugify_program_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.trim().chars() {
        if ch.is_alphanumeric() {
            out.extend(ch.to_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_end_matches('-').to_string()
}

/// POST /api/v1/loyalty/programs — create program.
pub async fn create_program(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<LoyaltyProgramInput>,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;

    let id = Uuid::new_v4();
    let campaign_id = body.campaign_id.and_then(|s| Uuid::parse_str(&s).ok());

    // The QR identifier (see slugify_program_name). The UNIQUE index — not the conflict retry
    // below — is the guarantee, and the retry is why a legitimate name clash is not a 500: a
    // concurrent create of the same name can win the bare form between the attempt and the
    // INSERT, which raises 23505 on `loyalty_programs_slug_uidx`; take the next suffixed
    // candidate instead.
    let base = {
        let s = slugify_program_name(&body.name);
        if s.is_empty() {
            "program".to_string()
        } else {
            s
        }
    };
    let points_per_checkin = body.points_per_checkin.unwrap_or(10);
    let max_checkins_per_day = body.max_checkins_per_day.unwrap_or(1);
    let point_decay_days = body.point_decay_days;
    let is_active = body.is_active.unwrap_or(true);
    let currency_name = body.currency_name.unwrap_or_else(|| "Points".to_string());
    let currency_icon = body.currency_icon.unwrap_or_else(|| "⭐".to_string());
    let currency_color = body.currency_color.unwrap_or_else(|| "#0d9488".to_string());

    // The OWNER (migration 20261005_loyalty_program_account_owner.sql). `loyalty_programs` had no
    // account column, so a campaign-less programme — which is every programme the console creates,
    // its "+ Add Program" modal sends no campaign_id — had no tenant at all, and `list_programs`
    // covered that shape with an `OR lp.campaign_id IS NULL` arm that listed it for EVERY
    // authenticated account. The authenticated caller is the owner, and the read is scoped by it.
    let mut slug = base.clone();
    let mut attempt = 1u32;
    loop {
        match sqlx::query(
            r#"INSERT INTO loyalty_programs (id, campaign_id, name, slug, recognition_method,
                points_per_checkin, max_checkins_per_day, point_decay_days, is_active,
                currency_name, currency_icon, currency_color, account_id)
               VALUES ($1, $2, $3, $4, 'both', $5, $6, $7, $8, $9, $10, $11, $12)"#,
        )
        .bind(id)
        .bind(campaign_id)
        .bind(&body.name)
        .bind(&slug)
        .bind(points_per_checkin)
        .bind(max_checkins_per_day)
        .bind(point_decay_days)
        .bind(is_active)
        .bind(&currency_name)
        .bind(&currency_icon)
        .bind(&currency_color)
        .bind(account_id)
        .execute(&state.db)
        .await
        {
            Ok(_) => break,
            Err(sqlx::Error::Database(ref d))
                if d.constraint() == Some("loyalty_programs_slug_uidx") && attempt < 50 =>
            {
                attempt += 1;
                slug = format!("{}-{}", base, attempt);
            }
            Err(e) => return Err(e.into()),
        }
    }

    let program = sqlx::query_as::<_, crate::db::loyalty::LoyaltyProgram>(
        r#"SELECT id, campaign_id, name, slug, recognition_method,
                  points_per_checkin, max_checkins_per_day,
                  point_decay_days, is_active, created_at,
                  tiers_enabled, milestones_enabled, streak_enabled,
                  streak_bonus, streak_days, referral_bonus, birthday_bonus,
                  points_expire_days, social_share_points, points_per_visit,
                  currency_name, currency_icon, currency_color
           FROM loyalty_programs WHERE id = $1"#,
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
    user: AuthenticatedUser,
    Json(body): Json<LoyaltyProgramInput>,
) -> Result<Json<Value>, AppError> {
    let program_id =
        Uuid::parse_str(&id).map_err(|_| AppError::BadRequest("Invalid program ID".to_string()))?;

    // Verify program exists
    let existing = sqlx::query_scalar::<_, Uuid>("SELECT id FROM loyalty_programs WHERE id = $1")
        .bind(program_id)
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Loyalty program not found".to_string()))?;

    let _ = existing;

    // NOTE: `slug` is deliberately NOT updated here. It is the identifier a printed counter QR
    // carries (kanban t_25e9f950), so a rename must not silently invalidate a QR already on a
    // business's counter; the lookup resolves the printed slug first and only falls back to the
    // name-derived form when that is unambiguous.
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
           WHERE id = $9"#,
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
        r#"SELECT id, campaign_id, name, slug, recognition_method,
                  points_per_checkin, max_checkins_per_day,
                  point_decay_days, is_active, created_at,
                  tiers_enabled, milestones_enabled, streak_enabled,
                  streak_bonus, streak_days, referral_bonus, birthday_bonus,
                  points_expire_days, social_share_points, points_per_visit,
                  currency_name, currency_icon, currency_color
           FROM loyalty_programs WHERE id = $1"#,
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
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let program_id =
        Uuid::parse_str(&id).map_err(|_| AppError::BadRequest("Invalid program ID".to_string()))?;

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
    user: AuthenticatedUser,
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
           FROM loyalty_reward_tiers WHERE id = $1"#,
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
    user: AuthenticatedUser,
    Json(body): Json<RewardTierUpdateInput>,
) -> Result<Json<Value>, AppError> {
    let tier_id =
        Uuid::parse_str(&id).map_err(|_| AppError::BadRequest("Invalid tier ID".to_string()))?;

    sqlx::query(
        r#"UPDATE loyalty_reward_tiers
           SET name = COALESCE($1, name),
               points_required = COALESCE($2, points_required),
               reward_tag = COALESCE($3, reward_tag),
               requires_approval = COALESCE($4, requires_approval),
               sort_order = COALESCE($5, sort_order),
               marketing_boost = COALESCE($6, marketing_boost)
           WHERE id = $7"#,
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
           FROM loyalty_reward_tiers WHERE id = $1"#,
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
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let tier_id =
        Uuid::parse_str(&id).map_err(|_| AppError::BadRequest("Invalid tier ID".to_string()))?;

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
    user: AuthenticatedUser,
    Query(query): Query<TierListQuery>,
) -> Result<Json<Value>, AppError> {
    let program_id = Uuid::parse_str(&query.program_id)
        .map_err(|_| AppError::BadRequest("Invalid program ID".to_string()))?;

    let tiers = sqlx::query_as::<_, crate::db::loyalty::RewardTier>(
        r#"SELECT id, program_id, name, points_required, requires_approval,
                  reward_tag, sort_order, marketing_boost
           FROM loyalty_reward_tiers
           WHERE program_id = $1
           ORDER BY sort_order ASC, points_required ASC"#,
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
        r#"SELECT id, program_id, contact_id, member_since, last_checkin_at,
                  COALESCE(points_balance, 0) AS points_balance, COALESCE(lifetime_points, 0) AS lifetime_points
           FROM loyalty_members WHERE referral_code = $1"#,
    )
    .bind(&body.referral_code)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Member not found for referral code".to_string()))?;

    // 2. Get program config for points_per_visit
    let program = sqlx::query_as::<_, crate::db::loyalty::LoyaltyProgram>(
        r#"SELECT id, campaign_id, name, slug, recognition_method,
                  points_per_checkin, max_checkins_per_day,
                  point_decay_days, is_active, created_at,
                  tiers_enabled, milestones_enabled, streak_enabled,
                  streak_bonus, streak_days, referral_bonus, birthday_bonus,
                  points_expire_days, social_share_points, points_per_visit,
                  currency_name, currency_icon, currency_color
           FROM loyalty_programs WHERE id = $1 AND is_active = true"#,
    )
    .bind(member.program_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Loyalty program not found or not active".to_string()))?;

    // 3. Check if they already visited today (no duplicate daily visit points)
    let today_visits: i64 = sqlx::query_scalar(
        r#"SELECT COUNT(*) FROM loyalty_online_actions
           WHERE member_id = $1
             AND action_type = 'daily_visit'
             AND created_at::date = CURRENT_DATE"#,
    )
    .bind(member.id)
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

    // 3b. Resolve the member's tier multiplier on the balance they hold now
    let member_id_str = member.id.to_string();
    let award = loyalty_checkin::resolve_award(
        &state,
        &member.program_id.to_string(),
        member.points_balance as i64,
        program.points_per_visit,
        program.tiers_enabled,
    )
    .await?;

    // 4. Record the online action (base + multiplier preserved in metadata)
    let mut metadata = serde_json::Map::new();
    metadata.insert("url".to_string(), json!(body.url));
    if let Some(ref ua) = body.user_agent {
        metadata.insert("user_agent".to_string(), json!(ua));
    }
    if let Some(ref r) = body.referrer {
        metadata.insert("referrer".to_string(), json!(r));
    }
    metadata.insert("base_points".to_string(), json!(award.base_points));
    metadata.insert("multiplier".to_string(), json!(award.multiplier));
    metadata.insert(
        "tier".to_string(),
        json!(award.tier.as_ref().map(|t| t.name.clone())),
    );

    sqlx::query(
        r#"INSERT INTO loyalty_online_actions (member_id, action_type, points_earned, metadata)
           VALUES ($1, 'daily_visit', $2, $3::jsonb)"#,
    )
    .bind(member.id)
    .bind(award.awarded as i64)
    .bind(json!(metadata).to_string())
    .execute(&state.db)
    .await?;

    // 5. Award the multiplied points to balance
    sqlx::query(
        r#"UPDATE loyalty_members
           SET points_balance = COALESCE(points_balance, 0) + $1,
               lifetime_points = COALESCE(lifetime_points, 0) + $1,
               current_streak = COALESCE(current_streak, 0) + 1,
               -- reset streak if last activity was more than 1 day ago
               last_activity_date = now()
           WHERE id = $2"#,
    )
    .bind(award.awarded)
    .bind(member.id)
    .execute(&state.db)
    .await?;

    // 5b. Member-facing audit trail + denormalised tier sync
    let _ = loyalty_checkin::record_activity(
        &state,
        &member_id_str,
        "daily_visit",
        &award.audit_note("daily_visit"),
        award.awarded,
    )
    .await;

    // 6. Fetch updated member for streak & balance
    #[derive(sqlx::FromRow, serde::Serialize)]
    struct MemberStreak {
        points_balance: i32,
        current_streak: i32,
    }

    let updated = sqlx::query_as::<_, MemberStreak>(
        r#"SELECT COALESCE(points_balance, 0) AS points_balance, current_streak
           FROM loyalty_members WHERE id = $1"#,
    )
    .bind(member.id)
    .fetch_one(&state.db)
    .await?;

    // The denormalised tier column reflects the member's standing AFTER this award.
    let _ = loyalty_checkin::sync_member_tier(
        &state,
        &member.program_id.to_string(),
        &member_id_str,
        updated.points_balance as i64,
        program.tiers_enabled,
    )
    .await;

    // Milestones are campaign-scoped and fired on every points award, not only
    // on check-in — a member who only ever visits daily must still reach them.
    let milestones_triggered = loyalty_checkin::fire_milestones(
        &state,
        &member.program_id.to_string(),
        &member.contact_id.to_string(),
        updated.points_balance,
        program.milestones_enabled,
    )
    .await;

    Ok(Json(json!({
        "status": "ok",
        "base_points": award.base_points,
        "multiplier": award.multiplier,
        "tier": award.tier.as_ref().map(|t| t.name.clone()),
        "points_awarded": award.awarded,
        "current_streak": updated.current_streak,
        "total_balance": updated.points_balance,
        "milestones_triggered": milestones_triggered.iter().map(|(name, action_type)| json!({
            "name": name,
            "action_type": action_type,
        })).collect::<Vec<_>>(),
    })))
}

/// POST /api/v1/loyalty/online/share — Track social share.
pub async fn online_share(
    State(state): State<AppState>,
    Json(body): Json<OnlineShareBody>,
) -> Result<Json<Value>, AppError> {
    // 1. Find member by referral code
    let member = sqlx::query_as::<_, crate::db::loyalty::LoyaltyMember>(
        r#"SELECT id, program_id, contact_id, member_since, last_checkin_at,
                  COALESCE(points_balance, 0) AS points_balance, COALESCE(lifetime_points, 0) AS lifetime_points
           FROM loyalty_members WHERE referral_code = $1"#,
    )
    .bind(&body.referral_code)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Member not found for referral code".to_string()))?;

    // 2. Get program config for social_share_points
    let program = sqlx::query_as::<_, crate::db::loyalty::LoyaltyProgram>(
        r#"SELECT id, campaign_id, name, slug, recognition_method,
                  points_per_checkin, max_checkins_per_day,
                  point_decay_days, is_active, created_at,
                  tiers_enabled, milestones_enabled, streak_enabled,
                  streak_bonus, streak_days, referral_bonus, birthday_bonus,
                  points_expire_days, social_share_points, points_per_visit,
                  currency_name, currency_icon, currency_color
           FROM loyalty_programs WHERE id = $1 AND is_active = true"#,
    )
    .bind(member.program_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Loyalty program not found or not active".to_string()))?;

    // 2b. Resolve the member's tier multiplier on the balance they hold now
    let member_id_str = member.id.to_string();
    let award = loyalty_checkin::resolve_award(
        &state,
        &member.program_id.to_string(),
        member.points_balance as i64,
        program.social_share_points,
        program.tiers_enabled,
    )
    .await?;

    // 3. Record the action (base + multiplier preserved in metadata)
    let metadata = json!({
        "platform": body.platform,
        "url": body.url,
        "base_points": award.base_points,
        "multiplier": award.multiplier,
        "tier": award.tier.as_ref().map(|t| t.name.clone()),
    });

    sqlx::query(
        r#"INSERT INTO loyalty_online_actions (member_id, action_type, points_earned, metadata)
           VALUES ($1, 'social_share', $2, $3::jsonb)"#,
    )
    .bind(member.id)
    .bind(award.awarded as i64)
    .bind(metadata.to_string())
    .execute(&state.db)
    .await?;

    // 4. Award the multiplied points
    sqlx::query(
        r#"UPDATE loyalty_members
           SET points_balance = COALESCE(points_balance, 0) + $1,
               lifetime_points = COALESCE(lifetime_points, 0) + $1
           WHERE id = $2"#,
    )
    .bind(award.awarded)
    .bind(member.id)
    .execute(&state.db)
    .await?;

    // 4b. Member-facing audit trail + denormalised tier sync
    let _ = loyalty_checkin::record_activity(
        &state,
        &member_id_str,
        "social_share",
        &award.audit_note("social_share"),
        award.awarded,
    )
    .await;

    // 5. Get updated balance
    let new_balance: i32 = sqlx::query_scalar(
        r#"SELECT COALESCE(points_balance, 0) FROM loyalty_members WHERE id = $1"#,
    )
    .bind(member.id)
    .fetch_one(&state.db)
    .await?;

    // The denormalised tier column reflects the member's standing AFTER this award.
    let _ = loyalty_checkin::sync_member_tier(
        &state,
        &member.program_id.to_string(),
        &member_id_str,
        new_balance as i64,
        program.tiers_enabled,
    )
    .await;

    let milestones_triggered = loyalty_checkin::fire_milestones(
        &state,
        &member.program_id.to_string(),
        &member.contact_id.to_string(),
        new_balance,
        program.milestones_enabled,
    )
    .await;

    Ok(Json(json!({
        "status": "ok",
        "base_points": award.base_points,
        "multiplier": award.multiplier,
        "tier": award.tier.as_ref().map(|t| t.name.clone()),
        "points_awarded": award.awarded,
        "total_balance": new_balance,
        "milestones_triggered": milestones_triggered.iter().map(|(name, action_type)| json!({
            "name": name,
            "action_type": action_type,
        })).collect::<Vec<_>>(),
    })))
}

/// POST /api/v1/loyalty/online/referral-click — Track referral link click.
pub async fn referral_click(
    State(state): State<AppState>,
    Json(body): Json<ReferralClickBody>,
) -> Result<Json<Value>, AppError> {
    // 1. Find member by referral code (the referrer)
    let member = sqlx::query_as::<_, crate::db::loyalty::LoyaltyMember>(
        r#"SELECT id, program_id, contact_id, member_since, last_checkin_at,
                  COALESCE(points_balance, 0) AS points_balance, COALESCE(lifetime_points, 0) AS lifetime_points
           FROM loyalty_members WHERE referral_code = $1"#,
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
           VALUES ($1, 'referral_click', 0, $2::jsonb)"#,
    )
    .bind(member.id)
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
        r#"SELECT id, program_id, contact_id, member_since, last_checkin_at,
                  COALESCE(points_balance, 0) AS points_balance, COALESCE(lifetime_points, 0) AS lifetime_points
           FROM loyalty_members WHERE referral_code = $1"#,
    )
    .bind(&code)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Member not found for referral code".to_string()))?;

    // 2. Get current streak
    let current_streak: i32 =
        sqlx::query_scalar("SELECT current_streak FROM loyalty_members WHERE id = $1")
            .bind(member.id)
            .fetch_one(&state.db)
            .await?;

    // 3. Count daily visits
    let total_visits: i64 = sqlx::query_scalar(
        r#"SELECT COUNT(*) FROM loyalty_online_actions
           WHERE member_id = $1 AND action_type = 'daily_visit'"#,
    )
    .bind(member.id)
    .fetch_one(&state.db)
    .await?;

    // 4. Count social shares
    let total_shares: i64 = sqlx::query_scalar(
        r#"SELECT COUNT(*) FROM loyalty_online_actions
           WHERE member_id = $1 AND action_type = 'social_share'"#,
    )
    .bind(member.id)
    .fetch_one(&state.db)
    .await?;

    // 5. Count referral clicks
    let referral_clicks: i64 = sqlx::query_scalar(
        r#"SELECT COUNT(*) FROM loyalty_online_actions
           WHERE member_id = $1 AND action_type = 'referral_click'"#,
    )
    .bind(member.id)
    .fetch_one(&state.db)
    .await?;

    // 6. Count total referrals
    let total_referrals: i32 =
        sqlx::query_scalar("SELECT total_referrals FROM loyalty_members WHERE id = $1")
            .bind(member.id)
            .fetch_one(&state.db)
            .await?;

    // 7. Sum points earned from online actions
    let online_points: i64 = sqlx::query_scalar(
        r#"SELECT COALESCE(SUM(points_earned), 0)::bigint FROM loyalty_online_actions
           WHERE member_id = $1"#,
    )
    .bind(member.id)
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
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;

    // The account's plan tier. `tenants` (the platform tenant: id/name/slug) never had a
    // `plan_tier` column — the tier an account is on is accounts.plan_tier_id -> plan_tiers,
    // which is exactly how features.rs resolves it (plain-statement drift, kanban t_cf7469bb).
    let tier_row: Option<(Uuid, String)> = sqlx::query_as(
        "SELECT p.id, p.slug FROM accounts a
           JOIN plan_tiers p ON p.id = a.plan_tier_id
          WHERE a.id = $1",
    )
    .bind(account_id)
    .fetch_optional(&state.db)
    .await?;

    let (tier_id, tier) = match &tier_row {
        Some((id, slug)) => (Some(*id), slug.clone()),
        None => (None, "free".to_string()),
    };

    // Entitlements live in `tier_features` — features.rs is the single source of truth and the
    // `feature_limits` table this used to read does not exist. Same convention as
    // features::enforce_feature_limit(): a feature with no row for the tier is simply not
    // configured, which allows it; only an explicit `enabled = false` turns loyalty off.
    let enabled = match tier_id {
        None => true,
        Some(id) => sqlx::query_scalar::<_, bool>(
            // COALESCE to the column's own DB DEFAULT (true): 'absent' and 'NULL' both mean
            // "not configured", which allows the feature (kanban t_310c6e66).
            "SELECT COALESCE(tf.enabled, true) FROM tier_features tf
               JOIN features f ON f.id = tf.feature_id
              WHERE tf.tier_id = $1 AND f.key = 'module_loyalty_program'",
        )
        .bind(id)
        .fetch_optional(&state.db)
        .await?
        .unwrap_or(true),
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
    user: AuthenticatedUser,
    Json(body): Json<SetSecretCodeInput>,
) -> Result<Json<Value>, AppError> {
    let program_id =
        Uuid::parse_str(&id).map_err(|_| AppError::BadRequest("Invalid program ID".to_string()))?;

    // Verify program exists and user has access via campaign
    let existing = sqlx::query_scalar::<_, Uuid>(
        r#"SELECT lp.id FROM loyalty_programs lp
           LEFT JOIN campaigns c ON c.id = lp.campaign_id
           WHERE lp.id = $1"#,
    )
    .bind(program_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Program not found".to_string()))?;
    let _ = existing;

    // `loyalty_programs` has no secret_code / secret_code_points columns. Codes live in
    // `loyalty_secret_codes` (program_id, code, points_reward, is_active) — the table
    // secret_codes_handler.rs serves and the redemption path reads — so this writes there
    // (plain-statement drift, kanban t_cf7469bb). Setting a code retires the program's previous
    // live code, so "the secret code" stays singular; clearing deactivates without inserting.
    let code = body
        .secret_code
        .as_deref()
        .unwrap_or("")
        .trim()
        .to_uppercase();

    if code.is_empty() {
        sqlx::query(
            "UPDATE loyalty_secret_codes SET is_active = false
              WHERE program_id = $1 AND is_active = true",
        )
        .bind(program_id)
        .execute(&state.db)
        .await?;
    } else {
        // Retire any OTHER live code first, then upsert this one: (program_id, code) is unique, so
        // re-setting the same code has to reactivate the existing row instead of inserting it.
        sqlx::query(
            "UPDATE loyalty_secret_codes SET is_active = false
              WHERE program_id = $1 AND is_active = true AND UPPER(code) <> UPPER($2)",
        )
        .bind(program_id)
        .bind(&code)
        .execute(&state.db)
        .await?;

        sqlx::query(
            "INSERT INTO loyalty_secret_codes (id, program_id, code, points_reward, is_active, created_by)
             VALUES ($1, $2, $3, $4, true, $5)
             ON CONFLICT (program_id, code) DO UPDATE
                SET points_reward = EXCLUDED.points_reward, is_active = true",
        )
        .bind(Uuid::new_v4())
        .bind(program_id)
        .bind(&code)
        .bind(body.secret_code_points.unwrap_or(25))
        .bind(Uuid::parse_str(&user.account_id).ok())
        .execute(&state.db)
        .await?;
    }

    Ok(Json(json!({
        "status": "ok",
        "message": if body.secret_code.is_some() && !body.secret_code.as_ref().unwrap().is_empty() {
            "Secret code set. Members can now enter this code to earn points."
        } else {
            "Secret code cleared."
        }
    })))
}

/// GET /api/v1/loyalty/public/program/:slug — PUBLIC, no auth, no API key.
///
/// The QR on a business's counter encodes
/// `https://app.incentiveswift.com/loyalty-checkin/<slug>` and the person who
/// scans it has no account yet, so this lookup cannot be authenticated. It
/// answers with only what that page renders — never a member balance, a contact
/// or a secret — and returns 404 for anything unknown.
///
/// `slug` is accepted in BOTH forms the app itself produces:
///   1. a CAMPAIGN slug (`campaigns.slug`, globally unique) — what the
///      check-in endpoint itself resolves; and
///   2. a PROGRAMME slug (`loyalty_programs.slug`, UNIQUE since migration
///      `20261004_loyalty_program_slug_unique.sql` and written by
///      `create_program`) — the identifier the printed QR carries.
/// The response always carries the campaign slug, so a page loaded from either
/// form can still complete a check-in.
///
/// A third, LEGACY form is still honoured: the name-derived slug old QRs carry
/// (`name.to_lowercase().replace(' ', "-")`, the way the retired `program_qr`
/// built them). It resolves only when it identifies EXACTLY ONE programme —
/// an ambiguous string carries no tenant, so it must resolve to nothing rather
/// than guess a winner (kanban t_25e9f950: it used to pick the OLDEST row
/// across all tenants, so one tenant's QR rendered and credited another
/// tenant's programme).
pub async fn public_program(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> Result<Json<Value>, AppError> {
    let campaign = match crate::db::campaigns::get_campaign_by_slug(&state.db, &slug).await {
        Ok(c) => c,
        Err(AppError::NotFound(_)) => {
            // 1. The identifier the printed QR carries for a programme: `loyalty_programs.slug`,
            //    UNIQUE since migration 20261004_loyalty_program_slug_unique.sql and written by
            //    create_program below. One programme = one account, so this arm is tenant-scoped
            //    by construction.
            let by_slug: Option<Uuid> =
                sqlx::query_scalar("SELECT id FROM loyalty_programs WHERE slug = $1")
                    .bind(&slug)
                    .fetch_optional(&state.db)
                    .await?;

            let program_id = match by_slug {
                Some(id) => id,
                None => {
                    // 2. LEGACY form: the programme-name slug older QRs carry, spelled exactly
                    //    the way the retired program_qr derived it. Resolve it ONLY when it
                    //    identifies exactly ONE programme.
                    //
                    //    This arm used to be `... ORDER BY created_at ASC LIMIT 1` with no
                    //    account predicate, so two tenants with the same programme name BOTH
                    //    resolved to the OLDER tenant's programme — measured live (kanban
                    //    t_25e9f950): tenant B's QR rendered business_name=ProbeCollideA and
                    //    currency_name=AAA-CURRENCY, then POSTed the returned campaign slug to
                    //    /loyalty/checkin, crediting tenant A's campaign. A string that matches
                    //    more than one programme is genuinely ambiguous — the QR itself carries
                    //    no tenant — so it must resolve to NOTHING. Refusing beats guessing
                    //    wrong; the business reprints a QR carrying its own unique slug.
                    let matches: i64 = sqlx::query_scalar(
                        r#"SELECT count(*) FROM loyalty_programs
                            WHERE lower(replace(name, ' ', '-')) = lower($1)"#,
                    )
                    .bind(&slug)
                    .fetch_one(&state.db)
                    .await?;

                    if matches > 1 {
                        return Err(AppError::NotFound(
                            "That check-in link matches more than one business — ask the business to reprint its QR code."
                                .to_string(),
                        ));
                    }

                    sqlx::query_scalar(
                        r#"SELECT id FROM loyalty_programs
                            WHERE lower(replace(name, ' ', '-')) = lower($1)
                            LIMIT 1"#,
                    )
                    .bind(&slug)
                    .fetch_optional(&state.db)
                    .await?
                    .ok_or_else(|| {
                        AppError::NotFound(
                            "No loyalty program or campaign for that link".to_string(),
                        )
                    })?
                }
            };
            // Which campaign serves this programme? Resolve it the way EVERY other loyalty
            // path already does — the canonical forward link `campaigns.loyalty_program_id`
            // first, and only then the historical reverse pointer `loyalty_programs.campaign_id`
            // (see handlers/loyalty.rs::checkin, handlers/secret_codes_handler.rs, and
            // mechanics/loyalty_checkin.rs::program_campaign_id, which this reuses).
            //
            // Reading the reverse pointer ALONE — as this arm used to — answered 404 to the
            // programme-name slug that `program_qr` itself writes into the printed counter QR
            // for every programme the console creates, because `create_program` only sets that
            // pointer when the caller passes a campaign_id, while the console wires the link
            // the canonical way (the campaign editor). Measured live (kanban t_2968cb33).
            let campaign_id = crate::mechanics::loyalty_checkin::program_campaign_id(
                &state,
                &program_id.to_string(),
            )
            .await?
            .ok_or_else(|| {
                AppError::NotFound("That program is not linked to a campaign".to_string())
            })?;
            crate::db::campaigns::get_campaign_by_id(&state.db, &campaign_id).await?
        }
        Err(e) => return Err(e),
    };

    let program = match campaign.loyalty_program_id {
        Some(program_id) => loyalty::get_program(&state.db, &program_id).await?,
        None => loyalty::get_program_by_campaign(&state.db, &campaign.id).await?,
    };

    let business_name: Option<String> =
        sqlx::query_scalar("SELECT name FROM accounts WHERE id = $1")
            .bind(campaign.account_id)
            .fetch_optional(&state.db)
            .await?
            .flatten();

    Ok(Json(json!({
        "campaign_slug": campaign.slug,
        "campaign_name": campaign.name,
        "business_name": business_name,
        "program_name": program.name,
        "points_per_checkin": program.points_per_checkin,
        "points_per_visit": program.points_per_visit,
        "currency_name": program.currency_name,
        "currency_icon": program.currency_icon,
        "currency_color": program.currency_color,
        "is_active": program.is_active,
        "streak_enabled": program.streak_enabled,
        "tiers_enabled": program.tiers_enabled,
        "milestones_enabled": program.milestones_enabled,
    })))
}
