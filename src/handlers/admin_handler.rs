//! Admin handlers — portfolio sync, impersonation, and admin utility endpoints.

use crate::error::AppError;
use crate::security::auth::AuthenticatedUser;
use crate::state::AppState;
use axum::{
    extract::{Path, State},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

/// Input for impersonation.
#[derive(Deserialize)]
pub struct ImpersonateInput {
    pub account_id: String,
}

/// POST /api/v1/admin/portfolio-sync
/// Syncs portfolio companies from configured external endpoints.
/// Currently logs intent — real integration is app-specific.
pub async fn portfolio_sync(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    tracing::info!("Portfolio sync requested");

    // Fetch existing portfolio companies
    let companies = sqlx::query_as::<_, (uuid::Uuid, String)>(
        "SELECT id, name FROM portfolio_companies ORDER BY name",
    )
    .fetch_all(&state.db)
    .await?;

    Ok(Json(json!({
        "status": "synced",
        "count": companies.len(),
        "companies": companies,
        "note": "Full external sync requires integration-specific configuration"
    })))
}

/// Create a temporary JWT for impersonating another user.
fn create_jwt(
    account_id: &str,
    email: &str,
    role: &str,
    secret: &str,
    impersonating: &str,
) -> Result<String, AppError> {
    use base64::Engine;
    use hmac::{Hmac, Mac};
    use serde_json::json;
    use sha2::Sha256;

    type HmacSha256 = Hmac<Sha256>;

    let header = json!({
        "alg": "HS256",
        "typ": "JWT",
    });

    let now = chrono::Utc::now().timestamp();
    let payload = json!({
        "sub": account_id,
        "email": email,
        "role": role,
        "impersonating": impersonating,
        "iat": now,
        "exp": now + 3600, // 1 hour for impersonation tokens
    });

    let header_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_string(&header).unwrap().as_bytes());
    let payload_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_string(&payload).unwrap().as_bytes());

    let message = format!("{}.{}", header_b64, payload_b64);

    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .map_err(|_| AppError::Internal("Failed to create HMAC".to_string()))?;
    mac.update(message.as_bytes());
    let sig = mac.finalize().into_bytes();

    let sig_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sig);

    Ok(format!("{}.{}", message, sig_b64))
}

