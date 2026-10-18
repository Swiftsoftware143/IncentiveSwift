//! Entry database operations — create entries, record delivery, list for contacts.

use crate::error::AppError;
use serde_json::Value as JsonValue;
use sqlx::PgPool;
use uuid::Uuid;

/// Input for creating an entry.
#[derive(Debug, serde::Deserialize)]
pub struct CreateEntryInput {
    pub contact_id: Uuid,
    pub campaign_id: Uuid,
    pub answers: JsonValue,
    pub score: Option<i32>,
    pub outcome: Option<String>,
    pub tags_applied: Option<Vec<String>>,
    pub utm_source: Option<String>,
    pub utm_medium: Option<String>,
    pub utm_campaign: Option<String>,
    pub referrer_url: Option<String>,
    pub page_url: Option<String>,
    pub user_agent: Option<String>,
    pub ip_address: Option<String>,
}

/// A entry record.
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct Entry {
    pub id: uuid::Uuid,
    pub contact_id: uuid::Uuid,
    pub campaign_id: uuid::Uuid,
    pub answers: serde_json::Value,
    pub score: Option<i32>,
    pub outcome: Option<String>,
    pub tags_applied: Option<Vec<String>>,
    pub utm_source: Option<String>,
    pub utm_medium: Option<String>,
    pub utm_campaign: Option<String>,
    pub referrer_url: Option<String>,
    pub page_url: Option<String>,
    pub user_agent: Option<String>,
    pub ip_address: Option<String>,
    pub delivered: bool,
    pub delivered_at: Option<chrono::DateTime<chrono::Utc>>,
    pub delivery_attempts: i32,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Create a new entry.
///
/// The account's lead allowance is enforced HERE, before the INSERT, because this helper is the
/// shared creator behind the generic capture route and seven mechanic handlers — see
/// `crate::features::enforce_lead_limit_for_campaign`, the same gate the direct writers of `entries`
/// (quiz, raffle, spin/prize-draw, chat-funnel SMS, milestone bonuses) call.
pub async fn create_entry(pool: &PgPool, input: &CreateEntryInput) -> Result<Uuid, AppError> {
    crate::features::enforce_lead_limit_for_campaign(pool, input.campaign_id).await?;

    let id = Uuid::new_v4();
    let tags_applied = input.tags_applied.clone().unwrap_or_default();

    sqlx::query(
        r#"INSERT INTO entries (id, contact_id, campaign_id, answers, score, outcome, tags_applied,
            utm_source, utm_medium, utm_campaign, referrer_url, page_url, user_agent, ip_address)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)"#,
    )
    .bind(id)
    .bind(input.contact_id)
    .bind(input.campaign_id)
    .bind(&input.answers)
    .bind(input.score)
    .bind(&input.outcome)
    .bind(&tags_applied)
    .bind(&input.utm_source)
    .bind(&input.utm_medium)
    .bind(&input.utm_campaign)
    .bind(&input.referrer_url)
    .bind(&input.page_url)
    .bind(&input.user_agent)
    .bind(&input.ip_address)
    .execute(pool)
    .await?;

    Ok(id)
}

/// Record a delivery attempt for an entry.
pub async fn record_delivery(
    pool: &PgPool,
    entry_id: &Uuid,
    success: bool,
    method: &str,
    target: &str,
) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE entries SET delivered = $1, delivered_at = CASE WHEN $1 THEN now() ELSE delivered_at END, delivery_attempts = delivery_attempts + 1 WHERE id = $2"
    )
    .bind(success)
    .bind(entry_id)
    .execute(pool)
    .await?;

    // Also log to delivery_log
    crate::db::delivery_log::log_delivery(pool, entry_id, method, target, success, None, None)
        .await?;

    Ok(())
}

/// Entry with campaign info for contact history.
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct EntryWithCampaign {
    pub id: uuid::Uuid,
    pub contact_id: uuid::Uuid,
    pub campaign_id: uuid::Uuid,
    pub answers: serde_json::Value,
    pub score: Option<i32>,
    pub outcome: Option<String>,
    pub tags_applied: Option<Vec<String>>,
    pub utm_source: Option<String>,
    pub utm_medium: Option<String>,
    pub utm_campaign: Option<String>,
    pub referrer_url: Option<String>,
    pub page_url: Option<String>,
    pub user_agent: Option<String>,
    pub ip_address: Option<String>,
    pub delivered: bool,
    pub delivered_at: Option<chrono::DateTime<chrono::Utc>>,
    pub delivery_attempts: i32,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub campaign_name: String,
    pub campaign_slug: String,
    pub campaign_type: String,
    pub tag_namespace: String,
}

