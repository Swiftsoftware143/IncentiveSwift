-- 20260925_industry_limit_entitlement.sql — seat the `industry_limit` FEATURE KEY in the
-- entitlement catalog (kanban t_0961f382).
--
-- WHY THIS FILE EXISTS
--   `industry_limit` was never a catalog row. `migrations/00017_industries.sql` tried to inject it
--   into `plans.features` (free 1 / starter 2 / pro 5 / enterprise 99), but `plans` is the
--   marketing and checkout table: its `features` column is a jsonb ARRAY with a '[]' default, and
--   NO code path reads tier data from it. Every entitlement in this app is a `features` catalog row
--   granted per tier through `tier_features` (`features.rs` is the single source of truth and the
--   admin CRUD is /api/v1/admin/plans/:id/features), and `accounts.plan_tier_id` has an FK to
--   `plan_tiers(id)` — so a plan-shaped `industry_limit` could never resolve. Three read sites in
--   `src/handlers/auth_handler.rs` looked for the key and always fell back to the restrictive
--   default 1, i.e. the entitlement was inert for every one of the 56 live accounts.
--
-- WHAT THIS FILE DOES (product structure only — no pricing, no tier assignment)
--   Seats the one catalog row so the operator can assign "Industry Dashboards" to a plan in the
--   admin UI with a numeric limit, like every other entitlement. It deliberately creates NO
--   `tier_features` row: which tier gets how many industry dashboards is the owner's call in the
--   UI, and leaving every tier unassigned keeps today's live behaviour (cap 1 from the documented
--   default in `features::industry_limit`) unchanged for every existing account.
--
-- HOW TO GRANT IT (operator, in the UI)
--   features::industry_limit reads `tier_features.limit_value` for this key on the account's plan
--   tier: -1 = unlimited, 0 = not available on the plan, N > 0 = cap N. Assign a NUMERIC limit —
--   a row with `limit_value` NULL counts as "not configured" and falls back to the default 1.
--
-- IDEMPOTENT: re-runnable, no statement depends on prior state.
INSERT INTO features (key, label, category, description)
VALUES ('industry_limit', 'Industry Dashboards', 'limits',
        'How many industry dashboards (industry = dashboard = template category) the account may select on its plan. Assign a numeric limit_value per plan: -1 = unlimited, 0 = not available, N = cap N. A plan with no row for this key gets the documented default of 1.')
ON CONFLICT (key) DO NOTHING;
