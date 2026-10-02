//! Call logs and deal tracking — two features David's spec requires, wired to tables that already
//! existed with zero code references.
//!
//! David's spec (`audits/incentiveswift-verify/`) listed both as MISSING. Measured 2026-10-02, they
//! were missing as CODE, not as schema:
//!
//!   * `call_logs` had id/tenant_id/caller/callee/duration_secs/notes — 0 rows, 0 references, no route.
//!   * `business_loyalty_deals` is a COMPLETE deal table (business, program, deal_type/value,
//!     points_required, validity window, redemption limits and counters) — also 0 rows, 0 references.
//!
//! Neither table is re-created here. A second deals table beside a good unused one is how a schema ends
//! up with two half-features and no answer to "which is the real one".
//!
//! SCOPING, and why the two differ:
//!   * call logs carry `tenant_id` directly, so they scope on it.
//!   * deals scope on their OWN column, `owner_account_id` (added 2026-10-02). They carry no tenant
//!     column, and `loyalty_programs` carries none either — a program reaches an account through its
//!     campaign, EXCEPT a program with no campaign, which the app treats as the SHARED platform program
//!     (`handlers/loyalty.rs::list_programs`: `c.account_id = $1 OR lp.campaign_id IS NULL`).
//!     Scoping deals by that programme was tried and MEASURED WRONG: the only programme in the database
//!     has campaign_id NULL, so every account could see and edit every deal (proof: "another account
//!     cannot edit this deal -> http 200"). A shared PROGRAM is legitimate; a shared DEAL is not.
//!     `program_is_usable` still gates which programme a deal may point at — it just no longer decides
//!     who owns the row.

use axum::{
    extract::{Path, State},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::error::AppError;
use crate::security::auth::AuthenticatedUser;
use crate::state::AppState;

// ─────────────────────────────────────────────────────────────────────────────── call logs

#[derive(Deserialize)]
pub struct CallLogInput {
    pub direction: Option<String>,
    pub caller: Option<String>,
    pub callee: Option<String>,
    pub outcome: Option<String>,
    pub duration_secs: Option<i32>,
    pub notes: Option<String>,
    pub contact_id: Option<Uuid>,
    /// ISO-8601. Absent = now, which is the common case (you log a call as it ends).
    pub called_at: Option<String>,
}

fn account_of(user: &AuthenticatedUser) -> Result<Uuid, AppError> {
    Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("This session has no usable account id".to_string()))
}

fn direction_of(v: Option<&str>) -> Result<String, AppError> {
    match v.unwrap_or("outbound") {
        "inbound" | "outbound" => Ok(v.unwrap_or("outbound").to_string()),
        other => Err(AppError::BadRequest(format!(
            "direction must be 'inbound' or 'outbound', got '{other}'"
        ))),
    }
}

/// GET /api/v1/call-logs — this account's calls, newest first.
pub async fn list_calls(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let account = account_of(&user)?;
    let rows: Vec<(
        Uuid,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<i32>,
        Option<String>,
        Option<Uuid>,
        Option<chrono::DateTime<chrono::Utc>>,
    )> = sqlx::query_as(
        r#"SELECT id, direction, caller, callee, outcome, duration_secs, notes, contact_id, called_at
             FROM call_logs
            WHERE tenant_id = $1
            ORDER BY called_at DESC
            LIMIT 500"#,
    )
    .bind(account)
    .fetch_all(&state.db)
    .await?;

    let items: Vec<Value> = rows
        .into_iter()
        .map(
            |(id, direction, caller, callee, outcome, secs, notes, contact, at)| {
                json!({
                    "id": id, "direction": direction, "caller": caller, "callee": callee,
                    "outcome": outcome, "duration_secs": secs, "notes": notes,
                    "contact_id": contact, "called_at": at,
                })
            },
        )
        .collect();
    Ok(Json(json!({ "calls": items, "count": items.len() })))
}

