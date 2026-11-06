-- zz_is10_public_support_intake.sql
--
-- A VISITOR who cannot sign in files a support request (2026-11-06). Every support address this
-- product printed lived on a domain publishing NO mail exchanger — measured 2026-11-06 on
-- dns.google and cloudflare-dns.com: MX = 0 for swiftsoftware.net, adaswift.com,
-- incentiveswift.com, workflowswift.com, missedcallrespondr.com and coreswiftcrm.com — and nothing
-- on this host listens on port 25, so a locked-out customer's "write to support@…" message bounced
-- and they stayed locked out.
--
-- The contact page now files that message into the operator's existing queue through
-- `POST /api/v1/public/support-request`. Such a row belongs to no workspace (`tenant_id` is already
-- nullable here) and was filed by no account, so the only address the team can answer is the one
-- the visitor typed: `requested_by` carries it. The signed-in arm leaves it '' and keeps using
-- `created_by` / `tenant_id`.
--
-- Idempotent by construction: `sqlx::migrate!` applies this file once, and the statement also
-- carries IF NOT EXISTS so a rehearsal or re-run can never fail the boot.
ALTER TABLE support_tickets ADD COLUMN IF NOT EXISTS requested_by text NOT NULL DEFAULT '';

-- The flood bound counts one visitor address's messages in the last hour, so that is the index.
CREATE INDEX IF NOT EXISTS idx_support_tickets_requested_by
    ON support_tickets (lower(requested_by), created_at DESC)
    WHERE requested_by <> '';
