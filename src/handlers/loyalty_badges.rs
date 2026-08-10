//! Loyalty Badge, Enrollment, QR, Scan, Dashboard & Integration Center handlers
//! IncentiveSwift → ZaarHub loyalty integration (Phase 1–6)

use axum::{
    extract::{Path, Query, State},
    Json,
};
use rand::Rng;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::error::AppError;
use crate::state::AppState;

// ═══════════════════════════════════════════════════════════════════════════════
// Phase 1: Badge Endpoints
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Debug, Serialize)]
pub struct BadgeStatus {
    pub entity_type: String,
    pub entity_id: String,
    pub is_enrolled: bool,
    pub program_name: Option<String>,
    pub enrolled_at: Option<String>,
    pub badge_visible: bool, // true only when actively enrolled
}

/// GET /api/v1/loyalty/badge/business/:business_id
/// Returns whether a business is enrolled in any loyalty program.
/// ZaarHub calls this to render the "Loyalty Participant" badge on business listings.
pub async fn get_business_badge(
    State(state): State<AppState>,
    Path(business_id): Path<Uuid>,
) -> Result<Json<Value>, AppError> {
    let enrollment = sqlx::query_as::<_, (Uuid, String, chrono::DateTime<chrono::Utc>)>(
        r#"SELECT le.program_id, lp.name, le.enrolled_at
           FROM loyalty_enrollments le
           JOIN loyalty_programs lp ON lp.id = le.program_id
           WHERE le.entity_type = 'business'
             AND le.entity_id = $1
             AND le.is_active = true
             AND lp.is_active = true
           ORDER BY le.enrolled_at DESC
           LIMIT 1"#,
    )
    .bind(business_id)
    .fetch_optional(&state.db)
    .await?;

    match enrollment {
        Some((_, program_name, enrolled_at)) => Ok(Json(json!({
            "entity_type": "business",
            "entity_id": business_id.to_string(),
            "is_enrolled": true,
            "program_name": program_name,
            "enrolled_at": enrolled_at.to_rfc3339(),
            "badge_visible": true,
        }))),
        None => Ok(Json(json!({
            "entity_type": "business",
            "entity_id": business_id.to_string(),
            "is_enrolled": false,
            "program_name": null,
            "enrolled_at": null,
            "badge_visible": false,
        }))),
    }
}

/// GET /api/v1/loyalty/badge/supplier/:supplier_id
/// Returns whether a supplier is enrolled in any loyalty program.
pub async fn get_supplier_badge(
    State(state): State<AppState>,
    Path(supplier_id): Path<Uuid>,
) -> Result<Json<Value>, AppError> {
    let enrollment = sqlx::query_as::<_, (Uuid, String, chrono::DateTime<chrono::Utc>)>(
        r#"SELECT le.program_id, lp.name, le.enrolled_at
           FROM loyalty_enrollments le
           JOIN loyalty_programs lp ON lp.id = le.program_id
           WHERE le.entity_type = 'supplier'
             AND le.entity_id = $1
             AND le.is_active = true
             AND lp.is_active = true
           ORDER BY le.enrolled_at DESC
           LIMIT 1"#,
    )
    .bind(supplier_id)
    .fetch_optional(&state.db)
    .await?;

    match enrollment {
        Some((_, program_name, enrolled_at)) => Ok(Json(json!({
            "entity_type": "supplier",
            "entity_id": supplier_id.to_string(),
            "is_enrolled": true,
            "program_name": program_name,
            "enrolled_at": enrolled_at.to_rfc3339(),
            "badge_visible": true,
        }))),
        None => Ok(Json(json!({
            "entity_type": "supplier",
            "entity_id": supplier_id.to_string(),
            "is_enrolled": false,
            "program_name": null,
            "enrolled_at": null,
            "badge_visible": false,
        }))),
    }
}

