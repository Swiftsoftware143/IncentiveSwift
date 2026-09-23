-- IS-7 part 2: the foreign keys of the baseline tables. They run last
-- (filename sorts after every other migration) because 7 of them reference
-- `accounts` / `campaigns`, which other files create *after* the baseline.
-- Every block is guarded on both the constraint and both tables, so re-running
-- this file -- on production, or on the next boot -- changes nothing.

DO $is7$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'account_industries_account_id_fkey')
       AND to_regclass('public.account_industries') IS NOT NULL
       AND to_regclass('public.accounts') IS NOT NULL THEN
    ALTER TABLE ONLY public.account_industries
        ADD CONSTRAINT account_industries_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;
  END IF;
END
$is7$;

DO $is7$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'account_industries_industry_id_fkey')
       AND to_regclass('public.account_industries') IS NOT NULL
       AND to_regclass('public.industries') IS NOT NULL THEN
    ALTER TABLE ONLY public.account_industries
        ADD CONSTRAINT account_industries_industry_id_fkey FOREIGN KEY (industry_id) REFERENCES public.industries(id) ON DELETE CASCADE;
  END IF;
END
$is7$;

DO $is7$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'campaign_email_templates_campaign_id_fkey')
       AND to_regclass('public.campaign_email_templates') IS NOT NULL
       AND to_regclass('public.campaigns') IS NOT NULL THEN
    ALTER TABLE ONLY public.campaign_email_templates
        ADD CONSTRAINT campaign_email_templates_campaign_id_fkey FOREIGN KEY (campaign_id) REFERENCES public.campaigns(id) ON DELETE CASCADE;
  END IF;
END
$is7$;

DO $is7$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'campaign_redirect_pages_campaign_id_fkey')
       AND to_regclass('public.campaign_redirect_pages') IS NOT NULL
       AND to_regclass('public.campaigns') IS NOT NULL THEN
    ALTER TABLE ONLY public.campaign_redirect_pages
        ADD CONSTRAINT campaign_redirect_pages_campaign_id_fkey FOREIGN KEY (campaign_id) REFERENCES public.campaigns(id) ON DELETE CASCADE;
  END IF;
END
$is7$;

DO $is7$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'credit_transactions_account_id_fkey')
       AND to_regclass('public.credit_transactions') IS NOT NULL
       AND to_regclass('public.accounts') IS NOT NULL THEN
    ALTER TABLE ONLY public.credit_transactions
        ADD CONSTRAINT credit_transactions_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;
  END IF;
END
$is7$;

DO $is7$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'inbound_messages_account_id_fkey')
       AND to_regclass('public.inbound_messages') IS NOT NULL
       AND to_regclass('public.accounts') IS NOT NULL THEN
    ALTER TABLE ONLY public.inbound_messages
        ADD CONSTRAINT inbound_messages_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id);
  END IF;
END
$is7$;

DO $is7$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'provider_keys_account_id_fkey')
       AND to_regclass('public.provider_keys') IS NOT NULL
       AND to_regclass('public.accounts') IS NOT NULL THEN
    ALTER TABLE ONLY public.provider_keys
        ADD CONSTRAINT provider_keys_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id);
  END IF;
END
$is7$;

DO $is7$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'stripe_checkout_sessions_account_id_fkey')
       AND to_regclass('public.stripe_checkout_sessions') IS NOT NULL
       AND to_regclass('public.accounts') IS NOT NULL THEN
    ALTER TABLE ONLY public.stripe_checkout_sessions
        ADD CONSTRAINT stripe_checkout_sessions_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id) ON DELETE CASCADE;
  END IF;
END
$is7$;

DO $is7$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'users_tenant_id_fkey')
       AND to_regclass('public.users') IS NOT NULL
       AND to_regclass('public.tenants') IS NOT NULL THEN
    ALTER TABLE ONLY public.users
        ADD CONSTRAINT users_tenant_id_fkey FOREIGN KEY (tenant_id) REFERENCES public.tenants(id);
  END IF;
END
$is7$;

-- The foreign key that 000001_password_resets.sql cannot create for itself (that file
-- sorts before the file that creates `accounts`). Production already carries it, so this
-- block is a no-op there; on a fresh database it lands here, after every other file.
DO $is7$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'password_resets_account_id_fkey')
       AND to_regclass('public.password_resets') IS NOT NULL
       AND to_regclass('public.accounts') IS NOT NULL THEN
    ALTER TABLE public.password_resets
        ADD CONSTRAINT password_resets_account_id_fkey FOREIGN KEY (account_id) REFERENCES public.accounts(id);
  END IF;
END
$is7$;
