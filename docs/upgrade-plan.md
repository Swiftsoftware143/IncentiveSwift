# IncentiveSwift Upgrade — White-Label Loyalty Platform

## New Tables

### 1. vouchers
Tracks issued vouchers from the rotating cross-promo engine. When a consumer completes a purchase, a voucher is issued for a non-competing business.

```sql
CREATE TABLE vouchers (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    campaign_id UUID REFERENCES campaigns(id) ON DELETE CASCADE,
    issued_to_contact_id UUID REFERENCES contacts(id) ON DELETE CASCADE,
    source_business_id UUID,         -- business that earned the voucher
    target_business_id UUID,         -- business where voucher can be redeemed
    voucher_type TEXT DEFAULT 'discount',  -- discount, free_item, fixed_amount
    discount_value TEXT,              -- "15%", "$20", "Free coffee"
    redemption_code TEXT UNIQUE,      -- 8-char alphanumeric
    status TEXT DEFAULT 'active' CHECK (status IN ('active','used','expired')),
    expires_at TIMESTAMPTZ,
    used_at TIMESTAMPTZ,
    rotation_week INTEGER DEFAULT 0, -- which rotation position
    created_at TIMESTAMPTZ DEFAULT NOW()
);
```

### 2. business_pledges
Claimed businesses commit rewards to participate in the loyalty network. Requires admin approval.

```sql
CREATE TABLE business_pledges (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    campaign_id UUID REFERENCES campaigns(id) ON DELETE CASCADE,
    business_id UUID NOT NULL,
    business_name TEXT NOT NULL,
    offer_type TEXT NOT NULL,         -- discount_percent, fixed_amount, free_item
    offer_value TEXT NOT NULL,        -- "10", "$15", "Free consultation"
    offer_description TEXT,
    min_purchase TEXT,
    status TEXT DEFAULT 'pending' CHECK (status IN ('pending','approved','rejected','active','paused')),
    reviewed_by UUID,                 -- admin who approved
    reviewed_at TIMESTAMPTZ,
    valid_from TIMESTAMPTZ,
    valid_until TIMESTAMPTZ,
    redemptions_limit INTEGER,
    current_redemptions INTEGER DEFAULT 0,
    is_active BOOLEAN DEFAULT true,
    created_at TIMESTAMPTZ DEFAULT NOW()
);
```

### 3. purchase_verifications
Records proof-of-service (PIN code or receipt). This is how the purchase is confirmed and the reward cycle triggers.

```sql
CREATE TABLE purchase_verifications (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    campaign_id UUID REFERENCES campaigns(id) ON DELETE CASCADE,
    contact_id UUID REFERENCES contacts(id) ON DELETE SET NULL,
    business_id UUID NOT NULL,
    verification_type TEXT NOT NULL CHECK (verification_type IN ('pin','receipt','api')),
    pin_code TEXT,                    -- 4-digit code from business portal
    receipt_url TEXT,                 -- uploaded receipt photo
    status TEXT DEFAULT 'pending' CHECK (status IN ('pending','verified','rejected','expired')),
    verified_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ DEFAULT NOW()
);
```

### 4. rotation_config
Defines cross-promotion groups and rotation schedules per campaign.

```sql
CREATE TABLE rotation_config (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    campaign_id UUID REFERENCES campaigns(id) ON DELETE CASCADE,
    name TEXT NOT NULL,                -- "Home & Hearth Rotation"
    group_size INTEGER DEFAULT 4,      -- number of non-competing businesses
    rotation_frequency TEXT DEFAULT 'weekly' CHECK (rotation_frequency IN ('daily','weekly','biweekly','monthly')),
    max_vouchers_per_rotation INTEGER DEFAULT 1,
    is_active BOOLEAN DEFAULT true,
    created_at TIMESTAMPTZ DEFAULT NOW()
);
```

### 5. currency_config
Per-tenant branding for points/credits. This is what makes ZaarCash branded differently from another tenant's currency.

```sql
ALTER TABLE accounts ADD COLUMN IF NOT EXISTS currency_name TEXT DEFAULT 'Points';
ALTER TABLE accounts ADD COLUMN IF NOT EXISTS currency_icon TEXT DEFAULT '⭐';
ALTER TABLE accounts ADD COLUMN IF NOT EXISTS currency_color TEXT DEFAULT '#0d9488';
ALTER TABLE accounts ADD COLUMN IF NOT EXISTS b2b_currency_name TEXT DEFAULT 'Pro Credits';
ALTER TABLE accounts ADD COLUMN IF NOT EXISTS b2b_currency_icon TEXT DEFAULT '💼';
ALTER TABLE accounts ADD COLUMN IF NOT EXISTS b2b_currency_color TEXT DEFAULT '#2b3255';
```

## New Endpoints to Build

### Consumer-facing
- `POST /api/v1/loyalty/verify-purchase` — consumer enters PIN or uploads receipt
- `POST /api/v1/loyalty/claim-voucher` — consumer redeems an active voucher
- `GET /api/v1/loyalty/my-vouchers` — list active/expired vouchers for a user
- `GET /api/v1/loyalty/available-rewards` — what ZaarCash/Pro Credits can buy

### Business-facing
- `POST /api/v1/business/generate-pin` — business generates a 4-digit PIN for a customer purchase
- `POST /api/v1/business/pledge` — business submits a reward pledge
- `GET /api/v1/business/pledges` — view their pledges and status
- `GET /api/v1/business/voucher-redemptions` — see vouchers redeemed at their business

### Admin-facing
- `GET /api/v1/admin/pledges` — list pending pledges for approval
- `POST /api/v1/admin/pledges/:id/approve` — approve/reject a business pledge
- `POST /api/v1/admin/rotation-config` — configure rotation groups

## Existing Tables That Need Minor Changes

- `loyalty_reward_tiers` — add `redeem_action` column (what webhook to fire on redeem: "featured_listing", "newsletter_ad", "ai_lead_campaign")
- `campaigns` — add `currency_name`, `currency_icon` override (inherits from account if not set)

## What Does NOT Change

- `campaign_points_balance` — still tracks ZaarCash and Pro Credits
- `loyalty_programs` — still works for check-ins, streaks, milestones
- `spin_handler` — still works for spin-to-win
- `viral_handler` — still works for referral codes and share links
- Multi-tenant isolation via `account_id` — unchanged

## Implementation Order

1. Create new tables (vouchers, business_pledges, purchase_verifications, rotation_config)
2. Add currency branding columns to accounts
3. Build purchase verification endpoints (PIN generation + consumer verify)
4. Build voucher engine (issue, claim, expire)
5. Build business pledge flow + admin approval
6. Build rotation config
7. Create ZaarHub account with branded currencies
