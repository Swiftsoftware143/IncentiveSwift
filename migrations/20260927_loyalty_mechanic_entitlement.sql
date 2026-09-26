-- 20260927_loyalty_mechanic_entitlement.sql — seat the `mechanic_loyalty` and
-- `mechanic_b2b_loyalty` feature KEYS that the product's own campaign editor sells but the
-- feature catalog never carried. (kanban t_f4aefe45)
--
-- MEASURED (live, 2026-09-26, binary 5bb4e643)
--   migrations/20260818_mechanic_feature_gates.sql declares `feat_keys TEXT[]` and assigns
--   every key in it to the pro and enterprise tiers. That assignment is a JOIN on
--   `features.key`, so a key with no `features` row inserts nothing and says nothing.
--   The array names `mechanic_loyalty`; the live catalogue holds 13 `mechanic_*` rows and
--   NOT one of them is `mechanic_loyalty`. `b2b_loyalty` is likewise a valid campaign type
--   (src/db/campaigns.rs VALID_MECHANIC_TYPES), and its key `mechanic_b2b_loyalty` is named
--   by neither the array nor the catalog. Measured as a class: of the 15 campaign types,
--   13 resolve to a `features` row and 2 (loyalty, b2b_loyalty) resolve to none, so both
--   fall through to `all_mechanics` — enabled on `enterprise` ONLY, and no live account is
--   on enterprise (52 free / 4 pro / 0 enterprise). `has_mechanic_access` is the FIRST
--   statement of POST /api/v1/campaigns (src/handlers/campaigns.rs:74), so both types were
--   uncreatable by every paying account:
--       pro  + type=loyalty      -> 403 "Your plan does not include the 'loyalty' mechanic."
--       pro  + type=b2b_loyalty  -> 403 "Your plan does not include the 'b2b_loyalty' mechanic."
--       pro  + type=quiz         -> 200   (control: the probe's body/auth are valid)
--   while the Operator Console's campaign editor offers both as selectable mechanics
--   (www-admin/index.html MECHANIC_TYPES: 'Loyalty', 'B2B Loyalty').
--
-- ARM CHOSEN: (a) SEAT IT. Arm (b) "loyalty is the module `module_loyalty_program`" is
--   refuted by measurement on two counts: (1) `module_loyalty_program` is seated on NO tier
--   either (0 tier_features rows) and its only consumer — handlers/loyalty.rs
--   `check_plan_loyalty` — COALESCEs an absent row to `true`, i.e. "not configured = allowed",
--   so it can never refuse anything; (2) no code path gates a loyalty *campaign* on it
--   (has_mechanic_access resolves `mechanic_<type>` only). Retiring the key would therefore
--   leave the create route still 403ing for every account, and would delete two mechanics the
--   served console offers. The module and the mechanic are different products: the module is
--   the recurring point program, the mechanic is a campaign that plays as a loyalty campaign.
--
-- WHAT THIS FILE DOES
--   Seats the two feature KEYS, so the keys the product sells exist and the operator's Plan
--   Tiers screen can offer them. It deliberately does NOT seat `tier_features`: per-plan
--   assignment is the operator's call in the admin UI (20260925_seed_catalog_rows.sql states
--   that explicitly for plan_tiers/tier_features, and 20260926_quiz_mechanic_entitlement.sql
--   followed the same split). The live pro + enterprise seats behind this card's proof were
--   made through the operator's own surface
--   (PUT /api/v1/admin/tiers/{id}/features/{key}) on the served Plan Tiers screen.
--
-- NOTE ON THE GATE FILE'S HEADER (not editable — it is an APPLIED migration; its recorded
-- row in `_migrations` is keyed by filename, and the file's own text is what a fresh install
-- replays): its comment claims "all 12 mechanics", its array names 14 `mechanic_*` keys, and
-- `b2b_loyalty` is absent from the array entirely. The array is now consistent with the
-- catalogue for every campaign type the app accepts; the header's count is corrected here,
-- in the file that is still editable.
INSERT INTO features (key, label, category, description) VALUES
    ('mechanic_loyalty', 'Loyalty Campaign', 'mechanic',
     'Loyalty campaigns that play as a mechanic (points, rewards, repeat visits)'),
    ('mechanic_b2b_loyalty', 'B2B Loyalty', 'mechanic',
     'Business-to-business loyalty campaigns between a business and its partner network')
ON CONFLICT (key) DO NOTHING;
