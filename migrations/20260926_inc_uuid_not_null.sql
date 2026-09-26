-- t_d6e55678 — IncentiveSwift, the app-side half of the t_91c53522 widening (`rkey()` now strips a
-- leading module path, so a struct field spelled `uuid::Uuid` is judged exactly like a bare `Uuid`).
-- Seven rows became visible, all of them a NULLABLE uuid column decoded into a NON-Option struct
-- field: ONE NULL in the column fails the WHOLE-ROW decode of the query that reads it. Live NULL
-- count measured 0 for every one of the six columns (10-census.txt) => LATENT, not ACTIVE, so this
-- is the t_aefe745a arm.
--
--   file:line              column                            table                   struct            line in this file
--   src/db/delivery_log.rs:55  entry_id                       delivery_log            DeliveryLogEntry  SET NOT NULL
--   src/db/entries.rs:134      contact_id                     entries                 EntryWithCampaign SET NOT NULL
--   src/db/entries.rs:134      campaign_id                    entries                 EntryWithCampaign SET NOT NULL
--   src/db/loyalty.rs:191      program_id                     loyalty_reward_tiers    RewardTier        SET NOT NULL
--   src/db/loyalty.rs:307      program_id (2nd query site)    loyalty_reward_tiers    RewardTier        ^ same column
--   src/db/loyalty.rs:264      tier_id                        loyalty_rewards_earned  RewardEarned       SET NOT NULL
--   src/db/loyalty.rs:264      member_id                      loyalty_rewards_earned  RewardEarned       NO — Option<Uuid> in Rust
--
-- ARM, decided per column from the writer census (`10-census.txt`) and not by taste. The tiebreaker
-- is whether any writer can PRODUCE the NULL:
--
--   * delivery_log.entry_id — one INSERT (src/db/delivery_log.rs:33) plus three in
--     src/delivery/integration_hub.rs, every one binding a `Uuid` (never an Option); the reader takes
--     `entry_id: &Uuid`. A delivery log row with no entry is not a state of this app; FK is
--     ON DELETE CASCADE, never SET NULL.
--   * entries.contact_id / entries.campaign_id — FIVE INSERT sites (src/db/entries.rs:55,
--     src/db/raffles.rs:52, src/mechanics/prize_draw.rs:440/:501,
--     src/mechanics/milestone_engine.rs:227, src/handlers/quiz_handler.rs:231,
--     src/handlers/sms_handler.rs:659) and every bound value is a `Uuid`: the input struct's fields
--     are `Uuid` (src/db/entries.rs:11-12), the mechanics take `contact_id: &Uuid`/`campaign_id: &Uuid`,
--     and the two ad-hoc ones come from `query_scalar::<_, Uuid>(..).fetch_one(..)` (+ `Uuid::new_v4()`
--     in the sms branch). Both FKs are ON DELETE CASCADE.
--   * loyalty_reward_tiers.program_id — exactly two writers: the INSERT (src/handlers/loyalty.rs:435)
--     binds `Uuid::parse_str(&body.program_id)?` (a bad id is a 400 BEFORE the write), and the only
--     UPDATE (src/handlers/loyalty.rs:472) does not name program_id at all. FK ON DELETE CASCADE.
--   * loyalty_rewards_earned.tier_id — three writers (src/db/loyalty.rs:223 `tier_id: &Uuid`,
--     src/mechanics/loyalty_checkin.rs:476 `$2::uuid`, src/handlers/loyalty_v2.rs:833 `tier.0: Uuid`),
--     all non-Option. FK is NO ACTION (confdeltype 'a'), so no delete arm can NULL it either.
--   * loyalty_rewards_earned.member_id — DELIBERATELY nullable: src/handlers/loyalty_v2.rs:821 binds
--     `Option<Uuid>` and its own comment says why ("a contact that earned campaign points without ever
--     enrolling has no member row, and member_id is nullable, so the redemption is still recorded").
--     SET NOT NULL would turn that live redemption into a 23502/500, so this column takes the OTHER
--     arm: `Option<uuid::Uuid>` in the struct (src/db/loyalty.rs RewardEarned, same commit) with
--     approve_reward handling the None explicitly. Nothing here changes its schema.
--
-- SET NOT NULL costs ZERO Rust edits for the other five: the existing non-Option decode becomes
-- correct, every response JSON stays byte-identical for the non-NULL rows that exist, and a future
-- NULL writer fails LOUDLY at write time (23502, naming the column) instead of 500-ing a read.
--
-- Reachability of the decode sites, measured (30-before.txt / 32-reachability.txt): tier_id and
-- member_id are HOT through POST /api/v1/loyalty/rewards/:id/{approve,deny} (db/loyalty.rs
-- get_reward -> RewardEarned, the only caller of it in the crate), program_id is HOT through the same
-- approve route (get_reward_tier -> RewardTier), and the two `entries` columns are NOT decodable from
-- a NULL row at all - the only reader (entries::get_entries_for_contact, called by
-- GET /api/v1/contacts/:id) filters `WHERE e.contact_id = $1` and INNER JOINs campaigns, so a NULL in
-- either column makes the row INVISIBLE (silent history loss) rather than 500. delivery_log.entry_id's
-- reader (DeliveryLogEntry) is dead code - zero callers (see the opt-in test in src/db/delivery_log.rs)
-- - so its leg is judged on the schema, both ways, not on a route.
--
-- The guard is the precondition check, not decoration: it counts the NULLs FIRST and the whole file
-- runs as ONE batch (src/db/migrations.rs, sqlx::raw_sql => implicit transaction), so a non-zero count
-- refuses the file loudly instead of half-constraining a table. There is nothing to backfill: a
-- delivery-log row with no entry, a lead entry with no contact/campaign, a reward tier with no
-- program, or an earned reward with no tier is not a state of this app.

DO $$
DECLARE
    bad text;
BEGIN
    SELECT string_agg(e.col || '=' || e.n, ', ' ORDER BY e.col) INTO bad
    FROM (
        SELECT 'delivery_log.entry_id' AS col,
               count(*) FILTER (WHERE entry_id IS NULL) AS n FROM delivery_log
        UNION ALL SELECT 'entries.contact_id',
               count(*) FILTER (WHERE contact_id IS NULL) FROM entries
        UNION ALL SELECT 'entries.campaign_id',
               count(*) FILTER (WHERE campaign_id IS NULL) FROM entries
        UNION ALL SELECT 'loyalty_reward_tiers.program_id',
               count(*) FILTER (WHERE program_id IS NULL) FROM loyalty_reward_tiers
        UNION ALL SELECT 'loyalty_rewards_earned.tier_id',
               count(*) FILTER (WHERE tier_id IS NULL) FROM loyalty_rewards_earned
    ) AS e
    WHERE e.n > 0;

    IF bad IS NOT NULL THEN
        RAISE EXCEPTION 'inc uuid nullability: refusing to SET NOT NULL - NULL rows present: %', bad;
    END IF;
END $$;

ALTER TABLE delivery_log            ALTER COLUMN entry_id   SET NOT NULL;
ALTER TABLE entries                 ALTER COLUMN contact_id SET NOT NULL;
ALTER TABLE entries                 ALTER COLUMN campaign_id SET NOT NULL;
ALTER TABLE loyalty_reward_tiers    ALTER COLUMN program_id SET NOT NULL;
ALTER TABLE loyalty_rewards_earned  ALTER COLUMN tier_id    SET NOT NULL;
