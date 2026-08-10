//! Internal portfolio sync handler — receives broadcasts from CoreSwift CRM.
//! Protected by x-internal-key header, not JWT.

use crate::error::AppError;
use crate::state::AppState;
use axum::{extract::State, http::HeaderMap, Json};
use serde_json::{json, Value};
use uuid::Uuid;

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
    if key != state.config.internal_sync_key {
        return Err(AppError::Forbidden("Invalid internal key".into()));
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
                sqlx::query(
                    "INSERT INTO accounts (id, name) VALUES ($1, $2) ON CONFLICT (id) DO NOTHING",
                )
                .bind(aid)
                .bind(&name)
                .execute(&state.db)
                .await
                .ok();
                sqlx::query("INSERT INTO portfolio_companies (id, account_id, name, slug, email) VALUES ($1, $2, $3, $4, $5) ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name, slug = EXCLUDED.slug, email = EXCLUDED.email, updated_at = NOW()")
                    .bind(pid).bind(aid).bind(&name).bind(&slug).bind(&email)
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
                        sqlx::query("INSERT INTO portfolio_companies (id, account_id, name, slug, email) VALUES ($1, $2, $3, $4, $5) ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name, slug = EXCLUDED.slug, email = EXCLUDED.email, updated_at = NOW()")
                            .bind(pid).bind(aid).bind(&name).bind(&slug).bind(&email)
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
