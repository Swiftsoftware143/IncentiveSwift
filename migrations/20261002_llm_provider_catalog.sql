-- 20261002_llm_provider_catalog.sql — seat the LLM providers the Chat Funnel's LLM arm reads.
-- (kanban t_f936aaab)
--
-- WHY THIS FILE EXISTS
--   src/handlers/chat_handler.rs::resolve_llm_key resolves a per-account credential from
--   provider_keys for provider IN ('deepseek','openai') (deepseek preferred) and uses its
--   api_key + base_url for the chat mechanic's LLM enrichment. The served Integrations screen
--   (www-app/integrations.html) renders ONE card per available_providers row and connects it
--   through POST /api/v1/provider-keys, and that route validates `provider` against this same
--   catalogue (provider_keys_handler.rs reads SELECT 1 FROM available_providers WHERE key = $1).
--   Neither deepseek nor openai was seated, so NO served surface could create the value the code
--   reads: Connect answered 400 Unknown provider and every chat campaign ran the scripted
--   fallback. Measured live 2026-09-26 (card t_f3c75b2a, evidence
--   /opt/swift/audits/t_f3c75b2a/94-adjacent-llm-provider.txt): 15 catalogue keys, no LLM one.
--
-- WHAT THIS SEATS, AND WHAT IT DELIBERATELY DOES NOT
--   Both providers keep their vendor endpoint as the working default when base_url is empty
--   (chat_handler.rs uses https://api.deepseek.com/chat/completions and
--   https://api.openai.com/v1/chat/completions), so requires_base_url = false and NO
--   integration_provider_presets row is seated here: the vendor endpoints are public and need no
--   carve-out. A tenant-supplied base_url is gated at write time AND at every use (t_f3c75b2a):
--   only a public destination is admitted unless it equals the platform preset for that provider,
--   so a self-hosted private relay stays refused until the platform seats a preset for it — that
--   is a platform decision, not a catalogue one.
--
-- IDEMPOTENT: the runtime runner applies each file once (filename-keyed in _migrations) and
--   ON CONFLICT DO NOTHING makes a re-run a no-op.
INSERT INTO available_providers (key, name, description, requires_base_url, requires_metadata, icon)
VALUES
  ('deepseek', 'DeepSeek', 'LLM replies for the Chat Funnel mechanic (preferred)', false, '[]'::jsonb, 'sparkles'),
  ('openai',   'OpenAI',   'LLM replies for the Chat Funnel mechanic (fallback)',  false, '[]'::jsonb, 'sparkles')
ON CONFLICT (key) DO NOTHING;
