-- Track consecutive loss streaks per contact per campaign for pity timer.
CREATE TABLE IF NOT EXISTS campaign_streaks (
    contact_id uuid NOT NULL REFERENCES contacts(id) ON DELETE CASCADE,
    campaign_id uuid NOT NULL REFERENCES campaigns(id) ON DELETE CASCADE,
    loss_streak integer NOT NULL DEFAULT 0,
    last_entry_at timestamptz NOT NULL DEFAULT now(),
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (contact_id, campaign_id)
);

-- Daily spin counter per contact per campaign.
CREATE TABLE IF NOT EXISTS campaign_daily_limits (
    contact_id uuid NOT NULL REFERENCES contacts(id) ON DELETE CASCADE,
    campaign_id uuid NOT NULL REFERENCES campaigns(id) ON DELETE CASCADE,
    entry_date date NOT NULL DEFAULT CURRENT_DATE,
    entry_count integer NOT NULL DEFAULT 1,
    PRIMARY KEY (contact_id, campaign_id, entry_date)
);
