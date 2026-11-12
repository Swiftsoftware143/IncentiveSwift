-- 026_workspace_on_tenant_insert.sql
--
-- WHY THIS EXISTS. 025 backfilled a default workspace for every account that already existed, but
-- nothing mints one for an account created AFTERWARDS — so a brand-new signup would open the
-- Workspaces screen on an empty list and (worse) have no default workspace to switch back to.
-- CoreSwift solves this with a trigger (its `133_first_workspace_on_account_mint` /
-- `134_default_workspace_on_tenant_insert`); this is the same rule for the same reason: the
-- invariant "every business account has exactly one default workspace" belongs to the DATABASE,
-- not to whichever handler happens to run next.
--
-- Additive and idempotent: the function is replaced, the trigger is dropped-if-exists and recreated,
-- and the backfill repeats harmlessly.

CREATE OR REPLACE FUNCTION mint_default_workspace() RETURNS TRIGGER AS $$
BEGIN
    INSERT INTO workspaces (tenant_id, name, is_default)
    VALUES (NEW.id, 'My Workspace', true)
    ON CONFLICT DO NOTHING;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS trg_accounts_mint_workspace ON accounts;
CREATE TRIGGER trg_accounts_mint_workspace
    AFTER INSERT ON accounts
    FOR EACH ROW EXECUTE FUNCTION mint_default_workspace();

-- Any tenant that slipped through before the trigger existed (a signup between 025 and this file).
INSERT INTO workspaces (tenant_id, name, is_default)
SELECT t.id, 'My Workspace', true
FROM accounts t
WHERE NOT EXISTS (SELECT 1 FROM workspaces w WHERE w.tenant_id = t.id);
