//! Analytics handler — real aggregate queries for dashboard-like analytics.
//! Provides endpoints for overview KPIs, per-campaign drill-down, source tracking,
//! loyalty metrics, contact analytics, and CSV export.

use crate::error::AppError;
use crate::state::AppState;
use crate::security::auth::AuthenticatedUser;
use axum::{
    extract::{Path, Query, State},
    http::header,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;
use std::collections::HashMap;

#[derive(Deserialize)]
pub struct AnalyticsQuery {
    pub sort: Option<String>,
    pub order: Option<String>,
}

#[derive(Deserialize)]
pub struct ExportQuery {
    pub r#type: Option<String>,
}

/// GET /api/v1/analytics/overview — Account-level KPIs
pub async fn overview(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let account_id = user.account_id.clone();
    let uuid = Uuid::parse_str(&account_id).map_err(|_| AppError::BadRequest("Invalid account ID".into()))?;

    // Total campaigns
    let total_campaigns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM campaigns WHERE account_id = $1")
        .bind(uuid).fetch_one(&state.db).await.unwrap_or(0);

    // Active campaigns
    let active_campaigns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM campaigns WHERE account_id = $1 AND status = 'active'")
        .bind(uuid).fetch_one(&state.db).await.unwrap_or(0);

    // Total entries
    let total_entries: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM entries e JOIN campaigns c ON c.id = e.campaign_id WHERE c.account_id = $1"
    ).bind(uuid).fetch_one(&state.db).await.unwrap_or(0);

    // Total wins
    let total_wins: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM campaign_wins w JOIN campaigns c ON c.id = w.campaign_id WHERE c.account_id = $1"
    ).bind(uuid).fetch_one(&state.db).await.unwrap_or(0);

    // Unique contacts
    let total_contacts: i64 = sqlx::query_scalar(
        "SELECT COUNT(DISTINCT e.contact_id) FROM entries e JOIN campaigns c ON c.id = e.campaign_id WHERE c.account_id = $1"
    ).bind(uuid).fetch_one(&state.db).await.unwrap_or(0);

    // Loyalty members
    let total_loyalty_members: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM loyalty_members m JOIN loyalty_programs p ON p.id = m.program_id JOIN campaigns c ON c.id = p.campaign_id WHERE c.account_id = $1"
    ).bind(uuid).fetch_one(&state.db).await.unwrap_or(0);

    // Points issued
    let total_points_issued: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(m.lifetime_points), 0) FROM loyalty_members m JOIN loyalty_programs p ON p.id = m.program_id JOIN campaigns c ON c.id = p.campaign_id WHERE c.account_id = $1"
    ).bind(uuid).fetch_one(&state.db).await.unwrap_or(0);

    // Total redemptions
    let total_redemptions: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM campaign_wins w JOIN campaigns c ON c.id = w.campaign_id WHERE c.account_id = $1 AND w.redeemed = true"
    ).bind(uuid).fetch_one(&state.db).await.unwrap_or(0);

    // Entry growth percent (this week vs last week)
    let this_week: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM entries e JOIN campaigns c ON c.id = e.campaign_id WHERE c.account_id = $1 AND e.created_at >= date_trunc('week', now())"
    ).bind(uuid).fetch_one(&state.db).await.unwrap_or(0);

    let last_week: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM entries e JOIN campaigns c ON c.id = e.campaign_id WHERE c.account_id = $1 AND e.created_at >= date_trunc('week', now()) - interval '1 week' AND e.created_at < date_trunc('week', now())"
    ).bind(uuid).fetch_one(&state.db).await.unwrap_or(0);

    let entry_growth_percent: f64 = if last_week > 0 {
        ((this_week as f64 - last_week as f64) / last_week as f64) * 100.0
    } else {
        0.0
    };

    let win_rate_percent: f64 = if total_entries > 0 {
        (total_wins as f64 / total_entries as f64) * 100.0
    } else {
        0.0
    };

    Ok(Json(json!({
        "total_campaigns": total_campaigns,
        "active_campaigns": active_campaigns,
        "total_entries": total_entries,
        "total_wins": total_wins,
        "total_contacts": total_contacts,
        "total_loyalty_members": total_loyalty_members,
        "total_points_issued": total_points_issued,
        "total_redemptions": total_redemptions,
        "entry_growth_percent": (entry_growth_percent * 100.0).round() / 100.0,
        "win_rate_percent": (win_rate_percent * 100.0).round() / 100.0
    })))
}

