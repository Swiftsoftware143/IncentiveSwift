-- Restore the daily check-in cap's database guard, which 00001_full_schema.sql never created.
--
-- The original statement was
--     create unique index loyalty_checkins_daily_cap
--         on public.loyalty_checkins (member_id, (checked_in_at::date))
-- and it never ran: checked_in_at::date is only STABLE (it resolves under the session
-- TimeZone), and an index expression must be IMMUTABLE, so the statement failed with
-- `functions in index expression must be marked IMMUTABLE` while the rest of its file
-- succeeded. The filename was still recorded in _migrations, so it was never retried.
-- Card t_47c00655.
--
-- WHY THIS IS NOT A RE-ISSUE OF THE ORIGINAL UNIQUE INDEX, even with an IMMUTABLE expression:
--
--   1. A unique index on (member_id, day) hard-codes a cap of ONE check-in per member per
--      day. The cap is configurable per program and the only ACTIVE program on this database
--      (loyalty_programs, "ZaarHub Rewards") has max_checkins_per_day = 5, which
--      src/mechanics/loyalty_checkin.rs:33-38 applies as `today_count >= max_checkins_per_day`.
--      A unique index would make the 2nd legitimate check-in of the day raise a unique
--      violation, turning the endpoint's friendly HTTP 200 {"status":"daily_cap_reached"} into
--      an HTTP 500. That is a regression, not a guard.
--   2. src/mechanics/loyalty_checkin.rs::process_checkin_from_entry deliberately does NOT
--      apply the daily cap ("entry is already gated") and writes rows to this same table, so a
--      blanket table-level cap would newly refuse campaign spin/raffle check-ins the product
--      intends to allow.
--
-- So the guard is a BEFORE INSERT trigger that reads the member's own program cap and refuses
-- only the (cap + 1)-th check-in of the day, skipping entry-gated rows (entry_id is not null).
-- That mirrors the two application paths exactly.
--
-- DAY BOUNDARY: the UTC day, explicitly. The application counts with
-- `checked_in_at::date = CURRENT_DATE`, STABLE because it resolves under the session TimeZone.
-- This database's TimeZone is UTC (verified live), so the explicit UTC day is the same
-- boundary on both sides -- and it is IMMUTABLE, so the supporting index below can actually
-- serve the guard's count. Pinning both sides of the comparison to one explicit UTC expression
-- means the guard cannot drift with a session variable.

-- Supporting index for the guard's count (and for any member-scoped check-in query). It is
-- deliberately NOT unique -- see reason 1 above. The expression is IMMUTABLE, which is the one
-- thing the original statement got wrong.
--
-- FRESH-BUILD NOTE (kanban t_ab9a342a, 2026-09-26): `00001_full_schema.sql` now creates this
-- index in its NON-unique form too, so `IF NOT EXISTS` below is a no-op on a from-zero build AND
-- on live, and the two agree (`pg_get_indexdef` on both sides). Before that edit a database built
-- from zero got the UNIQUE form from 00001 (its original statement failed on live while the old
-- error-swallowing runner recorded the filename anyway) and this file created the non-unique one
-- only where that statement had failed. If you ever need to change this index's shape, change it
-- in `00001_full_schema.sql` -- both files are already recorded in `_migrations`, so a change to
-- either reaches fresh builds only.
CREATE INDEX IF NOT EXISTS loyalty_checkins_daily_cap
    ON public.loyalty_checkins (member_id, (timezone('UTC', checked_in_at)::date));

CREATE OR REPLACE FUNCTION public.loyalty_checkins_daily_cap_guard()
RETURNS trigger
LANGUAGE plpgsql
AS $cap$
DECLARE
    cap  integer;
    seen integer;
BEGIN
    -- Entry-gated check-ins (campaign spin/raffle) bypass the cap by design, and a row with
    -- no member cannot be scoped to a program.
    IF NEW.entry_id IS NOT NULL OR NEW.member_id IS NULL THEN
        RETURN NEW;
    END IF;

    SELECT p.max_checkins_per_day
      INTO cap
      FROM public.loyalty_members m
      JOIN public.loyalty_programs p ON p.id = m.program_id
     WHERE m.id = NEW.member_id;

    -- No program resolved: nothing to enforce, so do not invent a cap.
    IF NOT FOUND THEN
        RETURN NEW;
    END IF;

    -- Column default is 1 (00001_full_schema.sql).
    cap := COALESCE(cap, 1);

    -- Serialise concurrent inserts for this member. The lock is held until the inserting
    -- transaction commits, so the loser of a race counts the winner's row and is refused
    -- here instead of slipping through -- which is the race the missing guard left open.
    PERFORM pg_advisory_xact_lock(
        hashtext('loyalty_checkins_daily_cap:' || NEW.member_id::text));

    SELECT count(*)
      INTO seen
      FROM public.loyalty_checkins c
     WHERE c.member_id = NEW.member_id
       AND timezone('UTC', c.checked_in_at)::date
           = timezone('UTC', COALESCE(NEW.checked_in_at, now()))::date;

    IF seen >= cap THEN
        RAISE EXCEPTION
            'loyalty_checkins_daily_cap: member % already has % check-in(s) on the UTC day, max_checkins_per_day = %',
            NEW.member_id, seen, cap
            USING ERRCODE = 'check_violation';
    END IF;

    RETURN NEW;
END
$cap$;

DROP TRIGGER IF EXISTS loyalty_checkins_daily_cap_guard ON public.loyalty_checkins;

CREATE TRIGGER loyalty_checkins_daily_cap_guard
    BEFORE INSERT ON public.loyalty_checkins
    FOR EACH ROW
    EXECUTE FUNCTION public.loyalty_checkins_daily_cap_guard();
