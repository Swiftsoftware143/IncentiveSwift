# IncentiveSwift — Inline API Reference

## Overview

Quick reference for every route group in the Axum router (`src/main.rs`). Grouped by router section. All routes are prefixed `/api/v1` unless noted.

---

## Health & Public Entry

| Method | Path | Auth | Handler | Description |
|--------|------|------|---------|-------------|
| GET | `/health` | None | `health::health_check` | Service health check |

## Campaigns

| Method | Path | Auth | Handler | Description |
|--------|------|------|---------|-------------|
| GET | `/campaigns` | JWT/API Key | `campaigns::list_campaigns` | List all campaigns |
| POST | `/campaigns` | JWT/API Key | `campaigns::create_campaign` | Create campaign (feature-gated) |
| GET | `/campaigns/:slug` | None | `campaigns::get_campaign` | Public campaign by slug |
| PUT | `/campaigns/:slug` | JWT | `campaigns::update_campaign` | Update campaign |
| DELETE | `/campaigns/:slug` | JWT | `campaigns::delete_campaign_by_id` | Delete campaign |
| GET | `/campaigns/subdomain/:t_slug` | None | `campaigns::get_campaigns_by_subdomain` | Campaigns by tenant subdomain |
| POST | `/campaigns/test-webhook` | None | `entries::test_entry_webhook` | Test entry webhook |

## Raffles / Sweepstakes

| Method | Path | Auth | Handler | Description |
|--------|------|------|---------|-------------|
| POST | `/raffles/:slug/enter` | None | `raffles::enter_raffle` | Enter a raffle (public) |
| POST | `/raffles/:slug/draw` | JWT | `raffles::draw` | Draw raffle winner |
| POST | `/raffles/:slug/redraw` | JWT | `raffles::redraw` | Redraw raffle |

## Spin Wheel / Prize Draw

| Method | Path | Auth | Handler | Description |
|--------|------|------|---------|-------------|
| POST | `/campaigns/:slug/spin` | None | `spin_handler::spin` | Spin the wheel |
| GET | `/campaigns/:slug/spin-status` | None | `spin_handler::spin_status` | Check spin availability |
| GET | `/campaigns/:slug/wins` | JWT | `spin_handler::list_wins` | List wins for campaign |
| POST | `/campaigns/:slug/wins/:win_id/redeem` | JWT | `spin_handler::redeem_win` | Redeem a win |

## Entries

| Method | Path | Auth | Handler | Description |
|--------|------|------|---------|-------------|
| POST | `/entries` | None | `entries::create_entry` | Core capture endpoint |

## Loyalty V1 — Check-in & Programs

| Method | Path | Auth | Handler | Description |
|--------|------|------|---------|-------------|
| POST | `/loyalty/checkin` | None | `loyalty::checkin` | Daily check-in |
| POST | `/loyalty/online/visit` | None | `loyalty::online_visit` | Online visit tracking |
| POST | `/loyalty/online/share` | None | `loyalty::online_share` | Share tracking |
| POST | `/loyalty/online/referral-click` | None | `loyalty::referral_click` | Referral click tracking |
| GET | `/loyalty/online/stats/:code` | None | `loyalty::online_stats` | Referral stats by code |
| GET | `/loyalty/programs` | JWT | `loyalty::list_programs` | List loyalty programs |
| POST | `/loyalty/programs` | JWT | `loyalty::create_program` | Create loyalty program |
| PUT | `/loyalty/programs/:id` | JWT | `loyalty::update_program` | Update program |
| DELETE | `/loyalty/programs/:id` | JWT | `loyalty::delete_program` | Delete program |
| GET | `/loyalty/check-plan` | JWT | `loyalty::check_plan_loyalty` | Check plan loyalty access |
| PUT | `/loyalty/programs/:id/secret-code` | JWT | `loyalty::set_secret_code` | Set program secret code |
| GET | `/loyalty/programs/:id/qr` | JWT | `loyalty::program_qr` | Get program QR code |

## Loyalty V2 — Purchase Verification & Vouchers