/// GET /api/v1/analytics/campaigns — Per-campaign summary
pub async fn campaign_list(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Query(query): Query<AnalyticsQuery>,
) -> Result<Json<Value>, AppError> {
    let account_id = user.account_id.clone();
    let uuid = Uuid::parse_str(&account_id).map_err(|_| AppError::BadRequest("Invalid account ID".into()))?;

    let sort_col = query.sort.as_deref().unwrap_or("total_entries");
    let order = query.order.as_deref().unwrap_or("desc");
    let order_sql = if order.eq_ignore_ascii_case("asc") { "ASC" } else { "DESC" };

    // Validate sort column to prevent SQL injection
    let sort_col = match sort_col {
        "name" => "c.name",
        "entries" | "total_entries" => "total_entries",
        "wins" | "total_wins" => "total_wins",
        "win_rate" => "win_rate",
        "created_at" => "c.created_at",
        "type" => "c.type",
        _ => "total_entries",
    };

    let sql = format!(
        "SELECT c.id, c.name, c.slug, c.type, c.status, c.created_at,
                COUNT(DISTINCT e.id) as total_entries,
                COUNT(DISTINCT e.contact_id) as unique_contacts,
                COUNT(DISTINCT w.id) FILTER (WHERE w.id IS NOT NULL) as total_wins,
                COALESCE(AVG(e.score) FILTER (WHERE e.score IS NOT NULL), 0)::float as avg_score
         FROM campaigns c
         LEFT JOIN entries e ON e.campaign_id = c.id
         LEFT JOIN campaign_wins w ON w.campaign_id = c.id
         WHERE c.account_id = $1
         GROUP BY c.id, c.name, c.slug, c.type, c.status, c.created_at
         ORDER BY {} {}",
        sort_col, order_sql
    );

    let rows = sqlx::query(&sql)
        .bind(uuid)
        .fetch_all(&state.db)
        .await?;

    let mut campaigns: Vec<Value> = Vec::new();
    let mut campaign_ids: Vec<Uuid> = Vec::new();

    for row in &rows {
        let id: Uuid = row.get("id");
        campaign_ids.push(id);
    }

    // Batch fetch top source for all campaigns
    let mut source_map: HashMap<Uuid, String> = HashMap::new();
    if !campaign_ids.is_empty() {
        let source_rows = sqlx::query(
            "SELECT campaign_id, utm_source, cnt FROM (
                SELECT campaign_id, utm_source, COUNT(*) as cnt,
                    ROW_NUMBER() OVER (PARTITION BY campaign_id ORDER BY COUNT(*) DESC) as rn
                FROM entries
                WHERE campaign_id = ANY($1) AND utm_source IS NOT NULL AND utm_source != ''
                GROUP BY campaign_id, utm_source
            ) sub WHERE rn = 1"
        )
        .bind(&campaign_ids[..])
        .fetch_all(&state.db)
        .await?;

        for sr in source_rows {
            let cid: Uuid = sr.get("campaign_id");
            let src: String = sr.get("utm_source");
            source_map.insert(cid, src);
        }
    }

    for row in &rows {
        let id: Uuid = row.get("id");
        let total_entries: i64 = row.get("total_entries");
        let total_wins: i64 = row.get("total_wins");
        let win_rate: f64 = if total_entries > 0 {
            (total_wins as f64 / total_entries as f64) * 100.0
        } else {
            0.0
        };
        let avg_score: f64 = row.get("avg_score");

        campaigns.push(json!({
            "id": id,
            "name": row.get::<String, _>("name"),
            "slug": row.get::<String, _>("slug"),
            "type": row.get::<String, _>("type"),
            "status": row.get::<String, _>("status"),
            "total_entries": total_entries,
            "unique_contacts": row.get::<i64, _>("unique_contacts"),
            "total_wins": total_wins,
            "total_losses": total_entries.saturating_sub(total_wins),
            "win_rate": (win_rate * 100.0).round() / 100.0,
            "avg_score": if avg_score > 0.0 { Some(avg_score) } else { None },
            "top_source": source_map.get(&id).cloned(),
            "created_at": row.get::<chrono::DateTime<chrono::Utc>, _>("created_at")
        }));
    }

    Ok(Json(json!({ "campaigns": campaigns, "total": campaigns.len() })))
}

