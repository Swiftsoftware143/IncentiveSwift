//! Support/ticket handlers — CRUD for support_tickets + ticket message thread.
use crate::error::AppError;
use crate::security::auth::AuthenticatedUser;
use crate::state::AppState;
use axum::{
    extract::{Path, State},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct SupportTicket {
    pub id: Uuid,
    pub tenant_id: Option<Uuid>,
    pub campaign_id: Option<Uuid>,
    pub contact_id: Option<Uuid>,
    pub subject: String,
    pub description: Option<String>,
    pub status: String,
    pub priority: String,
    pub category: Option<String>,
    pub assignee_id: Option<Uuid>,
    pub created_by: Option<Uuid>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub resolved_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct SupportTicketMessage {
    pub id: Uuid,
    pub ticket_id: Uuid,
    pub author_id: Option<Uuid>,
    pub body: String,
    pub is_internal: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Deserialize)]
pub struct CreateTicketInput {
    pub campaign_id: Option<Uuid>,
    pub contact_id: Option<Uuid>,
    pub subject: String,
    pub description: Option<String>,
    pub priority: Option<String>,
    pub category: Option<String>,
}

#[derive(Deserialize)]
pub struct UpdateTicketInput {
    pub status: Option<String>,
    pub priority: Option<String>,
    pub category: Option<String>,
    pub assignee_id: Option<Uuid>,
    pub subject: Option<String>,
}

#[derive(Deserialize)]
pub struct AddMessageInput {
    pub body: String,
    pub is_internal: Option<bool>,
}

async fn tenant_scope(state: &AppState, account_id: &str) -> Result<Uuid, AppError> {
    let uuid = Uuid::parse_str(account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;
    Ok(uuid)
}

/// GET /api/v1/support-tickets
pub async fn list_tickets(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let scope = tenant_scope(&state, &user.account_id).await?;
    let tickets = sqlx::query_as::<_, SupportTicket>(
        r#"SELECT id, tenant_id, campaign_id, contact_id, subject, description, status,
                  priority, category, assignee_id, created_by, created_at, updated_at, resolved_at
           FROM support_tickets
           WHERE tenant_id = $1 OR created_by = $1
           ORDER BY updated_at DESC"#,
    )
    .bind(scope)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(json!({ "tickets": tickets })))
}

/// POST /api/v1/support-tickets
pub async fn create_ticket(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<CreateTicketInput>,
) -> Result<Json<Value>, AppError> {
    let account = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;
    let id = Uuid::new_v4();
    let priority = body.priority.unwrap_or_else(|| "normal".to_string());
    sqlx::query(
        r#"INSERT INTO support_tickets
             (id, tenant_id, campaign_id, contact_id, subject, description, status, priority, category, created_by)
           VALUES ($1,$2,$3,$4,$5,$6,'open',$7,$8,$9)"#,
    )
    .bind(id)
    .bind(account)
    .bind(body.campaign_id)
    .bind(body.contact_id)
    .bind(&body.subject)
    .bind(&body.description)
    .bind(&priority)
    .bind(&body.category)
    .bind(account)
    .execute(&state.db)
    .await?;
    let row = sqlx::query_as::<_, SupportTicket>(
        r#"SELECT id, tenant_id, campaign_id, contact_id, subject, description, status,
                  priority, category, assignee_id, created_by, created_at, updated_at, resolved_at
           FROM support_tickets WHERE id = $1"#,
    )
    .bind(id)
    .fetch_one(&state.db)
    .await?;
    Ok(Json(json!({ "ticket": row })))
}

/// GET /api/v1/support-tickets/:id
pub async fn get_ticket(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, AppError> {
    let account = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;
    let row = sqlx::query_as::<_, SupportTicket>(
        r#"SELECT id, tenant_id, campaign_id, contact_id, subject, description, status,
                  priority, category, assignee_id, created_by, created_at, updated_at, resolved_at
           FROM support_tickets WHERE id = $1 AND (tenant_id = $2 OR created_by = $2)"#,
    )
    .bind(id)
    .bind(account)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Ticket not found".to_string()))?;

    let messages = sqlx::query_as::<_, SupportTicketMessage>(
        r#"SELECT id, ticket_id, author_id, body, is_internal, created_at
           FROM support_ticket_messages WHERE ticket_id = $1 ORDER BY created_at ASC"#,
    )
    .bind(id)
    .fetch_all(&state.db)
    .await?;

    Ok(Json(json!({ "ticket": row, "messages": messages })))
}

/// PUT /api/v1/support-tickets/:id
pub async fn update_ticket(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateTicketInput>,
) -> Result<Json<Value>, AppError> {
    let account = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;
    let existing = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM support_tickets WHERE id = $1 AND (tenant_id = $2 OR created_by = $2)",
    )
    .bind(id)
    .bind(account)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Ticket not found".to_string()))?;
    let _ = existing;

    let resolved_at: Option<chrono::DateTime<chrono::Utc>> = match body.status.as_deref() {
        Some("resolved") | Some("closed") => Some(chrono::Utc::now()),
        _ => None,
    };

    sqlx::query(
        r#"UPDATE support_tickets SET
             status = COALESCE($2, status),
             priority = COALESCE($3, priority),
             category = COALESCE($4, category),
             assignee_id = COALESCE($5, assignee_id),
             subject = COALESCE($6, subject),
             resolved_at = COALESCE($7, resolved_at),
             updated_at = now()
           WHERE id = $1"#,
    )
    .bind(id)
    .bind(&body.status)
    .bind(&body.priority)
    .bind(&body.category)
    .bind(body.assignee_id)
    .bind(&body.subject)
    .bind(resolved_at)
    .execute(&state.db)
    .await?;

    let row = sqlx::query_as::<_, SupportTicket>(
        r#"SELECT id, tenant_id, campaign_id, contact_id, subject, description, status,
                  priority, category, assignee_id, created_by, created_at, updated_at, resolved_at
           FROM support_tickets WHERE id = $1"#,
    )
    .bind(id)
    .fetch_one(&state.db)
    .await?;
    Ok(Json(json!({ "ticket": row })))
}

/// DELETE /api/v1/support-tickets/:id
pub async fn delete_ticket(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, AppError> {
    let account = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;
    sqlx::query(
        "DELETE FROM support_tickets WHERE id = $1 AND (tenant_id = $2 OR created_by = $2)",
    )
    .bind(id)
    .bind(account)
    .execute(&state.db)
    .await?;
    Ok(Json(json!({ "deleted": true })))
}

/// POST /api/v1/support-tickets/:id/messages
pub async fn add_message(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(id): Path<Uuid>,
    Json(body): Json<AddMessageInput>,
) -> Result<Json<Value>, AppError> {
    let account = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;
    let msg_id = Uuid::new_v4();
    let is_internal = body.is_internal.unwrap_or(false);
    sqlx::query(
        r#"INSERT INTO support_ticket_messages (id, ticket_id, author_id, body, is_internal)
           VALUES ($1,$2,$3,$4,$5)"#,
    )
    .bind(msg_id)
    .bind(id)
    .bind(account)
    .bind(&body.body)
    .bind(is_internal)
    .execute(&state.db)
    .await?;
    // touch ticket updated_at
    sqlx::query("UPDATE support_tickets SET updated_at = now() WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await?;
    Ok(Json(json!({ "id": msg_id })))
}
