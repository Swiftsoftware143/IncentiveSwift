//! IncentiveSwift's MCP door: read + write tools over a customer's own data.
//!
//! Credential: `api_keys` (`prefix`, `is_active`, `expires_at`). The trap this door resolves is
//! that the key table's `tenant_id` column is an `accounts.id` (FK `tenant_id -> accounts(id)`)
//! while the data tables split between an `account_id` column (campaigns, credit_transactions,
//! inbound_messages, tags, loyalty_programs) and a `tenant_id` column (offers, support_tickets) —
//! BOTH hold the account id in this app. The resolver maps the key row to `accounts.id` via its
//! `tenant_id` (FK-backed) and every tool filters on that single id.
//!
//! Read tools plus create/update/archive write tools, all scoped to the key's own account (a
//! foreign id answers `not_found`; nothing is ever hard-deleted without an explicit flag). No
//! cross-workspace or platform-admin tool is exposed — those actions stay on the admin surface.
//!
//! A key maps to exactly ONE account, so every read and write is a fully parameterized `= $1` on
//! that id — no SQL string is ever assembled from caller input or a scope clause (the fleet gate
//! forbids it).

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
/// `tools/list` page size: the WHOLE inventory in one answer. The transport default is 50, which
/// splits this door's 111 tools across three pages — invisible to any client that does not follow
/// `nextCursor`, and a one-page reading then sees 50 of 111 and files a false "61 tools declared
/// but never served" (the same trap t_8d54e37e fixed on MissedCall). A page big enough for the
/// inventory removes the trap; `one_page_lists_every_tool` fails the build the moment the
/// inventory outgrows it.
pub const TOOLS_PAGE_SIZE: usize = 200;

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

/// The slug for a campaign or funnel created through the door: a slugified name plus a short
/// random suffix, so two same-named rows never collide on their unique slug column.
fn slugify(name: &str) -> String {
    let base: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let base = base.trim_matches('-').to_string();
    let suffix = &Uuid::new_v4().to_string()[..6];
    if base.is_empty() {
        format!("campaign-{suffix}")
    } else {
        format!("{base}-{suffix}")
    }
}

/// `args[key]` when it is a string, otherwise `None`.
fn opt_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(|v| v.as_str())
}

/// `args[key]` as a bool, defaulting to `false`.
fn opt_bool(args: &Value, key: &str) -> bool {
    args.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
}

/// `args[key]` as a string array (`text[]`), or `None` when absent/non-array.
fn str_array(args: &Value, key: &str) -> Option<Vec<String>> {
    args.get(key).and_then(|v| v.as_array()).map(|arr| {
        arr.iter()
            .filter_map(|v| v.as_str().map(|s| s.trim().to_string()))
            .filter(|s| !s.is_empty())
            .collect()
    })
}

/// `args[key]` as an RFC3339 timestamp, or `None` when absent.
fn opt_ts(args: &Value, key: &str) -> Result<Option<DateTime<Utc>>, ToolError> {
    match opt_str(args, key) {
        None => Ok(None),
        Some(s) => DateTime::parse_from_rfc3339(s)
            .map(|d| Some(d.with_timezone(&Utc)))
            .map_err(|e| ToolError::Invalid(format!("{key} must be an RFC3339 timestamp: {e}"))),
    }
}

