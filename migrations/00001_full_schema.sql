-- IncentiveSwift Full Schema
-- Migration 00001: Core tables for campaigns, entries, contacts, delivery, and plan tiers

-- ============================================================
-- CONTACTS (light CRM — one row per person, de-duplicated)
-- ============================================================
create table public.contacts (
    id uuid primary key default gen_random_uuid(),
    first_name text,
    last_name text,
    email text,
    phone text,
    business_name text,
    first_seen_at timestamptz default now(),
    last_seen_at timestamptz default now(),
    total_entries integer default 0,
    notes text,
    created_at timestamptz default now(),
    constraint contacts_email_or_phone check (email is not null or phone is not null)
);

create unique index contacts_email_idx on public.contacts (lower(email)) where email is not null;
create unique index contacts_phone_idx on public.contacts (phone) where phone is not null;

-- ============================================================
-- CAMPAIGNS (tag namespace lives here)
-- ============================================================
create table public.campaigns (
    id uuid primary key default gen_random_uuid(),
    name text not null,
    slug text unique not null,
    type text not null, -- one of 12 mechanic types
    status text default 'active', -- active | paused | completed
    config jsonb default '{}', -- mechanic-specific config
    tag_namespace text not null, -- e.g. "Summer_Giveaway" — base for all outcome tags
    outcome_tags jsonb default '{}', -- {"winner": "...", "runner_up": "...", "entrant": "..."}
    delivery_method text default 'webhook', -- webhook | direct_api
    delivery_config jsonb default '{}', -- webhook URL, API key reference, etc.
    created_at timestamptz default now()
);

-- ============================================================
-- ENTRIES (every capture across all mechanic types)
-- ============================================================
create table public.entries (
    id uuid primary key default gen_random_uuid(),
    contact_id uuid references public.contacts(id) on delete cascade,
    campaign_id uuid references public.campaigns(id) on delete cascade,
    answers jsonb default '{}', -- full Q&A set (fast path for delivery payloads)
    score integer,
    outcome text, -- 'winner' | 'runner_up' | 'entrant' | mechanic-specific
    tags_applied text[] default '{}',
    delivered boolean default false,
    delivered_at timestamptz,
    delivery_attempts integer default 0,
    created_at timestamptz default now()
);

-- ============================================================
-- QUESTION LIBRARY (stable IDs per question for reporting)
-- ============================================================
create table public.questions (
    id uuid primary key default gen_random_uuid(),
    campaign_id uuid references public.campaigns(id) on delete cascade,
    question_key text not null, -- e.g. "q1"
    question_text text not null, -- snapshot of wording at time of asking
    question_type text, -- single | multiple | numeric | text
    sort_order integer default 0
);

-- ============================================================
-- ANSWERS (normalized — one row per question answered per entry)
-- ============================================================
create table public.answers (
    id uuid primary key default gen_random_uuid(),
    entry_id uuid references public.entries(id) on delete cascade,
    question_id uuid references public.questions(id) on delete cascade,
    value text, -- normalized string value
    raw_value jsonb, -- original structured value if needed
    created_at timestamptz default now()
);

-- ============================================================
-- DELIVERY LOG (audit trail for webhook/API pushes)
-- ============================================================
create table public.delivery_log (
    id uuid primary key default gen_random_uuid(),
    entry_id uuid references public.entries(id) on delete cascade,
    method text,
    target text, -- webhook URL or API endpoint
    success boolean,
    response_code integer,
    response_body text,
    attempted_at timestamptz default now()
);

-- ============================================================
-- PLAN TIERS (admin creates as many as they like)
-- ============================================================
create table public.plan_tiers (
    id uuid primary key default gen_random_uuid(),
    name text not null,
    slug text unique not null,
    price_monthly numeric(10,2),
    price_annual numeric(10,2),
    is_active boolean default true,
    sort_order integer default 0,
    max_campaigns integer, -- null = unlimited
    max_entries_per_month integer, -- null = unlimited
    created_at timestamptz default now()
);

-- ============================================================
-- FEATURE REGISTRY (every gateable feature)
-- ============================================================
create table public.features (
    id uuid primary key default gen_random_uuid(),
    key text unique not null, -- 'mechanic_spin_wheel', 'delivery_webhook', etc.
    label text not null,
    category text, -- 'mechanic' | 'delivery' | 'branding' | 'limits' | 'module'
    description text
);

