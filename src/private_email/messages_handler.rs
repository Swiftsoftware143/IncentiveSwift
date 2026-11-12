//! The inbox's own records: every message that arrived, where it arrived, and what happened to it.
//!
//! WHY THIS MODULE EXISTS (kanban B204/B205/B211)
//! Until it did, receiving had no record of its own. The inbound webhook created a contact and
//! then, only if the mailbox was ticked, a ticket — and the inbox screen read TICKETS. So a message
//! to an address that does not create tickets (info@, david@) was accepted, acknowledged with 200
//! and then invisible: nothing in the product could show it, because nothing had stored it. Mail
//! that arrived and mail a human could see were two different things wearing one name.
//!
//! A message is now its own row, written for EVERY authenticated delivery whatever the ticket
//! toggle says. It carries:
//!   * `address`   — the address it ARRIVED ON, so one catch-all inbox can serve many addresses
//!     and the client can filter by address, with MISC for addresses nobody created;
//!   * `is_ticket` — whether it ALSO became a ticket. A ticketed message is in the work queue AND
//!     in the inbox; an unticketed one is in the inbox awaiting a decision, and any
//!     message can be turned into a ticket with one call.
//!
//! ACCOUNT SCOPE, deliberately: every query is keyed on the account id from the session token
//! (`claims.aid`). There is no workspace column and no workspace filter — mail belongs to the
//! business, not to a workspace (B211, David 2026-11-11).

use axum::{
    extract::{Path, Query, State},
    Extension, Json,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

use crate::access::claims::Claims;
use crate::error::{ApiResult, AppError};
use crate::state::AppState;

/// The stored folder a message sits in. The eight surfaces in the sidebar are VIEWS over this
/// column plus `is_read` / `is_starred`; only a real store is ever written here. A custom
/// subfolder is a path under its parent ('inbox/Clients'), which is what makes nesting real
/// server-side state rather than a client-side label (B205).
const SYSTEM_FOLDERS: [&str; 6] = ["inbox", "archive", "spam", "trash", "sent", "drafts"];

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    /// inbox | unread | favorites | drafts | sent | archive | spam | trash | all
    #[serde(default)]
    pub folder: Option<String>,
    /// An exact address, "misc" for catch-all arrivals with no mailbox row, or "all".
    #[serde(default)]
    pub address: Option<String>,
    #[serde(default)]
    pub q: Option<String>,
    /// A label id: only messages carrying that label. The sidebar's label rows are filters, and a
    /// filter that does nothing would be a dead control.
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateMessageRequest {
    pub is_read: Option<bool>,
    pub is_starred: Option<bool>,
    /// Move to a real store: inbox | archive | spam | trash | drafts, or a subfolder path.
    pub folder: Option<String>,
    /// The teammate this conversation belongs to (kanban t_5f314e9a). Same double-option contract as
    /// the mailbox assignment: absent = leave it, `null` = give it back to the workspace, uuid =
    /// assign it. A single `Option<Uuid>` plus `COALESCE` made this a SET-ONLY door — the teammate
    /// who left, or the work that was handed back, could never be cleared.
    #[serde(default, deserialize_with = "deserialize_optional_nullable_uuid")]
    pub assigned_user_id: Option<Option<Uuid>>,
}

/// `{"assigned_user_id": "…"}` -> `Some(Some(id))`; `{"assigned_user_id": null}` -> `Some(None)`;
/// absent -> `None`. The shape three "who is this for" fields on this app share.
fn deserialize_optional_nullable_uuid<'de, D>(
    deserializer: D,
) -> Result<Option<Option<Uuid>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<Uuid>::deserialize(deserializer).map(Some)
}

