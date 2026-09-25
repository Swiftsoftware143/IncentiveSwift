//! Payment Provider CRUD handlers.
//!
//! Endpoints:
//!   GET    /api/v1/payment-providers              — list providers
//!   POST   /api/v1/payment-providers              — upsert provider config
//!   DELETE /api/v1/payment-providers/:provider_type — delete a provider
//!
//! At-rest shape
//! -------------
//! `api_key` and `webhook_secret` hold CUSTOMER-SUPPLIED payment credentials (a Stripe/PayPal
//! secret key and the HMAC key the inbound webhook signature is verified against). Both are
//! sealed with the app's own BYOK convention — `enc:v1:` + AES-256 ciphertext through
//! `crate::security::provider_key_crypto`, mastered by `PROVIDER_KEY_ENC_SECRET` in the process
//! environment only — and the DB CHECK constraints added by migration
//! 20260925_payment_providers_secrets_encrypted_at_rest make a future writer that forgets to
//! encrypt FAIL CLOSED.
//!
//! Every reader decrypts: the list mask, the upsert echo and `lookup_webhook_secret` (the HMAC
//! key the Stripe signature is verified with). Ciphertext is never returned to a client.

use crate::error::AppError;
use crate::security::auth::AuthenticatedUser;
use crate::security::provider_key_crypto::{
    decrypt_from_storage, encrypt_for_storage, is_encrypted, mask,
};
use crate::state::AppState;
use axum::{
    extract::{Path, State},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// At-rest guard (boot halves)
// ---------------------------------------------------------------------------

/// The two CHECK constraints the guard migration arms. Kept in one place so the boot
/// add-if-missing / validate path and the migration cannot drift apart.
pub const API_KEY_CONSTRAINT: &str = "payment_providers_api_key_encrypted";
pub const WEBHOOK_SECRET_CONSTRAINT: &str = "payment_providers_webhook_secret_encrypted";

/// The predicate both constraints apply, as a SQL fragment.
const UNSEALED_PREDICATE: &str =
    "(api_key <> '' AND api_key NOT LIKE 'enc:v1:%') OR (webhook_secret <> '' AND webhook_secret NOT LIKE 'enc:v1:%')";

/// Add a constraint if it is ABSENT. The migration runner records each file in `_migrations` and
/// never re-runs it, so a constraint dropped by hand (or left out by a restore) would stay missing
/// forever — and with it the only thing that stops a future writer from storing a plaintext
/// payment credential.
async fn ensure_constraint(
    pool: &sqlx::PgPool,
    name: &str,
    column: &str,
) -> Result<(), sqlx::Error> {
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = $1)")
            .bind(name)
            .fetch_one(pool)
            .await?;

    if !exists {
        // NOT VALID: pre-existing plaintext rows stay exempt so arming the guard can never block
        // a boot. New writes are checked either way, which is the point.
        let sql = format!(
            "ALTER TABLE payment_providers ADD CONSTRAINT {name} CHECK ({column} = '' OR {column} LIKE 'enc:v1:%') NOT VALID",
        );
        sqlx::query(&sql).execute(pool).await?;
        tracing::warn!(
            constraint = name,
            "payment_providers at-rest guard was missing — re-armed NOT VALID"
        );
    }

    Ok(())
}

