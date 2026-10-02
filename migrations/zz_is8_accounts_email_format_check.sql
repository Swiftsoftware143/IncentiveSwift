-- zz_is8_accounts_email_format_check
--
-- WHY (kanban t_d7ef4a88; class measured live under t_4722a331, reference shape proven on
-- missedcallrespondr under t_54b1ffab). `public.accounts.email` is an account's login identity AND
-- the only address its credentials/welcome mail can ever be delivered to. The column was
-- `text NOT NULL` with a UNIQUE index (`accounts_email_key`) and NO `CHECK`, and no writer on the
-- signup path validated the format — `POST /api/v1/auth/register` only tested `is_empty()`, so the
-- literal string `bad` (or `a@b`, or ` @x `) became a real account that no mail could ever reach.
--
-- The application boundary now refuses that input with a 4xx before any INSERT
-- (src/security/email_addr.rs::normalize, used by the five measured writers: auth_handler::register,
-- external_grants::{find_or_create_account,register_member}, business_handler::register_business,
-- billing::webhooks::deliver_credentials; the readers login/forgot-password match with lookup_key).
-- This constraint is the store-level backstop for writers nobody has written yet.
--
-- The pattern is deliberately LOOSER than the Rust validator so the database can never refuse a
-- value the application accepted: the application additionally rejects whitespace/control
-- characters, empty and dot-only local/domain parts, and over-long addresses. Everything this regex
-- requires — a non-empty part, exactly one `@`, a non-empty dotted domain — is required by the
-- application too. Plus-aliases (`a+b@x.com`), dotted locals (`a.b@x.com`) and IDN domains
-- (`user@münchen.de`) pass both.
--
-- IDEMPOTENT for the boot-time runner (src/db/migrations.rs applies each file once and records it in
-- `_migrations`, but a ledger-insert failure re-runs the file, and fromzero-baseline.sh replays the
-- whole directory on an empty database): `ADD CONSTRAINT` has no `IF NOT EXISTS`, so it is guarded
-- by a pg_constraint probe; the table is guarded by to_regclass so an empty database that has not
-- yet created `accounts` is a no-op rather than an error (a file that throws at boot is an outage).
-- Every live row was checked against the pattern before this constraint was added
-- (25 rows: 0 violations), and the migration was dry-run rolled back against live first.

DO $$
BEGIN
    IF to_regclass('public.accounts') IS NOT NULL
       AND NOT EXISTS (
            SELECT 1
            FROM pg_constraint
            WHERE conname = 'accounts_email_format_check'
              AND conrelid = 'public.accounts'::regclass
       )
    THEN
        ALTER TABLE public.accounts
            ADD CONSTRAINT accounts_email_format_check
            CHECK (email ~ '^[^[:space:]@]+@[^[:space:]@]+\.[^[:space:]@]+$');
    END IF;
END
$$;
