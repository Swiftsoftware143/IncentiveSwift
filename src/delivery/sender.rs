//! SMTP email sender — tenant-aware, reads SMTP config from tenant_settings
//!
//! Each tenant can configure their own SMTP server (host, port, username, password, from address).
//! An account with NO mail server of its own rides the PLATFORM mail service — the app's ONE
//! system-mail path, `email_provider` (`admin_settings.email`, the row the admin Email panel and
//! its test-send use) — so "removed" really means "back on platform mail" (kanban t_2e9117a5).
//!
//!
//! # The recipient side (kanban t_f56f4a79)
//!
//! Both arms refuse a recipient that provably cannot receive mail — RFC 2606 `example.com`/`.net`/
//! `.org` (any subdomain) and the special-use TLDs `.invalid`/`.test`/`.example`/`.local`/
//! `.localhost`, plus a malformed address — BEFORE any socket is opened, via
//! [`crate::security::email_addr::refuse_undeliverable_recipient`] (the same guard
//! `email_provider::deliver` applies on the platform arm). The refusal is an ordinary `Err` whose
//! text starts `recipient-refused:`, so the Settings → Email pane, the queue's `last_error` and
//! the ticker's log line all name WHY instead of reporting a transport failure. Sending anyway is
//! not an option: those domains are reserved precisely so they can never deliver, so the only
//! outcomes are a wasted send and a bounce against the sending reputation.
//! The `smtp_password` scalar is a CREDENTIAL and is stored under this app's `enc:v1:` envelope
//! (kanban t_123b886b): the tenant settings writer seals it, [`open_smtp_password`] is the read
//! half used before lettre ever sees it, and [`seal_legacy_tenant_smtp_passwords`] is the boot half
//! that converges a row arriving plaintext from an older dump. An envelope this deployment cannot
//! open is NEVER handed to the transport — the tenant config is skipped so the send falls back to
//! the system provider instead of authenticating with the envelope.

use lettre::message::header::ContentType;
use lettre::{
    transport::smtp::authentication::Credentials, AsyncSmtpTransport, AsyncTransport, Message,
    Tokio1Executor,
};
use serde_json::Value;
use sqlx::PgPool;
use std::time::Duration;
use uuid::Uuid;

/// The `tenant_settings` key carrying a tenant's SMTP password. It is a bare STRING, not a config
/// object, which is why the seal that covers `email_config`/`mailgun_config`/`smtp_config` never
/// covered it.
pub const SMTP_PASSWORD_KEY: &str = "smtp_password";

/// The `tenant_settings` key prefix of the tenant's own mail-server family: exactly the rows
/// [`load_smtp_config`] reads. "Remove my mail server" is therefore "delete my OWN rows under
/// this prefix" (kanban t_2e9117a5) — a purpose-scoped arm, so a removal can never touch an
/// unrelated setting and can never leave half a mail server behind.
pub const MAIL_KEY_PREFIX: &str = "smtp_";

/// SMTP configuration for a tenant
#[derive(Debug, Clone)]
pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub from_email: String,
    pub from_name: Option<String>,
}

/// Open a stored `smtp_password`: an `enc:v1:` value is decrypted with this deployment's master
/// key, a value without the envelope is a legacy plaintext row and passes through unchanged.
pub async fn open_smtp_password(
    pool: &PgPool,
    stored: &str,
) -> Result<String, crate::security::provider_key_crypto::CryptoError> {
    crate::security::provider_key_crypto::decrypt_from_storage(pool, stored).await
}