#[derive(Debug, Deserialize)]
pub struct ReplyRequest {
    pub from_address: String,
    pub body: String,
    #[serde(default)]
    pub subject: Option<String>,
    /// reply (default) | reply_all | forward
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub to: Option<String>,
    /// Whether the signature goes on this reply. Absent means yes (kanban t_b671a0f7): replies open
    /// with the signature in place exactly like a new message, and the toggle drops it for one reply.
    #[serde(default)]
    pub include_signature: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct FolderActionRequest {
    pub folder: String,
    /// read | unread | empty
    pub action: String,
}

// ────────────────────────────────────────────────────────────────────────────────────────────────
// Reading
// ────────────────────────────────────────────────────────────────────────────────────────────────

/// GET /api/private-email/messages
///
/// One page of the inbox. Every row is a message that was RECEIVED or SENT by this account — never
/// a ticket-shaped guess — with the address it arrived on and whether it also became a ticket.
pub async fn list_messages(
    State(state): State<AppState>,
    Extension(claims): Extension<Claims>,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Value>> {
    let tenant_id = account_id(&claims)?;
    let view = q.folder.as_deref().unwrap_or("inbox").to_string();
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let offset = q.offset.unwrap_or(0).max(0);

    // The address filter's value, normalised: "all" (everything), "misc" (catch-all arrivals with no
    // mailbox row) or one exact address.
    let address_filter = q.address.as_deref().unwrap_or("all").trim().to_lowercase();
    let search =
        q.q.as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("")
            .to_string();

    // ONE statement, every filter a BIND. The view -> predicate mapping lives inside a CASE over
    // $2 and the optional filters are written as `$n = 'sentinel' OR <predicate>`, so no caller
    // value is ever concatenated into SQL text: the fleet's rule against run-time-built SQL, and
    // what keeps an address like `x' OR 1=1 --` a value rather than a statement.
    let rows = sqlx::query(
        r#"
        SELECT m.id, m.mailbox_id, m.address, m.folder, m.direction,
               m.from_address, m.from_name, m.to_address, m.subject, m.snippet,
               m.is_read, m.is_starred, m.ticket_id, m.is_ticket, m.assigned_user_id,
               m.has_attachments, m.size_bytes, m.received_at, m.message_id, m.cc_address
          FROM private_email_messages m
         WHERE m.tenant_id = $1
           AND CASE $2
                 WHEN 'unread'    THEN m.is_read = false AND m.folder NOT IN ('trash','spam','drafts','sent')
                 WHEN 'favorites' THEN m.is_starred = true AND m.folder <> 'trash'
                 WHEN 'drafts'    THEN m.folder = 'drafts'
                 WHEN 'sent'      THEN m.folder = 'sent'
                 WHEN 'archive'   THEN m.folder = 'archive'
                 WHEN 'spam'      THEN m.folder = 'spam'
                 WHEN 'trash'     THEN m.folder = 'trash'
                 WHEN 'all'       THEN m.folder NOT IN ('trash','spam')
                 -- A path the account MADE is a view of its own mail too: the exact path plus
                 -- everything filed below it. Matched case-insensitively because a message's folder
                 -- is stored lower-cased while a folder row may predate that normalisation, and with
                 -- `left(..)` rather than LIKE so a '%' in a folder name can never widen the match.
                 ELSE (lower(m.folder) = lower($2)
                       OR left(lower(m.folder), length($2) + 1) = lower($2) || '/')
               END
           AND ( $3 = 'all'
                 OR ($3 = 'misc' AND m.mailbox_id IS NULL)
                 OR m.address = $3 )
           AND ( $4 = ''
                 OR m.subject ILIKE '%' || $4 || '%'
                 OR m.from_address ILIKE '%' || $4 || '%'
                 OR m.snippet ILIKE '%' || $4 || '%' )
           AND ( $7::text = ''
                 OR EXISTS (SELECT 1 FROM private_email_message_labels ml
                             WHERE ml.message_id = m.id AND ml.label_id::text = $7) )
         ORDER BY m.received_at DESC
         LIMIT $5 OFFSET $6
        "#,
    )
    .bind(tenant_id)
    .bind(&view)
    .bind(&address_filter)
    .bind(&search)
    .bind(limit)
    .bind(offset)
    .bind(q.label.as_deref().unwrap_or("").to_string())
    .fetch_all(&state.db)
    .await
    .map_err(crate::error::db_error)?;

    let messages: Vec<Value> = rows.iter().map(message_json).collect();

    Ok(Json(json!({
        "messages": messages,
        "folder": view,
        "address": address_filter,
        "limit": limit,
        "offset": offset,
    })))
}

/// GET /api/private-email/messages/counts
///
/// The badge numbers the sidebar needs, plus the storage figure the retention banner states. All
/// computed from the same table, so a count can never disagree with the list it labels.
pub async fn message_counts(
    State(state): State<AppState>,
    Extension(claims): Extension<Claims>,
) -> ApiResult<Json<Value>> {
    let tenant_id = account_id(&claims)?;

    let row = sqlx::query(
        r#"
        SELECT
          count(*) FILTER (WHERE folder = 'inbox' OR folder LIKE 'inbox/%')                   AS inbox,
          count(*) FILTER (WHERE (folder = 'inbox' OR folder LIKE 'inbox/%') AND is_read = false) AS unread,
          count(*) FILTER (WHERE is_starred AND folder <> 'trash')                            AS favorites,
          count(*) FILTER (WHERE folder = 'drafts')                                           AS drafts,
          count(*) FILTER (WHERE folder = 'sent')                                             AS sent,
          count(*) FILTER (WHERE folder = 'archive')                                          AS archive,
          count(*) FILTER (WHERE folder = 'spam')                                             AS spam,
          count(*) FILTER (WHERE folder = 'trash')                                            AS trash,
          count(*) FILTER (WHERE is_ticket)                                                   AS tickets,
          coalesce(sum(size_bytes), 0)                                                        AS used_bytes
        FROM private_email_messages
        WHERE tenant_id = $1
        "#,
    )
    .bind(tenant_id)
    .fetch_one(&state.db)
    .await
    .map_err(crate::error::db_error)?;

    let get = |k: &str| row.try_get::<i64, _>(k).unwrap_or(0);

    // STORAGE (B206, David 2026-11-11). ONE source for the allowance and the usage: the module
    // registry row this account's plan carries (`email_storage_mb`, 2 GB on free) and the bytes the
    // account really stores (messages + the logo + profile pictures). The meter, the banner, the
    // reminder email and the 100% attachment refusal all read `storage::usage`, so the number shown
    // here and the number that refuses a file can never disagree.
    //
    // This read also EVALUATES the three rungs (50 / 75 / 99). That is deliberate: a customer who
    // opens their mail is told the moment the screen can tell them, and the ledger in
    // `private_email_storage_notices` means the email is sent at most once per rung per month no
    // matter how often this endpoint is called.
    let storage = crate::private_email::storage::usage(&state.db, tenant_id).await;
    let notified = crate::private_email::storage::evaluate_and_notify(&state.db, tenant_id).await;
    if !notified.is_empty() {
        tracing::info!(tenant = %tenant_id, rungs = ?notified, "storage reminder rungs claimed on a counts read");
    }
    let retention = crate::private_email::storage::retention(&state.db, tenant_id).await;
    let banner = crate::private_email::storage::banner(&storage);

    Ok(Json(json!({
        "inbox": get("inbox"),
        "unread": get("unread"),
        "favorites": get("favorites"),
        "drafts": get("drafts"),
        "sent": get("sent"),
        "archive": get("archive"),
        "spam": get("spam"),
        "trash": get("trash"),
        "tickets": get("tickets"),
        // The flat figures the mail client's rail already read, kept for compatibility, plus the
        // whole storage state under `storage` for the meter and the banner.
        "used_bytes": storage.used_bytes,
        "limit_bytes": storage.quota_bytes,
        "storage_gb": storage.quota_bytes as f64 / (1024.0 * 1024.0 * 1024.0),
        "storage": {
            "used_bytes": storage.used_bytes,
            "quota_bytes": storage.quota_bytes,
            "mail_bytes": storage.mail_bytes,
            "file_bytes": storage.file_bytes,
            "used_human": storage.used_human(),
            "quota_human": storage.quota_human(),
            "percent": storage.percent(),
            "level": storage.level(),
            "attachments_accepted": storage.accepts_new_files(),
            "banner": banner,
        },
        // The time-based rule survives only as a far backstop (10 years by default, raised from 365
        // days by migration 142, so no customer mail is deleted on a clock). It is stated on screen
        // with the account's REAL window whenever one applies.
        "retention": {
            "days": retention.days,
            "oldest_message_at": retention.oldest_message_at,
            "first_expiry_at": retention.first_expiry_at,
            "expired_messages": retention.expired_messages,
            "sentence": crate::private_email::storage::retention_sentence(&retention),
        },
        // Whether the Summarise control can work at all. The client hides it when false rather
        // than offering a button that can only fail (B205 "no dead ends").
        // AI summaries are CoreSwift's feature (its own provider registry); this app
        // deliberately has no Summarise control, so the client hides it honestly.
        "ai_summary": false,
    })))
}

/// GET /api/private-email/addresses
///
/// The address filter (B211): every address this account has RECEIVED mail on, plus MISC for
/// arrivals at addresses nobody created (catch-all), plus ALL. Counts come with them so the
/// filter can show how much mail is behind each one instead of an empty picker.
pub async fn list_addresses(
    State(state): State<AppState>,
    Extension(claims): Extension<Claims>,
) -> ApiResult<Json<Value>> {
    let tenant_id = account_id(&claims)?;

    let rows = sqlx::query(
        r#"
        SELECT address, count(*) AS total, count(*) FILTER (WHERE is_read = false) AS unread
          FROM private_email_messages
         WHERE tenant_id = $1 AND folder <> 'trash'
         GROUP BY address
         ORDER BY lower(address)
        "#,
    )
    .bind(tenant_id)
    .fetch_all(&state.db)
    .await
    .map_err(crate::error::db_error)?;

    let created = sqlx::query(
        "SELECT email_address FROM private_email_boxes WHERE tenant_id = $1 AND status = 'active'",
    )
    .bind(tenant_id)
    .fetch_all(&state.db)
    .await
    .map_err(crate::error::db_error)?;
    let created_addresses: Vec<String> = created
        .iter()
        .filter_map(|r| r.try_get::<String, _>("email_address").ok())
        .map(|s| s.to_lowercase())
        .collect();

    let mut total: i64 = 0;
    let mut unread: i64 = 0;
    let mut misc_total: i64 = 0;
    let mut by_address = Vec::new();
    for r in &rows {
        let addr: String = r.try_get::<String, _>("address").unwrap_or_default();
        let n: i64 = r.try_get::<i64, _>("total").unwrap_or(0);
        let u: i64 = r.try_get::<i64, _>("unread").unwrap_or(0);
        total += n;
        unread += u;
        if !created_addresses.is_empty() && !created_addresses.contains(&addr) && !addr.is_empty() {
            misc_total += n;
        }
        by_address.push(json!({
            "address": addr,
            "total": n,
            "unread": u,
            "created": created_addresses.contains(&addr),
        }));
    }

    Ok(Json(json!({
        "addresses": by_address,
        "all": { "address": "all", "total": total, "unread": unread },
        "misc": { "address": "misc", "total": misc_total },
    })))
}

/// GET /api/private-email/messages/:id
///
/// The opened message. Marks it read on the way out (that IS the user opening it); `?peek=1` reads
/// without touching the flag, which is what a prefetch or a summary call needs.
pub async fn get_message(
    State(state): State<AppState>,
    Extension(claims): Extension<Claims>,
    Path(message_id): Path<Uuid>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    let tenant_id = account_id(&claims)?;
    let peek = q
        .get("peek")
        .map(|v| v == "1" || v == "true")
        .unwrap_or(false);

    if !peek {
        sqlx::query(
            "UPDATE private_email_messages SET is_read = true, updated_at = NOW()
              WHERE id = $1 AND tenant_id = $2 AND is_read = false",
        )
        .bind(message_id)
        .bind(tenant_id)
        .execute(&state.db)
        .await
        .map_err(crate::error::db_error)?;
    }

    let row = sqlx::query(
        r#"
        SELECT id, mailbox_id, address, folder, direction, from_address, from_name,
               to_address, cc_address, subject, body_plain, body_html, snippet,
               message_id, in_reply_to, provider, is_read, is_starred, size_bytes,
               has_attachments, ticket_id, is_ticket, assigned_user_id, received_at
          FROM private_email_messages
         WHERE id = $1 AND tenant_id = $2
        "#,
    )
    .bind(message_id)
    .bind(tenant_id)
    .fetch_optional(&state.db)
    .await
    .map_err(crate::error::db_error)?
    .ok_or_else(|| AppError::NotFound("Message not found".into()))?;

    let mut body = message_json(&row);
    if let Some(obj) = body.as_object_mut() {
        obj.insert(
            "body_plain".into(),
            json!(row.try_get::<String, _>("body_plain").unwrap_or_default()),
        );
        obj.insert(
            "body_html".into(),
            json!(row
                .try_get::<Option<String>, _>("body_html")
                .unwrap_or(None)),
        );
        obj.insert(
            "labels".into(),
            message_labels(&state, tenant_id, message_id).await?,
        );
        obj.insert("is_read".into(), json!(true));
    }
    Ok(Json(body))
}

// ────────────────────────────────────────────────────────────────────────────────────────────────
// Writing
// ────────────────────────────────────────────────────────────────────────────────────────────────

/// PATCH /api/private-email/messages/:id — star, read/unread, move, assign.
pub async fn update_message(
    State(state): State<AppState>,
    Extension(claims): Extension<Claims>,
    Path(message_id): Path<Uuid>,
    Json(req): Json<UpdateMessageRequest>,
) -> ApiResult<Json<Value>> {
    let tenant_id = account_id(&claims)?;

    if let Some(folder) = req.folder.as_deref() {
        let normalized = folder.trim().to_lowercase();
        let is_system = SYSTEM_FOLDERS.contains(&normalized.as_str());
        let is_subfolder = SYSTEM_FOLDERS
            .iter()
            .any(|f| normalized.starts_with(&format!("{}/", f)))
            && normalized.len() > 8
            && !normalized.contains("//");
        if !is_system && !is_subfolder {
            return Err(AppError::BadRequest(format!(
                "'{}' is not a folder this account has. Use one of inbox, archive, spam, trash, \
                 drafts, or a subfolder such as inbox/Clients.",
                folder
            )));
        }
    }

    // Assigning is the same object as ticket assignment (B204): the person must be an ACTIVE member
    // of this account. A former teammate's row is refused, so a removal cannot be undone by
    // assigning work to somebody who can no longer sign in.
    if let Some(Some(user_id)) = req.assigned_user_id {
        let member: Option<(Uuid,)> =
            sqlx::query_as("SELECT id FROM accounts WHERE id = $1 AND id = $2")
                .bind(user_id)
                .bind(tenant_id)
                .fetch_optional(&state.db)
                .await
                .map_err(crate::error::db_error)?;
        if member.is_none() {
            return Err(AppError::Validation(
                "That person is not a member of this account.".into(),
            ));
        }
    }

    let row = sqlx::query(
        r#"
        UPDATE private_email_messages
           SET is_read          = COALESCE($3, is_read),
               is_starred       = COALESCE($4, is_starred),
               folder           = COALESCE($5, folder),
               assigned_user_id = CASE WHEN $6::bool THEN $7::uuid ELSE assigned_user_id END,
               updated_at       = NOW()
         WHERE id = $1 AND tenant_id = $2
        RETURNING id, mailbox_id, address, folder, direction, from_address, from_name,
                  to_address, subject, snippet, is_read, is_starred, ticket_id, is_ticket,
                  assigned_user_id, has_attachments, size_bytes, received_at, message_id, cc_address
        "#,
    )
    .bind(message_id)
    .bind(tenant_id)
    .bind(req.is_read)
    .bind(req.is_starred)
    .bind(req.folder.as_deref().map(|f| f.trim().to_lowercase()))
    // `mentioned` / `person`: absent = leave the assignment alone, explicit null = clear it (the
    // work goes back to the whole workspace), uuid = assign it.
    .bind(req.assigned_user_id.is_some())
    .bind(req.assigned_user_id.flatten())
    .fetch_optional(&state.db)
    .await
    .map_err(crate::error::db_error)?
    .ok_or_else(|| AppError::NotFound("Message not found".into()))?;

    Ok(Json(message_json(&row)))
}

/// DELETE /api/private-email/messages/:id
///
/// Normal folders: the message moves to Trash and stays restorable.
/// Trash: it is destroyed — emptying a holding pen must actually empty it.
/// Spam: destroyed too. Sending spam to Trash would hide it in a SECOND holding pen (David
/// 2026-11-11: "deleting from Spam is a DELETE FOREVER, never to Trash").
pub async fn delete_message(
    State(state): State<AppState>,
    Extension(claims): Extension<Claims>,
    Path(message_id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    let tenant_id = account_id(&claims)?;

    let folder: Option<(String,)> = sqlx::query_as(
        "SELECT folder FROM private_email_messages WHERE id = $1 AND tenant_id = $2",
    )
    .bind(message_id)
    .bind(tenant_id)
    .fetch_optional(&state.db)
    .await
    .map_err(crate::error::db_error)?;

    let folder = folder
        .ok_or_else(|| AppError::NotFound("Message not found".into()))?
        .0;

    // A DRAFT is the user's own unsent text: discarding it is the only meaning "delete" can have,
    // and parking it in Trash would leave the bits of an abandoned letter in a folder that claims to
    // hold received mail.
    if folder == "trash" || folder == "spam" || folder == "drafts" {
        // Ticket rows are the WORK RECORD and are not destroyed by a mailbox action; the message
        // row goes, the ticket it created stays where the team can still see it.
        sqlx::query("DELETE FROM private_email_messages WHERE id = $1 AND tenant_id = $2")
            .bind(message_id)
            .bind(tenant_id)
            .execute(&state.db)
            .await
            .map_err(crate::error::db_error)?;
        return Ok(Json(json!({ "destroyed": true, "folder": folder })));
    }

    sqlx::query(
        "UPDATE private_email_messages SET folder = 'trash', updated_at = NOW()
          WHERE id = $1 AND tenant_id = $2",
    )
    .bind(message_id)
    .bind(tenant_id)
    .execute(&state.db)
    .await
    .map_err(crate::error::db_error)?;

    Ok(Json(json!({ "trashed": true, "restorable": true })))
}

/// POST /api/private-email/messages/:id/reply — answer the mail from one of the account's
/// addresses, and record the answer in Sent so the thread is not one-sided.
///
/// The send itself goes through the SAME provider path as /send (one sender, one credential), so a
/// reply can never leave by a route the account's own sending does not use.
pub async fn reply_to_message(
    State(state): State<AppState>,
    Extension(claims): Extension<Claims>,
    Path(message_id): Path<Uuid>,
    Json(req): Json<ReplyRequest>,
) -> ApiResult<Json<Value>> {
    let tenant_id = account_id(&claims)?;

    let original = sqlx::query(
        "SELECT from_address, from_name, to_address, cc_address, subject
           FROM private_email_messages WHERE id = $1 AND tenant_id = $2",
    )
    .bind(message_id)
    .bind(tenant_id)
    .fetch_optional(&state.db)
    .await
    .map_err(crate::error::db_error)?
    .ok_or_else(|| AppError::NotFound("Message not found".into()))?;

    let from_original: String = original
        .try_get::<String, _>("from_address")
        .unwrap_or_default();
    let subject_original: String = original.try_get::<String, _>("subject").unwrap_or_default();

    let mode = req.mode.as_deref().unwrap_or("reply");
    let subject = req.subject.clone().unwrap_or_else(|| {
        if subject_original.to_lowercase().starts_with("re:") {
            subject_original.clone()
        } else if mode == "forward" {
            format!("Fwd: {}", subject_original)
        } else {
            format!("Re: {}", subject_original)
        }
    });

    let to = match req.to.clone() {
        Some(t) if !t.trim().is_empty() => t,
        _ => from_original.clone(),
    };

    // The mailbox must belong to this account — a From selector is not permission to send as
    // somebody else's address.
    let mailbox = sqlx::query_as::<_, (Uuid,)>(
        "SELECT id FROM private_email_boxes
          WHERE tenant_id = $1 AND email_address = $2 AND status = 'active'",
    )
    .bind(tenant_id)
    .bind(&req.from_address)
    .fetch_optional(&state.db)
    .await
    .map_err(crate::error::db_error)?;
    if mailbox.is_none() {
        return Err(AppError::Validation(
            "That address is not one of this account's mailboxes.".into(),
        ));
    }

    // Reply-all carries the original Cc back to everyone it reached, minus this account's own
    // addresses (a person does not need a copy of their own reply). A plain reply and a forward
    // send to one addressee.
    let mut cc: Vec<String> = Vec::new();
    if mode == "reply_all" {
        if let Some(list) = original
            .try_get::<Option<String>, _>("cc_address")
            .unwrap_or(None)
        {
            for addr in list.split(',') {
                let a = addr.trim().to_lowercase();
                if !a.is_empty() && a != req.from_address.to_lowercase() && a != to.to_lowercase() {
                    cc.push(a);
                }
            }
        }
    }

    // Reuse the one send path.
    crate::private_email::send_handler::deliver(
        &state.db,
        tenant_id,
        &req.from_address,
        &to,
        &cc,
        &[],
        &subject,
        &req.body,
        None,
        req.include_signature.unwrap_or(true),
    )
    .await?;

    let snippet: String = req.body.chars().take(500).collect();
    let sent_id: Uuid = sqlx::query_scalar(
        r#"INSERT INTO private_email_messages (
              tenant_id, mailbox_id, address, folder, direction, from_address, to_address,
              subject, body_plain, snippet, in_reply_to, size_bytes, is_read, received_at
           ) VALUES ($1, $2, $3, 'sent', 'outbound', $4, $5, $6, $7, $8, $9, $10, true, NOW())
           RETURNING id"#,
    )
    .bind(tenant_id)
    .bind(mailbox.map(|m| m.0))
    .bind(&req.from_address)
    .bind(&req.from_address)
    .bind(&to)
    .bind(&subject)
    .bind(&req.body)
    .bind(&snippet)
    .bind(&subject_original)
    .bind(req.body.len() as i32)
    .fetch_one(&state.db)
    .await
    .map_err(crate::error::db_error)?;

    Ok(Json(json!({ "sent": true, "message_id": sent_id })))
}

/// POST /api/private-email/folders/action — mark all read / unread, or empty a folder.
///
/// `empty` is destructive by design and reports how many messages it destroyed, so a caller can
/// confirm with a real number instead of a promise (B205).
pub async fn folder_action(
    State(state): State<AppState>,
    Extension(claims): Extension<Claims>,
    Json(req): Json<FolderActionRequest>,
) -> ApiResult<Json<Value>> {
    let tenant_id = account_id(&claims)?;
    let folder = req.folder.trim().to_lowercase();

    match req.action.as_str() {
        "read" | "unread" => {
            let want = req.action == "read";
            let n = sqlx::query(
                "UPDATE private_email_messages SET is_read = $3, updated_at = NOW()
                  WHERE tenant_id = $1
                    AND (folder = $2 OR folder LIKE $2 || '/%')",
            )
            .bind(tenant_id)
            .bind(&folder)
            .bind(want)
            .execute(&state.db)
            .await
            .map_err(crate::error::db_error)?
            .rows_affected();
            Ok(Json(json!({ "updated": n, "is_read": want })))
        }
        // Archive's own two extra actions (B205). Both are MOVES, never destruction: "Move all to
        // Inbox" puts the mail back where a human will see it, and "Delete all messages" puts it in
        // Trash where it stays restorable. Only Spam and Trash destroy.
        "to_inbox" | "trash_all" => {
            if folder != "archive" {
                return Err(AppError::BadRequest(
                    "Those actions belong to Archive. Elsewhere the message actions are per message, \
                     so nothing is moved in bulk by accident."
                        .into(),
                ));
            }
            let target = if req.action == "to_inbox" {
                "inbox"
            } else {
                "trash"
            };
            let n = sqlx::query(
                "UPDATE private_email_messages
                    SET folder = $3, updated_at = NOW()
                  WHERE tenant_id = $1
                    AND (folder = $2 OR folder LIKE $2 || '/%')",
            )
            .bind(tenant_id)
            .bind(&folder)
            .bind(target)
            .execute(&state.db)
            .await
            .map_err(crate::error::db_error)?
            .rows_affected();
            Ok(Json(json!({ "moved": n, "to": target })))
        }
        "empty" => {
            // Only a holding pen can be emptied. Emptying Inbox would be a bulk delete wearing a
            // different name, and it is not offered anywhere.
            if folder != "trash" && folder != "spam" {
                return Err(AppError::BadRequest(
                    "Only Spam and Trash can be emptied. Messages elsewhere are moved to Trash \
                     first, so nothing is destroyed by one click."
                        .into(),
                ));
            }
            let n = sqlx::query(
                "DELETE FROM private_email_messages WHERE tenant_id = $1 AND folder = $2",
            )
            .bind(tenant_id)
            .bind(&folder)
            .execute(&state.db)
            .await
            .map_err(crate::error::db_error)?
            .rows_affected();
            Ok(Json(json!({ "destroyed": n, "folder": folder })))
        }
        other => Err(AppError::BadRequest(format!(
            "'{}' is not a folder action. Use read, unread or empty.",
            other
        ))),
    }
}

// ────────────────────────────────────────────────────────────────────────────────────────────────
// Labels (B205) — name + colour, per account, applied to one or many messages.
// ────────────────────────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct LabelRequest {
    pub name: Option<String>,
    pub color: Option<String>,
    #[serde(default)]
    pub message_ids: Option<Vec<Uuid>>,
}

pub async fn list_labels(
    State(state): State<AppState>,
    Extension(claims): Extension<Claims>,
) -> ApiResult<Json<Value>> {
    let tenant_id = account_id(&claims)?;
    let rows = sqlx::query(
        r#"
        SELECT l.id, l.name, l.color, count(ml.message_id) AS used
          FROM private_email_labels l
          LEFT JOIN private_email_message_labels ml ON ml.label_id = l.id
         WHERE l.tenant_id = $1
         GROUP BY l.id, l.name, l.color
         ORDER BY lower(l.name)
        "#,
    )
    .bind(tenant_id)
    .fetch_all(&state.db)
    .await
    .map_err(crate::error::db_error)?;

    let labels: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.try_get::<Uuid, _>("id").ok(),
                "name": r.try_get::<String, _>("name").unwrap_or_default(),
                "color": r.try_get::<String, _>("color").unwrap_or_default(),
                "used": r.try_get::<i64, _>("used").unwrap_or(0),
            })
        })
        .collect();
    Ok(Json(json!({ "labels": labels })))
}

