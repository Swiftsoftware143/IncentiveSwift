//! Email Templates handler — full CRUD with admin auth + merge-field master list.
//!
//! Corrected to match the actual `email_templates` schema:
//!   id, template_type, name, subject, body, html_body, is_default, aid, created_at, updated_at
//! (no `is_html`, no `account_id` column).

use crate::error::AppError;
use crate::security::auth::AuthenticatedUser;
use crate::state::AppState;
use axum::{
    extract::{Path, Query, State},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Merge-field master list (single source of truth)
// ---------------------------------------------------------------------------

fn merge_field_list() -> Vec<(&'static str, &'static str)> {
    vec![
        ("first_name", "Contact first name"),
        ("last_name", "Contact last name"),
        ("email", "Contact email address"),
        ("campaign_name", "Campaign display name"),
        (
            "campaign_type",
            "Campaign mechanic type (e.g. quiz, raffle)",
        ),
        ("prize_name", "Name of the prize/reward won"),
        ("prize_value", "Monetary/value of the prize"),
        ("voucher_code", "Issued voucher / PIN code"),
        ("points_awarded", "Loyalty points credited"),
        ("tier_name", "Loyalty tier assigned"),
        ("referral_link", "Personal referral link"),
        ("unsubscribe_link", "Email unsubscribe link"),
        ("campaign_url", "Direct link to the campaign entry"),
        ("expiry_date", "Offer/prize expiry date"),
        ("score", "Score/result value (quiz/calculator)"),
        ("company_name", "Merchant/brand name (from tenant settings)"),
    ]
}

// ---------------------------------------------------------------------------
// Shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct EmailTemplate {
    pub id: Uuid,
    pub template_type: Option<String>,
    pub name: String,
    pub subject: Option<String>,
    pub body: Option<String>,
    pub html_body: Option<String>,
    pub is_default: Option<bool>,
    pub aid: Option<Uuid>,
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
    pub template_type: String,
    pub name: String,
    pub subject: String,
    pub body: Option<String>,
    pub html_body: Option<String>,
    pub is_default: Option<bool>,
}

#[derive(Deserialize)]
pub struct UpdateInput {
    pub template_type: Option<String>,
    pub name: Option<String>,
    pub subject: Option<String>,
    pub body: Option<String>,
    pub html_body: Option<String>,
    pub is_default: Option<bool>,
}

// ---------------------------------------------------------------------------
// GET /api/v1/email-templates/merge-fields — canonical master list
// ---------------------------------------------------------------------------
pub async fn merge_fields(
    State(_state): State<AppState>,
    _user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let fields: Vec<Value> = merge_field_list()
        .into_iter()
        .map(|(token, desc)| {
            json!({
                "token": token,
                "description": desc,
                "placeholder": format!("{{{{{}}}}}", token),
            })
        })
        .collect();

    Ok(Json(json!({ "merge_fields": fields })))
}

// ---------------------------------------------------------------------------
// GET /api/v1/email-templates — list (defaults + account overrides)
// ---------------------------------------------------------------------------
pub async fn list(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Query(query): Query<ListQuery>,
) -> Result<Json<Value>, AppError> {
    let limit = query.limit.unwrap_or(100).min(200);
    let offset = query.offset.unwrap_or(0);
    let account_id = Uuid::parse_str(&user.account_id).unwrap_or_default();

    // Show defaults PLUS the account's own overrides, account override wins visually first.
    let items: Vec<EmailTemplate> = sqlx::query_as::<_, EmailTemplate>(
        "SELECT id, template_type, name, subject, body, html_body, is_default, aid, created_at, updated_at
         FROM email_templates
         WHERE (aid IS NULL OR aid = $2) AND is_default = true
            OR aid = $2
         ORDER BY template_type, (aid = $2) DESC, updated_at DESC
         LIMIT $3 OFFSET $4",
    )
    .bind(query.template_type.as_deref())
    .bind(account_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;

    Ok(Json(json!({ "items": items, "count": items.len() })))
}

// ---------------------------------------------------------------------------
// GET /api/v1/email-templates/:id
// ---------------------------------------------------------------------------
pub async fn get(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    _user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let item = sqlx::query_as::<_, EmailTemplate>(
        "SELECT id, template_type, name, subject, body, html_body, is_default, aid, created_at, updated_at
         FROM email_templates WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Email template not found".to_string()))?;

    Ok(Json(json!({"item": item})))
}

// ---------------------------------------------------------------------------
// POST /api/v1/email-templates — create account override template
// ---------------------------------------------------------------------------
pub async fn create(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<CreateInput>,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account id".to_string()))?;

    if body.template_type.trim().is_empty() || body.name.trim().is_empty() {
        return Err(AppError::BadRequest(
            "template_type and name are required".to_string(),
        ));
    }

    let item: EmailTemplate = sqlx::query_as::<_, EmailTemplate>(
        "INSERT INTO email_templates (template_type, name, subject, body, html_body, is_default, aid)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         RETURNING id, template_type, name, subject, body, html_body, is_default, aid, created_at, updated_at",
    )
    .bind(&body.template_type)
    .bind(&body.name)
    .bind(&body.subject)
    .bind(&body.body)
    .bind(&body.html_body)
    .bind(body.is_default.unwrap_or(false))
    .bind(account_id)
    .fetch_one(&state.db)
    .await?;

    Ok(Json(json!({"item": item})))
}

// ---------------------------------------------------------------------------
// PUT /api/v1/email-templates/:id — update an account override template
// ---------------------------------------------------------------------------
pub async fn update(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    user: AuthenticatedUser,
    Json(body): Json<UpdateInput>,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account id".to_string()))?;

    let item = sqlx::query_as::<_, EmailTemplate>(
        "UPDATE email_templates SET
            template_type = COALESCE($2, template_type),
            name = COALESCE($3, name),
            subject = COALESCE($4, subject),
            body = COALESCE($5, body),
            html_body = COALESCE($6, html_body),
            is_default = COALESCE($7, is_default),
            updated_at = NOW()
         WHERE id = $1 AND aid = $8
         RETURNING id, template_type, name, subject, body, html_body, is_default, aid, created_at, updated_at",
    )
    .bind(id)
    .bind(&body.template_type)
    .bind(&body.name)
    .bind(&body.subject)
    .bind(&body.body)
    .bind(&body.html_body)
    .bind(body.is_default)
    .bind(account_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Template not found or not owned".to_string()))?;

    Ok(Json(json!({"item": item})))
}

// ---------------------------------------------------------------------------
// DELETE /api/v1/email-templates/:id — delete an account override template
// ---------------------------------------------------------------------------
pub async fn delete(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account id".to_string()))?;

    let res = sqlx::query("DELETE FROM email_templates WHERE id = $1 AND aid = $2")
        .bind(id)
        .bind(account_id)
        .execute(&state.db)
        .await?;

    if res.rows_affected() == 0 {
        return Err(AppError::NotFound(
            "Template not found or not owned".to_string(),
        ));
    }

    Ok(Json(json!({"status": "deleted"})))
}