/// A scoped ownership probe used on the CREATE path for a sub-entity: the SQL must return the
/// parent's id only when `$1` (the parent id) names a row the caller's `$2` account owns. A hit
/// means "owned"; a miss means the parent is foreign or absent — both answer `not_found`.
async fn scoped_owned(
    pool: &PgPool,
    probe: &str,
    id: Uuid,
    account: Uuid,
) -> Result<bool, ToolError> {
    let hit: Option<Uuid> = sqlx::query_scalar(probe)
        .bind(id)
        .bind(account)
        .fetch_optional(pool)
        .await
        .map_err(|e| ToolError::Unavailable(e.to_string()))?;
    Ok(hit.is_some())
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

    // ── W1 campaigns: create / update / archive ────────────────────────────────
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_create_campaign",
                "Create a new campaign in this workspace. Requires name, type and tag_namespace.",
            )
            .schema(json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "type": {"type": "string"},
                    "tag_namespace": {"type": "string"},
                    "config": {"type": "object"},
                    "delivery_method": {"type": "string"}
                },
                "required": ["name", "type", "tag_namespace"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let name = ctx.str_arg(&args, "name")?.trim().to_string();
                    let ctype = ctx.str_arg(&args, "type")?.trim().to_string();
                    let namespace = ctx.str_arg(&args, "tag_namespace")?.trim().to_string();
                    if name.is_empty() {
                        return Err(ToolError::Invalid("name is required".into()));
                    }
                    if namespace.is_empty() {
                        return Err(ToolError::Invalid("tag_namespace is required".into()));
                    }
                    if !crate::db::campaigns::VALID_MECHANIC_TYPES.contains(&ctype.as_str()) {
                        return Err(ToolError::Invalid(format!(
                            "unknown campaign type '{ctype}'"
                        )));
                    }
                    let config = args.get("config").cloned().unwrap_or_else(|| json!({}));
                    let delivery_method = opt_str(&args, "delivery_method")
                        .unwrap_or("webhook")
                        .to_string();
                    let id = Uuid::new_v4();
                    let slug = slugify(&name);
                    sqlx::query(
                        "INSERT INTO campaigns (id, account_id, name, slug, type, status, config, \
                         tag_namespace, outcome_tags, delivery_method, delivery_config) \
                         VALUES ($1, $2, $3, $4, $5, 'draft', $6, $7, $8, $9, $10)",
                    )
                    .bind(id)
                    .bind(account)
                    .bind(&name)
                    .bind(&slug)
                    .bind(&ctype)
                    .bind(&config)
                    .bind(&namespace)
                    .bind(json!({}))
                    .bind(&delivery_method)
                    .bind(json!({}))
                    .execute(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    Ok(json!({ "id": id, "name": name, "slug": slug, "type": ctype, "status": "draft" }))
                })
            },
        );
    }
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_update_campaign",
                "Update a campaign's name, status, config or delivery method by id, scoped to this workspace.",
            )
            .schema(json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "name": {"type": "string"},
                    "status": {"type": "string"},
                    "config": {"type": "object"},
                    "delivery_method": {"type": "string"}
                },
                "required": ["id"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let id = ctx.uuid_arg(&args, "id")?;
                    let name = opt_str(&args, "name").map(|s| s.trim().to_string());
                    let status = opt_str(&args, "status").map(|s| s.trim().to_string());
                    let config = args.get("config").cloned();
                    let delivery_method = opt_str(&args, "delivery_method").map(|s| s.to_string());
                    let res = sqlx::query(
                        "UPDATE campaigns SET name = COALESCE($2, name), status = COALESCE($3, status), \
                         config = COALESCE($4, config), delivery_method = COALESCE($5, delivery_method) \
                         WHERE id = $1 AND account_id = $6",
                    )
                    .bind(id)
                    .bind(name)
                    .bind(status)
                    .bind(config)
                    .bind(delivery_method)
                    .bind(account)
                    .execute(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    if res.rows_affected() == 0 {
                        return Err(ToolError::NotFound);
                    }
                    Ok(json!({ "id": id, "updated": true }))
                })
            },
        );
    }
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_archive_campaign",
                "Archive a campaign by id (soft) or hard-delete it with the hard_delete flag.",
            )
            .schema(json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "hard_delete": {"type": "boolean"}
                },
                "required": ["id"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let id = ctx.uuid_arg(&args, "id")?;
                    let res = if opt_bool(&args, "hard_delete") {
                        sqlx::query("DELETE FROM campaigns WHERE id = $1 AND account_id = $2")
                            .bind(id)
                            .bind(account)
                            .execute(&pool)
                            .await
                            .map_err(|e| ToolError::Unavailable(e.to_string()))?
                    } else {
                        sqlx::query(
                            "UPDATE campaigns SET status = 'archived' WHERE id = $1 AND account_id = $2",
                        )
                        .bind(id)
                        .bind(account)
                        .execute(&pool)
                        .await
                        .map_err(|e| ToolError::Unavailable(e.to_string()))?
                    };
                    if res.rows_affected() == 0 {
                        return Err(ToolError::NotFound);
                    }
                    Ok(json!({ "id": id, "archived": true }))
                })
            },
        );
    }

    // ── W2 offers: create / update / archive ───────────────────────────────────
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_create_offer",
                "Create a promotional offer (name, discount percent, dollar cap) for this workspace.",
            )
            .schema(json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "discount_percent": {"type": "integer"},
                    "cap_dollars": {"type": "integer"}
                },
                "required": ["name", "discount_percent", "cap_dollars"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let name = ctx.str_arg(&args, "name")?.trim().to_string();
                    if name.is_empty() {
                        return Err(ToolError::Invalid("name is required".into()));
                    }
                    let discount = args
                        .get("discount_percent")
                        .and_then(|v| v.as_i64())
                        .map(|v| v as i32)
                        .ok_or_else(|| ToolError::Invalid("discount_percent must be an integer".into()))?;
                    let cap = args
                        .get("cap_dollars")
                        .and_then(|v| v.as_i64())
                        .map(|v| v as i32)
                        .ok_or_else(|| ToolError::Invalid("cap_dollars must be an integer".into()))?;
                    let id = Uuid::new_v4();
                    sqlx::query(
                        "INSERT INTO offers (id, tenant_id, name, discount_percent, cap_dollars) \
                         VALUES ($1, $2, $3, $4, $5)",
                    )
                    .bind(id)
                    .bind(account)
                    .bind(&name)
                    .bind(discount)
                    .bind(cap)
                    .execute(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    Ok(json!({ "id": id, "name": name, "discount_percent": discount, "cap_dollars": cap }))
                })
            },
        );
    }
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_update_offer",
                "Update an offer's name, discount, cap or active flag by id, scoped to this workspace.",
            )
            .schema(json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "name": {"type": "string"},
                    "discount_percent": {"type": "integer"},
                    "cap_dollars": {"type": "integer"},
                    "active": {"type": "boolean"}
                },
                "required": ["id"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let id = ctx.uuid_arg(&args, "id")?;
                    let name = opt_str(&args, "name").map(|s| s.trim().to_string());
                    let discount = args.get("discount_percent").and_then(|v| v.as_i64()).map(|v| v as i32);
                    let cap = args.get("cap_dollars").and_then(|v| v.as_i64()).map(|v| v as i32);
                    let active = args.get("active").and_then(|v| v.as_bool());
                    let res = sqlx::query(
                        "UPDATE offers SET name = COALESCE($2, name), \
                         discount_percent = COALESCE($3, discount_percent), \
                         cap_dollars = COALESCE($4, cap_dollars), active = COALESCE($5, active) \
                         WHERE id = $1 AND tenant_id = $6",
                    )
                    .bind(id)
                    .bind(name)
                    .bind(discount)
                    .bind(cap)
                    .bind(active)
                    .bind(account)
                    .execute(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    if res.rows_affected() == 0 {
                        return Err(ToolError::NotFound);
                    }
                    Ok(json!({ "id": id, "updated": true }))
                })
            },
        );
    }
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_archive_offer",
                "Archive an offer by id (soft) or hard-delete it with the hard_delete flag.",
            )
            .schema(json!({
                "type": "object",
                "properties": {"id": {"type": "string"}, "hard_delete": {"type": "boolean"}},
                "required": ["id"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let id = ctx.uuid_arg(&args, "id")?;
                    let res = if opt_bool(&args, "hard_delete") {
                        sqlx::query("DELETE FROM offers WHERE id = $1 AND tenant_id = $2")
                            .bind(id)
                            .bind(account)
                            .execute(&pool)
                            .await
                            .map_err(|e| ToolError::Unavailable(e.to_string()))?
                    } else {
                        sqlx::query(
                            "UPDATE offers SET active = false WHERE id = $1 AND tenant_id = $2",
                        )
                        .bind(id)
                        .bind(account)
                        .execute(&pool)
                        .await
                        .map_err(|e| ToolError::Unavailable(e.to_string()))?
                    };
                    if res.rows_affected() == 0 {
                        return Err(ToolError::NotFound);
                    }
                    Ok(json!({ "id": id, "archived": true }))
                })
            },
        );
    }

    // ── W3 loyalty_programs: create / update / archive ────────────────────────
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_create_loyalty_program",
                "Create a loyalty program (name, optional currency name and points-per-checkin).",
            )
            .schema(json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "currency_name": {"type": "string"},
                    "points_per_checkin": {"type": "integer"}
                },
                "required": ["name"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let name = ctx.str_arg(&args, "name")?.trim().to_string();
                    if name.is_empty() {
                        return Err(ToolError::Invalid("name is required".into()));
                    }
                    let currency = opt_str(&args, "currency_name")
                        .unwrap_or("Points")
                        .to_string();
                    let points = args
                        .get("points_per_checkin")
                        .and_then(|v| v.as_i64())
                        .map(|v| v as i32)
                        .unwrap_or(10);
                    let id = Uuid::new_v4();
                    sqlx::query(
                        "INSERT INTO loyalty_programs (id, account_id, name, currency_name, points_per_checkin) \
                         VALUES ($1, $2, $3, $4, $5)",
                    )
                    .bind(id)
                    .bind(account)
                    .bind(&name)
                    .bind(&currency)
                    .bind(points)
                    .execute(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    Ok(json!({ "id": id, "name": name, "currency_name": currency, "is_active": true }))
                })
            },
        );
    }
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_update_loyalty_program",
                "Update a loyalty program's name, currency or points-per-checkin by id, scoped to this workspace.",
            )
            .schema(json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "name": {"type": "string"},
                    "currency_name": {"type": "string"},
                    "points_per_checkin": {"type": "integer"}
                },
                "required": ["id"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let id = ctx.uuid_arg(&args, "id")?;
                    let name = opt_str(&args, "name").map(|s| s.trim().to_string());
                    let currency = opt_str(&args, "currency_name").map(|s| s.to_string());
                    let points = args.get("points_per_checkin").and_then(|v| v.as_i64()).map(|v| v as i32);
                    let res = sqlx::query(
                        "UPDATE loyalty_programs lp SET name = COALESCE($2, lp.name), \
                         currency_name = COALESCE($3, lp.currency_name), \
                         points_per_checkin = COALESCE($4, lp.points_per_checkin) \
                         WHERE lp.id = $1 AND (lp.account_id = $5 \
                            OR EXISTS (SELECT 1 FROM campaigns c WHERE c.id = lp.campaign_id AND c.account_id = $5) \
                            OR EXISTS (SELECT 1 FROM campaigns fwd WHERE fwd.loyalty_program_id = lp.id AND fwd.account_id = $5))",
                    )
                    .bind(id)
                    .bind(name)
                    .bind(currency)
                    .bind(points)
                    .bind(account)
                    .execute(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    if res.rows_affected() == 0 {
                        return Err(ToolError::NotFound);
                    }
                    Ok(json!({ "id": id, "updated": true }))
                })
            },
        );
    }
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_archive_loyalty_program",
                "Archive a loyalty program by id (soft) or hard-delete it with the hard_delete flag.",
            )
            .schema(json!({
                "type": "object",
                "properties": {"id": {"type": "string"}, "hard_delete": {"type": "boolean"}},
                "required": ["id"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let id = ctx.uuid_arg(&args, "id")?;
                    let res = if opt_bool(&args, "hard_delete") {
                        sqlx::query(
                            "DELETE FROM loyalty_programs lp WHERE lp.id = $1 AND \
                             (lp.account_id = $2 \
                              OR EXISTS (SELECT 1 FROM campaigns c WHERE c.id = lp.campaign_id AND c.account_id = $2) \
                              OR EXISTS (SELECT 1 FROM campaigns fwd WHERE fwd.loyalty_program_id = lp.id AND fwd.account_id = $2))",
                        )
                        .bind(id)
                        .bind(account)
                        .execute(&pool)
                        .await
                        .map_err(|e| ToolError::Unavailable(e.to_string()))?
                    } else {
                        sqlx::query(
                            "UPDATE loyalty_programs lp SET is_active = false WHERE lp.id = $1 AND \
                             (lp.account_id = $2 \
                              OR EXISTS (SELECT 1 FROM campaigns c WHERE c.id = lp.campaign_id AND c.account_id = $2) \
                              OR EXISTS (SELECT 1 FROM campaigns fwd WHERE fwd.loyalty_program_id = lp.id AND fwd.account_id = $2))",
                        )
                        .bind(id)
                        .bind(account)
                        .execute(&pool)
                        .await
                        .map_err(|e| ToolError::Unavailable(e.to_string()))?
                    };
                    if res.rows_affected() == 0 {
                        return Err(ToolError::NotFound);
                    }
                    Ok(json!({ "id": id, "archived": true }))
                })
            },
        );
    }

    // ── W4 loyalty_reward_tiers: create / update / archive ────────────────────
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_create_reward_tier",
                "Create a reward tier on a loyalty program this workspace owns (name, points, reward tag).",
            )
            .schema(json!({
                "type": "object",
                "properties": {
                    "program_id": {"type": "string"},
                    "name": {"type": "string"},
                    "points_required": {"type": "integer"},
                    "reward_tag": {"type": "string"}
                },
                "required": ["program_id", "name", "points_required", "reward_tag"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let program_id = ctx.uuid_arg(&args, "program_id")?;
                    let owned = scoped_owned(
                        &pool,
                        "SELECT lp.id FROM loyalty_programs lp WHERE lp.id = $1 AND \
                         (lp.account_id = $2 OR EXISTS (SELECT 1 FROM campaigns c WHERE c.id = lp.campaign_id AND c.account_id = $2) \
                          OR EXISTS (SELECT 1 FROM campaigns fwd WHERE fwd.loyalty_program_id = lp.id AND fwd.account_id = $2))",
                        program_id,
                        account,
                    )
                    .await?;
                    if !owned {
                        return Err(ToolError::NotFound);
                    }
                    let name = ctx.str_arg(&args, "name")?.trim().to_string();
                    let points = args
                        .get("points_required")
                        .and_then(|v| v.as_i64())
                        .map(|v| v as i32)
                        .ok_or_else(|| ToolError::Invalid("points_required must be an integer".into()))?;
                    let tag = ctx.str_arg(&args, "reward_tag")?.trim().to_string();
                    let id = Uuid::new_v4();
                    sqlx::query(
                        "INSERT INTO loyalty_reward_tiers (id, program_id, name, points_required, reward_tag) \
                         VALUES ($1, $2, $3, $4, $5)",
                    )
                    .bind(id)
                    .bind(program_id)
                    .bind(&name)
                    .bind(points)
                    .bind(&tag)
                    .execute(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    Ok(json!({ "id": id, "program_id": program_id, "name": name, "points_required": points }))
                })
            },
        );
    }
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_update_reward_tier",
                "Update a reward tier's name, points, tag or approval flag by id, scoped to this workspace.",
            )
            .schema(json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "name": {"type": "string"},
                    "points_required": {"type": "integer"},
                    "reward_tag": {"type": "string"},
                    "requires_approval": {"type": "boolean"}
                },
                "required": ["id"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let id = ctx.uuid_arg(&args, "id")?;
                    let name = opt_str(&args, "name").map(|s| s.trim().to_string());
                    let points = args.get("points_required").and_then(|v| v.as_i64()).map(|v| v as i32);
                    let tag = opt_str(&args, "reward_tag").map(|s| s.trim().to_string());
                    let approval = args.get("requires_approval").and_then(|v| v.as_bool());
                    let res = sqlx::query(
                        "UPDATE loyalty_reward_tiers rt SET name = COALESCE($2, rt.name), \
                         points_required = COALESCE($3, rt.points_required), \
                         reward_tag = COALESCE($4, rt.reward_tag), \
                         requires_approval = COALESCE($5, rt.requires_approval) \
                         WHERE rt.id = $1 AND rt.program_id IN (SELECT lp.id FROM loyalty_programs lp \
                            WHERE lp.account_id = $6 OR EXISTS (SELECT 1 FROM campaigns c WHERE c.id = lp.campaign_id AND c.account_id = $6) \
                            OR EXISTS (SELECT 1 FROM campaigns fwd WHERE fwd.loyalty_program_id = lp.id AND fwd.account_id = $6))",
                    )
                    .bind(id)
                    .bind(name)
                    .bind(points)
                    .bind(tag)
                    .bind(approval)
                    .bind(account)
                    .execute(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    if res.rows_affected() == 0 {
                        return Err(ToolError::NotFound);
                    }
                    Ok(json!({ "id": id, "updated": true }))
                })
            },
        );
    }
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_archive_reward_tier",
                "Hard-delete a reward tier by id (tiers have no archive state, so the flag is required).",
            )
            .schema(json!({
                "type": "object",
                "properties": {"id": {"type": "string"}, "hard_delete": {"type": "boolean"}},
                "required": ["id", "hard_delete"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let id = ctx.uuid_arg(&args, "id")?;
                    if !opt_bool(&args, "hard_delete") {
                        return Err(ToolError::Invalid(
                            "a reward tier has no archive state; pass hard_delete = true to delete it".into(),
                        ));
                    }
                    let res = sqlx::query(
                        "DELETE FROM loyalty_reward_tiers rt WHERE rt.id = $1 AND rt.program_id IN \
                         (SELECT lp.id FROM loyalty_programs lp WHERE lp.account_id = $2 \
                            OR EXISTS (SELECT 1 FROM campaigns c WHERE c.id = lp.campaign_id AND c.account_id = $2) \
                            OR EXISTS (SELECT 1 FROM campaigns fwd WHERE fwd.loyalty_program_id = lp.id AND fwd.account_id = $2))",
                    )
                    .bind(id)
                    .bind(account)
                    .execute(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    if res.rows_affected() == 0 {
                        return Err(ToolError::NotFound);
                    }
                    Ok(json!({ "id": id, "deleted": true }))
                })
            },
        );
    }

    // ── W5 campaign_milestones: create / update / archive ─────────────────────
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_create_milestone",
                "Create a milestone on a campaign this workspace owns (name and points required).",
            )
            .schema(json!({
                "type": "object",
                "properties": {
                    "campaign_id": {"type": "string"},
                    "name": {"type": "string"},
                    "points_required": {"type": "integer"},
                    "action_type": {"type": "string"}
                },
                "required": ["campaign_id", "name"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let campaign_id = ctx.uuid_arg(&args, "campaign_id")?;
                    let owned = scoped_owned(
                        &pool,
                        "SELECT id FROM campaigns WHERE id = $1 AND account_id = $2",
                        campaign_id,
                        account,
                    )
                    .await?;
                    if !owned {
                        return Err(ToolError::NotFound);
                    }
                    let name = ctx.str_arg(&args, "name")?.trim().to_string();
                    if name.is_empty() {
                        return Err(ToolError::Invalid("name is required".into()));
                    }
                    let points = args
                        .get("points_required")
                        .and_then(|v| v.as_i64())
                        .map(|v| v as i32)
                        .unwrap_or(0);
                    let action = opt_str(&args, "action_type").unwrap_or("fire_webhook").to_string();
                    let id = Uuid::new_v4();
                    sqlx::query(
                        "INSERT INTO campaign_milestones (id, campaign_id, name, points_required, action_type) \
                         VALUES ($1, $2, $3, $4, $5)",
                    )
                    .bind(id)
                    .bind(campaign_id)
                    .bind(&name)
                    .bind(points)
                    .bind(&action)
                    .execute(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    Ok(json!({ "id": id, "campaign_id": campaign_id, "name": name }))
                })
            },
        );
    }
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_update_milestone",
                "Update a milestone's name, points or action by id, scoped to this workspace.",
            )
            .schema(json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "name": {"type": "string"},
                    "points_required": {"type": "integer"},
                    "action_type": {"type": "string"}
                },
                "required": ["id"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let id = ctx.uuid_arg(&args, "id")?;
                    let name = opt_str(&args, "name").map(|s| s.trim().to_string());
                    let points = args.get("points_required").and_then(|v| v.as_i64()).map(|v| v as i32);
                    let action = opt_str(&args, "action_type").map(|s| s.to_string());
                    let res = sqlx::query(
                        "UPDATE campaign_milestones m SET name = COALESCE($2, m.name), \
                         points_required = COALESCE($3, m.points_required), \
                         action_type = COALESCE($4, m.action_type) \
                         WHERE m.id = $1 AND m.campaign_id IN (SELECT id FROM campaigns WHERE account_id = $5)",
                    )
                    .bind(id)
                    .bind(name)
                    .bind(points)
                    .bind(action)
                    .bind(account)
                    .execute(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    if res.rows_affected() == 0 {
                        return Err(ToolError::NotFound);
                    }
                    Ok(json!({ "id": id, "updated": true }))
                })
            },
        );
    }
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_archive_milestone",
                "Archive a milestone by id (soft) or hard-delete it with the hard_delete flag.",
            )
            .schema(json!({
                "type": "object",
                "properties": {"id": {"type": "string"}, "hard_delete": {"type": "boolean"}},
                "required": ["id"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let id = ctx.uuid_arg(&args, "id")?;
                    let res = if opt_bool(&args, "hard_delete") {
                        sqlx::query(
                            "DELETE FROM campaign_milestones m WHERE m.id = $1 AND m.campaign_id IN \
                             (SELECT id FROM campaigns WHERE account_id = $2)",
                        )
                        .bind(id)
                        .bind(account)
                        .execute(&pool)
                        .await
                        .map_err(|e| ToolError::Unavailable(e.to_string()))?
                    } else {
                        sqlx::query(
                            "UPDATE campaign_milestones m SET is_active = false WHERE m.id = $1 AND m.campaign_id IN \
                             (SELECT id FROM campaigns WHERE account_id = $2)",
                        )
                        .bind(id)
                        .bind(account)
                        .execute(&pool)
                        .await
                        .map_err(|e| ToolError::Unavailable(e.to_string()))?
                    };
                    if res.rows_affected() == 0 {
                        return Err(ToolError::NotFound);
                    }
                    Ok(json!({ "id": id, "archived": true }))
                })
            },
        );
    }

    // ── W6 campaign_secret_codes: create / update / archive ───────────────────
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_create_secret_code",
                "Create a secret code on a campaign this workspace owns (code and points).",
            )
            .schema(json!({
                "type": "object",
                "properties": {
                    "campaign_id": {"type": "string"},
                    "code": {"type": "string"},
                    "points": {"type": "integer"},
                    "max_uses": {"type": "integer"}
                },
                "required": ["campaign_id", "code"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let campaign_id = ctx.uuid_arg(&args, "campaign_id")?;
                    let owned = scoped_owned(
                        &pool,
                        "SELECT id FROM campaigns WHERE id = $1 AND account_id = $2",
                        campaign_id,
                        account,
                    )
                    .await?;
                    if !owned {
                        return Err(ToolError::NotFound);
                    }
                    let code = ctx.str_arg(&args, "code")?.trim().to_string();
                    if code.is_empty() {
                        return Err(ToolError::Invalid("code is required".into()));
                    }
                    let points = args
                        .get("points")
                        .and_then(|v| v.as_i64())
                        .map(|v| v as i32)
                        .unwrap_or(100);
                    let max_uses = args.get("max_uses").and_then(|v| v.as_i64()).map(|v| v as i32);
                    let id = Uuid::new_v4();
                    sqlx::query(
                        "INSERT INTO campaign_secret_codes (id, campaign_id, code, points, max_uses) \
                         VALUES ($1, $2, $3, $4, $5)",
                    )
                    .bind(id)
                    .bind(campaign_id)
                    .bind(&code)
                    .bind(points)
                    .bind(max_uses)
                    .execute(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    Ok(json!({ "id": id, "campaign_id": campaign_id, "code": code, "points": points }))
                })
            },
        );
    }
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_update_secret_code",
                "Update a secret code's code, points or max uses by id, scoped to this workspace.",
            )
            .schema(json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "code": {"type": "string"},
                    "points": {"type": "integer"},
                    "max_uses": {"type": "integer"}
                },
                "required": ["id"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let id = ctx.uuid_arg(&args, "id")?;
                    let code = opt_str(&args, "code").map(|s| s.trim().to_string());
                    let points = args.get("points").and_then(|v| v.as_i64()).map(|v| v as i32);
                    let max_uses = args.get("max_uses").and_then(|v| v.as_i64()).map(|v| v as i32);
                    let res = sqlx::query(
                        "UPDATE campaign_secret_codes sc SET code = COALESCE($2, sc.code), \
                         points = COALESCE($3, sc.points), max_uses = COALESCE($4, sc.max_uses) \
                         WHERE sc.id = $1 AND sc.campaign_id IN (SELECT id FROM campaigns WHERE account_id = $5)",
                    )
                    .bind(id)
                    .bind(code)
                    .bind(points)
                    .bind(max_uses)
                    .bind(account)
                    .execute(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    if res.rows_affected() == 0 {
                        return Err(ToolError::NotFound);
                    }
                    Ok(json!({ "id": id, "updated": true }))
                })
            },
        );
    }
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_archive_secret_code",
                "Archive a secret code by id (soft) or hard-delete it with the hard_delete flag.",
            )
            .schema(json!({
                "type": "object",
                "properties": {"id": {"type": "string"}, "hard_delete": {"type": "boolean"}},
                "required": ["id"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let id = ctx.uuid_arg(&args, "id")?;
                    let res = if opt_bool(&args, "hard_delete") {
                        sqlx::query(
                            "DELETE FROM campaign_secret_codes sc WHERE sc.id = $1 AND sc.campaign_id IN \
                             (SELECT id FROM campaigns WHERE account_id = $2)",
                        )
                        .bind(id)
                        .bind(account)
                        .execute(&pool)
                        .await
                        .map_err(|e| ToolError::Unavailable(e.to_string()))?
                    } else {
                        sqlx::query(
                            "UPDATE campaign_secret_codes sc SET is_active = false WHERE sc.id = $1 AND sc.campaign_id IN \
                             (SELECT id FROM campaigns WHERE account_id = $2)",
                        )
                        .bind(id)
                        .bind(account)
                        .execute(&pool)
                        .await
                        .map_err(|e| ToolError::Unavailable(e.to_string()))?
                    };
                    if res.rows_affected() == 0 {
                        return Err(ToolError::NotFound);
                    }
                    Ok(json!({ "id": id, "archived": true }))
                })
            },
        );
    }

    // ── W7 earn_channels: create / update / archive ───────────────────────────
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_create_earn_channel",
                "Create an earn channel on a campaign this workspace owns (channel_code and label).",
            )
            .schema(json!({
                "type": "object",
                "properties": {
                    "campaign_id": {"type": "string"},
                    "channel_code": {"type": "string"},
                    "label": {"type": "string"},
                    "points_per_click": {"type": "integer"}
                },
                "required": ["campaign_id", "channel_code"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let campaign_id = ctx.uuid_arg(&args, "campaign_id")?;
                    let owned = scoped_owned(
                        &pool,
                        "SELECT id FROM campaigns WHERE id = $1 AND account_id = $2",
                        campaign_id,
                        account,
                    )
                    .await?;
                    if !owned {
                        return Err(ToolError::NotFound);
                    }
                    let code = ctx.str_arg(&args, "channel_code")?.trim().to_string();
                    if code.is_empty() {
                        return Err(ToolError::Invalid("channel_code is required".into()));
                    }
                    let label = opt_str(&args, "label").unwrap_or("").to_string();
                    let points = args
                        .get("points_per_click")
                        .and_then(|v| v.as_i64())
                        .map(|v| v as i32)
                        .unwrap_or(0);
                    let id = Uuid::new_v4();
                    sqlx::query(
                        "INSERT INTO earn_channels (id, account_id, campaign_id, channel_code, label, points_per_click) \
                         VALUES ($1, $2, $3, $4, $5, $6)",
                    )
                    .bind(id)
                    .bind(account)
                    .bind(campaign_id)
                    .bind(&code)
                    .bind(&label)
                    .bind(points)
                    .execute(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    Ok(json!({ "id": id, "campaign_id": campaign_id, "channel_code": code }))
                })
            },
        );
    }
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_update_earn_channel",
                "Update an earn channel's code, label or points-per-click by id, scoped to this workspace.",
            )
            .schema(json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "channel_code": {"type": "string"},
                    "label": {"type": "string"},
                    "points_per_click": {"type": "integer"}
                },
                "required": ["id"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let id = ctx.uuid_arg(&args, "id")?;
                    let code = opt_str(&args, "channel_code").map(|s| s.trim().to_string());
                    let label = opt_str(&args, "label").map(|s| s.to_string());
                    let points = args.get("points_per_click").and_then(|v| v.as_i64()).map(|v| v as i32);
                    let res = sqlx::query(
                        "UPDATE earn_channels ec SET channel_code = COALESCE($2, ec.channel_code), \
                         label = COALESCE($3, ec.label), points_per_click = COALESCE($4, ec.points_per_click) \
                         WHERE ec.id = $1 AND ec.account_id = $5",
                    )
                    .bind(id)
                    .bind(code)
                    .bind(label)
                    .bind(points)
                    .bind(account)
                    .execute(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    if res.rows_affected() == 0 {
                        return Err(ToolError::NotFound);
                    }
                    Ok(json!({ "id": id, "updated": true }))
                })
            },
        );
    }
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_archive_earn_channel",
                "Archive an earn channel by id (soft) or hard-delete it with the hard_delete flag.",
            )
            .schema(json!({
                "type": "object",
                "properties": {"id": {"type": "string"}, "hard_delete": {"type": "boolean"}},
                "required": ["id"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let id = ctx.uuid_arg(&args, "id")?;
                    let res = if opt_bool(&args, "hard_delete") {
                        sqlx::query("DELETE FROM earn_channels WHERE id = $1 AND account_id = $2")
                            .bind(id)
                            .bind(account)
                            .execute(&pool)
                            .await
                            .map_err(|e| ToolError::Unavailable(e.to_string()))?
                    } else {
                        sqlx::query("UPDATE earn_channels SET is_active = false WHERE id = $1 AND account_id = $2")
                            .bind(id)
                            .bind(account)
                            .execute(&pool)
                            .await
                            .map_err(|e| ToolError::Unavailable(e.to_string()))?
                    };
                    if res.rows_affected() == 0 {
                        return Err(ToolError::NotFound);
                    }
                    Ok(json!({ "id": id, "archived": true }))
                })
            },
        );
    }

    // ── W8 email_templates: create / update / archive ─────────────────────────
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_create_email_template",
                "Create an email template (type, name, subject and optional bodies) for this workspace.",
            )
            .schema(json!({
                "type": "object",
                "properties": {
                    "template_type": {"type": "string"},
                    "name": {"type": "string"},
                    "subject": {"type": "string"},
                    "body": {"type": "string"},
                    "html_body": {"type": "string"}
                },
                "required": ["template_type", "name", "subject"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let ttype = ctx.str_arg(&args, "template_type")?.trim().to_string();
                    let name = ctx.str_arg(&args, "name")?.trim().to_string();
                    let subject = ctx.str_arg(&args, "subject")?.trim().to_string();
                    if name.is_empty() || subject.is_empty() {
                        return Err(ToolError::Invalid("name and subject are required".into()));
                    }
                    let body = opt_str(&args, "body");
                    let html = opt_str(&args, "html_body");
                    let id = Uuid::new_v4();
                    sqlx::query(
                        "INSERT INTO email_templates (id, template_type, name, subject, body, html_body, aid) \
                         VALUES ($1, $2, $3, $4, $5, $6, $7)",
                    )
                    .bind(id)
                    .bind(&ttype)
                    .bind(&name)
                    .bind(&subject)
                    .bind(body)
                    .bind(html)
                    .bind(account)
                    .execute(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    Ok(json!({ "id": id, "template_type": ttype, "name": name, "subject": subject }))
                })
            },
        );
    }
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_update_email_template",
                "Update an email template's name, subject or bodies by id, scoped to this workspace.",
            )
            .schema(json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "name": {"type": "string"},
                    "subject": {"type": "string"},
                    "body": {"type": "string"},
                    "html_body": {"type": "string"}
                },
                "required": ["id"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let id = ctx.uuid_arg(&args, "id")?;
                    let name = opt_str(&args, "name").map(|s| s.trim().to_string());
                    let subject = opt_str(&args, "subject").map(|s| s.trim().to_string());
                    let body = opt_str(&args, "body").map(|s| s.to_string());
                    let html = opt_str(&args, "html_body").map(|s| s.to_string());
                    let res = sqlx::query(
                        "UPDATE email_templates SET name = COALESCE($2, name), \
                         subject = COALESCE($3, subject), body = COALESCE($4, body), \
                         html_body = COALESCE($5, html_body) \
                         WHERE id = $1 AND aid = $6",
                    )
                    .bind(id)
                    .bind(name)
                    .bind(subject)
                    .bind(body)
                    .bind(html)
                    .bind(account)
                    .execute(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    if res.rows_affected() == 0 {
                        return Err(ToolError::NotFound);
                    }
                    Ok(json!({ "id": id, "updated": true }))
                })
            },
        );
    }
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_archive_email_template",
                "Hard-delete an email template by id (templates have no archive state, so the flag is required).",
            )
            .schema(json!({
                "type": "object",
                "properties": {"id": {"type": "string"}, "hard_delete": {"type": "boolean"}},
                "required": ["id", "hard_delete"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let id = ctx.uuid_arg(&args, "id")?;
                    if !opt_bool(&args, "hard_delete") {
                        return Err(ToolError::Invalid(
                            "an email template has no archive state; pass hard_delete = true to delete it".into(),
                        ));
                    }
                    let res = sqlx::query("DELETE FROM email_templates WHERE id = $1 AND aid = $2")
                        .bind(id)
                        .bind(account)
                        .execute(&pool)
                        .await
                        .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    if res.rows_affected() == 0 {
                        return Err(ToolError::NotFound);
                    }
                    Ok(json!({ "id": id, "deleted": true }))
                })
            },
        );
    }

    // ── W9 iqs_funnels: create / update / archive ─────────────────────────────
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_create_iqs_funnel",
                "Create an IQS lead-qualification funnel (name, optional funnel type and slug).",
            )
            .schema(json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "funnel_type": {"type": "string"},
                    "slug": {"type": "string"},
                    "description": {"type": "string"}
                },
                "required": ["name"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let name = ctx.str_arg(&args, "name")?.trim().to_string();
                    if name.is_empty() {
                        return Err(ToolError::Invalid("name is required".into()));
                    }
                    let slug = opt_str(&args, "slug")
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| slugify(&name));
                    let ftype = opt_str(&args, "funnel_type").unwrap_or("survey").to_string();
                    let description = opt_str(&args, "description");
                    let id = Uuid::new_v4();
                    sqlx::query(
                        "INSERT INTO iqs_funnels (id, account_id, name, funnel_type, status, slug, description) \
                         VALUES ($1, $2, $3, $4, 'draft', $5, $6)",
                    )
                    .bind(id)
                    .bind(account)
                    .bind(&name)
                    .bind(&ftype)
                    .bind(&slug)
                    .bind(description)
                    .execute(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    Ok(json!({ "id": id, "name": name, "slug": slug, "status": "draft" }))
                })
            },
        );
    }
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_update_iqs_funnel",
                "Update an IQS funnel's name, status or description by id, scoped to this workspace.",
            )
            .schema(json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "name": {"type": "string"},
                    "status": {"type": "string"},
                    "description": {"type": "string"}
                },
                "required": ["id"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let id = ctx.uuid_arg(&args, "id")?;
                    let name = opt_str(&args, "name").map(|s| s.trim().to_string());
                    let status = opt_str(&args, "status").map(|s| s.trim().to_string());
                    let description = opt_str(&args, "description").map(|s| s.to_string());
                    let res = sqlx::query(
                        "UPDATE iqs_funnels SET name = COALESCE($2, name), \
                         status = COALESCE($3, status), description = COALESCE($4, description) \
                         WHERE id = $1 AND account_id = $5",
                    )
                    .bind(id)
                    .bind(name)
                    .bind(status)
                    .bind(description)
                    .bind(account)
                    .execute(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    if res.rows_affected() == 0 {
                        return Err(ToolError::NotFound);
                    }
                    Ok(json!({ "id": id, "updated": true }))
                })
            },
        );
    }
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_archive_iqs_funnel",
                "Archive an IQS funnel by id (soft) or hard-delete it with the hard_delete flag.",
            )
            .schema(json!({
                "type": "object",
                "properties": {"id": {"type": "string"}, "hard_delete": {"type": "boolean"}},
                "required": ["id"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let id = ctx.uuid_arg(&args, "id")?;
                    let res = if opt_bool(&args, "hard_delete") {
                        sqlx::query("DELETE FROM iqs_funnels WHERE id = $1 AND account_id = $2")
                            .bind(id)
                            .bind(account)
                            .execute(&pool)
                            .await
                            .map_err(|e| ToolError::Unavailable(e.to_string()))?
                    } else {
                        sqlx::query("UPDATE iqs_funnels SET status = 'archived' WHERE id = $1 AND account_id = $2")
                            .bind(id)
                            .bind(account)
                            .execute(&pool)
                            .await
                            .map_err(|e| ToolError::Unavailable(e.to_string()))?
                    };
                    if res.rows_affected() == 0 {
                        return Err(ToolError::NotFound);
                    }
                    Ok(json!({ "id": id, "archived": true }))
                })
            },
        );
    }

    // ── W10 business_loyalty_deals: create / update / archive ─────────────────
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_create_business_deal",
                "Create a business loyalty deal (business, program, deal type and value) for this workspace.",
            )
            .schema(json!({
                "type": "object",
                "properties": {
                    "business_id": {"type": "string"},
                    "business_name": {"type": "string"},
                    "program_id": {"type": "string"},
                    "deal_type": {"type": "string"},
                    "deal_value": {"type": "string"},
                    "points_required": {"type": "integer"}
                },
                "required": ["business_id", "business_name", "program_id", "deal_type", "deal_value"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let business_id = ctx.uuid_arg(&args, "business_id")?;
                    let program_id = ctx.uuid_arg(&args, "program_id")?;
                    let owned = scoped_owned(
                        &pool,
                        "SELECT lp.id FROM loyalty_programs lp WHERE lp.id = $1 AND \
                         (lp.account_id = $2 OR EXISTS (SELECT 1 FROM campaigns c WHERE c.id = lp.campaign_id AND c.account_id = $2) \
                          OR EXISTS (SELECT 1 FROM campaigns fwd WHERE fwd.loyalty_program_id = lp.id AND fwd.account_id = $2))",
                        program_id,
                        account,
                    )
                    .await?;
                    if !owned {
                        return Err(ToolError::NotFound);
                    }
                    let business_name = ctx.str_arg(&args, "business_name")?.trim().to_string();
                    let deal_type = ctx.str_arg(&args, "deal_type")?.trim().to_string();
                    let deal_value = ctx.str_arg(&args, "deal_value")?.trim().to_string();
                    let points = args
                        .get("points_required")
                        .and_then(|v| v.as_i64())
                        .map(|v| v as i32)
                        .unwrap_or(0);
                    let id = Uuid::new_v4();
                    sqlx::query(
                        "INSERT INTO business_loyalty_deals (id, business_id, business_name, program_id, \
                         deal_type, deal_value, points_required, owner_account_id) \
                         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
                    )
                    .bind(id)
                    .bind(business_id)
                    .bind(&business_name)
                    .bind(program_id)
                    .bind(&deal_type)
                    .bind(&deal_value)
                    .bind(points)
                    .bind(account)
                    .execute(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    Ok(json!({ "id": id, "business_name": business_name, "deal_type": deal_type }))
                })
            },
        );
    }
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_update_business_deal",
                "Update a business deal's type, value, points or active flag by id, scoped to this workspace.",
            )
            .schema(json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "deal_type": {"type": "string"},
                    "deal_value": {"type": "string"},
                    "points_required": {"type": "integer"},
                    "is_active": {"type": "boolean"}
                },
                "required": ["id"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let id = ctx.uuid_arg(&args, "id")?;
                    let deal_type = opt_str(&args, "deal_type").map(|s| s.trim().to_string());
                    let deal_value = opt_str(&args, "deal_value").map(|s| s.trim().to_string());
                    let points = args.get("points_required").and_then(|v| v.as_i64()).map(|v| v as i32);
                    let active = args.get("is_active").and_then(|v| v.as_bool());
                    let res = sqlx::query(
                        "UPDATE business_loyalty_deals SET deal_type = COALESCE($2, deal_type), \
                         deal_value = COALESCE($3, deal_value), \
                         points_required = COALESCE($4, points_required), is_active = COALESCE($5, is_active) \
                         WHERE id = $1 AND owner_account_id = $6",
                    )
                    .bind(id)
                    .bind(deal_type)
                    .bind(deal_value)
                    .bind(points)
                    .bind(active)
                    .bind(account)
                    .execute(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    if res.rows_affected() == 0 {
                        return Err(ToolError::NotFound);
                    }
                    Ok(json!({ "id": id, "updated": true }))
                })
            },
        );
    }
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_archive_business_deal",
                "Archive a business deal by id (soft) or hard-delete it with the hard_delete flag.",
            )
            .schema(json!({
                "type": "object",
                "properties": {"id": {"type": "string"}, "hard_delete": {"type": "boolean"}},
                "required": ["id"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let id = ctx.uuid_arg(&args, "id")?;
                    let res = if opt_bool(&args, "hard_delete") {
                        sqlx::query("DELETE FROM business_loyalty_deals WHERE id = $1 AND owner_account_id = $2")
                            .bind(id)
                            .bind(account)
                            .execute(&pool)
                            .await
                            .map_err(|e| ToolError::Unavailable(e.to_string()))?
                    } else {
                        sqlx::query(
                            "UPDATE business_loyalty_deals SET is_active = false WHERE id = $1 AND owner_account_id = $2",
                        )
                        .bind(id)
                        .bind(account)
                        .execute(&pool)
                        .await
                        .map_err(|e| ToolError::Unavailable(e.to_string()))?
                    };
                    if res.rows_affected() == 0 {
                        return Err(ToolError::NotFound);
                    }
                    Ok(json!({ "id": id, "archived": true }))
                })
            },
        );
    }

    // ── W11 calendar_events: create / update / archive ────────────────────────
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_create_calendar_event",
                "Create a calendar event (title and RFC3339 starts_at) for this workspace.",
            )
            .schema(json!({
                "type": "object",
                "properties": {
                    "title": {"type": "string"},
                    "starts_at": {"type": "string"},
                    "ends_at": {"type": "string"},
                    "description": {"type": "string"},
                    "location": {"type": "string"},
                    "all_day": {"type": "boolean"}
                },
                "required": ["title", "starts_at"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let title = ctx.str_arg(&args, "title")?.trim().to_string();
                    if title.is_empty() {
                        return Err(ToolError::Invalid("title is required".into()));
                    }
                    let starts_at = opt_ts(&args, "starts_at")?
                        .ok_or_else(|| ToolError::Invalid("starts_at is required".into()))?;
                    let ends_at = opt_ts(&args, "ends_at")?;
                    let description = opt_str(&args, "description");
                    let location = opt_str(&args, "location");
                    let all_day = opt_bool(&args, "all_day");
                    let id = Uuid::new_v4();
                    sqlx::query(
                        "INSERT INTO calendar_events (id, tenant_id, title, description, location, starts_at, ends_at, all_day) \
                         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
                    )
                    .bind(id)
                    .bind(account)
                    .bind(&title)
                    .bind(description)
                    .bind(location)
                    .bind(starts_at)
                    .bind(ends_at)
                    .bind(all_day)
                    .execute(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    Ok(json!({ "id": id, "title": title, "starts_at": starts_at.to_rfc3339() }))
                })
            },
        );
    }
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_update_calendar_event",
                "Update a calendar event's title, times, description or status by id, scoped to this workspace.",
            )
            .schema(json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "title": {"type": "string"},
                    "starts_at": {"type": "string"},
                    "ends_at": {"type": "string"},
                    "description": {"type": "string"},
                    "status": {"type": "string"}
                },
                "required": ["id"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let id = ctx.uuid_arg(&args, "id")?;
                    let title = opt_str(&args, "title").map(|s| s.trim().to_string());
                    let starts_at = opt_ts(&args, "starts_at")?;
                    let ends_at = opt_ts(&args, "ends_at")?;
                    let description = opt_str(&args, "description").map(|s| s.to_string());
                    let status = opt_str(&args, "status").map(|s| s.trim().to_string());
                    let res = sqlx::query(
                        "UPDATE calendar_events SET title = COALESCE($2, title), \
                         starts_at = COALESCE($3, starts_at), ends_at = COALESCE($4, ends_at), \
                         description = COALESCE($5, description), status = COALESCE($6, status) \
                         WHERE id = $1 AND tenant_id = $7",
                    )
                    .bind(id)
                    .bind(title)
                    .bind(starts_at)
                    .bind(ends_at)
                    .bind(description)
                    .bind(status)
                    .bind(account)
                    .execute(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    if res.rows_affected() == 0 {
                        return Err(ToolError::NotFound);
                    }
                    Ok(json!({ "id": id, "updated": true }))
                })
            },
        );
    }
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_archive_calendar_event",
                "Archive a calendar event by id (soft) or hard-delete it with the hard_delete flag.",
            )
            .schema(json!({
                "type": "object",
                "properties": {"id": {"type": "string"}, "hard_delete": {"type": "boolean"}},
                "required": ["id"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let id = ctx.uuid_arg(&args, "id")?;
                    let res = if opt_bool(&args, "hard_delete") {
                        sqlx::query("DELETE FROM calendar_events WHERE id = $1 AND tenant_id = $2")
                            .bind(id)
                            .bind(account)
                            .execute(&pool)
                            .await
                            .map_err(|e| ToolError::Unavailable(e.to_string()))?
                    } else {
                        sqlx::query("UPDATE calendar_events SET status = 'cancelled' WHERE id = $1 AND tenant_id = $2")
                            .bind(id)
                            .bind(account)
                            .execute(&pool)
                            .await
                            .map_err(|e| ToolError::Unavailable(e.to_string()))?
                    };
                    if res.rows_affected() == 0 {
                        return Err(ToolError::NotFound);
                    }
                    Ok(json!({ "id": id, "archived": true }))
                })
            },
        );
    }

    // ── W12 reviews: create / update / archive ────────────────────────────────
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_create_review",
                "Create a review (rating plus optional title, body and reviewer name) for this workspace.",
            )
            .schema(json!({
                "type": "object",
                "properties": {
                    "rating": {"type": "integer"},
                    "title": {"type": "string"},
                    "body": {"type": "string"},
                    "reviewer_name": {"type": "string"}
                },
                "required": ["rating"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let rating = args
                        .get("rating")
                        .and_then(|v| v.as_i64())
                        .map(|v| v as i32)
                        .ok_or_else(|| ToolError::Invalid("rating must be an integer".into()))?;
                    let title = opt_str(&args, "title");
                    let body = opt_str(&args, "body");
                    let reviewer = opt_str(&args, "reviewer_name");
                    let id = Uuid::new_v4();
                    sqlx::query(
                        "INSERT INTO reviews (id, tenant_id, rating, title, body, reviewer_name) \
                         VALUES ($1, $2, $3, $4, $5, $6)",
                    )
                    .bind(id)
                    .bind(account)
                    .bind(rating)
                    .bind(title)
                    .bind(body)
                    .bind(reviewer)
                    .execute(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    Ok(json!({ "id": id, "rating": rating, "status": "pending" }))
                })
            },
        );
    }
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_update_review",
                "Update a review's rating, title, body or status by id, scoped to this workspace.",
            )
            .schema(json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "rating": {"type": "integer"},
                    "title": {"type": "string"},
                    "body": {"type": "string"},
                    "status": {"type": "string"}
                },
                "required": ["id"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let id = ctx.uuid_arg(&args, "id")?;
                    let rating = args.get("rating").and_then(|v| v.as_i64()).map(|v| v as i32);
                    let title = opt_str(&args, "title").map(|s| s.to_string());
                    let body = opt_str(&args, "body").map(|s| s.to_string());
                    let status = opt_str(&args, "status").map(|s| s.trim().to_string());
                    let res = sqlx::query(
                        "UPDATE reviews SET rating = COALESCE($2, rating), title = COALESCE($3, title), \
                         body = COALESCE($4, body), status = COALESCE($5, status) \
                         WHERE id = $1 AND tenant_id = $6",
                    )
                    .bind(id)
                    .bind(rating)
                    .bind(title)
                    .bind(body)
                    .bind(status)
                    .bind(account)
                    .execute(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    if res.rows_affected() == 0 {
                        return Err(ToolError::NotFound);
                    }
                    Ok(json!({ "id": id, "updated": true }))
                })
            },
        );
    }
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_archive_review",
                "Archive a review by id (soft) or hard-delete it with the hard_delete flag.",
            )
            .schema(json!({
                "type": "object",
                "properties": {"id": {"type": "string"}, "hard_delete": {"type": "boolean"}},
                "required": ["id"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let id = ctx.uuid_arg(&args, "id")?;
                    let res = if opt_bool(&args, "hard_delete") {
                        sqlx::query("DELETE FROM reviews WHERE id = $1 AND tenant_id = $2")
                            .bind(id)
                            .bind(account)
                            .execute(&pool)
                            .await
                            .map_err(|e| ToolError::Unavailable(e.to_string()))?
                    } else {
                        sqlx::query("UPDATE reviews SET status = 'archived' WHERE id = $1 AND tenant_id = $2")
                            .bind(id)
                            .bind(account)
                            .execute(&pool)
                            .await
                            .map_err(|e| ToolError::Unavailable(e.to_string()))?
                    };
                    if res.rows_affected() == 0 {
                        return Err(ToolError::NotFound);
                    }
                    Ok(json!({ "id": id, "archived": true }))
                })
            },
        );
    }

    // ── W13 support_tickets: create / update / archive ────────────────────────
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_create_support_ticket",
                "Create a support ticket (subject plus optional description, priority and category).",
            )
            .schema(json!({
                "type": "object",
                "properties": {
                    "subject": {"type": "string"},
                    "description": {"type": "string"},
                    "priority": {"type": "string"},
                    "category": {"type": "string"}
                },
                "required": ["subject"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let subject = ctx.str_arg(&args, "subject")?.trim().to_string();
                    if subject.is_empty() {
                        return Err(ToolError::Invalid("subject is required".into()));
                    }
                    let description = opt_str(&args, "description");
                    let priority = opt_str(&args, "priority").unwrap_or("normal").to_string();
                    let category = opt_str(&args, "category");
                    let id = Uuid::new_v4();
                    sqlx::query(
                        "INSERT INTO support_tickets (id, tenant_id, subject, description, priority, category) \
                         VALUES ($1, $2, $3, $4, $5, $6)",
                    )
                    .bind(id)
                    .bind(account)
                    .bind(&subject)
                    .bind(description)
                    .bind(&priority)
                    .bind(category)
                    .execute(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    Ok(json!({ "id": id, "subject": subject, "status": "open" }))
                })
            },
        );
    }
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_update_support_ticket",
                "Update a support ticket's subject, description, status or priority by id, scoped to this workspace.",
            )
            .schema(json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "subject": {"type": "string"},
                    "description": {"type": "string"},
                    "status": {"type": "string"},
                    "priority": {"type": "string"}
                },
                "required": ["id"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let id = ctx.uuid_arg(&args, "id")?;
                    let subject = opt_str(&args, "subject").map(|s| s.trim().to_string());
                    let description = opt_str(&args, "description").map(|s| s.to_string());
                    let status = opt_str(&args, "status").map(|s| s.trim().to_string());
                    let priority = opt_str(&args, "priority").map(|s| s.trim().to_string());
                    let res = sqlx::query(
                        "UPDATE support_tickets SET subject = COALESCE($2, subject), \
                         description = COALESCE($3, description), status = COALESCE($4, status), \
                         priority = COALESCE($5, priority) \
                         WHERE id = $1 AND tenant_id = $6",
                    )
                    .bind(id)
                    .bind(subject)
                    .bind(description)
                    .bind(status)
                    .bind(priority)
                    .bind(account)
                    .execute(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    if res.rows_affected() == 0 {
                        return Err(ToolError::NotFound);
                    }
                    Ok(json!({ "id": id, "updated": true }))
                })
            },
        );
    }
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_archive_support_ticket",
                "Archive a support ticket by id (soft) or hard-delete it with the hard_delete flag.",
            )
            .schema(json!({
                "type": "object",
                "properties": {"id": {"type": "string"}, "hard_delete": {"type": "boolean"}},
                "required": ["id"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let id = ctx.uuid_arg(&args, "id")?;
                    let res = if opt_bool(&args, "hard_delete") {
                        sqlx::query("DELETE FROM support_tickets WHERE id = $1 AND tenant_id = $2")
                            .bind(id)
                            .bind(account)
                            .execute(&pool)
                            .await
                            .map_err(|e| ToolError::Unavailable(e.to_string()))?
                    } else {
                        sqlx::query("UPDATE support_tickets SET status = 'closed' WHERE id = $1 AND tenant_id = $2")
                            .bind(id)
                            .bind(account)
                            .execute(&pool)
                            .await
                            .map_err(|e| ToolError::Unavailable(e.to_string()))?
                    };
                    if res.rows_affected() == 0 {
                        return Err(ToolError::NotFound);
                    }
                    Ok(json!({ "id": id, "archived": true }))
                })
            },
        );
    }

    // ── W14 custom_domains: create / update / archive ─────────────────────────
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_create_domain",
                "Create a sending domain for this workspace (the domain is unverified until DNS proves ownership).",
            )
            .schema(json!({
                "type": "object",
                "properties": {
                    "domain": {"type": "string"},
                    "target_type": {"type": "string"}
                },
                "required": ["domain"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let domain = ctx.str_arg(&args, "domain")?.trim().to_lowercase();
                    if domain.is_empty() {
                        return Err(ToolError::Invalid("domain is required".into()));
                    }
                    let target = opt_str(&args, "target_type")
                        .unwrap_or("incentiveswift")
                        .to_string();
                    let id = Uuid::new_v4();
                    sqlx::query(
                        "INSERT INTO custom_domains (id, tenant_id, domain, target_type) \
                         VALUES ($1, $2, $3, $4) \
                         RETURNING verification_token",
                    )
                    .bind(id)
                    .bind(account)
                    .bind(&domain)
                    .bind(&target)
                    .fetch_one(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    Ok(json!({ "id": id, "domain": domain, "is_active": false }))
                })
            },
        );
    }
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_update_domain",
                "Update a domain's hostname or target type by id, scoped to this workspace.",
            )
            .schema(json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "domain": {"type": "string"},
                    "target_type": {"type": "string"}
                },
                "required": ["id"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let id = ctx.uuid_arg(&args, "id")?;
                    let domain = opt_str(&args, "domain").map(|s| s.trim().to_lowercase());
                    let target = opt_str(&args, "target_type").map(|s| s.to_string());
                    let res = sqlx::query(
                        "UPDATE custom_domains SET domain = COALESCE($2, domain), \
                         target_type = COALESCE($3, target_type) \
                         WHERE id = $1 AND tenant_id = $4",
                    )
                    .bind(id)
                    .bind(domain)
                    .bind(target)
                    .bind(account)
                    .execute(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    if res.rows_affected() == 0 {
                        return Err(ToolError::NotFound);
                    }
                    Ok(json!({ "id": id, "updated": true }))
                })
            },
        );
    }
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_archive_domain",
                "Archive a sending domain by id (soft) or hard-delete it with the hard_delete flag.",
            )
            .schema(json!({
                "type": "object",
                "properties": {"id": {"type": "string"}, "hard_delete": {"type": "boolean"}},
                "required": ["id"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let id = ctx.uuid_arg(&args, "id")?;
                    let res = if opt_bool(&args, "hard_delete") {
                        sqlx::query("DELETE FROM custom_domains WHERE id = $1 AND tenant_id = $2")
                            .bind(id)
                            .bind(account)
                            .execute(&pool)
                            .await
                            .map_err(|e| ToolError::Unavailable(e.to_string()))?
                    } else {
                        sqlx::query("UPDATE custom_domains SET is_active = false WHERE id = $1 AND tenant_id = $2")
                            .bind(id)
                            .bind(account)
                            .execute(&pool)
                            .await
                            .map_err(|e| ToolError::Unavailable(e.to_string()))?
                    };
                    if res.rows_affected() == 0 {
                        return Err(ToolError::NotFound);
                    }
                    Ok(json!({ "id": id, "archived": true }))
                })
            },
        );
    }

    // ── W15 leads: create / update / archive ──────────────────────────────────
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_create_lead",
                "Create a lead (optional name, email and phone) for this workspace.",
            )
            .schema(json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "email": {"type": "string"},
                    "phone": {"type": "string"}
                },
                "required": [],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let name = opt_str(&args, "name");
                    let email = opt_str(&args, "email");
                    let phone = opt_str(&args, "phone");
                    let id = Uuid::new_v4();
                    sqlx::query(
                        "INSERT INTO leads (id, tenant_id, name, email, phone, status) \
                         VALUES ($1, $2, $3, $4, $5, 'new')",
                    )
                    .bind(id)
                    .bind(account)
                    .bind(name)
                    .bind(email)
                    .bind(phone)
                    .execute(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    Ok(json!({ "id": id, "status": "new" }))
                })
            },
        );
    }
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_update_lead",
                "Update a lead's name, email, phone or status by id, scoped to this workspace.",
            )
            .schema(json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "name": {"type": "string"},
                    "email": {"type": "string"},
                    "phone": {"type": "string"},
                    "status": {"type": "string"}
                },
                "required": ["id"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let id = ctx.uuid_arg(&args, "id")?;
                    let name = opt_str(&args, "name").map(|s| s.to_string());
                    let email = opt_str(&args, "email").map(|s| s.to_string());
                    let phone = opt_str(&args, "phone").map(|s| s.to_string());
                    let status = opt_str(&args, "status").map(|s| s.trim().to_string());
                    let res = sqlx::query(
                        "UPDATE leads SET name = COALESCE($2, name), email = COALESCE($3, email), \
                         phone = COALESCE($4, phone), status = COALESCE($5, status) \
                         WHERE id = $1 AND tenant_id = $6",
                    )
                    .bind(id)
                    .bind(name)
                    .bind(email)
                    .bind(phone)
                    .bind(status)
                    .bind(account)
                    .execute(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    if res.rows_affected() == 0 {
                        return Err(ToolError::NotFound);
                    }
                    Ok(json!({ "id": id, "updated": true }))
                })
            },
        );
    }
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_archive_lead",
                "Archive a lead by id (soft) or hard-delete it with the hard_delete flag.",
            )
            .schema(json!({
                "type": "object",
                "properties": {"id": {"type": "string"}, "hard_delete": {"type": "boolean"}},
                "required": ["id"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let id = ctx.uuid_arg(&args, "id")?;
                    let res = if opt_bool(&args, "hard_delete") {
                        sqlx::query("DELETE FROM leads WHERE id = $1 AND tenant_id = $2")
                            .bind(id)
                            .bind(account)
                            .execute(&pool)
                            .await
                            .map_err(|e| ToolError::Unavailable(e.to_string()))?
                    } else {
                        sqlx::query(
                            "UPDATE leads SET status = 'archived' WHERE id = $1 AND tenant_id = $2",
                        )
                        .bind(id)
                        .bind(account)
                        .execute(&pool)
                        .await
                        .map_err(|e| ToolError::Unavailable(e.to_string()))?
                    };
                    if res.rows_affected() == 0 {
                        return Err(ToolError::NotFound);
                    }
                    Ok(json!({ "id": id, "archived": true }))
                })
            },
        );
    }

    // ── W17 contacts: list / create / update / archive ─────────────────────────
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::read(
                "incentiveswift_list_contacts",
                "List contacts visible to this workspace (the contact_tenants link), newest first.",
            ),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let limit = ctx.limit(args.get("limit"), 25, 200);
                    let account = account_id(&ctx)?;
                    let rows = sqlx::query(
                        "SELECT c.id, c.first_name, c.last_name, c.email, c.phone, c.business_name, \
                                c.website, c.created_at \
                         FROM contacts c \
                         JOIN contact_tenants ct ON ct.contact_id = c.id \
                         WHERE ct.account_id = $1 ORDER BY c.created_at DESC LIMIT $2",
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
                                "first_name": r.get::<Option<String>, _>("first_name"),
                                "last_name": r.get::<Option<String>, _>("last_name"),
                                "email": r.get::<Option<String>, _>("email"),
                                "phone": r.get::<Option<String>, _>("phone"),
                                "business_name": r.get::<Option<String>, _>("business_name"),
                                "website": r.get::<Option<String>, _>("website"),
                                "created_at": r.get::<DateTime<Utc>, _>("created_at"),
                            })
                        })
                        .collect();
                    Ok(json!({ "count": items.len(), "items": items }))
                })
            },
        );
    }
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_create_contact",
                "Create a contact for this workspace (email is find-or-create within the account).",
            )
            .schema(json!({
                "type": "object",
                "properties": {
                    "first_name": {"type": "string"},
                    "last_name": {"type": "string"},
                    "email": {"type": "string"},
                    "phone": {"type": "string"},
                    "business_name": {"type": "string"},
                    "website": {"type": "string"}
                },
                "required": [],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let first_name = opt_str(&args, "first_name").map(|s| s.trim().to_string());
                    let last_name = opt_str(&args, "last_name").map(|s| s.trim().to_string());
                    let email = opt_str(&args, "email").map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
                    let phone = opt_str(&args, "phone").map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
                    let business_name = opt_str(&args, "business_name").map(|s| s.trim().to_string());
                    let website = opt_str(&args, "website").map(|s| s.trim().to_string());
                    if email.is_none() && phone.is_none() {
                        return Err(ToolError::Invalid("email or phone is required".into()));
                    }
                    // Find-or-create by email within this account's link set.
                    if let Some(ref em) = email {
                        let existing: Option<Uuid> = sqlx::query_scalar(
                            "SELECT c.id FROM contacts c \
                             JOIN contact_tenants ct ON ct.contact_id = c.id \
                             WHERE lower(c.email) = lower($1) AND ct.account_id = $2",
                        )
                        .bind(em)
                        .bind(account)
                        .fetch_optional(&pool)
                        .await
                        .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                        if let Some(id) = existing {
                            return Ok(json!({ "id": id, "existing": true }));
                        }
                    }
                    let id = Uuid::new_v4();
                    let ins = sqlx::query(
                        "INSERT INTO contacts (id, first_name, last_name, email, phone, business_name, website) \
                         VALUES ($1, $2, $3, $4, $5, $6, $7)",
                    )
                    .bind(id)
                    .bind(&first_name)
                    .bind(&last_name)
                    .bind(&email)
                    .bind(&phone)
                    .bind(&business_name)
                    .bind(&website)
                    .execute(&pool)
                    .await;
                    let id = match ins {
                        Ok(_) => id,
                        Err(e) => {
                            let is_dup = matches!(&e, sqlx::Error::Database(db) if db.code().as_deref() == Some("23505"));
                            if !is_dup {
                                return Err(ToolError::Unavailable(e.to_string()));
                            }
                            match email {
                                Some(ref em) => sqlx::query_scalar(
                                    "SELECT id FROM contacts WHERE lower(email) = lower($1)",
                                )
                                .bind(em)
                                .fetch_optional(&pool)
                                .await
                                .map_err(|e| ToolError::Unavailable(e.to_string()))?
                                .ok_or_else(|| ToolError::Unavailable("duplicate contact, re-read failed".into()))?,
                                None => return Err(ToolError::Unavailable("duplicate contact".into())),
                            }
                        }
                    };
                    sqlx::query(
                        "INSERT INTO contact_tenants (contact_id, account_id, source) VALUES ($1, $2, 'mcp') \
                         ON CONFLICT DO NOTHING",
                    )
                    .bind(id)
                    .bind(account)
                    .execute(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    Ok(json!({ "id": id, "first_name": first_name, "last_name": last_name, "email": email, "phone": phone }))
                })
            },
        );
    }
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_update_contact",
                "Update a contact's fields by id, scoped to this workspace's contact_tenants link.",
            )
            .schema(json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "first_name": {"type": "string"},
                    "last_name": {"type": "string"},
                    "email": {"type": "string"},
                    "phone": {"type": "string"},
                    "business_name": {"type": "string"},
                    "website": {"type": "string"},
                    "notes": {"type": "string"}
                },
                "required": ["id"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let id = ctx.uuid_arg(&args, "id")?;
                    let first_name = opt_str(&args, "first_name").map(|s| s.trim().to_string());
                    let last_name = opt_str(&args, "last_name").map(|s| s.trim().to_string());
                    let email = opt_str(&args, "email").map(|s| s.trim().to_string());
                    let phone = opt_str(&args, "phone").map(|s| s.trim().to_string());
                    let business_name = opt_str(&args, "business_name").map(|s| s.trim().to_string());
                    let website = opt_str(&args, "website").map(|s| s.trim().to_string());
                    let notes = opt_str(&args, "notes").map(|s| s.trim().to_string());
                    let res = sqlx::query(
                        "UPDATE contacts c SET first_name = COALESCE($2, c.first_name), \
                         last_name = COALESCE($3, c.last_name), email = COALESCE($4, c.email), \
                         phone = COALESCE($5, c.phone), business_name = COALESCE($6, c.business_name), \
                         website = COALESCE($7, c.website), notes = COALESCE($8, c.notes) \
                         WHERE c.id = $1 AND EXISTS (SELECT 1 FROM contact_tenants ct \
                             WHERE ct.contact_id = c.id AND ct.account_id = $9)",
                    )
                    .bind(id)
                    .bind(first_name)
                    .bind(last_name)
                    .bind(email)
                    .bind(phone)
                    .bind(business_name)
                    .bind(website)
                    .bind(notes)
                    .bind(account)
                    .execute(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    if res.rows_affected() == 0 {
                        return Err(ToolError::NotFound);
                    }
                    Ok(json!({ "id": id, "updated": true }))
                })
            },
        );
    }
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_archive_contact",
                "Unlink a contact from this workspace. Use hard_delete to also delete the row when no other tenant links it.",
            )
            .schema(json!({
                "type": "object",
                "properties": {"id": {"type": "string"}, "hard_delete": {"type": "boolean"}},
                "required": ["id"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let id = ctx.uuid_arg(&args, "id")?;
                    let res = sqlx::query(
                        "DELETE FROM contact_tenants WHERE contact_id = $1 AND account_id = $2",
                    )
                    .bind(id)
                    .bind(account)
                    .execute(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    if res.rows_affected() == 0 {
                        return Err(ToolError::NotFound);
                    }
                    if opt_bool(&args, "hard_delete") {
                        sqlx::query(
                            "DELETE FROM contacts WHERE id = $1 AND NOT EXISTS \
                             (SELECT 1 FROM contact_tenants WHERE contact_id = $1)",
                        )
                        .bind(id)
                        .execute(&pool)
                        .await
                        .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    }
                    Ok(json!({ "id": id, "archived": true }))
                })
            },
        );
    }

    // ── W18 tags: list / create / update / archive ────────────────────────────
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::read(
                "incentiveswift_list_tags",
                "List this workspace's contact tags with their color, newest first.",
            ),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let limit = ctx.limit(args.get("limit"), 25, 200);
                    let account = account_id(&ctx)?;
                    let rows = sqlx::query(
                        "SELECT id, name, color, group_id, created_at FROM tags \
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
                                "color": r.get::<Option<String>, _>("color"),
                                "group_id": r.get::<Option<Uuid>, _>("group_id"),
                                "created_at": r.get::<DateTime<Utc>, _>("created_at"),
                            })
                        })
                        .collect();
                    Ok(json!({ "count": items.len(), "items": items }))
                })
            },
        );
    }
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_create_tag",
                "Create a contact tag (name, optional color) for this workspace.",
            )
            .schema(json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "color": {"type": "string"}
                },
                "required": ["name"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let name = ctx.str_arg(&args, "name")?.trim().to_string();
                    let color = opt_str(&args, "color").map(|s| s.trim().to_string());
                    let id = Uuid::new_v4();
                    sqlx::query(
                        "INSERT INTO tags (id, account_id, name, color) VALUES ($1, $2, $3, $4)",
                    )
                    .bind(id)
                    .bind(account)
                    .bind(&name)
                    .bind(&color)
                    .execute(&pool)
                    .await
                    .map_err(|e| {
                        ToolError::Invalid(format!("tag could not be created (name taken?): {e}"))
                    })?;
                    Ok(json!({ "id": id, "name": name, "color": color }))
                })
            },
        );
    }
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_update_tag",
                "Rename a tag or change its color by id, scoped to this workspace.",
            )
            .schema(json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "name": {"type": "string"},
                    "color": {"type": "string"}
                },
                "required": ["id"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let id = ctx.uuid_arg(&args, "id")?;
                    let name = opt_str(&args, "name").map(|s| s.trim().to_string());
                    let color = opt_str(&args, "color").map(|s| s.trim().to_string());
                    let res = sqlx::query(
                        "UPDATE tags SET name = COALESCE($2, name), color = COALESCE($3, color), \
                         updated_at = now() WHERE id = $1 AND account_id = $4",
                    )
                    .bind(id)
                    .bind(name)
                    .bind(color)
                    .bind(account)
                    .execute(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    if res.rows_affected() == 0 {
                        return Err(ToolError::NotFound);
                    }
                    Ok(json!({ "id": id, "updated": true }))
                })
            },
        );
    }
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_archive_tag",
                "Delete a tag by id, scoped to this workspace (contact_tags links cascade).",
            )
            .schema(json!({
                "type": "object",
                "properties": {"id": {"type": "string"}},
                "required": ["id"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let id = ctx.uuid_arg(&args, "id")?;
                    let res = sqlx::query("DELETE FROM tags WHERE id = $1 AND account_id = $2")
                        .bind(id)
                        .bind(account)
                        .execute(&pool)
                        .await
                        .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    if res.rows_affected() == 0 {
                        return Err(ToolError::NotFound);
                    }
                    Ok(json!({ "id": id, "archived": true }))
                })
            },
        );
    }

    // ── W19 knowledge_base: list / create / update / archive ───────────────────
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::read(
                "incentiveswift_list_kb_articles",
                "List this workspace's knowledge-base articles (title, slug, audience, publish state).",
            ),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let limit = ctx.limit(args.get("limit"), 25, 200);
                    let account = account_id(&ctx)?;
                    let rows = sqlx::query(
                        "SELECT id, title, slug, audience, category, is_published, sort_order, created_at \
                         FROM knowledge_base WHERE tenant_id = $1 \
                         ORDER BY sort_order, created_at DESC LIMIT $2",
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
                                "slug": r.get::<String, _>("slug"),
                                "audience": r.get::<String, _>("audience"),
                                "category": r.get::<Option<String>, _>("category"),
                                "is_published": r.get::<bool, _>("is_published"),
                                "sort_order": r.get::<i32, _>("sort_order"),
                                "created_at": r.get::<DateTime<Utc>, _>("created_at"),
                            })
                        })
                        .collect();
                    Ok(json!({ "count": items.len(), "items": items }))
                })
            },
        );
    }
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_create_kb_article",
                "Create a knowledge-base article (title, content, audience, category) for this workspace.",
            )
            .schema(json!({
                "type": "object",
                "properties": {
                    "title": {"type": "string"},
                    "content": {"type": "string"},
                    "audience": {"type": "string"},
                    "category": {"type": "string"},
                    "sort_order": {"type": "integer"},
                    "is_published": {"type": "boolean"}
                },
                "required": ["title"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let title = ctx.str_arg(&args, "title")?.trim().to_string();
                    let content = opt_str(&args, "content").map(|s| s.to_string());
                    let audience = opt_str(&args, "audience")
                        .unwrap_or("user")
                        .trim()
                        .to_string();
                    if audience != "admin" && audience != "user" {
                        return Err(ToolError::Invalid("audience must be 'admin' or 'user'".into()));
                    }
                    let category = opt_str(&args, "category").map(|s| s.trim().to_string());
                    let sort_order = args.get("sort_order").and_then(|v| v.as_i64()).map(|v| v as i32).unwrap_or(100);
                    let published = args.get("is_published").and_then(|v| v.as_bool()).unwrap_or(true);
                    let id = Uuid::new_v4();
                    let slug = slugify(&title);
                    sqlx::query(
                        "INSERT INTO knowledge_base (id, tenant_id, title, content, audience, slug, category, sort_order, is_published) \
                         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
                    )
                    .bind(id)
                    .bind(account)
                    .bind(&title)
                    .bind(content)
                    .bind(&audience)
                    .bind(&slug)
                    .bind(category)
                    .bind(sort_order)
                    .bind(published)
                    .execute(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    Ok(json!({ "id": id, "title": title, "slug": slug, "audience": audience, "is_published": published }))
                })
            },
        );
    }
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_update_kb_article",
                "Update a knowledge-base article's title, content, audience, category, order or publish state.",
            )
            .schema(json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "title": {"type": "string"},
                    "content": {"type": "string"},
                    "audience": {"type": "string"},
                    "category": {"type": "string"},
                    "sort_order": {"type": "integer"},
                    "is_published": {"type": "boolean"}
                },
                "required": ["id"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let id = ctx.uuid_arg(&args, "id")?;
                    let title = opt_str(&args, "title").map(|s| s.trim().to_string());
                    let content = opt_str(&args, "content").map(|s| s.to_string());
                    let audience = opt_str(&args, "audience").map(|s| s.trim().to_string());
                    if let Some(a) = &audience {
                        if a != "admin" && a != "user" {
                            return Err(ToolError::Invalid("audience must be 'admin' or 'user'".into()));
                        }
                    }
                    let category = opt_str(&args, "category").map(|s| s.trim().to_string());
                    let sort_order = args.get("sort_order").and_then(|v| v.as_i64()).map(|v| v as i32);
                    let published = args.get("is_published").and_then(|v| v.as_bool());
                    let res = sqlx::query(
                        "UPDATE knowledge_base SET title = COALESCE($2, title), \
                         content = COALESCE($3, content), audience = COALESCE($4, audience), \
                         category = COALESCE($5, category), sort_order = COALESCE($6, sort_order), \
                         is_published = COALESCE($7, is_published), updated_at = now() \
                         WHERE id = $1 AND tenant_id = $8",
                    )
                    .bind(id)
                    .bind(title)
                    .bind(content)
                    .bind(audience)
                    .bind(category)
                    .bind(sort_order)
                    .bind(published)
                    .bind(account)
                    .execute(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    if res.rows_affected() == 0 {
                        return Err(ToolError::NotFound);
                    }
                    Ok(json!({ "id": id, "updated": true }))
                })
            },
        );
    }
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_archive_kb_article",
                "Unpublish (soft) or hard-delete a knowledge-base article by id.",
            )
            .schema(json!({
                "type": "object",
                "properties": {"id": {"type": "string"}, "hard_delete": {"type": "boolean"}},
                "required": ["id"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let id = ctx.uuid_arg(&args, "id")?;
                    let res = if opt_bool(&args, "hard_delete") {
                        sqlx::query("DELETE FROM knowledge_base WHERE id = $1 AND tenant_id = $2")
                            .bind(id)
                            .bind(account)
                            .execute(&pool)
                            .await
                            .map_err(|e| ToolError::Unavailable(e.to_string()))?
                    } else {
                        sqlx::query("UPDATE knowledge_base SET is_published = false, updated_at = now() WHERE id = $1 AND tenant_id = $2")
                            .bind(id)
                            .bind(account)
                            .execute(&pool)
                            .await
                            .map_err(|e| ToolError::Unavailable(e.to_string()))?
                    };
                    if res.rows_affected() == 0 {
                        return Err(ToolError::NotFound);
                    }
                    Ok(json!({ "id": id, "archived": true }))
                })
            },
        );
    }

    // ── W20 campaign_custom_fields: list / create / update / archive ───────────
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::read(
                "incentiveswift_list_custom_fields",
                "List a campaign's entry-form custom fields, scoped to this workspace.",
            )
            .schema(json!({
                "type": "object",
                "properties": {"campaign_id": {"type": "string"}},
                "required": ["campaign_id"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let campaign_id = ctx.uuid_arg(&args, "campaign_id")?;
                    let rows = sqlx::query(
                        "SELECT ccf.id, ccf.field_key, ccf.field_label, ccf.field_type, \
                                ccf.sort_order, ccf.required, ccf.options \
                         FROM campaign_custom_fields ccf \
                         JOIN campaigns c ON c.id = ccf.campaign_id \
                         WHERE c.account_id = $1 AND ccf.campaign_id = $2 ORDER BY ccf.sort_order",
                    )
                    .bind(account)
                    .bind(campaign_id)
                    .fetch_all(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    let items: Vec<Value> = rows
                        .iter()
                        .map(|r| {
                            json!({
                                "id": r.get::<Uuid, _>("id"),
                                "field_key": r.get::<String, _>("field_key"),
                                "field_label": r.get::<String, _>("field_label"),
                                "field_type": r.get::<String, _>("field_type"),
                                "sort_order": r.get::<i32, _>("sort_order"),
                                "required": r.get::<bool, _>("required"),
                                "options": r.get::<Vec<String>, _>("options"),
                            })
                        })
                        .collect();
                    Ok(json!({ "count": items.len(), "items": items }))
                })
            },
        );
    }
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_create_custom_field",
                "Add a custom field to a campaign's entry form, scoped to this workspace.",
            )
            .schema(json!({
                "type": "object",
                "properties": {
                    "campaign_id": {"type": "string"},
                    "field_key": {"type": "string"},
                    "field_label": {"type": "string"},
                    "field_type": {"type": "string"},
                    "sort_order": {"type": "integer"},
                    "required": {"type": "boolean"},
                    "options": {"type": "array", "items": {"type": "string"}}
                },
                "required": ["campaign_id", "field_key", "field_label"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let campaign_id = ctx.uuid_arg(&args, "campaign_id")?;
                    let owned = scoped_owned(
                        &pool,
                        "SELECT id FROM campaigns WHERE id = $1 AND account_id = $2",
                        campaign_id,
                        account,
                    )
                    .await?;
                    if !owned {
                        return Err(ToolError::NotFound);
                    }
                    let field_key = ctx.str_arg(&args, "field_key")?.trim().to_string();
                    let field_label = ctx.str_arg(&args, "field_label")?.trim().to_string();
                    let field_type = opt_str(&args, "field_type").unwrap_or("text").trim().to_string();
                    let sort_order = args.get("sort_order").and_then(|v| v.as_i64()).map(|v| v as i32).unwrap_or(0);
                    let required = args.get("required").and_then(|v| v.as_bool()).unwrap_or(false);
                    let options = str_array(&args, "options").unwrap_or_default();
                    let id = Uuid::new_v4();
                    sqlx::query(
                        "INSERT INTO campaign_custom_fields (id, campaign_id, field_key, field_label, field_type, sort_order, required, options) \
                         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
                    )
                    .bind(id)
                    .bind(campaign_id)
                    .bind(&field_key)
                    .bind(&field_label)
                    .bind(&field_type)
                    .bind(sort_order)
                    .bind(required)
                    .bind(&options)
                    .execute(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    Ok(json!({ "id": id, "field_key": field_key, "field_label": field_label, "field_type": field_type }))
                })
            },
        );
    }
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_update_custom_field",
                "Update a campaign custom field's label, type, order, required flag or options.",
            )
            .schema(json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "field_label": {"type": "string"},
                    "field_type": {"type": "string"},
                    "sort_order": {"type": "integer"},
                    "required": {"type": "boolean"},
                    "options": {"type": "array", "items": {"type": "string"}}
                },
                "required": ["id"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let id = ctx.uuid_arg(&args, "id")?;
                    let field_label = opt_str(&args, "field_label").map(|s| s.trim().to_string());
                    let field_type = opt_str(&args, "field_type").map(|s| s.trim().to_string());
                    let sort_order = args.get("sort_order").and_then(|v| v.as_i64()).map(|v| v as i32);
                    let required = args.get("required").and_then(|v| v.as_bool());
                    let options = str_array(&args, "options");
                    let res = sqlx::query(
                        "UPDATE campaign_custom_fields ccf SET field_label = COALESCE($2, field_label), \
                         field_type = COALESCE($3, field_type), sort_order = COALESCE($4, sort_order), \
                         required = COALESCE($5, required), options = COALESCE($6, options) \
                         WHERE ccf.id = $1 AND ccf.campaign_id IN (SELECT id FROM campaigns WHERE account_id = $7)",
                    )
                    .bind(id)
                    .bind(field_label)
                    .bind(field_type)
                    .bind(sort_order)
                    .bind(required)
                    .bind(options)
                    .bind(account)
                    .execute(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    if res.rows_affected() == 0 {
                        return Err(ToolError::NotFound);
                    }
                    Ok(json!({ "id": id, "updated": true }))
                })
            },
        );
    }
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_archive_custom_field",
                "Delete a campaign custom field by id, scoped to this workspace.",
            )
            .schema(json!({
                "type": "object",
                "properties": {"id": {"type": "string"}},
                "required": ["id"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let id = ctx.uuid_arg(&args, "id")?;
                    let res = sqlx::query(
                        "DELETE FROM campaign_custom_fields ccf WHERE ccf.id = $1 AND \
                         ccf.campaign_id IN (SELECT id FROM campaigns WHERE account_id = $2)",
                    )
                    .bind(id)
                    .bind(account)
                    .execute(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    if res.rows_affected() == 0 {
                        return Err(ToolError::NotFound);
                    }
                    Ok(json!({ "id": id, "archived": true }))
                })
            },
        );
    }

    // ── W21 questions (quiz): list / create / update / archive ─────────────────
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::read(
                "incentiveswift_list_questions",
                "List a campaign's quiz questions, scoped to this workspace.",
            )
            .schema(json!({
                "type": "object",
                "properties": {"campaign_id": {"type": "string"}},
                "required": ["campaign_id"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let campaign_id = ctx.uuid_arg(&args, "campaign_id")?;
                    let rows = sqlx::query(
                        "SELECT q.id, q.question_key, q.question_text, q.question_type, \
                                q.sort_order, q.correct_answer, q.score_weight, q.options, \
                                q.crm_field, q.crm_field_type \
                         FROM questions q JOIN campaigns c ON c.id = q.campaign_id \
                         WHERE c.account_id = $1 AND q.campaign_id = $2 ORDER BY q.sort_order",
                    )
                    .bind(account)
                    .bind(campaign_id)
                    .fetch_all(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    let items: Vec<Value> = rows
                        .iter()
                        .map(|r| {
                            json!({
                                "id": r.get::<Uuid, _>("id"),
                                "question_key": r.get::<String, _>("question_key"),
                                "question_text": r.get::<String, _>("question_text"),
                                "question_type": r.get::<Option<String>, _>("question_type"),
                                "sort_order": r.get::<i32, _>("sort_order"),
                                "correct_answer": r.get::<Option<String>, _>("correct_answer"),
                                "score_weight": r.get::<i32, _>("score_weight"),
                                "options": r.get::<Option<Value>, _>("options"),
                                "crm_field": r.get::<Option<String>, _>("crm_field"),
                                "crm_field_type": r.get::<Option<String>, _>("crm_field_type"),
                            })
                        })
                        .collect();
                    Ok(json!({ "count": items.len(), "items": items }))
                })
            },
        );
    }
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_create_question",
                "Add a quiz question to a campaign, scoped to this workspace.",
            )
            .schema(json!({
                "type": "object",
                "properties": {
                    "campaign_id": {"type": "string"},
                    "question_key": {"type": "string"},
                    "question_text": {"type": "string"},
                    "question_type": {"type": "string"},
                    "sort_order": {"type": "integer"},
                    "correct_answer": {"type": "string"},
                    "score_weight": {"type": "integer"},
                    "options": {"type": "object"},
                    "crm_field": {"type": "string"},
                    "crm_field_type": {"type": "string"}
                },
                "required": ["campaign_id", "question_key", "question_text"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let campaign_id = ctx.uuid_arg(&args, "campaign_id")?;
                    let owned = scoped_owned(
                        &pool,
                        "SELECT id FROM campaigns WHERE id = $1 AND account_id = $2",
                        campaign_id,
                        account,
                    )
                    .await?;
                    if !owned {
                        return Err(ToolError::NotFound);
                    }
                    let question_key = ctx.str_arg(&args, "question_key")?.trim().to_string();
                    let question_text = ctx.str_arg(&args, "question_text")?.trim().to_string();
                    let question_type = opt_str(&args, "question_type").map(|s| s.trim().to_string());
                    let sort_order = args.get("sort_order").and_then(|v| v.as_i64()).map(|v| v as i32).unwrap_or(0);
                    let correct_answer = opt_str(&args, "correct_answer").map(|s| s.to_string());
                    let score_weight = args.get("score_weight").and_then(|v| v.as_i64()).map(|v| v as i32).unwrap_or(1);
                    let options = args.get("options").cloned().unwrap_or_else(|| json!({}));
                    let crm_field = opt_str(&args, "crm_field").map(|s| s.to_string());
                    let crm_field_type = opt_str(&args, "crm_field_type").map(|s| s.to_string());
                    let id = Uuid::new_v4();
                    sqlx::query(
                        "INSERT INTO questions (id, campaign_id, question_key, question_text, question_type, \
                         sort_order, correct_answer, score_weight, options, crm_field, crm_field_type) \
                         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
                    )
                    .bind(id)
                    .bind(campaign_id)
                    .bind(&question_key)
                    .bind(&question_text)
                    .bind(question_type)
                    .bind(sort_order)
                    .bind(correct_answer)
                    .bind(score_weight)
                    .bind(&options)
                    .bind(crm_field)
                    .bind(crm_field_type)
                    .execute(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    Ok(json!({ "id": id, "question_key": question_key, "question_text": question_text }))
                })
            },
        );
    }
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_update_question",
                "Update a quiz question's text, type, answer, order, weight or options.",
            )
            .schema(json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "question_text": {"type": "string"},
                    "question_type": {"type": "string"},
                    "sort_order": {"type": "integer"},
                    "correct_answer": {"type": "string"},
                    "score_weight": {"type": "integer"},
                    "options": {"type": "object"}
                },
                "required": ["id"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let id = ctx.uuid_arg(&args, "id")?;
                    let question_text = opt_str(&args, "question_text").map(|s| s.trim().to_string());
                    let question_type = opt_str(&args, "question_type").map(|s| s.trim().to_string());
                    let sort_order = args.get("sort_order").and_then(|v| v.as_i64()).map(|v| v as i32);
                    let correct_answer = opt_str(&args, "correct_answer").map(|s| s.to_string());
                    let score_weight = args.get("score_weight").and_then(|v| v.as_i64()).map(|v| v as i32);
                    let options = args.get("options").cloned();
                    let res = sqlx::query(
                        "UPDATE questions q SET question_text = COALESCE($2, question_text), \
                         question_type = COALESCE($3, question_type), sort_order = COALESCE($4, sort_order), \
                         correct_answer = COALESCE($5, correct_answer), score_weight = COALESCE($6, score_weight), \
                         options = COALESCE($7, options) \
                         WHERE q.id = $1 AND q.campaign_id IN (SELECT id FROM campaigns WHERE account_id = $8)",
                    )
                    .bind(id)
                    .bind(question_text)
                    .bind(question_type)
                    .bind(sort_order)
                    .bind(correct_answer)
                    .bind(score_weight)
                    .bind(options)
                    .bind(account)
                    .execute(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    if res.rows_affected() == 0 {
                        return Err(ToolError::NotFound);
                    }
                    Ok(json!({ "id": id, "updated": true }))
                })
            },
        );
    }
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_archive_question",
                "Delete a quiz question by id, scoped to this workspace.",
            )
            .schema(json!({
                "type": "object",
                "properties": {"id": {"type": "string"}},
                "required": ["id"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let id = ctx.uuid_arg(&args, "id")?;
                    let res = sqlx::query(
                        "DELETE FROM questions q WHERE q.id = $1 AND \
                         q.campaign_id IN (SELECT id FROM campaigns WHERE account_id = $2)",
                    )
                    .bind(id)
                    .bind(account)
                    .execute(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    if res.rows_affected() == 0 {
                        return Err(ToolError::NotFound);
                    }
                    Ok(json!({ "id": id, "archived": true }))
                })
            },
        );
    }

    // ── W22 integration_targets: list / create / update / archive ──────────────
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::read(
                "incentiveswift_list_integration_targets",
                "List this workspace's integration targets (webhooks). The api_key is never returned.",
            ),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let limit = ctx.limit(args.get("limit"), 25, 200);
                    let account = account_id(&ctx)?;
                    let rows = sqlx::query(
                        "SELECT id, portfolio_company_id, name, provider, webhook_url, events, \
                                is_active, allowed_domains, daily_limit, created_at \
                         FROM integration_targets WHERE account_id = $1 ORDER BY created_at DESC LIMIT $2",
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
                                "portfolio_company_id": r.get::<Option<Uuid>, _>("portfolio_company_id"),
                                "name": r.get::<String, _>("name"),
                                "provider": r.get::<String, _>("provider"),
                                "webhook_url": r.get::<String, _>("webhook_url"),
                                "events": r.get::<Vec<String>, _>("events"),
                                "is_active": r.get::<bool, _>("is_active"),
                                "allowed_domains": r.get::<Vec<String>, _>("allowed_domains"),
                                "daily_limit": r.get::<i32, _>("daily_limit"),
                                "created_at": r.get::<DateTime<Utc>, _>("created_at"),
                            })
                        })
                        .collect();
                    Ok(json!({ "count": items.len(), "items": items }))
                })
            },
        );
    }
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_create_integration_target",
                "Create an integration target (webhook) for this workspace. The api_key is accepted but never echoed.",
            )
            .schema(json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "webhook_url": {"type": "string"},
                    "provider": {"type": "string"},
                    "api_key": {"type": "string"},
                    "events": {"type": "array", "items": {"type": "string"}},
                    "allowed_domains": {"type": "array", "items": {"type": "string"}},
                    "daily_limit": {"type": "integer"},
                    "portfolio_company_id": {"type": "string"}
                },
                "required": ["name", "webhook_url"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let name = ctx.str_arg(&args, "name")?.trim().to_string();
                    let webhook_url = ctx.str_arg(&args, "webhook_url")?.trim().to_string();
                    let provider = opt_str(&args, "provider").unwrap_or("webhook").trim().to_string();
                    let api_key = opt_str(&args, "api_key").map(|s| s.to_string());
                    let events = str_array(&args, "events").unwrap_or_default();
                    let allowed_domains = str_array(&args, "allowed_domains").unwrap_or_default();
                    let daily_limit = args.get("daily_limit").and_then(|v| v.as_i64()).map(|v| v as i32).unwrap_or(1000);
                    let portfolio_company_id = args.get("portfolio_company_id")
                        .and_then(|v| v.as_str())
                        .and_then(|s| Uuid::parse_str(s).ok());
                    let id = Uuid::new_v4();
                    sqlx::query(
                        "INSERT INTO integration_targets (id, account_id, portfolio_company_id, name, provider, \
                         webhook_url, api_key, events, allowed_domains, daily_limit) \
                         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
                    )
                    .bind(id)
                    .bind(account)
                    .bind(portfolio_company_id)
                    .bind(&name)
                    .bind(&provider)
                    .bind(&webhook_url)
                    .bind(api_key)
                    .bind(&events)
                    .bind(&allowed_domains)
                    .bind(daily_limit)
                    .execute(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    Ok(json!({ "id": id, "name": name, "provider": provider, "webhook_url": webhook_url }))
                })
            },
        );
    }
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_update_integration_target",
                "Update an integration target's fields by id, scoped to this workspace. The api_key is accepted but never echoed.",
            )
            .schema(json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "name": {"type": "string"},
                    "webhook_url": {"type": "string"},
                    "provider": {"type": "string"},
                    "api_key": {"type": "string"},
                    "events": {"type": "array", "items": {"type": "string"}},
                    "allowed_domains": {"type": "array", "items": {"type": "string"}},
                    "daily_limit": {"type": "integer"},
                    "is_active": {"type": "boolean"}
                },
                "required": ["id"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let id = ctx.uuid_arg(&args, "id")?;
                    let name = opt_str(&args, "name").map(|s| s.trim().to_string());
                    let webhook_url = opt_str(&args, "webhook_url").map(|s| s.trim().to_string());
                    let provider = opt_str(&args, "provider").map(|s| s.trim().to_string());
                    let api_key = opt_str(&args, "api_key").map(|s| s.to_string());
                    let events = str_array(&args, "events");
                    let allowed_domains = str_array(&args, "allowed_domains");
                    let daily_limit = args.get("daily_limit").and_then(|v| v.as_i64()).map(|v| v as i32);
                    let is_active = args.get("is_active").and_then(|v| v.as_bool());
                    let res = sqlx::query(
                        "UPDATE integration_targets SET name = COALESCE($2, name), \
                         webhook_url = COALESCE($3, webhook_url), provider = COALESCE($4, provider), \
                         api_key = COALESCE($5, api_key), events = COALESCE($6, events), \
                         allowed_domains = COALESCE($7, allowed_domains), daily_limit = COALESCE($8, daily_limit), \
                         is_active = COALESCE($9, is_active), updated_at = now() \
                         WHERE id = $1 AND account_id = $10",
                    )
                    .bind(id)
                    .bind(name)
                    .bind(webhook_url)
                    .bind(provider)
                    .bind(api_key)
                    .bind(events)
                    .bind(allowed_domains)
                    .bind(daily_limit)
                    .bind(is_active)
                    .bind(account)
                    .execute(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    if res.rows_affected() == 0 {
                        return Err(ToolError::NotFound);
                    }
                    Ok(json!({ "id": id, "updated": true }))
                })
            },
        );
    }
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_archive_integration_target",
                "Deactivate (soft) or hard-delete an integration target by id.",
            )
            .schema(json!({
                "type": "object",
                "properties": {"id": {"type": "string"}, "hard_delete": {"type": "boolean"}},
                "required": ["id"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let id = ctx.uuid_arg(&args, "id")?;
                    let res = if opt_bool(&args, "hard_delete") {
                        sqlx::query("DELETE FROM integration_targets WHERE id = $1 AND account_id = $2")
                            .bind(id)
                            .bind(account)
                            .execute(&pool)
                            .await
                            .map_err(|e| ToolError::Unavailable(e.to_string()))?
                    } else {
                        sqlx::query("UPDATE integration_targets SET is_active = false, updated_at = now() WHERE id = $1 AND account_id = $2")
                            .bind(id)
                            .bind(account)
                            .execute(&pool)
                            .await
                            .map_err(|e| ToolError::Unavailable(e.to_string()))?
                    };
                    if res.rows_affected() == 0 {
                        return Err(ToolError::NotFound);
                    }
                    Ok(json!({ "id": id, "archived": true }))
                })
            },
        );
    }

    // ── W23 portfolio_companies: list / create / update / archive ──────────────
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::read(
                "incentiveswift_list_portfolio_companies",
                "List this workspace's portfolio companies, newest first.",
            ),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let limit = ctx.limit(args.get("limit"), 25, 200);
                    let account = account_id(&ctx)?;
                    let rows = sqlx::query(
                        "SELECT id, name, slug, domain, domain_verified, email, description, created_at \
                         FROM portfolio_companies WHERE account_id = $1 ORDER BY created_at DESC LIMIT $2",
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
                                "domain": r.get::<Option<String>, _>("domain"),
                                "domain_verified": r.get::<bool, _>("domain_verified"),
                                "email": r.get::<String, _>("email"),
                                "description": r.get::<String, _>("description"),
                                "created_at": r.get::<DateTime<Utc>, _>("created_at"),
                            })
                        })
                        .collect();
                    Ok(json!({ "count": items.len(), "items": items }))
                })
            },
        );
    }
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_create_portfolio_company",
                "Create a portfolio company (name, optional domain and description) for this workspace.",
            )
            .schema(json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "domain": {"type": "string"},
                    "email": {"type": "string"},
                    "description": {"type": "string"},
                    "subdomain": {"type": "string"}
                },
                "required": ["name"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let name = ctx.str_arg(&args, "name")?.trim().to_string();
                    let domain = opt_str(&args, "domain").map(|s| s.trim().to_string());
                    let email = opt_str(&args, "email").map(|s| s.trim().to_string()).unwrap_or_default();
                    let description = opt_str(&args, "description").map(|s| s.trim().to_string()).unwrap_or_default();
                    let subdomain = opt_str(&args, "subdomain").map(|s| s.trim().to_string());
                    let id = Uuid::new_v4();
                    let slug = slugify(&name);
                    sqlx::query(
                        "INSERT INTO portfolio_companies (id, account_id, name, slug, email, description, subdomain, domain) \
                         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
                    )
                    .bind(id)
                    .bind(account)
                    .bind(&name)
                    .bind(&slug)
                    .bind(&email)
                    .bind(&description)
                    .bind(subdomain)
                    .bind(domain)
                    .execute(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    Ok(json!({ "id": id, "name": name, "slug": slug }))
                })
            },
        );
    }
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_update_portfolio_company",
                "Update a portfolio company's name, domain, email or description by id.",
            )
            .schema(json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "name": {"type": "string"},
                    "domain": {"type": "string"},
                    "email": {"type": "string"},
                    "description": {"type": "string"},
                    "subdomain": {"type": "string"}
                },
                "required": ["id"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let id = ctx.uuid_arg(&args, "id")?;
                    let name = opt_str(&args, "name").map(|s| s.trim().to_string());
                    let domain = opt_str(&args, "domain").map(|s| s.trim().to_string());
                    let email = opt_str(&args, "email").map(|s| s.trim().to_string());
                    let description = opt_str(&args, "description").map(|s| s.trim().to_string());
                    let subdomain = opt_str(&args, "subdomain").map(|s| s.trim().to_string());
                    let res = sqlx::query(
                        "UPDATE portfolio_companies SET name = COALESCE($2, name), \
                         domain = COALESCE($3, domain), email = COALESCE($4, email), \
                         description = COALESCE($5, description), subdomain = COALESCE($6, subdomain), \
                         updated_at = now() WHERE id = $1 AND account_id = $7",
                    )
                    .bind(id)
                    .bind(name)
                    .bind(domain)
                    .bind(email)
                    .bind(description)
                    .bind(subdomain)
                    .bind(account)
                    .execute(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    if res.rows_affected() == 0 {
                        return Err(ToolError::NotFound);
                    }
                    Ok(json!({ "id": id, "updated": true }))
                })
            },
        );
    }
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_archive_portfolio_company",
                "Delete a portfolio company by id, scoped to this workspace.",
            )
            .schema(json!({
                "type": "object",
                "properties": {"id": {"type": "string"}},
                "required": ["id"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let id = ctx.uuid_arg(&args, "id")?;
                    let res = sqlx::query(
                        "DELETE FROM portfolio_companies WHERE id = $1 AND account_id = $2",
                    )
                    .bind(id)
                    .bind(account)
                    .execute(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    if res.rows_affected() == 0 {
                        return Err(ToolError::NotFound);
                    }
                    Ok(json!({ "id": id, "archived": true }))
                })
            },
        );
    }

    // ── W24 surfaces: list / create / update / archive ─────────────────────────
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::read(
                "incentiveswift_list_surfaces",
                "List this workspace's surfaces (named embedding surfaces).",
            ),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let limit = ctx.limit(args.get("limit"), 25, 200);
                    let account = account_id(&ctx)?;
                    let rows = sqlx::query(
                        "SELECT id, name, config, created_at, updated_at FROM surfaces \
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
                                "name": r.get::<Option<String>, _>("name"),
                                "config": r.get::<Value, _>("config"),
                                "created_at": r.get::<DateTime<Utc>, _>("created_at"),
                                "updated_at": r.get::<DateTime<Utc>, _>("updated_at"),
                            })
                        })
                        .collect();
                    Ok(json!({ "count": items.len(), "items": items }))
                })
            },
        );
    }
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_create_surface",
                "Create a named surface for this workspace.",
            )
            .schema(json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "config": {"type": "object"}
                },
                "required": ["name"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let name = ctx.str_arg(&args, "name")?.trim().to_string();
                    let config = args.get("config").cloned().unwrap_or_else(|| json!({}));
                    let id = Uuid::new_v4();
                    sqlx::query(
                        "INSERT INTO surfaces (id, account_id, name, config) VALUES ($1, $2, $3, $4)",
                    )
                    .bind(id)
                    .bind(account)
                    .bind(&name)
                    .bind(&config)
                    .execute(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    Ok(json!({ "id": id, "name": name }))
                })
            },
        );
    }
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_update_surface",
                "Rename a surface or replace its config by id, scoped to this workspace.",
            )
            .schema(json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "name": {"type": "string"},
                    "config": {"type": "object"}
                },
                "required": ["id"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let id = ctx.uuid_arg(&args, "id")?;
                    let name = opt_str(&args, "name").map(|s| s.trim().to_string());
                    let config = args.get("config").cloned();
                    let res = sqlx::query(
                        "UPDATE surfaces SET name = COALESCE($2, name), config = COALESCE($3, config), \
                         updated_at = now() WHERE id = $1 AND account_id = $4",
                    )
                    .bind(id)
                    .bind(name)
                    .bind(config)
                    .bind(account)
                    .execute(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    if res.rows_affected() == 0 {
                        return Err(ToolError::NotFound);
                    }
                    Ok(json!({ "id": id, "updated": true }))
                })
            },
        );
    }
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_archive_surface",
                "Delete a surface by id, scoped to this workspace.",
            )
            .schema(json!({
                "type": "object",
                "properties": {"id": {"type": "string"}},
                "required": ["id"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let id = ctx.uuid_arg(&args, "id")?;
                    let res = sqlx::query("DELETE FROM surfaces WHERE id = $1 AND account_id = $2")
                        .bind(id)
                        .bind(account)
                        .execute(&pool)
                        .await
                        .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    if res.rows_affected() == 0 {
                        return Err(ToolError::NotFound);
                    }
                    Ok(json!({ "id": id, "archived": true }))
                })
            },
        );
    }

    // ── W25 campaign_integrations: link / unlink ───────────────────────────────
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_link_integration",
                "Link an integration target to a campaign (both must be owned by this workspace). Idempotent.",
            )
            .schema(json!({
                "type": "object",
                "properties": {
                    "campaign_id": {"type": "string"},
                    "integration_id": {"type": "string"},
                    "trigger_events": {"type": "array", "items": {"type": "string"}}
                },
                "required": ["campaign_id", "integration_id"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let campaign_id = ctx.uuid_arg(&args, "campaign_id")?;
                    let integration_id = ctx.uuid_arg(&args, "integration_id")?;
                    let trigger_events = str_array(&args, "trigger_events").unwrap_or_default();
                    let id = Uuid::new_v4();
                    let res = sqlx::query(
                        "INSERT INTO campaign_integrations (id, campaign_id, integration_id, trigger_events, enabled) \
                         SELECT $1, $2, $3, $4, true \
                         WHERE EXISTS (SELECT 1 FROM campaigns c WHERE c.id = $2 AND c.account_id = $5) \
                           AND EXISTS (SELECT 1 FROM integration_targets it WHERE it.id = $3 AND it.account_id = $5) \
                         ON CONFLICT (campaign_id, integration_id) \
                         DO UPDATE SET enabled = true, trigger_events = EXCLUDED.trigger_events, updated_at = now()",
                    )
                    .bind(id)
                    .bind(campaign_id)
                    .bind(integration_id)
                    .bind(&trigger_events)
                    .bind(account)
                    .execute(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    if res.rows_affected() == 0 {
                        return Err(ToolError::NotFound);
                    }
                    Ok(json!({ "campaign_id": campaign_id, "integration_id": integration_id, "linked": true }))
                })
            },
        );
    }
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_unlink_integration",
                "Unlink an integration target from a campaign, scoped to this workspace.",
            )
            .schema(json!({
                "type": "object",
                "properties": {
                    "campaign_id": {"type": "string"},
                    "integration_id": {"type": "string"}
                },
                "required": ["campaign_id", "integration_id"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let campaign_id = ctx.uuid_arg(&args, "campaign_id")?;
                    let integration_id = ctx.uuid_arg(&args, "integration_id")?;
                    let res = sqlx::query(
                        "DELETE FROM campaign_integrations ci \
                         WHERE ci.campaign_id = $1 AND ci.integration_id = $2 \
                           AND EXISTS (SELECT 1 FROM campaigns c WHERE c.id = $1 AND c.account_id = $3) \
                           AND EXISTS (SELECT 1 FROM integration_targets it WHERE it.id = $2 AND it.account_id = $3)",
                    )
                    .bind(campaign_id)
                    .bind(integration_id)
                    .bind(account)
                    .execute(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    if res.rows_affected() == 0 {
                        return Err(ToolError::NotFound);
                    }
                    Ok(json!({ "campaign_id": campaign_id, "integration_id": integration_id, "unlinked": true }))
                })
            },
        );
    }

    // ── S1 loyalty reward status (approve / deny) ──────────────────────────────
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_update_reward_status",
                "Approve or deny a pending loyalty reward redemption, scoped to this workspace's program.",
            )
            .schema(json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "status": {"type": "string"}
                },
                "required": ["id", "status"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let id = ctx.uuid_arg(&args, "id")?;
                    let status = ctx.str_arg(&args, "status")?.trim().to_string();
                    if status != "approved" && status != "denied" {
                        return Err(ToolError::Invalid("status must be 'approved' or 'denied'".into()));
                    }
                    let owned = scoped_owned(
                        &pool,
                        "SELECT lre.id FROM loyalty_rewards_earned lre \
                         JOIN loyalty_reward_tiers lrt ON lrt.id = lre.tier_id \
                         JOIN loyalty_programs lp ON lp.id = lrt.program_id \
                         LEFT JOIN campaigns c ON c.id = lp.campaign_id \
                         WHERE lre.id = $1 AND (lp.account_id = $2 OR c.account_id = $2)",
                        id,
                        account,
                    )
                    .await?;
                    if !owned {
                        return Err(ToolError::NotFound);
                    }
                    let current: Option<String> =
                        sqlx::query_scalar("SELECT status FROM loyalty_rewards_earned WHERE id = $1")
                            .bind(id)
                            .fetch_optional(&pool)
                            .await
                            .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    if current.as_deref() != Some("pending") {
                        return Err(ToolError::Invalid(format!(
                            "reward is already {}",
                            current.unwrap_or_else(|| "settled".into())
                        )));
                    }
                    sqlx::query(
                        "UPDATE loyalty_rewards_earned SET status = $2, approved_by = $3, fulfilled_at = now() WHERE id = $1",
                    )
                    .bind(id)
                    .bind(&status)
                    .bind(account)
                    .execute(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    Ok(json!({ "id": id, "status": status }))
                })
            },
        );
    }

    // ── S2 support-ticket message ──────────────────────────────────────────────
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_add_ticket_message",
                "Add a message to a support ticket (author is this workspace's account).",
            )
            .schema(json!({
                "type": "object",
                "properties": {
                    "ticket_id": {"type": "string"},
                    "body": {"type": "string"},
                    "is_internal": {"type": "boolean"}
                },
                "required": ["ticket_id", "body"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let ticket_id = ctx.uuid_arg(&args, "ticket_id")?;
                    let body = ctx.str_arg(&args, "body")?.trim().to_string();
                    let is_internal = args.get("is_internal").and_then(|v| v.as_bool()).unwrap_or(false);
                    let id = Uuid::new_v4();
                    let res = sqlx::query(
                        "INSERT INTO support_ticket_messages (id, ticket_id, author_id, body, is_internal) \
                         SELECT $1, $2, $3, $4, $5 \
                         WHERE EXISTS (SELECT 1 FROM support_tickets WHERE id = $2 AND tenant_id = $3)",
                    )
                    .bind(id)
                    .bind(ticket_id)
                    .bind(account)
                    .bind(&body)
                    .bind(is_internal)
                    .execute(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    if res.rows_affected() == 0 {
                        return Err(ToolError::NotFound);
                    }
                    Ok(json!({ "id": id, "ticket_id": ticket_id }))
                })
            },
        );
    }

    // ── S3 member QR regenerate ────────────────────────────────────────────────
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_regenerate_member_qr",
                "Regenerate a loyalty member's QR code (e.g. if compromised), scoped to this workspace's program.",
            )
            .schema(json!({
                "type": "object",
                "properties": {"member_id": {"type": "string"}},
                "required": ["member_id"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let member_id = ctx.uuid_arg(&args, "member_id")?;
                    let owned = scoped_owned(
                        &pool,
                        "SELECT lm.id FROM loyalty_members lm \
                         JOIN loyalty_programs lp ON lp.id = lm.program_id \
                         WHERE lm.id = $1 AND (lp.account_id = $2 \
                             OR EXISTS (SELECT 1 FROM campaigns c WHERE c.id = lp.campaign_id AND c.account_id = $2) \
                             OR EXISTS (SELECT 1 FROM campaigns fwd WHERE fwd.loyalty_program_id = lp.id AND fwd.account_id = $2))",
                        member_id,
                        account,
                    )
                    .await?;
                    if !owned {
                        return Err(ToolError::NotFound);
                    }
                    let suffix = &Uuid::new_v4().simple().to_string()[..8];
                    let qr_code = format!("{}-{}", member_id.simple().to_string().split_at(8).0, suffix);
                    sqlx::query(
                        "UPDATE loyalty_members SET qr_code = $1, qr_code_generated_at = now() WHERE id = $2",
                    )
                    .bind(&qr_code)
                    .bind(member_id)
                    .execute(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    Ok(json!({ "member_id": member_id, "qr_code": qr_code, "regenerated": true }))
                })
            },
        );
    }

    // ── S4 raffle draw / redraw ────────────────────────────────────────────────
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_draw_raffle_winner",
                "Draw (or, with redraw=true, redraw) a raffle campaign's winner, scoped to this workspace. Requires entries.",
            )
            .schema(json!({
                "type": "object",
                "properties": {
                    "campaign_id": {"type": "string"},
                    "redraw": {"type": "boolean"}
                },
                "required": ["campaign_id"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    use rand::Rng;
                    let account = account_id(&ctx)?;
                    let campaign_id = ctx.uuid_arg(&args, "campaign_id")?;
                    let ctype: Option<String> = sqlx::query_scalar(
                        "SELECT type FROM campaigns WHERE id = $1 AND account_id = $2",
                    )
                    .bind(campaign_id)
                    .bind(account)
                    .fetch_optional(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    let ctype = ctype.ok_or(ToolError::NotFound)?;
                    if ctype != "raffle" {
                        return Err(ToolError::Invalid("campaign is not a raffle".into()));
                    }
                    let redraw = opt_bool(&args, "redraw");
                    if !redraw
                        && crate::db::raffles::has_existing_draw(&pool, &campaign_id)
                            .await
                            .map_err(|e| ToolError::Unavailable(e.to_string()))?
                    {
                        return Err(ToolError::Invalid("raffle already drawn — use redraw".into()));
                    }
                    let mut entry_ids = crate::db::raffles::get_entries_for_draw(&pool, &campaign_id)
                        .await
                        .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    if redraw {
                        let current_winner: Option<Uuid> = sqlx::query_scalar(
                            "SELECT id FROM entries WHERE campaign_id = $1 AND outcome = 'winner' LIMIT 1",
                        )
                        .bind(campaign_id)
                        .fetch_optional(&pool)
                        .await
                        .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                        if let Some(w) = current_winner {
                            entry_ids.retain(|id| *id != w);
                        }
                    }
                    if entry_ids.is_empty() {
                        return Err(ToolError::Invalid("no entries to draw from".into()));
                    }
                    let seed: u64 = rand::thread_rng().gen();
                    let entry_strings: Vec<String> = entry_ids.iter().map(|id| id.to_string()).collect();
                    let winner_id = crate::mechanics::raffle_draw::seeded_fisher_yates(&entry_strings, seed)
                        .ok_or_else(|| ToolError::Unavailable("no winner found".into()))?;
                    let winner_uuid = Uuid::parse_str(&winner_id)
                        .map_err(|_| ToolError::Unavailable("invalid winner id".into()))?;
                    crate::db::raffles::record_draw(&pool, &campaign_id, &winner_uuid, seed)
                        .await
                        .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    Ok(json!({ "winner_entry_id": winner_id, "seed": seed, "total_entries": entry_ids.len() }))
                })
            },
        );
    }

    // ── S5 campaign clone ──────────────────────────────────────────────────────
    {
        let pool = pool.clone();
        reg = reg.with_fn(
            ToolSpec::write(
                "incentiveswift_clone_campaign",
                "Clone an existing campaign (name '… (Copy)', fresh slug and tag namespace, custom fields deep-copied), scoped to this workspace.",
            )
            .schema(json!({
                "type": "object",
                "properties": {"slug": {"type": "string"}},
                "required": ["slug"],
                "additionalProperties": false
            })),
            move |ctx, args| {
                let pool = pool.clone();
                Box::pin(async move {
                    let account = account_id(&ctx)?;
                    let slug = ctx.str_arg(&args, "slug")?.trim().to_string();
                    let row = sqlx::query(
                        "SELECT id, name, type, config, tag_namespace, outcome_tags, delivery_method, \
                                delivery_config, loyalty_program_id, loyalty_points_per_play, auto_enroll_loyalty \
                         FROM campaigns WHERE slug = $1 AND account_id = $2",
                    )
                    .bind(&slug)
                    .bind(account)
                    .fetch_optional(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?
                    .ok_or(ToolError::NotFound)?;
                    let original_id: Uuid = row.get("id");
                    let name: String = row.get("name");
                    let ctype: String = row.get("type");
                    let config: Value = row.get("config");
                    let tag_namespace: String = row.get("tag_namespace");
                    let outcome_tags: Value = row.get("outcome_tags");
                    let delivery_method: String = row.get("delivery_method");
                    let delivery_config: Value = row.get("delivery_config");
                    let loyalty_program_id: Option<Uuid> = row.get("loyalty_program_id");
                    let loyalty_points_per_play: i32 = row.get("loyalty_points_per_play");
                    let auto_enroll_loyalty: bool = row.get("auto_enroll_loyalty");

                    crate::features::enforce_feature_limit(
                        &pool,
                        &account.to_string(),
                        "max_campaigns",
                        "Campaigns",
                    )
                    .await
                    .map_err(|e| ToolError::Invalid(e.to_string()))?;

                    let new_name = format!("{} (Copy)", name);
                    let new_slug = crate::db::campaigns::generate_clone_slug(&name);
                    let new_namespace =
                        crate::db::campaigns::clone_tag_namespace(&pool, &account, &tag_namespace)
                            .await
                            .map_err(|e| ToolError::Unavailable(e.to_string()))?;
                    let new_id = Uuid::new_v4();
                    sqlx::query(
                        "INSERT INTO campaigns (id, account_id, name, slug, type, status, config, tag_namespace, \
                         outcome_tags, delivery_method, delivery_config, loyalty_program_id, loyalty_points_per_play, \
                         auto_enroll_loyalty) \
                         VALUES ($1, $2, $3, $4, $5, 'draft', $6, $7, $8, $9, $10, $11, $12, $13)",
                    )
                    .bind(new_id)
                    .bind(account)
                    .bind(&new_name)
                    .bind(&new_slug)
                    .bind(&ctype)
                    .bind(&config)
                    .bind(&new_namespace)
                    .bind(&outcome_tags)
                    .bind(&delivery_method)
                    .bind(&delivery_config)
                    .bind(loyalty_program_id)
                    .bind(loyalty_points_per_play)
                    .bind(auto_enroll_loyalty)
                    .execute(&pool)
                    .await
                    .map_err(|e| ToolError::Unavailable(e.to_string()))?;

                    // Deep-copy the source's entry-form fields; a failure here is non-fatal.
                    let _ = sqlx::query(
                        "INSERT INTO campaign_custom_fields \
                         (id, campaign_id, field_key, field_label, field_type, sort_order, required, options) \
                         SELECT gen_random_uuid(), $1, field_key, field_label, field_type, sort_order, required, options \
                         FROM campaign_custom_fields WHERE campaign_id = $2",
                    )
                    .bind(new_id)
                    .bind(original_id)
                    .execute(&pool)
                    .await;

                    Ok(json!({ "id": new_id, "name": new_name, "slug": new_slug }))
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
    .page_size(TOOLS_PAGE_SIZE)
    .instructions(
        "Read and write access to your IncentiveSwift data: campaigns, participants, loyalty, points, \
         offers, messages and tickets. Scoped to the account that owns the API key.",
    )
    .build()
}

/// The `/mcp` route handler. The door itself is the credential boundary for this path (it answers
/// 401 with the JSON-RPC unauthorized code), so it is mounted as a PUBLIC route.
pub async fn handle(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    state.mcp_door.dispatch(&headers, &body).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::postgres::PgPoolOptions;

    /// The whole inventory must arrive in ONE `tools/list` page at the size the door is built with.
    /// A door that paginates its own tools hides the tail from every client that does not follow
    /// `nextCursor` — the transport default of 50 put 61 of these 111 tools on pages 2 and 3, so a
    /// one-page reading called them "declared but never served" (t_1e521dc1). This fails the build
    /// the moment the inventory outgrows `TOOLS_PAGE_SIZE`.
    #[tokio::test]
    async fn one_page_lists_every_tool() {
        // The registry only clones the pool into each tool's closure — nothing connects during
        // construction or pagination. A lazy pool with a well-formed URL is enough, but
        // `connect_lazy` still needs a Tokio runtime to exist.
        let pool = PgPoolOptions::new()
            .connect_lazy("postgresql://incentiveswift@localhost/incentiveswift")
            .expect("lazy pool");
        let reg = registry(pool);

        let (tools, next) = reg.page(None, TOOLS_PAGE_SIZE).expect("page");
        assert!(
            next.is_none(),
            "one page at {TOOLS_PAGE_SIZE} must hold the whole inventory, got {} tools and a cursor",
            tools.len()
        );
        assert_eq!(
            tools.len(),
            reg.len(),
            "the first page must carry every tool"
        );

        // The first tool that used to spill onto page 2 (0-based index 50) must be on the first
        // page — it is the one a default-page-size reading would have reported missing.
        assert!(
            tools
                .iter()
                .any(|t| t["name"] == "incentiveswift_archive_email_template"),
            "incentiveswift_archive_email_template must be served on the first page"
        );
    }
}
