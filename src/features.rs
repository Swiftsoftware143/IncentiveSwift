//! Feature limit enforcement — single source of truth: `tier_features.limit_value`
//! (per-feature numeric limits) with `plan_tiers.max_campaigns` /
//! `max_entries_per_month` as the tier base columns.
//!
//! RECONCILIATION NOTE (2026-08-18): the previous `plan_tier_features` +
//! `feature_limits` table fallback was REMOVED — those tables do not exist in
//! the live schema, and the runtime gate reads `tier_features` only. There is
//! now exactly one feature model (see `access::feature_gate`).

use crate::error::AppError;
use sqlx::PgPool;
use uuid::Uuid;

pub async fn enforce_feature_limit(
    db: &PgPool,
    account_id: &str,
    feature_key: &str,
    label: &str,
) -> Result<(), AppError> {
    // Resolve the account's plan tier.
    let tier_id: Option<Uuid> =
        sqlx::query_scalar("SELECT plan_tier_id FROM accounts WHERE id = $1")
            .bind(account_id)
            .fetch_optional(db)
            .await?
            .flatten();

    let Some(tier_id) = tier_id else {
        return Ok(()); // No plan — allow
    };

    // Canonical per-feature numeric limit from tier_features.
    // `limit_value` is INT4, so it decodes as `i32`: asking sqlx for an `Option<i64>` here is a
    // `mismatched types; Rust type Option<i64> (as SQL type INT8) is not compatible with SQL type
    // INT4` error the moment a tier actually carries a numeric limit (measured on
    // `GET /api/v1/admin/plans/:id/domains`, kanban t_e2cecfcb — it reads the same column).
    // No caller invokes this function today, which is why the drift had never surfaced.
    let tf_val: Option<i32> = sqlx::query_scalar(
        "SELECT tf.limit_value FROM tier_features tf
         JOIN features f ON f.id = tf.feature_id
         WHERE tf.tier_id = $1 AND f.key = $2",
    )
    .bind(tier_id)
    .bind(feature_key)
    .fetch_optional(db)
    .await?
    .flatten();

    if let Some(val) = tf_val {
        return check_limit(db, account_id, feature_key, label, val as i64).await;
    }

    // Tier base columns for the two built-in numeric limits (INT4 as well).
    let base_val: Option<i32> = match feature_key {
        "max_campaigns" | "campaigns" => {
            sqlx::query_scalar("SELECT max_campaigns FROM plan_tiers WHERE id = $1")
                .bind(tier_id)
                .fetch_optional(db)
                .await?
                .flatten()
        }
        "max_entries_per_month" | "max_entries" | "entries" => {
            sqlx::query_scalar("SELECT max_entries_per_month FROM plan_tiers WHERE id = $1")
                .bind(tier_id)
                .fetch_optional(db)
                .await?
                .flatten()
        }
        _ => return Ok(()), // Unknown feature — no numeric limit, allow
    };

    match base_val {
        None | Some(-1) => Ok(()),
        Some(v) => check_limit(db, account_id, feature_key, label, v as i64).await,
    }
}

async fn check_limit(
    db: &PgPool,
    account_id: &str,
    feature_key: &str,
    label: &str,
    val: i64,
) -> Result<(), AppError> {
    if val == -1 {
        return Ok(()); // unlimited
    }
    if val == 0 {
        return Err(AppError::UpgradeRequired(format!(
            "{} is not available on your current plan. Upgrade to access this feature.",
            label
        )));
    }
    let usage = count_usage(db, account_id, feature_key).await?;
    if usage >= val {
        return Err(AppError::UpgradeRequired(format!(
            "{} limit reached ({}/{}). Upgrade to increase your limit.",
            label, usage, val
        )));
    }
    Ok(())
}

