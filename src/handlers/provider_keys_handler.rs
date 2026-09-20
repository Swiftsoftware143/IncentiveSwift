//! Provider Keys handler - manage per-account third-party API keys.
//!
//! Endpoints:
//!   GET  /api/v1/provider-keys          - list keys for current account
//!   POST /api/v1/provider-keys          - upsert a key
//!   DELETE /api/v1/provider-keys/:provider - delete a key by provider name
//!   GET  /api/v1/available-providers    - list all available provider types

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

/// Mask a key showing only first 3 and last 3 characters.
fn mask_key(key: &str) -> String {
    if key.len() <= 6 {
        return "***".to_string();
    }
    let first = &key[..3];
    let last = &key[key.len() - 3..];
    format!("{}...{}", first, last)
}

/// GET /api/v1/provider-keys
pub async fn list_provider_keys(
    State(state): State<AppState>,
    auth: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&auth.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;

    let rows = sqlx::query(
        r#"
        SELECT pk.id, pk.account_id, pk.provider, pk.api_key, pk.base_url,
               pk.metadata, pk.is_active, pk.scope, pk.created_at, pk.updated_at,
               ap.name AS provider_name, ap.description AS provider_description
        FROM provider_keys pk
        LEFT JOIN available_providers ap ON ap.key = pk.provider
        WHERE pk.account_id = $1
        ORDER BY pk.provider
        "#,
    )
    .bind(account_id)
    .fetch_all(&state.db)
    .await?;

    let items: Vec<Value> = rows
        .iter()
        .map(|row| {
            let raw_key: String = row.get("api_key");
            json!({
                "id": row.get::<Uuid, _>("id"),
                "account_id": row.get::<Uuid, _>("account_id"),
                "provider": row.get::<String, _>("provider"),
                "api_key_masked": mask_key(&raw_key),
                "base_url": row.get::<Option<String>, _>("base_url"),
                "metadata": row.get::<Option<serde_json::Value>, _>("metadata"),
                "is_active": row.get::<bool, _>("is_active"),
                "scope": row.get::<String, _>("scope"),
                "provider_name": row.get::<Option<String>, _>("provider_name"),
                "provider_description": row.get::<Option<String>, _>("provider_description"),
                "created_at": row.get::<chrono::DateTime<chrono::Utc>, _>("created_at"),
                "updated_at": row.get::<chrono::DateTime<chrono::Utc>, _>("updated_at"),
            })
        })
        .collect();

    Ok(Json(json!({ "items": items, "count": items.len() })))
}

/// POST /api/v1/provider-keys
pub async fn upsert_provider_key(
    State(state): State<AppState>,
    auth: AuthenticatedUser,
    Json(body): Json<UpsertInput>,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&auth.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;

    // Validate provider exists
    let provider_exists = sqlx::query("SELECT 1 FROM available_providers WHERE key = $1")
        .bind(&body.provider)
        .fetch_optional(&state.db)
        .await?;

    if provider_exists.is_none() {
        return Err(AppError::BadRequest(format!(
            "Unknown provider: '{}'. Must be one of the available providers.",
            body.provider
        )));
    }

    // Upsert using EXCLUDED pattern
    let row = sqlx::query(
        r#"
        INSERT INTO provider_keys (account_id, provider, api_key, base_url, metadata, is_active, scope)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        ON CONFLICT (account_id, provider)
        DO UPDATE SET
            api_key = CASE
                WHEN EXCLUDED.api_key IS NOT NULL AND EXCLUDED.api_key != ''
                THEN EXCLUDED.api_key
                ELSE provider_keys.api_key
            END,
            base_url = COALESCE(EXCLUDED.base_url, provider_keys.base_url),
            metadata = CASE
                WHEN EXCLUDED.metadata IS NOT NULL AND EXCLUDED.metadata != '{}'::jsonb
                THEN EXCLUDED.metadata
                ELSE provider_keys.metadata
            END,
            is_active = EXCLUDED.is_active,
            scope = EXCLUDED.scope,
            updated_at = now()
        RETURNING id, account_id, provider, api_key, base_url, metadata, is_active, scope, created_at, updated_at
        "#
    )
    .bind(account_id)
    .bind(&body.provider)
    .bind(body.api_key.as_deref().unwrap_or(""))
    .bind(&body.base_url)
    .bind(&body.metadata)
    .bind(body.is_active.unwrap_or(true))
    .bind(body.scope.as_deref().unwrap_or("account"))
    .fetch_one(&state.db)
    .await?;

    let raw_key: String = row.get("api_key");
    let item = json!({
        "id": row.get::<Uuid, _>("id"),
        "account_id": row.get::<Uuid, _>("account_id"),
        "provider": row.get::<String, _>("provider"),
        "api_key_masked": mask_key(&raw_key),
        "base_url": row.get::<Option<String>, _>("base_url"),
        "metadata": row.get::<Option<serde_json::Value>, _>("metadata"),
        "is_active": row.get::<bool, _>("is_active"),
        "scope": row.get::<String, _>("scope"),
        "created_at": row.get::<chrono::DateTime<chrono::Utc>, _>("created_at"),
        "updated_at": row.get::<chrono::DateTime<chrono::Utc>, _>("updated_at"),
    });

    Ok(Json(json!({ "item": item })))
}