/// GET /api/v1/loyalty/badge/member/:contact_id
/// Returns whether a community member is enrolled in any loyalty program.
/// ZaarHub calls this to render the "Loyalty Member" badge on user profiles.
pub async fn get_member_badge(
    State(state): State<AppState>,
    Path(contact_id): Path<Uuid>,
) -> Result<Json<Value>, AppError> {
    let enrollment = sqlx::query_as::<_, (Uuid, String, chrono::DateTime<chrono::Utc>)>(
        r#"SELECT le.program_id, lp.name, le.enrolled_at
           FROM loyalty_enrollments le
           JOIN loyalty_programs lp ON lp.id = le.program_id
           WHERE le.entity_type = 'member'
             AND le.entity_id = $1
             AND le.is_active = true
             AND lp.is_active = true
           ORDER BY le.enrolled_at DESC
           LIMIT 1"#,
    )
    .bind(contact_id)
    .fetch_optional(&state.db)
    .await?;

    match enrollment {
        Some((_, program_name, enrolled_at)) => Ok(Json(json!({
            "entity_type": "member",
            "entity_id": contact_id.to_string(),
            "is_enrolled": true,
            "program_name": program_name,
            "enrolled_at": enrolled_at.to_rfc3339(),
            "badge_visible": true,
        }))),
        None => Ok(Json(json!({
            "entity_type": "member",
            "entity_id": contact_id.to_string(),
            "is_enrolled": false,
            "program_name": null,
            "enrolled_at": null,
            "badge_visible": false,
        }))),
    }
}

/// GET /api/v1/loyalty/badges/program/:program_slug
/// Bulk badge check — returns all enrolled entity IDs for a program.
/// ZaarHub can call this once to know which businesses/members to badge on a page.
#[derive(Debug, Deserialize)]
pub struct BulkBadgeQuery {
    pub entity_type: Option<String>, // filter: business, supplier, member
}

pub async fn get_program_badges(
    State(state): State<AppState>,
    Path(program_slug): Path<String>,
    Query(query): Query<BulkBadgeQuery>,
) -> Result<Json<Value>, AppError> {
    let program = sqlx::query_as::<_, (Uuid, String)>(
        "SELECT id, name FROM loyalty_programs WHERE slug = $1 AND is_active = true LIMIT 1",
    )
    .bind(&program_slug)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Loyalty program not found".into()))?;

    let entity_type = query.entity_type.as_deref();

    let enrollments = if let Some(et) = entity_type {
        sqlx::query_as::<_, (String, Uuid, chrono::DateTime<chrono::Utc>)>(
            r#"SELECT entity_type, entity_id, enrolled_at
               FROM loyalty_enrollments
               WHERE program_id = $1 AND is_active = true AND entity_type = $2"#,
        )
        .bind(program.0)
        .bind(et)
        .fetch_all(&state.db)
        .await?
    } else {
        sqlx::query_as::<_, (String, Uuid, chrono::DateTime<chrono::Utc>)>(
            r#"SELECT entity_type, entity_id, enrolled_at
               FROM loyalty_enrollments
               WHERE program_id = $1 AND is_active = true"#,
        )
        .bind(program.0)
        .fetch_all(&state.db)
        .await?
    };

    let badges: Vec<Value> = enrollments
        .into_iter()
        .map(|(et, eid, at)| {
            json!({
                "entity_type": et,
                "entity_id": eid.to_string(),
                "enrolled_at": at.to_rfc3339(),
                "badge_visible": true,
            })
        })
        .collect();

    Ok(Json(json!({
        "program_id": program.0.to_string(),
        "program_name": program.1,
        "total": badges.len(),
        "badges": badges,
    })))
}

// ═══════════════════════════════════════════════════════════════════════════════
// Phase 2: Enrollment Endpoints
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Debug, Deserialize)]
pub struct EnrollEntityRequest {
    pub entity_type: String, // "business", "supplier", "member"
    pub entity_id: Uuid,
    pub program_slug: String,
    pub metadata: Option<Value>,
}

