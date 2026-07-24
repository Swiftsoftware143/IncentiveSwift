//! Rewards handler — wraps loyalty reward tiers for the admin UI.
//!
//! The admin UI expects:
//!   GET    /api/v1/rewards -> list reward tiers with campaign info
//!   POST   /api/v1/rewards -> create reward tier
//!   PUT    /api/v1/rewards/:id -> update reward tier
//!
//! Internally delegates to the loyalty_reward_tiers table.
//! Field mapping: UI "points_cost" → DB "points_required"

use crate::error::AppError;
use crate::state::AppState;
use crate::security::auth::AuthenticatedUser;
use axum::{
    extract::{Path, State},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

/// Reward row returned to the admin UI.
#[derive(Debug, serde::Serialize, sqlx::FromRow)]
pub struct RewardRow {
    pub id: Uuid,
    pub name: String,
    #[sqlx(rename = "points_required")]
    pub points_cost: i32,
    pub campaign_name: Option<String>,
    pub campaign_id: Option<Uuid>,
    pub marketing_boost: Option<Value>,
}

/// Input for creating/updating a reward (admin UI format).
#[derive(Deserialize)]
pub struct RewardInput {
    pub name: String,
    pub points_cost: Option<i32>,
    pub campaign_id: Option<String>,
    pub description: Option<String>,
    pub marketing_boost: Option<Value>,
}

/// GET /api/v1/rewards — list reward tiers with campaign context
pub async fn list_rewards(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;

    let rows = sqlx::query_as::<_, RewardRow>(
        r#"SELECT rt.id, rt.name, rt.points_required,
                  COALESCE(c.name, lp.name) as campaign_name,
                  COALESCE(c.id, lp.campaign_id) as campaign_id,
                  rt.marketing_boost
           FROM loyalty_reward_tiers rt
           JOIN loyalty_programs lp ON lp.id = rt.program_id
           LEFT JOIN campaigns c ON c.id = lp.campaign_id
           WHERE (c.account_id = $1 OR c.id IS NULL)
           ORDER BY rt.sort_order ASC, rt.points_required ASC"#
    )
    .bind(account_id)
    .fetch_all(&state.db)
    .await?;

    Ok(Json(json!({ "rewards": rows })))
}

/// POST /api/v1/rewards — create a reward tier
pub async fn create_reward(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<RewardInput>,
) -> Result<Json<Value>, AppError> {
    // Find the loyalty program for the given campaign, or use default program
    let program_id = if let Some(ref campaign_id_str) = body.campaign_id {
        let cid = Uuid::parse_str(campaign_id_str)
            .map_err(|_| AppError::BadRequest("Invalid campaign_id".to_string()))?;
        // Find or create a program linked to this campaign
        let existing: Option<Uuid> = sqlx::query_scalar(
            "SELECT id FROM loyalty_programs WHERE campaign_id = $1 LIMIT 1"
        )
        .bind(cid)
        .fetch_optional(&state.db)
        .await?;

        match existing {
            Some(pid) => pid,
            None => {
                // Get campaign info
                let camp = sqlx::query_as::<_, (String,)>(
                    "SELECT name FROM campaigns WHERE id = $1 LIMIT 1"
                )
                .bind(cid)
                .fetch_optional(&state.db)
                .await?
                .ok_or_else(|| AppError::BadRequest("Campaign not found".to_string()))?;

                let pid = Uuid::new_v4();
                sqlx::query(
                    r#"INSERT INTO loyalty_programs (id, campaign_id, name, recognition_method,
                        points_per_checkin, max_checkins_per_day, is_active)
                       VALUES ($1, $2, $3, 'both', 10, 1, true)"#
                )
                .bind(pid)
                .bind(cid)
                .bind(format!("{} Rewards", camp.0))
                .execute(&state.db)
                .await?;
                pid
            }
        }
    } else {
        // No campaign specified — use or create default program
        let existing: Option<Uuid> = sqlx::query_scalar(
            "SELECT id FROM loyalty_programs WHERE campaign_id IS NULL LIMIT 1"
        )
        .fetch_optional(&state.db)
        .await?;

        match existing {
            Some(pid) => pid,
            None => {
                let pid = Uuid::new_v4();
                sqlx::query(
                    r#"INSERT INTO loyalty_programs (id, name, recognition_method,
                        points_per_checkin, max_checkins_per_day, is_active)
                       VALUES ($1, 'Default Rewards', 'both', 10, 1, true)"#
                )
                .bind(pid)
                .execute(&state.db)
                .await?;
                pid
            }
        }
    };

    // Ensure unique sort_order
    let max_order: Option<i32> = sqlx::query_scalar(
        "SELECT MAX(sort_order) FROM loyalty_reward_tiers WHERE program_id = $1"
    )
    .bind(program_id)
    .fetch_optional(&state.db)
    .await?
    .flatten();
    let next_order = max_order.unwrap_or(0) + 1;

    let id = Uuid::new_v4();
    sqlx::query(
        r#"INSERT INTO loyalty_reward_tiers (id, program_id, name, points_required, reward_tag, sort_order, marketing_boost)
           VALUES ($1, $2, $3, $4, $5, $6, $7)"#
    )
    .bind(id)
    .bind(program_id)
    .bind(&body.name)
    .bind(body.points_cost.unwrap_or(100))
    .bind(&body.name) // reward_tag defaults to the reward name
    .bind(next_order)
    .bind(&body.marketing_boost)
    .execute(&state.db)
    .await?;

    let row = sqlx::query_as::<_, RewardRow>(
        r#"SELECT rt.id, rt.name, rt.points_required,
                  COALESCE(c.name, lp.name) as campaign_name,
                  COALESCE(c.id, lp.campaign_id) as campaign_id,
                  rt.marketing_boost
           FROM loyalty_reward_tiers rt
           JOIN loyalty_programs lp ON lp.id = rt.program_id
           LEFT JOIN campaigns c ON c.id = lp.campaign_id
           WHERE rt.id = $1"#
    )
    .bind(id)
    .fetch_one(&state.db)
    .await?;

    Ok(Json(json!({ "reward": row })))
}

