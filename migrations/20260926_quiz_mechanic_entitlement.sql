-- 20260926_quiz_mechanic_entitlement.sql — seat the `mechanic_quiz` feature KEY that
-- migrations/20260818_mechanic_feature_gates.sql authored but the catalog never carried.
-- (kanban t_d2c56fcb)
--
-- MEASURED (live, 2026-09-26, binary 33bf64d8)
--   20260818_mechanic_feature_gates.sql declares `feat_keys TEXT[]` containing
--   'mechanic_quiz' (and 'mechanic_loyalty') and assigns every key in that array to the
--   pro and enterprise tiers. The assignment is a JOIN on `features.key`, so a key with
--   no `features` row inserts nothing and says nothing. Live `features` holds 34 keys and
--   NONE of them is `mechanic_quiz`. `access::feature_gate::has_mechanic_access` resolves
--   `mechanic_<campaign type>`; finding no such feature row it falls through to the
--   `all_mechanics` catch-all, which is enabled on `enterprise` ONLY — and no live account
--   is on enterprise (52 free / 4 pro / 0 enterprise). So POST /api/v1/quiz/{id}/submit
--   answered 402 for every account, one layer above the defect this card is about.
--
-- WHAT THIS FILE DOES
--   Seats the feature KEY, so the key the gate file names exists and the operator's Plan
--   Tiers screen can offer it. It deliberately does NOT seat `tier_features`: per-plan
--   assignment is the operator's call in the admin UI (20260925_seed_catalog_rows.sql
--   states that explicitly for plan_tiers/tier_features), and the live `pro` seat behind
--   this card's proof was made through the operator's own surface
--   (PUT /api/v1/admin/tiers/{id}/features/mechanic_quiz), not here.
--   `mechanic_loyalty` — the other key in that same array with no `features` row — is left
--   alone: whether loyalty is a mechanic on pro or the separate `module_loyalty_program`
--   gate is its own product decision, carded separately.
INSERT INTO features (key, label, category, description) VALUES
    ('mechanic_quiz', 'Quiz / Trivia', 'mechanic',
     'Scored quiz and trivia campaigns with persona outcomes')
ON CONFLICT (key) DO NOTHING;
