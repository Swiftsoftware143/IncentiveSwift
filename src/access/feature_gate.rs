//! Feature gating — checks if an account's plan tier has access to a feature.

use crate::{error::AppError, state::AppState};
use uuid::Uuid;

/// Check if a feature is enabled for a given account's plan tier.
/// Returns true if the tier-feature mapping exists and is enabled.
/// Returns false if the mapping doesn't exist (feature not assigned to that tier).
pub async fn has_feature_access(
    state: &AppState,
    account_id: &str,
    feature_key: &str,
) -> Result<bool, AppError> {
    // Parse account_id string to UUID before binding to UUID column
    let account_uuid = Uuid::parse_str(account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID format".to_string()))?;

    // Get the account's plan tier
    let plan_tier_id: Option<Uuid> =
        sqlx::query_scalar("SELECT plan_tier_id FROM accounts WHERE id = $1")
            .bind(account_uuid)
            .fetch_optional(&state.db)
            .await?
            .flatten();

    let Some(tier_id) = plan_tier_id else {
        return Ok(false);
    };

    // Get the feature ID
    let feature_id: Option<Uuid> = sqlx::query_scalar("SELECT id FROM features WHERE key = $1")
        .bind(feature_key)
        .fetch_optional(&state.db)
        .await?
        .flatten();

    let Some(feature_id) = feature_id else {
        return Ok(false);
    };

    // Check if the tier has this feature enabled
    let enabled: Option<bool> = sqlx::query_scalar(
        "SELECT enabled FROM tier_features WHERE tier_id = $1 AND feature_id = $2",
    )
    .bind(tier_id)
    .bind(feature_id)
    .fetch_optional(&state.db)
    .await?
    .flatten();

    Ok(enabled.unwrap_or(false))
}

/// Enforce that an account's plan tier can play a given mechanic.
/// Returns `AppError::UpgradeRequired` (402) when the account's tier does not
/// include the `mechanic_<type>` feature (or the `all_mechanics` catch-all).
pub async fn enforce_mechanic_feature(
    state: &AppState,
    account_id: &str,
    mechanic_type: &str,
) -> Result<(), AppError> {
    let feature_key = format!("mechanic_{}", mechanic_type);
    let mut allowed = has_feature_access(state, account_id, &feature_key).await?;
    if !allowed {
        allowed = has_feature_access(state, account_id, "all_mechanics").await?;
    }
    if !allowed {
        return Err(AppError::UpgradeRequired(format!(
            "The '{}' mechanic is not available on your current plan. Upgrade to unlock it.",
            mechanic_type
        )));
    }
    Ok(())
}
