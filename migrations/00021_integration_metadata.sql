-- Migration 00021: Add provider_metadata to integration_targets
-- Stores provider-specific configuration per integration target
-- e.g., Marketing Boost offer_id, Mailgun sending domain, etc.
ALTER TABLE integration_targets
    ADD COLUMN IF NOT EXISTS provider_metadata JSONB NOT NULL DEFAULT '{}';

-- Add offer_id to campaign_integrations for per-binding offer selection
ALTER TABLE campaign_integrations
    ADD COLUMN IF NOT EXISTS provider_metadata JSONB NOT NULL DEFAULT '{}';

-- Update index
DROP INDEX IF EXISTS idx_integration_targets_provider_metadata;
CREATE INDEX IF NOT EXISTS idx_integration_targets_provider_metadata
    ON integration_targets USING GIN (provider_metadata);
