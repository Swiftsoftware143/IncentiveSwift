# IncentiveSwift — Full Handoff for Linux Agent

## Overview
IncentiveSwift is a single-purpose engagement & capture engine written in Rust (Axum). It runs 12 gamified mechanics (spin wheel, scratch card, personality quiz, calculator, mystery reveal, countdown, poll, chat funnel, benchmark/leaderboard, score reveal, raffle/giveaway, long-form qualifier) plus an optional Loyalty Program module.

**This is NOT a CRM.** It captures contacts, scores them, applies a tag, and pushes the full payload outward via webhook — then it's done. Pipeline, campaign, and follow-up logic belongs to whatever sits downstream.

### The One-Way Flow
```
IncentiveSwift → (webhook or direct API) → FunnelSwift → Third-Party CRM
                                          → OR direct to CRM
```
- IncentiveSwift never calls upstream apps
- No shared databases — ever
- Each app has its own Supabase project

### FunnelSwift Affiliate Integration
IncentiveSwift is an **affiliate product** in FunnelSwift — same pattern as WorkflowSwift and MissedCallResponder:
- Each plan tier gets a FunnelSwift tag for affiliate routing/commission tracking
- FunnelSwift webhook is the primary delivery target (configurable per campaign)
- Direct CRM API delivery is also available as a bypass for users without FunnelSwift

---

## Repository
- **GitHub:** `Swiftsoftware204/IncentiveSwift`
- **Branch:** `main`
- **Token:** Same as other SwiftSoftware repos (`ghp_1NwT...`)
- **Local path:** `C:\Users\Administrator\.openclaw\instances\swiftsoftware\workspace\incentiveswift`

## Supabase Project
- **Org:** `Swiftsoftware204` (same as FunnelSwift, WorkflowSwift)
- **Project:** Needs to be created via Supabase dashboard
- **Service role key:** Use the Swiftsoftware204 Supabase org to create a new project
- **Database:** Postgres — run all migrations from `migrations/` directory
- **Stack:** Supabase (Postgres + RLS) — same pattern as FunnelSwift

---

## Architecture (from provided spec)

### Database Schema (migrations/)
The full SQL schema is in `migrations/00001_full_schema.sql` — includes:
- `contacts` — light CRM, one row per person, dedup by email/phone
- `campaigns` — 12 mechanic types, tag namespace, outcome_tags, delivery config
- `entries` — every capture across all mechanics
- `questions` / `answers` — normalized Q&A storage
- `delivery_log` — audit trail for webhook/API pushes
- `plan_tiers` / `features` / `tier_features` — admin-configurable plan system
- `accounts` — customer accounts linked to plan tiers
- `api_credentials` — hashed API keys (bcrypt)
- `loyalty_programs` / `loyalty_members` / `loyalty_checkins` / `loyalty_reward_tiers` / `loyalty_rewards_earned` — loyalty module (upsell)
- RLS policies on all tables

### 12 Mechanic Types (campaign.type)
1. `score_reveal` — animated score + tier message
2. `spin_wheel` — weighted prize wheel
3. `scratch_card` — canvas scratch-to-reveal
4. `personality` — outcome-type quiz, shareable result
5. `calculator` — formula-driven dollar estimate (SAFE eval — restricted arithmetic parser, NO eval/exec)
6. `mystery` — locked reward, unlocks on completion
7. `countdown` — urgency timer layered on any mechanic
8. `poll` — single-question vote + real aggregate results
9. `chat` — conversational bubble-style quiz
10. `leaderboard` — percentile benchmark vs aggregate data
11. `raffle` — delayed-draw entry, scheduled or live-triggered
12. `long_form_qualifier` — deep logic-based pre-qualification for high-ticket offers

### Loyalty Module (separate upsell)
- Recurring point-based check-in system
- Daily cap enforcement (DB-level, not in-memory)
- Reward tiers with auto or manual approval
- Gated behind `module_loyalty_program` feature flag

### Delivery — Two Paths
1. **Webhook (primary):** Push to FunnelSwift ingest endpoint or any webhook URL
2. **Direct API (bypass):** Push straight to HubSpot, ActiveCampaign, GoHighLevel, etc.

Configurable per campaign in `campaigns.delivery_method` and `campaigns.delivery_config`.

