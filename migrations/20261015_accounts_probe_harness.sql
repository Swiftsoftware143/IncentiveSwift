-- IncentiveSwift: mark harness-created workspaces at creation (kanban t_4a744754).
--
-- WHY: the public signup (`POST /api/v1/auth/register`) mints its workspace as the `accounts`
-- row itself (`accounts.tenant_id` = the account's own id — a standalone workspace) and names it
-- from the caller's email local part, so a workspace a probe mints through the real signup route
-- is byte-indistinguishable from a customer's. Attribution must be DATA, not a guess about a name
-- (policy: /opt/swift/docs/fleet-probe-residue-policy-2026-09-28.md, answer 3c; shape proven on
-- FunnelSwift t_fc88ec2a and ADASwift t_ae307966).
--
-- The fleet's harnesses ALREADY send `X-Swift-Harness: <marker>`; this column is what lets the app
-- record it instead of dropping it, so a future sweep over this app is a QUERY rather than a guess.
-- The authoritative detector already implements the marker arm
-- (`scripts/test-tenant-sweep.py`, spec `tag`), and it is the only arm that reaches a workspace
-- whose USER/OWNER identity is already gone.
--
-- ROOT TABLE: `accounts`, not `tenants`. IncentiveSwift's `tenants` table holds only platform
-- fixtures (SwiftSoftware + the Tenancy-Canary), while EVERY self-serve signup lands in `accounts`
-- — that is the row the operator's customer list shows.
--
-- ADDITIVE, NO BACKFILL — on purpose. The column records where a workspace CAME FROM at creation
-- time and that fact was never captured for the rows that already exist; inventing a value for
-- them would be a lie in the one column a sweeper will trust. Legacy rows stay classified by the
-- owner-ADDRESS policy.
--
-- The column is nullable with no default, so the ALTER is catalog-only: no table rewrite, no data
-- rewrite, and no existing reader or writer of `accounts` changes shape. A NEW migration file
-- because this app's runner records every applied filename in `_migrations` and never retries a
-- recorded file (see `src/db/migrations.rs`).

ALTER TABLE accounts ADD COLUMN IF NOT EXISTS probe_harness text;

COMMENT ON COLUMN accounts.probe_harness IS
  'Harness marker taken from the X-Swift-Harness request header when the workspace was created (trimmed, lowercased, must match ^[a-z0-9][a-z0-9._-]{2,63}$), else NULL. Header only: never set from a request body or query field, and never set by any other route.';