| Method | Path | Auth | Handler | Description |
|--------|------|------|---------|-------------|
| POST | `/loyalty/generate-pin` | None | `loyalty_v2::generate_pin` | Business generates 4-digit PIN |
| POST | `/loyalty/verify-purchase` | None | `loyalty_v2::verify_purchase` | Consumer verifies purchase with PIN |
| POST | `/loyalty/issue-voucher` | None | `loyalty_v2::issue_voucher` | Issue cross-promo voucher |
| GET | `/loyalty/my-vouchers/:contact_id` | None | `loyalty_v2::list_my_vouchers` | List active vouchers |
| POST | `/loyalty/claim-voucher` | None | `loyalty_v2::claim_voucher` | Redeem voucher by code |
| POST | `/loyalty/expire-vouchers` | None | `loyalty_v2::expire_vouchers` | Expire overdue vouchers (cron) |
| POST | `/loyalty/redeem-reward` | None | `loyalty_v2::redeem_reward` | Redeem points for reward |
| GET | `/loyalty/rewards-earned/:contact_id` | None | `loyalty_v2::list_rewards_earned` | List earned rewards |

## Business Pledges

| Method | Path | Auth | Handler | Description |
|--------|------|------|---------|-------------|
| POST | `/business/pledge` | None | `loyalty_v2::create_pledge` | Submit reward offer |
| GET | `/business/pledges/:business_id` | None | `loyalty_v2::list_business_pledges` | List pledges for business |

## Admin — Pledges

| Method | Path | Auth | Handler | Description |
|--------|------|------|---------|-------------|
| GET | `/admin/pledges` | Admin | `loyalty_v2::list_pending_pledges` | List pending pledges |
| POST | `/admin/pledges/:id/review` | Admin | `loyalty_v2::review_pledge` | Approve/reject pledge |

## Admin — Rotation Config (Cross-Promotion)

| Method | Path | Auth | Handler | Description |
|--------|------|------|---------|-------------|
| POST | `/admin/rotation-configs` | Admin | `loyalty_v2::create_rotation_config` | Create rotation config |
| GET | `/admin/rotation-configs/:campaign_slug` | Admin | `loyalty_v2::list_rotation_configs` | List configs for campaign |
| POST | `/admin/rotation-members` | Admin | `loyalty_v2::add_rotation_member` | Add business to rotation |
| GET | `/admin/rotation-members/:config_id` | Admin | `loyalty_v2::list_rotation_members` | List members in config |
| DELETE | `/admin/rotation-members/:config_id/:business_id` | Admin | `loyalty_v2::remove_rotation_member` | Remove member |

## External Integration (MultiDirectory)

| Method | Path | Auth | Handler | Description |
|--------|------|------|---------|-------------|
| POST | `/loyalty/external/tag-contact` | Service | `loyalty_v2::external_tag_contact` | Cross-platform tag sync |
| POST | `/campaigns/external/survey-response` | Service | `loyalty_v2::survey_response` | Survey response from MD |

## Rewards Handler

| Method | Path | Auth | Handler | Description |
|--------|------|------|---------|-------------|
| GET | `/rewards` | JWT | `rewards_handler::list_rewards` | List rewards |
| POST | `/rewards` | JWT | `rewards_handler::create_reward` | Create reward |
| PUT | `/rewards/:id` | JWT | `rewards_handler::update_reward` | Update reward |

## Credit System

| Method | Path | Auth | Handler | Description |
|--------|------|------|---------|-------------|
| GET | `/credits/balance` | JWT | `credits_handler::get_balance` | Balance + plan limits |
| GET | `/credits/history` | JWT | `credits_handler::get_history` | Paginated transaction log |
| POST | `/credits/topup` | JWT | `credits_handler::create_topup_checkout` | Stripe checkout to buy credits |
| POST | `/admin/credits/adjust` | Admin | `credits_handler::admin_adjust_credits` | Manual credit adjustment |
| POST | `/webhooks/sms/` | None | `credits_handler::sms_inbound_webhook` | SMS credit trigger |

## Auth

| Method | Path | Auth | Handler | Description |
|--------|------|------|---------|-------------|
| POST | `/auth/register` | None | `auth_handler::register` | Register user |
| POST | `/auth/login` | None | `auth_handler::login` | Login (returns JWT) |
| GET | `/auth/me` | JWT | `auth_handler::me` | Verify token |
| PUT | `/auth/profile` | JWT | `auth_handler::update_profile` | Update profile |
| PUT | `/auth/password` | JWT | `auth_handler::change_password` | Change password |
| POST | `/auth/forgot-password` | None | `auth_handler::forgot_password` | Send reset email |
| POST | `/auth/reset-password` | None | `auth_handler::reset_password` | Reset with token |

