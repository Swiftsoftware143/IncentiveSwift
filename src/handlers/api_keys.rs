//! API key handlers ??? CRUD for api_keys table.

use crate::error::AppError;
use crate::security::auth::AuthenticatedUser;
use crate::state::AppState;
use axum::{
    extract::{Path, State},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{PgPool, Row};
use uuid::Uuid;

/// An API key record as returned by queries.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct ApiKey {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub name: String,
    pub prefix: String,
    pub permissions: Value,
    pub target_url: Option<String>,
    pub last_used_at: Option<chrono::DateTime<chrono::Utc>>,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    pub is_active: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// Input for creating an API key.
#[derive(Deserialize)]
pub struct CreateApiKeyInput {
    pub name: Option<String>,
    pub permissions: Option<Value>,
    pub target_url: Option<String>,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Input for updating an API key.
#[derive(Deserialize)]
pub struct UpdateApiKeyInput {
    pub name: Option<String>,
    pub permissions: Option<Value>,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    pub is_active: Option<bool>,
}

/// Input for `POST /api/v1/api-keys/verify`.
///
/// The key is carried in the body because that is what the sibling services send
/// (Multi-Directory's "Connect IncentiveSwift" flow posts `{"api_key": "..."}`).
/// An `Authorization: Bearer <key>` header is accepted as well.
#[derive(Deserialize, Default)]
pub struct VerifyApiKeyInput {
    #[serde(default)]
    pub api_key: Option<String>,
}

/// Generate a random API key.
/// Format: is_key_<random_alphanumeric>
/// Returns (full_key, prefix, key_hash)
/// Generate an issued API key as `(full_key, prefix, sha256_hex)`.
///
/// The stored hash is the lowercase SHA-256 hex digest of the WHOLE key — no salt — because the
/// fleet MCP server authenticates by `key_hash = sha256_hex(bearer)` (see `/opt/swift/mcp`, the
/// `incentiveswift` row of its `LOOKUPS`). A bcrypt hash there answered 401 for a live key, so the
/// mint was moved onto the same hash contract every other fleet app's `api_keys.key_hash` already
/// uses.
///
/// `pub(crate)` so every producer of an `api_keys` row uses one shape — the integration-key
/// handler in loyalty_badges.rs had its own literal and produced keys that could never
/// authenticate (kanban t_cf7469bb).
pub(crate) fn generate_api_key() -> Result<(String, String, String), AppError> {
    use rand::Rng;
    use sha2::{Digest, Sha256};

    let rng = rand::thread_rng();
    let random_bytes: Vec<u8> = rng
        .sample_iter(&rand::distributions::Alphanumeric)
        .take(48)
        .collect();
    let random_str: String = String::from_utf8(random_bytes)
        .map_err(|e| AppError::Internal(format!("Invalid UTF-8 in key generation: {}", e)))?;
    let prefix: String = random_str[..8].to_string();

    let full_key = format!("is_key_{}", random_str);

    // Lowercase SHA-256 hex — the fleet MCP server's exact comparison, no salt.
    let hash = hex::encode(Sha256::digest(full_key.as_bytes()));

    Ok((full_key, prefix, hash))
}

/// POST /api/v1/api-keys/verify
///
/// Answers 200 with `{"valid": bool}` for EVERY well-formed request — never a
/// redirect to auth, never a 4xx for a merely-wrong key. Multi-Directory's
/// "Connect IncentiveSwift" flow (`multi-directory/src/handlers/connected_services.rs`)
/// treats a non-2xx response as "verification failed" and only reads `valid` out of a
/// successful body, so `200 {"valid": false}` is the contract for a key we do not accept.
///
/// Validation goes through the SAME resolver that powers `AuthenticatedUser`
/// (`security::auth::validate_api_key`), so "verifies here" and "authenticates for
/// real" cannot drift apart. A key that is unknown, deactivated, expired or has the
/// wrong secret answers `valid: false` — the reason is never disclosed. The presented
/// key is never logged, and `last_used_at` is deliberately left alone: this is a
/// validity check, not a use.
///
/// The key travels in the body (`{"api_key": "..."}`); `Authorization: Bearer <key>`
/// is accepted as an alternative (send `{}` as the body in that case).
pub async fn verify_api_key(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(body): Json<VerifyApiKeyInput>,
) -> Result<Json<Value>, AppError> {
    let presented = body
        .api_key
        .or_else(|| {
            headers
                .get("Authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .map(str::to_string)
        })
        .unwrap_or_default();
    let presented = presented.trim();

    if presented.is_empty() {
        return Ok(Json(json!({ "valid": false })));
    }

    match crate::security::auth::validate_api_key(&state, presented).await {
        Ok(Some(user)) => Ok(Json(json!({
            "valid": true,
            // The IS account the key belongs to, so a caller can bind the key to its
            // own account instead of only asking "is this some valid key?".
            "account_id": user.account_id,
        }))),
        // A recognised-but-dead key (wrong secret, deactivated, expired) is just "not valid".
        Ok(None) | Err(AppError::Unauthorized(_)) => Ok(Json(json!({ "valid": false }))),
        // A real failure (db unreachable) must not masquerade as a bad key.
        Err(e) => Err(e),
    }
}

/// GET /api/v1/api-keys
pub async fn list_api_keys(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let user_id = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid user ID".to_string()))?;

    let keys = sqlx::query_as::<_, ApiKey>(
        r#"SELECT id, tenant_id, user_id, name, prefix, permissions, target_url,
                  last_used_at, expires_at, is_active, created_at, updated_at
           FROM api_keys
           WHERE (tenant_id = $1 OR user_id = $1)
           ORDER BY created_at DESC"#,
    )
    .bind(user_id)
    .fetch_all(&state.db)
    .await?;

    Ok(Json(json!({ "api_keys": keys })))
}

/// GET /api/v1/api-key-events — the credential lifecycle ledger, newest first.
///
/// `record_api_key_event` has written one row for every mint, rotation and revoke since
/// 2026-11-13, but nothing could read them back: the audit the feature records was invisible to the
/// owner it protects. This is its reader. Scoping is the SAME owner resolution the key list uses —
/// rows are selected by the resolved owner account, never by a caller-supplied id — so a teammate
/// sees their workspace's credential history and no other workspace's.
pub async fn list_api_key_events(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let user_id = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid user ID".to_string()))?;
    let owner_id = resolve_owner_account_id(&state.db, user_id).await?;

    let rows = sqlx::query(
        r#"SELECT e.action, e.key_prefix, e.created_at,
                  COALESCE(a.name, a.email) AS actor
             FROM api_key_events e
             LEFT JOIN accounts a ON a.id = e.actor_user_id
            WHERE e.owner_id = $1
            ORDER BY e.created_at DESC
            LIMIT 100"#,
    )
    .bind(owner_id)
    .fetch_all(&state.db)
    .await?;

    let events: Vec<Value> = rows
        .iter()
        .map(|row| {
            json!({
                "action": row.get::<String, _>("action"),
                "key_prefix": row.get::<Option<String>, _>("key_prefix"),
                "actor": row.get::<Option<String>, _>("actor"),
                "created_at": row.get::<chrono::DateTime<chrono::Utc>, _>("created_at"),
            })
        })
        .collect();

    Ok(Json(json!({ "events": events })))
}

/// Resolve the accounts row that owns rows written on behalf of `account_id`.
///
/// `accounts.tenant_id` is a legacy free-form uuid with NO foreign key. Of the 56 accounts live on
/// 2026-09-25, 53 set it to the account's own id, but 3 point at a uuid that is not an accounts row
/// at all (two dangle, one is the single row in `tenants`). `api_keys.tenant_id` does carry
/// `FOREIGN KEY (tenant_id) REFERENCES accounts(id) ON DELETE CASCADE`, so binding the raw
/// `COALESCE(accounts.tenant_id, accounts.id)` made `POST /api/v1/api-keys` return 500 for exactly
/// those 3 accounts
/// (the three oldest tenant accounts) while the other
/// 53 kept working (kanban t_47dcc978).
///
/// The fix keeps that foreign key honest instead of dropping it: use `accounts.tenant_id` only
/// when it really names an accounts row (the pre-existing multi-account tenant shape), otherwise
/// fall back to the account's own id — which is what the other 53 rows already look like. No
/// customer row is rewritten, and a bad reference can no longer 500 a shipped screen.
pub(crate) async fn resolve_owner_account_id(
    db: &PgPool,
    account_id: Uuid,
) -> Result<Uuid, AppError> {
    let resolved: Option<Uuid> = sqlx::query_scalar(
        r#"SELECT COALESCE((SELECT owner.id FROM accounts owner WHERE owner.id = a.tenant_id), a.id)
           FROM accounts a
           WHERE a.id = $1"#,
    )
    .bind(account_id)
    .fetch_optional(db)
    .await?;

    resolved.ok_or_else(|| AppError::NotFound("Account not found".to_string()))
}

/// Append one row to the `api_key_events` lifecycle ledger.
///
/// NEVER carries a secret: `key_prefix` only — `key_hash` and `key_sealed` are deliberately not
/// columns here, so a ledger dump hands over no credential material. The write is FAIL-CLOSED: it
/// returns `Err` on failure (the caller's `?` unwinds the surrounding transaction), so a credential
/// change can never silently succeed with its audit row missing.
pub(crate) async fn record_api_key_event<'e, E>(
    exec: E,
    key_id: Uuid,
    owner_id: Uuid,
    action: &str,
    key_prefix: &str,
    actor_user_id: Option<Uuid>,
    metadata: Option<Value>,
) -> Result<(), AppError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query(
        r#"INSERT INTO api_key_events (key_id, owner_id, action, key_prefix, actor_user_id, metadata)
           VALUES ($1, $2, $3, $4, $5, $6)"#,
    )
    .bind(key_id)
    .bind(owner_id)
    .bind(action)
    .bind(key_prefix)
    .bind(actor_user_id)
    .bind(metadata.unwrap_or_else(|| json!({})))
    .execute(exec)
    .await?;
    Ok(())
}

/// POST /api/v1/api-keys
pub async fn create_api_key(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<CreateApiKeyInput>,
) -> Result<Json<Value>, AppError> {
    let user_id = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid user ID".to_string()))?;

    // The account that owns the key. `api_keys.tenant_id` carries a real foreign key to
    // `accounts(id)`, so the bound value must name an accounts row —
    // see `resolve_owner_account_id` for why the raw `COALESCE(accounts.tenant_id, accounts.id)`
    // is not enough.
    let tenant_id = resolve_owner_account_id(&state.db, user_id).await?;

    // ONE active key per account, enforced by the partial unique index on
    // (tenant_id) WHERE is_active (see the migration). Refuse a second key with a clear
    // instruction to Regenerate, so the caller is told what to do instead of meeting a bare 500
    // from the index.
    let already_has_active: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM api_keys WHERE tenant_id = $1 AND is_active = true)",
    )
    .bind(tenant_id)
    .fetch_one(&state.db)
    .await?;
    if already_has_active {
        return Err(AppError::Conflict(
            "You already have an active API key. Use Regenerate to rotate it instead of creating a \
             second one."
                .to_string(),
        ));
    }

    let id = Uuid::new_v4();
    let (full_key, prefix, key_hash) = generate_api_key()?;
    // Store a sealed copy at rest (matches `mint_on`, kanban t_7863696b) so the key stays
    // viewable — without it the reveal handler has nothing to open. Fail closed on a missing
    // master key rather than writing a key nobody can ever read back.
    let sealed = crate::security::provider_key_crypto::seal_in(&state.db, &full_key)
        .await
        .map_err(|e| AppError::Internal(format!("could not seal the API key: {e}")))?;
    let name = body.name.unwrap_or_else(|| "default".to_string());
    // `api_keys.name` is `varchar(255)` and the label comes straight from the panel, so a long one
    // reached the INSERT and answered a bare 500 (measured LIVE 2026-10-17).
    crate::security::display_name::check_len(
        &name,
        crate::security::display_name::COLUMN_CHARS,
        "name",
    )
    .map_err(AppError::UnprocessableEntity)?;
    let permissions = body.permissions.unwrap_or_else(|| json!([]));

    // The mint and its ledger row share ONE transaction so they commit or roll back together.
    let mut tx = state.db.begin().await?;
    sqlx::query(
        r#"INSERT INTO api_keys (id, tenant_id, user_id, name, key_hash, prefix, permissions, target_url, expires_at, key_sealed)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)"#
    )
    .bind(id)
    .bind(tenant_id)
    .bind(user_id)
    .bind(&name)
    .bind(&key_hash)
    .bind(&prefix)
    .bind(&permissions)
    .bind(&body.target_url)
    .bind(body.expires_at)
    .bind(&sealed)
    .execute(&mut *tx)
    .await?;
    record_api_key_event(
        &mut *tx,
        id,
        tenant_id,
        "created",
        &prefix,
        Some(user_id),
        None,
    )
    .await?;
    tx.commit().await?;

    Ok(Json(json!({
        "api_key": {
            "id": id,
            // The accounts row the key is owned by, so the binding is measurable from outside
            // without reading the table by hand (kanban t_47dcc978).
            "tenant_id": tenant_id,
            "name": name,
            "prefix": prefix,
            "permissions": permissions,
            "is_active": true,
            "created_at": chrono::Utc::now(),
        },
        "full_key": full_key,
        "warning": "Save this key ??? it will not be shown again"
    })))
}

