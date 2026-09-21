-- A separate file because 20260921_never_built_feature_tables.sql was already applied (and
-- recorded in _migrations) when this relation was found -- editing an applied migration is a
-- silent no-op, the runner skips any filename it has already recorded.
--
-- Referral credit ledger -- src/db/viral.rs:233 (log_referral_credit), reached from
-- handle_referral_credit via GET /api/v1/earn/:channel_code?ref=<referral_code>
-- (src/handlers/viral_handler.rs:319). Same class as the earn_channel tables: a complete INSERT
-- with no relation behind it, so crediting a referrer failed with "relation does not exist".
-- Write-only (no SELECT anywhere), so the column list is exactly the INSERT's.
CREATE TABLE IF NOT EXISTS referral_credit_log (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    referral_id uuid NOT NULL,
    referrer_contact_id uuid,
    campaign_id uuid NOT NULL,
    entry_id uuid,
    action_type varchar(64) NOT NULL,
    points_awarded integer NOT NULL DEFAULT 0,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_referral_credit_log_referral
    ON referral_credit_log (referral_id);
