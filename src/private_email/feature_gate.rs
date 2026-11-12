//! Plan ceilings for the private inbox (IncentiveSwift half of CoreSwift's `feature_gate`).
//!
//! The gate answers two different questions and they must not be confused:
//!
//!   1. **Does this workspace's plan include private email at all?** (`get_plan_features` ->
//!      `None` means no: the screen shows the upgrade sentence instead of a dead button.)
//!   2. **Is there room for one more domain / mailbox?** (`check_domain_limit`,
//!      `check_mailbox_limit` -> `UpgradeRequired`, whose body is the sentence the screen prints.)
//!
//! WHERE THE NUMBERS COME FROM. IncentiveSwift sells its limits as `plan_tiers` ->
//! `tier_features` -> `features`; an operator's own per-account override lives in
//! `tenant_email_limits` and wins.
//! A plan that states no `max_domains` / `max_mailboxes` is UNLIMITED — a ceiling exists only where
//! one was sold, and inventing a default here would refuse a customer for a limit nobody sold them.
//! That is the ONE deliberate difference from CoreSwift, which reads a module registry this app does
//! not have (see `crate::module_registry`).
//!
//! CATCH-ALL is a sold feature: a workspace may switch it on only while its plan carries
//! `catch_all` in the blob, and every refusal sentence is built from the PLAN CATALOGUE (name and
//! price read live), never from a hardcoded tier name that drifts the moment a plan is repriced.

use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use super::models::{PrivateEmailPlanFeatures, TenantEmailLimits};
use crate::error::AppError;

/// Sentinel for "no numeric cap configured". Every check compares `count >= max`, so a very large
/// number behaves exactly like SQL's NULL.
const UNLIMITED: i32 = i32::MAX;

/// The two spellings `catch_all` is stored under.
const CATCH_ALL_KEYS: &[&str] = &["catch_all", "catch_all_enabled"];

/// Fetch any tenant-specific email limit overrides (None when the operator set none).
pub async fn get_tenant_limits(
    pool: &PgPool,
    tenant_id: Uuid,
) -> Result<Option<TenantEmailLimits>, AppError> {
    sqlx::query_as::<_, TenantEmailLimits>("SELECT * FROM tenant_email_limits WHERE tenant_id = $1")
        .bind(tenant_id)
        .fetch_optional(pool)
        .await
        .map_err(crate::error::db_error)
}

/// Read a boolean out of a plan/override JSON blob under ANY of `keys`; the first name PRESENT
/// wins. Both shapes are accepted: a bare boolean (`{"catch_all": true}`, what `plans.features`
/// carries) and the object form (`{"catch_all": {"enabled": true}}`), because reading only one
/// shape answered `false` for every plan and refused every tenant.
fn json_bool(features: &Value, keys: &[&str]) -> Option<bool> {
    for key in keys {
        match features.get(*key) {
            Some(Value::Bool(b)) => return Some(*b),
            Some(Value::Object(o)) => {
                if let Some(Value::Bool(b)) = o.get("enabled") {
                    return Some(*b);
                }
            }
            Some(Value::String(s)) => match s.trim() {
                "true" | "1" | "yes" => return Some(true),
                "false" | "0" | "no" => return Some(false),
                _ => {}
            },
            _ => {}
        }
    }
    None
}

/// A number spelled in the blob, tolerant of numbers-as-strings.
fn json_i32(features: &Value, key: &str) -> Option<i32> {
    match features.get(key) {
        Some(Value::Number(n)) => n.as_i64().map(|v| v.clamp(0, UNLIMITED as i64) as i32),
        Some(Value::String(s)) => s.trim().parse::<i32>().ok(),
        _ => None,
    }
}

/// The tenant's active tier's feature map, if it holds one.
///
/// IncentiveSwift sells allowances as `plan_tiers` -> `tier_features` -> `features`, so this is
/// [`crate::module_registry::plan_feature_map`]: one map per account, built from the tier the
/// `accounts.plan_tier_id` foreign key names. `None` = the account holds no active tier.
async fn plan_blob(pool: &PgPool, tenant_id: Uuid) -> Result<Option<Value>, AppError> {
    crate::module_registry::plan_feature_map(pool, tenant_id).await
}

