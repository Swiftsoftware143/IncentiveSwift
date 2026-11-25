//! IncentiveSwift's MCP door: ten read-only tools over a customer's own data.
//!
//! Credential: `api_keys` (`prefix`, `is_active`, `expires_at`). The trap this door resolves is
//! that the key table's `tenant_id` column is an `accounts.id` (FK `tenant_id -> accounts(id)`)
//! while the data tables split between an `account_id` column (campaigns, credit_transactions,
//! inbound_messages, tags, loyalty_programs) and a `tenant_id` column (offers, support_tickets) —
//! BOTH hold the account id in this app. The resolver maps the key row to `accounts.id` via its
//! `tenant_id` (FK-backed) and every tool filters on that single id.
//!
//! Read tools only: no write, destructive or cross-workspace tool is exposed (phase 2 is a
//! separate card). `treasury_holds` / `treasury_funding` are empty fleet-wide and are reported as
//! zero — never invented.
//!
//! A key maps to exactly ONE account, so every read is a fully parameterized `= $1` on that id —
//! no SQL string is ever assembled from caller input or a scope clause (the fleet gate forbids it).

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;
use chrono::{DateTime, Utc};
use mcp_door::credential::constant_time_eq;
use mcp_door::{
    AuthError, Caller, CredentialResolver, Ctx, Door, Registry, Role, ToolError, ToolSpec,
};
use serde_json::{json, Value};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::state::AppState;

/// The door's protocol identity.
pub const DOOR_NAME: &str = "incentiveswift";
/// The audit file. Lives inside the container (WORKDIR `/app`, owned by the `incentiveswift`
/// user) and is created `0600` on the first audited call.
pub const AUDIT_PATH: &str = "/app/mcp-audit.log";

const KEY_MARKER: &str = "is_key_";
const KEY_PREFIX_LEN: usize = 8;
/// Untrusted customer text (inbound message bodies, ticket descriptions) is excerpt-capped.
const EXCERPT_CAP: usize = 200;

// ─────────────────────────────────────────────────────────────────────────────
// Resolver
// ─────────────────────────────────────────────────────────────────────────────

struct ApiKeyResolver {
    pool: PgPool,
}

/// The first `KEY_PREFIX_LEN` chars of the random part of an `is_key_<…>` token.
fn issued_prefix(token: &str) -> Option<&str> {
    token
        .strip_prefix(KEY_MARKER)
        .and_then(|s| s.get(..KEY_PREFIX_LEN))
}

/// Compare a bearer against a stored key hash: the fleet sha256 (lowercase hex) in constant time,
/// with a legacy bcrypt fallback (the two historic formats this app's own resolver reads).
fn secret_matches(token: &str, stored: &str) -> bool {
    if stored.len() == 64 && stored.bytes().all(|b| b.is_ascii_hexdigit()) {
        let digest = mcp_door::hash_token(token);
        constant_time_eq(digest.as_bytes(), stored.as_bytes())
    } else {
        bcrypt::verify(token, stored).unwrap_or(false)
    }
}

#[async_trait]
impl CredentialResolver for ApiKeyResolver {
    async fn resolve(&self, bearer: &str) -> Result<Caller, AuthError> {
        let Some(prefix) = issued_prefix(bearer) else {
            return Err(AuthError::Invalid);
        };

        let rows: Vec<(String, Uuid, bool, Option<DateTime<Utc>>)> = sqlx::query_as(
            "SELECT key_hash, tenant_id, is_active, expires_at FROM api_keys WHERE prefix = $1",
        )
        .bind(prefix)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AuthError::Unavailable(e.to_string()))?;

        if rows.is_empty() {
            return Err(AuthError::Invalid);
        }

        let now = Utc::now();
        let mut has_live = false;
        let mut has_dead = false;
        for (key_hash, account_id, is_active, expires_at) in rows {
            let expired = expires_at.is_some_and(|e| e <= now);
            if !is_active || expired {
                has_dead = true;
                continue;
            }
            has_live = true;
            if secret_matches(bearer, &key_hash) {
                if !self.account_live(account_id).await? {
                    return Err(AuthError::Revoked);
                }
                let caller = Caller::new(format!("api_key:{prefix}"), Role::Workspace)
                    .with_tenant(account_id);
                return Ok(caller);
            }
        }

        // A live key with the wrong secret is a bad credential, not a revoked one. Revoked is only
        // the honest label when every row carrying this prefix is dead (inactive/expired).
        if has_live {
            Err(AuthError::Invalid)
        } else if has_dead {
            Err(AuthError::Revoked)
        } else {
            Err(AuthError::Invalid)
        }
    }
}

