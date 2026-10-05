-- t_e8faac56 — a loyalty programme had NO owner of its own.
--
-- `loyalty_programs` has no account column, so the only tenancy link was a campaign (either
-- pointer). MEASURED LIVE 2026-10-04 on binary df85e5f4c8befac1:
--   * the console's "+ Add Program" sends NO campaign_id at all
--     (www-admin/index.html: save({name,currency_name,currency_icon,currency_color,
--     points_per_checkin,...})), so EVERY programme the console creates is campaign-less;
--   * the live ZaarHub programme is campaign-less too (it is owned through the FORWARD pointer,
--     campaigns.loyalty_program_id, written by the campaign editor);
--   * `list_programs` covered that shape with `OR lp.campaign_id IS NULL`, an arm that handed
--     EVERY authenticated account every campaign-less programme — a probe account and the anchor
--     account both listed the ZaarHub programme.
--
-- That arm cannot stay (it is the arm that would hand out a slug the caller does not own), but a
-- strictly campaign-scoped read would make a console-created programme VANISH from its own
-- creator's list. So the owner becomes explicit here: `create_program` writes it from the
-- authenticated caller (kanban t_e8faac56), and this file recovers it for the rows that predate
-- it — the FORWARD pointer first, because that is the canonical link every loyalty path reads
-- (mechanics/loyalty_checkin.rs::program_campaign_id), then the historical reverse one.
--
-- A row owned by neither keeps account_id NULL and is visible to nobody. That is honest: with no
-- campaign it has no check-in link of its own that can resolve (public_program answers "That
-- program is not linked to a campaign"), so there is no tenant it could be handed to.
--
-- Idempotent (IF NOT EXISTS; the backfill touches NULLs only) because this app's boot runner
-- re-reads ./migrations on every boot and applies only the files it has not recorded, in filename
-- order.
ALTER TABLE IF EXISTS public.loyalty_programs ADD COLUMN IF NOT EXISTS account_id uuid;

-- 1. the canonical forward pointer: campaigns.loyalty_program_id -> the programme
UPDATE public.loyalty_programs p
   SET account_id = c.account_id
  FROM public.campaigns c
 WHERE c.loyalty_program_id = p.id
   AND p.account_id IS NULL;

-- 2. the historical reverse pointer: loyalty_programs.campaign_id -> campaigns.account_id
UPDATE public.loyalty_programs p
   SET account_id = c.account_id
  FROM public.campaigns c
 WHERE c.id = p.campaign_id
   AND p.account_id IS NULL;

CREATE INDEX IF NOT EXISTS loyalty_programs_account_id_idx
    ON public.loyalty_programs (account_id);
