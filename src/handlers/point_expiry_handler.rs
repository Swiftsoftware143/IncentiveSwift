//! Point expiry handler — expires points older than 12 months
//! Called by cron: POST /api/v1/admin/treasury/expire-points
use axum::{extract::State, Json};
use serde_json::{json, Value};

use crate::state::AppState;
use crate::error::AppError;

/// POST /api/v1/admin/treasury/expire-points
/// Expires all points earned more than 12 months ago.
/// Returns count of points expired and members affected.
pub async fn expire_points(
    State(s): State<AppState>,
) -> Result<Json<Value>, AppError> {
    let cutoff = chrono::Utc::now() - chrono::Duration::days(365);

    // Find and sum expired points per member
    let expired = sqlx::query_as::<_, (uuid::Uuid, i32)>(
        "SELECT member_id, SUM(points_awarded) as expired_points
         FROM loyalty_scans
         WHERE created_at < $1
           AND points_awarded > 0
           AND cleared_at IS NULL
         GROUP BY member_id"
    )
    .bind(cutoff)
    .fetch_all(&s.db)
    .await?;

    let mut total_expired: i64 = 0;
    let mut members_affected = 0;

    for (member_id, points) in &expired {
        sqlx::query(
            "UPDATE loyalty_members SET points_balance = GREATEST(0, points_balance - $1), updated_at = NOW() WHERE id = $2"
        )
        .bind(points)
        .bind(member_id)
        .execute(&s.db)
        .await?;

        // Mark scans as cleared
        sqlx::query(
            "UPDATE loyalty_scans SET cleared_at = NOW() WHERE member_id = $1 AND created_at < $2 AND cleared_at IS NULL AND points_awarded > 0"
        )
        .bind(member_id)
        .bind(cutoff)
        .execute(&s.db)
        .await?;

        // Update treasury liability
        let liability_reduction = *points as f64 * 0.01;
        sqlx::query(
            "UPDATE point_treasury SET outstanding_liability = GREATEST(0, outstanding_liability - $1), updated_at = NOW()"
        )
        .bind(liability_reduction)
        .execute(&s.db)
        .await?;

        total_expired += *points as i64;
        members_affected += 1;

        tracing::info!("Expired {} points for member {}", points, member_id);
    }

    Ok(Json(json!({
        "success": true,
        "total_points_expired": total_expired,
        "members_affected": members_affected,
        "cutoff_date": cutoff.to_rfc3339(),
        "message": format!("Expired {} points across {} members (cutoff: {})", total_expired, members_affected, cutoff.format("%Y-%m-%d"))
    })))
}
