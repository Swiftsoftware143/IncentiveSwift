-- Migration 00016: Secret Codes for Loyalty Check-In
-- Allows admins to create one-time or limited-use secret codes that
-- members can enter on the check-in page to earn points.
-- Perfect for: Facebook group codes, newsletter codes, social posts.

-- Secret codes table
CREATE TABLE IF NOT EXISTS loyalty_secret_codes (
  id uuid DEFAULT gen_random_uuid() PRIMARY KEY,
  program_id uuid NOT NULL REFERENCES loyalty_programs(id) ON DELETE CASCADE,
  code varchar(64) NOT NULL,
  description varchar(255) DEFAULT '', -- e.g. "FB Group July 2026", "Newsletter #12"
  points_reward integer NOT NULL DEFAULT 25,
  max_uses integer NOT NULL DEFAULT 0, -- 0 = unlimited
  uses_so_far integer NOT NULL DEFAULT 0,
  starts_at timestamptz NOT NULL DEFAULT now(),
  expires_at timestamptz, -- NULL = never expires
  is_active boolean NOT NULL DEFAULT true,
  created_by uuid, -- admin user id
  created_at timestamptz NOT NULL DEFAULT now(),
  UNIQUE(program_id, code)
);

-- Track which members have used which codes
CREATE TABLE IF NOT EXISTS loyalty_secret_code_redemptions (
  id uuid DEFAULT gen_random_uuid() PRIMARY KEY,
  code_id uuid NOT NULL REFERENCES loyalty_secret_codes(id) ON DELETE CASCADE,
  member_id uuid NOT NULL REFERENCES loyalty_members(id) ON DELETE CASCADE,
  redeemed_at timestamptz NOT NULL DEFAULT now(),
  UNIQUE(code_id, member_id) -- one use per member per code
);

-- Indexes
CREATE INDEX IF NOT EXISTS idx_loyalty_secret_codes_program ON loyalty_secret_codes(program_id);
CREATE INDEX IF NOT EXISTS idx_loyalty_secret_codes_code ON loyalty_secret_codes(code);
CREATE INDEX IF NOT EXISTS idx_loyalty_secret_codes_active ON loyalty_secret_codes(is_active);
CREATE INDEX IF NOT EXISTS idx_loyalty_secret_code_redemptions_code ON loyalty_secret_code_redemptions(code_id);
CREATE INDEX IF NOT EXISTS idx_loyalty_secret_code_redemptions_member ON loyalty_secret_code_redemptions(member_id);

-- Update loyalty_programs to have a setting for secret code points
ALTER TABLE loyalty_programs ADD COLUMN IF NOT EXISTS points_per_secret_code integer NOT NULL DEFAULT 25;
