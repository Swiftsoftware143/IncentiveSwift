//! Plan resolution for the private-inbox module (IncentiveSwift's half of CoreSwift's
//! `module_registry` seam).
//!
//! The inbox was ported from CoreSwift, where a `module_registry` table answers "does this tenant's
//! plan include private email, and what are its numeric limits". IncentiveSwift has no module
//! registry — it sells through `plan_tiers` (the tier an account is SEATED on, the row
//! `accounts.plan_tier_id` has its foreign key to) and `tier_features`/`features` (one row per
//! feature a tier carries, with an optional numeric allowance). This module answers the SAME
//! questions from the data this app already sells:
//!
//! * `private_email` module: granted when the account is seated on an ACTIVE `plan_tiers` row. An
//!   account with no tier is `no_plan` and is refused exactly as CoreSwift refuses it (an inbox is
//!   a sold feature; "no plan" is not a plan).
//! * numeric limits (`email_domains`, `email_mailboxes`, `max_workspaces`,
//!   `max_email_storage_mb`): read `tier_features.limit_value` for that feature key. A tier that
//!   states no number is UNLIMITED — the ceiling exists only where it was sold, and inventing a
//!   default here would refuse a customer for a limit nobody sold them. `-1` is this app's own
//!   spelling of "no ceiling" on the enterprise tier, so it is normalised to `None`.
//! * an operator's per-account override in `tenant_email_limits` is the most specific answer and
//!   always wins, for the two keys that table carries.
//!
//! Deliberately small: a handful of call sites in `private_email` and `workspaces`, one query
//! each, no cache to go stale when an operator moves an account between tiers.

use serde_json::{Map, Value};
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::AppError;

/// What one feature resolves to for a tenant.
#[derive(Debug, Clone)]
pub struct Resolved {
    /// May this tenant use the feature at all?
    pub enabled: bool,
    /// Where the answer came from: `plan`, `override`, or `no_plan`. Callers treat `no_plan` as
    /// "refused" even when `enabled` is tolerant, which is what keeps a plan-less account from
    /// receiving unlimited domains.
    pub source: &'static str,
    /// The numeric ceiling, when the feature is a limit. `None` = no ceiling stated.
    pub limit_value: Option<f64>,
}

impl Resolved {
    fn refused(source: &'static str) -> Self {
        Self {
            enabled: false,
            source,
            limit_value: None,
        }
    }
}

/// The tier an account is seated on, when that tier is on offer. `None` = no plan.
async fn active_tier(pool: &PgPool, tenant_id: Uuid) -> Result<Option<Uuid>, AppError> {
    sqlx::query_scalar(
        "SELECT a.plan_tier_id
           FROM accounts a
           JOIN plan_tiers pt ON pt.id = a.plan_tier_id
          WHERE a.id = $1 AND pt.is_active",
    )
    .bind(tenant_id)
    .fetch_optional(pool)
    .await
    .map_err(crate::error::db_error)
    .map(Option::flatten)
}

/// One feature's allowance on one tier. `Ok(None)` = the tier states no number (unlimited) or does
/// not carry the feature.
pub async fn tier_limit(
    pool: &PgPool,
    tier_id: Uuid,
    feature_key: &str,
) -> Result<Option<i32>, AppError> {
    let row: Option<(bool, Option<i32>)> = sqlx::query_as(
        "SELECT tf.enabled, tf.limit_value
           FROM tier_features tf
           JOIN features f ON f.id = tf.feature_id
          WHERE tf.tier_id = $1 AND f.key = $2",
    )
    .bind(tier_id)
    .bind(feature_key)
    .fetch_optional(pool)
    .await
    .map_err(crate::error::db_error)?;

    Ok(match row {
        // A disabled feature carries no allowance at all.
        Some((false, _)) => Some(0),
        // Unstated = unlimited (`-1` is this app's spelling of the same thing).
        Some((true, None)) => None,
        Some((true, Some(n))) if n < 0 => None,
        Some((true, Some(n))) => Some(n),
        None => None,
    })
}

