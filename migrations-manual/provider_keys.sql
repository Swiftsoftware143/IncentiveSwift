-- provider_keys table for IncentiveSwift
DROP TABLE IF EXISTS provider_keys CASCADE;

CREATE TABLE provider_keys (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    account_id UUID NOT NULL REFERENCES accounts(id),
    provider VARCHAR(64) NOT NULL,
    api_key TEXT NOT NULL,
    base_url VARCHAR(512),
    metadata JSONB DEFAULT '{}',
    is_active BOOLEAN DEFAULT true,
    scope VARCHAR(16) NOT NULL DEFAULT 'account',
    created_at TIMESTAMPTZ DEFAULT NOW(),
    updated_at TIMESTAMPTZ DEFAULT NOW(),
    UNIQUE(account_id, provider)
);

-- available_providers if not exists
CREATE TABLE IF NOT EXISTS available_providers (
    key VARCHAR(64) PRIMARY KEY,
    name VARCHAR(128) NOT NULL,
    description TEXT,
    requires_base_url BOOLEAN DEFAULT false,
    requires_metadata JSONB DEFAULT '[]',
    icon VARCHAR(32)
);

INSERT INTO available_providers (key, name, description, icon) VALUES
    ('mailgun', 'Mailgun', 'Transactional email sending', 'mail'),
    ('sendgrid', 'SendGrid', 'Email delivery service', 'mail'),
    ('sendiio', 'Sendiio', 'Email/SMS campaign delivery', 'mail'),
    ('letterman', 'Letterman', 'Newsletter content delivery', 'newspaper'),
    ('nexweave', 'Nexweave', 'Personalized video/image generation', 'video'),
    ('sam_gov', 'SAM.gov', 'Federal contracting opportunities', 'shield')
ON CONFLICT (key) DO NOTHING;
