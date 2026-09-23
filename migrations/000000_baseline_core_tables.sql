-- IS-7 baseline: the 31 tables that live in production but that NO migration
-- file creates. Without them a from-zero build cannot run 17 of the later files
-- (they ALTER tables nobody created) and stops at 69 of the live 101 tables.
--
-- Generated from `pg_dump --schema-only` of the production database
-- (2026-09-23, IS-7), then made idempotent: every object is created only if it
-- is absent, so this file is a NO-OP on production (which already has all of it)
-- and a real baseline on an empty database. Foreign keys live in
-- `zz_is7_baseline_foreign_keys.sql`, which runs after every other file so its
-- targets exist.
--
-- SAFETY: the migration runner exits non-zero when a file fails, so this file is
-- tested both ways (fresh database + production-in-a-rollback transaction) before
-- it is deployed.

CREATE TABLE IF NOT EXISTS public.account_industries (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    account_id uuid NOT NULL,
    industry_id uuid NOT NULL,
    is_primary boolean DEFAULT false,
    created_at timestamp with time zone DEFAULT now()
);
CREATE TABLE IF NOT EXISTS public.admin_settings (
    key text NOT NULL,
    value jsonb DEFAULT '{}'::jsonb,
    description text,
    updated_at timestamp with time zone DEFAULT now(),
    updated_by uuid
);
CREATE TABLE IF NOT EXISTS public.available_providers (
    key character varying(64) NOT NULL,
    name character varying(128) NOT NULL,
    description text,
    requires_base_url boolean DEFAULT false,
    requires_metadata jsonb DEFAULT '[]'::jsonb,
    icon character varying(32)
);
CREATE TABLE IF NOT EXISTS public.business_pledges (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    campaign_id uuid,
    business_id uuid NOT NULL,
    business_name text NOT NULL,
    business_phone text,
    offer_type text DEFAULT 'discount'::text NOT NULL,
    offer_value text,
    offer_description text,
    status text DEFAULT 'pending'::text,
    reviewed_by uuid,
    reviewed_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT now()
);
CREATE TABLE IF NOT EXISTS public.business_point_ledger (
    business_id uuid NOT NULL,
    business_name text,
    points_issued_this_month integer DEFAULT 0,
    points_redeemed_this_month integer DEFAULT 0,
    total_billed_this_month numeric(15,2) DEFAULT 0,
    total_reimbursed_this_month numeric(15,2) DEFAULT 0,
    net_position numeric(15,2) DEFAULT 0,
    next_payout_date date,
    payout_method text DEFAULT 'ach'::text,
    month_key text NOT NULL,
    updated_at timestamp with time zone DEFAULT now()
);
CREATE TABLE IF NOT EXISTS public.campaign_email_templates (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    campaign_id uuid NOT NULL,
    name text NOT NULL,
    trigger_event text DEFAULT 'on_win'::text NOT NULL,
    subject_template text DEFAULT 'You won {{prize.label}}!'::text NOT NULL,
    body_template text DEFAULT 'Congratulations! You won {{prize.label}}. Use code {{redemption.code}} to claim.'::text NOT NULL,
    from_name text DEFAULT 'IncentiveSwift'::text,
    cc_email text,
    bcc_email text,
    is_active boolean DEFAULT true NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
);
CREATE TABLE IF NOT EXISTS public.campaign_points_balance (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    campaign_id uuid NOT NULL,
    balance integer DEFAULT 0,
    updated_at timestamp with time zone DEFAULT now(),
    contact_id uuid,
    points_balance integer DEFAULT 0 NOT NULL,
    lifetime_points integer DEFAULT 0 NOT NULL
);
CREATE TABLE IF NOT EXISTS public.campaign_redirect_pages (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    campaign_id uuid NOT NULL,
    trigger_event text DEFAULT 'on_win'::text NOT NULL,
    title text DEFAULT 'Thank You!'::text,
    heading_text text,
    body_text text,
    button_text text DEFAULT 'Claim Your Prize'::text,
    button_url text,
    confetti boolean DEFAULT true,
    background_color text DEFAULT '#0f1117'::text,
    accent_color text DEFAULT '#a78bfa'::text,
    is_active boolean DEFAULT true NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
);
CREATE TABLE IF NOT EXISTS public.campaign_referrals (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    campaign_id uuid,
    referrer_contact_id uuid NOT NULL,
    referee_contact_id uuid,
    referral_code text NOT NULL,
    source text DEFAULT 'direct'::text,
    converted boolean DEFAULT false,
    converted_at timestamp with time zone,
    click_count integer DEFAULT 0,
    points_earned integer DEFAULT 0,
    created_at timestamp with time zone DEFAULT now()
);
CREATE TABLE IF NOT EXISTS public.category_redeem_caps (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    category_name text NOT NULL,
    max_redeem_percent numeric(5,2) DEFAULT 20.00,
    description text,
    is_active boolean DEFAULT true,
    created_at timestamp with time zone DEFAULT now()
);
CREATE TABLE IF NOT EXISTS public.credit_transactions (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    account_id uuid NOT NULL,
    amount integer NOT NULL,
    balance_after integer DEFAULT 0 NOT NULL,
    action text NOT NULL,
    reference_type text,
    reference_id text,
    description text,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);
