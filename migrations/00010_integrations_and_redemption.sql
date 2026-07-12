-- Campaign Integration Hub & Redemption
-- ============================================================
-- Adds redemption_code to campaign_wins
-- Creates campaign_integrations binding table
-- Creates campaign_delivery_log for per-outcome delivery tracking

-- ============================================================
-- Add redemption_code to campaign_wins
-- ============================================================
ALTER TABLE campaign_wins ADD COLUMN IF NOT EXISTS redemption_code text;
CREATE INDEX IF NOT EXISTS idx_campaign_wins_redemption_code ON campaign_wins(redemption_code);

-- ============================================================
-- CAMPAIGN INTEGRATIONS (many-to-many between campaigns and integration_targets)
-- ============================================================
CREATE TABLE IF NOT EXISTS campaign_integrations (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    campaign_id uuid NOT NULL REFERENCES campaigns(id) ON DELETE CASCADE,
    integration_id uuid NOT NULL REFERENCES integration_targets(id) ON DELETE CASCADE,
    trigger_events text[] NOT NULL DEFAULT '{"on_win"}',
    enabled boolean NOT NULL DEFAULT true,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (campaign_id, integration_id)
);

CREATE INDEX IF NOT EXISTS idx_campaign_integrations_campaign ON campaign_integrations(campaign_id);
CREATE INDEX IF NOT EXISTS idx_campaign_integrations_integration ON campaign_integrations(integration_id);

-- ============================================================
-- CAMPAIGN EMAIL TEMPLATES (stored templates for prize delivery)
-- ============================================================
CREATE TABLE IF NOT EXISTS campaign_email_templates (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    campaign_id uuid NOT NULL REFERENCES campaigns(id) ON DELETE CASCADE,
    name text NOT NULL,
    trigger_event text NOT NULL DEFAULT 'on_win', -- 'on_win', 'on_lose', 'on_spin'
    subject_template text NOT NULL DEFAULT 'You won {{prize.label}}!',
    body_template text NOT NULL DEFAULT 'Congratulations! You won {{prize.label}}. Use code {{redemption.code}} to claim.',
    from_name text DEFAULT 'IncentiveSwift',
    cc_email text,
    bcc_email text,
    is_active boolean NOT NULL DEFAULT true,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_campaign_email_templates_campaign ON campaign_email_templates(campaign_id);

-- ============================================================
-- CAMPAIGN REDIRECT PAGES (post-spin landing pages)
-- ============================================================
CREATE TABLE IF NOT EXISTS campaign_redirect_pages (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    campaign_id uuid NOT NULL REFERENCES campaigns(id) ON DELETE CASCADE,
    trigger_event text NOT NULL DEFAULT 'on_win', -- 'on_win', 'on_lose', 'default'
    title text DEFAULT 'Thank You!',
    heading_text text,
    body_text text,
    button_text text DEFAULT 'Claim Your Prize',
    button_url text,
    confetti boolean DEFAULT true,
    background_color text DEFAULT '#0f1117',
    accent_color text DEFAULT '#a78bfa',
    is_active boolean NOT NULL DEFAULT true,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_campaign_redirect_pages_campaign ON campaign_redirect_pages(campaign_id);

-- ============================================================
-- PROVIDER KEYS TABLE (BYOK — bring your own key for email/SMS)
-- ============================================================
-- This already exists as `provider_keys`, verify it has all needed columns
ALTER TABLE provider_keys ADD COLUMN IF NOT EXISTS provider_type text DEFAULT 'email'; -- email, sms, webhook, autoresponder
