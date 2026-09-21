-- Repair phantom columns on tables that were created by an earlier "create missing tables"
-- pass with a generic shape that matches no reader in src/. Card t_b1f346f9 (schema-drift audit).
--
-- Both tables are empty today (surfaces 0 rows, campaign_points_balance 0 rows) and every
-- statement below is additive + idempotent, so the file is safe to re-run: the migration
-- runner executes one file as ONE batch and does not record a file that fails, which means
-- a non-idempotent statement here would wedge every later boot.

-- surfaces -- src/handlers/surfaces_handler.rs binds (id, account_id, name, created_at, updated_at).
-- The table carried tenant_id (which no reader uses) and no updated_at at all, so:
--   GET  /api/v1/surfaces      answered 200 [] (list() swallows the decode error) -- the panel
--                              wired into the served admin console showed an empty list forever;
--   POST /api/v1/surfaces      INSERT (id, account_id, name) -> 500;
--   PUT  /api/v1/surfaces/{id} UPDATE ... SET updated_at -> 500.
ALTER TABLE surfaces ADD COLUMN IF NOT EXISTS account_id uuid;
ALTER TABLE surfaces ADD COLUMN IF NOT EXISTS updated_at timestamptz NOT NULL DEFAULT now();

-- campaign_points_balance -- read and written per contact by src/db/viral.rs,
-- src/handlers/viral_handler.rs, src/handlers/loyalty_v2.rs, src/handlers/campaign_secret_codes.rs
-- and src/handlers/external_grants.rs as (campaign_id, contact_id, points_balance, lifetime_points).
-- The table only had a campaign-level "balance" column, so every award failed at runtime with
-- "column contact_id does not exist" -- including the referral path already wired into the served
-- admin console, and the leaderboard (ORDER BY lifetime_points DESC).
ALTER TABLE campaign_points_balance ADD COLUMN IF NOT EXISTS contact_id uuid;
ALTER TABLE campaign_points_balance ADD COLUMN IF NOT EXISTS points_balance integer NOT NULL DEFAULT 0;
ALTER TABLE campaign_points_balance ADD COLUMN IF NOT EXISTS lifetime_points integer NOT NULL DEFAULT 0;
-- Unique index (not a constraint: CREATE CONSTRAINT has no IF NOT EXISTS and would fail on the
-- retry after a partially applied file). ON CONFLICT (campaign_id, contact_id) in
-- viral::upsert_campaign_points() needs exactly this index.
CREATE UNIQUE INDEX IF NOT EXISTS campaign_points_balance_campaign_contact_key
    ON campaign_points_balance (campaign_id, contact_id);
CREATE INDEX IF NOT EXISTS idx_campaign_points_balance_leaderboard
    ON campaign_points_balance (campaign_id, lifetime_points DESC);
