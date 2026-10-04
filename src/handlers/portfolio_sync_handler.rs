//! Internal portfolio sync handler — receives broadcasts from CoreSwift CRM.
//! Protected by x-internal-key header, not JWT.

use crate::error::AppError;
use crate::state::AppState;
use axum::{extract::State, http::HeaderMap, Json};
use serde_json::{json, Value};
use uuid::Uuid;

/// The parent `accounts` row a mirrored portfolio must have (kanban t_47315540), and the account id
/// the mirrored `portfolio_companies` row must point at.
///
/// This used to be `INSERT INTO accounts (id, name) ... .await.ok()`. `accounts.email` is
/// NOT NULL with no default, so that statement could never succeed: measured live 2026-10-02 it
/// raises `23502 not_null_violation`, the error was swallowed by `.ok()`, and the portfolio row
/// was written with no parent account — which is how the column minted orphans until
/// `portfolio_companies_account_id_fkey` was armed. The payload's `email` field was parsed at the
/// top of this handler and then never bound. Bind it; when it is empty use a non-routable
/// per-account placeholder so NOT NULL and the UNIQUE index are both satisfied without inventing
/// a deliverable mailbox.
///
/// It now RESOLVES instead of blindly minting (kanban t_b5784899). `portfolio_companies.account_id`
/// is `REFERENCES accounts(id)` and the incoming `tenant_id` is a *caller* tenant id, not an account
/// here — the sibling receiver `/api/v1/internal/portfolio-companies` inserted it raw, so every
/// portfolio sync from FunnelSwift raised `23503 portfolio_companies_account_id_fkey` and answered
/// 500 while the other four legs answered 200. Two live cases have to work against the same unique
/// index, so resolve in order and return the id the caller must bind:
///   1. an account already carries this caller tenant (id or tenant_id) — a re-sync, reuse it;
///   2. an account already carries this email — the same business lives here (David's sister
///      companies hold an account in every app), so attach the mirror to it rather than mint a twin
///      whose INSERT would raise `23505` on `accounts_email_key` and re-break the leg;
///   3. otherwise mint the mirror, id = the caller tenant id, so step 1 finds it next time.
pub(crate) async fn ensure_account(
    db: &sqlx::PgPool,
    aid: Uuid,
    name: &str,
    email: &str,
) -> Result<Uuid, AppError> {
    let existing: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM accounts WHERE id = $1 OR tenant_id = $1 LIMIT 1")
            .bind(aid)
            .fetch_optional(db)
            .await?;
    if let Some(id) = existing {
        return Ok(id);
    }
    if !email.is_empty() {
        let by_email: Option<Uuid> =
            sqlx::query_scalar("SELECT id FROM accounts WHERE lower(email) = lower($1) LIMIT 1")
                .bind(email)
                .fetch_optional(db)
                .await?;
        if let Some(id) = by_email {
            return Ok(id);
        }
    }
    let addr = if email.is_empty() {
        format!("portfolio-sync+{aid}@sync.invalid")
    } else {
        email.to_string()
    };
    sqlx::query(
        "INSERT INTO accounts (id, name, email) VALUES ($1, $2, $3) ON CONFLICT (id) DO NOTHING",
    )
    .bind(aid)
    .bind(name)
    .bind(&addr)
    .execute(db)
    .await?;
    Ok(aid)
}

/// POST /api/v1/internal/portfolio-sync
pub async fn portfolio_sync_internal(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, AppError> {
    let key = headers
        .get("x-internal-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    // An empty configured key must never authenticate a caller (kanban t_de6f2986).
    // Unauthorized (401) rather than Forbidden (403): one status for the whole class.
    if state.config.internal_sync_key.is_empty() || key != state.config.internal_sync_key {
        return Err(AppError::Unauthorized("Invalid internal key".into()));
    }

    let action = body
        .get("action")
        .and_then(|v| v.as_str())
        .unwrap_or("create");
    let portfolio_id = body
        .get("portfolio_id")
        .and_then(|v| v.as_str())
        .and_then(|s| Uuid::parse_str(s).ok());
    let tenant_id = body
        .get("tenant_id")
        .and_then(|v| v.as_str())
        .and_then(|s| Uuid::parse_str(s).ok());
    let name = body
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let slug = body
        .get("slug")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let email = body
        .get("email")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    // IncentiveSwift uses account_id not tenant_id for portfolio_companies
    let account_id = tenant_id;

    match action {
        "create" => {
            if let (Some(pid), Some(aid)) = (portfolio_id, account_id) {
                let parent_id = ensure_account(&state.db, aid, &name, &email).await?;
                sqlx::query("INSERT INTO portfolio_companies (id, account_id, name, slug, email) VALUES ($1, $2, $3, $4, $5) ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name, slug = EXCLUDED.slug, email = EXCLUDED.email, updated_at = NOW()")
                    .bind(pid).bind(parent_id).bind(&name).bind(&slug).bind(&email)
                    .execute(&state.db).await?;
            }
        }
        "update" => {
            if let Some(pid) = portfolio_id {
                let rows = sqlx::query("UPDATE portfolio_companies SET name = $1, slug = $2, email = $3, updated_at = NOW() WHERE id = $4")
                    .bind(&name).bind(&slug).bind(&email).bind(pid)
                    .execute(&state.db).await?;
                if rows.rows_affected() == 0 {
                    if let Some(aid) = account_id {
                        let parent_id = ensure_account(&state.db, aid, &name, &email).await?;
                        sqlx::query("INSERT INTO portfolio_companies (id, account_id, name, slug, email) VALUES ($1, $2, $3, $4, $5) ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name, slug = EXCLUDED.slug, email = EXCLUDED.email, updated_at = NOW()")
                            .bind(pid).bind(parent_id).bind(&name).bind(&slug).bind(&email)
                            .execute(&state.db).await?;
                    }
                }
            }
        }
        "delete" => {
            if let Some(pid) = portfolio_id {
                sqlx::query("DELETE FROM portfolio_companies WHERE id = $1")
                    .bind(pid)
                    .execute(&state.db)
                    .await?;
            }
        }
        _ => return Err(AppError::BadRequest("Invalid action".into())),
    }

    Ok(Json(json!({"status": "synced"})))
}
