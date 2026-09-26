-- t_f23d33fb — widget_snippets / tablet_sessions / loyalty_reward_tiers / custom_domains: six (plus
-- three adjacent) NULLABLE columns are decoded into NON-Option struct fields, so ONE NULL in ANY of
-- them fails the WHOLE-ROW decode of the route that reads that row. The classes were made visible by
-- the literal-extractor widening of t_4f4cba97 (surface_handler.rs went 10 -> 31 parsed literals).
--
-- The rows this file closes (all `NULLABLE-DECODED-AS-NON-OPTION (struct)`, live NULL count measured
-- 0 for every one => LATENT, exactly like t_2dc041e9 / t_8a896955 found theirs):
--
--   file:line  fn                 column                        table                 struct
--   :119       get_widget_js      created_at                    widget_snippets       WidgetSnippet
--   :119       get_widget_js      (is_active)                   widget_snippets       WidgetSnippet
--   :168       get_widget_config  created_at                    widget_snippets       WidgetSnippet
--   :168       get_widget_config  (is_active)                   widget_snippets       WidgetSnippet
--   :218       get_tablet_view    interaction_count, created_at tablet_sessions       TabletSession
--   :281       tablet_interaction interaction_count, created_at tablet_sessions       TabletSession
--   :446       get_loyalty_dash   requires_approval, sort_order  loyalty_reward_tiers  RewardTierRow
--   (adjacent, uncarded, same file+class: struct CustomDomain at :751 list_domains and :794
--    register_domain over custom_domains.is_active/created_at/updated_at)
--
-- ARM (decided per column from the writer census, not by taste): SET NOT NULL for all nine.
-- Both arms already ship in this app — COALESCE(col, <DEFAULT>) AS col (t_2dc041e9,
-- loyalty_members.points_balance/lifetime_points) and SET NOT NULL with a writer census
-- (t_8a896955, loyalty_members.program_id/contact_id/member_since). The tiebreaker is whether any
-- writer can PRODUCE the NULL, and here none can (`10-census.txt`):
--
--   * widget_snippets and tablet_sessions have NO writer anywhere: `grep -rniE
--     'insert +into +(widget_snippets|tablet_sessions)' src/` => 0 hits, and the same grep over
--     migrations/, /opt/swift and /root => 0. The single widget_snippets row and the zero
--     tablet_sessions rows can only have come from direct SQL, which this constraint now refuses
--     LOUDLY at write time (23502) instead of 500-ing a read.
--   * loyalty_reward_tiers has exactly two writers, neither able to write NULL:
--       src/handlers/loyalty.rs:435 INSERT names both columns and binds
--         `body.requires_approval.unwrap_or(false)` / `body.sort_order.unwrap_or(0)` (non-Option),
--       src/handlers/loyalty.rs:472 UPDATE sets `COALESCE($n, col)` for both (NULL means KEEP).
--   * custom_domains: the only crate writer (src/handlers/surface_handler.rs:784) names
--     (id, tenant_id, domain, target_type) and OMITS all three => the non-NULL DEFAULTs apply; the
--     only UPDATE (:877) sets the literals `now()` / `true` / `now()`. The one non-crate writer is
--     the retired t_274992c8 retire-proof harness, which re-inserts a snapshot of live values.
--   * every one of the nine columns is NULLABLE with a NON-NULL DEFAULT (true/false/0/now()), so
--     an omitted column always lands on a real value; no migration INSERTs into any of the four
--     tables.
--   * the FK actions cannot produce a NULL either (widget_snippets/tablet_sessions -> campaigns,
--     loyalty_reward_tiers -> loyalty_programs are ON DELETE CASCADE, never SET NULL).
--
-- Why not COALESCE/Option here: `created_at`/`updated_at` have DEFAULT now(), so
-- COALESCE(col, now()) would invent a DIFFERENT timestamp on every read (a fabrication), and
-- Option<T> would change the JSON of every one of these routes for a state no writer can produce.
-- SET NOT NULL costs ZERO Rust edits: the existing non-Option decode becomes correct, every
-- response JSON is byte-identical for the non-NULL rows that exist, and a future NULL writer fails
-- LOUDLY at write time instead of 500-ing a read.
--
-- Reachability, measured (`30-before.txt`): 5 of the 6 carded columns are HOT (a NULL row 500s the
-- route with `decoding column "<col>": unexpected null; try decoding as an Option`):
--   widget_snippets.created_at            GET /api/v1/widget/<hash> + /config   -> 500
--   tablet_sessions.interaction_count     GET /api/v1/tablet/<id>              -> 500
--   tablet_sessions.created_at            GET /api/v1/tablet/<id>              -> 500
--   tablet_sessions.interaction_count     POST /api/v1/tablet/<id>/interact    -> 500 (the write
--                                         side: `interaction_count + 1` keeps the NULL, the decode
--                                         at :281 is what fails)
--   loyalty_reward_tiers.requires_approval/sort_order  GET /api/v1/play/<campaign>/dashboard -> 500
--                                         (via a probe loyalty_programs row: the one live program has
--                                          campaign_id NULL, so the statement is unreachable today
--                                          without a fixture - measured, not assumed)
--   widget_snippets.is_active is the exception: BOTH statements filter `AND is_active = true`, and
--   `NULL = true` is NULL, so a NULL is_active row is EXCLUDED (404) rather than decoded - it is
--   constrained here so the state stops being representable, and its live leg reports 404 in both
--   phases (the predicate, not the decode, is what excludes it).
--
-- The guard is the precondition check, not decoration: it counts the NULLs FIRST and the whole file
-- runs as ONE batch (src/db/migrations.rs, sqlx::raw_sql => implicit transaction), so a non-zero
-- count refuses the file loudly instead of half-constraining a table. There is nothing to backfill:
-- a widget snippet with no creation moment, a session with no interaction count, a reward tier with
-- no approval flag/order, or a domain with no active flag/timestamps is not a state of this app.

