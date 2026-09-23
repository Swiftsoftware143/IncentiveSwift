-- Stage 5: Add currency_name to loyalty_programs
ALTER TABLE loyalty_programs ADD COLUMN IF NOT EXISTS currency_name TEXT NOT NULL DEFAULT 'Points';
ALTER TABLE loyalty_programs ADD COLUMN IF NOT EXISTS currency_icon TEXT NOT NULL DEFAULT '⭐';
ALTER TABLE loyalty_programs ADD COLUMN IF NOT EXISTS currency_color TEXT NOT NULL DEFAULT '#0d9488';

-- Backfill: inherit from account-level settings where possible
--
-- IS-7 note: `accounts.currency_name` does not exist -- not in a from-zero build and not in
-- production either (this statement is one the old runner swallowed). It is guarded so the
-- file applies instead of aborting the boot of a fresh database.
DO $is7$
BEGIN
    IF EXISTS (
        SELECT 1 FROM information_schema.columns
        WHERE table_schema = 'public' AND table_name = 'accounts' AND column_name = 'currency_name'
    ) THEN
        UPDATE loyalty_programs lp
        SET currency_name = COALESCE(
            (SELECT a.currency_name FROM accounts a
             JOIN campaigns c ON c.account_id = a.id
             WHERE c.id = lp.campaign_id),
            'Points'
        )
        WHERE lp.currency_name = 'Points' AND lp.campaign_id IS NOT NULL;
    END IF;
END
$is7$;