/// GET /api/v1/analytics/campaigns/{slug} — Single campaign drill-down
pub async fn campaign_detail(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(slug): Path<String>,
) -> Result<Json<Value>, AppError> {
    let account_id = user.account_id.clone();
    let uuid = Uuid::parse_str(&account_id).map_err(|_| AppError::BadRequest("Invalid account ID".into()))?;

    // Get campaign
    let campaign_row = sqlx::query(
        "SELECT id, name, slug, type, status, config, created_at, account_id,
                loyalty_program_id, auto_enroll_loyalty, loyalty_points_per_play
         FROM campaigns WHERE slug = $1 AND account_id = $2"
    )
    .bind(&slug)
    .bind(uuid)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Campaign not found".into()))?;

    let campaign_id: Uuid = campaign_row.get("id");

    let campaign = json!({
        "id": campaign_id,
        "name": campaign_row.get::<String, _>("name"),
        "slug": campaign_row.get::<String, _>("slug"),
        "type": campaign_row.get::<String, _>("type"),
        "status": campaign_row.get::<String, _>("status"),
        "config": campaign_row.get::<Value, _>("config"),
        "created_at": campaign_row.get::<chrono::DateTime<chrono::Utc>, _>("created_at"),
        "auto_enroll_loyalty": campaign_row.get::<bool, _>("auto_enroll_loyalty"),
        "loyalty_points_per_play": campaign_row.get::<i32, _>("loyalty_points_per_play")
    });

    // Entries over time
    let time_rows = sqlx::query(
        "SELECT created_at::date as date, COUNT(*) as count
         FROM entries WHERE campaign_id = $1
         GROUP BY created_at::date ORDER BY date"
    )
    .bind(campaign_id)
    .fetch_all(&state.db)
    .await?;

    let mut entries_over_time: Vec<Value> = Vec::new();
    let mut cumulative: i64 = 0;
    for tr in &time_rows {
        let cnt: i64 = tr.get("count");
        cumulative += cnt;
        let date: chrono::NaiveDate = tr.get("date");
        entries_over_time.push(json!({
            "date": date.format("%Y-%m-%d").to_string(),
            "count": cnt,
            "cumulative": cumulative
        }));
    }

    // Source breakdown
    let source_rows = sqlx::query(
        "SELECT COALESCE(NULLIF(utm_source, ''), 'direct') as source, COUNT(*) as count
         FROM entries WHERE campaign_id = $1
         GROUP BY source ORDER BY count DESC"
    )
    .bind(campaign_id)
    .fetch_all(&state.db)
    .await?;

    let source_total: i64 = source_rows.iter().map(|r| r.get::<i64, _>("count")).sum();
    let mut source_breakdown: Vec<Value> = Vec::new();
    for sr in &source_rows {
        let cnt: i64 = sr.get("count");
        let pct = if source_total > 0 { (cnt as f64 / source_total as f64) * 100.0 } else { 0.0 };
        source_breakdown.push(json!({
            "source": sr.get::<String, _>("source"),
            "count": cnt,
            "percent": (pct * 10.0).round() / 10.0
        }));
    }

    // Referrer breakdown
    let ref_rows = sqlx::query(
        "SELECT COALESCE(NULLIF(referrer_url, ''), 'direct') as domain, COUNT(*) as count
         FROM entries WHERE campaign_id = $1
         GROUP BY domain ORDER BY count DESC"
    )
    .bind(campaign_id)
    .fetch_all(&state.db)
    .await?;

    let referrer_breakdown: Vec<Value> = ref_rows.iter().map(|r| {
        json!({
            "domain": r.get::<String, _>("domain"),
            "count": r.get::<i64, _>("count")
        })
    }).collect();

    // Hourly heatmap
    let hour_rows = sqlx::query(
        "SELECT EXTRACT(HOUR FROM created_at)::int as hour, COUNT(*) as count
         FROM entries WHERE campaign_id = $1
         GROUP BY hour ORDER BY hour"
    )
    .bind(campaign_id)
    .fetch_all(&state.db)
    .await?;

    let hourly_heatmap: Vec<Value> = hour_rows.iter().map(|r| {
        json!({
            "hour": r.get::<i32, _>("hour"),
            "count": r.get::<i64, _>("count")
        })
    }).collect();

    // Prize distribution (from campaign_wins)
    let prize_rows = sqlx::query(
        "SELECT prize_label, COUNT(*) as total
         FROM campaign_wins WHERE campaign_id = $1
         GROUP BY prize_label ORDER BY total DESC"
    )
    .bind(campaign_id)
    .fetch_all(&state.db)
    .await?;

    let win_rate_by_prize: Vec<Value> = prize_rows.iter().map(|r| {
        json!({
            "prize_label": r.get::<String, _>("prize_label"),
            "total": r.get::<i64, _>("total")
        })
    }).collect();

    // Performance metrics
    let total_wins_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM campaign_wins WHERE campaign_id = $1"
    ).bind(campaign_id).fetch_one(&state.db).await.unwrap_or(0);

    let _total_entries_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM entries WHERE campaign_id = $1"
    ).bind(campaign_id).fetch_one(&state.db).await.unwrap_or(0);

    let total_redemptions: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM campaign_wins WHERE campaign_id = $1 AND redeemed = true"
    ).bind(campaign_id).fetch_one(&state.db).await.unwrap_or(0);

    let avg_score: Option<f64> = sqlx::query_scalar(
        "SELECT AVG(score::float) FROM entries WHERE campaign_id = $1 AND score IS NOT NULL"
    ).bind(campaign_id).fetch_one(&state.db).await.unwrap_or(None);

    let redemption_rate: f64 = if total_wins_count > 0 {
        (total_redemptions as f64 / total_wins_count as f64) * 100.0
    } else {
        0.0
    };

    let performance = json!({
        "avg_score": avg_score,
        "total_redemptions": total_redemptions,
        "redemption_rate": (redemption_rate * 100.0).round() / 100.0
    });

    // Loyalty bridge info
    let loyalty_row = sqlx::query(
        "SELECT c.auto_enroll_loyalty, c.loyalty_points_per_play,
                COUNT(DISTINCT lm.id) as members_gained,
                COALESCE(SUM(lm.lifetime_points), 0) as points_awarded
         FROM campaigns c
         LEFT JOIN loyalty_programs lp ON lp.id = c.loyalty_program_id
         LEFT JOIN loyalty_members lm ON lm.program_id = lp.id
         WHERE c.id = $1
         GROUP BY c.id, c.auto_enroll_loyalty, c.loyalty_points_per_play"
    )
    .bind(campaign_id)
    .fetch_optional(&state.db)
    .await?;

    let loyalty_bridge = if let Some(lr) = loyalty_row {
        json!({
            "auto_enroll": lr.get::<bool, _>("auto_enroll_loyalty"),
            "points_per_play": lr.get::<i32, _>("loyalty_points_per_play"),
            "members_gained": lr.get::<i64, _>("members_gained"),
            "points_awarded": lr.get::<i64, _>("points_awarded")
        })
    } else {
        json!({
            "auto_enroll": false,
            "points_per_play": 0,
            "members_gained": 0,
            "points_awarded": 0
        })
    };

    Ok(Json(json!({
        "campaign": campaign,
        "entries_over_time": entries_over_time,
        "source_breakdown": source_breakdown,
        "referrer_breakdown": referrer_breakdown,
        "hourly_heatmap": hourly_heatmap,
        "win_rate_by_prize": win_rate_by_prize,
        "performance": performance,
        "loyalty_bridge": loyalty_bridge
    })))
}

