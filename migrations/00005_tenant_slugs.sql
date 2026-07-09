-- Add a slug column to accounts for subdomain-based routing.
ALTER TABLE accounts ADD COLUMN IF NOT EXISTS slug text;
CREATE UNIQUE INDEX IF NOT EXISTS idx_accounts_slug ON accounts (slug) WHERE slug IS NOT NULL;

-- Generate slugs for existing accounts.
UPDATE accounts SET slug = lower(regexp_replace(coalesce(name, 'tenant-' || id::text), '[^a-zA-Z0-9]+', '-', 'g')) WHERE slug IS NULL;

-- Default tenant: give the main admin account a clear slug.
UPDATE accounts SET slug = 'admin' WHERE email = 'swiftsoftware143@yahoo.com';
