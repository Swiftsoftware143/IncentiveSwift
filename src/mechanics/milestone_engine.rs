#![allow(unused)]
//! Phase 2: Campaign Milestones Engine
//!
//! After any points addition, check if the contact crossed a milestone threshold
//! and fire the configured action (award coupon, bonus entry, fire webhook, etc.)

use crate::error::AppError;
use crate::state::AppState;
use serde_json::{json, Value};
use uuid::Uuid;

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize, serde::Deserialize)]
pub struct CampaignMilestone {
    pub id: Uuid,
    pub campaign_id: Uuid,
    pub name: String,
    pub description: Option<String>,
    pub points_required: i32,
    pub action_type: String,
    pub action_config: Value,
    pub is_repeatable: bool,
    pub max_repeats: Option<i32>,
    pub cooldown_hours: Option<i32>,
    pub is_active: bool,
    pub sort_order: i32,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct MilestoneAchieved {
    pub id: Uuid,
    pub milestone_id: Uuid,
    pub campaign_id: Uuid,
    pub contact_id: Uuid,
    pub action_executed: bool,
    pub action_result: Option<Value>,
    pub achieved_at: chrono::DateTime<chrono::Utc>,
}

/// Get active milestones for a campaign, ordered by points_required ascending
pub async fn get_active_milestones(
    pool: &sqlx::PgPool,
    campaign_id: &Uuid,
) -> Result<Vec<CampaignMilestone>, AppError> {
    let milestones = sqlx::query_as::<_, CampaignMilestone>(
        r#"SELECT id, campaign_id, name, description, points_required,
                  action_type, action_config, is_repeatable, max_repeats,
                  cooldown_hours, is_active, sort_order, created_at
           FROM campaign_milestones
           WHERE campaign_id = $1 AND is_active = true
           ORDER BY points_required ASC"#
    )
    .bind(campaign_id)
    .fetch_all(pool)
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;
    Ok(milestones)
}

/// Get achieved milestones for a contact in a campaign
pub async fn get_achieved_milestones(
    pool: &sqlx::PgPool,
    campaign_id: &Uuid,
    contact_id: &Uuid,
) -> Result<Vec<MilestoneAchieved>, AppError> {
    let achieved = sqlx::query_as::<_, MilestoneAchieved>(
        r#"SELECT id, milestone_id, campaign_id, contact_id,
                  action_executed, action_result, achieved_at
           FROM campaign_milestones_achieved
           WHERE campaign_id = $1 AND contact_id = $2"#
    )
    .bind(campaign_id)
    .bind(contact_id)
    .fetch_all(pool)
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;
    Ok(achieved)
}

/// Record a milestone achievement
async fn record_achieved(
    pool: &sqlx::PgPool,
    milestone_id: &Uuid,
    campaign_id: &Uuid,
    contact_id: &Uuid,
    action_result: Option<Value>,
) -> Result<(), AppError> {
    sqlx::query(
        r#"INSERT INTO campaign_milestones_achieved
           (milestone_id, campaign_id, contact_id, action_executed, action_result)
           VALUES ($1, $2, $3, true, $4)
           ON CONFLICT (milestone_id, contact_id) DO NOTHING"#
    )
    .bind(milestone_id)
    .bind(campaign_id)
    .bind(contact_id)
    .bind(&action_result)
    .execute(pool)
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;
    Ok(())
}

/// Main entry point: check milestones after points update.
/// Returns list of milestone names that were triggered.
pub async fn check_milestones(
    state: &AppState,
    campaign_id: &Uuid,
    contact_id: &Uuid,
    current_points: i32,
) -> Result<Vec<(String, String)>, AppError> {
    let milestones = get_active_milestones(&state.db, campaign_id).await?;
    if milestones.is_empty() {
        return Ok(Vec::new());
    }

    let achieved = get_achieved_milestones(&state.db, campaign_id, contact_id).await?;
    let achieved_set: std::collections::HashSet<Uuid> = achieved.iter().map(|a| a.milestone_id).collect();

    let mut triggered = Vec::new();

    for m in &milestones {
        if current_points < m.points_required {
            break; // ordered by points, so no later milestone will match either
        }

        if achieved_set.contains(&m.id) && !m.is_repeatable {
            continue;
        }

        // Fire action
        let result = match m.action_type.as_str() {
            "award_coupon" => fire_award_coupon(&state.db, campaign_id, contact_id, m).await,
            "bonus_entry" => fire_bonus_entry(&state.db, campaign_id, contact_id, m).await,
            "fire_webhook" => fire_webhook_action(campaign_id, contact_id, m).await,
            _ => Ok(None),
        };

        let action_result = match result {
            Ok(r) => r,
            Err(e) => {
                eprintln!("Milestone action failed: {} - {}", m.name, e);
                None
            }
        };

        record_achieved(&state.db, &m.id, campaign_id, contact_id, action_result).await?;
        triggered.push((m.name.clone(), m.action_type.clone()));
    }

    Ok(triggered)
}

/// Award a coupon code
async fn fire_award_coupon(
    pool: &sqlx::PgPool,
    campaign_id: &Uuid,
    contact_id: &Uuid,
    milestone: &CampaignMilestone,
) -> Result<Option<Value>, AppError> {
    let coupon_code = format!("MS-{:06x}", Uuid::new_v4().as_u128() % 0xFFFFFF);
    let value = milestone.action_config.get("value").and_then(|v| v.as_i64()).unwrap_or(0);

    sqlx::query(
        r#"INSERT INTO campaign_wins (campaign_id, contact_id, prize_label, coupon_code, is_redeemed)
           VALUES ($1, $2, $3, $4, false)"#
    )
    .bind(campaign_id)
    .bind(contact_id)
    .bind(format!("Milestone: {} - ${} coupon", milestone.name, value))
    .bind(&coupon_code)
    .execute(pool)
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;

    Ok(Some(json!({"coupon_code": coupon_code, "value": value})))
}

/// Grant bonus entries/spins
async fn fire_bonus_entry(
    pool: &sqlx::PgPool,
    campaign_id: &Uuid,
    contact_id: &Uuid,
    milestone: &CampaignMilestone,
) -> Result<Option<Value>, AppError> {
    let spin_count = milestone.action_config.get("spin_count").and_then(|v| v.as_i64()).unwrap_or(1);

    for _ in 0..spin_count {
        sqlx::query(
            r#"INSERT INTO entries (campaign_id, contact_id, source, is_bonus)
               VALUES ($1, $2, 'milestone', true)"#
        )
        .bind(campaign_id)
        .bind(contact_id)
        .execute(pool)
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    }

    Ok(Some(json!({"bonus_entries": spin_count})))
}

/// Fire a webhook (fire-and-forget)
async fn fire_webhook_action(
    campaign_id: &Uuid,
    contact_id: &Uuid,
    milestone: &CampaignMilestone,
) -> Result<Option<Value>, AppError> {
    let url = milestone.action_config.get("url")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if url.is_empty() {
        return Ok(None);
    }

    let body = json!({
        "event": "milestone_reached",
        "campaign_id": campaign_id,
        "contact_id": contact_id,
        "milestone": milestone.name,
        "points_required": milestone.points_required,
        "action_type": milestone.action_type,
        "config": milestone.action_config,
    });

    // Fire and forget ??? spawn a task
    let client = reqwest::Client::new();
    let _ = client.post(url).json(&body).send().await;

    Ok(Some(json!({"webhook_target": url})))
}

// ===== Admin CRUD for milestones (used by handlers) =====

#[derive(serde::Deserialize)]
pub struct CreateMilestoneInput {
    pub name: String,
    pub description: Option<String>,
    pub points_required: i32,
    pub action_type: String,
    pub action_config: Value,
    pub is_repeatable: Option<bool>,
    pub max_repeats: Option<i32>,
    pub cooldown_hours: Option<i32>,
}

#[derive(serde::Deserialize)]
pub struct UpdateMilestoneInput {
    pub name: Option<String>,
    pub description: Option<String>,
    pub points_required: Option<i32>,
    pub action_type: Option<String>,
    pub action_config: Option<Value>,
    pub is_repeatable: Option<bool>,
    pub max_repeats: Option<i32>,
    pub cooldown_hours: Option<i32>,
    pub is_active: Option<bool>,
}

pub async fn create_milestone(
    pool: &sqlx::PgPool,
    campaign_id: &Uuid,
    input: &CreateMilestoneInput,
) -> Result<CampaignMilestone, AppError> {
    let id = Uuid::new_v4();
    sqlx::query(
        r#"INSERT INTO campaign_milestones
           (id, campaign_id, name, description, points_required, action_type, action_config,
            is_repeatable, max_repeats, cooldown_hours)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)"#
    )
    .bind(id)
    .bind(campaign_id)
    .bind(&input.name)
    .bind(&input.description)
    .bind(input.points_required)
    .bind(&input.action_type)
    .bind(&input.action_config)
    .bind(input.is_repeatable.unwrap_or(false))
    .bind(input.max_repeats)
    .bind(input.cooldown_hours)
    .execute(pool)
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;

