-- Add email and description columns to portfolio_companies
ALTER TABLE portfolio_companies ADD COLUMN IF NOT EXISTS email TEXT DEFAULT '';
ALTER TABLE portfolio_companies ADD COLUMN IF NOT EXISTS description TEXT DEFAULT '';
