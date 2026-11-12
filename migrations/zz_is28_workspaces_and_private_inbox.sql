-- 025_workspaces_and_private_inbox.sql
--
-- The workspace model and the private (multi-address) inbox, ported from CoreSwift-CRM
-- (kanban t_cd522551) so every Swift app offers the same screens. Additive only: no existing
-- table is altered here, and every statement is IF NOT EXISTS so a repeated boot is a no-op.
--
-- Two separate concepts, deliberately kept apart:
--   * A WORKSPACE organises CRM work inside one business account (an account). Its settings stay
--     shared at the tenant; contacts, mailboxes and automations can be scoped to it.
--   * An INBOX is a real mailbox on the customer's OWN domain. Mail belongs to the ACCOUNT, not
--     to a workspace (`private_email_messages` has no workspace_id and must not grow one): a
--     message arrived at an address, and the address belongs to the business.

-- ── Workspaces ──────────────────────────────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS workspaces (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id   UUID NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name        VARCHAR(120) NOT NULL,
    is_default  BOOLEAN NOT NULL DEFAULT false,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- two workspaces in one account may not share a name
CREATE UNIQUE INDEX IF NOT EXISTS ux_workspaces_tenant_name
    ON workspaces (tenant_id, lower(name));

CREATE INDEX IF NOT EXISTS ix_workspaces_tenant
    ON workspaces (tenant_id);

-- exactly one default workspace per tenant (the one a fresh session opens)
CREATE UNIQUE INDEX IF NOT EXISTS ux_workspaces_one_default
    ON workspaces (tenant_id) WHERE is_default;

-- every existing account gets its default workspace so nothing is blank
INSERT INTO workspaces (tenant_id, name, is_default)
SELECT t.id, 'My Workspace', true
FROM accounts t
WHERE NOT EXISTS (SELECT 1 FROM workspaces w WHERE w.tenant_id = t.id);

-- ── Private inbox: domains (the customer's own, with their own credential) ─────────────────────

CREATE TABLE IF NOT EXISTS private_email_domains (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id     UUID NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    domain        VARCHAR(255) NOT NULL,
    -- The credential is sealed at rest (`enc:v1:` envelope, tenant-scoped AES-256-GCM). Structured
    -- as in CoreSwift so one mail client and one set of handler SQL serve every app.
    mailgun_api_key TEXT NOT NULL,
    mailgun_region  VARCHAR(4) NOT NULL DEFAULT 'us',
    catch_all_enabled BOOLEAN NOT NULL DEFAULT false,
    verified      BOOLEAN NOT NULL DEFAULT false,
    label         VARCHAR(120),
    provider_type VARCHAR(32) NOT NULL DEFAULT 'mailgun',
    smtp_host     VARCHAR(255),
    smtp_port     INTEGER DEFAULT 587,
    smtp_username VARCHAR(255),
    smtp_password_encrypted TEXT,
    smtp_tls      BOOLEAN NOT NULL DEFAULT true,
    inbound_mode  VARCHAR(32) NOT NULL DEFAULT 'webhook',
    webhook_signing_key_encrypted TEXT,
    inbound_provider VARCHAR(32) NOT NULL DEFAULT 'mailgun',
    inbound_token VARCHAR(255),
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE(tenant_id, domain)
);

CREATE INDEX IF NOT EXISTS idx_private_email_domains_tenant
    ON private_email_domains(tenant_id);

-- ── Private inbox: mailboxes (the addresses mail arrives on) ────────────────────────────────────

CREATE TABLE IF NOT EXISTS private_email_boxes (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id    UUID NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    domain_id    UUID NOT NULL REFERENCES private_email_domains(id) ON DELETE CASCADE,
    -- NULL means the address belongs to the whole workspace (assignable to anybody).
    user_id      UUID REFERENCES accounts(id) ON DELETE SET NULL,
    local_part   VARCHAR(64) NOT NULL,
    email_address VARCHAR(255) NOT NULL,
    mailgun_mailbox_id VARCHAR(255),
    forwarding_enabled BOOLEAN NOT NULL DEFAULT true,
    signature    TEXT,
    status       VARCHAR(32) NOT NULL DEFAULT 'active',
    -- Mail to this address ALSO opens a work item. Off everywhere on this app: ADASwift's queue is
    -- `support_tickets`, and the mail-to-queue bridge is separate work (see private_email::mod).
    creates_ticket BOOLEAN NOT NULL DEFAULT false,
    access_mode  VARCHAR(16) NOT NULL DEFAULT 'paid',
    trial_started_at TIMESTAMPTZ,
    trial_ends_at    TIMESTAMPTZ,
    trial_expired_at TIMESTAMPTZ,
    signature_mode   VARCHAR(16) NOT NULL DEFAULT 'inherit',
    signature_full_name  VARCHAR(160),
    signature_job_title  VARCHAR(160),
    signature_company    VARCHAR(160),
    signature_phone      VARCHAR(60),
    signature_line       VARCHAR(255),
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE(tenant_id, email_address)
);

CREATE INDEX IF NOT EXISTS idx_private_email_boxes_tenant ON private_email_boxes(tenant_id);
CREATE INDEX IF NOT EXISTS idx_private_email_boxes_domain ON private_email_boxes(domain_id);

-- ── Private inbox: messages (the inbox's own records) ───────────────────────────────────────────

CREATE TABLE IF NOT EXISTS private_email_messages (
    id                  uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id           uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    -- ON DELETE SET NULL, never CASCADE: deleting a mailbox must not destroy the mail that arrived
    -- through it, and `address` still says where it came from.
    mailbox_id          uuid REFERENCES private_email_boxes(id) ON DELETE SET NULL,
    address             varchar(320) NOT NULL,
    folder              varchar(64) NOT NULL DEFAULT 'inbox',
    direction           varchar(8) NOT NULL DEFAULT 'inbound',
    from_address        varchar(320) NOT NULL DEFAULT '',
    from_name           varchar(200),
    to_address          text NOT NULL DEFAULT '',
    cc_address          text,
    subject             text NOT NULL DEFAULT '',
    body_plain          text NOT NULL DEFAULT '',
    body_html           text,
    snippet             text NOT NULL DEFAULT '',
    message_id          text,
    in_reply_to         text,
    provider            varchar(32),
    provider_message_id text,
    is_read             boolean NOT NULL DEFAULT false,
    is_starred          boolean NOT NULL DEFAULT false,
    size_bytes          integer NOT NULL DEFAULT 0,
    has_attachments     boolean NOT NULL DEFAULT false,
    -- No foreign key to a ticket table: this app's work queue is `support_tickets`, whose shape is
    -- the ADA desk's. The column stays so the client's contract is identical across the fleet; a
    -- value written here would need the bridge that does not exist yet.
    ticket_id           uuid,
    is_ticket           boolean NOT NULL DEFAULT false,
    assigned_user_id    uuid REFERENCES accounts(id) ON DELETE SET NULL,
    received_at         timestamptz NOT NULL DEFAULT now(),
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now()
);

-- Idempotency: a provider retries a delivery it did not see acknowledged. (tenant, message_id) is
-- the natural guard; partial, because an empty string must never collide.
CREATE UNIQUE INDEX IF NOT EXISTS idx_private_email_messages_dedupe
    ON private_email_messages (tenant_id, message_id)
    WHERE message_id IS NOT NULL AND message_id <> '';

CREATE INDEX IF NOT EXISTS idx_private_email_messages_inbox
    ON private_email_messages (tenant_id, folder, received_at DESC);
CREATE INDEX IF NOT EXISTS idx_private_email_messages_address
    ON private_email_messages (tenant_id, address, received_at DESC);
CREATE INDEX IF NOT EXISTS idx_private_email_messages_mailbox
    ON private_email_messages (mailbox_id, received_at DESC);
CREATE INDEX IF NOT EXISTS idx_private_email_messages_unread
    ON private_email_messages (tenant_id) WHERE is_read = false;
CREATE INDEX IF NOT EXISTS idx_private_email_messages_starred
    ON private_email_messages (tenant_id) WHERE is_starred = true;

-- ── Private inbox: folders and labels (the rail and the tags) ───────────────────────────────────

CREATE TABLE IF NOT EXISTS private_email_folders (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id   uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    path        varchar(120) NOT NULL,
    name        varchar(120) NOT NULL,
    parent      varchar(64) NOT NULL DEFAULT 'inbox',
    created_at  timestamptz NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, path)
);

CREATE INDEX IF NOT EXISTS idx_private_email_folders_tenant
    ON private_email_folders (tenant_id);

CREATE TABLE IF NOT EXISTS private_email_labels (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id   uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name        varchar(60) NOT NULL,
    color       varchar(16) NOT NULL DEFAULT '#64748b',
    created_at  timestamptz NOT NULL DEFAULT now()
);

-- two labels in one account may not share a name (expression indexes cannot be table constraints)
CREATE UNIQUE INDEX IF NOT EXISTS ux_private_email_labels_tenant_name
    ON private_email_labels (tenant_id, lower(name));

CREATE TABLE IF NOT EXISTS private_email_message_labels (
    message_id  uuid NOT NULL REFERENCES private_email_messages(id) ON DELETE CASCADE,
    label_id    uuid NOT NULL REFERENCES private_email_labels(id) ON DELETE CASCADE,
    PRIMARY KEY (message_id, label_id)
);

-- ── Private inbox: operator overrides, retention, signature, storage notices ────────────────────

CREATE TABLE IF NOT EXISTS tenant_email_limits (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id   uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    max_domains integer,
    max_mailboxes integer,
    max_aliases_per_mailbox integer,
    retention_days integer NOT NULL DEFAULT 3650,
    last_purged_at timestamptz,
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now(),
    UNIQUE (tenant_id)
);

CREATE TABLE IF NOT EXISTS tenant_email_signatures (
    tenant_id  uuid PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
    full_name  varchar(160) NOT NULL DEFAULT '',
    job_title  varchar(160) NOT NULL DEFAULT '',
    company    varchar(160) NOT NULL DEFAULT '',
    phone      varchar(60) NOT NULL DEFAULT '',
    line       varchar(255) NOT NULL DEFAULT '',
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS private_email_storage_notices (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id   uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    threshold_pct smallint NOT NULL,
    period      varchar(16) NOT NULL,
    usage_bytes bigint NOT NULL,
    quota_bytes bigint NOT NULL,
    sent_at     timestamptz NOT NULL DEFAULT now()
);

-- Named, reusable provider credentials: a customer can keep two Mailgun accounts and point one
-- domain at each, and re-entering a dead key REPLACES it in place instead of orphaning every domain
-- that pointed at it.
CREATE TABLE IF NOT EXISTS private_email_api_keys (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id   uuid NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    label       varchar(120) NOT NULL,
    provider    varchar(32) NOT NULL DEFAULT 'mailgun',
    api_key_encrypted text NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_private_email_api_keys_tenant
    ON private_email_api_keys (tenant_id);
