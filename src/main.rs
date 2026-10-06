//! IncentiveSwift ??? Multi-tenant Engagement & Capture Engine
//!
// REST API server providing gamified incentive mechanics, raffle/giveaway system,
// long-form qualifier, and loyalty program module.

#![allow(unused_variables, dead_code)]
#![allow(clippy::too_many_arguments)]
#![allow(clippy::redundant_locals)]
#![allow(clippy::doc_lazy_continuation)]
#![allow(clippy::if_same_then_else)]
#![allow(clippy::collapsible_match)]
#![allow(clippy::needless_borrows_for_generic_args)]
#![allow(clippy::type_complexity)]
#![allow(clippy::incompatible_msrv)]
#![allow(non_snake_case)]
mod email;
mod email_provider;
mod email_queue;
mod lifecycle_emails;

pub mod access;
pub mod billing;
mod body_deadline;
mod config;
mod db;
pub mod delivery;
mod error;
mod features;
pub mod handlers;
pub mod iqs_validation;
pub mod mechanics;
pub mod security;
mod smtp;
mod state;
mod template_render;
pub mod template_types;
mod theme;

use axum::{
    http::HeaderValue,
    middleware,
    routing::{delete, get, patch, post, put},
    Router,
};
use std::sync::Arc;
use tokio::signal;
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use tracing_subscriber::EnvFilter;

