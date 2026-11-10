-- The Payment-Received template told a paying customer to write to a mailbox that cannot receive.
--
-- Measured 2026-11-10: `swiftsoftware.net` publishes NO MX record (dns.google and
-- cloudflare-dns.com both answer an empty ANSWER section) and nothing on this host listens on
-- port 25, so the receipt — the one mail a customer is most likely to reply to — handed out
-- `support@swiftsoftware.net` and every reply bounced. This app's code-built mail already names
-- its own served contact form (src/email.rs: "send us a message at
-- https://app.incentiveswift.com/contact.html (no sign-in needed)"); this STORED row was the last
-- place still printing the dead address.
--
-- Idempotent by construction: `replace()` is a no-op once the old string is gone, so the boot
-- runner may re-apply this file on every start (this app reads `migrations/` with read_dir and
-- keeps a filename-keyed ledger). Only the support sentence changes; every placeholder
-- ({{plan_name}}, {{app_url}}, …) and the rest of the body is untouched.

UPDATE email_templates
   SET body = replace(
           body,
           'If you have any questions, please contact support@swiftsoftware.net',
           'If you have any questions, send us a message at https://app.incentiveswift.com/contact.html (no sign-in needed)')
 WHERE body LIKE '%contact support@swiftsoftware.net%';

-- Any other occurrence of the dead address in either body column is pointed at the same page,
-- so no template can keep handing out a mailbox nothing can deliver to.
UPDATE email_templates
   SET body = replace(body, 'support@swiftsoftware.net', 'app.incentiveswift.com/contact.html')
 WHERE body LIKE '%support@swiftsoftware.net%';

UPDATE email_templates
   SET html_body = replace(html_body, 'support@swiftsoftware.net', 'app.incentiveswift.com/contact.html')
 WHERE html_body LIKE '%support@swiftsoftware.net%';
