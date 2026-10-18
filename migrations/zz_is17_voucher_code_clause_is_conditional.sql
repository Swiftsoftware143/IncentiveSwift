-- The voucher mail promised a code it does not always have (kanban t_cdc24339).
--
-- MEASURED live 2026-10-18: a spin_wheel entrant whose prize came from `campaigns.config` (a prize
-- with no `campaign_wins` row, therefore no redemption code) received
-- "You won Free coffee on <campaign>! Code: {{reward_code}}." with the braces intact, and the
-- renderer said so: `template_render: placeholders=["reward_code"]`.
--
-- Why a section rather than different copy: the code sentence is CORRECT for a winner who has a
-- code — that is every winner the spin/scratch mechanics produce, and their code is minted per win
-- (`campaign_wins.redemption_code`). Only a config-prize winner lacks one. A single flat renderer
-- could therefore either ship the promise or delete the prize sentence; neither is acceptable.
-- `template_render::resolve_sections` resolves `{{#if key}}…{{/if}}`, so the CLAUSE now carries its
-- own condition: kept byte-for-byte when the entry has a code, omitted when it does not. No words
-- are added or removed for a code-bearing winner.
--
-- Only the two prize-shaped stage-1 rows carry that sentence, and only their GLOBAL default rows
-- (aid IS NULL, is_default) are touched: a tenant's own override is that tenant's copy.
--
-- Idempotent: the target text is set unconditionally for those two global rows.

UPDATE email_templates
   SET body = 'You won {{prize_name}} on {{campaign_name}}!{{#if reward_code}} Code: {{reward_code}}.{{/if}}',
       updated_at = NOW()
 WHERE aid IS NULL
   AND is_default = true
   AND template_type = 'win_voucher';

UPDATE email_templates
   SET html_body = '<h2>You won!</h2><p>Prize: <b>{{prize_name}}</b></p>{{#if reward_code}}<p>Code: <b>{{reward_code}}</b></p>{{/if}}',
       updated_at = NOW()
 WHERE aid IS NULL
   AND is_default = true
   AND template_type = 'win_voucher';

UPDATE email_templates
   SET body = 'You won {{prize_name}}!{{#if reward_code}} Code: {{reward_code}}.{{/if}}',
       updated_at = NOW()
 WHERE aid IS NULL
   AND is_default = true
   AND template_type = 'scratch_confirm_prize';

UPDATE email_templates
   SET html_body = '<h2>You scratched a winner!</h2><p>Prize: <b>{{prize_name}}</b></p>{{#if reward_code}}<p>Code: <b>{{reward_code}}</b></p>{{/if}}',
       updated_at = NOW()
 WHERE aid IS NULL
   AND is_default = true
   AND template_type = 'scratch_confirm_prize';
