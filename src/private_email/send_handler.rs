use axum::{extract::State, Extension, Json};
use sqlx::Row;
use uuid::Uuid;

use super::models::*;
use super::providers::{self, ProviderConfig};

use crate::access::claims::Claims;
use crate::error::{ApiResult, AppError};
use crate::state::AppState;

/// The ONE place mail leaves this account.
///
/// Every sender funnels here — `POST /send`, the reply arm of the inbox, and (later) automation —
/// so a message can never leave by a second route with different rules: the From must be one of
/// THIS account's active mailboxes, the signature is appended here and nowhere else, and the
/// provider/credential is resolved from the mailbox's own domain.
pub async fn deliver(
    db: &sqlx::PgPool,
    tenant_id: Uuid,
    from_address: &str,
    to: &str,
    cc: &[String],
    bcc: &[String],
    subject: &str,
    body: &str,
    in_reply_to: Option<&str>,
    include_signature: bool,
) -> Result<Option<String>, AppError> {
    let mailbox = sqlx::query_as::<_, PrivateEmailBox>(
        "SELECT * FROM private_email_boxes WHERE tenant_id = $1 AND email_address = $2 AND status = 'active'",
    )
    .bind(tenant_id)
    .bind(from_address)
    .fetch_optional(db)
    .await
    .map_err(crate::error::db_error)?
    .ok_or_else(|| AppError::NotFound("Sending mailbox not found or not active".into()))?;

    // ── MAY THIS ACCOUNT SEND? (CEO spec 2026-11-11) ───────────────────────────────────────────
    // The free PLAN sells receiving, not sending: a free mailbox sends freely for its trial window
    // and then goes receive-only. Both callers (`POST /send` and the inbox's reply/forward arm) come
    // through here, so the rule cannot be skipped by using the other door. Refusals are 402 with the
    // sentence the console prints, or 429 for the fair-use daily ceiling — never a 500.
    // Sending is granted with the plan's mailbox allowance on this app (see `feature_gate`),
    // so there is no separate trial meter to enforce here. Kept as a named seam: the two
    // callers (POST /send and reply) stay behind ONE gate instead of growing their own.
    let _ = (&mailbox, tenant_id);

    let provider_config = load_provider_config(db, mailbox.domain_id, tenant_id)
        .await
        .map_err(|e| AppError::Internal(format!("Failed to load provider config: {}", e)))?;

    // ── THE MESSAGE BODY ───────────────────────────────────────────────────────────────────────
    // The typed text is ESCAPED and its line breaks kept (it used to be forwarded as raw "html"),
    // then the signature block is appended — logo above the five plain fields — as an inline MIME
    // part so the bytes travel with the message.
    let mut body_html = super::signature::body_to_html(body);
    let mut body_text = body.to_string();
    let mut inline: Option<providers::InlineImage> = None;

    if include_signature {
        let workspace = super::signature::load_workspace(db, tenant_id)
            .await
            .map_err(crate::error::db_error)?;
        let own = super::signature::Signature {
            full_name: mailbox.signature_full_name.clone().unwrap_or_default(),
            job_title: mailbox.signature_job_title.clone().unwrap_or_default(),
            company: mailbox.signature_company.clone().unwrap_or_default(),
            phone: mailbox.signature_phone.clone().unwrap_or_default(),
            line: mailbox.signature_line.clone().unwrap_or_default(),
        };
        let resolved = super::signature::resolve(
            &mailbox.signature_mode,
            &own,
            mailbox.signature.as_deref(),
            &workspace.signature,
        );
        if !resolved.is_empty() {
            let (logo_src, logo_part) = match super::signature::send_logo(&workspace, &resolved) {
                Some((src, logo)) => (Some(src), Some(logo.clone())),
                None => (None, None),
            };
            body_html.push_str(&super::signature::html(
                &resolved.signature,
                resolved.legacy.as_deref(),
                logo_src,
                &workspace.rule_color,
            ));
            let lines = resolved.signature.text_lines(resolved.legacy.as_deref());
            if !lines.is_empty() {
                body_text = format!("{}\n\n--\n{}", body, lines.join("\n"));
            }
            if let Some(logo) = logo_part {
                inline = Some(providers::InlineImage {
                    content_id: super::signature::LOGO_FILENAME.to_string(),
                    content_type: logo.content_type,
                    bytes: logo.bytes,
                });
            }
        }
    }

    let provider = providers::provider_for(&provider_config);
    let result = provider
        .send(
            &provider_config,
            from_address,
            to,
            cc,
            bcc,
            subject,
            &body_text,
            &body_html,
            inline.as_ref(),
            in_reply_to,
        )
        .await;

    if !result.success {
        let err = result.error.unwrap_or_else(|| "Unknown send error".into());
        return Err(AppError::Internal(format!(
            "{} send failed: {}",
            provider.name(),
            err
        )));
    }

    Ok(result.provider_message_id)
}

