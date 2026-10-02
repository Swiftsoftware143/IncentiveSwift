-- t_47315540 — the three `account_id` columns that referenced `accounts` with NO foreign key.
--
-- Measured live 2026-10-02 (kanban t_47315540; enumeration /opt/swift/audits/t_47315540/):
--   portfolio_companies.account_id   14 rows, 14 dangling   (11 machine-probe residue +
--                                    3 rows of the cross-app "demo portfolio" seeded
--                                    2026-08-08 under account 869c33e6, an id that exists in
--                                    neither `accounts` nor `tenants` here — the same three
--                                    rows, by id, were retired in CoreSwift-CRM by migration
--                                    090 / t_9dc0eb64)
--   pending_emails.account_id         4 rows,  4 dangling   (queue rows for the deleted
--                                    probe account of campaign "Probe t_cd35d1ff after quiz",
--                                    all terminal `failed`, SMTP unconfigured)
--   integration_targets.account_id    0 rows                (its 6 orphans were retired by
--                                    t_305a0549; verified from the 2026-09-18 nightly dump:
--                                    api_key NULL, webhook https://example.com/hook, events {})
--
-- Because no constraint existed, an `accounts` DELETE could not cascade any of them, and the
-- column kept minting new orphans: `portfolio_companies` rows are written by
-- `business_handler::register_business` (next to the account it just created) and by the
-- internal portfolio-sync receiver, `pending_emails` rows by `email_queue::schedule_email`.
--
-- This file is the class cure, in the house shape (migrations 087/090 of CoreSwift-CRM):
--   1. it clears any backlog FIRST, in-file, so a database that still carries one does not
--      abort its own boot on the VALIDATE (and the removed-row count lands in the boot log);
--   2. it arms the constraint for every table that was missing it, guarded on both the
--      constraint and the table so a re-run — or a fresh install — changes nothing.
--
-- Delete action: ON DELETE CASCADE. Every other account-owned table in this app already
-- declares it (account_industries, api_keys, campaigns, chat_sessions, checkout_sessions,
-- credit_transactions, offer/offers, payment_providers, tag_groups, tags, …), these rows are
-- account-owned data with no reader that can reach them once the account is gone, and the
-- sibling app's 088 declares CASCADE for the identical portfolio_companies column.

DO $is9$
DECLARE
  n_pc int; n_pe int; n_it int;
BEGIN
  DELETE FROM portfolio_companies p
   WHERE NOT EXISTS (SELECT 1 FROM accounts a WHERE a.id = p.account_id);
  GET DIAGNOSTICS n_pc = ROW_COUNT;
  DELETE FROM pending_emails p
   WHERE NOT EXISTS (SELECT 1 FROM accounts a WHERE a.id = p.account_id);
  GET DIAGNOSTICS n_pe = ROW_COUNT;
  DELETE FROM integration_targets p
   WHERE NOT EXISTS (SELECT 1 FROM accounts a WHERE a.id = p.account_id);
  GET DIAGNOSTICS n_it = ROW_COUNT;
  RAISE NOTICE 'is9: cleared dangling account_id rows — portfolio_companies=% pending_emails=% integration_targets=%',
    n_pc, n_pe, n_it;
END
$is9$;

DO $is9$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'portfolio_companies_account_id_fkey')
       AND to_regclass('public.portfolio_companies') IS NOT NULL
       AND to_regclass('public.accounts') IS NOT NULL THEN
    ALTER TABLE ONLY public.portfolio_companies
        ADD CONSTRAINT portfolio_companies_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;
  END IF;
END
$is9$;

DO $is9$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'pending_emails_account_id_fkey')
       AND to_regclass('public.pending_emails') IS NOT NULL
       AND to_regclass('public.accounts') IS NOT NULL THEN
    ALTER TABLE ONLY public.pending_emails
        ADD CONSTRAINT pending_emails_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;
  END IF;
END
$is9$;

DO $is9$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'integration_targets_account_id_fkey')
       AND to_regclass('public.integration_targets') IS NOT NULL
       AND to_regclass('public.accounts') IS NOT NULL THEN
    ALTER TABLE ONLY public.integration_targets
        ADD CONSTRAINT integration_targets_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;
  END IF;
END
$is9$;
