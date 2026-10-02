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
//
// ONE rule, enforced against the code (kanban t_375c8c40): a name is advertised only if
// some sender actually BINDS it, and every bound name is advertised. The list used to
// advertise 16 names of which `prize_value`, `voucher_code`, `points_awarded`, `tier_name`,
// `unsubscribe_link`, `campaign_url`, `expiry_date`, `score` and `company_name` were bound
// by NOBODY — a tenant who used one got the braces verbatim (plus the
// `template markup was NOT processed` warn) — while the names the lifecycle sender
// really does carry (`ticket_number`, `user_score`, `share_link`, `reward_code`) were not
// advertised at all.
//
// Bound by the lifecycle sender (`lifecycle_emails::entry_email_vars`, both stages):
//   first_name, last_name, email, campaign_name, campaign_type, ticket_number,
//   user_score (only when the caller supplied a score), prize_name, reward_code,
//   share_link, referral_link.
// Bound by the winner path (`handlers::entries` step 8): + phone, entry_id.
// Bound by the account mails (`email`/`handlers::auth_handler`): name, password,
//   app_name, login_url, plan_name, token, app_url.
fn merge_field_list() -> Vec<(&'static str, &'static str)> {
    vec![
        ("first_name", "Contact first name"),
        ("last_name", "Contact last name"),
        ("email", "Contact email address"),
        ("phone", "Contact phone number"),
        ("name", "Account holder name (account mails)"),
        ("campaign_name", "Campaign display name"),
        (
            "campaign_type",
            "Campaign mechanic type (e.g. quiz, raffle)",
        ),
        ("entry_id", "The entry's id (UUID)"),
        (
            "ticket_number",
            "The entry's own ticket reference (entries.id, first 8 chars, uppercased)",
        ),
        (
            "user_score",
            "Score the entry was submitted with (quiz/calculator)",
        ),
        (
            "prize_name",
            "Name of the prize/reward the contact won in this campaign",
        ),
        (
            "reward_code",
            "The won reward's redemption code (campaign_wins / the entry's answers)",
        ),
        (
            "share_link",
            "Campaign share link (the campaign's public play URL)",
        ),
        (
            "referral_link",
            "Campaign referral link (same public play URL)",
        ),
        ("app_name", "Product name"),
        ("login_url", "App login URL"),
        ("app_url", "App origin"),
        ("plan_name", "Plan name (purchase mails)"),
        ("password", "Minted password (welcome_credentials only)"),
        ("token", "Password-reset token (password_reset only)"),
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
// PUT /api/v1/email-templates/:id
//
// Two owners on one route (kanban t_d87422a3):
//   * an account OVERRIDE row (`aid = <caller>`) — unchanged: only its own account can edit it;
//   * a PLATFORM DEFAULT (`aid IS NULL`) — the seeded row every account inherits
//     (`delivery::sender::load_template_by_type`: `WHERE template_type = $1 AND (aid = $2 OR
//     (aid IS NULL AND is_default = true))`). Until this change `WHERE id = $1 AND aid = $8` gave
//     those rows NO editor at any role: an operator got 404 "Template not found or not owned"
//     (measured live). The whole `/api/v1/email-templates` family is operator-only by
//     `security::auth::is_admin_surface` (the `admin_guard` middleware, mounted above routing), so
//     reaching this handler already means `admin`/`super_admin` (or the internal sync key) — the
//     `OR aid IS NULL` arm therefore widens the statement only for a caller the gate has already
//     accepted. A non-owner still matches neither arm and still gets 404.
//
// Editing a platform default is a FLEET-WIDE edit: every account that has not saved its own copy
// receives this mail. The served console says so before the Save (see www-admin/index.html).
//
// KEY COLUMNS ARE NOT WRITABLE ON A PLATFORM DEFAULT, and that is deliberate, not an omission:
//   * `template_type` is the key every sender looks the row up by (`WHERE template_type = $1`), so
//     retyping a default silently re-points EVERY account's mail for that trigger — that is exactly
//     the drift migration 20260927_retire_unreachable_email_templates.sql had to repair (`scratch_winner`
//     -> `scratch_card_winner`) — and it collides with the sibling default on the partial unique index
//     `idx_email_templates_unique (template_type, COALESCE(aid,'0…'), is_default) WHERE aid IS NULL AND
//     is_default = true`, i.e. it would hand the console a 500 instead of an edit;
//   * `is_default = false` on an `aid IS NULL` row makes it unreachable for EVERY account (the lookup
//     above requires `aid IS NULL AND is_default = true`), a silent fleet-wide mail outage.
// Overrides keep both fields writable, exactly as before.
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
            template_type = CASE WHEN aid IS NULL THEN template_type ELSE COALESCE($2, template_type) END,
            name = COALESCE($3, name),
            subject = COALESCE($4, subject),
            body = COALESCE($5, body),
            html_body = COALESCE($6, html_body),
            is_default = CASE WHEN aid IS NULL THEN is_default ELSE COALESCE($7, is_default) END,
            updated_at = NOW()
         WHERE id = $1 AND (aid = $8 OR aid IS NULL)
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
//
// Deliberately still `WHERE id = $1 AND aid = $2` (kanban t_d87422a3): a platform default is NOT
// deletable from any surface, at any role. Removing one is an irreversible, fleet-wide removal of
// every account's fallback for that trigger — delete the `winner` row and the prize mail
// (`handlers::entries` step 8 falls back to template_type `winner`) stops for EVERY account, and no
// surface can restore it. The app's own precedent is that platform defaults are retired by a
// reviewed migration instead (20260927_retire_unreachable_email_templates.sql deleted 27 rows, after
// a census proved no producer could select them), which is how an operator removes one today. The
// served console therefore offers Edit and no Delete on a platform default row.
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