/// Seal any legacy plaintext secret in place, then validate both constraints.
///
/// Called once per boot from `AppState::new`, right after the migrations. Idempotent: on a
/// database where every row is already `enc:v1:` it validates and returns.
///
/// Fail-closed: without `PROVIDER_KEY_ENC_SECRET` nothing can be sealed, so the constraints are
/// left NOT VALID and the reason is logged at error level. Writes still fail closed in the
/// handler (a plaintext credential is never stored as a fallback).
pub async fn seal_payment_provider_secrets(pool: &sqlx::PgPool) {
    for (name, column) in [
        (API_KEY_CONSTRAINT, "api_key"),
        (WEBHOOK_SECRET_CONSTRAINT, "webhook_secret"),
    ] {
        if let Err(e) = ensure_constraint(pool, name, column).await {
            tracing::error!(
                constraint = name,
                error = %e,
                "could not arm the payment_providers at-rest guard — secret columns may be unguarded"
            );
            return;
        }
    }

    // 1. Seal what is still in the clear. Empty values are left alone (an empty slot is not a
    //    credential), and anything already carrying the prefix is skipped.
    let rows = match sqlx::query(&format!(
        "SELECT id, api_key, webhook_secret FROM payment_providers WHERE {UNSEALED_PREDICATE}"
    ))
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::error!(error = %e, "could not read payment_providers while sealing secrets");
            return;
        }
    };

    if !rows.is_empty() {
        if crate::security::provider_key_crypto::master_key().is_none() {
            tracing::error!(
                rows = rows.len(),
                "payment_providers holds plaintext secret(s) and PROVIDER_KEY_ENC_SECRET is not \
                 configured — cannot seal them, guard left NOT VALID"
            );
            return;
        }

        let mut sealed = 0usize;
        for row in &rows {
            let id: Uuid = row.get("id");
            let stored_key: String = row.get("api_key");
            let stored_webhook: String = row.get("webhook_secret");

            let new_key = if stored_key.is_empty() || is_encrypted(&stored_key) {
                stored_key.clone()
            } else {
                match encrypt_for_storage(pool, &stored_key).await {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::error!(error = %e, "could not seal payment_providers.api_key");
                        return;
                    }
                }
            };
            let new_webhook = if stored_webhook.is_empty() || is_encrypted(&stored_webhook) {
                stored_webhook.clone()
            } else {
                match encrypt_for_storage(pool, &stored_webhook).await {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::error!(error = %e, "could not seal payment_providers.webhook_secret");
                        return;
                    }
                }
            };

            if new_key == stored_key && new_webhook == stored_webhook {
                continue;
            }

            if let Err(e) = sqlx::query(
                "UPDATE payment_providers SET api_key = $2, webhook_secret = $3 WHERE id = $1",
            )
            .bind(id)
            .bind(&new_key)
            .bind(&new_webhook)
            .execute(pool)
            .await
            {
                tracing::error!(error = %e, row_id = %id, "could not seal a payment_providers row");
                return;
            }
            sealed += 1;
        }

        tracing::warn!(
            rows = sealed,
            "sealed plaintext payment_providers secret(s) in place"
        );
    }

    // 2. Validate, but only once nothing is left unsealed — VALIDATE against a plaintext row
    //    fails, and a half-validated guard reads as guarded when it is not.
    let unsealed: i64 = match sqlx::query_scalar(&format!(
        "SELECT count(*) FROM payment_providers WHERE {UNSEALED_PREDICATE}"
    ))
    .fetch_one(pool)
    .await
    {
        Ok(n) => n,
        Err(e) => {
            tracing::error!(error = %e, "could not count unsealed payment_providers rows");
            return;
        }
    };

    if unsealed > 0 {
        tracing::warn!(
            rows = unsealed,
            "payment_providers still holds plaintext secret(s) — guard left NOT VALID"
        );
        return;
    }

    for name in [API_KEY_CONSTRAINT, WEBHOOK_SECRET_CONSTRAINT] {
        if let Err(e) = sqlx::query(&format!(
            "ALTER TABLE payment_providers VALIDATE CONSTRAINT {name}"
        ))
        .execute(pool)
        .await
        {
            tracing::warn!(constraint = name, error = %e, "could not validate the payment_providers at-rest guard");
            continue;
        }

        let validated: Option<bool> =
            sqlx::query_scalar("SELECT convalidated FROM pg_constraint WHERE conname = $1")
                .bind(name)
                .fetch_optional(pool)
                .await
                .unwrap_or(None);

        tracing::info!(
            constraint = name,
            validated = validated.unwrap_or(false),
            "payment_providers at-rest guard armed (enc:v1: ciphertext only)"
        );
    }
}

// ---------------------------------------------------------------------------
// Input types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct UpsertPaymentProviderInput {
    pub provider_type: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub webhook_secret: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub metadata: Option<Value>,
    #[serde(default)]
    pub is_active: Option<bool>,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// GET /api/v1/payment-providers
