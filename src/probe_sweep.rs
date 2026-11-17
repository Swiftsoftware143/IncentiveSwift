//! `probe_sweep` — retire harness workspaces that nothing can sign into.
//!
//! Every fleet harness mints a REAL workspace through the real signup route and marks it
//! `probe_harness` (see [`crate::probe_harness`]); the harness then retires its identity but not the
//! workspace, so probe rows accumulate for ever. Measured 2026-11-09: this database carried nine
//! harness-tagged accounts, all from one day's probes, and nothing ever retired them. FunnelSwift has
//! had the sweeper since 2026-11-06 and ADASwift/MissedCall were ported the same day this one was;
//! the gap was this app, not the policy.
//!
//! On this app the `accounts` row IS the workspace (its `tenant_id` points at itself and all 39
//! child keys reference `accounts` with ON DELETE CASCADE, read from live 2026-11-09), so the
//! marker and the identity live on the same row and one statement retires the whole workspace.
//!
//! The retention rule is the fleet's own probe-residue policy
//! (`/opt/swift/docs/fleet-probe-residue-policy-2026-09-28.md`, answer 3c): a workspace is retired
//! only when it is ATTRIBUTED as harness machinery — `probe_harness` stamped from a request HEADER
//! (or by a door that recognises the recipient's domain), OR (since 2026-11-17, matching ADASwift,
//! FunnelSwift and WorkflowSwift) an owner address that is fleet machinery by the same predicate the
//! operator's tenant list already hides on — AND it is older than [`RETENTION_DAYS`]. Fourteen days
//! is deliberately generous: a harness that is mid-run, or a teammate demonstrating the product,
//! cannot lose a workspace it is still using.
//!
//! Two address classes are never touched even when tagged, because over-protection is the safe
//! direction: the fleet's own provider identity (`swiftsoftware143@…`, `certifiedtb143@…`), and
//! anything on a fleet-owned REAL domain (`swiftsoftware.com/.net`, `incentiveswift.com`) — a
//! workspace there can be a human's.

use sqlx::PgPool;
use std::time::Duration;

/// How often the sweeper looks. Six hours is cheap and bounds how long a stale workspace lives.
pub const SWEEP_INTERVAL_SECS: u64 = 6 * 60 * 60;

/// Age below which a tagged workspace is never considered. Matches the fleet residue policy.
pub const RETENTION_DAYS: i64 = 14;

/// Rows per sweep, so one transaction never holds a long lock on `accounts`.
pub const SWEEP_BATCH: i64 = 50;

/// Boot-time worker, started from `main` after the pool connects. A failure inside one sweep is
/// logged and the loop continues — a janitor must never take the app down.
pub fn spawn_probe_sweeper(pool: PgPool) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(SWEEP_INTERVAL_SECS));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            // The first tick fires immediately, so a deploy retires the backlog without waiting
            // six hours.
            ticker.tick().await;
            match sweep_once(&pool).await {
                Ok(0) => {}
                Ok(n) => tracing::info!(retired = n, "probe workspace sweep"),
                Err(e) => tracing::warn!(error = %e, "probe workspace sweep failed"),
            }
        }
    });
}

/// Retires at most [`SWEEP_BATCH`] expired harness workspaces and returns how many went.
///
/// TWO ARMS, ONE RULE (measured live 2026-11-17): the marker `probe_harness` is stamped only when a
/// harness sends `X-Swift-Harness` (or when a door recognises the recipient's domain), so a
/// workspace minted before the auto-tag shipped — or by any door that does not stamp — stayed for
/// ever while the operator's own tenant list already hid it
/// (`handlers::admin_handler::owner_is_fleet_machinery`). The two arms now agree, and the predicate
/// is CALLED from the same function the list uses, so the read rule and the retention rule cannot
/// drift. Matches ADASwift, FunnelSwift and WorkflowSwift.
///
/// The protected rows are excluded in SQL rather than in Rust so the predicate the operator reads
/// here is exactly the predicate the database applies.
pub async fn sweep_once(pool: &PgPool) -> Result<usize, sqlx::Error> {
    // Candidates are read first and judged in Rust, because the reserved/harness domain list lives
    // in one place (`security::probe_addr` / `security::email_addr`) and duplicating it as a SQL
    // literal is how a retention rule silently rots. The candidate window is wider than one batch so
    // the untagged arm cannot be starved by a run of tagged rows ahead of it.
    let candidates: Vec<(uuid::Uuid, String, Option<String>, Option<String>)> = sqlx::query_as(
        r#"
        SELECT c.id, c.name, c.probe_harness, c.email
          FROM accounts c
         WHERE c.created_at < now() - make_interval(days => $1::int)
           -- never a portfolio company: the registry the hub's own sync writes is authoritative,
           -- so a company mirrored tomorrow cannot be retired by the janitor.
           AND NOT EXISTS (
                 SELECT 1 FROM portfolio_companies pc
                  WHERE pc.account_id = c.id
                     OR lower(pc.name) = lower(c.name))
           -- never the fleet's own provider identity, and never a fleet-owned REAL domain: those can
           -- be a human's workspace (residue policy, FINDING 2).
           AND NOT (lower(coalesce(c.email, '')) LIKE 'swiftsoftware143%'
                 OR lower(coalesce(c.email, '')) LIKE 'certifiedtb143%'
                 OR lower(coalesce(c.email, '')) LIKE '%@swiftsoftware.com'
                 OR lower(coalesce(c.email, '')) LIKE '%@swiftsoftware.net'
                 OR lower(coalesce(c.email, '')) LIKE '%@incentiveswift.com')
         ORDER BY c.created_at ASC
         LIMIT $2
        "#,
    )
    .bind(RETENTION_DAYS)
    .bind(SWEEP_BATCH * 4)
    .fetch_all(pool)
    .await?;

    let doomed: Vec<uuid::Uuid> = candidates
        .into_iter()
        .filter(|(_, _, harness, owner_email)| {
            harness.is_some()
                || owner_email
                    .as_deref()
                    .map(crate::handlers::admin_handler::owner_is_fleet_machinery)
                    .unwrap_or(false)
        })
        .map(|(id, _, _, _)| id)
        .take(SWEEP_BATCH as usize)
        .collect();
    if doomed.is_empty() {
        return Ok(0);
    }

    let retired: Vec<(uuid::Uuid, String)> = sqlx::query_as(
        r#"DELETE FROM accounts a
            WHERE a.id = ANY($1)
        RETURNING a.id, a.name"#,
    )
    .bind(&doomed)
    .fetch_all(pool)
    .await?;

    for (id, name) in &retired {
        tracing::info!(account = %id, name = %name, "retired expired probe workspace");
    }
    Ok(retired.len())
}
