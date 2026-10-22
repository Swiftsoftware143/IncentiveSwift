-- Contact-level tags.
--
-- WHY (cron 2026-10-22): two promises had nowhere to land.
--   1. The admin console's Tags screen lists "Apply a tag to contacts" as an action, and the Tags
--      screen's own copy says a system tag can trigger other actions (e.g. minting a free account
--      in another app) — but `tags` had no link to a person at all: tags only ever lived on
--      `entries.tags_applied`, which is per-ENTRY and dies with the campaign.
--   2. The Loyalty tier editor tells the operator that the reward tag "is what the member is tagged
--      with when the reward is approved" — and the only thing that happened was a line appended to
--      `contacts.notes`, which no screen or segmentation reads.
--
-- `contact_tags` is the join that makes both true: a tag from the account's own `tags` library,
-- linked to a shared `contacts` row. Rows are scoped by the owning account (a caller may only link a
-- tag and a contact it can already see), so the shared identity pool stays readable per tenant.
CREATE TABLE IF NOT EXISTS contact_tags (
    contact_id  uuid        NOT NULL REFERENCES contacts(id) ON DELETE CASCADE,
    tag_id      uuid        NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
    assigned_at timestamptz NOT NULL DEFAULT now(),
    -- 'manual' (operator pressed the button), 'loyalty' (a reward tier fired), 'import', ...
    source      text        NOT NULL DEFAULT 'manual',
    PRIMARY KEY (contact_id, tag_id)
);

CREATE INDEX IF NOT EXISTS idx_contact_tags_tag ON contact_tags (tag_id);
