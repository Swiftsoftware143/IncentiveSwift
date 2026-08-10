-- loyalty_plans: subscription tiers for business loyalty program
CREATE TABLE IF NOT EXISTS loyalty_plans (
    id                UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name              TEXT NOT NULL,
    slug              TEXT NOT NULL UNIQUE,
    monthly_price     INT NOT NULL,             -- cents (1900 = $19.00)
    monthly_zc_pool   INT NOT NULL,
    overage_rate      INT NOT NULL DEFAULT 1,
    features          TEXT[],
    is_active         BOOLEAN NOT NULL DEFAULT true,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);

INSERT INTO loyalty_plans (name, slug, monthly_price, monthly_zc_pool, features) VALUES
  ('Starter',  'starter',  1900, 2000,  ARRAY['scanner','basic_offers']),
  ('Standard', 'standard', 4900, 6000,  ARRAY['scanner','offers','referrals','badges']),
  ('Premium',  'premium',  9900, 15000, ARRAY['scanner','offers','referrals','badges','analytics','featured'])
ON CONFLICT (slug) DO NOTHING;

-- Add loyalty plan columns to accounts
ALTER TABLE accounts ADD COLUMN IF NOT EXISTS loyalty_plan         TEXT;
ALTER TABLE accounts ADD COLUMN IF NOT EXISTS loyalty_plan_status  TEXT NOT NULL DEFAULT 'inactive';
ALTER TABLE accounts ADD COLUMN IF NOT EXISTS zc_pool_remaining    INT NOT NULL DEFAULT 0;
ALTER TABLE accounts ADD COLUMN IF NOT EXISTS zc_pool_total        INT NOT NULL DEFAULT 0;
ALTER TABLE accounts ADD COLUMN IF NOT EXISTS pool_reset_date      DATE;
ALTER TABLE accounts ADD COLUMN IF NOT EXISTS subscription_id      TEXT;
