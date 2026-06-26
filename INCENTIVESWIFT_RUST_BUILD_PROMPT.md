# IncentiveSwift — Rust Build Prompt
# Version 1.0 — for OpenClaw/DeepSeek code generation
# Stack: Rust (Axum) backend + Supabase (Postgres + RLS) + Railway hosting
# Frontend: React/Next.js (unchanged from prior architecture) — only the backend API changes

---

## WHY THIS STACK

This is a multi-tenant SaaS capture engine that needs to survive traffic spikes
(a viral spin-wheel campaign, a live raffle draw event) without falling over, and
needs to run cheaply and predictably on a rented compute budget.

**Framework choice: Axum.** Chosen over Actix-web for smaller, more predictable
surface area, built directly on Tokio + Tower. Easier for LLM code-generation to
produce correctly — fewer macro-heavy patterns, more straightforward function
signatures.

**Hosting: Railway.** Natively builds and runs a Dockerfile-based Rust service,
handles TLS/HTTPS, zero-downtime deploys, and autoscales.

**Database: Supabase** (Postgres + RLS), exactly as already architected.
Only the API layer changes from Next.js API routes to a Rust service.

---

## SCOPE BOUNDARY — DO NOT EXCEED

- It runs the 12 capture mechanics plus Loyalty Program module
- It keeps a light CRM (contacts, entries, questions, answers) — read/audit only
- It pushes captured entries outward via webhook or direct API call
- It NEVER calls another app's API to request an action
- It NEVER shares a database with anything downstream
- It does NOT run pipelines, deal stages, sales automation, or follow-up sequences

## SECURITY — NON-NEGOTIABLE

### Headers (Tower middleware, global)
- CSP: default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'
- connect-src 'self' https://*.supabase.co; frame-ancestors 'none'; form-action 'self'
- X-Frame-Options: DENY
- X-Content-Type-Options: nosniff
- Referrer-Policy: strict-origin-when-cross-origin
- Strict-Transport-Security: max-age=63072000; includeSubDomains; preload

### API Key Auth
- NEVER store or compare plaintext API keys
- On creation: bcrypt::hash(key, bcrypt::DEFAULT_COST)
- On request: bcrypt::verify(provided_key, &stored_hash)
- Do NOT hash the incoming key and do a direct equality lookup

### Formula Evaluation (calculator mechanic)
- NEVER pass admin-entered formulas to eval(), Command::new(), or any scripting engine
- Use a restricted-grammar parser supporting only + - * / ( ) and named variables
- Claude Code review pass must grep for any eval/Command/scripting engine usage

### Rate Limiting
- governor token bucket, Tower middleware (not ad-hoc per handler)
- Public routes: 20 req/min/IP
- Authenticated routes: 100 req/min

### Raffle Compliance
- official_rules_url required for any raffle campaign creation
- consent_gathered must be explicit true in entry body
- random_seed stored permanently, never overwritten

### Secrets
- api_credentials.key_encrypted: AES-encrypted with server-side key from env
- Decrypt only at moment of outbound direct-api call
- Never log decrypted values

## TESTING REQUIREMENTS

1. **payload_contract_test.rs** — Serialize DeliveryPayload, assert exact JSON shape
2. **entries_test.rs** — POST /entries with mocked Supabase, assert contact dedup works
3. **raffle_draw_test.rs** — Fixed random_seed produces reproducible winner selection
4. **feature_gate_test.rs** — Account without mechanic_raffle gets 403 on campaign create

## CLAUDE CODE REVIEW CHECKLIST

1. Does `questions_and_answers` get built from normalized `answers`+`questions` join?
2. Any code path where formula/webhook URL config reaches eval/Command/scripting engine?
3. Are API keys compared via bcrypt::verify, not direct hash equality?
4. Is has_feature_access() checked at handler level on every gated route?
5. Does raffle draw always use stored random_seed?
6. Is loyalty daily-cap check a real DB query (not in-memory counter)?
7. Any panic path (unwrap/expect) on request-handling code paths?
8. Are rate limits applied as global middleware?
