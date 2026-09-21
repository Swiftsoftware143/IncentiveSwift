-- Campaign milestones: the tables the milestone engine has always read from.
--
-- The engine (src/mechanics/milestone_engine.rs) shipped in Phase 2 and the admin
-- SPA has a Milestones panel wired to /api/v1/campaigns/:slug/milestones, but
-- neither table was ever migrated, so GET/POST on that route 500s with
-- `relation "campaign_milestones" does not exist` and no milestone can exist.
-- Idempotent so a re-run (or a partially applied deploy) is harmless.

CREATE TABLE IF NOT EXISTS campaign_milestones (
    id             uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    campaign_id    uuid NOT NULL REFERENCES campaigns(id) ON DELETE CASCADE,
    name           text NOT NULL,
    description    text,
    points_required integer NOT NULL DEFAULT 0,
    action_type    text NOT NULL DEFAULT 'fire_webhook',
    action_config  jsonb NOT NULL DEFAULT '{}'::jsonb,
    is_repeatable  boolean NOT NULL DEFAULT false,
    max_repeats    integer,
    cooldown_hours integer,
    is_active      boolean NOT NULL DEFAULT true,
    sort_order     integer NOT NULL DEFAULT 0,
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now()
);

-- The engine evaluates active milestones for one campaign on every points award.
CREATE INDEX IF NOT EXISTS idx_campaign_milestones_campaign
    ON campaign_milestones (campaign_id, points_required);

CREATE TABLE IF NOT EXISTS campaign_milestones_achieved (
    id              uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    milestone_id    uuid NOT NULL REFERENCES campaign_milestones(id) ON DELETE CASCADE,
    campaign_id     uuid NOT NULL REFERENCES campaigns(id) ON DELETE CASCADE,
    contact_id      uuid NOT NULL REFERENCES contacts(id) ON DELETE CASCADE,
    action_executed boolean NOT NULL DEFAULT true,
    action_result   jsonb,
    achieved_at     timestamptz NOT NULL DEFAULT now()
);

-- record_achieved() upserts ON CONFLICT (milestone_id, contact_id): one row per
-- milestone per contact is what makes a non-repeatable milestone fire exactly once.
CREATE UNIQUE INDEX IF NOT EXISTS uq_campaign_milestones_achieved
    ON campaign_milestones_achieved (milestone_id, contact_id);

CREATE INDEX IF NOT EXISTS idx_campaign_milestones_achieved_contact
    ON campaign_milestones_achieved (campaign_id, contact_id);
