//! Viral campaign engine ??? DB operations.
//! Earn channels, click-through logs, campaign referrals, referral credit, points balance.

use crate::error::AppError;
use sqlx::PgPool;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Earn Channels
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct EarnChannel {
    pub id: Uuid,
    pub account_id: Uuid,
    pub campaign_id: Uuid,
    pub channel_code: String,
    pub label: String,
    pub description: String,
    pub points_per_click: i32,
    pub max_clicks_per_contact: i32,
    pub redirect_url: String,
    pub verification_type: String,  /* auto_approve_all | auto_approve_answer | manual_approve */
    pub expected_answer: Option<String>,
    pub verification_label: String,
    pub approval_notes: String,
    pub is_active: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

pub async fn get_active_channel_by_code(
    pool: &PgPool,
    code: &str,
) -> Result<EarnChannel, AppError> {
    sqlx::query_as::<_, EarnChannel>(
        r#"SELECT id, account_id, campaign_id, channel_code, label, description,
                  points_per_click, max_clicks_per_contact, redirect_url,
                  is_active, created_at, updated_at
           FROM earn_channels WHERE channel_code = $1 AND is_active = true"#
    )
    .bind(code)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::NotFound("Earn channel not found or inactive".to_string()))
}

pub async fn count_contact_clicks_for_channel(
    pool: &PgPool,
    channel_id: &Uuid,
    contact_id: &Uuid,
) -> Result<i32, AppError> {
    let count: (i64,) = sqlx::query_as(
        r#"SELECT COUNT(*) FROM earn_click_log
           WHERE channel_id = $1 AND contact_id = $2"#
    )
    .bind(channel_id)
    .bind(contact_id)
    .fetch_one(pool)
    .await?;
    Ok(count.0 as i32)
}

