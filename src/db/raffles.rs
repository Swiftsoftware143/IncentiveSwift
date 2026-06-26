//! Raffle database operations — enter, get entries, record draw.

use crate::error::AppError;
use sqlx::PgPool;
use uuid::Uuid;

/// Enter a raffle: check campaign exists and is raffle type, check not already entered, insert entry.
pub async fn enter_raffle(
    pool: &PgPool,
    campaign_id: &Uuid,
    contact_id: &Uuid,
) -> Result<Uuid, AppError> {
    // Check campaign exists and is raffle type
    let campaign_type: Option<String> = sqlx::query_scalar(
        "SELECT type FROM campaigns WHERE id = $1 AND status = 'active'"
    )
    .bind(campaign_id)
    .fetch_optional(pool)
    .await?;

    match campaign_type {
        None => return Err(AppError::NotFound("Campaign not found or not active".to_string())),
        Some(t) if t != "raffle" => return Err(AppError::BadRequest("Campaign is not a raffle type".to_string())),
        _ => {}
    }

    // Check if contact already has an entry for this campaign
    let existing: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM entries WHERE campaign_id = $1 AND contact_id = $2 LIMIT 1"
    )
    .bind(campaign_id)
    .bind(contact_id)
    .fetch_optional(pool)
    .await?;

    if existing.is_some() {
        return Err(AppError::BadRequest("Contact already entered this raffle".to_string()));
    }

    // Create entry
    let entry_id = Uuid::new_v4();
    sqlx::query(
        r#"INSERT INTO entries (id, contact_id, campaign_id, answers, outcome, tags_applied)
           VALUES ($1, $2, $3, '{}'::jsonb, 'entrant', ARRAY[]::text[])"#
    )
    .bind(entry_id)
    .bind(contact_id)
    .bind(campaign_id)
    .execute(pool)
    .await?;

    Ok(entry_id)
}

/// Get all entry IDs for a campaign (used for draw).
pub async fn get_entries_for_draw(
    pool: &PgPool,
    campaign_id: &Uuid,
) -> Result<Vec<Uuid>, AppError> {
    let entry_ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM entries WHERE campaign_id = $1 ORDER BY created_at"
    )
    .bind(campaign_id)
    .fetch_all(pool)
    .await?;

    Ok(entry_ids)
}

/// Record a draw winner and seed.
pub async fn record_draw(
    pool: &PgPool,
    campaign_id: &Uuid,
    winner_entry_id: &Uuid,
    seed: u64,
) -> Result<(), AppError> {
    // Update the entry outcome to "winner"
    sqlx::query(
        "UPDATE entries SET outcome = 'winner' WHERE id = $1"
    )
    .bind(winner_entry_id)
    .execute(pool)
    .await?;

    // Store seed in campaign config
    sqlx::query(
        "UPDATE campaigns SET config = jsonb_set(COALESCE(config, '{}'::jsonb), '{draw_seed}', to_jsonb($1::text)::jsonb) WHERE id = $2"
    )
    .bind(seed.to_string())
    .bind(campaign_id)
    .execute(pool)
    .await?;

    Ok(())
}

/// Check if a campaign already has a draw seed (has been drawn before).
pub async fn has_existing_draw(
    pool: &PgPool,
    campaign_id: &Uuid,
) -> Result<bool, AppError> {
    let seed: Option<serde_json::Value> = sqlx::query_scalar(
        "SELECT config->>'draw_seed' FROM campaigns WHERE id = $1"
    )
    .bind(campaign_id)
    .fetch_optional(pool)
    .await?
    .flatten();

    Ok(seed.is_some())
}