/// Seal every `tenant_settings.smtp_password` still sitting in the clear.
///
/// The write path seals before it stores, but a row can also arrive plaintext from a database
/// restored out of an older dump — or from a writer added later that forgets. Idempotent; returns
/// the number of rows it had to rewrite.
pub async fn seal_legacy_tenant_smtp_passwords(
    pool: &PgPool,
) -> Result<u64, crate::security::provider_key_crypto::CryptoError> {
    use crate::security::provider_key_crypto;
    let rows: Vec<(Uuid, Value)> =
        sqlx::query_as("SELECT tenant_id, value FROM tenant_settings WHERE key = $1")
            .bind(SMTP_PASSWORD_KEY)
            .fetch_all(pool)
            .await?;
    let mut sealed = 0u64;
    for (tenant_id, value) in rows {
        let Some(current) = value.as_str() else {
            continue;
        };
        if current.is_empty() || provider_key_crypto::is_encrypted(current) {
            continue;
        }
        let envelope = provider_key_crypto::encrypt_for_storage(pool, current).await?;
        sqlx::query(
            "UPDATE tenant_settings SET value = to_jsonb($1::text), updated_at = NOW()
              WHERE tenant_id = $2 AND key = $3",
        )
        .bind(&envelope)
        .bind(tenant_id)
        .bind(SMTP_PASSWORD_KEY)
        .execute(pool)
        .await?;
        sealed += 1;
    }
    Ok(sealed)
}

/// Load SMTP config for a specific tenant account.
///
/// The stored `smtp_password` is opened HERE, before it becomes a lettre credential. This is a LIVE
/// path (the pending-email ticker, lifecycle emails, entry emails and output actions all reach it),
/// so the open is what makes the seal on write safe.
pub async fn load_smtp_config(pool: &PgPool, account_id: Uuid) -> Option<SmtpConfig> {
    let rows = sqlx::query_as::<_, (String, Value)>(
        "SELECT key, value FROM tenant_settings WHERE tenant_id = $1 AND key LIKE 'smtp_%'",
    )
    .bind(account_id)
    .fetch_all(pool)
    .await
    .ok()?;

    let mut config = std::collections::HashMap::new();
    for (key, value) in rows {
        config.insert(key, value);
    }

    let host = config.get("smtp_host")?.as_str()?.trim().to_string();
    if host.is_empty() {
        // A BLANK host is not a mail server (kanban t_2e9117a5). The write gate allows an empty
        // value, so a blank host could be STORED as a PRESENT config (`as_str()` succeeds) and
        // lettre then failed on it: the tenant's mail silently stopped instead of falling back to
        // the platform mailer. Treated as "no tenant mail server of its own" here so such a row
        // can never park a send; the writer refuses to store one in the first place.
        tracing::warn!(%account_id,
            "tenant smtp_host is blank — this send uses the platform mail service");
        return None;
    }
    let username = config.get("smtp_username")?.as_str()?.to_string();
    let stored_password = config.get(SMTP_PASSWORD_KEY)?.as_str()?.to_string();
    let password = match open_smtp_password(pool, &stored_password).await {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(
                error = %e,
                %account_id,
                "tenant smtp_password cannot be opened by this deployment — not handing the \
                 envelope to the SMTP transport (falling back to the system mail provider)"
            );
            return None;
        }
    };
    let from_email = config.get("smtp_from_email")?.as_str()?.to_string();
    let port = config
        .get("smtp_port")
        .and_then(|v| v.as_i64())
        .unwrap_or(587) as u16;
    let from_name = config
        .get("smtp_from_name")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    Some(SmtpConfig {
        host,
        port,
        username,
        password,
        from_email,
        from_name,
    })
}

// The `provider_keys`-based Mailgun SMTP fallback that used to live here was RETIRED (kanban
// t_2e9117a5): it read a row that does not exist on this app (0 `provider_keys` rows with
// provider='mailgun', measured 2026-10-02) and hardcoded this app's identity, while the LIVE
// system-mail row is `admin_settings.email`. An account with no mail server now rides the
// platform mail service through the app's ONE system-mail path (`email_provider`) instead of
// failing behind a dead source.

