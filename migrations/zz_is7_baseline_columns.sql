-- IS-7 part 3: the columns of the LIVE database that a from-zero build still did not
-- have. The base tables here (`accounts`, `loyalty_programs`, `loyalty_scans`,
-- `portfolio_companies`) are created by 00001_full_schema.sql as Supabase-era skeletons and
-- the rest of their columns exist only in the live database, so a fresh schema was missing
-- 24 of them -- including `accounts.password_hash` and `accounts.role`, without which
-- nobody can log in.
--
-- Every statement is guarded (IF EXISTS on the table, IF NOT EXISTS on the column) and runs
-- after every other file, so it is a NO-OP on production and a backfill on an empty
-- database. Definitions are copied from the live database, not invented.

ALTER TABLE IF EXISTS public.accounts ADD COLUMN IF NOT EXISTS loyalty_plan text;
ALTER TABLE IF EXISTS public.accounts ADD COLUMN IF NOT EXISTS loyalty_plan_status text DEFAULT 'inactive'::text;
ALTER TABLE IF EXISTS public.accounts ADD COLUMN IF NOT EXISTS password_hash text;
ALTER TABLE IF EXISTS public.accounts ADD COLUMN IF NOT EXISTS pool_reset_date date;
ALTER TABLE IF EXISTS public.accounts ADD COLUMN IF NOT EXISTS referrer_code text;
ALTER TABLE IF EXISTS public.accounts ADD COLUMN IF NOT EXISTS role character varying DEFAULT 'user'::character varying;
ALTER TABLE IF EXISTS public.accounts ADD COLUMN IF NOT EXISTS subscription_id text;
ALTER TABLE IF EXISTS public.accounts ADD COLUMN IF NOT EXISTS tenant_id uuid;
ALTER TABLE IF EXISTS public.accounts ADD COLUMN IF NOT EXISTS zc_pool_remaining integer DEFAULT 0;
ALTER TABLE IF EXISTS public.accounts ADD COLUMN IF NOT EXISTS zc_pool_total integer DEFAULT 0;
ALTER TABLE IF EXISTS public.loyalty_programs ADD COLUMN IF NOT EXISTS description text;
ALTER TABLE IF EXISTS public.loyalty_programs ADD COLUMN IF NOT EXISTS max_redeem_percent numeric(5,2) DEFAULT 20.00;
ALTER TABLE IF EXISTS public.loyalty_programs ADD COLUMN IF NOT EXISTS purchase_credit_rate numeric DEFAULT 1.0;
ALTER TABLE IF EXISTS public.loyalty_programs ADD COLUMN IF NOT EXISTS referral_points integer DEFAULT 50;
ALTER TABLE IF EXISTS public.loyalty_programs ADD COLUMN IF NOT EXISTS scan_points integer DEFAULT 5;
ALTER TABLE IF EXISTS public.loyalty_programs ADD COLUMN IF NOT EXISTS slug text;
ALTER TABLE IF EXISTS public.loyalty_programs ADD COLUMN IF NOT EXISTS tier_type text DEFAULT 'b2c'::text;
ALTER TABLE IF EXISTS public.loyalty_scans ADD COLUMN IF NOT EXISTS business_category text;
ALTER TABLE IF EXISTS public.loyalty_scans ADD COLUMN IF NOT EXISTS cleared_at timestamp with time zone;
ALTER TABLE IF EXISTS public.loyalty_scans ADD COLUMN IF NOT EXISTS clearinghouse_processed boolean DEFAULT false;
ALTER TABLE IF EXISTS public.loyalty_scans ADD COLUMN IF NOT EXISTS transaction_amount numeric(10,2);
ALTER TABLE IF EXISTS public.portfolio_companies ADD COLUMN IF NOT EXISTS domain text;
ALTER TABLE IF EXISTS public.portfolio_companies ADD COLUMN IF NOT EXISTS domain_verified boolean DEFAULT false;
ALTER TABLE IF EXISTS public.portfolio_companies ADD COLUMN IF NOT EXISTS subdomain text;
