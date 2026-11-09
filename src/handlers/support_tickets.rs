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
    let row = sqlx::query_as::<_, (Option<Uuid>, Option<Uuid>, String, String)>(
        "SELECT tenant_id, created_by, subject, requested_by FROM support_tickets WHERE id = $1",
    )
    .bind(ticket_id)
    .fetch_optional(&state.db)
    .await;

    let Ok(Some((tenant_id, created_by, subject, requested_by))) = row else {
        return;
    };
    // The workspace that filed it; `created_by` is the fallback for a ticket opened outside one.
    let Some(target) = tenant_id.or(created_by) else {
        // ── A MESSAGE FROM THE CONTACT PAGE (2026-11-06) ─────────────────────────────────────
        // This row belongs to no workspace and was filed by no account: the address to answer is
        // the one the visitor typed, in `requested_by`. Without this arm the operator's answer
        // would be stored and the visitor told nothing — the exact dead end the contact form
        // exists to close. A reserved/harness recipient is withheld by the send funnel itself, so
        // a probe is never mailed.
        let visitor = requested_by.trim().to_string();
        if visitor.is_empty() {
            return;
        }
        if let Err(e) = crate::email::send_public_support_reply_email(
            &state.db,
            &visitor,
            &ticket_id.to_string(),
            &subject,
            status_word,
            reply,
        )
        .await
        {
            tracing::warn!("public support reply mail to {} failed: {}", visitor, e);
        }
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
                      COALESCE(p.name, t.plan_slug) AS plan_name,
                      COALESCE(a.email, t.requested_by) AS account_email
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
        // A RETIRED tier (`is_active = false`) is not on sale: the billing screen filters it out,
        // so a request naming it asks for a tier the customer cannot even see. MEASURED LIVE
        // 2026-10-21: a request naming a retired row was accepted (200, row stored, receipt mail
        // queued) and the resolve arm below then seated the workspace on it — the same defect the
        // fleet fixed for the operator's assign action (missedcallrespondr 2edd595), still open on
        // this customer path. Refused by name, before the row, like an unknown or free plan.
        let plan: Option<(String, f64, bool)> = sqlx::query_as(
            "SELECT name, COALESCE(NULLIF(price_monthly, 0), price)::float8, is_active \
               FROM plans WHERE slug = $1",
        )
        .bind(slug)
        .fetch_optional(&state.db)
        .await?;
        let (name, price, on_sale) =
            plan.ok_or_else(|| AppError::BadRequest(format!("No plan is called '{slug}'")))?;
        if !on_sale {
            return Err(AppError::BadRequest(format!(
                "{name} is retired, so it cannot be requested - the billing screen lists only the \
                 plans on sale. The team can re-activate it on the Plans table if it is coming back."
            )));
        }
        if price <= 0.0 {
            return Err(AppError::BadRequest(format!(
                "{name} is a free plan - there is nothing to switch to"
            )));
        }
        plan_name = name;
    }

    // A SECOND identical request is not a second request (measured live 2026-10-21). The dashboard's
    // Upgrade button stays pressable after a request is filed, so a customer who pressed it twice
    // left two OPEN `plan_request` rows for the same plan — two requests in the operator's Plan
    // requests view, two receipt mails to one inbox, and a customer list that reads as if the first
    // press had been ignored. Their intent is unchanged, so the EXISTING open request is returned
    // (flagged `already_open`) instead of minting a twin; no second receipt is queued, because the
    // first one already told them truthfully that the request had been received.
    //
    // The key is the TIER ASKED FOR (`plan_slug`), so a workspace with a Pro request open can still
    // ask for Enterprise. Only plan/billing requests are deduped: an ordinary support question may
    // legitimately repeat.
    if matches!(source.as_deref(), Some("plan_request") | Some("billing")) {
        let existing_id: Option<Uuid> = sqlx::query_scalar(
            "SELECT id FROM support_tickets \
             WHERE tenant_id = $1 \
               AND source IN ('plan_request','billing') \
               AND status IN ('open','in_progress') \
               AND plan_slug IS NOT DISTINCT FROM $2 \
             ORDER BY created_at DESC LIMIT 1",
        )
        .bind(account)
        .bind(plan_slug.as_deref())
        .fetch_optional(&state.db)
        .await?;
        if let Some(existing_id) = existing_id {
            let ticket = sqlx::query_as::<_, SupportTicket>(
                r#"SELECT id, tenant_id, campaign_id, contact_id, subject, description, status,
                          priority, category, source, assignee_id, created_by, created_at, updated_at, resolved_at,
                          plan_slug, plan_applied_at
                   FROM support_tickets WHERE id = $1"#,
            )
            .bind(existing_id)
            .fetch_one(&state.db)
            .await?;
            return Ok(Json(json!({
                "acknowledged": false,
                "already_open": true,
                "ticket": ticket,
                "plan_name": plan_name,
            })));
        }
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
    let (filer_email, filer_name) = filer.clone().unwrap_or_default();
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
    // The OPERATOR's half of the same event (2026-11-09). The receipt above tells the FILER; this
    // tells the person who can actually move the row. Best effort: a mail problem must never turn a
    // recorded request into an error, and an unset operator address sends nothing at all.
    if !plan_name.is_empty() {
        let notified =
            crate::email::send_plan_request_operator_alert(&state.db, account, &plan_name).await;
        tracing::info!(ticket = %id, operator_notified = notified, "plan request filed");
    } else {
        // An ORDINARY help request (2026-11-09) — the plan arm above only ever covered a request to
        // buy. "I cannot sign in", "my reports are blank": both wrote a row and told nobody. One
        // alert per filed ticket; the plan arm is exclusive so this can never double-mail.
        let notified = crate::email::send_support_request_operator_alert(
            &state.db,
            &filer_name,
            &filer_email,
            &body.subject,
            body.description.as_deref().unwrap_or(""),
        )
        .await;
        tracing::info!(ticket = %id, operator_notified = notified, "support request filed");
    }
    Ok(Json(json!({
        "acknowledged": acknowledged,
        "ticket": row,
        "plan_name": plan_name,
    })))
}

