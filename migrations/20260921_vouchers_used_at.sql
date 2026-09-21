-- t_9da8d538: claim_voucher's redemption UPDATE (src/handlers/loyalty_v2.rs:352) sets
-- `used_at`, but the column was never created, so POST /api/v1/loyalty/claim-voucher 500'd
-- on every call:
--   column "used_at" of relation "vouchers" does not exist
-- (proved live 2026-09-21 while verifying the discount_value fix — the read side of the same
-- handler was the card's defect; this is the write side of that handler, same vouchers family.)
-- The column records when the voucher was redeemed, which is what the handler intends.
ALTER TABLE vouchers ADD COLUMN IF NOT EXISTS used_at timestamptz;

COMMENT ON COLUMN vouchers.used_at IS 'Set by claim_voucher when status flips to used (never created until t_9da8d538).';