/// POST /api/v1/call-logs
pub async fn create_call(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<CallLogInput>,
) -> Result<Json<Value>, AppError> {
    let account = account_of(&user)?;
    let direction = direction_of(body.direction.as_deref())?;
    if body.caller.as_deref().unwrap_or("").trim().is_empty()
        && body.callee.as_deref().unwrap_or("").trim().is_empty()
    {
        return Err(AppError::BadRequest(
            "A call log needs at least a caller or a callee — otherwise there is no number to ring back"
                .to_string(),
        ));
    }
    let id = Uuid::new_v4();
    sqlx::query(
        r#"INSERT INTO call_logs
              (id, tenant_id, direction, caller, callee, outcome, duration_secs, notes, contact_id,
               called_at, created_by)
           VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,
                   COALESCE($10::timestamptz, now()), $11)"#,
    )
    .bind(id)
    .bind(account)
    .bind(&direction)
    .bind(&body.caller)
    .bind(&body.callee)
    .bind(&body.outcome)
    .bind(body.duration_secs)
    .bind(&body.notes)
    .bind(body.contact_id)
    .bind(&body.called_at)
    .bind(account)
    .execute(&state.db)
    .await?;
    Ok(Json(json!({ "id": id, "created": true })))
}

/// PUT /api/v1/call-logs/:id
pub async fn update_call(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(id): Path<Uuid>,
    Json(body): Json<CallLogInput>,
) -> Result<Json<Value>, AppError> {
    let account = account_of(&user)?;
    let direction = direction_of(body.direction.as_deref())?;
    let res = sqlx::query(
        r#"UPDATE call_logs
              SET direction = $3, caller = $4, callee = $5, outcome = $6, duration_secs = $7,
                  notes = $8, contact_id = $9,
                  called_at = COALESCE($10::timestamptz, called_at),
                  updated_at = now()
            WHERE id = $1 AND tenant_id = $2"#,
    )
    .bind(id)
    .bind(account)
    .bind(&direction)
    .bind(&body.caller)
    .bind(&body.callee)
    .bind(&body.outcome)
    .bind(body.duration_secs)
    .bind(&body.notes)
    .bind(body.contact_id)
    .bind(&body.called_at)
    .execute(&state.db)
    .await?;
    // One row touched means it existed AND was ours: the WHERE carries both conditions, so a log
    // belonging to another account is indistinguishable from a missing one — which is the point.
    if res.rows_affected() == 0 {
        return Err(AppError::NotFound("No such call log".to_string()));
    }
    Ok(Json(json!({ "id": id, "updated": true })))
}

/// DELETE /api/v1/call-logs/:id
pub async fn delete_call(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, AppError> {
    let account = account_of(&user)?;
    let res = sqlx::query("DELETE FROM call_logs WHERE id = $1 AND tenant_id = $2")
        .bind(id)
        .bind(account)
        .execute(&state.db)
        .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::NotFound("No such call log".to_string()));
    }
    Ok(Json(json!({ "id": id, "deleted": true })))
}

// ─────────────────────────────────────────────────────────────────────────────── deals

/// The deal types the DATABASE will accept — mirrored from the live constraint
/// `business_loyalty_deals_deal_type_check`. Validating here turns what was a 500 ("Internal server
/// error", caused by passing an invented type straight through to Postgres) into a 400 that names the
/// options. A constraint the API does not know about is a constraint the UI cannot offer.
const DEAL_TYPES: [&str; 5] = [
    "discount_percent",
    "fixed_amount",
    "free_item",
    "bogo",
    "bonus_points",
];

fn deal_type_of(v: &str) -> Result<String, AppError> {
    if DEAL_TYPES.contains(&v) {
        Ok(v.to_string())
    } else {
        Err(AppError::BadRequest(format!(
            "deal_type must be one of {}",
            DEAL_TYPES.join(", ")
        )))
    }
}

