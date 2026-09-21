-- Tables that whole feature areas in this repo read but that were NEVER created by any migration
-- (verified 2026-09-21 against migrations/ and information_schema; card t_b1f346f9).
--
-- Two complete handler families were shipped against these relations:
--   src/handlers/viral_handler.rs  (+ src/db/viral.rs)   -- viral earn channels
--   src/handlers/campaign_secret_codes.rs                -- campaign promo codes
-- Every route in both families could only answer HTTP 500 "relation does not exist".
--
-- The column list below is derived FROM THE CODE (every SELECT/INSERT/UPDATE/RETURNING site and
-- the sqlx::FromRow structs), not invented: EarnChannel in src/db/viral.rs:13 and
-- CampaignSecretCode in src/handlers/campaign_secret_codes.rs:12. Defaults match what the
-- handlers assume when they omit a column (e.g. is_active must default true, because
-- get_active_channel_by_code() filters on is_active = true and INSERT does not set it).
--
-- All statements are additive + idempotent (CREATE ... IF NOT EXISTS).

-- ---------------------------------------------------------------------------
-- Viral earn channels -- src/db/viral.rs (EarnChannel), viral_handler.rs:485-587
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS earn_channels (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    account_id uuid NOT NULL,
    campaign_id uuid NOT NULL REFERENCES campaigns(id) ON DELETE CASCADE,
    -- channel_code is looked up WITHOUT a campaign filter (get_active_channel_by_code),
    -- so it has to be unique across the whole table, not per campaign.
    channel_code varchar(64) NOT NULL,
    label varchar(255) NOT NULL DEFAULT '',
    description text NOT NULL DEFAULT '',
    points_per_click integer NOT NULL DEFAULT 0,
    -- 0 = unlimited (viral_handler.rs:255 "if channel.max_clicks_per_contact > 0")
    max_clicks_per_contact integer NOT NULL DEFAULT 0,
    redirect_url text NOT NULL DEFAULT '',
    -- auto_approve_all | auto_approve_answer | manual_approve
    verification_type varchar(32) NOT NULL DEFAULT 'auto_approve_all',
    expected_answer text,
    verification_label varchar(255) NOT NULL DEFAULT '',
    approval_notes text NOT NULL DEFAULT '',
    is_active boolean NOT NULL DEFAULT true,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT earn_channels_channel_code_key UNIQUE (channel_code)
);
CREATE INDEX IF NOT EXISTS idx_earn_channels_campaign ON earn_channels (campaign_id);
CREATE INDEX IF NOT EXISTS idx_earn_channels_active_code ON earn_channels (channel_code) WHERE is_active;

-- Click-through log -- src/db/viral.rs:64-96 (log_earn_click) and :48 (count per contact)
CREATE TABLE IF NOT EXISTS earn_click_log (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    channel_id uuid NOT NULL REFERENCES earn_channels(id) ON DELETE CASCADE,
    contact_id uuid,
    campaign_id uuid NOT NULL,
    ip_address text,
    user_agent text,
    referrer_url text,
    utm_source text,
    utm_medium text,
    utm_campaign text,
    points_awarded integer NOT NULL DEFAULT 0,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_earn_click_log_channel_contact
    ON earn_click_log (channel_id, contact_id);

-- ---------------------------------------------------------------------------
-- Campaign secret codes -- src/handlers/campaign_secret_codes.rs (CampaignSecretCode)
-- Campaign-scoped promo codes that award CAMPAIGN points (campaign_points_balance), i.e. a
-- different currency from the loyalty-program codes in loyalty_secret_codes (00016).
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS campaign_secret_codes (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    campaign_id uuid NOT NULL REFERENCES campaigns(id) ON DELETE CASCADE,
    code varchar(64) NOT NULL,
    points integer NOT NULL DEFAULT 100,
    max_uses integer,
    uses_count integer NOT NULL DEFAULT 0,
    is_active boolean NOT NULL DEFAULT true,
    expires_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    -- The handler matches this exact constraint name to turn a duplicate into a 400
    -- (campaign_secret_codes.rs:88), so it is declared explicitly and in this column order.
    CONSTRAINT campaign_secret_codes_code_campaign_id_key UNIQUE (code, campaign_id)
);
CREATE INDEX IF NOT EXISTS idx_campaign_secret_codes_campaign
    ON campaign_secret_codes (campaign_id);

-- Redemption ledger -- read at campaign_secret_codes.rs:159 and :226-236
CREATE TABLE IF NOT EXISTS campaign_secret_code_redemptions (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    secret_code_id uuid NOT NULL REFERENCES campaign_secret_codes(id) ON DELETE CASCADE,
    contact_id uuid NOT NULL,
    campaign_id uuid NOT NULL,
    points_awarded integer NOT NULL DEFAULT 0,
    redeemed_at timestamptz NOT NULL DEFAULT now(),
    -- one use per member per code (also makes the check-then-insert race-safe)
    CONSTRAINT campaign_secret_code_redemptions_code_contact_key UNIQUE (secret_code_id, contact_id)
);
CREATE INDEX IF NOT EXISTS idx_campaign_secret_code_redemptions_campaign
    ON campaign_secret_code_redemptions (campaign_id);
