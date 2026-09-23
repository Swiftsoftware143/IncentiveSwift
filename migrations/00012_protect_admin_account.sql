-- Migration 00012: Protect admin account from being overwritten on redeploy
-- This ensures the admin seed migration (if any) never clobbers an existing password
--
-- IS-7 note: this file sorts BEFORE the base columns of `accounts` are restored by
-- `zz_is7_baseline_columns.sql` (00001_full_schema.sql creates accounts as a 5-column
-- skeleton; `password_hash` and `role` exist only in the live database). Rather than
-- aborting a fresh build on `column "password_hash" does not exist`, the seed is skipped
-- when the columns it writes are absent -- on an empty database there is no admin row to
-- protect anyway. Production already has both accounts and never re-runs this file
-- (it is recorded in _migrations), so nothing about the live database changes.
DO $is7$
BEGIN
    IF EXISTS (
        SELECT 1 FROM information_schema.columns
        WHERE table_schema = 'public' AND table_name = 'accounts' AND column_name = 'password_hash'
    ) AND EXISTS (
        SELECT 1 FROM information_schema.columns
        WHERE table_schema = 'public' AND table_name = 'accounts' AND column_name = 'role'
    ) THEN
        INSERT INTO accounts (id, email, name, password_hash, role, created_at)
        VALUES (
            gen_random_uuid(),
            'swiftsoftware143@yahoo.com',
            'Super Admin',
            '$argon2id$v=19$m=19456,t=2,p=1$MJ/gLMi0OYRylLishtPV4g$M1VuTUTraRfMnqO3eIZKex6IxX+8zkVwt2cP+pTZVGk',
            'admin',
            now()
        )
        ON CONFLICT (email) DO NOTHING;

        INSERT INTO accounts (id, email, name, password_hash, role, created_at)
        VALUES (
            gen_random_uuid(),
            'admin@swiftsoftware.com',
            'Admin',
            '$argon2id$v=19$m=19456,t=2,p=1$MJ/gLMi0OYRylLishtPV4g$M1VuTUTraRfMnqO3eIZKex6IxX+8zkVwt2cP+pTZVGk',
            'admin',
            now()
        )
        ON CONFLICT (email) DO NOTHING;
    ELSE
        RAISE NOTICE '00012 skipped: accounts.password_hash/role not present (fresh schema before the baseline columns run)';
    END IF;
END
$is7$;
