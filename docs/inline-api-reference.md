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
| PUT | `/campaigns/:slug` | JWT | `campaigns::update_campaign` | Update campaign (body may include `theme` → deep-merged into `surface_config.theme`) |
| DELETE | `/campaigns/:slug` | JWT | `campaigns::delete_campaign_by_id` | Delete campaign |
| GET | `/campaigns/subdomain/:t_slug` | None | `campaigns::get_campaigns_by_subdomain` | Campaigns by tenant subdomain |
| POST | `/campaigns/test-webhook` | JWT/API Key | `entries::test_entry_webhook` | Fire a sample `entry.created` payload at the caller's OWN campaign webhook — body `{"campaign_id": "<uuid>"}`; the URL comes from that campaign's `config.entry_webhook_url` (never from the caller) and must pass the outbound-webhook security gate. Returns the delivery `status` only. |

## Raffles / Sweepstakes

| Method | Path | Auth | Handler | Description |
|--------|------|------|---------|-------------|
| POST | `/raffles/:slug/enter` | None | `raffles::enter_raffle` | Enter a raffle (public) |
| POST | `/raffles/:slug/draw` | JWT | `raffles::draw` | Draw raffle winner |
| POST | `/raffles/:slug/redraw` | JWT | `raffles::redraw` | Redraw raffle |

## Spin Wheel / Prize Draw

| Method | Path | Auth | Handler | Description |
|--------|------|------|---------|-------------|
| POST | `/campaigns/:slug/spin` | None | `spin_handler::spin` | Spin the wheel (mechanic-gated; 402 if tier lacks `mechanic_spin_wheel`/`all_mechanics`) |
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

## Loyalty V2 — Vouchers

| Method | Path | Auth | Handler | Description |
|--------|------|------|---------|-------------|
| GET | `/loyalty/my-vouchers/:contact_id` | JWT | `loyalty_v2::list_my_vouchers` | List active vouchers |
| POST | `/loyalty/claim-voucher` | None | `loyalty_v2::claim_voucher` | Redeem voucher by code |
| GET | `/loyalty/rewards-earned/:contact_id` | JWT | `loyalty_v2::list_rewards_earned` | List earned rewards |

> `POST /loyalty/generate-pin` and `POST /loyalty/issue-voucher` were RETIRED in kanban
> t_b209d263 (both anonymous and unscoped — any caller could mint a live voucher with a
> caller-chosen discount, or a pending purchase verification, against any active campaign for
> any contact id). Both now answer a bare 404. The Auth column above was measured live in the
> same pass: `my-vouchers` and `rewards-earned` answer 401 to an anonymous
> caller (JWT); `claim-voucher` is keyed by a claim code (a bearer secret) and answers its own
> JSON 404 for a bad code — it takes no credential by design.
>
> `POST /loyalty/verify-purchase` (and the private `loyalty_v2::issue_rotation_voucher` it was the
> only caller of) was RETIRED in kanban t_7a16bf0b. It was a correctly-guarded
> `AuthenticatedUser` reader of `purchase_verifications`, but t_b209d263 removed that table's only
> writer, so it answered `404 {"error":"Invalid or expired PIN"}` for every caller forever. The
> live purchase-verification flow is `POST /loyalty/purchase/verify`, which validates the caller's
> OWN `accounts.purchase_pin`. The route now answers a bare 0-byte 404.

## Admin — Pledges

> The anonymous business-facing pledge arms (`POST /business/pledge`,
> `GET /business/pledges/:business_id`) were RETIRED in kanban t_5e244255 — 0 callers, 0 hits, 0
> rows, directory-entity ids with no account owner derivable here. The live pledge flow is the
> admin review surface below.

| Method | Path | Auth | Handler | Description |
|--------|------|------|---------|-------------|
| GET | `/admin/pledges` | Admin | `loyalty_v2::list_pending_pledges` | List pending pledges |
| POST | `/admin/pledges/:id/review` | Admin | `loyalty_v2::review_pledge` | Approve/reject pledge |

## Admin — Rotation Config (Cross-Promotion) — RETIRED

> The five rotation arms — `POST`/`GET /admin/rotation-configs` and `POST`/`GET`/`DELETE
> /admin/rotation-members` (`loyalty_v2::create_rotation_config`, `list_rotation_configs`,
> `add_rotation_member`, `list_rotation_members`, `remove_rotation_member`) — were **RETIRED** in
> kanban t_8e9d3a52. They wrote `rotation_configs` / `rotation_group_members`, whose only consumer
> was `loyalty_v2::issue_rotation_voucher` (retired in t_7a16bf0b); both tables have 0 rows ever
> and nothing read what those arms wrote.
>
> They ARE **admin-guarded** (`security::auth::admin_guard`, path-based over `/api/v1/admin/*`) —
> an anonymous caller gets `401 Admin authentication required` and a `company_admin` `403 Admin
> role required`, so they were never an anonymous surface. An operator token reached a LIVE
> handler, and `POST /admin/rotation-members` answered **500 for every caller**: its
> `ON CONFLICT (rotation_config_id, business_id)` has no matching unique constraint on the live
> table. All five now answer a bare 0-byte 404 to an operator token (a route that never existed
> answers the same shape). The now-orphaned tables' drop is a separate card.
> Proof: `/opt/swift/audits/t_8e9d3a52/proof.py`.

