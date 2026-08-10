//! Feature limit enforcement — reads limits from plans table columns.
//! Falls back to plan_tier_features/feature_limits tables if populated.

use crate::error::AppError;
use sqlx::PgPool;

pub async fn enforce_feature_limit(
    db: &PgPool,
    account_id: &str,
    feature_key: &str,
    label: &str,
) -> Result<(), AppError> {
    // Get plan tier slug from account
    let plan_slug: Option<String> = sqlx::query_scalar(
        "SELECT pt.slug FROM accounts a
         JOIN plans pt ON pt.id = a.plan_tier_id
         WHERE a.id = $1"
    )
    .bind(account_id)
    .fetch_optional(db)
    .await?
    .flatten();

    let slug = match plan_slug {
        Some(s) => s,
        None => return Ok(()), // No plan — allow
    };

    // First check feature_limits table (custom overrides)
    let fl_val: Option<i64> = sqlx::query_scalar(
        "SELECT fl.limit_value FROM feature_limits fl
         JOIN plan_tier_features ptf ON ptf.feature_id = fl.feature_id
         WHERE ptf.slug = $1 AND fl.feature_key = $2"
    )
    .bind(&slug)
    .bind(feature_key)
    .fetch_optional(db)
    .await?
    .flatten();

    if let Some(val) = fl_val {
        if val == -1 { return Ok(()); }
        if val == 0 {
            return Err(AppError::UpgradeRequired(
                format!("{} is not available on your current plan. Upgrade to access this feature.", label)
            ));
        }
        let usage = count_usage(db, account_id, feature_key).await?;
        if usage >= val {
            return Err(AppError::UpgradeRequired(
                format!("{} limit reached ({}/{}). Upgrade to increase your limit.", label, usage, val)
            ));
        }
        return Ok(());
    }

    // Fall back to plans table column
    let plan_col = match feature_key {
        "max_campaigns" | "campaigns" => "max_campaigns",
        "max_leads" | "leads" => "max_leads",
        "max_tags" | "tags" => "max_tags",
        "max_members" | "members" => "max_members",
        "max_entries" | "entries" => "max_entries",
        "max_forms" | "forms" => "max_forms",
        _ => return Ok(()), // Unknown feature — allow
    };

    let limit_val: Option<i64> = sqlx::query_scalar(
        &format!("SELECT {} FROM plans WHERE slug = $1", plan_col)
    )
    .bind(&slug)
    .fetch_optional(db)
    .await?
    .flatten();

    match limit_val {
        None | Some(-1) => Ok(()),
        Some(0) => Err(AppError::UpgradeRequired(
            format!("{} is not available on your current plan. Upgrade to access this feature.", label)
        )),
        Some(limit) => {
            let usage = count_usage(db, account_id, feature_key).await?;
            if usage >= limit {
                Err(AppError::UpgradeRequired(
                    format!("{} limit reached ({}/{}). Upgrade to increase your limit.", label, usage, limit)
                ))
            } else {
                Ok(())
            }
        }
    }
}

pub async fn get_usage_json(db: &PgPool, account_id: &str) -> serde_json::Value {
    let campaigns = count_usage(db, account_id, "max_campaigns").await.unwrap_or(0);
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
                "SELECT COUNT(*) FROM campaigns WHERE account_id = $1 AND deleted_at IS NULL"
            ).bind(account_id).fetch_one(db).await?;
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
            let count: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM leads WHERE account_id = $1"
            ).bind(account_id).fetch_one(db).await?;
            Ok(count)
        }
        "max_tags" | "tags" => {
            let count: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM tags WHERE account_id = $1"
            ).bind(account_id).fetch_one(db).await?;
            Ok(count)
        }
        _ => Ok(0)
    }
}
