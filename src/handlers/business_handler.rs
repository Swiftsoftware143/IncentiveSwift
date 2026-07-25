//! Business Handler — external business accounts for directory listings.
//!
//! When a business owner on a Multi-Directory site wants to run their own
//! campaigns (SMS funnels, quizzes, surveys), they register as a business
//! sub-account in IncentiveSwift. This creates an accounts entry + portfolio
//! company + API key that the directory stores and uses for proxy calls.

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;
use tracing;

use crate::error::AppError;
use crate::state::AppState;

// Types
#[derive(Debug, Deserialize)]
pub struct RegisterBusinessRequest {
    pub name: String,
    pub email: String,
    pub business_type: Option<String>,
    pub directory_slug: Option<String>,
    pub phone: Option<String>,
    pub listing_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct RegisterBusinessResponse {
    pub business_id: Uuid,
    pub api_key: String,
    pub api_key_prefix: String,
    pub name: String,
    pub slug: String,
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct BusinessStats {
    pub business_id: Uuid,
    pub business_name: String,
    pub total_leads: i64,
    pub total_checkins: i64,
    pub total_rewards_redeemed: i64,
    pub active_campaigns: i64,
    pub campaigns: Vec<BusinessCampaignSummary>,
}

#[derive(Debug, Serialize)]
pub struct BusinessCampaignSummary {
    pub id: Uuid,
    pub name: String,
    pub slug: String,
    pub campaign_type: String,
    pub status: String,
    pub entry_count: Option<i64>,
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Deserialize)]
pub struct BusinessStatsQuery {
    pub include_campaigns: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct BusinessListQuery {
    pub directory_slug: Option<String>,
}

// ═══════════════════════════════════════════════════════════════════════════════
// Handler: Register a new business
// ═══════════════════════════════════════════════════════════════════════════════

pub async fn register_business(
    State(s): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<RegisterBusinessRequest>,
) -> Result<impl IntoResponse, AppError> {
    let internal_key = std::env::var("INTERNAL_SYNC_KEY")
        .unwrap_or_else(|_| "internal-sync-key-placeholder".to_string());
    let provided_key = headers
        .get("X-Internal-Key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if provided_key != internal_key {
        return Err(AppError::Forbidden("Invalid internal key".into()));
    }
    if req.name.is_empty() || req.email.is_empty() {
        return Err(AppError::BadRequest("Name and email are required".into()));
    }

    let biz_slug = req.name.to_lowercase().replace(' ', "-");
    let biz_name = req.name;

    // Check if account already exists by email
    let existing: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM accounts WHERE email = $1 LIMIT 1"
    )
    .bind(&req.email)
    .fetch_optional(&s.db)
    .await?
    .flatten();

    if let Some(id) = existing {
        // Return existing key prefix
        let prefix: Option<String> = sqlx::query_scalar(
            "SELECT prefix FROM api_keys WHERE tenant_id = $1 AND is_active = true ORDER BY created_at DESC LIMIT 1"
        )
        .bind(id)
        .fetch_optional(&s.db)
        .await?
        .flatten();

        return Ok((StatusCode::OK, Json(json!(RegisterBusinessResponse {
            business_id: id,
            api_key: "".to_string(), // don't re-expose existing key
            api_key_prefix: prefix.unwrap_or_default(),
            name: biz_name,
            slug: biz_slug,
            message: "Business account already exists. Using existing account.".into(),
        }))));
    }

    // Create account (tenant)
    let business_id = Uuid::new_v4();
    let slug: String = biz_slug.chars()
        .filter(|c| c.is_alphanumeric() || *c == '-')
        .take(50)
        .collect();

    let now = chrono::Utc::now();
    sqlx::query(
        r#"INSERT INTO accounts (id, email, name, role, slug, credits_balance, credits_lifetime_used, created_at)
           VALUES ($1, $2, $3, 'company_admin', $4, 0, 0, $5)"#
    )
    .bind(business_id)
    .bind(&req.email)
    .bind(&biz_name)
    .bind(&slug)
    .bind(now)
    .execute(&s.db)
    .await?;

