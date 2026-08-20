-- Migration: Ticket / Support system (IncentiveSwift)
-- Adds a support-ticket module with status flow and optional campaign/contact linkage.
CREATE TABLE IF NOT EXISTS support_tickets (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id       UUID,                          -- owning account (nullable for system tickets)
    campaign_id     UUID,                          -- optional linked campaign
    contact_id      UUID,                          -- optional linked contact / member
    subject         TEXT NOT NULL,
    description     TEXT,
    status          TEXT NOT NULL DEFAULT 'open',  -- open | in_progress | resolved | closed
    priority        TEXT NOT NULL DEFAULT 'normal',-- low | normal | high | urgent
    category        TEXT,                          -- e.g. bug, feature, billing, other
    assignee_id     UUID,
    created_by      UUID,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    resolved_at     TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_support_tickets_tenant ON support_tickets (tenant_id);
CREATE INDEX IF NOT EXISTS idx_support_tickets_status ON support_tickets (status);
CREATE INDEX IF NOT EXISTS idx_support_tickets_campaign ON support_tickets (campaign_id);

-- Ticket conversation/messages thread
CREATE TABLE IF NOT EXISTS support_ticket_messages (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    ticket_id   UUID NOT NULL REFERENCES support_tickets(id) ON DELETE CASCADE,
    author_id   UUID,
    body        TEXT NOT NULL,
    is_internal BOOLEAN NOT NULL DEFAULT false,   -- internal note vs customer-visible reply
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_support_ticket_msgs ON support_ticket_messages (ticket_id);