/// The `industry_limit` entitlement for one account — how many industry dashboards
/// (industry = dashboard = template category) the account may hold at once.
///
/// Canonical read: `tier_features.limit_value` for the feature key `industry_limit` on the
/// account's OWN plan tier — the same table `enforce_feature_limit` reads and the same table the
/// admin UI writes (`/api/v1/admin/plans/:id/features`). It deliberately does NOT read
/// `plans.features`: `plans` is the marketing and checkout table, its `features` column is a jsonb
/// ARRAY, and `accounts.plan_tier_id` has an FK to `plan_tiers(id)`, so a plan-shaped read can only
/// ever match by id COINCIDENCE (that was the third and last `plans`-shaped statement in
/// `src/handlers/auth_handler.rs`, kanban t_0961f382 — the cap resolved to 1 for all 56 accounts).
///
/// Returns the configured `limit_value` when the tier has an ENABLED row for the key that carries a
/// numeric limit, otherwise `INDUSTRY_LIMIT_DEFAULT`. That default is deliberately RESTRICTIVE and
/// is NOT the `enforce_feature_limit` "no row = not configured = allow" convention: this
/// entitlement has always meant "one dashboard on the base plan" (migration 00017 intended 1 for
/// the free plan too), and loosening it silently would hand every account unlimited industries.
/// A tier that grants the feature but leaves `limit_value` NULL also gets the default (the key is a
/// numeric cap, so "enabled, no number" is not a meaningful grant).
///
/// `limit_value` is INT4, so it decodes as `i32` — asking sqlx for `i64` is a runtime
/// "mismatched types ... SQL type INT4" error the moment a tier actually carries a limit.
///
/// -1 => unlimited, 0 => not available on this plan, N > 0 => cap N (see `check_limit`).
pub const INDUSTRY_LIMIT_DEFAULT: i64 = 1;

pub async fn industry_limit(db: &PgPool, account_id: Uuid) -> Result<i64, AppError> {
    let configured: Option<i32> = sqlx::query_scalar(
        "SELECT tf.limit_value
           FROM accounts a
           JOIN tier_features tf ON tf.tier_id = a.plan_tier_id AND tf.enabled
           JOIN features f ON f.id = tf.feature_id
          WHERE a.id = $1 AND f.key = 'industry_limit'",
    )
    .bind(account_id)
    .fetch_optional(db)
    .await?
    .flatten();

    Ok(configured.map(i64::from).unwrap_or(INDUSTRY_LIMIT_DEFAULT))
}

pub async fn get_usage_json(db: &PgPool, account_id: &str) -> serde_json::Value {
    let campaigns = count_usage(db, account_id, "max_campaigns")
        .await
        .unwrap_or(0);
    let leads = count_usage(db, account_id, "max_leads").await.unwrap_or(0);
    let tags = count_usage(db, account_id, "max_tags").await.unwrap_or(0);
    serde_json::json!({
        "campaigns": campaigns,
        "leads": leads,
        "tags": tags
    })
}

async fn count_usage(db: &PgPool, account_id: &str, feature_key: &str) -> Result<i64, AppError> {
    match feature_key {
        "max_campaigns" | "campaigns" => {
            let count: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM campaigns WHERE account_id = $1 AND deleted_at IS NULL",
            )
            .bind(account_id)
            .fetch_one(db)
            .await?;
            Ok(count)
        }
        "max_entries" | "entries" => {
            let count: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM entries WHERE campaign_id IN (SELECT id FROM campaigns WHERE account_id = $1)"
            ).bind(account_id).fetch_one(db).await?;
            Ok(count)
        }
        "max_members" | "members" => {
            let count: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM loyalty_members WHERE program_id IN (SELECT id FROM loyalty_programs WHERE account_id = $1)"
            ).bind(account_id).fetch_one(db).await?;
            Ok(count)
        }
        "max_leads" | "leads" => {
            let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM leads WHERE account_id = $1")
                .bind(account_id)
                .fetch_one(db)
                .await?;
            Ok(count)
        }
        "max_tags" | "tags" => {
            let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tags WHERE account_id = $1")
                .bind(account_id)
                .fetch_one(db)
                .await?;
            Ok(count)
        }
        _ => Ok(0),
    }
}
