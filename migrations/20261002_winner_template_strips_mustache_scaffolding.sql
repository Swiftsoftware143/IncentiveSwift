-- 20261002_winner_template_strips_mustache_scaffolding.sql
--
-- kanban t_c8df11e6 — the reachable `winner` email row mailed mustache scaffolding verbatim.
--
-- WHAT WAS WRONG
--   The global default row `template_type = 'winner'` is the row `handlers::entries` step 8 falls
--   back to for every `campaign_type` that has NO `{type}_winner` row, and its `html_body` stored
--   mustache conditional scaffolding around the prize sentence:
--
--     <h2>🎉 Congratulations {{first_name}}!</h2><p>You won the <b>{{campaign_name}}</b>
--     campaign.</p>{{#if prize_name}}<p>Prize: <b>{{prize_name}}</b></p>{{/if}}
--
--   `delivery::sender::render_template` substitutes ONLY `{{key}}`, and
--   `template_render::placeholder_re` is `\{([A-Za-z_][A-Za-z0-9_.]*)\}` — `#` and `/` are not
--   identifier characters — so the scaffolding was neither substituted NOR reported as an
--   unsubstituted placeholder: the recipient received the raw markers.
--
--   REACHABLE, measured live 2026-10-02: nine creatable mechanics (score_reveal, personality,
--   chat, leaderboard, loyalty, countdown, poll, b2b_loyalty, long_form_qualifier) have no
--   `{type}_winner` row, so a win on any of them falls back to this row. 0 production sends had
--   used it yet; the mechanism was live.
--
-- ARM (a), decided by measurement — STRIP THE MARKUP FROM THE STORED COPY, do not teach the
--   renderer conditionals:
--   * the variables are already bound UNCONDITIONALLY by the producer. `handlers::entries` step 8
--     always binds `prize_name` (`campaign.config.prize_name`, `""` when unset) together with
--     first_name/last_name/email/phone/campaign_name/campaign_type/entry_id, so unwrapping the
--     block loses nothing: a configured prize still reads `Prize: <b>…</b>`.
--   * `body` — the text twin `send_template_by_type` falls back to when `html_body` is absent —
--     already carried the prize sentence UNCONDITIONALLY, so unwrapping the HTML twin restores the
--     two columns to the same shape.
--   * arm (b) (implement mustache in the email renderer) would add a SECOND template language to
--     the product; `template_render` deliberately keeps ONE vocabulary and a loud warn instead
--     (kanban t_375c8c40, t_e43521d2). Conditional copy is a product feature and would need its
--     own card, its own field list and a both-ways probe.
--
-- SCOPE — global default rows only (`aid IS NULL AND is_default`): those ARE the platform's own
--   shipped copy. A TENANT row (`aid IS NOT NULL`) carrying markup is the tenant's own text; it is
--   NOT rewritten here. This card instead widened `template_render` so markup the renderer cannot
--   process is REPORTED loudly by name (it was previously invisible: neither substituted nor
--   warned), which covers tenant rows and campaign output-action templates too.
--
-- MEASURED BEFORE (live `incentiveswift`, 2026-10-02): exactly ONE row in the whole 32-row
--   catalogue carried mustache in either body column —
--     select template_type from email_templates
--      where coalesce(html_body,'') like '%{{#if%' or coalesce(body,'') like '%{{#if%';
--   -> winner
--   The `score_reveal` lifecycle seed (20260820_email_lifecycle_seed.sql) carried it too and had
--   already been retired by 20260927_retire_unreachable_email_templates.sql.
--
-- IDEMPOTENT: on a replay both UPDATEs match 0 rows (the guard columns no longer hold the
--   markers), so the second run is a no-op.
-- REVERSING: re-insert the row text from 20260925_seed_catalog_rows.sql § 3.

-- ── 1. name, in the boot log, exactly which shipped rows this rewrites ────────────────────────
-- A bare data UPDATE that silently rewrites customer-visible copy is unauditable; the NOTICE makes
-- the boot log carry the row identity, so an operator can see what changed and from how many rows.
DO $$
DECLARE r record; n int := 0;
BEGIN
    FOR r IN
        SELECT template_type, name FROM email_templates
         WHERE aid IS NULL AND is_default
           AND (coalesce(html_body, '') LIKE '%{{#%' OR coalesce(body, '') LIKE '%{{#%')
         ORDER BY template_type
    LOOP
        n := n + 1;
        RAISE NOTICE 't_c8df11e6: stripping unprocessable mustache scaffolding from global default email template % (%)', r.template_type, r.name;
    END LOOP;
    RAISE NOTICE 't_c8df11e6: global default email templates carrying mustache (before): % row(s)', n;
END $$;

-- ── 2. html_body: unwrap `{{#if <ident>}}` / `{{/if}}`, keeping the branch body ───────────────
-- `IS NOT NULL` is load-bearing: `html_body` being present IS the "this row is HTML" flag
-- (`send_template_by_type` prefers it), so a NULL column must stay NULL, never become ''.
UPDATE email_templates
   SET html_body = regexp_replace(
                       regexp_replace(html_body, '\{\{#if [A-Za-z_][A-Za-z0-9_.]*\}\}', '', 'g'),
                       '\{\{/if\}\}', '', 'g'),
       updated_at = now()
 WHERE aid IS NULL
   AND is_default
   AND html_body LIKE '%{{#%';

-- ── 3. body: the same treatment for the text twin (none today, class-complete) ────────────────
UPDATE email_templates
   SET body = regexp_replace(
                  regexp_replace(body, '\{\{#if [A-Za-z_][A-Za-z0-9_.]*\}\}', '', 'g'),
                  '\{\{/if\}\}', '', 'g'),
       updated_at = now()
 WHERE aid IS NULL
   AND is_default
   AND body LIKE '%{{#%';

-- ── 4. the invariant, asserted in the boot log (never silent, never a boot failure) ───────────
-- A migration that raises would be retried forever by the in-app runner (it records only on
-- success), so an unmet assertion is reported, not thrown: the widened `template_render` warn
-- catches any survivor on the wire, and the card's census script asserts it mechanically.
DO $$
DECLARE n int; names text;
BEGIN
    SELECT count(*), string_agg(DISTINCT template_type, ', ') INTO n, names
      FROM email_templates
     WHERE aid IS NULL AND is_default
       AND (coalesce(html_body, '') LIKE '%{{#%' OR coalesce(html_body, '') LIKE '%{{/%'
            OR coalesce(body, '') LIKE '%{{#%' OR coalesce(body, '') LIKE '%{{/%');
    RAISE NOTICE 't_c8df11e6: global default email templates still carrying mustache (after): % row(s) [%]', n, coalesce(names, 'none');
END $$;