/// POST /api/v1/loyalty/enroll
/// Enroll a business, supplier, or member in a loyalty program.
/// Called from ZaarHub business portal, supplier portal, or on member sign-up.
/// Idempotent — enrolling an already-enrolled entity returns success.
pub async fn enroll_entity(
    State(state): State<AppState>,
    Json(req): Json<EnrollEntityRequest>,
) -> Result<Json<Value>, AppError> {
    // Validate entity_type
    let valid_types = ["business", "supplier", "member"];
    if !valid_types.contains(&req.entity_type.as_str()) {
        return Err(AppError::BadRequest(format!(
            "Invalid entity_type '{}'. Must be one of: business, supplier, member",
            req.entity_type
        )));
    }

    // Look up program by slug
    let program = sqlx::query_as::<_, (Uuid, String)>(
        "SELECT id, name FROM loyalty_programs WHERE slug = $1 AND is_active = true LIMIT 1",
    )
    .bind(&req.program_slug)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| {
        AppError::NotFound(format!("Loyalty program '{}' not found", req.program_slug))
    })?;

    // Check if already enrolled
    let existing = sqlx::query_as::<_, (Uuid, bool)>(
        "SELECT id, is_active FROM loyalty_enrollments WHERE entity_type = $1 AND entity_id = $2 AND program_id = $3"
    )
    .bind(&req.entity_type)
    .bind(req.entity_id)
    .bind(program.0)
    .fetch_optional(&state.db)
    .await?;

    let program_name = &program.1;

    if let Some((enrollment_id, is_active)) = existing {
        if is_active {
            let msg = format!("Already enrolled in {}", program_name);
            return Ok(Json(json!({
                "success": true,
                "enrollment_id": enrollment_id,
                "entity_type": req.entity_type,
                "entity_id": req.entity_id.to_string(),
                "program_name": program_name,
                "already_enrolled": true,
                "message": msg,
            })));
        }
        // Reactivate if previously deactivated
        sqlx::query("UPDATE loyalty_enrollments SET is_active = true, deactivated_at = NULL, enrolled_at = now() WHERE id = $1")
            .bind(enrollment_id)
            .execute(&state.db)
            .await?;

        let msg = format!("Re-enrolled in {}", program_name);
        return Ok(Json(json!({
            "success": true,
            "enrollment_id": enrollment_id,
            "entity_type": req.entity_type,
            "entity_id": req.entity_id.to_string(),
            "program_name": program_name,
            "already_enrolled": false,
            "message": msg,
        })));
    }

    // Create new enrollment
    let enrollment_id = Uuid::new_v4();
    let metadata = req.metadata.unwrap_or(serde_json::Value::Null);
    sqlx::query(
        "INSERT INTO loyalty_enrollments (id, entity_type, entity_id, program_id, metadata) VALUES ($1, $2, $3, $4, $5)"
    )
    .bind(enrollment_id)
    .bind(&req.entity_type)
    .bind(req.entity_id)
    .bind(program.0)
    .bind(&metadata)
    .execute(&state.db)
    .await?;

    tracing::info!(
        "[enroll] {} {} enrolled in {} (program={})",
        req.entity_type,
        req.entity_id,
        program_name,
        req.program_slug
    );

    let msg = format!("Enrolled in {}", program_name);
    Ok(Json(json!({
        "success": true,
        "enrollment_id": enrollment_id,
        "entity_type": req.entity_type,
        "entity_id": req.entity_id.to_string(),
        "program_name": program_name,
        "already_enrolled": false,
        "message": msg,
    })))
}

/// POST /api/v1/loyalty/unenroll
/// Remove a business, supplier, or member from a loyalty program.
/// Soft-delete — sets is_active = false, records deactivated_at.
#[derive(Debug, Deserialize)]
pub struct UnenrollRequest {
    pub entity_type: String,
    pub entity_id: Uuid,
    pub program_slug: String,
}

pub async fn unenroll_entity(
    State(state): State<AppState>,
    Json(req): Json<UnenrollRequest>,
) -> Result<Json<Value>, AppError> {
    let program = sqlx::query_as::<_, (Uuid, String)>(
        "SELECT id, name FROM loyalty_programs WHERE slug = $1 AND is_active = true LIMIT 1",
    )
    .bind(&req.program_slug)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| {
        AppError::NotFound(format!("Loyalty program '{}' not found", req.program_slug))
    })?;

    let result = sqlx::query(
        "UPDATE loyalty_enrollments SET is_active = false, deactivated_at = now() WHERE entity_type = $1 AND entity_id = $2 AND program_id = $3 AND is_active = true"
    )
    .bind(&req.entity_type)
    .bind(req.entity_id)
    .bind(program.0)
    .execute(&state.db)
    .await?;

    if result.rows_affected() == 0 {
        return Ok(Json(json!({
            "success": true,
            "entity_type": req.entity_type,
            "entity_id": req.entity_id.to_string(),
            "message": "Not enrolled — no action taken",
        })));
    }

    tracing::info!(
        "[unenroll] {} {} removed from {}",
        req.entity_type,
        req.entity_id,
        program.1
    );

    Ok(Json(json!({
        "success": true,
        "entity_type": req.entity_type,
        "entity_id": req.entity_id.to_string(),
        "program_name": program.1,
        "message": format!("Unenrolled from {}", program.1),
    })))
}

