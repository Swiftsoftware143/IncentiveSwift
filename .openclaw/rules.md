# .openclaw/rules.md

## RULES PROTECTION — READ THIS FIRST

**These rules are READ-ONLY.** Do NOT modify, rewrite, or "improve" this file unless the CEO Bot or David explicitly instructs you to do so. This file exists to guard against regression — editing it defeats its purpose.

**Before declaring any task complete:** Re-read these rules and confirm your changes satisfy every applicable rule. If you skip a rule, the task is NOT done.

---

 — Automated Agent Rules
# 
# This file is read by OpenClaw on EVERY context load for this repo.
# It defines permanent constraints that survive context windows and sessions.

## CRITICAL — NEVER VIOLATE THESE

### 1. No Direct VPS Edits
- NEVER edit files directly on the VPS without committing
- Script: `git add → git commit → git push → deploy`
- If it's not pushed, it doesn't exist

### 2. Workspace Hygiene
- Delete ALL temp scripts (`*.sh`, `*.py`, `*.json` test payloads) before `git commit`
- NEVER commit `/tmp/` files, `.cargo/`, `target/`, `.bak` files
- Run `git status` before every commit — if you see anything that isn't `src/`, `Cargo.toml`, `Cargo.lock`, or config, STOP

### 3. Full User Journey Required
- A feature is NOT done until backend + frontend + admin UI are all connected
- Verify: marketing page → register → backend → free plan → dashboard redirect
- "It works on localhost" is not sufficient — smoke test through the real domain

### 4. Plan-Based Feature Gating
- All limits come from the plans table — NEVER hardcode a limit value
- Free plan assignment: query by `slug = 'free'`, NEVER by hardcoded UUID
- Every create/insert handler must call `enforce_feature_limit()`
- Every frontend must catch HTTP 402 and show an upgrade prompt

### 5. Build Pipeline
- ALWAYS use `/opt/swift/build-lock.sh {app} cargo build --release` — NEVER raw `cargo build`
- `cargo check` must pass with zero errors before building
- `systemctl restart {service}` after every deploy
- Smoke test the app after restart

### 6. Routing (3-Layer)
- Main domain → marketing page ONLY
- `app.*` subdomain → user dashboard SPA
- `admin.*` subdomain → admin SPA  
- NEVER mix them — each subdomain has its own nginx root directory

### 7. No Dead Endpoints
- Every API route in `main.rs`/`routes.rs` must have a corresponding frontend caller
- If a route has no frontend, either add the UI or document it as `// INTERNAL`

### 8. Git Protocol
- `git pull` before starting any work
- Commit after every meaningful change
- Push after every commit
- Feature branches for multi-commit work: `ceobot/feature-name`

---

## App-Specific Conventions

### FunnelSwift (funnelswift.net:8080)
- Plan limits: plans.max_cards, plans.max_leads, plans.max_tags, plans.max_forms
- Register: marketing modal → `POST /api/v1/auth/register`
- Frontend gating: dashboard.js must have `showUpgradePrompt()` with 402 catch

### IncentiveSwift (incentiveswift.com:8083)
- Plan limits: plans.max_leads, plans.max_tags, plans.max_campaigns
- Register: app page has login/register tabs → `POST /api/v1/auth/register`
- Frontend: admin index.html must have 402 catch

### WorkflowSwift (workflowswift.com:8085)
- Plan limits: plan_tiers.max_workflows, plan_tiers.max_users, plan_tiers.features JSONB
- Register: app SPA has register toggle → `POST /register`
- Frontend: Preact app must include RegisterForm component

### ADASwift (adaswift.com:8087)
- Plan limits: plans.max_leads, plans.max_tags
- Register: marketing `openRegister()` modal → `POST /api/v1/auth/register`
- Frontend: admin must have 402 catch

### CoreSwift CRM (coreswiftcrm.com:8084)
- Plan limits: plans.max_industries, plans.features JSONB
- Register: NEVER hardcode plan UUID — query `slug = 'free'`
- Frontend: app login page must have register tab

### MissedCall (missedcallrespondr.com:8088)
- Plan limits: plans.max_leads, plans.max_tags
- Register: app page has register tab → `POST /api/v1/auth/register`
- Frontend: admin must have 402 catch

---

## Deployment Checklist Reference
After EVERY deploy, run through:
`memory/deployment-checklist.md` (in SwiftSoftware CEO workspace)

The 9 sections: Source Control → Build → Git Sync → Signup Flow → Admin UI → 402 Gating → Plan Management → Nginx Routing → Heartbeat


### 9. Admin Login — NEVER BREAK
- Admin credentials: `swiftsoftware143@yahoo.com` / `SwiftAdmin2026!`
- After EVERY deploy: verify admin login works
- For SaaS apps: `https://admin.{domain}/` must accept these credentials
- For Multi-Directory: `https://directory.swiftsoftware.net/admin` must accept these credentials
- If admin login returns 401/422/500 — deployment is BROKEN, roll back immediately
- This applies across ALL 7 apps — no exceptions