## Admin — System

| Method | Path | Auth | Handler | Description |
|--------|------|------|---------|-------------|
| POST | `/admin/portfolio-sync` | Admin | `admin_handler::portfolio_sync` | Cross-app portfolio sync |
| POST | `/admin/impersonate` | Admin | `admin_handler::impersonate` | Switch tenant |
| POST | `/admin/stop-impersonation` | Admin | `admin_handler::stop_impersonation` | End impersonation |
| GET | `/admin/tenants` | Admin | `admin_handler::list_all_tenants` | List all tenants |
| DELETE | `/admin/tenants/:id` | Admin | `admin_handler::delete_tenant` | Delete tenant |
| GET | `/admin/plans` | Admin | `plans_handler::list_plans` | List plan tiers |
| POST | `/admin/plans` | Admin | `plans_handler::create_plan` | Create plan |
| GET | `/admin/plans/:id` | Admin | `plans_handler::get_plan` | Get plan |
| PUT | `/admin/plans/:id` | Admin | `plans_handler::update_plan` | Update plan |
| DELETE | `/admin/plans/:id` | Admin | `plans_handler::delete_plan` | Delete plan |
| POST | `/admin/plans/assign` | Admin | `plans_handler::admin_assign_plan` | Assign plan to user |
| PUT | `/admin/plans/:id/features` | Admin | `plans_handler::admin_update_plan_features` | Update plan features |

## Admin — Industries

| Method | Path | Auth | Handler | Description |
|--------|------|------|---------|-------------|
| GET | `/admin/industries` | Admin | `industries_handler::admin_list_industries` | List all industries |
| POST | `/admin/industries` | Admin | `industries_handler::admin_create_industry` | Create industry |
| PUT | `/admin/industries/:id` | Admin | `industries_handler::admin_update_industry` | Update industry |
| DELETE | `/admin/industries/:id` | Admin | `industries_handler::admin_delete_industry` | Delete industry |

## Admin — Domains & Surfaces

| Method | Path | Auth | Handler | Description |
|--------|------|------|---------|-------------|
| GET | `/admin/campaigns/:id/surface` | Admin | `surface_handler::get_surface_config` | Get surface config |
| PUT | `/admin/campaigns/:id/surface` | Admin | `surface_handler::update_surface_config` | Update surface config |
| GET | `/admin/domains` | Admin | `surface_handler::list_domains` | List domains |
| POST | `/admin/domains` | Admin | `surface_handler::register_domain` | Register domain |
| DELETE | `/admin/domains/:id` | Admin | `surface_handler::remove_domain` | Remove domain |
| POST | `/admin/domains/:id/verify` | Admin | `surface_handler::verify_domain` | Verify domain |
| GET | `/admin/plans/:id/domains` | Admin | `surface_handler::check_plan_domains` | Check plan domain limit |

## Public — Surfaces

| Method | Path | Auth | Handler | Description |
|--------|------|------|---------|-------------|
| GET | `/widget/:hash` | None | `surface_handler::get_widget_js` | Embeddable widget JS |
| GET | `/widget/:hash/config` | None | `surface_handler::get_widget_config` | Widget config |
| GET | `/tablet/:id` | None | `surface_handler::get_tablet_view` | Tablet view |
| POST | `/tablet/:id/interact` | None | `surface_handler::tablet_interaction` | Tablet interaction |
| GET | `/play/:id` | None | `surface_handler::get_play_view` | Campaign play view |
| GET | `/play/:id/dashboard` | None | `surface_handler::get_loyalty_dashboard` | Loyalty dashboard |
| GET | `/embed/campaign/all` | None | `surface_handler::get_embed_campaign_list` | Embed campaign list |
| GET | `/embed/campaign/:slug` | None | `surface_handler::get_campaign_embed` | Campaign embed |
| GET | `/embed/:id` | None | `surface_handler::get_embed_view` | Embed view |

## Surfaces CRUD

| Method | Path | Auth | Handler | Description |
|--------|------|------|---------|-------------|
| GET | `/surfaces` | JWT | `surfaces_handler::list` | List surfaces |
| POST | `/surfaces` | JWT | `surfaces_handler::create` | Create surface |
| GET | `/surfaces/:id` | JWT | `surfaces_handler::get` | Get surface |
| PUT | `/surfaces/:id` | JWT | `surfaces_handler::update` | Update surface |
| DELETE | `/surfaces/:id` | JWT | `surfaces_handler::delete` | Delete surface |

