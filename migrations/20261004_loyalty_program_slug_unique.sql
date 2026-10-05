-- t_25e9f950 — the public QR lookup resolved a programme by a GLOBAL, oldest-first NAME
-- derivation with no tenant predicate.
--
-- MEASURED LIVE 2026-10-04 on binary 0ba4027822f94b74 (evidence: audits/t_25e9f950/
-- before-collide-output.txt): two tenants, two programmes, the SAME programme name, both
-- printed QRs carrying the SAME name-derived string ->
--   GET /api/v1/loyalty/public/program/probe-collide-t_2968cb33 -> 200
--   business_name=ProbeCollideA, currency_name=AAA-CURRENCY, campaign_slug=...-a
-- i.e. tenant B's QR rendered tenant A's business and programme. The landing page then POSTs
-- the RETURNED campaign slug to /api/v1/loyalty/checkin, so tenant B's customer credited
-- points to tenant A's campaign. The key was also guessable: any name derives a slug.
--
-- STRUCTURAL FIX. The identifier a printed QR carries for a programme is now
-- `loyalty_programs.slug`, UNIQUE here and written by create_program for every new programme.
-- One programme -> one account, so the key is tenant-scoped by construction. This file
-- (a) backfills the rows that predate that, and (b) enforces uniqueness so the property cannot
-- be lost again (before this index the column had no constraint at all and create_program
-- omitted it, so every console-created programme had slug NULL).
--
-- BACKFILL RULE — the bare name-derived form only when it is unambiguous. A programme keeps
-- `lower(replace(name,' ','-'))` only if that string identifies exactly ONE programme AND is not
-- already another programme's slug (the live ZaarHub Rewards programme already owns its
-- name-derived form, and the QR printed from it must keep resolving). When two programmes share
-- a derived name NEITHER gets the bare form: a legacy QR carrying that ambiguous string then
-- resolves to NOTHING (the lookup refuses to guess, see handlers/loyalty.rs::public_program)
-- instead of silently crediting the older tenant. Those rows get a stable
-- `<derived>-<first 8 hex of id>` instead.
--
-- Idempotent: ADD COLUMN IF NOT EXISTS, the backfill only touches slug IS NULL, and the index is
-- IF NOT EXISTS. Runs on an EMPTY database too — this filename sorts BEFORE the
-- zz_is7_baseline_columns.sql that also adds `slug`, so the column is added here defensively.
ALTER TABLE IF EXISTS public.loyalty_programs ADD COLUMN IF NOT EXISTS slug text;

-- (a) the unambiguous name-derived form
UPDATE public.loyalty_programs p
   SET slug = lower(replace(p.name, ' ', '-'))
 WHERE p.slug IS NULL
   AND btrim(lower(replace(p.name, ' ', '-'))) <> ''
   AND NOT EXISTS (
         SELECT 1 FROM public.loyalty_programs q
          WHERE q.id <> p.id
            AND lower(replace(q.name, ' ', '-')) = lower(replace(p.name, ' ', '-')))
   AND NOT EXISTS (
         SELECT 1 FROM public.loyalty_programs r
          WHERE r.id <> p.id
            AND r.slug = lower(replace(p.name, ' ', '-')));

-- (b) every remaining row: a stable, id-suffixed slug (never the ambiguous bare form)
UPDATE public.loyalty_programs p
   SET slug = lower(replace(p.name, ' ', '-')) || '-'
              || substr(replace(p.id::text, '-', ''), 1, 8)
 WHERE p.slug IS NULL;

-- The uniqueness the whole fix rests on. Postgres treats NULLs as distinct, so a legacy/edge
-- row without a slug cannot break the index; every writer sets it (create_program).
CREATE UNIQUE INDEX IF NOT EXISTS loyalty_programs_slug_uidx
    ON public.loyalty_programs (slug);
