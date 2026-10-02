-- IncentiveSwift — call logs and deal tracking: WIRE what is already there.
--
-- Both are required features in David's spec (`audits/incentiveswift-verify/`), and both were measured
-- MISSING on 2026-10-02. But "missing" meant missing CODE, not missing schema:
--
--   * `call_logs` — id, tenant_id, caller, callee, duration_secs, notes, created_at. 0 rows, 0 code
--     references, no API path.
--   * `business_loyalty_deals` — business_id/name, program_id, deal_type, deal_value,
--     deal_description, min_purchase, points_required, is_active, valid_from/until,
--     redemptions_limit, current_redemptions. A COMPLETE deal-tracking table, also with 0 rows and 0
--     code references.
--
-- So this migration deliberately creates NO new table. Creating a second deals table next to a
-- perfectly good unused one is how a schema ends up with two half-features and no answer to "which is
-- the real one" — the opposite of the clean-up David asked for.
--
-- What it does add is only what a USABLE call log needs and the phantom lacked: which way the call
-- went, how it ended, WHEN it happened (as opposed to when the row was written), and who it was about.

-- ---------------------------------------------------------------- call logs
ALTER TABLE call_logs ADD COLUMN IF NOT EXISTS direction  text        NOT NULL DEFAULT 'outbound';
ALTER TABLE call_logs ADD COLUMN IF NOT EXISTS outcome    text;
ALTER TABLE call_logs ADD COLUMN IF NOT EXISTS contact_id uuid;
ALTER TABLE call_logs ADD COLUMN IF NOT EXISTS called_at  timestamptz NOT NULL DEFAULT now();
ALTER TABLE call_logs ADD COLUMN IF NOT EXISTS updated_at timestamptz NOT NULL DEFAULT now();
ALTER TABLE call_logs ADD COLUMN IF NOT EXISTS created_by uuid;

-- 'inbound' / 'outbound' is the only pair that means anything; a free-text direction would make
-- "calls we made" unanswerable the first time someone typed "out".
ALTER TABLE call_logs DROP CONSTRAINT IF EXISTS call_logs_direction_check;
ALTER TABLE call_logs ADD CONSTRAINT call_logs_direction_check
    CHECK (direction IN ('inbound', 'outbound'));

-- The list view is "this account's calls, newest first".
CREATE INDEX IF NOT EXISTS call_logs_tenant_called_idx
    ON call_logs (tenant_id, called_at DESC);

-- ---------------------------------------------------------------- deals
-- No columns needed: business_loyalty_deals already carries everything the feature needs. Only the
-- index the list view filters on, so the wiring does not start with a sequential scan.
CREATE INDEX IF NOT EXISTS business_loyalty_deals_program_idx
    ON business_loyalty_deals (program_id, is_active);
CREATE INDEX IF NOT EXISTS business_loyalty_deals_valid_idx
    ON business_loyalty_deals (valid_until);
