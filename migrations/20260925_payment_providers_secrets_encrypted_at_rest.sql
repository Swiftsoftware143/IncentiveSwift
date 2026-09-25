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
-- migration file runs exactly ONCE, so it must never be able to block a boot, and a plaintext row
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

ALTER TABLE payment_providers DROP CONSTRAINT IF EXISTS payment_providers_api_key_encrypted;

ALTER TABLE payment_providers ADD CONSTRAINT payment_providers_api_key_encrypted CHECK (api_key = '' OR api_key LIKE 'enc:v1:%') NOT VALID;

ALTER TABLE payment_providers DROP CONSTRAINT IF EXISTS payment_providers_webhook_secret_encrypted;

ALTER TABLE payment_providers ADD CONSTRAINT payment_providers_webhook_secret_encrypted CHECK (webhook_secret = '' OR webhook_secret LIKE 'enc:v1:%') NOT VALID;
