//! Plan management handlers — CRUD for plans table and admin plan assignment.

use crate::error::AppError;
use crate::security::auth::AuthenticatedUser;
use crate::state::AppState;
use axum::{
    extract::{Path, State},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

// ── Retired (kanban t_141162e7): the plan -> affiliate-product sync ──
//
// `POST /api/v1/internal/sync-affiliate-plan` was deleted from FunnelSwift:
//   * its ONE WRITER contract requires the plan to exist in FunnelSwift's own `plans` table, which
//     a sibling app's plan_id (a uuid from another database) can never satisfy, and
//   * migration 070 forbids an affiliate product for a paid plan, so the only thing a sibling could
//     legitimately push is its own free row — which FunnelSwift already owns and seeds.
// The commissionable catalogue is FunnelSwift-owned. `notify_funnelswift_upgrade`
// (`POST /api/v1/internal/affiliate/upgrade-event`, x-internal-key) is the live cross-app call and
// is untouched.
/// A plan record.
///
/// TWO price columns, ONE number. `plans.price` is what the customer catalogue serves
/// (`dashboard_handler::list_public_plans` -> `monthly_price`); `plans.price_monthly` is what this
/// admin CRUD round-trips. Live rows carried `price` = 0/49/199 with `price_monthly` = 0 on every
/// row, so the operator's Plans panel showed $0 for two plans the pricing page sold at $49/$199,
/// and an edit was written to the column the page ignores — measured 2026-10-17, kanban fix in this
/// commit. Every read here now returns `COALESCE(NULLIF(price_monthly, 0), price) AS price_monthly`
/// and every write sets BOTH columns, so the panel and the page can no longer disagree.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Plan {
    pub id: Uuid,
    pub name: String,
    pub slug: String,
    pub description: Option<String>,
    pub price_monthly: f64,
    pub price_yearly: f64,
    pub features: Value,
    pub is_active: bool,
    pub sort_order: i32,
    pub payment_provider: Option<String>,
    /// The hosted payment page the OPERATOR pasted into this plan (2026-10-24).
    ///
    /// `plans.purchase_url` has existed since the plans table shipped and NO code path in this app
    /// could write or read it, so on an install whose `payment_providers` is EMPTY (this one) every
    /// press on Upgrade could only be refused even when the operator already had a working way to be
    /// paid. `billing::checkout` reads it at the till; this struct is what the operator's console
    /// round-trips, so the panel can show that a plan carries a link and clear it again.
    pub purchase_url: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// Input for creating a plan.
#[derive(Deserialize)]
pub struct CreatePlanInput {
    pub name: String,
    pub slug: Option<String>,
    pub description: Option<String>,
    pub price_monthly: Option<f64>,
    pub price_yearly: Option<f64>,
    pub features: Option<Value>,
    pub is_active: Option<bool>,
    pub sort_order: Option<i32>,
    pub payment_provider: Option<String>,
    pub purchase_url: Option<String>,
}

/// Input for updating a plan.
#[derive(Deserialize)]
pub struct UpdatePlanInput {
    pub name: Option<String>,
    pub slug: Option<String>,
    pub description: Option<String>,
    pub price_monthly: Option<f64>,
    pub price_yearly: Option<f64>,
    pub features: Option<Value>,
    pub is_active: Option<bool>,
    pub sort_order: Option<i32>,
    pub payment_provider: Option<String>,
    /// The plan's own hosted payment page. THREE cases, and they must stay distinguishable:
    /// absent = leave whatever is stored; `null` or `""` = CLEAR the link (that is how the operator
    /// takes a plan off sale-by-link); anything else = set it. A plain `Option<String>` collapses
    /// "absent" and "null" into one value, which is why this is a double option — without it a
    /// pasted link could never be removed again.
    #[serde(default, deserialize_with = "deserialize_some")]
    pub purchase_url: Option<Option<String>>,
}

/// Present-but-may-be-null: distinguishes an ABSENT json field (-> `None`) from an explicit `null`
/// (-> `Some(None)`). Used for `UpdatePlanInput::purchase_url`, whose clear arm is the whole point.
fn deserialize_some<'de, T, D>(deserializer: D) -> Result<Option<T>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Deserialize::deserialize(deserializer).map(Some)
}

