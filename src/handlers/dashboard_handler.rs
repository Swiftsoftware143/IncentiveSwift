//! Dashboard handler — aggregate stats for the authenticated user's campaigns.

use crate::error::AppError;
use crate::security::auth::AuthenticatedUser;
use crate::state::AppState;
use axum::{extract::State, Json};
use serde_json::{json, Value};
use uuid::Uuid;

/// GET /api/v1/dashboard/stats
/// Returns aggregate counts for the authenticated account's tenant.
pub async fn dashboard_stats(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;

    // Count campaigns for this tenant
    let total_campaigns: i64 = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT COUNT(*) FROM campaigns c
         WHERE c.account_id = $1",
    )
    .bind(account_id)
    .fetch_one(&state.db)
    .await?
    .unwrap_or(0);

    // Count contacts/participants for this tenant
    let total_participants: i64 = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT COUNT(DISTINCT e.contact_id) FROM entries e
         JOIN campaigns c ON c.id = e.campaign_id
         WHERE c.account_id = $1",
    )
    .bind(account_id)
    .fetch_one(&state.db)
    .await?
    .unwrap_or(0);

    // Count total entries (interactions)
    let total_entries: i64 = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT COUNT(*) FROM entries e
         JOIN campaigns c ON c.id = e.campaign_id
         WHERE c.account_id = $1",
    )
    .bind(account_id)
    .fetch_one(&state.db)
    .await?
    .unwrap_or(0);

    // Points awarded across this tenant's loyalty programmes. The SUM and the joins are the ones
    // `analytics_handler::overview` already uses for its `total_points_issued`, so the dashboard
    // tile and the Analytics panel cannot disagree about the same number.
    let total_points: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(m.lifetime_points), 0) FROM loyalty_members m
         JOIN loyalty_programs p ON p.id = m.program_id
         JOIN campaigns c ON c.id = p.campaign_id
         WHERE c.account_id = $1",
    )
    .bind(account_id)
    .fetch_one(&state.db)
    .await
    .unwrap_or(0);

    // Rewards = prizes this tenant's campaigns have actually awarded. Same query shape as
    // `analytics_handler::overview`'s `total_wins`; `loyalty_rewards_earned` is a separate ledger
    // and is deliberately NOT mixed in, or the two screens would count different things.
    let total_rewards: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM campaign_wins w
         JOIN campaigns c ON c.id = w.campaign_id
         WHERE c.account_id = $1",
    )
    .bind(account_id)
    .fetch_one(&state.db)
    .await
    .unwrap_or(0);

    // ── OPERATOR REVENUE VIEW (cron 2026-11-08) ────────────────────────────────────────────────
    // Until now the operator's dashboard could not answer "what are these workspaces on, and what
    // is that worth?" — it carried the five loyalty counters and nothing about plans. The plan
    // catalogue and the per-workspace tier were both live (`plan_tiers`, `accounts.plan_tier_id`)
    // and the console could ASSIGN a plan, so the numbers existed and no screen showed them.
    //
    // AGGREGATES ONLY, on purpose. This route is reachable by every signed-in workspace, and the
    // fleet's tenancy rule is that no tenant may read another's data — but the operator does need
    // the revenue picture. So the payload carries COUNTS and MONEY, never a workspace name, an
    // email or an id: the operator sees the shape of the book without this route becoming a
    // cross-tenant directory (`/admin/checkout-requests` is the separate, admin-guarded reader
    // for the individual purchase attempts). A non-operator gets an empty list and zeros, so the
    // console renders no card at all rather than a misleading one.
    //
    // The values are LIST PRICE — no money has necessarily been collected, which is why the
    // refused-purchase counts sit beside them. `price_monthly`/`price_annual` come from
    // `plan_tiers`; a workspace with no tier row is on the app's base plan, which is `Free` at 0
    // (the same fallback `features.rs` uses when `plan_tier_id IS NULL`), so those accounts are
    // coalesced into the Free bucket instead of inventing a "$0 unnamed" tier.
    let is_operator = user.role == "admin" || user.role == "super_admin";
    let mut plan_mix: Vec<Value> = Vec::new();
    let mut workspaces_total: i64 = 0;
    let mut plan_value_monthly: f64 = 0.0;
    let mut plan_value_yearly: f64 = 0.0;
    let mut checkouts_refused: i64 = 0;
    let mut checkouts_paid: i64 = 0;
    if is_operator {
        let rows: Vec<(String, String, f64, f64, i64)> = sqlx::query_as(
            "SELECT COALESCE(pt.name, 'Free') AS plan_name, \
                    COALESCE(pt.slug, 'free') AS plan_slug, \
                    COALESCE(pt.price_monthly, 0)::float8 AS price_monthly, \
                    COALESCE(pt.price_annual, 0)::float8 AS price_annual, \
                    COUNT(*)::bigint AS accounts \
               FROM accounts a \
               LEFT JOIN plan_tiers pt ON pt.id = a.plan_tier_id \
              WHERE COALESCE(a.probe_harness, '') = '' \
              GROUP BY 1, 2, 3, 4 \
              ORDER BY 3 DESC, 1",
        )
        .fetch_all(&state.db)
        .await
        .unwrap_or_default();
        for (plan_name, plan_slug, price_monthly, price_annual, accounts) in rows {
            workspaces_total += accounts;
            plan_value_monthly += price_monthly * accounts as f64;
            plan_value_yearly += price_annual * accounts as f64;
            plan_mix.push(json!({
                "plan_name": plan_name,
                "plan_slug": plan_slug,
                "price_monthly": price_monthly,
                "price_annual": price_annual,
                "accounts": accounts,
            }));
        }
        // The status vocabulary is `billing::checkout`'s own: `provider_missing` is a buyer the
        // installation could not charge, `failed`/`cancelled` are attempts that died, and
        // `operator_activated` means the operator already honoured the request — so it is NOT
        // counted as refused, or the number would never fall as the operator works through it.
        checkouts_refused = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM checkout_sessions \
              WHERE status IN ('provider_missing', 'failed', 'cancelled')",
        )
        .fetch_one(&state.db)
        .await
        .unwrap_or(0);
        checkouts_paid = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM checkout_sessions WHERE status = 'completed'",
        )
        .fetch_one(&state.db)
        .await
        .unwrap_or(0);
    }

    Ok(Json(json!({
        "total_campaigns": total_campaigns,
        "total_rewards": total_rewards,
        "total_participants": total_participants,
        "total_points": total_points,
        "total_entries": total_entries,
        "operator": is_operator,
        "workspaces_total": workspaces_total,
        "plan_mix": plan_mix,
        "plan_value_monthly": plan_value_monthly,
        "plan_value_yearly": plan_value_yearly,
        "checkouts_refused": checkouts_refused,
        "checkouts_paid": checkouts_paid,
    })))
}