#[derive(Deserialize)]
pub struct DealInput {
    pub business_name: String,
    pub program_id: Uuid,
    pub deal_type: String,
    pub deal_value: Option<String>,
    pub deal_description: Option<String>,
    pub min_purchase: Option<String>,
    pub points_required: Option<i32>,
    pub is_active: Option<bool>,
    pub valid_from: Option<String>,
    pub valid_until: Option<String>,
    pub redemptions_limit: Option<i32>,
}

/// May `program_id` be used by this account — at CREATE/EDIT time?
///
/// This is a validity check, NOT an ownership one. It answers "may I attach a deal to this programme",
/// never "whose deal is this" — the answer to the second is `business_loyalty_deals.owner_account_id`.
/// Conflating the two is precisely what let every account edit every deal on a shared programme.
///
/// Deliberately mirrors the app's own rule in `handlers/loyalty.rs` (`list_programs`):
///
///     WHERE c.account_id = $1 OR lp.campaign_id IS NULL
///
/// A program with NO campaign is the SHARED platform program and is usable by every account — my first
/// version joined strictly and so refused a deal on exactly that program, which would have made deals
/// unusable for everyone. Matching the existing convention is the point: two different notions of
/// "whose program is this" is how a feature works from one screen and 403s from another.
async fn program_is_usable(
    state: &AppState,
    account: Uuid,
    program_id: Uuid,
) -> Result<bool, AppError> {
    let found: Option<Uuid> = sqlx::query_scalar(
        r#"SELECT p.id
             FROM loyalty_programs p
             LEFT JOIN campaigns c ON c.id = p.campaign_id
            WHERE p.id = $1 AND (c.account_id = $2 OR p.campaign_id IS NULL)"#,
    )
    .bind(program_id)
    .bind(account)
    .fetch_optional(&state.db)
    .await?;
    Ok(found.is_some())
}

/// GET /api/v1/deals — deals on this account's loyalty programs.
pub async fn list_deals(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let account = account_of(&user)?;
    let rows: Vec<(
        Uuid,
        Option<String>,
        Uuid,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<i32>,
        Option<bool>,
        Option<chrono::DateTime<chrono::Utc>>,
        Option<i32>,
        Option<i32>,
    )> = sqlx::query_as(
        r#"SELECT d.id, d.business_name, d.program_id, d.deal_type, d.deal_value,
                  d.deal_description, d.min_purchase, d.points_required, d.is_active,
                  d.valid_until, d.redemptions_limit, d.current_redemptions
             FROM business_loyalty_deals d
            -- ownership is the deal's OWN column. Scoping through the programme instead let every
            -- account see (and edit) every deal on a shared programme.
            WHERE d.owner_account_id = $1
            ORDER BY d.is_active DESC NULLS LAST, d.valid_until ASC NULLS LAST
            LIMIT 500"#,
    )
    .bind(account)
    .fetch_all(&state.db)
    .await?;

    let items: Vec<Value> = rows
        .into_iter()
        .map(
            |(
                id,
                name,
                program,
                kind,
                value,
                desc,
                min_purchase,
                points,
                active,
                until,
                limit,
                used,
            )| {
                json!({
                    "id": id, "business_name": name, "program_id": program, "deal_type": kind,
                    "deal_value": value, "deal_description": desc, "min_purchase": min_purchase,
                    "points_required": points, "is_active": active, "valid_until": until,
                    "redemptions_limit": limit, "current_redemptions": used,
                })
            },
        )
        .collect();
    Ok(Json(json!({ "deals": items, "count": items.len() })))
}

