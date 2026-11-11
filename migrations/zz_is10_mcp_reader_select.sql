-- The fleet MCP server (systemd `mcp`, https://hooks.swiftsoftware.net/mcp) reads app data through
-- the `mcp_reader` role, which holds SELECT and nothing else.
--
-- `entries`, `contacts` and `accounts` have row level security ENABLED and no SELECT policy for any
-- non-owner role, so a SELECT as `mcp_reader` returned zero rows on every table - the role's grant
-- was real, the rows were not. Every IncentiveSwift MCP tool therefore answered 0 for ever, however
-- much data the workspace held. Measured before this migration: 0 rows as `mcp_reader`, 49 rows in
-- `campaign_referrals` as the owner in the same database.
--
-- These policies are read-only, are granted to `mcp_reader` alone, and change nothing for `anon`,
-- `public` or any other role. An app-side lead is an `entries` row reached through `campaigns` and
-- `accounts`, which is why all three tables are needed; the tenant filter itself is applied by the
-- MCP server on every query, and this role cannot write.

drop policy if exists mcp_reader_read_entries on entries;
create policy mcp_reader_read_entries on entries
    for select to mcp_reader using (true);

drop policy if exists mcp_reader_read_contacts on contacts;
create policy mcp_reader_read_contacts on contacts
    for select to mcp_reader using (true);

drop policy if exists mcp_reader_read_accounts on accounts;
create policy mcp_reader_read_accounts on accounts
    for select to mcp_reader using (true);
