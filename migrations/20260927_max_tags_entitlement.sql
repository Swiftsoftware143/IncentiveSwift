-- IncentiveSwift — the tenant tag library gets a PRODUCER and a REAL, ENFORCED allowance.
--
-- ARM (a) — WIRE THE PRODUCER (kanban t_286aead1). Decided by measurement, not taste:
--
--   * The card's premise "the served admin console has no tag-management view" is FALSE, and the
--     served bytes say so. The Operator Console at admin.incentiveswift.com
--     (`/opt/swift/nginx/www-admin/incentiveswift/index.html` == repo `www-admin/index.html` ==
--     HEAD, md5 bf3d47dc5b22aeda97348d71c218b692) carries `{ id: 'tags', label: 'Tags', icon: '🏷' }`
--     in its nav (line 2243) and a complete `Tags` view + `TagModal` (lines 1096-1150) that calls:
--         GET    /api/v1/tags           (view load)
--         POST   /api/v1/tags           ("+ Add Tag", body {name, color, group})
--         PUT    /api/v1/tags/:id       ("Edit")
--         DELETE /api/v1/tags/:id       ("Del")
--     Measured on the deployed binary before this change: GET 200, POST 405, PUT 404, DELETE 404.
--     The SURFACE is shipped; the PRODUCER was the missing half. Retiring the reader (arm b) would
--     have stranded a live screen, and this app's rule is no orphaned endpoints — in either
--     direction.
--   * The data is real and already counted: `tags` holds 85 rows on ONE account
--     (54c06ed4-4679-486e-b71c-70bea84398e9, "Swift Admin"), all written 2026-07-20, and
--     `GET /api/v1/me/usage` reports them for real (`{"tags":85}` live).
--   * `tag_groups` had no writer either. The console posts the group as a NAME, so the producer
--     find-or-creates the group row by (account_id, lower(name)) — the only way the served Group
--     field can mean anything.
--
-- What this file adds, and nothing it changes:
--   1. the `max_tags` entitlement key + the live catalogue's allowances migrated into the canonical
--      tier model BY SLUG — exactly the move `20260926_max_leads_entitlement.sql` made for
--      `max_leads` (Free 10 / Pro 50 / Enterprise -1 = unlimited).
--   2. the two idempotency keys the write paths rely on, as UNIQUE expression indexes.
--      Measured before creating them: `tags` = 85 rows, 85 DISTINCT lower(name) on the one account;
--      `tag_groups` = 0 rows. So neither index can fail on existing data. They are expression
--      indexes so `ON CONFLICT DO NOTHING` can absorb a concurrent duplicate instead of raising
--      23505 into a 500.
--   3. the COMMENT on `plans.max_tags` corrected: it used to promise there was no writer of the
--      `tags` table, which is no longer true.
--
-- `plans.max_tags` stays DISPLAY-ONLY (no reader, no writer anywhere in this crate). The ENFORCED
-- allowance is `tier_features.limit_value` for `features.key = 'max_tags'` on the account's OWN
-- `plan_tiers` row via `accounts.plan_tier_id`, applied at tag creation by
-- `handlers::tags_handler::create_tag` -> `features::enforce_feature_limit` (the same canonical
-- gate `max_leads` uses). -1 = unlimited, 0 = not on this plan, N > 0 = cap N.

INSERT INTO features (key, label, category)
VALUES ('max_tags', 'Tags (total)', 'limits')
ON CONFLICT (key) DO NOTHING;

INSERT INTO tier_features (tier_id, feature_id, enabled, limit_value)
SELECT pt.id, f.id, true, p.max_tags
  FROM plan_tiers pt
  JOIN plans p ON p.slug = pt.slug
  JOIN features f ON f.key = 'max_tags'
ON CONFLICT (tier_id, feature_id) DO NOTHING;

CREATE UNIQUE INDEX IF NOT EXISTS tags_account_lower_name_uidx
  ON tags (account_id, lower(name));

CREATE UNIQUE INDEX IF NOT EXISTS tag_groups_account_lower_name_uidx
  ON tag_groups (account_id, lower(name));

COMMENT ON COLUMN plans.max_tags IS
  'DISPLAY-ONLY column (no reader, no writer anywhere in this crate). The per-account TAG allowance is tier_features.limit_value for features.key = ''max_tags'' on the account''s own plan_tiers row via accounts.plan_tier_id, enforced at tag creation by handlers::tags_handler::create_tag.';
