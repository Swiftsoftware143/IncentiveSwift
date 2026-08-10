-- Migration 00015: Loyalty Overhaul — Tiers, Milestones, Activity, Streaks, Referrals

-- ============== NEW TABLES ==============

-- Loyalty Tiers (Bronze/Silver/Gold/Platinum)
CREATE TABLE IF NOT EXISTS loyalty_tiers (
  id uuid DEFAULT gen_random_uuid() PRIMARY KEY,
  loyalty_program_id uuid NOT NULL REFERENCES loyalty_programs(id) ON DELETE CASCADE,
  name varchar(100) NOT NULL,
  min_points bigint NOT NULL DEFAULT 0,
  color varchar(7) NOT NULL DEFAULT '#6B7280',
  perks jsonb DEFAULT '[]'::jsonb,
  multiplier decimal(5,2) NOT NULL DEFAULT 1.0,
  created_at timestamptz NOT NULL DEFAULT now()
);

-- Loyalty Milestones
CREATE TABLE IF NOT EXISTS loyalty_milestones (
  id uuid DEFAULT gen_random_uuid() PRIMARY KEY,
  loyalty_program_id uuid NOT NULL REFERENCES loyalty_programs(id) ON DELETE CASCADE,
  name varchar(200) NOT NULL,
  trigger_type varchar(50) NOT NULL,
  trigger_value bigint NOT NULL DEFAULT 0,
  bonus_points bigint NOT NULL DEFAULT 0,
  bonus_reward_id uuid REFERENCES loyalty_reward_tiers(id) ON DELETE SET NULL,
  once_per_member boolean NOT NULL DEFAULT true,
  created_at timestamptz NOT NULL DEFAULT now()
);

-- Loyalty Activity Log
CREATE TABLE IF NOT EXISTS loyalty_activity (
  id uuid DEFAULT gen_random_uuid() PRIMARY KEY,
  member_id uuid NOT NULL REFERENCES loyalty_members(id) ON DELETE CASCADE,
  activity_type varchar(50) NOT NULL,
  description text,
  points_earned bigint NOT NULL DEFAULT 0,
  created_at timestamptz NOT NULL DEFAULT now()
);

-- Milestones completed by members
CREATE TABLE IF NOT EXISTS loyalty_milestones_completed (
  id uuid DEFAULT gen_random_uuid() PRIMARY KEY,
  member_id uuid NOT NULL REFERENCES loyalty_members(id) ON DELETE CASCADE,
  milestone_id uuid NOT NULL REFERENCES loyalty_milestones(id) ON DELETE CASCADE,
  points_awarded bigint NOT NULL DEFAULT 0,
  completed_at timestamptz NOT NULL DEFAULT now(),
  UNIQUE(member_id, milestone_id)
);

-- ============== ALTER EXISTING TABLES ==============

-- Add new columns to loyalty_members
ALTER TABLE loyalty_members ADD COLUMN IF NOT EXISTS tier_id uuid REFERENCES loyalty_tiers(id) ON DELETE SET NULL;
ALTER TABLE loyalty_members ADD COLUMN IF NOT EXISTS current_streak integer NOT NULL DEFAULT 0;
ALTER TABLE loyalty_members ADD COLUMN IF NOT EXISTS longest_streak integer NOT NULL DEFAULT 0;
ALTER TABLE loyalty_members ADD COLUMN IF NOT EXISTS last_activity_date timestamptz;
ALTER TABLE loyalty_members ADD COLUMN IF NOT EXISTS birthday date;
ALTER TABLE loyalty_members ADD COLUMN IF NOT EXISTS referral_code varchar(50) UNIQUE;
ALTER TABLE loyalty_members ADD COLUMN IF NOT EXISTS total_referrals integer NOT NULL DEFAULT 0;

-- Add new columns to loyalty_programs
ALTER TABLE loyalty_programs ADD COLUMN IF NOT EXISTS tiers_enabled boolean NOT NULL DEFAULT false;
ALTER TABLE loyalty_programs ADD COLUMN IF NOT EXISTS milestones_enabled boolean NOT NULL DEFAULT false;
ALTER TABLE loyalty_programs ADD COLUMN IF NOT EXISTS streak_enabled boolean NOT NULL DEFAULT false;
ALTER TABLE loyalty_programs ADD COLUMN IF NOT EXISTS streak_bonus integer NOT NULL DEFAULT 0;
ALTER TABLE loyalty_programs ADD COLUMN IF NOT EXISTS streak_days integer NOT NULL DEFAULT 7;
ALTER TABLE loyalty_programs ADD COLUMN IF NOT EXISTS referral_bonus integer NOT NULL DEFAULT 0;
ALTER TABLE loyalty_programs ADD COLUMN IF NOT EXISTS birthday_bonus integer NOT NULL DEFAULT 0;
ALTER TABLE loyalty_programs ADD COLUMN IF NOT EXISTS points_expire_days integer NOT NULL DEFAULT 365;
ALTER TABLE loyalty_programs ADD COLUMN IF NOT EXISTS social_share_points integer NOT NULL DEFAULT 0;
ALTER TABLE loyalty_programs ADD COLUMN IF NOT EXISTS points_per_visit integer NOT NULL DEFAULT 5;

-- Loyalty Online Actions (cookie, social share, referral tracking)
CREATE TABLE IF NOT EXISTS loyalty_online_actions (
  id uuid DEFAULT gen_random_uuid() PRIMARY KEY,
  member_id uuid NOT NULL REFERENCES loyalty_members(id) ON DELETE CASCADE,
  action_type varchar(50) NOT NULL, -- 'daily_visit', 'social_share', 'referral_click', 'newsletter_open', 'link_click'
  points_earned bigint NOT NULL DEFAULT 0,
  metadata jsonb DEFAULT '{}'::jsonb, -- stores url shared, platform, referrer, etc.
  created_at timestamptz NOT NULL DEFAULT now()
);

-- Indexes for online actions
CREATE INDEX IF NOT EXISTS idx_loyalty_online_actions_member ON loyalty_online_actions(member_id);
CREATE INDEX IF NOT EXISTS idx_loyalty_online_actions_type ON loyalty_online_actions(action_type);
CREATE INDEX IF NOT EXISTS idx_loyalty_online_actions_created ON loyalty_online_actions(created_at DESC);

-- ============== INDEXES ==============
CREATE INDEX IF NOT EXISTS idx_loyalty_tiers_program ON loyalty_tiers(loyalty_program_id);
CREATE INDEX IF NOT EXISTS idx_loyalty_milestones_program ON loyalty_milestones(loyalty_program_id);
CREATE INDEX IF NOT EXISTS idx_loyalty_activity_member ON loyalty_activity(member_id);
CREATE INDEX IF NOT EXISTS idx_loyalty_activity_type ON loyalty_activity(activity_type);
CREATE INDEX IF NOT EXISTS idx_loyalty_activity_created ON loyalty_activity(created_at DESC);
CREATE INDEX IF NOT EXISTS idx_loyalty_members_streak ON loyalty_members(current_streak);
CREATE INDEX IF NOT EXISTS idx_loyalty_members_tier ON loyalty_members(tier_id);
CREATE INDEX IF NOT EXISTS idx_loyalty_members_referral ON loyalty_members(referral_code);
CREATE INDEX IF NOT EXISTS idx_loyalty_milestones_completed_member ON loyalty_milestones_completed(member_id);
