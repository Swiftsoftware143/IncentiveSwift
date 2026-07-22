//! Prize draw engine for multi-spin weighted prize pools.
//!
//! Supports:
//! - Weighted random prize selection (cumulative distribution)
//! - Pity timer integration (forces win after N consecutive losses)
//! - Inventory tracking (decrement remaining, block if exhausted)
//! - Daily and campaign-level spin limits
//! - Seeded RNG per draw for auditability

use crate::error::AppError;
use rand::{Rng, SeedableRng};
use rand::rngs::StdRng;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use sqlx::PgPool;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// A single prize definition from campaign config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrizeConfig {
    pub id: String,
    pub label: String,
    pub color: Option<String>,
    pub weight: u32,
    pub prize_type: String,
    pub inventory: Option<i64>,
    pub marketing_boost: Option<serde_json::Value>,
}

/// The prize pool section of campaign config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrizePoolConfig {
    pub prizes: Vec<PrizeConfig>,
    #[serde(default = "default_total_weight")]
    pub total_weight: u32,
    #[serde(default)]
    pub inventory_tracking: bool,
    #[serde(default)]
    pub allow_when_exhausted: bool,
}

fn default_total_weight() -> u32 {
    100
}

/// The pity timer section of campaign config.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PityTimerConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_threshold")]
    pub threshold: u32,
}

fn default_threshold() -> u32 {
    5
}

/// Result of a single prize draw spin.
#[derive(Debug, Clone, Serialize)]
pub struct PrizeDrawResult {
    pub prize_id: String,
    pub prize_label: String,
    pub prize_type: String,
    pub color: String,
    pub won: bool,
    pub was_pity: bool,
    pub streak: i32,
    pub total_spins: i32,
    pub remaining_daily_spins: i64,
    pub remaining_campaign_spins: i64,
}

/// Inventory row for a prize in a campaign.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PrizeInventoryRow {
    pub id: Uuid,
    pub campaign_id: Uuid,
    pub prize_id: String,
    pub label: String,
    pub prize_type: String,
    pub total: Option<i32>,
    pub remaining: Option<i32>,
    pub claimed: i32,
    pub color: Option<String>,
    pub marketing_boost: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Weighted Random Draw
// ---------------------------------------------------------------------------

/// Perform a weighted random selection from the prize pool.
/// Returns a reference to the selected prize, or None if pool is empty.
pub fn weighted_random_draw(prizes: &[PrizeConfig]) -> Option<&PrizeConfig> {
    if prizes.is_empty() {
        return None;
    }

    // Build cumulative distribution
    let total_weight: u64 = prizes.iter().map(|p| p.weight as u64).sum();
    if total_weight == 0 {
        // Edge case: all weights zero — pick uniform at random
        let mut rng = create_rng();
        let idx = rng.gen_range(0..prizes.len());
        return Some(&prizes[idx]);
    }

    // Seed RNG with current timestamp for non-deterministic draws
    let mut rng = create_rng();
    let pick: u64 = rng.gen_range(0..total_weight);

    // Walk cumulative distribution
    let mut cumulative: u64 = 0;
    for prize in prizes {
        cumulative += prize.weight as u64;
        if pick < cumulative {
            return Some(prize);
        }
    }

    // Fallback (shouldn't reach here)
    Some(&prizes[prizes.len() - 1])
}

fn create_rng() -> StdRng {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    StdRng::seed_from_u64(nanos as u64)
}

// ---------------------------------------------------------------------------
// Inventory helpers
// ---------------------------------------------------------------------------

/// Check if inventory exists and has remaining stock for a prize.
async fn check_inventory(
    pool: &PgPool,
    campaign_id: &Uuid,
    prize_id: &str,
) -> Result<Option<PrizeInventoryRow>, AppError> {
    let inv = sqlx::query_as::<_, PrizeInventoryRow>(
        r#"SELECT id, campaign_id, prize_id, label, prize_type, total, remaining, claimed, color, marketing_boost
           FROM campaign_prize_inventory
           WHERE campaign_id = $1 AND prize_id = $2"#
    )
    .bind(campaign_id)
    .bind(prize_id)
    .fetch_optional(pool)
    .await?;

    Ok(inv)
}

