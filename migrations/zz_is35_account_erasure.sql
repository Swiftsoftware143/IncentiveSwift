-- Account erasure (2026-11-24, kanban t_07e3f130; fleet standard
-- /opt/swift/docs/account-closure-erasure-standard-2026-11-24.md).
--
-- Erasure is DESTRUCTIVE where closure is a state: the account row is deleted and every
-- FK to `accounts(id)` CASCADEs (campaigns, the loyalty journal, the credit ledger, …). Two
-- things must survive that cascade, so they live in their own tables keyed on an erasure row
-- rather than on `accounts`:
--
--   * `account_erasures` — the ONE audit row the standard keeps ("its audit trail except the
--     erasure entry itself"). `account_id` is a PLAIN uuid, deliberately NOT a foreign key, so
--     deleting the account cannot delete the record of the erasure. It names the actor (who
--     erased their own data) and when. No password, no token, no email, no name.
--
--   * `account_erasure_billing_docs` — the billing documents tax law requires us to keep
--     (invoices / payment receipts), retained DETACHED from the account: the money facts only
--     (provider, amount, currency, description, status, provider reference, when), never the
--     account id, email or workspace name.

CREATE TABLE IF NOT EXISTS public.account_erasures (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    account_id uuid NOT NULL,
    erased_at  timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS public.account_erasure_billing_docs (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    erasure_id  uuid NOT NULL REFERENCES public.account_erasures(id) ON DELETE CASCADE,
    provider    text,
    amount      numeric(14,2),
    currency    text,
    description text,
    status      text,
    reference   text,
    charged_at  timestamptz
);

CREATE INDEX IF NOT EXISTS idx_account_erasures_account
    ON public.account_erasures(account_id);
CREATE INDEX IF NOT EXISTS idx_account_erasure_billing_erasure
    ON public.account_erasure_billing_docs(erasure_id);
