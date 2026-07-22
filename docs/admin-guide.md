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
| `/api/v1/admin/credits/adjust` | POST | Adjust user credits |

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