/// The plan's own answer for private email, `None` when the workspace has no active plan.
pub async fn get_plan_features(
    pool: &PgPool,
    tenant_id: Uuid,
) -> Result<Option<PrivateEmailPlanFeatures>, AppError> {
    let module = crate::module_registry::resolve(pool, tenant_id, "private_email").await?;
    if !module.enabled || module.source == "no_plan" {
        return Ok(None);
    }

    let features = plan_blob(pool, tenant_id).await?.unwrap_or(Value::Null);

    // `None` (unstated) is UNLIMITED, `Some(0)` is an explicit "not on this plan".
    let max_domains = json_i32(&features, "max_domains").unwrap_or(UNLIMITED);
    let max_mailboxes = json_i32(&features, "max_mailboxes").unwrap_or(UNLIMITED);

    Ok(Some(PrivateEmailPlanFeatures {
        private_email: true,
        max_domains,
        max_mailboxes,
        max_aliases_per_mailbox: json_i32(&features, "max_aliases_per_mailbox").unwrap_or(0),
        catch_all_enabled: json_bool(&features, CATCH_ALL_KEYS).unwrap_or(false),
    }))
}

/// `79` for a whole-dollar price, `29.50` when a tier is not — never `79.000000`.
fn price_label(price: f64) -> String {
    if (price - price.round()).abs() < 0.005 {
        format!("{:.0}", price)
    } else {
        format!("{:.2}", price)
    }
}

/// Every active tier with its feature map, cheapest first (name, monthly price, map).
///
/// Read from this app's own catalogue — `plan_tiers` joined through `tier_features`/`features` —
/// so a refusal names a tier that is really on offer and a price the customer would really pay.
async fn plans_cheapest_first(pool: &PgPool) -> Result<Vec<(String, f64, Value)>, AppError> {
    let tiers: Vec<(Uuid, String, f64)> = sqlx::query_as(
        r#"
        SELECT id, name, COALESCE(price_monthly, 0)::float8
          FROM plan_tiers
         WHERE is_active
         ORDER BY COALESCE(price_monthly, 0) ASC, name ASC
        "#,
    )
    .fetch_all(pool)
    .await
    .map_err(crate::error::db_error)?;

    let mut out = Vec::with_capacity(tiers.len());
    for (id, name, price) in tiers {
        let blob = crate::module_registry::tier_feature_map(pool, id).await?;
        out.push((name, price, blob));
    }
    Ok(out)
}

/// The CHEAPEST active plan that actually sells catch-all — name and monthly price, read from the
/// catalogue so the refusal cannot name a tier that is not on offer.
pub async fn catch_all_unlock_plan(pool: &PgPool) -> Result<Option<(String, f64)>, AppError> {
    Ok(plans_cheapest_first(pool)
        .await?
        .iter()
        .find(|(_, _, features)| json_bool(features, CATCH_ALL_KEYS) == Some(true))
        .map(|(name, price, _)| (name.clone(), *price)))
}

/// The door to catch-all, as an English clause: `the Professional plan ($79/month)`.
pub async fn catch_all_upgrade_clause(pool: &PgPool) -> String {
    match catch_all_unlock_plan(pool).await {
        Ok(Some((name, price))) => format!("the {} plan (${}/month)", name, price_label(price)),
        Ok(None) => "a plan that includes it — contact support and we will enable it".to_string(),
        Err(_) => "a higher plan — open Upgrade to see what each plan includes".to_string(),
    }
}

/// The 400 a workspace sees when its plan does not sell catch-all.
pub async fn catch_all_refusal(pool: &PgPool) -> String {
    format!(
        "Catch-all requires {} or higher.",
        catch_all_upgrade_clause(pool).await
    )
}

/// The cheapest active plan with MORE mailboxes than `current`, as (name, price, allowance).
async fn next_mailbox_plan(pool: &PgPool, current: i32) -> Option<(String, f64, i32)> {
    plans_cheapest_first(pool)
        .await
        .ok()?
        .iter()
        .filter_map(|(name, price, features)| {
            json_i32(features, "max_mailboxes").map(|limit| (name.clone(), *price, limit))
        })
        .find(|(_, _, limit)| *limit > current)
}

/// Check if the tenant has room for another domain.
pub async fn check_domain_limit(pool: &PgPool, tenant_id: Uuid) -> Result<(), AppError> {
    let features = get_plan_features(pool, tenant_id).await?.ok_or_else(|| {
        AppError::UpgradeRequired(
            "Private email is not available on your current plan. Upgrade to add your own \
             domain and mailboxes."
                .into(),
        )
    })?;

    let max_domains = match get_tenant_limits(pool, tenant_id).await? {
        Some(limits) => limits.max_domains.unwrap_or(features.max_domains),
        None => features.max_domains,
    };

    if max_domains == 0 {
        return Err(AppError::UpgradeRequired(
            "Domain provisioning is not available on your plan. Upgrade to add a domain.".into(),
        ));
    }

    let count: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM private_email_domains WHERE tenant_id = $1")
            .bind(tenant_id)
            .fetch_one(pool)
            .await
            .map_err(crate::error::db_error)?;

    if max_domains > 0 && count.0 >= max_domains as i64 {
        return Err(AppError::UpgradeRequired(format!(
            "Domain limit reached ({}/{}). Upgrade to add more domains.",
            count.0, max_domains
        )));
    }

    Ok(())
}

