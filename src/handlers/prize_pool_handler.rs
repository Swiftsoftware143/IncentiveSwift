//! The prize pool — what a spin campaign cannot run without, and what nothing could write.
//!
//! Measured 2026-10-01 on the live app: `POST /api/v1/campaigns/<slug>/spin` answered
//!
//! ```text
//! 400 {"code":400,"error":"Campaign has no prize_pool configured"}
//! ```
//!
//! for the only two campaigns that exist, so the core loyalty mechanic could not run at all —
//! product-wide, for every campaign, since none had ever been given a prize.
//!
//! The read side has always been there (`src/mechanics/prize_draw.rs:581`:
//! `campaign_config.get("prize_pool")`), the campaign-creation default writes
//! `"prize_pool": { "prizes": [] }`, and **nothing in the codebase ever put a prize in it**. A config
//! that is created empty with no way to fill it is the same defect as a route with no caller: the
//! capability reads as built and can never be used.
//!
//! This is the writer. It is deliberately NOT a raw `config` passthrough: the draw indexes
//! `prize_pool.prizes[0]` as a fallback and divides by `total_weight`, so an empty list or a
//! zero total is a panic or a 500 waiting to happen. Weights are summed here rather than trusted from
//! the client, ids are made unique, and the odds that result are returned so the screen can show the
//! operator what they just configured instead of making them infer it.

use crate::error::AppError;
use crate::security::auth::AuthenticatedUser;
use crate::state::AppState;
use axum::{
    extract::{Path, State},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

/// The prize types the draw treats specially. `lose` is deliberately weighted-but-not-won and is
/// skipped by the inventory check (`prize_draw.rs:544`), so it must stay spellable.
const PRIZE_TYPES: &[&str] = &["prize", "lose", "coupon", "points", "physical", "discount"];

#[derive(Deserialize)]
pub struct PrizePoolBody {
    pub prizes: Vec<PrizeIn>,
    #[serde(default)]
    pub inventory_tracking: bool,
    #[serde(default)]
    pub allow_when_exhausted: bool,
}

#[derive(Deserialize)]
pub struct PrizeIn {
    /// Stable id the draw stores against a win and the inventory table keys on. Optional so the
    /// screen can add a prize and save without inventing one.
    #[serde(default)]
    pub id: Option<String>,
    pub label: String,
    #[serde(default)]
    pub color: Option<String>,
    pub weight: i64,
    #[serde(default)]
    pub prize_type: Option<String>,
    #[serde(default)]
    pub inventory: Option<i64>,
    #[serde(default)]
    pub marketing_boost: Option<Value>,
}

fn slugify_id(label: &str, taken: &[String], index: usize) -> String {
    let mut base: String = label
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    while base.contains("--") {
        base = base.replace("--", "-");
    }
    let base = base.trim_matches('-').to_string();
    let mut candidate = if base.is_empty() {
        format!("prize-{}", index + 1)
    } else {
        base
    };
    // Two prizes labelled the same must still be two distinct ids: the id is the key a win is
    // recorded against, so a duplicate silently makes the second prize unreachable.
    let mut n = 2;
    while taken.contains(&candidate) {
        candidate = format!(
            "{}-{}",
            candidate.trim_end_matches(&format!("-{}", n - 1)),
            n
        );
        n += 1;
    }
    candidate
}

/// PUT /api/v1/campaigns/:slug/prize-pool — set the prize pool (authenticated).
pub async fn set_prize_pool(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(slug): Path<String>,
    Json(body): Json<PrizePoolBody>,
) -> Result<Json<Value>, AppError> {
    let account_id = uuid::Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;

    // Scoped to the caller's own account: a campaign belonging to someone else must be a 404, not a
    // successful write. The slug is caller-supplied, so resolving it unscoped would be an id guess
    // away from editing another tenant's campaign.
    let campaign_id: Option<uuid::Uuid> = sqlx::query_scalar(
        "SELECT id FROM campaigns WHERE (slug = $1 OR id::text = $1) AND account_id = $2",
    )
    .bind(&slug)
    .bind(account_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| AppError::Internal(format!("Failed to resolve campaign: {e}")))?;

    let Some(campaign_id) = campaign_id else {
        return Err(AppError::NotFound("Campaign not found".to_string()));
    };

    if body.prizes.is_empty() {
        return Err(AppError::BadRequest(
            "A prize pool needs at least one prize — a campaign with none cannot be drawn."
                .to_string(),
        ));
    }

    let mut taken: Vec<String> = Vec::new();
    let mut prizes: Vec<Value> = Vec::with_capacity(body.prizes.len());
    let mut total_weight: i64 = 0;

    for (i, p) in body.prizes.iter().enumerate() {
        let label = p.label.trim();
        if label.is_empty() {
            return Err(AppError::BadRequest(format!(
                "Prize {} has no label — an unlabelled prize cannot be announced to a winner.",
                i + 1
            )));
        }
        if p.weight <= 0 {
            return Err(AppError::BadRequest(format!(
                "\"{}\" has a weight of {} — every prize needs a weight of at least 1, or it can never be drawn.",
                label, p.weight
            )));
        }
        if p.inventory.is_some_and(|n| n < 0) {
            return Err(AppError::BadRequest(format!(
                "\"{}\" has negative inventory.",
                label
            )));
        }
        let prize_type = p
            .prize_type
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("prize")
            .to_lowercase();
        if !PRIZE_TYPES.contains(&prize_type.as_str()) {
            return Err(AppError::BadRequest(format!(
                "\"{}\" has prize type \"{}\", which the draw does not know. Use one of: {}.",
                label,
                prize_type,
                PRIZE_TYPES.join(", ")
            )));
        }

        let id = match p.id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(given) => {
                if taken.iter().any(|t| t == given) {
                    return Err(AppError::BadRequest(format!(
                        "Two prizes share the id \"{}\" — a win is recorded against that id, so the second would be unreachable.",
                        given
                    )));
                }
                given.to_string()
            }
            None => slugify_id(label, &taken, i),
        };
        taken.push(id.clone());
        total_weight += p.weight;

        let mut prize = json!({
            "id": id,
            "label": label,
            "weight": p.weight,
            "prize_type": prize_type,
        });
        if let Some(c) = p.color.as_deref().filter(|s| !s.trim().is_empty()) {
            prize["color"] = json!(c);
        }
        if let Some(n) = p.inventory {
            prize["inventory"] = json!(n);
        }
        if let Some(mb) = &p.marketing_boost {
            prize["marketing_boost"] = mb.clone();
        }
        prizes.push(prize);
    }

    // The draw divides by `total_weight` and falls back to `prizes[0]`, so the sum is computed here
    // from the weights that will actually be stored — never taken from the request.
    let pool = json!({
        "prizes": prizes,
        "total_weight": total_weight,
        "inventory_tracking": body.inventory_tracking,
        "allow_when_exhausted": body.allow_when_exhausted,
    });

    let updated: Option<uuid::Uuid> = sqlx::query_scalar(
        // `campaigns` has NO `updated_at` column (verified with \\d campaigns 2026-10-01, after
        // shipping an UPDATE that set one and taking a 500 for it) — the table carries `created_at`
        // only. The handler's own error path is what named the column, so the failure was honest
        // rather than silent; do not add a column here that the schema does not have.
        "UPDATE campaigns
            SET config = jsonb_set(COALESCE(config, '{}'::jsonb), '{prize_pool}', $2::jsonb, true)
          WHERE id = $1
      RETURNING id",
    )
    .bind(campaign_id)
    .bind(&pool)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| AppError::Internal(format!("Failed to save the prize pool: {e}")))?;

    if updated.is_none() {
        return Err(AppError::NotFound("Campaign not found".to_string()));
    }

    Ok(Json(json!({
        "campaign_id": campaign_id.to_string(),
        "prize_pool": pool,
        "prize_count": total_weight_as_count(&pool),
        "message": "Prize pool saved. A spin will now draw from these prizes.",
    })))
}