CREATE TABLE IF NOT EXISTS public.email_templates (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    template_type text NOT NULL,
    name text NOT NULL,
    subject text NOT NULL,
    body text,
    html_body text,
    is_default boolean DEFAULT false,
    aid uuid,
    created_at timestamp with time zone DEFAULT now(),
    updated_at timestamp with time zone DEFAULT now()
);
CREATE TABLE IF NOT EXISTS public.inbound_messages (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    message_id text,
    from_number text NOT NULL,
    to_number text NOT NULL,
    body text,
    media_urls jsonb DEFAULT '[]'::jsonb,
    direction text DEFAULT 'inbound'::text,
    campaign_slug text,
    account_id uuid,
    processed boolean DEFAULT false,
    processed_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);
CREATE TABLE IF NOT EXISTS public.industries (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    name text NOT NULL,
    slug text NOT NULL,
    description text,
    icon text,
    is_active boolean DEFAULT true,
    sort_order integer DEFAULT 0,
    created_at timestamp with time zone DEFAULT now(),
    updated_at timestamp with time zone DEFAULT now()
);
CREATE TABLE IF NOT EXISTS public.integration_provider_presets (
    key character varying(64) NOT NULL,
    base_url character varying(512),
    updated_at timestamp with time zone DEFAULT now() NOT NULL
);
CREATE TABLE IF NOT EXISTS public.loyalty_plans (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    name character varying(255) NOT NULL,
    slug character varying(255) NOT NULL,
    description text,
    how_it_works text,
    monthly_price integer DEFAULT 0,
    monthly_zc_pool integer DEFAULT 100,
    features jsonb DEFAULT '[]'::jsonb,
    is_active boolean DEFAULT true,
    created_at timestamp with time zone DEFAULT now(),
    updated_at timestamp with time zone DEFAULT now()
);
CREATE TABLE IF NOT EXISTS public.plans (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    name character varying NOT NULL,
    slug character varying NOT NULL,
    price double precision DEFAULT 0,
    max_leads integer DEFAULT 100,
    max_tags integer DEFAULT 50,
    has_dual_routing boolean DEFAULT false,
    has_multi_tenant boolean DEFAULT false,
    has_white_label boolean DEFAULT false,
    features jsonb DEFAULT '[]'::jsonb,
    created_at timestamp with time zone DEFAULT now(),
    updated_at timestamp with time zone DEFAULT now(),
    purchase_url text,
    payment_provider character varying DEFAULT 'none'::character varying,
    description text,
    price_monthly double precision DEFAULT 0,
    price_yearly double precision DEFAULT 0,
    is_active boolean DEFAULT true,
    sort_order integer DEFAULT 0
);
CREATE TABLE IF NOT EXISTS public.point_issuance_log (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    issuing_business_id uuid NOT NULL,
    business_name text,
    member_id uuid NOT NULL,
    points_issued integer NOT NULL,
    bill_rate_cents numeric(5,4) DEFAULT 0.01,
    total_billed numeric(15,4) DEFAULT 0,
    transaction_id uuid,
    program_id uuid,
    issuance_type text DEFAULT 'scan'::text,
    created_at timestamp with time zone DEFAULT now()
);
CREATE TABLE IF NOT EXISTS public.point_redemption_log (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    redeeming_business_id uuid NOT NULL,
    business_name text,
    member_id uuid NOT NULL,
    points_redeemed integer NOT NULL,
    reimbursement_rate_cents numeric(5,4) DEFAULT 0.008,
    total_reimbursement numeric(15,4) DEFAULT 0,
    transaction_amount numeric(10,2),
    max_redeem_percent numeric(5,2) DEFAULT 20.00,
    capped_at numeric(15,4),
    transaction_id uuid,
    program_id uuid,
    created_at timestamp with time zone DEFAULT now()
);
CREATE TABLE IF NOT EXISTS public.point_treasury (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    total_points_issued bigint DEFAULT 0,
    total_points_redeemed bigint DEFAULT 0,
    total_revenue_collected numeric(15,2) DEFAULT 0,
    total_reimbursements_paid numeric(15,2) DEFAULT 0,
    outstanding_liability numeric(15,2) DEFAULT 0,
    minimum_float numeric(15,2) DEFAULT 100.00,
    updated_at timestamp with time zone DEFAULT now()
);
CREATE TABLE IF NOT EXISTS public.provider_keys (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    account_id uuid NOT NULL,
    provider character varying(64) NOT NULL,
    api_key text NOT NULL,
    base_url character varying(512),
    metadata jsonb DEFAULT '{}'::jsonb,
    is_active boolean DEFAULT true,
    scope character varying(16) DEFAULT 'account'::character varying NOT NULL,
    created_at timestamp with time zone DEFAULT now(),
    updated_at timestamp with time zone DEFAULT now(),
    CONSTRAINT provider_keys_api_key_encrypted CHECK (((api_key = ''::text) OR (api_key ~~ 'enc:v1:%'::text)))
);
CREATE TABLE IF NOT EXISTS public.referrals (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    referrer_id uuid NOT NULL,
    referred_email text,
    referred_name text,
    referral_code text NOT NULL,
    status text DEFAULT 'pending'::text,
    points_awarded integer DEFAULT 0,
    created_at timestamp with time zone DEFAULT now()
);
CREATE TABLE IF NOT EXISTS public.rotation_configs (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    campaign_id uuid NOT NULL,
    name text NOT NULL,
    description text,
    group_size integer DEFAULT 3,
    rotation_frequency text DEFAULT 'weekly'::text,
    voucher_validity_days integer DEFAULT 30,
    created_at timestamp with time zone DEFAULT now(),
    is_active boolean DEFAULT true NOT NULL,
    max_vouchers_per_rotation integer DEFAULT 5 NOT NULL
);
CREATE TABLE IF NOT EXISTS public.rotation_group_members (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    rotation_config_id uuid NOT NULL,
    business_id uuid NOT NULL,
    business_name text,
    business_category text,
    rotation_order integer DEFAULT 0,
    is_active boolean DEFAULT true,
    created_at timestamp with time zone DEFAULT now()
);
CREATE TABLE IF NOT EXISTS public.stripe_checkout_sessions (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    account_id uuid NOT NULL,
    stripe_session_id text NOT NULL,
    amount integer NOT NULL,
    credits integer NOT NULL,
    status text DEFAULT 'pending'::text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    completed_at timestamp with time zone
);
CREATE TABLE IF NOT EXISTS public.supplier_milestones (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    business_id uuid NOT NULL,
    milestone_type text NOT NULL,
    description text,
    points_awarded integer DEFAULT 0,
    awarded_at timestamp with time zone DEFAULT now(),
    verified_by uuid,
    verified_at timestamp with time zone,
    contract_value numeric(12,2),
    contract_partner text,
    metadata jsonb,
    CONSTRAINT supplier_milestones_milestone_type_check CHECK ((milestone_type = ANY (ARRAY['contract_sign'::text, 'verified_review'::text, 'supplier_referral'::text, 'onboarding'::text, 'community_contribution'::text])))
);
CREATE TABLE IF NOT EXISTS public.supplier_tier_config (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    program_id uuid NOT NULL,
    contract_sign_points integer DEFAULT 500,
    verified_review_points integer DEFAULT 250,
    supplier_referral_points integer DEFAULT 1000,
    onboarding_points integer DEFAULT 100,
    community_contribution_points integer DEFAULT 150,
    max_monthly_earn integer DEFAULT 5000,
    is_active boolean DEFAULT true,
    created_at timestamp with time zone DEFAULT now(),
    updated_at timestamp with time zone DEFAULT now()
);
CREATE TABLE IF NOT EXISTS public.tenant_settings (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    tenant_id uuid NOT NULL,
    key text NOT NULL,
    value jsonb DEFAULT '{}'::jsonb NOT NULL,
    created_at timestamp with time zone DEFAULT now(),
    updated_at timestamp with time zone DEFAULT now()
);
CREATE TABLE IF NOT EXISTS public.tenants (
    id uuid NOT NULL,
    name text NOT NULL,
    slug text,
    created_at timestamp with time zone DEFAULT now()
);
CREATE TABLE IF NOT EXISTS public.users (
    id uuid NOT NULL,
    email text NOT NULL,
    password_hash text,
    name text,
    tenant_id uuid,
    role text DEFAULT 'user'::text,
    is_active boolean DEFAULT true,
    created_at timestamp without time zone DEFAULT now(),
    updated_at timestamp without time zone DEFAULT now()
);
CREATE TABLE IF NOT EXISTS public.vouchers (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    campaign_id uuid,
    issued_to_contact_id uuid,
    source_business_id uuid,
    target_business_id uuid,
    voucher_type text DEFAULT 'discount'::text,
    discount_value text DEFAULT 0,
    redemption_code text NOT NULL,
    expires_at timestamp with time zone,
    status text DEFAULT 'active'::text,
    created_at timestamp with time zone DEFAULT now(),
    used_at timestamp with time zone
);
CREATE UNIQUE INDEX IF NOT EXISTS campaign_points_balance_campaign_contact_key ON public.campaign_points_balance USING btree (campaign_id, contact_id);
CREATE INDEX IF NOT EXISTS idx_account_industries_account ON public.account_industries USING btree (account_id);
CREATE INDEX IF NOT EXISTS idx_campaign_email_templates_campaign ON public.campaign_email_templates USING btree (campaign_id);
CREATE INDEX IF NOT EXISTS idx_campaign_points_balance_leaderboard ON public.campaign_points_balance USING btree (campaign_id, lifetime_points DESC);
CREATE INDEX IF NOT EXISTS idx_campaign_redirect_pages_campaign ON public.campaign_redirect_pages USING btree (campaign_id);
CREATE INDEX IF NOT EXISTS idx_credit_trans_account ON public.credit_transactions USING btree (account_id);
CREATE INDEX IF NOT EXISTS idx_credit_trans_action ON public.credit_transactions USING btree (action);
CREATE INDEX IF NOT EXISTS idx_credit_trans_created ON public.credit_transactions USING btree (created_at DESC);
CREATE UNIQUE INDEX IF NOT EXISTS idx_email_templates_unique ON public.email_templates USING btree (template_type, COALESCE(aid, '00000000-0000-0000-0000-000000000000'::uuid), is_default) WHERE ((aid IS NULL) AND (is_default = true));
CREATE INDEX IF NOT EXISTS idx_inbound_messages_from ON public.inbound_messages USING btree (from_number);
CREATE INDEX IF NOT EXISTS idx_inbound_messages_processed ON public.inbound_messages USING btree (processed);
CREATE INDEX IF NOT EXISTS idx_stripe_session_account ON public.stripe_checkout_sessions USING btree (account_id);
CREATE INDEX IF NOT EXISTS idx_stripe_session_id ON public.stripe_checkout_sessions USING btree (stripe_session_id);
CREATE INDEX IF NOT EXISTS idx_tenant_settings_tenant ON public.tenant_settings USING btree (tenant_id);

