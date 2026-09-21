-- Restore the password_resets table that 000001_password_resets.sql never created.
--
-- That file sorts FIRST in ./migrations and declares account_id uuid NOT NULL REFERENCES
-- accounts(id). On the database's first migration run the accounts table did not exist yet,
-- so CREATE TABLE failed -- and the pre-fix runner swallowed the failure and still recorded
-- the filename in _migrations, so it was never retried. Live consequence: forgot-password
-- answered HTTP 500 with `relation "password_resets" does not exist`
-- (src/handlers/auth_handler.rs:758 INSERT, :795 SELECT, :825 UPDATE used = true) and nobody
-- could reset a password at all. Card t_47c00655. The runner fix in t_1c8e06d1 makes future
-- failures loud and retryable but does NOT repair an already-recorded row, so the repair is a
-- NEW filename -- a recorded filename is never retried.
--
-- The DDL is byte-for-byte the intent of 000001_password_resets.sql (same columns, same
-- types, same defaults, same plain REFERENCES accounts(id) with no ON DELETE clause): this
-- file repairs the ledger, it does not redesign the table. `accounts` exists now, so the
-- statement applies cleanly. Every statement is IF NOT EXISTS, so re-running is a no-op.

CREATE TABLE IF NOT EXISTS public.password_resets (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    account_id UUID NOT NULL REFERENCES public.accounts(id),
    token TEXT NOT NULL UNIQUE,
    expires_at TIMESTAMPTZ NOT NULL,
    used BOOLEAN NOT NULL DEFAULT false,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- The reset-token lookup filters on token, which is already UNIQUE. This index serves the
-- account-scoped side (housekeeping / auditing of a single account's reset tokens).
CREATE INDEX IF NOT EXISTS password_resets_account_id_idx
    ON public.password_resets (account_id);
