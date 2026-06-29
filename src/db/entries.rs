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
    pub delivered: bool,
    pub delivered_at: Option<chrono::DateTime<chrono::Utc>>,
    pub delivery_attempts: i32,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Create a new entry.
pub async fn create_entry(
    pool: &PgPool,
    input: &CreateEntryInput,
) -> Result<Uuid, AppError> {
    let id = Uuid::new_v4();
    let tags_applied = input.tags_applied.clone().unwrap_or_default();

    sqlx::query(
        r#"INSERT INTO entries (id, contact_id, campaign_id, answers, score, outcome, tags_applied)
           VALUES ($1, $2, $3, $4, $5, $6, $7)"#
    )
    .bind(id)
    .bind(input.contact_id)
    .bind(input.campaign_id)
    .bind(&input.answers)
    .bind(input.score)
    .bind(&input.outcome)
    .bind(&tags_applied)
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
    crate::db::delivery_log::log_delivery(
        pool,
        entry_id,
        method,
        target,
        success,
        None,
        None,
    )
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
    pub delivered: bool,
    pub delivered_at: Option<chrono::DateTime<chrono::Utc>>,
    pub delivery_attempts: i32,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub campaign_name: String,
    pub campaign_slug: String,
    pub campaign_type: String,
    pub tag_namespace: String,
}

/// Get all entries for a contact with campaign info.
pub async fn get_entries_for_contact(
    pool: &PgPool,
    contact_id: &uuid::Uuid,
) -> Result<Vec<EntryWithCampaign>, AppError> {
    let rows = sqlx::query_as::<_, EntryWithCampaign>(
        r#"SELECT e.id, e.contact_id, e.campaign_id, e.answers,
                  e.score, e.outcome, e.tags_applied,
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