fn total_weight_as_count(pool: &Value) -> usize {
    pool.get("prizes")
        .and_then(|p| p.as_array())
        .map(|a| a.len())
        .unwrap_or(0)
}

/// GET /api/v1/campaigns/:slug/prize-pool — what the screen renders, with the odds worked out.
pub async fn get_prize_pool(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(slug): Path<String>,
) -> Result<Json<Value>, AppError> {
    let account_id = uuid::Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;

    let row: Option<(Value,)> = sqlx::query_as(
        "SELECT COALESCE(config, '{}'::jsonb) FROM campaigns
          WHERE (slug = $1 OR id::text = $1) AND account_id = $2",
    )
    .bind(&slug)
    .bind(account_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| AppError::Internal(format!("Failed to read campaign: {e}")))?;

    let Some((config,)) = row else {
        return Err(AppError::NotFound("Campaign not found".to_string()));
    };

    let pool = config.get("prize_pool").cloned().unwrap_or(json!({}));
    let total = pool
        .get("total_weight")
        .and_then(|t| t.as_i64())
        .unwrap_or(0);
    let prizes = pool
        .get("prizes")
        .and_then(|p| p.as_array())
        .cloned()
        .unwrap_or_default();

    // The odds the operator is actually configuring. Shown as a percentage with two decimals so a
    // 1-in-3 prize does not read as "33%".
    let with_odds: Vec<Value> = prizes
        .iter()
        .map(|p| {
            let w = p.get("weight").and_then(|w| w.as_i64()).unwrap_or(0);
            let pct = if total > 0 {
                (w as f64 / total as f64) * 100.0
            } else {
                0.0
            };
            let mut v = p.clone();
            v["odds_percent"] = json!((pct * 100.0).round() / 100.0);
            v
        })
        .collect();

    Ok(Json(json!({
        "prize_pool": {
            "prizes": with_odds,
            "total_weight": total,
            "inventory_tracking": pool.get("inventory_tracking").and_then(|b| b.as_bool()).unwrap_or(false),
            "allow_when_exhausted": pool.get("allow_when_exhausted").and_then(|b| b.as_bool()).unwrap_or(false),
        },
        "configured": !prizes.is_empty(),
    })))
}