/// Check if the tenant has room for another mailbox. The refusal names the door AND its price:
/// "Upgrade to create more mailboxes" tells a customer nothing about what to buy.
pub async fn check_mailbox_limit(pool: &PgPool, tenant_id: Uuid) -> Result<(), AppError> {
    let features = get_plan_features(pool, tenant_id).await?.ok_or_else(|| {
        AppError::UpgradeRequired(
            "Private email is not available on your current plan. Upgrade to add your own \
             domain and mailboxes."
                .into(),
        )
    })?;

    let max_mailboxes = match get_tenant_limits(pool, tenant_id).await? {
        Some(limits) => limits.max_mailboxes.unwrap_or(features.max_mailboxes),
        None => features.max_mailboxes,
    };

    if max_mailboxes == 0 {
        let unlock = match next_mailbox_plan(pool, 0).await {
            Some((name, price, limit)) => format!(
                "{} includes up to {} inboxes for ${:.0}/month.",
                name, limit, price
            ),
            None => "Add an inbox plan to your account to use it.".to_string(),
        };
        return Err(AppError::UpgradeRequired(unlock));
    }

    let count: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM private_email_boxes WHERE tenant_id = $1")
            .bind(tenant_id)
            .fetch_one(pool)
            .await
            .map_err(crate::error::db_error)?;

    if max_mailboxes > 0 && count.0 >= max_mailboxes as i64 {
        let unlock = match next_mailbox_plan(pool, max_mailboxes).await {
            Some((name, price, limit)) => format!(
                " {} includes up to {} inboxes for ${:.0}/month.",
                name, limit, price
            ),
            None => String::new(),
        };
        return Err(AppError::UpgradeRequired(format!(
            "You have used all {} inboxes on your plan.{unlock}",
            max_mailboxes
        )));
    }

    Ok(())
}

/// The plan's own answer for catch-all: `None` when the plan does not include private email.
async fn plan_catch_all(pool: &PgPool, tenant_id: Uuid) -> Result<Option<bool>, AppError> {
    let module = crate::module_registry::resolve(pool, tenant_id, "private_email").await?;
    if !module.enabled || module.source == "no_plan" {
        return Ok(None);
    }
    let features = plan_blob(pool, tenant_id).await?.unwrap_or(Value::Null);
    Ok(Some(json_bool(&features, CATCH_ALL_KEYS).unwrap_or(false)))
}

/// May this workspace switch catch-all ON right now?
pub async fn can_enable_catch_all(pool: &PgPool, tenant_id: Uuid) -> Result<bool, AppError> {
    Ok(plan_catch_all(pool, tenant_id).await?.unwrap_or(false))
}

/// Is catch-all granted to this workspace's plan? (The inbound router's question.)
pub async fn catch_all_granted(pool: &PgPool, tenant_id: Uuid) -> Result<bool, AppError> {
    Ok(plan_catch_all(pool, tenant_id).await?.unwrap_or(false))
}

/// The inbound router's decision, named as its own type so the seam is testable.
#[derive(Debug, PartialEq, Eq)]
pub enum CatchAllRoute {
    Route,
    Refuse,
}

/// THE one rule: a stored catch-all preference is in force only while the plan still sells it.
pub fn catch_all_route(stored: bool, plan_sells: bool) -> CatchAllRoute {
    if stored && plan_sells {
        CatchAllRoute::Route
    } else {
        CatchAllRoute::Refuse
    }
}

/// The same rule as a boolean, for the reporting surfaces.
pub fn catch_all_effective(stored: bool, plan_sells: bool) -> bool {
    catch_all_route(stored, plan_sells) == CatchAllRoute::Route
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catch_all_needs_both_halves() {
        assert!(catch_all_effective(true, true));
        assert!(!catch_all_effective(true, false));
        assert!(!catch_all_effective(false, true));
    }

    #[test]
    fn json_bool_accepts_both_spellings_and_shapes() {
        let bare = serde_json::json!({"catch_all": true});
        assert_eq!(json_bool(&bare, CATCH_ALL_KEYS), Some(true));
        let object = serde_json::json!({"catch_all": {"enabled": true}});
        assert_eq!(json_bool(&object, CATCH_ALL_KEYS), Some(true));
        let other_spelling = serde_json::json!({"catch_all_enabled": true});
        assert_eq!(json_bool(&other_spelling, CATCH_ALL_KEYS), Some(true));
        assert_eq!(json_bool(&serde_json::json!({}), CATCH_ALL_KEYS), None);
    }

    #[test]
    fn price_label_is_whole_or_two_decimals() {
        assert_eq!(price_label(79.0), "79");
        assert_eq!(price_label(29.5), "29.50");
    }
}
