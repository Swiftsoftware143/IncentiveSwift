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
    /// WHERE the ticket came from — `plan_request` is what the customer's Plan & billing screen
    /// files when they ask to buy (this install has no payment provider, so that request IS the
    /// buying path). NULL means an ordinary ticket. `#[sqlx(default)]` keeps every scoped SELECT
    /// that predates the column decoding unchanged.
    #[sqlx(default)]
    pub source: Option<String>,
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
    /// The plan CATALOGUE slug this request is about (cron 2026-10-20). NULL for every ticket that
    /// names no plan, which is almost all of them. `#[sqlx(default)]` keeps every scoped SELECT
    /// that predates the column decoding unchanged.
    #[sqlx(default)]
    pub plan_slug: Option<String>,
    /// When the operator's Resolve moved the workspace onto `plan_slug`. This is the idempotency
    /// mark: re-saving `resolved` must not move the plan a second time.
    #[sqlx(default)]
    pub plan_applied_at: Option<chrono::DateTime<chrono::Utc>>,
    /// The plan's DISPLAY name, joined from `plans` by the operator's list. Without it the console
    /// printed the bare catalogue slug ("pro") at the operator, which is an internal identifier, not
    /// the plan a customer asked for. `#[sqlx(default)]` keeps every SELECT that does not join it
    /// decoding unchanged.
    #[sqlx(default)]
    pub plan_name: Option<String>,
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
    /// The ticket's origin, normalised by the handler: `plan_request` for the customer's "Request
    /// the <plan> plan" button. Absent or junk is stored as an ordinary ticket (NULL), never a 4xx:
    /// a malformed origin must not cost the customer their buying request.
    #[serde(default)]
    pub source: Option<String>,
    /// The plan CATALOGUE slug this request is about (cron 2026-10-20). Optional: a ticket about a
    /// broken mechanic names no plan. When present it is validated against `plans` BEFORE the row is
    /// written, so an unknown slug is a sentence at the edge rather than a request the operator has
    /// to interpret out of prose — and it is the value `update_ticket` later switches the workspace
    /// onto when the request is resolved.
    #[serde(default)]
    pub plan_slug: Option<String>,
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