/// The transport half of a tenant send: gate the host, build the message, hand it to lettre.
///
/// Split out of [`send_email`] so that the PANEL's test-send (kanban t_ba200ddf) can exercise the
/// tenant's OWN configuration WITHOUT the silent platform fallback — a test that fell through to
/// the platform provider would report on a mail server the operator never configured.
async fn deliver_via(
    pool: &PgPool,
    config: &SmtpConfig,
    to: &str,
    subject: &str,
    body_html: &str,
) -> Result<(), String> {
    // THE RECIPIENT-SIDE GUARD (kanban t_f56f4a79) — the same seam as the platform arm's, on a
    // tenant's own mail server: a recipient that provably cannot receive mail is refused before
    // the dial (no socket is opened for a message that can never arrive).
    let to = crate::security::email_addr::refuse_undeliverable_recipient(to)?;

    // The host is TENANT-SETTABLE (`tenant_settings` keys `smtp_*`, written by
    // `PUT /api/v1/settings`), so it is gated before any socket is opened (kanban t_f3c75b2a):
    // a private/reserved host is refused unless it is the platform's own preset for `smtp`.
    crate::security::webhook_security::gate_provider_endpoint_host(pool, "smtp", &config.host)
        .await
        .map_err(|reason| format!("SMTP host refused by security policy: {}", reason))?;

    // Build the email
    let from_name = config
        .from_name
        .clone()
        .unwrap_or_else(|| "IncentiveSwift".to_string());
    let email = Message::builder()
        .from(
            format!("{} <{}>", from_name, config.from_email)
                .parse()
                .map_err(|e: lettre::address::AddressError| {
                    format!("Invalid from address: {}", e)
                })?,
        )
        .to(to.parse().map_err(|e: lettre::address::AddressError| {
            format!("{INVALID_RECIPIENT_PREFIX} {}", e)
        })?)
        .subject(subject)
        .header(ContentType::TEXT_HTML)
        .body(body_html.to_string())
        .map_err(|e| format!("Failed to build email: {}", e))?;

    // Connect via STARTTLS
    let creds = Credentials::new(config.username.clone(), config.password.clone());

    let label = format!("{}:{}", config.host, config.port);
    let mailer = AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&config.host)
        .map_err(|e| format!("Invalid SMTP host: {}", e))?
        .port(config.port)
        .credentials(creds)
        // Bounds the TCP CONNECT. On lettre 0.11's tokio1 path this knob is used ONLY for the
        // connect (`client::async_net::try_connect` wraps `socket.connect(addr)` in
        // `tokio::time::timeout`); it does NOT set a read/write timeout on the async stream, so it
        // cannot bound a server that accepts the connection and then says nothing. That half is
        // [`send_with_deadline`]'s job.
        .timeout(Some(TENANT_SMTP_DEADLINE))
        .build();

    // Send, under the same deadline, so the WHOLE dial is bounded (kanban t_05b6efa2).
    send_with_deadline(mailer, email, &label, TENANT_SMTP_DEADLINE).await?;

    Ok(())
}

/// How long ONE tenant SMTP dial may take, end to end: TCP connect, STARTTLS handshake and the
/// SMTP dialogue (banner / EHLO / AUTH / MAIL / RCPT / DATA). The tenant's mail server is a
/// host:port the tenant types into Settings → Email that this box has no control over.
///
/// Why 10 s (kanban t_05b6efa2): it is this app's dominant outbound bound — `delivery/webhook.rs`,
/// `delivery/direct_api/*`, `handlers/sms_handler.rs` and `handlers/provider_keys_handler.rs` all
/// use 10 s; `delivery/output_actions.rs` and `state.rs` use 15 s; `email_provider.rs` 20 s. A mail
/// server that is up answers a banner in well under a second, so 10 s is generous for a real one
/// and is the difference between "the pane answers" and "the pane sits on Sending…".
///
/// Why it takes TWO bounds and not just lettre's `.timeout(...)` (read in the vendored lettre
/// 0.11.23 source): the builder's timeout reaches `AsyncSmtpTransportBuilder::info.timeout` →
/// `E::connect(server, timeout)` → `AsyncNetworkStream::connect_tokio1(..)`, which wraps ONLY
/// `socket.connect(addr)` in `tokio::time::timeout`. The async stream it then builds carries no
/// `set_read_timeout`/`set_write_timeout` (those exist on the SYNC path only, `client/net.rs`), so
/// the dialogue's `conn.read_response().await` is unbounded. The default is `Some(60 s)`
/// (`smtp::DEFAULT_TIMEOUT`): a BLACKHOLED SYN was therefore already answered at ~60 s, while a
/// server that ACCEPTS and then stays silent hung for ever — which is what the pane did (measured
/// pre-change: still pending at t+75 s against a silent sink).
pub const TENANT_SMTP_DEADLINE: Duration = Duration::from_secs(10);

