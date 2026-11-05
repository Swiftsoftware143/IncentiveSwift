-- A member who earns a reward is told about it (the reward email was a log line).
--
-- MEASURED before this file: `src/mechanics/loyalty_checkin.rs::push_reward_notification` was a
-- placeholder — it wrote `tracing::info!("Reward notification would be sent for: …")` and returned
-- Ok(()). Both auto-approve call sites (`process_checkin` and `process_checkin_from_entry`) call it,
-- and `handlers::loyalty::approve_reward` settled a manually-approved reward without telling anyone.
-- So the reward row appeared in the console and the tag was applied to the contact, while the
-- person who earned it got silence.
--
-- The send now uses the app's ONE queued/template path
-- (`delivery::sender::send_template_by_type`), which paints the account's own logo and name onto the
-- header exactly like every other customer-facing message and withholds a fleet-harness recipient at
-- its choke point. That path selects a template by `template_type`, so this type needs a row: there
-- was no `loyalty_reward_earned` row in `email_templates` (34 other types, none for a reward).
--
-- Global default (`aid IS NULL AND is_default = true`), the same shape as the other 34, so every
-- account gets it and any account can override it with its own row later.
--
-- Merge fields bound by the producer: `first_name`, `reward_name`, `program_name`; `app_name`,
-- `app_url` and `login_url` are bound by `email::bind_app_fields`, and the branding fields by
-- `branding::bind_branding`. `{{#if first_name}}` is resolved by `template_render::resolve_sections`
-- before substitution, so a contact with no first name gets "You earned a reward" instead of
-- "You earned a reward, ."
--
-- Idempotent: inserted only when absent, plus `ON CONFLICT DO NOTHING`. Re-read on every boot by
-- this app's own runner (`src/db/migrations.rs` reads ./migrations), so it must be safe to re-run.

INSERT INTO email_templates (template_type, name, subject, body, html_body, is_default, aid)
SELECT
    'loyalty_reward_earned',
    'Loyalty - Reward Earned',
    'You earned a reward: {{reward_name}}',
    E'{{#if first_name}}Hi {{first_name}},\n\n{{/if}}You earned {{reward_name}} in {{program_name}}.\n\n'
        || E'Your reward is on your account and ready to use.\n\n'
        || E'Open {{app_name}}: {{app_url}}',
    '<h2>You earned a reward{{#if first_name}}, {{first_name}}{{/if}}</h2>'
        || '<p>You earned <strong>{{reward_name}}</strong> in {{program_name}}.</p>'
        || '<p>Your reward is on your account and ready to use.</p>'
        || '<p><a href="{{app_url}}">Open {{app_name}}</a></p>',
    true,
    NULL
WHERE NOT EXISTS (
    SELECT 1 FROM email_templates
     WHERE template_type = 'loyalty_reward_earned' AND aid IS NULL AND is_default = true
)
ON CONFLICT DO NOTHING;
