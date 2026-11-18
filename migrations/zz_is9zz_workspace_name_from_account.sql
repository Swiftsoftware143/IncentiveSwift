-- zz_is9zz_workspace_name_from_account.sql
--
-- A new account's default workspace carries the ACCOUNT'S OWN NAME, not the generic
-- 'My Workspace'. Measured defect (2026-11-18): every account in this app showed a workspace
-- called "My Workspace" (seven identical rows), so neither the customer on their own Workspaces
-- screen nor the operator could tell one workspace from another.
--
-- Cause: zz_is29's `mint_default_workspace()` hardcoded the label at INSERT time. The Rust mint
-- (crate::account_mint::mint_account) only renames the trigger's row when the caller supplies a
-- "Workspace name", and the machine door (FunnelSwift tag -> free account) supplies none, so every
-- account minted through it kept the placeholder.
--
-- Fixing the trigger fixes EVERY door at once - self-serve signup, the machine door, the portfolio
-- sync and any future writer - because they all INSERT an accounts row and let this trigger mint the
-- workspace. The account NAME is the only label that is meaningful before the customer has touched
-- anything; a caller that does supply a "Workspace name" still wins, because the Rust mint renames
-- the row afterwards.
--
-- FILE NAME / ORDER IS LOAD-BEARING. This app's runner (`src/db/migrations.rs`) sorts entries with
-- `sort_by_key(file_name)` - BYTE order - and the from-zero harness honours the same order. A name
-- like `zz_is100_...` byte-sorts BEFORE `zz_is28_workspaces_and_private_inbox.sql` (which creates the
-- `workspaces` table) and before `zz_is29_workspace_on_account_insert.sql` (which would then REPLACE
-- this function back to the hardcoded label on a fresh database). `zz_is9zz_` sorts after every
-- existing file, so it always runs last and wins on both the live upgrade and a from-zero build.
--
-- Additive and idempotent (same shape as zz_is29): the function is REPLACED - the trigger keeps
-- pointing at it - and the backfill only touches rows still carrying the trigger's own default
-- label 'My Workspace' with an account whose name is non-blank. A name a customer chose is never
-- overwritten (it is not 'My Workspace'). Portfolio companies ARE included: the owner standard is
-- that a portfolio company must look exactly like a hand signup (docs/fleet-portfolio-*-provisioning),
-- and their workspace rows carry only this trigger's placeholder - nothing a human picked.
-- No BEGIN/COMMIT: the runner executes each file inside one implicit transaction.

CREATE OR REPLACE FUNCTION mint_default_workspace() RETURNS TRIGGER AS $$
BEGIN
    INSERT INTO workspaces (tenant_id, name, is_default)
    VALUES (
        NEW.id,
        left(
            COALESCE(NULLIF(btrim(COALESCE(NEW.name, '')), ''), 'My Workspace'),
            120
        ),
        true
    )
    ON CONFLICT DO NOTHING;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

-- Backfill: every workspace still carrying the trigger's placeholder from before this file, whose
-- account has a usable name. Guarded against `ux_workspaces_tenant_name` (tenant_id, lower(name)) so
-- an account that already holds a workspace of the same name cannot abort the boot; the backfill then
-- repeats harmlessly on every start (it is recorded once, and re-running it updates zero rows).
UPDATE workspaces w
   SET name = left(btrim(a.name), 120),
       updated_at = now()
  FROM accounts a
 WHERE w.tenant_id = a.id
   AND w.name = 'My Workspace'
   AND btrim(COALESCE(a.name, '')) <> ''
   AND left(btrim(a.name), 120) <> 'My Workspace'
   AND NOT EXISTS (
        SELECT 1
          FROM workspaces o
         WHERE o.tenant_id = w.tenant_id
           AND o.id <> w.id
           AND lower(o.name) = lower(left(btrim(a.name), 120))
   );
