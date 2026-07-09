//! Admin handlers — portfolio sync, impersonation, and admin utility endpoints.

use crate::error::AppError;
use crate::state::AppState;
use crate::security::auth::AuthenticatedUser;
use axum::{
    extract::State,
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
        LEFT JOIN plan_tiers pt ON a.plan_tier_id = pt.id
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