## Secret Codes

| Method | Path | Auth | Handler | Description |
|--------|------|------|---------|-------------|
| GET | `/loyalty/secret-codes` | JWT | `secret_codes_handler::list_secret_codes` | List secret codes |
| POST | `/loyalty/secret-codes` | JWT | `secret_codes_handler::create_secret_code` | Create secret code |
| DELETE | `/loyalty/secret-codes/:id` | JWT | `secret_codes_handler::delete_secret_code` | Delete code |
| POST | `/loyalty/secret-codes/:id/toggle` | JWT | `secret_codes_handler::toggle_secret_code` | Toggle active |
| POST | `/loyalty/secret-code/verify` | JWT | `secret_codes_handler::verify_secret_code` | Verify code entry |

## Campaign Secret Codes (Promo Codes)

| Method | Path | Auth | Handler | Description |
|--------|------|------|---------|-------------|
| GET | `/campaigns/:campaign_id/secret-codes` | JWT | `campaign_secret_codes::list_secret_codes` | List campaign codes |
| POST | `/campaigns/:campaign_id/secret-codes` | JWT | `campaign_secret_codes::create_secret_code` | Create campaign code |
| PUT | `/campaigns/:campaign_id/secret-codes/:code_id` | JWT | `campaign_secret_codes::update_secret_code` | Update campaign code |
| DELETE | `/campaigns/:campaign_id/secret-codes/:code_id` | JWT | `campaign_secret_codes::delete_secret_code` | Delete campaign code |
| GET | `/campaigns/:campaign_id/secret-codes/redemptions` | JWT | `campaign_secret_codes::list_redemptions` | List redemptions |
| POST | `/campaigns/:campaign_id/redeem-code` | None | `campaign_secret_codes::redeem_secret_code` | Redeem a code |

## Viral / Referral Engine

| Method | Path | Auth | Handler | Description |
|--------|------|------|---------|-------------|
| POST | `/campaigns/:slug/referral-codes` | JWT | `viral_handler::create_referral_code` | Generate referral code |
| GET | `/campaigns/:slug/referral-stats` | JWT | `viral_handler::get_referral_stats` | Referral stats |
| GET | `/campaigns/:slug/earn-channels` | JWT | `viral_handler::list_earn_channels` | List earn channels |
| POST | `/campaigns/:slug/earn-channels` | JWT | `viral_handler::create_earn_channel` | Create earn channel |
| PATCH | `/campaigns/:slug/earn-channels/:channel_id` | JWT | `viral_handler::update_earn_channel` | Update channel |
| DELETE | `/campaigns/:slug/earn-channels/:channel_id` | JWT | `viral_handler::delete_earn_channel` | Delete channel |
| POST | `/campaigns/:slug/earn/verify` | JWT | `viral_handler::verify_earn_action` | Verify earn action |
| GET | `/campaigns/:slug/leaderboard` | JWT | `viral_handler::campaign_leaderboard` | Campaign leaderboard |
| GET | `/earn/:channel_code` | None | `viral_handler::earn_click_through` | Public earn click-through |
| GET | `/c/:campaign_slug` | None | `viral_handler::campaign_share_link` | Public share link |

## Milestones

| Method | Path | Auth | Handler | Description |
|--------|------|------|---------|-------------|
| GET | `/campaigns/:slug/milestones` | JWT | `milestone_handler::list_milestones` | List milestones |
| POST | `/campaigns/:slug/milestones` | JWT | `milestone_handler::create_milestone` | Create milestone |
| PUT | `/campaigns/:slug/milestones/:milestone_id` | JWT | `milestone_handler::update_milestone` | Update milestone |
| DELETE | `/campaigns/:slug/milestones/:milestone_id` | JWT | `milestone_handler::delete_milestone` | Delete milestone |
| GET | `/campaigns/:slug/milestones/achieved` | JWT | `milestone_handler::list_achieved_milestones` | List achieved milestones |

## Dashboard & Analytics

