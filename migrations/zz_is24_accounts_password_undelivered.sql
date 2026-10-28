-- The latch behind the "choose your own password" door (2026-10-28 cron).
--
-- Signup mints the first password and MAILS it; the mail is the customer's only way in. When that
-- mail cannot be delivered (the provider refuses, or the address is one the sender withholds) the
-- account exists with a password nobody has ever seen, and until now the signup page still said
-- "we have emailed your password" — a lie — leaving the customer with an account they could not
-- enter and no self-service way to fix it.
--
-- `password_undelivered` is that exact state and nothing else:
--   * SET by `register` when it minted the password and the send did not go out;
--   * CLEARED by `POST /api/v1/auth/initial-password` (which writes the password the customer
--     chooses, in the same statement that clears it, so the door is one-shot) and by a successful
--     password login (if they got in, they know their password and the window is moot).
--
-- Idempotent: this app's runner re-reads ./migrations on every boot (filename-keyed ledger).
ALTER TABLE accounts ADD COLUMN IF NOT EXISTS password_undelivered boolean NOT NULL DEFAULT false;

COMMENT ON COLUMN accounts.password_undelivered IS
    'true only when signup minted a first password and its credentials mail did not reach the customer; cleared by the first initial-password set or a successful login';
