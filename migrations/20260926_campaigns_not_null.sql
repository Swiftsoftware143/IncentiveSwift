-- t_aefe745a — campaigns.account_id (and its 8 struct siblings) are decoded as
-- non-Option fields by `struct Campaign` (#[derive(sqlx::FromRow)],
-- src/db/campaigns.rs:46), so ONE NULL in ANY of the nine NULLABLE columns fails the
-- WHOLE-ROW decode of get_campaign_by_slug / get_campaign_by_id / list_campaigns.
--
-- Measured on the deployed binary before this file (HEAD 622de8ab, sha256 332a723c…):
-- a NULL in each of the nine columns made
--   GET /api/v1/loyalty/public/program/<slug>  -> 500
--   POST /api/v1/loyalty/checkin               -> 500
-- with the literal `error occurred while decoding column "<col>": unexpected null;
-- try decoding as an `Option`` in the container log — including account_id, the column
-- this card names.
--
-- ARM: SET NOT NULL (not Option<T>). A campaign without an owner is not a state of this
-- app: account_id is FK -> accounts(id) ON DELETE CASCADE (a deleted account deletes the
-- campaign, it never NULLs the column), every account-scoped read filters on it
-- (list_campaigns, the max_campaigns usage count, the admin owner_name join), and a
-- campaign with a NULL owner is invisible to every one of them. Decoding Option<Uuid>
-- would require ~20 consumer sites to invent an answer (the card forbids unwrap_or_default
-- / an invented account id) and would leave the other eight columns of the same statement
-- just as fatal.
--
-- CENSUS (the reason SET NOT NULL is honest here — no writer can produce these NULLs):
--   * only two writers of public.campaigns exist in the fleet (grep 'INSERT INTO campaigns'):
--     src/db/campaigns.rs:175 create_campaign and src/handlers/campaigns.rs:201
--     clone_campaign. Both NAME account_id and bind a non-Option Uuid (the create path takes
--     the owner from the authenticated JWT, never from the request body).
--   * every other column below either is omitted by both writers (=> its DEFAULT applies:
--     status 'active', config/outcome_tags/delivery_config '{}', delivery_method 'webhook',
--     created_at now(), loyalty_points_per_play 0, auto_enroll_loyalty false) or is bound to
--     a non-None expression (delivery_method.unwrap_or_else("webhook"), config.unwrap_or_else
--     (json!({})), loyalty_points_per_play.unwrap_or(0), auto_enroll_loyalty.unwrap_or(false)).
--   * no UPDATE names any of them with a nullable bind: 5 UPDATE sites, all bind non-None
--     values or `COALESCE(col, ...)`. A NULL account_id is therefore reachable only by direct
--     SQL — which is exactly what this constraint now refuses.
--
-- The guard is the precondition check, not decoration: it counts the NULLs FIRST and the file
-- is executed as ONE batch (src/db/migrations.rs, sqlx::raw_sql => implicit transaction), so a
-- non-zero count refuses the whole file loudly instead of half-constraining the table. There is
-- nothing to backfill: a NULL owner has no derivable owner, so a future NULL means a human
-- decides (and this file tells them which column and how many rows).

DO $$
DECLARE
    bad text;
BEGIN
    SELECT string_agg(x.col || '=' || x.n, ', ' ORDER BY x.col) INTO bad
    FROM (
        SELECT u.col AS col,
               (SELECT count(*) FROM campaigns c WHERE to_jsonb(c) ->> u.col IS NULL) AS n
        FROM unnest(ARRAY[
            'account_id', 'status', 'config', 'outcome_tags', 'delivery_method',
            'delivery_config', 'created_at', 'loyalty_points_per_play', 'auto_enroll_loyalty'
        ]) AS u(col)
    ) AS x
    WHERE x.n > 0;

    IF bad IS NOT NULL THEN
        RAISE EXCEPTION 'campaigns: refusing to SET NOT NULL — NULL rows present: %', bad;
    END IF;
END $$;

ALTER TABLE campaigns ALTER COLUMN account_id SET NOT NULL;
ALTER TABLE campaigns ALTER COLUMN status SET NOT NULL;
ALTER TABLE campaigns ALTER COLUMN config SET NOT NULL;
ALTER TABLE campaigns ALTER COLUMN outcome_tags SET NOT NULL;
ALTER TABLE campaigns ALTER COLUMN delivery_method SET NOT NULL;
ALTER TABLE campaigns ALTER COLUMN delivery_config SET NOT NULL;
ALTER TABLE campaigns ALTER COLUMN created_at SET NOT NULL;
ALTER TABLE campaigns ALTER COLUMN loyalty_points_per_play SET NOT NULL;
ALTER TABLE campaigns ALTER COLUMN auto_enroll_loyalty SET NOT NULL;
