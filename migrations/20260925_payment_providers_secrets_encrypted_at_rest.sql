-- 20260925_payment_providers_secrets_encrypted_at_rest.sql
--
-- payment_providers.api_key and payment_providers.webhook_secret hold CUSTOMER-SUPPLIED payment
-- credentials: a Stripe/PayPal secret key, and the HMAC key the inbound webhook signature
-- (POST /api/v1/webhooks/stripe -> billing::providers::lookup_webhook_secret) is verified
-- against. Before this migration the upsert bound the raw request value straight into both
-- columns, so they sat in the clear at rest and would fall out of any database dump, backup or
-- read-only SQL grant. webhook_secret is the sharper of the two: whoever holds it can forge a
-- signature and mint credited purchases.
--
-- The app now seals both through its own BYOK convention before the write — AES-256 via
-- pgcrypto, master key held ONLY in the process environment (PROVIDER_KEY_ENC_SECRET), stored as
-- 'enc:v1:' + base64 ciphertext. See src/security/provider_key_crypto.rs for the format and the
-- fail-closed rule. No second scheme is introduced.
--
-- These two constraints are the regression guard: a future writer that forgets to encrypt FAILS
-- CLOSED at the database instead of silently storing a plaintext payment credential. An empty
-- string stays allowed, because '' is the "keep the stored value" signal the upsert's
-- ON CONFLICT CASE WHEN EXCLUDED.api_key <> '' clause depends on.
--
-- NOT VALID is deliberate, and differs from the sibling provider_keys guard: a ledger-tracked
-- migration file runs exactly ONCE, so it must never be able to block a boot [corrected by
-- t_ec33d19b: "runs once" is why it cannot block a boot TWICE — it still blocks the FIRST boot of
-- a database that never ran it, which is what the guard below now prevents], and a plaintext row
-- that appears later (a restore, a rollback to an older binary) would make a VALID constraint
-- fail the whole file. New writes are checked either way. The boot path
-- (billing::providers::seal_payment_provider_secrets, called from AppState::new) re-arms each
-- constraint if it is missing, seals every plaintext row in place, and then runs VALIDATE
-- CONSTRAINT once nothing is left unsealed — so the end state on any database is the same as
-- provider_keys: fully enforced.
--
-- The no-semicolon rule applies to every comment above: the runner executes each file as one
-- batch, so a stray statement separator inside a comment is a latent hazard worth avoiding.

CREATE EXTENSION IF NOT EXISTS pgcrypto;

-- ORDER FIX (kanban t_ec33d19b). `payment_providers` is created by 20260925_phantom_tables.sql,
-- which sorts AFTER this filename, and src/db/migrations.rs applies files in FILENAME order. On a
-- brand-new database the four ALTERs therefore ran against a table that did not exist yet, the
-- whole file failed (`relation "payment_providers" does not exist`) and, with MIGRATIONS_FATAL at
-- its default, the process exited before binding — a fresh environment could not serve. The same
-- defect class killed its sibling 20260926_surface_nullable_not_null.sql in the same boot
-- (measured from zero: 2 failed files, 74 of 76 recorded, no listener).
--
-- ARM (b) — guarded IN PLACE, filename untouched. The filename IS the `_migrations` key: a rename
-- (the other arm) would make this already-recorded file RE-RUN on every live database, DROPping
-- and re-ADDing ... NOT VALID two constraints that are already VALID there. That is a production
-- DDL event for zero end-state benefit, plus a permanently stale ledger row. The guard touches
-- live not at all and reaches exactly the fresh installs the defect is about.
--
-- On a fresh database this file now emits nothing: the table does not exist YET, so the guard
-- skips, and the two constraints are armed by the boot path that owns them anyway —
-- billing::providers::seal_payment_provider_secrets (src/main.rs, immediately after the
-- migrations) ADDs each constraint when it is missing (NOT VALID) and then VALIDATEs both once
-- nothing is left unsealed, on EVERY boot of EVERY database. End state is therefore identical on
-- both sides: measured from zero, boot 1 records 77/77 and reads
-- convalidated = true for both constraints. The guard still does real work on the one database
-- shape where it matters — a restore that brings the table but not this ledger row runs the
-- ALTERs exactly as before. The POSITION-CORRECT owner of the arming (and of the VALIDATE that
-- live already carries) is 20260925_phantom_tables_payment_providers_guard.sql, which sorts after
-- the file that CREATEs the table and is what makes a from-zero build of migrations/*.sql alone
-- match live.

DO $$
BEGIN
    IF to_regclass('public.payment_providers') IS NOT NULL THEN
        ALTER TABLE payment_providers DROP CONSTRAINT IF EXISTS payment_providers_api_key_encrypted;
        ALTER TABLE payment_providers ADD CONSTRAINT payment_providers_api_key_encrypted CHECK (api_key = '' OR api_key LIKE 'enc:v1:%') NOT VALID;

        ALTER TABLE payment_providers DROP CONSTRAINT IF EXISTS payment_providers_webhook_secret_encrypted;
        ALTER TABLE payment_providers ADD CONSTRAINT payment_providers_webhook_secret_encrypted CHECK (webhook_secret = '' OR webhook_secret LIKE 'enc:v1:%') NOT VALID;
    ELSE
        RAISE NOTICE 'payment_providers is created later in filename order (20260925_phantom_tables.sql) - skipping the at-rest guard ALTERs, which the app boot path arms and validates';
    END IF;
END $$;
