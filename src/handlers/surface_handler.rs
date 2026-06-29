//! Surface handlers — widget, tablet, play, embed views, and domain management.

use crate::error::AppError;
use crate::state::AppState;
use crate::security::auth::AuthenticatedUser;
use axum::{
    extract::{Path, State},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

/// A widget snippet record.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct WidgetSnippet {
    pub id: Uuid,
    pub campaign_id: Uuid,
    pub snippet_hash: String,
    pub is_active: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// A tablet session record.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct TabletSession {
    pub id: Uuid,
    pub campaign_id: Uuid,
    pub tenant_id: Uuid,
    pub device_id: Option<String>,
    pub interaction_count: i32,
    pub last_interaction_at: Option<chrono::DateTime<chrono::Utc>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// A custom domain record.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct CustomDomain {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub domain: String,
    pub target_type: String,
    pub verification_token: String,
    pub verified_at: Option<chrono::DateTime<chrono::Utc>>,
    pub ssl_provisioned_at: Option<chrono::DateTime<chrono::Utc>>,
    pub is_active: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// A loyalty member record for the dashboard.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct LoyaltyMemberRow {
    pub id: Uuid,
    pub contact_id: Option<Uuid>,
    pub points_balance: i32,
    pub lifetime_points: i32,
    pub member_since: chrono::DateTime<chrono::Utc>,
    pub last_checkin_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// A reward tier record for the dashboard.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct RewardTierRow {
    pub id: Uuid,
    pub name: String,
    pub points_required: i32,
    pub requires_approval: bool,
    pub reward_tag: String,
    pub sort_order: i32,
}

/// Input for registering a domain.
#[derive(Deserialize)]
pub struct RegisterDomainInput {
    pub domain: String,
    pub target_type: Option<String>,
}

/// Input for updating surface config.
#[derive(Deserialize)]
pub struct UpdateSurfaceConfigInput {
    pub surface_config: Value,
}

/// Public widget — basic JS snippet for display
const WIDGET_JS_TEMPLATE: &str = r#"(function() {
    var s = document.createElement('script');
    s.src = 'WIDGET_URL';
    s.async = true;
    s.setAttribute('data-campaign-hash', 'HASH');
    document.head.appendChild(s);
    console.log('IncentiveSwift widget loaded for campaign hash: HASH');
})();
"#;

/// GET /api/v1/widget/{hash}
/// Returns a JavaScript snippet for embedding the widget.
pub async fn get_widget_js(
    State(state): State<AppState>,
    Path(hash): Path<String>,
) -> Result<Json<Value>, AppError> {
    let snippet = sqlx::query_as::<_, WidgetSnippet>(
        r#"SELECT id, campaign_id, snippet_hash, is_active, created_at
           FROM widget_snippets WHERE snippet_hash = $1 AND is_active = true"#
    )
    .bind(&hash)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Widget snippet not found".to_string()))?;

    let widget_url = format!("/api/v1/widget/{}/config", &hash);
    let js = WIDGET_JS_TEMPLATE
        .replace("WIDGET_URL", &widget_url)
        .replace("HASH", &hash);

    Ok(Json(json!({
        "hash": hash,
        "campaign_id": snippet.campaign_id,
        "javascript": js,
        "status": "active",
    })))
}