/// POST /api/v1/admin/impersonate
/// Generates a temporary JWT to impersonate another user (admin only).
pub async fn impersonate(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<ImpersonateInput>,
) -> Result<Json<Value>, AppError> {
    // Verify the requester is an admin
    if user.role != "admin" && user.role != "super_admin" {
        return Err(AppError::Forbidden(
            "Only admins can impersonate users".to_string(),
        ));
    }

    // Look up the target account
    let target_id = uuid::Uuid::parse_str(&body.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account_id".to_string()))?;

    let row = sqlx::query("SELECT id, email, role FROM accounts WHERE id = $1")
        .bind(target_id)
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Target account not found".to_string()))?;

    let target_email: String = row.get("email");
    let target_role: String = row.get("role");

    // Create impersonation JWT (short-lived, 1 hour)
    let token = create_jwt(
        &body.account_id,
        &target_email,
        &target_role,
        &state.config.jwt_secret,
        &user.account_id,
    )?;

    Ok(Json(json!({
        "token": token,
        "impersonating": {
            "id": body.account_id,
            "email": target_email,
            "role": target_role,
        },
        "expires_in": 3600,
    })))
}

/// POST /api/v1/admin/stop-impersonation
/// Simply returns a confirmation — the client should discard the impersonation token.
pub async fn stop_impersonation(user: AuthenticatedUser) -> Result<Json<Value>, AppError> {
    Ok(Json(json!({
        "status": "impersonation_stopped",
        "message": "Discard your impersonation token to complete the process"
    })))
}

/// Retire ONE tenant: every dependent row, then the tenant, inside ONE transaction.
///
/// Returns the number of `accounts` rows removed (0 = no such tenant). The transaction is the
/// point: the previous version ran its deletes as separate AUTOCOMMIT statements, so a failure on
/// the last one left the earlier ones committed and the account row alive.
///
/// WHY THIS EXISTS (kanban t_9f3d85dc, measured 2026-10-08): `DELETE /api/v1/admin/tenants/:id`
/// answered HTTP 500 and the account SURVIVED. The handler deleted four hand-picked child tables
/// and then the `accounts` row, so Postgres refused on the first child it had not been told about:
///
/// ```text
/// update or delete on table "accounts" violates foreign key constraint
/// ```
///
/// Measured causes, all six of them sitting on this delete path: `inbound_messages.account_id`,
/// `iqs_funnels.account_id`, `password_resets.account_id` and `provider_keys.account_id` carried NO
/// ACTION edges to `accounts`; `loyalty_checkins.entry_id` and
/// `loyalty_rewards_earned.tier_id` block the SECOND level (accounts -> campaigns -> entries,
/// accounts -> loyalty_programs -> loyalty_reward_tiers); and `loyalty_programs.account_id` had no
/// foreign key at all, so deleting an account left live, readable loyalty programs behind.
/// Migration `20261008_tenant_delete_cascade_arms.sql` gives all six an `ON DELETE CASCADE` arm
/// (the column is the ownership pointer of a row nothing can read without the parent), which is
/// what fixes every OTHER delete path too. The four `accounts` children and `loyalty_programs` are
/// still deleted explicitly below so this endpoint stays correct even on a database where that
/// migration has not run yet.
async fn retire_tenant(state: &AppState, account_id: Uuid) -> Result<u64, AppError> {
    let mut tx = state.db.begin().await?;

    // 1. The direct `accounts` children whose edge was NO ACTION, plus the table that had no edge
    //    at all. Each of these is the ownership pointer of a row no reader can reach without this
    //    account (every SELECT in src/ scopes by it), so the row is not preserved data.
    sqlx::query("DELETE FROM loyalty_programs WHERE account_id = $1")
        .bind(account_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM inbound_messages WHERE account_id = $1")
        .bind(account_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM iqs_funnels WHERE account_id = $1")
        .bind(account_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM password_resets WHERE account_id = $1")
        .bind(account_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM provider_keys WHERE account_id = $1")
        .bind(account_id)
        .execute(&mut *tx)
        .await?;

    // 2. The fleet vocabulary rows that share this id. This app never reads `tenants` or `users`
    //    (no SELECT and no INSERT in src/ — they are the shared canary/portfolio-sync vocabulary),
    //    but a delete that leaves them behind has NOT deleted the tenant: the operator's list would
    //    drop the row while the tenant id stayed live. `users.tenant_id -> tenants(id)` is NO
    //    ACTION, so `users` has to go first or `tenants` refuses with 23503.
    sqlx::query("DELETE FROM users WHERE tenant_id = $1")
        .bind(account_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM tenants WHERE id = $1")
        .bind(account_id)
        .execute(&mut *tx)
        .await?;

    // 3. The tenant itself. Everything else it owned (campaigns, entries, loyalty children,
    //    contacts, leads, credits, keys, avatars, …) is reached by the ON DELETE CASCADE arms
    //    measured on `accounts`, so this one statement retires the whole tree.
    let result = sqlx::query("DELETE FROM accounts WHERE id = $1")
        .bind(account_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    Ok(result.rows_affected())
}

/// David's sister companies (portfolio). Their accounts exist in every app and are kept by rule, so
/// the mass-delete must never be able to remove one — a mistaken click would silently drop a
/// business that is supposed to be permanent.
const PORTFOLIO_MARKERS: [&str; 3] = ["swiftimpact", "zaarhub", "giraudy"];

/// Refuse a retirement that must not happen: the account the operator is signed in as (a lockout,
/// not a cleanup) or a portfolio company. Everything else is allowed — this only ever ADDS a
/// refusal, so the operator's ordinary cleanup path is unchanged.
async fn guard_protected(
    state: &AppState,
    account_id: Uuid,
    caller: &AuthenticatedUser,
) -> Result<(), AppError> {
    if let Ok(own) = Uuid::parse_str(caller.account_id.trim()) {
        if own == account_id {
            return Err(AppError::BadRequest(
                "refusing to delete the account you are signed in as".to_string(),
            ));
        }
    }
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT COALESCE(name, ''), COALESCE(email, '') FROM accounts WHERE id = $1",
    )
    .bind(account_id)
    .fetch_optional(&state.db)
    .await?;
    if let Some((name, email)) = row {
        let hay = format!("{} {}", name, email).to_lowercase();
        if let Some(hit) = PORTFOLIO_MARKERS.iter().find(|m| hay.contains(*m)) {
            return Err(AppError::BadRequest(format!(
                "refusing to delete a portfolio account ({})",
                hit
            )));
        }
    }
    Ok(())
}

/// DELETE /api/v1/admin/tenants/:id
/// Deletes a tenant account and cleans up related data, atomically.
pub async fn delete_tenant(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    let account_id = uuid::Uuid::parse_str(&id)
        .map_err(|_| AppError::BadRequest("Invalid tenant ID".to_string()))?;

    guard_protected(&state, account_id, &user).await?;

    let removed = retire_tenant(&state, account_id).await?;
    if removed == 0 {
        return Err(AppError::NotFound(format!("Tenant not found: {}", id)));
    }

    Ok(Json(json!({
        "status": "deleted",
        "tenant_id": id,
        "deleted": true
    })))
}

/// Input for the bulk tenant delete.
#[derive(Deserialize)]
pub struct BulkDeleteTenantsInput {
    pub ids: Vec<String>,
}

/// POST /api/v1/admin/tenants/bulk-delete
///
/// The console's "Delete selected" control's only caller (kanban t_9f3d85dc). Each id is retired
/// by the SAME `retire_tenant` the single-id route uses, in its own transaction: a mass cleanup
/// must not be all-or-nothing, so one id that has already gone (or that never existed) cannot roll
/// back the fifteen that are real. The reply carries both lists — what went and what did not, with
/// the reason — so the operator sees a partial result instead of a silent 500.
pub async fn bulk_delete_tenants(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<BulkDeleteTenantsInput>,
) -> Result<Json<Value>, AppError> {
    if body.ids.is_empty() {
        return Err(AppError::BadRequest("ids must not be empty".to_string()));
    }

    let mut deleted: Vec<String> = Vec::new();
    let mut failed: Vec<Value> = Vec::new();

    for raw in &body.ids {
        match Uuid::parse_str(raw.trim()) {
            Err(_) => failed.push(json!({ "id": raw, "error": "invalid tenant id" })),
            Ok(account_id) => match guard_protected(&state, account_id, &user).await {
                Err(e) => failed.push(json!({ "id": raw, "error": e.to_string() })),
                Ok(()) => match retire_tenant(&state, account_id).await {
                    Ok(0) => failed.push(json!({ "id": raw, "error": "tenant not found" })),
                    Ok(_) => deleted.push(raw.clone()),
                    Err(e) => failed.push(json!({ "id": raw, "error": e.to_string() })),
                },
            },
        }
    }

    Ok(Json(json!({
        "status": if failed.is_empty() { "deleted" } else { "partial" },
        "deleted": deleted.len(),
        "deleted_ids": deleted,
        "failed": failed
    })))
}

/// GET /api/v1/admin/tenants
/// Lists all accounts with their plan info — for the super admin dashboard.
pub async fn list_all_tenants(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let rows = sqlx::query(
        r#"
        SELECT
            a.id,
            a.name,
            a.email,
            a.created_at,
            COALESCE(pt.name, 'No Plan') as plan_name,
            COALESCE(pt.id::text, '') as plan_id,
            COALESCE(pt.price_monthly::float8, 0.0) as price_monthly,
            u.user_count
        FROM accounts a
        LEFT JOIN plan_tiers pt ON a.plan_tier_id = pt.id
        LEFT JOIN (
            SELECT a2.id as acc_id, COUNT(DISTINCT a3.id)::bigint as user_count
            FROM accounts a2
            LEFT JOIN accounts a3 ON a3.tenant_id = a2.id OR a3.id = a2.id
            WHERE a2.id IS NOT NULL
            GROUP BY a2.id
        ) u ON u.acc_id = a.id
        ORDER BY a.created_at DESC
        "#,
    )
    .fetch_all(&state.db)
    .await?;

    let tenants: Vec<Value> = rows
        .iter()
        .map(|row| {
            let id: uuid::Uuid = row.get("id");
            let name: Option<String> = row.get("name");
            let email: String = row.get("email");
            let plan_name: String = row.get("plan_name");
            let plan_id: String = row.get("plan_id");
            let price_monthly: f64 = row.get("price_monthly");
            let user_count: i64 = row.get("user_count");
            json!({
                "id": id.to_string(),
                "name": name.unwrap_or_default(),
                "email": email,
                "plan_name": plan_name,
                "plan_id": plan_id,
                "price_monthly": price_monthly,
                "user_count": user_count,
            })
        })
        .collect();

    Ok(Json(json!({
        "tenants": tenants
    })))
}

/// Auth guard helper: ensures company_admin can only access their own tenant.
/// super_admin and admin can access any tenant.
async fn check_tenant_access(
    state: &AppState,
    user: &AuthenticatedUser,
    tenant_id: &str,
) -> Result<(), AppError> {
    if user.role != "super_admin" && user.role != "admin" {
        // company_admin — verify they own this tenant
        // Parse user's account_id as UUID first
        let user_uuid = uuid::Uuid::parse_str(&user.account_id)
            .map_err(|_| AppError::BadRequest("Invalid user account ID".to_string()))?;
        let account_tenant_id: Option<uuid::Uuid> =
            sqlx::query_scalar("SELECT tenant_id FROM accounts WHERE id = $1")
                .bind(user_uuid)
                .fetch_optional(&state.db)
                .await?;
        let tid = uuid::Uuid::parse_str(tenant_id)
            .map_err(|_| AppError::BadRequest("Invalid tenant ID".to_string()))?;
        if account_tenant_id.map(|t| t == tid) != Some(true) {
            return Err(AppError::Forbidden(
                "You can only access your own tenant".into(),
            ));
        }
    }
    Ok(())
}

/// GET /api/v1/admin/tenants/:tenant_id/credits-rate
/// Returns the credit rate for a tenant (account).
/// super_admin, admin, and company_admin all allowed (company_admin on their own tenant).
pub async fn get_credit_rate(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(tenant_id): Path<String>,
) -> Result<Json<Value>, AppError> {
    check_tenant_access(&state, &user, &tenant_id).await?;

    let id = uuid::Uuid::parse_str(&tenant_id)
        .map_err(|_| AppError::BadRequest("Invalid tenant ID".to_string()))?;

    let credit_rate: Option<i32> =
        sqlx::query_scalar("SELECT credit_rate FROM accounts WHERE id = $1")
            .bind(id)
            .fetch_optional(&state.db)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("Tenant not found: {}", tenant_id)))?;

    Ok(Json(json!({
        "credit_rate": credit_rate
    })))
}

/// Input for updating credit rate.
#[derive(Deserialize)]
pub struct UpdateCreditRateInput {
    pub credit_rate: i32,
}

/// PATCH /api/v1/admin/tenants/:tenant_id/credits-rate
/// Updates the credit rate for a tenant (account).
/// super_admin, admin, and company_admin all allowed (company_admin on their own tenant).
pub async fn update_credit_rate(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(tenant_id): Path<String>,
    Json(body): Json<UpdateCreditRateInput>,
) -> Result<Json<Value>, AppError> {
    check_tenant_access(&state, &user, &tenant_id).await?;

    let id = uuid::Uuid::parse_str(&tenant_id)
        .map_err(|_| AppError::BadRequest("Invalid tenant ID".to_string()))?;

    if body.credit_rate < 0 {
        return Err(AppError::BadRequest(
            "Credit rate must be non-negative".to_string(),
        ));
    }

    let updated = sqlx::query_scalar::<_, i32>(
        "UPDATE accounts SET credit_rate = $1 WHERE id = $2 RETURNING credit_rate",
    )
    .bind(body.credit_rate)
    .bind(id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound(format!("Tenant not found: {}", tenant_id)))?;

    Ok(Json(json!({
        "credit_rate": updated
    })))
}

/// GET /api/v1/admin/tenants/:tenant_id/purchase-pin
/// Returns the purchase PIN for a tenant (account). Read-only.
pub async fn get_purchase_pin(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(tenant_id): Path<String>,
) -> Result<Json<Value>, AppError> {
    check_tenant_access(&state, &user, &tenant_id).await?;

    let id = uuid::Uuid::parse_str(&tenant_id)
        .map_err(|_| AppError::BadRequest("Invalid tenant ID".to_string()))?;

    let pin: Option<String> = sqlx::query_scalar("SELECT purchase_pin FROM accounts WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Tenant not found: {}", tenant_id)))?;

    Ok(Json(json!({
        "pin": pin
    })))
}

/// GET /api/v1/admin/allcampaigns
/// List ALL campaigns across every account (admin only).
///
/// Nullability (kanban t_518a02dc): `campaigns.account_id`, `campaigns.status` and
/// `campaigns.created_at` are all NULLABLE in the schema, and a NULL in ANY of those
/// three tuple positions used to fail the whole decode (`?` propagates) — so ONE row
/// with a NULL lost the ENTIRE admin list, not one row. Arms, decided per column:
///   * `account_id` — NULLABLE, NO default ⇒ `Option<Uuid>`: the SQL NULL is data
///     (a seeded campaign need not carry an account) and renders as JSON `null`.
///   * `status` — NULLABLE with DEFAULT `'active'` ⇒ `COALESCE(c.status, 'active')`,
///     the DB's own answer for an unset column; the Rust type stays `String` and the
///     JSON of every non-NULL row is byte-identical.
///   * `created_at` — NULLABLE with DEFAULT `now()`; a `now()` DEFAULT is an
///     insertion-time fact, not a value for an unset column (family call, t_d5da34d0)
///     ⇒ `Option<String>`, rendered as JSON `null`; a synthetic timestamp would put a
///     fabricated date in front of an admin.
pub async fn admin_list_all_campaigns(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    if user.role != "admin" {
        return Err(AppError::Forbidden("Admin access required".to_string()));
    }

    let rows = sqlx::query_as::<
        _,
        (
            Uuid,
            Option<Uuid>,
            String,
            String,
            String,
            String,
            String,
            Option<String>,
        ),
    >(
        r#"SELECT c.id, c.account_id, COALESCE(pc.name, a.name, 'Unknown') as owner_name,
               c.name, c.slug, c.type, COALESCE(c.status, 'active') as status, c.created_at::text
         FROM campaigns c
         LEFT JOIN accounts a ON a.id = c.account_id
         LEFT JOIN portfolio_companies pc ON pc.id = c.account_id
         ORDER BY c.created_at DESC
         LIMIT 200"#,
    )
    .fetch_all(&state.db)
    .await?;

    let campaigns: Vec<Value> = rows
        .iter()
        .map(
            |(id, account_id, owner_name, name, slug, ctype, status, created_at)| {
                if created_at.is_none() {
                    tracing::warn!(
                        campaign_id = %id,
                        "admin campaign list: campaigns.created_at is NULL; rendering created_at as null"
                    );
                }
                json!({
                    "id": id.to_string(),
                    "account_id": account_id.map(|a| a.to_string()),
                    "owner_name": owner_name,
                    "name": name,
                    "slug": slug,
                    "type": ctype,
                    "status": status,
                    "created_at": created_at,
                })
            },
        )
        .collect();

    Ok(Json(
        json!({"campaigns": campaigns, "total": campaigns.len()}),
    ))
}

// Note: purchase_pin is auto-generated on account creation. Only read endpoint is exposed.

/// GET /api/v1/admin/email-queue — the OPERATOR's view of the outbound email queue.
///
/// WHY THIS ROUTE EXISTS (kanban t_9d711589). `email_queue::process_due_emails` only ever flushes
/// `status = 'pending'`; a send that failed was recorded in `last_error` alone, so a dead letter was
/// invisible to every human — no log line, no panel, nothing. 18 rows sat `failed` from 2026-09-20
/// with nobody able to see it until a card measured the table by hand. This is the missing surface:
/// counts per status plus the dead letters themselves (`failed` = gave up, `retired` = deliberately
/// withdrawn), with the row's own stored reason.
///
/// READ-ONLY by design. It does not resend, re-queue or retire anything — a resend is a product
/// decision (see `retire-dead-letters.py` in the ticket's audit dir for how the 18 probe rows were
/// retired), and an operator control that silently re-sends customer mail is worse than none.
///
/// Since kanban t_44a990da the ticker retries a failed send a bounded number of times before a row
/// lands here, so `rows` is the set the app has GIVEN UP on; mail still inside a retry window is
/// neither listed nor lost, and is counted by `retrying`.
///
/// Mounted under `/api/v1/admin/*`, so `security::auth::admin_guard` covers it: anonymous callers
/// answer 401 and a non-admin session cannot read another tenant's queue.
pub async fn email_queue(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let counts = sqlx::query("SELECT status, count(*)::bigint AS n FROM pending_emails GROUP BY 1")
        .fetch_all(&state.db)
        .await?;
    let mut by_status = serde_json::Map::new();
    let mut total: i64 = 0;
    for row in counts {
        let status: String = row.get("status");
        let n: i64 = row.get("n");
        total += n;
        by_status.insert(status, json!(n));
    }

    // POLICY (kanban t_44a990da): a row that failed an attempt is RE-ARMED, not written off — it
    // stays `pending` with `attempts > 0` and `send_at` pushed past its backoff. That is neither
    // `sent` nor a dead letter, so it must NOT appear in the list below, but an operator still has
    // to be able to see that mail is sitting in a retry window. This is that number, and
    // `counts.pending` already includes it.
    let retrying: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM pending_emails WHERE status = 'pending' AND attempts > 0",
    )
    .fetch_one(&state.db)
    .await?;

    let dead: Vec<(
        Uuid,
        Uuid,
        String,
        String,
        String,
        i32,
        Option<String>,
        chrono::DateTime<chrono::Utc>,
        chrono::DateTime<chrono::Utc>,
        Option<chrono::DateTime<chrono::Utc>>,
    )> = sqlx::query_as(
        "SELECT id, account_id, to_email, template_type, status, attempts, last_error,
                created_at, send_at, sent_at
           FROM pending_emails
          WHERE status IN ('failed', 'retired')
          ORDER BY created_at DESC
          LIMIT 100",
    )
    .fetch_all(&state.db)
    .await?;

    let rows: Vec<Value> = dead
        .into_iter()
        .map(
            |(
                id,
                account_id,
                to_email,
                template_type,
                status,
                attempts,
                last_error,
                created_at,
                send_at,
                sent_at,
            )| {
                json!({
                    "id": id.to_string(),
                    "account_id": account_id.to_string(),
                    "to_email": to_email,
                    "template_type": template_type,
                    "status": status,
                    "attempts": attempts,
                    "last_error": last_error,
                    "created_at": created_at.to_rfc3339(),
                    "send_at": send_at.to_rfc3339(),
                    "sent_at": sent_at.map(|t| t.to_rfc3339()),
                })
            },
        )
        .collect();

    Ok(Json(json!({
        "counts": Value::Object(by_status),
        "total": total,
        "retrying": retrying,
        "dead_letters": rows.len(),
        "note": "read-only. 'failed' = the ticker gave up after 3 attempts (it never retries a failed row); 'retired' = withdrawn on purpose, the reason is in last_error. A row that failed and is waiting for its next attempt is NOT listed here — it is still 'pending' with attempts > 0, and is counted by `retrying`.",
        "rows": rows,
    })))
}
