-- IncentiveSwift — a deal needs an OWNER.
--
-- Measured 2026-10-02, from my own proof: with deals scoped through the loyalty program, and the app's
-- convention that a program with no campaign is SHARED (`handlers/loyalty.rs::list_programs`:
-- `c.account_id = $1 OR lp.campaign_id IS NULL`), the only program that exists has campaign_id NULL —
-- so EVERY account could see and edit EVERY deal. The proof printed it plainly:
--     FAIL another account cannot edit this deal -> http 200
--     FAIL another account cannot even see this deal
--
-- The cause is structural, not a wrong WHERE clause: `business_loyalty_deals` has no column that says
-- WHO OWNS the deal. Its `business_id` names the business the deal is WITH, and `program_id` points at
-- a program that may be shared by everyone — so neither can answer "whose row is this?".
--
-- A shared program is a legitimate concept (the platform program belongs to no single account); a
-- shared deal is not. The fix is an explicit owner, so visibility of the PROGRAM and ownership of the
-- DEAL stop being the same question.

ALTER TABLE business_loyalty_deals ADD COLUMN IF NOT EXISTS owner_account_id uuid;

-- Best-effort backfill for any pre-existing rows: a deal's creator was recorded in business_id by every
-- writer that existed. This table measured 0 rows on 2026-10-02, so in practice this changes nothing —
-- it is here so the SET NOT NULL below cannot fail on a database that does have rows.
UPDATE business_loyalty_deals SET owner_account_id = business_id WHERE owner_account_id IS NULL;

ALTER TABLE business_loyalty_deals ALTER COLUMN owner_account_id SET NOT NULL;

-- The list view is "my account's deals, active first, expiring soonest first".
CREATE INDEX IF NOT EXISTS business_loyalty_deals_owner_idx
    ON business_loyalty_deals (owner_account_id, is_active);
