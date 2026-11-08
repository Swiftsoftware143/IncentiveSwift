-- A loyalty member's referral code is the handle every online-loyalty endpoint resolves against
-- (`loyalty/online/share`, `/referral-click`, `/stats/:code` all do `WHERE referral_code = $1`).
-- Until now NOTHING guaranteed it identified one row: `loyalty_members` had no index on the column
-- at all, and the member created by a public check-in was inserted with no code whatsoever
-- (`mechanics/loyalty_checkin.rs::get_or_create_member`), so every member who joined by scanning the
-- counter QR was invisible to the referral engine.
--
-- A partial unique index is the honest guarantee rather than a SELECT-then-INSERT check in Rust: the
-- check has a race between two concurrent first check-ins, the index does not. Partial because the
-- column stays nullable — rows minted before this index keep NULL and are simply not referral
-- handles (`WHERE referral_code IS NOT NULL`), so the index can never reject a pre-existing row.
--
-- Idempotent: the boot runner re-executes unrecorded files, and IF NOT EXISTS makes a second run a
-- no-op. Verified against live before the deploy (0 rows in `loyalty_members`, so nothing to clash).
CREATE UNIQUE INDEX IF NOT EXISTS loyalty_members_referral_code_uniq
    ON loyalty_members (referral_code)
    WHERE referral_code IS NOT NULL;
