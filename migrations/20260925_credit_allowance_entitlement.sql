-- 20260925_credit_allowance_entitlement.sql — seat the CREDIT allowance FEATURE KEYS in the
-- entitlement catalog (kanban t_329b61b2).
--
-- WHY THIS FILE EXISTS
--   `GET /api/v1/credits/balance` (src/handlers/credits_handler.rs) published `credits_monthly`,
--   `credits_overdraft` and `plan_name` by reading
--       "FROM accounts a JOIN plans p ON a.plan_tier_id = p.id"
--   and pulling `p.features->>'credits_monthly'` / `->>'credits_overdraft'` / `p.name`. That join
--   is a tier shape pointed at the MARKETING table: `accounts.plan_tier_id` has an FK to
--   `plan_tiers(id)`, and `plans.features` is a jsonb ARRAY (default '[]'), not an object. Two
--   independent failure modes, both measured live on 2026-09-25:
--     * the 4 accounts on the `pro` tier got NO row at all (only the `free` tier uuid coincides
--       with the `free` plan uuid, 8b8cc0e5-dbe0-4c3b-9128-2b220a67392c) -> plan_name "Unknown";
--     * even the coinciding row answered NULL for every key, so COALESCE(...,0) published "0
--       monthly / 0 overdraft" for all 56 accounts — including a tier with an explicit allowance.
--   The catalog backed none of it: 31 `features` rows, 0 keys matching `credit%` or `cost_%`.
--
-- WHAT THIS FILE DOES (product structure only — no pricing, no tier assignment)
--   Seats the two allowance keys so the operator can assign "Monthly Credits" / "Credit Overdraft"
--   to a plan in the admin UI with a numeric limit, like every other entitlement. `features.rs`
--   resolves them through `credit_limit()` from `tier_features.limit_value` on the account's own
--   plan tier. It deliberately creates NO `tier_features` row: which plan includes how many credits
--   is the owner's call in the UI, and leaving every tier unassigned keeps today's live behaviour
--   (0 from the documented default `CREDIT_LIMIT_DEFAULT`) unchanged for every existing account.
--
--   `cost_<action>` keys are deliberately NOT seated: no code path can produce a live read for
--   them (both readers, `deduct_credits` and `check_credits`, have no caller in the crate) and an
--   action price is a global constant, `credits_handler::DEFAULT_ACTION_COST`. A key that exists
--   only to be fillable in the UI would be dead product structure.
--
-- HOW TO GRANT IT (operator, in the UI: Plans -> a plan -> Features)
--   credits_monthly   — credits included per month on this plan
--   credits_overdraft — extra credits spendable beyond the balance
--   Assign a NUMERIC limit: 0 = none included (the documented default), N > 0 = N, -1 = unlimited.
--   A row with `limit_value` NULL counts as "not configured" and falls back to 0.
--
-- IDEMPOTENT: re-runnable, no statement depends on prior state.
INSERT INTO features (key, label, category, description)
VALUES
  ('credits_monthly', 'Monthly Credits', 'limits',
   'Credits included per month on this plan. Assign a numeric limit_value per plan: 0 = none included (default), N > 0 = N, -1 = unlimited. A plan with no row for this key advertises 0 on GET /api/v1/credits/balance.'),
  ('credits_overdraft', 'Credit Overdraft', 'limits',
   'Extra credits spendable beyond the account balance on this plan. Assign a numeric limit_value per plan: 0 = none (default), N > 0 = N, -1 = unlimited. A plan with no row for this key advertises 0 on GET /api/v1/credits/balance.')
ON CONFLICT (key) DO NOTHING;
