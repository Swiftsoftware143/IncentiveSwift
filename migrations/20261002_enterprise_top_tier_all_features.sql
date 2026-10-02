-- 20261002_enterprise_top_tier_all_features.sql
-- AF-5 (kanban t_f3141461): "The top tier plan gets everything."
--
-- WHAT THIS FILE DOES — gap-fills ONLY, on the TOP plan tier, one row per registry key that tier
-- does not already carry. Nothing is overwritten and no other tier is touched.
--
-- MEASURED BEFORE (live, 2026-10-02, `features` LEFT JOIN `tier_features` for the top tier):
--     38 registry keys, enterprise granted 18  ->  20 keys with NO row at all:
--     branding_custom_domain, branding_white_label, credits_monthly, credits_overdraft,
--     custom_domains, delivery_direct_api, delivery_webhook, full_page, industry_limit,
--     limit_unlimited_campaigns, module_loyalty_program, stripe_credit_topups,
--     surface_custom_domains, surface_full_page, surface_tablet_mode, surface_white_label,
--     surface_widget_embed, tablet_mode, white_label, widget_embed.
--     After this file: 38/38.
--
-- WHY THE NUMBER IS DERIVED, NOT HARDCODED
--   The work list comes from the SAME statement the audit used (`features` anti-joined against
--   `tier_features` for the top tier), evaluated at apply time. A hardcoded IN (...) list would go
--   stale the moment a key is registered, and would silently skip a key the owner added in the UI.
--
-- WHY `category = 'limits'` GETS `-1` AND EVERYTHING ELSE GETS NULL
--   `features` has no `kind` column, so the value's shape is read from the app's own conventions:
--     * a `limits` key is an allowance, and this app encodes allowances as INT4 on
--       `tier_features.limit_value` with `-1` = unlimited, `0` = not included, N > 0 = cap N
--       (`src/features.rs::check_limit`, `industry_limit`, `credit_limit` — all three documented
--       there, and the tier already carrying `-1` on `max_leads`/`max_tags` on the top tier is the
--       live proof of the convention). "Everything" for an allowance is therefore `-1`.
--     * every other key is a capability flag; `enabled = true` with no number is the grant
--       (`access::feature_gate`).
--   Four of the twenty are `limits` keys: credits_monthly, credits_overdraft, industry_limit,
--   limit_unlimited_campaigns.
--
-- WHY GAP-FILL AND NOT UPSERT
--   The tier already carries authored numbers the owner set through the admin panel
--   (`max_leads = -1`, `max_tags = -1`). An `ON CONFLICT DO UPDATE` would be a write to the
--   owner's pricing data; this file must not touch a row that already exists. `NOT EXISTS` + the
--   conflict guard makes a re-run a no-op and makes a second run after the owner has changed
--   something change nothing.
--
-- FROM-ZERO IS A NO-OP BY CONSTRUCTION
--   `plan_tiers` is OPERATOR data and is deliberately NOT seeded by any migration (see
--   20260925_seed_catalog_rows.sql: "WHAT IS DELIBERATELY NOT HERE ... plans (3 live),
--   plan_tiers (3 live), tier_features (24 live)"). On a from-zero database this file therefore
--   finds no tier, inserts nothing, and must still APPLY cleanly — the `IF top_tier_id IS NULL`
--   guard below is what keeps the migrations runner (which exits(1) on a failing file) boot-safe.
--
-- RE-RUNNABILITY / AFTER A RENAME
--   The top tier is resolved from data (highest price, then highest sort_order) rather than by the
--   literal slug 'enterprise', so renaming the tier cannot make this a silent no-op. The resolved
--   slug is RAISE NOTICE'd for the audit trail.

DO $$
DECLARE
    top_tier_id uuid;
    top_tier_slug text;
    granted int := 0;
BEGIN
    SELECT pt.id, pt.slug
      INTO top_tier_id, top_tier_slug
      FROM plan_tiers pt
     ORDER BY pt.price_monthly DESC NULLS LAST, pt.sort_order DESC NULLS LAST, pt.slug
     LIMIT 1;

    IF top_tier_id IS NULL THEN
        RAISE NOTICE '20261002_enterprise_top_tier_all_features: no plan_tiers row on this database (from-zero install) - nothing to grant';
        RETURN;
    END IF;

    INSERT INTO tier_features (tier_id, feature_id, enabled, limit_value)
    SELECT top_tier_id,
           f.id,
           true,
           CASE WHEN f.category = 'limits' THEN -1 ELSE NULL END
      FROM features f
     WHERE NOT EXISTS (
               SELECT 1
                 FROM tier_features tf
                WHERE tf.tier_id = top_tier_id
                  AND tf.feature_id = f.id
           )
    ON CONFLICT (tier_id, feature_id) DO NOTHING;

    GET DIAGNOSTICS granted = ROW_COUNT;
    RAISE NOTICE '20261002_enterprise_top_tier_all_features: top tier "%" -> granted % missing registry key(s)', top_tier_slug, granted;
END $$;
