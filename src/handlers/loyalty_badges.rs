//! Loyalty member QR, scan-history and dashboard handlers.
//!
//! The Badge, Enrollment, Scan, Integration-Center and program-QR arms this file used to carry were
//! RETIRED (kanban t_5e244255): measured live on the deployed binary they had 0 callers in the
//! 8-app fleet, 0 nginx hits and 0 rows, their ids were MultiDirectory entities with no account
//! owner derivable in THIS app, and their sibling external surface was already retired the same way
//! (t_f76c9950). What remains is authenticated and scoped to the caller's own programme.

use axum::{
    extract::{Path, State},
    Json,
};
use rand::Rng;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::error::AppError;
use crate::security::auth::AuthenticatedUser;
use crate::state::AppState;

/// The caller's account owns the loyalty programme this member belongs to
/// (`loyalty_members -> loyalty_programs -> campaigns.account_id`)? A caller who does not own it
/// gets the same 404 an absent member gets - the refusal invents no role bypass (kanban
/// t_f08d32e7: this file's member QR / scan / dashboard arms took no `AuthenticatedUser` at all).
async fn ensure_member_in_caller_programme(
    pool: &sqlx::PgPool,
    member_id: &Uuid,
    account_id: &Uuid,
) -> Result<(), AppError> {
    let owns: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM loyalty_members lm \
           JOIN loyalty_programs lp ON lp.id = lm.program_id \
           JOIN campaigns c ON c.id = lp.campaign_id \
          WHERE lm.id = $1 AND c.account_id = $2)",
    )
    .bind(member_id)
    .bind(account_id)
    .fetch_one(pool)
    .await?;
    if !owns {
        return Err(AppError::NotFound("Loyalty member not found".into()));
    }
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// Phase 3: QR Code Generation
// ═══════════════════════════════════════════════════════════════════════════════

