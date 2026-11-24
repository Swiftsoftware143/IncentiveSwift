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
