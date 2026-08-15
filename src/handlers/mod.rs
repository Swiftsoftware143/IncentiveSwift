//! Handlers for each API endpoint.

pub mod campaigns;
pub mod delivery;
pub mod entries;
pub mod health;
pub mod loyalty;
pub mod raffles;
pub use loyalty::*;
pub mod loyalty_v2;
pub use loyalty_v2::*;
pub mod admin_handler;
pub mod api_keys;
pub mod auth_handler;
pub mod contacts;
pub mod email_templates_handler;
pub mod integration_target_handler;
pub mod plans_handler;
pub mod portfolio_handler;
pub mod portfolio_sync_handler;
pub mod surface_handler;

pub mod analytics_handler;
pub mod campaign_integrations;
pub mod campaign_secret_codes;
pub mod custom_fields_handler;
pub mod dashboard_handler;
pub mod industries_handler;
pub mod iqs_handler;
pub mod marketing_boost_handler;
pub mod milestone_handler;
pub mod provider_keys_handler;
pub mod quiz_handler;
pub mod secret_codes_handler;
pub mod settings_handler;
pub mod site_handler;
pub mod spin_handler;
pub mod surfaces_handler;
pub mod viral_handler;

pub mod sms_handler;

pub mod credits_handler;
pub mod external_grants;
pub use external_grants::*;
pub mod business_handler;
pub mod clearinghouse_config_handler;
pub mod loyalty_badges;
pub mod loyalty_plans;
pub mod offers_handler;
pub mod point_expiry_handler;
pub mod stripe_webhook;
pub mod supplier_handler;
pub mod tag_provision_handler;
pub mod treasury_handler;
