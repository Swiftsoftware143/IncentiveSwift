-- IQS Campaign Link + Classification
-- Migration 00016

-- Link campaigns to IQS funnels for qualifying surveys
ALTER TABLE campaigns ADD COLUMN IF NOT EXISTS iqs_funnel_id UUID REFERENCES iqs_funnels(id) ON DELETE SET NULL;

-- Add classification field to IQS submissions for Hot/Warm/Cold auto-classification
ALTER TABLE iqs_submissions ADD COLUMN IF NOT EXISTS classification VARCHAR(20);
