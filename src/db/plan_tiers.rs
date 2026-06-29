//! Plan tiers database operations.

use crate::error::AppError;
use sqlx::{PgPool, Row};
use uuid::Uuid;

/// A plan tier record.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PlanTier {
    pub id: uuid::Uuid,
    pub name: String,
    pub slug: String,
    pub price_monthly: Option<rust_decimal::Decimal>,
    pub price_annual: Option<rust_decimal::Decimal>,
    pub is_active: bool,
    pub sort_order: i32,
    pub max_campaigns: Option<i32>,
    pub max_entries_per_month: Option<i32>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Get a plan tier by ID.
pub async fn get_plan_tier(
    pool: &PgPool,
    tier_id: &Uuid,
) -> Result<PlanTier, AppError> {
    let row = sqlx::query(
        r#"SELECT id, name, slug, price_monthly, price_annual, is_active,
                  sort_order, max_campaigns, max_entries_per_month, created_at
           FROM plan_tiers WHERE id = $1"#
    )
    .bind(tier_id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::NotFound("Plan tier not found".to_string()))?;

    Ok(PlanTier {
        id: row.get("id"),
        name: row.get("name"),
        slug: row.get("slug"),
        price_monthly: row.get("price_monthly"),
        price_annual: row.get("price_annual"),
        is_active: row.get("is_active"),
        sort_order: row.get("sort_order"),
        max_campaigns: row.get("max_campaigns"),
        max_entries_per_month: row.get("max_entries_per_month"),
        created_at: row.get("created_at"),
    })
}

/// Check if a feature is enabled for a given plan tier.
pub async fn feature_enabled(
    pool: &PgPool,
    tier_id: &Uuid,
    feature_key: &str,
) -> Result<bool, AppError> {
    let enabled: Option<bool> = sqlx::query_scalar(
        r#"SELECT tf.enabled
           FROM tier_features tf
           JOIN features f ON f.id = tf.feature_id
           WHERE tf.tier_id = $1 AND f.key = $2"#
    )
    .bind(tier_id)
    .bind(feature_key)
    .fetch_optional(pool)
    .await?
    .flatten();

    Ok(enabled.unwrap_or(false))
}
