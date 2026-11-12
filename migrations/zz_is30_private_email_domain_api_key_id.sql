-- zz_is30_private_email_domain_api_key_id.sql
--
-- WHY THIS EXISTS. The ported inbox's domain row carries `api_key_id`, the id of a NAMED provider
-- credential a customer keeps in the credential store and points several domains at (so a rotated
-- key is replaced in one place instead of orphaning every domain that had the old one). The table
-- the port created did not carry that column, which is invisible while the table is empty and
-- turns into a hard failure the moment a real domain exists: `SELECT *` into the row struct
-- answers `column "api_key_id" does not exist` and the Domains screen 500s.
--
-- Deliberately NO foreign key: `api_key_id` names a row either in `private_email_api_keys` (a
-- Mailgun key) or in `provider_keys` (every other provider), so a single FK would refuse a
-- legitimate id from the other store.
--
-- Additive and idempotent, so a repeated boot is a no-op.

ALTER TABLE private_email_domains
    ADD COLUMN IF NOT EXISTS api_key_id uuid;
