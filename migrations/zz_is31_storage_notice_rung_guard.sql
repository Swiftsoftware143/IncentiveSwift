-- THE GUARD THE 50/75/99 STORAGE REMINDER CLAIMS ON (kanban t_82551415).
--
-- `evaluate_and_notify` claims a rung with
--     INSERT INTO private_email_storage_notices (...) ON CONFLICT (tenant_id, threshold_pct, period)
--     DO NOTHING RETURNING id
-- so that a provider retry, a second inbound delivery in the same second and the periodic sweep can
-- never mail the same rung twice in a month. `zz_is28_workspaces_and_private_inbox.sql` ported the
-- TABLE from CoreSwift (whose own 142 carries `UNIQUE (tenant_id, threshold_pct, period)`) but not
-- that UNIQUE guard, so every claim raised 42P10 "there is no unique or exclusion constraint matching
-- the ON CONFLICT specification", the error was swallowed as "already claimed", and NO storage
-- reminder was ever mailed on this app — the customer only ever saw the banner.
--
-- Deduplicate first (a table with no guard may already hold repeats on an install that ran this
-- code), keeping the earliest claim per rung and month, then add the unique index the clause needs.

DELETE FROM private_email_storage_notices a
 USING private_email_storage_notices b
 WHERE a.tenant_id = b.tenant_id
   AND a.threshold_pct = b.threshold_pct
   AND a.period = b.period
   AND (a.sent_at, a.id) > (b.sent_at, b.id);

CREATE UNIQUE INDEX IF NOT EXISTS private_email_storage_notices_rung_uidx
    ON private_email_storage_notices (tenant_id, threshold_pct, period);
