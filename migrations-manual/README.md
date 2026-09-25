# migrations-manual/ — what these files are, and when (if ever) to run them

These four `.sql` files are **not** part of the migration chain. The startup runner
(`src/db/migrations.rs`) applies **only** `migrations/*.sql` and records each filename in
`_migrations`; this directory was deliberately excluded in commit `7e310e5e` because the files
were hand-run against production once and are not re-runnable in their original form. Until now
nothing referenced them, which is why a from-zero install came up data-empty (kanban
**t_7451fc99**).

**Do not run these by hand any more.** Their product-structure content is now a real, guarded
migration: `migrations/20260925_seed_catalog_rows.sql`. The files are kept as the provenance
record of how production got those rows.

| file | what it did | still needed? |
|---|---|---|
| `insert_surface_features.sql` | inserted 5 `features` rows with the `surface_*` keys (category `surface`) | **No** — absorbed into `20260925_seed_catalog_rows.sql` §1 |
| `register_feature_keys.sql` | inserted 5 `features` rows with the short keys the code gates on (`custom_domains`, `tablet_mode`, `widget_embed`, `full_page`, `white_label`) **and** assigned them to the `enterprise` / `pro` tiers in `tier_features` | **No** — the feature rows are absorbed into `20260925_seed_catalog_rows.sql` §1. The tier assignment is **operator config** and is deliberately NOT seeded (see below). |
| `provider_keys.sql` | created the `provider_keys` table, created `available_providers` if absent, and inserted 6 `available_providers` rows | **Partially** — the 6 provider rows are absorbed into `20260925_seed_catalog_rows.sql` §2. The rest is stale. |
| `check_features.sql` | two read-only `SELECT`s used to eyeball the feature catalog | No — a query, not a migration |

## ⚠️ Hazard

`provider_keys.sql` begins with

```sql
DROP TABLE IF EXISTS provider_keys CASCADE;
```

Running it against a live database **drops every stored provider credential** and everything
cascading from that table. Both tables it touches already exist in the live schema (created by
`migrations/00010_integrations_and_redemption.sql`), so there is no reason to run this file at all.

## Post-install procedure for a fresh install (the part migrations must not do)

A fresh install builds the whole schema and — from `20260925_seed_catalog_rows.sql` — the product
catalog: `features` (30), `available_providers` (15), `email_templates` (58). Everything below is
**commercial configuration, owned by the operator, entered in the admin UI, never hardcoded:**

1. **Plans.** Sign in to the admin SPA (`app.incentiveswift.com`) → **Plans** → **+ Add Plan**.
   A fresh install has `plans = 0`; production seats three (`free`, `pro`, `enterprise`) with
   `payment_provider` `none` / `stripe` / `stripe`. `plan_tiers` follows the plan slugs (the app
   treats `plan_tiers.slug == plans.slug` as the join key).
2. **Tier entitlements.** `tier_features` starts empty: an absent row means *"feature not assigned
   to that tier"* (`access::feature_gate::has_feature_access`), i.e. upgrade required — the same
   state production is in for the surface gates. Assign per tier via
   `POST /api/v1/admin/plans/:id/features` (batch upsert; `enabled:true` adds, `false` removes) or
   the Plans/Tiers screens. `limit_value` convention (`features.rs`): `NULL` or `-1` = no cap,
   `0` = not available, positive = the cap.
   Reference (production, 2026-09-25): `enterprise` holds `all_mechanics` + 12 `mechanic_*` keys
   (13 rows), `pro` holds 11 `mechanic_*` keys, and **no tier holds a surface gate** — the
   `custom_domains` gate therefore currently reads "not assigned" on every plan.
3. **Payment/provider credentials.** Add each account's provider keys in the Integrations Center
   (`available_providers` is only the *catalog* of what may be configured; `provider_keys` holds
   the actual per-account rows). Nothing in this repo carries a real credential.
4. **Email templates.** The 58 rows from `migrations/20260820_email_templates_seed.sql` +
   `20260925_seed_catalog_rows.sql` are global defaults (`aid IS NULL`, `is_default = true`) and
   are per-account overridable; edit them in the UI, do not edit the SQL.
5. **`integration_targets` is not seeded on purpose.** Production's 6 rows are
   `webhook` / `https://example.com/hook` written under one test account — probe residue, not
   product structure.
