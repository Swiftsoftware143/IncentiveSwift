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
/// Full flow: resolve the programme and its owner -> upsert contact -> find/create member ->
/// check daily cap -> create entry -> award points -> check thresholds -> auto-approve or
/// pending -> push delivery.
///
/// `program_slug` is resolved by the SAME resolver the public page used to render
/// (`mechanics::loyalty_checkin::resolve_public_program`), so a campaign slug and a programme slug
/// mean the same thing on both doors. A programme the console's "+ Add Program" modal creates has
/// no campaign — its own slug and its own `account_id` are all this flow needs (kanban t_b2a052c9);
/// before that, every scan of such a programme's printed QR answered 404.
pub async fn checkin(
    State(state): State<AppState>,
    Json(body): Json<CheckinBody>,
) -> Result<Json<Value>, AppError> {
    // 1. Resolve the identifier to the programme being credited and the account that owns it. The
    //    account is what the contact captured at check-in belongs to, and the `contact_tenants`
    //    link the upsert writes below is what makes them visible to that owner (kanban t_369cb159).
    let target = loyalty_checkin::resolve_public_program(&state, &body.program_slug).await?;
    let program = target.program;

    // 2. Upsert contact (shared identity row) and link it to the owning account.
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
        Some(target.account_id),
        "checkin",
    )
    .await?;

    // 3. Process checkin against the resolved programme.
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

