# GUARDRAILS.md — IncentiveSwift

**Rust Guardrails — Vibe Engineering Standard**

## Non-Negotiable
- No `unwrap()` or `expect()` in production code.
- Phantom table fix already applied (categories, leads, surfaces) — do not recreate.
- Voucher issuance posts to IncentiveSwift endpoint on port 8083.
- All new features: check if table exists before CREATE TABLE.
- `cargo clippy -- -D warnings` must pass.
- Build through `/usr/local/bin/swift-build.sh incentiveswift`.
