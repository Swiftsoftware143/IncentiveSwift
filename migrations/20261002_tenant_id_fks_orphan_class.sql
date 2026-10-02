-- t_b17cc53c — the rest of the "no foreign key at all" class that t_47315540 cured one table over.
--
-- t_47315540 armed the three `account_id` columns (portfolio_companies, pending_emails,
-- integration_targets) and retired their 18 orphans. Its class census
-- (/opt/swift/audits/t_47315540/00b-parent-resolution.txt) found the same defect on the `tenant_id`
-- columns of other tables and deliberately left them, because they were outside that card's scope.
-- Measured live 2026-10-02 for this card (/opt/swift/audits/t_b17cc53c/00b-dangling.txt): of the
-- 14 `tenant_id`/`account_id` columns in this database that carry NO foreign key at all,
--   calendar_events.tenant_id   2 rows, 2 dangling    reviews.tenant_id          2 rows, 2 dangling
--   support_tickets.tenant_id   2 rows, 2 dangling
-- and the remaining 11 are clean-but-unconstrained: `knowledge_base` (10 rows) and `tenant_settings`
-- (2 rows) resolve to live accounts rows, and `call_logs`, `categories`, `custom_domains`,
-- `earn_channels`, `journal_entries`, `leads`, `surfaces` (account_id and tenant_id) are empty.
-- Because no constraint existed, an `accounts` DELETE could not cascade any of them and each column
-- kept minting new orphans — exactly the shape of the 18 retired by t_47315540.
--
-- The six dangling rows (all created 2026-08-19 13:47 in the `list-dbg`/`fmtchk` probe window, all
-- with created_by = tenant_id = a uuid that is in neither `accounts` nor `tenants`, all with
-- campaign_id and contact_id NULL) are retired below in-file, in the house shape (CoreSwift 087/090,
-- this app's 20261002_account_id_fks_portfolio_pending_integration.sql): a database that still
-- carries the backlog clears it FIRST so it cannot abort its own boot on the VALIDATE, and the
-- removed-row count lands in the boot log as deploy evidence. Everything is guarded on the
-- constraint and on both tables, so a re-run — or a fresh install — changes nothing.
--
-- Parent: `accounts(id)`. Every other ownership column in this app already points there
-- (account_industries, api_keys, campaigns, chat_sessions, checkout_sessions, credit_transactions,
-- inbound_messages, iqs_funnels, offers, payment_providers, portfolio_companies, provider_keys, …),
-- and the two unconstrained columns that DO carry rows here (`knowledge_base`, `tenant_settings`)
-- resolve to `accounts` ids, not to the one legacy `tenants` row. `users.tenant_id` is the only
-- column in this database with a validated FK to `tenants`; that is the legacy shape, not this one.
-- Delete action: ON DELETE CASCADE, matching those same tables.
--
-- Deliberately NOT armed: `accounts.tenant_id` (2 dangling of 25). It is a documented legacy
-- free-form uuid with a shipped code fix (t_47dcc978, `api_keys::resolve_owner_account_id`), it can
-- legitimately name the `tenants` row, and arming it would be a product change, not a cleanup.
-- See /opt/swift/audits/t_b17cc53c/REPORT.md.

DO $fkc$
DECLARE
  n_ce int; n_rv int; n_st int;
BEGIN
  DELETE FROM calendar_events x
   WHERE x.tenant_id IS NOT NULL
     AND NOT EXISTS (SELECT 1 FROM accounts a WHERE a.id = x.tenant_id);
  GET DIAGNOSTICS n_ce = ROW_COUNT;
  DELETE FROM reviews x
   WHERE x.tenant_id IS NOT NULL
     AND NOT EXISTS (SELECT 1 FROM accounts a WHERE a.id = x.tenant_id);
  GET DIAGNOSTICS n_rv = ROW_COUNT;
  DELETE FROM support_tickets x
   WHERE x.tenant_id IS NOT NULL
     AND NOT EXISTS (SELECT 1 FROM accounts a WHERE a.id = x.tenant_id);
  GET DIAGNOSTICS n_st = ROW_COUNT;
  RAISE NOTICE 'fkc: cleared dangling tenant_id rows — calendar_events=% reviews=% support_tickets=%',
    n_ce, n_rv, n_st;
END
$fkc$;

-- Arm every unconstrained ownership column, so the class cannot come back. Names follow the
-- catalog's own convention (`<table>_<column>_fkey`).
DO $fkc$
DECLARE r record; cname text;
BEGIN
  FOR r IN SELECT * FROM (VALUES
      ('calendar_events','tenant_id'), ('reviews','tenant_id'), ('support_tickets','tenant_id'),
      ('knowledge_base','tenant_id'),  ('tenant_settings','tenant_id'),
      ('call_logs','tenant_id'),       ('categories','tenant_id'),
      ('custom_domains','tenant_id'),  ('earn_channels','account_id'),
      ('journal_entries','tenant_id'), ('leads','tenant_id'),
      ('surfaces','account_id'),       ('surfaces','tenant_id')
    ) AS v(tbl, col)
  LOOP
    cname := r.tbl || '_' || r.col || '_fkey';
    IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = cname)
       AND to_regclass('public.' || r.tbl) IS NOT NULL
       AND to_regclass('public.accounts') IS NOT NULL THEN
      EXECUTE format(
        'ALTER TABLE ONLY public.%I ADD CONSTRAINT %I FOREIGN KEY (%I) REFERENCES public.accounts(id) ON DELETE CASCADE',
        r.tbl, cname, r.col);
    END IF;
  END LOOP;
END
$fkc$;