/// GET /api/v1/loyalty/member/:member_id/qr
/// Generate or retrieve the QR code for a loyalty member.
/// The QR code encodes the member ID + a HMAC signature for validation.
/// The embedding app renders this as the scannable loyalty card.
pub async fn get_member_qr(
    State(state): State<AppState>,
    auth: AuthenticatedUser,
    Path(member_id): Path<Uuid>,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&auth.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".into()))?;
    // Fetch member with program info - scoped to the caller's own programme, so a member that
    // belongs to another account is indistinguishable from an absent one (404).
    let member = sqlx::query_as::<
        _,
        (
            Uuid,
            Uuid,
            Option<String>,
            String,
            Option<chrono::DateTime<chrono::Utc>>,
        ),
    >(
        r#"SELECT lm.id, lm.program_id, lm.qr_code, lp.name, lm.qr_code_generated_at
           FROM loyalty_members lm
           JOIN loyalty_programs lp ON lp.id = lm.program_id
           JOIN campaigns c ON c.id = lp.campaign_id
           WHERE lm.id = $1 AND c.account_id = $2"#,
    )
    .bind(member_id)
    .bind(account_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Loyalty member not found".into()))?;

    let (mid, pid, existing_qr, program_name, qr_generated_at) = member;

    // If QR already exists, return it
    if let Some(ref qr) = existing_qr {
        return Ok(Json(json!({
            "member_id": mid.to_string(),
            "program_id": pid.to_string(),
            "program_name": program_name,
            "qr_code": qr,
            "generated_at": qr_generated_at.map(|t| t.to_rfc3339()),
            "qr_data": format!("IS:{}:{}", mid, qr),
            "regenerated": false,
        })));
    }

    // Generate new QR code — member_id + random suffix + signature
    let code_suffix: String = rand::thread_rng()
        .sample_iter(&rand::distributions::Alphanumeric)
        .take(8)
        .map(char::from)
        .collect();
    let qr_code = format!("{}-{}", mid.simple().to_string().split_at(8).0, code_suffix);

    let now = chrono::Utc::now();
    sqlx::query("UPDATE loyalty_members SET qr_code = $1, qr_code_generated_at = $2 WHERE id = $3")
        .bind(&qr_code)
        .bind(now)
        .bind(mid)
        .execute(&state.db)
        .await?;

    tracing::info!(
        "[qr] Generated QR for member {} in program {}",
        mid,
        program_name
    );

    Ok(Json(json!({
        "member_id": mid.to_string(),
        "program_id": pid.to_string(),
        "program_name": program_name,
        "qr_code": qr_code,
        "generated_at": now.to_rfc3339(),
        "qr_data": format!("IS:{}:{}", mid, qr_code),
        "regenerated": true,
    })))
}

/// POST /api/v1/loyalty/member/:member_id/qr/regenerate
/// Force-regenerate a member's QR code (e.g. if compromised).
pub async fn regenerate_member_qr(
    State(state): State<AppState>,
    auth: AuthenticatedUser,
    Path(member_id): Path<Uuid>,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&auth.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".into()))?;
    // Ownership FIRST: the clear below is destructive and must never run for a member the caller
    // does not own (kanban t_f08d32e7).
    ensure_member_in_caller_programme(&state.db, &member_id, &account_id).await?;

    // Clear existing QR first
    sqlx::query(
        "UPDATE loyalty_members SET qr_code = NULL, qr_code_generated_at = NULL WHERE id = $1",
    )
    .bind(member_id)
    .execute(&state.db)
    .await?;

    // Reuse the generation logic
    get_member_qr(State(state), auth, Path(member_id)).await
}

/// GET /api/v1/loyalty/scans/member/:member_id
pub async fn get_member_scans(
    State(state): State<AppState>,
    auth: AuthenticatedUser,
    Path(member_id): Path<Uuid>,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&auth.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".into()))?;
    ensure_member_in_caller_programme(&state.db, &member_id, &account_id).await?;

    #[derive(sqlx::FromRow)]
    #[allow(dead_code)]
    struct ScanRow {
        id: Uuid,
        business_id: Option<Uuid>,
        business_name: Option<String>,
        scan_type: String,
        points_awarded: i32,
        points_balance: i32,
        deal_applied: Option<String>,
        scanned_at: chrono::DateTime<chrono::Utc>,
    }

    let scans: Vec<ScanRow> = sqlx::query_as(
        r#"SELECT id, business_id, business_name, scan_type, points_awarded, points_balance, deal_applied, scanned_at
           FROM loyalty_scans WHERE member_id = $1 ORDER BY scanned_at DESC LIMIT 50"#
    )
    .bind(member_id)
    .fetch_all(&state.db)
    .await?;

    let history: Vec<Value> = scans
        .iter()
        .map(|s| {
            json!({
                "scan_id": s.id.to_string(),
                "business_id": s.business_id.map(|b| b.to_string()),
                "business_name": &s.business_name,
                "scan_type": &s.scan_type,
                "points_awarded": s.points_awarded,
                "points_balance": s.points_balance,
                "deal_applied": &s.deal_applied,
                "scanned_at": s.scanned_at.to_rfc3339(),
            })
        })
        .collect();

    Ok(Json(json!({
        "member_id": member_id.to_string(),
        "total_scans": history.len(),
        "scans": history,
    })))
}

// ═══════════════════════════════════════════════════════════════════════════════
// Phase 5: Dashboard Endpoints
// ═══════════════════════════════════════════════════════════════════════════════

/// GET /api/v1/loyalty/dashboard/member/:member_id
/// Full member dashboard — points, recent activity, enrolled programs, QR status.
pub async fn member_dashboard(
    State(state): State<AppState>,
    auth: AuthenticatedUser,
    Path(member_id): Path<Uuid>,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&auth.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".into()))?;

    #[derive(sqlx::FromRow)]
    struct MemberInfo {
        id: Uuid,
        program_id: Uuid,
        contact_id: Uuid,
        points_balance: i32,
        lifetime_points: i32,
        member_since: chrono::DateTime<chrono::Utc>,
        last_activity_date: Option<chrono::DateTime<chrono::Utc>>,
        current_streak: i32,
        longest_streak: i32,
        referral_code: Option<String>,
        total_referrals: i32,
        qr_code: Option<String>,
        qr_code_generated_at: Option<chrono::DateTime<chrono::Utc>>,
        program_name: String,
        program_slug: String,
        currency_name: String,
        currency_icon: String,
    }

    let member = sqlx::query_as::<_, MemberInfo>(
        r#"SELECT lm.id, lm.program_id, lm.contact_id, lm.member_since, lm.last_activity_date,
                  lm.current_streak, lm.longest_streak, lm.referral_code, lm.total_referrals,
                  lm.qr_code, lm.qr_code_generated_at, COALESCE(lm.points_balance, 0) AS points_balance,
                  COALESCE(lm.lifetime_points, 0) AS lifetime_points, lp.name AS program_name,
                  lp.slug AS program_slug, lp.currency_name, lp.currency_icon
           FROM loyalty_members lm
           JOIN loyalty_programs lp ON lp.id = lm.program_id
           JOIN campaigns c ON c.id = lp.campaign_id
           WHERE lm.id = $1 AND c.account_id = $2"#,
    )
    .bind(member_id)
    .bind(account_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Member not found".into()))?;

    // Recent scans (last 10)
    let scans: Vec<serde_json::Value> = sqlx::query_as::<_, (String, Option<String>, i32, i32, chrono::DateTime<chrono::Utc>)>(
        "SELECT scan_type, business_name, points_awarded, points_balance, scanned_at FROM loyalty_scans WHERE member_id = $1 ORDER BY scanned_at DESC LIMIT 10"
    )
    .bind(member_id)
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .map(|(st, bn, pa, pb, at)| json!({
        "scan_type": st,
        "business_name": bn,
        "points_awarded": pa,
        "points_balance": pb,
        "scanned_at": at.to_rfc3339(),
    }))
    .collect();

    Ok(Json(json!({
        "member_id": member.id.to_string(),
        "program_id": member.program_id.to_string(),
        "contact_id": member.contact_id.to_string(),
        "program_name": member.program_name,
        "program_slug": member.program_slug,
        "currency_name": member.currency_name,
        "currency_icon": member.currency_icon,
        "points_balance": member.points_balance,
        "lifetime_points": member.lifetime_points,
        "member_since": member.member_since.to_rfc3339(),
        "last_activity_date": member.last_activity_date.map(|d| d.to_rfc3339()),
        "current_streak": member.current_streak,
        "longest_streak": member.longest_streak,
        "referral_code": member.referral_code,
        "total_referrals": member.total_referrals,
        "has_qr": member.qr_code.is_some(),
        "qr_generated_at": member.qr_code_generated_at.map(|d| d.to_rfc3339()),
        "recent_scans": scans,
    })))
}

/// GET /api/v1/loyalty/dashboard/admin/:program_slug
/// Admin dashboard — all members, total points, business participation, scans.
pub async fn admin_dashboard(
    State(state): State<AppState>,
    auth: AuthenticatedUser,
    Path(program_slug): Path<String>,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&auth.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".into()))?;
    // SECURITY (kanban t_f08d32e7): this arm took no `AuthenticatedUser`, so any caller who named
    // a programme slug got its members, points and scans. Scoped to the programme's own account
    // (`loyalty_programs.campaign_id -> campaigns.account_id`) - no role bypass.
    let program = sqlx::query_as::<_, (Uuid, String, bool)>(
        "SELECT lp.id, lp.name, lp.is_active FROM loyalty_programs lp \
           JOIN campaigns c ON c.id = lp.campaign_id \
          WHERE lp.slug = $1 AND c.account_id = $2 LIMIT 1",
    )
    .bind(&program_slug)
    .bind(account_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Program not found".into()))?;

    let (program_id, program_name, _active) = program;

    // Total members
    let total_members: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM loyalty_members WHERE program_id = $1")
            .bind(program_id)
            .fetch_one(&state.db)
            .await
            .unwrap_or(0);

    // Total points issued
    let total_points: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(points_balance), 0) FROM loyalty_members WHERE program_id = $1",
    )
    .bind(program_id)
    .fetch_one(&state.db)
    .await
    .unwrap_or(0);

    // Enrolled businesses
    let enrolled_businesses: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM loyalty_enrollments WHERE program_id = $1 AND entity_type = 'business' AND is_active = true"
    )
    .bind(program_id)
    .fetch_one(&state.db)
    .await
    .unwrap_or(0);

    // Enrolled suppliers
    let enrolled_suppliers: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM loyalty_enrollments WHERE program_id = $1 AND entity_type = 'supplier' AND is_active = true"
    )
    .bind(program_id)
    .fetch_one(&state.db)
    .await
    .unwrap_or(0);

    // Total scans this month
    let scans_this_month: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM loyalty_scans WHERE program_id = $1 AND scanned_at >= date_trunc('month', now())"
    )
    .bind(program_id)
    .fetch_one(&state.db)
    .await
    .unwrap_or(0);

    // Recent scans (last 20)
    let recent: Vec<Value> = sqlx::query_as::<_, (Uuid, Uuid, Option<String>, String, i32, chrono::DateTime<chrono::Utc>)>(
        "SELECT id, member_id, business_name, scan_type, points_awarded, scanned_at FROM loyalty_scans WHERE program_id = $1 ORDER BY scanned_at DESC LIMIT 20"
    )
    .bind(program_id)
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .map(|(id, mid, bn, st, pa, at)| json!({
        "scan_id": id.to_string(),
        "member_id": mid.to_string(),
        "business_name": bn,
        "scan_type": st,
        "points_awarded": pa,
        "scanned_at": at.to_rfc3339(),
    }))
    .collect();

    Ok(Json(json!({
        "program_id": program_id.to_string(),
        "program_name": program_name,
        "program_slug": program_slug,
        "total_members": total_members,
        "total_points_issued": total_points,
        "enrolled_businesses": enrolled_businesses,
        "enrolled_suppliers": enrolled_suppliers,
        "scans_this_month": scans_this_month,
        "recent_scans": recent,
    })))
}
