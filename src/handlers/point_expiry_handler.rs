//! Point expiry handler — idempotent RECOMPUTE of unexpired balances.
//!
//! Called by cron: `POST /api/v1/admin/treasury/expire-points`
//!
//! ## Why this is a recompute and not a decrement
//!
//! The previous implementation swept `loyalty_scans` (a table nothing writes),
//! subtracted points from `loyalty_members.points_balance`, wrote a column that
//! does not exist (`updated_at`), and ran a WHERE-less `point_treasury` update
//! inside the per-member loop. A crash mid-run therefore double-deducted on the
//! next run, and there was no way to tell what had been taken from whom.
//!
//! Expiry is now expressed as: "what does the ledger say this member may still
//! spend?" and the balance is SET to that answer.
//!
//! ```text
//! unexpired_balance = GREATEST(0, unexpired_earned - redeemed)
//! ```
//!
//! Because the inputs are the immutable ledgers, a second run computes the
//! identical number, observes `current == recomputed`, and writes nothing.
//! Repeat runs are no-ops, which is the only defensible way to ship a
//! destructive sweep pre-launch.
//!
//! ## Ledgers used
//!
//! - **Earn**, all three ledgers that credit `loyalty_members.points_balance`:
//!   `loyalty_checkins.points_awarded` (timestamped `checked_in_at`),
//!   `loyalty_online_actions.points_earned` (timestamped `created_at`, the
//!   social/online-action path written from `auth_handler.rs:218` and
//!   `loyalty.rs:658/796/891`), and `point_issuance_log.points_issued`
//!   (timestamped `created_at`, the badge-award path at `loyalty_badges.rs:659`).
//!
//!   Omitting `loyalty_online_actions` is NOT a smaller version of the same
//!   bug: because the balance is SET to the recomputed value, a ledger missing
//!   from this sum does not merely go un-expired, it gets **deleted**. That is
//!   why the sum must enumerate every balance-crediting ledger.
//!
//!   `loyalty_activity` is deliberately NOT summed: it mirrors the check-in
//!   rows (`record_activity` is called next to every `loyalty_checkins` insert),
//!   so including it would double count every check-in.
//! - **Spend**: `point_redemption_log.points_redeemed`.
//! - Redemptions are assumed to consume the OLDEST points first (FIFO), so the
//!   points a member still holds are their newest ones. That is why the
//!   unexpired earn total is reduced by the full redemption total rather than
//!   allocating redemptions against specific earn rows.
//!
//! ## Cutoff
//!
//! Per member, from that member's OWN program:
//! `loyalty_programs.points_expire_days` (falling back to [`DEFAULT_EXPIRE_DAYS`]
//! when the column is NULL or the program row is missing). The hardcoded 365
//! that ignored the column is gone.
//!
//! ## Safety
//!
//! - Each member's balance write is its own transaction with an optimistic
//!   guard (`WHERE ... AND points_balance = <observed>`), so a concurrent award
//!   is never clobbered — the member is skipped and recorded instead.
//! - The treasury liability is updated ONCE, after aggregation, never in a loop.
//! - Every run writes a `point_expiry_audit` row. It is inserted *before* any
//!   balance is touched and carries the previous balance of every member it
//!   planned to change, so the sweep is inspectable and reversible.
use std::collections::BTreeMap;

use axum::{extract::State, Json};
use serde_json::{json, Value};
use sqlx::Row;

use crate::error::AppError;
use crate::state::AppState;

/// Fallback lifetime for points when a program has no `points_expire_days`.
const DEFAULT_EXPIRE_DAYS: i32 = 365;

/// Points are redeemed at 1 point = $0.01 of outstanding liability, matching
/// the issuance path (`loyalty_badges.rs`: `bill_amount = points_awarded * 0.01`).
const POINTS_TO_DOLLARS_SCALE: u32 = 2;

