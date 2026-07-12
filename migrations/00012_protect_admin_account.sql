-- Migration 00012: Protect admin account from being overwritten on redeploy
-- This ensures the admin seed migration (if any) never clobbers an existing password

INSERT INTO accounts (id, email, name, password_hash, role, created_at)
VALUES (
    gen_random_uuid(),
    'swiftsoftware143@yahoo.com',
    'Super Admin',
    '\$argon2id\$v=19\$m=19456,t=2,p=1\$MJ/gLMi0OYRylLishtPV4g\$M1VuTUTraRfMnqO3eIZKex6IxX+8zkVwt2cP+pTZVGk',
    'admin',
    now()
)
ON CONFLICT (email) DO NOTHING;

INSERT INTO accounts (id, email, name, password_hash, role, created_at)
VALUES (
    gen_random_uuid(),
    'admin@swiftsoftware.com',
    'Admin',
    '\$argon2id\$v=19\$m=19456,t=2,p=1\$MJ/gLMi0OYRylLishtPV4g\$M1VuTUTraRfMnqO3eIZKex6IxX+8zkVwt2cP+pTZVGk',
    'admin',
    now()
)
ON CONFLICT (email) DO NOTHING;
