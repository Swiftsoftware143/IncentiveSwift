-- Add missing source_tag column to iqs_funnels.
-- The Rust IqsFunnel struct + create/update handlers reference source_tag,
-- but the 00022 migration never created it (schema/code drift).
-- Idempotent.
ALTER TABLE iqs_funnels ADD COLUMN IF NOT EXISTS source_tag VARCHAR(255);
