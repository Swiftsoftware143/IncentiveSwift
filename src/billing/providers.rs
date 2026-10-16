//! Payment Provider CRUD handlers.
//!
//! Endpoints:
//!   GET    /api/v1/payment-providers              — list providers
//!   POST   /api/v1/payment-providers              — upsert provider config
//!   DELETE /api/v1/payment-providers/:provider_type — delete THIS account's provider config
//!
//! Scope
//! -----
//! All three arms bind the CALLER'S `account_id`, unconditionally — no `role == "admin"` bypass.
//! The row is the account's own payment configuration: `checkout/create` resolves exactly this
//! account's active row (`WHERE account_id = $1 AND is_active = true`) and the console presents
//! the row as "this workspace's". `delete_payment_provider` carried NO account predicate until
//! kanban t_27514bae (measured live, see the handler).
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
//! Every reader decrypts: the list mask, the upsert echo and `lookup_webhook_secrets` (the HMAC
//! keys the Stripe signature is verified with). Ciphertext is never returned to a client.
//!
//! Inbound webhook verification is PER-ACCOUNT, and that is a decision (kanban t_3c98f2b6)
//! ---------------------------------------------------------------------------------------------
//! The table is per-account BY CONSTRUCTION: `UNIQUE (account_id, provider_type)`, an FK to
//! `accounts`, all three CRUD arms bind the CALLER'S `account_id`, `checkout/create` resolves the
//! account's own active row, and the console and the admin guide both present the row as "this
//! workspace's". The inbound endpoints, however, are ONE shared URL per provider
//! (`POST /api/v1/webhooks/<provider>` — the table has no per-account webhook URL column), so a
//! receiver cannot know which account a delivery belongs to before it authenticates it.
//!
//! Before t_3c98f2b6 the reader was `WHERE provider_type = $1 AND is_active = true LIMIT 1` with no
//! account predicate and no ORDER BY, so whichever row the planner returned first was the secret
//! used to verify EVERY account's delivery: the moment two accounts configured the same provider,
//! only one of them had working webhooks and the others were answered "Invalid webhook signature"
//! (and Stripe's retries could never succeed). The fix keeps the per-account model and makes the
//! shared door work for every account: `lookup_webhook_secrets` returns EVERY active row of the
//! provider type and the receiver accepts the delivery when ANY one of them verifies it, while the
//! destination account keeps coming from the event's OWN identity (the provider session id resolved
//! against `stripe_checkout_sessions`), never from which secret matched.

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
///
/// A `macro_rules!` and not a `const` since kanban t_563a3f10: both readers below must be ONE
/// compile-time literal, so the predicate is expanded by `concat!` instead of interpolated by
/// `format!` at run time (gate rule 5d / class 14). The statement bytes are unchanged.
macro_rules! unsealed_predicate {
    () => {
        "(api_key <> '' AND api_key NOT LIKE 'enc:v1:%') OR (webhook_secret <> '' AND webhook_secret NOT LIKE 'enc:v1:%')"
    };
}

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
    let rows = match sqlx::query(concat!(
        "SELECT id, api_key, webhook_secret FROM payment_providers WHERE ",
        unsealed_predicate!()
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
    let unsealed: i64 = match sqlx::query_scalar(concat!(
        "SELECT count(*) FROM payment_providers WHERE ",
        unsealed_predicate!()
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
            // At-rest posture of the SAVED copies, measured off the ciphertext COLUMN (kanban
            // t_d1fcda7e) and never off the decrypted value above: `is_encrypted` is the same
            // predicate the write path seals with, so the console's Encrypted column cannot claim
            // more than the database actually holds. `_set` is reported beside it because the mask
            // is `***` for BOTH a short credential and an empty slot, so a page cannot tell
            // "configured" from "absent" by the masked string alone.
            "api_key_set": !stored_key.trim().is_empty(),
            "webhook_secret_set": !stored_webhook.trim().is_empty(),
            "api_key_sealed": is_encrypted(stored_key.trim()),
            "webhook_secret_sealed": is_encrypted(stored_webhook.trim()),
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
///
/// Scoped UNCONDITIONALLY to the caller's `account_id` — no `role == "admin"` bypass. Decided by
/// measurement, not by taste: the GET/POST twins above are both `WHERE account_id = $1`,
/// `checkout/create` resolves the account's OWN active row (`WHERE account_id = $1 AND
/// is_active = true`), and the convention this app already established for every account-scoped
/// CRUD family is unconditional scoping (`surfaces_handler`, `integration_target_handler`,
/// `delivery`, `tags`).
///
/// Measured live 2026-10-14 on the pre-fix binary, two probe tenants each owning one `stripe` row:
/// the statement was `DELETE FROM payment_providers WHERE provider_type = $1` — i.e. EVERY
/// account's row — behind `role != "super_admin" -> 403`. A super-admin session of tenant A
/// answered 200 and BOTH rows were gone (A=1 B=1 -> 0 0) while a normal operator was refused even
/// for its OWN row. That is a cross-tenant destructive write reachable from a console row that
/// presents the configuration as this workspace's. No account on this app holds
/// `role = 'super_admin'` (9 accounts: 5 company_admin, 2 admin, 1 user, 1 unrecognised), so the
/// gate left the route dead for every real caller AND kept the platform-wide blast radius for the
/// one session shape that could reach it.
///
/// `rows_affected() == 0 -> 404`: a provider type this account does not own is "not found for this
/// account", never a silent success (and never another account's row).
pub async fn delete_payment_provider(
    State(state): State<AppState>,
    auth: AuthenticatedUser,
    Path(provider_type): Path<String>,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&auth.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;

    let result = sqlx::query(
        r#"
        DELETE FROM payment_providers
        WHERE provider_type = $1 AND account_id = $2
        "#,
    )
    .bind(&provider_type)
    .bind(account_id)
    .execute(&state.db)
    .await?;

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound(format!(
            "No '{}' payment provider is configured for this account",
            provider_type
        )));
    }

    Ok(Json(
        json!({ "status": "deleted", "provider_type": provider_type }),
    ))
}

/// One account's active signing credential for a provider type.
#[derive(Debug, Clone)]
pub struct WebhookSecretRow {
    /// The account the credential belongs to.
    pub account_id: Uuid,
    /// The stored secret, DECRYPTED. Never empty: an unusable row is dropped by the lookup.
    pub secret: String,
}

/// Every account's ACTIVE webhook signing credential for `provider_type`, DECRYPTED.
///
/// This is the read-for-use path for the inbound receivers: the values returned here are the HMAC
/// keys a delivery's signature is checked against, so they must be plaintext (a mask or the raw
/// ciphertext would make every signature check fail). A legacy plaintext row is returned unchanged
/// by the helper.
///
/// Multi-account contract (kanban t_3c98f2b6): the inbound endpoints are one shared URL per
/// provider while the rows are per-account, so the receiver must be able to authenticate ANY
/// account's delivery. The lookup therefore returns EVERY active row of the type, ordered
/// deterministically by `created_at, id`, and the receiver accepts a delivery when ANY one of them
/// verifies it.
///
/// Rows that cannot authenticate anything are skipped LOUDLY rather than failing the whole lookup,
/// because one account's unusable row must never deny another account's live webhooks: an active
/// row whose stored secret is blank (`''` is the column default for "no secret entered yet") is
/// skipped at debug level, and one whose stored value cannot be decrypted with the current master
/// key is skipped at error level with its account id (never the value). If that leaves NO usable
/// credential, the lookup fails, so a misconfigured provider is loud instead of silent.
///
/// The failure is a `503` carrying a REASON SLUG — `<provider>_webhook_secret_not_configured` when
/// no active row exists at all, `<provider>_webhook_secret_unreadable` when rows exist but none is
/// usable (kanban t_b474bd74). It used to be `AppError::Internal`, which answers a bare
/// `500 "Internal server error"`: a payment provider cannot act on that, Stripe retries a 5xx
/// forever, and the operator is told nothing. Every sibling fleet receiver answers the same
/// condition the same way (FunnelSwift `stripe_webhook_secret_not_configured`), so the two apps
/// agree on what an unconfigured receiver says.
///
/// Re-exported from `billing` so the webhook receivers share this lookup.
pub async fn lookup_webhook_secrets(
    state: &AppState,
    provider_type: &str,
) -> Result<Vec<WebhookSecretRow>, AppError> {
    let rows = sqlx::query(
        "SELECT pp.account_id, pp.webhook_secret FROM payment_providers pp \
         WHERE pp.provider_type = $1 AND pp.is_active = true \
         ORDER BY pp.created_at, pp.id",
    )
    .bind(provider_type)
    .fetch_all(&state.db)
    .await?;

    if rows.is_empty() {
        return Err(AppError::ServiceUnavailable(not_configured_reason(
            provider_type,
        )));
    }

    let mut usable: Vec<WebhookSecretRow> = Vec::with_capacity(rows.len());
    for row in &rows {
        let account_id: Uuid = row.get("account_id");
        let stored: String = row.get("webhook_secret");
        if stored.trim().is_empty() {
            tracing::debug!(
                %account_id,
                provider_type,
                "active payment provider row carries no webhook secret — skipped for signature verification"
            );
            continue;
        }
        // Fails closed per row: a wrong/missing master key is never a plaintext or ciphertext HMAC
        // key. The row is dropped and reported; the other accounts keep verifying.
        match decrypt_from_storage(&state.db, stored.trim()).await {
            Ok(secret) if !secret.is_empty() => {
                usable.push(WebhookSecretRow { account_id, secret })
            }
            Ok(_) => tracing::debug!(
                %account_id,
                provider_type,
                "active payment provider webhook secret decrypted to empty — skipped"
            ),
            Err(e) => tracing::error!(
                %account_id,
                provider_type,
                error = %e,
                "active payment provider webhook secret could not be decrypted — skipped for signature verification"
            ),
        }
    }

    if usable.is_empty() {
        return Err(AppError::ServiceUnavailable(unusable_reason(provider_type)));
    }

    Ok(usable)
}

/// The reason slug a receiver answers when NO active `<provider>` row exists at all.
///
/// The vocabulary is the sibling fleet receivers' own: FunnelSwift's Stripe receiver refuses the
/// same condition with `stripe_webhook_secret_not_configured`, so a delivery to either app reads
/// the same way (kanban t_b474bd74). Kept in ONE place so the two arms below cannot drift apart —
/// and, because it is a slug and not the stored value, it is safe to return to an unauthenticated
/// caller.
fn not_configured_reason(provider_type: &str) -> String {
    format!("{}_webhook_secret_not_configured", provider_type)
}

/// The reason slug for the OTHER configuration arm: rows exist, but not one of them carries a
/// usable secret (every one is blank, or none decrypts under the current master key).
///
/// A different slug on purpose — "nothing is configured" and "what is configured cannot be read"
/// need different fixes, and only this one means the master key (or a hand-written row) is wrong.
fn unusable_reason(provider_type: &str) -> String {
    format!("{}_webhook_secret_unreadable", provider_type)
}
