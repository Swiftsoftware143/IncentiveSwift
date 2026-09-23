-- rotation_configs — the two columns the rotation-voucher path has always read.
--
-- Found 2026-09-23 while closing IS-3: with `purchase_verifications` created, the
-- PIN flow got past `generate_pin` and then died inside
-- `loyalty_v2::issue_rotation_voucher` with
--
--     column "max_vouchers_per_rotation" does not exist
--
-- Two call sites read columns this table never had:
--   loyalty_v2.rs:513/520  issue_rotation_voucher  -> max_vouchers_per_rotation
--                                                     (SELECT ... WHERE ... is_active = true)
--   loyalty_v2.rs:697/699  list_rotation_configs    -> is_active (returned as a bool)
--
-- `is_active` gets TRUE, which is the behaviour the callers already assume: every
-- config row that exists today was written by create_rotation_config, which sets
-- no flag, and issue_rotation_voucher's WHERE clause would otherwise select
-- nothing and silently return no voucher.
--
-- `max_vouchers_per_rotation` is only destructured today (the loop body does not
-- consume it), so the default is a cap the API can start honouring rather than a
-- behaviour change: 5, matching the `LIMIT 5` on the same lookup.

ALTER TABLE rotation_configs
    ADD COLUMN IF NOT EXISTS is_active BOOLEAN NOT NULL DEFAULT TRUE,
    ADD COLUMN IF NOT EXISTS max_vouchers_per_rotation INTEGER NOT NULL DEFAULT 5;
