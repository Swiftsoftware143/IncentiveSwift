-- The float rule for IncentiveSwift's OWN loyalty programme (kanban t_69e6598e, David 2026-10-04).
--
-- WHAT WAS THERE: `point_treasury` carried the counters, `minimum_float` and `on_float_breach`, and the
-- guard compared exactly ONE number — `available >= minimum_float`. One flat figure cannot adapt: a
-- programme with a hundred points outstanding and one with a million need the same *relationship* held,
-- not the same dollar amount. And a brand-new programme has near-zero liability AND near-zero recent
-- redemption volume, so the two adaptive conditions would both pass while it held nothing at all.
--
-- THE RULE IS THREE INDEPENDENT CONDITIONS; a redemption is HELD if any of them fails:
--   coverage : available >= float_coverage_pct% x outstanding_liability
--   burn     : available >= float_burn_months  x trailing-30-day redemption volume
--   floor    : available >= minimum_float
--
-- `minimum_float` is KEPT AS THE FLOOR (the risk condition), so no data migration and no existing
-- setting changes meaning. These two columns are the adaptive conditions, and both default to the
-- conservative best practice: cover 100% of the liability, hold one month of recent redemptions.
--
-- A malformed setting falls back to the conservative default IN CODE (float_rule.rs), so a bad value
-- fails safe (toward holding) and never open (toward paying). The CHECKs below are the second arm: a
-- zero or negative value cannot even be configured.
--
-- Idempotent: the filename-based runner in src/db/migrations.rs records this file in `_migrations`
-- and never re-runs it, but a fresh install and a re-run both land the same schema.

ALTER TABLE point_treasury
    ADD COLUMN IF NOT EXISTS float_coverage_pct numeric(6,2) NOT NULL DEFAULT 100.00;

ALTER TABLE point_treasury
    ADD COLUMN IF NOT EXISTS float_burn_months numeric(6,2) NOT NULL DEFAULT 1.00;

-- ADD CONSTRAINT has no IF NOT EXISTS, so guard each one on pg_constraint (the runner re-runs the whole
-- file as one batch, and this makes the file safe to execute more than once).
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint WHERE conname = 'point_treasury_float_coverage_pct_positive'
    ) THEN
        ALTER TABLE point_treasury
            ADD CONSTRAINT point_treasury_float_coverage_pct_positive CHECK (float_coverage_pct > 0);
    END IF;

    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint WHERE conname = 'point_treasury_float_burn_months_positive'
    ) THEN
        ALTER TABLE point_treasury
            ADD CONSTRAINT point_treasury_float_burn_months_positive CHECK (float_burn_months > 0);
    END IF;
END $$;
