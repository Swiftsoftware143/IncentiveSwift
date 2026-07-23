//! Handlers for each API endpoint.

pub mod health;
pub mod campaigns;
pub mod entries;
pub mod raffles;
pub mod loyalty;
pub mod delivery;
pub use loyalty::*;
pub mod loyalty_v2;
pub use loyalty_v2::*;
pub mod contacts;
pub mod surface_handler;
pub mod portfolio_handler;
pub mod integration_target_handler;
pub mod auth_handler;
pub mod admin_handler;
pub mod plans_handler;
pub mod api_keys;
pub mod affiliates_handler;
pub mod leads_handler;
pub mod tags_handler;
pub mod tag_groups_handler;
pub mod clients_handler;
pub mod workflows_handler;
pub mod deals_handler;
pub mod tickets_handler;
pub mod email_templates_handler;
pub mod webhooks_handler;
pub mod reviews_handler;
pub mod categories_handler;
pub mod reports_handler;
pub mod knowledge_base_handler;
pub mod import_logs_handler;
pub mod export_templates_handler;
pub mod call_logs_handler;
pub mod calendar_events_handler;

pub mod surfaces_handler;
pub mod settings_handler;
pub mod spin_handler;
pub mod marketing_boost_handler;
pub mod campaign_integrations;
pub mod provider_keys_handler;
pub mod dashboard_handler;
pub mod custom_fields_handler;
pub mod analytics_handler;
pub mod secret_codes_handler;
pub mod viral_handler;
pub mod milestone_handler;
pub mod campaign_secret_codes;
pub mod industries_handler;
pub mod quiz_handler;


pub mod sms_handler;

pub mod credits_handler;
pub mod external_grants;
pub use external_grants::*;
pub mod rewards_handler;
pub mod offers_handler;
pub mod tag_provision_handler;