/// Input for admin plan assignment.
#[derive(Deserialize)]
pub struct AssignPlanInput {
    pub plan_id: String,
    pub account_id: String,
}

/// Input for updating plan features JSONB only.
#[derive(Deserialize)]
pub struct UpdateFeaturesInput {
    pub features: Value,
}

/// Generate a URL-safe slug.
fn generate_slug(name: &str) -> String {
    let slug: String = name
        .to_lowercase()
        .chars()
        .map(|c| match c {
            'a'..='z' | '0'..='9' | '-' => c,
            ' ' | '_' => '-',
            _ => '-',
        })
        .collect();

    let slug: String = slug
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");

    if slug.is_empty() {
        Uuid::new_v4().to_string()
    } else {
        slug
    }
}

/// GET /api/v1/admin/plans
///
/// Ordered as a PRICE LADDER, cheapest-first (kanban t_1fcaacb2): the operator's Plans panel
/// renders this list in the order returned, and `ORDER BY sort_order, name` put ENTERPRISE ($199)
/// FIRST and FREE ($0) second — the free tier rendered BELOW a paid tier, and the ladder read
/// 199 / 0 / 49. `sort_order` is 0 on every live `plans` row, so the name tie-break decided the
/// order; it stays first so an operator can still pin one, but the tie now breaks on the plan's
/// EFFECTIVE price — `COALESCE(NULLIF(price_monthly, 0), price)`, the same expression
/// `get_plan`/`update_plan` use — because `plans.price_monthly` is 0 on every row while `price`
/// carries the real 0/49/199. Same rule as the public catalogue (dashboard_handler::list_public_plans)
/// and the fleet convention (ADASwift `ORDER BY price ASC`).
pub async fn list_plans(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let plans = sqlx::query_as::<_, Plan>(
        r#"-- The OPERATOR catalogue, so it must include RETIRED rows: `DELETE /admin/plans/:id` is a soft
           -- retire (`is_active = false`), and this list is the only surface an operator has for the
           -- catalogue. Filtering it to `is_active = true` meant a plan retired by one click vanished
           -- from the panel with NO way back — and `admin_assign_plan` now answers 409 telling the
           -- operator to re-activate it on this very list (measured live 2026-10-22: GET /admin/plans
           -- returned 3 rows, all active, while the 4th sat retired and invisible).
           -- The CUSTOMER-facing read is a different handler (`dashboard_handler::list_public_plans`)
           -- and still filters `is_active = true`; the panel's assign picker excludes retired rows
           -- client-side, so nothing on sale changes. Active rows stay first, so the ladder an
           -- operator reads is the one customers see.
           SELECT id, name, slug, description,
                  COALESCE(NULLIF(price_monthly, 0), price) AS price_monthly,
                  price_yearly,
                  features, is_active, sort_order, payment_provider, purchase_url, created_at, updated_at
           FROM plans
           ORDER BY is_active DESC,
                    sort_order ASC NULLS LAST,
                    COALESCE(NULLIF(price_monthly, 0), price) ASC NULLS LAST, name ASC"#,
    )
    .fetch_all(&state.db)
    .await?;

    Ok(Json(json!({ "plans": plans })))
}