| Method | Path | Auth | Handler | Description |
|--------|------|------|---------|-------------|
| GET | `/dashboard/stats` | JWT | `dashboard_handler::dashboard_stats` | Dashboard stats |
| GET | `/analytics/overview` | JWT | `analytics_handler::overview` | Analytics overview |
| GET | `/analytics/campaigns` | JWT | `analytics_handler::campaign_list` | Campaign analytics |
| GET | `/analytics/campaigns/:slug` | JWT | `analytics_handler::campaign_detail` | Campaign detail |
| GET | `/analytics/contacts` | JWT | `analytics_handler::contacts_analytics` | Contacts analytics |
| GET | `/analytics/loyalty` | JWT | `analytics_handler::loyalty_analytics` | Loyalty analytics |
| GET | `/analytics/export` | JWT | `analytics_handler::export_csv` | Export CSV |

## Provider Keys & Checkout

| Method | Path | Auth | Handler | Description |
|--------|------|------|---------|-------------|
| GET | `/provider-keys` | JWT | `provider_keys_handler::list_provider_keys` | List provider keys |
| POST | `/provider-keys` | JWT | `provider_keys_handler::upsert_provider_key` | Upsert provider key |
| DELETE | `/provider-keys/:provider` | JWT | `provider_keys_handler::delete_provider_key` | Delete provider key |
| GET | `/available-providers` | JWT | `provider_keys_handler::list_available_providers` | Available providers |
| GET | `/payment-providers` | JWT | `checkout_handler::list_payment_providers` | List payment providers |
| POST | `/payment-providers` | JWT | `checkout_handler::upsert_payment_provider` | Upsert payment provider |
| DELETE | `/payment-providers/{provider_type}` | JWT | `checkout_handler::delete_payment_provider` | Delete payment provider |
| POST | `/checkout/create` | JWT | `checkout_handler::create_checkout_session` | Create checkout session |
| GET | `/checkout/sessions` | JWT | `checkout_handler::list_checkout_sessions` | List checkout sessions |

## Webhooks

| Method | Path | Auth | Handler | Description |
|--------|------|------|---------|-------------|
| POST | `/webhooks/stripe` | None* | `checkout_handler::stripe_webhook` | Stripe (signature-verified) |
| POST | `/webhooks/paypal` | None* | `checkout_handler::paypal_webhook` | PayPal (signature-verified) |
| POST | `/webhooks/sms/` | None | `credits_handler::sms_inbound_webhook` | SMS credit triggers |

*Webhook endpoints are public but verify signatures in handler body.

## Integration Hub

| Method | Path | Auth | Handler | Description |
|--------|------|------|---------|-------------|
| GET | `/campaigns/:slug/integrations` | JWT | `campaign_integrations::list_campaign_integrations` | List campaign integrations |
| POST | `/campaigns/:slug/integrations` | JWT | `campaign_integrations::link_campaign_integration` | Link integration |
| DELETE | `/campaigns/:slug/integrations/:integration_id` | JWT | `campaign_integrations::unlink_campaign_integration` | Unlink integration |
| GET | `/campaigns/:slug/marketing-boost` | JWT | `campaign_integrations::get_marketing_boost` | Get Marketing Boost config |
| PUT | `/campaigns/:slug/marketing-boost` | JWT | `campaign_integrations::set_marketing_boost` | Set Marketing Boost config |
| GET | `/marketing-boost/destinations` | JWT | `marketing_boost_handler::get_destinations` | List destinations |

## Contacts

| Method | Path | Auth | Handler | Description |
|--------|------|------|---------|-------------|
| GET | `/contacts` | JWT | `contacts::list_contacts` | List contacts |
| POST | `/contacts` | JWT | `contacts::create_contact` | Create contact |
| GET | `/contacts/:id` | JWT | `contacts::get_contact` | Get contact |
| PUT | `/contacts/:id` | JWT | `contacts::update_contact` | Update contact |
| DELETE | `/contacts/:id` | JWT | `contacts::delete_contact` | Delete contact |

## Portfolio Companies & Integration Targets

