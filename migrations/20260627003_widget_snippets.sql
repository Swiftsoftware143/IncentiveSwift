-- Migration: Widget embed snippets

CREATE TABLE IF NOT EXISTS public.widget_snippets (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    campaign_id UUID NOT NULL REFERENCES public.campaigns(id) ON DELETE CASCADE,
    snippet_hash TEXT UNIQUE NOT NULL,
    is_active BOOLEAN DEFAULT true,
    created_at TIMESTAMPTZ DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_widget_snippets_campaign ON widget_snippets(campaign_id);
