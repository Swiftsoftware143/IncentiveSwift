use axum::{
    extract::{Path, State},
    Extension, Json,
};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

use crate::access::claims::Claims;
use crate::error::{ApiResult, AppError};
use crate::state::AppState;

#[derive(Debug, Serialize, Deserialize, FromRow)]
pub struct ApiKeyRow {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub label: String,
    pub provider: String,
    pub api_key_encrypted: String,
}

#[derive(Debug, Deserialize)]
pub struct AddApiKeyRequest {
    pub label: String,
    pub api_key: String,
    #[serde(default = "default_provider")]
    pub provider: String,
    /// Re-enter an EXISTING key IN PLACE: the row keeps its id, so every domain that picked it by
    /// `api_key_id` starts working again instead of being left pointing at a dead credential
    /// (t_72f0bc83). Omitted ⇒ this request behaves exactly as it always did (insert a new row).
    #[serde(default)]
    pub replace_key_id: Option<Uuid>,
}

fn default_provider() -> String {
    "mailgun".into()
}

/// How a stored credential reads for THIS deployment — decided by OPENING the value, never by its
/// shape (t_72f0bc83). A row that exists but cannot be opened used to be indistinguishable from a
/// healthy one: it listed as a configured key while every send failed with "Stored API key for
/// domain … cannot be read", and the only signal in the whole system was a boot WARN that never
/// reaches the response path. `secret_box::open` never fails, so an empty result for a non-empty
/// ciphertext means exactly "this deployment cannot read it".
pub(crate) fn stored_key_status(tenant_id: Uuid, stored: &str) -> &'static str {
    if stored.is_empty() {
        "empty"
    } else if crate::secret_box::open(tenant_id, stored).trim().is_empty() {
        "unreadable"
    } else {
        "usable"
    }
}

/// The operator-facing sentence that goes with `stored_key_status == "unreadable"`.
pub(crate) const REENTER_HINT: &str = "This key cannot be opened by the server (the encryption \
     secret changed, or the row was written by another deployment). Sends using it fail — re-enter \
     the key.";

pub async fn list_api_keys(
    State(state): State<AppState>,
    Extension(claims): Extension<Claims>,
) -> ApiResult<Json<serde_json::Value>> {
    let account_id = Uuid::parse_str(&claims.aid)
        .map_err(|_| AppError::Unauthorized("Invalid session".into()))?;
    let rows = sqlx::query_as::<_, ApiKeyRow>(
        "SELECT id, tenant_id, label, provider, api_key_encrypted FROM private_email_api_keys WHERE tenant_id = $1 ORDER BY created_at DESC"
    )
    .bind(account_id)
    .fetch_all(&state.db)
    .await
    .map_err(crate::error::db_error)?;

    // Return without the encrypted key — just metadata, plus the ONE thing metadata alone could not
    // answer: whether this deployment can actually open the stored ciphertext (t_72f0bc83).
    let safe: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            let status = stored_key_status(account_id, &r.api_key_encrypted);
            let mut v = serde_json::json!({
                "id": r.id,
                "label": r.label,
                "provider": r.provider,
                "key_status": status,
            });
            if status == "unreadable" {
                v["key_hint"] = serde_json::json!(REENTER_HINT);
                v["replaceable"] = serde_json::json!(true);
            }
            v
        })
        .collect();

    Ok(Json(serde_json::json!(safe)))
}

pub async fn add_api_key(
    State(state): State<AppState>,
    Extension(claims): Extension<Claims>,
    Json(req): Json<AddApiKeyRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let account_id = Uuid::parse_str(&claims.aid)
        .map_err(|_| AppError::Unauthorized("Invalid session".into()))?;

    // One envelope for the whole app (CS-21/t_45772522): `secret_box::seal` writes the `enc:v1:`
    // form and FAILS CLOSED when the master key is missing. This column used to be written with the
    // bare AES-GCM body (no prefix), which no reader other than this module could recognise and
    // which the boot audit could only judge by trying to decrypt it.
    let encrypted = crate::secret_box::seal(account_id, &req.api_key)?;

    // Re-entry for a key this deployment cannot open (t_72f0bc83): UPDATE the row in place, keeping
    // its id, so `private_email_domains.api_key_id` keeps pointing at a working credential. Without
    // this the only way out of an unreadable key is delete + re-create, and the delete sets every
    // referencing domain's `api_key_id` to NULL (ON DELETE SET NULL) — i.e. it silently breaks the
    // domains instead of repairing them.
    if let Some(kid) = req.replace_key_id {
        let row = sqlx::query_as::<_, ApiKeyRow>(
            r#"
            UPDATE private_email_api_keys
               SET label = $3, provider = $4, api_key_encrypted = $5, updated_at = NOW()
             WHERE id = $1 AND tenant_id = $2
            RETURNING id, tenant_id, label, provider, api_key_encrypted
            "#,
        )
        .bind(kid)
        .bind(account_id)
        .bind(&req.label)
        .bind(&req.provider)
        .bind(&encrypted)
        .fetch_optional(&state.db)
        .await
        .map_err(crate::error::db_error)?
        .ok_or_else(|| AppError::NotFound("API key not found".into()))?;

        return Ok(Json(serde_json::json!({
            "id": row.id,
            "label": row.label,
            "provider": row.provider,
            "created": false,
            "replaced": true,
            "key_status": stored_key_status(account_id, &row.api_key_encrypted),
        })));
    }

    let row = sqlx::query_as::<_, ApiKeyRow>(
        r#"
        INSERT INTO private_email_api_keys (tenant_id, label, provider, api_key_encrypted)
        VALUES ($1, $2, $3, $4)
        RETURNING id, tenant_id, label, provider, api_key_encrypted
        "#,
    )
    .bind(account_id)
    .bind(&req.label)
    .bind(&req.provider)
    .bind(&encrypted)
    .fetch_one(&state.db)
    .await
    .map_err(crate::error::db_error)?;

    Ok(Json(serde_json::json!({
        "id": row.id,
        "label": row.label,
        "provider": row.provider,
        "created": true,
        "key_status": stored_key_status(account_id, &row.api_key_encrypted),
    })))
}

pub async fn delete_api_key(
    State(state): State<AppState>,
    Extension(claims): Extension<Claims>,
    Path(key_id): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    let account_id = Uuid::parse_str(&claims.aid)
        .map_err(|_| AppError::Unauthorized("Invalid session".into()))?;

    let result = sqlx::query("DELETE FROM private_email_api_keys WHERE id = $1 AND tenant_id = $2")
        .bind(key_id)
        .bind(account_id)
        .execute(&state.db)
        .await
        .map_err(crate::error::db_error)?;

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound("API key not found".into()));
    }

    Ok(Json(serde_json::json!({"deleted": true})))
}