/// GET /api/v1/dashboard/activity
/// Returns recent activity feed for the authenticated user's campaigns.
pub async fn dashboard_activity(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;

    let rows = sqlx::query(
        r#"SELECT e.id, COALESCE(c.name, 'Unknown') as campaign_name,
                  COALESCE(ct.email, ct.phone, 'Anonymous') as participant,
                  e.created_at
           FROM entries e
           JOIN campaigns c ON c.id = e.campaign_id
           LEFT JOIN contacts ct ON ct.id = e.contact_id
           WHERE c.account_id = $1 ORDER BY e.created_at DESC LIMIT 20"#,
    )
    .bind(account_id)
    .fetch_all(&state.db)
    .await?;

    let mut activities: Vec<Value> = Vec::new();
    for row in &rows {
        use sqlx::Row;
        let id: Uuid = row.get("id");
        let campaign: String = row.get("campaign_name");
        let participant: String = row.get("participant");
        // `entries.created_at` is TIMESTAMPTZ (`information_schema` says so). Asking sqlx for a
        // `NaiveDateTime` (SQL type TIMESTAMP) is not a decode it can do, and `row.get` PANICS:
        // `mismatched types; Rust type chrono::naive::datetime::NaiveDateTime (as SQL type TIMESTAMP)
        // is not compatible with SQL type TIMESTAMPTZ` — measured this route live on the deployed
        // binary, where it killed the whole API process and restarted it (kanban t_04553fa6). The
        // crate's working precedent is `DateTime<Utc>` (offers_handler, campaign_integrations, …).
        let created_at: chrono::DateTime<chrono::Utc> = row.get("created_at");
        activities.push(json!({
            "id": id,
            "type": "entry",
            "campaign": campaign,
            "participant": participant,
            "created_at": created_at,
            "action": format!("New entry in {}", campaign),
        }));
    }

    Ok(Json(json!({ "activities": activities })))
}