/// PUT /api/v1/api-keys/{id}
pub async fn update_api_key(
    State(state): State<AppState>,
    Path(id): Path<String>,
    user: AuthenticatedUser,
    Json(body): Json<UpdateApiKeyInput>,
) -> Result<Json<Value>, AppError> {
    let key_id =
        Uuid::parse_str(&id).map_err(|_| AppError::BadRequest("Invalid API key ID".to_string()))?;
    let user_id = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid user ID".to_string()))?;

    // Get existing key
    let existing = sqlx::query(
        r#"SELECT name, permissions, is_active, expires_at
           FROM api_keys WHERE id = $1 AND (user_id = $2 OR tenant_id = $2)"#,
    )
    .bind(key_id)
    .bind(user_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("API key not found".to_string()))?;

    let name = body.name.unwrap_or_else(|| existing.get("name"));
    // Same column, on the rename path (measured LIVE 2026-10-17: create answered 500, so the
    // rename shared the defect).
    crate::security::display_name::check_len(
        &name,
        crate::security::display_name::COLUMN_CHARS,
        "name",
    )
    .map_err(AppError::UnprocessableEntity)?;
    let permissions = body
        .permissions
        .unwrap_or_else(|| existing.get("permissions"));
    let expires_at: Option<chrono::DateTime<chrono::Utc>> =
        body.expires_at.or(existing.get("expires_at"));
    let is_active = body.is_active.unwrap_or_else(|| existing.get("is_active"));

    sqlx::query(
        r#"UPDATE api_keys SET
               name = $1, permissions = $2, expires_at = $3, is_active = $4, updated_at = now()
           WHERE id = $5"#,
    )
    .bind(&name)
    .bind(&permissions)
    .bind(expires_at)
    .bind(is_active)
    .bind(key_id)
    .execute(&state.db)
    .await?;

    let key = sqlx::query_as::<_, ApiKey>(
        r#"SELECT id, tenant_id, user_id, name, prefix, permissions, target_url,
                  last_used_at, expires_at, is_active, created_at, updated_at
           FROM api_keys WHERE id = $1"#,
    )
    .bind(key_id)
    .fetch_one(&state.db)
    .await?;

    Ok(Json(json!({ "api_key": key })))
}

