-- t_9da8d538: vouchers.discount_value was declared `numeric`, but every writer in this
-- app stores a human-readable offer string:
--   * src/handlers/loyalty_v2.rs:1697 (survey path) binds the SQL literal '$50.00'
--   * src/handlers/loyalty_v2.rs:576 (rotation path) binds business_pledges.offer_value,
--     which is free text ("10% Off", "free dessert")
--   * src/handlers/loyalty_v2.rs:242 (issue-voucher) binds the request's discount_value
--   * docs/admin-guide.md documents the outbound `voucher_issued` payload as
--     "discount_value": "10% Off"
--
-- Live consequence (proved before this migration, 2026-09-21):
--   POST /api/v1/loyalty/issue-voucher -> 500, log:
--     column "discount_value" is of type numeric but expression is of type text
--   GET  /api/v1/loyalty/my-vouchers/:contact_id -> 500 (numeric decoded into String)
-- So no voucher could be created through the API at all, and none could be read.
--
-- The column type is the defect, not the Rust side: `numeric` cannot hold the app's own
-- values. numeric -> text makes the three read sites (String) correct as written and both
-- INSERT paths work with the values the product actually stores.
-- vouchers is empty (0 rows) at the time of this migration, so the rewrite is a no-op data-wise.
ALTER TABLE vouchers
    ALTER COLUMN discount_value TYPE text USING discount_value::text;

COMMENT ON COLUMN vouchers.discount_value IS
    'Human-readable offer value, e.g. ''10% Off'' / ''$50.00'' (text since t_9da8d538; was numeric, which no writer could satisfy).';
