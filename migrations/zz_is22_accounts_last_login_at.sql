-- The operator's workspace list could not answer "did this customer ever get in?".
--
-- Every signup mints an account and mails its credentials, but whether the person then actually
-- signed in was recorded nowhere -- so a workspace that never once logged in looked identical on
-- the Tenants panel to one in daily use. FunnelSwift and WorkflowSwift answer this from
-- `users.last_login_at` -- this app's identity row is `accounts`, so the column belongs here.
--
-- Written by `auth_handler::login` on every successful sign-in (best effort -- a bookkeeping
-- failure must never fail a login) and published by `admin_handler::list_all_tenants`.
--
-- Idempotent: this app's runner re-executes every file in ./migrations on each boot and keeps only
-- a filename-keyed ledger, so a re-run must be safe. No semicolons appear in this comment.
ALTER TABLE accounts ADD COLUMN IF NOT EXISTS last_login_at TIMESTAMPTZ;

COMMENT ON COLUMN accounts.last_login_at IS 'Last successful sign-in (UTC). NULL means the account has never signed in.';