/// DELETE /api/v1/api-keys/{id} ??? soft delete via is_active = false
pub async fn delete_api_key(
    State(state): State<AppState>,
    Path(id): Path<String>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let key_id =
        Uuid::parse_str(&id).map_err(|_| AppError::BadRequest("Invalid API key ID".to_string()))?;
    let user_id = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid user ID".to_string()))?;

    // Read the owner + prefix BEFORE the write so the ledger row can name them (prefix only —
    // never a secret). The lock pins the row against a concurrent rotate/regenerate.
    let mut tx = state.db.begin().await?;
    let existing = sqlx::query(
        "SELECT tenant_id, prefix, is_active FROM api_keys \
         WHERE id = $1 AND (user_id = $2 OR tenant_id = $2) FOR UPDATE",
    )
    .bind(key_id)
    .bind(user_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| AppError::NotFound("API key not found".to_string()))?;
    let tenant_id: Uuid = existing.get("tenant_id");
    let prefix: String = existing.get("prefix");
    let was_active: bool = existing.get("is_active");

    let result =
        sqlx::query("UPDATE api_keys SET is_active = false, updated_at = now() WHERE id = $1")
            .bind(key_id)
            .execute(&mut *tx)
            .await?;

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound("API key not found".to_string()));
    }
    // A revoke that actually took a key out of service is an event; a repeat revoke is a no-op and
    // is not re-recorded.
    if was_active {
        record_api_key_event(
            &mut *tx,
            key_id,
            tenant_id,
            "revoked",
            &prefix,
            Some(user_id),
            None,
        )
        .await?;
    }
    tx.commit().await?;

    Ok(Json(json!({ "status": "deleted", "id": id })))
}

