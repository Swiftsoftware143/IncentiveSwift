-- Migration: Add surface_config column to campaigns table
-- Stores tablet, widget, full-page settings per campaign

ALTER TABLE public.campaigns ADD COLUMN IF NOT EXISTS surface_config JSONB DEFAULT '{}';

COMMENT ON COLUMN public.campaigns.surface_config IS 'Surface engagement config: { "tablet": {...}, "widget": {...}, "full_page": {...} }';
