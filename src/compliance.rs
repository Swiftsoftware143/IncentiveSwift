//! Email compliance — one gate in front of every message this app sends.
//!
//! The rules are the same ones CoreSwift already enforces, because a customer's inbox does not care
//! which of our products mailed them:
//!
//!  * `transactional` (receipts, password resets, security notices) is exempt from opt-out by law and
//!    must keep working for somebody who unsubscribed from everything. NOT exempt from harm control: an
//!    address that hard-bounced or complained is never mailed again, in any class.
//!  * `notification` and `bulk` are stoppable. They carry the opt-out footer and the one-click headers.
//!
//! Suppression is PER ACCOUNT, never global: one customer's opt-out must never silence another
//! customer's mail to the same person.

use axum::{
    extract::{Path, State},
    http::{header, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::get,
    Json, Router,
};
use base64::Engine as _;
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::Sha256;
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::middleware::AuthUser;
use crate::state::AppState;

pub const BULK: &str = "bulk";
pub const NOTIFICATION: &str = "notification";
pub const TRANSACTIONAL: &str = "transactional";

/// The scope this app's own mail belongs to.
///
/// This app mails as ITSELF (one brand, one sending identity), so its do-not-mail list is the app's and
/// not one account's. A fixed id keeps that honest and keeps the tables' tenant column meaningful for the
/// day an account sends under its own name — that day the same code passes the account's tenant instead,
/// with no migration.
pub const PLATFORM_TENANT: Uuid = Uuid::from_u128(1);

/// Where the customer-facing pages live for this app.
const APP_URL: &str = "https://app.incentiveswift.com";

pub fn normalise(address: &str) -> String {
    address.trim().to_lowercase()
}

fn sign(secret: &str, payload: &str) -> String {
    let mut mac =
        <Hmac<Sha256> as Mac>::new_from_slice(secret.as_bytes()).expect("hmac accepts any key len");
    mac.update(payload.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// A tamper-proof opt-out token: account + address + class. No expiry — a link in a year-old message
/// must still work, and there is no state to lose in a table.
pub fn token_for(secret: &str, tenant_id: Uuid, address: &str, mail_class: &str) -> String {
    let addr = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(normalise(address));
    let payload = format!("{tenant_id}.{addr}.{mail_class}");
    let sig = sign(secret, &payload);
    format!("{payload}.{sig}")
}

pub fn verify_token(secret: &str, token: &str) -> Option<(Uuid, String, String)> {
    let (payload, sig) = token.rsplit_once('.')?;
    if sign(secret, payload) != sig {
        return None;
    }
    let mut it = payload.split('.');
    let tenant_id: Uuid = it.next()?.parse().ok()?;
    let address = String::from_utf8(
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(it.next()?)
            .ok()?,
    )
    .ok()?;
    let mail_class = it.next()?.to_string();
    Some((tenant_id, address, mail_class))
}

pub fn unsubscribe_url(secret: &str, tenant_id: Uuid, address: &str, mail_class: &str) -> String {
    format!(
        "{APP_URL}/api/public/unsubscribe/{}",
        token_for(secret, tenant_id, address, mail_class)
    )
}

fn blocking_classes(mail_class: &str) -> &'static [&'static str] {
    match mail_class {
        // Harm control stops EVERY class. `compliance_events` writes a hard bounce / spam
        // complaint / provider "unsubscribed" as class `all`, and an address that proved
        // undeliverable (or asked to be gone entirely) must silence even transactional mail —
        // delivering at all is the harm. Fleet parity: FunnelSwift and ADASwift both read
        // TRANSACTIONAL as `["all"]`; the previous `["transactional"]` matched NOTHING the live
        // writer ever emits, so a hard-bounced address was still mailed transactional receipts.
        TRANSACTIONAL => &["all"],
        NOTIFICATION => &["all", "notification"],
        _ => &["all", "bulk"],
    }
}

/// Somebody asked this account to mail this address less. Returns the reason when that covers the class
/// we were about to send.
pub async fn suppression(
    db: &PgPool,
    tenant_id: Uuid,
    address: &str,
    mail_class: &str,
) -> Option<String> {
    let classes: Vec<String> = blocking_classes(mail_class)
        .iter()
        .map(|c| c.to_string())
        .collect();
    sqlx::query_scalar::<_, String>(
        "SELECT reason FROM email_suppressions
          WHERE tenant_id = $1 AND address = $2 AND mail_class = ANY($3)
          ORDER BY created_at LIMIT 1",
    )
    .bind(tenant_id)
    .bind(normalise(address))
    .bind(&classes)
    .fetch_optional(db)
    .await
    .ok()
    .flatten()
}

pub async fn suppress(
    db: &PgPool,
    tenant_id: Uuid,
    address: &str,
    reason: &str,
    mail_class: &str,
    source: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO email_suppressions (tenant_id, address, mail_class, reason, source)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (tenant_id, address, mail_class)
         DO UPDATE SET reason = EXCLUDED.reason, source = EXCLUDED.source, updated_at = now()",
    )
    .bind(tenant_id)
    .bind(normalise(address))
    .bind(mail_class)
    .bind(reason)
    .bind(source)
    .execute(db)
    .await?;

    if mail_class != TRANSACTIONAL {
        let _ = sqlx::query(
            "INSERT INTO email_consent (tenant_id, address, mail_class, state, how)
             VALUES ($1, $2, $3, 'revoked', $4)",
        )
        .bind(tenant_id)
        .bind(normalise(address))
        .bind(if mail_class == "all" {
            BULK
        } else {
            mail_class
        })
        .bind(source)
        .execute(db)
        .await;
    }
    Ok(())
}

/// One row per send attempt, written before the provider is asked.
pub async fn log_attempt(
    db: &PgPool,
    tenant_id: Uuid,
    mail_class: &str,
    address: &str,
    subject: Option<&str>,
    provider: Option<&str>,
    status: &str,
    detail: Option<&str>,
) {
    let _ = sqlx::query(
        "INSERT INTO email_message_log
            (tenant_id, mail_class, address, subject, provider, status, detail)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(tenant_id)
    .bind(if mail_class == "all" {
        BULK
    } else {
        mail_class
    })
    .bind(normalise(address))
    .bind(subject)
    .bind(provider)
    .bind(status)
    .bind(detail)
    .execute(db)
    .await;
}

/// The gate's verdict for one message.
pub struct Gate {
    pub blocked: Option<String>,
    pub headers: Vec<(String, String)>,
    pub footer_html: String,
    pub footer_text: String,
}

/// Build the gate for one message. A database that cannot answer must not silently turn an opt-out off:
/// a failed suppression read refuses the two stoppable classes and lets transactional mail through.
pub async fn gate(db: &PgPool, tenant_id: Uuid, address: &str, mail_class: &str) -> Gate {
    let addr = normalise(address);
    let classes: Vec<String> = blocking_classes(mail_class)
        .iter()
        .map(|c| c.to_string())
        .collect();

    let blocked = match sqlx::query_scalar::<_, String>(
        "SELECT reason FROM email_suppressions
          WHERE tenant_id = $1 AND address = $2 AND mail_class = ANY($3)
          ORDER BY created_at LIMIT 1",
    )
    .bind(tenant_id)
    .bind(&addr)
    .bind(&classes)
    .fetch_optional(db)
    .await
    {
        Ok(found) => found,
        Err(e) => {
            tracing::error!("suppression read failed for tenant {tenant_id}: {e}");
            if mail_class != TRANSACTIONAL {
                Some("suppression list unavailable".to_string())
            } else {
                None
            }
        }
    };

    // Account identity for the footer, from this app's own branding row.
    let account: Option<String> = sqlx::query_scalar(
        "SELECT name FROM accounts WHERE tenant_id = $1 ORDER BY created_at LIMIT 1",
    )
    .bind(tenant_id)
    .fetch_optional(db)
    .await
    .ok()
    .flatten();

    let account = account.unwrap_or_else(|| "IncentiveSwift".to_string());

    let mut headers = Vec::new();
    let mut footer_html = String::new();
    let mut footer_text = String::new();
    if mail_class != TRANSACTIONAL {
        // The token needs the signing secret, which the caller supplies by building the gate through
        // `gate_with_secret`; this plain `gate` is used where no link is emitted.
        let _ = &mut headers;
        footer_text = format!("\n\n--\n{account}\n");
        footer_html = format!(
            "<hr style=\"border:0;border-top:1px solid #e5e7eb;margin:28px 0 12px\">\
             <p style=\"font:13px/1.5 system-ui,sans-serif;color:#6b7280;margin:0\">{account}</p>"
        );
    }

    Gate {
        blocked,
        headers,
        footer_html,
        footer_text,
    }
}

/// The same gate, plus a working opt-out link and the one-click headers. Preferred everywhere an email
/// is actually rendered.
pub async fn gate_with_secret(
    db: &PgPool,
    secret: &str,
    tenant_id: Uuid,
    address: &str,
    mail_class: &str,
) -> Gate {
    let mut g = gate(db, tenant_id, address, mail_class).await;
    if mail_class != TRANSACTIONAL {
        let url = unsubscribe_url(secret, tenant_id, &normalise(address), mail_class);
        g.headers
            .push(("h:List-Unsubscribe".to_string(), format!("<{url}>")));
        g.headers.push((
            "h:List-Unsubscribe-Post".to_string(),
            "List-Unsubscribe=One-Click".to_string(),
        ));
        if let Some(first_line) = g.footer_text.lines().next() {
            let _ = first_line;
        }
        g.footer_text = format!("{}\\nStop these emails: {url}\\n", g.footer_text.trim_end());
        let link = format!(
            "<p style=\"font:13px/1.5 system-ui,sans-serif;color:#6b7280;margin:0\">\
             <a href=\"{url}\" style=\"color:#4f46e5\">Stop these emails</a></p>"
        );
        g.footer_html.push_str(&link);
    }
    g
}

// ── public surface: the opt-out itself ────────────────────────────────────────────────────────────

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn page_shell(title: &str, inner: &str) -> String {
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <meta name=\"robots\" content=\"noindex,nofollow\"><title>{}</title></head>\
         <body style=\"margin:0;background:#fff\"><div style=\"font:16px/1.6 system-ui,sans-serif;\
         max-width:560px;margin:10vh auto;padding:0 20px;color:#111\">{}</div></body></html>",
        html_escape(title),
        inner
    )
}

pub async fn show_unsubscribe(
    State(state): State<AppState>,
    Path(token): Path<String>,
) -> Response {
    let Some((tenant_id, address, _class)) = verify_token(&state.config.jwt_secret, &token) else {
        return (
            StatusCode::GONE,
            Html(page_shell(
                "Link expired",
                "<h1 style=\"font-size:1.2rem\">This link is not valid</h1><p>Reply to any message \
                 from this sender and ask them to remove you.</p>",
            )),
        )
            .into_response();
    };
    let addr = normalise(&address);
    let body = format!(
        "<h1 style=\"font-size:1.25rem;margin:0 0 8px\">Mail preferences for {addr}</h1>\
         <p style=\"color:#555;margin:0 0 20px\">Choose what you would like to stop receiving. Messages \
         you need — receipts, password resets, security notices — keep working.</p>\
         <form method=post action=\"/api/public/unsubscribe/{token}\">\
         <button style=\"font:inherit;padding:12px 22px;border:0;border-radius:8px;background:#4f46e5;\
         color:#fff;cursor:pointer\">Unsubscribe from everything</button></form>",
        addr = html_escape(&addr),
        token = token
    );
    let _ = tenant_id;
    (
        StatusCode::OK,
        [(header::CACHE_CONTROL, "no-store")],
        Html(page_shell("Unsubscribe", &body)),
    )
        .into_response()
}

pub async fn one_click(State(state): State<AppState>, Path(token): Path<String>) -> Response {
    let Some((tenant_id, address, mail_class)) = verify_token(&state.config.jwt_secret, &token)
    else {
        return (StatusCode::GONE, Json(json!({ "error": "invalid link" }))).into_response();
    };
    let stored_class = if mail_class == TRANSACTIONAL {
        "all"
    } else {
        mail_class.as_str()
    };
    match suppress(
        &state.db,
        tenant_id,
        &address,
        "unsubscribed",
        stored_class,
        "one_click",
    )
    .await
    {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({ "unsubscribed": true, "address": normalise(&address) })),
        )
            .into_response(),
        Err(e) => {
            tracing::error!("unsubscribe failed for tenant {tenant_id}: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "could not record the opt-out" })),
            )
                .into_response()
        }
    }
}

