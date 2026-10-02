-- t_d8eef6ae — the QUIZ follow-up promised a score, was de-scored because nothing served could
-- produce one, and can promise it again now that a served surface does.
--
-- MEASURED on the live database (`incentiveswift`, 3 campaigns / 0 of type quiz, 0 `questions`
-- rows, 18 entries with `score` NULL, version `20260928_challenge_share_stops_promising_a_score`
-- applied) and in `src/`:
--
--   * `challenge_share` is the follow-up for the QUIZ arm AND for the DEFAULT arm —
--     `personality`, `chat`, `leaderboard`, `loyalty` (`lifecycle_emails::LIFECYCLE_MAP`,
--     `DEFAULT_LIFECYCLE`). It was de-scored by `20260928_...` because NO served surface could
--     fill `{{user_score}}` for ANY of those five mechanics.
--   * That is still true for four of them, and it is no longer true for `quiz`: this card ships
--     the served quiz player (`www-app/play.html` fetches `GET /api/v1/play/{id}/questions` and
--     posts `POST /api/v1/quiz/{id}/submit`), and `handlers::quiz_handler::submit_quiz` scores the
--     answers through `score_quiz_submission`, writes `entries.score`, and now carries that number
--     into the lifecycle vars (`lifecycle_emails::entry_email_vars` -> `user_score`).
--   * So the score-shaped copy goes back on a row — but NOT on the shared row: re-scoring
--     `challenge_share` would put braces back in the SUBJECT of every personality/chat/
--     leaderboard/loyalty follow-up, which is the defect `20260928_...` removed. The quiz arm gets
--     its OWN row (`src/lifecycle_emails.rs`: `("quiz", "entry_ack", "quiz_challenge_share")`) and
--     the default pair keeps the de-scored one.
--
-- The copy below is the pre-de-scoring text of `challenge_share`, byte-for-byte as
-- `20260928_challenge_share_stops_promising_a_score.sql` records it in its own `replace()`
-- needles — restored, not re-invented. Every placeholder it names is bound for a quiz entry:
-- `{{campaign_name}}`, `{{share_link}}` and `{{user_score}}` by
-- `lifecycle_emails::entry_email_vars`.
--
-- IDEMPOTENT: the INSERT is predicated on the row not already existing, so a re-run is a no-op
-- (the partial unique index `idx_email_templates_unique` would otherwise raise). No RAISE: the
-- runner retries a raising file forever, and "already seated" is this migration's success.
-- REVERSAL: `DELETE FROM email_templates WHERE template_type = 'quiz_challenge_share' AND aid IS
-- NULL AND is_default`, plus reverting the one line in `LIFECYCLE_MAP`; the de-scored
-- `challenge_share` row then carries the follow-up again, exactly as before this card.
INSERT INTO email_templates (template_type, name, subject, body, html_body, is_default, aid)
SELECT 'quiz_challenge_share',
       'Quiz Challenge & Share (scored)',
       'Think you can beat {{user_score}}?',
       'Challenge your friends to beat your {{campaign_name}} score of {{user_score}}. Share now!',
       '<h2>Beat this score!</h2><p>You scored <b>{{user_score}}</b> on {{campaign_name}}. Challenge your friends: <b>{{share_link}}</b></p>',
       true,
       NULL
WHERE NOT EXISTS (SELECT 1
                    FROM email_templates
                   WHERE template_type = 'quiz_challenge_share'
                     AND aid IS NULL
                     AND is_default);

-- LOUD-ADJACENT CHECK, not a refusal: the shared default row must still be the DE-SCORED one, or
-- the quiz row above is pointless (a personality campaign's follow-up would carry braces). A
-- NOTICE is the mechanism here because the runner retries a raising file forever; the deployment's
-- own probe asserts the shipped bytes.
DO $$
BEGIN
    IF EXISTS (SELECT 1
                 FROM email_templates
                WHERE template_type = 'challenge_share'
                  AND aid IS NULL
                  AND is_default
                  AND (subject LIKE '%{{user_score}}%'
                       OR body LIKE '%{{user_score}}%'
                       OR html_body LIKE '%{{user_score}}%')) THEN
        RAISE NOTICE 'challenge_share (the DEFAULT arm''s follow-up) promises {{user_score}} again — the default arm has no score producer, so four mechanics would mail braces';
    ELSE
        RAISE NOTICE 'quiz_challenge_share seated; challenge_share (default arm) stays de-scored';
    END IF;
END $$;