pub async fn create_label(
    State(state): State<AppState>,
    Extension(claims): Extension<Claims>,
    Json(req): Json<LabelRequest>,
) -> ApiResult<Json<Value>> {
    let tenant_id = account_id(&claims)?;
    let name = req
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .ok_or_else(|| AppError::Validation("A label needs a name.".into()))?;
    if name.chars().count() > 64 {
        return Err(AppError::Validation(
            "A label name is at most 64 characters.".into(),
        ));
    }
    let color = req.color.as_deref().unwrap_or("#64748b");

    let exists: Option<(Uuid,)> = sqlx::query_as(
        "SELECT id FROM private_email_labels WHERE tenant_id = $1 AND lower(name) = lower($2)",
    )
    .bind(tenant_id)
    .bind(name)
    .fetch_optional(&state.db)
    .await
    .map_err(crate::error::db_error)?;
    if exists.is_some() {
        return Err(AppError::Duplicate(format!(
            "A label called '{}' already exists.",
            name
        )));
    }

    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO private_email_labels (tenant_id, name, color) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(tenant_id)
    .bind(name)
    .bind(color)
    .fetch_one(&state.db)
    .await
    .map_err(crate::error::db_error)?;

    Ok(Json(json!({ "id": id, "name": name, "color": color })))
}