// ── authenticated surface ─────────────────────────────────────────────────────────────────────────

/// The tenant this request acts for, from the account the credential names.
async fn tenant_of(state: &AppState, auth: &AuthUser) -> Option<Uuid> {
    let account_id = auth.account_id.as_deref()?;
    sqlx::query_scalar::<_, Uuid>("SELECT tenant_id FROM accounts WHERE id = $1")
        .bind(Uuid::parse_str(account_id).ok()?)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()
}

/// Which suppression rows this caller may read.
///
/// The SENDER consults `email_suppressions` under [`PLATFORM_TENANT`] (`email_provider.rs`), so an
/// operator's list must include that scope or it answers an empty list for ever: the mechanism would
/// exist while the population never matched. A tenant that is not a platform admin keeps its own rows
/// ONLY — one customer's opt-outs are never another customer's business.
async fn suppression_scopes(state: &AppState, auth: &AuthUser) -> Option<Vec<Uuid>> {
    let own = tenant_of(state, auth).await;
    if auth.is_admin {
        let mut scopes = vec![PLATFORM_TENANT];
        if let Some(t) = own {
            if t != PLATFORM_TENANT {
                scopes.push(t);
            }
        }
        return Some(scopes);
    }
    own.map(|t| vec![t])
}

async fn list_suppressions(State(state): State<AppState>, auth: AuthUser) -> Response {
    // `AuthUser` is an EXTRACTOR (`FromRequestParts`, src/auth/middleware.rs) — it is not inserted
    // into the request extensions by anything, so taking it as `Extension<AuthUser>` answered
    // `500 Missing request extension` to every authenticated caller (measured live 2026-11-20:
    // anon 401, bearer 500). The extractor runs off the same credential the default-deny boundary
    // already checked.
    let Some(scopes) = suppression_scopes(&state, &auth).await else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "no account" })),
        )
            .into_response();
    };
    let rows = match sqlx::query_as::<_, (String, String, String, Option<String>, DateTime<Utc>)>(
        "SELECT address, mail_class, reason, source, created_at FROM email_suppressions
          WHERE tenant_id = ANY($1) ORDER BY created_at DESC LIMIT 1000",
    )
    .bind(&scopes)
    .fetch_all(&state.db)
    .await
    {
        Ok(rows) => rows,
        // A FAILED read must never paint as an empty list: "Nobody has opted out" is a statement
        // about the customer's list, and `unwrap_or_default()` made a database error say it. The
        // screen shows its own load-error arm instead.
        Err(e) => {
            tracing::error!("do-not-mail list read failed: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "the do-not-mail list could not be read", "status": 500 })),
            )
                .into_response();
        }
    };
    let items: Vec<Value> = rows
        .into_iter()
        .map(|(address, mail_class, reason, source, created_at)| {
            json!({ "address": address, "mail_class": mail_class, "reason": reason,
                    "source": source, "created_at": created_at })
        })
        .collect();
    Json(json!({ "suppressions": items })).into_response()
}

