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
-- NOT VALID by design: rows written before today (legacy plaintext) stay exempt so the app
-- keeps reading them, while every NEW insert/update is checked. After the one-off backfill
-- the constraint is validated once with
--     ALTER TABLE provider_keys VALIDATE CONSTRAINT provider_keys_api_key_encrypted
--
-- The no-semicolon rule applies to every comment above: the migration runner splits files on
-- the statement separator, so a comment must never contain one.

CREATE EXTENSION IF NOT EXISTS pgcrypto;

ALTER TABLE provider_keys DROP CONSTRAINT IF EXISTS provider_keys_api_key_encrypted;

ALTER TABLE provider_keys ADD CONSTRAINT provider_keys_api_key_encrypted CHECK (api_key = '' OR api_key LIKE 'enc:v1:%') NOT VALID;
