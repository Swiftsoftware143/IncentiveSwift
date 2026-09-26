-- t_375c8c40 — the shipped `win_voucher` row promises an expiry date the platform never has.
--
-- MEASURED on the live database (`incentiveswift`, 59 `email_templates` rows) and in `src/`:
--
--   * `win_voucher` used `{{expiry_date}}` in three places
--     (subject/body `<...>Expires {{expiry_date}}.` / html `<p>Expires: <b>{{expiry_date}}</b></p>`).
--   * NOTHING in this schema carries an expiry for a CAMPAIGN reward:
--       - `campaign_wins` (the row the platform writes when a mechanic awards something —
--         `prize_id, prize_label, prize_type, redeemed, redemption_code, …`) has NO expiry column;
--       - `vouchers` has `expires_at`, but the win path does not create a voucher row and
--         `vouchers` is empty on live; the loyalty voucher flow (`handlers::loyalty_v2`) is the only
--         writer of `expires_at`, and no lifecycle producer selects a template for it;
--       - `campaign_secret_codes` / `loyalty_secret_codes` have `expires_at`, but those rows are read
--         by the secret-code redemption surfaces, whose templates (`code_accepted_reward`,
--         `code_expiring_soon`, …) have NO producer: `lifecycle_emails::lifecycle_templates` keys
--         them on `"secret_codes"`/`"tier"`/`"scratch"`, which are NOT in
--         `db::campaigns::VALID_MECHANIC_TYPES`, so those arms can never match a campaign.
--   * so for the ONE row a producer can select, the value has no source at all — and
--     `delivery::sender::entry_email_vars` (the fix in `src/`) binds `expiry_date` for nobody,
--     by design: a value that does not exist is never invented.
--
-- ARM — remove the placeholder (and the sentence that promised it) from the reachable row, exactly
-- as the card allows; the alternative ("bind it") has no datum to bind, and leaving it would keep
-- mailing `Expires: {{expiry_date}}` + the loud `template placeholders were NOT substituted` warn
-- forever. Placeholders that a sender CAN answer are bound in code instead (`ticket_number`,
-- `user_score`, `prize_name`, `reward_code`, `share_link`, `referral_link`) — no placeholder is
-- rewritten, no second brace style is introduced, and the renderer still never guesses.
--
-- The dead rows keep their placeholders: an unbound name on a row NO producer can select is a
-- dead-data finding (carded), not a mail that ships.
--
-- Idempotent: both statements are `replace()`s of text that is present exactly once and is a
-- no-op once removed. Only the fleet-wide default row (`aid IS NULL AND is_default = true`) that
-- the migration's own measurement names is touched.
UPDATE email_templates
   SET body      = replace(body, ' Expires {{expiry_date}}.', ''),
       html_body = replace(html_body, '<p>Expires: <b>{{expiry_date}}</b></p>', '')
 WHERE template_type = 'win_voucher'
   AND aid IS NULL
   AND is_default = true
   AND (body LIKE '%Expires {{expiry_date}}%' OR html_body LIKE '%Expires: <b>{{expiry_date}}%');