/// Build a CORS origin predicate from allowed origins list.
fn cors_allowed_origins(allowed: &[String]) -> tower_http::cors::AllowOrigin {
    use std::sync::Arc;
    let origins: Vec<Arc<str>> = allowed.iter().map(|s| Arc::from(s.as_str())).collect();
    tower_http::cors::AllowOrigin::predicate(
        move |origin: &HeaderValue, _parts: &axum::http::request::Parts| {
            origins.iter().any(|allowed| {
                if let Ok(origin_str) = origin.to_str() {
                    origin_str == allowed.as_ref()
                } else {
                    false
                }
            })
        },
    )
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(true)
        .with_thread_ids(true)
        .init();

    // ── Host-side applier mode ────────────────────────────────────────────────────────────────
    // The static marketing page + the three legal pages live on the HOST
    // (/opt/swift/nginx/www/incentiveswift/) and the server runs in a container with ZERO mounts
    // for them, so the request path never writes them (that attempt is what made
    // PUT /api/v1/admin/site answer 500 after its row had already committed — kanban t_3fb0d3d2).
    // Running the SAME binary on the host with this argument is what materializes them;
    // /opt/swift/bin/is-site-apply.sh drives it from cron (*/5). Checked BEFORE the config load,
    // the migrations/backfills and the listener — it needs DATABASE_URL and nothing else, and it
    // must never start a second API against the live port.
    //
    //   incentiveswift-api apply-site-settings            write only the files whose bytes changed
    //   incentiveswift-api apply-site-settings --check    render and report, write NOTHING
    //   incentiveswift-api apply-site-settings --emit DIR also drop the rendered bytes under DIR (a
    //                                                     read-only-in-SITE_ROOT comparison artifact)
    if std::env::args().nth(1).as_deref() == Some("apply-site-settings") {
        return apply_site_settings_mode().await;
    }

    // Load configuration
    let config = config::AppConfig::from_env()?;
    let config = Arc::new(config);

    // Build shared state (this also applies the migrations)
    let state = state::AppState::new(&config).await?;

    // At-rest guard, boot halves, for the payment_providers secret columns. The migration file
    // arms both CHECK constraints NOT VALID and is ledger-tracked, so it runs exactly once — this
    // call is what re-arms a constraint that went missing, seals any plaintext row that appears
    // afterwards, and validates the guard. Runs after the migrations, before the first request.
    billing::providers::seal_payment_provider_secrets(&state.db).await;

    // Same boot half for the system-mail credential (kanban t_a794cb09): both writers of
    // `admin_settings.email` seal before they store, and this is what seals a row that arrives
    // plaintext from a database restored out of an older dump — or from a writer added later.
    // A failure is logged, never fatal: a broken credential row must not stop the app booting.
    match email_provider::seal_legacy_config_secrets(&state.db).await {
        Ok(0) => {}
        Ok(n) => tracing::warn!(
            rows = n,
            "admin_settings.email: sealed legacy plaintext credential(s) at rest"
        ),
        Err(e) => tracing::error!(
            "admin_settings.email credential backfill failed (plaintext may remain at rest): {}",
            e
        ),
    }

    // Same boot half for the TENANT's own SMTP password (kanban t_123b886b). The bare
    // `tenant_settings.smtp_password` scalar is a LIVE credential — `delivery::sender::load_smtp_config`
    // hands it to lettre for every tenant-scoped send — and the tenant settings writer seals it. This
    // seals a row that arrives plaintext from a database restored out of an older dump, or from a
    // writer added later. Never fatal: a broken credential row must not stop the app booting.
    match delivery::sender::seal_legacy_tenant_smtp_passwords(&state.db).await {
        Ok(0) => {}
        Ok(n) => tracing::warn!(
            rows = n,
            "tenant_settings smtp_password: sealed legacy plaintext credential(s) at rest"
        ),
        Err(e) => tracing::error!(
            "tenant_settings smtp_password backfill failed (plaintext may remain at rest): {}",
            e
        ),
    }

    // Start background email ticker (flushes scheduled follow-ups/reminders)
    email_queue::spawn_email_ticker(state.clone());

    // Request-body read deadline (kanban t_af70c0ca). Printed here so the number an operator sees
    // at boot is the number the middleware enforces — both come from this one config field.
    tracing::info!(
        "Request body-read deadline: {}s on every route that reads a body, 408 above that (BODY_READ_DEADLINE_SECS overrides, clamped 5..=300)",
        config.body_read_deadline_secs
    );

    // Build router
    let app = Router::new()
        // Public routes
        .route("/api/v1/health", get(handlers::health::health_check))
        .route(
            "/api/v1/channels/inbound",
            post(handlers::sms_handler::channel_inbound_webhook),
        )
        .route(
            "/api/v1/campaigns/:slug",
            get(handlers::campaigns::get_campaign)
                .put(handlers::campaigns::update_campaign)
                .delete(handlers::campaigns::delete_campaign_by_id),
        )
        // The prize pool. `prize_draw` has always READ `config.prize_pool` and nothing ever WROTE
        // it, so every campaign answered 400 "Campaign has no prize_pool configured" and the loyalty
        // mechanic could not run. This is the writer (2026-10-01).
        .route(
            "/api/v1/campaigns/:slug/prize-pool",
            get(handlers::prize_pool_handler::get_prize_pool)
                .put(handlers::prize_pool_handler::set_prize_pool),
        )
        .route(
            "/api/v1/campaigns/subdomain/:t_slug",
            get(handlers::campaigns::get_campaigns_by_subdomain),
        )
        .route(
            "/api/v1/campaigns/test-webhook",
            post(handlers::entries::test_entry_webhook),
        )
        .route("/api/v1/entries", post(handlers::entries::create_entry))
        .route(
            "/api/v1/raffles/:slug/enter",
            post(handlers::raffles::enter_raffle),
        )
        // Spin wheel / prize draw routes
        .route(
            "/api/v1/campaigns/:slug/spin",
            post(handlers::spin_handler::spin),
        )
        .route(
            "/api/v1/campaigns/:slug/spin-status",
            get(handlers::spin_handler::spin_status),
        )
        // Score reveal
        .route(
            "/api/v1/campaigns/:slug/score-reveal",
            post(handlers::score_reveal_handler::score_reveal),
        )
        // Scratch card
        .route(
            "/api/v1/campaigns/:slug/scratch-card",
            post(handlers::scratch_handler::scratch),
        )
        // Mystery reveal
        .route(
            "/api/v1/campaigns/:slug/mystery",
            post(handlers::mystery_handler::mystery),
        )
        // Countdown (urgency gate)
        .route(
            "/api/v1/campaigns/:slug/countdown",
            get(handlers::countdown_handler::countdown_get)
                .post(handlers::countdown_handler::countdown_post),
        )
        // Poll (single-question vote + results)
        .route(
            "/api/v1/campaigns/:slug/poll",
            post(handlers::poll_handler::poll_vote),
        )
        .route(
            "/api/v1/campaigns/:slug/poll/results",
            get(handlers::poll_handler::poll_results),
        )
        // Chat funnel (conversational bubble quiz)
        .route(
            "/api/v1/campaigns/:slug/chat",
            post(handlers::chat_handler::chat),
        )
        // Long-form qualifier (logic-based scoring)
        .route(
            "/api/v1/campaigns/:slug/long-form-qualifier",
            post(handlers::long_form_qualifier_handler::long_form_qualifier),
        )
        .route("/api/v1/loyalty/checkin", post(handlers::loyalty::checkin))
        .route(
            "/api/v1/loyalty/online/visit",
            post(handlers::loyalty::online_visit),
        )
        .route(
            "/api/v1/loyalty/online/share",
            post(handlers::loyalty::online_share),
        )
        .route(
            "/api/v1/loyalty/online/referral-click",
            post(handlers::loyalty::referral_click),
        )
        .route(
            "/api/v1/loyalty/online/stats/:code",
            get(handlers::loyalty::online_stats),
        )
        // Viral campaign engine -- Phase 1 (public)
        .route(
            "/api/v1/earn/:channel_code",
            get(handlers::viral_handler::earn_click_through),
        )
        .route(
            "/api/v1/c/:campaign_slug",
            get(handlers::viral_handler::campaign_share_link),
        )
        // FunnelSwift tag provision webhook — auto-provision contacts (no auth, internal key)
        .route(
            "/api/v1/internal/tag-provision",
            post(handlers::tag_provision_handler::handle_tag_provision),
        )
        .route(
            "/api/v1/internal/portfolio-companies",
            post(handlers::portfolio_handler::internal_create_portfolio_company),
        )
        .route(
            "/api/v1/internal/portfolio-sync",
            post(handlers::portfolio_sync_handler::portfolio_sync_internal),
        )
        // Loyalty V2 — Purchase Verification & Vouchers (public)
        // POST /api/v1/loyalty/generate-pin and POST /api/v1/loyalty/issue-voucher were RETIRED
        // here (kanban t_b209d263). Neither took an `AuthenticatedUser` and neither carried an
        // account predicate, so an uncredentialed caller could mint a live voucher (with a
        // caller-chosen `discount_value`) or a pending `purchase_verifications` row against ANY
        // active campaign, for ANY contact id, subject to no role check.
        //   * issue-voucher had exactly ONE caller in the 8-app fleet — MultiDirectory's
        //     `src/handlers/tag_automation.rs::execute_voucher_action` — and that call path is
        //     retired: MD retired the IncentiveSwift loyalty integration on 2026-09-23
        //     (`service="incentiveswift"` is refused with 400), its `tag_rules` table has 0 rows
        //     ever, and the MD console's tag-rule form does not even offer `issue_voucher` as an
        //     action. Nothing was left to predicate against, and no role bypass was invented.
        //   * generate-pin had 0 callers anywhere and was the sole writer of
        //     `purchase_verifications` (0 rows ever).
        // 0 nginx hits for either path over 718k log lines. Both tables were empty before and
        // after (proof: /opt/swift/audits/t_b209d263/proof.py).
        // POST /api/v1/loyalty/verify-purchase was RETIRED here too (kanban t_7a16bf0b): the
        // AuthenticatedUser-guarded reader of `purchase_verifications`. It was correctly guarded
        // (it scoped the contact through `contact_tenants`), but with its only writer
        // `generate_pin` retired above it could never find a `pending` row — 404 for every caller
        // forever — and it was the sole caller of `loyalty_v2::issue_rotation_voucher` (also
        // retired). Nothing called the route: 0 nginx hits over 719k log lines, no served page,
        // no fleet app. Proof: /opt/swift/audits/t_7a16bf0b/proof.py.
        // Account-level loyalty routes (via auth)
        .route(
            "/api/v1/loyalty/referrals",
            get(handlers::loyalty_v2::get_referrals),
        )
        .route(
            "/api/v1/loyalty/referrals/create",
            post(handlers::loyalty_v2::account_create_referral),
        )
        .route(
            "/api/v1/loyalty/rewards",
            get(handlers::loyalty_v2::get_rewards),
        )
        // GET /api/v1/loyalty/vouchers, GET /api/v1/loyalty/my-vouchers/:contact_id and
        // POST /api/v1/loyalty/claim-voucher were RETIRED here (kanban t_30dfc98c), together with
        // their handlers (`loyalty_v2::get_vouchers` / `list_my_vouchers` / `claim_voucher` +
        // `ClaimVoucherRequest`) and the `vouchers` table itself. Their only producer was the
        // anonymous survey-response mint retired in t_3bde2e27; with it gone, no writer of a
        // `vouchers` row survives anywhere in the 8-app fleet (t_b209d263 retired `generate_pin`
        // /`issue_voucher`, t_7a16bf0b `verify_purchase`/`issue_rotation_voucher`), the table has
        // held 0 rows ever, and the only live caller of `GET /loyalty/vouchers` was the served
        // console's Vouchers tab (1 browser hit in 693,108 nginx lines, 0 fleet callers). Keeping
        // a mounted read surface nothing can ever populate only advertises a capability that does
        // not exist, so the whole surface goes and the table is dropped in
        // migrations/20261004_drop_vouchers.sql. Proof: /opt/swift/audits/t_30dfc98c/proof.py.
        // POST /api/v1/loyalty/expire-vouchers was RETIRED here (kanban t_5e244255): an anonymous
        // mutation on a public mount that expired EVERY active voucher for EVERY account, called by
        // no cron, no repo and no page (0 nginx hits). The fleet's expiry sweep is MultiDirectory's
        // own /api/v1/networks/<slug>/clear/expire (scripts/md-clear-expire.sh).
        // Purchase Verify (business scanner — auto-credit)
        .route(
            "/api/v1/loyalty/purchase/verify",
            post(handlers::loyalty_v2::purchase_verify),
        )
        // POST /api/v1/business/pledge and GET /api/v1/business/pledges/:business_id were RETIRED
        // here (kanban t_5e244255): the anonymous half of the never-wired MultiDirectory pledge
        // integration - 0 callers, 0 nginx hits, `business_pledges` 0 rows, and `business_id` is a
        // directory entity, so no account owner is derivable in this app. The live pledge flow is
        // the admin-guarded /api/v1/admin/pledges family the admin console already uses.
        // POST /api/v1/loyalty/redeem-reward was RETIRED here (kanban t_32c87f33): an anonymous,
        // unscoped mutation that took campaign_slug + reward_tier_id + contact_id, three ids the
        // caller does not own, then deducted that contact's points and wrote a reward row with NO
        // credential and NO treasury/float check. 0 callers in the 8-app fleet, 0 nginx hits ever,
        // no served page calls it, and it is the last remnant of a dead redemption family (its
        // sibling redemption path in loyalty_badges.rs went in t_5e244255, which was also the ONLY
        // writer of point_redemption_log — the rows the float/burn rule measures). A contact cannot
        // authenticate in this app at all, so there is no customer to scope it to; the live
        // redemption flow is the AuthenticatedUser-guarded /loyalty/rewards/:id/{approve,deny} the
        // admin console's Rewards/Ledger tabs already use.
        .route(
            "/api/v1/loyalty/rewards-earned/:contact_id",
            get(handlers::loyalty_v2::list_rewards_earned),
        )
        // The external loyalty surface (tag-contact / grant-credits / register-member /
        // external program lookup) was RETIRED here (kanban t_f76c9950): measured on the live app,
        // nothing called it, two of its four arms carried NO credential at all (an anonymous
        // caller could mint a `company_admin` account and enrol members), and its only issuance
        // path let any tenant mint a GLOBAL system_api_key. IncentiveSwift's host-to-host seam is
        // the `x-internal-key` /api/v1/internal/* family; MultiDirectory's loyalty is native.
        // Public program lookup for the QR landing page. The customer who scans
        // the QR has no account, so this one carries no auth and no API key.
        .route(
            "/api/v1/loyalty/public/program/:slug",
            get(handlers::loyalty::public_program),
        )
        // POST /api/v1/campaigns/external/survey-response was RETIRED here (kanban t_3bde2e27).
        // Measured on the deployed binary 98ab2c0012ed4396: the arm took NO credential, and an
        // anonymous caller who named a live `directory-*` campaign slug minted an ACTIVE $50
        // `restaurant_card` voucher, injected a `contacts` row from the caller-supplied email,
        // awarded 100 Zaarcash and fired the voucher webhook — repeat calls minted again
        // (unbounded). Same class as t_b209d263. Its only named caller (MultiDirectory, on
        // onboarding completion) retired the IncentiveSwift loyalty integration on 2026-09-23
        // (`service="incentiveswift"` answers 400; MD credits rewards natively now). Census:
        // 0 nginx hits over 692,556 access-log lines, 0 fleet callers, 0 served-root call sites.
        // This was also `vouchers`' LAST writer. Its three mounted readers were RETIRED and the
        // table dropped in kanban t_30dfc98c (see the note at the loyalty/vouchers mount above).
        // Business accounts (Phase 1: directory business integration)
        .route(
            "/api/v1/business/register",
            post(handlers::business_handler::register_business),
        )
        .route(
            "/api/v1/business/:business_id/stats",
            get(handlers::business_handler::get_business_stats),
        )
        // Campaign widget (embeddable for directory listings)
        .route(
            "/api/v1/campaigns/:slug/widget",
            get(handlers::business_handler::get_campaign_widget),
        )
        // Loyalty Plans — subscription tiers for business loyalty gating
        .route(
            "/api/v1/loyalty/plan/status",
            get(handlers::loyalty_plans::plan_status),
        )
        .route(
            "/api/v1/loyalty/plans",
            get(handlers::loyalty_plans::list_plans),
        )
        .route(
            "/api/v1/loyalty/subscribe",
            post(handlers::loyalty_plans::subscribe),
        )
        .route(
            "/api/v1/loyalty/webhook/stripe",
            post(handlers::stripe_webhook::stripe_webhook),
        )
        // Treasury / ledger
        .route(
            "/api/v1/admin/treasury/summary",
            get(handlers::treasury_handler::treasury_summary),
        )
        .route(
            "/api/v1/admin/treasury/businesses",
            get(handlers::treasury_handler::business_ledgers),
        )
        .route(
            "/api/v1/admin/treasury/issuance-log",
            get(handlers::treasury_handler::issuance_log),
        )
        .route(
            "/api/v1/admin/treasury/expire-points",
            post(handlers::point_expiry_handler::expire_points),
        )
        // B2B Supplier milestones
        .route(
            "/api/v1/loyalty/supplier/milestone",
            post(handlers::supplier_handler::record_milestone),
        )
        .route(
            "/api/v1/loyalty/supplier/milestones/:business_id",
            get(handlers::supplier_handler::get_milestones),
        )
        // Clearinghouse configuration
        .route(
            "/api/v1/admin/treasury/state",
            get(handlers::treasury_engine_handler::get_state),
        )
        .route(
            "/api/v1/admin/treasury/funding",
            get(handlers::treasury_engine_handler::list_funding)
                .post(handlers::treasury_engine_handler::record_funding),
        )
        .route(
            "/api/v1/admin/treasury/rule",
            axum::routing::put(handlers::treasury_engine_handler::set_rule),
        )
        .route(
            "/api/v1/admin/treasury/holds",
            get(handlers::treasury_engine_handler::list_holds),
        )
        .route(
            "/api/v1/admin/treasury/holds/:id/resolve",
            axum::routing::post(handlers::treasury_engine_handler::resolve_hold),
        )
        .route(
            "/api/v1/treasury/rules",
            get(handlers::treasury_engine_handler::get_business_rules),
        )
        .route(
            "/api/v1/admin/clearinghouse/config",
            get(handlers::clearinghouse_config_handler::get_treasury_config)
                .put(handlers::clearinghouse_config_handler::update_treasury_config),
        )
        .route(
            "/api/v1/admin/clearinghouse/caps",
            get(handlers::clearinghouse_config_handler::get_category_caps)
                .put(handlers::clearinghouse_config_handler::update_category_cap),
        )
        .route(
            "/api/v1/admin/clearinghouse/supplier-config",
            get(handlers::clearinghouse_config_handler::get_supplier_config),
        )
        .route(
            "/api/v1/admin/clearinghouse/supplier-config/:id",
            put(handlers::clearinghouse_config_handler::update_supplier_config),
        )
        // The anonymous loyalty Badge + Enrollment surface was RETIRED here (kanban t_5e244255):
        // GET /api/v1/loyalty/badge/business/:business_id, .../badge/supplier/:supplier_id,
        // .../badge/member/:contact_id, GET /api/v1/loyalty/badges/program/:program_slug and
        // POST /api/v1/loyalty/enroll + /unenroll. Measured live 2026-10-04: 0 callers in the
        // 8-app fleet (MultiDirectory's loyalty is native), 0 nginx hits, `loyalty_enrollments`
        // 0 rows - and with `enroll` gone nothing can ever write one, so every badge arm could
        // only ever have answered "not enrolled". The business/supplier ids are MultiDirectory
        // entities, so no account owner is derivable in THIS app; the sibling external surface
        // was retired the same way (t_f76c9950).
        // Loyalty QR endpoints (Phase 3)
        .route(
            "/api/v1/loyalty/member/:member_id/qr",
            get(handlers::loyalty_badges::get_member_qr),
        )
        .route(
            "/api/v1/loyalty/member/:member_id/qr/regenerate",
            post(handlers::loyalty_badges::regenerate_member_qr),
        )
        // POST /api/v1/loyalty/scan (an anonymous clearinghouse points award) and GET
        // /api/v1/loyalty/scans/business/:business_id were RETIRED here (kanban t_5e244255): 0
        // callers, `loyalty_scans` 0 rows, and the counter QR flow is POST /api/v1/loyalty/checkin
        // (www-app/loyalty-checkin.html) - never this arm. The authenticated member read stays.
        .route(
            "/api/v1/loyalty/scans/member/:member_id",
            get(handlers::loyalty_badges::get_member_scans),
        )
        // Loyalty Dashboard endpoints (Phase 5)
        .route(
            "/api/v1/loyalty/dashboard/member/:member_id",
            get(handlers::loyalty_badges::member_dashboard),
        )
        .route(
            "/api/v1/loyalty/dashboard/admin/:program_slug",
            get(handlers::loyalty_badges::admin_dashboard),
        )
        // The Integration Center was RETIRED here (kanban t_5e244255): POST /api/v1/integration/keys,
        // GET /api/v1/integration/keys/:owner_type/:owner_id, DELETE /api/v1/integration/keys/:key_id
        // and GET /api/v1/integration/services. It was an ANONYMOUS mint / list / REVOKE of API keys
        // for `owner_type=business|supplier` ids that are MultiDirectory entities - no owner is
        // derivable in this app, 0 callers, 0 nginx hits. The two pre-existing `api_keys` rows are
        // directory-owned and untouched; account-scoped key management stays at /api/v1/api-keys.
        // Credits system (used by MultiDirectory proxy)
        .route(
            "/api/v1/credits/balance",
            get(handlers::credits_handler::get_balance),
        )
        .route(
            "/api/v1/credits/history",
            get(handlers::credits_handler::get_history),
        )
        // Admin routes
        .route(
            "/api/v1/admin/pledges",
            get(handlers::loyalty_v2::list_pending_pledges),
        )
        .route(
            "/api/v1/admin/pledges/:id/review",
            post(handlers::loyalty_v2::review_pledge),
        )
        // Rotation Config API — RETIRED (kanban t_8e9d3a52). The five arms
        //   POST   /api/v1/admin/rotation-configs
        //   GET    /api/v1/admin/rotation-configs/:campaign_slug
        //   POST   /api/v1/admin/rotation-members
        //   GET    /api/v1/admin/rotation-members/:config_id
        //   DELETE /api/v1/admin/rotation-members/:config_id/:business_id
        // wrote `rotation_configs` / `rotation_group_members`, whose ONLY consumer was
        // `loyalty_v2::issue_rotation_voucher` (retired in t_7a16bf0b). Both tables have 0 rows
        // ever; nothing reads what they write. The arms ARE admin-guarded (security::auth::
        // admin_guard, path-based over /api/v1/admin/*) — contrary to the card, which called them
        // anonymous; measured live, anon=401 and company_admin=403. Operator token = LIVE handler:
        // POST rotation-configs 200 + row, GET 200, POST rotation-members **500 forever** (the ON
        // CONFLICT target has no unique constraint — a second, independent defect), GET 200.
        // A scope+wire arm would need the retired anonymous generator back and a credential a
        // contact cannot hold, so RETIRE. 0 nginx hits over 692,285 access-log lines; no served
        // call site; no fleet caller. Drop of the now-orphaned tables is a separate card.
        // Proof: /opt/swift/audits/t_8e9d3a52/proof.py.
        // Offers CRUD (admin endpoints)
        .route(
            "/api/v1/admin/offers",
            get(handlers::offers_handler::list_offers).post(handlers::offers_handler::create_offer),
        )
        .route(
            "/api/v1/admin/offers/:id",
            get(handlers::offers_handler::get_offer)
                .put(handlers::offers_handler::update_offer)
                .delete(handlers::offers_handler::delete_offer),
        )
        // Authenticated routes
        .route(
            "/api/v1/campaigns",
            get(handlers::campaigns::list_campaigns).post(handlers::campaigns::create_campaign),
        )
        .route("/api/v1/raffles/:slug/draw", post(handlers::raffles::draw))
        .route(
            "/api/v1/raffles/:slug/redraw",
            post(handlers::raffles::redraw),
        )
        .route(
            "/api/v1/loyalty/programs",
            get(handlers::loyalty::list_programs).post(handlers::loyalty::create_program),
        )
        .route(
            "/api/v1/loyalty/programs/:id",
            put(handlers::loyalty::update_program).delete(handlers::loyalty::delete_program),
        )
        .route(
            "/api/v1/loyalty/rewards/:id/approve",
            post(handlers::loyalty::approve_reward),
        )
        .route(
            "/api/v1/loyalty/rewards/:id/deny",
            post(handlers::loyalty::deny_reward),
        )
        .route(
            "/api/v1/loyalty/tiers",
            get(handlers::loyalty::list_tiers).post(handlers::loyalty::create_tier),
        )
        .route(
            "/api/v1/loyalty/tiers/:id",
            put(handlers::loyalty::update_tier).delete(handlers::loyalty::delete_tier),
        )
        .route(
            "/api/v1/loyalty/check-plan",
            get(handlers::loyalty::check_plan_loyalty),
        )
        // Secret code admin routes
        .route(
            "/api/v1/loyalty/secret-codes",
            get(handlers::secret_codes_handler::list_secret_codes)
                .post(handlers::secret_codes_handler::create_secret_code),
        )
        .route(
            "/api/v1/loyalty/secret-codes/:id",
            delete(handlers::secret_codes_handler::delete_secret_code),
        )
        .route(
            "/api/v1/loyalty/secret-codes/:id/toggle",
            post(handlers::secret_codes_handler::toggle_secret_code),
        )
        // Viral campaign engine -- Admin routes
        // POST /api/v1/campaigns/:slug/referral-codes (viral_handler::create_referral_code) was
        // RETIRED here (kanban t_9d983e50): it took no contact and no authenticated user, bound
        // NULL into campaign_referrals.referrer_contact_id (NOT NULL, no default) and therefore
        // answered 500 for every caller (live-reproduced), while no served surface ever called it.
        // The served referral contract is the account/loyalty pair
        // (GET|POST /api/v1/loyalty/referrals[/create]); see HANDOFF.md.
        .route(
            "/api/v1/campaigns/:slug/referral-stats",
            get(handlers::viral_handler::get_referral_stats),
        )
        .route(
            "/api/v1/campaigns/:slug/earn-channels",
            get(handlers::viral_handler::list_earn_channels)
                .post(handlers::viral_handler::create_earn_channel),
        )
        .route(
            "/api/v1/campaigns/:slug/earn-channels/:channel_id",
            patch(handlers::viral_handler::update_earn_channel)
                .delete(handlers::viral_handler::delete_earn_channel),
        )
        .route(
            "/api/v1/campaigns/:slug/earn/verify",
            post(handlers::viral_handler::verify_earn_action),
        )
        .route(
            "/api/v1/campaigns/:slug/leaderboard",
            get(handlers::viral_handler::campaign_leaderboard),
        )
        // Phase 2: Milestone Rewards (admin)
        .route(
            "/api/v1/campaigns/:slug/milestones",
            get(handlers::milestone_handler::list_milestones)
                .post(handlers::milestone_handler::create_milestone),
        )
        .route(
            "/api/v1/campaigns/:slug/milestones/achieved",
            get(handlers::milestone_handler::list_achieved_milestones),
        )
        .route(
            "/api/v1/campaigns/:slug/milestones/:milestone_id",
            put(handlers::milestone_handler::update_milestone)
                .delete(handlers::milestone_handler::delete_milestone),
        )
        // Campaign secret codes (promo-code style, type-to-redeem)
        .route(
            "/api/v1/campaigns/:campaign_id/secret-codes",
            get(handlers::campaign_secret_codes::list_secret_codes)
                .post(handlers::campaign_secret_codes::create_secret_code),
        )
        // NOTE: the unprefixed /api/v1/secret-codes, /:id and /:id/toggle aliases were removed
        // (dead-endpoint triage t_3db21b91): they bound the exact same secret_codes_handler
        // functions as the canonical /api/v1/loyalty/secret-codes family, had no caller in any
        // shipped surface, and two names for one behaviour only invites drift.
        .route(
            "/api/v1/campaigns/:campaign_id/secret-codes/:code_id",
            put(handlers::campaign_secret_codes::update_secret_code)
                .delete(handlers::campaign_secret_codes::delete_secret_code),
        )
        .route(
            "/api/v1/campaigns/:campaign_id/secret-codes/redemptions",
            get(handlers::campaign_secret_codes::list_redemptions),
        )
        .route(
            "/api/v1/campaigns/:campaign_id/redeem-code",
            post(handlers::campaign_secret_codes::redeem_secret_code),
        )
        // Verify uses the new loyalty_secret_codes table
        .route(
            "/api/v1/loyalty/secret-code/verify",
            post(handlers::secret_codes_handler::verify_secret_code),
        )
        .route(
            "/api/v1/loyalty/programs/:id/secret-code",
            put(handlers::loyalty::set_secret_code),
        )
        // GET /api/v1/loyalty/programs/:id/qr was RETIRED here (kanban t_5e244255): an anonymous QR
        // generator nothing called (0 nginx hits, no console, no page). It built the check-in URL
        // from the programme NAME, the same derivation the collision card t_25e9f950 covers; the
        // served landing page (/loyalty-checkin/<slug>) is unchanged.
        .route("/api/v1/delivery/resend", post(handlers::delivery::resend))
        // Leads list
        .route(
            "/api/v1/leads",
            get(handlers::dashboard_handler::list_leads),
        )
        .route(
            "/api/v1/contacts",
            get(handlers::contacts::list_contacts).post(handlers::contacts::create_contact),
        )
        .route(
            "/api/v1/contacts/:id",
            get(handlers::contacts::get_contact)
                .put(handlers::contacts::update_contact)
                .delete(handlers::contacts::delete_contact),
        )
        // Tags — the tenant's own tag library. The served Operator Console has shipped a `Tags`
        // screen with create/edit/delete all along (`www-admin/index.html` nav id `tags`, view
        // `Tags` + `TagModal`), which POSTs/PUTs/DELETEs these paths, but only the GET existed here
        // (POST answered 405, PUT/DELETE 404) and `tags` had no writer anywhere in the crate — so
        // the screen could only ever render its 85 backfilled rows. The write verbs below are the
        // producer that screen was always calling; creation is gated on the account's own
        // `max_tags` allowance (kanban t_286aead1).
        .route(
            "/api/v1/tags",
            get(handlers::tags_handler::list_tags).post(handlers::tags_handler::create_tag),
        )
        .route(
            "/api/v1/tags/:id",
            put(handlers::tags_handler::update_tag).delete(handlers::tags_handler::delete_tag),
        )
        .route(
            "/api/v1/portfolio-companies",
            get(handlers::portfolio_handler::list_portfolio_companies)
                .post(handlers::portfolio_handler::create_portfolio_company),
        )
        .route(
            "/api/v1/portfolio-companies/:id",
            get(handlers::portfolio_handler::get_portfolio_company)
                .put(handlers::portfolio_handler::update_portfolio_company)
                .delete(handlers::portfolio_handler::delete_portfolio_company),
        )
        // Support / Tickets
        .route(
            "/api/v1/support-tickets",
            get(handlers::support_tickets::list_tickets)
                .post(handlers::support_tickets::create_ticket),
        )
        .route(
            "/api/v1/support-tickets/:id",
            get(handlers::support_tickets::get_ticket)
                .put(handlers::support_tickets::update_ticket)
                .delete(handlers::support_tickets::delete_ticket),
        )
        .route(
            "/api/v1/support-tickets/:id/messages",
            post(handlers::support_tickets::add_message),
        )
        // Reviews & Ratings
        .route(
            "/api/v1/reviews",
            get(handlers::reviews::list_reviews).post(handlers::reviews::create_review),
        )
        .route(
            "/api/v1/reviews/:id",
            put(handlers::reviews::update_review).delete(handlers::reviews::delete_review),
        )
        // Calendar Events
        .route(
            "/api/v1/calendar-events",
            get(handlers::calendar_events::list_events)
                .post(handlers::calendar_events::create_event),
        )
        .route(
            "/api/v1/calendar-events/:id",
            put(handlers::calendar_events::update_event)
                .delete(handlers::calendar_events::delete_event),
        )
        .route(
            "/api/v1/integration-targets",
            get(handlers::integration_target_handler::list_integration_targets)
                .post(handlers::integration_target_handler::create_integration_target),
        )
        .route(
            "/api/v1/integration-targets/:id",
            put(handlers::integration_target_handler::update_integration_target)
                .delete(handlers::integration_target_handler::delete_integration_target),
        )
        // Auth endpoints
        .route(
            "/api/v1/auth/register",
            post(crate::handlers::auth_handler::register),
        )
        .route(
            "/api/v1/auth/login",
            post(crate::handlers::auth_handler::login),
        )
        .route("/api/v1/auth/me", get(crate::handlers::auth_handler::me))
        .route(
            "/api/v1/me/usage",
            get(crate::handlers::auth_handler::get_usage),
        )
        .route(
            "/api/v1/auth/profile",
            put(crate::handlers::auth_handler::update_profile),
        )
        .route(
            "/api/v1/auth/password",
            put(crate::handlers::auth_handler::change_password),
        )
        .route(
            "/api/v1/auth/forgot-password",
            post(crate::handlers::auth_handler::forgot_password),
        )
        .route(
            "/api/v1/auth/reset-password",
            post(crate::handlers::auth_handler::reset_password),
        )
        // Admin endpoints (cross-app portfolio sync + impersonation)
        .route(
            "/api/v1/admin/portfolio-sync",
            post(crate::handlers::admin_handler::portfolio_sync),
        )
        .route(
            "/api/v1/admin/impersonate",
            post(crate::handlers::admin_handler::impersonate),
        )
        .route(
            "/api/v1/admin/stop-impersonation",
            post(crate::handlers::admin_handler::stop_impersonation),
        )
        .route(
            "/api/v1/admin/allcampaigns",
            get(crate::handlers::admin_handler::admin_list_all_campaigns),
        )
        .route(
            "/api/v1/admin/tenants",
            get(crate::handlers::admin_handler::list_all_tenants),
        )
        .route(
            "/api/v1/admin/tenants/:id",
            delete(crate::handlers::admin_handler::delete_tenant),
        )
        .route(
            "/api/v1/admin/tenants/:tenant_id/credits-rate",
            get(crate::handlers::admin_handler::get_credit_rate)
                .put(crate::handlers::admin_handler::update_credit_rate),
        )
        .route(
            "/api/v1/admin/tenants/:tenant_id/purchase-pin",
            get(crate::handlers::admin_handler::get_purchase_pin),
        )
        // The credits READ surface. The served admin guide has documented GET /api/v1/admin/credits
        // since the guide was written ("View all tenant credits (admin)") and no route was ever
        // mounted, so an operator had to sign in AS an account to see its balance (kanban
        // t_24b17131). The console's "18 · Credits (all accounts)" panel is its caller.
        .route(
            "/api/v1/admin/credits",
            get(handlers::credits_handler::admin_list_credits),
        )
        // Operator control for the credits ledger. The handler existed but no route
        // was ever mounted, so the admin console could not reach it (kanban t_0fc42946).
        // Wired here AND given a caller: the "Adjust credits" action on the served admin
        // console's Tenants panel (www-admin/index.html, OPS_PANELS).
        .route(
            "/api/v1/admin/credits/adjust",
            post(handlers::credits_handler::admin_adjust_credits),
        )
        // The email queue's operator surface (kanban t_9d711589). The ticker flushed only
        // 'pending' and recorded a failure in `last_error` alone, so 18 dead rows sat invisible
        // from 2026-09-20. READ-ONLY: counts per status + the dead letters with their reason.
        // The console's "22 · Email queue (dead letters)" panel is its caller.
        .route(
            "/api/v1/admin/email-queue",
            get(handlers::admin_handler::email_queue),
        )
        // Phase 1: Business account management
        .route(
            "/api/v1/admin/businesses",
            get(handlers::business_handler::admin_list_businesses),
        )
        .route(
            "/api/v1/admin/businesses/:business_id",
            get(handlers::business_handler::admin_get_business),
        )
        .route(
            "/api/v1/admin/businesses/:business_id/rotate-key",
            post(handlers::business_handler::admin_rotate_business_key),
        )
        // Admin plan management
        // Public plans listing
        .route(
            "/api/v1/plans",
            get(handlers::dashboard_handler::list_public_plans),
        )
        .route(
            "/api/v1/admin/plans",
            get(crate::handlers::plans_handler::list_plans)
                .post(crate::handlers::plans_handler::create_plan),
        )
        .route(
            "/api/v1/admin/plans/assign",
            post(crate::handlers::plans_handler::admin_assign_plan),
        )
        .route(
            "/api/v1/admin/plans/:id",
            get(crate::handlers::plans_handler::get_plan)
                .put(crate::handlers::plans_handler::update_plan)
                .delete(crate::handlers::plans_handler::delete_plan),
        )
        .route(
            "/api/v1/admin/plans/:id/features",
            put(crate::handlers::plans_handler::admin_update_plan_features)
                .post(crate::handlers::tier_handler::post_plan_features),
        )
        // Plan tier feature management (canonical tier_features CRUD)
        .route(
            "/api/v1/admin/tiers",
            get(crate::handlers::tier_handler::list_tiers),
        )
        .route(
            "/api/v1/admin/tiers/:tier_id",
            put(crate::handlers::tier_handler::update_tier),
        )
        .route(
            "/api/v1/admin/tiers/:tier_id/features",
            get(crate::handlers::tier_handler::get_tier_features),
        )
        .route(
            "/api/v1/admin/tiers/:tier_id/features/:feature_key",
            put(crate::handlers::tier_handler::update_tier_feature),
        )
        // Industry routes
        .route(
            "/api/v1/industries",
            get(crate::handlers::industries_handler::list_active_industries),
        )
        .route(
            "/api/v1/admin/industries",
            get(crate::handlers::industries_handler::admin_list_industries)
                .post(crate::handlers::industries_handler::admin_create_industry),
        )
        .route(
            "/api/v1/admin/industries/:id",
            put(crate::handlers::industries_handler::admin_update_industry)
                .delete(crate::handlers::industries_handler::admin_delete_industry),
        )
        .route(
            "/api/v1/api-keys",
            get(handlers::api_keys::list_api_keys).post(handlers::api_keys::create_api_key),
        )
        // Public by contract: sibling services (Multi-Directory's "Connect
        // IncentiveSwift" flow) verify a pasted key here. Answers 200 {"valid": ...}
        // for any well-formed request — see handlers::api_keys::verify_api_key.
        .route(
            "/api/v1/api-keys/verify",
            post(handlers::api_keys::verify_api_key),
        )
        .route(
            "/api/v1/api-keys/:id",
            put(handlers::api_keys::update_api_key).delete(handlers::api_keys::delete_api_key),
        )
        // Surface routes (public ??? no auth required)
        .route(
            "/api/v1/widget/:hash",
            get(handlers::surface_handler::get_widget_js),
        )
        .route(
            "/api/v1/widget/:hash/config",
            get(handlers::surface_handler::get_widget_config),
        )
        // Widget embed snippets: the producer for `widget_snippets`. Before this pair
        // existed nothing on the box could insert that row, so GET /api/v1/widget/:hash
        // could only ever serve hand-made rows and the embed route handed customers a
        // script URL that 404s (kanban t_e3a33d15).
        .route(
            "/api/v1/campaigns/:slug/widget-snippet",
            post(handlers::surface_handler::create_widget_snippet)
                .delete(handlers::surface_handler::disable_widget_snippet),
        )
        .route(
            "/api/v1/dashboard/stats",
            get(handlers::dashboard_handler::dashboard_stats),
        )
        // Dashboard recent activity feed
        .route(
            "/api/v1/dashboard/activity",
            get(handlers::dashboard_handler::dashboard_activity),
        )
        .route(
            "/api/v1/play/:id",
            get(handlers::surface_handler::get_play_view),
        )
        .route(
            "/api/v1/play/:id/dashboard",
            get(handlers::surface_handler::get_loyalty_dashboard),
        )
        .route(
            "/api/v1/embed/campaign/all",
            get(handlers::surface_handler::get_embed_campaign_list),
        )
        .route(
            "/api/v1/embed/campaign/:slug",
            get(handlers::surface_handler::get_campaign_embed),
        )
        .route(
            "/api/v1/embed/:id",
            get(handlers::surface_handler::get_embed_view),
        )
        // Surface routes (admin ??? protected by auth middleware)
        // Quiz/Trivia question CRUD + submission
        .route(
            "/api/v1/campaigns/:slug/questions",
            get(handlers::quiz_handler::list_campaign_questions)
                .post(handlers::quiz_handler::create_question),
        )
        .route(
            "/api/v1/campaigns/:slug/questions/:question_id",
            put(handlers::quiz_handler::update_question)
                .delete(handlers::quiz_handler::delete_question),
        )
        .route(
            "/api/v1/play/:campaign_id/questions",
            get(handlers::quiz_handler::play_campaign_questions),
        )
        .route(
            "/api/v1/quiz/:campaign_id/submit",
            post(handlers::quiz_handler::submit_quiz),
        )
        .route(
            "/api/v1/admin/campaigns/:id/surface",
            get(handlers::surface_handler::get_surface_config)
                .put(handlers::surface_handler::update_surface_config),
        )
        .route(
            "/api/v1/admin/domains",
            get(handlers::surface_handler::list_domains)
                .post(handlers::surface_handler::register_domain),
        )
        .route(
            "/api/v1/admin/domains/:id",
            delete(handlers::surface_handler::remove_domain),
        )
        .route(
            "/api/v1/admin/domains/:id/verify",
            post(handlers::surface_handler::verify_domain),
        )
        .route(
            "/api/v1/admin/plans/:id/domains",
            get(handlers::surface_handler::check_plan_domains),
        )
        // Site configuration (SEO, tracking, legal pages, homepage)
        .route(
            "/api/v1/admin/site",
            get(handlers::site_handler::get_site).put(handlers::site_handler::update_site),
        )
        // Surfaces REST CRUD routes
        .route(
            "/api/v1/surfaces",
            get(handlers::surfaces_handler::list).post(handlers::surfaces_handler::create),
        )
        .route(
            "/api/v1/surfaces/:id",
            get(handlers::surfaces_handler::get)
                .put(handlers::surfaces_handler::update)
                .delete(handlers::surfaces_handler::delete),
        )
        // Provider Keys routes
        .route(
            "/api/v1/admin/email-settings",
            get(handlers::email_settings_handler::get_email_settings)
                .put(handlers::email_settings_handler::update_email_settings),
        )
        .route(
            "/api/v1/admin/email-settings/test",
            post(handlers::email_settings_handler::test_email_settings),
        )
        .route(
            "/api/v1/provider-keys",
            get(handlers::provider_keys_handler::list_provider_keys)
                .post(handlers::provider_keys_handler::upsert_provider_key),
        )
        .route(
            "/api/v1/provider-keys/:provider",
            delete(handlers::provider_keys_handler::delete_provider_key),
        )
        .route(
            "/api/v1/available-providers",
            get(handlers::provider_keys_handler::list_available_providers),
        )
        .route(
            "/api/v1/integrations/coreswift/lists",
            get(handlers::provider_keys_handler::coreswift_lists),
        )
        .route(
            "/api/v1/integrations/coreswift/status",
            get(handlers::provider_keys_handler::coreswift_status),
        )
        .route(
            "/api/v1/integrations/coreswift/push",
            post(handlers::provider_keys_handler::coreswift_push),
        )
        .route(
            "/api/v1/provider-keys/:provider/test",
            post(handlers::provider_keys_handler::test_provider_key),
        )
        // Payment provider, checkout & webhook routes (via billing module)
        .merge(billing::router(state.clone()))
        // Campaign Integration Hub routes
        .route(
            "/api/v1/campaigns/:slug/integrations",
            get(handlers::campaign_integrations::list_campaign_integrations)
                .post(handlers::campaign_integrations::link_campaign_integration),
        )
        .route(
            "/api/v1/campaigns/:slug/integrations/:integration_id",
            delete(handlers::campaign_integrations::unlink_campaign_integration),
        )
        // Marketing Boost -- per-campaign webhook for external marketing systems
        .route(
            "/api/v1/campaigns/:slug/marketing-boost",
            get(handlers::campaign_integrations::get_marketing_boost)
                .put(handlers::campaign_integrations::set_marketing_boost),
        )
        // Marketing Boost destinations list (cached, requires auth)
        .route(
            "/api/v1/marketing-boost/destinations",
            get(handlers::marketing_boost_handler::get_destinations),
        )
        // Campaign wins / admin routes
        .route(
            "/api/v1/campaigns/:slug/clone",
            post(handlers::campaigns::clone_campaign),
        )
        .route(
            "/api/v1/campaigns/:slug/wins",
            get(handlers::spin_handler::list_wins),
        )
        .route(
            "/api/v1/campaigns/:slug/wins/:win_id/redeem",
            post(handlers::spin_handler::redeem_win),
        )
        // Custom fields routes
        .route(
            "/api/v1/campaigns/:slug/custom-fields",
            get(handlers::custom_fields_handler::list_custom_fields)
                .post(handlers::custom_fields_handler::create_custom_field),
        )
        .route(
            "/api/v1/campaigns/:slug/custom-fields/reorder",
            put(handlers::custom_fields_handler::reorder_custom_fields),
        )
        .route(
            "/api/v1/campaigns/:slug/custom-fields/:field_id",
            delete(handlers::custom_fields_handler::delete_custom_field)
                .put(handlers::custom_fields_handler::update_custom_field),
        )
        // Email Templates routes
        .route(
            "/api/v1/email-templates/merge-fields",
            get(handlers::email_templates_handler::merge_fields),
        )
        // The canonical SENDABLE type vocabulary the console's Type picker renders and the write
        // path validates against (kanban t_0eed3151). Static segment, so it outranks `:id`.
        .route(
            "/api/v1/email-templates/types",
            get(handlers::email_templates_handler::types),
        )
        .route(
            "/api/v1/email-templates",
            get(handlers::email_templates_handler::list)
                .post(handlers::email_templates_handler::create),
        )
        // kanban t_4f20bd5e: the `get(..)` arm was REMOVED — GET /api/v1/email-templates/:id had no
        // caller in any served surface (the console's Edit modal is prefilled from the row the LIST
        // already returned, with the same 10-column SELECT list) and was the family's only unscoped
        // read (`WHERE id = $1` with an unused caller). PUT/DELETE keep the path registered.
        .route(
            "/api/v1/email-templates/:id",
            put(handlers::email_templates_handler::update)
                .delete(handlers::email_templates_handler::delete),
        )
        // Settings routes
        .route(
            "/api/v1/settings",
            get(handlers::settings_handler::get_settings)
                .put(handlers::settings_handler::update_settings),
        )
        // The TENANT's own mail server, tested for real — the shipped console's Settings → Email
        // pane drives this (kanban t_ba200ddf). Rate-limited like every other authenticated route;
        // the recipient is the caller's own address, never a body field.
        .route(
            "/api/v1/settings/email/test",
            post(handlers::settings_handler::test_settings_email),
        )
        // REMOVE the caller's own mail server, so the account goes back to the platform mail
        // service (kanban t_2e9117a5). Same pane, same family: the DELETE arm the PUT writer and
        // the blank-value refusal leave as the only way out of a saved server.
        .route(
            "/api/v1/settings/email",
            delete(handlers::settings_handler::delete_settings_mail),
        )
        // Analytics routes
        .route(
            "/api/v1/analytics/overview",
            get(handlers::analytics_handler::overview),
        )
        .route(
            "/api/v1/analytics/campaigns",
            get(handlers::analytics_handler::campaign_list),
        )
        .route(
            "/api/v1/analytics/campaigns/:slug",
            get(handlers::analytics_handler::campaign_detail),
        )
        .route(
            "/api/v1/analytics/contacts",
            get(handlers::analytics_handler::contacts_analytics),
        )
        .route(
            "/api/v1/analytics/loyalty",
            get(handlers::analytics_handler::loyalty_analytics),
        )
        .route(
            "/api/v1/analytics/export",
            get(handlers::analytics_handler::export_csv),
        )
        .route(
            "/api/v1/analytics/import",
            post(handlers::analytics_handler::import_csv),
        )
        // ── KNOWLEDGE BASE (David 2026-10-02: an admin side and a user side, "in line respectively") ──
        // The user side is public on purpose: the people being walked through the product are often
        // not signed in (a player on a kiosk, a link follow). The admin side requires a caller, and
        // the authoring routes sit under /api/v1/admin/* so admin_guard covers them automatically.
        // Call logs and deal tracking — David's spec requires both; the tables existed with zero code
        // references, so these wire what was already there (see handlers/tracking_handler.rs).
        .route(
            "/api/v1/call-logs",
            get(handlers::tracking_handler::list_calls)
                .post(handlers::tracking_handler::create_call),
        )
        .route(
            "/api/v1/call-logs/:id",
            put(handlers::tracking_handler::update_call)
                .delete(handlers::tracking_handler::delete_call),
        )
        .route(
            "/api/v1/deals",
            get(handlers::tracking_handler::list_deals)
                .post(handlers::tracking_handler::create_deal),
        )
        .route(
            "/api/v1/deals/:id",
            put(handlers::tracking_handler::update_deal)
                .delete(handlers::tracking_handler::delete_deal),
        )
        .route(
            "/api/v1/knowledge-base",
            get(handlers::knowledge_base_handler::list_articles),
        )
        .route(
            "/api/v1/knowledge-base/article/:slug",
            get(handlers::knowledge_base_handler::get_article),
        )
        // Authoring sits at /api/v1/knowledge-base (NOT under /api/v1/admin/*): every account holder
        // signs into this console and owns their own help content, whereas admin_guard would reserve it
        // for the platform operator alone. Tenancy is enforced in the handler
        // (owner_for_new / authorise_edit): an operator writes the SHIPPED set, everyone else writes
        // their own rows, and neither may touch the other's.
        .route(
            "/api/v1/knowledge-base",
            post(handlers::knowledge_base_handler::create_article),
        )
        .route(
            "/api/v1/knowledge-base/:id",
            put(handlers::knowledge_base_handler::update_article)
                .delete(handlers::knowledge_base_handler::delete_article),
        )
        // ------------------------------------------------------------------
        // IQS — Intelligent Qualifying Surveys
        // ------------------------------------------------------------------
        // Public play endpoints (no auth)
        .route(
            "/api/v1/iqs/play/:slug",
            get(handlers::iqs_handler::get_play_funnel),
        )
        .route(
            "/api/v1/iqs/play/:slug/submit",
            post(handlers::iqs_handler::submit_funnel),
        )
        // Campaign-attached IQS questions (for gating a campaign entry)
        .route(
            "/api/v1/campaigns/:slug/iqs-questions",
            get(handlers::iqs_handler::get_campaign_iqs_questions),
        )
        // Authenticated funnel CRUD
        .route(
            "/api/v1/iqs/funnels",
            get(handlers::iqs_handler::list_funnels).post(handlers::iqs_handler::create_funnel),
        )
        .route(
            "/api/v1/iqs/funnels/:id",
            get(handlers::iqs_handler::get_funnel)
                .put(handlers::iqs_handler::update_funnel)
                .delete(handlers::iqs_handler::delete_funnel),
        )
        // Questions
        .route(
            "/api/v1/iqs/funnels/:id/questions",
            get(handlers::iqs_handler::list_questions).post(handlers::iqs_handler::create_question),
        )
        .route(
            "/api/v1/iqs/funnels/:id/questions/reorder",
            post(handlers::iqs_handler::reorder_questions),
        )
        .route(
            "/api/v1/iqs/funnels/:fid/questions/:qid",
            put(handlers::iqs_handler::update_question)
                .delete(handlers::iqs_handler::delete_question),
        )
        // Rules (conditional branching / classification)
        .route(
            "/api/v1/iqs/funnels/:id/rules",
            get(handlers::iqs_handler::list_rules).post(handlers::iqs_handler::create_rule),
        )
        .route(
            "/api/v1/iqs/funnels/:fid/rules/:rid",
            put(handlers::iqs_handler::update_rule).delete(handlers::iqs_handler::delete_rule),
        )
        // Submissions
        .route(
            "/api/v1/iqs/funnels/:id/submissions",
            get(handlers::iqs_handler::list_submissions),
        )
        // File upload (image-choice question assets)
        .route(
            "/api/v1/iqs/upload",
            post(handlers::iqs_handler::upload_file),
        )
        // Request-body read deadline (kanban t_af70c0ca). Mounted as the FIRST `.layer()` of this
        // chain, i.e. INNERMOST — in axum the first `.layer()` applied is the one closest to the
        // handlers — so `admin_guard` and the security headers stay OUTSIDE it: an unauthenticated
        // /api/v1/admin/* request answers 401 at once instead of ever waiting for a body, and a
        // declared body is never buffered before the credential is checked.
        //
        // Its own scope is method + declared body (see `body_deadline::declares_a_body`), so the
        // ~700 GET-only routes on this one flat router are handed to their handlers untouched;
        // that the scope is complete is measured, not asserted: every handler registered with
        // `get(...)` takes no body extractor.
        //
        // This REPLACES the accidental whole-request `.layer(TimeoutLayer::new(Duration::from_secs(30)))`
        // that used to sit below: that answered 408 for any request still in flight at 30 s,
        // including one whose body had arrived instantly and whose handler was legitimately still
        // working (measured live: a row-lock-held POST /api/v1/loyalty/checkin was 408'd and its
        // work dropped at t+30.0 s; on this binary the same request finishes). The resource bound
        // is the same 30 s — only a body that has STOPPED arriving is affected now — and every
        // outbound HTTP call in this app already carries its own client timeout (10-20 s).
        .layer(middleware::from_fn_with_state(
            body_deadline::BodyReadDeadline::from_secs(config.body_read_deadline_secs),
            body_deadline::body_read_deadline_middleware,
        ))
        // SECURITY: /api/v1/admin/* must never answer anonymous callers.
        .layer(middleware::from_fn_with_state(
            state.clone(),
            security::auth::admin_guard,
        ))
        // DEFAULT-DENY (kanban t_28a832dd). Applied OUTSIDE `admin_guard`, so it runs first: a
        // matched route that is not in `security::route_policy::PUBLIC_ROUTES` must present a
        // credential (issued API key, app JWT, or the shared internal key) before any handler runs.
        // This is what makes a NEWLY mounted route private by default instead of relying on its
        // author remembering an `AuthenticatedUser` extractor. See the module docs for the census
        // that produced the allowlist and for the four previously-anonymous routes it closes.
        //
        // MOUNTED WITH `route_layer`, NOT `layer` (kanban t_a0c272ec; measured live 2026-10-06).
        // `Router::layer` also wraps the router's FALLBACK — see its impl (`routing/mod.rs`:
        // `fallback_router: this.fallback_router.layer(layer.clone())`) — so an UNMATCHED path was
        // refused by default-deny and answered 401 `"Authentication required"` instead of the
        // router's own 404, contradicting this middleware's doc comment. That scope is wrong for an
        // authorization gate; axum's own `route_layer` docs name this exact case: middleware "that
        // return early (such as authorization) which might otherwise convert a `404 Not Found` into
        // a `401 Unauthorized`". `route_layer` layers only `path_router`, so matched routes are
        // guarded exactly as before and an unmatched path keeps the router's 404. Safe because this
        // router serves NO static files — no `ServeDir`/`ServeFile`/`.fallback(...)` anywhere in
        // `src/`, so the fallback IS the plain 404 and hides nothing; the served statics live in
        // nginx (`/opt/swift/nginx/www*`), never in this process.
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            security::route_policy::default_deny,
        ))
        .layer(middleware::from_fn(security::headers::add_security_headers))
        .layer(TraceLayer::new_for_http())
        .layer(
            CorsLayer::new()
                .allow_origin(cors_allowed_origins(&config.allowed_origins))
                .allow_methods(tower_http::cors::Any)
                .allow_headers(tower_http::cors::Any),
        )
        .with_state(state);

    // Start background email ticker (flushes scheduled follow-ups/reminders)
    // Start server
    let addr = format!("{}:{}", config.host, config.port);
    tracing::info!("Starting IncentiveSwift API on {}", addr);

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    tracing::info!("Shutdown signal received, starting graceful shutdown...");
}

