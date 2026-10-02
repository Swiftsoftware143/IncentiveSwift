# SwiftSoftware Architecture — Single Source of Truth
# Last updated: 2026-07-24
# IF YOU CHANGE ANYTHING BELOW, UPDATE THIS FILE.

## Golden Rules (Read Before Touching ANY App)

### Rule 1: One VPS, One Brain
All 7 apps share this Miami VPS. No app owns the entire machine.
- CARGO_BUILD_JOBS=1 always (2GB RAM constraint)
- One build at a time — check `ps aux | grep rustc` before starting
- If load > 3.0 or memory > 80%, STOP. Tell David.

### Rule 2: FunnelSwift IS the Affiliate Hub
There is ONE affiliate system and it lives in FunnelSwift.
- FunnelSwift owns: affiliate codes, tracking links, clicks, conversions, commissions, payouts
- FunnelSwift admin = affiliate director
- ALL other apps sync their plans INTO FunnelSwift's `affiliate_products` table
- NO other app has independent commission/affiliate logic beyond plan sync

### Rule 3: Two Free Plans in FunnelSwift
| Plan | Slug | Entry Point | What User Gets |
|------|------|-------------|----------------|
| Free | `free` | `app.funnelswift.net/signup` | Full CRM dashboard |
| Kinetic Free | `kinetic_free` | `funnelswift.net/kinetic` (modal) | Bio-link card + lead capture |

### Rule 4: The Commissionable Catalogue Is FunnelSwift-Owned
FunnelSwift owns `affiliate_products`; a sibling app does NOT push its plans into it. The endpoint
that used to accept them is RETIRED (kanban t_141162e7) and its senders were deleted:
```
[App] → POST /api/v1/internal/sync-affiliate-plan → REMOVED (no receiver, no sender)
```
Why it could never work — both decided after it was written:
- The receiver required the plan to exist in FunnelSwift's OWN `plans` table (ONE WRITER, kanban
  t_6d326447). A sibling's `plan_id` is a uuid from a different database and can never resolve —
  measured live 2026-10-01: a perfectly-keyed call answers `400 plan <uuid> not found`, writes nothing.
- Migration 070 (David, 2026-09-29) allows an affiliate product ONLY for a free plan, so the live
  catalogue is exactly one `… Free` / `$0` row per app (7 rows measured 2026-10-01: two FunnelSwift
  plans by `plan_id`, one tag-linked row per sibling `source_app`).

The cross-app call that IS live is the commission trigger — an app fires it from its own plan-change
path when a referred customer moves onto a paid plan:
```
[App] → POST /api/v1/internal/affiliate/upgrade-event (header x-internal-key) → [FunnelSwift]
```

### Rule 5: Zaarcash ≠ Affiliate
- **Zaarcash** = loyalty points, owned by IncentiveSwift, used by ZaarHub
- **Affiliate** = commission tracking, owned by FunnelSwift, used by ALL apps
- These are COMPLETELY separate. Never merge them.
- Multi-Directory: has loyalty proxy (Zaarcash) ONLY. No affiliate logic.

## App Directory & Port Map

| App | Path | Port | Service | Domain |
|-----|------|------|---------|--------|
| Multi-Directory | `/opt/swift/multidirectory-rust` | 3001 | multidirectory | directory.swiftsoftware.net |
| CoreSwift CRM | `/opt/swift/coreswift` | 8084 | coreswift-crm | coreswiftcrm.com |
| FunnelSwift | `/opt/swift/funnelswift` | 8080 | funnelswift | funnelswift.net |
| IncentiveSwift | `/opt/swift/incentiveswift` | 8083 | incentiveswift-api | incentiveswift.com |
| WorkflowSwift | `/opt/swift/workflowswift` | 8085 | workflowswift-api | workflowswift.com |
| MissedCall | `/opt/swift/missedcall_respondr` | 8088 | missedcall-respondr | missedcallrespondr.com |
| ADA Swift | `/opt/swift/adaswift` | 8087 | adaswift | adaswift.com |

## Database
All apps share one Postgres instance (Docker: swift-postgres-1).
- Host: 127.0.0.1:5432
- User: swift
- Each app has its own database: `coreswift`, `funnelswift`, `incentiveswift`, etc.

## Each App's Responsibility