-- primary/unique constraints (inline PKs came with the tables)
DO $is7$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'account_industries_account_id_industry_id_key')
       AND to_regclass('public.account_industries') IS NOT NULL THEN
    ALTER TABLE ONLY public.account_industries
        ADD CONSTRAINT account_industries_account_id_industry_id_key UNIQUE (account_id, industry_id);
  END IF;
END
$is7$;

DO $is7$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'account_industries_pkey')
       AND to_regclass('public.account_industries') IS NOT NULL THEN
    ALTER TABLE ONLY public.account_industries
        ADD CONSTRAINT account_industries_pkey PRIMARY KEY (id);
  END IF;
END
$is7$;

DO $is7$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'admin_settings_pkey')
       AND to_regclass('public.admin_settings') IS NOT NULL THEN
    ALTER TABLE ONLY public.admin_settings
        ADD CONSTRAINT admin_settings_pkey PRIMARY KEY (key);
  END IF;
END
$is7$;

DO $is7$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'available_providers_pkey')
       AND to_regclass('public.available_providers') IS NOT NULL THEN
    ALTER TABLE ONLY public.available_providers
        ADD CONSTRAINT available_providers_pkey PRIMARY KEY (key);
  END IF;
END
$is7$;

DO $is7$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'business_pledges_pkey')
       AND to_regclass('public.business_pledges') IS NOT NULL THEN
    ALTER TABLE ONLY public.business_pledges
        ADD CONSTRAINT business_pledges_pkey PRIMARY KEY (id);
  END IF;
