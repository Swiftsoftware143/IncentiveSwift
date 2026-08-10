-- =============================================================================
-- IncentiveSwift: Loyalty Badge, Enrollment, QR & Integration Center
-- Phase 1–6: Complete ZaarHub loyalty integration
-- =============================================================================

-- 1. Loyalty enrollment table (tracks which entities are in the loyalty program)
--    This is separate from loyalty_members — enrollment is opt-in per entity type.
CREATE TABLE IF NOT EXISTS loyalty_enrollments (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    entity_type     TEXT NOT NULL CHECK (entity_type IN ('business', 'supplier', 'member')),
    entity_id       UUID NOT NULL,          -- business_id, supplier_id, or contact_id
    program_id      UUID NOT NULL REFERENCES loyalty_programs(id) ON DELETE CASCADE,
    enrolled_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    enrolled_by     UUID,                   -- who enrolled them (account_id)
    is_active       BOOLEAN NOT NULL DEFAULT true,
    deactivated_at  TIMESTAMPTZ,
    metadata        JSONB DEFAULT '{}'::jsonb,

    UNIQUE (entity_type, entity_id, program_id)
);

CREATE INDEX idx_loyalty_enrollments_program ON loyalty_enrollments(program_id);
CREATE INDEX idx_loyalty_enrollments_entity ON loyalty_enrollments(entity_type, entity_id);
CREATE INDEX idx_loyalty_enrollments_active ON loyalty_enrollments(is_active) WHERE is_active = true;

-- 2. Member QR codes (scannable loyalty card)
--    Each loyalty member gets a unique QR code tied to their member record.
ALTER TABLE loyalty_members ADD COLUMN IF NOT EXISTS qr_code TEXT;
ALTER TABLE loyalty_members ADD COLUMN IF NOT EXISTS qr_code_generated_at TIMESTAMPTZ;

CREATE INDEX IF NOT EXISTS idx_loyalty_members_qr ON loyalty_members(qr_code) WHERE qr_code IS NOT NULL;

-- 3. Loyalty scan log (every QR scan = a tracked event)
CREATE TABLE IF NOT EXISTS loyalty_scans (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    member_id       UUID NOT NULL REFERENCES loyalty_members(id) ON DELETE CASCADE,
    business_id     UUID,                   -- business that performed the scan
    business_name   TEXT,
    program_id      UUID NOT NULL REFERENCES loyalty_programs(id) ON DELETE CASCADE,
    scan_type       TEXT NOT NULL DEFAULT 'checkin' CHECK (scan_type IN ('checkin', 'purchase', 'redemption', 'reward_claim')),
    points_awarded  INTEGER NOT NULL DEFAULT 0,
    points_balance  INTEGER NOT NULL DEFAULT 0,
    deal_applied    TEXT,                   -- which deal was applied
    metadata        JSONB DEFAULT '{}'::jsonb,
    scanned_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_loyalty_scans_member ON loyalty_scans(member_id, scanned_at DESC);
CREATE INDEX idx_loyalty_scans_business ON loyalty_scans(business_id, scanned_at DESC);
CREATE INDEX idx_loyalty_scans_program ON loyalty_scans(program_id, scanned_at DESC);
CREATE INDEX idx_loyalty_scans_type ON loyalty_scans(scan_type);

-- 4. Business deals for loyalty program
--    Business owners list deals they want to include in the loyalty program.
CREATE TABLE IF NOT EXISTS business_loyalty_deals (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    business_id     UUID NOT NULL,
    business_name   TEXT NOT NULL,
    program_id      UUID NOT NULL REFERENCES loyalty_programs(id) ON DELETE CASCADE,
    deal_type       TEXT NOT NULL CHECK (deal_type IN ('discount_percent', 'fixed_amount', 'free_item', 'bogo', 'bonus_points')),
    deal_value      TEXT NOT NULL,          -- e.g. "10%", "$5", "2x points"
    deal_description TEXT,
    min_purchase    TEXT,                   -- e.g. "$20 minimum"
    points_required INTEGER NOT NULL DEFAULT 0,  -- points needed to redeem
    is_active       BOOLEAN NOT NULL DEFAULT true,
    valid_from      TIMESTAMPTZ DEFAULT now(),
    valid_until     TIMESTAMPTZ,
    redemptions_limit INTEGER NOT NULL DEFAULT 0,  -- 0 = unlimited
    current_redemptions INTEGER NOT NULL DEFAULT 0,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_business_deals_program ON business_loyalty_deals(program_id);
CREATE INDEX idx_business_deals_business ON business_loyalty_deals(business_id);
CREATE INDEX idx_business_deals_active ON business_loyalty_deals(is_active) WHERE is_active = true;

-- 5. Loyalty community events
CREATE TABLE IF NOT EXISTS loyalty_events (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    program_id      UUID NOT NULL REFERENCES loyalty_programs(id) ON DELETE CASCADE,
    title           TEXT NOT NULL,
    description     TEXT,
    event_type      TEXT NOT NULL DEFAULT 'general' CHECK (event_type IN ('general', 'sale', 'workshop', 'meetup', 'holiday', 'promotion')),
    location        TEXT,
    event_date      TIMESTAMPTZ NOT NULL,
    end_date        TIMESTAMPTZ,
    is_active       BOOLEAN NOT NULL DEFAULT true,
    created_by      UUID,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_loyalty_events_program ON loyalty_events(program_id, event_date);
CREATE INDEX idx_loyalty_events_active ON loyalty_events(is_active) WHERE is_active = true;

-- 6. Integration Center: service-scoped API keys for businesses/suppliers
--    Each business gets unique keys for each service they use.
ALTER TABLE api_keys ADD COLUMN IF NOT EXISTS service_type TEXT;
ALTER TABLE api_keys ADD COLUMN IF NOT EXISTS owner_id UUID;
ALTER TABLE api_keys ADD COLUMN IF NOT EXISTS owner_type TEXT CHECK (owner_type IN ('business', 'supplier', 'account'));

CREATE INDEX IF NOT EXISTS idx_api_keys_owner ON api_keys(owner_type, owner_id);
CREATE INDEX IF NOT EXISTS idx_api_keys_service ON api_keys(service_type);
