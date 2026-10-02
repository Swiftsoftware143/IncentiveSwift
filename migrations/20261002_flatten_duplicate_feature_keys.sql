-- 20261002_flatten_duplicate_feature_keys.sql — ONE NAME PER CAPABILITY in the `features` registry.
-- kanban t_c307f7a7 (found by t_f3141461 while seating all 38 registry keys on the top tier).
--
-- WHY THIS FILE EXISTS
--   `features` held 38 keys and 12 of them named only 5 capabilities, because two naming waves were
--   never reconciled:
--     * `migrations/00001_full_schema.sql` (+ `migrations/00019_credit_system.sql`) seed the
--       `branding_*` / `delivery_*` / `limit_*` / `stripe_*` vocabulary;
--     * `migrations-manual/insert_surface_features.sql` and
--       `migrations-manual/register_feature_keys.sql` add the `surface_*` rows AND the five short
--       "…(check)" keys;
--     * `migrations/20260925_seed_catalog_rows.sql` seats them for a fresh install and records the
--       deferral in its own header — "Flattening those two naming sets is a separate decision and is
--       NOT taken here" — but no card ever tracked that decision (this card is that decision).
--   MEASURED 2026-10-02 on the live database: of the 12 rows only `custom_domains` is read at runtime
--   (`surface_handler::check_plan_domains`); the other 11 grant nothing. Because the Plan Tiers panel
--   renders ONE SWITCH PER ROW, the operator saw two or three switches for one capability of which at
--   most one did anything — a footgun, not just clutter.
--
-- THE DECISION (option (a) of the card — FLATTEN). The SURVIVOR is the short key
--   (`custom_domains`, `white_label`, `tablet_mode`, `widget_embed`, `full_page`): that is the
--   vocabulary the gate reads (`access::feature_gate::has_feature_access`,
--   `db::plans::feature_enabled`, and the `f.key = 'custom_domains'` statement in
--   `surface_handler::check_plan_domains`) and the vocabulary every future reader will use. The
--   survivor INHERITS the duplicate's product label/description, which carry no "(check)" artefact.
--
--   retired key                   -> survivor        (was granted to)
--   surface_custom_domains        -> custom_domains        enterprise
--   branding_custom_domain        -> custom_domains        enterprise
--   surface_white_label           -> white_label           enterprise
--   branding_white_label          -> white_label           enterprise
--   surface_tablet_mode           -> tablet_mode           enterprise
--   surface_widget_embed          -> widget_embed          enterprise
--   surface_full_page             -> full_page             enterprise
--
-- THE TRAP THIS FILE IS WRITTEN AROUND: `tier_features.feature_id` is ON DELETE CASCADE, so a bare
--   DELETE silently drops every grant the retiree carried and SHRINKS the tier. Every retiree's grants
--   are therefore RE-POINTED onto the survivor, INSIDE this transaction, BEFORE the DELETE, through an
--   upsert that can never disarm a survivor that was already there:
--       enabled     = tier_features.enabled OR EXCLUDED.enabled   (never turns a tier off)
--       limit_value = COALESCE(tier_features.limit_value, EXCLUDED.limit_value)
--   (`limit_value` is deliberately NOT overwritten: in this app's convention `0` means "not available
--   on this tier", `-1` means "no cap" and a positive value is the cap, so a survivor that already
--   carries a cap keeps it — exactly the verdict `check_plan_domains` gave before this file ran.)
--
-- IDEMPOTENT: every step is guarded; on a second run there is no retiree row left, so nothing moves,
--   nothing is deleted and the labels are re-asserted. The boot runner applies this file as ONE
--   implicit transaction (src/db/migrations.rs, `sqlx::raw_sql`), so it lands whole or not at all.
--
-- PROOF (all legs live, /opt/swift/audits/t_c307f7a7/):
--   * `features` LEFT JOIN `tier_features` per tier before AND after; per-tier granted-key sets
--     compared key-by-key; the 5 capabilities held by the same tiers before and after.
--   * the Plan Tiers panel re-driven in real Chromium on the served console.
--   * no tier lost a grant it had: free 5 -> 5, pro 17 -> 17, enterprise 38 -> 31 (the 7 retired rows
--     were the ONLY difference; every capability enterprise held it still holds).
DO $flatten$
DECLARE
    cap        RECORD;
    ret        text;
    surv       uuid;
    n          int := 0;
    n_moved    int := 0;
    n_retired  int := 0;
    n_before   int;
    n_after    int;
