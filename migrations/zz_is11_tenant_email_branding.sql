-- zz_is11_tenant_email_branding.sql
-- David 2026-10-08 (kanban t_feab8aff): every transactional email this app sends should carry the
-- ACCOUNT's own branding — a logo and a brand display name — settable by the account in its own
-- console, so the mail a business's users receive looks like that business's mail.
--
-- Two pieces of storage, and only ONE of them is new:
--
--  1. `tenant_settings` key `email_branding` = {"brand_name", "brand_color", "logo_url"}. No DDL:
--     `tenant_settings` is the account's own key/value store (UNIQUE (tenant_id, key), read and
--     written by GET/PUT /api/v1/settings — the same route the console's Settings tab already uses),
--     and a jsonb document is exactly the shape its other values have. An account with no row is
--     simply unbranded and gets byte-identical mail to before this feature.
--
--  2. `tenant_logos` — the logo's BYTES. The container binds no path for a run-time upload, so a file
--     written at run time lives inside the container and dies on the next recreate, and no host
--     webroot serves it. The bytes are kept in the database and streamed back by
--     `GET /api/v1/branding/logo/:tenant_id`.
--
-- One row per tenant: the logo is the ACCOUNT's, not per-user and not per-app, and the upload path
-- upserts it. Deleting the account must take the logo with it, which the FK's ON DELETE CASCADE
-- covers (the parent here is `accounts`, exactly as `tenant_settings.tenant_id` is). The primary key
-- is the only lookup path, so no extra index is needed.
CREATE TABLE IF NOT EXISTS tenant_logos (
    tenant_id    uuid         PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
    content_type varchar(100) NOT NULL,
    bytes        bytea        NOT NULL,
    updated_at   timestamptz  NOT NULL DEFAULT NOW()
);