/// Initialize inventory for a prize (insert if not exists).
pub async fn ensure_inventory(
    pool: &PgPool,
    campaign_id: &Uuid,
    prize: &PrizeConfig,
) -> Result<(), AppError> {
    // Check if inventory row already exists
    let exists: Option<PrizeInventoryRow> = sqlx::query_as::<_, PrizeInventoryRow>(
        r#"SELECT id, campaign_id, prize_id, label, prize_type, total, remaining, claimed, color, marketing_boost
           FROM campaign_prize_inventory
           WHERE campaign_id = $1 AND prize_id = $2"#
    )
    .bind(campaign_id)
    .bind(&prize.id)
    .fetch_optional(pool)
    .await?;

    if exists.is_some() {
        return Ok(());
    }

    let color = prize.color.clone().unwrap_or_else(|| "#6b7280".to_string());
    let inv_i32: Option<i32> = prize.inventory.map(|v| v as i32);

    sqlx::query(
        r#"INSERT INTO campaign_prize_inventory (campaign_id, prize_id, label, prize_type, total, remaining, color, marketing_boost)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8)"#
    )
    .bind(campaign_id)
    .bind(&prize.id)
    .bind(&prize.label)
    .bind(&prize.prize_type)
    .bind(inv_i32)
    .bind(inv_i32)
    .bind(&color)
    .bind(&prize.marketing_boost)
    .execute(pool)
    .await?;

    Ok(())
}

/// Decrement inventory for a prize. Returns Ok(true) if decremented, Ok(false) if exhausted.
async fn decrement_inventory(
    pool: &PgPool,
    campaign_id: &Uuid,
    prize_id: &str,
) -> Result<bool, AppError> {
    let result = sqlx::query(
        r#"UPDATE campaign_prize_inventory
           SET remaining = CASE
               WHEN remaining IS NOT NULL AND remaining > 0 THEN remaining - 1
               ELSE remaining
           END,
           claimed = claimed + 1,
           updated_at = now()
           WHERE campaign_id = $1 AND prize_id = $2
           AND (remaining IS NULL OR remaining > 0)"#
    )
    .bind(campaign_id)
    .bind(prize_id)
    .execute(pool)
    .await?;

    Ok(result.rows_affected() > 0)
}

// ---------------------------------------------------------------------------
// Streak helpers
// ---------------------------------------------------------------------------

/// Read current loss_streak and total_spins for a contact on a campaign.
async fn read_streaks(
    pool: &PgPool,
    campaign_id: &Uuid,
    contact_id: &Uuid,
) -> Result<(i32, i32), AppError> {
    let row = sqlx::query_as::<_, (i32, i32)>(
        r#"SELECT COALESCE(loss_streak, 0), COALESCE(total_spins, 0)
           FROM campaign_streaks
           WHERE contact_id = $1 AND campaign_id = $2"#
    )
    .bind(contact_id)
    .bind(campaign_id)
    .fetch_optional(pool)
    .await?;

    Ok(row.unwrap_or((0, 0)))
}

/// Update streaks after a spin.
async fn update_streaks(
    pool: &PgPool,
    campaign_id: &Uuid,
    contact_id: &Uuid,
    won: bool,
) -> Result<(i32, i32), AppError> {
    if won {
        // Win: reset streak to 0, increment total_spins
        let row = sqlx::query_as::<_, (i32, i32)>(
            r#"INSERT INTO campaign_streaks (contact_id, campaign_id, loss_streak, total_spins, last_entry_at, last_spin_at)
               VALUES ($1, $2, 0, 1, now(), now())
               ON CONFLICT (contact_id, campaign_id)
               DO UPDATE SET loss_streak = 0,
                             total_spins = campaign_streaks.total_spins + 1,
                             last_entry_at = now(),
                             last_spin_at = now()
               RETURNING loss_streak, total_spins"#
        )
        .bind(contact_id)
        .bind(campaign_id)
        .fetch_one(pool)
        .await?;
        Ok(row)
    } else {
        // Loss: increment streak and total_spins
        let row = sqlx::query_as::<_, (i32, i32)>(
            r#"INSERT INTO campaign_streaks (contact_id, campaign_id, loss_streak, total_spins, last_entry_at, last_spin_at)
               VALUES ($1, $2, 1, 1, now(), now())
               ON CONFLICT (contact_id, campaign_id)
               DO UPDATE SET loss_streak = campaign_streaks.loss_streak + 1,
                             total_spins = campaign_streaks.total_spins + 1,
                             last_entry_at = now(),
                             last_spin_at = now()
               RETURNING loss_streak, total_spins"#
        )
        .bind(contact_id)
        .bind(campaign_id)
        .fetch_one(pool)
        .await?;
        Ok(row)
    }
}