-- ============================================================
-- TIER FEATURE ASSIGNMENT (many-to-many)
-- ============================================================
create table public.tier_features (
    tier_id uuid references public.plan_tiers(id) on delete cascade,
    feature_id uuid references public.features(id) on delete cascade,
    enabled boolean default true,
    limit_value integer, -- optional numeric cap
    primary key (tier_id, feature_id)
);

-- ============================================================
-- ACCOUNTS (customer accounts linked to plan tiers)
-- ============================================================
create table public.accounts (
    id uuid primary key default gen_random_uuid(),
    name text,
    email text unique not null,
    plan_tier_id uuid references public.plan_tiers(id),
    created_at timestamptz default now()
);

-- ============================================================
-- API CREDENTIALS (hashed, never plaintext)
-- ============================================================
create table public.api_credentials (
    id uuid primary key default gen_random_uuid(),
    account_id uuid references public.accounts(id) on delete cascade,
    key_identifier text unique not null,
    key_hash text not null, -- bcrypt hash
    key_encrypted text, -- AES-encrypted for outbound direct API calls
    created_at timestamptz default now()
);

-- ============================================================
-- LOYALTY PROGRAM MODULE (upsell add-on)
-- ============================================================
create table public.loyalty_programs (
    id uuid primary key default gen_random_uuid(),
    campaign_id uuid references public.campaigns(id) on delete cascade,
    name text not null,
    recognition_method text default 'both', -- 'qr_nfc' | 'manual_lookup' | 'both'
    points_per_checkin integer default 10,
    max_checkins_per_day integer default 1,
    point_decay_days integer, -- null = never expire
    is_active boolean default true,
    created_at timestamptz default now()
);

create table public.loyalty_members (
    id uuid primary key default gen_random_uuid(),
    program_id uuid references public.loyalty_programs(id) on delete cascade,
    contact_id uuid references public.contacts(id) on delete cascade,
    points_balance integer default 0,
    lifetime_points integer default 0,
    member_since timestamptz default now(),
    last_checkin_at timestamptz,
    unique(program_id, contact_id)
);

create table public.loyalty_checkins (
    id uuid primary key default gen_random_uuid(),
    member_id uuid references public.loyalty_members(id) on delete cascade,
    points_awarded integer not null,
    method text, -- 'qr_scan' | 'nfc_tap' | 'manual_lookup'
    entry_id uuid references public.entries(id),
    checked_in_at timestamptz default now()
);

-- Enforce daily cap at DB level (common case: 1/day)
create unique index loyalty_checkins_daily_cap
    on public.loyalty_checkins (member_id, (checked_in_at::date));

create table public.loyalty_reward_tiers (
    id uuid primary key default gen_random_uuid(),
    program_id uuid references public.loyalty_programs(id) on delete cascade,
    name text not null,
    points_required integer not null,
    requires_approval boolean default false,
    reward_tag text not null,
    sort_order integer default 0
);

create table public.loyalty_rewards_earned (
    id uuid primary key default gen_random_uuid(),
    member_id uuid references public.loyalty_members(id) on delete cascade,
    tier_id uuid references public.loyalty_reward_tiers(id),
    status text default 'pending', -- pending | approved | fulfilled | denied
    earned_at timestamptz default now(),
    approved_by uuid,
    fulfilled_at timestamptz
);

-- ============================================================
-- SEED DATA: Feature Registry
-- ============================================================
insert into public.features (key, label, category, description) values
    ('mechanic_score_reveal', 'Score Reveal', 'mechanic', 'Animated score + tier message'),
    ('mechanic_spin_wheel', 'Spin to Win', 'mechanic', 'Weighted prize wheel'),
    ('mechanic_scratch_card', 'Scratch Card', 'mechanic', 'Canvas scratch-to-reveal'),
    ('mechanic_personality', 'Personality Quiz', 'mechanic', 'Shareable outcome-type result'),
    ('mechanic_calculator', 'Calculator', 'mechanic', 'Formula-driven dollar estimate'),
    ('mechanic_mystery', 'Mystery Reveal', 'mechanic', 'Locked reward unlock'),
    ('mechanic_countdown', 'Countdown Timer', 'mechanic', 'Urgency layer on any mechanic'),
    ('mechanic_poll', 'Poll', 'mechanic', 'Single-question vote + results'),
    ('mechanic_chat', 'Chat Funnel', 'mechanic', 'Conversational bubble quiz'),
    ('mechanic_leaderboard', 'Leaderboard', 'mechanic', 'Percentile benchmark'),
    ('mechanic_raffle', 'Raffle & Giveaway', 'mechanic', 'Delayed-draw entry system'),
    ('mechanic_long_form_qualifier', 'Long-Form Qualifier', 'mechanic', 'Deep logic-based pre-qualification for high-ticket offers'),
    ('delivery_webhook', 'Webhook Delivery', 'delivery', 'Push to any webhook URL'),
    ('delivery_direct_api', 'Direct API Delivery', 'delivery', 'Push straight to CRM/email API'),
    ('branding_white_label', 'White Label', 'branding', 'Remove IncentiveSwift branding'),
    ('branding_custom_domain', 'Custom Domain', 'branding', 'Host campaigns on own domain'),
    ('module_loyalty_program', 'Loyalty Program', 'module', 'Recurring point-based check-in system — sellable as standalone upsell'),
    ('limit_unlimited_campaigns', 'Unlimited Campaigns', 'limits', 'No cap on active campaigns');