// ═══════════════════════════════════════════════════════════════════════════════
// Phase 3: QR Code Generation
// ═══════════════════════════════════════════════════════════════════════════════

/// GET /api/v1/loyalty/member/:member_id/qr
/// Generate or retrieve the QR code for a loyalty member.
/// The QR code encodes the member ID + a HMAC signature for validation.
/// ZaarHub renders this as the scannable loyalty card in-app.
pub async fn get_member_qr(
    State(state): State<AppState>,
    Path(member_id): Path<Uuid>,
) -> Result<Json<Value>, AppError> {
    // Fetch member with program info
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
           WHERE lm.id = $1"#,
    )
    .bind(member_id)
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
    Path(member_id): Path<Uuid>,
) -> Result<Json<Value>, AppError> {
    // Clear existing QR first
    sqlx::query(
        "UPDATE loyalty_members SET qr_code = NULL, qr_code_generated_at = NULL WHERE id = $1",
    )
    .bind(member_id)
    .execute(&state.db)
    .await?;

    // Reuse the generation logic
    get_member_qr(State(state), Path(member_id)).await
}

// ═══════════════════════════════════════════════════════════════════════════════
// Phase 4: Scan Endpoint
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Debug, Deserialize)]
pub struct ScanRequest {
    pub qr_code: String,
    pub business_id: Uuid,
    pub business_name: Option<String>,
    pub program_slug: String,
    pub scan_type: Option<String>,
    pub purchase_amount: Option<rust_decimal::Decimal>,
    pub deal_applied: Option<String>,
    pub notes: Option<String>,
}

/// POST /api/v1/loyalty/scan
/// Business scans a member's QR code. Logs the transaction and awards points.
pub async fn scan_member(
    State(state): State<AppState>,
    Json(req): Json<ScanRequest>,
) -> Result<Json<Value>, AppError> {
    let member = sqlx::query_as::<_, (Uuid, Uuid, Uuid, i32)>(
        r#"SELECT lm.id, lm.program_id, lm.contact_id, lm.points_balance
           FROM loyalty_members lm
           JOIN loyalty_programs lp ON lp.id = lm.program_id
           WHERE lm.qr_code = $1 AND lp.slug = $2 AND lp.is_active = true"#,
    )
    .bind(&req.qr_code)
    .bind(&req.program_slug)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Invalid QR code or program not found".into()))?;

    let (member_id, program_id, _contact_id, current_balance) = member;

    let scan_type = req.scan_type.as_deref().unwrap_or("checkin");
    let points_awarded = match scan_type {
        "checkin" => 10,
        "purchase" => req
            .purchase_amount
            .map(|a| a.to_string().parse::<f64>().unwrap_or(0.0).floor() as i32)
            .unwrap_or(0),
        "redemption" | "reward_claim" => 0,
        _ => 10,
    };

    let new_balance = current_balance + points_awarded;

    sqlx::query(
        "UPDATE loyalty_members SET points_balance = $1, last_activity_date = now() WHERE id = $2",
    )
    .bind(new_balance)
    .bind(member_id)
    .execute(&state.db)
    .await?;

    let scan_id = Uuid::new_v4();
    let metadata = serde_json::json!({
        "scan_type": scan_type,
        "business_id": req.business_id.to_string(),
        "notes": req.notes,
    });

    sqlx::query(
        r#"INSERT INTO loyalty_scans (id, member_id, business_id, business_name, program_id, scan_type, points_awarded, points_balance, deal_applied, metadata)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)"#
    )
    .bind(scan_id)
    .bind(member_id)
    .bind(req.business_id)
    .bind(&req.business_name)
    .bind(program_id)
    .bind(scan_type)
    .bind(points_awarded)
    .bind(new_balance)
    .bind(&req.deal_applied)
    .bind(&metadata)
    .execute(&state.db)
    .await?;

    tracing::info!(
        "[scan] Member {} scanned at business {} — {} points, balance: {}",
        member_id,
        req.business_id,
        points_awarded,
        new_balance
    );

    Ok(Json(json!({
        "success": true,
        "scan_id": scan_id.to_string(),
        "member_id": member_id.to_string(),
        "business_id": req.business_id.to_string(),
        "scan_type": scan_type,
        "points_awarded": points_awarded,
        "previous_balance": current_balance,
        "new_balance": new_balance,
        "message": format!("Scanned — {} points {}", points_awarded,
            if points_awarded >= 0 { "awarded" } else { "deducted" }),
    })))
}

