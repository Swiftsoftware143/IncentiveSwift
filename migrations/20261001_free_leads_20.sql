-- IncentiveSwift — David raised the free allowance: a free campaign can run 20 leads (was 5).
--
-- David, 2026-10-01: *"Free campaigns can do 20 leads."*
--
-- Measured before this file: `features.key='max_leads'` carried free=5 / pro=100 /
-- enterprise=-1, and `features::enforce_lead_limit_for_campaign` refuses the 6th entry with
--     "Leads limit reached (5/5). Upgrade to increase your limit."
-- so a free campaign stopped after five plays — not enough to demonstrate a prize wheel to anyone,
-- and the reason my own 60-spin distribution proof had to put its throwaway account on a higher tier.
--
-- BOTH arms move, because 20260926_max_leads_entitlement.sql seated the enforced value FROM the
-- display column, and leaving the two disagreeing is exactly how a catalogue ends up advertising an
-- allowance the gate does not apply:
--   * `tier_features.limit_value` — the ENFORCED number (features.rs reads it first);
--   * `plans.max_leads` — the display-only legacy column.
--
-- pro (100) and enterprise (-1 = unlimited) are untouched, so the ordering free < pro < enterprise
-- still holds. Idempotent: re-running sets the same values.

UPDATE tier_features tf
   SET limit_value = 20
  FROM plan_tiers pt, features f
 WHERE tf.tier_id = pt.id
   AND tf.feature_id = f.id
   AND pt.slug = 'free'
   AND f.key = 'max_leads';

UPDATE plans SET max_leads = 20 WHERE slug = 'free' AND max_leads < 20;

COMMENT ON COLUMN plans.max_leads IS
  'DISPLAY-ONLY, NOT an entitlement. The enforced per-account lead allowance is tier_features.limit_value for features.key = ''max_leads'' on the account''s own plan_tiers row (accounts.plan_tier_id), checked at entry creation by features::enforce_lead_limit_for_campaign. Set to the same number here so the catalogue cannot advertise a different allowance than the gate applies. Free = 20 since 2026-10-01 (David).';