| Method | Path | Auth | Handler | Description |
|--------|------|------|---------|-------------|
| GET | `/portfolio-companies` | JWT | `portfolio_handler::list_portfolio_companies` | List portfolio companies |
| POST | `/portfolio-companies` | JWT | `portfolio_handler::create_portfolio_company` | Create company |
| GET | `/portfolio-companies/:id` | JWT | `portfolio_handler::get_portfolio_company` | Get company |
| PUT | `/portfolio-companies/:id` | JWT | `portfolio_handler::update_portfolio_company` | Update company |
| DELETE | `/portfolio-companies/:id` | JWT | `portfolio_handler::delete_portfolio_company` | Delete company |
| GET | `/integration-targets` | JWT | `integration_target_handler::list_integration_targets` | List targets |
| POST | `/integration-targets` | JWT | `integration_target_handler::create_integration_target` | Create target |
| PUT | `/integration-targets/:id` | JWT | `integration_target_handler::update_integration_target` | Update target |
| DELETE | `/integration-targets/:id` | JWT | `integration_target_handler::delete_integration_target` | Delete target |

## Email Templates

| Method | Path | Auth | Handler | Description |
|--------|------|------|---------|-------------|
| GET | `/email-templates` | JWT | `email_templates_handler::list` | List templates |
| POST | `/email-templates` | JWT | `email_templates_handler::create` | Create template |
| GET | `/email-templates/:id` | JWT | `email_templates_handler::get` | Get template |
| PUT | `/email-templates/:id` | JWT | `email_templates_handler::update` | Update template |
| DELETE | `/email-templates/:id` | JWT | `email_templates_handler::delete` | Delete template |

## Settings & Custom Fields

| Method | Path | Auth | Handler | Description |
|--------|------|------|---------|-------------|
| GET | `/settings` | JWT | `settings_handler::get_settings` | Get settings |
| PUT | `/settings` | JWT | `settings_handler::update_settings` | Update settings |
| GET | `/api-keys` | JWT | `api_keys::list_api_keys` | List API keys |
| POST | `/api-keys` | JWT | `api_keys::create_api_key` | Create API key |
| PUT | `/api-keys/:id` | JWT | `api_keys::update_api_key` | Update API key |
| DELETE | `/api-keys/:id` | JWT | `api_keys::delete_api_key` | Delete API key |
| GET | `/industries` | None | `industries_handler::list_active_industries` | Active industries |

| Method | Path | Auth | Handler | Description |
|--------|------|------|---------|-------------|
| GET | `/campaigns/:slug/custom-fields` | JWT | `custom_fields_handler::list_custom_fields` | List custom fields |
| POST | `/campaigns/:slug/custom-fields` | JWT | `custom_fields_handler::create_custom_field` | Create field |
| PUT | `/campaigns/:slug/custom-fields/reorder` | JWT | `custom_fields_handler::reorder_custom_fields` | Reorder fields |
| PUT | `/campaigns/:slug/custom-fields/:field_id` | JWT | `custom_fields_handler::update_custom_field` | Update field |
| DELETE | `/campaigns/:slug/custom-fields/:field_id` | JWT | `custom_fields_handler::delete_custom_field` | Delete field |

## Quiz / Trivia

| Method | Path | Auth | Handler | Description |
|--------|------|------|---------|-------------|
| GET | `/campaigns/:slug/questions` | JWT | `quiz_handler::list_campaign_questions` | List questions |
| POST | `/campaigns/:slug/questions` | JWT | `quiz_handler::create_question` | Create question |
| PUT | `/campaigns/:slug/questions/:question_id` | JWT | `quiz_handler::update_question` | Update question |
| DELETE | `/campaigns/:slug/questions/:question_id` | JWT | `quiz_handler::delete_question` | Delete question |
| GET | `/play/:campaign_id/questions` | None | `quiz_handler::play_campaign_questions` | Play questions |
| POST | `/quiz/:campaign_id/submit` | None | `quiz_handler::submit_quiz` | Submit quiz |

## SMS Channel

| Method | Path | Auth | Handler | Description |
|--------|------|------|---------|-------------|
| POST | `/channels/inbound` | None | `sms_handler::channel_inbound_webhook` | SMS channel inbound |

---

## CORS

Configured via `ALLOWED_ORIGINS` env var (comma-separated). Uses predicate-based `CorsLayer` — all methods and headers allowed, origin must match list. Configured in `main.rs`:

```rust
CorsLayer::new()
    .allow_origin(cors_allowed_origins(&config.allowed_origins))
    .allow_methods(tower_http::cors::Any)
    .allow_headers(tower_http::cors::Any)
```

## Middleware Stack (outer → inner)

1. `TraceLayer` — request logging
2. `CorsLayer` — origin filtering
3. `TimeoutLayer` (30s) — request timeout
4. Security headers middleware — CSP, HSTS, XFO, etc.