/// A plan's `purchase_url` is a hosted payment page the OPERATOR types in, so refuse a typo at
/// SAVE time instead of at the till.
///
/// The till already drops an unusable link (`billing::checkout::is_hosted_payment_url`) — but the
/// operator only found out when a customer pressed Buy: measured live 2026-10-25,
/// `POST /api/v1/admin/plans` stored `buy.stripe.com/not-a-url` and answered 201, after which every
/// press on Upgrade fell through to the refusal arm on a plan the operator believed was sellable.
/// A blank box stays allowed (a free plan, or one sold another way, carries no link).
fn require_hosted_payment_url(raw: &str) -> Result<(), AppError> {
    let u = raw.trim();
    if u.is_empty() || crate::billing::checkout::is_hosted_payment_url(u) {
        return Ok(());
    }
    Err(AppError::BadRequest(
        "The payment link must be a full https:// address, for example https://buy.stripe.com/abc123. \
         It is where a buyer is sent when they pick this plan, so a link without https:// cannot take \
         a payment. Leave the box empty if this plan is not sold by link."
            .to_string(),
    ))
}

/// POST /api/v1/admin/plans
pub async fn create_plan(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<CreatePlanInput>,
) -> Result<Json<Value>, AppError> {
    let slug = body.slug.unwrap_or_else(|| generate_slug(&body.name));
    let id = Uuid::new_v4();
    let features = body.features.unwrap_or_else(|| json!({}));
    // ── A PLAN CAN CARRY ITS OWN PAYMENT PAGE (2026-10-24) ──────────────────────────────────
    // `plans.purchase_url` has existed since the plans table shipped and NO code path in this app
    // could write or read it, so on an install whose `payment_providers` is EMPTY every press on
    // Upgrade could only be refused. An operator who already has a working way to be paid — a
    // Stripe Payment Link, a PayPal button, any hosted page — can now paste it here and the till
    // hands the buyer straight to it. `billing::checkout::is_hosted_payment_url` judges a typo at
    // the till, so a bad value cannot send a buyer somewhere broken. Blank means "no link".
    let purchase_url = body
        .purchase_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(url) = purchase_url {
        require_hosted_payment_url(url)?;
    }

    sqlx::query(
        r#"INSERT INTO plans (id, name, slug, description, price_monthly, price, price_yearly, features, is_active, sort_order, payment_provider, purchase_url)
           VALUES ($1, $2, $3, $4, $5, $5, $6, $7, $8, $9, $10, $11)"#
    )
    .bind(id)
    .bind(&body.name)
    .bind(&slug)
    .bind(&body.description)
    .bind(body.price_monthly.unwrap_or(0.0))
    .bind(body.price_yearly.unwrap_or(0.0))
    .bind(&features)
    .bind(body.is_active.unwrap_or(true))
    .bind(body.sort_order.unwrap_or(0))
    .bind(&body.payment_provider)
    .bind(purchase_url)
    .execute(&state.db)
    .await?;

    let plan = sqlx::query_as::<_, Plan>(
        r#"SELECT id, name, slug, description,
                  COALESCE(NULLIF(price_monthly, 0), price) AS price_monthly,
                  price_yearly,
                  features, is_active, sort_order, payment_provider, purchase_url, created_at, updated_at
           FROM plans WHERE id = $1"#,
    )
    .bind(id)
    .fetch_one(&state.db)
    .await?;

    Ok(Json(json!({ "plan": plan })))
}

/// Fire-and-forget notification to FunnelSwift that an account upgraded to a paid plan,
/// so the referring affiliate is credited (permanent, no expiry).
async fn notify_funnelswift_upgrade(
    config: &crate::config::AppConfig,
    email: &str,
    plan_name: &str,
    plan_price: f64,
    event_id: &str,
) {
    if email.is_empty() || config.funnelswift_url.is_empty() {
        return;
    }
    let url = format!(
        "{}/api/v1/internal/affiliate/upgrade-event",
        config.funnelswift_url.trim_end_matches('/')
    );
    let key = config.internal_sync_key.clone();
    let payload = json!({
        "source_app": "incentiveswift",
        "email": email,
        "plan_name": plan_name,
        "plan_price": plan_price,
        "event_id": event_id,
    });
    tokio::spawn(async move {
        let _ = reqwest::Client::new()
            .post(&url)
            .header("x-internal-key", key)
            .json(&payload)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await;
    });
}

