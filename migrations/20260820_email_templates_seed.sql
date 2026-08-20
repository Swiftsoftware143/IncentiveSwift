-- Seed default email templates for IncentiveSwift (per-account overridable).
-- template_type is the key; aid NULL + is_default=true = global default.
-- Each account can override with aid=<account_id>. render uses {{key}} vars.

-- Generic winner (fallback when no per-type template exists)
INSERT INTO email_templates (template_type, name, subject, body, html_body, is_default, aid)
VALUES ('winner', 'Default Winner Email',
  '🎉 Congratulations — You Won!',
  'Congratulations {{first_name}}! You won the "{{campaign_name}}" campaign. Prize: {{prize_name}}',
  '<h2>🎉 Congratulations {{first_name}}!</h2><p>You won the <b>{{campaign_name}}</b> campaign.</p>{{#if prize_name}}<p>Prize: <b>{{prize_name}}</b></p>{{/if}}',
  true, NULL)
ON CONFLICT DO NOTHING;

-- Welcome (already exists as default; ensure present)
INSERT INTO email_templates (template_type, name, subject, body, html_body, is_default, aid)
VALUES ('welcome', 'Default Welcome Email',
  'Welcome to IncentiveSwift!',
  'Welcome {{name}}! Your account is ready.',
  '<h2>Welcome {{name}}!</h2><p>Your IncentiveSwift account is ready to go.</p>',
  true, NULL)
ON CONFLICT DO NOTHING;

-- Entry confirmation
INSERT INTO email_templates (template_type, name, subject, body, html_body, is_default, aid)
VALUES ('entry_confirmation', 'Entry Confirmation',
  'You are entered!',
  'Thanks {{first_name}} — you are entered in {{campaign_name}}.',
  '<h2>You are entered!</h2><p>Thanks {{first_name}}, your entry in <b>{{campaign_name}}</b> is confirmed.</p>',
  true, NULL)
ON CONFLICT DO NOTHING;

-- Per-type winner templates (default texts, accounts can override)
INSERT INTO email_templates (template_type, name, subject, body, html_body, is_default, aid) VALUES
('quiz_winner', 'Quiz Winner', 'You aced the quiz!', 'Nice work {{first_name}}! You won {{prize_name}}.', '<h2>You aced the quiz!</h2><p>Nice work {{first_name}} — you won <b>{{prize_name}}</b>.</p>', true, NULL),
('raffle_winner', 'Raffle Winner', 'You won the raffle!', 'Congratulations {{first_name}}! You won the raffle.', '<h2>You won the raffle!</h2><p>Congratulations {{first_name}}!</p>', true, NULL),
('spin_wheel_winner', 'Spin Winner', 'Your spin paid off!', '{{first_name}}, your spin won: {{prize_name}}', '<h2>Your spin paid off!</h2><p>{{first_name}}, you won <b>{{prize_name}}</b>.</p>', true, NULL),
('scratch_winner', 'Scratch Winner', 'You scratched a winner!', '{{first_name}}, you won: {{prize_name}}', '<h2>You scratched a winner!</h2><p>{{first_name}}, you won <b>{{prize_name}}</b>.</p>', true, NULL),
('mystery_winner', 'Mystery Winner', 'You unlocked a prize!', '{{first_name}}, you unlocked: {{prize_name}}', '<h2>You unlocked a prize!</h2><p>{{first_name}}, you unlocked <b>{{prize_name}}</b>.</p>', true, NULL),
('calculator_winner', 'Calculator Result', 'Here is your result', '{{first_name}}, here is your result for {{campaign_name}}.', '<h2>Your result</h2><p>{{first_name}}, here is your result for <b>{{campaign_name}}</b>.</p>', true, NULL),
('poll_thank_you', 'Poll Thanks', 'Thanks for voting!', 'Thanks for your vote, {{first_name}}!', '<h2>Thanks for voting!</h2><p>Thanks {{first_name}}!</p>', true, NULL),
('survey_thank_you', 'Survey Thanks', 'Thanks for your feedback!', 'Thanks for completing the survey, {{first_name}}!', '<h2>Thanks for your feedback!</h2><p>Thanks {{first_name}}!</p>', true, NULL)
ON CONFLICT DO NOTHING;
