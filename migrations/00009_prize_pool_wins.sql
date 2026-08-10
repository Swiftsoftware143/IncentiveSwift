-- Prize Pool & Campaign Wins
-- Supports multi-spin weighted prize draws with inventory tracking

-- ============================================================
-- Add total_spins and last_spin_at to campaign_streaks
-- ============================================================
ALTER TABLE campaign_streaks
  ADD COLUMN IF NOT EXISTS total_spins integer NOT NULL DEFAULT 1,
  ADD COLUMN IF NOT EXISTS last_spin_at timestamptz NOT NULL DEFAULT now();

-- ============================================================
-- CAMPAIGN PRIZE INVENTORY (per-campaign, per-prize tracking)
-- ============================================================
CREATE TABLE IF NOT EXISTS campaign_prize_inventory (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    campaign_id uuid NOT NULL REFERENCES campaigns(id) ON DELETE CASCADE,
    prize_id text NOT NULL,
    label text NOT NULL,
    prize_type text NOT NULL DEFAULT 'coupon',
    total integer,
    remaining integer,
    claimed integer NOT NULL DEFAULT 0,
    color text DEFAULT '#6b7280',
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (campaign_id, prize_id)
);

-- ============================================================
-- CAMPAIGN WINS (every prize awarded)
-- ============================================================
CREATE TABLE IF NOT EXISTS campaign_wins (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    entry_id uuid REFERENCES entries(id) ON DELETE SET NULL,
    contact_id uuid NOT NULL REFERENCES contacts(id) ON DELETE CASCADE,
    campaign_id uuid NOT NULL REFERENCES campaigns(id) ON DELETE CASCADE,
    prize_id text NOT NULL,
    prize_label text NOT NULL,
    prize_type text NOT NULL DEFAULT 'coupon',
    streak_when_won integer NOT NULL DEFAULT 0,
    was_pity boolean NOT NULL DEFAULT false,
    redeemed boolean NOT NULL DEFAULT false,
    redeemed_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_campaign_wins_campaign ON campaign_wins(campaign_id);
CREATE INDEX IF NOT EXISTS idx_campaign_wins_contact ON campaign_wins(contact_id);
CREATE INDEX IF NOT EXISTS idx_campaign_wins_redeemed ON campaign_wins(campaign_id, redeemed);
CREATE INDEX IF NOT EXISTS idx_campaign_prize_inventory_campaign ON campaign_prize_inventory(campaign_id);