/// Hand the message to lettre under `deadline`, so a server that never answers cannot park the
/// caller. `label` is `host:port`, named in the refusal so an operator — and the ticker's
/// `last_error`, and the console's "Not sent. …" line — reads WHICH server was abandoned.
async fn send_with_deadline(
    mailer: AsyncSmtpTransport<Tokio1Executor>,
    email: Message,
    label: &str,
    deadline: Duration,
) -> Result<(), String> {
    match tokio::time::timeout(deadline, mailer.send(email)).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(format!("SMTP send failed: {}", e)),
        Err(_elapsed) => Err(format!(
            "SMTP send failed: {} did not answer within {}s — the dial was abandoned at the bound \
             (TCP connect, STARTTLS or SMTP dialogue)",
            label,
            deadline.as_secs()
        )),
    }
}

/// Send an email through the account's OWN mail server, or — when it has none — through the
/// PLATFORM mail service (kanban t_2e9117a5).
///
/// The platform arm is [`crate::email::send_email_request`], the app's ONE system-mail path: the
/// `admin_settings.email` row the admin Email settings panel edits and tests. Before this, the
/// fallback read `provider_keys` (`provider = 'mailgun'`) — a row that has never existed on this
/// app (0 rows, measured 2026-10-02) — so "no tenant mail server" did not mean platform mail, it
/// meant `No SMTP configuration found` and a failed send. That is why the panel could not offer a
/// removal (there was nothing to return the account TO) and why queued lifecycle mail was
/// failing. A tenant's own server is still preferred, and is still the only thing this app hands
/// a tenant credential to.
pub async fn send_email(
    pool: &PgPool,
    account_id: Uuid,
    to: &str,
    subject: &str,
    body_html: &str,
) -> Result<(), String> {
    if let Some(config) = load_smtp_config(pool, account_id).await {
        return deliver_via(pool, &config, to, subject, body_html).await;
    }
    crate::email::send_email_request(pool, to, subject, body_html, body_html).await
}

/// The answer a tenant test-send gives when the account has no mail server of its own. It names
/// where to fix it, and deliberately says NOTHING about the platform provider: the platform
/// provider is not what this panel is testing (kanban t_ba200ddf).
pub const NO_TENANT_SMTP: &str = "No mail server saved for this account — fill in host, username, \
     from address and password in Settings → Email, save, then test again.";

/// Test the TENANT's OWN mail server, for real: load the tenant config and send one message
/// through it.
///
/// This is the arm `POST /api/v1/settings/email/test` drives (kanban t_ba200ddf). It deliberately
/// does NOT fall back to the platform provider: a test-send that silently used the platform's
/// mailer would answer `success` for a mail server the panel is not showing, which is the
/// "the panel lies" class this route exists to close. The [`SmtpConfig`] returned is the one that
/// ACKNOWLEDGED the message, so the caller can name the host it really used.
pub async fn test_tenant_smtp_config(
    pool: &PgPool,
    account_id: Uuid,
    to_email: &str,
) -> Result<SmtpConfig, String> {
    let config = load_smtp_config(pool, account_id)
        .await
        .ok_or_else(|| NO_TENANT_SMTP.to_string())?;
    deliver_via(
        pool,
        &config,
        to_email,
        "Test email from IncentiveSwift",
        "<h2>Your mail server works</h2><p>IncentiveSwift reached your SMTP server with the \
         credentials saved in Settings → Email and it accepted this message.</p>\
         <p>— IncentiveSwift</p>",
    )
    .await?;
    Ok(config)
}