pub async fn list_payment_providers(
    State(state): State<AppState>,
    auth: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&auth.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;

    let rows = sqlx::query(
        r#"
        SELECT pp.id, pp.account_id, pp.provider_type, pp.api_key, pp.webhook_secret,
               pp.base_url, pp.metadata, pp.is_active, pp.created_at, pp.updated_at
        FROM payment_providers pp
        WHERE pp.account_id = $1
        ORDER BY pp.provider_type
        "#,
    )
    .bind(account_id)
    .fetch_all(&state.db)
    .await?;

    // The columns hold `enc:v1:` ciphertext: mask the DECRYPTED value so the read path reports
    // "a credential is stored" without echoing it (and never returns the ciphertext). A legacy
    // plaintext row passes through the helper unchanged.
    let mut items: Vec<Value> = Vec::with_capacity(rows.len());
    for row in &rows {
        let stored_key: String = row.get("api_key");
        let stored_webhook: String = row.get("webhook_secret");
        let plain_key = decrypt_from_storage(&state.db, stored_key.trim()).await?;
        let plain_webhook = decrypt_from_storage(&state.db, stored_webhook.trim()).await?;

        items.push(json!({
            "id": row.get::<Uuid, _>("id"),
            "account_id": row.get::<Uuid, _>("account_id"),
            "provider_type": row.get::<String, _>("provider_type"),
            "api_key_masked": mask(&plain_key),
            "webhook_secret_masked": mask(&plain_webhook),
            "base_url": row.get::<Option<String>, _>("base_url"),
            "metadata": row.get::<Option<serde_json::Value>, _>("metadata"),
            "is_active": row.get::<bool, _>("is_active"),
            "created_at": row.get::<chrono::DateTime<chrono::Utc>, _>("created_at"),
            "updated_at": row.get::<chrono::DateTime<chrono::Utc>, _>("updated_at"),
        }));
    }

    Ok(Json(json!({ "items": items, "count": items.len() })))
}

/// POST /api/v1/payment-providers
pub async fn upsert_payment_provider(
    State(state): State<AppState>,
    auth: AuthenticatedUser,
    Json(body): Json<UpsertPaymentProviderInput>,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&auth.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;

    // Encrypt BEFORE the write so both columns only ever hold `enc:v1:` ciphertext. This FAILS
    // CLOSED (500) when PROVIDER_KEY_ENC_SECRET is missing — a payment credential is never stored
    // in the clear as a fallback. A blank/omitted credential encrypts to '' , which is exactly the
    // "keep the stored value" signal the ON CONFLICT CASE below depends on.
    let plain_key = body.api_key.as_deref().unwrap_or("").trim().to_string();
    let plain_webhook = body
        .webhook_secret
        .as_deref()
        .unwrap_or("")
        .trim()
        .to_string();
    let stored_api_key = encrypt_for_storage(&state.db, &plain_key).await?;
    let stored_webhook_secret = encrypt_for_storage(&state.db, &plain_webhook).await?;

    // Upsert using ON CONFLICT pattern
    let row = sqlx::query(
        r#"
        INSERT INTO payment_providers (account_id, provider_type, api_key, webhook_secret, base_url, metadata, is_active)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        ON CONFLICT (account_id, provider_type)
        DO UPDATE SET
            api_key = CASE
                WHEN EXCLUDED.api_key IS NOT NULL AND EXCLUDED.api_key != ''
                THEN EXCLUDED.api_key
                ELSE payment_providers.api_key
            END,
            webhook_secret = CASE
                WHEN EXCLUDED.webhook_secret IS NOT NULL AND EXCLUDED.webhook_secret != ''
                THEN EXCLUDED.webhook_secret
                ELSE payment_providers.webhook_secret
            END,
            base_url = COALESCE(EXCLUDED.base_url, payment_providers.base_url),
            metadata = CASE
                WHEN EXCLUDED.metadata IS NOT NULL AND EXCLUDED.metadata != '{}'::jsonb
                THEN EXCLUDED.metadata
                ELSE payment_providers.metadata
            END,
            is_active = EXCLUDED.is_active,
            updated_at = now()
        RETURNING id, account_id, provider_type, api_key, webhook_secret,
                  base_url, metadata, is_active, created_at, updated_at
        "#
    )
    .bind(account_id)
    .bind(&body.provider_type)
    .bind(&stored_api_key)
    .bind(&stored_webhook_secret)
    .bind(&body.base_url)
    .bind(&body.metadata)
    .bind(body.is_active.unwrap_or(true))
    .fetch_one(&state.db)
    .await?;

    let raw_key: String = row.get("api_key");
    let raw_webhook: String = row.get("webhook_secret");
    // Mask the credential that is actually in the row: the submitted value when one was supplied,
    // otherwise the DECRYPTED previously-stored one. Ciphertext never reaches the client.
    let key_mask_source = if plain_key.is_empty() {
        decrypt_from_storage(&state.db, raw_key.trim()).await?
    } else {
        plain_key.clone()
    };
    let webhook_mask_source = if plain_webhook.is_empty() {
        decrypt_from_storage(&state.db, raw_webhook.trim()).await?
    } else {
        plain_webhook.clone()
    };
    let item = json!({
        "id": row.get::<Uuid, _>("id"),
        "account_id": row.get::<Uuid, _>("account_id"),
        "provider_type": row.get::<String, _>("provider_type"),
        "api_key_masked": mask(&key_mask_source),
        "webhook_secret_masked": mask(&webhook_mask_source),
        "base_url": row.get::<Option<String>, _>("base_url"),
        "metadata": row.get::<Option<serde_json::Value>, _>("metadata"),
        "is_active": row.get::<bool, _>("is_active"),
        "created_at": row.get::<chrono::DateTime<chrono::Utc>, _>("created_at"),
        "updated_at": row.get::<chrono::DateTime<chrono::Utc>, _>("updated_at"),
    });

    Ok(Json(json!({ "item": item })))
}