// ── Auto-provisioned, retrievable keys (kanban t_7863696b) ─────────────────────────────────────
//
// David, 2026-11-11 (withdrawing "shown once, never again"): every account is created with a key,
// and the key stays viewable. `key_hash` (sha256 hex for new mints, bcrypt for legacy rows — see
// `security::auth::verify_issued_api_key`) is what `validate_api_key` reads; `key_sealed` (fleet
// provider-key envelope) exists ONLY so the owner can see and copy the key.

/// A key that was just minted. `full_key` is the plaintext: sealed before the row is written,
/// handed to the caller's UI, never logged.
pub struct MintedKey {
    pub id: Uuid,
    pub name: String,
    pub full_key: String,
    pub prefix: String,
}

/// Mint one key and INSERT it on the caller's connection. The ONE writer for every mint in this
/// app: the signup transaction, `create_api_key`, regenerate and the one-time backfill.
pub async fn mint_on(
    conn: &mut sqlx::PgConnection,
    tenant_id: Uuid,
    user_id: Uuid,
    name: &str,
) -> Result<MintedKey, AppError> {
    let (full_key, prefix, key_hash) = generate_api_key()?;
    // Fail closed: without PROVIDER_KEY_ENC_SECRET this errors rather than storing a key nobody can
    // ever read back.
    let sealed = crate::security::provider_key_crypto::seal_in(&mut *conn, &full_key)
        .await
        .map_err(|e| AppError::Internal(format!("could not seal the API key: {e}")))?;
    let id = Uuid::new_v4();

    sqlx::query(
        r#"INSERT INTO api_keys (id, tenant_id, user_id, name, key_hash, prefix, permissions, key_sealed)
           VALUES ($1, $2, $3, $4, $5, $6, '[]'::jsonb, $7)"#,
    )
    .bind(id)
    .bind(tenant_id)
    .bind(user_id)
    .bind(name)
    .bind(&key_hash)
    .bind(&prefix)
    .bind(&sealed)
    .execute(&mut *conn)
    .await?;

    // The mint and its ledger row share the caller's transaction: either both land or neither does.
    record_api_key_event(
        &mut *conn,
        id,
        tenant_id,
        "created",
        &prefix,
        Some(user_id),
        None,
    )
    .await?;

    Ok(MintedKey {
        id,
        name: name.to_string(),
        full_key,
        prefix,
    })
}

