-- Migration: Calendar Events (IncentiveSwift)
-- Schedule/appointment events per tenant, optionally linked to a campaign.
CREATE TABLE IF NOT EXISTS calendar_events (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id     UUID,
    campaign_id   UUID,                          -- optional linked campaign
    contact_id    UUID,                          -- optional linked contact
    title         TEXT NOT NULL,
    description   TEXT,
    location      TEXT,
    event_type    TEXT DEFAULT 'event',          -- event | reminder | appointment
    starts_at     TIMESTAMPTZ NOT NULL,
    ends_at       TIMESTAMPTZ,
    all_day       BOOLEAN NOT NULL DEFAULT false,
    status        TEXT NOT NULL DEFAULT 'scheduled', -- scheduled | confirmed | cancelled | completed
    color         TEXT,
    created_by    UUID,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_calendar_events_tenant ON calendar_events (tenant_id);
CREATE INDEX IF NOT EXISTS idx_calendar_events_start ON calendar_events (starts_at);
CREATE INDEX IF NOT EXISTS idx_calendar_events_campaign ON calendar_events (campaign_id);
