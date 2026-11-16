-- zz_is33_undelivered_alert_subject_names_customer.sql — the locked-out-customer alert must name
-- the CUSTOMER in its subject, not just the account's uuid (kanban t_0f8bf1c0, parity with
-- FunnelSwift 2332b6c).
--
-- WHY: the operator's copy of this alert is what he scans in his inbox. The row seated by zz_is32
-- read 'A customer cannot sign in — no password was delivered ({{account_ref}})'. {{account_ref}}
-- is the account uuid, so every one of these messages looked identical until it was opened: the
-- one question the alert exists to answer — WHO is locked out — could not be read from the list.
-- The subject now leads with {{who}} (the owner's name with their address, or the address alone
-- when no name was given).
--
-- `{{account_ref}}` STAYS in the subject on purpose: `credential_watch`'s "already reported" test is
-- `email_send_log.subject LIKE '%' || accounts.id || '%' AND to_email = operator AND status='sent'`,
-- and `send_undelivered_credential_operator_alert` reads its own status back the same way. Drop the
-- id and every 15-minute pass re-mails the operator about every account that is still latched.
--
-- SAFE TO RE-RUN AND SAFE FOR AN OPERATOR'S WORDING: the runner records this filename after the
-- batch commits, but a boot that dies before that record lands re-runs the file, so the UPDATE is
-- guarded on the EXACT seeded sentence — an operator who rewrote this template in the console keeps
-- their wording, and a second run finds nothing to change. The INSERT covers the case where the
-- default row is missing altogether (an operator deleted it and zz_is32 is already recorded), so
-- the app never falls back to a subject that names nobody.
DO $$
DECLARE
  updated int;
  inserted int;
BEGIN
  UPDATE email_templates
     SET subject = 'A customer cannot sign in — {{who}} has no password ({{account_ref}})'
   WHERE template_type = 'undelivered_credential_operator_alert'
     AND aid IS NULL
     AND is_default = true
     AND subject = 'A customer cannot sign in — no password was delivered ({{account_ref}})';
  GET DIAGNOSTICS updated = ROW_COUNT;

  INSERT INTO email_templates (template_type, name, subject, body, html_body, is_default, aid)
  SELECT 'undelivered_credential_operator_alert',
         'Operator alert — a customer''s first password never arrived (default)',
         'A customer cannot sign in — {{who}} has no password ({{account_ref}})',
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
  GET DIAGNOSTICS inserted = ROW_COUNT;

  RAISE NOTICE 't_0f8bf1c0: undelivered_credential_operator_alert subject now names the customer (updated=%, inserted=%)',
    updated, inserted;
END
$$;
