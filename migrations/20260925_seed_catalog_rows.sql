-- 20260925_seed_catalog_rows.sql — seat the PRODUCT CATALOG that a fresh install cannot work
-- without, and nothing else. (kanban t_7451fc99)
--
-- WHY THIS FILE EXISTS
--   A from-zero install (empty database + this repo's migrations + the shipped image) built the
--   live schema exactly (101/101 tables, 995/995 columns, 0 missing / 0 extra, harness
--   scripts/is-baseline-fromzero.sh) and booted and served — with an EMPTY product catalog:
--
--     features 20/30, available_providers 9/15, email_templates 58/57 (zero/live, 2026-09-25)
--
--   The 10 missing feature keys and the 6 missing provider rows were only ever loaded by hand
--   from migrations-manual/ (insert_surface_features.sql, register_feature_keys.sql,
--   provider_keys.sql), which the runner deliberately excludes (7e310e5e) and which no doc or
--   procedure referenced. So a fresh install had no documented way to reach the shipped product:
--   the operator could not assign "Custom Domains" (the key `surface_handler.rs` gates on) to any
--   plan, and could not pick Mailgun/SendGrid/Sendiio/Letterman/Nexweave/SAM.gov as an
--   integration at all. Product structure, not pricing.
--
-- WHAT IS DELIBERATELY NOT HERE — the operator seats it in the UI (owner's call, nothing hardcoded):
--   plans (3 live), plan_tiers (3 live), tier_features (24 live), integration_targets (0 live rows
--   are product structure — all 6 are `https://example.com/hook` webhooks written by the
--   integration-targets API under one test account, i.e. probe residue, not seed data).
--   Measured live assignment used as the reference: tier_features live holds ONLY the mechanic_*
--   keys (pro 11 + enterprise 13); the surface gates are assigned to no tier, so a fresh install
--   starts from the same place. See migrations-manual/README.md for the documented procedure.
--
-- PROVENANCE of every row below: `migrations-manual/`. Byte-for-byte the same (key, label,
--   category, description) / (key, name, description, icon) values production holds today.
--
-- LIVE EFFECT, measured before shipping by applying this exact file to a pg_dump copy of
--   production (harness /opt/swift/bin/is-seed-parity-proof.sh): +0 features, +0 providers,
--   +1 email_template (the generic `winner` fallback, ABSENT from production — see below), every
--   other row and every schema object byte-identical. Prod already holds the other 16 rows, so
--   the guards make every insert there a no-op, and a second run inserts nothing anywhere.

-- ── 1. feature catalog ──────────────────────────────────────────────────────────────────────
-- The 5 keys read by code: `custom_domains` (the plan gate in surface_handler.rs), `tablet_mode`,
-- `widget_embed`, `full_page`, `white_label`. The 5 `surface_*` rows are surface-flavoured
-- duplicates with no reader anywhere in the code; they are seeded only so that a fresh install's
-- catalog matches production exactly. Flattening those two naming sets is a separate decision and
-- is NOT taken here.
INSERT INTO features (key, label, category, description) VALUES
    ('surface_custom_domains', 'Custom Domains',       'surface', 'Host campaigns on custom domains'),
    ('surface_tablet_mode',    'Tablet Mode',          'surface', 'Full-screen tablet-optimized engagement'),
    ('surface_widget_embed',   'Widget Embed',         'surface', 'Floating widget for embedding on any website'),
    ('surface_full_page',      'Full Page Experience',  'surface', 'Branded full-page gamified campaign landing page'),
    ('surface_white_label',    'White Label',           'surface', 'Remove all IncentiveSwift branding from surfaces'),
    ('custom_domains',         'Custom Domains (check)', 'surface', 'Custom domain feature gate'),
    ('tablet_mode',            'Tablet Mode (check)',    'surface', 'Tablet mode feature gate'),
    ('widget_embed',           'Widget Embed (check)',   'surface', 'Widget embed feature gate'),
    ('full_page',              'Full Page (check)',      'surface', 'Full page experience feature gate'),
    ('white_label',            'White Label (check)',    'surface', 'Remove branding feature gate')
ON CONFLICT (key) DO NOTHING;

-- ── 2. integration provider catalog ─────────────────────────────────────────────────────────
-- Without these rows the Integrations Center picker cannot offer the delivery providers the
-- product ships (email/SMS, newsletter, personalised video, federal contracting).
INSERT INTO available_providers (key, name, description, icon) VALUES
    ('mailgun',   'Mailgun',    'Transactional email sending',            'mail'),
    ('sendgrid',  'SendGrid',   'Email delivery service',                 'mail'),
    ('sendiio',   'Sendiio',    'Email/SMS campaign delivery',            'mail'),
    ('letterman', 'Letterman',  'Newsletter content delivery',            'newspaper'),
    ('nexweave',  'Nexweave',   'Personalized video/image generation',    'video'),
    ('sam_gov',   'SAM.gov',    'Federal contracting opportunities',      'shield')
ON CONFLICT (key) DO NOTHING;

-- ── 3. the generic winner email fallback ────────────────────────────────────────────────────
-- `src/handlers/entries.rs` resolves a prize email as `<campaign_type>_winner` and, when that
-- template is missing, falls back to template_type `winner`. Production holds NO row with
-- template_type = 'winner' (measured 2026-09-25: 57 rows, 57 distinct types, none of them
-- 'winner'), so on production that documented fallback always fails with "No email template found
-- for type 'winner'" and the prize email is dropped (warning only, best-effort). The from-zero
-- build of 20260820_email_templates_seed.sql DOES create it — that is the whole 58 vs 57.
-- Verdict: production is missing one, the seed is not a duplicate. Seeded here so both converge.
-- Guarded by NOT EXISTS rather than ON CONFLICT: the unique index that covers a global default is
-- partial (idx_email_templates_unique ... WHERE aid IS NULL AND is_default), and being explicit
-- keeps this idempotent on any schema shape.
INSERT INTO email_templates (template_type, name, subject, body, html_body, is_default, aid)
SELECT 'winner', 'Default Winner Email',
       '🎉 Congratulations — You Won!',
       'Congratulations {{first_name}}! You won the "{{campaign_name}}" campaign. Prize: {{prize_name}}',
       '<h2>🎉 Congratulations {{first_name}}!</h2><p>You won the <b>{{campaign_name}}</b> campaign.</p>{{#if prize_name}}<p>Prize: <b>{{prize_name}}</b></p>{{/if}}',
       true, NULL
WHERE NOT EXISTS (
    SELECT 1 FROM email_templates
    WHERE template_type = 'winner' AND aid IS NULL AND is_default
);
