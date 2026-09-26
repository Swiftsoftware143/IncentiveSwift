-- t_8a896955 — loyalty_members.program_id / contact_id / member_since are decoded as
-- non-Option fields by every read site that touches a member row, so ONE NULL in ANY of the
-- three columns fails the WHOLE-ROW decode of every route that reads that row:
--   * struct LoyaltyMember   src/db/loyalty.rs:323 (+ 5 more sites with the same select list:
--                            handlers/loyalty.rs:584/750/875/913, handlers/auth_handler.rs:174)
--                            program_id: Uuid, contact_id: Uuid, member_since: DateTime<Utc>
--   * struct MemberInfo      src/handlers/loyalty_badges.rs:877 (same three fields)
--   * tuple (Uuid,Uuid,...)  src/handlers/loyalty_badges.rs:518 (scan_member)
--                            and :407 (get_member_qr, program_id only)
--   * struct LoyaltyMemberRow src/handlers/surface_handler.rs:63 (member_since only;
--                            its contact_id is already Option<Uuid>)
--
-- Measured on the deployed binary before this file (HEAD cdb9f9e0, sha256 59373537bc8db04c),
-- after first proving the state is REPRESENTABLE (both the INSERT and the UPDATE of each NULL
-- accepted, 3/3) and with one committed probe row per column plus an all-set CONTROL row:
--   POST /api/v1/loyalty/online/visit|share|referral-click  -> 500 per column (LoyaltyMember)
--   GET  /api/v1/loyalty/online/stats/<code>                -> 500 per column (LoyaltyMember)
--   POST /api/v1/auth/register (referral_code)              -> 500 per column (LoyaltyMember)
--   POST /api/v1/loyalty/rewards/<id>/approve               -> 500 per column (get_member)
--   GET  /api/v1/loyalty/dashboard/member/<id>              -> 500 per column (MemberInfo)
--   GET  /api/v1/play/<campaign>/dashboard                  -> 500 member_since (LoyaltyMemberRow)
--   POST /api/v1/loyalty/scan                               -> 500 for contact_id (tuple, "column 2")
-- with the literal `decoding column "<col>": unexpected null; try decoding as an `Option`` (the
-- tuple reports the position, `decoding column 2`) and the CONTROL row 200 on every one of them,
-- which is what makes the 500s attributable. A NULL program_id is NOT reachable at the three
-- sites that INNER JOIN loyalty_programs ON lp.id = lm.program_id (they 404 instead, measured).
--
-- ARM: SET NOT NULL (not Option<Uuid> / Option<DateTime<Utc>>). A membership without a program,
-- without a contact, or with no enrollment moment is not a state of this app:
--   * program_id and contact_id are FK -> loyalty_programs(id)/contacts(id) ON DELETE CASCADE
--     (a deleted parent DELETES the membership; the schema never SET NULLs either column), they
--     are the UNIQUE(program_id, contact_id) key of the table, and every read path either filters
--     on program_id or joins it.
--   * member_since has DEFAULT now(): an unset enrollment moment is the INSERT time, and
--     COALESCE(member_since, now()) at read time would invent a DIFFERENT timestamp per read.
-- Decoding Option<T> instead would force ~20 consumer sites (`.bind(member.program_id)`,
-- `&member.program_id.to_string()`, `&member.contact_id`, `member.member_since.to_rfc3339()`)
-- to invent an answer for a state no writer can produce, change the JSON of 8 routes, and leave
-- the other two columns of the same statements just as fatal. Zero Rust edits here: the existing
-- non-Option decode becomes correct.
--
-- CENSUS (why SET NOT NULL is honest — no writer can produce these NULLs):
--   * THREE writers exist in the crate (grep 'INSERT INTO loyalty_members' src/), and no
--     migration inserts into the table at all (grep -rl in migrations/ = 0 hits):
--       src/db/loyalty.rs:29              find_or_create_member(program_id: &Uuid, contact_id: &Uuid)
--                                         names both, non-Option params
--       src/handlers/auth_handler.rs:255  the referee membership: program_member.program_id
--                                         (a decoded non-Option field) + `cid` inside
--                                         `if let Some(cid)`, and it OMITS member_since => DEFAULT
--       src/mechanics/loyalty_checkin.rs:357  $1::uuid/$2::uuid binds + a literal now()
--   * the eleven `UPDATE loyalty_members` sites name points_balance / tier_id / qr_code /
--     qr_code_generated_at / last_activity_date only; `SET program_id|contact_id|member_since`
--     has 0 hits in src/.
--   * the table is empty today (0 rows, 0 NULLs in all three columns), and the audit that found
--     this class classified it LATENT: reachable only by direct SQL, which is exactly what this
--     constraint now refuses (loudly, at write time: 23502) instead of 500-ing a read.
--
-- The guard is the precondition check, not decoration: it counts the NULLs FIRST and the file is
-- executed as ONE batch (src/db/migrations.rs, sqlx::raw_sql => implicit transaction), so a
-- non-zero count refuses the whole file loudly instead of half-constraining the table. There is
-- nothing to backfill: a member with no program/contact cannot be derived from anything.

DO $$
DECLARE
    bad text;
BEGIN
    SELECT string_agg(x.col || '=' || x.n, ', ' ORDER BY x.col) INTO bad
    FROM (
        SELECT u.col AS col,
               (SELECT count(*) FROM loyalty_members m WHERE to_jsonb(m) ->> u.col IS NULL) AS n
        FROM unnest(ARRAY['program_id', 'contact_id', 'member_since']) AS u(col)
    ) AS x
    WHERE x.n > 0;

    IF bad IS NOT NULL THEN
        RAISE EXCEPTION 'loyalty_members: refusing to SET NOT NULL — NULL rows present: %', bad;
    END IF;
END $$;

ALTER TABLE loyalty_members ALTER COLUMN program_id SET NOT NULL;
ALTER TABLE loyalty_members ALTER COLUMN contact_id SET NOT NULL;
ALTER TABLE loyalty_members ALTER COLUMN member_since SET NOT NULL;