/// GET /api/v1/leads - list all entries as leads
pub async fn list_leads(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;
    let rows = sqlx::query(
        r#"SELECT e.id, ct.email, ct.phone, ct.first_name, ct.last_name, c.name as campaign_name, e.created_at
           FROM entries e JOIN campaigns c ON c.id = e.campaign_id
           LEFT JOIN contacts ct ON ct.id = e.contact_id
           WHERE c.account_id = $1 ORDER BY e.created_at DESC LIMIT 100"#,
    ).bind(account_id).fetch_all(&state.db).await?;
    let mut leads: Vec<Value> = Vec::new();
    for row in &rows {
        use sqlx::Row;
        leads.push(json!({
            "id": row.get::<Uuid,_>("id"),
            "email": row.get::<Option<String>,_>("email"),
            "phone": row.get::<Option<String>,_>("phone"),
            "first_name": row.get::<Option<String>,_>("first_name"),
            "last_name": row.get::<Option<String>,_>("last_name"),
            "campaign": row.get::<String,_>("campaign_name"),
            // TIMESTAMPTZ, not TIMESTAMP — same decode defect as `dashboard_activity` above: a
            // `NaiveDateTime` here PANICS the worker (measured live: it restarted the API process,
            // kanban t_04553fa6). The admin console's Leads view renders this route
            // (www-admin/index.html:1086-1108), so the panic was a console-visible outage.
            "created_at": row.get::<chrono::DateTime<chrono::Utc>,_>("created_at"),
        }));
    }
    Ok(Json(json!({"leads": leads})))
}

