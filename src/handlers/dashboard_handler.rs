//! Dashboard handler — aggregate stats for the authenticated user's campaigns.

use crate::error::AppError;
use crate::state::AppState;
use crate::security::auth::AuthenticatedUser;
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
        "SELECT COALESCE(tenant_id, id) FROM accounts WHERE id = $1"
    )
    .bind(account_id)
    .fetch_one(&state.db)
    .await?;
    let tenant_id = tenant_id.unwrap_or(account_id);

    // Count campaigns for this tenant
    let total_campaigns: i64 = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT COUNT(*) FROM campaigns c
         WHERE c.account_id = $1"
    )
    .bind(account_id)
    .fetch_one(&state.db)
    .await?
    .unwrap_or(0);

    // Count contacts/participants for this tenant
    let total_participants: i64 = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT COUNT(DISTINCT e.contact_id) FROM entries e
         JOIN campaigns c ON c.id = e.campaign_id
         WHERE c.account_id = $1"
    )
    .bind(account_id)
    .fetch_one(&state.db)
    .await?
    .unwrap_or(0);

    // Count total entries (interactions)
    let total_entries: i64 = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT COUNT(*) FROM entries e
         JOIN campaigns c ON c.id = e.campaign_id
         WHERE c.account_id = $1"
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
