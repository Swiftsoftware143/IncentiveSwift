-- zz_is18 — a PLAN REQUEST is a ticket, and nothing on the row said so (2026-10-20).
--
-- Why this file exists: the customer dashboard's "Request the <plan> plan" button files a support
-- ticket (POST /api/v1/support-tickets), because no payment provider is stored on this install. So
-- the operator's queue holds BUYING requests and ordinary questions in ONE list, with no field to
-- tell them apart — the only person who can take the money could not find the request.
-- Every sibling app files the same ticket with `source = 'plan_request'` (missedcallrespondr,
-- WorkflowSwift, FunnelSwift and ADASwift all read that spelling). IncentiveSwift had no column.
--
-- `source` is free text on purpose: 'plan_request' today, and any later origin needs no migration.
-- NULL means "an ordinary ticket".
ALTER TABLE support_tickets ADD COLUMN IF NOT EXISTS source text;

-- The operator's Plan requests view reads WHERE source = 'plan_request'; a PARTIAL index keeps that
-- view cheap on a queue that is mostly ordinary tickets.
CREATE INDEX IF NOT EXISTS idx_support_tickets_source
    ON support_tickets (source) WHERE source IS NOT NULL;

-- Backfill the requests filed BEFORE this file. The dashboard's shape is category='billing' with a
-- subject of "<Move> request: <Plan> plan". Scoped to that exact pair so an ordinary billing
-- question is never relabelled as a buying request. Idempotent: it only touches rows still NULL.
UPDATE support_tickets
   SET source = 'plan_request'
 WHERE source IS NULL
   AND category = 'billing'
   AND subject ILIKE '% request:% plan';