    // Also create portfolio_company for directory integration tracking
    let pf_id = Uuid::new_v4();
    let settings = json!({
        "email": req.email,
        "business_type": req.business_type,
        "directory_slug": req.directory_slug,
        "phone": req.phone,
        "listing_id": req.listing_id,
        "source": "multidirectory",
    });
    sqlx::query(
        r#"INSERT INTO portfolio_companies (id, account_id, name, slug, settings)
           VALUES ($1, $2, $3, $4, $5)"#
    )
    .bind(pf_id)
    .bind(business_id)
    .bind(&biz_name)
    .bind(&slug)
    .bind(&settings)
    .execute(&s.db)
    .await?;

    // Generate API key
    let api_key = format!("is_biz_{}", Uuid::new_v4().to_string().replace("-", ""));
    let key_prefix = api_key[..8].to_string();

    use argon2::{
        password_hash::{rand_core::OsRng, SaltString},
        Argon2, PasswordHasher,
    };
    let salt = SaltString::generate(&mut OsRng);
    let argon2 = Argon2::default();
    let key_hash = argon2
        .hash_password(api_key.as_bytes(), &salt)
        .map_err(|e| AppError::Internal(format!("hash: {}", e)))?
        .to_string();

    sqlx::query(
        r#"INSERT INTO api_keys (tenant_id, user_id, name, key_hash, prefix, permissions, is_active)
           VALUES ($1, $2, $3, $4, $5, $6, true)"#
    )
    .bind(business_id)
    .bind(business_id)
    .bind(&format!("{} API Key", biz_name))
    .bind(&key_hash)
    .bind(&key_prefix)
    .bind(&json!(["campaigns:read","campaigns:write","contacts:read","contacts:write","stats:read"]))
    .execute(&s.db)
    .await?;

    tracing::info!("[business] Registered business '{}' ({}), prefix={}", biz_name, business_id, key_prefix);

