-- Marketing Boost per-reward/per-prize config
-- Adds JSONB columns to loyalty_reward_tiers and campaign_prize_inventory

ALTER TABLE loyalty_reward_tiers
  ADD COLUMN IF NOT EXISTS marketing_boost jsonb;

ALTER TABLE campaign_prize_inventory
  ADD COLUMN IF NOT EXISTS marketing_boost jsonb;