// ---------------------------------------------------------------------------
// Spin limit checks
// ---------------------------------------------------------------------------

/// Check daily spin limit for a contact on a campaign.
async fn check_daily_spin_limit(
    pool: &PgPool,
    campaign_id: &Uuid,
    contact_id: &Uuid,
    max_per_day: i64,
) -> Result<(), AppError> {
    if max_per_day <= 0 {
        return Ok(());
    }

    let today_count: i32 = sqlx::query_scalar(
        r#"SELECT COALESCE(entry_count, 0)
           FROM campaign_daily_limits
           WHERE contact_id = $1 AND campaign_id = $2 AND entry_date = CURRENT_DATE"#
    )
    .bind(contact_id)
    .bind(campaign_id)
    .fetch_optional(pool)
    .await?
    .flatten()
    .unwrap_or(0);

    if (today_count as i64) >= max_per_day {
        return Err(AppError::Forbidden(format!(
            "Daily spin limit reached ({}). Try again tomorrow.",
            max_per_day
        )));
    }

    Ok(())
}

/// Check campaign-level spin limit for a contact.
async fn check_campaign_spin_limit(
    pool: &PgPool,
    campaign_id: &Uuid,
    contact_id: &Uuid,
    max_per_campaign: i64,
) -> Result<(), AppError> {
    if max_per_campaign <= 0 {
        return Ok(());
    }

    let total: i32 = sqlx::query_scalar(
        r#"SELECT COALESCE(total_spins, 0)
           FROM campaign_streaks
           WHERE contact_id = $1 AND campaign_id = $2"#
    )
    .bind(contact_id)
    .bind(campaign_id)
    .fetch_optional(pool)
    .await?
    .flatten()
    .unwrap_or(0);

    if total as i64 >= max_per_campaign {
        return Err(AppError::Forbidden(format!(
            "Campaign spin limit reached ({}). No more spins available.",
            max_per_campaign
        )));
    }

    Ok(())
}

