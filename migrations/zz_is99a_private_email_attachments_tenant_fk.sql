-- zz_is99a_private_email_attachments_tenant_fk.sql
--
-- THE WRONG PARENT TABLE. `zz_is99_private_email_attachments.sql` was ported from ADASwift's
-- 029 / CoreSwift's 147 where `tenants` IS the account table. In THIS app the account table is
-- `accounts` — every other private-email table (domains, boxes, messages, folders, labels,
-- storage_notices, api_keys) carries `tenant_id uuid REFERENCES accounts(id) ON DELETE CASCADE`.
-- The attachments table alone pointed at `tenants`, which here is a two-row legacy table.
--
-- Consequence, measured live on 2026-11-14 12:55 UTC:
--   WARN incentiveswift_api::private_email::attachments: an attachment could not be stored with
--   its message … error=insert or update on table "private_email_attachments" violates foreign
--   key constraint "private_email_attachments_tenant_id_fkey"
--   DETAIL: Key (tenant_id)=(…) is not present in table "tenants".
-- The parent message inserted fine (its FK is `accounts`), so the inbox kept a message that
-- claimed `has_attachments = true` and had nothing to open — the same silent data loss the
-- original migration was written to close, reintroduced one table away.
--
-- Reproduced in a rolled-back transaction against live before this file existed:
--   INSERT INTO private_email_messages (tenant_id = <a real account id>)  -> INSERT 0 1
--   INSERT INTO private_email_attachments (same tenant_id)                -> FK violation, table "tenants"
--
-- The table has never accepted a row (wrong FK), so there is no data to move: dropping the
-- constraint loses nothing. Idempotent in both directions — the app re-reads ./migrations on
-- every boot, so this file is re-run against a database that may already carry the good FK.

DO $$
BEGIN
    -- Drop the legacy constraint ONLY when it still points somewhere other than accounts.
    IF EXISTS (
        SELECT 1
          FROM pg_constraint c
         WHERE c.conrelid = 'private_email_attachments'::regclass
           AND c.conname  = 'private_email_attachments_tenant_id_fkey'
           AND pg_get_constraintdef(c.oid) NOT LIKE '%REFERENCES accounts(id)%'
    ) THEN
        ALTER TABLE private_email_attachments
            DROP CONSTRAINT private_email_attachments_tenant_id_fkey;
    END IF;

    -- Add the correct one when it is absent (first run, or just dropped above).
    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint c
         WHERE c.conrelid = 'private_email_attachments'::regclass
           AND c.conname  = 'private_email_attachments_tenant_id_fkey'
    ) THEN
        ALTER TABLE private_email_attachments
            ADD CONSTRAINT private_email_attachments_tenant_id_fkey
            FOREIGN KEY (tenant_id) REFERENCES accounts(id) ON DELETE CASCADE;
    END IF;
END $$;
