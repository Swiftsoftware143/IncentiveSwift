//! Feature limit enforcement — checks numeric usage limits for IncentiveSwift accounts.
//!
//! Flow:
//!   1. Get account's plan_tier_id from accounts table
//!   2. Join plan_tiers to get the slug (e.g. "free", "pro")
//!   3. Look up limit_value from feature_limits table by plan_tier slug + feature_key
//!   4. If limit_value == -1, unlimited
//!   5. Count current usage, compare against limit
#![allow(dead_code)]

use crate::error::AppError;
use sqlx::PgPool;

#[derive(Debug, Clone)]
pub struct FeatureLimitResult {
    pub allowed: bool,
    pub usage: i64,
    pub limit: i64,
}

/// Check a numeric feature limit for an account.
pub async fn check_feature_limit(
    db: &PgPool,
    account_id: &str,
    feature_key: &str,
) -> Result<FeatureLimitResult, AppError> {
    // Get the account's plan tier slug
    let plan_tier_slug: Option<String> = sqlx::query_scalar(
        "SELECT pt.slug FROM accounts a
         JOIN plan_tiers pt ON pt.id = a.plan_tier_id
         WHERE a.id = $1",
    )
    .bind(account_id)
    .fetch_optional(db)
    .await?
    .flatten();

    let slug = match plan_tier_slug {
        Some(s) => s,
        None => {
            return Ok(FeatureLimitResult {
                allowed: false,
                usage: 0,
                limit: 0,
            })
        }
    };

    // Look up the limit for this feature on this plan tier
    let limit_value: Option<i64> = sqlx::query_scalar(
        "SELECT fl.limit_value FROM feature_limits fl
         JOIN plan_tier_features ptf ON ptf.feature_id = fl.feature_id
         WHERE ptf.slug = $1 AND fl.feature_key = $2",
    )
    .bind(&slug)
    .bind(feature_key)
    .fetch_optional(db)
    .await?
    .flatten();

    let limit = limit_value.unwrap_or(0);

    // -1 means unlimited
    if limit == -1 {
        return Ok(FeatureLimitResult {
            allowed: true,
            usage: 0,
            limit: -1,
        });
    }

    let usage = count_usage(db, account_id, feature_key).await?;

    Ok(FeatureLimitResult {
        allowed: usage < limit,
        usage,
        limit,
    })
}

/// Enforce a feature limit — returns an error if limit exceeded.
pub async fn enforce_feature_limit(
    db: &PgPool,
    account_id: &str,
    feature_key: &str,
) -> Result<(), AppError> {
    let result = check_feature_limit(db, account_id, feature_key).await?;
    if result.allowed {
        Ok(())
    } else {
        Err(AppError::BadRequest(
            format!(
                "Feature limit reached for '{}': {}/{}",
                feature_key, result.usage, result.limit
            ),
        ))
    }
}

/// Count current usage for a feature.
async fn count_usage(
    db: &PgPool,
    account_id: &str,
    feature_key: &str,
) -> Result<i64, AppError> {
    // Dynamically count by feature
    match feature_key {
        "max_campaigns" => {
            let count: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM campaigns WHERE account_id = $1 AND deleted_at IS NULL",
            )
            .bind(account_id)
            .fetch_one(db)
            .await?;
            Ok(count)
        }
        "max_entries" => {
            let count: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM entries WHERE campaign_id IN (SELECT id FROM campaigns WHERE account_id = $1)",
            )
            .bind(account_id)
            .fetch_one(db)
            .await?;
            Ok(count)
        }
        "max_members" => {
            let count: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM loyalty_members WHERE program_id IN (SELECT id FROM loyalty_programs WHERE account_id = $1)",
            )
            .bind(account_id)
            .fetch_one(db)
            .await?;
            Ok(count)
        }
        _ => {
            // Generic fallback: try to count from a dynamically named table
            let count: i64 = sqlx::query_scalar(
                &format!(
                    "SELECT COUNT(*) FROM {} WHERE account_id = $1 AND deleted_at IS NULL",
                    feature_key.trim_start_matches("max_")
                ),
            )
            .bind(account_id)
            .fetch_one(db)
            .await?;
            Ok(count)
        }
    }
}
