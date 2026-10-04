-- IncentiveSwift — contacts become tenant-scoped, many-to-many.
--
-- David, 2026-10-04: "contacts should always be tenant scoped. Tenants should not see other tenants
-- contacts" and "there may be tenants that share leads".
--
-- MEASURED BEFORE TOUCHING ANYTHING:
--   * public.contacts has NO tenant column at all, and its unique indexes are GLOBAL:
--     contacts_email_idx on lower(email), contacts_phone_idx on phone.
--   * Those global indexes are CORRECT for a shared identity — one row per human — so they are NOT
--     changed. Two businesses CAN share the same person; that is the point.
--   * 146 contacts exist; only 20 are attributable from history (entries -> campaigns -> account_id).
--     The other 126 predate tracking, so no owner can be recovered for them.
--
-- A NEW migration: an applied migration's checksum aborts boot if edited.
--
-- The 126 unattributed contacts are deliberately linked to NOTHING. For a privacy fix the safe
-- direction is invisible-to-everyone, not guess-an-owner: the rows are preserved in `contacts` and
-- become visible again the moment a tenant imports or captures them, which writes a link row.

CREATE TABLE IF NOT EXISTS contact_tenants (
    contact_id      uuid NOT NULL REFERENCES contacts(id) ON DELETE CASCADE,
    account_id      uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    first_linked_at timestamptz NOT NULL DEFAULT now(),
    source          text,
    PRIMARY KEY (contact_id, account_id)
);
CREATE INDEX IF NOT EXISTS idx_contact_tenants_account ON contact_tenants (account_id);

-- Backfill from history. A contact captured by two businesses legitimately gets TWO rows, and both
-- keep seeing them.
INSERT INTO contact_tenants (contact_id, account_id, source)
SELECT DISTINCT e.contact_id, c.account_id, 'entry'
  FROM entries e
  JOIN campaigns c ON c.id = e.campaign_id
 WHERE e.contact_id IS NOT NULL
   AND c.account_id IS NOT NULL
ON CONFLICT DO NOTHING;
