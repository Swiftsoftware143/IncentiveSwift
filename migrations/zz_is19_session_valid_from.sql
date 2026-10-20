-- A password change or reset must end the sessions that predate it.
--
-- Measured 2026-10-20 (cron `default`): on a live probe account, a JWT minted BEFORE a password
-- reset still answered `200` from `GET /api/v1/auth/me` after the reset, and a JWT minted before a
-- password CHANGE survived the change the same way. A token lives 30 days, so the one action a user
-- takes when they believe their session is compromised — changing the password — revoked nothing,
-- and the attacker's session outlived it.
--
-- NULL means "never restricted": every existing account and every token already in the wild keeps
-- working exactly as before, so this is additive. The column only takes a value when a password is
-- changed or reset, and `security::auth` refuses any JWT whose `iat` is older than it.
ALTER TABLE accounts ADD COLUMN IF NOT EXISTS sessions_valid_from timestamptz;

COMMENT ON COLUMN accounts.sessions_valid_from IS
    'JWTs issued before this instant are refused (revocation on password change/reset); NULL = never restricted.';
