# IncentiveSwift Admin Guide

## Overview
Multi-tenant engagement & capture engine powering loyalty programs, purchase verification, rotating cross-promotions, referral systems, and credit management. Works for **any business or tenant** — not wired to any specific directory.

## Tenant Roles

| Role | Description |
|---|---|
| `admin` | Full system access, impersonation, all tenants |
| `portfolio_company` | Sister company tenant — owns its campaigns and loyalty |
| `company_admin` | External business tenant — isolated from other tenants |

## Core Concepts

### Campaigns
Campaigns are the top-level container. Every loyalty program, referral system, and reward tier belongs to a campaign. Campaigns have slugs (used in API URLs), types (lead_gen, loyalty, sweepstakes), and config that controls mechanics, webhooks, and delivery.

### Loyalty Programs
Programs define earning rules within a campaign:
- **Check-in points** — points per daily check-in
- **Purchase points** — points per verified purchase (PIN)
- **Referral points** — points when a referral converts
- **Social share points** — points for social actions

### Currency
Each tenant has branded currency configured on their account:
- `currency_name` — e.g., "Points", "ZaarCash", "Stars"
- `currency_icon` — emoji or text icon
- `currency_color` — hex color for UI
- `b2b_currency_name` — separate currency name for B2B (optional)

### Reward Tiers
Rewards that consumers can redeem points/credits for. Supports:
- **Auto** — immediate redemption, no approval
- **Admin approval** — requires manual review before issuance

### Pledges (Business Offers)
Businesses submit reward offers (e.g., "15% off first service"). Status flow: `pending` → admin review → `approved` or `rejected`. Only approved pledges participate in the rotating cross-promotion network.

### Rotating Cross-Promotion Engine
When a purchase is verified at Business A, the system auto-issues a voucher for Business B (a non-competing business in the same rotation group). This drives cross-traffic between businesses.

### Purchase Verification (PIN)
1. Business generates a 4-digit PIN via the API
2. Customer enters the PIN to verify their purchase
3. Rotating cross-promo voucher is issued automatically
4. Customer redeems the voucher at another business within the expiry window

### Referral System
- Campaign-scoped referral codes and earn channels
- Click tracking, points awarding, milestone triggers
- Leaderboard for top referrers
- Supports anonymous clicks (logged, no points) and identified clicks (points awarded)

#### MultiDirectory Referral Integration

MultiDirectory's referral system (see its Admin Guide → Referral System) integrates with IncentiveSwift for Zaarcash payouts:

1. Visitor/business generates referral link in MultiDirectory
2. New user signs up via referral link (status: `pending`)
3. Admin verifies the referral in MultiDirectory admin panel
4. MultiDirectory calls `POST /api/v1/loyalty/external/grant-credits` with:
   `{"email": "referrer@email.com", "amount": <zaarcash>, "reason": "referral", "program": "zaarhub"}`
5. IncentiveSwift grants credits; balance visible in MultiDirectory dashboards

**Zaarcash amounts by direction:** visitor→visitor: 50, business→business: 200, business→visitor: 50, visitor→business: 100.

## Affiliate Product Auto-Sync

IncentiveSwift plans are automatically synced to FunnelSwift's `affiliate_products` table.

**How it works:**

| Action | What happens |
|--------|-------------|
| **Plan created** | `POST /api/v1/internal/sync-affiliate-plan` fires with `action: create`, `source_app: incentiveswift` |
| **Plan updated** | Same endpoint with `action: update` |
| **Plan deactivated** | Same endpoint with `action: deactivate` — marks the affiliate product inactive |

The sync fires asynchronously. FunnelSwift must be reachable at `FUNNELSWIFT_URL` (default `http://localhost:8080`).

**Requires:** `FUNNELSWIFT_URL` environment variable.

### Credit System
Tenant credits for platform-level actions — fully wired and production-ready:
- **Balance** — current available credits + plan limits (monthly allowance, overdraft)
- **History** — paginated transaction log with type, amount, description
- **Top-up** — buy credits via Stripe checkout (creates session, webhook confirms)
- **Admin adjust** — manual credit adjustment (admin only)
- **Deduct** — programmatic deduction (internal helper for cross-module use)
- **SMS inbound** — webhook-based credit action triggers

Credits are tracked at the account level with `credits_balance` and `credits_lifetime_used` columns. Plan tiers define `credits_monthly` and `credits_overdraft` via the `features` JSONB field.

