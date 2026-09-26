-- 20260925_phantom_tables_payment_providers_guard.sql
--
-- OWNER of the payment_providers at-rest guard (kanban t_ec33d19b). Its position is the point of
-- the file: this filename sorts directly AFTER 20260925_phantom_tables.sql, the file that CREATEs
-- payment_providers, because src/db/migrations.rs applies migrations/*.sql in FILENAME order
-- ('20260925_phantom_tables.sql' < '20260925_phantom_tables_payment_providers_guard.sql' <
-- '20260925_plain_statement_column_drift.sql').
--
-- WHY A SECOND FILE. The guard was introduced by
-- 20260925_payment_providers_secrets_encrypted_at_rest.sql, whose filename sorts BEFORE
-- phantom_tables. On a brand-new database its four ALTERs therefore ran against a table that did
-- not exist yet, the file failed (`relation "payment_providers" does not exist`) and
-- MIGRATIONS_FATAL made the process exit before it ever bound a port: a fresh environment could
-- not serve, and 20260926_surface_nullable_not_null.sql failed in the same boot for the same
-- class (it named tablet_sessions, which 20260926_retire_tablet_sessions.sql had just dropped).
-- That file is now guarded in place so it can never block a boot again, which means it emits
-- nothing on a fresh database. THIS file is where the arming happens instead: after the table
-- exists, so a from-zero build of migrations/*.sql ALONE reaches the state LIVE is in, including
-- convalidated = true. Measured: psql-only from-zero build vs live = 0 missing / 0 extra in every
-- object class, both constraints VALID on both sides.
--
-- ARM (b), decided by measurement rather than by taste, matching its sibling: the filename IS the
-- `_migrations` key, so RENAMING the old file would have made an already-recorded file re-run on
-- every live database — a DROP + ADD ... NOT VALID of two constraints that are already VALID
-- there (a production DDL event that also leaves a permanently stale ledger row, and that lands a
-- psql-only build in NOT VALID, i.e. still drifted from live). A NEW file at the correct position
-- costs live one ledger row and two no-op VALIDATEs, and nothing else.
--
-- WHAT IT DOES — both arms idempotent, both arm-if-needed, neither can fail a boot:
--   * ADD CONSTRAINT ... NOT VALID only when that constraint is ABSENT (dropped by hand, left out
--     by a restore). NOT VALID because a plaintext row may exist on a database this file has never
--     run on, and this file must never be able to refuse a boot. New writes are checked either way.
--   * VALIDATE CONSTRAINT only once nothing is left unsealed — the same precondition the boot path
--     uses (src/billing/providers.rs::seal_payment_provider_secrets, called from src/main.rs after
--     the migrations, which is also what seals a plaintext row in place and then validates). A
--     VALIDATE with a plaintext row present errors, and wrapping it in this precondition is what
--     keeps the file safe on any database.
--
-- LIVE IS A NO-OP: both constraints already exist and are already VALID there, so this file adds
-- one _migrations row and runs two no-op VALIDATEs. Measured on a restored copy of the live
-- database before shipping, and again on live after the deploy: 0 lines of catalog diff and 0
-- changed rows outside _migrations.
--
-- The predicate is the app's BYOK convention — 'enc:v1:' + ciphertext, sealed through
-- pgcrypto by src/security/provider_key_crypto.rs. An empty string stays allowed, because '' is
-- the "keep the stored value" signal the upsert's ON CONFLICT CASE WHEN EXCLUDED.api_key <> ''
-- clause depends on. The same two names are constants in src/billing/providers.rs, which is the
-- boot half of this guard and must stay in step with the text below.
--
-- The no-semicolon rule applies to every comment above: the runner executes each file as one
-- batch, so a stray statement separator inside a comment is a latent hazard worth avoiding.

CREATE EXTENSION IF NOT EXISTS pgcrypto;

DO $$
DECLARE
    unsealed integer;
BEGIN
    IF to_regclass('public.payment_providers') IS NULL THEN
        RAISE NOTICE 'payment_providers is absent - nothing to arm (this file is positioned after the file that creates it, so this line only appears on a database restored without it)';
    ELSE
        IF NOT EXISTS (
            SELECT 1 FROM pg_constraint
            WHERE conname = 'payment_providers_api_key_encrypted'
              AND conrelid = 'public.payment_providers'::regclass
        ) THEN
            ALTER TABLE public.payment_providers
                ADD CONSTRAINT payment_providers_api_key_encrypted
                CHECK (api_key = '' OR api_key LIKE 'enc:v1:%') NOT VALID;
        END IF;

        IF NOT EXISTS (
            SELECT 1 FROM pg_constraint
            WHERE conname = 'payment_providers_webhook_secret_encrypted'
              AND conrelid = 'public.payment_providers'::regclass
        ) THEN
            ALTER TABLE public.payment_providers
                ADD CONSTRAINT payment_providers_webhook_secret_encrypted
                CHECK (webhook_secret = '' OR webhook_secret LIKE 'enc:v1:%') NOT VALID;
        END IF;

        SELECT count(*) INTO unsealed
        FROM public.payment_providers
        WHERE (api_key <> '' AND api_key NOT LIKE 'enc:v1:%')
           OR (webhook_secret <> '' AND webhook_secret NOT LIKE 'enc:v1:%');

        IF unsealed > 0 THEN
            RAISE NOTICE 'payment_providers still holds % plaintext secret(s) - guards left NOT VALID, the app boot path seals them and then validates', unsealed;
        ELSE
            ALTER TABLE public.payment_providers VALIDATE CONSTRAINT payment_providers_api_key_encrypted;
            ALTER TABLE public.payment_providers VALIDATE CONSTRAINT payment_providers_webhook_secret_encrypted;
        END IF;
    END IF;
END $$;
