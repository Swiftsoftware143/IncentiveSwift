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
- **Loyalty V1:** Recurring point-based check-in, online visit/share/referral tracking, reward tiers (auto/manual approval), daily cap enforcement (DB-level)
- **Loyalty V2 (fully built):** Purchase verification via PIN, rotating cross-promotion vouchers, business pledges (admin review), reward redemption, rotation group config (non-competing business pairs)
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

### Email Templates (New)

IncentiveSwift has a database-backed email template system (`email_templates` table) for transactional emails.

**Template Types:**

| Type | When Sent | Merge Fields |
|---|---|---|
| `welcome` | Account created after checkout | `{{name}}`, `{{email}}`, `{{password}}`, `{{app_url}}` |
| `purchase_confirmed` | Successful payment | `{{name}}`, `{{plan_name}}`, `{{app_url}}` |

**API Endpoints:**

| Method | Path | Description |
|---|---|---|
| GET/POST | `/api/email-templates` | List / Create |
| GET/PUT/DELETE | `/api/email-templates/:id` | Read / Update / Delete |
| GET | `/api/email-templates/merge-fields` | Available merge fields |

**Template fields:** name, template_type, subject, body (txt), html_body, is_html, is_default

**How it works:**
1. `send_template_email()` called with type + variable map
2. Looks up DB template (account-scoped → default fallback)
3. Renders `{{variable}}` placeholders from the variable map
4. Falls back to hardcoded inline template if no DB match
5. Queues to `outbound_messages` for async SMTP delivery

**Default seeds:** Welcome Email (credentials) + Purchase Confirmation (plan name, login link)

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

### API Endpoints (expanded)

Full route table in `docs/inline-api-reference.md`. Key additions beyond initial build:

**Credits (fully wired):**
```
GET  /api/v1/credits/balance           — Balance + plan limits
GET  /api/v1/credits/history           — Paginated transaction log
POST /api/v1/credits/topup             — Stripe checkout
POST /api/v1/admin/credits/adjust      — Admin adjustment
POST /api/v1/webhooks/sms/             — SMS credit triggers
```

**Loyalty V2 (purchase verification & vouchers):**
```
POST /api/v1/loyalty/generate-pin      — Business generates PIN
POST /api/v1/loyalty/verify-purchase   — Consumer verifies purchase
POST /api/v1/loyalty/issue-voucher     — Issue cross-promo voucher
GET  /api/v1/loyalty/my-vouchers/:id   — List active vouchers
POST /api/v1/loyalty/claim-voucher     — Redeem voucher
POST /api/v1/loyalty/expire-vouchers   — Expire overdue (cron)
POST /api/v1/loyalty/redeem-reward     — Points for reward
```

**Referral Engine:**
```
POST /api/v1/campaigns/:slug/referral-codes     — Generate code
GET  /api/v1/campaigns/:slug/referral-stats      — Stats
GET  /api/v1/campaigns/:slug/leaderboard         — Leaderboard
POST /api/v1/campaigns/:slug/earn-channels       — Earn channels
GET  /api/v1/earn/:channel_code                  — Public click-through
```

**Legacy (initial build):**
```
GET  /api/v1/health                    — Public
POST /api/v1/entries                   — Core capture
GET  /api/v1/campaigns                 — Authenticated
POST /api/v1/campaigns                 — Feature-gated
POST /api/v1/loyalty/checkin           — Daily check-in
POST /api/v1/loyalty/rewards/:id/approve  — Approve reward
GET  /api/v1/contacts                  — Light CRM list
```

### Cargo.toml Dependencies
- axum = "0.7", tokio = "1" (full), tower = "0.4", tower-http = "0.5" (cors, trace, timeout)
- serde = "1" (derive), serde_json = "1", sqlx = "0.7" (runtime-tokio-rustls, postgres, uuid, chrono, json)
- uuid = "1" (v4, serde), chrono = "0.4" (serde), reqwest = "0.12" (json, rustls-tls)
- bcrypt = "0.15", rand = "0.8", governor = "0.6"
- tracing = "0.1", tracing-subscriber = "0.3" (env-filter), dotenvy = "0.15", thiserror = "1"

---

## Guide Files (this repo)
- `docs/admin-guide.md` — Full admin/API reference with concepts, setup, endpoint tables, webhook events
- `docs/inline-api-reference.md` — Complete route-to-handler mapping extracted from `src/main.rs`
- `INCENTIVESWIFT_ARCHITECTURE.md` — Ecosystem architecture, one-way flow, payload contract
- `INCENTIVESWIFT_RUST_BUILD_PROMPT.md` — Rust build spec for code generation (includes security non-negotiables, testing requirements, Claude Code checklist)

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
