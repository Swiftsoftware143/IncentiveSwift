-- The account's session VERSION (2026-11-01).
--
-- Why: session revocation in this app is timestamp-based (`accounts.sessions_valid_from`, floored
-- to a whole second so a fresh token can be minted in the same second as the change). That floor
-- leaves a one-second ambiguity: measured live on 2026-11-01, a SECOND device that signed in during
-- the same wall-clock second as `POST /api/v1/auth/logout-all` kept full access afterwards — the
-- owner pressed "sign out every other device" and the other device stayed signed in.
--
-- A version counter removes the ambiguity entirely: every session JWT carries the `sv` claim it was
-- minted with, and revoking a session bumps the row. A token whose `sv` no longer matches the row is
-- refused whatever second it was minted in. Tokens minted before this column existed carry no `sv`
-- and keep the timestamp rule, so no live session is dropped by the deploy.
--
-- Idempotent: the migration runner re-reads ./migrations on every boot.
ALTER TABLE accounts ADD COLUMN IF NOT EXISTS session_epoch integer NOT NULL DEFAULT 1;

COMMENT ON COLUMN accounts.session_epoch IS
  'Session version. A JWT carries it as the `sv` claim; logout-all and every password change bump it, which ends every session minted under an older value (2026-11-01).';