/// POST /api/v1/admin/treasury/expire-points
///
/// Recomputes every member's unexpired balance from the ledgers, honours each
/// program's `points_expire_days`, and SETs the corrected balance. Idempotent.
pub async fn expire_points(State(s): State<AppState>) -> Result<Json<Value>, AppError> {
    let now = chrono::Utc::now();
    let default_cutoff = now - chrono::Duration::days(DEFAULT_EXPIRE_DAYS as i64);

    // ── 1. Read-only plan: per-member ledger facts, cutoff resolved per program ──
    let rows = sqlx::query(
        "SELECT
             m.id::text                              AS member_id,
             m.program_id::text                      AS program_id,
             COALESCE(m.points_balance, 0)           AS points_balance,
             COALESCE(p.points_expire_days, $1::int4) AS expire_days,
             COALESCE(e.unexpired_earned, 0)::bigint  AS unexpired_earned,
             COALESCE(e.expired_earned, 0)::bigint    AS expired_earned,
             COALESCE(r.redeemed, 0)::bigint          AS redeemed
         FROM loyalty_members m
         LEFT JOIN loyalty_programs p ON p.id = m.program_id
         LEFT JOIN LATERAL (
             SELECT
                 SUM(CASE WHEN t.ts >= (NOW() - make_interval(days => COALESCE(p.points_expire_days, $1::int4)))
                          THEN t.pts ELSE 0 END) AS unexpired_earned,
                 SUM(CASE WHEN t.ts <  (NOW() - make_interval(days => COALESCE(p.points_expire_days, $1::int4)))
                          THEN t.pts ELSE 0 END) AS expired_earned
             FROM (
                 SELECT c.checked_in_at AS ts, c.points_awarded::bigint AS pts
                   FROM loyalty_checkins c
                  WHERE c.member_id = m.id AND c.points_awarded > 0
                 UNION ALL
                 SELECT o.created_at AS ts, o.points_earned::bigint AS pts
                   FROM loyalty_online_actions o
                  WHERE o.member_id = m.id AND o.points_earned > 0
                 UNION ALL
                 SELECT i.created_at AS ts, i.points_issued::bigint AS pts
                   FROM point_issuance_log i
                  WHERE i.member_id = m.id AND i.points_issued > 0
             ) t
         ) e ON TRUE
         LEFT JOIN LATERAL (
             SELECT SUM(rd.points_redeemed) AS redeemed
               FROM point_redemption_log rd
              WHERE rd.member_id = m.id AND rd.points_redeemed > 0
         ) r ON TRUE
         WHERE m.points_balance > 0
         ORDER BY m.id",
    )
    .bind(DEFAULT_EXPIRE_DAYS)
    .fetch_all(&s.db)
    .await?;

    struct Plan {
        member_id: String,
        program_id: Option<String>,
        previous_balance: i32,
        new_balance: i32,
        expired_points: i64,
        expire_days: i32,
        unexpired_earned: i64,
        expired_earned: i64,
        redeemed: i64,
        cutoff: chrono::DateTime<chrono::Utc>,
    }

    let mut plans: Vec<Plan> = Vec::new();
    let mut program_cutoffs: BTreeMap<String, (i32, String)> = BTreeMap::new();

    for row in &rows {
        let member_id: String = row.try_get("member_id")?;
        let program_id: Option<String> = row.try_get("program_id")?;
        let previous_balance: i32 = row.try_get("points_balance")?;
        let expire_days: i32 = row.try_get("expire_days")?;
        let unexpired_earned: i64 = row.try_get("unexpired_earned")?;
        let expired_earned: i64 = row.try_get("expired_earned")?;
        let redeemed: i64 = row.try_get("redeemed")?;

        let key = program_id.clone().unwrap_or_else(|| "none".to_string());
        let member_cutoff = now - chrono::Duration::days(expire_days as i64);
        program_cutoffs
            .entry(key)
            .or_insert_with(|| (expire_days, member_cutoff.to_rfc3339()));

        // The only arithmetic that decides a balance: the ledgers and nothing else.
        let recomputed = std::cmp::max(0i64, unexpired_earned - redeemed);
        let recomputed_i32 = i32::try_from(recomputed).unwrap_or(i32::MAX);
        let expired_points = previous_balance as i64 - recomputed_i32 as i64;

        // expired_points <= 0 means the member is already correct (or holds
        // untracked points): leave them alone. This is what makes run 2 a no-op.
        if expired_points <= 0 {
            continue;
        }

        plans.push(Plan {
            member_id,
            program_id,
            previous_balance,
            new_balance: recomputed_i32,
            expired_points,
            expire_days,
            unexpired_earned,
            expired_earned,
            redeemed,
            cutoff: member_cutoff,
        });
    }

    let planned_total: i64 = plans.iter().map(|p| p.expired_points).sum();
    let liability_reduction = rust_decimal::Decimal::new(planned_total, POINTS_TO_DOLLARS_SCALE);

    // ── 2. Audit row FIRST, holding the previous balance of every member we
    //       intend to change, so the sweep is reversible even if we die midway ──
    let audit_id = uuid::Uuid::new_v4();
    let planned_details: Vec<Value> = plans
        .iter()
        .map(|p| {
            json!({
                "member_id": p.member_id,
                "program_id": p.program_id,
                "expire_days": p.expire_days,
                "cutoff": p.cutoff.to_rfc3339(),
                "previous_balance": p.previous_balance,
                "new_balance": p.new_balance,
                "expired_points": p.expired_points,
                "unexpired_earned": p.unexpired_earned,
                "expired_earned": p.expired_earned,
                "redeemed": p.redeemed,
            })
        })
        .collect();

    sqlx::query(
        "INSERT INTO point_expiry_audit
             (id, status, default_expire_days, members_affected, total_points_expired,
              treasury_liability_reduction, details)
         VALUES ($1, 'pending', $2, $3, $4, $5, $6)",
    )
    .bind(audit_id)
    .bind(DEFAULT_EXPIRE_DAYS)
    .bind(plans.len() as i32)
    .bind(planned_total)
    .bind(liability_reduction)
    .bind(sqlx::types::Json(&planned_details))
    .execute(&s.db)
    .await?;

    // ── 3. Apply: one transaction per member, guarded on the observed balance ──
    let mut applied_details: Vec<Value> = Vec::new();
    let mut total_expired: i64 = 0;
    let mut members_affected: i64 = 0;
    let mut conflicts: i64 = 0;

    for p in &plans {
        let mut tx = s.db.begin().await?;

        let updated = sqlx::query(
            "UPDATE loyalty_members
                SET points_balance = $1
              WHERE id = $2::uuid AND points_balance = $3",
        )
        .bind(p.new_balance)
        .bind(&p.member_id)
        .bind(p.previous_balance)
        .execute(&mut *tx)
        .await?
        .rows_affected();

        if updated == 0 {
            // A concurrent award/redemption moved the balance between the plan
            // and the write. Roll back and let the next run see the new state.
            tx.rollback().await?;
            conflicts += 1;
            applied_details.push(json!({
                "member_id": p.member_id,
                "outcome": "skipped_conflict",
                "observed_balance": p.previous_balance,
            }));
            tracing::warn!(
                member_id = %p.member_id,
                "point expiry: balance changed underneath plan, skipped"
            );
            continue;
        }

        tx.commit().await?;

        total_expired += p.expired_points;
        members_affected += 1;
        applied_details.push(json!({
            "member_id": p.member_id,
            "program_id": p.program_id,
            "outcome": "applied",
            "previous_balance": p.previous_balance,
            "new_balance": p.new_balance,
            "expired_points": p.expired_points,
            "expire_days": p.expire_days,
        }));

        tracing::info!(
            "Expired {} points for member {} ({} -> {})",
            p.expired_points,
            p.member_id,
            p.previous_balance,
            p.new_balance
        );
    }

    let applied_liability = rust_decimal::Decimal::new(total_expired, POINTS_TO_DOLLARS_SCALE);

    // ── 4. Treasury + audit reconciliation, ONCE, after aggregation ──
    if members_affected > 0 {
        let mut tx = s.db.begin().await?;

        sqlx::query(
            "UPDATE point_treasury
                SET outstanding_liability = GREATEST(0, outstanding_liability - $1),
                    updated_at = NOW()",
        )
        .bind(applied_liability)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "UPDATE point_expiry_audit
                SET status = 'applied',
                    members_affected = $1,
                    total_points_expired = $2,
                    treasury_liability_reduction = $3,
                    details = $4,
                    completed_at = NOW()
              WHERE id = $5",
        )
        .bind(members_affected as i32)
        .bind(total_expired)
        .bind(applied_liability)
        .bind(sqlx::types::Json(&applied_details))
        .bind(audit_id)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
    } else {
        sqlx::query(
            "UPDATE point_expiry_audit
                SET status = 'noop',
                    members_affected = 0,
                    total_points_expired = 0,
                    treasury_liability_reduction = 0,
                    details = $1,
                    completed_at = NOW()
              WHERE id = $2",
        )
        .bind(sqlx::types::Json(&applied_details))
        .bind(audit_id)
        .execute(&s.db)
        .await?;
    }

    let program_cutoffs_json: Value = json!(program_cutoffs
        .iter()
        .map(|(program_id, (days, cutoff))| json!({
            "program_id": program_id,
            "expire_days": days,
            "cutoff_date": cutoff,
        }))
        .collect::<Vec<_>>());

    Ok(Json(json!({
        // Keys a UI or cron already reads — unchanged.
        "success": true,
        "total_points_expired": total_expired,
        "members_affected": members_affected,
        "cutoff_date": default_cutoff.to_rfc3339(),
        "message": format!(
            "Expired {} points across {} members (default cutoff: {})",
            total_expired,
            members_affected,
            default_cutoff.format("%Y-%m-%d")
        ),
        // Additive: provenance for the run.
        "audit_id": audit_id.to_string(),
        "default_expire_days": DEFAULT_EXPIRE_DAYS,
        "treasury_liability_reduction": applied_liability.to_string(),
        "skipped_conflicts": conflicts,
        "program_cutoffs": program_cutoffs_json,
    })))
}