### CORS Configuration
IncentiveSwift uses predicate-based CORS — allowed origins are loaded at startup from the `ALLOWED_ORIGINS` environment variable (comma-separated list). Requests from non-matching origins are rejected. Default allowed origins include:
- `zaarhub.com`, `www.zaarhub.com`
- `funnelswift.net`, `www.funnelswift.net`
- `incentiveswift.com`, `www.incentiveswift.com`
- `localhost:5173`, `localhost:3000` (local dev)

All methods and headers are permitted. Update `ALLOWED_ORIGINS` in the environment to add or remove tenants.

## Campaign Theming (Campaign-Level Color Scheme)

Every campaign can carry its own **theme** (color scheme) stored at `surface_config.theme`. When no `theme` key is present, the surface_config object itself is read for theme keys (backwards compatible). The theme renders live into the widget JS, widget config, and embed views, and is editable from the **admin SPA → Campaign → Theme** tab.

### Theme Object Keys

| Key | Default | Description |
|---|---|---|
| `primary_color` | `#2563eb` | Primary brand color (buttons, CTAs) → CSS var `--is-primary` |
| `accent_color` | `#7c3aed` | Accent/highlight color → CSS var `--is-accent` |
| `background_color` | `#ffffff` | Widget background → CSS var `--is-bg` |
| `text_color` | `#1e293b` | Text color → CSS var `--is-text` |
| `button_text_color` | `#ffffff` | Button label color → CSS var `--is-btn-text` |
| `font_family` | `Inter, system-ui, sans-serif` | Font stack → CSS var `--is-font` |
| `border_radius` | `12` | Corner radius (px) → CSS var `--is-radius` |
| `dark_mode` | `false` | Dark-mode flag |

Hex colors are validated/normalized (`#rgb` expanded to `#rrggbb`); invalid values fall back to the default. Missing keys fall back to defaults so existing surfaces never break.

### Where the Theme Renders (all live, verified)

| Surface | Endpoint | Renders |
|---|---|---|
| Widget JS | `GET /api/v1/widget/:hash` | Injects a `<style>` block with `--is-primary:...` vars + `theme` object |
| Widget config | `GET /api/v1/widget/:hash/config` | Returns resolved `theme` object + raw `surface_config.theme` |
| Embed views | `GET /api/v1/embed/campaign/:slug`, `GET /api/v1/embed/:id` | Injects theme CSS vars into the embed HTML `<style data-incentiveswift-theme>` |
| Admin save path | `PUT /api/v1/campaigns/:slug` (body `theme`) | `update_campaign` → `merge_campaign_theme` deep-merges into `surface_config.theme` |

**Admin SPA:** the Campaign editor has a **Theme** tab with a color picker per key (`input[type=color]`) and font/radius selects, saving via `API.put('/campaigns/' + slug, { theme })`. Theme changes reflect immediately in the widget/embed render.

**Verified live (off-peak 2026-08-19):** a campaign created with `theme: { primary_color: "#ff0000" }` rendered `--is-primary:#ff0000` in the widget JS, `theme.primary_color = #ff0000` in the widget config, and the equivalent CSS vars in both embed views. Updating the theme re-rendered instantly.

## Plan Tiers, Features & Mechanic Gating

Plan tiers (`plan_tiers`) define **feature access** through the `tier_features` join table (feature definitions live in `features`). This is the single source of truth for feature gating — the legacy `plans.features` JSONB is no longer consulted.

### Feature Model

- `plan_tiers` — base tier (name, slug, price, `max_campaigns`, `max_entries_per_month`, `is_active`)
- `features` — feature definitions by `key` (e.g. `mechanic_spin_wheel`, `all_mechanics`, `surface_*`, `module_*`)
- `tier_features` — `(tier_id, feature_id, enabled, limit_value)`; per-tier grants

### Mechanic Access Resolution

`has_mechanic_access` (in `access/feature_gate.rs`) resolves a mechanic like `spin_wheel`:

1. `mechanic_spin_wheel` row `enabled = true` → **allow**
2. `mechanic_spin_wheel` row `enabled = false` → **deny** (explicit disable overrides catch-all)
3. No row → allow iff **`all_mechanics`** is enabled

At play time, `enforce_mechanic_feature` returns **402 Upgrade Required** when the account's tier lacks the mechanic. Every game mechanic (spin wheel, scratch card, mystery, countdown, poll, chat, long-form qualifier, leaderboard, personality, calculator, raffle, score reveal, quiz) is gated this way.

### Admin UI (Plan Tiers Tab)

