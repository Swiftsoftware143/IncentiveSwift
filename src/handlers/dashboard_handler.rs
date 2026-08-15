//! Dashboard handler — aggregate stats for the authenticated user's campaigns.

use crate::error::AppError;
use crate::security::auth::AuthenticatedUser;
use crate::state::AppState;
use axum::{extract::State, Json};
use serde_json::{json, Value};
use uuid::Uuid;

/// GET /api/v1/dashboard/stats
/// Returns aggregate counts for the authenticated account's tenant.
pub async fn dashboard_stats(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;

    let tenant_id: Option<Uuid> = sqlx::query_scalar::<_, Option<Uuid>>(
        "SELECT COALESCE(tenant_id, id) FROM accounts WHERE id = $1",
    )
    .bind(account_id)
    .fetch_one(&state.db)
    .await?;
    let tenant_id = tenant_id.unwrap_or(account_id);

    // Count campaigns for this tenant
    let total_campaigns: i64 = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT COUNT(*) FROM campaigns c
         WHERE c.account_id = $1",
    )
    .bind(account_id)
    .fetch_one(&state.db)
    .await?
    .unwrap_or(0);

    // Count contacts/participants for this tenant
    let total_participants: i64 = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT COUNT(DISTINCT e.contact_id) FROM entries e
         JOIN campaigns c ON c.id = e.campaign_id
         WHERE c.account_id = $1",
    )
    .bind(account_id)
    .fetch_one(&state.db)
    .await?
    .unwrap_or(0);

    // Count total entries (interactions)
    let total_entries: i64 = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT COUNT(*) FROM entries e
         JOIN campaigns c ON c.id = e.campaign_id
         WHERE c.account_id = $1",
    )
    .bind(account_id)
    .fetch_one(&state.db)
    .await?
    .unwrap_or(0);

    Ok(Json(json!({
        "total_campaigns": total_campaigns,
        "total_rewards": 0,
        "total_participants": total_participants,
        "total_points": 0,
        "total_entries": total_entries,
    })))
}

/// GET /api/v1/dashboard/activity
/// Returns recent activity feed for the authenticated user's campaigns.
pub async fn dashboard_activity(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;

    let rows = sqlx::query(
        r#"SELECT e.id, COALESCE(c.name, 'Unknown') as campaign_name,
                  COALESCE(ct.email, ct.phone, 'Anonymous') as participant,
                  e.created_at
           FROM entries e
           JOIN campaigns c ON c.id = e.campaign_id
           LEFT JOIN contacts ct ON ct.id = e.contact_id
           WHERE c.account_id = $1 ORDER BY e.created_at DESC LIMIT 20"#,
    )
    .bind(account_id)
    .fetch_all(&state.db)
    .await?;

    let mut activities: Vec<Value> = Vec::new();
    for row in &rows {
        use sqlx::Row;
        let id: Uuid = row.get("id");
        let campaign: String = row.get("campaign_name");
        let participant: String = row.get("participant");
        let created_at: chrono::NaiveDateTime = row.get("created_at");
        activities.push(json!({
            "id": id,
            "type": "entry",
            "campaign": campaign,
            "participant": participant,
            "created_at": created_at,
            "action": format!("New entry in {}", campaign),
        }));
    }

    Ok(Json(json!({ "activities": activities })))
}

/// GET /api/v1/leads - list all entries as leads
pub async fn list_leads(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;
    let rows = sqlx::query(
        r#"SELECT e.id, ct.email, ct.phone, ct.first_name, ct.last_name, c.name as campaign_name, e.created_at
           FROM entries e JOIN campaigns c ON c.id = e.campaign_id
           LEFT JOIN contacts ct ON ct.id = e.contact_id
           WHERE c.account_id = $1 ORDER BY e.created_at DESC LIMIT 100"#,
    ).bind(account_id).fetch_all(&state.db).await?;
    let mut leads: Vec<Value> = Vec::new();
    for row in &rows {
        use sqlx::Row;
        leads.push(json!({
            "id": row.get::<Uuid,_>("id"),
            "email": row.get::<Option<String>,_>("email"),
            "phone": row.get::<Option<String>,_>("phone"),
            "first_name": row.get::<Option<String>,_>("first_name"),
            "last_name": row.get::<Option<String>,_>("last_name"),
            "campaign": row.get::<String,_>("campaign_name"),
            "created_at": row.get::<chrono::NaiveDateTime,_>("created_at"),
        }));
    }
    Ok(Json(json!({"leads": leads})))
}

/// GET /api/v1/tags - list all tags for tenant
pub async fn list_tags(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;
    let rows = sqlx::query(
        r#"SELECT t.id, t.name, t.color, tg.name as group_name FROM tags t
           LEFT JOIN tag_groups tg ON tg.id = t.group_id
           WHERE t.account_id = $1 ORDER BY tg.name, t.name"#,
    )
    .bind(account_id)
    .fetch_all(&state.db)
    .await?;
    let mut tags: Vec<Value> = Vec::new();
    for row in &rows {
        use sqlx::Row;
        tags.push(json!({
            "id": row.get::<Uuid,_>("id"),
            "name": row.get::<String,_>("name"),
            "color": row.get::<Option<String>,_>("color"),
            "group": row.get::<Option<String>,_>("group_name"),
        }));
    }
    Ok(Json(json!({"tags": tags})))
}

/// GET /api/v1/plans (public - no auth required)
pub async fn list_public_plans(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let rows = sqlx::query(
        r#"SELECT id, name, slug, price::int as monthly_price, 0 as monthly_zc_pool, NULL::text as description, features, NULL::text as how_it_works
           FROM plans ORDER BY name"#,
    ).fetch_all(&state.db).await?;
    let mut plans: Vec<Value> = Vec::new();
    for row in &rows {
        use sqlx::Row;
        plans.push(json!({
            "id": row.get::<Uuid,_>("id"),
            "name": row.get::<String,_>("name"),
            "slug": row.get::<String,_>("slug"),
            "monthly_price": row.get::<i32,_>("monthly_price"),
            "monthly_zc_pool": row.get::<i32,_>("monthly_zc_pool"),
            "description": row.get::<Option<String>,_>("description"),
            "features": row.get::<Option<serde_json::Value>,_>("features"),
            "how_it_works": row.get::<Option<String>,_>("how_it_works"),
        }));
    }
    Ok(Json(json!({"plans": plans})))
}
