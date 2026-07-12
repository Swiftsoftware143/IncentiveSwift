-- Entry Form Fields Migration
-- Adds website and notes2 to contacts, and creates campaign_custom_fields table

-- Add website column to contacts (text, nullable)
ALTER TABLE contacts ADD COLUMN IF NOT EXISTS website text;

-- Add notes2 column to contacts (text, nullable)
ALTER TABLE contacts ADD COLUMN IF NOT EXISTS notes2 text;

-- Create campaign_custom_fields table for per-campaign entry form custom fields
CREATE TABLE IF NOT EXISTS campaign_custom_fields (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    campaign_id uuid NOT NULL REFERENCES campaigns(id) ON DELETE CASCADE,
    field_key text NOT NULL,
    field_label text NOT NULL,
    field_type text NOT NULL DEFAULT 'text', -- text, email, phone, select, checkbox, textarea
    sort_order int NOT NULL DEFAULT 0,
    required boolean NOT NULL DEFAULT false,
    options text[] DEFAULT '{}', -- for select/checkbox
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_custom_fields_campaign ON campaign_custom_fields(campaign_id);
