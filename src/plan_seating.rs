//! `plan_seating` — no account is ever left without a named plan tier.
//!
//! WHY THIS EXISTS (measured 2026-11-14, cron run): this app's own live `accounts` table held EIGHT
//! rows and only FIVE carried a `plan_tier_id`. One of the bare rows is a REAL customer
//! (`xridedrivers@yahoo.com`, created 2026-11-10, `role = user`); the other two are the fleet's
//! standing tenancy canaries. Every surface that NAMES a level reads `accounts.plan_tier_id` joined
//! to `plan_tiers` (`/api/v1/auth/me`, the admin console's account list, the welcome mail's plan
//! line), so a bare account shows no plan at all: the operator cannot tell a customer what they are
//! on and the customer cannot either. Functionally the account is NOT broken —
//! `features::account_tier` deliberately reads a NULL tier as the base plan, so the limits are the
//! free ones — it is UNNAMED, which is the part the fleet rule forbids ("a level is always shown,
//! never a blank or a bare user").
//!
//! HOW A ROW GOT BARE: three doors INSERT an account and none of them wrote a tier —
//! `handlers::business_handler::register_business` (the cross-app directory provisioner),
//! `handlers::portfolio_sync_handler` (portfolio sync) and `billing::webhooks` (the provider
//! checkout's new-customer arm). The served signup and the tag door both go through
//! `account_mint::mint_account`, which DOES seat the entry tier, so those two were never affected.
//! All three doors now seat the entry tier in the same statement (one scalar sub-select, no extra
//! roundtrip), and this module is the REPAIR for the rows minted before that plus the belt for any
//! future door that lands a bare account.
//!
//! SAFETY: it only ever fills a NULL. An account that already names a tier — free, trial, paid,
//! cancelled — is untouched by the `WHERE plan_tier_id IS NULL` predicate, so nothing a customer
//! bought can be overwritten, and a re-run (every boot) writes zero rows.

use sqlx::PgPool;

/// The tier a PORTFOLIO company's workspace must hold: this app's OWN top tier — the highest
/// `price_monthly` among active `plan_tiers`, the same catalogue every level read joins.
///
/// WHY (measured live 2026-11-15): David's standing rule is that his own businesses hold the top
/// plan in EVERY app. The portfolio-sync door is the only place these workspaces are written, and it
/// seated each new mirror on the FREE tier, so two of the five portfolio companies — BV Premiere
/// Products (created that morning) and Xride Drivers — were served the free ceiling while Giraudy
/// Capital, SwiftImpact Solutions and ZaarHub held Enterprise. Nothing in the app was broken; the
/// owners' own businesses were simply capped by an INSERT default.
///
/// WHY A REPAIR AND NOT ONLY THE SYNC FIX: the door only runs when the hub broadcasts, and an
/// existing account's tier was never rewritten by a re-sync. Rows already seated on the wrong tier
/// need this boot pass to move, exactly as `SEAT_STATEMENT` repairs rows minted before its own fix.
///
/// SAFETY: the population is `portfolio_companies` alone — the table the hub writes — so a real
/// customer's account can never be reached by it. A portfolio workspace already on the top tier is
/// left alone (the `IS DISTINCT FROM` predicate writes zero rows), which makes a re-run, every boot,
/// a no-op.
const SEAT_PORTFOLIO_TOP_STATEMENT: &str = "\
UPDATE accounts \
   SET plan_tier_id = (SELECT id FROM plan_tiers \
                        WHERE is_active = true \
                        ORDER BY price_monthly DESC NULLS LAST, sort_order DESC NULLS LAST, name \
                        LIMIT 1), \
       updated_at = NOW() \
 WHERE id IN (SELECT account_id FROM portfolio_companies WHERE account_id IS NOT NULL) \
   AND plan_tier_id IS DISTINCT FROM (SELECT id FROM plan_tiers \
                                       WHERE is_active = true \
                                       ORDER BY price_monthly DESC NULLS LAST, sort_order DESC NULLS LAST, name \
                                       LIMIT 1)";

/// The entry tier this app seats a bare account on: the SAME predicate `account_mint::ENTRY_TIER_SQL`
/// uses for the served signup (`slug = 'free'`, active, zero price), so the repair and the signup
/// door can never disagree about what "the free plan" is.
///
/// Written as ONE literal statement — no `format!` and no concatenation — because the deploy gate
/// refuses a query assembled from strings (parameterised/literal SQL only).
const SEAT_STATEMENT: &str = "\
UPDATE accounts \
   SET plan_tier_id = (SELECT id FROM plan_tiers \
                        WHERE slug = 'free' AND is_active = true AND price_monthly = 0 LIMIT 1), \
       updated_at = NOW() \
 WHERE plan_tier_id IS NULL \
   AND EXISTS (SELECT 1 FROM plan_tiers \
                WHERE slug = 'free' AND is_active = true AND price_monthly = 0)";

/// Seat every account that names no tier on this app's free tier.
///
/// Returns how many rows were written. **Zero is the normal answer** on a database whose accounts
/// all came through a door that seats a tier; a non-zero answer means bare accounts existed and are
/// now named. Never panics on a mid-boot error: the caller logs it and the app still serves.
pub async fn seat_unseated_accounts(pool: &PgPool) -> Result<u64, sqlx::Error> {
    // The `EXISTS` guard keeps a misconfigured database (no active zero-price `free` tier) from
    // seating anyone on nothing: the sub-select would answer NULL and the UPDATE would be a no-op
    // anyway, but the guard makes that intent explicit instead of relying on NULL semantics.
    let written = sqlx::query(SEAT_STATEMENT).execute(pool).await?;
    Ok(written.rows_affected())
}

/// Seat every PORTFOLIO company's workspace on this app's top tier.
///
/// Returns how many rows were written. **Zero is the normal answer** once the portfolio workspaces
/// are already on the top tier; a non-zero answer means one or more of the owner's own businesses was
/// being served a lower ceiling and has now been moved up. Never panics mid-boot: the caller logs it.
pub async fn seat_portfolio_top_tier(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let written = sqlx::query(SEAT_PORTFOLIO_TOP_STATEMENT)
        .execute(pool)
        .await?;
    Ok(written.rows_affected())
}

/// Boot-time worker. A janitor must never take the app down, so the failure path is a log line.
pub fn spawn_plan_seater(pool: PgPool) {
    tokio::spawn(async move {
        match seat_unseated_accounts(&pool).await {
            Ok(0) => {}
            Ok(n) => tracing::info!(
                seated = n,
                "plan seating: accounts that carried no plan tier now name the free plan"
            ),
            Err(e) => tracing::warn!(error = %e, "plan seating failed (app keeps serving)"),
        }
        // The owner's own businesses hold the TOP plan in every app (2026-11-15). This runs after
        // the NULL seat so a bare portfolio row is named and then moved up in the same pass.
        match seat_portfolio_top_tier(&pool).await {
            Ok(0) => {}
            Ok(n) => tracing::info!(
                seated = n,
                "plan seating: portfolio workspaces moved onto this app's top tier"
            ),
            Err(e) => {
                tracing::warn!(error = %e, "portfolio plan seating failed (app keeps serving)")
            }
        }
    });
}
