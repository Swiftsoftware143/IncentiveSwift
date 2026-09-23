-- purchase_verifications — the PIN purchase-verification ledger.
--
-- `src/handlers/loyalty_v2.rs` has written to this table since the loyalty-v2
-- work (generate_pin INSERTs a `pending` row, verify_purchase flips it to
-- `verified` and attaches the contact, issue_rotation_voucher stamps the
-- voucher_id), but no migration ever created it. On a database built from these
-- files the whole flow therefore answered HTTP 500 with "Internal server
-- error": measured live 2026-09-23, `POST /api/v1/loyalty/generate-pin` ->
-- 500 while `pg_tables` held no `purchase_verifications`.
--
-- Columns are exactly the ones the handlers bind:
--   generate_pin      -> id, campaign_id, business_id, business_name,
--                        verification_type, pin_code, purchase_amount, status
--   verify_purchase   -> pin_code, status, purchase_amount, contact_id, verified_at
--   issue_rotation_voucher -> voucher_id, issued_to_contact_id, created_at
--
-- campaign_id is intentionally NULL-able with ON DELETE SET NULL: a printed
-- receipt PIN must not be destroyed by a campaign being deleted mid-flow.
-- The FKs to contacts are SET NULL so that removing a contact cannot abort a
-- verification that is already recorded.

CREATE TABLE IF NOT EXISTS purchase_verifications (
    id                   UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    campaign_id          UUID REFERENCES campaigns(id) ON DELETE SET NULL,
    business_id          UUID,
    business_name        TEXT NOT NULL DEFAULT '',
    verification_type    TEXT NOT NULL DEFAULT 'pin',
    pin_code             TEXT,
    purchase_amount      NUMERIC(12, 2),
    status               TEXT NOT NULL DEFAULT 'pending',
    contact_id           UUID REFERENCES contacts(id) ON DELETE SET NULL,
    issued_to_contact_id UUID,
    voucher_id           UUID,
    verified_at          TIMESTAMPTZ,
    expires_at           TIMESTAMPTZ,
    created_at           TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- verify_purchase looks a PIN up by value among the pending rows.
CREATE INDEX IF NOT EXISTS idx_purchase_verifications_pending_pin
    ON purchase_verifications (pin_code) WHERE status = 'pending';

-- issue_rotation_voucher picks the newest row for a contact.
CREATE INDEX IF NOT EXISTS idx_purchase_verifications_contact_created
    ON purchase_verifications (contact_id, created_at DESC);
