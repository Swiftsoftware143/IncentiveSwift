//! Portfolio company handlers — CRUD for portfolio_companies table.

use crate::error::AppError;
use crate::security::auth::AuthenticatedUser;
use crate::state::AppState;
use axum::{
    extract::{Path, State},
    http::HeaderMap,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

/// A portfolio company record.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct PortfolioCompany {
    pub id: Uuid,
    pub account_id: Uuid,
    pub name: String,
    pub slug: String,
    pub settings: Value,
    pub email: Option<String>,
    pub description: Option<String>,
    pub subdomain: Option<String>,
    pub domain: Option<String>,
    pub domain_verified: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// Input for creating a portfolio company.
#[derive(Deserialize)]
pub struct CreatePortfolioCompanyInput {
    pub name: String,
    pub slug: Option<String>,
    pub settings: Option<Value>,
    pub email: Option<String>,
    pub description: Option<String>,
    pub subdomain: Option<String>,
    pub domain: Option<String>,
}

/// Input for updating a portfolio company.
#[derive(Deserialize)]
pub struct UpdatePortfolioCompanyInput {
    pub name: Option<String>,
    pub slug: Option<String>,
    pub settings: Option<Value>,
    pub email: Option<String>,
    pub description: Option<String>,
    pub subdomain: Option<String>,
    pub domain: Option<String>,
}

fn generate_slug(name: &str) -> String {
    let slug: String = name
        .to_lowercase()
        .chars()
        .map(|c| match c {
            'a'..='z' | '0'..='9' | '-' => c,
            ' ' | '_' => '-',
            _ => '-',
        })
        .collect();

    let slug: String = slug
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");

    if slug.is_empty() {
        Uuid::new_v4().to_string()
    } else {
        slug
    }
}

/// The caller's account, or a 400 — never silently defaulted (same helper shape as
/// `integration_target_handler::account_uuid`).
fn account_uuid(user: &AuthenticatedUser) -> Result<Uuid, AppError> {
    Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid user ID".to_string()))
}

/// GET /api/v1/portfolio-companies — the caller's OWN companies.
///
/// SECURITY (kanban t_b7f3c191). This statement carried no `WHERE` clause at all: measured live
/// 2026-10-02, a fresh probe account that owned exactly ONE company received the whole table —
/// 15 rows belonging to other accounts (their names, slugs, emails, subdomains and domains). Same
/// class as `integration_targets` (t_305a0549) and `tags` (t_286aead1): the fix is `WHERE
/// account_id = $1` bound to the caller, unconditional, with no role bypass — every other reader of
/// this table (the delivery hub, the internal sync route) resolves rows by id or by the internal key,
/// never through this route, so scoping removes no capability.
pub async fn list_portfolio_companies(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let account_id = account_uuid(&user)?;
    let companies = sqlx::query_as::<_, PortfolioCompany>(
        r#"SELECT id, account_id, name, slug, settings, email, description, subdomain, domain, domain_verified, created_at, updated_at
           FROM portfolio_companies
           WHERE account_id = $1
           ORDER BY name"#
    )
    .bind(account_id)
    .fetch_all(&state.db)
    .await?;

    Ok(Json(json!({ "companies": companies })))
}

/// POST /api/v1/portfolio-companies
pub async fn create_portfolio_company(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<CreatePortfolioCompanyInput>,
) -> Result<Json<Value>, AppError> {
    let id = Uuid::new_v4();
    let slug = body.slug.unwrap_or_else(|| generate_slug(&body.name));
    let settings = body.settings.unwrap_or_else(|| json!({}));

    sqlx::query(
        r#"INSERT INTO portfolio_companies (id, account_id, name, slug, settings, email, description, subdomain, domain)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)"#
    )
    .bind(id)
    .bind(Uuid::parse_str(&user.account_id).map_err(|_| AppError::BadRequest("Invalid user ID".to_string()))?)
    .bind(&body.name)
    .bind(&slug)
    .bind(&settings)
    .bind(&body.email)
    .bind(&body.description)
    .bind(&body.subdomain)
    .bind(&body.domain)
    .execute(&state.db)
    .await?;

    let company = sqlx::query_as::<_, PortfolioCompany>(
        r#"SELECT id, account_id, name, slug, settings, email, description, subdomain, domain, domain_verified, created_at, updated_at
           FROM portfolio_companies WHERE id = $1"#
    )
    .bind(id)
    .fetch_one(&state.db)
    .await?;

    Ok(Json(json!({ "company": company })))
}

/// GET /api/v1/portfolio-companies/{id} — the caller's OWN company, or 404.
///
/// SECURITY (kanban t_b7f3c191): used to match on the row id alone. Measured live 2026-10-02, a
/// foreign id answered 200 with the other account's row.
pub async fn get_portfolio_company(
    State(state): State<AppState>,
    Path(id): Path<String>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let company_id =
        Uuid::parse_str(&id).map_err(|_| AppError::BadRequest("Invalid company ID".to_string()))?;
    let account_id = account_uuid(&user)?;

    let company = sqlx::query_as::<_, PortfolioCompany>(
        r#"SELECT id, account_id, name, slug, settings, email, description, subdomain, domain, domain_verified, created_at, updated_at
           FROM portfolio_companies WHERE id = $1 AND account_id = $2"#
    )
    .bind(company_id)
    .bind(account_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Portfolio company not found".to_string()))?;

    Ok(Json(json!({ "company": company })))
}

/// PUT /api/v1/portfolio-companies/{id} — the caller's OWN company, or 404.
///
/// SECURITY (kanban t_b7f3c191): ownership is on the *read* as well as the *write*. This handler is
/// read-then-write, so scoping only the UPDATE would 404 on a foreign id and then write the body
/// back anyway — the exact trap `integration_target_handler::update_integration_target` documents.
/// 404 (not 403) is this app's convention: a 403 would confirm the id exists somewhere.
pub async fn update_portfolio_company(
    State(state): State<AppState>,
    Path(id): Path<String>,
    user: AuthenticatedUser,
    Json(body): Json<UpdatePortfolioCompanyInput>,
) -> Result<Json<Value>, AppError> {
    let company_id =
        Uuid::parse_str(&id).map_err(|_| AppError::BadRequest("Invalid company ID".to_string()))?;
    let account_id = account_uuid(&user)?;

    // The column list used to omit `subdomain`/`domain` while the arms below read them with
    // `Row::get`, which PANICS on ColumnNotFound — and those closures run exactly when the body
    // omits the field, which is what the operator console sends ({name, slug}). Measured live
    // 2026-10-02 (kanban t_8e0ae96e): PUT /api/v1/portfolio-companies/:id answered 502 through the
    // vhost, the log carried `panicked at src/handlers/portfolio_handler.rs:179: called
    // Result::unwrap() on an Err value: ColumnNotFound("subdomain")`, and the API process died
    // (docker RestartCount 1 -> 2). Select every column the arms read.
    let existing = sqlx::query(
        r#"SELECT name, slug, settings, email, description, subdomain, domain
           FROM portfolio_companies WHERE id = $1 AND account_id = $2"#,
    )
    .bind(company_id)
    .bind(account_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Portfolio company not found".to_string()))?;

    let name = body.name.unwrap_or_else(|| existing.get("name"));
    let slug = body.slug.unwrap_or_else(|| existing.get("slug"));
    let settings = body.settings.unwrap_or_else(|| existing.get("settings"));
    let email: Option<String> = body.email.or_else(|| existing.get("email"));
    let description: Option<String> = body.description.or_else(|| existing.get("description"));
    let subdomain: Option<String> = body.subdomain.or_else(|| existing.get("subdomain"));
    let domain: Option<String> = body.domain.or_else(|| existing.get("domain"));

    let updated = sqlx::query(
        r#"UPDATE portfolio_companies SET
               name = $1, slug = $2, settings = $3, email = $4, description = $5, subdomain = $6, domain = $7, updated_at = now()
           WHERE id = $8 AND account_id = $9"#
    )
    .bind(&name)
    .bind(&slug)
    .bind(&settings)
    .bind(&email)
    .bind(&description)
    .bind(&subdomain)
    .bind(&domain)
    .bind(company_id)
    .bind(account_id)
    .execute(&state.db)
    .await?;

    if updated.rows_affected() == 0 {
        return Err(AppError::NotFound(
            "Portfolio company not found".to_string(),
        ));
    }

    let company = sqlx::query_as::<_, PortfolioCompany>(
        r#"SELECT id, account_id, name, slug, settings, email, description, subdomain, domain, domain_verified, created_at, updated_at
           FROM portfolio_companies WHERE id = $1 AND account_id = $2"#
    )
    .bind(company_id)
    .bind(account_id)
    .fetch_one(&state.db)
    .await?;

    Ok(Json(json!({ "company": company })))
}

/// POST /api/v1/internal/portfolio-companies — internal sync, no JWT
pub async fn internal_create_portfolio_company(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, AppError> {
    let key = headers
        .get("x-internal-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    // An empty configured key must never authenticate a caller (kanban t_de6f2986).
    if state.config.internal_sync_key.is_empty() || key != state.config.internal_sync_key {
        return Err(AppError::Unauthorized("Invalid internal key".into()));
    }

    let account_id = body
        .get("tenant_id")
        .and_then(|v| v.as_str())
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or_else(|| AppError::BadRequest("tenant_id required".into()))?;

    let name = body
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("Company")
        .to_string();
    let slug = body
        .get("slug")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| generate_slug(&name));
    let email = body
        .get("email")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let domain = body
        .get("domain")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let description = body
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let id = Uuid::new_v4();

    sqlx::query(
        r#"INSERT INTO portfolio_companies (id, account_id, name, slug, email, description, settings, subdomain, domain)
           VALUES ($1, $2, $3, $4, $5, $6, '{}'::jsonb, NULL, NULL) ON CONFLICT (id) DO NOTHING"#
    )
    .bind(id)
    .bind(account_id)
    .bind(&name)
    .bind(&slug)
    .bind(&email)
    .bind(&description)
    .execute(&state.db)
    .await?;

    Ok(Json(json!({"status": "synced", "id": id.to_string()})))
}

/// DELETE /api/v1/portfolio-companies/{id} — the caller's OWN company, or 404.
///
/// SECURITY (kanban t_b7f3c191): used to be `DELETE … WHERE id = $1`. Measured live 2026-10-02, a
/// foreign id answered 200 and the other account's company was gone (its integration_targets going
/// with it through the CASCADE).
pub async fn delete_portfolio_company(
    State(state): State<AppState>,
    Path(id): Path<String>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let company_id =
        Uuid::parse_str(&id).map_err(|_| AppError::BadRequest("Invalid company ID".to_string()))?;
    let account_id = account_uuid(&user)?;

    let result = sqlx::query("DELETE FROM portfolio_companies WHERE id = $1 AND account_id = $2")
        .bind(company_id)
        .bind(account_id)
        .execute(&state.db)
        .await?;

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound(
            "Portfolio company not found".to_string(),
        ));
    }

    Ok(Json(json!({ "status": "deleted", "id": id })))
}
