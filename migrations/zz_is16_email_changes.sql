-- zz_is16_email_changes.sql
-- David 2026-10-18 (kanban t_6b2702e3): the profile screen shows the sign-in address read-only.
-- Two of the fleet's apps (FunnelSwift, WorkflowSwift) already let a customer move it; this app had
-- no such path at all (`grep -rn 'email_change' src/` was 0 hits). This table is the PENDING half of
-- the two-arm flow: `POST /api/v1/auth/email-change` writes one row and mails a confirmation link to
-- the NEW address; `POST /api/v1/auth/email-change/confirm` redeems the token and moves
-- `accounts.email`.
--
-- Why a table rather than a token on the account: a move must be reversible by simply NOT opening
-- the link, and the old row must keep proving what was asked for after a second request replaces it.
-- Uniqueness + expiry are enforced here so a redeemed or expired link can never be replayed, and
-- `used` is set by BOTH the redeem and the retire-on-new-request paths (one live request per account).
--
-- Idempotent on purpose: this app's boot runner (`src/db/migrations.rs`) re-reads ./migrations on
-- every boot and applies the files it has not recorded, so every statement here must be safe to run
-- against a database that already has the object.
CREATE TABLE IF NOT EXISTS email_changes (
    id          uuid DEFAULT gen_random_uuid() NOT NULL PRIMARY KEY,
    account_id  uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    new_email   text NOT NULL,
    token       text NOT NULL UNIQUE,
    expires_at  timestamptz NOT NULL,
    used        boolean DEFAULT false NOT NULL,
    created_at  timestamptz DEFAULT now() NOT NULL
);

-- The confirm arm looks a token up by value (covered by the UNIQUE above); the request arm retires
-- the caller's outstanding rows, so it wants this partial index rather than a full-table scan on
-- every profile save.
CREATE INDEX IF NOT EXISTS email_changes_open_by_account
    ON email_changes (account_id) WHERE used = false;

COMMENT ON TABLE email_changes IS
    'Pending sign-in-address changes (kanban t_6b2702e3). One live row per account; redeemed or superseded rows keep used=true.';