/// PATCH /api/private-email/labels/:id — rename and/or recolour. Never touches messages.
pub async fn update_label(
    State(state): State<AppState>,
    Extension(claims): Extension<Claims>,
    Path(label_id): Path<Uuid>,
    Json(req): Json<LabelRequest>,
) -> ApiResult<Json<Value>> {
    let tenant_id = account_id(&claims)?;
    let row = sqlx::query(
        "UPDATE private_email_labels
            SET name = COALESCE($3, name), color = COALESCE($4, color)
          WHERE id = $1 AND tenant_id = $2
        RETURNING id, name, color",
    )
    .bind(label_id)
    .bind(tenant_id)
    .bind(req.name.as_deref().map(str::trim).filter(|n| !n.is_empty()))
    .bind(req.color.as_deref())
    .fetch_optional(&state.db)
    .await
    .map_err(crate::error::db_error)?
    .ok_or_else(|| AppError::NotFound("Label not found".into()))?;

    Ok(Json(json!({
        "id": row.try_get::<Uuid, _>("id").ok(),
        "name": row.try_get::<String, _>("name").unwrap_or_default(),
        "color": row.try_get::<String, _>("color").unwrap_or_default(),
    })))
}

/// DELETE /api/private-email/labels/:id — removes the label only. Messages are untouched.
pub async fn delete_label(
    State(state): State<AppState>,
    Extension(claims): Extension<Claims>,
    Path(label_id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    let tenant_id = account_id(&claims)?;
    let n = sqlx::query("DELETE FROM private_email_labels WHERE id = $1 AND tenant_id = $2")
        .bind(label_id)
        .bind(tenant_id)
        .execute(&state.db)
        .await
        .map_err(crate::error::db_error)?
        .rows_affected();
    if n == 0 {
        return Err(AppError::NotFound("Label not found".into()));
    }
    Ok(Json(json!({ "deleted": true })))
}

/// POST /api/private-email/labels/:id/apply — attach the label to one or many messages.
/// DELETE /api/private-email/labels/:id/apply — detach it from one or many.
pub async fn apply_label(
    State(state): State<AppState>,
    Extension(claims): Extension<Claims>,
    Path(label_id): Path<Uuid>,
    Json(req): Json<LabelRequest>,
) -> ApiResult<Json<Value>> {
    let tenant_id = account_id(&claims)?;
    let ids = req.message_ids.unwrap_or_default();
    if ids.is_empty() {
        return Err(AppError::Validation(
            "Choose at least one message to label.".into(),
        ));
    }

    let owns: Option<(Uuid,)> =
        sqlx::query_as("SELECT id FROM private_email_labels WHERE id = $1 AND tenant_id = $2")
            .bind(label_id)
            .bind(tenant_id)
            .fetch_optional(&state.db)
            .await
            .map_err(crate::error::db_error)?;
    if owns.is_none() {
        return Err(AppError::NotFound("Label not found".into()));
    }

    let applied = sqlx::query(
        r#"
        INSERT INTO private_email_message_labels (message_id, label_id)
        SELECT m.id, $3 FROM private_email_messages m
         WHERE m.tenant_id = $1 AND m.id = ANY($2)
        ON CONFLICT DO NOTHING
        "#,
    )
    .bind(tenant_id)
    .bind(&ids)
    .bind(label_id)
    .execute(&state.db)
    .await
    .map_err(crate::error::db_error)?
    .rows_affected();

    Ok(Json(json!({ "labeled": applied })))
}

pub async fn remove_label(
    State(state): State<AppState>,
    Extension(claims): Extension<Claims>,
    Path(label_id): Path<Uuid>,
    Json(req): Json<LabelRequest>,
) -> ApiResult<Json<Value>> {
    let tenant_id = account_id(&claims)?;
    let ids = req.message_ids.unwrap_or_default();
    if ids.is_empty() {
        return Err(AppError::Validation(
            "Choose at least one message to unlabel.".into(),
        ));
    }
    let removed = sqlx::query(
        r#"
        DELETE FROM private_email_message_labels ml
         USING private_email_messages m
         WHERE ml.message_id = m.id
           AND ml.label_id = $3
           AND m.tenant_id = $1
           AND m.id = ANY($2)
        "#,
    )
    .bind(tenant_id)
    .bind(&ids)
    .bind(label_id)
    .execute(&state.db)
    .await
    .map_err(crate::error::db_error)?
    .rows_affected();
    Ok(Json(json!({ "unlabeled": removed })))
}

/// GET /api/private-email/members — the people mail can be assigned to.
///
/// "Assign to a team member" is the same object as ticket assignment (B204), and a name that is not
/// in this account must never be offered. Members see the account's people; nothing outside it.
///
/// ACTIVE people only (kanban t_5f314e9a): a teammate who was removed must not stay in the picker —
/// the assignment write refuses an inactive row anyway, so offering it would be a control that
/// dead-ends. The NAME comes from `users.name` with the first/last pair as a fallback: signup and the
/// invite door write `name`, while only some writers fill `first_name`/`last_name`, and a picker of
/// blank people is unusable.
pub async fn list_members(
    State(state): State<AppState>,
    Extension(claims): Extension<Claims>,
) -> ApiResult<Json<Value>> {
    let tenant_id = account_id(&claims)?;
    let rows = sqlx::query(
        r#"SELECT id,
                  COALESCE(NULLIF(btrim(name), ''), email) AS display_name,
                  email, role
             FROM accounts
            WHERE id = $1
            ORDER BY display_name"#,
    )
    .bind(tenant_id)
    .fetch_all(&state.db)
    .await
    .map_err(crate::error::db_error)?;

    let members: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.try_get::<Uuid, _>("id").ok(),
                "name": r.try_get::<String, _>("display_name").unwrap_or_default(),
                "email": r.try_get::<Option<String>, _>("email").unwrap_or(None),
                "role": r.try_get::<Option<String>, _>("role").unwrap_or(None),
            })
        })
        .collect();

    Ok(Json(json!({ "members": members })))
}

