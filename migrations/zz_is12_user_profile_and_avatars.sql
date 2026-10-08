-- zz_is12_user_profile_and_avatars.sql
-- David 2026-10-08 (kanban t_4fcbe895): the account console has no profile screen at all, so a
-- customer cannot set their own name/company or change their password, and there is no profile
-- picture anywhere in this app (`grep -rn avatar src/` was 0 hits before this card). Two pieces of
-- storage were missing; the routes that read them are `handlers::auth_handler`'s.
--
--  1. `accounts.company` (and `accounts.username`). Signup collects NAME + EMAIL only, so nothing in
--     the product has ever written a company. The profile screen asks for one, so the column has to
--     exist. `username` is the second optional field of the same request (the fleet's
--     `PUT /api/v1/auth/profile` contract): the console's Profile screen does not collect it, but the
--     field is accepted and stored rather than silently dropped.
--
--  2. `user_avatars` — the picture's BYTES. This container binds only its release binary and
--     `migrations/` (`docker inspect incentiveswift`), so a file written at run time lives inside the
--     container and dies with the next recreate, and no host webroot could serve it either. The bytes
--     are kept in the database and streamed back by `GET /api/v1/auth/avatar/:user_id`, which is what
--     makes the picture survive a deploy. Same storage decision as `tenant_logos` (zz_is11).
--
-- One row per account: the picture is the ACCOUNT holder's, and the upload path upserts it. Deleting
-- the account must take the picture with it, which the FK's ON DELETE CASCADE covers (the parent here
-- is `accounts`, exactly as `tenant_logos.tenant_id` is). The primary key is the only lookup path, so
-- no extra index is needed.
ALTER TABLE accounts ADD COLUMN IF NOT EXISTS company varchar(255);
ALTER TABLE accounts ADD COLUMN IF NOT EXISTS username varchar(255);

CREATE TABLE IF NOT EXISTS user_avatars (
    user_id      uuid         PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
    content_type varchar(100) NOT NULL,
    bytes        bytea        NOT NULL,
    updated_at   timestamptz  NOT NULL DEFAULT NOW()
);