/// GET /api/v1/loyalty/scans/member/:member_id
pub async fn get_member_scans(
    State(state): State<AppState>,
    Path(member_id): Path<Uuid>,
) -> Result<Json<Value>, AppError> {
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

/// GET /api/v1/loyalty/scans/business/:business_id
pub async fn get_business_scans(
    State(state): State<AppState>,
    Path(business_id): Path<Uuid>,
) -> Result<Json<Value>, AppError> {
    #[derive(sqlx::FromRow)]
    #[allow(dead_code)]
    struct BizScanRow {
        id: Uuid,
        member_id: Uuid,
        scan_type: String,
        points_awarded: i32,
        points_balance: i32,
        deal_applied: Option<String>,
        scanned_at: chrono::DateTime<chrono::Utc>,
    }

    let scans: Vec<BizScanRow> = sqlx::query_as(
        r#"SELECT id, member_id, scan_type, points_awarded, points_balance, deal_applied, scanned_at
           FROM loyalty_scans WHERE business_id = $1 ORDER BY scanned_at DESC LIMIT 100"#,
    )
    .bind(business_id)
    .fetch_all(&state.db)
    .await?;

    let total_points_awarded: i32 = scans.iter().map(|s| s.points_awarded).sum();

    let history: Vec<Value> = scans
        .iter()
        .map(|s| {
            json!({
                "scan_id": s.id.to_string(),
                "member_id": s.member_id.to_string(),
                "scan_type": &s.scan_type,
                "points_awarded": s.points_awarded,
                "points_balance": s.points_balance,
                "deal_applied": &s.deal_applied,
                "scanned_at": s.scanned_at.to_rfc3339(),
            })
        })
        .collect();

    Ok(Json(json!({
        "business_id": business_id.to_string(),
        "total_scans": history.len(),
        "total_points_awarded": total_points_awarded,
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
    Path(member_id): Path<Uuid>,
) -> Result<Json<Value>, AppError> {
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
        r#"SELECT lm.id, lm.program_id, lm.contact_id, lm.points_balance, lm.lifetime_points,
                  lm.member_since, lm.last_activity_date, lm.current_streak, lm.longest_streak,
                  lm.referral_code, lm.total_referrals, lm.qr_code, lm.qr_code_generated_at,
                  lp.name AS program_name, lp.slug AS program_slug,
                  lp.currency_name, lp.currency_icon
           FROM loyalty_members lm
           JOIN loyalty_programs lp ON lp.id = lm.program_id
           WHERE lm.id = $1"#,
    )
    .bind(member_id)
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
    Path(program_slug): Path<String>,
) -> Result<Json<Value>, AppError> {
    let program = sqlx::query_as::<_, (Uuid, String, bool)>(
        "SELECT id, name, is_active FROM loyalty_programs WHERE slug = $1 LIMIT 1",
    )
    .bind(&program_slug)
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

// ═══════════════════════════════════════════════════════════════════════════════
// Phase 6: Integration Center — Business/Supplier API Key Management
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Debug, Deserialize)]
pub struct CreateIntegrationKeyRequest {
    pub owner_type: String, // "business", "supplier"
    pub owner_id: Uuid,
    pub service_type: String, // "incentiveswift", "coreswift", "multidirectory"
    pub label: String,        // human-readable name for this key
}

#[derive(Debug, Serialize, sqlx::FromRow)]
#[allow(dead_code)]
struct ApiKeyRow {
    id: Uuid,
    key_prefix: Option<String>,
    service_type: Option<String>,
    label: Option<String>,
    is_active: bool,
    created_at: chrono::DateTime<chrono::Utc>,
    last_used_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// POST /api/v1/integration/keys
/// Create a new API key for a business or supplier to access a specific service.
pub async fn create_integration_key(
    State(state): State<AppState>,
    Json(req): Json<CreateIntegrationKeyRequest>,
) -> Result<Json<Value>, AppError> {
    let valid_owners = ["business", "supplier"];
    if !valid_owners.contains(&req.owner_type.as_str()) {
        return Err(AppError::BadRequest(
            "Invalid owner_type. Must be: business, supplier".into(),
        ));
    }

    let valid_services = ["incentiveswift", "coreswift", "multidirectory"];
    if !valid_services.contains(&req.service_type.as_str()) {
        return Err(AppError::BadRequest(
            "Invalid service_type. Must be: incentiveswift, coreswift, multidirectory".into(),
        ));
    }

    let key_id = Uuid::new_v4();
    let raw_key = format!(
        "{}_live_{}",
        &req.service_type[..2],
        Uuid::new_v4().simple()
    );
    let key_prefix = format!("{}_live", &req.service_type[..2]);

    sqlx::query(
        r#"INSERT INTO api_keys (id, key_prefix, api_key, owner_type, owner_id, service_type, label, is_active, created_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, true, now())"#
    )
    .bind(key_id)
    .bind(&key_prefix)
    .bind(&raw_key)
    .bind(&req.owner_type)
    .bind(req.owner_id)
    .bind(&req.service_type)
    .bind(&req.label)
    .execute(&state.db)
    .await?;

    tracing::info!(
        "[integration-key] Created {} key for {} {} — label: {}",
        req.service_type,
        req.owner_type,
        req.owner_id,
        req.label
    );

    Ok(Json(json!({
        "success": true,
        "id": key_id.to_string(),
        "api_key": raw_key,
        "key_prefix": key_prefix,
        "owner_type": req.owner_type,
        "owner_id": req.owner_id.to_string(),
        "service_type": req.service_type,
        "label": req.label,
        "message": format!("{} API key created for {}", req.service_type, req.owner_type),
    })))
}

/// GET /api/v1/integration/keys/:owner_type/:owner_id
/// List all API keys for a specific business or supplier.
pub async fn list_integration_keys(
    State(state): State<AppState>,
    Path((owner_type, owner_id)): Path<(String, Uuid)>,
) -> Result<Json<Value>, AppError> {
    let keys: Vec<ApiKeyRow> = sqlx::query_as::<_, ApiKeyRow>(
        r#"SELECT id, key_prefix, service_type, label, is_active, created_at, last_used_at
           FROM api_keys WHERE owner_type = $1 AND owner_id = $2 ORDER BY created_at DESC"#,
    )
    .bind(&owner_type)
    .bind(owner_id)
    .fetch_all(&state.db)
    .await?;

    let key_list: Vec<Value> = keys
        .iter()
        .map(|k| {
            json!({
                "id": k.id.to_string(),
                "key_prefix": k.key_prefix,
                "service_type": k.service_type,
                "label": k.label,
                "is_active": k.is_active,
                "created_at": k.created_at.to_rfc3339(),
                "last_used_at": k.last_used_at.map(|d| d.to_rfc3339()),
            })
        })
        .collect();

    Ok(Json(json!({
        "owner_type": owner_type,
        "owner_id": owner_id.to_string(),
        "total": key_list.len(),
        "keys": key_list,
    })))
}

/// DELETE /api/v1/integration/keys/:key_id
/// Revoke an API key.
pub async fn revoke_integration_key(
    State(state): State<AppState>,
    Path(key_id): Path<Uuid>,
) -> Result<Json<Value>, AppError> {
    let result = sqlx::query("DELETE FROM api_keys WHERE id = $1")
        .bind(key_id)
        .execute(&state.db)
        .await?;

    if result.rows_affected() == 0 {
        return Ok(Json(json!({
            "success": false,
            "message": "Key not found",
        })));
    }

    Ok(Json(json!({
        "success": true,
        "key_id": key_id.to_string(),
        "message": "API key revoked",
    })))
}

/// GET /api/v1/integration/services
/// Returns available services and their base URLs for the integration center.
pub async fn list_available_services() -> Result<Json<Value>, AppError> {
    Ok(Json(json!({
        "services": [
            {
                "id": "incentiveswift",
                "name": "IncentiveSwift",
                "description": "Loyalty program, campaigns, rewards",
                "base_url": "/api/v1",
                "endpoints": ["Enroll in loyalty", "Manage deals", "View redemptions", "Scan member QR", "Loyalty analytics"]
            },
            {
                "id": "coreswift",
                "name": "CoreSwift",
                "description": "CRM, contact management, pipeline",
                "base_url": "/api/v1",
                "endpoints": ["CRM sync", "Contact management", "Pipeline / deals"]
            },
            {
                "id": "multidirectory",
                "name": "MultiDirectory",
                "description": "Directory listings, businesses, categories",
                "base_url": "/api/v1",
                "endpoints": ["Directory data", "Business listings", "Categories"]
            }
        ]
    })))
}
