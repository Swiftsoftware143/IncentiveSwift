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
    /// The requesting workspace's sign-in email. Filled ONLY by the operator's cross-tenant list, so
    /// the team can tell whose request it is; `#[sqlx(default)]` keeps every scoped SELECT that does
    /// not join `accounts` decoding unchanged.
    #[sqlx(default)]
    pub account_email: Option<String>,
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

/// Tell the REQUESTER that their request moved (2026-10-19).
///
/// The operator console is the only place a plan request is answered, and until now the answer
/// stopped there: the customer's dashboard read the state back, but nothing ever told them to look.
/// This is called from the two operator actions that ARE an answer — a status change, and a message
/// the team marked visible to the customer — and from nowhere else, so a customer replying to their
/// own ticket never mails themselves.
///
/// It never fails the caller: the request has already been written by the time this runs, and a mail
/// problem must not roll back or 500 the operator's action. Every failure is logged and dropped,
/// exactly like the reset-mail sender.
async fn notify_requester(
    state: &AppState,
    ticket_id: Uuid,
    actor: Uuid,
    status_word: &str,
    reply: &str,
) {
    let row = sqlx::query_as::<_, (Option<Uuid>, Option<Uuid>, String)>(
        "SELECT tenant_id, created_by, subject FROM support_tickets WHERE id = $1",
    )
    .bind(ticket_id)
    .fetch_optional(&state.db)
    .await;

    let Ok(Some((tenant_id, created_by, subject))) = row else {
        return;
    };
    // The workspace that filed it; `created_by` is the fallback for a ticket opened outside one.
    let Some(target) = tenant_id.or(created_by) else {
        return;
    };
    // Never mail the account that just made the change (an operator answering their own ticket).
    if target == actor {
        return;
    }
    let who = sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT COALESCE(email, ''), name FROM accounts WHERE id = $1",
    )
    .bind(target)
    .fetch_optional(&state.db)
    .await;

    let Ok(Some((email, name))) = who else {
        return;
    };
    if email.trim().is_empty() {
        return;
    }

    if let Err(e) = crate::email::send_plan_request_update_email(
        &state.db,
        target,
        email.trim(),
        name.as_deref().unwrap_or(""),
        &subject,
        status_word,
        reply,
    )
    .await
    {
        tracing::warn!("plan-request update mail to {} failed: {}", email, e);
    }
}

/// The caller's platform role, read from the database rather than the JWT claim: a token minted
/// before a promotion carries the old role, and this decides who may see a customer's private notes.
async fn caller_is_operator(state: &AppState, account: Uuid) -> bool {
    let role =
        sqlx::query_scalar::<_, String>("SELECT COALESCE(role, '') FROM accounts WHERE id = $1")
            .bind(account)
            .fetch_optional(&state.db)
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
    matches!(role.as_str(), "admin" | "super_admin")
}

