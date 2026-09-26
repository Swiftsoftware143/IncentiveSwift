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
    // `accounts.id` is `uuid` — see `account_uuid`.
    let account_id = account_uuid(account_id)?;

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
    // `limit_value` is INT4, so it decodes as `i32`: asking sqlx for an `Option<i64>` here is a
    // `mismatched types; Rust type Option<i64> (as SQL type INT8) is not compatible with SQL type
    // INT4` error the moment a tier actually carries a numeric limit (measured on
    // `GET /api/v1/admin/plans/:id/domains`, kanban t_e2cecfcb — it reads the same column).
    // No caller invokes this function today, which is why the drift had never surfaced.
    let tf_val: Option<i32> = sqlx::query_scalar(
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
        return check_limit(db, account_id, feature_key, label, val as i64).await;
    }

    // Tier base columns for the two built-in numeric limits (INT4 as well).
    let base_val: Option<i32> = match feature_key {
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
        Some(v) => check_limit(db, account_id, feature_key, label, v as i64).await,
    }
}

/// Enforce the account's `max_leads` allowance for whichever account owns `campaign_id`.
///
/// This is the app's LEAD-creation choke point. A lead in IncentiveSwift is an entry of a campaign the
/// account owns (`count_usage`'s `max_leads` arm, `GET /api/v1/leads` and `business_handler`'s
/// `total_leads` all say so with the identical statement), and EVERY writer of `entries` has the
/// campaign in hand, so this one helper gives every one of them the same gate:
///
///   * `db::entries::create_entry` — the shared creator behind the generic capture route
///     (`POST /api/v1/entries`) and the mystery / long-form-qualifier / scratch-card / score-reveal /
///     poll / countdown / chat handlers;
///   * the direct writers that do not go through it: `handlers::quiz_handler`,
///     `db::raffles::enter_raffle`, `handlers::sms_handler`'s chat-funnel entry,
///     `mechanics::milestone_engine` (bonus entries) and `mechanics::prize_draw`'s
///     `record_win` / `record_loss` (the spin mechanic's entries).
///
/// The allowance itself is the canonical entitlement read — `enforce_feature_limit` for the key
/// `max_leads`, which resolves `tier_features.limit_value` on the account's OWN `plan_tiers` row
/// (`accounts.plan_tier_id`). It is NOT read from `plans.max_leads`: `plans` has no FK from `accounts`,
/// that column has no reader and no writer in this crate (the admin plans API never selects or binds
/// it), and its only link to an account is a slug join that coincides for exactly one of the three
/// live plans. Its live values were migrated into `tier_features` by
/// `migrations/20260926_max_leads_entitlement.sql`, so the numbers the catalogue advertised
/// (Free 5 / Pro 100 / Enterprise -1 = unlimited) are exactly what this gate now applies.
///
/// An account with no tier, or a tier with no `max_leads` row, is ALLOWED (the
/// `enforce_feature_limit` convention: "not configured" is not a cap).
pub async fn enforce_lead_limit_for_campaign(
    db: &PgPool,
    campaign_id: Uuid,
) -> Result<(), AppError> {
    let owner: Option<Uuid> = sqlx::query_scalar("SELECT account_id FROM campaigns WHERE id = $1")
        .bind(campaign_id)
        .fetch_optional(db)
        .await?;

    // No campaign row: nothing to attribute the lead to, and the caller's own INSERT will fail the
    // campaign FK if it is really gone. Not a limit verdict, so do not manufacture one.
    let Some(owner) = owner else {
        return Ok(());
    };

    enforce_feature_limit(db, &owner.to_string(), "max_leads", "Leads").await
}