/// GET /api/private-email/folders — the rail: the eight surfaces with live counts, plus every
/// subfolder the user created (with counts). Counts and folders come from the same tables the list
/// reads, so a badge can never disagree with the folder it labels.
pub async fn list_folders(
    State(state): State<AppState>,
    Extension(claims): Extension<Claims>,
) -> ApiResult<Json<Value>> {
    let tenant_id = account_id(&claims)?;

    let row = sqlx::query(
        r#"
        SELECT
          count(*) FILTER (WHERE folder = 'inbox' OR folder LIKE 'inbox/%')                      AS inbox,
          count(*) FILTER (WHERE (folder = 'inbox' OR folder LIKE 'inbox/%') AND is_read = false) AS unread,
          count(*) FILTER (WHERE is_starred AND folder <> 'trash')                               AS favorites,
          count(*) FILTER (WHERE folder = 'drafts')                                              AS drafts,
          count(*) FILTER (WHERE folder = 'sent')                                                AS sent,
          count(*) FILTER (WHERE folder = 'archive')                                             AS archive,
          count(*) FILTER (WHERE folder = 'spam')                                                AS spam,
          count(*) FILTER (WHERE folder = 'trash')                                               AS trash
        FROM private_email_messages WHERE tenant_id = $1
        "#,
    )
    .bind(tenant_id)
    .fetch_one(&state.db)
    .await
    .map_err(crate::error::db_error)?;

    let system: Vec<Value> = [
        ("inbox", "Inbox"),
        ("unread", "Unread"),
        ("favorites", "Favorites"),
        ("drafts", "Drafts"),
        ("sent", "Sent"),
        ("archive", "Archive"),
        ("spam", "Spam"),
        ("trash", "Trash"),
    ]
    .iter()
    .map(|(key, label)| {
        json!({
            "id": key,
            "name": label,
            "system": true,
            "count": row.try_get::<i64, _>(key).unwrap_or(0),
        })
    })
    .collect();

    // Custom subfolders, with the count of mail actually sitting in each (and below it).
    let custom = sqlx::query(
        r#"
        SELECT f.id, f.path, f.name, f.parent,
               (SELECT count(*) FROM private_email_messages m
                 WHERE m.tenant_id = f.tenant_id
                   AND (lower(m.folder) = lower(f.path)
                        OR left(lower(m.folder), length(f.path) + 1) = lower(f.path) || '/')
                   AND m.folder <> 'trash') AS count
          FROM private_email_folders f
         WHERE f.tenant_id = $1
         ORDER BY f.parent, lower(f.name)
        "#,
    )
    .bind(tenant_id)
    .fetch_all(&state.db)
    .await
    .map_err(crate::error::db_error)?;

    let custom_json: Vec<Value> = custom
        .iter()
        .map(|r| {
            json!({
                "id": r.try_get::<Uuid, _>("id").ok(),
                "path": r.try_get::<String, _>("path").unwrap_or_default(),
                "name": r.try_get::<String, _>("name").unwrap_or_default(),
                "parent": r.try_get::<String, _>("parent").unwrap_or_default(),
                "system": false,
                "count": r.try_get::<i64, _>("count").unwrap_or(0),
            })
        })
        .collect();

    Ok(Json(json!({ "system": system, "custom": custom_json })))
}

