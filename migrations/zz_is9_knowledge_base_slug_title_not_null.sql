-- IncentiveSwift — knowledge_base.slug and .title become NOT NULL.
--
-- WHY (kanban t_56bb9bb9): the fleet decode-type scanner reported this table's two text columns as
-- NULLABLE while every reader decodes them into a non-Option `String`:
--   * src/handlers/knowledge_base_handler.rs:390  the LIST_SQL tuple  (slug, title)
--   * src/handlers/knowledge_base_handler.rs:113  the by-slug tuple   (title)
-- A NULL in either column makes sqlx fail the WHOLE-ROW decode ("unexpected null; try decoding as
-- an Option"), so ONE bad row would 500 the reader instead of answering the other articles.
--
-- MEASURED BEFORE THIS FILE (live, incentiveswift): knowledge_base holds 10 rows, 0 NULLs in
-- `slug`, 0 NULLs in `title` -> LATENT today. The WRITER CENSUS says no code path can produce the
-- state:
--   * the only INSERT site (knowledge_base_handler::create_article) binds `slugify(...)`, which is
--     non-empty by construction (`format!("article-{}", ..)` fallback), and a title it has already
--     rejected as empty with a 400;
--   * the only UPDATE site (update_article) does not name `slug` at all, and writes
--     `title = COALESCE($3, title)` with a non-Option bind;
--   * no migration inserts a row (019_fix_phantom_tables.sql created the table empty;
--     20261002_knowledge_base_two_sided.sql only ADDs the columns).
-- `title` is ALREADY unrepresentable as NULL: knowledge_base_title_check is
-- `title IS NOT NULL AND length(btrim(title)) > 0`, so a direct NULL write is refused today (23514).
-- `slug` is not: a direct NULL write is ACCEPTED today and would 500 the reader (measured on a copy
-- of live, both ways, in /opt/swift/audits/t_56bb9bb9/).
--
-- SO THE ARM IS THE SCHEMA, NOT THE DECODE: SET NOT NULL makes the latent state unrepresentable,
-- clears the scanner rows for EVERY site that decodes these columns (both tuples above — a COALESCE
-- inside LIST_SQL would have left :113 reachable), leaves the Rust untouched and the JSON
-- byte-identical for every non-NULL row. A future writer that omits `slug` now fails loudly (23502)
-- instead of silently planting a row that breaks the reader.
--
-- GUARD: the file refuses if any NULL is present and names each column's count, so it can never
-- half-constrain the table. The ALTERs themselves are idempotent (a no-op once the column is set).
--
-- The partial unique index `knowledge_base_audience_slug_idx ... WHERE slug IS NOT NULL` and the
-- INSERT's `ON CONFLICT (audience, slug) WHERE slug IS NOT NULL` inference keep working: every row
-- now satisfies the index predicate, so the predicate still selects the same index (proved live by
-- the create + duplicate-create legs).

DO $$
DECLARE
    n_slug  integer;
    n_title integer;
BEGIN
    SELECT count(*) INTO n_slug  FROM knowledge_base WHERE slug  IS NULL;
    SELECT count(*) INTO n_title FROM knowledge_base WHERE title IS NULL;
    IF n_slug > 0 OR n_title > 0 THEN
        RAISE EXCEPTION
            'refusing SET NOT NULL — NULL rows present: knowledge_base.slug=% knowledge_base.title=%',
            n_slug, n_title;
    END IF;
END $$;

ALTER TABLE knowledge_base ALTER COLUMN slug  SET NOT NULL;
ALTER TABLE knowledge_base ALTER COLUMN title SET NOT NULL;
