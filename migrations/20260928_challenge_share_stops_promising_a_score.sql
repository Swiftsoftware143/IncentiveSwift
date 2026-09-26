-- t_0e99038b — `challenge_share` promises a score the platform cannot produce for the campaigns
-- that select it, and offers no way to render one.
--
-- MEASURED on the live database (`incentiveswift`, 32 `email_templates` rows, 26 entries / 0 with
-- a non-NULL `score`) and in `src/`:
--
--   * `challenge_share` is the ONLY row using `{{user_score}}` that no served surface can fill:
--       - it is the 24h follow-up for `quiz` (`lifecycle_emails::LIFECYCLE_MAP`) and the follow-up
--         for the DEFAULT arm — `personality`, `chat`, `leaderboard`, `loyalty`;
--       - every one of those campaigns is played through `POST /api/v1/entries`, whose only served
--         caller is `www-app/play.html`: it posts `contact + utm_*`, no answers and no score;
--       - `HANDOFF.md` describes `personality` as an "outcome-type quiz, SHAREABLE RESULT" — its
--         result is an outcome label, not a number to beat — so the row's `beat your score`
--         promise has no datum to bind for the default arm either.
--     The subject is the worst of it: `Think you can beat {{user_score}}?` reaches the RECIPIENT
--     with the braces in the subject line.
--   * The score's real producer exists but is not on this path: `handlers::quiz_handler::submit_quiz`
--     (`POST /api/v1/quiz/{campaign_id}/submit`) scores answers and writes `entries.score`, and no
--     served page calls it (`grep -rn 'quiz' www*/` -> 0 callers). Wiring it is a FEATURE (a quiz
--     player on the served surface), carded, not a defect fix. `calc_summary` — the other
--     score-shaped row — is handled in `src/` instead: a calculator campaign's formula is
--     evaluated over the customer's inputs to produce the entry's score, so its row keeps its
--     promise (kanban t_0e99038b).
--
-- ARM — de-score the row (card arm (b), restricted to the rows where no datum can exist): the
-- promise is removed, the row's actual ask (challenge your friends / share the campaign) is kept,
-- and only placeholders a sender binds are used — no new placeholder is introduced and
-- `{{share_link}}` is already bound by `lifecycle_emails::entry_email_vars` for every lifecycle
-- send. The alternative (add `{{#if user_score}}`) is impossible: the renderer does not implement
-- mustache sections and ships them verbatim (carded t_c8df11e6).
--
-- A score-shaped challenge copy belongs on a row the day a served surface carries a score again.
--
-- Idempotent: each `replace()` is a no-op once applied, the predicate then matches nothing, and
-- the backstop has nothing to raise.
UPDATE email_templates
   SET subject   = replace(subject,
                           'Think you can beat {{user_score}}?',
                           'Challenge your friends to {{campaign_name}}'),
       body      = replace(body,
                           'Challenge your friends to beat your {{campaign_name}} score of {{user_score}}. Share now!',
                           'Challenge your friends to play {{campaign_name}}. Share your link: {{share_link}}'),
       html_body = replace(html_body,
                           '<h2>Beat this score!</h2><p>You scored <b>{{user_score}}</b> on {{campaign_name}}. Challenge your friends: <b>{{share_link}}</b></p>',
                           '<h2>Challenge your friends!</h2><p>Send them {{campaign_name}}: <b>{{share_link}}</b></p>')
 WHERE template_type = 'challenge_share'
   AND aid IS NULL
   AND is_default = true
   AND (subject LIKE '%user_score%' OR body LIKE '%user_score%' OR html_body LIKE '%user_score%');

-- LOUD BACKSTOP. `replace()` is silent when its needle does not match, and this migration exists
-- precisely to stop a mail promising a number nobody computes: if the shipped row still carries
-- the placeholder after the UPDATE above, fail the migration — it is then NOT recorded in
-- `_migrations` and the next boot retries, instead of the braces shipping as a "successful" fix.
DO $$
DECLARE still_promised integer;
BEGIN
    SELECT count(*) INTO still_promised
      FROM email_templates
     WHERE template_type = 'challenge_share'
       AND aid IS NULL
       AND is_default = true
       AND (subject LIKE '%{{user_score}}%'
            OR body LIKE '%{{user_score}}%'
            OR html_body LIKE '%{{user_score}}%');
    IF still_promised > 0 THEN
        RAISE EXCEPTION 'challenge_share still promises {{user_score}} after the de-scoring UPDATE — the shipped text did not match the needles in this migration';
    END IF;
END $$;