/// GET /api/v1/analytics/contacts — Contact analytics
pub async fn contacts_analytics(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let account_id = user.account_id.clone();
    let uuid = Uuid::parse_str(&account_id).map_err(|_| AppError::BadRequest("Invalid account ID".into()))?;

    let total_contacts: i64 = sqlx::query_scalar(
        "SELECT COUNT(DISTINCT e.contact_id) FROM entries e JOIN campaigns c ON c.id = e.campaign_id WHERE c.account_id = $1"
    ).bind(uuid).fetch_one(&state.db).await.unwrap_or(0);

    let new_contacts_30d: i64 = sqlx::query_scalar(
        "SELECT COUNT(DISTINCT e.contact_id) FROM entries e JOIN campaigns c ON c.id = e.campaign_id WHERE c.account_id = $1 AND e.created_at >= now() - interval '30 days'"
    ).bind(uuid).fetch_one(&state.db).await.unwrap_or(0);

    let repeat_entriers: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM (SELECT e.contact_id, COUNT(*) as cnt FROM entries e JOIN campaigns c ON c.id = e.campaign_id WHERE c.account_id = $1 GROUP BY e.contact_id HAVING COUNT(*) > 1) sub"
    ).bind(uuid).fetch_one(&state.db).await.unwrap_or(0);

    let repeat_entry_rate: f64 = if total_contacts > 0 {
        (repeat_entriers as f64 / total_contacts as f64) * 100.0
    } else {
        0.0
    };

    // Growth by day (last 30 days)
    let growth_rows = sqlx::query(
        "SELECT e.created_at::date as date, COUNT(DISTINCT e.contact_id) as new_contacts
         FROM entries e JOIN campaigns c ON c.id = e.campaign_id
         WHERE c.account_id = $1 AND e.created_at >= now() - interval '30 days'
         GROUP BY date ORDER BY date"
    )
    .bind(uuid)
    .fetch_all(&state.db)
    .await?;

    let growth_by_day: Vec<Value> = growth_rows.iter().map(|r| {
        let date: chrono::NaiveDate = r.get("date");
        json!({
            "date": date.format("%Y-%m-%d").to_string(),
            "new_contacts": r.get::<i64, _>("new_contacts")
        })
    }).collect();

    // Top contacts (by entry count)
    let top_rows = sqlx::query(
        "SELECT e.contact_id, ct.first_name, ct.last_name, ct.email, COUNT(*) as entry_count
         FROM entries e JOIN campaigns c ON c.id = e.campaign_id JOIN contacts ct ON ct.id = e.contact_id
         WHERE c.account_id = $1
         GROUP BY e.contact_id, ct.first_name, ct.last_name, ct.email
         ORDER BY entry_count DESC LIMIT 10"
    )
    .bind(uuid)
    .fetch_all(&state.db)
    .await?;

    let top_contacts: Vec<Value> = top_rows.iter().map(|r| {
        json!({
            "id": r.get::<Uuid, _>("contact_id"),
            "first_name": r.get::<Option<String>, _>("first_name"),
            "last_name": r.get::<Option<String>, _>("last_name"),
            "email": r.get::<Option<String>, _>("email"),
            "entry_count": r.get::<i64, _>("entry_count")
        })
    }).collect();

    Ok(Json(json!({
        "total_contacts": total_contacts,
        "new_contacts_30d": new_contacts_30d,
        "repeat_entriers": repeat_entriers,
        "repeat_entry_rate": (repeat_entry_rate * 100.0).round() / 100.0,
        "growth_by_day": growth_by_day,
        "top_contacts": top_contacts
    })))
}

