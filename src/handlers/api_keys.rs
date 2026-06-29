//! API key handlers — CRUD for api_keys table.

use crate::error::AppError;
use crate::state::AppState;
use crate::security::auth::AuthenticatedUser;
use axum::{
    extract::{Path, State},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::Row;
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

/// Generate a random API key.
/// Format: is_key_<random_alphanumeric>
/// Returns (full_key, prefix, key_hash)
fn generate_api_key() -> (String, String, String) {
    use rand::Rng;
    use sha2::Sha256;

    let rng = rand::thread_rng();
    let random_bytes: Vec<u8> = rng
        .sample_iter(&rand::distributions::Alphanumeric)
        .take(48)
        .collect();
    let random_str: String = String::from_utf8(random_bytes).unwrap();
    let prefix: String = random_str[..8].to_string();

    let full_key = format!("is_key_{}", random_str);

    // Hash with bcrypt for storage
    let hash = bcrypt::hash(&full_key, 6).expect("Failed to hash API key");

    (full_key, prefix, hash)
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
           ORDER BY created_at DESC"#
    )
    .bind(user_id)
    .fetch_all(&state.db)
    .await?;

    Ok(Json(json!({ "api_keys": keys })))
}

/// POST /api/v1/api-keys
pub async fn create_api_key(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<CreateApiKeyInput>,
) -> Result<Json<Value>, AppError> {
    let user_id = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid user ID".to_string()))?;

    // Get the account's tenant_id
    let tenant_id: Uuid = sqlx::query_scalar("SELECT COALESCE(tenant_id, id) FROM accounts WHERE id = $1")
        .bind(user_id)
        .fetch_one(&state.db)
        .await?;

    let id = Uuid::new_v4();
    let (full_key, prefix, key_hash) = generate_api_key();
    let name = body.name.unwrap_or_else(|| "default".to_string());
    let permissions = body.permissions.unwrap_or_else(|| json!([]));

    sqlx::query(
        r#"INSERT INTO api_keys (id, tenant_id, user_id, name, key_hash, prefix, permissions, target_url, expires_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)"#
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
    .execute(&state.db)
    .await?;

    Ok(Json(json!({
        "api_key": {
            "id": id,
            "name": name,
            "prefix": prefix,
            "permissions": permissions,
            "is_active": true,
            "created_at": chrono::Utc::now(),
        },
        "full_key": full_key,
        "warning": "Save this key — it will not be shown again"
    })))
}

/// PUT /api/v1/api-keys/{id}
pub async fn update_api_key(
    State(state): State<AppState>,
    Path(id): Path<String>,
    user: AuthenticatedUser,
    Json(body): Json<UpdateApiKeyInput>,
) -> Result<Json<Value>, AppError> {
    let key_id = Uuid::parse_str(&id)
        .map_err(|_| AppError::BadRequest("Invalid API key ID".to_string()))?;
    let user_id = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid user ID".to_string()))?;

    // Get existing key
    let existing = sqlx::query(
        r#"SELECT name, permissions, is_active, expires_at
           FROM api_keys WHERE id = $1 AND (user_id = $2 OR tenant_id = $2)"#
    )
    .bind(key_id)
    .bind(user_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("API key not found".to_string()))?;

    let name = body.name.unwrap_or_else(|| existing.get("name"));
    let permissions = body.permissions.unwrap_or_else(|| existing.get("permissions"));
    let expires_at: Option<chrono::DateTime<chrono::Utc>> = body.expires_at.or(existing.get("expires_at"));
    let is_active = body.is_active.unwrap_or_else(|| existing.get("is_active"));

    sqlx::query(
        r#"UPDATE api_keys SET
               name = $1, permissions = $2, expires_at = $3, is_active = $4, updated_at = now()
           WHERE id = $5"#
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
           FROM api_keys WHERE id = $1"#
    )
    .bind(key_id)
    .fetch_one(&state.db)
    .await?;

    Ok(Json(json!({ "api_key": key })))
}

/// DELETE /api/v1/api-keys/{id} — soft delete via is_active = false
pub async fn delete_api_key(
    State(state): State<AppState>,
    Path(id): Path<String>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let key_id = Uuid::parse_str(&id)
        .map_err(|_| AppError::BadRequest("Invalid API key ID".to_string()))?;
    let user_id = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid user ID".to_string()))?;

    let result = sqlx::query(
        "UPDATE api_keys SET is_active = false, updated_at = now() WHERE id = $1 AND (user_id = $2 OR tenant_id = $2)"
    )
    .bind(key_id)
    .bind(user_id)
    .execute(&state.db)
    .await?;

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound("API key not found".to_string()));
    }

    Ok(Json(json!({ "status": "deleted", "id": id })))
}