/// Send an email via the domain's configured provider (Mailgun, SMTP, etc).
/// The provider is selected automatically based on the domain's provider_type.
pub async fn send_email(
    State(state): State<AppState>,
    Extension(claims): Extension<Claims>,
    Json(req): Json<SendEmailRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let account_id = Uuid::parse_str(&claims.aid)
        .map_err(|_| AppError::Unauthorized("Invalid session".into()))?;

    let provider_message_id = deliver(
        &state.db,
        account_id,
        &req.from_address,
        &req.to,
        &req.cc,
        &req.bcc,
        &req.subject,
        &req.body,
        req.in_reply_to.as_deref(),
        // Absent means "do what the composer showed": signed. Only an explicit false drops it.
        req.include_signature.unwrap_or(true),
    )
    .await?;
    let provider = "account-provider";

    // The answer is recorded in Sent, so the account's own outgoing mail is visible where a mail
    // client expects it. A failure to record is logged, never escalated: the mail HAS left, and
    // answering 500 would invite the caller to send it twice.
    let snippet: String = req.body.chars().take(500).collect();
    let mailbox_id: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM private_email_boxes
          WHERE tenant_id = $1 AND email_address = $2 AND status = 'active'",
    )
    .bind(account_id)
    .bind(&req.from_address)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();

    if let Err(e) = sqlx::query(
        r#"INSERT INTO private_email_messages (
              tenant_id, mailbox_id, address, folder, direction, from_address, to_address,
              subject, body_plain, snippet, in_reply_to, provider, provider_message_id,
              size_bytes, is_read, received_at
           ) VALUES ($1,$2,$3,'sent','outbound',$4,$5,$6,$7,$8,$9,$10,$11,$12,true,NOW())"#,
    )
    .bind(account_id)
    .bind(mailbox_id)
    .bind(&req.from_address)
    .bind(&req.from_address)
    .bind(&req.to)
    .bind(&req.subject)
    .bind(&req.body)
    .bind(&snippet)
    .bind(req.in_reply_to.as_deref())
    .bind(provider)
    .bind(provider_message_id.as_deref())
    .bind(req.body.len() as i32)
    .execute(&state.db)
    .await
    {
        tracing::warn!(error = %e, "mail was sent, but recording it in Sent failed");
    }

    // No contact match / event log on this app: IncentiveSwift has no `contacts` table and its
    // `events` rows are calendar entries (title/start_time), not a timeline. Recording the
    // send in `private_email_messages` (above) is the contract; the timeline arm is CoreSwift's.

    Ok(Json(serde_json::json!({
        "sent": true,
        "provider": provider,
        "provider_message_id": provider_message_id,
        "from": req.from_address,
        "to": req.to,
        "subject": req.subject,
    })))
}

