-- Migration: Tablet engagement sessions

CREATE TABLE IF NOT EXISTS public.tablet_sessions (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    campaign_id UUID NOT NULL REFERENCES public.campaigns(id) ON DELETE CASCADE,
    tenant_id UUID NOT NULL,
    device_id TEXT,
    interaction_count INTEGER DEFAULT 0,
    last_interaction_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_tablet_sessions_campaign ON tablet_sessions(campaign_id);
