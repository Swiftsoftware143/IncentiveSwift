-- IQS Campaign Link + Classification
-- Migration 00016
--
-- IS-7 note: `iqs_funnels` is created by 00022_iqs_funnels.sql, which sorts AFTER this
-- file, so the inline `REFERENCES iqs_funnels(id)` this migration used to carry aborted the
-- whole file on a fresh database (and with it the boot). Production's column is a plain
-- varchar with no foreign key, and production has no such constraint either, so the
-- constraint is removed rather than invented -- the column itself is added with IF EXISTS
-- guards so the file applies whether or not the IQS tables exist yet.

-- Link campaigns to IQS funnels for qualifying surveys
ALTER TABLE IF EXISTS campaigns ADD COLUMN IF NOT EXISTS iqs_funnel_id VARCHAR(255);

-- Add classification field to IQS submissions for Hot/Warm/Cold auto-classification
ALTER TABLE IF EXISTS iqs_submissions ADD COLUMN IF NOT EXISTS classification VARCHAR(20);