/// Load provider configuration for a domain from the database.
pub async fn load_provider_config(
    db: &sqlx::PgPool,
    domain_id: Uuid,
    tenant_id: Uuid,
) -> Result<ProviderConfig, String> {
    let row = sqlx::query(
        r#"
        SELECT 
            d.provider_type, d.domain, d.mailgun_region, d.mailgun_api_key,
            d.smtp_host, d.smtp_port, d.smtp_username, d.smtp_password_encrypted,
            d.smtp_tls, d.inbound_mode, d.webhook_signing_key_encrypted,
            d.api_key_id, d.inbound_provider
        FROM private_email_domains d
        WHERE d.id = $1 AND d.tenant_id = $2
        "#,
    )
    .bind(domain_id)
    .bind(tenant_id)
    .fetch_optional(db)
    .await
    .map_err(|e| format!("Database error: {}", e))?
    .ok_or_else(|| "Domain not found".to_string())?;

    // Every field below keeps the fallback it needs (the live probe domain has NULL smtp_host /
    // smtp_port / smtp_username / smtp_password_encrypted / webhook_signing_key_encrypted, so a
    // propagating error would 500 sends that work today) - but the fallback is no longer SILENT:
    // a decode failure is logged with the column, so a schema drift cannot pass as a default
    // (kanban t_31ae74fa, same class as the row-level card t_5d7f823e).
    let provider_type: String = row
        .try_get("provider_type")
        .inspect_err(|e| tracing::warn!(column = "provider_type", error = %e, "decode failed"))
        .unwrap_or_else(|_| "mailgun".into());
    let region: Option<String> = row
        .try_get("mailgun_region")
        .inspect_err(|e| tracing::warn!(column = "mailgun_region", error = %e, "decode failed"))
        .ok();
    let encrypted_api_key: Option<String> = row
        .try_get("mailgun_api_key")
        .inspect_err(|e| tracing::warn!(column = "mailgun_api_key", error = %e, "decode failed"))
        .ok();

    // api_key_id is decoded ONCE. It used to be decoded twice in this statement (:140 to test, :149
    // to pass) - two silent defaults that could disagree with each other on a decode failure.
    let api_key_id: Option<Uuid> = row
        .try_get::<Option<Uuid>, _>("api_key_id")
        .inspect_err(|e| tracing::warn!(column = "api_key_id", error = %e, "decode failed"))
        .ok()
        .flatten();

    // For backward compat: if api_key_id is set, resolve the key
    let final_encrypted_key = if api_key_id.is_some() {
        // Resolve from the key table based on provider_type
        resolve_api_key(db, tenant_id, api_key_id, &provider_type).await
    } else {
        encrypted_api_key
    };

    // The five NULLABLE columns below are decoded as `Option<T>`: with `.ok()` alone, try_get
    // inferred the NON-Option `T` (the `.ok()` supplies the Option), so a NULL row raised
    // "unexpected null" on EVERY send and the fallback swallowed it - the app was using a
    // swallowed decode error as its NULL handling. As Option<T> a NULL is data (None, no log) and
    // a real type mismatch still logs (kanban t_31ae74fa).
    Ok(ProviderConfig {
        tenant_id,
        domain_id,
        domain: row
            .try_get("domain")
            .inspect_err(|e| tracing::warn!(column = "domain", error = %e, "decode failed"))
            .unwrap_or_default(),
        provider_type,
        encrypted_api_key: final_encrypted_key,
        region,
        smtp_host: row
            .try_get::<Option<String>, _>("smtp_host")
            .inspect_err(|e| tracing::warn!(column = "smtp_host", error = %e, "decode failed"))
            .ok()
            .flatten(),
        smtp_port: row
            .try_get::<Option<i32>, _>("smtp_port")
            .inspect_err(|e| tracing::warn!(column = "smtp_port", error = %e, "decode failed"))
            .ok()
            .flatten(),
        smtp_username: row
            .try_get::<Option<String>, _>("smtp_username")
            .inspect_err(|e| tracing::warn!(column = "smtp_username", error = %e, "decode failed"))
            .ok()
            .flatten(),
        encrypted_smtp_password: row
            .try_get::<Option<String>, _>("smtp_password_encrypted")
            .inspect_err(
                |e| tracing::warn!(column = "smtp_password_encrypted", error = %e, "decode failed"),
            )
            .ok()
            .flatten(),
        smtp_tls: row
            .try_get("smtp_tls")
            .inspect_err(|e| tracing::warn!(column = "smtp_tls", error = %e, "decode failed"))
            .unwrap_or(true),
        inbound_mode: row
            .try_get("inbound_mode")
            .inspect_err(|e| tracing::warn!(column = "inbound_mode", error = %e, "decode failed"))
            .unwrap_or_else(|_| "webhook".into()),
        encrypted_webhook_key: row
            .try_get::<Option<String>, _>("webhook_signing_key_encrypted")
            .inspect_err(|e| {
                tracing::warn!(
                    column = "webhook_signing_key_encrypted",
                    error = %e,
                    "decode failed"
                )
            })
            .ok()
            .flatten(),
        // HOW THIS DOMAIN RECEIVES (kanban t_04db9228) — a decode failure falls back to "mailgun",
        // which is what every row written before the column existed really is, and is logged rather
        // than swallowed like every other fallback in this function.
        inbound_provider: row
            .try_get::<String, _>("inbound_provider")
            .inspect_err(
                |e| tracing::warn!(column = "inbound_provider", error = %e, "decode failed"),
            )
            .unwrap_or_else(|_| "mailgun".into()),
    })
}