/// One entry of ONE campaign as the OPERATOR reads it — the participant's identity beside the
/// answers they gave (kanban t_7cb597c4).
///
/// Before this existed the only reader of `entries` was `get_entries_for_contact` (a participant's
/// own history), so the operator who DEFINED a per-campaign entry-form question could never see the
/// answer to it: the value was stored in `entries.answers._custom` and read by nothing. The columns
/// here are the read side of that write — name/address from `contacts` because the entry carries
/// `contact_id` alone, and the whole `answers` document so a field that was deleted after the entry
/// was taken still shows its stored value.
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct CampaignEntryRow {
    pub id: Uuid,
    pub contact_id: Uuid,
    pub answers: JsonValue,
    pub score: Option<i32>,
    pub outcome: Option<String>,
    pub delivered: bool,
    pub delivered_at: Option<chrono::DateTime<chrono::Utc>>,
    pub utm_source: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub contact_name: Option<String>,
    pub contact_email: Option<String>,
    pub contact_phone: Option<String>,
    pub contact_business: Option<String>,
}

/// One entry-form field the operator defined for a campaign (the label side of `answers._custom`).
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct CampaignEntryField {
    pub field_key: String,
    pub field_label: String,
    pub field_type: String,
}

/// The entries of one campaign, newest first, with the participant attached.
///
/// The caller must have resolved the campaign against the caller's OWN account first
/// (`campaign_for_caller`) — this helper takes a `campaign_id` and trusts it, so it is never a
/// tenancy boundary on its own.
pub async fn list_entries_for_campaign(
    pool: &PgPool,
    campaign_id: &Uuid,
    limit: i64,
    offset: i64,
) -> Result<Vec<CampaignEntryRow>, AppError> {
    let rows = sqlx::query_as::<_, CampaignEntryRow>(
        r#"SELECT e.id, e.contact_id, e.answers, e.score, e.outcome,
                  e.delivered, e.delivered_at, e.utm_source, e.created_at,
                  NULLIF(TRIM(CONCAT_WS(' ', c.first_name, c.last_name)), '') AS contact_name,
                  c.email AS contact_email,
                  c.phone AS contact_phone,
                  c.business_name AS contact_business
           FROM entries e
           LEFT JOIN contacts c ON c.id = e.contact_id
           WHERE e.campaign_id = $1
           ORDER BY e.created_at DESC
           LIMIT $2 OFFSET $3"#,
    )
    .bind(campaign_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

/// Total entries on one campaign (the pager's denominator).
pub async fn count_entries_for_campaign(
    pool: &PgPool,
    campaign_id: &Uuid,
) -> Result<i64, AppError> {
    let total = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM entries WHERE campaign_id = $1")
        .bind(campaign_id)
        .fetch_one(pool)
        .await?;
    Ok(total)
}

/// How many entries on this campaign carry at least one entry-form answer (`answers ? '_custom'`).
/// The screen uses it to say "N of M answered the questions you added", which is the fact the
/// operator needs and cannot get from a full answers dump.
pub async fn count_entries_with_custom_answers(
    pool: &PgPool,
    campaign_id: &Uuid,
) -> Result<i64, AppError> {
    let total = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM entries WHERE campaign_id = $1 AND answers ? '_custom'",
    )
    .bind(campaign_id)
    .fetch_one(pool)
    .await?;
    Ok(total)
}

/// The entry-form fields the operator defined for this campaign, in the order the form shows them.
pub async fn list_entry_fields_for_campaign(
    pool: &PgPool,
    campaign_id: &Uuid,
) -> Result<Vec<CampaignEntryField>, AppError> {
    let rows = sqlx::query_as::<_, CampaignEntryField>(
        r#"SELECT field_key, field_label, field_type
           FROM campaign_custom_fields
           WHERE campaign_id = $1
           ORDER BY sort_order, field_label"#,
    )
    .bind(campaign_id)
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

/// Get all entries for a contact with campaign info.
pub async fn get_entries_for_contact(
    pool: &PgPool,
    contact_id: &uuid::Uuid,
) -> Result<Vec<EntryWithCampaign>, AppError> {
    let rows = sqlx::query_as::<_, EntryWithCampaign>(
        r#"SELECT e.id, e.contact_id, e.campaign_id, e.answers,
                  e.score, e.outcome, e.tags_applied,
                  e.utm_source, e.utm_medium, e.utm_campaign,
                  e.referrer_url, e.page_url, e.user_agent, e.ip_address,
                  e.delivered, e.delivered_at, e.delivery_attempts, e.created_at,
                  c.name as campaign_name, c.slug as campaign_slug,
                  c.type as campaign_type, c.tag_namespace
           FROM entries e
           JOIN campaigns c ON c.id = e.campaign_id
           WHERE e.contact_id = $1
           ORDER BY e.created_at DESC"#,
    )
    .bind(contact_id)
    .fetch_all(pool)
    .await?;

    Ok(rows)
}