The admin SPA **Plan Tiers** tab (wired to `tier_features` CRUD endpoints) lets admins:
- Create/rename tiers, set pricing and numeric limits
- Toggle per-tier features (`mechanic_*`, `all_mechanics`, modules, surfaces) on/off
- Set `limit_value` for numeric features

This replaced the previous `plan_tier_features`/`feature_limits` tables (removed — see reconciliation note in `src/features.rs`).

## Setup for a New Tenant

1. **Create an account** — via API or direct DB
2. **Configure branded currency** — set `currency_name`, `currency_icon`, `currency_color`
3. **Create a campaign** — set type, slug, delivery config
4. **Create loyalty programs** — define earning rules per campaign
5. **Create reward tiers** — set redemption costs and approval type
6. **Configure rotation groups** — create rotation configs and add businesses
7. **Businesses submit pledges** → admin reviews → approved pledges join the network
8. **System auto-issues vouchers** on purchase verification
9. **Set up webhooks** — configure Marketing Boost and delivery config for real-time event notifications

## Support Tickets, Reviews & Calendar Modules

The admin dashboard includes three support modules added with the ticketing/reviews/calendar feature set. All are scoped per-tenant (owned by the calling account).

### Support Tickets
Customer/support request tracking with an internal-note thread.

- **View:** `Support Tickets` (sidebar 🎫)
- Create a ticket (subject, description, priority, category), filter by status (all / open / in_progress / resolved / closed), open a detail drawer, post internal notes, and flip status (`Set In Progress / Resolved / Closed`).
- **API:**
  - `GET /api/v1/support-tickets` — list
  - `POST /api/v1/support-tickets` — create `{ subject, description?, priority?, category?, campaign_id?, contact_id? }`
  - `GET /api/v1/support-tickets/:id` — ticket + message thread
  - `PUT /api/v1/support-tickets/:id` — update `{ status?, priority?, category?, assignee_id?, subject? }` (status `resolved`/`closed` stamps `resolved_at`)
  - `DELETE /api/v1/support-tickets/:id`
  - `POST /api/v1/support-tickets/:id/messages` — add message `{ body, is_internal? }`

### Reviews & Ratings
Customer reviews with a 1–5 rating and approve/reject moderation.

- **View:** `Reviews & Ratings` (sidebar ⭐) — shows count, average rating, pending approvals; create review, approve/reject, delete.
- **API:**
  - `GET /api/v1/reviews` — list + `{ count, average_rating }` aggregates
  - `POST /api/v1/reviews` — create `{ rating (1..5), title?, body?, reviewer_name?, campaign_id?, contact_id?, status? }`
  - `PUT /api/v1/reviews/:id` — moderate `{ status?, moderation_note?, ... }`
  - `DELETE /api/v1/reviews/:id`

### Calendar Events
Schedule/event tracking per tenant (event / reminder / appointment), optional campaign link.

- **View:** `Calendar` (sidebar 📅) — upcoming/past/completed counts, create event, confirm/complete/cancel, delete.
- **API:**
  - `GET /api/v1/calendar-events?from=&to=` — list (RFC3339 range filter)
  - `POST /api/v1/calendar-events` — create `{ title, starts_at, ends_at?, event_type?, all_day?, color?, campaign_id?, contact_id? }`
  - `PUT /api/v1/calendar-events/:id` — update `{ title?, status?, color?, starts_at?, ends_at?, ... }`
  - `DELETE /api/v1/calendar-events/:id`

> **Note:** pass range timestamps in `Z` / RFC3339 format (e.g. `2026-08-21T12:00:00Z`). A raw unencoded `+00:00` in the URL will be treated as a space and fail to parse.

## API Endpoints

### Authentication
| Endpoint | Method | Description |
|---|---|---|
| `/api/v1/auth/register` | POST | Register new user |
| `/api/v1/auth/login` | POST | Login, returns JWT |
| `/api/v1/auth/me` | GET | Verify token |
| `/api/v1/auth/profile` | PUT | Update profile |
| `/api/v1/auth/password` | PUT | Change password |

### Loyalty — PIN Purchase Verification
| Endpoint | Method | Description |
|---|---|---|
| `/api/v1/loyalty/generate-pin` | POST | Business generates PIN for customer |
| `/api/v1/loyalty/verify-purchase` | POST | Consumer enters PIN to verify purchase |
| `/api/v1/loyalty/issue-voucher` | POST | Issue a cross-promo voucher |
| `/api/v1/loyalty/my-vouchers/:contact_id` | GET | List active vouchers for a contact |
| `/api/v1/loyalty/claim-voucher` | POST | Redeem a voucher by code |
| `/api/v1/loyalty/expire-vouchers` | POST | Expire overdue vouchers (cron) |