async fn suppressions_csv(State(state): State<AppState>, auth: AuthUser) -> Response {
    let Some(scopes) = suppression_scopes(&state, &auth).await else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "no account" })),
        )
            .into_response();
    };
    let rows = match sqlx::query_as::<_, (String, String, String, Option<String>, DateTime<Utc>)>(
        "SELECT address, mail_class, reason, source, created_at FROM email_suppressions
          WHERE tenant_id = ANY($1) ORDER BY created_at DESC",
    )
    .bind(&scopes)
    .fetch_all(&state.db)
    .await
    {
        Ok(rows) => rows,
        // Same rule as the list above: a failed read is an ERROR, never an empty download.
        Err(e) => {
            tracing::error!("do-not-mail CSV read failed: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "the do-not-mail list could not be read", "status": 500 })),
            )
                .into_response();
        }
    };
    let mut out = String::from("address,scope,reason,source,recorded_at\n");
    for (address, mail_class, reason, source, created_at) in rows {
        out.push_str(&format!(
            "{},{},{},{},{}\n",
            address,
            mail_class,
            reason,
            source.unwrap_or_default(),
            created_at.to_rfc3339()
        ));
    }
    let disposition = HeaderValue::from_static("attachment; filename=\"unsubscribes.csv\"");
    (
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/csv; charset=utf-8"),
            ),
            (header::CONTENT_DISPOSITION, disposition),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
        ],
        out,
    )
        .into_response()
}

pub fn router(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/suppressions", get(list_suppressions))
        .route("/suppressions.csv", get(suppressions_csv))
        .with_state(state)
}

/// Merged at the ROOT: the opt-out arms are anonymous by necessity — the token in the message IS the
/// credential — and the page must be reachable from a mail client with no session.
pub fn public_router(state: AppState) -> Router<AppState> {
    Router::new()
        .route(
            "/api/public/unsubscribe/:token",
            get(show_unsubscribe).post(one_click),
        )
        .with_state(state)
}