/// DELETE /api/v1/provider-keys/:provider
pub async fn delete_provider_key(
    State(state): State<AppState>,
    auth: AuthenticatedUser,
    Path(provider): Path<String>,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&auth.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;

    let result = sqlx::query("DELETE FROM provider_keys WHERE account_id = $1 AND provider = $2")
        .bind(account_id)
        .bind(&provider)
        .execute(&state.db)
        .await?;

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound(format!(
            "Provider key not found for provider '{}'",
            provider
        )));
    }

    Ok(Json(json!({ "status": "deleted", "provider": provider })))
}

/// GET /api/v1/available-providers
pub async fn list_available_providers(
    State(state): State<AppState>,
) -> Result<Json<Value>, AppError> {
    let rows = sqlx::query(
        "SELECT key, name, description, requires_base_url, requires_metadata, icon FROM available_providers ORDER BY name"
    )
    .fetch_all(&state.db)
    .await?;

    let items: Vec<Value> = rows
        .iter()
        .map(|row| {
            json!({
                "key": row.get::<String, _>("key"),
                "name": row.get::<String, _>("name"),
                "description": row.get::<Option<String>, _>("description"),
                "requires_base_url": row.get::<bool, _>("requires_base_url"),
                "requires_metadata": row.get::<serde_json::Value, _>("requires_metadata"),
                "icon": row.get::<Option<String>, _>("icon"),
            })
        })
        .collect();

    Ok(Json(json!({ "items": items, "count": items.len() })))
}

// ---- Input types ----

#[derive(Deserialize)]
pub struct UpsertInput {
    pub provider: String,
    pub api_key: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
    #[serde(default)]
    pub is_active: Option<bool>,
    #[serde(default)]
    pub scope: Option<String>,
}

// ---- CoreSwift integration proxy (server-side, key stays server-side) ----

pub async fn get_coreswift_conn(
    state: &AppState,
    account_id: &Uuid,
) -> Result<(String, String), AppError> {
    let row = sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT api_key, base_url FROM provider_keys
         WHERE account_id = $1 AND provider = 'coreswift' AND is_active = true",
    )
    .bind(account_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| AppError::Database(format!("DB error: {e}")))?
    .ok_or_else(|| AppError::NotFound("CoreSwift is not connected".to_string()))?;

    let api_key = row.0;
    let base_url = row
        .1
        .filter(|u| !u.is_empty())
        .or_else(|| {
            let d = state.config.coreswift_url.trim().to_string();
            if d.is_empty() {
                None
            } else {
                Some(d)
            }
        })
        .map(|u| u.trim_end_matches('/').to_string())
        .ok_or_else(|| AppError::NotFound("CoreSwift base URL not configured".to_string()))?;

    Ok((api_key, base_url))
}

/// GET /api/v1/integrations/coreswift/lists
pub async fn coreswift_lists(
    State(state): State<AppState>,
    auth: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&auth.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;

    let (api_key, base_url) = get_coreswift_conn(&state, &account_id).await?;

    let url = format!("{base_url}/api/external/lists");
    let resp = state
        .http_client
        .get(&url)
        .bearer_auth(&api_key)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("CoreSwift unreachable: {e}")))?;

    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or_else(|_| json!({ "lists": [] }));

    if !status.is_success() {
        return Err(AppError::Internal(format!(
            "CoreSwift returned {status}: {}",
            serde_json::to_string(&body).unwrap_or_default()
        )));
    }

    Ok(Json(body))
}

/// GET /api/v1/integrations/coreswift/status
pub async fn coreswift_status(
    State(state): State<AppState>,
    auth: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&auth.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;

    match crate::delivery::coreswift_external::get_coreswift_connection(&state, &account_id).await {
        Some((_, base_url)) => Ok(Json(json!({ "connected": true, "base_url": base_url }))),
        None => Ok(Json(json!({ "connected": false, "base_url": null }))),
    }
}

// ---- Manual push (the documented manual fallback for the inbound path) ----