/// PUT /api/v1/rewards/:id — update a reward tier
pub async fn update_reward(
    State(state): State<AppState>,
    Path(id): Path<String>,
    user: AuthenticatedUser,
    Json(body): Json<RewardInput>,
) -> Result<Json<Value>, AppError> {
    let reward_id = Uuid::parse_str(&id)
        .map_err(|_| AppError::BadRequest("Invalid reward ID".to_string()))?;

    // Verify it exists by trying to parse marketing_boost for update
    let existing_mb: Option<Value> = sqlx::query_scalar(
        "SELECT marketing_boost FROM loyalty_reward_tiers WHERE id = $1"
    )
    .bind(reward_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Reward not found".to_string()))?;

    // Update the tier
    sqlx::query(
        r#"UPDATE loyalty_reward_tiers
           SET name = COALESCE($1, name),
               points_required = COALESCE($2, points_required),
               marketing_boost = $3
           WHERE id = $4"#
    )
    .bind(if body.name.is_empty() { None } else { Some(&body.name) })
    .bind(body.points_cost)
    .bind(&body.marketing_boost)
    .bind(reward_id)
    .execute(&state.db)
    .await?;

    let row = sqlx::query_as::<_, RewardRow>(
        r#"SELECT rt.id, rt.name, rt.points_required,
                  COALESCE(c.name, lp.name) as campaign_name,
                  COALESCE(c.id, lp.campaign_id) as campaign_id,
                  rt.marketing_boost
           FROM loyalty_reward_tiers rt
           JOIN loyalty_programs lp ON lp.id = rt.program_id
           LEFT JOIN campaigns c ON c.id = lp.campaign_id
           WHERE rt.id = $1"#
    )
    .bind(reward_id)
    .fetch_one(&state.db)
    .await?;

    Ok(Json(json!({ "reward": row })))
}