/// A number the tier states for one feature, or `None` when it states nothing.
pub async fn limit(
    pool: &PgPool,
    tenant_id: Uuid,
    feature_key: &str,
) -> Result<Option<i32>, AppError> {
    // An operator's own per-account override is the most specific answer and always wins.
    if let Some((domains, boxes)) = overrides(pool, tenant_id).await? {
        match feature_key {
            "email_domains" => {
                if let Some(n) = domains {
                    return Ok(if n < 0 { None } else { Some(n) });
                }
            }
            "email_mailboxes" => {
                if let Some(n) = boxes {
                    return Ok(if n < 0 { None } else { Some(n) });
                }
            }
            _ => {}
        }
    }
    let Some(tier) = active_tier(pool, tenant_id).await? else {
        return Ok(None);
    };
    tier_limit(pool, tier, feature_key).await
}

/// The operator overrides for one account, when a row exists.
async fn overrides(
    pool: &PgPool,
    tenant_id: Uuid,
) -> Result<Option<(Option<i32>, Option<i32>)>, AppError> {
    sqlx::query_as(
        "SELECT max_domains, max_mailboxes
           FROM tenant_email_limits WHERE tenant_id = $1",
    )
    .bind(tenant_id)
    .fetch_optional(pool)
    .await
    .map_err(crate::error::db_error)
}

/// Every feature this account's tier carries, as the JSON shape the ported gate already reads.
///
/// `None` = the account holds no active tier (the gate shows the upgrade sentence instead of a
/// dead button). A Boolean `true` means "granted, no number sold"; a number is the allowance; a
/// feature the tier does not carry is simply absent (unstated).
pub async fn plan_feature_map(pool: &PgPool, tenant_id: Uuid) -> Result<Option<Value>, AppError> {
    let Some(tier) = active_tier(pool, tenant_id).await? else {
        return Ok(None);
    };
    Ok(Some(tier_feature_map(pool, tier).await?))
}

/// The same map for one explicit tier (used by the catalogue readers in `feature_gate`).
pub async fn tier_feature_map(pool: &PgPool, tier_id: Uuid) -> Result<Value, AppError> {
    let rows: Vec<(String, bool, Option<i32>)> = sqlx::query_as(
        "SELECT f.key, tf.enabled, tf.limit_value
           FROM tier_features tf
           JOIN features f ON f.id = tf.feature_id
          WHERE tf.tier_id = $1",
    )
    .bind(tier_id)
    .fetch_all(pool)
    .await
    .map_err(crate::error::db_error)?;
    Ok(feature_map(&rows))
}

fn feature_map(rows: &[(String, bool, Option<i32>)]) -> Value {
    let mut map = Map::new();
    for (key, enabled, limit) in rows {
        if !*enabled {
            map.insert(key.clone(), Value::Bool(false));
            continue;
        }
        match limit {
            Some(n) if *n >= 0 => map.insert(key.clone(), Value::from(*n)),
            // Unstated, or this app's `-1` spelling of "no ceiling", is UNLIMITED and therefore
            // absent from the blob (the gate treats a missing key as no stated number).
            _ => map.insert(key.clone(), Value::Bool(true)),
        };
    }
    Value::Object(map)
}

/// Resolve one feature for one tenant — the ported module's `resolve(pool, tenant, key)`.
pub async fn resolve(pool: &PgPool, tenant_id: Uuid, key: &str) -> Result<Resolved, AppError> {
    // An operator's own per-account override is the most specific answer and always wins.
    if let Some((domains, boxes)) = overrides(pool, tenant_id).await? {
        let override_limit = match key {
            "email_domains" => domains,
            "email_mailboxes" => boxes,
            _ => None,
        };
        if let Some(limit) = override_limit {
            return Ok(Resolved {
                enabled: limit != 0,
                source: "override",
                limit_value: if limit < 0 { None } else { Some(limit as f64) },
            });
        }
    }

    let Some(tier) = active_tier(pool, tenant_id).await? else {
        return Ok(Resolved::refused("no_plan"));
    };

    match key {
        "private_email" => Ok(Resolved {
            enabled: true,
            source: "plan",
            limit_value: None,
        }),
        other => {
            let stated = tier_limit(pool, tier, other).await?;
            Ok(Resolved {
                enabled: stated != Some(0),
                source: "plan",
                limit_value: stated.filter(|n| *n > 0).map(|n| n as f64),
            })
        }
    }
}
