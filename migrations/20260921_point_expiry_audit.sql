-- Point expiry audit trail: one row per expire-points run.
-- NOTE: the runner splits this file on the statement separator, so no comment
-- in here may contain one.
CREATE TABLE IF NOT EXISTS point_expiry_audit (
    id UUID PRIMARY KEY,
    run_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    completed_at TIMESTAMPTZ,
    status TEXT NOT NULL DEFAULT 'pending',
    default_expire_days INTEGER NOT NULL,
    members_affected INTEGER NOT NULL DEFAULT 0,
    total_points_expired BIGINT NOT NULL DEFAULT 0,
    treasury_liability_reduction NUMERIC NOT NULL DEFAULT 0,
    details JSONB NOT NULL DEFAULT '[]'::jsonb
)