async fn resolve_api_key(
    db: &sqlx::PgPool,
    tenant_id: Uuid,
    api_key_id: Option<Uuid>,
    provider_type: &str,
) -> Option<String> {
    let kid = api_key_id?;
    match provider_type {
        "mailgun" => {
            let row = sqlx::query(
                "SELECT api_key_encrypted FROM private_email_api_keys WHERE id = $1 AND tenant_id = $2",
            )
            .bind(kid)
            .bind(tenant_id)
            .fetch_optional(db)
            .await
            .inspect_err(|e| {
                tracing::warn!(table = "private_email_api_keys", error = %e,
                    "key lookup failed: this domain resolves no API key from the key table")
            })
            .ok()??;
            Some(
                row.try_get::<String, _>("api_key_encrypted")
                    .inspect_err(|e| {
                        tracing::warn!(column = "api_key_encrypted", error = %e, "decode failed")
                    })
                    .ok()?,
            )
        }
        _ => {
            // Provider api keys (SES, Postmark)
            let row = sqlx::query(
                "SELECT api_key FROM provider_keys WHERE id = $1 AND tenant_id = $2 AND provider = $3 AND is_active",
            )
            .bind(kid)
            .bind(tenant_id)
            .bind(provider_type)
            .fetch_optional(db)
            .await
            .inspect_err(|e| {
                tracing::warn!(table = "provider_keys", error = %e,
                    "key lookup failed: this domain resolves no API key from the key table")
            })
            .ok()??;
            // provider_api_keys.access_key_encrypted is NULLABLE (information_schema: text, YES).
            // Decoding it as a non-Option String turned a NULL row into a decode error that `.ok()`
            // swallowed into "no key" - indistinguishable from a real mismatch, and invisible.
            // Decode it as the column actually is.
            row.try_get::<Option<String>, _>("access_key_encrypted")
                .inspect_err(|e| {
                    tracing::warn!(column = "access_key_encrypted", error = %e, "decode failed")
                })
                .ok()
                .flatten()
        }
    }
}

/// Low-level send via the domain's configured provider — used by auto-reply engine.
///
/// The auto-reply body is authored as HTML, so the plain-text alternative is derived from it here:
/// a text-only client (and a spam filter) should still read the sentence, not a tag soup.
pub async fn send_via_provider(
    db: &sqlx::PgPool,
    domain_id: Uuid,
    tenant_id: Uuid,
    from_address: &str,
    to: &str,
    subject: &str,
    body_html: &str,
) -> Result<(), String> {
    let config = load_provider_config(db, domain_id, tenant_id).await?;
    let provider = providers::provider_for(&config);
    let body_text = html_to_text(body_html);
    let result = provider
        .send(
            &config,
            from_address,
            to,
            &[],
            &[],
            subject,
            &body_text,
            body_html,
            None,
            None,
        )
        .await;

    if result.success {
        Ok(())
    } else {
        Err(result.error.unwrap_or_else(|| "Unknown send error".into()))
    }
}

// Backward compat alias — auto-reply engine calls this.
pub async fn send_via_mailgun(
    _base_url: &str,
    _api_key: &str,
    _domain: &str,
    _from_address: &str,
    _to: &str,
    _subject: &str,
    _body_html: &str,
) -> Result<(), String> {
    // This function is kept for backward compat with auto_reply_handler.
    // New code should call send_via_provider instead.
    // For now, forward to Mailgun provider directly.
    Err("send_via_mailgun is deprecated — use send_via_provider with db connection".into())
}

/// A crude HTML-to-text for the auto-reply arm: strip tags, unescape the four entities the templates
/// use, and collapse the whitespace the markup leaves behind. The auto-reply body is short and
/// authored by the customer, so this is enough to give a text-only reader the sentence.
fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    let out = out
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}
