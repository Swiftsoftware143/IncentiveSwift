-- ============================================================
-- Migration 00018: Quiz/Trivia Correct Answers + Scoring
-- ============================================================

-- Add correct_answer column to questions table for quiz/trivia scoring
ALTER TABLE public.questions
  ADD COLUMN IF NOT EXISTS correct_answer text,
  ADD COLUMN IF NOT EXISTS score_weight integer NOT NULL DEFAULT 1,
  ADD COLUMN IF NOT EXISTS options jsonb; -- stores MC options as JSON array of strings

-- Add outcome persona/tier mapping to campaigns config will use existing config JSONB

-- Add CRM field mapping columns to questions for delivery integration
ALTER TABLE public.questions
  ADD COLUMN IF NOT EXISTS crm_field text, -- e.g. "budget", "authority", "timeline", "pain_point"
  ADD COLUMN IF NOT EXISTS crm_field_type text; -- "custom_field" | "standard_field" | "tag" | "score"

-- Create a delivery_mapping table for campaign-level CRM field configuration
CREATE TABLE IF NOT EXISTS public.campaign_crm_mapping (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    campaign_id uuid NOT NULL REFERENCES public.campaigns(id) ON DELETE CASCADE,
    source_field text NOT NULL, -- "quiz_score" | "persona" | "pain_point" | "budget" | "authority" | "timeline" | "utm_source" | question_id
    crm_system text NOT NULL, -- "hubspot" | "salesforce" | "activecampaign" | "gohighlevel" | "coreswift"
    crm_field_type text NOT NULL, -- "custom_field" | "standard_field" | "tag" | "score" | "lead_property"
    crm_field_key text NOT NULL, -- the field name/id in the CRM
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_campaign_crm_mapping_campaign ON public.campaign_crm_mapping(campaign_id);

-- No RLS — app uses JWT-based auth, not Postgres RLS