async fn check_limit(
    db: &PgPool,
    account_id: Uuid,
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

/// The `industry_limit` entitlement for one account — how many industry dashboards
/// (industry = dashboard = template category) the account may hold at once.
///
/// Canonical read: `tier_features.limit_value` for the feature key `industry_limit` on the
/// account's OWN plan tier — the same table `enforce_feature_limit` reads and the same table the
/// admin UI writes (`/api/v1/admin/plans/:id/features`). It deliberately does NOT read
/// `plans.features`: `plans` is the marketing and checkout table, its `features` column is a jsonb
/// ARRAY, and `accounts.plan_tier_id` has an FK to `plan_tiers(id)`, so a plan-shaped read can only
/// ever match by id COINCIDENCE (that was the third and last `plans`-shaped statement in
/// `src/handlers/auth_handler.rs`, kanban t_0961f382 — the cap resolved to 1 for all 56 accounts).
///
/// Returns the configured `limit_value` when the tier has an ENABLED row for the key that carries a
/// numeric limit, otherwise `INDUSTRY_LIMIT_DEFAULT`. That default is deliberately RESTRICTIVE and
/// is NOT the `enforce_feature_limit` "no row = not configured = allow" convention: this
/// entitlement has always meant "one dashboard on the base plan" (migration 00017 intended 1 for
/// the free plan too), and loosening it silently would hand every account unlimited industries.
/// A tier that grants the feature but leaves `limit_value` NULL also gets the default (the key is a
/// numeric cap, so "enabled, no number" is not a meaningful grant).
///
/// `limit_value` is INT4, so it decodes as `i32` — asking sqlx for `i64` is a runtime
/// "mismatched types ... SQL type INT4" error the moment a tier actually carries a limit.
///
/// -1 => unlimited, 0 => not available on this plan, N > 0 => cap N (see `check_limit`).
pub const INDUSTRY_LIMIT_DEFAULT: i64 = 1;

pub async fn industry_limit(db: &PgPool, account_id: Uuid) -> Result<i64, AppError> {
    let configured: Option<i32> = sqlx::query_scalar(
        "SELECT tf.limit_value
           FROM accounts a
           JOIN tier_features tf ON tf.tier_id = a.plan_tier_id AND tf.enabled
           JOIN features f ON f.id = tf.feature_id
          WHERE a.id = $1 AND f.key = 'industry_limit'",
    )
    .bind(account_id)
    .fetch_optional(db)
    .await?
    .flatten();

    Ok(configured.map(i64::from).unwrap_or(INDUSTRY_LIMIT_DEFAULT))
}

/// The credit allowance entitlement keys. Both are read by `GET /api/v1/credits/balance`
/// (`handlers/credits_handler.rs`) through `credit_limit` below.
pub const CREDIT_MONTHLY_KEY: &str = "credits_monthly";
pub const CREDIT_OVERDRAFT_KEY: &str = "credits_overdraft";

/// Fallback allowance for a tier with no ENABLED row for a credit key — and the documented default
/// when `limit_value` is NULL (a numeric allowance with no number is not a grant).
///
/// 0 = "no credits included", which is exactly what every one of the 56 live accounts advertises
/// today (measured 2026-09-25, kanban t_329b61b2), so seating the keys does not move any account
/// until the owner assigns a numeric limit in the UI. It is deliberately not the
/// `enforce_feature_limit` "no row = not configured = allow" convention: an allowance has no
/// permissive reading.
pub const CREDIT_LIMIT_DEFAULT: i64 = 0;

/// The credit allowance (`credits_monthly` / `credits_overdraft`) for one account, resolved from
/// the canonical entitlement model: `tier_features.limit_value` for the key on the account's OWN
/// plan tier — the same table `enforce_feature_limit` and `industry_limit` read and the same table
/// the admin UI writes (`POST /api/v1/admin/plans/:id/features`).
///
/// It deliberately does NOT read `plans.features`: `plans` is the marketing and checkout table,
/// `accounts.plan_tier_id` has an FK to `plan_tiers(id)`, and `plans.features` is a jsonb ARRAY.
/// A `plans`-shaped read therefore has two independent ways to be inert (kanban t_329b61b2,
/// measured live before this fix): the 4 accounts on the `pro` tier got NO row at all (only the
/// `free` tier id coincides with the `free` plan id, `8b8cc0e5…`), and the row that did match
/// answered NULL for every key because an array has no object keys — which `COALESCE(…,0)` then
/// published as "0 monthly credits / 0 overdraft" for all 56 accounts, including tiers explicitly
/// configured with an allowance.
///
/// `limit_value` is INT4, so it decodes as `i32`. `-1`/`0`/`N` semantics are the app's standard
/// entitlement semantics (see `check_limit`): -1 unlimited, 0 none included, N > 0 => N.
pub async fn credit_limit(db: &PgPool, account_id: Uuid, key: &str) -> Result<i64, AppError> {
    let configured: Option<i32> = sqlx::query_scalar(
        "SELECT tf.limit_value
           FROM accounts a
           JOIN tier_features tf ON tf.tier_id = a.plan_tier_id AND tf.enabled
           JOIN features f ON f.id = tf.feature_id
          WHERE a.id = $1 AND f.key = $2",
    )
    .bind(account_id)
    .bind(key)
    .fetch_optional(db)
    .await?
    .flatten();

    Ok(configured.map(i64::from).unwrap_or(CREDIT_LIMIT_DEFAULT))
}

/// The account's own plan tier name — the human label of the plan it is actually on
/// (`plan_tiers.name`), never `plans.name`. `plans.name` is the marketing row's name and can only
/// be reached by the id coincidence described on `credit_limit`: before this fix the 4 accounts on
/// the `pro` tier were told their plan was "Unknown" while `GET /api/v1/credits/balance` answered
/// 200. `None` when the account has no tier.
pub async fn plan_tier_name(db: &PgPool, account_id: Uuid) -> Result<Option<String>, AppError> {
    let name: Option<String> = sqlx::query_scalar(
        "SELECT pt.name
           FROM accounts a
           JOIN plan_tiers pt ON pt.id = a.plan_tier_id
          WHERE a.id = $1",
    )
    .bind(account_id)
    .fetch_optional(db)
    .await?;
    Ok(name)
}

/// The account id arrives as a string (the JWT `sub`), but every column this module compares it
/// with is `uuid` (`accounts.id`, `campaigns.account_id`, `tags.account_id`). Binding the `&str`
/// makes sqlx send the parameter as **TEXT**, and Postgres then refuses the comparison outright
/// instead of coercing it — `operator does not exist: uuid = text` (measured: `PREPARE p(text) AS
/// SELECT COUNT(*) FROM campaigns WHERE account_id = $1`). So parse once at each string boundary
/// and bind a real `Uuid`, exactly as `access::feature_gate::account_tier_id` and
/// `handlers::support_tickets::tenant_scope` already do.
fn account_uuid(account_id: &str) -> Result<Uuid, AppError> {
    Uuid::parse_str(account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID format".to_string()))
}

/// One usage number for the `me/usage` payload, with a failing arm made OBSERVABLE.
///
/// The payload shape is a published contract (`campaigns`/`leads`/`tags` are always present), so
/// the `0` fallback stays — but it used to be `unwrap_or(0)` on the error itself, and the
/// statements this endpoint runs were failing 100% of the time with nothing in the container log
/// to find them by. A failing arm is now logged.
async fn usage_or_zero(db: &PgPool, account_id: Uuid, feature_key: &str) -> i64 {
    match count_usage(db, account_id, feature_key).await {
        Ok(count) => count,
        Err(e) => {
            tracing::error!("me/usage: could not count {feature_key} for {account_id}: {e}");
            0
        }
    }
}

pub async fn get_usage_json(db: &PgPool, account_id: &str) -> serde_json::Value {
    let Ok(account_id) = account_uuid(account_id) else {
        tracing::error!("me/usage: the authenticated account id is not a uuid");
        return serde_json::json!({ "campaigns": 0, "leads": 0, "tags": 0 });
    };
    let campaigns = usage_or_zero(db, account_id, "max_campaigns").await;
    let leads = usage_or_zero(db, account_id, "max_leads").await;
    let tags = usage_or_zero(db, account_id, "max_tags").await;
    serde_json::json!({
        "campaigns": campaigns,
        "leads": leads,
        "tags": tags
    })
}

async fn count_usage(db: &PgPool, account_id: Uuid, feature_key: &str) -> Result<i64, AppError> {
    match feature_key {
        "max_campaigns" | "campaigns" => {
            // No `deleted_at` conjunct: `campaigns` has never had a soft-delete column. Measured —
            // this database's `information_schema.columns` holds ZERO columns named `deleted_at`
            // in ANY table, no migration ever adds one to `campaigns`, and this app retires a
            // campaign with a real `DELETE FROM campaigns` (src/db/campaigns.rs:278, and the admin
            // console's own delete button at www-admin/index.html:408). The conjunct was sister-app
            // bleed and it was the only place in the app that believed a campaign could be
            // soft-deleted; leaving it out changes nothing for a hard-deleted row, which is gone.
            let count: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM campaigns WHERE account_id = $1")
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
            // MOVED — not added, not dropped, not repointed at `tenant_id` (kanban t_04553fa6; the
            // card asked for one of `add account_id` / `read tenant_id` / `drop the key`).
            //
            // Why not the stub table: `leads` was created as a phantom stub by
            // `migrations/019_fix_phantom_tables.sql` (id, tenant_id, name, email, phone, status,
            // created_at) — no `account_id`, no FK, and measured this hour: ZERO rows and ZERO
            // writers. `grep -rn "FROM leads\|INTO leads\|UPDATE leads\|DELETE FROM leads" src/
            // migrations/` finds this statement and nothing else, so no fixture a user could create
            // can ever make `WHERE account_id = $1` true; and `leads.tenant_id` is not an account id
            // (`tenants` is a real, separate table with 1 row, `accounts` has 56), so reading
            // `tenant_id` instead would answer 0 for ever — the same silent zero, only with a quieter
            // error, which is exactly what this card exists to end. Adding an `account_id` + a writer
            // would invent a second lead store that duplicates `entries`.
            //
            // Why this statement: this app's lead IS an entry of a campaign it owns — that is its
            // lead capture (a giveaway/quiz entry carries name, email, phone). The app says so in
            // three live places, and this arm now shares one definition with all of them:
            //   * `GET /api/v1/leads` -> `dashboard_handler::list_leads`, documented "list all entries
            //     as leads" — the route the admin console's own Leads view renders
            //     (www-admin/index.html:1086-1108);
            //   * `business_handler`'s `total_leads`, twice: "Total leads = entries from campaigns
            //     owned by this account" (`SELECT COUNT(*) FROM entries e JOIN campaigns c ON
            //     c.id = e.campaign_id WHERE c.account_id = $1`) — the identical statement.
            // `plans.max_leads` is real and per plan (live: Free 5, Pro 100, Enterprise -1 = unlimited),
            // so this arm answers a real usage figure against a real allowance.
            let count: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM entries e JOIN campaigns c ON c.id = e.campaign_id WHERE c.account_id = $1",
            )
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

#[cfg(test)]
mod usage_arm_tests {
    //! RED/GREEN guard for `count_usage` and the plan-limit path built on it (kanban t_d23d413f).
    //!
    //! These arms run plain `sqlx::query_scalar` strings, so the compiler never sees the column
    //! names or the bind types they use — and every one of them was broken against the live
    //! schema. Two measured causes, both fixed in this module's commit:
    //!
    //! 1. `account_id` arrives as a string (the JWT `sub`) and was bound raw. sqlx sends a `&str`
    //!    as TEXT, so Postgres refused the comparison outright: `operator does not exist:
    //!    uuid = text` — the error the RED run of this test printed (`Database("error returned from
    //!    database: operator does not exist: uuid = text")`), which is what `count_usage` really
    //!    returned to both of its callers.
    //! 2. the `max_campaigns` arm additionally filtered on `campaigns.deleted_at`, a column this
    //!    schema has never had — measured, `information_schema.columns` holds zero columns named
    //!    `deleted_at` in ANY table of this database, and psql reports `column "deleted_at" does not
    //!    exist` for the same statement once the bind is typed correctly.
    //!
    //! `get_usage_json` swallowed either one into `unwrap_or(0)`, so `GET /api/v1/me/usage` answered
    //! `{"campaigns":0,"leads":0,"tags":0}` for every account, with nothing in the container log.
    //!
    //! DB-backed by construction — the defect only exists against a real schema — so the tests are
    //! OPT-IN and read-only: they run only when `INC_ARM_DB_TEST=1` **and** `DATABASE_URL` are both
    //! set, and they create nothing and delete nothing, so a plain `cargo test` can never touch
    //! whatever database `DATABASE_URL` happens to point at.
    //!
    //! Run them with:
    //!   INC_ARM_DB_TEST=1 \
    //!   DATABASE_URL="$(grep '^DATABASE_URL=' /etc/swift/env/incentiveswift.env | cut -d= -f2-)" \
    //!     cargo test --lib usage_arm_tests -- --nocapture
    use super::*;

    async fn pool_or_skip() -> Option<PgPool> {
        if std::env::var("INC_ARM_DB_TEST").as_deref() != Ok("1") {
            return None; // opt-in: a plain `cargo test` never opens a database
        }
        let url = std::env::var("DATABASE_URL").ok()?; // opt-in: no schema, nothing to prove
        Some(
            PgPool::connect(&url)
                .await
                .expect("connect to DATABASE_URL"),
        )
    }

    /// Read-only: whichever account the live schema says already owns a campaign.
    async fn account_with_campaigns(pool: &PgPool) -> Option<Uuid> {
        sqlx::query_scalar(
            "SELECT account_id FROM campaigns GROUP BY account_id HAVING COUNT(*) >= 1 LIMIT 1",
        )
        .fetch_optional(pool)
        .await
        .expect("pick a campaign-owning account")
    }

    #[tokio::test]
    async fn max_campaigns_arm_counts_instead_of_erroring() {
        let Some(pool) = pool_or_skip().await else {
            return;
        };
        let Some(acct) = account_with_campaigns(&pool).await else {
            eprintln!("SKIP: no campaign row in the database to count");
            return;
        };

        // RED before the fix: `Database("error returned from database: operator does not exist:
        // uuid = text")`.
        let usage = match count_usage(&pool, acct, "max_campaigns").await {
            Ok(n) => n,
            Err(e) => panic!("count_usage(max_campaigns) must answer with a count, got {e:?}"),
        };
        assert!(
            usage >= 1,
            "that account owns >= 1 campaign, so the arm must count >= 1, counted {usage}"
        );
        println!("max_campaigns arm counted {usage}");

        // ...and the same statement with the cap set to the measured usage must produce the arm's
        // OWN verdict, not a 500-shaped database error.
        match check_limit(&pool, acct, "max_campaigns", "Campaigns", usage).await {
            Err(AppError::UpgradeRequired(msg)) => {
                println!("arm verdict: {msg}");
                assert!(
                    msg.contains(&format!("({usage}/{usage})")),
                    "the arm must report the usage it measured, got: {msg}"
                );
            }
            other => panic!("check_limit must answer with its own verdict, got {other:?}"),
        }
    }

    /// The whole plan-limit path the card names: `enforce_feature_limit` -> `check_limit` ->
    /// `count_usage`, driven end to end against the live schema. Before the fix EVERY step of it
    /// answered a database error.
    #[tokio::test]
    async fn plan_limit_path_reaches_its_own_verdict() {
        let Some(pool) = pool_or_skip().await else {
            return;
        };
        let Some(acct) = account_with_campaigns(&pool).await else {
            eprintln!("SKIP: no campaign row in the database to count");
            return;
        };

        match enforce_feature_limit(&pool, &acct.to_string(), "max_campaigns", "Campaigns").await {
            // "no limit configured for that tier" — the function's own allow path.
            Ok(()) => println!("plan-limit path: allowed (no limit configured for that tier)"),
            // "limit reached (n/n)" / "not available on your plan" — check_limit's two arms.
            Err(AppError::UpgradeRequired(msg)) => println!("plan-limit path verdict: {msg}"),
            other => panic!("the plan-limit path must reach its own verdict, got {other:?}"),
        }
    }

    /// Read-only: whichever account the live schema says already owns >= 1 entry — this app's lead.
    /// Discovered with a SELECT; the test creates nothing.
    async fn account_with_leads(pool: &PgPool) -> Option<Uuid> {
        sqlx::query_scalar(
            "SELECT c.account_id FROM entries e JOIN campaigns c ON c.id = e.campaign_id
             GROUP BY c.account_id HAVING COUNT(*) >= 1 ORDER BY COUNT(*) DESC LIMIT 1",
        )
        .fetch_optional(pool)
        .await
        .expect("pick a lead-owning account")
    }

    /// Read-only: an account that owns no entry at all — the negative control.
    async fn account_without_leads(pool: &PgPool) -> Option<Uuid> {
        sqlx::query_scalar(
            "SELECT a.id FROM accounts a
             WHERE NOT EXISTS (SELECT 1 FROM entries e JOIN campaigns c ON c.id = e.campaign_id
                               WHERE c.account_id = a.id) LIMIT 1",
        )
        .fetch_optional(pool)
        .await
        .expect("pick a lead-free account")
    }

    /// The arm this card is about (kanban t_04553fa6).
    ///
    /// RED before the fix, printed by this test:
    /// `count_usage(max_leads) must answer with a count, got Database("error returned from database:
    /// column \"account_id\" does not exist")` — the arm counted a column of the phantom stub table
    /// `leads`, which has no `account_id`, **0 rows and 0 writers in the whole crate**. That is why
    /// `GET /api/v1/me/usage` answered a silent `"leads": 0` for every account while logging one
    /// `could not count max_leads` error per call: no fixture a user could ever create would have made
    /// the statement true.
    ///
    /// GREEN after: the arm answers the number the app's own lead definition gives — an entry of a
    /// campaign this account owns — so it agrees with `GET /api/v1/leads` (`list_leads`) and with
    /// `business_handler`'s `total_leads`, which run the identical statement.
    #[tokio::test]
    async fn leads_arm_counts_the_apps_leads() {
        let Some(pool) = pool_or_skip().await else {
            return;
        };
        let Some(acct) = account_with_leads(&pool).await else {
            eprintln!("SKIP: no entry row in the database to count");
            return;
        };

        let usage = match count_usage(&pool, acct, "max_leads").await {
            Ok(n) => n,
            Err(e) => panic!("count_usage(max_leads) must answer with a count, got {e:?}"),
        };

        // Cross-check with a DIFFERENT phrasing of the same predicate, so the test cannot pass by
        // sharing a mistake with the arm.
        let cross_check: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM entries WHERE campaign_id IN (SELECT id FROM campaigns WHERE account_id = $1)",
        )
        .bind(acct)
        .fetch_one(&pool)
        .await
        .expect("cross-check count");
        assert_eq!(
            usage, cross_check,
            "the arm must count this account's entries, arm={usage} cross-check={cross_check}"
        );
        assert!(
            usage >= 1,
            "the account owns >= 1 entry, arm counted {usage}"
        );

        // Falsification of the old home: the stub table still holds nothing, so a non-zero answer
        // cannot have come from it.
        let stub_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM leads")
            .fetch_one(&pool)
            .await
            .expect("count the stub table");
        assert_eq!(stub_rows, 0, "the `leads` stub is expected to stay empty");
        println!("leads arm counted {usage} (stub table `leads` still holds {stub_rows})");

        // Negative control: an account with no entries answers 0 WITHOUT an error — 0 is now a
        // measured fact, not a swallowed failure.
        if let Some(empty) = account_without_leads(&pool).await {
            match count_usage(&pool, empty, "max_leads").await {
                Ok(0) => {
                    println!("control: account {empty} owns no entry and the arm answered Ok(0)")
                }
                Ok(n) => panic!("control account owns no entry but the arm counted {n}"),
                Err(e) => panic!("control account must answer Ok(0), got {e:?}"),
            }
        }
    }
}