/// Does this redemption belong to the caller's account?
///
/// The chain is `loyalty_rewards_earned -> loyalty_reward_tiers -> loyalty_programs`, and a programme
/// belongs to an account either directly (`loyalty_programs.account_id`) or through the campaign it
/// runs on (`campaigns.account_id`). Both arms are accepted because both are how a programme is
/// created; nothing else is.
async fn reward_belongs_to_account(
    db: &sqlx::PgPool,
    reward_id: &Uuid,
    account_id: &Uuid,
) -> Result<bool, AppError> {
    let owned = sqlx::query_scalar::<_, bool>(
        r#"SELECT EXISTS(
               SELECT 1
                 FROM loyalty_rewards_earned lre
                 JOIN loyalty_reward_tiers lrt ON lrt.id = lre.tier_id
                 JOIN loyalty_programs lp ON lp.id = lrt.program_id
                 LEFT JOIN campaigns c ON c.id = lp.campaign_id
                WHERE lre.id = $1
                  AND (lp.account_id = $2 OR c.account_id = $2))"#,
    )
    .bind(reward_id)
    .bind(account_id)
    .fetch_one(db)
    .await?;
    Ok(owned)
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

    let account_id = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;

    // SECURITY: knowing a reward id must not authorise the write. A redemption belongs to a
    // programme, so the caller's own account must own that programme (directly or through its
    // campaign). Graded 404, never 403 — a 403 would confirm the row exists to a stranger.
    if !reward_belongs_to_account(&state.db, &reward_id, &account_id).await? {
        return Err(AppError::NotFound("Reward not found".to_string()));
    }

    // Get reward
    let reward = loyalty::get_reward(&state.db, &reward_id).await?;

    if reward.status != "pending" {
        return Err(AppError::BadRequest(format!(
            "Reward is already {}",
            reward.status
        )));
    }

    // The actor is recorded even when the caller sends no `approved_by`: the account that
    // performed the approval IS the identity this app holds (`accounts.id` is the id a token
    // carries and the id `/auth/me` returns as `user.id`). Before, the console's button sent no
    // body, so `approved_by` stayed NULL and a redemption carried no record of who settled it.
    let approved_by = body
        .approved_by
        .and_then(|s| Uuid::parse_str(&s).ok())
        .or(Some(account_id));

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
    let mut notified = false;
    if let Some(member) = &member {
        loyalty::apply_reward_tag(&state.db, &member.contact_id, &tier.reward_tag).await?;
        // ...and TELL the member (this card). A manually-approved reward used to be settled in
        // silence: the tag went on, the console showed "approved", and the customer who earned it
        // was never sent anything. Same best-effort sender the auto-approve path uses, so the mail
        // carries the account's own logo/name and a harness recipient is withheld, not mailed.
        notified = crate::mechanics::loyalty_checkin::push_reward_notification(
            &state,
            &tier.program_id.to_string(),
            &member.contact_id.to_string(),
            &tier.name,
        )
        .await
        .unwrap_or(false);
    }

    Ok(Json(json!({
        "status": "approved",
        "reward_id": id,
        "reward_tag": tier.reward_tag,
        "notified": notified,
        "message": if member.is_none() {
            "Reward approved; the redemption has no loyalty member, so no tag was applied".to_string()
        } else if notified {
            "Reward approved, tag applied, and the reward email was sent to the member".to_string()
        } else {
            "Reward approved and tag applied; the reward email was NOT sent (no address, or the mail \
             path refused it — see the log)"
                .to_string()
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

    // Which campaign SERVES each programme, so the console can state per row whether the programme
    // is linked. The canonical link is the forward pointer `campaigns.loyalty_program_id`;
    // `loyalty_programs.campaign_id` is only the reverse one `create_program` may set. Reading the
    // reverse pointer alone labelled the live ZaarHub programme "Standalone" while a campaign
    // served it — a false statement on the operator's own row (kanban t_b2a052c9).
    let ids: Vec<Uuid> = programs.iter().map(|p| p.id).collect();
    let links: Vec<(Uuid, Option<String>)> = sqlx::query_as::<_, (Uuid, Option<String>)>(
        r#"SELECT lp.id,
                  COALESCE(
                    (SELECT c.slug FROM campaigns c
                      WHERE c.loyalty_program_id = lp.id
                      ORDER BY c.created_at DESC LIMIT 1),
                    (SELECT c2.slug FROM campaigns c2 WHERE c2.id = lp.campaign_id))
             FROM loyalty_programs lp
            WHERE lp.id = ANY($1)"#,
    )
    .bind(&ids)
    .fetch_all(&state.db)
    .await?;
    let linked: std::collections::HashMap<Uuid, Option<String>> = links.into_iter().collect();

    let programs: Vec<Value> = programs
        .into_iter()
        .map(|p| {
            let pid = p.id;
            let mut v = json!(p);
            if let Value::Object(ref mut map) = v {
                map.insert(
                    "campaign_slug".to_string(),
                    json!(linked.get(&pid).cloned().flatten()),
                );
            }
            v
        })
        .collect();

    Ok(Json(json!({ "programs": programs })))
}

/// POST /api/v1/loyalty/rewards/:id/deny — authenticated.
pub async fn deny_reward(
    State(state): State<AppState>,
    Path(id): Path<String>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let reward_id =
        Uuid::parse_str(&id).map_err(|_| AppError::BadRequest("Invalid reward ID".to_string()))?;

    let account_id = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;

    // SECURITY: knowing a reward id must not authorise the write. A redemption belongs to a
    // programme, so the caller's own account must own that programme (directly or through its
    // campaign). Graded 404, never 403 — a 403 would confirm the row exists to a stranger.
    if !reward_belongs_to_account(&state.db, &reward_id, &account_id).await? {
        return Err(AppError::NotFound("Reward not found".to_string()));
    }

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
    // The console's "+ Add Program" modal has carried these seven controls since it was written
    // and sends them on every save (`streak_bonus`, `streak_days`, `referral_bonus`,
    // `birthday_bonus`, `streak_enabled`, `tiers_enabled`, `milestones_enabled`) — but this
    // struct did not declare them, so serde dropped them and neither the INSERT nor the UPDATE
    // named their columns. Measured live 2026-11-05: a save with Streak Bonus 25 / Referral
    // Bonus 50 / Birthday Bonus 100 and the "Tiers" box ticked left the row at the DB defaults
    // (0/0/0, tiers_enabled = false) behind a green success toast, so the programme ran with
    // tiers, streaks and referrals off no matter what the operator entered.
    pub streak_bonus: Option<i32>,
    pub streak_days: Option<i32>,
    pub referral_bonus: Option<i32>,
    pub birthday_bonus: Option<i32>,
    pub streak_enabled: Option<bool>,
    pub tiers_enabled: Option<bool>,
    pub milestones_enabled: Option<bool>,
    // THREE earning rules the app already AWARDS at runtime but no owner could CONFIGURE.
    // `points_per_visit` is credited when staff record a visit, `social_share_points` when the
    // member shares, and `points_expire_days` bounds how long a balance lasts. All three columns
    // carried NOT-NULL defaults (5 / 0 / 365), so a programme LOOKED configured while the
    // operator had no control over any of them — the same shape as the seven fields above.
    pub points_per_visit: Option<i32>,
    pub social_share_points: Option<i32>,
    pub points_expire_days: Option<i32>,
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

/// The name is REQUIRED and bounded. It is not only a label: `create_program` slugifies it into the
/// identifier a printed counter QR carries. Measured live 2026-11-05, the create arm accepted a
/// 400-character name, which produced a 400-character slug and therefore a check-in link no phone
/// can hold — the row sat in the operator's own console as an unusable programme. The console's
/// form has no maxlength either, so the API is the only place this can be enforced. 120 characters
/// is generous for a business name and still leaves a slug short enough to print.
fn checked_program_name(name: &str) -> Result<&str, AppError> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(AppError::BadRequest(
            "A loyalty programme needs a name".to_string(),
        ));
    }
    if trimmed.chars().count() > 120 {
        return Err(AppError::BadRequest(
            "A loyalty programme name must be 120 characters or fewer — it becomes the QR code's \
             link, so a very long name produces a check-in URL that cannot be scanned"
                .to_string(),
        ));
    }
    Ok(trimmed)
}

/// POST /api/v1/loyalty/programs — create program.
pub async fn create_program(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<LoyaltyProgramInput>,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;

    let program_name = checked_program_name(&body.name)?;
    let id = Uuid::new_v4();
    let campaign_id = body.campaign_id.and_then(|s| Uuid::parse_str(&s).ok());

    // The QR identifier (see slugify_program_name). The UNIQUE index — not the conflict retry
    // below — is the guarantee, and the retry is why a legitimate name clash is not a 500: a
    // concurrent create of the same name can win the bare form between the attempt and the
    // INSERT, which raises 23505 on `loyalty_programs_slug_uidx`; take the next suffixed
    // candidate instead.
    let base = {
        let s = slugify_program_name(program_name);
        if s.is_empty() {
            "program".to_string()
        } else {
            // The slug travels in a printed QR; 64 bytes is plenty and keeps the link short.
            let capped: String = s.chars().take(64).collect();
            capped.trim_end_matches('-').to_string()
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
                currency_name, currency_icon, currency_color, account_id,
                streak_bonus, streak_days, referral_bonus, birthday_bonus,
                streak_enabled, tiers_enabled, milestones_enabled,
                points_per_visit, social_share_points, points_expire_days)
               VALUES ($1, $2, $3, $4, 'both', $5, $6, $7, $8, $9, $10, $11, $12,
                       COALESCE($13, 0), COALESCE($14, 7), COALESCE($15, 0), COALESCE($16, 0),
                       COALESCE($17, false), COALESCE($18, false), COALESCE($19, false),
                       COALESCE($20, 5), COALESCE($21, 0), COALESCE($22, 365))"#,
        )
        .bind(id)
        .bind(campaign_id)
        .bind(program_name)
        .bind(&slug)
        .bind(points_per_checkin)
        .bind(max_checkins_per_day)
        .bind(point_decay_days)
        .bind(is_active)
        .bind(&currency_name)
        .bind(&currency_icon)
        .bind(&currency_color)
        .bind(account_id)
        .bind(body.streak_bonus)
        .bind(body.streak_days)
        .bind(body.referral_bonus)
        .bind(body.birthday_bonus)
        .bind(body.streak_enabled)
        .bind(body.tiers_enabled)
        .bind(body.milestones_enabled)
        .bind(body.points_per_visit)
        .bind(body.social_share_points)
        .bind(body.points_expire_days)
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
    let account_id = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;

    // Ownership gate (kanban t_1fe72bf8). The SAME predicate `list_programs` reads with: the
    // explicit owner column, the campaign this programme points at (the reverse pointer), or a
    // campaign that points FORWARD at this programme (the canonical link every loyalty path
    // reads first). A programme the caller owns none of those ways gets the same 404 an absent
    // id gets — never a role bypass, because the read has none either. Without this, ANY
    // authenticated account could rename, re-price, deactivate or delete any tenant's programme
    // by id (the row carries no ownership in the statement at all).
    let existing = sqlx::query_scalar::<_, Uuid>(
        r#"SELECT lp.id FROM loyalty_programs lp
           WHERE lp.id = $1
             AND (lp.account_id = $2
                  OR EXISTS (SELECT 1 FROM campaigns c
                              WHERE c.id = lp.campaign_id AND c.account_id = $2)
                  OR EXISTS (SELECT 1 FROM campaigns fwd
                              WHERE fwd.loyalty_program_id = lp.id AND fwd.account_id = $2))"#,
    )
    .bind(program_id)
    .bind(account_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Loyalty program not found".to_string()))?;

    let _ = existing;

    let program_name = checked_program_name(&body.name)?;

    // NOTE: `slug` is deliberately NOT updated here. It is the identifier a printed counter QR
    // carries (kanban t_25e9f950), so a rename must not silently invalidate a QR already on a
    // business's counter; the lookup resolves the printed slug first and only falls back to the
    // name-derived form when that is unambiguous.
    // The predicate is bound on the UPDATE itself as well, so a foreign id cannot be written
    // even if the gate above were reached with a stale row (belt-and-braces, and the
    // `rows_affected() == 0` check below is what turns it into the 404).
    let updated = sqlx::query(
        r#"UPDATE loyalty_programs AS lp
           SET name = COALESCE($1, lp.name),
               points_per_checkin = COALESCE($2, lp.points_per_checkin),
               max_checkins_per_day = COALESCE($3, lp.max_checkins_per_day),
               point_decay_days = COALESCE($4, lp.point_decay_days),
               is_active = COALESCE($5, lp.is_active),
               currency_name = COALESCE($6, lp.currency_name),
               currency_icon = COALESCE($7, lp.currency_icon),
               currency_color = COALESCE($8, lp.currency_color),
               streak_bonus = COALESCE($9, lp.streak_bonus),
               streak_days = COALESCE($10, lp.streak_days),
               referral_bonus = COALESCE($11, lp.referral_bonus),
               birthday_bonus = COALESCE($12, lp.birthday_bonus),
               streak_enabled = COALESCE($13, lp.streak_enabled),
               tiers_enabled = COALESCE($14, lp.tiers_enabled),
               milestones_enabled = COALESCE($15, lp.milestones_enabled),
               points_per_visit = COALESCE($16, lp.points_per_visit),
               social_share_points = COALESCE($17, lp.social_share_points),
               points_expire_days = COALESCE($18, lp.points_expire_days)
           WHERE lp.id = $19
             AND (lp.account_id = $20
                  OR EXISTS (SELECT 1 FROM campaigns c
                              WHERE c.id = lp.campaign_id AND c.account_id = $20)
                  OR EXISTS (SELECT 1 FROM campaigns fwd
                              WHERE fwd.loyalty_program_id = lp.id AND fwd.account_id = $20))"#,
    )
    .bind(program_name)
    .bind(body.points_per_checkin)
    .bind(body.max_checkins_per_day)
    .bind(body.point_decay_days)
    .bind(body.is_active)
    .bind(body.currency_name)
    .bind(body.currency_icon)
    .bind(body.currency_color)
    .bind(body.streak_bonus)
    .bind(body.streak_days)
    .bind(body.referral_bonus)
    .bind(body.birthday_bonus)
    .bind(body.streak_enabled)
    .bind(body.tiers_enabled)
    .bind(body.milestones_enabled)
    .bind(body.points_per_visit)
    .bind(body.social_share_points)
    .bind(body.points_expire_days)
    .bind(program_id)
    .bind(account_id)
    .execute(&state.db)
    .await?;

    if updated.rows_affected() == 0 {
        return Err(AppError::NotFound("Loyalty program not found".to_string()));
    }

    // The Tiers switch must move the members with it (2026-11-08). The ladder writer
    // (`put_status_tiers`) re-resolves every member when the RUNGS change, but this modal flips
    // `tiers_enabled` on its own: switching Tiers OFF left each member still carrying the rung the
    // console prints, and switching it ON resolved nobody until that member checked in again.
    // Runtime truth is `tiers_enabled` — `resolve_award` returns no tier when it is off — so the
    // stored rung is brought in line here, by the same rule the check-in award applies.
    let tiers_enabled: bool = sqlx::query_scalar(
        "SELECT COALESCE(tiers_enabled, false) FROM loyalty_programs WHERE id = $1",
    )
    .bind(program_id)
    .fetch_one(&state.db)
    .await?;
    if tiers_enabled {
        sqlx::query(
            "UPDATE loyalty_members lm \
                SET tier_id = (SELECT t.id FROM loyalty_tiers t \
                                WHERE t.loyalty_program_id = $1 \
                                  AND t.min_points <= COALESCE(lm.points_balance, 0) \
                                ORDER BY t.min_points DESC LIMIT 1) \
              WHERE lm.program_id = $1",
        )
        .bind(program_id)
        .execute(&state.db)
        .await?;
    } else {
        sqlx::query("UPDATE loyalty_members SET tier_id = NULL WHERE program_id = $1")
            .bind(program_id)
            .execute(&state.db)
            .await?;
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

    let account_id = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;

    // Ownership predicate ON the statement (kanban t_1fe72bf8) — the same three arms
    // `list_programs` reads with, so a foreign id deletes nothing and answers the same 404 an
    // absent id gets. `campaigns.loyalty_program_id` is ON DELETE SET NULL, so removing a
    // programme a campaign still points at is already safe.
    let result = sqlx::query(
        r#"DELETE FROM loyalty_programs AS lp
           WHERE lp.id = $1
             AND (lp.account_id = $2
                  OR EXISTS (SELECT 1 FROM campaigns c
                              WHERE c.id = lp.campaign_id AND c.account_id = $2)
                  OR EXISTS (SELECT 1 FROM campaigns fwd
                              WHERE fwd.loyalty_program_id = lp.id AND fwd.account_id = $2))"#,
    )
    .bind(program_id)
    .bind(account_id)
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

/// The caller's own programme, or `None` when the id names a programme the caller owns by none of
/// the three arms the read family uses (kanban t_1fe72bf8): the explicit owner column, the campaign
/// this programme points at (the reverse pointer), or a campaign that points FORWARD at this
/// programme. Never a role bypass — the read has none either — so a foreign id and an absent id
/// answer the same 404 and no caller can enumerate another tenant's programmes.
///
/// A reward tier carries no owner column of its own, so every tier arm below is scoped by the
/// ownership of the PROGRAMME it hangs off (kanban t_3bc758b5).
async fn program_for_caller(
    pool: &sqlx::PgPool,
    program_id: Uuid,
    account_id: Uuid,
) -> Result<Option<Uuid>, AppError> {
    Ok(sqlx::query_scalar::<_, Uuid>(
        r#"SELECT lp.id FROM loyalty_programs lp
           WHERE lp.id = $1
             AND (lp.account_id = $2
                  OR EXISTS (SELECT 1 FROM campaigns c
                              WHERE c.id = lp.campaign_id AND c.account_id = $2)
                  OR EXISTS (SELECT 1 FROM campaigns fwd
                              WHERE fwd.loyalty_program_id = lp.id AND fwd.account_id = $2))"#,
    )
    .bind(program_id)
    .bind(account_id)
    .fetch_optional(pool)
    .await?)
}

/// POST /api/v1/loyalty/tiers — create reward tier.
pub async fn create_tier(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<RewardTierInput>,
) -> Result<Json<Value>, AppError> {
    let program_id = Uuid::parse_str(&body.program_id)
        .map_err(|_| AppError::BadRequest("Invalid program ID".to_string()))?;
    let account_id = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;

    // Ownership gate on the PARENT (kanban t_3bc758b5). `RewardTierInput` takes the programme id
    // from the BODY, so without this probe any authenticated account could insert a tier —
    // `points_required` (what a member earns) and `reward_tag` (the tag applied on approval) —
    // into ANY tenant's programme by naming its id. A foreign id and an absent id both 404.
    if program_for_caller(&state.db, program_id, account_id)
        .await?
        .is_none()
    {
        return Err(AppError::NotFound("Loyalty program not found".to_string()));
    }

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
    let account_id = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;

    // The predicate is bound ON the statement too (kanban t_3bc758b5), and deliberately as a single
    // statement: a scoped probe followed by an unscoped UPDATE would still write the foreign row.
    // A tier is writable only while its own programme is one the caller owns by one of the three
    // arms; `rows_affected() == 0` below turns a foreign (or absent) tier id into the same 404.
    let updated = sqlx::query(
        r#"UPDATE loyalty_reward_tiers AS rt
           SET name = COALESCE($1, rt.name),
               points_required = COALESCE($2, rt.points_required),
               reward_tag = COALESCE($3, rt.reward_tag),
               requires_approval = COALESCE($4, rt.requires_approval),
               sort_order = COALESCE($5, rt.sort_order),
               marketing_boost = COALESCE($6, rt.marketing_boost)
           WHERE rt.id = $7
             AND EXISTS (SELECT 1 FROM loyalty_programs lp
                          WHERE lp.id = rt.program_id
                            AND (lp.account_id = $8
                                 OR EXISTS (SELECT 1 FROM campaigns c
                                             WHERE c.id = lp.campaign_id
                                               AND c.account_id = $8)
                                 OR EXISTS (SELECT 1 FROM campaigns fwd
                                             WHERE fwd.loyalty_program_id = lp.id
                                               AND fwd.account_id = $8)))"#,
    )
    .bind(&body.name)
    .bind(body.points_required)
    .bind(&body.reward_tag)
    .bind(body.requires_approval)
    .bind(body.sort_order)
    .bind(&body.marketing_boost)
    .bind(tier_id)
    .bind(account_id)
    .execute(&state.db)
    .await?;

    if updated.rows_affected() == 0 {
        return Err(AppError::NotFound("Reward tier not found".to_string()));
    }

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
    let account_id = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;

    // Ownership predicate ON the DELETE (kanban t_3bc758b5): a foreign tier id deletes nothing and
    // answers the same 404 an absent id gets. `loyalty_rewards_earned.tier_id` is a plain FK (no
    // ON DELETE action) but only ever points at a tier of the same programme, and
    // `loyalty_milestones.bonus_reward_id` is ON DELETE SET NULL, so removing an owned tier is
    // already safe — the predicate is what keeps it to the owner's own programme.
    let result = sqlx::query(
        r#"DELETE FROM loyalty_reward_tiers AS rt
           WHERE rt.id = $1
             AND EXISTS (SELECT 1 FROM loyalty_programs lp
                          WHERE lp.id = rt.program_id
                            AND (lp.account_id = $2
                                 OR EXISTS (SELECT 1 FROM campaigns c
                                             WHERE c.id = lp.campaign_id
                                               AND c.account_id = $2)
                                 OR EXISTS (SELECT 1 FROM campaigns fwd
                                             WHERE fwd.loyalty_program_id = lp.id
                                               AND fwd.account_id = $2)))"#,
    )
    .bind(tier_id)
    .bind(account_id)
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
    let account_id = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;

    // The PARENT gate (kanban t_3bc758b5): this route was `AuthenticatedUser` + `WHERE program_id =
    // $1`, so it disclosed any tenant's tier ladder (name / points_required / reward_tag) to any
    // authenticated account that named the programme id. A foreign or absent programme id answers
    // the same 404 the write arms answer, instead of an empty 200 or another tenant's rows.
    if program_for_caller(&state.db, program_id, account_id)
        .await?
        .is_none()
    {
        return Err(AppError::NotFound("Loyalty program not found".to_string()));
    }

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

    let account_id = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;

    // Verify the programme exists AND belongs to the caller. The comment here always claimed
    // "and user has access via campaign", but the statement never checked it (kanban
    // t_1fe72bf8): any authenticated account could set or clear ANY programme's secret code —
    // the code that programme's members redeem for points. Same ownership predicate as
    // `list_programs`; a programme the caller does not own is the same 404 an absent id gets.
    let existing = sqlx::query_scalar::<_, Uuid>(
        r#"SELECT lp.id FROM loyalty_programs lp
           WHERE lp.id = $1
             AND (lp.account_id = $2
                  OR EXISTS (SELECT 1 FROM campaigns c
                              WHERE c.id = lp.campaign_id AND c.account_id = $2)
                  OR EXISTS (SELECT 1 FROM campaigns fwd
                              WHERE fwd.loyalty_program_id = lp.id AND fwd.account_id = $2))"#,
    )
    .bind(program_id)
    .bind(account_id)
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

    // Same `varchar(64)` column, on the programme screen's single-code path: an over-long code
    // reached the upsert and answered a bare 500. `check_len` passes the empty clear-code case.
    crate::security::display_name::check_len(&code, 64, "secret_code")
        .map_err(AppError::UnprocessableEntity)?;

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
/// `slug` is accepted in EVERY form the app itself produces, through the one
/// resolver the two award doors also use
/// (`mechanics::loyalty_checkin::resolve_public_program`):
///   1. a CAMPAIGN slug (`campaigns.slug`, globally unique);
///   2. a PROGRAMME slug (`loyalty_programs.slug`, UNIQUE since migration
///      `20261004_loyalty_program_slug_unique.sql` and written by
///      `create_program`) — the identifier the printed QR carries, and the ONLY
///      one a programme created from the console has (its "+ Add Program" modal
///      sends no campaign_id); and
///   3. the LEGACY name-derived slug old QRs carry
///      (`name.to_lowercase().replace(' ', "-")`, the way the retired `program_qr`
///      built them), resolved only when it identifies EXACTLY ONE programme — an
///      ambiguous string carries no tenant, so it must resolve to nothing rather
///      than guess a winner (kanban t_25e9f950: it used to pick the OLDEST row
///      across all tenants, so one tenant's QR rendered and credited another
///      tenant's programme).
///
/// The response carries `checkin_slug`: the identifier the page must POST to
/// `/loyalty/checkin` to award the points. That is the campaign's slug when a
/// campaign serves the programme (delivery and milestones keep their
/// attribution) and the programme's own slug when none does — kanban t_b2a052c9,
/// where a console-created programme's QR link answered 404 "not linked to a
/// campaign" because this endpoint refused to render without one.
pub async fn public_program(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> Result<Json<Value>, AppError> {
    let target = crate::mechanics::loyalty_checkin::resolve_public_program(&state, &slug).await?;
    let program = target.program;

    let business_name: Option<String> =
        sqlx::query_scalar("SELECT name FROM accounts WHERE id = $1")
            .bind(target.account_id)
            .fetch_optional(&state.db)
            .await?
            .flatten();

    // What the page posts to award the points. A campaign slug when one serves this programme;
    // otherwise the programme's own slug — a first-class check-in target, not a fallback.
    let checkin_slug = match &target.campaign {
        Some((_, campaign_slug, _)) => campaign_slug.clone(),
        None => program.slug.clone().ok_or_else(|| {
            AppError::NotFound(
                "That loyalty programme has no check-in link — re-save it from the console."
                    .to_string(),
            )
        })?,
    };
    let campaign_name = match &target.campaign {
        Some((_, _, name)) => name.clone(),
        None => program.name.clone(),
    };

    let mut payload = json!({
        // Kept for every existing reader/cached page; `checkin_slug` is the field to use.
        "campaign_slug": checkin_slug,
        "checkin_slug": checkin_slug,
        "campaign_name": campaign_name,
        "linked_to_campaign": target.campaign.is_some(),
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
    });

    // ── THE OWNER'S TIER LADDER, ON THE MEMBER'S OWN PAGE (2026-11-08) ──────────────────────────
    // The console lets an owner switch tiers on and define them (name, threshold, colour) and the
    // check-in path already APPLIES the tier multiplier (mechanics::loyalty_checkin) — but the page a
    // customer reaches by scanning the counter QR said nothing about tiers at all, so the ladder the
    // owner built was invisible to the only people who can climb it. Name + threshold + colour are
    // the shop's own printed poster material, not member data, so nothing private is exposed; the
    // block is absent entirely when the owner has tiers switched off (payload then unchanged).
    if program.tiers_enabled {
        let tiers: Vec<(String, i64, String)> = sqlx::query_as(
            "SELECT name, min_points, color FROM loyalty_tiers \
             WHERE loyalty_program_id = $1 ORDER BY min_points, name",
        )
        .bind(program.id)
        .fetch_all(&state.db)
        .await?;
        if !tiers.is_empty() {
            payload["tiers"] = json!(tiers
                .into_iter()
                .map(|(name, min_points, color)| json!({
                    "name": name,
                    "min_points": min_points,
                    "color": color,
                }))
                .collect::<Vec<_>>());
        }
    }

    // THE OWNER'S BRAND ON THEIR OWN COUNTER QR (2026-11-07). The check-in page is the surface a
    // CUSTOMER reaches by scanning the business's QR at the till, and it carried only the programme
    // name and the account name — no logo, no colour — while the same tenant's campaign player and
    // transactional email both open with their own identity. Same document, same read, same rules as
    // the play page's projection (`surface_handler::public_play`): `tenant_settings.email_branding`
    // through `branding::load`, whose logo pointer is already verified against the stored bytes so a
    // removed logo can never surface here as a broken image. Both fields are already public by
    // design (the logo is served by the PUBLIC `GET /api/v1/branding/logo/:tenant_id`), so nothing
    // new is exposed. Present ONLY for a tenant that configured something: an unbranded tenant's
    // payload is byte-identical to before this change.
    if let Some(b) = crate::branding::load(&state.db, target.account_id).await {
        payload["brand"] = json!({
            "name": b.brand_name,
            "color": b.effective_color(),
            "logo_url": b.logo_url,
        });
    }

    Ok(Json(payload))
}

// ── THE OWNER'S STATUS LADDER — the missing writer (2026-11-08) ───────────────────────────────────
// `loyalty_tiers` is what `mechanics::loyalty_checkin::resolve_tier` reads to decide which tier a
// member holds and what multiplier their award earns, and it is what the public check-in page now
// prints as the ladder. Until this block, NOTHING in the product could write those rows: the
// console's "Tiers" switch turned the multiplier on and the tier editor at `/loyalty/tiers` writes a
// DIFFERENT table (`loyalty_reward_tiers` — the rewards unlocked at N points). So every programme an
// owner created carried `tiers_enabled = true` with an EMPTY ladder: the switch did nothing, and the
// member page had nothing to show. Measured live 2026-11-08 on a fresh programme (0 rows).
//
// Replace-the-whole-ladder is the surface an editor needs: one PUT, one transaction, no orphan rows
// from a half-applied edit. Ownership is judged on the PROGRAMME with the same three arms every other
// loyalty read uses (own column, reverse campaign pointer, forward campaign pointer); a foreign id
// and an absent id both answer 404, so no caller can enumerate another tenant's programmes.
const MAX_STATUS_TIERS: usize = 8;
const DEFAULT_STATUS_TIER_COLOR: &str = "#94a3b8";

#[derive(Clone, Deserialize)]
pub struct StatusTierInput {
    pub name: String,
    pub min_points: i64,
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub multiplier: Option<f64>,
}

#[derive(Deserialize)]
pub struct StatusTiersBody {
    pub tiers: Vec<StatusTierInput>,
}

/// A colour reaches the member's page as a CSS value, so only a hex literal is accepted; anything
/// else falls back to the neutral grey rather than injecting an arbitrary style.
fn status_tier_color(raw: Option<&str>) -> String {
    let c = raw.unwrap_or("").trim();
    // #RGB or #RRGGBB only. The column is varchar(7), so accepting an 8-digit value here would
    // pass validation and then fail the INSERT — a 500 for input the caller was told was fine.
    let hex = (c.len() == 4 || c.len() == 7)
        && c.starts_with('#')
        && c[1..].chars().all(|ch| ch.is_ascii_hexdigit());
    if hex {
        c.to_string()
    } else {
        DEFAULT_STATUS_TIER_COLOR.to_string()
    }
}

/// GET /api/v1/loyalty/programs/:id/status-tiers — the ladder exactly as the member page sees it.
pub async fn list_status_tiers(
    State(state): State<AppState>,
    Path(id): Path<String>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let program_id =
        Uuid::parse_str(&id).map_err(|_| AppError::BadRequest("Invalid program ID".to_string()))?;
    let account_id = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;
    if program_for_caller(&state.db, program_id, account_id)
        .await?
        .is_none()
    {
        return Err(AppError::NotFound("Loyalty program not found".to_string()));
    }

    let rows: Vec<(String, String, i64, String, f64)> = sqlx::query_as(
        "SELECT id::text, name, min_points, color, multiplier::float8 \
           FROM loyalty_tiers WHERE loyalty_program_id = $1 ORDER BY min_points, name",
    )
    .bind(program_id)
    .fetch_all(&state.db)
    .await?;

    let tiers = rows
        .into_iter()
        .map(|(tier_id, name, min_points, color, multiplier)| {
            json!({
                "id": tier_id,
                "name": name,
                "min_points": min_points,
                "color": color,
                "multiplier": multiplier,
            })
        })
        .collect::<Vec<_>>();

    Ok(Json(json!({ "tiers": tiers })))
}

/// PUT /api/v1/loyalty/programs/:id/status-tiers — replace the whole ladder atomically.
pub async fn put_status_tiers(
    State(state): State<AppState>,
    Path(id): Path<String>,
    user: AuthenticatedUser,
    Json(body): Json<StatusTiersBody>,
) -> Result<Json<Value>, AppError> {
    let program_id =
        Uuid::parse_str(&id).map_err(|_| AppError::BadRequest("Invalid program ID".to_string()))?;
    let account_id = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;

    // Validate everything BEFORE the first write, so a refusal never leaves a half-saved ladder.
    if body.tiers.len() > MAX_STATUS_TIERS {
        return Err(AppError::UnprocessableEntity(format!(
            "A ladder can hold at most {MAX_STATUS_TIERS} tiers"
        )));
    }
    let mut thresholds = std::collections::HashSet::new();
    for tier in &body.tiers {
        let name = tier.name.trim();
        if name.is_empty() {
            return Err(AppError::UnprocessableEntity(
                "Every tier needs a name".to_string(),
            ));
        }
        if name.chars().count() > 40 {
            return Err(AppError::UnprocessableEntity(
                "Tier names are limited to 40 characters".to_string(),
            ));
        }
        if tier.min_points < 0 {
            return Err(AppError::UnprocessableEntity(
                "A tier's threshold cannot be negative".to_string(),
            ));
        }
        if !thresholds.insert(tier.min_points) {
            return Err(AppError::UnprocessableEntity(
                "Two tiers cannot share the same threshold".to_string(),
            ));
        }
        let m = tier.multiplier.unwrap_or(1.0);
        if !(m.is_finite() && (1.0..=10.0).contains(&m)) {
            return Err(AppError::UnprocessableEntity(
                "A tier multiplier must be between 1.0 and 10.0".to_string(),
            ));
        }
    }

    if program_for_caller(&state.db, program_id, account_id)
        .await?
        .is_none()
    {
        return Err(AppError::NotFound("Loyalty program not found".to_string()));
    }

    let mut ordered = body.tiers.clone();
    ordered.sort_by_key(|t| t.min_points);

    let mut tx = state.db.begin().await?;
    sqlx::query("DELETE FROM loyalty_tiers WHERE loyalty_program_id = $1")
        .bind(program_id)
        .execute(&mut *tx)
        .await?;
    for tier in &ordered {
        sqlx::query(
            "INSERT INTO loyalty_tiers \
                (id, loyalty_program_id, name, min_points, color, perks, multiplier) \
             VALUES ($1, $2, $3, $4, $5, '[]'::jsonb, $6::numeric)",
        )
        .bind(Uuid::new_v4())
        .bind(program_id)
        .bind(tier.name.trim())
        .bind(tier.min_points)
        .bind(status_tier_color(tier.color.as_deref()))
        .bind(tier.multiplier.unwrap_or(1.0))
        .execute(&mut *tx)
        .await?;
    }

    // Saving a ladder must not UNASSIGN the members who are already standing on it (2026-11-08).
    // `loyalty_members.tier_id` is `ON DELETE SET NULL`, and this endpoint replaces the whole row
    // set atomically, so every member who held a rung was nulled and nothing put it back until
    // that member's NEXT check-in. Live effect: an owner nudged one threshold and the Members
    // screen showed every customer as untiered. Re-resolve each member from the balance they
    // already hold, by the same rule `mechanics::loyalty_checkin::resolve_tier` applies (the
    // highest rung whose threshold the balance meets), inside the same transaction.
    let tiers_enabled: bool = sqlx::query_scalar(
        "SELECT COALESCE(tiers_enabled, false) FROM loyalty_programs WHERE id = $1",
    )
    .bind(program_id)
    .fetch_one(&mut *tx)
    .await?;
    if tiers_enabled && !ordered.is_empty() {
        sqlx::query(
            "UPDATE loyalty_members lm \
                SET tier_id = (SELECT t.id FROM loyalty_tiers t \
                                WHERE t.loyalty_program_id = $1 \
                                  AND t.min_points <= COALESCE(lm.points_balance, 0) \
                                ORDER BY t.min_points DESC LIMIT 1) \
              WHERE lm.program_id = $1",
        )
        .bind(program_id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;

    Ok(Json(json!({
        "status": "saved",
        "program_id": id,
        "tier_count": ordered.len(),
    })))
}