#[derive(Deserialize)]
pub struct CoreswiftPushInput {
    /// Contact to push. Omitted -> the account's most recently captured contact.
    pub contact_id: Option<Uuid>,
    /// Optional CoreSwift list id (picker value from /integrations/coreswift/lists).
    pub list_id: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// POST /api/v1/integrations/coreswift/push
///
/// Same code path as the automatic capture push (`push_lead_to_coreswift`).
/// Unlike the capture path this one is user-triggered, so failures are surfaced.
pub async fn coreswift_push(
    State(state): State<AppState>,
    auth: AuthenticatedUser,
    Json(body): Json<CoreswiftPushInput>,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&auth.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;

    // Contacts are shared across the install, so ownership is proven through the
    // entries -> campaigns -> account chain rather than a contacts.account_id.
    let contact_id = match body.contact_id {
        Some(cid) => {
            let owned: Option<Uuid> = sqlx::query_scalar(
                r#"SELECT c.account_id
                   FROM entries e
                   JOIN campaigns c ON c.id = e.campaign_id
                   WHERE e.contact_id = $1 AND c.account_id = $2
                   ORDER BY e.created_at DESC
                   LIMIT 1"#,
            )
            .bind(cid)
            .bind(account_id)
            .fetch_optional(&state.db)
            .await?;

            if owned.is_none() {
                return Err(AppError::NotFound(
                    "Contact not found for this account".to_string(),
                ));
            }
            cid
        }
        None => sqlx::query_scalar(
            r#"SELECT e.contact_id
               FROM entries e
               JOIN campaigns c ON c.id = e.campaign_id
               WHERE c.account_id = $1 AND e.contact_id IS NOT NULL
               ORDER BY e.created_at DESC
               LIMIT 1"#,
        )
        .bind(account_id)
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("No captured contacts yet".to_string()))?,
    };

    if crate::delivery::coreswift_external::get_coreswift_connection(&state, &account_id)
        .await
        .is_none()
    {
        return Err(AppError::BadRequest(
            "CoreSwift is not connected — store your CoreSwift key first".to_string(),
        ));
    }

    let pushed = crate::delivery::coreswift_external::push_lead_to_coreswift(
        &state,
        &account_id,
        &contact_id,
        &body.tags,
        body.list_id.clone(),
        json!({}),
        "manual_push",
    )
    .await;

    if !pushed {
        return Err(AppError::Internal(
            "CoreSwift rejected the push — check the server log for the hub response".to_string(),
        ));
    }

    Ok(Json(json!({
        "pushed": true,
        "contact_id": contact_id,
        "list_id": body.list_id,
    })))
}

/// POST /api/v1/provider-keys/:provider/test
///
/// Live probe where the provider supports one (CoreSwift -> hub /api/external/lists).
/// For the rest: honest "credential stored, no live probe" — never a fake green.
pub async fn test_provider_key(
    State(state): State<AppState>,
    auth: AuthenticatedUser,
    Path(provider): Path<String>,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&auth.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;

    if provider == "coreswift" {
        return match get_coreswift_conn(&state, &account_id).await {
            Err(_) => Ok(Json(json!({
                "provider": provider,
                "ok": false,
                "live": false,
                "message": "Not connected — store your CoreSwift API key first",
            }))),
            Ok((api_key, base_url)) => {
                let url = format!("{base_url}/api/external/lists");
                let resp = state
                    .http_client
                    .get(&url)
                    .bearer_auth(&api_key)
                    .timeout(std::time::Duration::from_secs(10))
                    .send()
                    .await;

                match resp {
                    Ok(r) if r.status().is_success() => {
                        let body: Value = r.json().await.unwrap_or_else(|_| json!({}));
                        let n = body
                            .get("lists")
                            .and_then(|l| l.as_array())
                            .map(|a| a.len())
                            .unwrap_or(0);
                        Ok(Json(json!({
                            "provider": provider,
                            "ok": true,
                            "live": true,
                            "base_url": base_url,
                            "message": format!("CoreSwift reachable — {n} list(s)"),
                        })))
                    }
                    Ok(r) => {
                        let status = r.status();
                        Ok(Json(json!({
                            "provider": provider,
                            "ok": false,
                            "live": true,
                            "base_url": base_url,
                            "message": format!("CoreSwift returned {status} — key rejected or URL wrong"),
                        })))
                    }
                    Err(e) => Ok(Json(json!({
                        "provider": provider,
                        "ok": false,
                        "live": true,
                        "base_url": base_url,
                        "message": format!("CoreSwift unreachable: {e}"),
                    }))),
                }
            }
        };
    }

    let stored: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM provider_keys
         WHERE account_id = $1 AND provider = $2 AND is_active = true AND api_key <> ''",
    )
    .bind(account_id)
    .bind(&provider)
    .fetch_one(&state.db)
    .await?;

    Ok(Json(json!({
        "provider": provider,
        "ok": stored > 0,
        "live": false,
        "message": if stored > 0 {
            "Credential stored — no live probe for this provider"
        } else {
            "No credential stored for this provider"
        },
    })))
}