/// DELETE /api/v1/payment-providers/:provider_type
pub async fn delete_payment_provider(
    State(state): State<AppState>,
    auth: AuthenticatedUser,
    Path(provider_type): Path<String>,
) -> Result<Json<Value>, AppError> {
    // Super admin gate: only super admins can delete payment providers
    if auth.role != "super_admin" {
        return Err(AppError::Forbidden(
            "Only super admins can delete payment providers".to_string(),
        ));
    }

    let result = sqlx::query("DELETE FROM payment_providers WHERE provider_type = $1")
        .bind(&provider_type)
        .execute(&state.db)
        .await?;

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound(format!(
            "Payment provider not found for type '{}'",
            provider_type
        )));
    }

    Ok(Json(
        json!({ "status": "deleted", "provider_type": provider_type }),
    ))
}

/// Look up a payment provider's webhook secret from the database by provider type.
/// Returns the first active provider's webhook secret, DECRYPTED.
///
/// The column holds `enc:v1:` ciphertext, and this is the read-for-use path: the value returned
/// here is the HMAC key the inbound Stripe signature is verified with, so it must be the
/// plaintext (a mask or the raw ciphertext would make every signature check fail). A legacy
/// plaintext row is returned unchanged by the helper.
/// Re-exported from `billing` so webhooks can share this lookup.
pub async fn lookup_webhook_secret(
    state: &AppState,
    provider_type: &str,
) -> Result<String, AppError> {
    let row = sqlx::query(
        "SELECT pp.webhook_secret FROM payment_providers pp WHERE pp.provider_type = $1 AND pp.is_active = true LIMIT 1"
    )
    .bind(provider_type)
    .fetch_optional(&state.db)
    .await?;

    match row {
        Some(r) => {
            let stored: String = r.get("webhook_secret");
            if stored.trim().is_empty() {
                return Err(AppError::Internal(format!(
                    "{} webhook secret is configured but empty",
                    provider_type
                )));
            }
            // Fails closed: a wrong/missing master key is an error (500), never a plaintext or
            // ciphertext HMAC key.
            let secret = decrypt_from_storage(&state.db, stored.trim()).await?;
            if secret.is_empty() {
                Err(AppError::Internal(format!(
                    "{} webhook secret is configured but empty",
                    provider_type
                )))
            } else {
                Ok(secret)
            }
        }
        None => Err(AppError::Internal(format!(
            "No active {} payment provider configured",
            provider_type
        ))),
    }
}
