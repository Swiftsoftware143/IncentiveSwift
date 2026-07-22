//! Admin handlers — portfolio sync, impersonation, and admin utility endpoints.

use crate::error::AppError;
use crate::state::AppState;
use crate::security::auth::AuthenticatedUser;
use axum::{
    extract::{Path, State},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;

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
    _user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    tracing::info!("Portfolio sync requested");

    // Fetch existing portfolio companies
    let companies = sqlx::query_as::<_, (uuid::Uuid, String)>(
        "SELECT id, name FROM portfolio_companies ORDER BY name"
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
fn create_jwt(account_id: &str, email: &str, role: &str, secret: &str, impersonating: &str) -> Result<String, AppError> {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    use base64::Engine;
    use serde_json::json;

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
        return Err(AppError::Forbidden("Only admins can impersonate users".to_string()));
    }

    // Look up the target account
    let target_id = uuid::Uuid::parse_str(&body.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account_id".to_string()))?;

    let row = sqlx::query(
        "SELECT id, email, role FROM accounts WHERE id = $1"
    )
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
pub async fn stop_impersonation(
    _user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    Ok(Json(json!({
        "status": "impersonation_stopped",
        "message": "Discard your impersonation token to complete the process"
    })))
}

/// DELETE /api/v1/admin/tenants/:id
/// Deletes a tenant account and cleans up related data.
pub async fn delete_tenant(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    let account_id = uuid::Uuid::parse_str(&id)
        .map_err(|_| AppError::BadRequest("Invalid tenant ID".to_string()))?;

    // Delete related data first
    // 1. Delete campaigns belonging to this account
    sqlx::query("DELETE FROM campaigns WHERE account_id = $1")
        .bind(account_id)
        .execute(&state.db)
        .await?;

    // 2. Delete portfolio companies for this account
    sqlx::query("DELETE FROM portfolio_companies WHERE account_id = $1")
        .bind(account_id)
        .execute(&state.db)
        .await?;

    // 3. Delete API keys (uses tenant_id)
    sqlx::query("DELETE FROM api_keys WHERE tenant_id = $1")
        .bind(account_id)
        .execute(&state.db)
        .await?;

    // 4. Delete integration targets
    sqlx::query("DELETE FROM integration_targets WHERE account_id = $1")
        .bind(account_id)
        .execute(&state.db)
        .await?;

    // 5. Delete the account itself
    let result = sqlx::query("DELETE FROM accounts WHERE id = $1")
        .bind(account_id)
        .execute(&state.db)
        .await?;

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound(format!("Tenant not found: {}", id)));
    }

    Ok(Json(json!({
        "status": "deleted",
        "tenant_id": id
    })))
}

/// GET /api/v1/admin/tenants
/// Lists all accounts with their plan info — for the super admin dashboard.
pub async fn list_all_tenants(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
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
        LEFT JOIN plans pt ON a.plan_tier_id = pt.id
        LEFT JOIN (
            SELECT a2.id as acc_id, COUNT(DISTINCT a3.id)::bigint as user_count
            FROM accounts a2
            LEFT JOIN accounts a3 ON a3.tenant_id = a2.id OR a3.id = a2.id
            WHERE a2.id IS NOT NULL
            GROUP BY a2.id
        ) u ON u.acc_id = a.id
        ORDER BY a.created_at DESC
        "#
    )
    .fetch_all(&state.db)
    .await?;

    let tenants: Vec<Value> = rows.iter().map(|row| {
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
    }).collect();

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
        let user_uuid = uuid::Uuid::parse_str(&user.account_id).map_err(|_| {
            AppError::BadRequest("Invalid user account ID".to_string())
        })?;
        let account_tenant_id: Option<uuid::Uuid> = sqlx::query_scalar(
            "SELECT tenant_id FROM accounts WHERE id = $1"
        )
        .bind(user_uuid)
        .fetch_optional(&state.db)
        .await?;
        let tid = uuid::Uuid::parse_str(tenant_id).map_err(|_| {
            AppError::BadRequest("Invalid tenant ID".to_string())
        })?;
        if account_tenant_id.map(|t| t == tid) != Some(true) {
            return Err(AppError::Forbidden("You can only access your own tenant".into()));
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

    let credit_rate: Option<i32> = sqlx::query_scalar(
        "SELECT credit_rate FROM accounts WHERE id = $1"
    )
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
        return Err(AppError::BadRequest("Credit rate must be non-negative".to_string()));
    }

    let updated = sqlx::query_scalar::<_, i32>(
        "UPDATE accounts SET credit_rate = $1 WHERE id = $2 RETURNING credit_rate"
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

    let pin: Option<String> = sqlx::query_scalar(
        "SELECT purchase_pin FROM accounts WHERE id = $1"
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound(format!("Tenant not found: {}", tenant_id)))?;

    Ok(Json(json!({
        "pin": pin
    })))
}

// Note: purchase_pin is auto-generated on account creation. Only read endpoint is exposed.
