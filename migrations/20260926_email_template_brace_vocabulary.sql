-- t_e43521d2 — `email_templates` shipped a placeholder vocabulary that NOTHING binds.
--
-- MEASURED on the live database (`incentiveswift`) before this file:
--   * `src/email.rs::render_template`, `src/delivery/sender.rs::render_template` and
--     `src/delivery/output_actions.rs::render_template` all substitute `{{key}}` — DOUBLE
--     braces — and nothing else.
--   * Both sides of the ADMIN surface advertise the same double-brace vocabulary:
--     `GET /api/v1/email-templates/merge-fields` (`handlers::email_templates_handler::
--     merge_field_list`, rendered as `format!("{{{{{}}}}}", token)`) and the served console's
--     system-mail buttons (`MERGE_FIELDS` in `www-admin/index.html:1192`).
--   * 57 of the 58 `email_templates` rows agree with both. Exactly ONE row did not: the
--     fleet-wide default `welcome` row (`aid IS NULL AND is_default = true`,
--     id 3b87e752-fc88-4629-92b7-4528e228a25c, created 2026-08-08T19:34:54Z — i.e. legacy
--     data, its text appears nowhere in `migrations/`), whose subject was
--     `Welcome to {app_name}!` and whose body/html_body carried 12 more single-brace
--     occurrences of `{app_name}`/`{name}`/`{email}`/`{password}`/`{login_url}`.
--     The live register producer selects THAT row, so every signup welcome mail left the box
--     with the braces verbatim (observed on the deployed binary at the app's own
--     `subject=Welcome to {app_name}!` warn line, /opt/swift/audits/t_017517a9/41-live-both-ways.txt).
--
-- ARM — (b) rewrite the affected ROW to the double-brace vocabulary, not (a) teach the
-- renderers to also accept single braces. (a) would have hidden the drift that produced this
-- defect (the next single-brace row would render by accident, so nobody would learn the
-- vocabulary was still split) and would corrupt any HTML/CSS body, where `{` and `}` are
-- ordinary characters. The renderers now log every unsubstituted placeholder BY NAME
-- (`src/template_render.rs`), so a future mismatch is loud instead of being mailed as copy.
--
-- TWO STATEMENTS OF CONTENT SURGERY, both forced by the vocabulary and both measured:
--
--  1. The `welcome` row is selected by `handlers::auth_handler::register`, where the account
--     holder typed their OWN password seconds earlier. That producer has no password to bind —
--     and mailing a user-chosen one is what every other credential mail in the fleet avoids
--     (WorkflowSwift `checkout_handler.rs`, missedcallrespondr `checkout_handler.rs`,
--     FunnelSwift `admin_handler.rs` all bind a GENERATED password). Leaving `{{password}}` in
--     the row would ship a half-substituted body: prose promising a credential that no caller
--     supplies. So the credentials line moves to its own template_type.
--  2. `welcome_credentials` is the ONLY flow that mints the password
--     (`generate_temp_password()` in `billing::webhooks` -> `email::send_welcome_email`, which
--     now selects this type). Its row is an INSERT..SELECT COPY of the `welcome` row taken
--     BEFORE the line is removed, so the credential text is relocated byte-for-byte, not
--     retyped and not lost.
--
-- Idempotent: the brace pass parks `{{`/`}}` in chr(1)/chr(2) sentinels, so a row that already
-- speaks double braces is returned unchanged (that is why the other 57 rows are unaffected),
-- the credentials row is inserted only when absent, and the password-line removal is a
-- `replace()` that is a no-op once the line is gone.

-- 1 ── the vocabulary pass -----------------------------------------------------------------
CREATE OR REPLACE FUNCTION _t_e43521d2_double_braces(t text) RETURNS text
LANGUAGE sql IMMUTABLE STRICT AS $$
  SELECT replace(replace(replace(replace(replace(replace(t,
           '{{', chr(1)), '}}', chr(2)), '{', '{{'), '}', '}}'), chr(1), '{{'), chr(2), '}}')
$$;

-- `(^|[^{])\{[a-z_]+\}` matches a genuinely single-braced placeholder only: inside `{{name}}`
-- the inner `{` is preceded by `{`, and the outer one is not followed by an identifier
-- character. Mustache scaffolding (`{{#if x}}` / `{{/if}}`) never matches either.
UPDATE email_templates
   SET subject   = _t_e43521d2_double_braces(subject),
       body      = _t_e43521d2_double_braces(body),
       html_body = _t_e43521d2_double_braces(html_body)
 WHERE subject   ~ '(^|[^{])\{[a-z_]+\}'
    OR body      ~ '(^|[^{])\{[a-z_]+\}'
    OR html_body ~ '(^|[^{])\{[a-z_]+\}';

-- 2 ── the credentials row (copy taken while the password line is still in place) -----------
INSERT INTO email_templates (template_type, name, subject, body, html_body, is_default, aid)
SELECT 'welcome_credentials', 'Welcome - Login Credentials', subject, body, html_body, true, NULL
  FROM email_templates
 WHERE template_type = 'welcome'
   AND aid IS NULL
   AND is_default = true
   AND NOT EXISTS (
         SELECT 1 FROM email_templates WHERE template_type = 'welcome_credentials'
       )
 ORDER BY created_at DESC
 LIMIT 1
ON CONFLICT DO NOTHING;

-- 3 ── the self-signup welcome row stops promising a credential it cannot bind -------------
UPDATE email_templates
   SET body      = replace(body, E'\nPassword: {{password}}\n', E'\n'),
       html_body = replace(html_body, '<br>Password: {{password}}', '')
 WHERE template_type = 'welcome'
   AND aid IS NULL
   AND is_default = true;

DROP FUNCTION _t_e43521d2_double_braces(text);