/// Record a daily spin entry in the limits table.
async fn record_daily_spin(
    pool: &PgPool,
    campaign_id: &Uuid,
    contact_id: &Uuid,
) -> Result<(), AppError> {
    sqlx::query(
        r#"INSERT INTO campaign_daily_limits (contact_id, campaign_id, entry_date, entry_count)
           VALUES ($1, $2, CURRENT_DATE, 1)
           ON CONFLICT (contact_id, campaign_id, entry_date)
           DO UPDATE SET entry_count = campaign_daily_limits.entry_count + 1"#
    )
    .bind(contact_id)
    .bind(campaign_id)
    .execute(pool)
    .await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Record win
// ---------------------------------------------------------------------------

/// Update the latest win for a contact+campaign with a redemption code.
pub async fn set_redemption_code(
    pool: &PgPool,
    campaign_id: &Uuid,
    contact_id: &Uuid,
    prize_id: &str,
    code: &str,
) -> Result<(), AppError> {
    sqlx::query(
        r#"UPDATE campaign_wins
           SET redemption_code = $1
           WHERE campaign_id = $2 AND contact_id = $3 AND prize_id = $4
             AND redemption_code IS NULL
        "#
    )
    .bind(code)
    .bind(campaign_id)
    .bind(contact_id)
    .bind(prize_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Record a win in campaign_wins table and optionally create an entry.
pub async fn record_win(
    pool: &PgPool,
    campaign_id: &Uuid,
    contact_id: &Uuid,
    prize: &PrizeConfig,
    streak: i32,
    was_pity: bool,
    utm_source: Option<String>,
    utm_medium: Option<String>,
    utm_campaign: Option<String>,
    referrer_url: Option<String>,
    page_url: Option<String>,
    user_agent: Option<String>,
    ip_address: Option<String>,
) -> Result<Uuid, AppError> {
    let entry_id = Uuid::new_v4();
    let answers = serde_json::json!({
        "prize_id": prize.id,
        "prize_label": prize.label,
        "prize_type": prize.prize_type,
        "was_pity": was_pity,
        "streak": streak,
    });

    sqlx::query(
        r#"INSERT INTO entries (id, contact_id, campaign_id, answers, outcome, tags_applied,
            utm_source, utm_medium, utm_campaign, referrer_url, page_url, user_agent, ip_address)
           VALUES ($1, $2, $3, $4, 'winner', $5, $6, $7, $8, $9, $10, $11, $12)"#
    )
    .bind(entry_id)
    .bind(contact_id)
    .bind(campaign_id)
    .bind(&answers)
    .bind(&vec!["Prize_Winner".to_string()])
    .bind(&utm_source)
    .bind(&utm_medium)
    .bind(&utm_campaign)
    .bind(&referrer_url)
    .bind(&page_url)
    .bind(&user_agent)
    .bind(&ip_address)
    .execute(pool)
    .await?;

    sqlx::query(
        r#"INSERT INTO campaign_wins (entry_id, contact_id, campaign_id, prize_id, prize_label, prize_type, streak_when_won, was_pity)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8)"#
    )
    .bind(entry_id)
    .bind(contact_id)
    .bind(campaign_id)
    .bind(&prize.id)
    .bind(&prize.label)
    .bind(&prize.prize_type)
    .bind(streak)
    .bind(was_pity)
    .execute(pool)
    .await?;

    Ok(entry_id)
}

/// Record a loss (no-win outcome) as an entry with entrant/background outcome.
pub async fn record_loss(
    pool: &PgPool,
    campaign_id: &Uuid,
    contact_id: &Uuid,
    prize: &PrizeConfig,
    streak: i32,
    utm_source: Option<String>,
    utm_medium: Option<String>,
    utm_campaign: Option<String>,
    referrer_url: Option<String>,
    page_url: Option<String>,
    user_agent: Option<String>,
    ip_address: Option<String>,
) -> Result<Uuid, AppError> {
    let entry_id = Uuid::new_v4();
    let answers = serde_json::json!({
        "prize_id": prize.id,
        "prize_label": prize.label,
        "prize_type": prize.prize_type,
        "streak": streak,
    });

    sqlx::query(
        r#"INSERT INTO entries (id, contact_id, campaign_id, answers, outcome, tags_applied,
            utm_source, utm_medium, utm_campaign, referrer_url, page_url, user_agent, ip_address)
           VALUES ($1, $2, $3, $4, 'entrant', $5, $6, $7, $8, $9, $10, $11, $12)"#
    )
    .bind(entry_id)
    .bind(contact_id)
    .bind(campaign_id)
    .bind(&answers)
    .bind(&vec!["Spin_Entrant".to_string()])
    .bind(&utm_source)
    .bind(&utm_medium)
    .bind(&utm_campaign)
    .bind(&referrer_url)
    .bind(&page_url)
    .bind(&user_agent)
    .bind(&ip_address)
    .execute(pool)
    .await?;

    Ok(entry_id)
}

/// Check which prizes have exhausted inventory (for filtering before draw).
async fn get_exhausted_prize_ids(
    pool: &PgPool,
    campaign_id: &Uuid,
    prizes: &[PrizeConfig],
) -> Result<Vec<String>, AppError> {
    let mut exhausted = Vec::new();
    for p in prizes {
        if p.inventory.is_none() || p.prize_type == "lose" {
            continue;
        }
        let inv = check_inventory(pool, campaign_id, &p.id).await?;
        match inv {
            Some(row) => {
                if row.remaining.unwrap_or(0) <= 0 {
                    exhausted.push(p.id.clone());
                }
            }
            None => {
                // No row means not initialized — treat as having full inventory
            }
        }
    }
    Ok(exhausted)
}

// ---------------------------------------------------------------------------
// Main prize draw orchestrator
// ---------------------------------------------------------------------------

/// Execute a full prize draw spin for a contact on a campaign.
pub async fn apply_prize_draw(
    pool: &PgPool,
    campaign_id: &Uuid,
    contact_id: &Uuid,
    campaign_config: &JsonValue,
    utm_source: Option<String>,
    utm_medium: Option<String>,
    utm_campaign: Option<String>,
    referrer_url: Option<String>,
    page_url: Option<String>,
    user_agent: Option<String>,
    ip_address: Option<String>,
) -> Result<PrizeDrawResult, AppError> {
    // --- Parse configs ---
    let prize_pool_config: PrizePoolConfig = match campaign_config.get("prize_pool") {
        Some(pp) => serde_json::from_value(pp.clone())
            .map_err(|e| AppError::BadRequest(format!("Invalid prize_pool config: {}", e)))?,
        None => return Err(AppError::BadRequest(
            "Campaign has no prize_pool configured".to_string()
        )),
    };

    let pity_config: PityTimerConfig = match campaign_config.get("pity_timer") {
        Some(pt) => serde_json::from_value(pt.clone())
            .unwrap_or_default(),
        None => PityTimerConfig::default(),
    };

    let max_spins_per_day = campaign_config.get("max_spins_per_day")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);

    let max_spins_per_campaign = campaign_config.get("max_spins_per_campaign")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);

    let inventory_tracking = prize_pool_config.inventory_tracking;
    let allow_when_exhausted = prize_pool_config.allow_when_exhausted;

    // --- Ensure inventory rows exist for all trackable prizes ---
    if inventory_tracking {
        for prize in &prize_pool_config.prizes {
            if prize.prize_type != "lose" && prize.inventory.is_some() {
                if let Err(e) = ensure_inventory(pool, campaign_id, prize).await {
                    tracing::warn!("Failed to init inventory for prize {}: {}", prize.id, e);
                }
            }
        }
    }

    // --- Check daily spin limit ---
    check_daily_spin_limit(pool, campaign_id, contact_id, max_spins_per_day).await?;

    // --- Check campaign spin limit ---
    check_campaign_spin_limit(pool, campaign_id, contact_id, max_spins_per_campaign).await?;

    // --- Read current streaks ---
    let (current_streak, _total_spins) = read_streaks(pool, campaign_id, contact_id).await?;

    // --- Determine if pity timer should trigger ---
    let pity_should_trigger = pity_config.enabled && (current_streak + 1) >= pity_config.threshold as i32;

    // --- Determine which prizes are available (not exhausted) ---
    let exhausted_prize_ids = if inventory_tracking {
        get_exhausted_prize_ids(pool, campaign_id, &prize_pool_config.prizes).await?
    } else {
        Vec::new()
    };

    // --- Build available prize list ---
    let make_available = |prizes: &[PrizeConfig]| -> Vec<PrizeConfig> {
        prizes.iter()
            .filter(|p| {
                if inventory_tracking && !allow_when_exhausted
                    && p.inventory.is_some() && exhausted_prize_ids.contains(&p.id)
                {
                    return false;
                }
                true
            })
            .cloned()
            .collect()
    };

    // --- Select prize ---
    let (selected_prize, was_pity) = if pity_should_trigger {
        // Pity: force a win from non-lose available prizes
        let available: Vec<PrizeConfig> = prize_pool_config.prizes.iter()
            .filter(|p| p.prize_type != "lose")
            .filter(|p| {
                if inventory_tracking && !allow_when_exhausted
                    && p.inventory.is_some() && exhausted_prize_ids.contains(&p.id)
                {
                    return false;
                }
                true
            })
            .cloned()
            .collect();

        let eligible = if available.is_empty() {
            // Fallback: any non-lose prize, even if exhausted
            prize_pool_config.prizes.iter()
                .find(|p| p.prize_type != "lose")
                .cloned()
                .unwrap_or_else(|| prize_pool_config.prizes[0].clone())
        } else {
            weighted_random_draw(&available)
                .cloned()
                .unwrap_or_else(|| available[0].clone())
        };

        (eligible, true)
    } else {
        // Normal weighted draw
        let available = make_available(&prize_pool_config.prizes);

        if available.is_empty() {
            return Err(AppError::Forbidden(
                "All prizes are exhausted. No spins available.".to_string()
            ));
        }

        let selected = weighted_random_draw(&available)
            .ok_or_else(|| AppError::BadRequest("No eligible prizes in pool".to_string()))?;

        (selected.clone(), false)
    };

    // --- Determine if this is a win ---
    let won = selected_prize.prize_type != "lose";

    // --- Handle inventory ---
    if won && inventory_tracking && selected_prize.inventory.is_some() {
        let decremented = decrement_inventory(pool, campaign_id, &selected_prize.id).await?;
        if !decremented {
            if allow_when_exhausted {
                // Allow the win anyway — inventory may go negative
            } else {
                return Err(AppError::Forbidden(
                    "Prize inventory exhausted. Please try again.".to_string()
                ));
            }
        }
    }

    // --- Update streaks ---
    let (new_streak, new_total_spins) = update_streaks(pool, campaign_id, contact_id, won).await?;

    // --- Record entry (always) ---
    if won {
        record_win(
            pool, campaign_id, contact_id, &selected_prize, new_streak, was_pity,
            utm_source, utm_medium, utm_campaign, referrer_url, page_url, user_agent, ip_address,
        ).await?;
    } else {
        record_loss(
            pool, campaign_id, contact_id, &selected_prize, new_streak,
            utm_source, utm_medium, utm_campaign, referrer_url, page_url, user_agent, ip_address,
        ).await?;
    }

    // --- Record daily spin ---
    record_daily_spin(pool, campaign_id, contact_id).await?;

    // --- Calculate remaining spins ---
    let remaining_daily = if max_spins_per_day > 0 {
        let today_count_i: i32 = sqlx::query_scalar(
            r#"SELECT COALESCE(entry_count, 0)
               FROM campaign_daily_limits
               WHERE contact_id = $1 AND campaign_id = $2 AND entry_date = CURRENT_DATE"#
        )
        .bind(contact_id)
        .bind(campaign_id)
        .fetch_optional(pool)
        .await?
        .flatten()
        .unwrap_or(0);
        (max_spins_per_day - today_count_i as i64).max(0)
    } else {
        -1 // Unlimited
    };

    let remaining_campaign = if max_spins_per_campaign > 0 {
        (max_spins_per_campaign - new_total_spins as i64).max(0)
    } else {
        -1 // Unlimited
    };

    let color = selected_prize.color.clone().unwrap_or_else(|| {
        if won { "#22c55e".to_string() } else { "#6b7280".to_string() }
    });

    Ok(PrizeDrawResult {
        prize_id: selected_prize.id.clone(),
        prize_label: selected_prize.label.clone(),
        prize_type: selected_prize.prize_type.clone(),
        color,
        won,
        was_pity,
        streak: new_streak,
        total_spins: new_total_spins,
        remaining_daily_spins: remaining_daily,
        remaining_campaign_spins: remaining_campaign,
    })
}