/// GET /api/v1/analytics/loyalty — Loyalty program analytics
pub async fn loyalty_analytics(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let account_id = user.account_id.clone();
    let uuid = Uuid::parse_str(&account_id).map_err(|_| AppError::BadRequest("Invalid account ID".into()))?;

    let total_programs: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM loyalty_programs p JOIN campaigns c ON c.id = p.campaign_id WHERE c.account_id = $1"
    ).bind(uuid).fetch_one(&state.db).await.unwrap_or(0);

    let total_members: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM loyalty_members m JOIN loyalty_programs p ON p.id = m.program_id JOIN campaigns c ON c.id = p.campaign_id WHERE c.account_id = $1"
    ).bind(uuid).fetch_one(&state.db).await.unwrap_or(0);

    let total_points_issued: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(m.lifetime_points), 0) FROM loyalty_members m JOIN loyalty_programs p ON p.id = m.program_id JOIN campaigns c ON c.id = p.campaign_id WHERE c.account_id = $1"
    ).bind(uuid).fetch_one(&state.db).await.unwrap_or(0);

    let active_members_30d: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM loyalty_members m JOIN loyalty_programs p ON p.id = m.program_id JOIN campaigns c ON c.id = p.campaign_id WHERE c.account_id = $1 AND m.last_checkin_at >= now() - interval '30 days'"
    ).bind(uuid).fetch_one(&state.db).await.unwrap_or(0);

    // Programs with stats
    let prog_rows = sqlx::query(
        "SELECT p.id, p.name, p.points_per_checkin, p.is_active,
                COUNT(DISTINCT m.id) as member_count,
                COALESCE(AVG(m.points_balance), 0)::int as avg_balance
         FROM loyalty_programs p
         JOIN campaigns c ON c.id = p.campaign_id
         LEFT JOIN loyalty_members m ON m.program_id = p.id
         WHERE c.account_id = $1
         GROUP BY p.id, p.name, p.points_per_checkin, p.is_active"
    )
    .bind(uuid)
    .fetch_all(&state.db)
    .await?;

    let programs: Vec<Value> = prog_rows.iter().map(|r| {
        json!({
            "id": r.get::<Uuid, _>("id"),
            "name": r.get::<String, _>("name"),
            "points_per_checkin": r.get::<i32, _>("points_per_checkin"),
            "is_active": r.get::<bool, _>("is_active"),
            "member_count": r.get::<i64, _>("member_count"),
            "avg_balance": r.get::<i32, _>("avg_balance")
        })
    }).collect();

    // Top members
    let top_rows = sqlx::query(
        "SELECT m.id, m.points_balance, m.lifetime_points, ct.first_name, ct.last_name, ct.email
         FROM loyalty_members m
         JOIN loyalty_programs p ON p.id = m.program_id
         JOIN campaigns c ON c.id = p.campaign_id
         JOIN contacts ct ON ct.id = m.contact_id
         WHERE c.account_id = $1
         ORDER BY m.lifetime_points DESC LIMIT 10"
    )
    .bind(uuid)
    .fetch_all(&state.db)
    .await?;

    let top_members: Vec<Value> = top_rows.iter().map(|r| {
        json!({
            "id": r.get::<Uuid, _>("id"),
            "points_balance": r.get::<i32, _>("points_balance"),
            "lifetime_points": r.get::<i32, _>("lifetime_points"),
            "first_name": r.get::<Option<String>, _>("first_name"),
            "last_name": r.get::<Option<String>, _>("last_name"),
            "email": r.get::<Option<String>, _>("email")
        })
    }).collect();

    Ok(Json(json!({
        "total_programs": total_programs,
        "total_members": total_members,
        "total_points_issued": total_points_issued,
        "active_members_30d": active_members_30d,
        "programs": programs,
        "top_members": top_members
    })))
}