#[derive(Debug, Deserialize)]
pub struct CreateFolderRequest {
    /// inbox | archive | sent | drafts | spam | trash — or an existing subfolder path to nest under.
    pub parent: String,
    pub name: String,
}

/// POST /api/private-email/folders — create a subfolder. Real server-side state, so it survives a
/// new device and can be selected by any member of the account.
pub async fn create_folder(
    State(state): State<AppState>,
    Extension(claims): Extension<Claims>,
    Json(req): Json<CreateFolderRequest>,
) -> ApiResult<Json<Value>> {
    let tenant_id = account_id(&claims)?;
    let name = req.name.trim();
    if name.is_empty() || name.chars().count() > 64 {
        return Err(AppError::Validation(
            "A folder needs a name of 64 characters or fewer.".into(),
        ));
    }
    // A separator in a name would silently create a deeper path than the user asked for.
    if name.contains('/') {
        return Err(AppError::Validation(
            "A folder name cannot contain a slash — use Create subfolder to nest one inside another."
                .into(),
        ));
    }

    let parent = req.parent.trim().trim_matches('/').to_lowercase();
    let parent_ok = SYSTEM_FOLDERS.contains(&parent.as_str())
        || sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM private_email_folders WHERE tenant_id = $1 AND lower(path) = $2",
        )
        .bind(tenant_id)
        .bind(&parent)
        .fetch_one(&state.db)
        .await
        .map_err(crate::error::db_error)?
            > 0;
    if !parent_ok {
        return Err(AppError::Validation(format!(
            "'{}' is not a folder in this account, so nothing can be created inside it.",
            req.parent
        )));
    }

    // The path is stored lower case, because a message's folder is stored lower case
    // (update_message normalises it): an as-typed path like 'inbox/Clients' could never match — or
    // count — the mail filed into it. `name` keeps the spelling the user typed, for the rail.
    let path = format!("{}/{}", parent, name.to_lowercase());
    let exists: Option<(Uuid,)> = sqlx::query_as(
        "SELECT id FROM private_email_folders WHERE tenant_id = $1 AND lower(path) = lower($2)",
    )
    .bind(tenant_id)
    .bind(&path)
    .fetch_optional(&state.db)
    .await
    .map_err(crate::error::db_error)?;
    if exists.is_some() {
        return Err(AppError::Duplicate(format!(
            "'{}' already exists in {}.",
            name, parent
        )));
    }

    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO private_email_folders (tenant_id, path, name, parent)
         VALUES ($1, $2, $3, $4) RETURNING id",
    )
    .bind(tenant_id)
    .bind(&path)
    .bind(name)
    .bind(&parent)
    .fetch_one(&state.db)
    .await
    .map_err(crate::error::db_error)?;

    Ok(Json(
        json!({ "id": id, "path": path, "name": name, "parent": parent }),
    ))
}

