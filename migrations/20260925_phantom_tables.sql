-- Phantom tables (42P01) — IncentiveSwift (kanban t_e2cecfcb).
--
-- Same failure shape as t_cf7469bb, one level up: the statement names a TABLE that no migration
-- creates, so Postgres answers `42P01 relation "..." does not exist` and the wired route dies.
-- `fleet-dbtype-audit.py` cannot see these by design (its plain-statement pass only judges
-- statements whose target table exists in its inventory).
--
-- Class census this run (/opt/swift/audits/is-table-exists-t_e2cecfcb/table-census.py): every table
-- name appearing after FROM/JOIN/INTO/UPDATE in every string literal under src/, checked with
-- to_regclass(). 96 names referenced, 4 of them real and missing — the card named 2
-- (checkout_sessions, feature_limits), the census found 2 more (payment_providers, chat_sessions).
-- feature_limits is fixed in the CODE (the canonical model is tier_features) — see
-- src/handlers/surface_handler.rs. The other three get the table the statements already describe.
--
-- Verdict for all three: CREATE (never repoint). Each name is used by nothing but the statements in
-- this app, each statement's column list is the contract, and in two of the three cases the same
-- file also writes the sibling table that already exists — so pointing them elsewhere would have
-- merged two different ledgers (see each block below). Nothing here drops, rewrites or backfills.

-- ---------------------------------------------------------------------------
-- 1. checkout_sessions — POST /api/v1/checkout/create (checkout.rs:94) and
--    GET /api/v1/checkout/sessions (checkout.rs:151); also updated by the Stripe and PayPal
--    webhook handlers (webhooks.rs:304, webhooks.rs:383).
--
--    NOT repointed at `stripe_checkout_sessions`: that is the Stripe CREDIT ledger
--    (stripe_session_id NOT NULL, amount, credits NOT NULL) written by credits_handler.rs and
--    loyalty_plans.rs, and ONE handler reads BOTH — webhooks.rs updates checkout_sessions and then
--    reads stripe_checkout_sessions in the same function body. The create handler has no Stripe
--    session and no credits (it mints a placeholder URL), so pointing it at the ledger would have
--    to invent `credits` and a `stripe_session_id` for every checkout. `checkout_sessions` is also
--    the name all six call sites already use, so no third name is introduced.
--
--    Column types are the shapes the call sites decode: price_amount is read with
--    `row.get::<rust_decimal::Decimal, _>` (NUMERIC — a float8 would fail the decode), created_at/
--    updated_at with `DateTime<Utc>`, metadata with `serde_json::Value` (JSONB), and
--    price_currency/status/payment_provider as `String` (so NOT NULL). user_id is NOT NULL because
--    the handler always binds a UUID, but it carries NO foreign key: this app maps 1 account : 1
--    user and the handler deliberately binds a placeholder `Uuid::new_v4()` (checkout.rs:47), so an
--    FK to a users table would reject every insert.
CREATE TABLE IF NOT EXISTS public.checkout_sessions (
    id                  uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    account_id          uuid NOT NULL REFERENCES public.accounts(id) ON DELETE CASCADE,
    user_id             uuid NOT NULL,
    price_amount        numeric(12,2) NOT NULL,
    price_currency      text NOT NULL,
    description         text,
    success_url         text,
    cancel_url          text,
    metadata            jsonb,
    status              text NOT NULL DEFAULT 'pending',
    payment_provider    text NOT NULL DEFAULT 'stripe',
    payment_id          text,
    provider_session_id text,
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_checkout_sessions_account
    ON public.checkout_sessions (account_id);
CREATE INDEX IF NOT EXISTS idx_checkout_sessions_provider_session
    ON public.checkout_sessions (provider_session_id);

-- ---------------------------------------------------------------------------
-- 2. payment_providers — GET/POST/DELETE /api/v1/payment-providers and the inbound-webhook
--    signature secret lookup (billing/providers.rs:66,112,180,205). Live HTTP 500 with a token.
--
--    NOT repointed at `provider_keys`: that table is the BYOK key store (provider, api_key with a
--    CHECK (api_key = '' OR api_key LIKE 'enc:v1:%'), scope, no webhook_secret), while this one is
--    the payment-provider config the billing module documents, and the handler binds a PLAINTEXT
--    api_key — which the provider_keys CHECK would reject outright. They are different stores with
--    different write contracts.
--
--    Shapes from the call sites: api_key / webhook_secret are read as `String` (NOT NULL; the
--    upsert binds "" when the field is omitted, which the ON CONFLICT CASE guards against
--    overwriting an existing secret), is_active as `bool`, base_url/metadata optional, and
--    created_at/updated_at as `DateTime<Utc>`. The ON CONFLICT (account_id, provider_type) clause
--    requires the matching unique index — without it the upsert is a 42702/42P10, not a 500 on a
--    missing table, so it is part of the contract.
CREATE TABLE IF NOT EXISTS public.payment_providers (
    id             uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    account_id     uuid NOT NULL REFERENCES public.accounts(id) ON DELETE CASCADE,
    provider_type  text NOT NULL,
    api_key        text NOT NULL DEFAULT '',
    webhook_secret text NOT NULL DEFAULT '',
    base_url       text,
    metadata       jsonb,
    is_active      boolean NOT NULL DEFAULT true,
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX IF NOT EXISTS payment_providers_account_provider_type_key
    ON public.payment_providers (account_id, provider_type);
CREATE INDEX IF NOT EXISTS idx_payment_providers_active_secret
    ON public.payment_providers (provider_type) WHERE is_active = true;

-- ---------------------------------------------------------------------------
-- 3. chat_sessions — the chat-funnel state machine behind POST /api/v1/channels/inbound
--    (the public Telnyx webhook), sms_handler.rs:91 (resume lookup), :155 (create),
--    :250 (end), :357 (advance step), :670 (complete with entry_id).
--
--    This one does not 500: the resume lookup's `Err` is caught by a `match … _ =>` arm, so a
--    missing table was silently read as "no active session" — every inbound message started a
--    fresh session and the insert at :155 is `.ok()`-swallowed. The funnel is dead, quietly.
--
--    Types from the decode/bind sites: the resume lookup is
--    `query_as::<_, (Uuid, String, String, i32, Option<Value>, Option<Uuid>)>` = id, phone,
--    campaign_slug, step (non-Option i32 -> NOT NULL), collected_data (nullable jsonb),
--    campaign_id (nullable); account_id comes from the matched campaign (NOT NULL there), and
--    entry_id is set only on completion. `status` is only ever written as a literal
--    ('active'/'ended'/'completed').
CREATE TABLE IF NOT EXISTS public.chat_sessions (
    id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    phone         text NOT NULL,
    campaign_slug text NOT NULL,
    campaign_id   uuid,
    account_id    uuid NOT NULL REFERENCES public.accounts(id) ON DELETE CASCADE,
    step          integer NOT NULL DEFAULT 0,
    collected_data jsonb,
    status        text NOT NULL DEFAULT 'active',
    entry_id      uuid,
    created_at    timestamptz NOT NULL DEFAULT now(),
    updated_at    timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_chat_sessions_phone_active
    ON public.chat_sessions (phone, status);
CREATE INDEX IF NOT EXISTS idx_chat_sessions_campaign_phone
    ON public.chat_sessions (campaign_id, phone, status);