/// Render {{key}} placeholders from a vars object.
///
/// Double braces ONLY — the vocabulary both admin surfaces advertise. A leftover is logged by
/// name (`template_render::warn_unsubstituted`) rather than silently mailed: kanban t_e43521d2.
pub fn render_template(template: &str, vars: &serde_json::Value) -> String {
    let mut result = template.to_string();
    if let Some(obj) = vars.as_object() {
        for (key, value) in obj {
            let placeholder = format!("{{{{{}}}}}", key);
            let replacement = match value {
                serde_json::Value::String(sv) => sv.clone(),
                other => other.to_string(),
            };
            result = result.replace(&placeholder, &replacement);
        }
    }
    crate::template_render::warn_unsubstituted(&result, "email template (sender)");
    result
}

/// The error text [`send_template_by_type`] returns when the row's `template_type` matches no
/// template. PUBLIC because it is one of the two pieces of vocabulary the ticker's fairness rule
/// classifies on (kanban t_96695538): a missing template is a property of the ROW, so it must not
/// defer the account's other rows. Kept as a const so the producer and the classifier cannot drift.
pub const NO_TEMPLATE_PREFIX: &str = "No email template found for type";

/// The error text [`deliver_via`] (and `crate::smtp::send_via_smtp`) returns for a recipient lettre
/// cannot parse. ROW-scoped for the same reason: the next row may be a perfectly good address.
pub const INVALID_RECIPIENT_PREFIX: &str = "Invalid to address:";

/// THE selection rule for "the template of this type, for this account" — the only
/// place in the app that decides it (kanban t_0fb81177).
///
/// Before this, the app had TWO answers to the same question and they disagreed:
/// * `delivery::sender` used the predicate below;
/// * `email::send_template_email` used `WHERE template_type = $1 AND (aid IS NULL OR
///   is_default = true) ORDER BY is_default ASC, created_at DESC` — it took NO account
///   argument at all, so every tenant's mail was rendered from one shared row.
///
/// MEASURED on the live DB (59 rows, all `aid IS NULL AND is_default = true`), by running both
/// predicates over the real `welcome` row UNIONed with the exact shapes
/// `POST /api/v1/email-templates` can create:
///
/// | row shape                              | old predicate    | new predicate (for tenant B) |
/// |----------------------------------------|------------------|------------------------------|
/// | `aid IS NULL, is_default = true`       | matches          | matches (fleet default)      |
/// | `aid = T,     is_default = false`      | EXCLUDED (dead)  | matches for T only           |
/// | `aid = T,     is_default = true`       | matches for ALL  | matches for T only           |
///
/// * the third row is the leak: with the account never consulted, tenant T's own
///   `is_default = true` row is selected for EVERY tenant (measured: asked for tenant B, the
///   old predicate answered `BBB-TENANT-A-FLAGGED-DEFAULT`), and `is_default ASC` put it
///   ahead of the fleet default.
/// * the second row is the same defect's other face: every template created through the
///   console lands `aid = <the tenant>, is_default = false` (the console never sends the
///   field), and the old predicate EXCLUDED it — so a tenant's own template was dead for
///   its own author as well.
///
/// `account_id = None` (a caller with no account in hand) therefore degrades to the
/// fleet-wide default, never to "whatever sorts first".
///
/// The ordering's first term is `aid IS NOT NULL AND aid = $2`, NOT `aid = $2`: for the
/// fleet-wide row `$2 = <a real account>` makes `aid = $2` NULL, and `ORDER BY ... DESC` puts
/// NULLs FIRST by default, so the fleet default would have outranked the account's OWN row
/// (measured: with `(aid = $2) DESC` the pick for the very tenant that owns the row was still
/// the fleet default). The `IS NOT NULL` guard makes the term a real boolean.
pub async fn load_template_by_type(
    pool: &PgPool,
    account_id: Option<Uuid>,
    template_type: &str,
) -> Result<Option<SelectedTemplate>, sqlx::Error> {
    sqlx::query_as::<_, SelectedTemplate>(
        "SELECT subject, body, html_body FROM email_templates
         WHERE template_type = $1 AND (aid = $2 OR (aid IS NULL AND is_default = true))
         ORDER BY (aid IS NOT NULL AND aid = $2) DESC, is_default DESC, created_at DESC
         LIMIT 1",
    )
    .bind(template_type)
    .bind(account_id)
    .fetch_optional(pool)
    .await
}

