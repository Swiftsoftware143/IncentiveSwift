-- t_1adeb952 — 30 of the 59 shipped `email_templates` rows could not be selected by ANY producer.
--
-- MEASURED on the live database (`incentiveswift`) and in `src/`, reproducibly:
--   /opt/swift/audits/t_1adeb952/20-census.py -> 20-census.txt
--
-- A row can only be selected through the app's own call sites, and there are exactly these:
--   * `lifecycle_emails::lifecycle_templates` — stage 1 (immediate) and stage 3 (the 24h
--     `pending_emails` replay in `email_queue::process_due_emails`; `schedule_email`, the ONLY
--     writer of `pending_emails`, is called from exactly one place: `trigger_entry_lifecycle`);
--   * `handlers::entries` step 8 — `format!("{}_winner", campaign.r#type)`, then the literal
--     `winner`;
--   * the two account mails: `auth_handler::register` -> `welcome`, `email::send_welcome_email`
--     -> `welcome_credentials`.
--
-- Applying that to all 59 rows (aid IS NULL AND is_default = true) leaves 30 that no producer can
-- reach, in two classes:
--
--   1. DEAD MAP ARM (10 rows). `lifecycle_templates` keyed arms on `survey`, `iqs`,
--      `secret_codes`, `tier` and `scratch`. NONE of those strings is in
--      `db::campaigns::VALID_MECHANIC_TYPES` (15 types; `scratch_card` is the real name of the
--      last one), and `POST /api/v1/campaigns` refuses any type outside that set
--      (`db::campaigns::create_campaign` -> `validate_mechanic_type`); the served console's
--      MECHANICS list carries those same 15. So those arms could never match a campaign.
--        `survey`       -> submission_thanks, impact_report
--        `iqs`          -> submission_receipt, nurture_followup
--        `secret_codes` -> code_accepted_reward, next_code_hint
--        `tier`         -> tier_status_assign, tier_upgrade_progress
--        `scratch`      -> scratch_confirm_prize, second_chance_replay   <-- mechanically REAL
--
--   2. NO PRODUCER AT ALL (20 rows): big_reveal, closing_notice, code_expiring_soon,
--      community_spotlight, consultation_offer, expiry_warning, final_countdown,
--      further_info_request, milestone_reached, official_score_release, poll_thank_you,
--      qualification_approved, qualification_next_steps, redemption_reminder, results_update,
--      reward_delivery, score_reveal, scratch_winner, survey_thank_you, winner_announcement.
--      Most are the unused "middle" row of a mechanic group (the seed ships three rows per
--      mechanic; the lifecycle map fires two stages), and `poll_thank_you`/`survey_thank_you`
--      duplicate an entry row of their own group.
--
-- ARM — FIX THE ONE DRIFTED KEY THAT NAMES A REAL MECHANIC, RETIRE THE ROWS NOTHING CAN SELECT.
-- This is the same rule the parent card used (t_375c8c40): the code stops claiming a producer that
-- does not exist, and the data stops advertising a template a recipient can never receive. The
-- retired rows are visible in the tenant console's Email Templates list (GET
-- /api/v1/email-templates), which is the only surface that shows them.
--
--   a. `src/lifecycle_emails.rs` keys the scratch mechanic on `scratch_card` (the arm existed, the
--      key was a typo), and drops the other four arms — so `scratch_confirm_prize` /
--      `second_chance_replay` become LIVE for every scratch-card campaign, and nothing claims a
--      producer for tier/secret-code/survey/IQS campaigns the product cannot create.
--   b. `scratch_winner` -> `scratch_card_winner`: the winner path resolves
--      `format!("{campaign_type}_winner")`, so the seeded per-type winner row for the scratch
--      mechanic was unreachable under the old name and every scratch-card win mailed the generic
--      `winner` copy instead. Same single drift as (a).
--   c. the 27 rows nothing can select are DELETED, which removes them from the tenant template
--      picker. Every remaining row is selectable by a caller that exists today.
--
-- RESTORING: the deleted text lives in `20260820_email_lifecycle_seed.sql` and
-- `20260820_email_templates_seed.sql`; re-inserting from them (aid NULL, is_default true) is the
-- reverse. Nothing is dropped schema-wise and no other table references `email_templates`
-- (measured: 0 foreign keys target it).
--
-- Idempotent: the rename is guarded by NOT EXISTS on its target (the global-default unique index
-- is partial, so a blind second UPDATE would violate it), the placeholder removal is a no-op once
-- the text is gone, and the DELETE matches only rows that are still there.

-- ── (b) the scratch mechanic's per-type winner row gets the name the winner path resolves ─────
UPDATE email_templates
   SET template_type = 'scratch_card_winner'
 WHERE template_type = 'scratch_winner'
   AND aid IS NULL
   AND is_default = true
   AND NOT EXISTS (
       SELECT 1 FROM email_templates t2
        WHERE t2.template_type = 'scratch_card_winner' AND t2.aid IS NULL AND t2.is_default
   );

-- ── the now-live scratch row promises an expiry date no datum in this schema carries ──────────
-- Exactly the parent card's measurement (`campaign_wins` has no expiry column, `vouchers` is
-- empty and written only by the loyalty flow, which selects no template): remove the promise
-- instead of inventing the value. Same treatment `win_voucher` got in
-- 20260926_lifecycle_email_placeholders_bound.sql.
--
-- Scope, stated exactly: this row's `html_body` carries no expiry (measured), and
-- `delivery::sender::send_template_by_type` PREFERS `html_body` — so the WIRE was already free of
-- this placeholder and this statement changes no mail. It removes a promise the platform cannot
-- keep from the row's own TEXT body, which is what the tenant console shows when the row is
-- opened in the editor, i.e. the value a tenant would believe is merged.
UPDATE email_templates
   SET body = replace(body, ' Expires {{expiry_date}}.', '')
 WHERE template_type = 'scratch_confirm_prize'
   AND aid IS NULL
   AND is_default = true
   AND body LIKE '%Expires {{expiry_date}}%';

-- ── (c) retire the rows no producer can select ───────────────────────────────────────────────
DELETE FROM email_templates
 WHERE aid IS NULL
   AND is_default = true
   AND template_type IN (
       -- class 2: no producer at all
       'big_reveal',
       'closing_notice',
       'code_expiring_soon',
       'community_spotlight',
       'consultation_offer',
       'expiry_warning',
       'final_countdown',
       'further_info_request',
       'milestone_reached',
       'official_score_release',
       'poll_thank_you',
       'qualification_approved',
       'qualification_next_steps',
       'redemption_reminder',
       'results_update',
       'reward_delivery',
       'score_reveal',
       'survey_thank_you',
       'winner_announcement',
       -- class 1: the four dead arms' rows (the scratch pair is KEPT — its mechanic exists)
       'code_accepted_reward',
       'impact_report',
       'next_code_hint',
       'nurture_followup',
       'submission_receipt',
       'submission_thanks',
       'tier_status_assign',
       'tier_upgrade_progress'
   );