/// GET /api/v1/plans (public - no auth required)
///
/// Ordered as a PRICE LADDER, not alphabetically (kanban, cron `default`, 2026-10-13). The tenant's
/// "Plan & Pricing" screen renders this catalogue in the order returned, and `ORDER BY name` listed
/// Enterprise ($199) ABOVE Free ($0) — a scrambled ladder on the one screen whose whole job is "what
/// the other plans cost". The fleet's convention for a plan catalogue is cheapest-first (ADASwift:
/// `ORDER BY price ASC`). `sort_order` is honoured first so an operator can still pin an explicit
/// order; every row carries 0 today, so the price ladder decides.
pub async fn list_public_plans(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let rows = sqlx::query(
        r#"-- WHAT A PLAN INCLUDES is read from the canonical entitlement model, never from `plans.features`.
           -- Measured LIVE 2026-10-18: this app's `plans.features` still carried FunnelSwift's
           -- vocabulary ("basic_leads", "unlimited_leads", "advanced_tags", "dual_routing",
           -- "multi_tenant"), so the tenant's Plan & Pricing screen advertised features this product
           -- does not have — while the SAME screen's "your current plan" card listed the real
           -- IncentiveSwift keys from /auth/me (mechanic_quiz, mechanic_spin_wheel, ...) — two halves
           -- of one screen contradicting each other. The entitlement model is
           -- `plan_tiers` -> `tier_features` -> `features`, the same tables every runtime gate reads
           -- (features.rs), joined here by slug. The legacy array stays as the fallback for a `plans`
           -- row whose slug has no tier.
           -- THE YEARLY FIGURE (2026-10-24). The tenant dashboard now offers a Monthly/Yearly switch
           -- and checkout can sell either term, so the catalogue has to publish both amounts. Same
           -- precedence checkout charges: `plans.price_yearly` when an operator filled it in, else
           -- `plan_tiers.price_annual` — the column the operator's Tier editor actually writes
           -- (`plans.price_yearly` is 0 on every row live; the tiers table carries 399 / 1699). A
           -- tier with no yearly price answers 0 and the screen says "monthly only" rather than
           -- inventing a figure.
           SELECT p.id, p.name, p.slug, ROUND(p.price)::int as monthly_price, 0 as monthly_zc_pool, NULL::text as description,
                  COALESCE(NULLIF(p.price_yearly, 0), NULLIF(pt.price_annual, 0), 0)::float8 as annual_price,
                  COALESCE((
                      SELECT jsonb_agg(CASE WHEN tf.limit_value IS NULL THEN f.label
                                            WHEN tf.limit_value < 0 THEN f.label
                                                 || CASE WHEN f.label ILIKE 'Unlimited%' THEN '' ELSE ' - unlimited' END
                                            ELSE f.label || ' - ' || tf.limit_value::text END
                                       ORDER BY f.category NULLS LAST, f.label)
                      FROM plan_tiers pt
                      JOIN tier_features tf ON tf.tier_id = pt.id AND tf.enabled
                      JOIN features f ON f.id = tf.feature_id
                      WHERE lower(pt.slug) = lower(p.slug)
                  ), p.features) as features,
                  NULL::text as how_it_works
           FROM plans p LEFT JOIN plan_tiers pt ON lower(pt.slug) = lower(p.slug) WHERE p.is_active = true
           ORDER BY p.sort_order ASC NULLS LAST, p.price ASC NULLS LAST, p.name ASC"#,
    ).fetch_all(&state.db).await?;
    let mut plans: Vec<Value> = Vec::new();
    for row in &rows {
        use sqlx::Row;
        plans.push(json!({
            "id": row.get::<Uuid,_>("id"),
            "name": row.get::<String,_>("name"),
            "slug": row.get::<String,_>("slug"),
            "monthly_price": row.get::<i32,_>("monthly_price"),
            // What a yearly subscription costs, from the same precedence checkout charges. 0 means
            // "this tier has no yearly price", and the dashboard says so instead of guessing.
            "annual_price": row.get::<f64,_>("annual_price"),
            "monthly_zc_pool": row.get::<i32,_>("monthly_zc_pool"),
            "description": row.get::<Option<String>,_>("description"),
            "features": row.get::<Option<serde_json::Value>,_>("features"),
            "how_it_works": row.get::<Option<String>,_>("how_it_works"),
        }));
    }
    Ok(Json(json!({"plans": plans})))
}

#[cfg(test)]
mod timestamp_decode_tests {
    //! RED/GREEN guard for the TIMESTAMPTZ-as-NaiveDateTime decode that PANICKED two live routes
    //! (kanban t_04553fa6), driven against the real schema.
    //!
    //! Measured on the deployed binary: `GET /api/v1/leads` (the route the admin console's Leads view
    //! renders) and `GET /api/v1/dashboard/activity` each killed the API process —
    //!
    //! ```text
    //! thread 'tokio-rt-worker' panicked at src/handlers/dashboard_handler.rs:125:31:
    //! called `Result::unwrap()` on an `Err` value: ColumnDecode { index: "\"created_at\"",
    //! source: "mismatched types; Rust type `chrono::naive::datetime::NaiveDateTime` (as SQL type
    //! `TIMESTAMP`) is not compatible with SQL type `TIMESTAMPTZ`" }
    //! ```
    //!
    //! `entries.created_at` is TIMESTAMPTZ, so the decode must be `DateTime<Utc>` (the crate's own
    //! precedent). This test executes both routes' exact statements, decodes `created_at` the way the
    //! handlers do, and pins the column's type — so the type and the decode cannot drift apart again
    //! without this failing.
    //!
    //! DB-backed and read-only, OPT-IN on the same two variables as `features::usage_arm_tests`:
    //! `INC_ARM_DB_TEST=1` and `DATABASE_URL`.
    use sqlx::{PgPool, Row};
    use uuid::Uuid;