END
$is7$;

DO $is7$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'business_point_ledger_pkey')
       AND to_regclass('public.business_point_ledger') IS NOT NULL THEN
    ALTER TABLE ONLY public.business_point_ledger
        ADD CONSTRAINT business_point_ledger_pkey PRIMARY KEY (business_id, month_key);
  END IF;
END
$is7$;

DO $is7$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'campaign_email_templates_pkey')
       AND to_regclass('public.campaign_email_templates') IS NOT NULL THEN
    ALTER TABLE ONLY public.campaign_email_templates
        ADD CONSTRAINT campaign_email_templates_pkey PRIMARY KEY (id);
  END IF;
END
$is7$;

DO $is7$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'campaign_points_balance_pkey')
       AND to_regclass('public.campaign_points_balance') IS NOT NULL THEN
    ALTER TABLE ONLY public.campaign_points_balance
        ADD CONSTRAINT campaign_points_balance_pkey PRIMARY KEY (id);
  END IF;
END
$is7$;

DO $is7$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'campaign_redirect_pages_pkey')
       AND to_regclass('public.campaign_redirect_pages') IS NOT NULL THEN
    ALTER TABLE ONLY public.campaign_redirect_pages
        ADD CONSTRAINT campaign_redirect_pages_pkey PRIMARY KEY (id);
  END IF;
