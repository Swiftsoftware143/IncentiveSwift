-- zz_is13_iqs_assets.sql
-- kanban: cron `default` 2026-10-15 — `POST /api/v1/iqs/upload` answered 500 on EVERY call.
--
-- MEASURED BEFORE THIS FILE (2026-10-15 10:38 UTC, live on app.incentiveswift.com's origin
-- 127.0.0.1:8083, a real session minted through the public register route):
--
--   POST /api/v1/iqs/upload -> 500 {"code":500,"error":"Internal server error"}
--   docker logs incentiveswift:
--     ERROR incentiveswift_api::error: Internal error: Failed to save file:
--       No such file or directory (os error 2)
--
-- The handler wrote the upload with `std::fs::File::create("/var/www/incentiveswift-app/uploads/...")`.
-- `File::create` does not create parent directories, and this container has no `/var/www` at all
-- (`docker exec incentiveswift ls /var/www` -> "No such file or directory") — so the very first
-- multipart part failed and the endpoint was dead for every IQS file-type question. Even had it
-- written, the URL it returned (`https://app.incentiveswift.com/uploads/<name>`) is not served by
-- anything: that host's `/uploads/` answers the SPA shell (14181 B, byte-identical to `/`), and the
-- app root has no `uploads` directory.
--
-- STORAGE DECISION (the fleet's own, `zz_is12` for `user_avatars` and `zz_is11` for `tenant_logos`):
-- this container binds only its release binary and `migrations/`, so a file written at run time lives
-- inside the container and dies with the next recreate, and no host webroot could serve it either.
-- The bytes therefore live HERE and are streamed back by the new
-- `GET /api/v1/iqs/assets/:id` (`handlers::iqs_handler::get_asset`).
--
-- Unlike the avatar/logo tables this one is NOT images-only: an IQS file-type question accepts
-- arbitrary documents (`iqs_validation::validate_file`'s config carries `allowed_extensions` such as
-- `.pdf`, max 10 MB), so the column set stores the caller's declared type and the route decides at
-- SERVE time whether that type may be rendered inline (a safelist) or must be an opaque download —
-- which is what keeps an uploaded document from becoming stored HTML on the app's own origin.
--
-- Scoped to the uploading ACCOUNT (`accounts.id`, ON DELETE CASCADE) so an account's removal takes
-- its assets with it. The row id is the only lookup path (an unguessable uuid, exactly like
-- `user_avatars.user_id`), so no extra index is needed. IF NOT EXISTS everywhere: this app's runner
-- re-reads the directory on every boot and re-applies unrecorded files only, but a partially applied
-- file must still be safe to re-run.
CREATE TABLE IF NOT EXISTS iqs_assets (
    id            uuid         PRIMARY KEY,
    account_id    uuid         REFERENCES accounts(id) ON DELETE CASCADE,
    original_name varchar(255) NOT NULL,
    content_type  varchar(120) NOT NULL,
    size_bytes    integer      NOT NULL,
    bytes         bytea        NOT NULL,
    created_at    timestamptz  NOT NULL DEFAULT NOW()
);

-- Listing/cleanup by owner (the read route is by primary key, so this is only for housekeeping and
-- for an account's own "my uploads" view if one is ever added).
CREATE INDEX IF NOT EXISTS iqs_assets_account_id_created_at_idx
    ON iqs_assets (account_id, created_at DESC);
