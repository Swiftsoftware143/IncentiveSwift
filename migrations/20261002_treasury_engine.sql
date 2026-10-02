-- IncentiveSwift — THE TREASURY ENGINE: collect from businesses, guard the float, keep a ledger.
--
-- David, 2026-10-02: *"the main concern is the loyalty engine so there's always money in there so I also
-- have to be able to collect payment from the businesses for the loyalty program. And I'll connect
-- Stripe later on but that system needs to be built in ... so I can automatically collect payments from
-- the businesses and those rules need to be added into it so the businesses understand."*
--
-- WHAT ALREADY EXISTED (measured before writing this, see the queue):
--   * `point_treasury` — 1 row, tracking total_points_issued / total_points_redeemed /
--     total_revenue_collected / total_reimbursements_paid / outstanding_liability / minimum_float.
--     The counters ARE written on issue (`loyalty_badges.rs:694`) and on redemption (`:626`).
--   * `minimum_float` is READ for display and WRITTEN by the clearinghouse config screen — and
--     COMPARED AGAINST NOTHING. The "never runs out of money" rule was a number on a screen.
--   * Nothing anywhere COLLECTED money from a business: no charge, no invoice, no top-up. The
--     "revenue collected" counter only moved as bookkeeping when points were issued.
--   * `journal_entries` — id/tenant_id/entry_type/description/amount/created_at, 0 rows and 0 code
--     references: the money trail had a table and no writer.
--
-- The live treasury row reads issued=110 redeemed=200 collected=1.10 reimbursed=1.60 min_float=100.00
-- — redeemed already double issued and the float already underwater. That is the state this file exists
-- to make impossible to reach silently.

-- ── 1. MONEY IN: what a business has paid in. ─────────────────────────────────────────────────────
-- A row per payment RECEIVED. `method` is the provider seam David asked for: 'manual' today, 'stripe'
-- when he connects it, with no other change — the reference carries the provider's own id so a payment
-- can always be traced back to the system that took it.
CREATE TABLE IF NOT EXISTS treasury_funding (
    id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    business_id   uuid,
    business_name text NOT NULL,
    amount        numeric(14,2) NOT NULL CHECK (amount > 0),
    method        text NOT NULL DEFAULT 'manual',
    reference     text,
    status        text NOT NULL DEFAULT 'received',
    received_at   timestamptz NOT NULL DEFAULT now(),
    recorded_by   uuid,
    created_at    timestamptz NOT NULL DEFAULT now()
);
ALTER TABLE treasury_funding DROP CONSTRAINT IF EXISTS treasury_funding_method_check;
ALTER TABLE treasury_funding ADD CONSTRAINT treasury_funding_method_check
    CHECK (method IN ('manual','stripe','paypal','bank_transfer'));
ALTER TABLE treasury_funding DROP CONSTRAINT IF EXISTS treasury_funding_status_check;
ALTER TABLE treasury_funding ADD CONSTRAINT treasury_funding_status_check
    CHECK (status IN ('received','refunded'));
CREATE INDEX IF NOT EXISTS treasury_funding_business_idx ON treasury_funding (business_id, received_at DESC);

-- ── 2. THE LEDGER gets what a money trail needs. ──────────────────────────────────────────────────
-- `journal_entries` existed with no writer and no notion of direction, so "what moved and why" could not
-- be answered from it. It also required a tenant_id, which is wrong for a PLATFORM treasury movement:
-- the treasury belongs to the platform, not to any one tenant. A row that has to lie about its tenant to
-- be written is a row nobody can trust.
ALTER TABLE journal_entries ALTER COLUMN tenant_id DROP NOT NULL;
ALTER TABLE journal_entries ADD COLUMN IF NOT EXISTS direction text;
ALTER TABLE journal_entries DROP CONSTRAINT IF EXISTS journal_entries_direction_check;
ALTER TABLE journal_entries ADD CONSTRAINT journal_entries_direction_check
    CHECK (direction IS NULL OR direction IN ('credit','debit'));
ALTER TABLE journal_entries ADD COLUMN IF NOT EXISTS account text;
ALTER TABLE journal_entries ADD COLUMN IF NOT EXISTS reference_id uuid;
ALTER TABLE journal_entries ADD COLUMN IF NOT EXISTS reference_type text;
ALTER TABLE journal_entries ADD COLUMN IF NOT EXISTS balance_after numeric(14,2);
CREATE INDEX IF NOT EXISTS journal_entries_created_idx ON journal_entries (created_at DESC);

-- ── 3. THE RULE becomes explicit and enforceable. ─────────────────────────────────────────────────
-- `on_float_breach` is the ONE decision that is David's, not mine, because it decides who carries the
-- shortfall. It is a column rather than a constant so he can change it without a deploy, and it is
-- CHECK-constrained so an unknown behaviour can never be configured and then silently ignored.
--   hold           — the redemption is parked for review and the business is told to top up (default)
--   allow_and_bill — the redemption completes and the shortfall is recorded against the business
--   suspend        — the business's loyalty program stops redeeming until it tops up
ALTER TABLE point_treasury ADD COLUMN IF NOT EXISTS on_float_breach text NOT NULL DEFAULT 'hold';
ALTER TABLE point_treasury DROP CONSTRAINT IF EXISTS point_treasury_on_float_breach_check;
ALTER TABLE point_treasury ADD CONSTRAINT point_treasury_on_float_breach_check
    CHECK (on_float_breach IN ('hold','allow_and_bill','suspend'));
ALTER TABLE point_treasury ADD COLUMN IF NOT EXISTS float_breaches integer NOT NULL DEFAULT 0;
ALTER TABLE point_treasury ADD COLUMN IF NOT EXISTS last_breach_at timestamptz;

-- ── 4. A HELD REDEMPTION needs somewhere to wait. ─────────────────────────────────────────────────
-- Under the default behaviour a redemption that would breach the float must not simply fail — the
-- customer is standing at the counter. It is recorded here, the customer is told it is being confirmed,
-- and the business is asked to top up. Nothing is silently refused and nothing is paid from money that
-- is not there.
CREATE TABLE IF NOT EXISTS treasury_holds (
    id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    campaign_id   uuid,
    contact_id    uuid,
    business_id   uuid,
    business_name text,
    points        bigint NOT NULL DEFAULT 0,
    amount        numeric(14,2) NOT NULL DEFAULT 0,
    shortfall     numeric(14,2) NOT NULL DEFAULT 0,
    status        text NOT NULL DEFAULT 'pending',
    reason        text,
    resolved_by   uuid,
    resolved_at   timestamptz,
    created_at    timestamptz NOT NULL DEFAULT now()
);
ALTER TABLE treasury_holds DROP CONSTRAINT IF EXISTS treasury_holds_status_check;
ALTER TABLE treasury_holds ADD CONSTRAINT treasury_holds_status_check
    CHECK (status IN ('pending','approved','declined','expired'));
CREATE INDEX IF NOT EXISTS treasury_holds_pending_idx ON treasury_holds (status, created_at DESC);
