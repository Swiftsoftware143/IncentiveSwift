//! Pity timer — guarantees a win after N consecutive losses per contact per campaign.
//!
//! Configured via campaign.config.pity_timer:
//! ```json
//! {
//!   "pity_timer": {
//!     "enabled": true,
//!     "threshold": 5,
//!     "force_win_outcome": "winner",
//!     "force_win_tag": "Campaign_Winner"
//!   }
//! }
//! ```

use crate::error::AppError;
use serde_json::Value as JsonValue;
use sqlx::PgPool;
use uuid::Uuid;

/// Check pity timer: if the contact has hit the loss streak threshold,
/// force a win and reset the streak. Otherwise increment the streak.
/// Returns (forced_win, outcome, tag) where forced_win is true if the
/// pity timer triggered.
pub async fn apply_pity_timer(
    pool: &PgPool,
    campaign_id: &Uuid,
    contact_id: &Uuid,
    campaign_config: &JsonValue,
    tag_namespace: &str,
    base_outcome: &str,
    base_tags: &[String],
) -> Result<(bool, String, Vec<String>), AppError> {
    // Check if pity timer is configured
    let pity_config = match campaign_config.get("pity_timer") {
        Some(c) => c,
        None => return Ok((false, base_outcome.to_string(), base_tags.to_vec())),
    };

    let enabled = pity_config
        .get("enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if !enabled {
        return Ok((false, base_outcome.to_string(), base_tags.to_vec()));
    }

    let threshold = pity_config
        .get("threshold")
        .and_then(|v| v.as_i64())
        .unwrap_or(5);

    // Read current streak
    let current_streak: Option<i32> = sqlx::query_scalar(
        "SELECT loss_streak FROM campaign_streaks WHERE contact_id = $1 AND campaign_id = $2",
    )
    .bind(contact_id)
    .bind(campaign_id)
    .fetch_optional(pool)
    .await?
    .flatten();

    let streak = current_streak.unwrap_or(0);

    // Check if the current entry is already a win outcome
    let is_win =
        base_outcome == "winner" || base_outcome == "grand_prize" || base_outcome == "runner_up";

    if is_win {
        // Win resets the streak
        sqlx::query(
            "INSERT INTO campaign_streaks (contact_id, campaign_id, loss_streak, last_entry_at)
             VALUES ($1, $2, 0, now())
             ON CONFLICT (contact_id, campaign_id)
             DO UPDATE SET loss_streak = 0, last_entry_at = now()",
        )
        .bind(contact_id)
        .bind(campaign_id)
        .execute(pool)
        .await?;
        return Ok((false, base_outcome.to_string(), base_tags.to_vec()));
    }

    // Not a win — check if pity timer should trigger
    if streak + 1 >= threshold as i32 {
        // Force win
        let force_outcome = pity_config
            .get("force_win_outcome")
            .and_then(|v| v.as_str())
            .unwrap_or("winner")
            .to_string();
        let force_tag = pity_config
            .get("force_win_tag")
            .and_then(|v| v.as_str())
            .map(|t| t.to_string())
            .unwrap_or_else(|| format!("{}_Winner", tag_namespace));

        // Reset streak
        sqlx::query(
            "INSERT INTO campaign_streaks (contact_id, campaign_id, loss_streak, last_entry_at)
             VALUES ($1, $2, 0, now())
             ON CONFLICT (contact_id, campaign_id)
             DO UPDATE SET loss_streak = 0, last_entry_at = now()",
        )
        .bind(contact_id)
        .bind(campaign_id)
        .execute(pool)
        .await?;

        tracing::info!(
            "Pity timer triggered for contact {} on campaign {} (streak {})",
            contact_id,
            campaign_id,
            streak
        );

        return Ok((true, force_outcome, vec![force_tag]));
    }

    // Increment loss streak
    let new_streak = streak + 1;
    sqlx::query(
        "INSERT INTO campaign_streaks (contact_id, campaign_id, loss_streak, last_entry_at)
         VALUES ($1, $2, $3, now())
         ON CONFLICT (contact_id, campaign_id)
         DO UPDATE SET loss_streak = $3, last_entry_at = now()",
    )
    .bind(contact_id)
    .bind(campaign_id)
    .bind(new_streak)
    .execute(pool)
    .await?;

    Ok((false, base_outcome.to_string(), base_tags.to_vec()))
}

/// Check and enforce max spins per day for a contact on a campaign.
/// Returns Ok(()) if within limit, Err if limit exceeded.
pub async fn check_daily_limit(
    pool: &PgPool,
    campaign_id: &Uuid,
    contact_id: &Uuid,
    campaign_config: &JsonValue,
) -> Result<(), AppError> {
    let max_spins = match campaign_config.get("max_spins_per_day") {
        Some(v) => v.as_i64().unwrap_or(0),
        None => return Ok(()), // No limit configured
    };

    if max_spins <= 0 {
        return Ok(()); // Disabled
    }

    let today: i64 = sqlx::query_scalar(
        "SELECT entry_count FROM campaign_daily_limits
         WHERE contact_id = $1 AND campaign_id = $2 AND entry_date = CURRENT_DATE",
    )
    .bind(contact_id)
    .bind(campaign_id)
    .fetch_optional(pool)
    .await?
    .flatten()
    .unwrap_or(0);

    if today >= max_spins {
        return Err(AppError::Forbidden(format!(
            "Daily spin limit reached ({}). Try again tomorrow.",
            max_spins
        )));
    }

    Ok(())
}

/// Record a daily spin entry (called after successful entry creation).
pub async fn record_daily_spin(
    pool: &PgPool,
    campaign_id: &Uuid,
    contact_id: &Uuid,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO campaign_daily_limits (contact_id, campaign_id, entry_date, entry_count)
         VALUES ($1, $2, CURRENT_DATE, 1)
         ON CONFLICT (contact_id, campaign_id, entry_date)
         DO UPDATE SET entry_count = campaign_daily_limits.entry_count + 1",
    )
    .bind(contact_id)
    .bind(campaign_id)
    .execute(pool)
    .await?;

    Ok(())
}
