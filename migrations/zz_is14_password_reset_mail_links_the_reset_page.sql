-- zz_is14_password_reset_mail_links_the_reset_page.sql
-- The password-reset mail sent the customer to a page that cannot accept the code.
--
-- MEASURED 2026-10-16: the shipped row read
--
--   Open {{login_url}} and click "Forgot password?", then enter the code above to choose a new
--   password.
--
-- `{{login_url}}` is the bare app origin (src/email.rs binds it to APP_URL), and
-- app.incentiveswift.com/login has NO code field — its only inputs are email / password, and its
-- "Forgot password?" link goes to /forgot-password.html, which asks for the ADDRESS and mails
-- ANOTHER code. The page that accepts the code is /reset-password (input id="t", prefilled from
-- `?token=`), and the mail never named it: a customer who forgot their password followed the mail,
-- landed somewhere with nowhere to put the code, and clicking the only link offered produced a
-- second code instead.
--
-- The stated validity is CORRECT here and is left alone: this app mints a 1-hour token
-- (auth_handler::forgot_password -> `Utc::now() + chrono::Duration::hours(1)`), unlike the siblings
-- that promise an hour and mint a day.
--
-- Placeholders are the ones the renderer already binds: {{name}}, {{app_name}}, {{token}},
-- {{login_url}}. Nothing is sent by this migration; there are no side effects beyond the row.
--
-- Idempotent by construction: the same UPDATE on the same single row yields the same values.

UPDATE email_templates
   SET body = E'Hi {{name}},\n\nWe received a request to reset the password on your {{app_name}} account.\n\nYour reset code is:\n\n{{token}}\n\nOpen {{login_url}}/reset-password?token={{token}} to choose a new password - the code is filled in for you. If that link does not open the form, go to {{login_url}}/reset-password and paste the code above.\n\nThis code expires in 1 hour, so use it soon.\n\nIf you didn''t ask for a password reset, you don''t need to do anything - your password stays the same and no changes have been made to your account.\n\n- The IncentiveSwift Team',
       html_body = E'<h2 style="margin:0 0 12px;font-size:18px;">Reset your password</h2>\n<p style="margin:0 0 12px;">Hi {{name}},</p>\n<p style="margin:0 0 12px;">We received a request to reset the password on your {{app_name}} account.</p>\n<p style="margin:0 0 12px;font-size:20px;"><code>{{token}}</code></p>\n<p style="margin:0 0 12px;">Open <a href="{{login_url}}/reset-password?token={{token}}">{{login_url}}/reset-password</a> and choose a new password - the reset code is filled in for you.</p>\n<p style="margin:0 0 12px;">If that link does not open the form, go to <strong>{{login_url}}/reset-password</strong> and paste the code above.</p>\n<p style="margin:0 0 12px;">This code expires in 1 hour, so use it soon.</p>\n<p style="margin:0 0 12px;">If you didn''t ask for a password reset, you don''t need to do anything - your password stays the same and no changes have been made to your account.</p>\n<p style="margin:0;">- The IncentiveSwift Team</p>',
       updated_at = NOW()
 WHERE id = '4836d11a-d8da-4c9c-b1fe-936682fc6822'
   AND template_type = 'password_reset';