impl ApiKeyResolver {
    async fn account_live(&self, account_id: Uuid) -> Result<bool, AuthError> {
        let live: Option<bool> =
            sqlx::query_scalar("SELECT (closed_at IS NULL) FROM accounts WHERE id = $1")
                .bind(account_id)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| AuthError::Unavailable(e.to_string()))?;
        Ok(live == Some(true))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tools
// ─────────────────────────────────────────────────────────────────────────────

/// The single account id a key resolves to. A key maps to exactly one account, so every read is a
/// fully parameterized `= $1` on this id (never a scope clause assembled into a SQL string).
fn account_id(ctx: &Ctx) -> Result<Uuid, ToolError> {
    ctx.scope
        .ids()
        .first()
        .copied()
        .ok_or_else(|| ToolError::Invalid("no workspace bound to this credential".into()))
}

fn excerpt(text: Option<&str>) -> Option<String> {
    text.and_then(|t| {
        let t = t.trim();
        if t.is_empty() {
            None
        } else {
            let mut chars = t.chars();
            let head: String = chars.by_ref().take(EXCERPT_CAP).collect();
            if chars.next().is_some() {
                Some(format!("{head}…"))
            } else {
                Some(head)
            }
        }
    })
}

fn registry(pool: PgPool) -> Registry {
    let mut reg = Registry::new();

    // 1 ── list_campaigns ─────────────────────────────────────────────────────
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::read(
                "incentiveswift_list_campaigns",
                "List incentive campaigns in this workspace, newest first. Returns name, status and dates.",
            ),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let limit = ctx.limit(args.get("limit"), 25, 200);
                    let account = account_id(&ctx)?;
                    let rows = sqlx::query(
                        "SELECT id, name, slug, type, status, created_at FROM campaigns \
                         WHERE account_id = $1 ORDER BY created_at DESC LIMIT $2",
                    )
                    .bind(account)
                    .bind(limit)
                    .fetch_all(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    let items: Vec<Value> = rows
                        .iter()
                        .map(|r| {
                            json!({
                                "id": r.get::<Uuid, _>("id"),
                                "name": r.get::<String, _>("name"),
                                "slug": r.get::<String, _>("slug"),
                                "type": r.get::<String, _>("type"),
                                "status": r.get::<String, _>("status"),
                                "created_at": r.get::<DateTime<Utc>, _>("created_at"),
                            })
                        })
                        .collect();
                    Ok(json!({ "count": items.len(), "items": items }))
                })
            },
        );
    }

    // 2 ── get_campaign ───────────────────────────────────────────────────────
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::read(
                "incentiveswift_get_campaign",
                "Fetch one campaign by id with its referral totals (total and converted).",
            ),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let id = ctx.uuid_arg(&args, "id")?;
                    let row = sqlx::query(
                        "SELECT id, name, slug, type, status, account_id, created_at \
                         FROM campaigns WHERE id = $1",
                    )
                    .bind(id)
                    .fetch_optional(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?
                    .ok_or(ToolError::NotFound)?;

                    let owner = row.get::<Uuid, _>("account_id");
                    // The 404-not-403 rule: a foreign campaign answers not_found.
                    ctx.require_scope(owner)?;

                    let (total, converted): (i64, i64) = sqlx::query_as(
                        "SELECT count(*), count(*) FILTER (WHERE converted) \
                         FROM campaign_referrals WHERE campaign_id = $1",
                    )
                    .bind(id)
                    .fetch_one(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;

                    Ok(json!({
                        "id": row.get::<Uuid, _>("id"),
                        "name": row.get::<String, _>("name"),
                        "slug": row.get::<String, _>("slug"),
                        "type": row.get::<String, _>("type"),
                        "status": row.get::<String, _>("status"),
                        "created_at": row.get::<DateTime<Utc>, _>("created_at"),
                        "referrals_total": total,
                        "referrals_converted": converted,
                    }))
                })
            },
        );
    }

    // 3 ── list_participants ──────────────────────────────────────────────────
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::read(
                "incentiveswift_list_participants",
                "List campaign participants (referrals) with their contact details for this workspace.",
            ),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let limit = ctx.limit(args.get("limit"), 25, 200);
                    let account = account_id(&ctx)?;
                    let rows = sqlx::query(
                        "SELECT r.id, r.campaign_id, r.referral_code, r.converted, r.points_earned, \
                                r.created_at, con.first_name, con.last_name, con.email, con.phone \
                         FROM campaign_referrals r \
                         JOIN campaigns c ON c.id = r.campaign_id \
                         LEFT JOIN contacts con ON con.id = r.referrer_contact_id \
                         WHERE c.account_id = $1 ORDER BY r.created_at DESC LIMIT $2",
                    )
                    .bind(account)
                    .bind(limit)
                    .fetch_all(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    let items: Vec<Value> = rows
                        .iter()
                        .map(|r| {
                            json!({
                                "id": r.get::<Uuid, _>("id"),
                                "campaign_id": r.get::<Uuid, _>("campaign_id"),
                                "referral_code": r.get::<Option<String>, _>("referral_code"),
                                "converted": r.get::<bool, _>("converted"),
                                "points_earned": r.get::<i32, _>("points_earned"),
                                "created_at": r.get::<DateTime<Utc>, _>("created_at"),
                                "contact": {
                                    "first_name": r.get::<Option<String>, _>("first_name"),
                                    "last_name": r.get::<Option<String>, _>("last_name"),
                                    "email": r.get::<Option<String>, _>("email"),
                                    "phone": r.get::<Option<String>, _>("phone"),
                                },
                            })
                        })
                        .collect();
                    Ok(json!({ "count": items.len(), "items": items }))
                })
            },
        );
    }

    // 4 ── list_rewards ───────────────────────────────────────────────────────
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::read(
                "incentiveswift_list_rewards",
                "List loyalty programs and their reward tiers (names and points required) for this workspace.",
            ),
            move |ctx, _args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let programs = sqlx::query(
                        "SELECT id, name, slug, is_active, currency_name FROM loyalty_programs \
                         WHERE account_id = $1 ORDER BY created_at",
                    )
                    .bind(account)
                    .fetch_all(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    let mut items = Vec::with_capacity(programs.len());
                    for p in &programs {
                        let pid = p.get::<Uuid, _>("id");
                        let tiers = sqlx::query(
                            "SELECT name, points_required FROM loyalty_reward_tiers \
                             WHERE program_id = $1 ORDER BY sort_order",
                        )
                        .bind(pid)
                        .fetch_all(&pool)
                        .await
                        .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                        let tiers: Vec<Value> = tiers
                            .iter()
                            .map(|t| {
                                json!({
                                    "name": t.get::<String, _>("name"),
                                    "points_required": t.get::<i32, _>("points_required"),
                                })
                            })
                            .collect();
                        items.push(json!({
                            "id": pid,
                            "name": p.get::<String, _>("name"),
                            "slug": p.get::<String, _>("slug"),
                            "is_active": p.get::<bool, _>("is_active"),
                            "currency_name": p.get::<Option<String>, _>("currency_name"),
                            "tiers": tiers,
                        }));
                    }
                    Ok(json!({ "count": items.len(), "items": items }))
                })
            },
        );
    }

    // 5 ── list_scans ─────────────────────────────────────────────────────────
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::read(
                "incentiveswift_list_scans",
                "List loyalty scans for this workspace's programs. Zero rows is a real zero.",
            ),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let limit = ctx.limit(args.get("limit"), 25, 200);
                    let account = account_id(&ctx)?;
                    let rows = sqlx::query(
                        "SELECT s.id, s.business_name, s.scan_type, s.points_awarded, \
                                s.points_balance, s.transaction_amount::text AS transaction_amount, \
                                s.scanned_at \
                         FROM loyalty_scans s JOIN loyalty_programs p ON p.id = s.program_id \
                         WHERE p.account_id = $1 ORDER BY s.scanned_at DESC LIMIT $2",
                    )
                    .bind(account)
                    .bind(limit)
                    .fetch_all(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    let items: Vec<Value> = rows
                        .iter()
                        .map(|r| {
                            json!({
                                "id": r.get::<Uuid, _>("id"),
                                "business_name": r.get::<Option<String>, _>("business_name"),
                                "scan_type": r.get::<String, _>("scan_type"),
                                "points_awarded": r.get::<i32, _>("points_awarded"),
                                "points_balance": r.get::<i32, _>("points_balance"),
                                "transaction_amount": r.get::<Option<String>, _>("transaction_amount"),
                                "scanned_at": r.get::<DateTime<Utc>, _>("scanned_at"),
                            })
                        })
                        .collect();
                    Ok(json!({ "count": items.len(), "items": items }))
                })
            },
        );
    }

    // 6 ── list_point_ledger ──────────────────────────────────────────────────
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::read(
                "incentiveswift_list_point_ledger",
                "List point transactions (the credit ledger) for this workspace, with the running balance.",
            ),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let limit = ctx.limit(args.get("limit"), 25, 200);
                    let account = account_id(&ctx)?;
                    let rows = sqlx::query(
                        "SELECT id, amount, balance_after, action, reference_type, description, created_at \
                         FROM credit_transactions WHERE account_id = $1 ORDER BY created_at DESC LIMIT $2",
                    )
                    .bind(account)
                    .bind(limit)
                    .fetch_all(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    let balance: i64 = sqlx::query_scalar(
                        "SELECT COALESCE(SUM(amount), 0)::bigint FROM credit_transactions \
                         WHERE account_id = $1",
                    )
                    .bind(account)
                    .fetch_one(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    let items: Vec<Value> = rows
                        .iter()
                        .map(|r| {
                            json!({
                                "id": r.get::<Uuid, _>("id"),
                                "amount": r.get::<i32, _>("amount"),
                                "balance_after": r.get::<i32, _>("balance_after"),
                                "action": r.get::<String, _>("action"),
                                "reference_type": r.get::<Option<String>, _>("reference_type"),
                                "description": r.get::<Option<String>, _>("description"),
                                "created_at": r.get::<DateTime<Utc>, _>("created_at"),
                            })
                        })
                        .collect();
                    Ok(json!({ "count": items.len(), "balance": balance, "items": items }))
                })
            },
        );
    }

    // 7 ── list_payout_state ──────────────────────────────────────────────────
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::read(
                "incentiveswift_list_payout_state",
                "Report payout/clearinghouse state (holds, funding, supplier milestones) for this workspace.",
            ),
            move |ctx, _args| {
                let pool = pool.clone();
                Box::pin(async move {
                    // Holds ARE account-scoped (treasury_holds.campaign_id -> campaigns.account_id);
                    // funding and supplier milestones key a Multi-Directory business id with no
                    // account owner derivable in this app, so they report a real zero, never a row.
                    let account = account_id(&ctx)?;
                    let (holds, holds_points): (i64, i64) = sqlx::query_as(
                        "SELECT count(*)::bigint, COALESCE(SUM(h.points), 0)::bigint \
                         FROM treasury_holds h JOIN campaigns c ON c.id = h.campaign_id \
                         WHERE c.account_id = $1",
                    )
                    .bind(account)
                    .fetch_one(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;

                    Ok(json!({
                        "holds": { "count": holds, "total_points": holds_points },
                        "funding": { "count": 0, "total_amount": "0" },
                        "milestones": { "count": 0, "total_points": 0 },
                    }))
                })
            },
        );
    }

    // 8 ── list_offers ────────────────────────────────────────────────────────
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::read(
                "incentiveswift_list_offers",
                "List discount offers for this workspace (discount percent, cap and active flag only).",
            ),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let limit = ctx.limit(args.get("limit"), 25, 200);
                    let account = account_id(&ctx)?;
                    // offers.tenant_id holds the account id in this app.
                    let rows = sqlx::query(
                        "SELECT id, name, discount_percent, cap_dollars, active, created_at \
                         FROM offers WHERE tenant_id = $1 ORDER BY created_at DESC LIMIT $2",
                    )
                    .bind(account)
                    .bind(limit)
                    .fetch_all(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    let items: Vec<Value> = rows
                        .iter()
                        .map(|r| {
                            json!({
                                "id": r.get::<Uuid, _>("id"),
                                "name": r.get::<String, _>("name"),
                                "discount_percent": r.get::<i32, _>("discount_percent"),
                                "cap_dollars": r.get::<Option<i32>, _>("cap_dollars"),
                                "active": r.get::<bool, _>("active"),
                                "created_at": r.get::<DateTime<Utc>, _>("created_at"),
                            })
                        })
                        .collect();
                    Ok(json!({ "count": items.len(), "items": items }))
                })
            },
        );
    }

    // 9 ── list_inbound_messages ──────────────────────────────────────────────
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::read(
                "incentiveswift_list_inbound_messages",
                "List inbound messages for this workspace, with untrusted body text excerpted.",
            ),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let limit = ctx.limit(args.get("limit"), 25, 200);
                    let account = account_id(&ctx)?;
                    let rows = sqlx::query(
                        "SELECT id, from_number, to_number, direction, campaign_slug, processed, \
                                body, created_at \
                         FROM inbound_messages WHERE account_id = $1 ORDER BY created_at DESC LIMIT $2",
                    )
                    .bind(account)
                    .bind(limit)
                    .fetch_all(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    let items: Vec<Value> = rows
                        .iter()
                        .map(|r| {
                            json!({
                                "id": r.get::<Uuid, _>("id"),
                                "from_number": r.get::<Option<String>, _>("from_number"),
                                "to_number": r.get::<Option<String>, _>("to_number"),
                                "direction": r.get::<Option<String>, _>("direction"),
                                "campaign_slug": r.get::<Option<String>, _>("campaign_slug"),
                                "processed": r.get::<bool, _>("processed"),
                                "body_excerpt": excerpt(r.get::<Option<String>, _>("body").as_deref()),
                                "created_at": r.get::<DateTime<Utc>, _>("created_at"),
                            })
                        })
                        .collect();
                    Ok(json!({ "count": items.len(), "items": items }))
                })
            },
        );
    }

    // 10 ── list_tickets ──────────────────────────────────────────────────────
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::read(
                "incentiveswift_list_tickets",
                "List support tickets for this workspace, with untrusted description text excerpted.",
            ),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let limit = ctx.limit(args.get("limit"), 25, 200);
                    let account = account_id(&ctx)?;
                    // support_tickets.tenant_id holds the account id in this app.
                    let rows = sqlx::query(
                        "SELECT id, subject, status, priority, category, description, created_at \
                         FROM support_tickets WHERE tenant_id = $1 ORDER BY created_at DESC LIMIT $2",
                    )
                    .bind(account)
                    .bind(limit)
                    .fetch_all(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    let items: Vec<Value> = rows
                        .iter()
                        .map(|r| {
                            json!({
                                "id": r.get::<Uuid, _>("id"),
                                "subject": r.get::<String, _>("subject"),
                                "status": r.get::<String, _>("status"),
                                "priority": r.get::<Option<String>, _>("priority"),
                                "category": r.get::<Option<String>, _>("category"),
                                "description_excerpt": excerpt(r.get::<Option<String>, _>("description").as_deref()),
                                "created_at": r.get::<DateTime<Utc>, _>("created_at"),
                            })
                        })
                        .collect();
                    Ok(json!({ "count": items.len(), "items": items }))
                })
            },
        );
    }

    // 11 ── list_loyalty_programs (+tiers) ────────────────────────────────────
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::read(
                "incentiveswift_list_loyalty_programs",
                "List loyalty programs with their member tiers and earn/streak settings for this workspace.",
            ),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let limit = ctx.limit(args.get("limit"), 25, 200);
                    let account = account_id(&ctx)?;
                    let lprogs = sqlx::query(
                        "SELECT id, name, slug, currency_name, currency_icon, is_active, \
                                tiers_enabled, milestones_enabled, streak_enabled, \
                                points_per_checkin, points_per_visit, referral_bonus, \
                                birthday_bonus, points_expire_days, \
                                purchase_credit_rate::text AS purchase_credit_rate, created_at \
                         FROM loyalty_programs WHERE account_id = $1 ORDER BY created_at LIMIT $2",
                    )
                    .bind(account)
                    .bind(limit)
                    .fetch_all(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    let mut items = Vec::with_capacity(lprogs.len());
                    for pr in &lprogs {
                        let pid = pr.get::<Uuid, _>("id");
                        let tier_rows = sqlx::query(
                            "SELECT name, min_points, color, multiplier::text AS multiplier, \
                                    COALESCE(perks, '{}'::jsonb) AS perks \
                             FROM loyalty_tiers WHERE loyalty_program_id = $1 ORDER BY min_points",
                        )
                        .bind(pid)
                        .fetch_all(&pool)
                        .await
                        .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                        let tiers: Vec<Value> = tier_rows
                            .iter()
                            .map(|t| {
                                json!({
                                    "name": t.get::<String, _>("name"),
                                    "min_points": t.get::<i64, _>("min_points"),
                                    "color": t.get::<String, _>("color"),
                                    "multiplier": t.get::<String, _>("multiplier"),
                                    "perks": t.get::<Value, _>("perks"),
                                })
                            })
                            .collect();
                        items.push(json!({
                            "id": pr.get::<Uuid, _>("id"),
                            "name": pr.get::<String, _>("name"),
                            "slug": pr.get::<Option<String>, _>("slug"),
                            "currency_name": pr.get::<String, _>("currency_name"),
                            "currency_icon": pr.get::<String, _>("currency_icon"),
                            "is_active": pr.get::<Option<bool>, _>("is_active"),
                            "tiers_enabled": pr.get::<bool, _>("tiers_enabled"),
                            "milestones_enabled": pr.get::<bool, _>("milestones_enabled"),
                            "streak_enabled": pr.get::<bool, _>("streak_enabled"),
                            "points_per_checkin": pr.get::<Option<i32>, _>("points_per_checkin"),
                            "points_per_visit": pr.get::<i32, _>("points_per_visit"),
                            "referral_bonus": pr.get::<i32, _>("referral_bonus"),
                            "birthday_bonus": pr.get::<i32, _>("birthday_bonus"),
                            "points_expire_days": pr.get::<i32, _>("points_expire_days"),
                            "purchase_credit_rate": pr.get::<Option<String>, _>("purchase_credit_rate"),
                            "created_at": pr.get::<Option<DateTime<Utc>>, _>("created_at"),
                            "tiers": tiers,
                        }));
                    }
                    Ok(json!({ "count": items.len(), "items": items }))
                })
            },
        );
    }

    // 12 ── list_loyalty_members ───────────────────────────────────────────────
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::read(
                "incentiveswift_list_loyalty_members",
                "List loyalty members (balances, streaks, referrals) across this workspace's programs.",
            ),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let limit = ctx.limit(args.get("limit"), 25, 200);
                    let account = account_id(&ctx)?;
                    let rows = sqlx::query(
                        "SELECT m.id, m.program_id, m.contact_id, m.points_balance, \
                                m.lifetime_points, m.current_streak, m.longest_streak, \
                                m.total_referrals, m.referral_code, m.member_since, \
                                m.last_checkin_at, m.tier_id, \
                                con.first_name, con.last_name, con.email, con.phone \
                         FROM loyalty_members m \
                         JOIN loyalty_programs p ON p.id = m.program_id \
                         LEFT JOIN contacts con ON con.id = m.contact_id \
                         WHERE p.account_id = $1 ORDER BY m.member_since DESC LIMIT $2",
                    )
                    .bind(account)
                    .bind(limit)
                    .fetch_all(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    let items: Vec<Value> = rows
                        .iter()
                        .map(|r| {
                            json!({
                                "id": r.get::<Uuid, _>("id"),
                                "program_id": r.get::<Uuid, _>("program_id"),
                                "contact_id": r.get::<Uuid, _>("contact_id"),
                                "points_balance": r.get::<Option<i32>, _>("points_balance"),
                                "lifetime_points": r.get::<Option<i32>, _>("lifetime_points"),
                                "current_streak": r.get::<i32, _>("current_streak"),
                                "longest_streak": r.get::<i32, _>("longest_streak"),
                                "total_referrals": r.get::<i32, _>("total_referrals"),
                                "referral_code": r.get::<Option<String>, _>("referral_code"),
                                "member_since": r.get::<DateTime<Utc>, _>("member_since"),
                                "last_checkin_at": r.get::<Option<DateTime<Utc>>, _>("last_checkin_at"),
                                "tier_id": r.get::<Option<Uuid>, _>("tier_id"),
                                "first_name": r.get::<Option<String>, _>("first_name"),
                                "last_name": r.get::<Option<String>, _>("last_name"),
                                "email": r.get::<Option<String>, _>("email"),
                                "phone": r.get::<Option<String>, _>("phone"),
                            })
                        })
                        .collect();
                    Ok(json!({ "count": items.len(), "items": items }))
                })
            },
        );
    }

    // 13 ── list_raffles (+prize inventory) ────────────────────────────────────
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::read(
                "incentiveswift_list_raffles",
                "List raffle-type campaigns with their prize inventory and remaining stock for this workspace.",
            ),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let limit = ctx.limit(args.get("limit"), 25, 200);
                    let account = account_id(&ctx)?;
                    let raffle_rows = sqlx::query(
                        "SELECT id, name, slug, status, created_at FROM campaigns \
                         WHERE account_id = $1 AND type = 'raffle' \
                         ORDER BY created_at DESC LIMIT $2",
                    )
                    .bind(account)
                    .bind(limit)
                    .fetch_all(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    let mut items = Vec::with_capacity(raffle_rows.len());
                    for rf in &raffle_rows {
                        let cid = rf.get::<Uuid, _>("id");
                        let prize_rows = sqlx::query(
                            "SELECT prize_id, label, prize_type, total, remaining, claimed, created_at \
                             FROM campaign_prize_inventory WHERE campaign_id = $1 ORDER BY created_at",
                        )
                        .bind(cid)
                        .fetch_all(&pool)
                        .await
                        .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                        let prizes: Vec<Value> = prize_rows
                            .iter()
                            .map(|p| {
                                json!({
                                    "prize_id": p.get::<String, _>("prize_id"),
                                    "label": p.get::<String, _>("label"),
                                    "prize_type": p.get::<String, _>("prize_type"),
                                    "total": p.get::<Option<i32>, _>("total"),
                                    "remaining": p.get::<Option<i32>, _>("remaining"),
                                    "claimed": p.get::<i32, _>("claimed"),
                                    "created_at": p.get::<DateTime<Utc>, _>("created_at"),
                                })
                            })
                            .collect();
                        items.push(json!({
                            "id": rf.get::<Uuid, _>("id"),
                            "name": rf.get::<String, _>("name"),
                            "slug": rf.get::<String, _>("slug"),
                            "status": rf.get::<String, _>("status"),
                            "created_at": rf.get::<DateTime<Utc>, _>("created_at"),
                            "prize_inventory": prizes,
                        }));
                    }
                    Ok(json!({ "count": items.len(), "items": items }))
                })
            },
        );
    }

    // 14 ── list_milestones (+achieved) ────────────────────────────────────────
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::read(
                "incentiveswift_list_milestones",
                "List campaign milestones with how many contacts achieved each, for this workspace.",
            ),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let limit = ctx.limit(args.get("limit"), 25, 200);
                    let account = account_id(&ctx)?;
                    let rows = sqlx::query(
                        "SELECT m.id, m.campaign_id, m.name, m.description, m.points_required, \
                                m.action_type, m.is_repeatable, m.max_repeats, m.cooldown_hours, \
                                m.is_active, m.sort_order, m.created_at, \
                                (SELECT count(*) FROM campaign_milestones_achieved a \
                                 WHERE a.milestone_id = m.id) AS achieved_count \
                         FROM campaign_milestones m \
                         JOIN campaigns c ON c.id = m.campaign_id \
                         WHERE c.account_id = $1 ORDER BY m.sort_order, m.created_at LIMIT $2",
                    )
                    .bind(account)
                    .bind(limit)
                    .fetch_all(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    let items: Vec<Value> = rows
                        .iter()
                        .map(|r| {
                            json!({
                                "id": r.get::<Uuid, _>("id"),
                                "campaign_id": r.get::<Uuid, _>("campaign_id"),
                                "name": r.get::<String, _>("name"),
                                "description": r.get::<Option<String>, _>("description"),
                                "points_required": r.get::<i32, _>("points_required"),
                                "action_type": r.get::<String, _>("action_type"),
                                "is_repeatable": r.get::<bool, _>("is_repeatable"),
                                "max_repeats": r.get::<Option<i32>, _>("max_repeats"),
                                "cooldown_hours": r.get::<Option<i32>, _>("cooldown_hours"),
                                "is_active": r.get::<bool, _>("is_active"),
                                "sort_order": r.get::<i32, _>("sort_order"),
                                "created_at": r.get::<DateTime<Utc>, _>("created_at"),
                                "achieved_count": r.get::<i64, _>("achieved_count"),
                            })
                        })
                        .collect();
                    Ok(json!({ "count": items.len(), "items": items }))
                })
            },
        );
    }

    // 15 ── list_secret_codes (+redemptions) ───────────────────────────────────
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::read(
                "incentiveswift_list_secret_codes",
                "List campaign secret codes with their redemption counts for this workspace.",
            ),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let limit = ctx.limit(args.get("limit"), 25, 200);
                    let account = account_id(&ctx)?;
                    let rows = sqlx::query(
                        "SELECT sc.id, sc.campaign_id, sc.code, sc.points, sc.max_uses, \
                                sc.uses_count, sc.is_active, sc.expires_at, sc.created_at, \
                                (SELECT count(*) FROM campaign_secret_code_redemptions r \
                                 WHERE r.secret_code_id = sc.id) AS redemption_count \
                         FROM campaign_secret_codes sc \
                         JOIN campaigns c ON c.id = sc.campaign_id \
                         WHERE c.account_id = $1 ORDER BY sc.created_at DESC LIMIT $2",
                    )
                    .bind(account)
                    .bind(limit)
                    .fetch_all(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    let items: Vec<Value> = rows
                        .iter()
                        .map(|r| {
                            json!({
                                "id": r.get::<Uuid, _>("id"),
                                "campaign_id": r.get::<Uuid, _>("campaign_id"),
                                "code": r.get::<String, _>("code"),
                                "points": r.get::<i32, _>("points"),
                                "max_uses": r.get::<Option<i32>, _>("max_uses"),
                                "uses_count": r.get::<i32, _>("uses_count"),
                                "is_active": r.get::<bool, _>("is_active"),
                                "expires_at": r.get::<Option<DateTime<Utc>>, _>("expires_at"),
                                "created_at": r.get::<DateTime<Utc>, _>("created_at"),
                                "redemption_count": r.get::<i64, _>("redemption_count"),
                            })
                        })
                        .collect();
                    Ok(json!({ "count": items.len(), "items": items }))
                })
            },
        );
    }

    // 16 ── get_campaign_stats ─────────────────────────────────────────────────
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::read(
                "incentiveswift_get_campaign_stats",
                "Fetch aggregate stats (entries, referrals, wins, streaks) for one campaign in this workspace.",
            ),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let id = ctx.uuid_arg(&args, "id")?;
                    let row = sqlx::query("SELECT id, account_id FROM campaigns WHERE id = $1")
                        .bind(id)
                        .fetch_optional(&pool)
                        .await
                        .map_err(|e| ToolError::Unavailable(e.to_string()))?
                        .ok_or(ToolError::NotFound)?;
                    let owner = row.get::<Uuid, _>("account_id");
                    ctx.require_scope(owner)?;

                    let stats = sqlx::query(
                        "SELECT \
                            (SELECT count(*) FROM entries WHERE campaign_id = $1) AS entries, \
                            (SELECT count(*) FROM campaign_referrals WHERE campaign_id = $1) AS referrals_total, \
                            (SELECT count(*) FILTER (WHERE converted) FROM campaign_referrals WHERE campaign_id = $1) AS referrals_converted, \
                            (SELECT count(*) FROM campaign_wins WHERE campaign_id = $1) AS wins, \
                            (SELECT count(*) FROM campaign_streaks WHERE campaign_id = $1) AS streaks, \
                            (SELECT COALESCE(sum(total_entries), 0) FROM campaign_daily_stats WHERE campaign_id = $1) AS daily_total_entries, \
                            (SELECT COALESCE(sum(total_wins), 0) FROM campaign_daily_stats WHERE campaign_id = $1) AS daily_total_wins, \
                            (SELECT COALESCE(sum(total_redemptions), 0) FROM campaign_daily_stats WHERE campaign_id = $1) AS daily_total_redemptions",
                    )
                    .bind(id)
                    .fetch_one(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;

                    Ok(json!({
                        "campaign_id": id,
                        "entries": stats.get::<i64, _>("entries"),
                        "referrals_total": stats.get::<i64, _>("referrals_total"),
                        "referrals_converted": stats.get::<i64, _>("referrals_converted"),
                        "wins": stats.get::<i64, _>("wins"),
                        "streaks": stats.get::<i64, _>("streaks"),
                        "daily_total_entries": stats.get::<i64, _>("daily_total_entries"),
                        "daily_total_wins": stats.get::<i64, _>("daily_total_wins"),
                        "daily_total_redemptions": stats.get::<i64, _>("daily_total_redemptions"),
                    }))
                })
            },
        );
    }

    // 17 ── list_domains ───────────────────────────────────────────────────────
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::read(
                "incentiveswift_list_domains",
                "List custom domains (verification and SSL state) configured for this workspace.",
            ),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let limit = ctx.limit(args.get("limit"), 25, 200);
                    let account = account_id(&ctx)?;
                    // verification_token is deliberately NOT selected: it is a secret, never echoed.
                    let rows = sqlx::query(
                        "SELECT id, domain, target_type, verified_at, ssl_provisioned_at, \
                                is_active, created_at \
                         FROM custom_domains WHERE tenant_id = $1 ORDER BY created_at DESC LIMIT $2",
                    )
                    .bind(account)
                    .bind(limit)
                    .fetch_all(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    let items: Vec<Value> = rows
                        .iter()
                        .map(|r| {
                            json!({
                                "id": r.get::<Uuid, _>("id"),
                                "domain": r.get::<String, _>("domain"),
                                "target_type": r.get::<String, _>("target_type"),
                                "verified_at": r.get::<Option<DateTime<Utc>>, _>("verified_at"),
                                "ssl_provisioned_at": r.get::<Option<DateTime<Utc>>, _>("ssl_provisioned_at"),
                                "is_active": r.get::<bool, _>("is_active"),
                                "created_at": r.get::<DateTime<Utc>, _>("created_at"),
                            })
                        })
                        .collect();
                    Ok(json!({ "count": items.len(), "items": items }))
                })
            },
        );
    }

    // 18 ── list_email_suppressions ────────────────────────────────────────────
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::read(
                "incentiveswift_list_email_suppressions",
                "List suppressed email addresses and the reason for each, for this workspace.",
            ),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let limit = ctx.limit(args.get("limit"), 25, 200);
                    let account = account_id(&ctx)?;
                    let rows = sqlx::query(
                        "SELECT id, address, mail_class, reason, source, created_at \
                         FROM email_suppressions WHERE tenant_id = $1 ORDER BY created_at DESC LIMIT $2",
                    )
                    .bind(account)
                    .bind(limit)
                    .fetch_all(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    let items: Vec<Value> = rows
                        .iter()
                        .map(|r| {
                            json!({
                                "id": r.get::<Uuid, _>("id"),
                                "address": r.get::<String, _>("address"),
                                "mail_class": r.get::<String, _>("mail_class"),
                                "reason": r.get::<String, _>("reason"),
                                "source": r.get::<Option<String>, _>("source"),
                                "created_at": r.get::<DateTime<Utc>, _>("created_at"),
                            })
                        })
                        .collect();
                    Ok(json!({ "count": items.len(), "items": items }))
                })
            },
        );
    }

    // 19 ── list_email_templates ───────────────────────────────────────────────
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::read(
                "incentiveswift_list_email_templates",
                "List email templates available to this workspace (shared defaults plus this account's own).",
            ),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let limit = ctx.limit(args.get("limit"), 50, 500);
                    let account = account_id(&ctx)?;
                    // `aid` holds the account id; `aid IS NULL` rows are the shared system defaults.
                    let rows = sqlx::query(
                        "SELECT id, template_type, name, subject, is_default, created_at \
                         FROM email_templates WHERE aid = $1 OR aid IS NULL \
                         ORDER BY template_type, name LIMIT $2",
                    )
                    .bind(account)
                    .bind(limit)
                    .fetch_all(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    let items: Vec<Value> = rows
                        .iter()
                        .map(|r| {
                            json!({
                                "id": r.get::<Uuid, _>("id"),
                                "template_type": r.get::<String, _>("template_type"),
                                "name": r.get::<String, _>("name"),
                                "subject": r.get::<String, _>("subject"),
                                "is_default": r.get::<Option<bool>, _>("is_default"),
                                "created_at": r.get::<Option<DateTime<Utc>>, _>("created_at"),
                            })
                        })
                        .collect();
                    Ok(json!({ "count": items.len(), "items": items }))
                })
            },
        );
    }

    // 20 ── list_iqs_funnels (+submissions) ────────────────────────────────────
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::read(
                "incentiveswift_list_iqs_funnels",
                "List IQS funnels with their recent submissions (score, outcome) for this workspace.",
            ),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let limit = ctx.limit(args.get("limit"), 25, 200);
                    let account = account_id(&ctx)?;
                    let funnel_rows = sqlx::query(
                        "SELECT id, name, funnel_type, status, slug, response_count, created_at \
                         FROM iqs_funnels WHERE account_id = $1 ORDER BY created_at DESC LIMIT $2",
                    )
                    .bind(account)
                    .bind(limit)
                    .fetch_all(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    let mut items = Vec::with_capacity(funnel_rows.len());
                    for fu in &funnel_rows {
                        let fid = fu.get::<Uuid, _>("id");
                        let sub_rows = sqlx::query(
                            "SELECT id, contact_id, total_score, outcome, classification, \
                                    created_at, answers \
                             FROM iqs_submissions WHERE funnel_id = $1 \
                             ORDER BY created_at DESC LIMIT 20",
                        )
                        .bind(fid)
                        .fetch_all(&pool)
                        .await
                        .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                        let submissions: Vec<Value> = sub_rows
                            .iter()
                            .map(|s| {
                                json!({
                                    "id": s.get::<Uuid, _>("id"),
                                    "contact_id": s.get::<Uuid, _>("contact_id"),
                                    "total_score": s.get::<i32, _>("total_score"),
                                    "outcome": s.get::<Option<String>, _>("outcome"),
                                    "classification": s.get::<Option<String>, _>("classification"),
                                    "created_at": s.get::<DateTime<Utc>, _>("created_at"),
                                    "answers": s.get::<Value, _>("answers"),
                                })
                            })
                            .collect();
                        items.push(json!({
                            "id": fu.get::<Uuid, _>("id"),
                            "name": fu.get::<String, _>("name"),
                            "funnel_type": fu.get::<String, _>("funnel_type"),
                            "status": fu.get::<String, _>("status"),
                            "slug": fu.get::<String, _>("slug"),
                            "response_count": fu.get::<i32, _>("response_count"),
                            "created_at": fu.get::<DateTime<Utc>, _>("created_at"),
                            "submissions": submissions,
                        }));
                    }
                    Ok(json!({ "count": items.len(), "items": items }))
                })
            },
        );
    }

    // 21 ── list_business_deals (+pledges) ─────────────────────────────────────
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::read(
                "incentiveswift_list_business_deals",
                "List business loyalty deals and the pledges businesses made, for this workspace.",
            ),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let limit = ctx.limit(args.get("limit"), 25, 200);
                    let account = account_id(&ctx)?;
                    let deal_rows = sqlx::query(
                        "SELECT id, business_id, business_name, program_id, deal_type, deal_value, \
                                deal_description, min_purchase, points_required, is_active, \
                                valid_from, valid_until, redemptions_limit, current_redemptions, \
                                created_at \
                         FROM business_loyalty_deals WHERE owner_account_id = $1 \
                         ORDER BY created_at DESC LIMIT $2",
                    )
                    .bind(account)
                    .bind(limit)
                    .fetch_all(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    let deals: Vec<Value> = deal_rows
                        .iter()
                        .map(|r| {
                            json!({
                                "id": r.get::<Uuid, _>("id"),
                                "business_id": r.get::<Uuid, _>("business_id"),
                                "business_name": r.get::<String, _>("business_name"),
                                "program_id": r.get::<Uuid, _>("program_id"),
                                "deal_type": r.get::<String, _>("deal_type"),
                                "deal_value": r.get::<String, _>("deal_value"),
                                "deal_description": r.get::<Option<String>, _>("deal_description"),
                                "min_purchase": r.get::<Option<String>, _>("min_purchase"),
                                "points_required": r.get::<i32, _>("points_required"),
                                "is_active": r.get::<bool, _>("is_active"),
                                "valid_from": r.get::<Option<DateTime<Utc>>, _>("valid_from"),
                                "valid_until": r.get::<Option<DateTime<Utc>>, _>("valid_until"),
                                "redemptions_limit": r.get::<i32, _>("redemptions_limit"),
                                "current_redemptions": r.get::<i32, _>("current_redemptions"),
                                "created_at": r.get::<DateTime<Utc>, _>("created_at"),
                            })
                        })
                        .collect();
                    let pledge_rows = sqlx::query(
                        "SELECT id, business_id, business_name, business_phone, offer_type, \
                                offer_value, offer_description, status, min_purchase, valid_until, \
                                created_at \
                         FROM business_pledges \
                         WHERE campaign_id IN (SELECT id FROM campaigns WHERE account_id = $1) \
                         ORDER BY created_at DESC LIMIT 20",
                    )
                    .bind(account)
                    .fetch_all(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    let pledges: Vec<Value> = pledge_rows
                        .iter()
                        .map(|r| {
                            json!({
                                "id": r.get::<Uuid, _>("id"),
                                "business_id": r.get::<Uuid, _>("business_id"),
                                "business_name": r.get::<String, _>("business_name"),
                                "business_phone": r.get::<Option<String>, _>("business_phone"),
                                "offer_type": r.get::<String, _>("offer_type"),
                                "offer_value": r.get::<Option<String>, _>("offer_value"),
                                "offer_description": r.get::<Option<String>, _>("offer_description"),
                                "status": r.get::<Option<String>, _>("status"),
                                "min_purchase": r.get::<Option<String>, _>("min_purchase"),
                                "valid_until": r.get::<Option<DateTime<Utc>>, _>("valid_until"),
                                "created_at": r.get::<Option<DateTime<Utc>>, _>("created_at"),
                            })
                        })
                        .collect();
                    Ok(json!({
                        "deals": { "count": deals.len(), "items": deals },
                        "pledges": { "count": pledges.len(), "items": pledges },
                    }))
                })
            },
        );
    }

    // 22 ── list_calendar_events ───────────────────────────────────────────────
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::read(
                "incentiveswift_list_calendar_events",
                "List calendar events (title, time, status) for this workspace.",
            ),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let limit = ctx.limit(args.get("limit"), 25, 200);
                    let account = account_id(&ctx)?;
                    let rows = sqlx::query(
                        "SELECT id, title, description, location, event_type, starts_at, ends_at, \
                                all_day, status, created_at \
                         FROM calendar_events WHERE tenant_id = $1 ORDER BY starts_at DESC LIMIT $2",
                    )
                    .bind(account)
                    .bind(limit)
                    .fetch_all(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    let items: Vec<Value> = rows
                        .iter()
                        .map(|r| {
                            json!({
                                "id": r.get::<Uuid, _>("id"),
                                "title": r.get::<String, _>("title"),
                                "description": r.get::<Option<String>, _>("description"),
                                "location": r.get::<Option<String>, _>("location"),
                                "event_type": r.get::<Option<String>, _>("event_type"),
                                "starts_at": r.get::<DateTime<Utc>, _>("starts_at"),
                                "ends_at": r.get::<Option<DateTime<Utc>>, _>("ends_at"),
                                "all_day": r.get::<bool, _>("all_day"),
                                "status": r.get::<String, _>("status"),
                                "created_at": r.get::<DateTime<Utc>, _>("created_at"),
                            })
                        })
                        .collect();
                    Ok(json!({ "count": items.len(), "items": items }))
                })
            },
        );
    }

    // 23 ── list_reviews ───────────────────────────────────────────────────────
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::read(
                "incentiveswift_list_reviews",
                "List customer reviews (rating, status) for this workspace, with body excerpted.",
            ),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let limit = ctx.limit(args.get("limit"), 25, 200);
                    let account = account_id(&ctx)?;
                    let rows = sqlx::query(
                        "SELECT id, campaign_id, contact_id, rating, title, body, reviewer_name, \
                                status, created_at \
                         FROM reviews WHERE tenant_id = $1 ORDER BY created_at DESC LIMIT $2",
                    )
                    .bind(account)
                    .bind(limit)
                    .fetch_all(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    let items: Vec<Value> = rows
                        .iter()
                        .map(|r| {
                            json!({
                                "id": r.get::<Uuid, _>("id"),
                                "campaign_id": r.get::<Option<Uuid>, _>("campaign_id"),
                                "contact_id": r.get::<Option<Uuid>, _>("contact_id"),
                                "rating": r.get::<i32, _>("rating"),
                                "title": r.get::<Option<String>, _>("title"),
                                "body_excerpt": excerpt(r.get::<Option<String>, _>("body").as_deref()),
                                "reviewer_name": r.get::<Option<String>, _>("reviewer_name"),
                                "status": r.get::<String, _>("status"),
                                "created_at": r.get::<DateTime<Utc>, _>("created_at"),
                            })
                        })
                        .collect();
                    Ok(json!({ "count": items.len(), "items": items }))
                })
            },
        );
    }

    // 24 ── list_leads ─────────────────────────────────────────────────────────
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::read(
                "incentiveswift_list_leads",
                "List leads (name, email, phone, status) captured for this workspace.",
            ),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let limit = ctx.limit(args.get("limit"), 25, 200);
                    let account = account_id(&ctx)?;
                    let rows = sqlx::query(
                        "SELECT id, name, email, phone, status, created_at \
                         FROM leads WHERE tenant_id = $1 ORDER BY created_at DESC LIMIT $2",
                    )
                    .bind(account)
                    .bind(limit)
                    .fetch_all(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    let items: Vec<Value> = rows
                        .iter()
                        .map(|r| {
                            json!({
                                "id": r.get::<Uuid, _>("id"),
                                "name": r.get::<Option<String>, _>("name"),
                                "email": r.get::<Option<String>, _>("email"),
                                "phone": r.get::<Option<String>, _>("phone"),
                                "status": r.get::<Option<String>, _>("status"),
                                "created_at": r.get::<Option<DateTime<Utc>>, _>("created_at"),
                            })
                        })
                        .collect();
                    Ok(json!({ "count": items.len(), "items": items }))
                })
            },
        );
    }

    // 25 ── list_earn_channels (+clicks) ───────────────────────────────────────
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::read(
                "incentiveswift_list_earn_channels",
                "List earn channels (points per click, redirect) with click totals, for this workspace.",
            ),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let limit = ctx.limit(args.get("limit"), 25, 200);
                    let account = account_id(&ctx)?;
                    // expected_answer / approval_notes are deliberately NOT selected (secrets / internal).
                    let rows = sqlx::query(
                        "SELECT id, campaign_id, channel_code, label, description, \
                                points_per_click, max_clicks_per_contact, redirect_url, \
                                verification_type, is_active, created_at, \
                                (SELECT count(*) FROM earn_click_log cl \
                                 WHERE cl.channel_id = ec.id) AS click_count \
                         FROM earn_channels ec WHERE account_id = $1 \
                         ORDER BY created_at DESC LIMIT $2",
                    )
                    .bind(account)
                    .bind(limit)
                    .fetch_all(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    let items: Vec<Value> = rows
                        .iter()
                        .map(|r| {
                            json!({
                                "id": r.get::<Uuid, _>("id"),
                                "campaign_id": r.get::<Uuid, _>("campaign_id"),
                                "channel_code": r.get::<String, _>("channel_code"),
                                "label": r.get::<String, _>("label"),
                                "description": r.get::<String, _>("description"),
                                "points_per_click": r.get::<i32, _>("points_per_click"),
                                "max_clicks_per_contact": r.get::<i32, _>("max_clicks_per_contact"),
                                "redirect_url": r.get::<String, _>("redirect_url"),
                                "verification_type": r.get::<String, _>("verification_type"),
                                "is_active": r.get::<bool, _>("is_active"),
                                "created_at": r.get::<DateTime<Utc>, _>("created_at"),
                                "click_count": r.get::<i64, _>("click_count"),
                            })
                        })
                        .collect();
                    Ok(json!({ "count": items.len(), "items": items }))
                })
            },
        );
    }

    // 26 ── list_streaks_wins ──────────────────────────────────────────────────
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::read(
                "incentiveswift_list_streaks_wins",
                "List campaign streaks and wins (prize label, redeemed) for this workspace.",
            ),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let limit = ctx.limit(args.get("limit"), 25, 200);
                    let account = account_id(&ctx)?;
                    let streak_rows = sqlx::query(
                        "SELECT s.contact_id, s.campaign_id, s.loss_streak, s.total_spins, \
                                s.last_spin_at \
                         FROM campaign_streaks s \
                         JOIN campaigns c ON c.id = s.campaign_id \
                         WHERE c.account_id = $1 ORDER BY s.last_spin_at DESC LIMIT $2",
                    )
                    .bind(account)
                    .bind(limit)
                    .fetch_all(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    let streaks: Vec<Value> = streak_rows
                        .iter()
                        .map(|r| {
                            json!({
                                "contact_id": r.get::<Uuid, _>("contact_id"),
                                "campaign_id": r.get::<Uuid, _>("campaign_id"),
                                "loss_streak": r.get::<i32, _>("loss_streak"),
                                "total_spins": r.get::<i32, _>("total_spins"),
                                "last_spin_at": r.get::<DateTime<Utc>, _>("last_spin_at"),
                            })
                        })
                        .collect();
                    let win_rows = sqlx::query(
                        "SELECT w.id, w.campaign_id, w.contact_id, w.prize_id, w.prize_label, \
                                w.prize_type, w.streak_when_won, w.was_pity, w.redeemed, \
                                w.redeemed_at, w.created_at \
                         FROM campaign_wins w \
                         JOIN campaigns c ON c.id = w.campaign_id \
                         WHERE c.account_id = $1 ORDER BY w.created_at DESC LIMIT $2",
                    )
                    .bind(account)
                    .bind(limit)
                    .fetch_all(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    let wins: Vec<Value> = win_rows
                        .iter()
                        .map(|r| {
                            json!({
                                "id": r.get::<Uuid, _>("id"),
                                "campaign_id": r.get::<Uuid, _>("campaign_id"),
                                "contact_id": r.get::<Uuid, _>("contact_id"),
                                "prize_id": r.get::<String, _>("prize_id"),
                                "prize_label": r.get::<String, _>("prize_label"),
                                "prize_type": r.get::<String, _>("prize_type"),
                                "streak_when_won": r.get::<i32, _>("streak_when_won"),
                                "was_pity": r.get::<bool, _>("was_pity"),
                                "redeemed": r.get::<bool, _>("redeemed"),
                                "redeemed_at": r.get::<Option<DateTime<Utc>>, _>("redeemed_at"),
                                "created_at": r.get::<DateTime<Utc>, _>("created_at"),
                            })
                        })
                        .collect();
                    Ok(json!({
                        "streaks": { "count": streaks.len(), "items": streaks },
                        "wins": { "count": wins.len(), "items": wins },
                    }))
                })
            },
        );
    }

    // 27 ── get_point_treasury ─────────────────────────────────────────────────
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::read(
                "incentiveswift_get_point_treasury",
                "Report the point treasury totals (issued, redeemed, liability, float) for the network.",
            ),
            move |_ctx, _args| {
                let pool = pool.clone();
                Box::pin(async move {
                    // point_treasury is a single network-wide ledger (no per-account axis), so there
                    // is exactly one row and no cross-tenant data to leak.
                    let row = sqlx::query(
                        "SELECT id, total_points_issued, total_points_redeemed, \
                                total_revenue_collected::text AS total_revenue_collected, \
                                total_reimbursements_paid::text AS total_reimbursements_paid, \
                                outstanding_liability::text AS outstanding_liability, \
                                minimum_float::text AS minimum_float, updated_at, on_float_breach, \
                                float_breaches, last_breach_at, \
                                float_coverage_pct::text AS float_coverage_pct, \
                                float_burn_months::text AS float_burn_months \
                         FROM point_treasury LIMIT 1",
                    )
                    .fetch_optional(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?
                    .ok_or(ToolError::NotFound)?;
                    Ok(json!({
                        "id": row.get::<Uuid, _>("id"),
                        "total_points_issued": row.get::<Option<i64>, _>("total_points_issued"),
                        "total_points_redeemed": row.get::<Option<i64>, _>("total_points_redeemed"),
                        "total_revenue_collected": row.get::<Option<String>, _>("total_revenue_collected"),
                        "total_reimbursements_paid": row.get::<Option<String>, _>("total_reimbursements_paid"),
                        "outstanding_liability": row.get::<Option<String>, _>("outstanding_liability"),
                        "minimum_float": row.get::<Option<String>, _>("minimum_float"),
                        "updated_at": row.get::<Option<DateTime<Utc>>, _>("updated_at"),
                        "on_float_breach": row.get::<String, _>("on_float_breach"),
                        "float_breaches": row.get::<i32, _>("float_breaches"),
                        "last_breach_at": row.get::<Option<DateTime<Utc>>, _>("last_breach_at"),
                        "float_coverage_pct": row.get::<String, _>("float_coverage_pct"),
                        "float_burn_months": row.get::<String, _>("float_burn_months"),
                    }))
                })
            },
        );
    }

    reg
}

// ─────────────────────────────────────────────────────────────────────────────
// Door construction + the axum handler
// ─────────────────────────────────────────────────────────────────────────────

/// Build the door against a live pool. Fails closed: a bad registry or config refuses startup.
pub fn build_door(pool: PgPool) -> Result<Door, mcp_door::transport::DoorError> {
    Door::builder(
        DOOR_NAME,
        env!("CARGO_PKG_VERSION"),
        registry(pool.clone()),
        Arc::new(ApiKeyResolver { pool }),
    )
    .audit(AUDIT_PATH)
    .instructions(
        "Read-only access to your IncentiveSwift data: campaigns, participants, loyalty, points, \
         offers, messages and tickets. Scoped to the account that owns the API key.",
    )
    .build()
}

/// The `/mcp` route handler. The door itself is the credential boundary for this path (it answers
/// 401 with the JSON-RPC unauthorized code), so it is mounted as a PUBLIC route.
pub async fn handle(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    state.mcp_door.dispatch(&headers, &body).await
}
