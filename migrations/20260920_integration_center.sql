-- Integration Center standard (2026-09-20) -- IncentiveSwift.
-- 1) Fleet CoreSwift base-URL preset = step 2 of the documented resolution order.
-- 2) Canonical CoreSwift catalogue row, present exactly once.
-- 3) Catalogue rows for providers this app already reads out of provider_keys
--    (upsert_provider_key validates against the catalogue, so a missing row makes
--    the provider unusable even though the code already reads it).
-- NOTE: the filename-based runner splits statements on the statement separator,
-- so a comment must never contain that character.

CREATE TABLE IF NOT EXISTS integration_provider_presets (
    key        VARCHAR(64) PRIMARY KEY,
    base_url   VARCHAR(512),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

INSERT INTO integration_provider_presets (key, base_url)
VALUES ('coreswift', 'http://127.0.0.1:8084')
ON CONFLICT (key) DO NOTHING;

INSERT INTO available_providers (key, name, description, requires_base_url, requires_metadata, icon)
VALUES ('coreswift', 'CoreSwift CRM', 'Push leads into CoreSwift CRM', false, '[]'::jsonb, NULL)
ON CONFLICT (key) DO UPDATE
SET name = EXCLUDED.name, description = EXCLUDED.description,
    requires_base_url = EXCLUDED.requires_base_url,
    requires_metadata = EXCLUDED.requires_metadata;

INSERT INTO available_providers (key, name, description, requires_base_url, requires_metadata, icon)
VALUES
  ('telnyx_sms', 'Telnyx SMS', 'Two-way SMS delivery for prize and campaign messages', false, '["from_number"]'::jsonb, 'message'),
  ('marketing_boost', 'Marketing Boost', 'Gift-card and dining-voucher delivery for campaign winners', false, '["sender"]'::jsonb, 'gift'),
  ('activecampaign', 'ActiveCampaign', 'Autoresponder contact sync', false, '[]'::jsonb, 'mail'),
  ('mailchimp', 'Mailchimp', 'Autoresponder contact sync', false, '[]'::jsonb, 'mail'),
  ('convertkit', 'ConvertKit', 'Autoresponder contact sync', false, '[]'::jsonb, 'mail'),
  ('gohighlevel', 'GoHighLevel', 'Autoresponder and CRM contact sync', false, '[]'::jsonb, 'mail'),
  ('hubspot', 'HubSpot', 'CRM contact sync', false, '[]'::jsonb, 'mail'),
  ('system_api_key', 'System API key', 'Server-to-server key for inbound external grants', false, '[]'::jsonb, 'key')
ON CONFLICT (key) DO NOTHING;
