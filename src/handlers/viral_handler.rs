//! Viral campaign engine ??? handlers for earn links, share links, referrals.
//!
//! Phase 1:
//!   GET /earn/{channel_code} ??? click-through earn (honor-system points) PUBLIC
//!   GET /c/{campaign_slug}?ref={code} ??? campaign share link with referral PUBLIC
//!   POST /api/v1/campaigns/{slug}/referral-codes ??? generate referral code (ADMIN)
//!   GET /api/v1/campaigns/{slug}/referral-stats ??? admin referral stats
//!   GET /api/v1/campaigns/{slug}/earn-channels ??? list earn channels
//!   POST /api/v1/campaigns/{slug}/earn-channels ??? create earn channel
//!   PATCH /earn-channels/{channel_id} ??? update earn channel
//!   DELETE /earn-channels/{channel_id} ??? delete earn channel

use crate::db::{campaigns, contacts, viral};
use crate::error::AppError;
use crate::security::auth::AuthenticatedUser;
use crate::state::AppState;
use axum::{
    extract::{Path, Query, State},
    http::HeaderMap,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Query / Body types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct ShareQuery {
    pub r#ref: Option<String>,
    pub source: Option<String>,
    pub utm_source: Option<String>,
    pub utm_medium: Option<String>,
    pub utm_campaign: Option<String>,
}

#[derive(Deserialize)]
pub struct EarnQuery {
    pub r#ref: Option<String>,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub contact_id: Option<Uuid>,
    pub utm_source: Option<String>,
    pub utm_medium: Option<String>,
    pub utm_campaign: Option<String>,
}

#[derive(Deserialize)]
pub struct CreateReferralCodeBody {
    pub contact_id: Uuid,
    pub source: Option<String>,
    pub campaign_slug: String,
}

#[derive(Deserialize)]
pub struct CreateEarnChannelBody {
    pub channel_code: String,
    pub label: String,
    pub description: Option<String>,
    pub points_per_click: i32,
    pub max_clicks_per_contact: Option<i32>,
    pub redirect_url: String,
    pub verification_type: Option<String>,
    pub expected_answer: Option<String>,
    pub verification_label: Option<String>,
    pub approval_notes: Option<String>,
}

#[derive(Deserialize)]
pub struct ReferralStatsQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Deserialize)]
pub struct VerifyEarnBody {
    pub channel_id: String,
    pub contact_id: String,
    pub answer: Option<String>,
}

// ---------------------------------------------------------------------------
// Helper: extract IP and UA from headers
// ---------------------------------------------------------------------------

fn extract_headers(headers: &HeaderMap) -> (Option<String>, Option<String>) {
    let ip = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .or_else(|| headers.get("x-real-ip").and_then(|v| v.to_str().ok()))
        .map(|s| s.to_string());

    let ua = headers
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    (ip, ua)
}

/// Resolve or create a contact from earn query params.
async fn resolve_earn_contact(
    state: &AppState,
    query: &EarnQuery,
) -> Result<Option<Uuid>, AppError> {
    if let Some(cid) = query.contact_id {
        contacts::get_contact(&state.db, &cid).await?;
        return Ok(Some(cid));
    }
    if let Some(ref email) = query.email {
        let input = contacts::ContactInput {
            first_name: None,
            last_name: None,
            email: Some(email.clone()),
            phone: None,
            website: None,
            business_name: None,
        };
        let id = contacts::upsert_contact(&state.db, &input).await?;
        return Ok(Some(id));
    }
    if let Some(ref phone) = query.phone {
        let input = contacts::ContactInput {
            first_name: None,
            last_name: None,
            email: None,
            phone: Some(phone.clone()),
            website: None,
            business_name: None,
        };
        let id = contacts::upsert_contact(&state.db, &input).await?;
        return Ok(Some(id));
    }
    Ok(None)
}

