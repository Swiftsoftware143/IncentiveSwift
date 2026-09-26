-- IncentiveSwift — the lead allowance becomes a REAL, ENFORCED entitlement.
--
-- ARM (b): the TIER model is the entitlement source of truth; `plans.max_leads`/`plans.max_tags`
-- are legacy baseline columns, declared display-only below.
--
-- Measured on prod before this file (2026-09-26):
--   * `accounts.plan_tier_id` -> `plan_tiers(id)` is the ONLY per-account plan link (FK). `plans` has
--     no FK from `accounts`; an account can reach a `plans` row only through `plans.slug =
--     plan_tiers.slug`, and `plans.id = plan_tiers.id` holds for slug `free` ONLY (two
--     gen_random_uuid() defaults that happen to coincide) and FAILS for pro/enterprise.
--   * `plans.max_leads` / `plans.max_tags` are read by NOTHING and written by NOTHING in this crate:
--     the only references anywhere are this baseline table definition. The admin plans API
--     (GET/POST/PUT /api/v1/admin/plans) neither selects nor binds them, the served console's Plans
--     view edits name/price/interval only, and the marketing site advertises no allowance.
--   * `tier_features.limit_value` is the canonical numeric-limit model: `features::enforce_feature_limit`
--     reads it first, `features::industry_limit` / `credit_limit` read it by key, and the admin
--     console's Plan Tiers screen writes it (PUT /api/v1/admin/tiers/:id/features/:key).
--
-- This migration MIGRATES the live catalogue's allowances into the canonical model, matched BY SLUG,
-- so no account gains or loses an allowance it had before (Free 5, Pro 100, Enterprise -1 = unlimited
-- — the same -1 semantics `features::check_limit` already implements). It is idempotent: the runner
-- re-runs any file it could not record.

INSERT INTO features (key, label, category)
VALUES ('max_leads', 'Leads (total)', 'limits')
ON CONFLICT (key) DO NOTHING;

INSERT INTO tier_features (tier_id, feature_id, enabled, limit_value)
SELECT pt.id, f.id, true, p.max_leads
  FROM plan_tiers pt
  JOIN plans p ON p.slug = pt.slug
  JOIN features f ON f.key = 'max_leads'
ON CONFLICT (tier_id, feature_id) DO NOTHING;

-- `max_tags` is deliberately NOT seated. The `tags` table is READ-ONLY in this crate: one GET route
-- (dashboard_handler::list_tags) and ZERO writers in src/ and migrations/. Its 85 live rows were all
-- created 2026-07-20 21:09-00:06 on a single account, so an enforcement arm on it would be dead code.
-- The `max_tags` number is still counted for real by GET /api/v1/me/usage.

COMMENT ON COLUMN plans.max_leads IS
  'DISPLAY-ONLY, NOT an entitlement (no reader, no writer anywhere in this crate). The per-account lead allowance is tier_features.limit_value for features.key = ''max_leads'' on the account''s own plan_tiers row via accounts.plan_tier_id, enforced at entry creation by features::enforce_lead_limit_for_campaign.';

COMMENT ON COLUMN plans.max_tags IS
  'DISPLAY-ONLY, NOT an entitlement (no reader, no writer anywhere in this crate, and no writer of the `tags` table either). No per-account tag allowance is enforced; GET /api/v1/me/usage counts the account''s `tags` rows.';
