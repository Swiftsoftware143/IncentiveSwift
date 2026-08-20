-- Pending email queue for scheduled/delayed sends (24-48h follow-ups, expiry reminders).
CREATE TABLE IF NOT EXISTS pending_emails (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    account_id UUID NOT NULL,
    to_email TEXT NOT NULL,
    template_type TEXT NOT NULL,
    vars JSONB NOT NULL DEFAULT '{}'::jsonb,
    send_at TIMESTAMPTZ NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',  -- pending | sent | failed | cancelled
    attempts INT NOT NULL DEFAULT 0,
    last_error TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    sent_at TIMESTAMPTZ
);
CREATE INDEX IF NOT EXISTS idx_pending_emails_due ON pending_emails (status, send_at) WHERE status = 'pending';
CREATE INDEX IF NOT EXISTS idx_pending_emails_account ON pending_emails (account_id);
