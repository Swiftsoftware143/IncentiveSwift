-- Migration: Add credit_rate and next_pin_number to accounts (tenant table)
-- and ensure purchase_pin exists on accounts.
-- 
-- In this system, accounts IS the tenant store (tenant_id = self for tenant accounts).
-- credit_rate: credits earned per $1 in purchase verification
-- next_pin_number: sequential number generator for purchase PINs ("Z" + number)
-- purchase_pin: business purchase verification PIN, auto-generated on signup

-- Remove incorrectly-placed columns from the bad migration (20260721001)
-- These might not exist if migration 001 hasn't been run yet, so use IF EXISTS
ALTER TABLE accounts DROP COLUMN IF EXISTS credit_rate;
ALTER TABLE accounts DROP COLUMN IF EXISTS purchase_pin;

-- Add columns properly
ALTER TABLE accounts ADD COLUMN IF NOT EXISTS credit_rate INTEGER NOT NULL DEFAULT 10;
ALTER TABLE accounts ADD COLUMN IF NOT EXISTS next_pin_number INTEGER NOT NULL DEFAULT 100;
ALTER TABLE accounts ADD COLUMN IF NOT EXISTS purchase_pin VARCHAR(10) DEFAULT '0000';

COMMENT ON COLUMN accounts.credit_rate IS 'Credits per dollar earned in purchase verify';
COMMENT ON COLUMN accounts.next_pin_number IS 'Next sequential number for purchase PIN generation';
COMMENT ON COLUMN accounts.purchase_pin IS 'Business purchase verification PIN, auto-generated on signup';

-- Set default PIN for all existing accounts (temporary until they regenerate)
UPDATE accounts SET purchase_pin = '0000' WHERE purchase_pin IS NULL;