DO $$
DECLARE
    bad text;
BEGIN
    SELECT string_agg(e.col || '=' || e.n, ', ' ORDER BY e.col) INTO bad
    FROM (
        SELECT 'widget_snippets.is_active' AS col,
               count(*) FILTER (WHERE is_active IS NULL) AS n FROM widget_snippets
        UNION ALL SELECT 'widget_snippets.created_at',
               count(*) FILTER (WHERE created_at IS NULL) FROM widget_snippets
        UNION ALL SELECT 'tablet_sessions.interaction_count',
               count(*) FILTER (WHERE interaction_count IS NULL) FROM tablet_sessions
        UNION ALL SELECT 'tablet_sessions.created_at',
               count(*) FILTER (WHERE created_at IS NULL) FROM tablet_sessions
        UNION ALL SELECT 'loyalty_reward_tiers.requires_approval',
               count(*) FILTER (WHERE requires_approval IS NULL) FROM loyalty_reward_tiers
        UNION ALL SELECT 'loyalty_reward_tiers.sort_order',
               count(*) FILTER (WHERE sort_order IS NULL) FROM loyalty_reward_tiers
        UNION ALL SELECT 'custom_domains.is_active',
               count(*) FILTER (WHERE is_active IS NULL) FROM custom_domains
        UNION ALL SELECT 'custom_domains.created_at',
               count(*) FILTER (WHERE created_at IS NULL) FROM custom_domains
        UNION ALL SELECT 'custom_domains.updated_at',
               count(*) FILTER (WHERE updated_at IS NULL) FROM custom_domains
    ) AS e
    WHERE e.n > 0;

    IF bad IS NOT NULL THEN
        RAISE EXCEPTION 'surface nullability: refusing to SET NOT NULL - NULL rows present: %', bad;
    END IF;
END $$;

ALTER TABLE widget_snippets ALTER COLUMN is_active SET NOT NULL;
ALTER TABLE widget_snippets ALTER COLUMN created_at SET NOT NULL;
ALTER TABLE tablet_sessions ALTER COLUMN interaction_count SET NOT NULL;
ALTER TABLE tablet_sessions ALTER COLUMN created_at SET NOT NULL;
ALTER TABLE loyalty_reward_tiers ALTER COLUMN requires_approval SET NOT NULL;
ALTER TABLE loyalty_reward_tiers ALTER COLUMN sort_order SET NOT NULL;
ALTER TABLE custom_domains ALTER COLUMN is_active SET NOT NULL;
ALTER TABLE custom_domains ALTER COLUMN created_at SET NOT NULL;
ALTER TABLE custom_domains ALTER COLUMN updated_at SET NOT NULL;
