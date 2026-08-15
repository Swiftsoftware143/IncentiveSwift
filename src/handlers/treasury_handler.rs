//! Treasury/ledger admin endpoints
use axum::{extract::State, Json};
use rust_decimal::Decimal;
use serde::Serialize;
use serde_json::{json, Value};

use crate::error::AppError;
use crate::state::AppState;
use chrono;
use uuid;

#[derive(Serialize, sqlx::FromRow)]
pub struct TreasurySummary {
    total_points_issued: i64,
    total_points_redeemed: i64,
    total_revenue_collected: Decimal,
    total_reimbursements_paid: Decimal,
    outstanding_liability: Decimal,
    minimum_float: Decimal,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct BusinessLedgerRow {
    business_id: uuid::Uuid,
    business_name: Option<String>,
    points_issued_this_month: Option<i32>,
    points_redeemed_this_month: Option<i32>,
    total_billed_this_month: Option<Decimal>,
    total_reimbursed_this_month: Option<Decimal>,
    net_position: Option<Decimal>,
    month_key: Option<String>,
}

/// GET /api/v1/admin/treasury/summary
pub async fn treasury_summary(
    State(s): State<AppState>,
) -> Result<Json<TreasurySummary>, AppError> {
    let row = sqlx::query_as::<_, TreasurySummary>(
        "SELECT COALESCE(total_points_issued,0) as total_points_issued,
                COALESCE(total_points_redeemed,0) as total_points_redeemed,
                COALESCE(total_revenue_collected,0) as total_revenue_collected,
                COALESCE(total_reimbursements_paid,0) as total_reimbursements_paid,
                COALESCE(outstanding_liability,0) as outstanding_liability,
                COALESCE(minimum_float,100.00) as minimum_float
         FROM point_treasury LIMIT 1",
    )
    .fetch_optional(&s.db)
    .await?
    .unwrap_or(TreasurySummary {
        total_points_issued: 0,
        total_points_redeemed: 0,
        total_revenue_collected: Decimal::new(0, 0),
        total_reimbursements_paid: Decimal::new(0, 0),
        outstanding_liability: Decimal::new(0, 0),
        minimum_float: Decimal::new(10000, 2),
    });

    Ok(Json(row))
}

/// GET /api/v1/admin/treasury/businesses
pub async fn business_ledgers(
    State(s): State<AppState>,
) -> Result<Json<Vec<BusinessLedgerRow>>, AppError> {
    let rows = sqlx::query_as::<_, BusinessLedgerRow>(
        "SELECT business_id, business_name,
                COALESCE(points_issued_this_month,0) as points_issued_this_month,
                COALESCE(points_redeemed_this_month,0) as points_redeemed_this_month,
                COALESCE(total_billed_this_month,0) as total_billed_this_month,
                COALESCE(total_reimbursed_this_month,0) as total_reimbursed_this_month,
                COALESCE(net_position,0) as net_position,
                month_key
         FROM business_point_ledger
         WHERE month_key = TO_CHAR(NOW(), 'YYYY-MM')
         ORDER BY total_billed_this_month DESC",
    )
    .fetch_all(&s.db)
    .await?;

    Ok(Json(rows))
}

/// GET /api/v1/admin/treasury/issuance-log?limit=50
pub async fn issuance_log(
    State(s): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, AppError> {
    let limit: i64 = params
        .get("limit")
        .and_then(|l| l.parse().ok())
        .unwrap_or(50);
    let rows = sqlx::query_as::<_, (uuid::Uuid, uuid::Uuid, String, i32, Decimal, chrono::DateTime<chrono::Utc>)>(
        "SELECT issuing_business_id, member_id, business_name, points_issued, total_billed, created_at FROM point_issuance_log ORDER BY created_at DESC LIMIT $1"
    )
    .bind(limit)
    .fetch_all(&s.db)
    .await?;

    let items: Vec<Value> = rows
        .into_iter()
        .map(|(bid, mid, name, pts, bill, ts)| {
            json!({
                "business_id": bid,
                "member_id": mid,
                "business_name": name,
                "points": pts,
                "billed": bill,
                "time": ts
            })
        })
        .collect();

    Ok(Json(json!({ "items": items, "count": items.len() })))
}
