-- Plain-statement column drift — IncentiveSwift (kanban t_cf7469bb).
--
-- `fleet-dbtype-audit.py` (t_cd486503) can now read the column lists of PLAIN statements
-- (`sqlx::query("INSERT/UPDATE/SELECT ...")`), and 25 sites in this app named a column that
-- does not exist. Each was verified twice: information_schema has no such column, and Postgres
-- itself rejects the generated statement (ERROR 42703) — so every one of them is a live 500 on
-- a wired route (see the card body for the route list).
--
-- This file carries the sites where the statement is RIGHT and the SCHEMA is missing the column
-- it needs. The sites with the opposite verdict (a statement naming a column that has a canonical
-- home already) were fixed in the code instead — see the commit message and
-- /opt/swift/audits/is-plaincol-t_cf7469bb/REPORT.md. Nothing here drops or rewrites anything:
-- every statement is additive and idempotent, because the runner executes one file as ONE batch
-- and a file that fails is retried on the next boot (src/db/migrations.rs).

-- business_pledges — POST /api/v1/business/pledge (loyalty_v2.rs:398). The request carries both
-- fields (min_purchase: Option<String>, valid_until: Option<DateTime<Utc>>) and the INSERT names
-- them; the table was created without them, so the route 500s on every call. Both are optional
-- by design (a pledge may have no minimum and no expiry), so no default and no backfill.
ALTER TABLE business_pledges ADD COLUMN IF NOT EXISTS min_purchase text;
ALTER TABLE business_pledges ADD COLUMN IF NOT EXISTS valid_until timestamptz;

-- accounts.updated_at — three writers already set it (loyalty_plans.rs:224 subscribe,
-- stripe_webhook.rs:59 handle_checkout_completed, external_grants.rs:120 find_or_create_account);
-- accounts was the only table in that family without the column, so plan subscribe, the Stripe
-- activation webhook and the referral grant path all 500'd. Every other mutable table in this
-- schema has it, so the column (not the three statements) is the defect.
-- NOT NULL DEFAULT now() backfills the 56 existing rows with their row-creation moment, which is
-- the only honest value available and is what a freshly created row would carry anyway.
ALTER TABLE accounts ADD COLUMN IF NOT EXISTS updated_at timestamptz NOT NULL DEFAULT now();

-- accounts.redemption_cap_pct / min_redemption_credits — the tenant-level ZaarCash redemption
-- guardrails read by POST purchase_verify (loyalty_v2.rs:1140). The call site already carries the
-- intended defaults in two places (`tenant_id.is_none()` -> (10, 100) and the fetch `.unwrap_or((10,
-- 100))` precisely because the columns were expected to be optional overrides), so the column
-- defaults ARE the behaviour the code documents: 10% cap, 100 ZaarCash minimum to redeem. Seeding
-- them NOT NULL means the read can never fall back to a different number than the code assumes,
-- and an operator can still widen or tighten either per account.
ALTER TABLE accounts
    ADD COLUMN IF NOT EXISTS redemption_cap_pct integer NOT NULL DEFAULT 10,
    ADD COLUMN IF NOT EXISTS min_redemption_credits integer NOT NULL DEFAULT 100;

-- stripe_checkout_sessions.webhook_raw — handle_checkout_completed (stripe_webhook.rs:50) stores
-- the Stripe event payload it just processed on the session row. The table is the checkout
-- session's own audit trail (status/completed_at are written by the same statement), so the raw
-- event belongs here; jsonb because the payload is stored and re-read as JSON, never as text.
ALTER TABLE stripe_checkout_sessions ADD COLUMN IF NOT EXISTS webhook_raw jsonb;

-- plans.thank_you_url — POST /api/v1/checkout/create (billing/checkout.rs:77) resolves the
-- post-payment landing page as `explicit success_url > plan's thank_you_url > /thank-you.html`.
-- `plans.purchase_url` is a different thing (the URL the plan is SOLD at, empty for every row
-- today) and reusing it would send a buyer to the storefront after paying, so this is a new
-- column, not an alias. Nullable: NULL means "use /thank-you.html", which is the documented
-- fallback and the behaviour of every existing row.
ALTER TABLE plans ADD COLUMN IF NOT EXISTS thank_you_url text;