END
$is7$;

DO $is7$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'campaign_referrals_pkey')
       AND to_regclass('public.campaign_referrals') IS NOT NULL THEN
    ALTER TABLE ONLY public.campaign_referrals
        ADD CONSTRAINT campaign_referrals_pkey PRIMARY KEY (id);
  END IF;
END
$is7$;

DO $is7$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'category_redeem_caps_category_name_key')
       AND to_regclass('public.category_redeem_caps') IS NOT NULL THEN
    ALTER TABLE ONLY public.category_redeem_caps
        ADD CONSTRAINT category_redeem_caps_category_name_key UNIQUE (category_name);
  END IF;
END
$is7$;

DO $is7$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'category_redeem_caps_pkey')
       AND to_regclass('public.category_redeem_caps') IS NOT NULL THEN
    ALTER TABLE ONLY public.category_redeem_caps
        ADD CONSTRAINT category_redeem_caps_pkey PRIMARY KEY (id);
  END IF;
END
$is7$;

DO $is7$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'credit_transactions_pkey')
       AND to_regclass('public.credit_transactions') IS NOT NULL THEN
    ALTER TABLE ONLY public.credit_transactions
        ADD CONSTRAINT credit_transactions_pkey PRIMARY KEY (id);
  END IF;
END
$is7$;

DO $is7$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'email_templates_pkey')
       AND to_regclass('public.email_templates') IS NOT NULL THEN
    ALTER TABLE ONLY public.email_templates
        ADD CONSTRAINT email_templates_pkey PRIMARY KEY (id);
  END IF;
END
$is7$;