/// A message from someone who is NOT signed in — the served contact page's form.
///
/// `name` is context for the operator; `email` is the address the team replies to and the one the
/// hourly bound counts. Nothing here identifies a workspace, because a visitor has none.
#[derive(Deserialize)]
pub struct PublicTicketInput {
    #[serde(default)]
    pub name: String,
    pub email: String,
    pub subject: String,
    #[serde(default)]
    pub description: String,
}

/// At most this many messages from one address within an hour. A visitor with no account cannot be
/// trusted to be one visitor, so the bound is on the ADDRESS they type — the only thing the team can
/// reply to.
const PUBLIC_HOURLY_LIMIT: i64 = 3;

/// `POST /api/v1/public/support-request` — file a help request from the contact page.
///
/// WHY THIS EXISTS (measured 2026-11-06 on dns.google and cloudflare-dns.com): every support
/// address this product printed — the login page, the 404 page, the marketing pages and the
/// transactional footers — lived on a domain publishing **MX = 0**, and nothing on this host listens
/// on port 25. A locked-out customer's "write to support@…" message therefore BOUNCED, and they had
/// no way back in. This arm files the same kind of row the signed-in arm writes, attached to no
/// workspace and filed by no account, so it lands in the operator queue the team already answers.
///
/// Deliberately narrow: no workspace is created, nothing is mailed at filing time, no plan moves.
/// The row IS the effect. A double press with the same subject while the first is still open returns
/// that row instead of minting a second one for the operator to reconcile.
pub async fn create_public_ticket(
    State(state): State<AppState>,
    Json(body): Json<PublicTicketInput>,
) -> Result<(axum::http::StatusCode, Json<Value>), AppError> {
    let email = body.email.trim().to_lowercase();
    let at = email.find('@').unwrap_or(0);
    if at == 0
        || at == email.len() - 1
        || email.len() > 254
        || email.chars().any(|c| c.is_whitespace() || c.is_control())
        || !email[at + 1..].contains('.')
    {
        return Err(AppError::BadRequest(
            "Enter an email address we can reply to.".to_string(),
        ));
    }

    let subject: String = body
        .subject
        .trim()
        .chars()
        .take(200)
        .collect::<String>()
        .trim()
        .to_string();
    if subject.is_empty() {
        return Err(AppError::BadRequest(
            "Give your message a short subject so the team knows what it is about.".to_string(),
        ));
    }
    let message: String = body
        .description
        .trim()
        .chars()
        .take(4000)
        .collect::<String>()
        .trim()
        .to_string();
    if message.is_empty() {
        return Err(AppError::BadRequest(
            "Write your message so the team knows what you need.".to_string(),
        ));
    }
    let name: String = body.name.trim().chars().take(120).collect();
    let description = if name.trim().is_empty() {
        message
    } else {
        format!("{} wrote:\n\n{}", name.trim(), message)
    };

    let recent: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM support_tickets \
         WHERE lower(requested_by) = $1 AND created_at > now() - interval '1 hour'",
    )
    .bind(&email)
    .fetch_one(&state.db)
    .await?;
    if recent >= PUBLIC_HOURLY_LIMIT {
        // Its OWN sentence: `AppError::RateLimited` is the promo-code/guess throttle's copy, which
        // would tell a visitor who never guessed anything something plainly untrue.
        return Ok((
            axum::http::StatusCode::TOO_MANY_REQUESTS,
            Json(json!({
                "status": 429,
                "error": "You have sent us several messages in the last hour. Please wait a little, or reply to the email we already have from you.",
                "retry_after_seconds": 3600
            })),
        ));
    }

    let existing: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM support_tickets \
          WHERE tenant_id IS NULL AND lower(requested_by) = $1 \
            AND lower(subject) = lower($2) AND status IN ('open', 'in_progress') \
          ORDER BY created_at DESC LIMIT 1",
    )
    .bind(&email)
    .bind(&subject)
    .fetch_optional(&state.db)
    .await?;
    if let Some(id) = existing {
        return Ok((
            axum::http::StatusCode::OK,
            Json(json!({
                "id": id,
                "status": "existing",
                "message": "We already have your message with that subject — the team will reply to the address you gave."
            })),
        ));
    }

    let id = Uuid::new_v4();
    sqlx::query(
        r#"INSERT INTO support_tickets
             (id, tenant_id, subject, description, status, priority, category, source, created_by, requested_by)
           VALUES ($1, NULL, $2, $3, 'open', 'normal', 'other', 'contact_form', NULL, $4)"#,
    )
    .bind(id)
    .bind(&subject)
    .bind(&description)
    .bind(&email)
    .execute(&state.db)
    .await?;

    // TELL THE OPERATOR (2026-11-09). The row above is the record; this is the notification. Until
    // it existed a locked-out customer's only way back in was a console nobody had been told to
    // open. Best effort: the row is already written, so a mail failure is logged and the 201 kept.
    let notified = crate::email::send_support_request_operator_alert(
        &state.db,
        body.name.trim(),
        &email,
        &subject,
        &description,
    )
    .await;
    tracing::info!(ticket = %id, operator_notified = notified, "public support request filed");

    Ok((
        axum::http::StatusCode::CREATED,
        Json(json!({
            "id": id,
            "status": "open",
            "message": "Your message is with our team. We will reply to the address you gave."
        })),
    ))
}