/// The three body columns of a selected template. `html_body` being present IS the
/// "this row is HTML" flag — `email_templates` has no `is_html` column.
#[derive(Debug, sqlx::FromRow)]
pub struct SelectedTemplate {
    pub subject: Option<String>,
    pub body: Option<String>,
    pub html_body: Option<String>,
}

/// Load an email template by type for the given account (account override first,
/// then global default), render vars, and send via the tenant's SMTP.
/// Returns Err if no template exists for that type OR no SMTP is configured.
pub async fn send_template_by_type(
    pool: &PgPool,
    account_id: Uuid,
    to: &str,
    template_type: &str,
    vars: &serde_json::Value,
) -> Result<(), String> {
    let row = load_template_by_type(pool, Some(account_id), template_type)
        .await
        .map_err(|e| format!("DB error loading template: {e}"))?
        .ok_or_else(|| format!("{NO_TEMPLATE_PREFIX} '{template_type}'"))?;

    let subject = row
        .subject
        .unwrap_or_else(|| format!("IncentiveSwift: {}", template_type));
    // Prefer html_body, fall back to body
    let body = row.html_body.or(row.body).unwrap_or_default();
    let subject = render_template(&subject, vars);
    let body = render_template(&body, vars);

    send_email(pool, account_id, to, &subject, &body).await
}

#[cfg(test)]
mod smtp_deadline_tests {
    use super::*;
    use lettre::message::Mailbox;
    use tokio::net::TcpListener;

    /// The case the tenant dial had to be fixed for (kanban t_05b6efa2): a server that ACCEPTS the
    /// TCP connection and then sends nothing — no banner, no RST. lettre's own `.timeout(...)` does
    /// NOT bound that on the tokio1 path (it only bounds the connect), so without the wrapper in
    /// `send_with_deadline` this dial never returns. The sink is a real socket; the bound is
    /// shortened so the test stays fast.
    #[tokio::test]
    async fn a_silent_sink_is_abandoned_at_the_deadline() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            // Accept, hold the socket open, never write: the "accepts and says nothing" sink.
            if let Ok((stream, _)) = listener.accept().await {
                tokio::time::sleep(Duration::from_secs(60)).await;
                drop(stream);
            }
        });

        let from: Mailbox = "probe@example.com".parse().unwrap();
        let to: Mailbox = "probe@example.com".parse().unwrap();
        let email = Message::builder()
            .from(from)
            .to(to)
            .subject("probe")
            .body("probe".to_string())
            .unwrap();
        let mailer = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous("127.0.0.1")
            .port(port)
            .timeout(Some(Duration::from_millis(200)))
            .build();

        let started = std::time::Instant::now();
        let err = send_with_deadline(
            mailer,
            email,
            "127.0.0.1:silent-sink",
            Duration::from_millis(400),
        )
        .await
        .expect_err("a silent sink is not a delivered message");
        assert!(
            err.contains("did not answer within"),
            "the refusal must name the bound, got: {err}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the dial was not bounded: {:?}",
            started.elapsed()
        );
    }
}