### Payload Contract
Every push carries the full Q&A set (built from normalized `answers` + `questions` join, never from raw JSONB):
```json
{
  "event": "entry.captured",
  "contact": { "first_name": "...", "last_name": "...", "email": "...", "phone": "...", "business_name": "..." },
  "campaign": { "name": "...", "type": "...", "tag_namespace": "..." },
  "outcome": "winner",
  "tags_applied": ["Summer_Giveaway_Winner"],
  "score": 74,
  "questions_and_answers": [{ "question": "...", "answer": "..." }],
  "entry_id": "uuid",
  "captured_at": "2026-06-12T14:32:00Z"
}
```

### Security Requirements
- **All headers via Tower middleware** (CSP, HSTS, X-Frame-Options, X-Content-Type-Options, Referrer-Policy)
- **Rate limiting** via `governor`: 20 req/min/IP public, 100 req/min authenticated
- **API keys** hashed with bcrypt — NEVER compare via direct hash equality
- **Formula evaluation** in calculator mechanic: restricted arithmetic parser only — NEVER eval/exec/scripting engine
- **Raffle compliance**: official_rules_url required, consent_gathered must be explicit `true`, random_seed stored permanently
- **Input validation**: `#[serde(deny_unknown_fields)]` on public request structs, email/phone format validation
- **Secrets in api_credentials**: AES-encrypted at rest, decrypted only at push time, never logged
- **`panic = "abort"`** in release profile — any panic kills the instance, so ALL error paths must use Result

### Build & Deploy
- **Stack:** Rust (Axum) + Tower middleware
- **Frontend:** React/Next.js (unchanged from architecture — only backend API changes)
- **Hosting:** Railway (builds from Dockerfile)
- **Dockerfile:** Multi-stage build (rust:1.82-slim → debian:bookworm-slim)
- **Railway config:** railway.toml with healthcheck path `/api/v1/health`
- **Formula evaluator:** Use `mathjs-rs` or a hand-rolled recursive-descent parser restricted to `+ - * / ( )` and named variables

### API Endpoints
```
GET  /api/v1/health                    — Public, rate-limited (20/min/IP)
GET  /api/v1/campaigns/:slug           — Public, edge-cacheable 60s
POST /api/v1/entries                   — Public, rate-limited — core capture
POST /api/v1/raffles/:slug/enter       — Public, rate-limited

GET  /api/v1/campaigns                 — Authenticated (JWT or API key)
POST /api/v1/campaigns                 — Authenticated, feature-gated
POST /api/v1/raffles/:slug/draw        — Authenticated
POST /api/v1/raffles/:slug/redraw      — Authenticated
POST /api/v1/loyalty/checkin           — Public-but-scoped (QR), DB-level cap enforcement
POST /api/v1/loyalty/rewards/:id/approve  — Authenticated
POST /api/v1/loyalty/rewards/:id/deny     — Authenticated
POST /api/v1/delivery/resend           — Authenticated
GET  /api/v1/contacts                  — Authenticated, light CRM list
GET  /api/v1/contacts/:id              — Authenticated, full entry history
```

### Cargo.toml Dependencies
- axum = "0.7", tokio = "1" (full), tower = "0.4", tower-http = "0.5" (cors, trace, timeout)
- serde = "1" (derive), serde_json = "1", sqlx = "0.7" (runtime-tokio-rustls, postgres, uuid, chrono, json)
- uuid = "1" (v4, serde), chrono = "0.4" (serde), reqwest = "0.12" (json, rustls-tls)
- bcrypt = "0.15", rand = "0.8", governor = "0.6"
- tracing = "0.1", tracing-subscriber = "0.3" (env-filter), dotenvy = "0.15", thiserror = "1"

---

## Rust Build Prompt
The full build prompt from the original spec is in `INCENTIVESWIFT_RUST_BUILD_PROMPT.md` in this repo. It includes:
- Why Axum over Actix-web
- Exact project structure
- Security non-negotiables
- Claude Code review checklist
- Testing requirements

## Architecture Document
The full architecture spec is in `INCENTIVESWIFT_ARCHITECTURE.md` in this repo. It includes:
- Complete data flow diagrams
- Database schema (also in migrations/)
- Light CRM design
- Payload contract
- Plan tier system
- Loyalty module design

---

## Order of Operations for Linux Agent
1. Create Supabase project under Swiftsoftware204 org
2. Copy service role key and anon key into `.env` and Railway
3. Push the repo to GitHub (`Swiftsoftware204/IncentiveSwift`)
4. Run migrations against Supabase
5. Build and test (`cargo check`, `cargo test`)
6. Deploy to Railway from Dockerfile
7. Create FunnelSwift affiliate product + plan tags for IncentiveSwift
8. Test delivery webhook to FunnelSwift
9. Verify loyalty check-in flow end-to-end