/// Get spin status for a contact on a campaign (no side effects).
pub async fn get_spin_status(
    pool: &PgPool,
    campaign_id: &Uuid,
    contact_id: &Uuid,
    campaign_config: &JsonValue,
) -> Result<PrizeDrawResult, AppError> {
    let (streak, total_spins) = read_streaks(pool, campaign_id, contact_id).await?;

    let max_per_day = campaign_config.get("max_spins_per_day")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);

    let max_per_campaign = campaign_config.get("max_spins_per_campaign")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);

    let remaining_daily = if max_per_day > 0 {
        let today_count_i: i32 = sqlx::query_scalar(
            r#"SELECT COALESCE(entry_count, 0)
               FROM campaign_daily_limits
               WHERE contact_id = $1 AND campaign_id = $2 AND entry_date = CURRENT_DATE"#
        )
        .bind(contact_id)
        .bind(campaign_id)
        .fetch_optional(pool)
        .await?
        .flatten()
        .unwrap_or(0);
        (max_per_day - today_count_i as i64).max(0)
    } else {
        -1
    };

    let remaining_campaign = if max_per_campaign > 0 {
        (max_per_campaign - total_spins as i64).max(0)
    } else {
        -1
    };

    Ok(PrizeDrawResult {
        prize_id: String::new(),
        prize_label: String::new(),
        prize_type: "status".to_string(),
        color: "#6b7280".to_string(),
        won: false,
        was_pity: false,
        streak,
        total_spins,
        remaining_daily_spins: remaining_daily,
        remaining_campaign_spins: remaining_campaign,
    })
}
