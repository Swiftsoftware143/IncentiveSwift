# AGENTS.md — Vibe Engineering Rules for AI Agents

## Rust Guardrails (MANDATORY)
- **Zero unsafe blocks** unless explicitly approved by the Lead Architect
- **Zero .unwrap() or .expect()** in non-test production code — use `thiserror`/`anyhow`
- **All async state must implement Send + Sync**
- **Parameterized SQL only** — use `sqlx::query_as!` for compile-time validation
- **Secrets in env vars only** — never hardcoded
- **cargo fmt** before commit

## Verification Sequence (NON-NEGOTIABLE)
After ANY code change:
1. `cargo check` — syntax + borrow checker. Read stderr. Fix. Repeat until clean.
2. `cargo test` — all tests must pass
3. `cargo clippy -- -D warnings` — zero warnings tolerated
4. `cargo fmt -- --check` — formatting must be consistent

## Self-Correction Loop
- Compiler error → read diagnostic → understand → fix → re-compile
- Test failure → fix logic → re-run
- Clippy warning → clean up → re-run
- **NEVER paste errors to a human. FIX THEM.**
- 3 attempts max, then escalate with evidence of what you tried.

## Hermes Delegation Pattern
For complex feature implementation:
1. Draft trait signatures and types FIRST
2. Run `cargo check` to validate types before writing method bodies
3. Then implement method logic — iterate with check/test/clippy
4. Re-run full verification before declaring done

## Build Lock Protocol
- ALWAYS use `/opt/swift/build-lock.sh <app> <command>`
- Never raw `cargo build --release` on shared repos
- Exit 2 = another bot building → wait 30s, retry once
- Stale lock >30min: clear and proceed

## Post-Deploy Smoke Test
- `curl -s -o /dev/null -w "%{http_code}" <domain>` must return 200

## Project File Architecture
```
src/access/feature_gate.rs
src/access/mod.rs
src/billing/checkout.rs
src/billing/mod.rs
src/billing/providers.rs
src/billing/webhooks.rs
src/config.rs
src/db/accounts.rs
src/db/campaigns.rs
src/db/contacts.rs
src/db/delivery_log.rs
src/db/entries.rs
src/db/loyalty.rs
src/db/mod.rs
src/db/plans.rs
src/db/questions_answers.rs
src/db/raffles.rs
src/db/viral.rs
src/delivery/coreswift.rs
src/delivery/coreswift_push.rs
src/delivery/coreswift_sync.rs
src/delivery/direct_api/activecampaign.rs
src/delivery/direct_api/gohighlevel.rs
src/delivery/direct_api/hubspot.rs
src/delivery/direct_api/marketing_boost.rs
src/delivery/direct_api/mod.rs
src/delivery/entry_webhook.rs
src/delivery/integration_hub.rs
src/delivery/mod.rs
src/delivery/output_actions.rs
src/delivery/payload.rs
src/delivery/sender.rs
src/delivery/webhook.rs
src/email.rs
src/error.rs
src/features.rs
src/handlers/admin_handler.rs
src/handlers/analytics_handler.rs
src/handlers/api_keys.rs
src/handlers/auth_handler.rs
```
