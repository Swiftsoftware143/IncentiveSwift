-- Add source tracking columns to entries
ALTER TABLE entries ADD COLUMN IF NOT EXISTS utm_source text;
ALTER TABLE entries ADD COLUMN IF NOT EXISTS utm_medium text;
ALTER TABLE entries ADD COLUMN IF NOT EXISTS utm_campaign text;
ALTER TABLE entries ADD COLUMN IF NOT EXISTS referrer_url text;
ALTER TABLE entries ADD COLUMN IF NOT EXISTS page_url text;
ALTER TABLE entries ADD COLUMN IF NOT EXISTS user_agent text;
ALTER TABLE entries ADD COLUMN IF NOT EXISTS ip_address text;

-- Add conversion tracking columns to campaign_wins
ALTER TABLE campaign_wins ADD COLUMN IF NOT EXISTS converted_to_action boolean DEFAULT false;
ALTER TABLE campaign_wins ADD COLUMN IF NOT EXISTS converted_at timestamp with time zone;

-- Add daily stats materialized view
CREATE TABLE IF NOT EXISTS campaign_daily_stats (
  id uuid DEFAULT gen_random_uuid() PRIMARY KEY,
  campaign_id uuid NOT NULL REFERENCES campaigns(id) ON DELETE CASCADE,
  stat_date date NOT NULL DEFAULT CURRENT_DATE,
  total_entries integer DEFAULT 0,
  unique_contacts integer DEFAULT 0,
  total_wins integer DEFAULT 0,
  total_losses integer DEFAULT 0,
  total_redemptions integer DEFAULT 0,
  utm_sources jsonb DEFAULT '{}'::jsonb,
  referrer_domains jsonb DEFAULT '{}'::jsonb,
  hourly_breakdown jsonb DEFAULT '{}'::jsonb,
  UNIQUE(campaign_id, stat_date)
);
