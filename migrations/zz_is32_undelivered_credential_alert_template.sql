-- zz_is32_undelivered_credential_alert_template.sql — the WORDING for the operator alert that
-- `credential_watch` mails when a customer's first password never arrived (kanban t_b4ca6bfe).
--
-- WHY A ROW: this app has no raw send. Every transactional message goes through
-- `email::send_template_email`, which selects a row by `template_type` (aid NULL = the fleet-wide
-- default) and falls back to inline copy only when no row exists. The console's Email templates
-- card edits these rows, so the operator can reword the alert without a deploy — which is the whole
-- reason the sibling apps' hard-coded strings were not copied across.
--
-- `{{account_ref}}` is the ACCOUNT ID and it is deliberate: `email_send_log` keeps only
-- `to_email/subject/status/detail`, so the subject is the one field that can name the account.
-- `credential_watch` reads it back to decide "this operator was already told about THIS account",
-- and only a row with status `sent` counts — a withheld send must stay unreported so it retries.
--
-- IDEMPOTENT BY CONSTRUCTION: the file is read at every boot (the runner records it in
-- `_migrations`, but a boot before that record lands re-runs it), so it inserts only when the
-- default row for this type is absent. No RAISE EXCEPTION: a failure here would make the deploy
-- retry the file for ever, and a NOTICE is the success-path output by design.
DO $$
DECLARE
  n int;
BEGIN
  INSERT INTO email_templates (template_type, name, subject, body, html_body, is_default, aid)
  SELECT 'undelivered_credential_operator_alert',
         'Operator alert — a customer''s first password never arrived (default)',
         'A customer cannot sign in — no password was delivered ({{account_ref}})',
         '{{workspace}} signed up, but the email carrying their first password never reached them, so they cannot sign in.' || E'\n\n' ||
         'Workspace: {{workspace}}' || E'\n' ||
         'Customer: {{who}}' || E'\n' ||
         'Signed up: {{signed_up}}' || E'\n\n' ||
         'A reset code travels the same mail path that just failed, so it may not reach them either. Set a first password for this account from the admin console (Accounts), then tell them the password by a route that is not that mailbox: {{admin_url}}' || E'\n\n' ||
         'This message is sent once per account, and only after the mail has actually failed to arrive.' || E'\n\n' ||
         '- {{app_name}}',
         NULL, true, NULL
  WHERE NOT EXISTS (
      SELECT 1 FROM email_templates
       WHERE template_type = 'undelivered_credential_operator_alert'
         AND aid IS NULL
         AND is_default = true);
  GET DIAGNOSTICS n = ROW_COUNT;
  RAISE NOTICE 't_b4ca6bfe: undelivered_credential_operator_alert default seated (inserted=%)', n;
END
$$;