pub async fn log_earn_click(
    pool: &PgPool,
    channel_id: &Uuid,
    contact_id: Option<&Uuid>,
    campaign_id: &Uuid,
    ip_address: Option<&str>,
    user_agent: Option<&str>,
    referrer_url: Option<&str>,
    utm_source: Option<&str>,
    utm_medium: Option<&str>,
    utm_campaign: Option<&str>,
    points_awarded: i32,
) -> Result<(), AppError> {
    sqlx::query(
        r#"INSERT INTO earn_click_log
           (channel_id, contact_id, campaign_id, ip_address, user_agent,
            referrer_url, utm_source, utm_medium, utm_campaign, points_awarded)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)"#
    )
    .bind(channel_id)
    .bind(contact_id)
    .bind(campaign_id)
    .bind(ip_address)
    .bind(user_agent)
    .bind(referrer_url)
    .bind(utm_source)
    .bind(utm_medium)
    .bind(utm_campaign)
    .bind(points_awarded)
    .execute(pool)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Campaign Referrals
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct CampaignReferral {
    pub id: Uuid,
    pub campaign_id: Uuid,
    pub referrer_contact_id: Option<Uuid>,
    pub referee_contact_id: Option<Uuid>,
    pub referral_code: String,
    pub source: String,
    pub converted: bool,
    pub converted_at: Option<chrono::DateTime<chrono::Utc>>,
    pub click_count: i32,
    pub points_earned: i32,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

pub async fn find_referral_by_code(
    pool: &PgPool,
    campaign_id: &Uuid,
    code: &str,
) -> Result<Option<CampaignReferral>, AppError> {
    let result = sqlx::query_as::<_, CampaignReferral>(
        r#"SELECT id, campaign_id, referrer_contact_id, referee_contact_id,
                  referral_code, source, converted, converted_at,
                  click_count, points_earned, created_at
           FROM campaign_referrals
           WHERE campaign_id = $1 AND referral_code = $2"#
    )
    .bind(campaign_id)
    .bind(code)
    .fetch_optional(pool)
    .await?;
    Ok(result)
}

pub async fn generate_unique_referral_code(
    pool: &PgPool,
    campaign_id: &Uuid,
) -> Result<String, AppError> {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let attempts = 0..20;
    for _ in attempts {
        let code: String = (0..6)
            .map(|_| {
                let idx = rng.gen_range(0..36);
                if idx < 10 { (b'0' + idx as u8) as char }
                else { (b'a' + (idx - 10) as u8) as char }
            })
            .collect();
        let exists: Option<(String,)> = sqlx::query_as(
            "SELECT referral_code FROM campaign_referrals WHERE campaign_id = $1 AND referral_code = $2"
        )
        .bind(campaign_id)
        .bind(&code)
        .fetch_optional(pool)
        .await?;
        if exists.is_none() {
            return Ok(code);
        }
    }
    // Fallback: append random suffix
    Ok(format!("ref{}", rng.gen_range(100000..999999)))
}

pub async fn create_referral(
    pool: &PgPool,
    campaign_id: &Uuid,
    referrer_contact_id: Option<Uuid>,
    referral_code: &str,
    source: &str,
) -> Result<CampaignReferral, AppError> {
    let id = Uuid::new_v4();
    sqlx::query(
        r#"INSERT INTO campaign_referrals
           (id, campaign_id, referrer_contact_id, referral_code, source)
           VALUES ($1, $2, $3, $4, $5)"#
    )
    .bind(id)
    .bind(campaign_id)
    .bind(referrer_contact_id)
    .bind(referral_code)
    .bind(source)
    .execute(pool)
    .await?;

    Ok(CampaignReferral {
        id,
        campaign_id: *campaign_id,
        referrer_contact_id: referrer_contact_id,
        referee_contact_id: None,
        referral_code: referral_code.to_string(),
        source: source.to_string(),
        converted: false,
        converted_at: None,
        click_count: 0,
        points_earned: 0,
        created_at: chrono::Utc::now(),
    })
}

pub async fn increment_referral_click(
    pool: &PgPool,
    referral_id: &Uuid,
) -> Result<(), AppError> {
    sqlx::query(
        r#"UPDATE campaign_referrals SET click_count = click_count + 1 WHERE id = $1"#
    )
    .bind(referral_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn mark_referral_converted(
    pool: &PgPool,
    referral_id: &Uuid,
    referee_contact_id: &Uuid,
) -> Result<(), AppError> {
    sqlx::query(
        r#"UPDATE campaign_referrals
           SET converted = true, converted_at = now(), referee_contact_id = COALESCE(referee_contact_id, $2)
           WHERE id = $1"#
    )
    .bind(referral_id)
    .bind(referee_contact_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn log_referral_credit(
    pool: &PgPool,
    referral_id: &Uuid,
    referrer_contact_id: Option<Uuid>,
    campaign_id: &Uuid,
    entry_id: Option<&Uuid>,
    action_type: &str,
    points_awarded: i32,
) -> Result<(), AppError> {
    sqlx::query(
        r#"INSERT INTO referral_credit_log
           (referral_id, referrer_contact_id, campaign_id, entry_id, action_type, points_awarded)
           VALUES ($1, $2, $3, $4, $5, $6)"#
    )
    .bind(referral_id)
    .bind(referrer_contact_id)
    .bind(campaign_id)
    .bind(entry_id)
    .bind(action_type)
    .bind(points_awarded)
    .execute(pool)
    .await?;

    // Update referrer's total points earned
    sqlx::query(
        r#"UPDATE campaign_referrals SET points_earned = points_earned + $1 WHERE id = $2"#
    )
    .bind(points_awarded)
    .bind(referral_id)
    .execute(pool)
    .await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Campaign-Specific Points Balance
// ---------------------------------------------------------------------------

pub async fn upsert_campaign_points(
    pool: &PgPool,
    campaign_id: &Uuid,
    contact_id: &Uuid,
    points_to_add: i32,
) -> Result<i32, AppError> {
    // Upsert and return new balance
    let row: (i32,) = sqlx::query_as(
        r#"INSERT INTO campaign_points_balance (campaign_id, contact_id, points_balance, lifetime_points)
           VALUES ($1, $2, $3, GREATEST($3, 0))
           ON CONFLICT (campaign_id, contact_id)
           DO UPDATE SET
               points_balance = campaign_points_balance.points_balance + $3,
               lifetime_points = campaign_points_balance.lifetime_points + GREATEST($3, 0),
               updated_at = now()
           RETURNING points_balance"#
    )
    .bind(campaign_id)
    .bind(contact_id)
    .bind(points_to_add)
    .fetch_one(pool)
    .await?;

    Ok(row.0)
}

pub async fn get_campaign_points(
    pool: &PgPool,
    campaign_id: &Uuid,
    contact_id: &Uuid,
) -> Result<i32, AppError> {
    let result: Option<(i32,)> = sqlx::query_as(
        r#"SELECT points_balance FROM campaign_points_balance
           WHERE campaign_id = $1 AND contact_id = $2"#
    )
    .bind(campaign_id)
    .bind(contact_id)
    .fetch_optional(pool)
    .await?;

    Ok(result.map(|r| r.0).unwrap_or(0))
}

pub async fn get_campaign_leaderboard(
    pool: &PgPool,
    campaign_id: &Uuid,
    limit: i64,
) -> Result<Vec<(Uuid, Uuid, i32, i32)>, AppError> {
    let rows: Vec<(Uuid, Uuid, i32, i32)> = sqlx::query_as(
        r#"SELECT contact_id, campaign_id, lifetime_points, points_balance
           FROM campaign_points_balance
           WHERE campaign_id = $1 AND lifetime_points > 0
           ORDER BY lifetime_points DESC
           LIMIT $2"#
    )
    .bind(campaign_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

// ---------------------------------------------------------------------------
// Referral + Earn Stats (for admin)
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Serialize, sqlx::FromRow)]
pub struct ReferralStats {
    pub total_referrals: i64,
    pub total_conversions: i64,
    pub total_points_awarded: i64,
    pub total_clicks: i64,
}

pub async fn get_campaign_referral_stats(
    pool: &PgPool,
    campaign_id: &Uuid,
) -> Result<ReferralStats, AppError> {
    let stats = sqlx::query_as::<_, ReferralStats>(
        r#"SELECT
               COUNT(*)::bigint AS total_referrals,
               COUNT(*) FILTER (WHERE converted)::bigint AS total_conversions,
               COALESCE(SUM(points_earned), 0)::bigint AS total_points_awarded,
               COALESCE(SUM(click_count), 0)::bigint AS total_clicks
           FROM campaign_referrals WHERE campaign_id = $1"#
    )
    .bind(campaign_id)
    .fetch_one(pool)
    .await?;
    Ok(stats)
}

pub async fn list_campaign_earn_channels(
    pool: &PgPool,
    campaign_id: &Uuid,
) -> Result<Vec<EarnChannel>, AppError> {
    let channels = sqlx::query_as::<_, EarnChannel>(
        r#"SELECT id, account_id, campaign_id, channel_code, label, description,
                  points_per_click, max_clicks_per_contact, redirect_url,
                  is_active, created_at, updated_at
           FROM earn_channels WHERE campaign_id = $1
           ORDER BY created_at DESC"#
    )
    .bind(campaign_id)
    .fetch_all(pool)
    .await?;
    Ok(channels)
}
