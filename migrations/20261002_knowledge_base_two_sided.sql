-- IncentiveSwift — the knowledge base becomes a REAL, two-sided feature.
--
-- David, 2026-10-02: *"Every software should have a knowledge base for the admin, for the users and
-- they should be in line respectively. So that way users can be walked through. And then in FunnelSwift
-- obviously there's going to be affiliate guide."*
--
-- Measured before this file: `knowledge_base` held 0 rows, had ZERO code references and ZERO routes
-- (`GET /api/v1/knowledge-base` answered 404), and no served screen mentioned it. It was created by
-- `019_fix_phantom_tables.sql` — a table built to satisfy a schema expectation, never a feature.
--
-- The shape was, however, the right starting point: `id, tenant_id, title, content, created_at`, with
-- `tenant_id` NULLABLE and no foreign keys. What it lacked is everything that makes a knowledge base
-- two-sided and walkable:
--   * `audience`   — WHICH side an article belongs to. David asked for an admin side and a user side
--                    "in line respectively"; without this column there is one undifferentiated pile.
--   * `slug`       — a stable link target so an article can be linked and deep-linked.
--   * `category`   — the walkthrough grouping shown in the reader.
--   * `sort_order` — the order a reader walks through, instead of "whatever the planner returns".
--   * `is_published` — a draft an operator can write without it appearing to users.
--   * `updated_at` — when it last changed; a reader wants to know if it is current.
--
-- `tenant_id` STAYS NULLABLE and stays the override hook: NULL means platform-wide content every tenant
-- reads (the walkthrough that ships with the product), a row with a tenant_id is that tenant's own
-- article. The reader prefers the tenant's row and falls back to the platform one.

ALTER TABLE knowledge_base ADD COLUMN IF NOT EXISTS audience     text        NOT NULL DEFAULT 'user';
ALTER TABLE knowledge_base ADD COLUMN IF NOT EXISTS slug         text;
ALTER TABLE knowledge_base ADD COLUMN IF NOT EXISTS category     text;
ALTER TABLE knowledge_base ADD COLUMN IF NOT EXISTS sort_order   integer     NOT NULL DEFAULT 100;
ALTER TABLE knowledge_base ADD COLUMN IF NOT EXISTS is_published boolean     NOT NULL DEFAULT true;
ALTER TABLE knowledge_base ADD COLUMN IF NOT EXISTS updated_at   timestamptz NOT NULL DEFAULT now();

-- One of exactly two sides. A typo must not be able to create a third audience that no reader asks for.
ALTER TABLE knowledge_base DROP CONSTRAINT IF EXISTS knowledge_base_audience_check;
ALTER TABLE knowledge_base ADD  CONSTRAINT knowledge_base_audience_check
  CHECK (audience IN ('admin', 'user'));

-- A title is the one thing a reader must see; content may legitimately start empty as a draft.
ALTER TABLE knowledge_base DROP CONSTRAINT IF EXISTS knowledge_base_title_check;
ALTER TABLE knowledge_base ADD  CONSTRAINT knowledge_base_title_check
  CHECK (title IS NOT NULL AND length(btrim(title)) > 0);

-- Platform-wide articles have slug set; two articles can share a slug only across the two audiences
-- (the admin's "Getting started" and the user's "Getting started" are different pages).
CREATE UNIQUE INDEX IF NOT EXISTS knowledge_base_audience_slug_idx
  ON knowledge_base (audience, slug) WHERE slug IS NOT NULL;

-- The reader's own query: one audience, in walking order.
CREATE INDEX IF NOT EXISTS knowledge_base_audience_order_idx
  ON knowledge_base (audience, sort_order);

COMMENT ON TABLE knowledge_base IS
  'Two-sided help content: audience ''admin'' is walked through by the account holder (rendered in the admin console and at /admin/guide.html), audience ''user'' is walked through by the end user (rendered at /guide.html). tenant_id NULL = platform-wide shipped content; a tenant_id row is that tenant''s own article and wins for that tenant. Read by handlers::knowledge_base_handler; authored in the console''s Knowledge Base view.';
COMMENT ON COLUMN knowledge_base.audience IS
  'Which side of the product this article walks through: ''admin'' (account holder) or ''user'' (end user). Constrained to those two values.';