/// GET /api/v1/analytics/export?type=campaigns|contacts|entries — CSV export
pub async fn export_csv(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Query(query): Query<ExportQuery>,
) -> Result<axum::response::Response, AppError> {
    let account_id = user.account_id.clone();
    let uuid = Uuid::parse_str(&account_id).map_err(|_| AppError::BadRequest("Invalid account ID".into()))?;

    let export_type = query.r#type.as_deref().unwrap_or("campaigns");
    let mut csv = String::new();

    match export_type {
        "contacts" => {
            csv.push_str("Contact ID,First Name,Last Name,Email,Total Entries,First Seen,Last Seen\n");
            let rows = sqlx::query(
                "SELECT DISTINCT e.contact_id, ct.first_name, ct.last_name, ct.email,
                        COUNT(*) OVER (PARTITION BY e.contact_id) as total_entries,
                        MIN(e.created_at) OVER (PARTITION BY e.contact_id) as first_seen,
                        MAX(e.created_at) OVER (PARTITION BY e.contact_id) as last_seen
                 FROM entries e
                 JOIN campaigns c ON c.id = e.campaign_id
                 JOIN contacts ct ON ct.id = e.contact_id
                 WHERE c.account_id = $1
                 ORDER BY total_entries DESC"
            )
            .bind(uuid)
            .fetch_all(&state.db)
            .await?;

            for r in &rows {
                let id: Uuid = r.get("contact_id");
                let fn_ = r.get::<Option<String>, _>("first_name").unwrap_or_default();
                let ln = r.get::<Option<String>, _>("last_name").unwrap_or_default();
                let em = r.get::<Option<String>, _>("email").unwrap_or_default();
                let te: i64 = r.get("total_entries");
                let fs: chrono::DateTime<chrono::Utc> = r.get("first_seen");
                let ls: chrono::DateTime<chrono::Utc> = r.get("last_seen");
                csv.push_str(&format!("{},{},{},{},{},{},{}\n", id, esc_csv(&fn_), esc_csv(&ln), esc_csv(&em), te, fs.format("%Y-%m-%d %H:%M"), ls.format("%Y-%m-%d %H:%M")));
            }
        }
        "entries" => {
            csv.push_str("Entry ID,Campaign,Contact Email,Score,Outcome,Source,Referrer,Page URL,Created At\n");
            let rows = sqlx::query(
                "SELECT e.id, c.name as campaign_name, ct.email as contact_email,
                        e.score, e.outcome, e.utm_source, e.referrer_url, e.page_url, e.created_at
                 FROM entries e
                 JOIN campaigns c ON c.id = e.campaign_id
                 JOIN contacts ct ON ct.id = e.contact_id
                 WHERE c.account_id = $1
                 ORDER BY e.created_at DESC"
            )
            .bind(uuid)
            .fetch_all(&state.db)
            .await?;

            for r in &rows {
                let id: Uuid = r.get("id");
                let cn: String = r.get("campaign_name");
                let ce: Option<String> = r.get("contact_email");
                let sc: Option<i32> = r.get("score");
                let oc: Option<String> = r.get("outcome");
                let us: Option<String> = r.get("utm_source");
                let ru: Option<String> = r.get("referrer_url");
                let pu: Option<String> = r.get("page_url");
                let ca: chrono::DateTime<chrono::Utc> = r.get("created_at");
                csv.push_str(&format!("{},{},{},{},{},{},{},{},{}\n",
                    id, esc_csv(&cn), esc_csv(&ce.unwrap_or_default()),
                    sc.map(|s| s.to_string()).unwrap_or_default(),
                    oc.unwrap_or_default(),
                    us.unwrap_or_default(),
                    ru.unwrap_or_default(),
                    pu.unwrap_or_default(),
                    ca.format("%Y-%m-%d %H:%M")
                ));
            }
        }
        _ => {
            // Default: campaigns export
            csv.push_str("Campaign Name,Type,Status,Total Entries,Unique Contacts,Total Wins,Win Rate %,Created At\n");
            let rows = sqlx::query(
                "SELECT c.name, c.type, c.status,
                        COUNT(DISTINCT e.id) as total_entries,
                        COUNT(DISTINCT e.contact_id) as unique_contacts,
                        COUNT(DISTINCT w.id) FILTER (WHERE w.id IS NOT NULL) as total_wins,
                        c.created_at
                 FROM campaigns c
                 LEFT JOIN entries e ON e.campaign_id = c.id
                 LEFT JOIN campaign_wins w ON w.campaign_id = c.id
                 WHERE c.account_id = $1
                 GROUP BY c.id, c.name, c.type, c.status, c.created_at
                 ORDER BY c.created_at DESC"
            )
            .bind(uuid)
            .fetch_all(&state.db)
            .await?;

            for r in &rows {
                let name: String = r.get("name");
                let type_: String = r.get("type");
                let status: String = r.get("status");
                let te: i64 = r.get("total_entries");
                let uc: i64 = r.get("unique_contacts");
                let tw: i64 = r.get("total_wins");
                let wr = if te > 0 { (tw as f64 / te as f64) * 100.0 } else { 0.0 };
                let ca: chrono::DateTime<chrono::Utc> = r.get("created_at");
                csv.push_str(&format!("{},{},{},{},{},{},{:.1},{}\n",
                    esc_csv(&name), type_, status, te, uc, tw, wr, ca.format("%Y-%m-%d %H:%M")));
            }
        }
    }

    let response = axum::response::Response::builder()
        .header(header::CONTENT_TYPE, "text/csv; charset=utf-8")
        .header(header::CONTENT_DISPOSITION, format!("attachment; filename=\"analytics-{}-export.csv\"", export_type))
        .body(axum::body::Body::from(csv))
        .map_err(|e| AppError::Internal(format!("Failed to build CSV response: {}", e)))?;

    Ok(response)
}

fn esc_csv(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}