### Loyalty — Business Pledges
| Endpoint | Method | Description |
|---|---|---|
| `/api/v1/business/pledge` | POST | Submit a reward offer |
| `/api/v1/business/pledges/:business_id` | GET | List pledges for a business |
| `/api/v1/admin/pledges` | GET | List pending pledges for review |
| `/api/v1/admin/pledges/:id/review` | POST | Approve or reject a pledge |

### Loyalty — Reward Redemption
| Endpoint | Method | Description |
|---|---|---|
| `/api/v1/loyalty/redeem-reward` | POST | Redeem points for a reward |
| `/api/v1/loyalty/rewards-earned/:contact_id` | GET | List rewards earned by a contact |

### Credit System
| Endpoint | Method | Auth | Description |
|---|---|---|---|
| `/api/v1/credits/balance` | GET | User | Current balance + plan limits (credits_monthly, credits_overdraft) |
| `/api/v1/credits/history` | GET | User | Paginated transaction history (type, amount, description) |
| `/api/v1/credits/topup` | POST | User | Create Stripe checkout to buy credits |
| `/api/v1/webhooks/sms/` | POST | None | SMS-based credit action triggers |
| `/api/v1/admin/credits/adjust` | POST | Admin | Manually adjust any user's credits (amount, reason) |

### Campaign Management
| Endpoint | Method | Description |
|---|---|---|
| `/api/v1/campaigns` | GET | List campaigns |
| `/api/v1/campaigns/:slug/leaderboard` | GET | Campaign referral leaderboard |
| `/api/v1/campaigns/:slug/milestones` | GET/POST | List/create milestone rewards |
| `/api/v1/campaigns/:slug/milestones/:id` | PUT/DELETE | Update/delete milestone |
| `/api/v1/campaigns/:slug/questions` | GET/POST | Quiz trivia questions |
| `/api/v1/campaigns/:slug/custom-fields` | GET/POST | Custom entry fields |
| `/api/v1/campaigns/:slug/marketing-boost` | GET/PUT | Marketing webhook config |

### Referral / Viral Engine
| Endpoint | Method | Description |
|---|---|---|
| `/api/v1/campaigns/:slug/referral-codes` | POST | Generate referral code |
| `/api/v1/campaigns/:slug/referral-stats` | GET | Referral stats + list |
| `/api/v1/campaigns/:slug/earn-channels` | GET/POST | List/create earn channels |
| `/api/v1/campaigns/:slug/earn-channels/:id` | PATCH/DELETE | Update/delete channel |
| `/api/v1/campaigns/:slug/earn/verify` | POST | Verify an earn action |
| `/api/v1/earn/:channel_code` | GET | Public earn click-through (tracks + awards points) |
| `/api/v1/c/:campaign_slug` | GET | Public campaign share link |

### Secret Codes (Promo Codes)
| Endpoint | Method | Description |
|---|---|---|
| `/api/v1/loyalty/secret-codes` | GET/POST | List/create secret codes |
| `/api/v1/loyalty/secret-codes/:id` | DELETE | Delete a code |
| `/api/v1/loyalty/secret-code/verify` | POST | Verify a code entry |
| `/api/v1/campaigns/:campaign_id/secret-codes` | GET/POST | Campaign-scoped codes |
| `/api/v1/campaigns/:campaign_id/redeem-code` | POST | Redeem a code against campaign |

### Rotation Config (Cross-Promotion)
| Endpoint | Method | Description |
|---|---|---|
| `/api/v1/admin/rotation-configs` | POST | Create rotation config |
| `/api/v1/admin/rotation-configs/:campaign_slug` | GET | List configs for campaign |
| `/api/v1/admin/rotation-members` | POST | Add business to rotation |
| `/api/v1/admin/rotation-members/:config_id` | GET | List members in config |
| `/api/v1/admin/rotation-members/:config_id/:business_id` | DELETE | Remove member |

### Admin
| Endpoint | Method | Description |
|---|---|---|
| `/api/v1/admin/portfolio-sync` | POST | Cross-app portfolio sync |
| `/api/v1/admin/impersonate` | POST | Switch to another tenant |
| `/api/v1/admin/tenants` | GET | List all tenants |
| `/api/v1/admin/plans` | GET/POST | List/create plan tiers |
| `/api/v1/admin/plans/:id` | PUT/DELETE | Update/delete a plan tier |
| `/api/v1/admin/plans/:id/features` | PUT/POST | Update/set plan feature mapping (legacy alias) |
| `/api/v1/admin/tiers` | GET | List plan tiers (tier_handler) |
| `/api/v1/admin/tiers/:tier_id` | PUT | Update a tier |
| `/api/v1/admin/tiers/:tier_id/features` | GET | List features for a tier (tier_features) |
| `/api/v1/admin/tiers/:tier_id/features/:feature_key` | PUT | Enable/disable a feature for a tier (canonical tier_features CRUD) |
| `/api/v1/admin/credits/adjust` | POST | Adjust user credits |