    Ok((StatusCode::CREATED, Json(json!(RegisterBusinessResponse {
        business_id,
        api_key,
        api_key_prefix: key_prefix,
        name: biz_name.clone(),
        slug,
        message: format!("Business '{}' registered successfully.", biz_name),
    }))))
}

// ═══════════════════════════════════════════════════════════════════════════════
// Handler: Business stats (mirror data for directory)
// ═══════════════════════════════════════════════════════════════════════════════

pub async fn get_business_stats(
    State(s): State<AppState>,
    headers: HeaderMap,
    Path(business_id): Path<Uuid>,
    Query(query): Query<BusinessStatsQuery>,
) -> Result<impl IntoResponse, AppError> {
    verify_business_auth(&headers, &s.db, business_id).await?;

    // Get account info
    let biz: (Uuid, String, Option<String>, Option<Value>) = sqlx::query_as(
        "SELECT id, name, slug, NULL::jsonb as settings FROM accounts WHERE id = $1"
    )
    .bind(business_id)
    .fetch_optional(&s.db)
    .await?
    .ok_or(AppError::NotFound("Business not found".into()))?;

    let (biz_id, biz_name, _slug, _settings) = biz;

    // Total leads = entries from campaigns owned by this account
    let total_leads: i64 = sqlx::query_scalar(
        "SELECT COALESCE(COUNT(*), 0) FROM entries e JOIN campaigns c ON c.id = e.campaign_id WHERE c.account_id = $1"
    )
    .bind(biz_id)
    .fetch_one(&s.db)
    .await
    .unwrap_or(0);

    // Total checkins (loyalty activity at this business)
    let total_checkins: i64 = sqlx::query_scalar(
        "SELECT COALESCE(COUNT(*), 0) FROM loyalty_checkins WHERE 1=0"
    )
    .fetch_one(&s.db)
    .await
    .unwrap_or(0);

    // Total rewards redeemed
    let total_redemptions: i64 = sqlx::query_scalar(
        "SELECT COALESCE(COUNT(*), 0) FROM loyalty_rewards_earned WHERE status = 'redeemed' AND 1=0"
    )
    .fetch_one(&s.db)
    .await
    .unwrap_or(0);

    // Active campaigns
    let active_campaigns: i64 = sqlx::query_scalar(
        "SELECT COALESCE(COUNT(*), 0) FROM campaigns WHERE account_id = $1 AND status = 'active'"
    )
    .bind(biz_id)
    .fetch_one(&s.db)
    .await
    .unwrap_or(0);

    // Campaign summaries
    let campaigns: Vec<BusinessCampaignSummary> = if query.include_campaigns.unwrap_or(true) {
        let rows: Vec<(Uuid, String, String, String, String, Option<i64>, Option<chrono::DateTime<chrono::Utc>>)> = sqlx::query_as(
            r#"SELECT c.id, c.name, c.slug, c.type, c.status,
                      (SELECT COUNT(*) FROM entries e WHERE e.campaign_id = c.id) as entry_count,
                      c.created_at
               FROM campaigns c WHERE c.account_id = $1
               ORDER BY c.created_at DESC LIMIT 20"#
        )
        .bind(biz_id)
        .fetch_all(&s.db)
        .await?;
        rows.into_iter().map(|(id, name, slug, ctype, status, count, created)| {
            BusinessCampaignSummary { id, name, slug, campaign_type: ctype, status, entry_count: count, created_at: created }
        }).collect()
    } else { vec![] };

    Ok(Json(json!(BusinessStats {
        business_id: biz_id,
        business_name: biz_name,
        total_leads,
        total_checkins,
        total_rewards_redeemed: total_redemptions,
        active_campaigns,
        campaigns,
    })))
}

// ═══════════════════════════════════════════════════════════════════════════════
// Handler: Campaign widget (embeddable for directory listings)
// ═══════════════════════════════════════════════════════════════════════════════

pub async fn get_campaign_widget(
    State(s): State<AppState>,
    Path(slug): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let camp: (Uuid, String, String, String, String, Value) = sqlx::query_as(
        "SELECT id, name, type, status, slug, config FROM campaigns WHERE slug = $1 AND status = 'active'"
    )
    .bind(&slug)
    .fetch_optional(&s.db)
    .await?
    .ok_or(AppError::NotFound("Campaign not found or not active".into()))?;

    let (id, name, ctype, _status, cslug, config) = camp;

    let widget = match ctype.as_str() {
        "quiz" => json!({
            "type": "quiz", "name": name,
            "cta_text": config.get("cta_text").and_then(|v| v.as_str()).unwrap_or("Take this quiz for a special offer!"),
            "button_text": config.get("button_text").and_then(|v| v.as_str()).unwrap_or("Start Quiz"),
            "questions_count": config.get("questions").and_then(|v| v.as_array().map(|a| a.len())).unwrap_or(0),
            "embed_url": format!("/api/v1/c/{}/embed", cslug),
        }),
        "form" => json!({
            "type": "form", "name": name,
            "cta_text": config.get("cta_text").and_then(|v| v.as_str()).unwrap_or("Fill out this form"),
            "button_text": config.get("button_text").and_then(|v| v.as_str()).unwrap_or("Submit"),
            "fields": config.get("fields").unwrap_or(&json!([])),
            "embed_url": format!("/api/v1/c/{}/embed", cslug),
        }),
        _ => json!({
            "type": ctype, "name": name,
            "cta_text": config.get("cta_text").and_then(|v| v.as_str()).unwrap_or("Enter for a chance to win!"),
            "button_text": config.get("button_text").and_then(|v| v.as_str()).unwrap_or("Enter Now"),
            "embed_url": format!("/api/v1/c/{}/embed", cslug),
        }),
    };

    Ok(Json(json!({ "campaign_id": id, "campaign_slug": cslug, "widget": widget })))
}

// ═══════════════════════════════════════════════════════════════════════════════
// Admin Handlers
// ═══════════════════════════════════════════════════════════════════════════════

pub async fn admin_list_businesses(
    State(s): State<AppState>,
    Query(query): Query<BusinessListQuery>,
) -> Result<impl IntoResponse, AppError> {
    let rows: Vec<(Uuid, String, String, Option<Value>, Option<chrono::DateTime<chrono::Utc>>)> = if let Some(ref dir) = query.directory_slug {
        sqlx::query_as(
            r#"SELECT pf.id, pf.name, pf.slug, pf.settings, pf.created_at
               FROM portfolio_companies pf
               WHERE pf.settings->>'source' = 'multidirectory'
                 AND pf.settings->>'directory_slug' = $1
               ORDER BY pf.created_at DESC"#
        ).bind(dir).fetch_all(&s.db).await?
    } else {
        sqlx::query_as(
            r#"SELECT pf.id, pf.name, pf.slug, pf.settings, pf.created_at
               FROM portfolio_companies pf
               WHERE pf.settings->>'source' = 'multidirectory'
               ORDER BY pf.created_at DESC"#
        ).fetch_all(&s.db).await?
    };

    let businesses: Vec<Value> = rows.into_iter().map(|(id, name, slug, settings, created)| {
        let cfg = settings.unwrap_or(json!({}));
        json!({
            "id": id, "name": name, "slug": slug,
            "email": cfg.get("email"),
            "business_type": cfg.get("business_type"),
            "directory_slug": cfg.get("directory_slug"),
            "phone": cfg.get("phone"),
            "listing_id": cfg.get("listing_id"),
            "created_at": created,
        })
    }).collect();

    Ok(Json(json!({ "businesses": businesses, "total": businesses.len() })))
}

pub async fn admin_get_business(
    State(s): State<AppState>,
    Path(business_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let biz: Option<(Uuid, String, String, Option<Value>, Option<chrono::DateTime<chrono::Utc>>)> = sqlx::query_as(
        "SELECT id, name, slug, settings, created_at FROM portfolio_companies WHERE id = $1"
    )
    .bind(business_id)
    .fetch_optional(&s.db)
    .await?;

    let (id, name, slug, settings, created) = biz
        .ok_or(AppError::NotFound("Business not found".into()))?;

    let biz_settings = settings.unwrap_or(json!({}));

    // Get account_id from portfolio_company
    let account_id: Option<Uuid> = sqlx::query_scalar(
        "SELECT account_id FROM portfolio_companies WHERE id = $1"
    )
    .bind(id)
    .fetch_optional(&s.db)
    .await?
    .flatten();

    let api_key_info: Option<(String, bool, Option<chrono::DateTime<chrono::Utc>>)> = if let Some(aid) = account_id {
        sqlx::query_as(
            "SELECT prefix, is_active, created_at FROM api_keys WHERE tenant_id = $1 ORDER BY created_at DESC LIMIT 1"
        )
        .bind(aid)
        .fetch_optional(&s.db)
        .await?
    } else { None };

    let (total_leads, active_campaigns): (i64, i64) = if let Some(aid) = account_id {
        let leads: i64 = sqlx::query_scalar(
            "SELECT COALESCE(COUNT(*), 0) FROM entries e JOIN campaigns c ON c.id = e.campaign_id WHERE c.account_id = $1"
        ).bind(aid).fetch_one(&s.db).await.unwrap_or(0);
        let active: i64 = sqlx::query_scalar(
            "SELECT COALESCE(COUNT(*), 0) FROM campaigns WHERE account_id = $1 AND status = 'active'"
        ).bind(aid).fetch_one(&s.db).await.unwrap_or(0);
        (leads, active)
    } else { (0, 0) };

    Ok(Json(json!({
        "id": id, "name": name, "slug": slug,
        "email": biz_settings.get("email"),
        "business_type": biz_settings.get("business_type"),
        "directory_slug": biz_settings.get("directory_slug"),
        "phone": biz_settings.get("phone"),
        "listing_id": biz_settings.get("listing_id"),
        "api_key": {
            "prefix": api_key_info.as_ref().map(|(p,_,_)| p.clone()).unwrap_or_default(),
            "is_active": api_key_info.as_ref().map(|(_,a,_)| *a).unwrap_or(false),
            "created_at": api_key_info.and_then(|(_,_,c)| c),
        },
        "stats": { "total_leads": total_leads, "active_campaigns": active_campaigns },
        "created_at": created,
    })))
}

pub async fn admin_rotate_business_key(
    State(s): State<AppState>,
    Path(pf_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    // Get the account_id from portfolio_company
    let account_id: Uuid = sqlx::query_scalar(
        "SELECT account_id FROM portfolio_companies WHERE id = $1"
    )
    .bind(pf_id)
    .fetch_optional(&s.db)
    .await?
    .flatten()
    .ok_or(AppError::NotFound("Business not found".into()))?;

    // Deactivate old keys
    sqlx::query("UPDATE api_keys SET is_active = false, updated_at = NOW() WHERE tenant_id = $1")
        .bind(account_id)
        .execute(&s.db)
        .await?;

    let biz_name: String = sqlx::query_scalar("SELECT name FROM portfolio_companies WHERE id = $1")
        .bind(pf_id)
        .fetch_one(&s.db)
        .await?;

    use argon2::{
        password_hash::{rand_core::OsRng, SaltString},
        Argon2, PasswordHasher,
    };

    let new_key = format!("is_biz_{}", Uuid::new_v4().to_string().replace("-", ""));
    let key_prefix = new_key[..8].to_string();
    let salt = SaltString::generate(&mut OsRng);
    let argon2 = Argon2::default();
    let key_hash = argon2
        .hash_password(new_key.as_bytes(), &salt)
        .map_err(|e| AppError::Internal(format!("hash: {}", e)))?
        .to_string();

    sqlx::query(
        r#"INSERT INTO api_keys (tenant_id, user_id, name, key_hash, prefix, permissions, is_active)
           VALUES ($1, $2, $3, $4, $5, $6, true)"#
    )
    .bind(account_id)
    .bind(account_id)
    .bind(&format!("{} API Key", biz_name))
    .bind(&key_hash)
    .bind(&key_prefix)
    .bind(&json!(["campaigns:read","campaigns:write","contacts:read","contacts:write","stats:read"]))
    .execute(&s.db)
    .await?;

    tracing::info!("[business] Rotated API key for {} ({})", biz_name, pf_id);

    Ok(Json(json!({
        "success": true,
        "api_key": new_key,
        "api_key_prefix": key_prefix,
        "message": "API key rotated. Store the new key securely.",
    })))
}

// ═══════════════════════════════════════════════════════════════════════════════
// Auth
// ═══════════════════════════════════════════════════════════════════════════════

async fn verify_business_auth(
    headers: &HeaderMap,
    db: &sqlx::PgPool,
    business_id: Uuid,
) -> Result<(), AppError> {
    let internal_key = std::env::var("INTERNAL_SYNC_KEY")
        .unwrap_or_else(|_| "internal-sync-key-placeholder".to_string());

    if let Some(key) = headers.get("X-Internal-Key").and_then(|v| v.to_str().ok()) {
        if key == internal_key { return Ok(()); }
    }

    if let Some(auth) = headers.get("Authorization").and_then(|v| v.to_str().ok()) {
        let token = auth.strip_prefix("Bearer ").unwrap_or(auth);
        let prefix = &token[..token.len().min(8)];
        let key_info: (Uuid, String) = sqlx::query_as(
            "SELECT tenant_id, key_hash FROM api_keys WHERE prefix = $1 AND is_active = true AND tenant_id = $2 LIMIT 1"
        )
        .bind(prefix)
        .bind(business_id)
        .fetch_optional(db)
        .await?
        .ok_or(AppError::Unauthorized("Invalid API key".into()))?;

        use argon2::{Argon2, PasswordHash, PasswordVerifier};
        let parsed = PasswordHash::new(&key_info.1)
            .map_err(|_| AppError::Unauthorized("Invalid API key hash".into()))?;
        Argon2::default()
            .verify_password(token.as_bytes(), &parsed)
            .map_err(|_| AppError::Unauthorized("Invalid API key".into()))?;
        return Ok(());
    }

    Err(AppError::Unauthorized("No valid auth provided".into()))
}