/// `POST /api/v1/public/support-answer` — read the team's answer to a message filed from the
/// contact page (2026-11-06).
///
/// WHY THIS EXISTS. A visitor who filed from `contact.html` is not signed in and has no workspace,
/// so there is no Support screen they could ever load: the answer an operator writes was stored on
/// a row nobody who asked could open. Mail is one channel and mail gets lost (spam folder, a
/// mistyped address), so the answer needs a second door. ADASwift, WorkflowSwift and FunnelSwift
/// already ship this read-back; this app mailed and could not show.
///
/// The reference is an unguessable uuid and the address is the one they filed with, so the PAIR is
/// the credential. Either one alone answers the same neutral 404, which keeps the route from
/// confirming that a reference exists. Narrow by construction: ONE row, only when it carries no
/// workspace (a workspace ticket is read by its own signed-in users), returning the subject, the
/// status and the team's visible answers — never the description, never a list of tickets, never
/// another visitor's row.
#[derive(Debug, Deserialize)]
pub struct PublicAnswerInput {
    pub ticket_id: String,
    pub email: String,
}

pub async fn public_answer(
    State(state): State<AppState>,
    Json(body): Json<PublicAnswerInput>,
) -> Result<Json<Value>, AppError> {
    // Neutral by design: an unknown reference, a malformed one and a reference that belongs to a
    // different address all read the same, so the route cannot be used to test whether a reference
    // is real.
    let missing = || {
        AppError::NotFound(
            "We have no message from that address with that reference. Check the reference and the \
             email address you filed with."
                .to_string(),
        )
    };
    let id = Uuid::parse_str(body.ticket_id.trim()).map_err(|_| missing())?;
    // The filing arm stores the address lower-cased, so the lookup normalises the same way —
    // `Probe@Example.com ` must find the row filed as `probe@example.com`.
    let email = body.email.trim().to_lowercase();
    if email.is_empty() || !email.contains('@') {
        return Err(missing());
    }
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT subject, status FROM support_tickets \
          WHERE id = $1 AND tenant_id IS NULL AND lower(requested_by) = $2",
    )
    .bind(id)
    .bind(&email)
    .fetch_optional(&state.db)
    .await?;
    let Some((subject, status)) = row else {
        return Err(missing());
    };
    // The team's answers live in the thread. `is_internal = false` is the SAME filter the
    // signed-in customer read applies: a private note is never delivered to the browser of the
    // person who asked. A contact-form row has no customer user at all (`created_by IS NULL`), so
    // every visible message on it is the team's.
    let replies: Vec<(String, String)> = sqlx::query_as(
        "SELECT body, created_at::text FROM support_ticket_messages \
          WHERE ticket_id = $1 AND is_internal = false ORDER BY created_at ASC",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(json!({
        "id": id.to_string(),
        "subject": subject,
        "status": status,
        // An empty list means "not answered yet", which the page says in its own words rather
        // than showing an empty green box.
        "replies": replies
            .iter()
            .map(|(b, t)| json!({ "body": b, "sent_at": t }))
            .collect::<Vec<_>>(),
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
                    let target: Option<(Uuid, String, bool)> =
                        sqlx::query_as("SELECT id, name, is_active FROM plans WHERE slug = $1")
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
                        Some((_, ref retired_name, false)) => {
                            tracing::warn!(
                                ticket = %id, plan = %ticket_plan,
                                "plan request resolved, but the plan is retired (is_active = false) - the plan did NOT move"
                            );
                            plan_note = Some(format!(
                                "{retired_name} is retired, so the workspace was not moved onto it"
                            ));
                        }
                        Some((plan_id, plan_name, true)) => {
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
///
/// TWO ARMS, decided by the caller's role (cron 2026-10-27).
///
/// * The OPERATOR arm is the console's Delete: it removes whichever row the console is looking at,
///   exactly as before.
/// * The CUSTOMER arm is a WITHDRAWAL. Pressing Upgrade on the Plan & billing screen files a plan
///   request and tells the buyer "our team will switch this workspace"; until now there was no way to
///   take that ask back, so a request filed by a mis-click (or one the workspace no longer wants) sat
///   in the team's queue for ever while the customer's own card kept saying a switch was coming. A
///   customer may therefore withdraw ONLY their OWN plan request, and only while it is still open: an
///   ordinary ticket is the message they sent us (kept), and a request whose plan has already been
///   applied is the record of that switch (kept). Each refusal is a 400 carrying a sentence, and an id
///   belonging to another workspace is a 404 — never another workspace's row.
pub async fn delete_ticket(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, AppError> {
    let account = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;
    let is_operator = caller_is_operator(&state, account).await;

    if !is_operator {
        let row: Option<(
            String,
            Option<String>,
            Option<chrono::DateTime<chrono::Utc>>,
        )> = sqlx::query_as(
            "SELECT status, plan_slug, plan_applied_at FROM support_tickets
                 WHERE id = $1 AND (tenant_id = $2 OR created_by = $2)",
        )
        .bind(id)
        .bind(account)
        .fetch_optional(&state.db)
        .await?;
        let (status, plan_slug, applied) = row.ok_or_else(|| {
            AppError::NotFound("That request was not found on this workspace".to_string())
        })?;
        if plan_slug.as_deref().unwrap_or("").trim().is_empty() {
            return Err(AppError::BadRequest(
                "Only a plan request can be withdrawn. Send us a message and we will pick it up"
                    .to_string(),
            ));
        }
        if applied.is_some() {
            return Err(AppError::BadRequest(
                "This request has already been completed, so there is nothing to withdraw"
                    .to_string(),
            ));
        }
        let st = status.to_ascii_lowercase();
        if st == "closed" || st == "resolved" {
            return Err(AppError::BadRequest(
                "This request is already closed, so there is nothing to withdraw".to_string(),
            ));
        }
    }

    // The tenant scope is repeated in the WRITE, so a row that changed hands between the read above
    // and this delete cannot be removed by this call. A zero-row delete is a 404 rather than the old
    // unconditional `{"deleted": true}`, which told the caller a row was gone when nothing matched.
    let r = sqlx::query(
        "DELETE FROM support_tickets
         WHERE id = $1 AND (tenant_id = $2 OR created_by = $2 OR $3)",
    )
    .bind(id)
    .bind(account)
    .bind(is_operator)
    .execute(&state.db)
    .await?;
    if r.rows_affected() == 0 {
        return Err(AppError::NotFound(
            "That request was not found on this workspace".to_string(),
        ));
    }
    Ok(Json(json!({ "deleted": true, "withdrawn": !is_operator })))
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

/// Record the paid tier a visitor PICKED on the public pricing page, at the moment they sign up.
///
/// The pricing cards already carry the tier into `/signup?plan=pro`, and that page told the visitor
/// "You picked Pro" — then posted `{name,email}` and dropped the pick, so the account landed on Free
/// and nothing in the operator's console knew a paid tier had been asked for. Signup is the only
/// anonymous door that mints a workspace, so it is the one place the pick can be kept.
///
/// The row is the SAME `support_tickets` shape the dashboard's "Request the plan" button files
/// (`source='plan_request'` + `plan_slug`, deduped on one open request per tier) — which is what the
/// operator's Plan requests view reads and what its Resolve action switches the workspace with. So a
/// signup pick is workable, not a line in a log. Best-effort by design: an unknown, retired or free
/// slug returns `None` and the signup still succeeds — a preference must never refuse an account.
pub async fn record_signup_plan_request(
    db: &sqlx::PgPool,
    account_id: Uuid,
    picked: &str,
) -> Result<Option<(String, String)>, AppError> {
    let slug: String = picked
        .trim()
        .to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    if slug.is_empty() || slug.len() > 60 {
        return Ok(None);
    }
    // The same price expression the ticket door and checkout use: `price_monthly` holds 0 for the
    // paid tiers on this install (measured live 2026-10-20), the money is in the legacy `price`.
    let plan: Option<(String, f64, bool)> = sqlx::query_as(
        "SELECT name, COALESCE(NULLIF(price_monthly, 0), price)::float8, is_active \
           FROM plans WHERE slug = $1",
    )
    .bind(&slug)
    .fetch_optional(db)
    .await?;
    let Some((name, price, on_sale)) = plan else {
        return Ok(None);
    };
    if !on_sale || price <= 0.0 {
        return Ok(None);
    }
    let existing: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM support_tickets \
          WHERE tenant_id = $1 AND source = 'plan_request' \
            AND status IN ('open','in_progress') \
            AND plan_slug IS NOT DISTINCT FROM $2 \
          ORDER BY created_at DESC LIMIT 1",
    )
    .bind(account_id)
    .bind(&slug)
    .fetch_optional(db)
    .await?;
    if existing.is_some() {
        return Ok(Some((slug, name)));
    }
    sqlx::query(
        r#"INSERT INTO support_tickets
             (id, tenant_id, campaign_id, contact_id, subject, description, status, priority,
              category, source, created_by, plan_slug)
           VALUES ($1,$2,NULL,NULL,$3,$4,'open','normal','billing','plan_request',$2,$5)"#,
    )
    .bind(Uuid::new_v4())
    .bind(account_id)
    .bind(format!("Plan request at signup: {name} plan"))
    .bind(format!(
        "This workspace picked the {name} plan (${price:.0}/month) on the pricing page before it \
         signed up. New accounts start on Free and no card was taken, so the switch has to be made \
         by the team. Filed automatically by the signup door."
    ))
    .bind(&slug)
    .execute(db)
    .await?;
    // The OPERATOR is told too (2026-11-09): a pick made on the pricing page is the same buying
    // intent as the dashboard's Upgrade button, and the row alone waits for someone to open the
    // console. Best effort — the account already exists and nothing here may fail the signup.
    let notified = crate::email::send_plan_request_operator_alert(db, account_id, &name).await;
    tracing::info!(
        account = %account_id,
        operator_notified = notified,
        "signup plan request filed"
    );
    Ok(Some((slug, name)))
}
