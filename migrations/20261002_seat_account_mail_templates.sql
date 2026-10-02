-- kanban t_0eed3151 — seat the two ACCOUNT mails a real caller asks for and no row carried.
--
-- MEASURED (live `incentiveswift`, HEAD fa006f25): `select count(*) from email_templates where
-- template_type in ('purchase_confirmed','password_reset','notification','general')` -> 0, while two
-- shipped producers ask for two of them:
--   * `src/billing/webhooks.rs::deliver_credentials` -> `email::send_purchase_confirmed_email`
--     (`template_type = 'purchase_confirmed'`; its vars are name / plan_name / app_url);
--   * `src/email.rs::send_reset_email` (`template_type = 'password_reset'`; its vars are token /
--     name / app_url).
-- Both go through `email::send_template_email`, whose inline fallback (`send_inline`) carries a
-- hardcoded body for exactly these two types — measured live on a copy of this DB: nothing was
-- dropped, the mail DID go on the wire, but the copy lived in the BINARY. An operator could not see
-- it, could not edit it, and the console's Type picker could not even name it (it offered only
-- welcome / password_reset / notification / general). This migration moves that copy into the
-- catalogue — byte-for-byte what `send_inline` produced — using only the merge fields each caller
-- really binds, so it becomes visible and editable like every other row.
--
-- `html_body` is deliberately NULL: presence of that column IS the "this row is HTML" flag
-- (`email::send_template_email` turns on the HTML part when `html_body.is_some()`), and the inline
-- arm sends no HTML part. A non-NULL value here would silently change the mail's shape.
--
-- Idempotent: each INSERT is guarded by NOT EXISTS on the platform-default shape
-- (`aid IS NULL AND is_default = true`), the only shape `delivery::sender::load_template_by_type`
-- accepts as a fleet default. Reversing = DELETE these two rows: the inline fallback remains, so the
-- mails keep going out exactly as they did before this migration.
--
-- A NOTICE is the only output on the success path by design: the in-app runner records a file
-- `_migrations` row only when the whole batch succeeds, and a RAISE EXCEPTION here would make the
-- deploy retry this file forever (see 20261002_winner_template_strips_mustache_scaffolding.sql).

DO $$
DECLARE
  n int;
  total int;
BEGIN
  INSERT INTO email_templates (template_type, name, subject, body, html_body, is_default, aid)
  SELECT 'purchase_confirmed',
         'Purchase Confirmation (default)',
         'Payment Received - Thank You!',
         'Hi {{name}},' || E'\n\n' ||
         'Thank you for your purchase! Your payment for the {{plan_name}} plan has been received successfully.' || E'\n\n' ||
         'You can access your dashboard at: {{app_url}}/dashboard' || E'\n\n' ||
         'If you have any questions, please contact support@incentiveswift.com' || E'\n\n' ||
         'Best regards,' || E'\n' ||
         'The IncentiveSwift Team',
         NULL, true, NULL
  WHERE NOT EXISTS (
      SELECT 1 FROM email_templates
       WHERE template_type = 'purchase_confirmed' AND aid IS NULL AND is_default = true);
  GET DIAGNOSTICS n = ROW_COUNT;
  RAISE NOTICE 't_0eed3151: purchase_confirmed default seated (inserted=%)', n;

  INSERT INTO email_templates (template_type, name, subject, body, html_body, is_default, aid)
  SELECT 'password_reset',
         'Password Reset (default)',
         'Password Reset Request',
         'Your password reset code is: {{token}}' || E'\n\n' ||
         'This code expires in 1 hour.' || E'\n\n' ||
         'If you did not request this password reset, please ignore this email.' || E'\n\n' ||
         '- SwiftSoftware',
         NULL, true, NULL
  WHERE NOT EXISTS (
      SELECT 1 FROM email_templates
       WHERE template_type = 'password_reset' AND aid IS NULL AND is_default = true);
  GET DIAGNOSTICS n = ROW_COUNT;
  RAISE NOTICE 't_0eed3151: password_reset default seated (inserted=%)', n;

  SELECT count(*) INTO total FROM email_templates;
  RAISE NOTICE 't_0eed3151: email_templates catalogue now holds % rows', total;
END
$$;