### 1. FunnelSwift — The Affiliate Hub
- **AFFILIATE SYSTEM**: Codes, links, tracking, conversions, commissions, payouts
- **Owns**: `affiliate_products`, `affiliate_users`, `affiliate_clicks`, `affiliate_conversions`, `affiliate_links`
- **Two free entry points**: Kinetic modal + standard signup page
- **Cross-app call**: `POST /api/v1/internal/affiliate/upgrade-event` (`x-internal-key` header) — a sibling's paid upgrade credits the referring affiliate. The plan→product sync route was RETIRED (kanban t_141162e7).

### 2. Multi-Directory — Directory SaaS
- **Zaarcash loyalty proxy** → IncentiveSwift (routes loyalty requests)
- **NO affiliate logic** (was removed, do NOT re-add)
- Serves ZaarHub frontend + multiple tenant directories
- Onboarding survey system for city/preference config

### 3. CoreSwift CRM — CRM Platform
- **Plan sync to FunnelSwift** via `src/native_apps/connectors/funnelswift.rs`
- **Webhook system** for cross-app events
- **Branch**: `master` (NOT `main`)
- **Affiliates module**: plans handler reads from `affiliates` table, syncs to FunnelSwift

### 4. IncentiveSwift — Loyalty/Zaarcash Engine
- **OWNS Zaarcash**: points per check-in, credit rate, offers, vouchers, rewards
- **Credit rate config**: per-tenant, defaults to 10 (10 Zaarcash per $1)
- **Plan sync to FunnelSwift**: `src/handlers/plans_handler.rs`
- **DO NOT touch Zaarcash/loyalty code** when modifying affiliate wiring

### 5. WorkflowSwift — Workflow Automation
- **Plan sync to FunnelSwift**: `src/handlers/plan_handler.rs`
- n8n integration via `n8n.swiftsoftware.net:5678`
- Affiliates handler is thin CRUD for local table only

### 6. MissedCall Respondr — Missed Call Management
- **Plan sync + checkout conversions** → FunnelSwift
- Checkout fires `POST /api/v1/webhooks/conversion` to FunnelSwift
- Affiliates handler is local CRUD

### 7. ADA Swift — ADA Compliance Scanning
- **Service under SwiftImpact Solutions** (not a standalone SaaS)
- **Plan sync to FunnelSwift**: `src/handlers/plans_handler.rs`
- Scans are free, affiliates get paid on plan upgrades only
- `ADASwift Monthly Scan` is INACTIVE in affiliate_products

## Cross-App Flow: Affiliate Signup → Commission

```
1. Affiliate signs up on FunnelSwift → gets affiliate code (AFF-XXXX)
2. Affiliate creates Kinetic card → gets /k/:slug with ?src= param tracking
3. Lead finds card → submits info on the card
   → Auto-created account on FunnelSwift (kinetic_free plan)
   → Tagged with affiliate's tag
   → affiliate_conversions row (pending, $5.00)
4. Lead upgrades to paid plan on FunnelSwift
   → Commission calculated by plan price × commission_rate
   → affiliate_conversions updated to "approved"
5. Affiliate sees earnings in FunnelSwift dashboard
```

## Anti-Rules (Never Do These)

- ❌ Create a separate affiliate module in any app other than FunnelSwift
- ❌ Add `affiliate_code` processing to non-FunnelSwift signup handlers
- ❌ Build parallel sub-agents for builds (serial only)
- ❌ Use `#[allow(...)]` to silence compiler warnings — fix the code
- ❌ Use `.unwrap()` or `.expect()` in production code
- ❌ Merge Zaarcash (loyalty) with Affiliate (commission) — they are separate

## Deployment Sequence

1. Run `cargo check` → fix errors
2. Run `cargo test` → fix failures
3. Run `cargo clippy -- -D warnings` → fix warnings
4. `cargo build --release` (CARGO_BUILD_JOBS=1)
5. `cp target/release/{binary} /opt/swift/{app}/{binary}`
6. `systemctl restart {service}.service`
7. `curl -s localhost:{port}/api/health` → verify 200

## Git
- All repos use `origin` remote
- CoreSwift uses `master` branch, all others use `main`
- Never force push
- Use `/opt/swift/sync-to-repo.sh` for bulk VPS→GitHub sync
