-- CoreSwift CRM integration (per-user) — schema additions
-- Adds: iqs_submissions.classification, iqs_questions.crm_field/crm_field_type,
--       and registers the 'coreswift' available provider.

ALTER TABLE iqs_submissions ADD COLUMN IF NOT EXISTS classification varchar(50);

ALTER TABLE iqs_questions ADD COLUMN IF NOT EXISTS crm_field text;
ALTER TABLE iqs_questions ADD COLUMN IF NOT EXISTS crm_field_type text;

INSERT INTO available_providers (key, name, description)
VALUES ('coreswift', 'CoreSwift CRM', 'Push leads into CoreSwift CRM')
ON CONFLICT (key) DO NOTHING;
