-- IS-7 note: this filename sorts BEFORE 00001_full_schema.sql, which is the file that
-- creates `accounts` -- so the inline `REFERENCES accounts(id)` this file used to carry
-- could never be created here: it aborted the whole file, and on a fresh database it
-- aborted the boot (the runner exits non-zero when a file fails). In production the old
-- runner swallowed that error and still recorded the filename, which is why the table
-- exists there now.
--
-- The column stays here; the foreign key is added by
-- `zz_is7_baseline_foreign_keys.sql`, which runs after every other file so `accounts`
-- exists by then. Every statement is IF NOT EXISTS, so re-running is a no-op.
CREATE TABLE IF NOT EXISTS password_resets (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    account_id UUID NOT NULL,
    token TEXT NOT NULL UNIQUE,
    expires_at TIMESTAMPTZ NOT NULL,
    used BOOLEAN NOT NULL DEFAULT false,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
