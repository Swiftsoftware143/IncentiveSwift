-- ============================================================================
-- Tenant delete arms — kanban t_9f3d85dc
-- ============================================================================
-- WHY: DELETE /api/v1/admin/tenants/:id answered HTTP 500 and the account row
-- SURVIVED. The handler deleted four hand-picked child tables and then the
-- `accounts` row, so Postgres refused with 23503 the moment any other
-- account-owned child row existed:
--
--   update or delete on table "accounts" violates foreign key constraint
--
-- Measured 2026-10-08 on the live DB (bash /opt/swift/bin/isq.sh incentiveswift):
-- eight FK edges in this schema carry no delete arm (confdeltype = 'a', NO
-- ACTION). Six of them sit on the tenant delete path and are what made even a
-- SINGLE delete impossible, let alone a mass delete:
--
--   accounts              <- inbound_messages.account_id      (nullable)
--   accounts              <- iqs_funnels.account_id           (NOT NULL)
--   accounts              <- password_resets.account_id       (NOT NULL)
--   accounts              <- provider_keys.account_id         (NOT NULL)
--   entries               <- loyalty_checkins.entry_id        (reachable only
--                              through campaigns, which cascade from accounts)
--   loyalty_reward_tiers  <- loyalty_rewards_earned.tier_id   (reachable only
--                              through loyalty_programs)
--
-- ARM CHOICE (per edge, from the reader + the delete path — not from taste):
-- every one of these columns is the OWNERSHIP pointer of a row that is
-- unreachable without the parent (no reader selects it except scoped by that
-- parent: inbound_messages/sms_handler, iqs_funnels/iqs_handler,
-- password_resets/auth_handler, provider_keys/coreswift_external +
-- output_actions, loyalty_checkins/loyalty_checkin, loyalty_rewards_earned via
-- loyalty_reward_tiers), so the row is not preserved data and CASCADE is the
-- honest arm. SET NULL is not even available for three of them (NOT NULL).
-- The remaining two NO ACTION edges (accounts.plan_tier_id -> plan_tiers,
-- users.tenant_id -> tenants) are left alone deliberately: plan_tiers is a
-- catalogue parent, not a tenant, and users.tenant_id is the fleet
-- `tenants`/`users` vocabulary, which the delete path retires explicitly in the
-- handler (and must, because users has to go before tenants).
--
-- ALSO: loyalty_programs had NO foreign key at all on its ownership column
-- (`account_id` was added by 20261005_loyalty_program_account_owner.sql without
-- one), so deleting an account left live, readable loyalty programs behind.
-- Measured before this file was written: 0 orphaned
-- `loyalty_programs.account_id` values, so the edge can be added VALIDATED
-- (no NOT VALID needed, no backfill).
--
-- SAFETY: the runner executes each file as ONE implicit transaction over the
-- simple query protocol, so the whole file lands or none of it does. No row is
-- touched — only constraint definitions change; the ON DELETE trigger each new
-- edge installs is what future deletes use.
-- ============================================================================

-- 1. accounts <- inbound_messages
ALTER TABLE inbound_messages DROP CONSTRAINT IF EXISTS inbound_messages_account_id_fkey;
ALTER TABLE inbound_messages
    ADD CONSTRAINT inbound_messages_account_id_fkey
    FOREIGN KEY (account_id) REFERENCES accounts(id) ON DELETE CASCADE;

-- 2. accounts <- iqs_funnels
ALTER TABLE iqs_funnels DROP CONSTRAINT IF EXISTS iqs_funnels_account_id_fkey;
ALTER TABLE iqs_funnels
    ADD CONSTRAINT iqs_funnels_account_id_fkey
    FOREIGN KEY (account_id) REFERENCES accounts(id) ON DELETE CASCADE;

-- 3. accounts <- password_resets
ALTER TABLE password_resets DROP CONSTRAINT IF EXISTS password_resets_account_id_fkey;
ALTER TABLE password_resets
    ADD CONSTRAINT password_resets_account_id_fkey
    FOREIGN KEY (account_id) REFERENCES accounts(id) ON DELETE CASCADE;

-- 4. accounts <- provider_keys
ALTER TABLE provider_keys DROP CONSTRAINT IF EXISTS provider_keys_account_id_fkey;
ALTER TABLE provider_keys
    ADD CONSTRAINT provider_keys_account_id_fkey
    FOREIGN KEY (account_id) REFERENCES accounts(id) ON DELETE CASCADE;

-- 5. entries <- loyalty_checkins (second level: accounts -> campaigns -> entries)
ALTER TABLE loyalty_checkins DROP CONSTRAINT IF EXISTS loyalty_checkins_entry_id_fkey;
ALTER TABLE loyalty_checkins
    ADD CONSTRAINT loyalty_checkins_entry_id_fkey
    FOREIGN KEY (entry_id) REFERENCES entries(id) ON DELETE CASCADE;

-- 6. loyalty_reward_tiers <- loyalty_rewards_earned (second level: accounts ->
--    loyalty_programs -> loyalty_reward_tiers)
ALTER TABLE loyalty_rewards_earned DROP CONSTRAINT IF EXISTS loyalty_rewards_earned_tier_id_fkey;
ALTER TABLE loyalty_rewards_earned
    ADD CONSTRAINT loyalty_rewards_earned_tier_id_fkey
    FOREIGN KEY (tier_id) REFERENCES loyalty_reward_tiers(id) ON DELETE CASCADE;

-- 7. loyalty_programs.account_id — the ownership pointer that had no edge at all.
ALTER TABLE loyalty_programs DROP CONSTRAINT IF EXISTS loyalty_programs_account_id_fkey;
ALTER TABLE loyalty_programs
    ADD CONSTRAINT loyalty_programs_account_id_fkey
    FOREIGN KEY (account_id) REFERENCES accounts(id) ON DELETE CASCADE;
