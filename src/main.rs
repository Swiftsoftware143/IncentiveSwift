//! IncentiveSwift ??? Multi-tenant Engagement & Capture Engine
//!
// REST API server providing gamified incentive mechanics, raffle/giveaway system,
// long-form qualifier, and loyalty program module.

mod email;

mod config;
mod features;
mod state;
mod error;
mod db;
pub mod handlers;
pub mod delivery;
pub mod mechanics;
pub mod access;
pub mod security;

use axum::{
    routing::{get, post, put, delete, patch},
    Router,
    middleware,
    http::HeaderValue,
};
use tokio::signal;
use tower_http::{
    cors::CorsLayer,
    trace::TraceLayer,
    timeout::TimeoutLayer,
};
use tracing_subscriber::EnvFilter;
use std::time::Duration;
use std::sync::Arc;



/// Build a CORS origin predicate from allowed origins list.
fn cors_allowed_origins(allowed: &[String]) -> tower_http::cors::AllowOrigin {
    use std::sync::Arc;
    let origins: Vec<Arc<str>> = allowed.iter().map(|s| Arc::from(s.as_str())).collect();
    tower_http::cors::AllowOrigin::predicate(move |origin: &HeaderValue, _parts: &axum::http::request::Parts| {
        origins.iter().any(|allowed| {
            if let Ok(origin_str) = origin.to_str() {
                origin_str == allowed.as_ref()
            } else {
                false
            }
        })
    })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(true)
        .with_thread_ids(true)
        .init();

    // Load configuration
    let config = config::AppConfig::from_env()?;
    let config = Arc::new(config);

    // Build shared state
    let state = state::AppState::new(&config).await?;

    // Build router
    let app = Router::new()
        // Public routes
        .route("/api/v1/health", get(handlers::health::health_check))
        .route("/api/v1/channels/inbound", post(handlers::sms_handler::channel_inbound_webhook))
        .route("/api/v1/campaigns/:slug", get(handlers::campaigns::get_campaign).put(handlers::campaigns::update_campaign).delete(handlers::campaigns::delete_campaign_by_id))
        .route("/api/v1/campaigns/subdomain/:t_slug", get(handlers::campaigns::get_campaigns_by_subdomain))
        .route("/api/v1/campaigns/test-webhook", post(handlers::entries::test_entry_webhook))
        .route("/api/v1/entries", post(handlers::entries::create_entry))
        .route("/api/v1/raffles/:slug/enter", post(handlers::raffles::enter_raffle))
        // Spin wheel / prize draw routes
        .route("/api/v1/campaigns/:slug/spin", post(handlers::spin_handler::spin))
        .route("/api/v1/campaigns/:slug/spin-status", get(handlers::spin_handler::spin_status))
        .route("/api/v1/loyalty/checkin", post(handlers::loyalty::checkin))
        .route("/api/v1/loyalty/online/visit", post(handlers::loyalty::online_visit))
        .route("/api/v1/loyalty/online/share", post(handlers::loyalty::online_share))
        .route("/api/v1/loyalty/online/referral-click", post(handlers::loyalty::referral_click))
        .route("/api/v1/loyalty/online/stats/:code", get(handlers::loyalty::online_stats))
        // Viral campaign engine -- Phase 1 (public)
        .route("/api/v1/earn/:channel_code", get(handlers::viral_handler::earn_click_through))
        .route("/api/v1/c/:campaign_slug", get(handlers::viral_handler::campaign_share_link))
        // Authenticated routes
        .route("/api/v1/campaigns", get(handlers::campaigns::list_campaigns).post(handlers::campaigns::create_campaign))
        .route("/api/v1/raffles/:slug/draw", post(handlers::raffles::draw))
        .route("/api/v1/raffles/:slug/redraw", post(handlers::raffles::redraw))
        .route("/api/v1/loyalty/programs", get(handlers::loyalty::list_programs).post(handlers::loyalty::create_program))
        .route("/api/v1/loyalty/programs/:id", put(handlers::loyalty::update_program).delete(handlers::loyalty::delete_program))
        .route("/api/v1/loyalty/rewards", get(handlers::loyalty::list_rewards))
        .route("/api/v1/loyalty/rewards/:id/approve", post(handlers::loyalty::approve_reward))
        .route("/api/v1/loyalty/rewards/:id/deny", post(handlers::loyalty::deny_reward))
        .route("/api/v1/loyalty/tiers", get(handlers::loyalty::list_tiers).post(handlers::loyalty::create_tier))
        .route("/api/v1/loyalty/tiers/:id", put(handlers::loyalty::update_tier).delete(handlers::loyalty::delete_tier))
        .route("/api/v1/loyalty/check-plan", get(handlers::loyalty::check_plan_loyalty))
        // Secret code admin routes
        .route("/api/v1/loyalty/secret-codes", get(handlers::secret_codes_handler::list_secret_codes).post(handlers::secret_codes_handler::create_secret_code))
        .route("/api/v1/loyalty/secret-codes/:id", delete(handlers::secret_codes_handler::delete_secret_code))
        .route("/api/v1/loyalty/secret-codes/:id/toggle", post(handlers::secret_codes_handler::toggle_secret_code))

        // Viral campaign engine -- Admin routes
        .route("/api/v1/campaigns/:slug/referral-codes", post(handlers::viral_handler::create_referral_code))
        .route("/api/v1/campaigns/:slug/referral-stats", get(handlers::viral_handler::get_referral_stats))
        .route("/api/v1/campaigns/:slug/earn-channels", get(handlers::viral_handler::list_earn_channels).post(handlers::viral_handler::create_earn_channel))
        .route("/api/v1/campaigns/:slug/earn-channels/:channel_id", patch(handlers::viral_handler::update_earn_channel).delete(handlers::viral_handler::delete_earn_channel))
        .route("/api/v1/campaigns/:slug/earn/verify", post(handlers::viral_handler::verify_earn_action))
        .route("/api/v1/campaigns/:slug/leaderboard", get(handlers::viral_handler::campaign_leaderboard))
        // Phase 2: Milestone Rewards (admin)
        .route("/api/v1/campaigns/:slug/milestones",
            get(handlers::milestone_handler::list_milestones)
            .post(handlers::milestone_handler::create_milestone))
        .route("/api/v1/campaigns/:slug/milestones/achieved",
            get(handlers::milestone_handler::list_achieved_milestones))
        .route("/api/v1/campaigns/:slug/milestones/:milestone_id",
            put(handlers::milestone_handler::update_milestone)
            .delete(handlers::milestone_handler::delete_milestone))
        // Campaign secret codes (promo-code style, type-to-redeem)
        .route("/api/v1/campaigns/:campaign_id/secret-codes", get(handlers::campaign_secret_codes::list_secret_codes).post(handlers::campaign_secret_codes::create_secret_code))
        .route("/api/v1/secret-codes", get(handlers::secret_codes_handler::list_secret_codes).post(handlers::secret_codes_handler::create_secret_code))
        .route("/api/v1/secret-codes/:id", delete(handlers::secret_codes_handler::delete_secret_code))
        .route("/api/v1/secret-codes/:id/toggle", post(handlers::secret_codes_handler::toggle_secret_code))

        .route("/api/v1/campaigns/:campaign_id/secret-codes/:code_id", put(handlers::campaign_secret_codes::update_secret_code).delete(handlers::campaign_secret_codes::delete_secret_code))
        .route("/api/v1/campaigns/:campaign_id/secret-codes/redemptions", get(handlers::campaign_secret_codes::list_redemptions))
        .route("/api/v1/campaigns/:campaign_id/redeem-code", post(handlers::campaign_secret_codes::redeem_secret_code))

        // Verify uses the new loyalty_secret_codes table
        .route("/api/v1/loyalty/secret-code/verify", post(handlers::secret_codes_handler::verify_secret_code))
        .route("/api/v1/loyalty/programs/:id/secret-code", put(handlers::loyalty::set_secret_code))
        .route("/api/v1/loyalty/programs/:id/qr", get(handlers::loyalty::program_qr))
        .route("/api/v1/delivery/resend", post(handlers::delivery::resend))
        .route("/api/v1/contacts", get(handlers::contacts::list_contacts).post(handlers::contacts::create_contact))
        .route("/api/v1/contacts/:id", get(handlers::contacts::get_contact).put(handlers::contacts::update_contact).delete(handlers::contacts::delete_contact))
        .route("/api/v1/portfolio-companies", get(handlers::portfolio_handler::list_portfolio_companies).post(handlers::portfolio_handler::create_portfolio_company))
        .route("/api/v1/portfolio-companies/:id", get(handlers::portfolio_handler::get_portfolio_company).put(handlers::portfolio_handler::update_portfolio_company).delete(handlers::portfolio_handler::delete_portfolio_company))
        .route("/api/v1/integration-targets", get(handlers::integration_target_handler::list_integration_targets).post(handlers::integration_target_handler::create_integration_target))
        .route("/api/v1/integration-targets/:id", put(handlers::integration_target_handler::update_integration_target).delete(handlers::integration_target_handler::delete_integration_target))

        // Auth endpoints
        .route("/api/v1/auth/register", post(crate::handlers::auth_handler::register))
        .route("/api/v1/auth/login", post(crate::handlers::auth_handler::login))
        .route("/api/v1/auth/me", get(crate::handlers::auth_handler::me))
        .route("/api/v1/auth/profile", put(crate::handlers::auth_handler::update_profile))
        .route("/api/v1/auth/password", put(crate::handlers::auth_handler::change_password))
        .route("/api/v1/auth/forgot-password", post(crate::handlers::auth_handler::forgot_password))
        .route("/api/v1/auth/reset-password", post(crate::handlers::auth_handler::reset_password))

        // Admin endpoints (cross-app portfolio sync + impersonation)
        .route("/api/v1/admin/portfolio-sync", post(crate::handlers::admin_handler::portfolio_sync))
        .route("/api/v1/admin/impersonate", post(crate::handlers::admin_handler::impersonate))
        .route("/api/v1/admin/stop-impersonation", post(crate::handlers::admin_handler::stop_impersonation))
        .route("/api/v1/admin/tenants", get(crate::handlers::admin_handler::list_all_tenants))
        .route("/api/v1/admin/tenants/:id", delete(crate::handlers::admin_handler::delete_tenant))

        // Admin plan management
        .route("/api/v1/admin/plans", get(crate::handlers::plans_handler::list_plans).post(crate::handlers::plans_handler::create_plan))
        .route("/api/v1/admin/plans/assign", post(crate::handlers::plans_handler::admin_assign_plan))
        .route("/api/v1/admin/plans/:id", get(crate::handlers::plans_handler::get_plan).put(crate::handlers::plans_handler::update_plan).delete(crate::handlers::plans_handler::delete_plan))
        .route("/api/v1/admin/plans/:id/features", put(crate::handlers::plans_handler::admin_update_plan_features))
        // Industry routes
        .route("/api/v1/industries", get(crate::handlers::industries_handler::list_active_industries))
        .route("/api/v1/admin/industries", get(crate::handlers::industries_handler::admin_list_industries).post(crate::handlers::industries_handler::admin_create_industry))
        .route("/api/v1/admin/industries/:id", put(crate::handlers::industries_handler::admin_update_industry).delete(crate::handlers::industries_handler::admin_delete_industry))
        .route("/api/v1/api-keys", get(handlers::api_keys::list_api_keys).post(handlers::api_keys::create_api_key))
        .route("/api/v1/api-keys/:id", put(handlers::api_keys::update_api_key).delete(handlers::api_keys::delete_api_key))
        // Surface routes (public ??? no auth required)
        .route("/api/v1/widget/:hash", get(handlers::surface_handler::get_widget_js))
        .route("/api/v1/widget/:hash/config", get(handlers::surface_handler::get_widget_config))
        .route("/api/v1/tablet/:id", get(handlers::surface_handler::get_tablet_view))
        .route("/api/v1/tablet/:id/interact", post(handlers::surface_handler::tablet_interaction))
        .route("/api/v1/dashboard/stats", get(handlers::dashboard_handler::dashboard_stats))
        .route("/api/v1/play/:id", get(handlers::surface_handler::get_play_view))
        .route("/api/v1/play/:id/dashboard", get(handlers::surface_handler::get_loyalty_dashboard))
        .route("/api/v1/embed/campaign/all", get(handlers::surface_handler::get_embed_campaign_list))
        .route("/api/v1/embed/campaign/:slug", get(handlers::surface_handler::get_campaign_embed))
        .route("/api/v1/embed/:id", get(handlers::surface_handler::get_embed_view))
        // Surface routes (admin ??? protected by auth middleware)
        // Quiz/Trivia question CRUD + submission
        .route("/api/v1/campaigns/:slug/questions", get(handlers::quiz_handler::list_campaign_questions).post(handlers::quiz_handler::create_question))
        .route("/api/v1/campaigns/:slug/questions/:question_id", put(handlers::quiz_handler::update_question).delete(handlers::quiz_handler::delete_question))
        .route("/api/v1/play/:campaign_id/questions", get(handlers::quiz_handler::play_campaign_questions))
        .route("/api/v1/quiz/:campaign_id/submit", post(handlers::quiz_handler::submit_quiz))
        .route("/api/v1/admin/campaigns/:id/surface", get(handlers::surface_handler::get_surface_config).put(handlers::surface_handler::update_surface_config))
        .route("/api/v1/admin/domains", get(handlers::surface_handler::list_domains).post(handlers::surface_handler::register_domain))
        .route("/api/v1/admin/domains/:id", delete(handlers::surface_handler::remove_domain))
        .route("/api/v1/admin/domains/:id/verify", post(handlers::surface_handler::verify_domain))
        .route("/api/v1/admin/plans/:id/domains", get(handlers::surface_handler::check_plan_domains))
        // Surfaces REST CRUD routes
        .route("/api/v1/surfaces", get(handlers::surfaces_handler::list).post(handlers::surfaces_handler::create))
        .route("/api/v1/surfaces/:id", get(handlers::surfaces_handler::get).put(handlers::surfaces_handler::update).delete(handlers::surfaces_handler::delete))
        // Provider Keys routes
        .route("/api/v1/provider-keys", get(handlers::provider_keys_handler::list_provider_keys).post(handlers::provider_keys_handler::upsert_provider_key))
        .route("/api/v1/provider-keys/:provider", delete(handlers::provider_keys_handler::delete_provider_key))
        .route("/api/v1/available-providers", get(handlers::provider_keys_handler::list_available_providers))
        // Campaign Integration Hub routes
        .route("/api/v1/campaigns/:slug/integrations", get(handlers::campaign_integrations::list_campaign_integrations).post(handlers::campaign_integrations::link_campaign_integration))
        .route("/api/v1/campaigns/:slug/integrations/:integration_id", delete(handlers::campaign_integrations::unlink_campaign_integration))
        // Campaign wins / admin routes
        .route("/api/v1/campaigns/:slug/clone", post(handlers::campaigns::clone_campaign))
        .route("/api/v1/campaigns/:slug/wins", get(handlers::spin_handler::list_wins))
        .route("/api/v1/campaigns/:slug/wins/:win_id/redeem", post(handlers::spin_handler::redeem_win))
        // Custom fields routes
        .route("/api/v1/campaigns/:slug/custom-fields",
            get(handlers::custom_fields_handler::list_custom_fields)
            .post(handlers::custom_fields_handler::create_custom_field))
        .route("/api/v1/campaigns/:slug/custom-fields/reorder",
            put(handlers::custom_fields_handler::reorder_custom_fields))
        .route("/api/v1/campaigns/:slug/custom-fields/:field_id",
            delete(handlers::custom_fields_handler::delete_custom_field)
            .put(handlers::custom_fields_handler::update_custom_field))
        // Settings routes
        .route("/api/v1/settings", get(handlers::settings_handler::get_settings).put(handlers::settings_handler::update_settings))
        // Analytics routes
        .route("/api/v1/analytics/overview", get(handlers::analytics_handler::overview))
        .route("/api/v1/analytics/campaigns", get(handlers::analytics_handler::campaign_list))
        .route("/api/v1/analytics/campaigns/:slug", get(handlers::analytics_handler::campaign_detail))
        .route("/api/v1/analytics/contacts", get(handlers::analytics_handler::contacts_analytics))
        .route("/api/v1/analytics/loyalty", get(handlers::analytics_handler::loyalty_analytics))
        .route("/api/v1/analytics/export", get(handlers::analytics_handler::export_csv))
        .layer(middleware::from_fn(security::headers::add_security_headers))
        .layer(TimeoutLayer::new(Duration::from_secs(30)))
        
        .layer(TraceLayer::new_for_http())
        .layer(
            CorsLayer::new()
                .allow_origin(cors_allowed_origins(&config.allowed_origins))
                .allow_methods(tower_http::cors::Any)
                .allow_headers(tower_http::cors::Any),
        )
        .with_state(state);

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
