-- Migration: Custom domains for tenant campaigns

CREATE TABLE IF NOT EXISTS public.custom_domains (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id UUID NOT NULL,
    domain TEXT NOT NULL,
    target_type TEXT NOT NULL DEFAULT 'incentiveswift',
    verification_token TEXT NOT NULL DEFAULT gen_random_uuid()::text,
    verified_at TIMESTAMPTZ,
    ssl_provisioned_at TIMESTAMPTZ,
    is_active BOOLEAN DEFAULT false,
    created_at TIMESTAMPTZ DEFAULT NOW(),
    updated_at TIMESTAMPTZ DEFAULT NOW(),
    UNIQUE(domain)
);

CREATE INDEX IF NOT EXISTS idx_custom_domains_tenant ON custom_domains(tenant_id);
CREATE INDEX IF NOT EXISTS idx_custom_domains_active ON custom_domains(is_active) WHERE is_active = true;