/// GET /api/v1/support-tickets
pub async fn list_tickets(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let scope = tenant_scope(&state, &user.account_id).await?;
    // An OPERATOR reads the whole queue — otherwise a customer's plan request is invisible to the
    // only people who can answer it (measured: an admin's GET on a customer's ticket was a 404).
    // A customer keeps the strict workspace scope.
    let tickets = if caller_is_operator(&state, scope).await {
        sqlx::query_as::<_, SupportTicket>(
            r#"SELECT t.id, t.tenant_id, t.campaign_id, t.contact_id, t.subject, t.description,
                      t.status, t.priority, t.category, t.assignee_id, t.created_by, t.created_at,
                      t.updated_at, t.resolved_at, a.email AS account_email
               FROM support_tickets t
               LEFT JOIN accounts a ON a.id = t.tenant_id
               ORDER BY t.updated_at DESC
               LIMIT 500"#,
        )
        .fetch_all(&state.db)
        .await?
    } else {
        sqlx::query_as::<_, SupportTicket>(
            r#"SELECT id, tenant_id, campaign_id, contact_id, subject, description, status,
                      priority, category, assignee_id, created_by, created_at, updated_at, resolved_at
               FROM support_tickets
               WHERE tenant_id = $1 OR created_by = $1
               ORDER BY updated_at DESC"#,
        )
        .bind(scope)
        .fetch_all(&state.db)
        .await?
    };
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
    let is_operator = caller_is_operator(&state, account).await;
    let row = sqlx::query_as::<_, SupportTicket>(
        r#"SELECT id, tenant_id, campaign_id, contact_id, subject, description, status,
                  priority, category, assignee_id, created_by, created_at, updated_at, resolved_at
           FROM support_tickets
           WHERE id = $1 AND (tenant_id = $2 OR created_by = $2 OR $3)"#,
    )
    .bind(id)
    .bind(account)
    .bind(is_operator)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Ticket not found".to_string()))?;

    // A customer may read the thread but NEVER the team's private notes: before this, the flag was
    // honoured by the console only, so every internal note was delivered to the customer's browser.
    let messages = if is_operator {
        sqlx::query_as::<_, SupportTicketMessage>(
            r#"SELECT id, ticket_id, author_id, body, is_internal, created_at
               FROM support_ticket_messages WHERE ticket_id = $1 ORDER BY created_at ASC"#,
        )
        .bind(id)
        .fetch_all(&state.db)
        .await?
    } else {
        sqlx::query_as::<_, SupportTicketMessage>(
            r#"SELECT id, ticket_id, author_id, body, is_internal, created_at
               FROM support_ticket_messages
               WHERE ticket_id = $1 AND is_internal = false ORDER BY created_at ASC"#,
        )
        .bind(id)
        .fetch_all(&state.db)
        .await?
    };

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
    let is_operator = caller_is_operator(&state, account).await;
    let existing = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM support_tickets
         WHERE id = $1 AND (tenant_id = $2 OR created_by = $2 OR $3)",
    )
    .bind(id)
    .bind(account)
    .bind(is_operator)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Ticket not found".to_string()))?;
    let _ = existing;

    // The state the customer last saw, so "moved" means moved: a PUT that re-sends the same status
    // must not mail the customer again.
    let before_status =
        sqlx::query_scalar::<_, String>("SELECT status FROM support_tickets WHERE id = $1")
            .bind(id)
            .fetch_optional(&state.db)
            .await?
            .unwrap_or_default();

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

    // A status move by the TEAM is an answer, so the customer is told (2026-10-19). A customer's own
    // PUT is not, and an unchanged status is not.
    if is_operator {
        if let Some(new_status) = body.status.as_deref() {
            if new_status != before_status {
                notify_requester(&state, id, account, new_status, "").await;
            }
        }
    }

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
    let is_operator = caller_is_operator(&state, account).await;
    sqlx::query(
        "DELETE FROM support_tickets
         WHERE id = $1 AND (tenant_id = $2 OR created_by = $2 OR $3)",
    )
    .bind(id)
    .bind(account)
    .bind(is_operator)
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
    // Before this, ANY authenticated account could append to ANY ticket id it could guess. A message
    // is allowed only on a ticket the caller can see (own workspace, own creation, or operator), and
    // a private note is the operator's instrument alone — a customer must not be able to mint one.
    let is_operator = caller_is_operator(&state, account).await;
    let visible = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM support_tickets
         WHERE id = $1 AND (tenant_id = $2 OR created_by = $2 OR $3)",
    )
    .bind(id)
    .bind(account)
    .bind(is_operator)
    .fetch_optional(&state.db)
    .await?;
    if visible.is_none() {
        return Err(AppError::NotFound("Ticket not found".to_string()));
    }
    let msg_id = Uuid::new_v4();
    let is_internal = is_operator && body.is_internal.unwrap_or(false);
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

    // A reply the TEAM marked visible IS the answer the customer asked for, so it is mailed to them
    // (2026-10-19). Before this, the console posted every reply as an internal note and the customer
    // could not read it at all. An internal note, a customer's own message, and the operator's own
    // ticket are all silent.
    if is_operator && !is_internal {
        let status =
            sqlx::query_scalar::<_, String>("SELECT status FROM support_tickets WHERE id = $1")
                .bind(id)
                .fetch_optional(&state.db)
                .await
                .ok()
                .flatten()
                .unwrap_or_else(|| "updated".to_string());
        notify_requester(&state, id, account, &status, &body.body).await;
    }

    Ok(Json(json!({ "id": msg_id })))
}
