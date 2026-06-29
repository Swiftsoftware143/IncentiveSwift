//! Feature limit enforcement — checks numeric usage limits for IncentiveSwift accounts.
//!
//! Flow:
//!   1. Get account's plan_tier_id from accounts table
//!   2. Join plan_tiers to get the slug (e.g. "free", "pro")
//!   3. Look up limit_value from feature_limits table by plan_tier slug + feature_key
//!   4. If limit_value == -1, unlimited
//!   5. Count current usage, compare against limit

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
         WHERE a.id = $1"
    )
    .bind(account_id)
    .fetch_optional(db)
    .await?
    .flatten();

    let limit_value = match plan_tier_slug {
        Some(ref slug) => {
            let limit: Option<i32> = sqlx::query_scalar(
                "SELECT limit_value FROM feature_limits WHERE plan_tier = $1 AND feature_key = $2"
            )
            .bind(slug)
            .bind(feature_key)
            .fetch_optional(db)
            .await?
            .flatten();
            limit.unwrap_or(0) as i64
        }
        None => 0_i64,
    };

    // -1 means unlimited
    if limit_value == -1 {
        return Ok(FeatureLimitResult {
            allowed: true,
            usage: 0,
            limit: -1,
        });
    }

    // Count current usage based on feature_key
    let usage = count_usage(db, account_id, feature_key).await?;

    Ok(FeatureLimitResult {
        allowed: usage < limit_value,
        usage,
        limit: limit_value,
    })
}

/// Enforce a feature limit; returns Forbidden error if over the limit.
pub async fn enforce_feature_limit(
    db: &PgPool,
    account_id: &str,
    feature_key: &str,
    label: &str,
) -> Result<(), AppError> {
    let result = check_feature_limit(db, account_id, feature_key).await?;
    if !result.allowed {
        return Err(AppError::Forbidden(format!(
            "{} limit reached ({} / {})",
            label, result.usage, result.limit
        )));
    }
    Ok(())
}

/// Count existing resources for an account based on the feature key.
async fn count_usage(db: &PgPool, account_id: &str, feature_key: &str) -> Result<i64, AppError> {
    let count: i64 = match feature_key {
        "max_campaigns" => {
            sqlx::query_scalar::<_, Option<i64>>(
                "SELECT COUNT(*) FROM campaigns WHERE account_id = $1"
            )
            .bind(account_id)
            .fetch_one(db)
            .await?
            .unwrap_or(0)
        }
        "max_entries_per_month" => {
            // Count entries created in the current month for all campaigns belonging to this account
            sqlx::query_scalar::<_, Option<i64>>(
                "SELECT COUNT(*) FROM entries e
                 JOIN campaigns c ON c.id = e.campaign_id
                 WHERE c.account_id = $1
                 AND e.created_at >= date_trunc('month', NOW())"
            )
            .bind(account_id)
            .fetch_one(db)
            .await?
            .unwrap_or(0)
        }
        "max_leads" => {
            sqlx::query_scalar::<_, Option<i64>>(
                "SELECT COUNT(*) FROM contacts"
            )
            .fetch_one(db)
            .await?
            .unwrap_or(0)
        }
        "max_api_keys" => {
            sqlx::query_scalar::<_, Option<i64>>(
                "SELECT COUNT(*) FROM api_keys WHERE tenant_id = $1"
            )
            .bind(account_id)
            .fetch_one(db)
            .await?
            .unwrap_or(0)
        }
        "max_entries" => {
            sqlx::query_scalar::<_, Option<i64>>(
                "SELECT COUNT(*) FROM entries e
                 JOIN campaigns c ON c.id = e.campaign_id
                 WHERE c.account_id = $1"
            )
            .bind(account_id)
            .fetch_one(db)
            .await?
            .unwrap_or(0)
        }
        "max_integrations" => {
            sqlx::query_scalar::<_, Option<i64>>(
                "SELECT COUNT(*) FROM integration_targets WHERE account_id = $1"
            )
            .bind(account_id)
            .fetch_one(db)
            .await?
            .unwrap_or(0)
        }
        "max_portfolios" => {
            sqlx::query_scalar::<_, Option<i64>>(
                "SELECT COUNT(*) FROM portfolio_companies WHERE account_id = $1"
            )
            .bind(account_id)
            .fetch_one(db)
            .await?
            .unwrap_or(0)
        }
        _ => 0,
    };
    Ok(count)
}
