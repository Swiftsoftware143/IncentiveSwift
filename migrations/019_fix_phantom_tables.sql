CREATE TABLE IF NOT EXISTS categories (
    id UUID PRIMARY KEY,
    tenant_id UUID,
    name VARCHAR(255),
    description TEXT,
    created_at TIMESTAMPTZ DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS leads (
    id UUID PRIMARY KEY,
    tenant_id UUID,
    name VARCHAR(255),
    email VARCHAR(255),
    phone VARCHAR(50),
    status VARCHAR(50),
    created_at TIMESTAMPTZ DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS knowledge_base (
    id UUID PRIMARY KEY,
    tenant_id UUID,
    title VARCHAR(255),
    content TEXT,
    created_at TIMESTAMPTZ DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS call_logs (
    id UUID PRIMARY KEY,
    tenant_id UUID,
    caller VARCHAR(100),
    callee VARCHAR(100),
    duration_secs INTEGER,
    notes TEXT,
    created_at TIMESTAMPTZ DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS surfaces (
    id UUID PRIMARY KEY,
    tenant_id UUID,
    name VARCHAR(255),
    config JSONB DEFAULT '{}',
    created_at TIMESTAMPTZ DEFAULT NOW()
);
