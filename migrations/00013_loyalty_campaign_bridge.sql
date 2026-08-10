-- Add loyalty_program_id to campaigns for the campaign→loyalty bridge
ALTER TABLE campaigns ADD COLUMN IF NOT EXISTS loyalty_program_id uuid REFERENCES loyalty_programs(id) ON DELETE SET NULL;

-- Add loyalty_points_per_play to campaigns for points awarded per game play
ALTER TABLE campaigns ADD COLUMN IF NOT EXISTS loyalty_points_per_play integer DEFAULT 0;

-- Add auto_enroll_loyalty flag to campaigns
ALTER TABLE campaigns ADD COLUMN IF NOT EXISTS auto_enroll_loyalty boolean DEFAULT false;

-- Add loyalty_program_id index
CREATE INDEX IF NOT EXISTS idx_campaigns_loyalty_program_id ON campaigns(loyalty_program_id);