DO $is7$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'inbound_messages_message_id_key')
       AND to_regclass('public.inbound_messages') IS NOT NULL THEN
    ALTER TABLE ONLY public.inbound_messages
        ADD CONSTRAINT inbound_messages_message_id_key UNIQUE (message_id);
  END IF;
END
$is7$;

DO $is7$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'inbound_messages_pkey')
       AND to_regclass('public.inbound_messages') IS NOT NULL THEN
    ALTER TABLE ONLY public.inbound_messages
        ADD CONSTRAINT inbound_messages_pkey PRIMARY KEY (id);
  END IF;
END
$is7$;

DO $is7$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'industries_pkey')
       AND to_regclass('public.industries') IS NOT NULL THEN
    ALTER TABLE ONLY public.industries
        ADD CONSTRAINT industries_pkey PRIMARY KEY (id);
  END IF;
END
$is7$;

DO $is7$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'industries_slug_key')
       AND to_regclass('public.industries') IS NOT NULL THEN
    ALTER TABLE ONLY public.industries
        ADD CONSTRAINT industries_slug_key UNIQUE (slug);
  END IF;
END
$is7$;

DO $is7$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'integration_provider_presets_pkey')
       AND to_regclass('public.integration_provider_presets') IS NOT NULL THEN
    ALTER TABLE ONLY public.integration_provider_presets
        ADD CONSTRAINT integration_provider_presets_pkey PRIMARY KEY (key);
  END IF;
END
$is7$;

DO $is7$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'loyalty_plans_pkey')
       AND to_regclass('public.loyalty_plans') IS NOT NULL THEN
    ALTER TABLE ONLY public.loyalty_plans
        ADD CONSTRAINT loyalty_plans_pkey PRIMARY KEY (id);
  END IF;
END
$is7$;

DO $is7$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'loyalty_plans_slug_key')
       AND to_regclass('public.loyalty_plans') IS NOT NULL THEN
    ALTER TABLE ONLY public.loyalty_plans
        ADD CONSTRAINT loyalty_plans_slug_key UNIQUE (slug);
  END IF;
END
$is7$;

DO $is7$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'plans_pkey')
       AND to_regclass('public.plans') IS NOT NULL THEN
    ALTER TABLE ONLY public.plans
        ADD CONSTRAINT plans_pkey PRIMARY KEY (id);
  END IF;
END
$is7$;

DO $is7$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'plans_slug_key')
       AND to_regclass('public.plans') IS NOT NULL THEN
    ALTER TABLE ONLY public.plans
        ADD CONSTRAINT plans_slug_key UNIQUE (slug);
  END IF;
END
$is7$;

DO $is7$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'point_issuance_log_pkey')
       AND to_regclass('public.point_issuance_log') IS NOT NULL THEN
    ALTER TABLE ONLY public.point_issuance_log
        ADD CONSTRAINT point_issuance_log_pkey PRIMARY KEY (id);
  END IF;
END
$is7$;

DO $is7$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'point_redemption_log_pkey')
       AND to_regclass('public.point_redemption_log') IS NOT NULL THEN
    ALTER TABLE ONLY public.point_redemption_log
        ADD CONSTRAINT point_redemption_log_pkey PRIMARY KEY (id);
  END IF;
END
$is7$;

DO $is7$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'point_treasury_pkey')
       AND to_regclass('public.point_treasury') IS NOT NULL THEN
    ALTER TABLE ONLY public.point_treasury
        ADD CONSTRAINT point_treasury_pkey PRIMARY KEY (id);
  END IF;
END
$is7$;

DO $is7$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'provider_keys_account_id_provider_key')
       AND to_regclass('public.provider_keys') IS NOT NULL THEN
    ALTER TABLE ONLY public.provider_keys
        ADD CONSTRAINT provider_keys_account_id_provider_key UNIQUE (account_id, provider);
  END IF;
END
$is7$;

DO $is7$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'provider_keys_pkey')
       AND to_regclass('public.provider_keys') IS NOT NULL THEN
    ALTER TABLE ONLY public.provider_keys
        ADD CONSTRAINT provider_keys_pkey PRIMARY KEY (id);
  END IF;
END
$is7$;

DO $is7$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'referrals_pkey')
       AND to_regclass('public.referrals') IS NOT NULL THEN
    ALTER TABLE ONLY public.referrals
        ADD CONSTRAINT referrals_pkey PRIMARY KEY (id);
  END IF;
