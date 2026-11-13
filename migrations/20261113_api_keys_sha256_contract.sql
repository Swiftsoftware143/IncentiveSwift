-- 20261113 — one active key per account, enforced by the database.
--
-- A customer must hold exactly ONE active key at a time: the create route refuses a second, and
-- regenerate retires the old key before minting the replacement. This partial unique index is the
-- database-level backstop that closes the check-then-write race those handlers cannot see, so a
-- second active key for one tenant is impossible whatever writer produced it. Inactive rows are
-- exempt on purpose — they stay for support and audit after a revoke.
CREATE UNIQUE INDEX IF NOT EXISTS api_keys_one_active_per_tenant
    ON api_keys (tenant_id) WHERE is_active;