#[derive(Debug, Deserialize)]
pub struct RevealApiKeyInput {
    /// The caller's CURRENT password — a stolen session token must not be enough to read a live
    /// credential.
    pub password: String,
}

/// POST /api/v1/api-keys/:id/reveal — show the key again (repeatable).
pub async fn reveal_api_key(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(id): Path<Uuid>,
    Json(body): Json<RevealApiKeyInput>,
) -> Result<Json<Value>, AppError> {
    let user_id = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid user ID".to_string()))?;

    let row = sqlx::query(
        "SELECT prefix, key_sealed FROM api_keys \
         WHERE id = $1 AND user_id = $2 AND is_active = true",
    )
    .bind(id)
    .bind(user_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("API key not found".to_string()))?;

    // IncentiveSwift's column is `prefix` (unlike the sibling apps' `key_prefix`) — verified with
    // \d api_keys before this query was written.
    let prefix: String = row.get("prefix");
    let sealed: Option<String> = row.get("key_sealed");

    // ── re-authenticate ───────────────────────────────────────────────────────────────────────
    let stored: Option<String> =
        sqlx::query_scalar("SELECT password_hash FROM accounts WHERE id = $1")
            .bind(user_id)
            .fetch_optional(&state.db)
            .await?;
    let stored = stored.ok_or_else(|| AppError::Unauthorized("Account not found".to_string()))?;
    if !crate::handlers::auth_handler::verify_password(&body.password, &stored) {
        return Err(AppError::Unauthorized(
            "Current password is incorrect".to_string(),
        ));
    }

    let sealed = sealed.ok_or_else(|| {
        AppError::Conflict(
            "This key was created before keys were stored recoverably, so it cannot be displayed. \
             Regenerate it to get a key you can view and copy."
                .to_string(),
        )
    })?;
    let full_key = crate::security::provider_key_crypto::open_in(&state.db, &sealed)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, key = %id, "API key reveal: sealed copy could not be opened");
            AppError::Internal(
                "This key could not be opened by the server's encryption key. Regenerate it."
                    .to_string(),
            )
        })?;
    if full_key.is_empty() {
        return Err(AppError::Internal(
            "This key could not be opened by the server's encryption key. Regenerate it."
                .to_string(),
        ));
    }

    if let Err(e) = sqlx::query(
        "INSERT INTO api_key_reveals (id, key_id, tenant_id, user_id, key_prefix) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(Uuid::new_v4())
    .bind(id)
    .bind(resolve_owner_account_id(&state.db, user_id).await?)
    .bind(user_id)
    .bind(&prefix)
    .execute(&state.db)
    .await
    {
        tracing::error!(error = %e, key = %id, "API key reveal: audit row could not be written");
    }
    tracing::info!(key = %id, key_prefix = %prefix, user = %user_id, "API key REVEALED");

    Ok(Json(
        json!({ "id": id, "prefix": prefix, "api_key": full_key }),
    ))
}

