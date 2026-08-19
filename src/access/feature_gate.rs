//! Feature gating — checks if an account's plan tier has access to a feature.
//!
//! Single source of truth: `tier_features` (joined through `features` by key),
//! keyed by the account's `plan_tier_id` (which references `plan_tiers`).
//! The legacy `plans.features` JSONB column is NOT consulted here.

use crate::{error::AppError, state::AppState};
use uuid::Uuid;

/// Resolve the account's plan tier id, or `None` if the account has no tier.
async fn account_tier_id(state: &AppState, account_id: &str) -> Result<Option<Uuid>, AppError> {
    let account_uuid = Uuid::parse_str(account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID format".to_string()))?;

    let plan_tier_id: Option<Uuid> =
        sqlx::query_scalar("SELECT plan_tier_id FROM accounts WHERE id = $1")
            .bind(account_uuid)
            .fetch_optional(&state.db)
            .await?
            .flatten();

    Ok(plan_tier_id)
}

/// Tri-state feature state for a tier:
/// `Some(true)`  = explicitly enabled,
/// `Some(false)` = explicitly disabled,
/// `None`        = no row (feature never assigned to this tier).
async fn tier_feature_state(
    state: &AppState,
    tier_id: Uuid,
    feature_key: &str,
) -> Result<Option<bool>, AppError> {
    let feature_id: Option<Uuid> = sqlx::query_scalar("SELECT id FROM features WHERE key = $1")
        .bind(feature_key)
        .fetch_optional(&state.db)
        .await?
        .flatten();

    let Some(feature_id) = feature_id else {
        return Ok(None);
    };

    let enabled: Option<bool> = sqlx::query_scalar(
        "SELECT enabled FROM tier_features WHERE tier_id = $1 AND feature_id = $2",
    )
    .bind(tier_id)
    .bind(feature_id)
    .fetch_optional(&state.db)
    .await?
    .flatten();

    Ok(enabled)
}

/// Check if a feature is explicitly enabled for a given account's plan tier.
/// Returns true only when a `tier_features` row exists AND `enabled = true`.
/// Missing row = false (feature not assigned to that tier).
pub async fn has_feature_access(
    state: &AppState,
    account_id: &str,
    feature_key: &str,
) -> Result<bool, AppError> {
    let Some(tier_id) = account_tier_id(state, account_id).await? else {
        return Ok(false);
    };
    Ok(tier_feature_state(state, tier_id, feature_key)
        .await?
        .unwrap_or(false))
}

/// Check if a tier may use a mechanic, honoring the `all_mechanics` catch-all
/// with explicit-disable override:
/// - `mechanic_<type>` row `enabled = true`   -> allow
/// - `mechanic_<type>` row `enabled = false`  -> deny (explicit disable overrides catch-all)
/// - no `mechanic_<type>` row                 -> allow iff `all_mechanics` is enabled
pub async fn has_mechanic_access(
    state: &AppState,
    account_id: &str,
    mechanic_type: &str,
) -> Result<bool, AppError> {
    let Some(tier_id) = account_tier_id(state, account_id).await? else {
        return Ok(false);
    };
    let key = format!("mechanic_{}", mechanic_type);
    match tier_feature_state(state, tier_id, &key).await? {
        Some(enabled) => Ok(enabled),
        None => Ok(tier_feature_state(state, tier_id, "all_mechanics")
            .await?
            .unwrap_or(false)),
    }
}

/// Enforce that an account's plan tier can play a given mechanic.
/// Returns `AppError::UpgradeRequired` (402) when the account's tier does not
/// include the `mechanic_<type>` feature (or the `all_mechanics` catch-all).
pub async fn enforce_mechanic_feature(
    state: &AppState,
    account_id: &str,
    mechanic_type: &str,
) -> Result<(), AppError> {
    if has_mechanic_access(state, account_id, mechanic_type).await? {
        Ok(())
    } else {
        Err(AppError::UpgradeRequired(format!(
            "The '{}' mechanic is not available on your current plan. Upgrade to unlock it.",
            mechanic_type
        )))
    }
}
