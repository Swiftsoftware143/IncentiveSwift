-- ============================================================
-- Migration 00017: Industries + Account Industry Assignment
-- ============================================================
-- Industry = Dashboard = Template Category
-- Plans define industry_limit; accounts get X industries based on their plan.

-- ============================================================
-- INDUSTRIES (admin-managed registry)
-- ============================================================
CREATE TABLE IF NOT EXISTS public.industries (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name TEXT NOT NULL,
    slug TEXT UNIQUE NOT NULL,          -- matches template_categories slug
    description TEXT,
    icon TEXT,                          -- optional emoji or icon identifier
    is_active BOOLEAN DEFAULT true,
    sort_order INTEGER DEFAULT 0,
    created_at TIMESTAMPTZ DEFAULT now(),
    updated_at TIMESTAMPTZ DEFAULT now()
);

-- ============================================================
-- ACCOUNT INDUSTRIES (which industries an account has activated)
-- ============================================================
CREATE TABLE IF NOT EXISTS public.account_industries (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    account_id UUID NOT NULL REFERENCES public.accounts(id) ON DELETE CASCADE,
    industry_id UUID NOT NULL REFERENCES public.industries(id) ON DELETE CASCADE,
    is_primary BOOLEAN DEFAULT false,   -- which dashboard shows by default
    created_at TIMESTAMPTZ DEFAULT now(),
    UNIQUE (account_id, industry_id)
);

-- Index for quick count queries (enforce plan limit)
CREATE INDEX IF NOT EXISTS idx_account_industries_account
    ON public.account_industries(account_id);

-- ============================================================
-- SEED: Default industries (common starting set)
-- ============================================================
INSERT INTO public.industries (name, slug, description, icon, sort_order) VALUES
    ('General', 'general', 'All-purpose campaign dashboard', '📋', 0),
    ('E-Commerce', 'ecommerce', 'Online retail & sales campaigns', '🛒', 1),
    ('Real Estate', 'real-estate', 'Property & agent campaigns', '🏠', 2),
    ('Healthcare', 'healthcare', 'Medical & wellness campaigns', '🏥', 3),
    ('Education', 'education', 'Schools, courses & training', '🎓', 4),
    ('Restaurant & Hospitality', 'hospitality', 'Restaurants, hotels & venues', '🍽️', 5),
    ('Non-Profit', 'nonprofit', 'Fundraising & awareness campaigns', '💚', 6),
    ('Events & Entertainment', 'events', 'Event promotion & ticketing', '🎪', 7),
    ('Financial Services', 'financial', 'Banking, insurance & finance', '💰', 8),
    ('Automotive', 'automotive', 'Dealerships & auto services', '🚗', 9)
ON CONFLICT (slug) DO NOTHING;

-- ============================================================
-- UPDATE EXISTING PLANS: Add industry_limit to features JSONB
-- Free → 1 industry, Starter → 2, Pro → 5, Enterprise → 99
-- ============================================================
UPDATE public.plans
SET features = features || '{"industry_limit": 1}'
WHERE slug = 'free' AND (features->>'industry_limit') IS NULL;

UPDATE public.plans
SET features = features || '{"industry_limit": 2}'
WHERE slug = 'starter' AND (features->>'industry_limit') IS NULL;

UPDATE public.plans
SET features = features || '{"industry_limit": 5}'
WHERE slug = 'pro' AND (features->>'industry_limit') IS NULL;

UPDATE public.plans
SET features = features || '{"industry_limit": 99}'
WHERE slug = 'enterprise' AND (features->>'industry_limit') IS NULL;

-- Assign default 'General' industry to all existing accounts that have no industries
INSERT INTO public.account_industries (account_id, industry_id, is_primary)
SELECT a.id, i.id, true
FROM public.accounts a
CROSS JOIN public.industries i
WHERE i.slug = 'general'
  AND NOT EXISTS (
      SELECT 1 FROM public.account_industries ai WHERE ai.account_id = a.id
  );
