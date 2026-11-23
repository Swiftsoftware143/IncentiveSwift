-- The operator can now add a suppression BY HAND (POST /api/compliance/suppressions, 2026-11-23),
-- and the reason is their own words ("asked by phone", "complained in person") — not one of a closed
-- set. FunnelSwift and ADASwift carry the same control with a FREE-TEXT reason; this app's enum CHECK
-- (shipped in zz_is10_email_compliance.sql) rejected every hand-added entry, so the mechanism existed
-- but the population was empty. The gate (compliance::suppression) reads `reason` as an opaque string
-- and never branches on its value, so loosening the set is safe; the other writers (unsubscribe,
-- provider events) still write their canonical tokens and are unaffected.
ALTER TABLE email_suppressions DROP CONSTRAINT IF EXISTS email_suppressions_reason_check;