/// Resolve the account's email + plan, and notify FunnelSwift if it's a PAID upgrade.
/// Free plan is the initial attribution (handled at tag time), not an upgrade.
pub(crate) async fn attribute_plan_upgrade(state: &AppState, account_id: Uuid, plan_id: Uuid) {
    // THE PRICE LIVES IN `plans.price`, NOT `plans.price_monthly` (measured live 2026-10-12): every
    // row of `plans` carries `price_monthly = 0` / `price_yearly = 0` while `price` holds the number
    // the PUBLIC listing serves (`GET /api/v1/plans` reads `price::int as monthly_price`: Free 0,
    // Pro 49, Enterprise 199 — `plan_tiers.price_monthly` agrees). Reading `price_monthly` alone
    // therefore made the `<= 0.0` guard below fire for EVERY plan, so a paid upgrade was never
    // reported to FunnelSwift and the referring affiliate was never credited — the whole
    // cross-app attribution was dead. `COALESCE(NULLIF(price_monthly, 0), price)` keeps
    // `price_monthly` authoritative the moment an operator fills it in, and falls back to the price
    // the app actually sells at until then.
    let plan: Option<(String, Option<f64>)> = sqlx::query_as(
        "SELECT name, COALESCE(NULLIF(price_monthly, 0), price)::float8 FROM plans WHERE id = $1",
    )
    .bind(plan_id)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();
    let Some((plan_name, price)) = plan else {
        return;
    };
    let plan_price = price.unwrap_or(0.0);
    if plan_price <= 0.0 {
        return;
    }
    let email: Option<String> = sqlx::query_scalar("SELECT email FROM accounts WHERE id = $1")
        .bind(account_id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();
    let Some(email) = email else {
        return;
    };
    notify_funnelswift_upgrade(
        &state.config,
        &email,
        &plan_name,
        plan_price,
        &Uuid::new_v4().to_string(),
    )
    .await;
}

/// POST /api/v1/admin/plans/assign
pub async fn admin_assign_plan(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<AssignPlanInput>,
) -> Result<Json<Value>, AppError> {
    let plan_id = Uuid::parse_str(&body.plan_id)
        .map_err(|_| AppError::BadRequest("Invalid plan_id".to_string()))?;
    let account_id = Uuid::parse_str(&body.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account_id".to_string()))?;

    // The plan must exist AND still be on sale. MEASURED LIVE 2026-10-22: this action answered 200
    // for a `plans` row with is_active = false and moved `accounts.plan_tier_id` onto its tier —
    // while the customer's own Plan & Pricing screen reads `FROM plans p WHERE p.is_active = true`
    // (`dashboard_handler::list_public_plans`). The workspace was therefore seated on a tier its
    // catalogue cannot show and its own picker cannot re-offer, and nothing on that screen explained
    // the limits it was getting. Same defect class missedcallrespondr fixed in 2edd595 and ADASwift
    // in 2026-10-21; the operator's Plans table has the re-activate control, so seating a retired
    // tier is never the intended action — refuse it by name instead of writing it.
    // The slug is carried out of this query because the SAME call settles the workspace's open
    // purchase request below, and `checkout_sessions.metadata->>'plan_slug'` is keyed on it.
    let plan: Option<(String, bool, String)> =
        sqlx::query_as("SELECT name, is_active, slug FROM plans WHERE id = $1")
            .bind(plan_id)
            .fetch_optional(&state.db)
            .await?;
    let (plan_name, plan_active, plan_slug) =
        plan.ok_or_else(|| AppError::NotFound("Plan not found".to_string()))?;
    if !plan_active {
        return Err(AppError::Conflict(format!(
            "'{plan_name}' is retired, so it cannot be assigned: the customer's plan screen would \
             not list the plan they are on. Re-activate it on the Plans table first, then assign it."
        )));
    }

    // Verify account exists
    let _account = sqlx::query_scalar::<_, Uuid>("SELECT id FROM accounts WHERE id = $1")
        .bind(account_id)
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Account not found".to_string()))?;

    // The admin picks a row in `plans` — the checkout/marketing table. The account's TIER is
    // `plan_tiers`: `accounts.plan_tier_id` carries the FK
    // `accounts_plan_tier_id_fkey FOREIGN KEY (plan_tier_id) REFERENCES plan_tiers(id)`. So the
    // plan row must be mapped to its canonical TIER row through the shared slug, never by
    // re-reading `plans.id`. Measured live 2026-09-25: 2 of the 3 `plans` ids are NOT
    // `plan_tiers` ids (`pro=a43b9f9e…`, `enterprise=f0bbc994…`; the `pro` TIER is
    // `c3a2d26b…`), so binding a `plans` id here was an FK violation on the UPDATE — HTTP 500
    // for every plan except `free`, whose two rows coincide on `8b8cc0e5…`, and the account's
    // tier silently never changed. Same direction as `tier_handler::post_plan_features`.
    // `plans` keeps its checkout/marketing role: `attribute_plan_upgrade` below legitimately
    // still receives a `plans.id` (it reads `plans.name` / `price_monthly`).
    let tier_id = assign_plan(&state, account_id, plan_id).await?;

    // ── Close the request this call just satisfied (cron 2026-10-23) ────────────────────────────
    // The operator's Upgrade-requests screen used to name the action and offer no button: seating a
    // workspace meant leaving the screen and re-finding it on Tiers. The screen now calls this same
    // route, so the record of the refused purchase has to move with it — otherwise the operator
    // switches the workspace, the table still reads "No payment provider yet", and the next operator
    // switches it again. `operator_activated` is the state that says "the money still never moved,
    // but the plan was given by hand", and `list_checkout_requests` keeps it as history.
    //
    // Scoped to THIS plan's slug (plus a request that recorded no slug at all, which cannot be a
    // different plan) so a workspace with two open requests keeps the one this click did not answer.
    // A failure here must not fail the assignment: the tier is already written, and reporting an
    // error would tell the operator nothing was done.
    let requests_closed = sqlx::query(
        "UPDATE checkout_sessions SET status = 'operator_activated', updated_at = NOW() \
         WHERE account_id = $1 AND status = 'provider_missing' \
           AND (metadata->>'plan_slug' = $2 OR metadata->>'plan_slug' IS NULL)",
    )
    .bind(account_id)
    .bind(&plan_slug)
    .execute(&state.db)
    .await
    .map(|r| r.rows_affected())
    .unwrap_or(0);

    Ok(Json(json!({
        "status": "assigned",
        "plan_id": plan_id.to_string(),
        "account_id": account_id.to_string(),
        "plan_tier_id": tier_id.to_string(),
        "requests_closed": requests_closed,
    })))
}

/// The ONE writer of an account's plan tier (cron 2026-10-20).
///
/// TWO callers need this: the operator's Change Plan picker ([`admin_assign_plan`]) and
/// `support_tickets::update_ticket` when it closes a plan request the customer filed. They must
/// write the SAME column and settle the same referrer — a second hand-rolled UPDATE inside the
/// ticket handler would have switched the tier and silently skipped `attribute_plan_upgrade`, so a
/// customer who asked to buy, and was switched, would never have credited the affiliate who sent
/// them.
///
/// `plan_id` is a row of `plans` (the catalogue the console and the checkout read); the canonical
/// TIER row is resolved through the shared slug. Returns the tier id that was written.
pub async fn assign_plan(
    state: &AppState,
    account_id: Uuid,
    plan_id: Uuid,
) -> Result<Uuid, AppError> {
    let mapping = sqlx::query(
        r#"SELECT p.slug AS plan_slug, pt.id AS tier_id
             FROM plans p
             LEFT JOIN plan_tiers pt ON pt.slug = p.slug
            WHERE p.id = $1"#,
    )
    .bind(plan_id)
    .fetch_one(&state.db)
    .await?;
    let plan_slug: String = mapping.get("plan_slug");
    let tier_id: Option<Uuid> = mapping.get("tier_id");

    // Honest failure when the plan has no tier of that slug: the account must not be left on a
    // stale tier, so nothing is written.
    let tier_id = tier_id.ok_or_else(|| {
        AppError::NotFound(format!(
            "No plan found matching plan slug '{}' — create the plan first",
            plan_slug
        ))
    })?;

    sqlx::query("UPDATE accounts SET plan_tier_id = $1 WHERE id = $2")
        .bind(tier_id)
        .bind(account_id)
        .execute(&state.db)
        .await?;

    // Credit the referring affiliate if this is a paid-plan assignment.
    attribute_plan_upgrade(state, account_id, plan_id).await;

    Ok(tier_id)
}

/// GET /api/v1/admin/plans/{id}
pub async fn get_plan(
    State(state): State<AppState>,
    Path(id): Path<String>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let plan_id =
        Uuid::parse_str(&id).map_err(|_| AppError::BadRequest("Invalid plan ID".to_string()))?;

    let plan = sqlx::query_as::<_, Plan>(
        r#"SELECT id, name, slug, description,
                  COALESCE(NULLIF(price_monthly, 0), price) AS price_monthly,
                  price_yearly,
                  features, is_active, sort_order, payment_provider, purchase_url, created_at, updated_at
           FROM plans WHERE id = $1"#,
    )
    .bind(plan_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Plan not found".to_string()))?;

    Ok(Json(json!({ "plan": plan })))
}

/// PUT /api/v1/admin/plans/{id}
pub async fn update_plan(
    State(state): State<AppState>,
    Path(id): Path<String>,
    user: AuthenticatedUser,
    Json(body): Json<UpdatePlanInput>,
) -> Result<Json<Value>, AppError> {
    let plan_id =
        Uuid::parse_str(&id).map_err(|_| AppError::BadRequest("Invalid plan ID".to_string()))?;

    // Get existing plan
    let existing = sqlx::query(
        r#"SELECT name, slug, description,
                  COALESCE(NULLIF(price_monthly, 0), price) AS price_monthly,
                  price_yearly,
                  features, is_active, sort_order, payment_provider, purchase_url
           FROM plans WHERE id = $1"#,
    )
    .bind(plan_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Plan not found".to_string()))?;

    let name: String = body.name.unwrap_or_else(|| existing.get("name"));
    // The slug is an IDENTITY, not a display field. `assign_plan` maps `plans` -> `plan_tiers` BY
    // SLUG, and the entitlement reads join the same way, so a rewritten slug silently unlinks a plan
    // from its tier. MEASURED LIVE 2026-10-22: a PUT carrying only `is_active` regenerated the slug
    // from the name (`probe-life-life1` -> `probe-lifecycle`), after which assigning that plan
    // answers "No plan found matching plan slug" and the tier lookup has nothing to join to. The
    // live rows hide it because slugify(name) happens to equal their stored slug (Pro -> pro), so it
    // only bites a plan whose slug is not the slug of its name — and then it bites silently.
    let slug: String = match body.slug {
        Some(s) => {
            let s = s.trim().to_string();
            if s.is_empty() {
                return Err(AppError::BadRequest("slug cannot be empty".to_string()));
            }
            s
        }
        None => existing.get("slug"),
    };
    let description: Option<String> = body.description.or_else(|| existing.get("description"));
    let price_monthly: f64 = body
        .price_monthly
        .unwrap_or_else(|| existing.get("price_monthly"));
    let price_yearly: f64 = body
        .price_yearly
        .unwrap_or_else(|| existing.get("price_yearly"));
    let features: Value = body.features.unwrap_or_else(|| existing.get("features"));
    let is_active: bool = body.is_active.unwrap_or_else(|| existing.get("is_active"));
    let sort_order: i32 = body
        .sort_order
        .unwrap_or_else(|| existing.get("sort_order"));
    let payment_provider: Option<String> = existing.get("payment_provider");
    let payment_provider: Option<String> = body.payment_provider.or(payment_provider);
    // The plan's own hosted payment page, with the CLEAR arm this column needs: absent leaves what
    // is stored, `null` or a blank string REMOVES the link (how the operator takes a plan off
    // sale-by-link), anything else stores the trimmed value. `billing::checkout` reads it per
    // purchase, so the next Upgrade press after a clear is refused again.
    let purchase_url: Option<String> = match body.purchase_url {
        // Only a link the REQUEST supplied is judged: an omitted field keeps whatever is stored, and
        // re-judging stored bytes would refuse an unrelated save on a plan that predates this guard.
        Some(supplied) => {
            let trimmed = supplied.as_deref().map(str::trim).filter(|s| !s.is_empty());
            if let Some(url) = trimmed {
                require_hosted_payment_url(url)?;
            }
            trimmed.map(|s| s.to_string())
        }
        None => existing.get("purchase_url"),
    };

    sqlx::query(
        r#"UPDATE plans SET
               name = $1, slug = $2, description = $3,
               price_monthly = $4, price = $4, price_yearly = $5,
               features = $6, is_active = $7, sort_order = $8,
               payment_provider = $9, purchase_url = $10, updated_at = now()
           WHERE id = $11"#,
    )
    .bind(&name)
    .bind(&slug)
    .bind(&description)
    .bind(price_monthly)
    .bind(price_yearly)
    .bind(&features)
    .bind(is_active)
    .bind(sort_order)
    .bind(&payment_provider)
    .bind(&purchase_url)
    .bind(plan_id)
    .execute(&state.db)
    .await?;

    let plan = sqlx::query_as::<_, Plan>(
        r#"SELECT id, name, slug, description,
                  COALESCE(NULLIF(price_monthly, 0), price) AS price_monthly,
                  price_yearly,
                  features, is_active, sort_order, payment_provider, purchase_url, created_at, updated_at
           FROM plans WHERE id = $1"#,
    )
    .bind(plan_id)
    .fetch_one(&state.db)
    .await?;

    Ok(Json(json!({ "plan": plan })))
}

/// DELETE /api/v1/admin/plans/{id} — soft delete via is_active = false
pub async fn delete_plan(
    State(state): State<AppState>,
    Path(id): Path<String>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let plan_id =
        Uuid::parse_str(&id).map_err(|_| AppError::BadRequest("Invalid plan ID".to_string()))?;

    let result = sqlx::query(
        "UPDATE plans SET is_active = false, updated_at = now() WHERE id = $1 AND is_active = true",
    )
    .bind(plan_id)
    .execute(&state.db)
    .await?;

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound(
            "Plan not found or already inactive".to_string(),
        ));
    }

    Ok(Json(json!({ "status": "deleted", "id": id })))
}

/// PUT /api/v1/admin/plans/{id}/features
pub async fn admin_update_plan_features(
    State(state): State<AppState>,
    Path(id): Path<String>,
    user: AuthenticatedUser,
    Json(body): Json<UpdateFeaturesInput>,
) -> Result<Json<Value>, AppError> {
    let plan_id =
        Uuid::parse_str(&id).map_err(|_| AppError::BadRequest("Invalid plan ID".to_string()))?;

    sqlx::query("UPDATE plans SET features = $1, updated_at = now() WHERE id = $2")
        .bind(&body.features)
        .bind(plan_id)
        .execute(&state.db)
        .await?;

    let plan = sqlx::query_as::<_, Plan>(
        r#"SELECT id, name, slug, description,
                  COALESCE(NULLIF(price_monthly, 0), price) AS price_monthly,
                  price_yearly,
                  features, is_active, sort_order, payment_provider, purchase_url, created_at, updated_at
           FROM plans WHERE id = $1"#,
    )
    .bind(plan_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Plan not found".to_string()))?;

    Ok(Json(json!({ "plan": plan })))
}