BEGIN
    SELECT count(*) INTO n_before FROM features;

    FOR cap IN
        SELECT * FROM (VALUES
            ('custom_domains', 'Custom Domains',
             'Host campaigns on custom domains',
             ARRAY['surface_custom_domains', 'branding_custom_domain']),
            ('white_label', 'White Label',
             'Remove all IncentiveSwift branding from surfaces',
             ARRAY['surface_white_label', 'branding_white_label']),
            ('tablet_mode', 'Tablet Mode',
             'Full-screen tablet-optimized engagement',
             ARRAY['surface_tablet_mode']),
            ('widget_embed', 'Widget Embed',
             'Floating widget for embedding on any website',
             ARRAY['surface_widget_embed']),
            ('full_page', 'Full Page Experience',
             'Branded full-page gamified campaign landing page',
             ARRAY['surface_full_page'])
        ) AS t(survivor, label, description, retirees)
    LOOP
        SELECT id INTO surv FROM features WHERE key = cap.survivor;

        -- A retiree that is the ONLY holder of the capability must never be deleted blind: if the
        -- survivor is missing while a duplicate exists, refuse rather than silently lose it.
        IF surv IS NULL THEN
            IF EXISTS (SELECT 1 FROM features WHERE key = ANY (cap.retirees)) THEN
                RAISE EXCEPTION
                    'flatten: survivor % is missing while one of its duplicates still exists (%)',
                    cap.survivor, cap.retirees;
            END IF;
            CONTINUE;   -- already flattened: nothing to do, nothing to assert
        END IF;

        -- 1. RE-POINT every grant the duplicate carried onto the survivor (before any DELETE).
        FOREACH ret IN ARRAY cap.retirees LOOP
            INSERT INTO tier_features (tier_id, feature_id, enabled, limit_value)
            SELECT tf.tier_id, surv, tf.enabled, tf.limit_value
              FROM tier_features tf
              JOIN features f ON f.id = tf.feature_id
             WHERE f.key = ret
            ON CONFLICT (tier_id, feature_id) DO UPDATE
               SET enabled     = tier_features.enabled OR EXCLUDED.enabled,
                   limit_value = COALESCE(tier_features.limit_value, EXCLUDED.limit_value);
            GET DIAGNOSTICS n = ROW_COUNT;
            n_moved := n_moved + n;
        END LOOP;

        -- 2. retire the duplicate registry rows (their grants now live on the survivor)
        DELETE FROM features WHERE key = ANY (cap.retirees);
        n_retired := n_retired + 1;

        -- 3. the survivor wears the product vocabulary, not the "(check)" artefact
        UPDATE features
           SET label = cap.label, description = cap.description
         WHERE id = surv;
    END LOOP;

    -- 4. no duplicate key may survive this file
    IF EXISTS (
        SELECT 1 FROM features
         WHERE key IN ('surface_custom_domains', 'surface_white_label', 'surface_tablet_mode',
                       'surface_widget_embed', 'surface_full_page', 'branding_custom_domain',
                       'branding_white_label')
    ) THEN
        RAISE EXCEPTION 'flatten: a duplicate feature key survived the migration';
    END IF;

    SELECT count(*) INTO n_after FROM features;
    RAISE NOTICE 'flatten: features % -> %; % grant(s) re-pointed; % capability/capabilities flattened',
        n_before, n_after, n_moved, n_retired;
END
$flatten$;

-- Ledger of what is deliberately NOT flattened here (the card's recorded verdict): the four
-- capability keys `full_page` / `tablet_mode` / `white_label` / `widget_embed` are the surviving
-- names of their pairs but still have no runtime reader, and `delivery_webhook` /
-- `delivery_direct_api` / `limit_unlimited_campaigns` / `stripe_credit_topups` name capabilities
-- whose readers would be a PRICING decision (granting `delivery_webhook` to free/pro, or gating a
-- surface option every live plan can already use). Those stay catalogue-only, by decision, and are
-- recorded in /opt/swift/audits/t_c307f7a7/REPORT.md rather than wired here.
