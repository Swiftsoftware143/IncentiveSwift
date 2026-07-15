-- Migration 00019: Credit system + Stripe + SMS webhook
-- Adds credit balance tracking, credit transactions, and Stripe customer info

-- Add credit balance column to accounts
ALTER TABLE accounts ADD COLUMN IF NOT EXISTS credits_balance INTEGER NOT NULL DEFAULT 0;
ALTER TABLE accounts ADD COLUMN IF NOT EXISTS credits_lifetime_used INTEGER NOT NULL DEFAULT 0;
ALTER TABLE accounts ADD COLUMN IF NOT EXISTS stripe_customer_id TEXT;
ALTER TABLE accounts ADD COLUMN IF NOT EXISTS stripe_subscription_id TEXT;
ALTER TABLE accounts ADD COLUMN IF NOT EXISTS last_credit_reset TIMESTAMPTZ;

-- Credit transactions ledger
CREATE TABLE IF NOT EXISTS credit_transactions (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    account_id UUID NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    amount INTEGER NOT NULL,  -- positive = credit, negative = debit
    balance_after INTEGER NOT NULL DEFAULT 0,
    action TEXT NOT NULL,     -- 'top_up', 'usage_spin', 'usage_chat', 'usage_sms', 'usage_quiz', 'usage_raffle', 'usage_email', 'usage_checkin', 'usage_referral', 'admin_adjust', 'monthly_reset', 'refund'
    reference_type TEXT,       -- 'campaign', 'spin', 'entry', 'stripe', etc.
    reference_id TEXT,         -- UUID or external reference
    description TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_credit_trans_account ON credit_transactions(account_id);
CREATE INDEX IF NOT EXISTS idx_credit_trans_created ON credit_transactions(created_at DESC);
CREATE INDEX IF NOT EXISTS idx_credit_trans_action ON credit_transactions(action);

-- Stripe checkout sessions (for tracking top-ups)
CREATE TABLE IF NOT EXISTS stripe_checkout_sessions (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    account_id UUID NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    stripe_session_id TEXT NOT NULL UNIQUE,
    amount INTEGER NOT NULL,  -- cents (e.g. 1000 = $10)
    credits INTEGER NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',  -- 'pending', 'completed', 'expired', 'failed'
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    completed_at TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_stripe_session_account ON stripe_checkout_sessions(account_id);
CREATE INDEX IF NOT EXISTS idx_stripe_session_id ON stripe_checkout_sessions(stripe_session_id);

-- Inbound SMS/WhatsApp messages (from Telnyx webhook)
CREATE TABLE IF NOT EXISTS inbound_messages (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    message_id TEXT UNIQUE,         -- Telnyx message UUID
    from_number TEXT NOT NULL,
    to_number TEXT NOT NULL,
    body TEXT,
    media_urls JSONB DEFAULT '[]'::jsonb,
    direction TEXT DEFAULT 'inbound',  -- 'inbound', 'inbound_whatsapp'
    campaign_slug TEXT,              -- resolved campaign (if any)
    account_id UUID REFERENCES accounts(id),  -- resolved account
    processed BOOLEAN DEFAULT false,
    processed_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_inbound_messages_from ON inbound_messages(from_number);
CREATE INDEX IF NOT EXISTS idx_inbound_messages_processed ON inbound_messages(processed);

-- Stripe credit packages (configurable top-up amounts)
INSERT INTO features (key, label, category, description) VALUES
    ('stripe_credit_topups', 'Stripe Credit Top-Ups', 'billing', 'Allow credit purchases via Stripe')
ON CONFLICT (key) DO NOTHING;

-- Add stripe to available providers if not there
INSERT INTO available_providers (provider, label, category, description) VALUES
    ('stripe', 'Stripe', 'payments', 'Payment processing via Stripe')
ON CONFLICT (provider) DO NOTHING;