### Admin Impersonation

Admins can impersonate any portfolio company to manage their campaigns directly.

**How to use:**
1. Navigate to **Admin → Portfolio Co.** in the sidebar
2. Find the company you want to manage
3. Click the **👤 Login As** button in their row
4. Confirm the impersonation dialog

**What happens when you impersonate:**
- You immediately switch to the company's view — their dashboard, campaigns, loyalty programs, etc.
- A yellow ⚠️ banner appears at the top: *"Impersonating: [Company Name] | [Stop Impersonation]"*
- All API calls are authenticated as the portfolio company until you stop

**How to stop:**
- Click the **❌ Stop Impersonation** button in the yellow banner
- The page reloads and you return to your original admin account

**Important notes:**
- Impersonation tokens expire after **1 hour**
- Your original admin token is preserved in browser storage and restored on stop
- The banner persists across page navigation so you never lose track

| Endpoint | Method | Description |
|---|---|---|
| `/api/v1/admin/impersonate` | POST | Start impersonating (`{ account_id }` → returns JWT with `impersonating` claim) |
| `/api/v1/admin/stop-impersonation` | POST | End impersonation, restore original identity |

### Spin / Prize Draw
| Endpoint | Method | Description |
|---|---|---|
| `/api/v1/campaigns/:slug/spin` | POST | Spin the wheel |
| `/api/v1/campaigns/:slug/spin-status` | GET | Check spin availability |
| `/api/v1/campaigns/:slug/wins` | GET | List wins for campaign |
| `/api/v1/campaigns/:slug/wins/:win_id/redeem` | POST | Redeem a win |

### Public Embed / Widget
| Endpoint | Method | Description |
|---|---|---|
| `/api/v1/play/:id` | GET | Public campaign play view |
| `/api/v1/widget/:hash` | GET | Embeddable widget JS |
| `/api/v1/embed/campaign/all` | GET | Embed campaign list |
| `/api/v1/embed/:id` | GET | Embed view |

## Webhook Events

IncentiveSwift fires webhooks for real-time event notifications. Configure via campaign `delivery_config.entry_webhook_url` or Marketing Boost per-campaign webhooks.

### Events

| Event | Trigger |
|---|---|
| `reward_redeemed` | Reward claimed by consumer |
| `purchase_verified` | PIN entered and verified |
| `voucher_issued` | Cross-promo voucher generated |
| `entry_created` | New campaign entry submitted |
| `milestone_achieved` | Consumer hit a milestone |

### Marketing Boost

A per-campaign webhook that fires on high-value events. Configured via the `marketing-boost` endpoint. Supports auth header passthrough and event filtering.

**Example payload:**
```json
{
  "event": "voucher_issued",
  "campaign_id": "uuid",
  "timestamp": "2026-07-20T00:00:00Z",
  "data": {
    "voucher_id": "uuid",
    "code": "ABC12345",
    "discount_value": "10% Off",
    "contact_id": "uuid",
    "source_business_id": "uuid",
    "target_business_id": "uuid"
  }
}
```

## Auto-Expire

Vouchers expire after 30 days (configurable per-issuance). Run `POST /api/v1/loyalty/expire-vouchers` periodically as a cron job (every 6 hours recommended).

## Reward Tiers Table (Example)

| Reward Name | Points Cost | Approval | Tags |
|---|---|---|---|
| Free Coffee Voucher | 100 | Auto | `free_coffee` |
| 25% Off Dining | 300 | Auto | `dining_discount` |
| VIP Service Credit ($100) | 1,000 | Admin | `vip_service` |
| Featured Spotlight (30 days) | 2,500 | Admin | `featured_spotlight` |

## B2B Credit Tiers (Example)

| Reward Name | Credits Cost | Approval | Tags |
|---|---|---|---|
| Featured Supplier Badge (30d) | 500 | Admin | `featured_supplier` |
| Newsletter Ad Placement | 1,500 | Admin | `newsletter_ad` |
| AI Lead Campaign (100 leads) | 3,000 | Admin | `lead_campaign` |
