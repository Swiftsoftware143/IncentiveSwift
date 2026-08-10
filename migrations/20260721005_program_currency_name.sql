-- Stage 5: Add currency_name to loyalty_programs
ALTER TABLE loyalty_programs ADD COLUMN IF NOT EXISTS currency_name TEXT NOT NULL DEFAULT 'Points';
ALTER TABLE loyalty_programs ADD COLUMN IF NOT EXISTS currency_icon TEXT NOT NULL DEFAULT '⭐';
ALTER TABLE loyalty_programs ADD COLUMN IF NOT EXISTS currency_color TEXT NOT NULL DEFAULT '#0d9488';

-- Backfill: inherit from account-level settings where possible
UPDATE loyalty_programs lp
SET currency_name = COALESCE(
    (SELECT a.currency_name FROM accounts a
     JOIN campaigns c ON c.account_id = a.id
     WHERE c.id = lp.campaign_id),
    'Points'
)
WHERE lp.currency_name = 'Points' AND lp.campaign_id IS NOT NULL;

INSERT INTO _migrations (filename) VALUES ('20260721005_program_currency_name.sql');