/// Credit referrer if a valid referral code is present.
async fn handle_referral_credit(
    state: &AppState,
    campaign_id: &Uuid,
    ref_code: Option<&str>,
    earning_contact_id: &Uuid,
    action_type: &str,
    campaign_config: &Value,
) -> Result<(), AppError> {
    if let Some(code) = ref_code {
        if let Ok(Some(referral)) = viral::find_referral_by_code(&state.db, campaign_id, code).await
        {
            if referral.referrer_contact_id != Some(*earning_contact_id) {
                let referral_bonus = campaign_config
                    .get("referrer_points")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(10) as i32;

                viral::log_referral_credit(
                    &state.db,
                    &referral.id,
                    referral.referrer_contact_id,
                    campaign_id,
                    None,
                    action_type,
                    referral_bonus,
                )
                .await?;

                if let Some(ref_contact_id) = referral.referrer_contact_id {
                    viral::upsert_campaign_points(
                        &state.db,
                        campaign_id,
                        &ref_contact_id,
                        referral_bonus,
                    )
                    .await?;

                    // Milestone check for referrer
                    let current_pts =
                        viral::get_campaign_points(&state.db, campaign_id, &ref_contact_id).await?;
                    let _ = crate::mechanics::milestone_engine::check_milestones(
                        state,
                        campaign_id,
                        &ref_contact_id,
                        current_pts,
                    )
                    .await;
                }

                if !referral.converted {
                    viral::mark_referral_converted(&state.db, &referral.id, earning_contact_id)
                        .await?;
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// GET /earn/{channel_code}
// Public click-through earn link ??? "Honor System"
// ---------------------------------------------------------------------------

pub async fn earn_click_through(
    State(state): State<AppState>,
    Path(channel_code): Path<String>,
    headers: HeaderMap,
    Query(query): Query<EarnQuery>,
) -> Result<Json<Value>, AppError> {
    let channel = viral::get_active_channel_by_code(&state.db, &channel_code).await?;
    let campaign = campaigns::get_campaign_by_id(&state.db, &channel.campaign_id).await?;

    if campaign.status != "active" {
        return Err(AppError::Forbidden("Campaign is not active".to_string()));
    }

    let (ip, ua) = extract_headers(&headers);

    // Try to resolve contact
    let contact_id = resolve_earn_contact(&state, &query).await?;

    match contact_id {
        None => {
            // Anonymous ??? log the click but don't award points
            viral::log_earn_click(
                &state.db,
                &channel.id,
                None,
                &channel.campaign_id,
                ip.as_deref(),
                ua.as_deref(),
                None,
                query.utm_source.as_deref(),
                query.utm_medium.as_deref(),
                query.utm_campaign.as_deref(),
                0,
            )
            .await?;

            let redirect = if channel.redirect_url.is_empty() {
                format!("/play/{}", campaign.slug)
            } else {
                channel.redirect_url.clone()
            };

            Ok(Json(json!({
                "status": "logged",
                "points_awarded": 0,
                "message": "Click logged. Provide email or contact_id to earn points.",
                "redirect": redirect,
            })))
        }
        Some(cid) => {
            // Check max clicks
            if channel.max_clicks_per_contact > 0 {
                let count =
                    viral::count_contact_clicks_for_channel(&state.db, &channel.id, &cid).await?;
                if count >= channel.max_clicks_per_contact {
                    let redirect = if channel.redirect_url.is_empty() {
                        format!("/play/{}", campaign.slug)
                    } else {
                        channel.redirect_url.clone()
                    };
                    return Ok(Json(json!({
                        "status": "limit_reached",
                        "points_awarded": 0,
                        "message": "You've already earned from this channel.",
                        "redirect": redirect,
                    })));
                }
            }

            // Award points
            let points = channel.points_per_click;

            viral::log_earn_click(
                &state.db,
                &channel.id,
                Some(&cid),
                &channel.campaign_id,
                ip.as_deref(),
                ua.as_deref(),
                None,
                query.utm_source.as_deref(),
                query.utm_medium.as_deref(),
                query.utm_campaign.as_deref(),
                points,
            )
            .await?;

            // Campaign-specific points balance
            let new_balance =
                viral::upsert_campaign_points(&state.db, &channel.campaign_id, &cid, points)
                    .await?;

            // Milestone check
            let triggered_milestones = crate::mechanics::milestone_engine::check_milestones(
                &state,
                &channel.campaign_id,
                &cid,
                new_balance,
            )
            .await?;

            // Loyalty bridge: award loyalty points if linked
            if let Some(prog_id) = campaign.loyalty_program_id {
                let _ = crate::mechanics::loyalty_checkin::award_points_from_action(
                    &state,
                    &prog_id.to_string(),
                    &cid.to_string(),
                    points,
                    "earn",
                    &channel_code,
                )
                .await;
            }

            // Referral credit
            handle_referral_credit(
                &state,
                &channel.campaign_id,
                query.r#ref.as_deref(),
                &cid,
                "earn",
                &campaign.config,
            )
            .await?;

            let redirect = if channel.redirect_url.is_empty() {
                format!("/play/{}", campaign.slug)
            } else {
                channel
                    .redirect_url
                    .replace("{code}", &channel_code)
                    .replace("{points}", &points.to_string())
                    .replace("{contact_id}", &cid.to_string())
            };

            let milestone_names: Vec<&str> = triggered_milestones
                .iter()
                .map(|(n, _)| n.as_str())
                .collect();

            Ok(Json(json!({
                "status": "success",
                "points_awarded": points,
                "campaign_points_balance": new_balance,
                "redirect": redirect,
                "contact_id": cid,
                "milestones_triggered": milestone_names,
            })))
        }
    }
}

// ---------------------------------------------------------------------------
// GET /c/{campaign_slug}?ref={code}
// Campaign share link ??? public, redirects to campaign play page
// ---------------------------------------------------------------------------

pub async fn campaign_share_link(
    State(state): State<AppState>,
    Path(campaign_slug): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ShareQuery>,
) -> Result<Json<Value>, AppError> {
    let campaign = campaigns::get_campaign_by_slug(&state.db, &campaign_slug).await?;

    if campaign.status != "active" {
        return Err(AppError::Forbidden("Campaign is not active".to_string()));
    }

    // Track referral click
    if let Some(ref ref_code) = query.r#ref {
        if let Ok(Some(referral)) =
            viral::find_referral_by_code(&state.db, &campaign.id, ref_code).await
        {
            viral::increment_referral_click(&state.db, &referral.id).await?;
        }
    }

    Ok(Json(json!({
        "campaign": {
            "id": campaign.id,
            "name": campaign.name,
            "slug": campaign.slug,
            "type": campaign.r#type,
        },
        "referral_code": query.r#ref,
        "redirect": format!("/play/{}", campaign.slug),
    })))
}

// ---------------------------------------------------------------------------
// POST /api/v1/campaigns/{slug}/referral-codes
// Admin: create a referral code for a specific contact
// ---------------------------------------------------------------------------

pub async fn create_referral_code(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> Result<Json<Value>, AppError> {
    // This function causes axum Handler trait resolution to fail when body references
    // anything from db::viral module. Hypothesis: circular dependency or trait inference.
    // Using raw SQL directly as workaround.
    let campaign = campaigns::get_campaign_by_slug(&state.db, &slug).await?;
    let code = format!("REF{:08x}", uuid::Uuid::new_v4().as_u128() % 0xFFFFFFFF);
    let id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO campaign_referrals (id, campaign_id, referrer_contact_id, referral_code, source, converted, click_count, points_earned) VALUES ($1, $2, $3, $4, $5, false, 0, 0)"
    )
    .bind(id)
    .bind(campaign.id)
    .bind(None::<uuid::Uuid>)
    .bind(&code)
    .bind("admin")
    .execute(&state.db)
    .await?;
    Ok(Json(json!({
        "referral_code": code,
        "share_link": format!("/c/{}?ref={}", campaign.slug, code),
    })))
}

// ---------------------------------------------------------------------------
// GET /api/v1/campaigns/{slug}/referral-stats
// Admin: referral stats + list
// ---------------------------------------------------------------------------

pub async fn get_referral_stats(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    user: AuthenticatedUser,
    Query(query): Query<ReferralStatsQuery>,
) -> Result<Json<Value>, AppError> {
    let campaign = campaigns::get_campaign_by_slug(&state.db, &slug).await?;
    let limit = query.limit.unwrap_or(50).min(200);
    let offset = query.offset.unwrap_or(0);

    let stats = viral::get_campaign_referral_stats(&state.db, &campaign.id).await?;

    let referrals: Vec<viral::CampaignReferral> = sqlx::query_as(
        r#"SELECT id, campaign_id, referrer_contact_id, referee_contact_id,
                  referral_code, source, converted, converted_at,
                  click_count, points_earned, created_at
           FROM campaign_referrals
           WHERE campaign_id = $1
           ORDER BY created_at DESC
           LIMIT $2 OFFSET $3"#,
    )
    .bind(campaign.id)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;

    Ok(Json(json!({
        "stats": stats,
        "referrals": referrals,
        "limit": limit,
        "offset": offset,
    })))
}

// ---------------------------------------------------------------------------
// GET /api/v1/campaigns/{slug}/earn-channels
// Admin: list earn channels
// ---------------------------------------------------------------------------

pub async fn list_earn_channels(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let campaign = campaigns::get_campaign_by_slug(&state.db, &slug).await?;
    let channels = viral::list_campaign_earn_channels(&state.db, &campaign.id).await?;
    Ok(Json(json!({ "earn_channels": channels })))
}

// ---------------------------------------------------------------------------
// POST /api/v1/campaigns/{slug}/earn-channels
// Admin: create earn channel
// ---------------------------------------------------------------------------

pub async fn create_earn_channel(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    user: AuthenticatedUser,
    Json(body): Json<CreateEarnChannelBody>,
) -> Result<Json<Value>, AppError> {
    let campaign = campaigns::get_campaign_by_slug(&state.db, &slug).await?;
    let account_id = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;

    let channel_id = Uuid::new_v4();
    let code = body.channel_code.to_uppercase();

    sqlx::query(
        r#"INSERT INTO earn_channels
           (id, account_id, campaign_id, channel_code, label, description,
            points_per_click, max_clicks_per_contact, redirect_url,
            verification_type, expected_answer, verification_label, approval_notes)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)"#,
    )
    .bind(channel_id)
    .bind(account_id)
    .bind(campaign.id)
    .bind(&code)
    .bind(&body.label)
    .bind(body.description.unwrap_or_default())
    .bind(body.points_per_click)
    .bind(body.max_clicks_per_contact.unwrap_or(0))
    .bind(&body.redirect_url)
    .bind(
        body.verification_type
            .as_deref()
            .unwrap_or("auto_approve_all"),
    )
    .bind(body.expected_answer.as_deref().unwrap_or(""))
    .bind(body.verification_label.as_deref().unwrap_or(""))
    .execute(&state.db)
    .await
    .map_err(|e| {
        let msg = e.to_string();
        if msg.contains("unique") || msg.contains("duplicate") {
            AppError::BadRequest(format!("Channel code '{}' already exists", code))
        } else {
            AppError::Internal(msg)
        }
    })?;

    let channel = viral::get_active_channel_by_code(&state.db, &code).await?;

    Ok(Json(json!({
        "earn_channel": channel,
        "earn_url": format!("/earn/{}", code),
    })))
}

// ---------------------------------------------------------------------------
// PATCH /api/v1/campaigns/{slug}/earn-channels/{channel_id}
// Admin: update earn channel
// ---------------------------------------------------------------------------

pub async fn update_earn_channel(
    State(state): State<AppState>,
    Path((slug, channel_id)): Path<(String, String)>,
    user: AuthenticatedUser,
    Json(body): Json<CreateEarnChannelBody>,
) -> Result<Json<Value>, AppError> {
    let _campaign = campaigns::get_campaign_by_slug(&state.db, &slug).await?;
    let ch_id = Uuid::parse_str(&channel_id)
        .map_err(|_| AppError::BadRequest("Invalid channel ID".to_string()))?;

    let code = body.channel_code.to_uppercase();

    sqlx::query(
        r#"UPDATE earn_channels
           SET channel_code = $1, label = $2, description = $3,
               points_per_click = $4, max_clicks_per_contact = $5,
               redirect_url = $6,
               verification_type = $7, expected_answer = $8, verification_label = $9,
               approval_notes = $10,
               updated_at = now()
           WHERE id = $11"#,
    )
    .bind(&code)
    .bind(&body.label)
    .bind(body.description.unwrap_or_default())
    .bind(body.points_per_click)
    .bind(body.max_clicks_per_contact.unwrap_or(0))
    .bind(&body.redirect_url)
    .bind(
        body.verification_type
            .as_deref()
            .unwrap_or("auto_approve_all"),
    )
    .bind(body.expected_answer.as_deref().unwrap_or(""))
    .bind(body.verification_label.as_deref().unwrap_or(""))
    .bind(ch_id)
    .execute(&state.db)
    .await?;

    let channel = viral::get_active_channel_by_code(&state.db, &code).await?;

    Ok(Json(json!({ "earn_channel": channel })))
}

// ---------------------------------------------------------------------------
// DELETE /api/v1/campaigns/{slug}/earn-channels/{channel_id}
// Admin: delete earn channel
// ---------------------------------------------------------------------------
// POST /api/v1/campaigns/{slug}/earn/verify
// Public: verify and complete an earn action (for 2-step verification)
// ---------------------------------------------------------------------------

pub async fn verify_earn_action(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    Json(body): Json<VerifyEarnBody>,
) -> Result<Json<Value>, AppError> {
    let campaign = campaigns::get_campaign_by_slug(&state.db, &slug).await?;
    let channel_id = Uuid::parse_str(&body.channel_id)
        .map_err(|_| AppError::BadRequest("Invalid channel ID".to_string()))?;
    let contact_id = Uuid::parse_str(&body.contact_id)
        .map_err(|_| AppError::BadRequest("Invalid contact ID".to_string()))?;

    // Fetch the earn channel
    let channel = sqlx::query_as::<_, crate::db::viral::EarnChannel>(
        "SELECT * FROM earn_channels WHERE id = $1 AND campaign_id = $2",
    )
    .bind(channel_id)
    .bind(campaign.id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Earn channel not found".to_string()))?;

    if !channel.is_active {
        return Err(AppError::BadRequest("Earn channel is inactive".to_string()));
    }

    match channel.verification_type.as_str() {
        "auto_approve_all" => {
            // Award points immediately, no verification needed
            let cur_pts = crate::db::viral::upsert_campaign_points(
                &state.db,
                &campaign.id,
                &contact_id,
                channel.points_per_click,
            )
            .await?;

            let _ = crate::mechanics::milestone_engine::check_milestones(
                &state,
                &campaign.id,
                &contact_id,
                cur_pts,
            )
            .await;

            Ok(Json(json!({
                "verified": true,
                "verification_type": "auto_approve_all",
                "points_awarded": channel.points_per_click,
                "new_balance": cur_pts,
            })))
        }
        "auto_approve_answer" => {
            // Require exact answer match
            let expected = channel.expected_answer.unwrap_or_default().to_uppercase();
            let given = body.answer.as_deref().unwrap_or("").to_uppercase();

            if given != expected {
                return Err(AppError::BadRequest("Incorrect answer".to_string()));
            }

            let cur_pts = crate::db::viral::upsert_campaign_points(
                &state.db,
                &campaign.id,
                &contact_id,
                channel.points_per_click,
            )
            .await?;

            let _ = crate::mechanics::milestone_engine::check_milestones(
                &state,
                &campaign.id,
                &contact_id,
                cur_pts,
            )
            .await;

            Ok(Json(json!({
                "verified": true,
                "verification_type": "auto_approve_answer",
                "points_awarded": channel.points_per_click,
                "new_balance": cur_pts,
            })))
        }
        "manual_approve" => {
            // Create a pending approval ??? points not awarded until admin approves
            // For now, log the request and return pending
            let cur_pts =
                crate::db::viral::get_campaign_points(&state.db, &campaign.id, &contact_id).await?;

            Ok(Json(json!({
                "verified": false,
                "verification_type": "manual_approve",
                "points_awarded": 0,
                "new_balance": cur_pts,
                "message": "Pending manual approval",
            })))
        }
        _ => Err(AppError::BadRequest(format!(
            "Unknown verification type: {}",
            channel.verification_type
        ))),
    }
}

pub async fn delete_earn_channel(
    State(state): State<AppState>,
    Path((_slug, channel_id)): Path<(String, String)>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let ch_id = Uuid::parse_str(&channel_id)
        .map_err(|_| AppError::BadRequest("Invalid channel ID".to_string()))?;

    sqlx::query("DELETE FROM earn_channels WHERE id = $1")
        .bind(ch_id)
        .execute(&state.db)
        .await?;

    Ok(Json(json!({ "status": "deleted" })))
}

// ---------------------------------------------------------------------------
// GET /api/v1/campaigns/{slug}/leaderboard
// Public: campaign points leaderboard
// ---------------------------------------------------------------------------

pub async fn campaign_leaderboard(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    Query(query): Query<ReferralStatsQuery>,
) -> Result<Json<Value>, AppError> {
    let campaign = campaigns::get_campaign_by_slug(&state.db, &slug).await?;
    let limit = query.limit.unwrap_or(20).min(100);

    let leaderboard = viral::get_campaign_leaderboard(&state.db, &campaign.id, limit).await?;

    let mut entries = Vec::new();
    for (contact_id, campaign_id, lifetime_points, balance) in &leaderboard {
        let contact = contacts::get_contact(&state.db, contact_id).await.ok();
        let name = contact
            .as_ref()
            .map(|c| {
                format!(
                    "{} {}",
                    c.first_name.as_deref().unwrap_or(""),
                    c.last_name.as_deref().unwrap_or("")
                )
                .trim()
                .to_string()
            })
            .filter(|n| !n.is_empty());
        let email = contact.and_then(|c| c.email);
        entries.push(json!({
            "contact_id": contact_id,
            "name": name,
            "email": email,
            "lifetime_points": lifetime_points,
            "current_balance": balance,
        }));
    }

    Ok(Json(json!({
        "campaign_slug": slug,
        "leaderboard": entries,
        "count": entries.len(),
    })))
}