END
$is7$;

DO $is7$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'referrals_referral_code_key')
       AND to_regclass('public.referrals') IS NOT NULL THEN
    ALTER TABLE ONLY public.referrals
        ADD CONSTRAINT referrals_referral_code_key UNIQUE (referral_code);
  END IF;
END
$is7$;

DO $is7$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'rotation_configs_pkey')
       AND to_regclass('public.rotation_configs') IS NOT NULL THEN
    ALTER TABLE ONLY public.rotation_configs
        ADD CONSTRAINT rotation_configs_pkey PRIMARY KEY (id);
  END IF;
END
$is7$;

DO $is7$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'rotation_group_members_pkey')
       AND to_regclass('public.rotation_group_members') IS NOT NULL THEN
    ALTER TABLE ONLY public.rotation_group_members
        ADD CONSTRAINT rotation_group_members_pkey PRIMARY KEY (id);
  END IF;
END
$is7$;

DO $is7$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'stripe_checkout_sessions_pkey')
       AND to_regclass('public.stripe_checkout_sessions') IS NOT NULL THEN
    ALTER TABLE ONLY public.stripe_checkout_sessions
        ADD CONSTRAINT stripe_checkout_sessions_pkey PRIMARY KEY (id);
  END IF;
END
$is7$;

DO $is7$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'stripe_checkout_sessions_stripe_session_id_key')
       AND to_regclass('public.stripe_checkout_sessions') IS NOT NULL THEN
    ALTER TABLE ONLY public.stripe_checkout_sessions
        ADD CONSTRAINT stripe_checkout_sessions_stripe_session_id_key UNIQUE (stripe_session_id);
  END IF;
END
$is7$;

DO $is7$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'supplier_milestones_pkey')
       AND to_regclass('public.supplier_milestones') IS NOT NULL THEN
    ALTER TABLE ONLY public.supplier_milestones
        ADD CONSTRAINT supplier_milestones_pkey PRIMARY KEY (id);
  END IF;
END
$is7$;

DO $is7$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'supplier_tier_config_pkey')
       AND to_regclass('public.supplier_tier_config') IS NOT NULL THEN
    ALTER TABLE ONLY public.supplier_tier_config
        ADD CONSTRAINT supplier_tier_config_pkey PRIMARY KEY (id);
  END IF;
END
$is7$;

DO $is7$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'tenant_settings_pkey')
       AND to_regclass('public.tenant_settings') IS NOT NULL THEN
    ALTER TABLE ONLY public.tenant_settings
        ADD CONSTRAINT tenant_settings_pkey PRIMARY KEY (id);
  END IF;
END
$is7$;

DO $is7$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'tenant_settings_tenant_id_key_key')
       AND to_regclass('public.tenant_settings') IS NOT NULL THEN
    ALTER TABLE ONLY public.tenant_settings
        ADD CONSTRAINT tenant_settings_tenant_id_key_key UNIQUE (tenant_id, key);
  END IF;
END
$is7$;

DO $is7$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'tenants_pkey')
       AND to_regclass('public.tenants') IS NOT NULL THEN
    ALTER TABLE ONLY public.tenants
        ADD CONSTRAINT tenants_pkey PRIMARY KEY (id);
  END IF;
END
$is7$;

DO $is7$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'users_pkey')
       AND to_regclass('public.users') IS NOT NULL THEN
    ALTER TABLE ONLY public.users
        ADD CONSTRAINT users_pkey PRIMARY KEY (id);
  END IF;
END
$is7$;

DO $is7$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'vouchers_pkey')
       AND to_regclass('public.vouchers') IS NOT NULL THEN
    ALTER TABLE ONLY public.vouchers
        ADD CONSTRAINT vouchers_pkey PRIMARY KEY (id);
  END IF;
END
$is7$;

DO $is7$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'vouchers_redemption_code_key')
       AND to_regclass('public.vouchers') IS NOT NULL THEN
    ALTER TABLE ONLY public.vouchers
        ADD CONSTRAINT vouchers_redemption_code_key UNIQUE (redemption_code);
  END IF;
END
$is7$;

