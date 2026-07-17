-- Migration 00020: Webhook Security — allowlisted domains + daily rate limits
-- Adds columns to integration_targets for URL allowlisting and per-target daily caps.

ALTER TABLE integration_targets
    ADD COLUMN IF NOT EXISTS allowed_domains TEXT[] DEFAULT '{}',
    ADD COLUMN IF NOT EXISTS daily_limit INT NOT NULL DEFAULT 1000,
    ADD COLUMN IF NOT EXISTS daily_reset_at TIMESTAMPTZ NOT NULL DEFAULT NOW();

-- Index for daily-limit counting queries
CREATE INDEX IF NOT EXISTS idx_delivery_log_target_date
    ON delivery_log (target, attempted_at);