-- ============================================================
-- RLS POLICIES
-- ============================================================
alter table public.contacts enable row level security;
alter table public.campaigns enable row level security;
alter table public.entries enable row level security;
alter table public.questions enable row level security;
alter table public.answers enable row level security;
alter table public.delivery_log enable row level security;
alter table public.plan_tiers enable row level security;
alter table public.features enable row level security;
alter table public.tier_features enable row level security;
alter table public.accounts enable row level security;
alter table public.api_credentials enable row level security;
alter table public.loyalty_programs enable row level security;
alter table public.loyalty_members enable row level security;
alter table public.loyalty_checkins enable row level security;
alter table public.loyalty_reward_tiers enable row level security;
alter table public.loyalty_rewards_earned enable row level security;

-- Public insert policies (entry points)
create policy "public_insert_contacts" on public.contacts for insert with check (true);
create policy "public_insert_entries" on public.entries for insert with check (true);
create policy "public_insert_answers" on public.answers for insert with check (true);
create policy "public_read_active_campaigns" on public.campaigns for select using (status = 'active');
create policy "public_read_active_programs" on public.loyalty_programs for select using (is_active = true);
create policy "public_insert_checkins" on public.loyalty_checkins for insert with check (true);

-- Authenticated access
create policy "auth_all_contacts" on public.contacts for all using (auth.role() = 'authenticated');
create policy "auth_all_campaigns" on public.campaigns for all using (auth.role() = 'authenticated');
create policy "auth_read_entries" on public.entries for select using (auth.role() = 'authenticated');
create policy "auth_all_questions" on public.questions for all using (auth.role() = 'authenticated');
create policy "auth_read_delivery_log" on public.delivery_log for select using (auth.role() = 'authenticated');
create policy "auth_all_plan_tiers" on public.plan_tiers for all using (auth.role() = 'authenticated');
create policy "auth_all_features" on public.features for all using (auth.role() = 'authenticated');
create policy "auth_all_tier_features" on public.tier_features for all using (auth.role() = 'authenticated');
create policy "auth_all_loyalty_programs" on public.loyalty_programs for all using (auth.role() = 'authenticated');
create policy "auth_all_loyalty_members" on public.loyalty_members for all using (auth.role() = 'authenticated');
create policy "auth_read_loyalty_checkins" on public.loyalty_checkins for select using (auth.role() = 'authenticated');
create policy "auth_all_reward_tiers" on public.loyalty_reward_tiers for all using (auth.role() = 'authenticated');
create policy "auth_all_rewards_earned" on public.loyalty_rewards_earned for all using (auth.role() = 'authenticated');

-- ============================================================
-- API KEYS (for CoreSwift webhook push)
-- ============================================================
CREATE TABLE IF NOT EXISTS public.api_keys (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id UUID NOT NULL REFERENCES public.accounts(id) ON DELETE CASCADE,
    user_id UUID NOT NULL REFERENCES public.accounts(id) ON DELETE CASCADE,
    name VARCHAR(255) NOT NULL DEFAULT 'default',
    key_hash VARCHAR(255) NOT NULL,
    prefix VARCHAR(8) NOT NULL,
    permissions JSONB DEFAULT '[]',
    target_url TEXT,
    last_used_at TIMESTAMPTZ,
    expires_at TIMESTAMPTZ,
    is_active BOOLEAN DEFAULT true,
    created_at TIMESTAMPTZ DEFAULT NOW(),
    updated_at TIMESTAMPTZ DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_api_keys_tenant ON public.api_keys(tenant_id);
CREATE INDEX IF NOT EXISTS idx_api_keys_user ON public.api_keys(user_id);