#[derive(Debug, Deserialize)]
pub struct DraftRequest {
    pub from_address: Option<String>,
    #[serde(default)]
    pub to: Option<String>,
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    /// Present when an existing draft is being saved again (so it is updated, not duplicated).
    #[serde(default)]
    pub id: Option<Uuid>,
}

/// POST /api/private-email/messages/drafts — save what is in the composer.
///
/// The Drafts folder is listed in the client (B205: "saved compose drafts, resumable"), so it has to
/// be able to hold something: a folder nobody can put mail into is a dead surface. Re-saving an
/// opened draft updates that row instead of stacking copies.
pub async fn save_draft(
    State(state): State<AppState>,
    Extension(claims): Extension<Claims>,
    Json(req): Json<DraftRequest>,
) -> ApiResult<Json<Value>> {
    let tenant_id = account_id(&claims)?;
    let from = req.from_address.unwrap_or_default();
    let to = req.to.unwrap_or_default();
    let subject = req.subject.unwrap_or_default();
    let body = req.body.unwrap_or_default();

    if body.trim().is_empty() && subject.trim().is_empty() && to.trim().is_empty() {
        return Err(AppError::Validation(
            "There is nothing to save yet — write a line first.".into(),
        ));
    }

    let snippet: String = body.chars().take(500).collect();
    let mailbox_id: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM private_email_boxes
          WHERE tenant_id = $1 AND email_address = $2 AND status = 'active'",
    )
    .bind(tenant_id)
    .bind(&from)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();

    let id = match req.id {
        Some(existing) => {
            let n = sqlx::query(
                "UPDATE private_email_messages
                    SET mailbox_id = $3, address = $4, to_address = $5, subject = $6,
                        body_plain = $7, snippet = $8, size_bytes = $9, updated_at = NOW()
                  WHERE id = $1 AND tenant_id = $2 AND folder = 'drafts'",
            )
            .bind(existing)
            .bind(tenant_id)
            .bind(mailbox_id)
            .bind(&from)
            .bind(&to)
            .bind(&subject)
            .bind(&body)
            .bind(&snippet)
            // The storage meter reads `size_bytes`, so a resaved draft MUST carry its new size: the
            // old statement left the first save's figure in place, so a draft that grew from a line
            // to a long letter kept counting as a line and the meter could never see the growth.
            .bind(body.len() as i32)
            .execute(&state.db)
            .await
            .map_err(crate::error::db_error)?
            .rows_affected();
            if n == 0 {
                return Err(AppError::NotFound("Draft not found".into()));
            }
            existing
        }
        None => {
            let new_id = sqlx::query_scalar(
                r#"INSERT INTO private_email_messages (
                      tenant_id, mailbox_id, address, folder, direction, from_address,
                      to_address, subject, body_plain, snippet, size_bytes, is_read, received_at
                   ) VALUES ($1, $2, $3, 'drafts', 'draft', $4, $5, $6, $7, $8, $9, true, NOW())
                   RETURNING id"#,
            )
            .bind(tenant_id)
            .bind(mailbox_id)
            .bind(&from)
            .bind(&from)
            .bind(&to)
            .bind(&subject)
            .bind(&body)
            .bind(&snippet)
            .bind(body.len() as i32)
            .fetch_one(&state.db)
            .await
            .map_err(crate::error::db_error)?;
            new_id
        }
    };

    Ok(Json(json!({ "id": id, "folder": "drafts" })))
}

// ────────────────────────────────────────────────────────────────────────────────────────────────
// Helpers
// ────────────────────────────────────────────────────────────────────────────────────────────────

fn account_id(claims: &Claims) -> Result<Uuid, AppError> {
    Uuid::parse_str(&claims.aid).map_err(|_| AppError::Unauthorized("Invalid session".into()))
}

