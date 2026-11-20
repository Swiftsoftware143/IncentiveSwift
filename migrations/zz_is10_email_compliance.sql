-- Email compliance: the opt-out, the suppression list, and the evidence.
--
-- Why this exists here (2026-11-20): this app sends mail — lifecycle stages, queue ticker, delivery
-- sender, receipts — and had NO opt-out, no suppression list and no record of what the provider did with
-- anything. Bulk mail without an opt-out is unlawful, and one bad send from a shared sending identity
-- damages the reputation that carries every other customer's receipts.
--
-- Shapes match CoreSwift's tables on purpose (`email_suppressions`, `email_message_log`,
-- `email_consent`), so the two apps' compliance layers behave identically and a report written for one
-- is true of the other.
--
-- Additive only.

CREATE TABLE IF NOT EXISTS email_suppressions (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id   uuid NOT NULL,
    address     text NOT NULL,
    -- 'all' stops everything except transactional; 'transactional' is written only by a hard bounce or a
    -- spam complaint, where delivering at all is the harm.
    mail_class  text NOT NULL DEFAULT 'all' CHECK (mail_class IN ('all', 'bulk', 'notification', 'transactional')),
    reason      text NOT NULL CHECK (reason IN ('unsubscribed', 'hard_bounce', 'complaint', 'manual', 'provider')),
    source      text,
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, address, mail_class)
);

CREATE INDEX IF NOT EXISTS idx_is_email_suppressions_lookup
    ON email_suppressions (tenant_id, address);

CREATE TABLE IF NOT EXISTS email_message_log (
    id                    uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id             uuid NOT NULL,
    mail_class            text NOT NULL CHECK (mail_class IN ('bulk', 'notification', 'transactional')),
    address               text NOT NULL,
    subject               text,
    provider              text,
    provider_message_id   text,
    status                text NOT NULL CHECK (status IN ('sent', 'failed', 'refused_suppressed', 'refused_quota')),
    detail                text,
    created_at            timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_is_email_message_log_tenant
    ON email_message_log (tenant_id, created_at DESC);

CREATE TABLE IF NOT EXISTS email_consent (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id   uuid NOT NULL,
    address     text NOT NULL,
    mail_class  text NOT NULL DEFAULT 'bulk',
    state       text NOT NULL CHECK (state IN ('granted', 'revoked')),
    how         text,
    ip          text,
    user_agent  text,
    created_at  timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_is_email_consent_tenant
    ON email_consent (tenant_id, address, created_at DESC);