/// GET /api/v1/widget/{hash}/config
/// Returns the widget configuration for a campaign.
pub async fn get_widget_config(
    State(state): State<AppState>,
    Path(hash): Path<String>,
) -> Result<Json<Value>, AppError> {
    let snippet = sqlx::query_as::<_, WidgetSnippet>(
        r#"SELECT ws.id, ws.campaign_id, ws.snippet_hash, ws.is_active, ws.created_at
           FROM widget_snippets ws WHERE ws.snippet_hash = $1 AND ws.is_active = true"#
    )
    .bind(&hash)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Widget snippet not found".to_string()))?;

    // Get the campaign config for widget settings
    let campaign = sqlx::query(
        r#"SELECT id, name, slug, type, config, surface_config, outcome_tags
           FROM campaigns WHERE id = $1"#
    )
    .bind(snippet.campaign_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Campaign not found".to_string()))?;

    let campaign_id: Uuid = campaign.get("id");
    let name: String = campaign.get("name");
    let slug: String = campaign.get("slug");
    let campaign_type: String = campaign.get("type");
    let config: Value = campaign.get("config");
    let surface_config: Value = campaign.get("surface_config");
    let outcome_tags: Value = campaign.get("outcome_tags");

    Ok(Json(json!({
        "campaign": {
            "id": campaign_id,
            "name": name,
            "slug": slug,
            "type": campaign_type,
        },
        "config": config,
        "surface_config": surface_config,
        "outcome_tags": outcome_tags,
    })))
}

/// GET /api/v1/tablet/{id}
pub async fn get_tablet_view(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    let session_id = Uuid::parse_str(&id)
        .map_err(|_| AppError::BadRequest("Invalid tablet session ID".to_string()))?;

    let session = sqlx::query_as::<_, TabletSession>(
        r#"SELECT id, campaign_id, tenant_id, device_id, interaction_count,
                  last_interaction_at, created_at
           FROM tablet_sessions WHERE id = $1"#
    )
    .bind(session_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Tablet session not found".to_string()))?;

    // Get campaign info for the tablet view
    let campaign = sqlx::query(
        r#"SELECT name, slug, type, config, surface_config
           FROM campaigns WHERE id = $1"#
    )
    .bind(session.campaign_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Campaign not found".to_string()))?;

    let campaign_name: String = campaign.get("name");
    let campaign_slug: String = campaign.get("slug");
    let campaign_type: String = campaign.get("type");
    let campaign_config: Value = campaign.get("config");
    let surface_config: Value = campaign.get("surface_config");

    Ok(Json(json!({
        "session": session,
        "campaign": {
            "name": campaign_name,
            "slug": campaign_slug,
            "type": campaign_type,
        },
        "config": campaign_config,
        "surface_config": surface_config,
    })))
}

/// POST /api/v1/tablet/{id}/interact
pub async fn tablet_interaction(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    let session_id = Uuid::parse_str(&id)
        .map_err(|_| AppError::BadRequest("Invalid tablet session ID".to_string()))?;

    let result = sqlx::query(
        r#"UPDATE tablet_sessions
           SET interaction_count = interaction_count + 1, last_interaction_at = now()
           WHERE id = $1"#
    )
    .bind(session_id)
    .execute(&state.db)
    .await?;

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound("Tablet session not found".to_string()));
    }

    let session = sqlx::query_as::<_, TabletSession>(
        r#"SELECT id, campaign_id, tenant_id, device_id, interaction_count,
                  last_interaction_at, created_at
           FROM tablet_sessions WHERE id = $1"#
    )
    .bind(session_id)
    .fetch_one(&state.db)
    .await?;

    Ok(Json(json!({
        "status": "recorded",
        "session": session,
    })))
}

/// GET /api/v1/play/{id}
pub async fn get_play_view(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    let campaign_id = Uuid::parse_str(&id)
        .map_err(|_| AppError::BadRequest("Invalid campaign ID".to_string()))?;

    let campaign = sqlx::query(
        r#"SELECT id, name, slug, type, status, config, surface_config,
                  tag_namespace, outcome_tags, delivery_method, delivery_config, created_at
           FROM campaigns WHERE id = $1 AND status = 'active'"#
    )
    .bind(campaign_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Active campaign not found".to_string()))?;

    let cid: Uuid = campaign.get("id");
    let name: String = campaign.get("name");
    let slug: String = campaign.get("slug");
    let campaign_type: String = campaign.get("type");
    let status: String = campaign.get("status");
    let config: Value = campaign.get("config");
    let surface_config: Value = campaign.get("surface_config");
    let tag_namespace: String = campaign.get("tag_namespace");
    let outcome_tags: Value = campaign.get("outcome_tags");
    let delivery_method: String = campaign.get("delivery_method");
    let delivery_config: Value = campaign.get("delivery_config");
    let created_at: chrono::DateTime<chrono::Utc> = campaign.get("created_at");

    Ok(Json(json!({
        "campaign": {
            "id": cid,
            "name": name,
            "slug": slug,
            "type": campaign_type,
            "status": status,
            "tag_namespace": tag_namespace,
            "created_at": created_at,
        },
        "config": config,
        "surface_config": surface_config,
        "outcome_tags": outcome_tags,
        "delivery_method": delivery_method,
        "delivery_config": delivery_config,
    })))
}

/// GET /api/v1/play/{id}/dashboard
pub async fn get_loyalty_dashboard(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    let campaign_id = Uuid::parse_str(&id)
        .map_err(|_| AppError::BadRequest("Invalid campaign ID".to_string()))?;

    // Get the loyalty program for this campaign
    let program_row = sqlx::query(
        r#"SELECT id, name, recognition_method, points_per_checkin,
                  max_checkins_per_day, point_decay_days, is_active
           FROM loyalty_programs WHERE campaign_id = $1 AND is_active = true"#
    )
    .bind(campaign_id)
    .fetch_optional(&state.db)
    .await?;

    let program_data = match program_row {
        Some(p) => {
            let program_id: Uuid = p.get("id");
            let program_name: String = p.get("name");
            let recognition_method: String = p.get("recognition_method");
            let points_per_checkin: i32 = p.get("points_per_checkin");
            let max_checkins_per_day: i32 = p.get("max_checkins_per_day");

            // Get reward tiers
            let reward_tiers: Vec<RewardTierRow> = sqlx::query_as(
                r#"SELECT id, name, points_required, requires_approval, reward_tag, sort_order
                   FROM loyalty_reward_tiers WHERE program_id = $1
                   ORDER BY sort_order, points_required"#
            )
            .bind(program_id)
            .fetch_all(&state.db)
            .await?;

            // Get member count
            let member_count: i64 = sqlx::query_scalar::<_, Option<i64>>(
                "SELECT COUNT(*) FROM loyalty_members WHERE program_id = $1"
            )
            .bind(program_id)
            .fetch_one(&state.db)
            .await?
            .unwrap_or(0);

            // Get checkin stats
            let total_checkins: i64 = sqlx::query_scalar::<_, Option<i64>>(
                "SELECT COUNT(*) FROM loyalty_checkins lc JOIN loyalty_members lm ON lm.id = lc.member_id WHERE lm.program_id = $1"
            )
            .bind(program_id)
            .fetch_one(&state.db)
            .await?
            .unwrap_or(0);

            // Top members by points
            let top_members: Vec<LoyaltyMemberRow> = sqlx::query_as(
                r#"SELECT lm.id, lm.contact_id, lm.points_balance, lm.lifetime_points,
                          lm.member_since, lm.last_checkin_at
                   FROM loyalty_members lm
                   WHERE lm.program_id = $1
                   ORDER BY lm.points_balance DESC
                   LIMIT 10"#
            )
            .bind(program_id)
            .fetch_all(&state.db)
            .await?;

            json!({
                "id": program_id,
                "name": program_name,
                "recognition_method": recognition_method,
                "points_per_checkin": points_per_checkin,
                "max_checkins_per_day": max_checkins_per_day,
                "reward_tiers": reward_tiers,
                "member_count": member_count,
                "total_checkins": total_checkins,
                "top_members": top_members,
            })
        }
        None => json!(null),
    };

    Ok(Json(json!({
        "campaign_id": campaign_id,
        "program": program_data,
    })))
}

/// GET /api/v1/embed/{id}
pub async fn get_embed_view(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    let campaign_id = Uuid::parse_str(&id)
        .map_err(|_| AppError::BadRequest("Invalid campaign ID".to_string()))?;

    let campaign = sqlx::query(
        r#"SELECT id, name, slug, type, config, surface_config, outcome_tags
           FROM campaigns WHERE id = $1 AND status = 'active'"#
    )
    .bind(campaign_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Active campaign not found".to_string()))?;

    let cid: Uuid = campaign.get("id");
    let name: String = campaign.get("name");
    let slug: String = campaign.get("slug");
    let campaign_type: String = campaign.get("type");
    let config: Value = campaign.get("config");
    let surface_config: Value = campaign.get("surface_config");
    let outcome_tags: Value = campaign.get("outcome_tags");

    // Build embed HTML snippet with campaign config
    let embed_html = format!(
        r#"<div id="is-embed-{}" data-campaign="{}" data-type="{}"></div>
<script src="/api/v1/widget/{}/config" async></script>"#,
        &cid.to_string()[..8], &slug, &campaign_type, &slug
    );

    Ok(Json(json!({
        "campaign": {
            "id": cid,
            "name": name,
            "slug": slug,
            "type": campaign_type,
        },
        "embed_html": embed_html,
        "config": config,
        "surface_config": surface_config,
        "outcome_tags": outcome_tags,
    })))
}

/// GET /api/v1/admin/campaigns/{id}/surface
pub async fn get_surface_config(
    State(state): State<AppState>,
    Path(id): Path<String>,
    _user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let campaign_id = Uuid::parse_str(&id)
        .map_err(|_| AppError::BadRequest("Invalid campaign ID".to_string()))?;

    let surface_config: Option<Value> = sqlx::query_scalar(
        "SELECT surface_config FROM campaigns WHERE id = $1"
    )
    .bind(campaign_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Campaign not found".to_string()))?;

    Ok(Json(json!({ "surface_config": surface_config })))
}

/// PUT /api/v1/admin/campaigns/{id}/surface
pub async fn update_surface_config(
    State(state): State<AppState>,
    Path(id): Path<String>,
    _user: AuthenticatedUser,
    Json(body): Json<UpdateSurfaceConfigInput>,
) -> Result<Json<Value>, AppError> {
    let campaign_id = Uuid::parse_str(&id)
        .map_err(|_| AppError::BadRequest("Invalid campaign ID".to_string()))?;

    sqlx::query(
        "UPDATE campaigns SET surface_config = $1 WHERE id = $2"
    )
    .bind(&body.surface_config)
    .bind(campaign_id)
    .execute(&state.db)
    .await?;

    Ok(Json(json!({
        "status": "updated",
        "surface_config": body.surface_config,
    })))
}

/// GET /api/v1/admin/domains
pub async fn list_domains(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let user_id = Uuid::parse_str(&_user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid user ID".to_string()))?;

    // Get the account's tenant_id
    let tenant_id: Option<Uuid> = sqlx::query_scalar(
        "SELECT COALESCE(tenant_id, id) FROM accounts WHERE id = $1"
    )
    .bind(user_id)
    .fetch_optional(&state.db)
    .await?
    .flatten();

    let domains = if let Some(tid) = tenant_id {
        sqlx::query_as::<_, CustomDomain>(
            r#"SELECT id, tenant_id, domain, target_type, verification_token,
                      verified_at, ssl_provisioned_at, is_active, created_at, updated_at
               FROM custom_domains WHERE tenant_id = $1
               ORDER BY created_at DESC"#
        )
        .bind(tid)
        .fetch_all(&state.db)
        .await?
    } else {
        Vec::new()
    };

    Ok(Json(json!({ "domains": domains })))
}

/// POST /api/v1/admin/domains
pub async fn register_domain(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Json(body): Json<RegisterDomainInput>,
) -> Result<Json<Value>, AppError> {
    let user_id = Uuid::parse_str(&_user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid user ID".to_string()))?;

    let tenant_id: Uuid = sqlx::query_scalar(
        "SELECT COALESCE(tenant_id, id) FROM accounts WHERE id = $1"
    )
    .bind(user_id)
    .fetch_one(&state.db)
    .await?;

    let id = Uuid::new_v4();
    let target_type = body.target_type.unwrap_or_else(|| "incentiveswift".to_string());

    sqlx::query(
        r#"INSERT INTO custom_domains (id, tenant_id, domain, target_type)
           VALUES ($1, $2, $3, $4)"#
    )
    .bind(id)
    .bind(tenant_id)
    .bind(&body.domain)
    .bind(&target_type)
    .execute(&state.db)
    .await?;

    let domain = sqlx::query_as::<_, CustomDomain>(
        r#"SELECT id, tenant_id, domain, target_type, verification_token,
                  verified_at, ssl_provisioned_at, is_active, created_at, updated_at
           FROM custom_domains WHERE id = $1"#
    )
    .bind(id)
    .fetch_one(&state.db)
    .await?;

    Ok(Json(json!({
        "domain": domain,
        "verification_instructions": format!(
            "Add a TXT record for _swift-verify.{} with value '{}'",
            body.domain, domain.verification_token
        ),
    })))
}

/// DELETE /api/v1/admin/domains/{id}
pub async fn remove_domain(
    State(state): State<AppState>,
    Path(id): Path<String>,
    _user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let domain_id = Uuid::parse_str(&id)
        .map_err(|_| AppError::BadRequest("Invalid domain ID".to_string()))?;
    let user_id = Uuid::parse_str(&_user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid user ID".to_string()))?;

    let tenant_id: Uuid = sqlx::query_scalar(
        "SELECT COALESCE(tenant_id, id) FROM accounts WHERE id = $1"
    )
    .bind(user_id)
    .fetch_one(&state.db)
    .await?;

    let result = sqlx::query(
        "DELETE FROM custom_domains WHERE id = $1 AND tenant_id = $2"
    )
    .bind(domain_id)
    .bind(tenant_id)
    .execute(&state.db)
    .await?;

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound("Domain not found".to_string()));
    }

    Ok(Json(json!({ "status": "removed", "id": id })))
}

/// POST /api/v1/admin/domains/{id}/verify
pub async fn verify_domain(
    State(state): State<AppState>,
    Path(id): Path<String>,
    _user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let domain_id = Uuid::parse_str(&id)
        .map_err(|_| AppError::BadRequest("Invalid domain ID".to_string()))?;
    let user_id = Uuid::parse_str(&_user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid user ID".to_string()))?;

    let tenant_id: Uuid = sqlx::query_scalar(
        "SELECT COALESCE(tenant_id, id) FROM accounts WHERE id = $1"
    )
    .bind(user_id)
    .fetch_one(&state.db)
    .await?;

    let domain = sqlx::query(
        r#"SELECT id, domain, verification_token, verified_at, is_active
           FROM custom_domains WHERE id = $1 AND tenant_id = $2"#
    )
    .bind(domain_id)
    .bind(tenant_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Domain not found".to_string()))?;

    let domain_name: String = domain.get("domain");
    let verification_token: String = domain.get("verification_token");
    let verified_at: Option<chrono::DateTime<chrono::Utc>> = domain.get("verified_at");

    if verified_at.is_some() {
        return Ok(Json(json!({
            "status": "already_verified",
            "message": "This domain has already been verified"
        })));
    }

    // Try to verify via DNS TXT record lookup
    let verified = check_dns_verification(&domain_name, &verification_token).await;

    if verified {
        sqlx::query(
            r#"UPDATE custom_domains
               SET verified_at = now(), is_active = true, updated_at = now()
               WHERE id = $1"#
        )
        .bind(domain_id)
        .execute(&state.db)
        .await?;

        Ok(Json(json!({
            "status": "verified",
            "domain": domain_name,
            "verified_at": chrono::Utc::now(),
        })))
    } else {
        Ok(Json(json!({
            "status": "pending",
            "domain": domain_name,
            "message": format!(
                "DNS verification record not found. Please add a TXT record for _swift-verify.{} with value '{}'",
                domain_name, verification_token
            ),
            "verification_token": verification_token,
        })))
    }
}

/// Attempt DNS TXT record lookup for domain verification.
async fn check_dns_verification(domain: &str, token: &str) -> bool {
    let lookup_name = format!("_swift-verify.{}", domain);
    let result = tokio::net::lookup_host(&lookup_name).await;
    match result {
        Ok(_) => {
            // DNS lookup succeeded — use system `dig` to check TXT records
            let output = std::process::Command::new("dig")
                .arg("TXT")
                .arg(&lookup_name)
                .arg("+short")
                .output();

            match output {
                Ok(out) => {
                    let stdout = String::from_utf8_lossy(&out.stdout);
                    stdout.contains(token)
                }
                Err(_) => {
                    // dig not available, assume verification is manual
                    false
                }
            }
        }
        Err(_) => false,
    }
}

/// GET /api/v1/admin/plans/{id}/domains
/// Check how many domains a plan tier allows.
pub async fn check_plan_domains(
    State(state): State<AppState>,
    Path(id): Path<String>,
    _user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let plan_id = Uuid::parse_str(&id)
        .map_err(|_| AppError::BadRequest("Invalid plan ID".to_string()))?;

    // Get plan to find matching plan_tier
    let plan = sqlx::query(
        "SELECT slug FROM plans WHERE id = $1"
    )
    .bind(plan_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Plan not found".to_string()))?;

    let plan_slug: String = plan.get("slug");

    // Check if the plan_tier's feature_limits allow domains
    let domain_limit: Option<i32> = sqlx::query_scalar(
        "SELECT limit_value FROM feature_limits WHERE plan_tier = $1 AND feature_key = 'custom_domains'"
    )
    .bind(&plan_slug)
    .fetch_optional(&state.db)
    .await?
    .flatten();

    let (allowed, limit) = match domain_limit {
        Some(-1) => (true, -1_i64),   // unlimited
        Some(l) if l > 0 => (true, l as i64),
        Some(_) => (false, 0_i64),
        None => (false, 0_i64),
    };

    Ok(Json(json!({
        "plan_id": plan_id,
        "plan_slug": plan_slug,
        "custom_domains_allowed": allowed,
        "custom_domain_limit": limit,
        "message": if allowed {
            if limit == -1 {
                "Unlimited custom domains".to_string()
            } else {
                format!("Up to {} custom domains allowed", limit)
            }
        } else {
            "Custom domains not included in this plan".to_string()
        },
    })))
}
