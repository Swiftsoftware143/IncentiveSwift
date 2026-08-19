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
    let tf_val: Option<i64> = sqlx::query_scalar(
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
        return check_limit(db, account_id, feature_key, label, val).await;
    }

    // Tier base columns for the two built-in numeric limits.
    let base_val: Option<i64> = match feature_key {
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
        Some(v) => check_limit(db, account_id, feature_key, label, v).await,
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
