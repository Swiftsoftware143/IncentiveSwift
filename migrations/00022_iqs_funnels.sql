-- IQS Funnels — qualifying survey/questionnaire module
-- Separate from campaigns, replaces quiz/trivia with a full qualifying system

-- Main funnel entity (like a campaign but for surveys)
CREATE TABLE IF NOT EXISTS iqs_funnels (
    id UUID PRIMARY KEY,
    account_id UUID NOT NULL REFERENCES accounts(id),
    name VARCHAR(255) NOT NULL,
    funnel_type VARCHAR(50) NOT NULL DEFAULT 'survey',
    description TEXT,
    status VARCHAR(20) NOT NULL DEFAULT 'draft',
    theme JSONB NOT NULL DEFAULT '{"preset": "dark_modern", "bg_gradient": "linear-gradient(135deg, #0f172a 0%, #1e293b 100%)", "accent_color": "#8b5cf6", "font_family": "Inter", "logo_url": null, "button_style": "rounded"}',
    config JSONB NOT NULL DEFAULT '{"show_progress_bar": true, "allow_skip": false, "collect_email": true, "collect_name": true, "collect_phone": false, "redirect_url": null, "passing_score": 70, "max_attempts": 1}',
    slug VARCHAR(255) UNIQUE NOT NULL,
    response_count INT NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Funnel questions
CREATE TABLE IF NOT EXISTS iqs_questions (
    id UUID PRIMARY KEY,
    funnel_id UUID NOT NULL REFERENCES iqs_funnels(id) ON DELETE CASCADE,
    question_key VARCHAR(100) NOT NULL,
    question_text TEXT NOT NULL,
    question_type VARCHAR(50) NOT NULL DEFAULT 'single_choice',
    sort_order INT NOT NULL DEFAULT 0,
    required BOOLEAN NOT NULL DEFAULT true,
    options JSONB NOT NULL DEFAULT '[]',
    config JSONB NOT NULL DEFAULT '{}',
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Conditional logic & score threshold rules
CREATE TABLE IF NOT EXISTS iqs_rules (
    id UUID PRIMARY KEY,
    funnel_id UUID NOT NULL REFERENCES iqs_funnels(id) ON DELETE CASCADE,
    rule_type VARCHAR(50) NOT NULL DEFAULT 'always',
    priority INT NOT NULL DEFAULT 0,
    conditions JSONB NOT NULL DEFAULT '[]',
    actions JSONB NOT NULL DEFAULT '[]',
    is_active BOOLEAN NOT NULL DEFAULT true,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Submissions (filled-out funnels)
CREATE TABLE IF NOT EXISTS iqs_submissions (
    id UUID PRIMARY KEY,
    funnel_id UUID NOT NULL REFERENCES iqs_funnels(id) ON DELETE CASCADE,
    contact_id UUID NOT NULL REFERENCES contacts(id) ON DELETE CASCADE,
    answers JSONB NOT NULL DEFAULT '[]',
    total_score INT NOT NULL DEFAULT 0,
    outcome VARCHAR(50),
    tags_applied TEXT[] NOT NULL DEFAULT '{}',
    source JSONB NOT NULL DEFAULT '{}',
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Indexes
CREATE INDEX IF NOT EXISTS idx_iqs_funnels_account_id ON iqs_funnels(account_id);
CREATE INDEX IF NOT EXISTS idx_iqs_funnels_slug ON iqs_funnels(slug);
CREATE INDEX IF NOT EXISTS idx_iqs_questions_funnel_id ON iqs_questions(funnel_id);
CREATE INDEX IF NOT EXISTS idx_iqs_rules_funnel_id ON iqs_rules(funnel_id);
CREATE INDEX IF NOT EXISTS idx_iqs_submissions_funnel_id ON iqs_submissions(funnel_id);
CREATE INDEX IF NOT EXISTS idx_iqs_submissions_contact_id ON iqs_submissions(contact_id);
CREATE INDEX IF NOT EXISTS idx_iqs_submissions_created_at ON iqs_submissions(created_at);
