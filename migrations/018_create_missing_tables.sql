CREATE TABLE IF NOT EXISTS journal_entries (
    id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    entry_type VARCHAR(100),
    description TEXT,
    amount DECIMAL(12,2),
    created_at TIMESTAMPTZ DEFAULT NOW()
);
