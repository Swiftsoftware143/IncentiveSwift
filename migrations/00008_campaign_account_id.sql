-- Add account_id to campaigns for tenant-scoped access.
ALTER TABLE campaigns ADD COLUMN IF NOT EXISTS account_id uuid REFERENCES accounts(id) ON DELETE CASCADE;
CREATE INDEX IF NOT EXISTS idx_campaigns_account_id ON campaigns (account_id);