    let milestone = sqlx::query_as::<_, CampaignMilestone>(
        "SELECT * FROM campaign_milestones WHERE id = $1"
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;

    Ok(milestone)
}

pub async fn update_milestone(
    pool: &sqlx::PgPool,
    milestone_id: &Uuid,
    input: &UpdateMilestoneInput,
) -> Result<CampaignMilestone, AppError> {
    // Build dynamic UPDATE with sequential $1, $2, ... params
    // Use placeholders array; each entry corresponds to a bind
    let mut fields: Vec<String> = Vec::new();

    if input.name.is_some() { fields.push("name".to_string()); }
    if input.description.is_some() { fields.push("description".to_string()); }
    if input.points_required.is_some() { fields.push("points_required".to_string()); }
    if input.action_type.is_some() { fields.push("action_type".to_string()); }
    if input.action_config.is_some() { fields.push("action_config".to_string()); }
    if input.is_repeatable.is_some() { fields.push("is_repeatable".to_string()); }
    if input.max_repeats.is_some() { fields.push("max_repeats".to_string()); }
    if input.cooldown_hours.is_some() { fields.push("cooldown_hours".to_string()); }
    if input.is_active.is_some() { fields.push("is_active".to_string()); }

    if fields.is_empty() {
        return Err(AppError::BadRequest("No fields to update".to_string()));
    }

    let set_clauses: Vec<String> = fields.iter().enumerate()
        .map(|(i, name)| format!("{} = ${}", name, i + 1))
        .collect();
    let set_clause = set_clauses.join(", ");
    let sql = format!(
        "UPDATE campaign_milestones SET {}, updated_at = now() WHERE id = ${}",
        set_clause,
        fields.len() + 1
    );

    let mut query = sqlx::query(&sql);

    if let Some(v) = &input.name { query = query.bind(v); }
    if let Some(v) = &input.description { query = query.bind(v); }
    if let Some(v) = input.points_required { query = query.bind(v); }
    if let Some(v) = &input.action_type { query = query.bind(v); }
    if let Some(v) = &input.action_config { query = query.bind(v); }
    if let Some(v) = input.is_repeatable { query = query.bind(v); }
    if let Some(v) = input.max_repeats { query = query.bind(v); }
    if let Some(v) = input.cooldown_hours { query = query.bind(v); }
    if let Some(v) = input.is_active { query = query.bind(v); }

    query.bind(milestone_id)
        .execute(pool)
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;

    let milestone = sqlx::query_as::<_, CampaignMilestone>(
        "SELECT * FROM campaign_milestones WHERE id = $1"
    )
    .bind(milestone_id)
    .fetch_one(pool)
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;

    Ok(milestone)
}

pub async fn delete_milestone(
    pool: &sqlx::PgPool,
    milestone_id: &Uuid,
) -> Result<(), AppError> {
    sqlx::query("DELETE FROM campaign_milestones WHERE id = $1")
        .bind(milestone_id)
        .execute(pool)
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    Ok(())
}

pub async fn list_milestones(
    pool: &sqlx::PgPool,
    campaign_id: &Uuid,
) -> Result<Vec<CampaignMilestone>, AppError> {
    let milestones = sqlx::query_as::<_, CampaignMilestone>(
        r#"SELECT id, campaign_id, name, description, points_required,
                  action_type, action_config, is_repeatable, max_repeats,
                  cooldown_hours, is_active, sort_order, created_at
           FROM campaign_milestones
           WHERE campaign_id = $1
           ORDER BY sort_order ASC, points_required ASC"#
    )
    .bind(campaign_id)
    .fetch_all(pool)
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;
    Ok(milestones)
}