/// `resolved` / `closed` is the operator saying the request was handled (cron 2026-10-20).
fn closes_request(status: &str) -> bool {
    matches!(status, "resolved" | "closed")
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
                      t.status, t.priority, t.category, t.source, t.assignee_id, t.created_by, t.created_at,
                      t.updated_at, t.resolved_at, t.plan_slug, t.plan_applied_at,
                      COALESCE(p.name, t.plan_slug) AS plan_name, a.email AS account_email
               FROM support_tickets t
               LEFT JOIN accounts a ON a.id = t.tenant_id
               LEFT JOIN plans p ON p.slug = t.plan_slug
               ORDER BY t.updated_at DESC
               LIMIT 500"#,
        )
        .fetch_all(&state.db)
        .await?
    } else {
        sqlx::query_as::<_, SupportTicket>(
            // THE CUSTOMER'S OWN LIST CARRIES THE PLAN'S NAME (cron 2026-10-20). The row it read
            // before selected `plan_slug` alone, so a request filed from the dashboard came back
            // with `plan_name: null` and the console had to re-derive the display name from a local
            // slug map. The operator's arm has joined `plans` since the plan column existed; this is
            // the same join in the read direction, so BOTH callers get one shape. `COALESCE(…, slug)`
            // keeps a request naming a plan that has since been retired readable (it prints the
            // slug rather than nothing).
            r#"SELECT t.id, t.tenant_id, t.campaign_id, t.contact_id, t.subject, t.description, t.status,
                      t.priority, t.category, t.source, t.assignee_id, t.created_by, t.created_at,
                      t.updated_at, t.resolved_at, t.plan_slug, t.plan_applied_at,
                      COALESCE(p.name, t.plan_slug) AS plan_name
               FROM support_tickets t
               LEFT JOIN plans p ON p.slug = t.plan_slug
               WHERE t.tenant_id = $1 OR t.created_by = $1
               ORDER BY t.updated_at DESC"#,
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
    // One spelling, so the operator's Plan requests view is a single-value match. Trimmed,
    // lower-cased, and limited to slug characters; empty after that becomes NULL (ordinary ticket).
    let source: Option<String> = body
        .source
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase())
        .map(|s| {
            s.chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
                .collect::<String>()
        })
        .filter(|s| !s.is_empty() && s.len() <= 40);
    // ── THE PLAN THE REQUEST IS ABOUT (cron 2026-10-20) ────────────────────────────────────────
    // Before this, the plan existed only as words inside `subject` ("Move request: Pro plan"), so
    // the operator's "Resolved" told the customer the request was handled while the workspace
    // stayed on Free. Resolving now does the switch, and it needs the slug to do it. Refusals are
    // specific and land BEFORE the row: an unknown slug and a free plan are both 400s, never a
    // stored request nobody can act on.
    //
    // The price is read the way this app reads it everywhere else — `plans.price_monthly` on this
    // install holds 0 for the paid tiers (measured live 2026-10-20: pro/enterprise), the money is
    // in the legacy `price` column, so the COALESCE is the same one `checkout` uses. A bare
    // `price::float8` would have refused to let a customer ask for Pro at all.
    let plan_slug: Option<String> = body
        .plan_slug
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase());
    let mut plan_name = String::new();
    if let Some(slug) = plan_slug.as_deref() {
        if slug.len() > 60 {
            return Err(AppError::BadRequest(
                "That plan name is too long".to_string(),
            ));
        }
        let plan: Option<(String, f64)> = sqlx::query_as(
            "SELECT name, COALESCE(NULLIF(price_monthly, 0), price)::float8 \
               FROM plans WHERE slug = $1",
        )
        .bind(slug)
        .fetch_optional(&state.db)
        .await?;
        let (name, price) =
            plan.ok_or_else(|| AppError::BadRequest(format!("No plan is called '{slug}'")))?;
        if price <= 0.0 {
            return Err(AppError::BadRequest(format!(
                "{name} is a free plan - there is nothing to switch to"
            )));
        }
        plan_name = name;
    }

    sqlx::query(
        r#"INSERT INTO support_tickets
             (id, tenant_id, campaign_id, contact_id, subject, description, status, priority, category, source, created_by, plan_slug)
           VALUES ($1,$2,$3,$4,$5,$6,'open',$7,$8,$9,$10,$11)"#,
    )
    .bind(id)
    .bind(account)
    .bind(body.campaign_id)
    .bind(body.contact_id)
    .bind(&body.subject)
    .bind(&body.description)
    .bind(&priority)
    .bind(&body.category)
    .bind(&source)
    .bind(account)
    .bind(plan_slug.as_deref())
    .execute(&state.db)
    .await?;
    let row = sqlx::query_as::<_, SupportTicket>(
        r#"SELECT id, tenant_id, campaign_id, contact_id, subject, description, status,
                  priority, category, source, assignee_id, created_by, created_at, updated_at, resolved_at,
                  plan_slug, plan_applied_at
           FROM support_tickets WHERE id = $1"#,
    )
    .bind(id)
    .fetch_one(&state.db)
    .await?;
    // Tell the filer we have it (cron 2026-10-20). On this install no payment provider is stored, so
    // this row IS the buying path: the dashboard's "Request the plan" button files one of these. The
    // row was written and the response was 201 while the filer's inbox stayed empty — they asked to
    // hand over money and heard nothing until an operator happened to answer. Best-effort on purpose:
    // the row is what the operator works from, so a mail problem never fails (or 500s) the request;
    // the response carries `acknowledged`, which the dashboard reads before promising a copy.
    let mut acknowledged = false;
    let filer: Option<(String, String)> =
        sqlx::query_as("SELECT email, COALESCE(NULLIF(name, ''), '') FROM accounts WHERE id = $1")
            .bind(account)
            .fetch_optional(&state.db)
            .await
            .unwrap_or(None);
    if let Some((email, name)) = filer {
        if let Err(e) = crate::email::send_support_request_receipt_email(
            &state.db,
            account,
            &email,
            &name,
            &body.subject,
        )
        .await
        {
            tracing::warn!(
                ticket = %id,
                to = %email,
                "support-request receipt mail was not sent: {e}"
            );
        } else {
            acknowledged = true;
        }
    }
    Ok(Json(json!({
        "acknowledged": acknowledged,
        "ticket": row,
        "plan_name": plan_name,
    })))
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
        r#"SELECT t.id, t.tenant_id, t.campaign_id, t.contact_id, t.subject, t.description,
                  t.status, t.priority, t.category, t.source, t.assignee_id, t.created_by,
                  t.created_at, t.updated_at, t.resolved_at, t.plan_slug, t.plan_applied_at,
                  COALESCE(p.name, t.plan_slug) AS plan_name
           FROM support_tickets t
           LEFT JOIN plans p ON p.slug = t.plan_slug
           WHERE t.id = $1 AND (t.tenant_id = $2 OR t.created_by = $2 OR $3)"#,
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
                  priority, category, source, assignee_id, created_by, created_at, updated_at, resolved_at,
                  plan_slug, plan_applied_at
           FROM support_tickets WHERE id = $1"#,
    )
    .bind(id)
    .fetch_one(&state.db)
    .await?;

    // ── CLOSING THE LOOP (cron 2026-10-20) ────────────────────────────────────────────────────
    // `resolved`/`closed` is the operator saying the request was handled. Until this arm the state
    // moved and the workspace's plan did not: the customer read a green "Resolved" chip while their
    // dashboard still said Free, and the switch was a SEPARATE action on the Plans screen that
    // nothing asked for.
    //
    // Only on the TRANSITION into a closing state, only when the request named a plan, only when
    // `plan_applied_at` is still empty — and ONLY FOR AN OPERATOR. That last guard is load-bearing:
    // a customer may PUT their own request (the `tenant_id = $2 OR created_by = $2` scope above),
    // so without it any customer could mark their own plan request resolved and hand themselves a
    // paid tier for free.
    //
    // A plan that cannot be resolved does not fail the operator's save — the state change is the
    // record — but the note travels back so the screen can say the plan did NOT move.
    let mut plan_applied = false;
    let mut plan_note: Option<String> = None;
    if is_operator {
        if let Some(new_status) = body.status.as_deref() {
            let ticket_plan = row.plan_slug.clone().unwrap_or_default();
            if closes_request(new_status)
                && !closes_request(&before_status)
                && !ticket_plan.is_empty()
            {
                if row.plan_applied_at.is_some() {
                    plan_note = Some(format!("already switched to {ticket_plan}"));
                } else if let Some(move_account) = row.tenant_id {
                    let target: Option<(Uuid, String)> =
                        sqlx::query_as("SELECT id, name FROM plans WHERE slug = $1")
                            .bind(&ticket_plan)
                            .fetch_optional(&state.db)
                            .await?;
                    match target {
                        None => {
                            tracing::warn!(
                                ticket = %id, plan = %ticket_plan,
                                "plan request resolved, but no plan carries that slug - the plan did NOT move"
                            );
                            plan_note = Some(format!("no plan is called '{ticket_plan}' any more"));
                        }
                        Some((plan_id, plan_name)) => {
                            let current: Option<String> = sqlx::query_scalar(
                                "SELECT pt.slug FROM accounts a \
                                   JOIN plan_tiers pt ON pt.id = a.plan_tier_id \
                                  WHERE a.id = $1",
                            )
                            .bind(move_account)
                            .fetch_optional(&state.db)
                            .await
                            .unwrap_or(None);
                            if current.as_deref() == Some(ticket_plan.as_str()) {
                                plan_note = Some(format!("already on {plan_name}"));
                            } else {
                                // The plan writer is SHARED with the operator's Change Plan
                                // picker (`plans_handler::assign_plan`), so the same column is
                                // written and the referring affiliate is settled exactly as it is
                                // there.
                                match crate::handlers::plans_handler::assign_plan(
                                    &state,
                                    move_account,
                                    plan_id,
                                )
                                .await
                                {
                                    Ok(_) => {
                                        // The mark and the move belong together: a workspace that
                                        // moved but kept an empty stamp could be moved again by a
                                        // later re-save of the same request.
                                        if let Err(e) = sqlx::query(
                                            "UPDATE support_tickets SET plan_applied_at = NOW() WHERE id = $1",
                                        )
                                        .bind(id)
                                        .execute(&state.db)
                                        .await
                                        {
                                            tracing::warn!(
                                                ticket = %id, error = %e,
                                                "the plan moved but plan_applied_at could not be stamped"
                                            );
                                        }
                                        plan_applied = true;
                                        plan_note = Some(plan_name);
                                        tracing::info!(
                                            ticket = %id, account = %move_account, plan = %ticket_plan,
                                            "plan request resolved: the workspace was moved onto the plan it asked for"
                                        );
                                    }
                                    Err(e) => {
                                        tracing::error!(
                                            ticket = %id, account = %move_account, plan = %ticket_plan,
                                            "plan request resolved, but the plan move failed: {e}"
                                        );
                                        plan_note = Some(format!("the plan move failed: {e}"));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // A status move by the TEAM is an answer, so the customer is told (2026-10-19). A customer's own
    // PUT is not, and an unchanged status is not.
    if is_operator {
        if let Some(new_status) = body.status.as_deref() {
            if new_status != before_status {
                // The mail is the customer's only signal to look, so it carries the plan that
                // moved rather than a bare "resolved" (cron 2026-10-20).
                notify_requester(
                    &state,
                    id,
                    account,
                    new_status,
                    plan_note.as_deref().unwrap_or(""),
                )
                .await;
            }
        }
    }

    // THE RESPONSE MUST CARRY THE STATE THIS CALL JUST CAUSED (cron 2026-10-20). `row` was read
    // BEFORE the plan move above, so a request this very save switched came back as
    // `plan_applied_at: null` with `plan_name: null` — the operator's screen was handed an object
    // that contradicted the green "switched" sentence beside it. One re-read with the SAME join the
    // detail route uses makes the two endpoints answer one shape, and a caller that renders the
    // response instead of re-fetching the list is no longer shown a stale row.
    let ticket = sqlx::query_as::<_, SupportTicket>(
        r#"SELECT t.id, t.tenant_id, t.campaign_id, t.contact_id, t.subject, t.description,
                  t.status, t.priority, t.category, t.source, t.assignee_id, t.created_by,
                  t.created_at, t.updated_at, t.resolved_at, t.plan_slug, t.plan_applied_at,
                  COALESCE(p.name, t.plan_slug) AS plan_name
           FROM support_tickets t
           LEFT JOIN plans p ON p.slug = t.plan_slug
           WHERE t.id = $1"#,
    )
    .bind(id)
    .fetch_one(&state.db)
    .await?;

    Ok(Json(json!({
        "ticket": ticket,
        "plan_applied": plan_applied,
        "plan_note": plan_note,
    })))
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