/// The host-side applier (`incentiveswift-api apply-site-settings`, driven by
/// /opt/swift/bin/is-site-apply.sh from cron */5). It is the ONLY writer of
/// /opt/swift/nginx/www/incentiveswift/*.
///
/// It prints one machine-readable summary line — `site artifacts: written=N skipped=M` — and one
/// line per file it touched or deliberately left alone, so the cron log and the card's proof can both
/// read what happened without a second probe and an unchanged tree stays a one-line no-op.
async fn apply_site_settings_mode() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let check = args.iter().any(|a| a == "--check");
    let emit_dir = args
        .iter()
        .position(|a| a == "--emit")
        .and_then(|i| args.get(i + 1))
        .cloned();

    let url = std::env::var("DATABASE_URL").map_err(|_| {
        anyhow::anyhow!("apply-site-settings: DATABASE_URL is not set (run it through /opt/swift/bin/is-site-apply.sh)")
    })?;

    let pool = sqlx::PgPool::connect(&url).await.map_err(|e| {
        anyhow::anyhow!("apply-site-settings: cannot connect to the database: {}", e)
    })?;

    let settings = handlers::site_handler::load_settings(&pool)
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "apply-site-settings: cannot read the site settings row: {:?}",
                e
            )
        })?;

    let (targets, skipped) = handlers::site_handler::plan(&settings);

    // --emit: drop the rendered bytes somewhere else so they can be compared byte-for-byte with the
    // served file WITHOUT this process writing anything under SITE_ROOT.
    if let Some(dir) = emit_dir {
        std::fs::create_dir_all(&dir).map_err(|e| {
            anyhow::anyhow!(
                "apply-site-settings: cannot create --emit dir {}: {}",
                dir,
                e
            )
        })?;
        for (path, rendered) in &targets {
            let name = std::path::Path::new(path)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "rendered".to_string());
            let dest = std::path::Path::new(&dir).join(name);
            std::fs::write(&dest, rendered.as_bytes()).map_err(|e| {
                anyhow::anyhow!(
                    "apply-site-settings: cannot write {}: {}",
                    dest.display(),
                    e
                )
            })?;
            println!("emit {} -> {}", path, dest.display());
        }
    }

    if check {
        for (path, reason) in &skipped {
            println!("skip {} {}", path, reason);
        }
        for (path, rendered) in &targets {
            let now = std::fs::read_to_string(path).unwrap_or_default();
            let state = if now == *rendered {
                "unchanged"
            } else {
                "would-write"
            };
            println!(
                "check {} state={} sha256={} served_sha256={}",
                path,
                state,
                sha256_hex(rendered.as_bytes()),
                sha256_hex(now.as_bytes())
            );
        }
        println!(
            "site artifacts (check, nothing written): targets={} skipped={}",
            targets.len(),
            skipped.len()
        );
        return Ok(());
    }

    let (written, skipped) = handlers::site_handler::apply_to_disk(&settings);
    for path in &written {
        println!("write {} (the rendered bytes differ from the file)", path);
    }
    for (path, reason) in &skipped {
        println!("skip {} {}", path, reason);
    }
    println!(
        "site artifacts: written={} skipped={}",
        written.len(),
        skipped.len()
    );
    Ok(())
}

/// Lowercase SHA-256 hex, used by the `--check` leg so a rendered/served mismatch is one comparable
/// line in the log (the same shape the ADASwift applier prints).
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}
