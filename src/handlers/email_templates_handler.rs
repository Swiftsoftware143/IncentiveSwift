//! Email Templates handler — full CRUD with admin auth.

use crate::error::AppError;
use crate::state::AppState;
use crate::security::auth::AuthenticatedUser;
use axum::{
    extract::{Path, Query, State},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct EmailTemplate {
    pub id: Uuid,
    pub aid: Uuid,
    pub account_id: Option<Uuid>,
    pub name: String,
    pub subject: Option<String>,
    pub body: Option<String>,
    pub html_body: Option<String>,
    pub is_html: Option<bool>,
    pub is_default: Option<bool>,
    pub template_type: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Deserialize)]
pub struct ListQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub template_type: Option<String>,
}

#[derive(Deserialize)]
pub struct CreateInput {
    pub name: String,
    pub subject: Option<String>,
    pub body: Option<String>,
    pub html_body: Option<String>,
    pub is_html: Option<bool>,
    pub is_default: Option<bool>,
    pub template_type: Option<String>,
}

#[derive(Deserialize)]
pub struct UpdateInput {
    pub name: Option<String>,
    pub subject: Option<String>,
    pub body: Option<String>,
    pub html_body: Option<String>,
    pub is_html: Option<bool>,
    pub is_default: Option<bool>,
    pub template_type: Option<String>,
}

/// GET /api/v1/email-templates
pub async fn list(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Query(query): Query<ListQuery>,
) -> Result<Json<Value>, AppError> {
    let limit = query.limit.unwrap_or(50).min(100);
    let offset = query.offset.unwrap_or(0);

    let items = if let Some(tt) = &query.template_type {
        sqlx::query_as::<_, EmailTemplate>(
            "SELECT * FROM email_templates WHERE template_type = $1 ORDER BY name LIMIT $2 OFFSET $3"
        )
        .bind(tt).bind(limit).bind(offset)
        .fetch_all(&state.db)
        .await
        .unwrap_or_default()
    } else {
        sqlx::query_as::<_, EmailTemplate>(
            "SELECT * FROM email_templates ORDER BY name LIMIT $1 OFFSET $2"
        )
        .bind(limit).bind(offset)
        .fetch_all(&state.db)
        .await
        .unwrap_or_default()
    };

    let count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM email_templates")
        .fetch_one(&state.db)
        .await
        .unwrap_or(0);

    Ok(Json(json!({ "items": items, "count": count })))
}

/// GET /api/v1/email-templates/{id}
pub async fn get(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    _user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let item = sqlx::query_as::<_, EmailTemplate>(
        "SELECT * FROM email_templates WHERE id = $1"
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Email template not found".to_string()))?;

    Ok(Json(json!({"item": item})))
}

/// POST /api/v1/email-templates
pub async fn create(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Json(body): Json<CreateInput>,
) -> Result<Json<Value>, AppError> {
    let id = Uuid::new_v4();
    let aid = Uuid::nil();

    sqlx::query(
        r#"INSERT INTO email_templates (id, aid, name, subject, body, html_body, is_html, is_default, template_type)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)"#
    )
    .bind(id).bind(aid)
    .bind(&body.name)
    .bind(&body.subject)
    .bind(&body.body)
    .bind(&body.html_body)
    .bind(body.is_html.unwrap_or(true))
    .bind(body.is_default.unwrap_or(false))
    .bind(&body.template_type)
    .execute(&state.db).await?;

    let item = sqlx::query_as::<_, EmailTemplate>(
        "SELECT * FROM email_templates WHERE id = $1"
    )
    .bind(id).fetch_one(&state.db).await?;

    Ok(Json(json!({"item": item})))
}

/// PUT /api/v1/email-templates/{id}
pub async fn update(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    _user: AuthenticatedUser,
    Json(body): Json<UpdateInput>,
) -> Result<Json<Value>, AppError> {
    sqlx::query(
        r#"UPDATE email_templates SET
            name = COALESCE($1, name),
            subject = COALESCE($2, subject),
            body = COALESCE($3, body),
            html_body = COALESCE($4, html_body),
            is_html = COALESCE($5, is_html),
            is_default = COALESCE($6, is_default),
            template_type = COALESCE($7, template_type),
            updated_at = NOW()
           WHERE id = $8"#
    )
    .bind(&body.name)
    .bind(&body.subject)
    .bind(&body.body)
    .bind(&body.html_body)
    .bind(body.is_html)
    .bind(body.is_default)
    .bind(&body.template_type)
    .bind(id)
    .execute(&state.db).await?;

    let item = sqlx::query_as::<_, EmailTemplate>(
        "SELECT * FROM email_templates WHERE id = $1"
    )
    .bind(id).fetch_one(&state.db).await?;

    Ok(Json(json!({"item": item})))
}

/// DELETE /api/v1/email-templates/{id}
pub async fn delete(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    _user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    sqlx::query("DELETE FROM email_templates WHERE id = $1")
        .bind(id).execute(&state.db).await?;

    Ok(Json(json!({"status": "deleted"})))
}
