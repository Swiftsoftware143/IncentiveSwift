-- Migration: Reviews & Ratings (IncentiveSwift)
-- Customer reviews with a 1..max_rating score, optional campaign/contact linkage.
CREATE TABLE IF NOT EXISTS reviews (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id     UUID,
    campaign_id   UUID,                         -- optional linked campaign
    contact_id    UUID,                         -- reviewer (contact/member)
    rating        INTEGER NOT NULL CHECK (rating BETWEEN 1 AND 5),
    title         TEXT,
    body          TEXT,
    reviewer_name TEXT,
    status        TEXT NOT NULL DEFAULT 'pending',  -- pending | approved | rejected
    moderation_note TEXT,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_reviews_tenant ON reviews (tenant_id);
CREATE INDEX IF NOT EXISTS idx_reviews_campaign ON reviews (campaign_id);
CREATE INDEX IF NOT EXISTS idx_reviews_status ON reviews (status);