## Cross-App Integration

The `/loyalty/external/*` family — `tag-contact`, `grant-credits`, `register-member` and the external
program lookup — was **RETIRED** (kanban t_f76c9950). Nothing called it, and two of its four arms
carried no credential at all (an anonymous caller could mint a `company_admin` account). All four
paths now answer `404`. IncentiveSwift's host-to-host seam is the `x-internal-key` `/api/v1/internal/*`
family.

> `POST /campaigns/external/survey-response` was **RETIRED** (kanban t_3bde2e27): an anonymous,
> unscoped value mint — any uncredentialed caller who named a live `directory-*` campaign slug
> minted an active $50 voucher, injected a contact from the caller-supplied email and awarded 100
> Zaarcash, repeatably. Its only named caller (MultiDirectory, onboarding completion) retired the
> IncentiveSwift integration on 2026-09-23.

| Method | Path | Auth | Handler | Description |
|--------|------|------|---------|-------------|
| GET | `/credits/balance` | Service* | `credits_handler::get_balance` | Check Zaarcash balance (filtered by program) |

*Service-level auth — called by a sister app's backend, not a user JWT.

**Directions & amounts (MultiDirectory's native referral rewards):** visitor→visitor: 50, business→business: 200 (2× bonus), business→visitor: 50, visitor→business: 100 Zaarcash.

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
| GET | `/widget/:hash` | None | `surface_handler::get_widget_js` | Embeddable widget runtime (JavaScript; `?format=json` returns the snippet JSON) |
| GET | `/widget/:hash/config` | None | `surface_handler::get_widget_config` | Widget config |
| POST | `/campaigns/:slug/widget-snippet` | Bearer | `surface_handler::create_widget_snippet` | Create/return the campaign's embed snippet (the producer for `widget_snippets`) |
| DELETE | `/campaigns/:slug/widget-snippet` | Bearer | `surface_handler::disable_widget_snippet` | Stop serving the campaign's embed |
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
| POST | `/channels/inbound` | None | `sms_handler::channel_inbound_webhook` | Telnyx SMS/WhatsApp inbound (chat-funnel routing) |

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

## Tags

The tenant's own tag library (`tags`, grouped by `tag_groups`). Every verb is scoped to the caller's
account (`account_id` from the JWT); an id belonging to another account answers `404 Tag not found`.
`POST /tags` is idempotent on `(account_id, lower(name))` — re-submitting an existing name returns
that tag with `created:false`, so the `max_tags` allowance is consumed only when a row is really
created. The allowance is `tier_features.limit_value` for `features.key = 'max_tags'` on the account's
own `plan_tiers` row; at the cap, `POST` answers `402 Tags limit reached (used/cap). Upgrade to
increase your limit.` `color` must be `#rrggbb` (or empty for the default); `group` is the group's
NAME and is find-or-created for the account, `""` clears it on `PUT`.

| Method | Path | Auth | Handler | Description |
|--------|------|------|---------|-------------|
| GET | `/tags` | JWT | `tags_handler::list_tags` | List the account's tags |
| POST | `/tags` | JWT | `tags_handler::create_tag` | Create a tag (idempotent on name; plan-gated) |
| PUT | `/tags/:id` | JWT | `tags_handler::update_tag` | Rename / recolour / regroup a tag |
| DELETE | `/tags/:id` | JWT | `tags_handler::delete_tag` | Delete a tag |

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
| PUT | `/email-templates/:id` | JWT | `email_templates_handler::update` | Update template |
| DELETE | `/email-templates/:id` | JWT | `email_templates_handler::delete` | Delete template |

## Support Tickets

| Method | Path | Auth | Handler | Description |
|--------|------|------|---------|-------------|
| GET | `/support-tickets` | JWT | `support_tickets::list_tickets` | List tickets |
| POST | `/support-tickets` | JWT | `support_tickets::create_ticket` | Create ticket |
| GET | `/support-tickets/:id` | JWT | `support_tickets::get_ticket` | Ticket + message thread |
| PUT | `/support-tickets/:id` | JWT | `support_tickets::update_ticket` | Update status/priority/category/assignee |
| DELETE | `/support-tickets/:id` | JWT | `support_tickets::delete_ticket` | Delete ticket |
| POST | `/support-tickets/:id/messages` | JWT | `support_tickets::add_message` | Add internal/customer note |

## Reviews & Ratings

| Method | Path | Auth | Handler | Description |
|--------|------|------|---------|-------------|
| GET | `/reviews` | JWT | `reviews::list_reviews` | List reviews + count/average |
| POST | `/reviews` | JWT | `reviews::create_review` | Create review (rating 1..5) |
| PUT | `/reviews/:id` | JWT | `reviews::update_review` | Moderate (approve/reject) or edit |
| DELETE | `/reviews/:id` | JWT | `reviews::delete_review` | Delete review |

## Calendar Events

| Method | Path | Auth | Handler | Description |
|--------|------|------|---------|-------------|
| GET | `/calendar-events?from=&to=` | JWT | `calendar_events::list_events` | List events (RFC3339 range) |
| POST | `/calendar-events` | JWT | `calendar_events::create_event` | Create event |
| PUT | `/calendar-events/:id` | JWT | `calendar_events::update_event` | Update event/status |
| DELETE | `/calendar-events/:id` | JWT | `calendar_events::delete_event` | Delete event |

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