    async fn pool_or_skip() -> Option<PgPool> {
        if std::env::var("INC_ARM_DB_TEST").as_deref() != Ok("1") {
            return None;
        }
        let url = std::env::var("DATABASE_URL").ok()?;
        Some(
            PgPool::connect(&url)
                .await
                .expect("connect to DATABASE_URL"),
        )
    }

    #[tokio::test]
    async fn leads_and_activity_rows_decode_their_timestamptz() {
        let Some(pool) = pool_or_skip().await else {
            return;
        };

        // The column must be TIMESTAMPTZ. A `NaiveDateTime` decode of it is not a slow path — it is
        // the panic above, for every row, on every account.
        let column_type: String = sqlx::query_scalar(
            "SELECT data_type FROM information_schema.columns
             WHERE table_name = 'entries' AND column_name = 'created_at'",
        )
        .fetch_one(&pool)
        .await
        .expect("entries.created_at must exist");
        assert_eq!(
            column_type, "timestamp with time zone",
            "entries.created_at changed type; revisit the decode in dashboard_activity / list_leads"
        );

        let acct: Option<Uuid> = sqlx::query_scalar(
            "SELECT c.account_id FROM entries e JOIN campaigns c ON c.id = e.campaign_id LIMIT 1",
        )
        .fetch_optional(&pool)
        .await
        .expect("pick an account with an entry");
        let Some(acct) = acct else {
            eprintln!("SKIP: no entry row in the database to decode");
            return;
        };

        // list_leads' own statement (`GET /api/v1/leads`).
        let rows = sqlx::query(
            r#"SELECT e.id, ct.email, ct.phone, ct.first_name, ct.last_name, c.name as campaign_name, e.created_at
               FROM entries e JOIN campaigns c ON c.id = e.campaign_id
               LEFT JOIN contacts ct ON ct.id = e.contact_id
               WHERE c.account_id = $1 ORDER BY e.created_at DESC LIMIT 100"#,
        )
        .bind(acct)
        .fetch_all(&pool)
        .await
        .expect("list_leads statement");
        assert!(!rows.is_empty(), "the chosen account owns >= 1 entry");
        let decoded: chrono::DateTime<chrono::Utc> = rows[0].get("created_at");
        println!(
            "list_leads decoded created_at = {decoded} ({} row(s))",
            rows.len()
        );

        // dashboard_activity's own statement (`GET /api/v1/dashboard/activity`).
        let rows = sqlx::query(
            r#"SELECT e.id, COALESCE(c.name, 'Unknown') as campaign_name,
                      COALESCE(ct.email, ct.phone, 'Anonymous') as participant,
                      e.created_at
               FROM entries e
               JOIN campaigns c ON c.id = e.campaign_id
               LEFT JOIN contacts ct ON ct.id = e.contact_id
               WHERE c.account_id = $1 ORDER BY e.created_at DESC LIMIT 20"#,
        )
        .bind(acct)
        .fetch_all(&pool)
        .await
        .expect("dashboard_activity statement");
        assert!(!rows.is_empty());
        let decoded: chrono::DateTime<chrono::Utc> = rows[0].get("created_at");
        println!(
            "dashboard_activity decoded created_at = {decoded} ({} row(s))",
            rows.len()
        );
    }
}
