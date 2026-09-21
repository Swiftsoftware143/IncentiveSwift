-- The phantom-table shape of campaign_points_balance also carried UNIQUE (campaign_id) -- one row
-- per campaign, the aggregate model that no reader in this repo implements. Every code path treats
-- the table as one row per (campaign, contact): viral::upsert_campaign_points() upserts on
-- (campaign_id, contact_id) and the leaderboard selects per contact_id. With the legacy constraint
-- in place the first contact's row lands and the second one 500s
-- ("duplicate key value violates unique constraint campaign_points_balance_campaign_id_key") --
-- reproduced live on GET /api/v1/earn/:channel_code?ref=<code>, which credits the referrer as a
-- second contact. Found while proving card t_b1f346f9; the table is empty, so nothing is lost.
ALTER TABLE campaign_points_balance
    DROP CONSTRAINT IF EXISTS campaign_points_balance_campaign_id_key;
