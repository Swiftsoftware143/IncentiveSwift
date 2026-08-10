-- Campaign Integrations Hub & Prize Delivery
-- Phase 2: Bind integrations to campaigns, add redemption codes

-- ============================================================
-- Add redemption_code to campaign_wins
-- ============================================================
ALTER TABLE campaign_wins
  ADD COLUMN IF NOT EXISTS redemption_code text;

CREATE INDEX IF NOT EXISTS idx_campaign_wins_redemption_code ON campaign_wins(redemption_code);

-- ============================================================
-- CAMPAIGN INTEGRATIONS (bind integration_targets to campaigns)
-- ============================================================
CREATE TABLE IF NOT EXISTS campaign_integrations (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    campaign_id uuid NOT NULL REFERENCES campaigns(id) ON DELETE CASCADE,
    integration_id uuid NOT NULL REFERENCES integration_targets(id) ON DELETE CASCADE,
    trigger_events text[] NOT NULL DEFAULT '{}',
    enabled boolean NOT NULL DEFAULT true,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (campaign_id, integration_id)
);

CREATE INDEX IF NOT EXISTS idx_campaign_integrations_campaign ON campaign_integrations(campaign_id);
CREATE INDEX IF NOT EXISTS idx_campaign_integrations_integration ON campaign_integrations(integration_id);
CREATE INDEX IF NOT EXISTS idx_campaign_integrations_enabled ON campaign_integrations(campaign_id, enabled);