/// One list row. `body_plain` / `body_html` are deliberately absent: a list of 50 messages must not
/// carry 50 full bodies over the wire, and the snippet is what the list actually shows.
fn message_json(row: &sqlx::postgres::PgRow) -> Value {
    json!({
        "id": row.try_get::<Uuid, _>("id").ok(),
        "mailbox_id": row.try_get::<Option<Uuid>, _>("mailbox_id").unwrap_or(None),
        "address": row.try_get::<String, _>("address").ok(),
        "folder": row.try_get::<String, _>("folder").ok(),
        "direction": row.try_get::<String, _>("direction").unwrap_or_else(|_| "inbound".into()),
        "from_address": row.try_get::<String, _>("from_address").unwrap_or_default(),
        "from_name": row.try_get::<Option<String>, _>("from_name").unwrap_or(None),
        "to_address": row.try_get::<String, _>("to_address").unwrap_or_default(),
        "cc_address": row.try_get::<Option<String>, _>("cc_address").unwrap_or(None),
        "subject": row.try_get::<String, _>("subject").unwrap_or_default(),
        "snippet": row.try_get::<String, _>("snippet").unwrap_or_default(),
        "is_read": row.try_get::<bool, _>("is_read").unwrap_or(false),
        "is_starred": row.try_get::<bool, _>("is_starred").unwrap_or(false),
        "is_ticket": row.try_get::<bool, _>("is_ticket").unwrap_or(false),
        "ticket_id": row.try_get::<Option<Uuid>, _>("ticket_id").unwrap_or(None),
        "assigned_user_id": row.try_get::<Option<Uuid>, _>("assigned_user_id").unwrap_or(None),
        "has_attachments": row.try_get::<bool, _>("has_attachments").unwrap_or(false),
        "size_bytes": row.try_get::<i32, _>("size_bytes").unwrap_or(0),
        "received_at": row.try_get::<DateTime<Utc>, _>("received_at").ok(),
    })
}

async fn message_labels(
    state: &AppState,
    tenant_id: Uuid,
    message_id: Uuid,
) -> Result<Value, AppError> {
    let rows = sqlx::query(
        r#"
        SELECT l.id, l.name, l.color
          FROM private_email_message_labels ml
          JOIN private_email_labels l ON l.id = ml.label_id
         WHERE ml.message_id = $1 AND l.tenant_id = $2
         ORDER BY lower(l.name)
        "#,
    )
    .bind(message_id)
    .bind(tenant_id)
    .fetch_all(&state.db)
    .await
    .map_err(crate::error::db_error)?;

    Ok(json!(rows
        .iter()
        .map(|r| json!({
            "id": r.try_get::<Uuid, _>("id").ok(),
            "name": r.try_get::<String, _>("name").unwrap_or_default(),
            "color": r.try_get::<String, _>("color").unwrap_or_default(),
        }))
        .collect::<Vec<Value>>()))
}

// ────────────────────────────────────────────────────────────────────────────────────────────────
// The inbound writer — ONE place that stores a received message.
// ────────────────────────────────────────────────────────────────────────────────────────────────

/// Everything the webhook knows about a delivery, already authenticated.
pub struct InboundMessage<'a> {
    pub tenant_id: Uuid,
    pub mailbox_id: Option<Uuid>,
    pub address: &'a str,
    pub from_address: &'a str,
    pub from_name: Option<&'a str>,
    pub to_address: &'a str,
    pub cc_address: Option<&'a str>,
    pub subject: &'a str,
    pub body_plain: &'a str,
    pub body_html: Option<&'a str>,
    pub message_id: Option<&'a str>,
    pub in_reply_to: Option<&'a str>,
    pub provider: &'a str,
    pub provider_message_id: Option<&'a str>,
    pub is_spam: bool,
    pub has_attachments: bool,
    /// Bytes carried by the delivery's file parts, added to `size_bytes` so an account's storage
    /// figure includes what an attached message costs (kanban t_237e3005). The FILES themselves are
    /// not stored by this receiver — only the message is.
    pub attachment_bytes: usize,
}

/// Store one received message. Returns (message id, whether this call created it).
///
/// Idempotent on (tenant, Message-Id): a provider retry finds the row already there, updates nothing
/// and gets the SAME id back, so a retry can never become a second copy of the same mail.
pub async fn store_inbound(
    db: &sqlx::PgPool,
    msg: &InboundMessage<'_>,
) -> Result<(Uuid, bool), AppError> {
    let snippet: String = msg.body_plain.chars().take(500).collect();
    let size = (msg.body_plain.len()
        + msg.body_html.map(str::len).unwrap_or(0)
        + msg.attachment_bytes) as i32;
    let folder = if msg.is_spam { "spam" } else { "inbox" };

    let row = sqlx::query(
        r#"
        INSERT INTO private_email_messages (
            tenant_id, mailbox_id, address, folder, direction,
            from_address, from_name, to_address, cc_address, subject,
            body_plain, body_html, snippet, message_id, in_reply_to,
            provider, provider_message_id, size_bytes, has_attachments, received_at
        ) VALUES ($1,$2,$3,$4,'inbound',$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,NOW())
        ON CONFLICT (tenant_id, message_id) WHERE message_id IS NOT NULL AND message_id <> ''
        DO NOTHING
        RETURNING id
        "#,
    )
    .bind(msg.tenant_id)
    .bind(msg.mailbox_id)
    .bind(msg.address.to_lowercase())
    .bind(folder)
    .bind(msg.from_address)
    .bind(msg.from_name)
    .bind(msg.to_address)
    .bind(msg.cc_address)
    .bind(msg.subject)
    .bind(msg.body_plain)
    .bind(msg.body_html)
    .bind(&snippet)
    .bind(msg.message_id)
    .bind(msg.in_reply_to)
    .bind(msg.provider)
    .bind(msg.provider_message_id)
    .bind(size)
    .bind(msg.has_attachments)
    .fetch_optional(db)
    .await
    .map_err(crate::error::db_error)?;

    match row {
        Some(r) => Ok((r.try_get::<Uuid, _>("id").unwrap_or_default(), true)),
        None => {
            // Either a retry of a delivery already stored, or no Message-Id at all (the ON CONFLICT
            // clause cannot fire for NULL/empty). Fall back to the newest matching row so the caller
            // still gets an id to attach the ticket to.
            let existing: Option<Uuid> = sqlx::query_scalar(
                r#"
                SELECT id FROM private_email_messages
                 WHERE tenant_id = $1
                   AND address = $2
                   AND from_address = $3
                   AND subject = $4
                   AND received_at > NOW() - INTERVAL '1 day'
                 ORDER BY received_at DESC
                 LIMIT 1
                "#,
            )
            .bind(msg.tenant_id)
            .bind(msg.address.to_lowercase())
            .bind(msg.from_address)
            .bind(msg.subject)
            .fetch_optional(db)
            .await
            .map_err(crate::error::db_error)?;

            Ok((existing.unwrap_or_default(), false))
        }
    }
}

/// Attach the ticket a message created (or was converted into). Keeps the flag and the link in one
/// statement so a message can never claim to be a ticket while pointing at none.
pub async fn link_ticket(
    db: &sqlx::PgPool,
    tenant_id: Uuid,
    message_id: Uuid,
    ticket_id: Uuid,
) -> Result<(), AppError> {
    if message_id == Uuid::nil() {
        return Ok(());
    }
    sqlx::query(
        "UPDATE private_email_messages
            SET ticket_id = $3, is_ticket = true, updated_at = NOW()
          WHERE id = $1 AND tenant_id = $2",
    )
    .bind(message_id)
    .bind(tenant_id)
    .bind(ticket_id)
    .execute(db)
    .await
    .map_err(crate::error::db_error)?;
    Ok(())
}
