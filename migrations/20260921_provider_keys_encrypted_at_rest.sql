-- 20260921_provider_keys_encrypted_at_rest.sql
--
-- provider_keys.api_key holds CUSTOMER-SUPPLIED third-party credentials (OpenAI, DeepSeek,
-- Stripe, Telnyx, Mailgun, CoreSwift hub keys). Before this migration the write path bound
-- the raw request value straight into the column, so every BYOK key an account entered sat
-- in the clear at rest and would fall out of any database dump or leaked backup.
--
-- The app now encrypts before it writes: AES-256 via pgcrypto, master key held ONLY in the
-- process environment (PROVIDER_KEY_ENC_SECRET), stored as 'enc:v1:' + base64 ciphertext.
-- See src/security/provider_key_crypto.rs for the format and the fail-closed rule.
--
-- pgcrypto is the encryption primitive. The runner splits this file on the statement
-- separator and treats each statement independently, so the extension is installed first.
--
-- This constraint is the regression guard: a future writer that forgets to encrypt FAILS
-- CLOSED at the database instead of silently storing a plaintext credential. An empty
-- string stays allowed so an empty slot is still representable.
--
-- NOT VALID was the correct shape ON THE LIVE DATABASE on the day this first ran: rows
-- written before the backfill stayed exempt so the app kept reading them, and the
-- constraint was validated once by hand afterwards. Measured 2026-09-25, live carries it
-- VALID (pg_constraint.convalidated = true for provider_keys_api_key_encrypted).
--
-- A from-zero build has no legacy rows to exempt, so leaving it NOT VALID here made a fresh
-- database diverge from live permanently (000000_baseline_core_tables.sql, a pg_dump of
-- live, already creates the constraint VALID, and this file then dropped it and re-added it
-- unvalidated). It is now added valid: a fresh build reproduces the live state exactly, and
-- a database that still holds plaintext rows (backfill never run) fails the deploy loudly
-- instead of serving with the guard present but unvalidated.  (Card t_468c4e29.)
--
-- The no-semicolon rule applies to every comment above: the migration runner splits files on
-- the statement separator, so a comment must never contain one.

CREATE EXTENSION IF NOT EXISTS pgcrypto;

ALTER TABLE provider_keys DROP CONSTRAINT IF EXISTS provider_keys_api_key_encrypted;

ALTER TABLE provider_keys ADD CONSTRAINT provider_keys_api_key_encrypted CHECK (api_key = '' OR api_key LIKE 'enc:v1:%');