/// POST /api/v1/deals
pub async fn create_deal(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<DealInput>,
) -> Result<Json<Value>, AppError> {
    let account = account_of(&user)?;
    if body.business_name.trim().is_empty() {
        return Err(AppError::BadRequest(
            "A deal needs the business it is with — otherwise nobody knows who to redeem it at"
                .to_string(),
        ));
    }
    // Refusing a program that is not ours is what stops one account attaching deals to another's
    // loyalty program (which the join in list_deals would then never show them — a silent orphan).
    if !program_is_usable(&state, account, body.program_id).await? {
        return Err(AppError::NotFound(
            "No such loyalty program available to this account".to_string(),
        ));
    }
    let deal_type = deal_type_of(&body.deal_type)?;
    let id = Uuid::new_v4();
    sqlx::query(
        r#"INSERT INTO business_loyalty_deals
              (id, owner_account_id, business_id, business_name, program_id, deal_type, deal_value,
               deal_description, min_purchase, points_required, is_active, valid_from, valid_until,
               redemptions_limit, current_redemptions, created_at, updated_at)
           VALUES ($1, $2, $2, $3, $4, $5, $6, $7, $8, $9, COALESCE($10, true),
                   $11::timestamptz, $12::timestamptz, $13, 0, now(), now())"#,
    )
    .bind(id)
    .bind(account) // business_id: the account that owns the deal
    .bind(&body.business_name)
    .bind(body.program_id)
    .bind(&deal_type)
    // deal_value and the two counters are NOT NULL with no default on this table, so an omitted value
    // must arrive as '' / 0 rather than NULL — otherwise the insert violates NOT NULL and the caller
    // sees a 500 for leaving an optional field blank.
    .bind(body.deal_value.clone().unwrap_or_default())
    .bind(&body.deal_description)
    .bind(&body.min_purchase)
    .bind(body.points_required.unwrap_or(0))
    .bind(body.is_active)
    .bind(&body.valid_from)
    .bind(&body.valid_until)
    .bind(body.redemptions_limit.unwrap_or(0))
    .execute(&state.db)
    .await?;
    Ok(Json(json!({ "id": id, "created": true })))
}

/// PUT /api/v1/deals/:id
pub async fn update_deal(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(id): Path<Uuid>,
    Json(body): Json<DealInput>,
) -> Result<Json<Value>, AppError> {
    let account = account_of(&user)?;
    if !program_is_usable(&state, account, body.program_id).await? {
        return Err(AppError::NotFound(
            "No such loyalty program available to this account".to_string(),
        ));
    }
    let deal_type = deal_type_of(&body.deal_type)?;
    // The WHERE joins the same ownership chain, so a deal on someone else's program cannot be edited
    // even if its uuid is known.
    let res = sqlx::query(
        r#"UPDATE business_loyalty_deals d
              SET business_name = $3, deal_type = $4, deal_value = $5, deal_description = $6,
                  min_purchase = $7, points_required = $8, is_active = COALESCE($9, d.is_active),
                  valid_from = $10::timestamptz, valid_until = $11::timestamptz,
                  redemptions_limit = $12, updated_at = now()
            WHERE d.id = $1 AND d.owner_account_id = $2"#,
    )
    .bind(id)
    .bind(account)
    .bind(&body.business_name)
    .bind(&deal_type)
    .bind(body.deal_value.clone().unwrap_or_default())
    .bind(&body.deal_description)
    .bind(&body.min_purchase)
    .bind(body.points_required.unwrap_or(0))
    .bind(body.is_active)
    .bind(&body.valid_from)
    .bind(&body.valid_until)
    .bind(body.redemptions_limit.unwrap_or(0))
    .execute(&state.db)
    .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::NotFound("No such deal".to_string()));
    }
    Ok(Json(json!({ "id": id, "updated": true })))
}

/// DELETE /api/v1/deals/:id
pub async fn delete_deal(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, AppError> {
    let account = account_of(&user)?;
    let res = sqlx::query(
        r#"DELETE FROM business_loyalty_deals d
            WHERE d.id = $1 AND d.owner_account_id = $2"#,
    )
    .bind(id)
    .bind(account)
    .execute(&state.db)
    .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::NotFound("No such deal".to_string()));
    }
    Ok(Json(json!({ "id": id, "deleted": true })))
}
