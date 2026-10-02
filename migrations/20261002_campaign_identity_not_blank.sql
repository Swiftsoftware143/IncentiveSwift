-- t_a56c03a1 — a campaign's identity strings (`name`, `tag_namespace`) could be EMPTY, and a
-- `tag_namespace` could be shared by two campaigns in the same account.
--
-- MEASURED LIVE on binary f03d7fe8 (2026-10-02, evidence /opt/swift/audits/t_a56c03a1/01-before-api.txt):
--   POST /api/v1/campaigns {"name":"","type":"spin_wheel","tag_namespace":"campaign",...}
--     -> 200 and a row whose console <td> renders blank (nothing in the Campaigns table tells the
--        operator which campaign it is; GET /api/v1/campaigns/:slug and the customer surface show
--        the same empty name).
--   POST with name="   "           -> 200, stored verbatim as '   '.
--   POST with tag_namespace=""     -> 200; every outcome tag this campaign applies is
--        `{tag_namespace}_entrant` / `_winner` / `_runner_up` (src/handlers/entries.rs:604-639,
--        score_reveal_handler.rs:110, long_form_qualifier_handler.rs:136,
--        mystery_handler.rs:95), so an empty namespace produces the account-wide tags
--        `_entrant`/`_winner` that no other campaign can distinguish from its own.
--   POST a SECOND campaign with the SAME tag_namespace -> 200; `public.tags` is
--        UNIQUE (account_id, lower(name)) (migrations/00001_full_schema.sql), so both campaigns
--        write and read the SAME tag rows and their audiences become indistinguishable.
--   PUT /api/v1/campaigns/:slug {"name":""} -> 200, DB row name='' (a rename to empty).
--
-- Both columns are NOT NULL with NO default (00001_full_schema.sql:32-35), so an empty string is the
-- only way any caller can omit them. This file is the class-wide guard for the WRITE, not only for the
-- two handlers (src/db/campaigns.rs create_campaign, src/handlers/campaigns.rs clone_campaign) that
-- are the fleet's only writers.
--
-- The precondition check counts the offending rows FIRST and the file runs as ONE batch
-- (sqlx::raw_sql => implicit transaction), so a non-zero count refuses the whole file loudly instead
-- of half-constraining the table. Measured at authoring time: 0 blank names, 0 blank namespaces,
-- 0 duplicate namespace groups — nothing to backfill, so a future offender means a human decides.
--
-- `lower(tag_namespace)` (not the bare column) is deliberate and mirrors the `tags` table's own
-- UNIQUE (account_id, lower(name)): `Summer` and `summer` produce the same `Summer_entrant` /
-- `summer_entrant` tag vocabulary, and `tags.name` is compared case-insensitively.
--
-- Idempotent by construction: src/db/migrations.rs re-runs a file whose ledger row could not be
-- written, so every statement below is a no-op when it already holds.

DO $$
DECLARE
    bad text;
BEGIN
    SELECT string_agg(x.col || '=' || x.n, ', ' ORDER BY x.col) INTO bad
    FROM (
        SELECT 'blank_name' AS col, (SELECT count(*) FROM campaigns WHERE btrim(name) = '') AS n
        UNION ALL
        SELECT 'blank_tag_namespace', (SELECT count(*) FROM campaigns WHERE btrim(tag_namespace) = '')
        UNION ALL
        SELECT 'duplicate_tag_namespace_groups', (
            SELECT count(*) FROM (
                SELECT account_id, lower(tag_namespace)
                FROM campaigns GROUP BY 1, 2 HAVING count(*) > 1
            ) d
        )
    ) AS x
    WHERE x.n > 0;

    IF bad IS NOT NULL THEN
        RAISE EXCEPTION 'campaigns: refusing to add identity constraints — offending rows present: %', bad;
    END IF;
END $$;

DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'campaigns_name_not_blank') THEN
        ALTER TABLE campaigns ADD CONSTRAINT campaigns_name_not_blank CHECK (btrim(name) <> '');
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'campaigns_tag_namespace_not_blank') THEN
        ALTER TABLE campaigns ADD CONSTRAINT campaigns_tag_namespace_not_blank CHECK (btrim(tag_namespace) <> '');
    END IF;
END $$;

CREATE UNIQUE INDEX IF NOT EXISTS campaigns_account_tag_namespace_uidx
    ON campaigns (account_id, lower(tag_namespace));