/// POST /api/v1/api-keys/:id/regenerate — one atomic revoke-and-issue.
pub async fn regenerate_api_key(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, AppError> {
    let user_id = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid user ID".to_string()))?;

    let old = sqlx::query(
        "SELECT name, prefix FROM api_keys WHERE id = $1 AND user_id = $2 AND is_active = true",
    )
    .bind(id)
    .bind(user_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("API key not found".to_string()))?;
    let name: String = old.get("name");
    let old_prefix: String = old.get("prefix");
    let tenant_id = resolve_owner_account_id(&state.db, user_id).await?;

    // Retire the OLD key FIRST, then mint the replacement, in ONE transaction. The partial unique
    // index on (tenant_id) WHERE is_active refuses two active keys per account, so the old
    // insert-then-delete order would violate it; retire-then-mint keeps exactly one active key at
    // every instant. If either step fails, both roll back and the old key keeps working.
    let mut tx = state.db.begin().await?;
    sqlx::query(
        "UPDATE api_keys SET is_active = false, updated_at = now() WHERE id = $1 AND user_id = $2",
    )
    .bind(id)
    .bind(user_id)
    .execute(&mut *tx)
    .await?;
    // The retire is the 'regenerated' event (the old key stopped authenticating); `mint_on` below
    // writes the new key's own 'created' row on the same transaction.
    record_api_key_event(
        &mut *tx,
        id,
        tenant_id,
        "regenerated",
        &old_prefix,
        Some(user_id),
        None,
    )
    .await?;
    let minted = mint_on(&mut tx, tenant_id, user_id, &name).await?;
    tx.commit().await?;

    tracing::info!(
        new_key = %minted.id,
        old_key = %id,
        key_prefix = %minted.prefix,
        "API key REGENERATED — the previous key no longer authenticates"
    );

    Ok(Json(json!({
        "id": minted.id,
        "rotated_from": id,
        "name": minted.name,
        "prefix": minted.prefix,
        "api_key": minted.full_key,
        "note": "The previous key no longer works. This one can be viewed and copied again at any time."
    })))
}

/// One-time backfill: every account that exists today gets a key it can view.
/// Gated on the `fleet_data_backfills` marker so it runs ONCE — an account whose keys were
/// deliberately revoked is never re-minted behind its back.
pub async fn backfill_missing_keys(db: &PgPool) -> Result<usize, AppError> {
    const MARKER: &str = "api_keys_auto_v1";
    let done: Option<String> =
        sqlx::query_scalar("SELECT name FROM fleet_data_backfills WHERE name = $1")
            .bind(MARKER)
            .fetch_optional(db)
            .await?;
    if done.is_some() {
        return Ok(0);
    }

    let mut tx = db.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(7263114004)")
        .execute(&mut *tx)
        .await?;

    let accounts: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT a.id, a.id FROM accounts a
         WHERE NOT EXISTS (
            SELECT 1 FROM api_keys k WHERE k.user_id = a.id AND k.is_active = true
         )
         ORDER BY a.created_at",
    )
    .fetch_all(&mut *tx)
    .await?;

    let mut minted = 0usize;
    for (account_id, tenant_id) in accounts {
        mint_on(&mut tx, tenant_id, account_id, "auto-provisioned").await?;
        minted += 1;
    }

    let unrecoverable: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM api_keys WHERE is_active = true AND key_sealed IS NULL",
    )
    .fetch_one(&mut *tx)
    .await?;

    sqlx::query(
        "INSERT INTO fleet_data_backfills (name, detail) VALUES ($1, $2)
         ON CONFLICT (name) DO NOTHING",
    )
    .bind(MARKER)
    .bind(format!(
        "minted {minted} key(s) for accounts with none; {unrecoverable} pre-existing key(s) have no \
         sealed copy (plaintext unrecoverable — regenerate to hand off)"
    ))
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(minted)
}
