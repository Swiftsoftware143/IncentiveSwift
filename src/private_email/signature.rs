//! The company signature: one logo, five plain fields, a workspace default and a per-mailbox choice
//! (kanban t_b671a0f7).
//!
//! David, 2026-11-11: *"one place in the workspace to upload the company logo … a plain signature
//! editor (name, title, company, phone, a line) renders below the logo, per mailbox with a workspace
//! default. Compose opens with both already in place so the user types subject and body only; a
//! toggle drops the signature for a given message; applies to replies as well; must look right on a
//! phone as well as desktop. Deleting/updating the logo never breaks already-sent mail."*
//!
//! # Where the pieces live
//!
//! * **Logo** — the workspace's one logo, bytes in `tenant_logos`, resized at upload by
//!   `crate::image_store::read_uploaded_image` and served by `GET /api/branding/logo/:tenant_id`.
//!   This module never writes it.
//! * **Workspace default** — `tenant_email_signatures`, one row per tenant, written only by
//!   `PUT /api/private-email/signature/workspace`.
//! * **Per-mailbox override** — `private_email_boxes.signature_mode` + five columns, written only by
//!   `PUT /api/private-email/signature` (the legacy free-text `signature` column is untouched: it is
//!   still appended, so nothing a customer already typed disappears).
//!
//! # The three mailbox states
//!
//! `inherit` (default) uses the workspace default; `custom` uses the mailbox's own fields and falls
//! back to the workspace default per blank field; `off` adds no signature at all (a `no-reply@` or a
//! support queue). The state is one dropdown a person can read, not a set of checkboxes that can
//! contradict each other.
//!
//! # How it reaches a message
//!
//! [`crate::private_email::send_handler::deliver`] is the ONE place mail leaves the account, so the
//! block is built there — compose, replies and forwards all inherit it and a second sender cannot
//! forget it. `include_signature: false` on the send/reply request drops it for that one message.
//!
//! # Why the logo is EMBEDDED, not linked
//!
//! The sent message carries the logo as an inline MIME part with a Content-ID, and the HTML points at
//! `cid:`. That is what makes "deleting or updating the logo never breaks already-sent mail" true:
//! the bytes travelled with the message, so a mailbox holding last month's email still draws last
//! month's logo after the workspace replaced or removed it. A remote `<img src>` would break exactly
//! those messages. It is also the most reliable rendering path — Gmail's "images blocked" default
//! still shows an inline part, and Outlook/Apple Mail draw PNG inline parts natively.

use axum::{
    extract::{Query, State},
    Extension, Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

use crate::access::claims::Claims;
use crate::error::{ApiResult, AppError};
use crate::state::AppState;

/// The longest a single signature field may be. Long enough for a legal company name and a phone
/// number with an extension, short enough that a field cannot become a paragraph in the layout.
pub const MAX_FIELD: usize = 120;
pub const MAX_LINE: usize = 160;

/// The Content-ID the logo travels under inside the message, and the part's filename.
///
/// Mailgun derives the Content-ID from the inline part's FILENAME, so the two must agree:
/// `cid:coreswift-logo.png` in the HTML is what both providers resolve.
pub const LOGO_CID: &str = "coreswift-logo.png";
pub const LOGO_FILENAME: &str = "coreswift-logo.png";

/// The five plain fields. Empty means "not set" everywhere in this module.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Signature {
    pub full_name: String,
    pub job_title: String,
    pub company: String,
    pub phone: String,
    pub line: String,
}

impl Signature {
    /// True when nothing would be printed for these fields.
    pub fn is_blank(&self) -> bool {
        [
            &self.full_name,
            &self.job_title,
            &self.company,
            &self.phone,
            &self.line,
        ]
        .iter()
        .all(|v| v.trim().is_empty())
    }

    /// Per-field fallback: the mailbox's own value when it has one, otherwise the workspace default.
    fn merged_over(&self, fallback: &Signature) -> Signature {
        let pick = |own: &str, dflt: &str| {
            let own = own.trim();
            if own.is_empty() {
                dflt.trim().to_string()
            } else {
                own.to_string()
            }
        };
        Signature {
            full_name: pick(&self.full_name, &fallback.full_name),
            job_title: pick(&self.job_title, &fallback.job_title),
            company: pick(&self.company, &fallback.company),
            phone: pick(&self.phone, &fallback.phone),
            line: pick(&self.line, &fallback.line),
        }
    }

    /// The plain-text form, appended to the body's text part so a text-only client sees it too.
    pub fn text_lines(&self, legacy: Option<&str>) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        if !self.full_name.trim().is_empty() {
            out.push(self.full_name.trim().to_string());
        }
        let mut role: Vec<String> = Vec::new();
        if !self.job_title.trim().is_empty() {
            role.push(self.job_title.trim().to_string());
        }
        if !self.company.trim().is_empty() {
            role.push(self.company.trim().to_string());
        }
        if !role.is_empty() {
            out.push(role.join(" · "));
        }
        if !self.phone.trim().is_empty() {
            out.push(self.phone.trim().to_string());
        }
        if !self.line.trim().is_empty() {
            out.push(self.line.trim().to_string());
        }
        if let Some(l) = legacy {
            for extra in l.lines().map(str::trim).filter(|s| !s.is_empty()) {
                out.push(extra.to_string());
            }
        }
        out
    }
}

/// The stored logo, as the send path needs it (bytes) and as the renderer needs it (size).
#[derive(Debug, Clone)]
pub struct Logo {
    pub content_type: String,
    pub bytes: Vec<u8>,
    pub width_px: Option<i32>,
    pub height_px: Option<i32>,
}

/// Everything a workspace has configured: the default signature, its logo and the branding bits the
/// block borrows (the rule colour and the alt text).
#[derive(Debug, Clone, Default)]
pub struct WorkspaceSignature {
    pub signature: Signature,
    /// The public URL the PREVIEW pane draws (same origin as both consoles). Sends never use it.
    pub logo_url: String,
    pub brand_name: String,
    pub rule_color: String,
    pub logo: Option<Logo>,
}

/// One mailbox's choice, resolved to what would actually be printed.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub mode: String,
    pub signature: Signature,
    /// The legacy free-text column, appended after the five fields.
    pub legacy: Option<String>,
}

impl Resolved {
    pub fn is_off(&self) -> bool {
        self.mode == "off"
    }
    /// True when there is nothing to add to a message.
    pub fn is_empty(&self) -> bool {
        self.is_off() || (self.signature.is_blank() && self.legacy_is_blank())
    }
    fn legacy_is_blank(&self) -> bool {
        self.legacy
            .as_deref()
            .map(|s| s.trim().is_empty())
            .unwrap_or(true)
    }
}

/// Read the workspace default, its logo and the branding colour in ONE call.
pub async fn load_workspace(
    db: &PgPool,
    tenant_id: Uuid,
) -> Result<WorkspaceSignature, sqlx::Error> {
    let row: Option<(String, String, String, String, String)> = sqlx::query_as(
        "SELECT full_name, job_title, company, phone, line
           FROM tenant_email_signatures WHERE tenant_id = $1",
    )
    .bind(tenant_id)
    .fetch_optional(db)
    .await?;

    let signature = match row {
        Some((full_name, job_title, company, phone, line)) => Signature {
            full_name,
            job_title,
            company,
            phone,
            line,
        },
        None => Signature::default(),
    };

    let branding = crate::branding::load(db, tenant_id).await;
    let (logo_url, brand_name, rule_color) = match branding {
        Some(b) => (
            b.logo_url.clone(),
            b.brand_name.clone(),
            b.effective_color().to_string(),
        ),
        None => (
            String::new(),
            String::new(),
            crate::branding::DEFAULT_BRAND_COLOR.to_string(),
        ),
    };

    #[allow(clippy::type_complexity)]
    let logo_row: Option<(String, Vec<u8>, Option<i32>, Option<i32>)> = sqlx::query_as(
        "SELECT content_type, bytes, NULL::int, NULL::int FROM tenant_logos WHERE tenant_id = $1",
    )
    .bind(tenant_id)
    .fetch_optional(db)
    .await?;

    Ok(WorkspaceSignature {
        signature,
        logo_url,
        brand_name,
        rule_color,
        logo: logo_row.map(|(content_type, bytes, width_px, height_px)| Logo {
            content_type,
            bytes,
            width_px,
            height_px,
        }),
    })
}

/// Resolve one mailbox's signature against the workspace default.
pub fn resolve(
    mode: &str,
    own: &Signature,
    legacy: Option<&str>,
    workspace: &Signature,
) -> Resolved {
    let signature = match mode {
        "off" => Signature::default(),
        "custom" => own.merged_over(workspace),
        // Anything unrecognised (a row written by hand) behaves like `inherit`, never like `off`:
        // a typo must not silently strip a customer's signature.
        _ => workspace.clone(),
    };
    // `off` means NOTHING is printed, so the legacy free-text lines go too — otherwise an address
    // set to send without a signature would still carry its old wording.
    let legacy = if mode == "off" {
        None
    } else {
        legacy
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.to_string())
    };
    Resolved {
        mode: if mode == "custom" || mode == "off" {
            mode.to_string()
        } else {
            "inherit".to_string()
        },
        signature,
        legacy,
    }
}

/// The size to draw a stored logo at, in CSS pixels.
///
/// The stored image is 2x the display size on purpose (see `image_store`), so a 440px-wide file is
/// drawn at 220px and stays sharp on a retina screen. A logo smaller than the display box keeps its
/// own size — stretching it up would only blur it.
pub fn display_size(stored_w: u32, stored_h: u32) -> (u32, u32) {
    let max_w: u32 = 220;
    let max_h: u32 = 88;
    if stored_w == 0 || stored_h == 0 {
        return (max_w, max_h);
    }
    if stored_w <= max_w && stored_h <= max_h {
        return (stored_w, stored_h);
    }
    let scale = f64::min(
        max_w as f64 / stored_w as f64,
        max_h as f64 / stored_h as f64,
    );
    (
        ((stored_w as f64) * scale).round().max(1.0) as u32,
        ((stored_h as f64) * scale).round().max(1.0) as u32,
    )
}

/// Where the logo `<img>` points and how big it is drawn.
pub struct LogoSrc {
    /// `cid:coreswift-logo.png` for a real send; the public URL for an on-screen preview.
    pub src: String,
    pub width: u32,
    pub height: u32,
    pub alt: String,
}

/// The signature block, or `""` when there is nothing to print.
///
/// Table-based markup with inline styles only: no `<style>` block, no external CSS and no flexbox,
/// because mail clients strip or ignore all three. Sizes are in CSS pixels with
/// `max-width:100%` on the image, which is what keeps a 220px logo from overflowing a 320px phone
/// screen; the `width`/`height` ATTRIBUTES are what Outlook uses before it has loaded the image.
pub fn html(
    sig: &Signature,
    legacy: Option<&str>,
    logo: Option<LogoSrc>,
    rule_color: &str,
) -> String {
    let lines = sig.text_lines(legacy);
    if lines.is_empty() && logo.is_none() {
        return String::new();
    }

    let esc = crate::branding::escape_html;
    let color = if crate::branding::is_hex_color(rule_color) {
        rule_color
    } else {
        crate::branding::DEFAULT_BRAND_COLOR
    };

    let mut logo_html = String::new();
    if let Some(l) = logo {
        logo_html = format!(
            "<tr><td style=\"padding:0 0 10px 0\">\
               <img src=\"{src}\" width=\"{w}\" height=\"{h}\" alt=\"{alt}\" \
                    style=\"display:block;width:{w}px;height:{h}px;max-width:100%;border:0;outline:none;text-decoration:none\">\
             </td></tr>",
            src = esc(&l.src),
            w = l.width,
            h = l.height,
            alt = esc(&l.alt),
        );
    }

    // Structured lines first, in the order a person reads a card; the legacy free-text lines follow
    // unchanged so an old signature keeps its wording.
    let mut body = String::new();
    if !sig.full_name.trim().is_empty() {
        body.push_str(&format!(
            "<div style=\"font-weight:600;color:#111827\">{}</div>",
            esc(sig.full_name.trim())
        ));
    }
    let mut role: Vec<String> = Vec::new();
    if !sig.job_title.trim().is_empty() {
        role.push(esc(sig.job_title.trim()));
    }
    if !sig.company.trim().is_empty() {
        role.push(esc(sig.company.trim()));
    }
    if !role.is_empty() {
        body.push_str(&format!(
            "<div style=\"color:#4b5563\">{}</div>",
            role.join(" · ")
        ));
    }
    if !sig.phone.trim().is_empty() {
        let phone = sig.phone.trim();
        // A tel: link is what makes a signature useful on the phone it is read on. Only characters
        // that can appear in a dial string are kept, so the href cannot become another scheme.
        let dial: String = phone
            .chars()
            .filter(|c| c.is_ascii_digit() || matches!(c, '+' | '-' | '(' | ')' | ' ' | '.'))
            .collect();
        if dial.chars().any(|c| c.is_ascii_digit()) {
            body.push_str(&format!(
                "<div style=\"color:#4b5563\"><a href=\"tel:{}\" style=\"color:#4b5563;text-decoration:none\">{}</a></div>",
                esc(dial.trim()),
                esc(phone)
            ));
        } else {
            body.push_str(&format!(
                "<div style=\"color:#4b5563\">{}</div>",
                esc(phone)
            ));
        }
    }
    if !sig.line.trim().is_empty() {
        body.push_str(&format!(
            "<div style=\"font-size:13px;color:#6b7280;margin-top:6px\">{}</div>",
            esc(sig.line.trim())
        ));
    }
    if let Some(l) = legacy {
        for extra in l.lines().map(str::trim).filter(|s| !s.is_empty()) {
            body.push_str(&format!(
                "<div style=\"font-size:13px;color:#6b7280\">{}</div>",
                esc(extra)
            ));
        }
    }
    let lines_html = if body.is_empty() {
        String::new()
    } else {
        format!(
            "<tr><td style=\"border-top:2px solid {color};padding:10px 0 0 0;font-size:14px;line-height:1.45\">{body}</td></tr>",
            color = esc(color),
            body = body
        )
    };

    format!(
        "<table role=\"presentation\" cellpadding=\"0\" cellspacing=\"0\" border=\"0\" \
           style=\"border-collapse:collapse;margin:20px 0 0 0;font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,sans-serif;color:#111827\">\
           {logo}{lines}\
         </table>",
        logo = logo_html,
        lines = lines_html
    )
}

/// The user's typed text as email HTML.
///
/// It is ESCAPED first: the previous code sent the plain-text body as the `html` part, so a body
/// containing `<` or `&` was interpreted as markup, and newlines collapsed into one paragraph. Both
/// are fixed here, and because every send goes through `deliver`, compose, reply and forward all get
/// the fix at once.
pub fn body_to_html(body: &str) -> String {
    let esc = crate::branding::escape_html(body);
    let with_breaks = esc.replace("\r\n", "\n").replace('\n', "<br>");
    format!(
        "<div style=\"font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,sans-serif;font-size:14px;line-height:1.55;color:#111827\">{}</div>",
        with_breaks
    )
}

// ── HTTP surface ────────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct MailboxSignatureQuery {
    /// The mailbox's address, e.g. `info@example.com`.
    #[serde(default)]
    pub mailbox: Option<String>,
}

/// `GET /api/private-email/signature?mailbox=<address>` — what Compose should show for that mailbox.
///
/// The HTML in the answer is built by the SAME renderer the send path uses (with the public logo URL
/// instead of the `cid:` reference), so the pane cannot describe a signature the mailer would not
/// emit.
pub async fn get_signature(
    State(s): State<AppState>,
    Extension(c): Extension<Claims>,
    Query(q): Query<MailboxSignatureQuery>,
) -> ApiResult<Json<Value>> {
    let tenant_id =
        Uuid::parse_str(&c.aid).map_err(|_| AppError::Unauthorized("Invalid session".into()))?;
    let workspace = load_workspace(&s.db, tenant_id).await?;

    let mailbox = match q
        .mailbox
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
    {
        Some(addr) => Some(load_mailbox_choice(&s.db, tenant_id, addr).await?),
        None => None,
    };

    let resolved = match &mailbox {
        Some(m) => resolve(
            &m.mode,
            &m.signature,
            m.legacy.as_deref(),
            &workspace.signature,
        ),
        None => Resolved {
            mode: "inherit".into(),
            signature: workspace.signature.clone(),
            legacy: None,
        },
    };

    let logo_src = preview_logo(&workspace, &resolved);
    let html = if resolved.is_empty() {
        String::new()
    } else {
        html(
            &resolved.signature,
            resolved.legacy.as_deref(),
            logo_src,
            &workspace.rule_color,
        )
    };

    Ok(Json(json!({
        "mailbox": mailbox.as_ref().map(|m| json!({
            "id": m.id,
            "email_address": m.email_address,
            "mode": resolved.mode,
            "own": m.signature,
        })),
        "workspace": workspace.signature,
        "resolved": resolved.signature,
        "legacy": resolved.legacy,
        "mode": resolved.mode,
        "has_signature": !resolved.is_empty(),
        "logo": workspace.logo.as_ref().map(|l| json!({
            "url": workspace.logo_url,
            "width_px": l.width_px,
            "height_px": l.height_px,
        })),
        "html": html,
        "detail": if resolved.is_off() {
            "This address sends without a signature."
        } else if resolved.is_empty() {
            "No signature yet. Set one in Email Branding, or write one for this address."
        } else {
            "This signature is added to every message from this address. Untick the box to leave it off one message."
        },
    })))
}

/// `PUT /api/private-email/signature` — save one mailbox's choice.
#[derive(Debug, Deserialize)]
pub struct MailboxSignatureBody {
    pub mailbox: String,
    /// inherit | custom | off
    pub mode: String,
    #[serde(default)]
    pub full_name: Option<String>,
    #[serde(default)]
    pub job_title: Option<String>,
    #[serde(default)]
    pub company: Option<String>,
    #[serde(default)]
    pub phone: Option<String>,
    #[serde(default)]
    pub line: Option<String>,
}

pub async fn put_signature(
    State(s): State<AppState>,
    Extension(c): Extension<Claims>,
    Json(req): Json<MailboxSignatureBody>,
) -> ApiResult<Json<Value>> {
    let tenant_id =
        Uuid::parse_str(&c.aid).map_err(|_| AppError::Unauthorized("Invalid session".into()))?;
    let mode = req.mode.trim().to_lowercase();
    if !matches!(mode.as_str(), "inherit" | "custom" | "off") {
        return Err(AppError::Validation(
            "Choose one of: use the workspace signature, write one for this address, or no signature."
                .into(),
        ));
    }
    let addr = req.mailbox.trim();
    if addr.is_empty() {
        return Err(AppError::Validation("Which address is this for?".into()));
    }

    let full_name = clean_field(req.full_name, MAX_FIELD, "Name")?;
    let job_title = clean_field(req.job_title, MAX_FIELD, "Title")?;
    let company = clean_field(req.company, MAX_FIELD, "Company")?;
    let phone = clean_field(req.phone, MAX_FIELD, "Phone")?;
    let line = clean_field(req.line, MAX_LINE, "Line")?;

    let updated: Option<(Uuid,)> = sqlx::query_as(
        "UPDATE private_email_boxes
            SET signature_mode = $3,
                signature_full_name = NULLIF(btrim($4), ''),
                signature_job_title = NULLIF(btrim($5), ''),
                signature_company   = NULLIF(btrim($6), ''),
                signature_phone     = NULLIF(btrim($7), ''),
                signature_line      = NULLIF(btrim($8), ''),
                updated_at = NOW()
          WHERE tenant_id = $1 AND email_address = $2
          RETURNING id",
    )
    .bind(tenant_id)
    .bind(addr)
    .bind(&mode)
    .bind(&full_name)
    .bind(&job_title)
    .bind(&company)
    .bind(&phone)
    .bind(&line)
    .fetch_optional(&s.db)
    .await?;

    let Some((id,)) = updated else {
        return Err(AppError::NotFound(
            "That address is not one of this workspace's mailboxes.".into(),
        ));
    };

    Ok(Json(json!({
        "saved": true,
        "mailbox_id": id,
        "mailbox": addr,
        "mode": mode,
        "detail": match mode.as_str() {
            "off" => "Saved. This address will send without a signature.",
            "custom" => "Saved. This address uses its own signature, with the workspace default filling any blank field.",
            _ => "Saved. This address uses the workspace signature.",
        },
    })))
}

/// `GET /api/private-email/signature/workspace` — the workspace default, for the Branding screen.
pub async fn get_workspace_signature(
    State(s): State<AppState>,
    Extension(c): Extension<Claims>,
) -> ApiResult<Json<Value>> {
    let tenant_id =
        Uuid::parse_str(&c.aid).map_err(|_| AppError::Unauthorized("Invalid session".into()))?;
    let workspace = load_workspace(&s.db, tenant_id).await?;
    let resolved = Resolved {
        mode: "inherit".into(),
        signature: workspace.signature.clone(),
        legacy: None,
    };
    let logo_src = preview_logo(&workspace, &resolved);
    let html = if resolved.is_empty() {
        String::new()
    } else {
        html(&resolved.signature, None, logo_src, &workspace.rule_color)
    };
    Ok(Json(json!({
        "signature": workspace.signature,
        "has_signature": !workspace.signature.is_blank(),
        "logo": workspace.logo.as_ref().map(|l| json!({
            "url": workspace.logo_url,
            "width_px": l.width_px,
            "height_px": l.height_px,
        })),
        "html": html,
    })))
}

/// `PUT /api/private-email/signature/workspace` — save the workspace default. The ONLY writer.
#[derive(Debug, Deserialize)]
pub struct WorkspaceSignatureBody {
    #[serde(default)]
    pub full_name: Option<String>,
    #[serde(default)]
    pub job_title: Option<String>,
    #[serde(default)]
    pub company: Option<String>,
    #[serde(default)]
    pub phone: Option<String>,
    #[serde(default)]
    pub line: Option<String>,
}

pub async fn put_workspace_signature(
    State(s): State<AppState>,
    Extension(c): Extension<Claims>,
    Json(req): Json<WorkspaceSignatureBody>,
) -> ApiResult<Json<Value>> {
    let tenant_id =
        Uuid::parse_str(&c.aid).map_err(|_| AppError::Unauthorized("Invalid session".into()))?;
    let full_name = clean_field(req.full_name, MAX_FIELD, "Name")?;
    let job_title = clean_field(req.job_title, MAX_FIELD, "Title")?;
    let company = clean_field(req.company, MAX_FIELD, "Company")?;
    let phone = clean_field(req.phone, MAX_FIELD, "Phone")?;
    let line = clean_field(req.line, MAX_LINE, "Line")?;

    sqlx::query(
        "INSERT INTO tenant_email_signatures
             (tenant_id, full_name, job_title, company, phone, line, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, NOW())
         ON CONFLICT (tenant_id) DO UPDATE
           SET full_name = EXCLUDED.full_name,
               job_title = EXCLUDED.job_title,
               company   = EXCLUDED.company,
               phone     = EXCLUDED.phone,
               line      = EXCLUDED.line,
               updated_at = NOW()",
    )
    .bind(tenant_id)
    .bind(&full_name)
    .bind(&job_title)
    .bind(&company)
    .bind(&phone)
    .bind(&line)
    .execute(&s.db)
    .await?;

    let workspace = load_workspace(&s.db, tenant_id).await?;
    Ok(Json(json!({
        "saved": true,
        "signature": workspace.signature,
        "has_signature": !workspace.signature.is_blank(),
        "detail": if workspace.signature.is_blank() {
            "Saved. The workspace has no signature, so a blank field stays blank in every message."
        } else {
            "Saved. This signature is now the default for every address, under your logo."
        },
    })))
}

// ── Internals ──────────────────────────────────────────────────────────────────────────────────

struct MailboxChoice {
    id: Uuid,
    email_address: String,
    mode: String,
    signature: Signature,
    legacy: Option<String>,
}

async fn load_mailbox_choice(
    db: &PgPool,
    tenant_id: Uuid,
    address: &str,
) -> Result<MailboxChoice, AppError> {
    let row = sqlx::query(
        "SELECT id, email_address, signature_mode,
                signature_full_name, signature_job_title, signature_company,
                signature_phone, signature_line, signature
           FROM private_email_boxes
          WHERE tenant_id = $1 AND email_address = $2",
    )
    .bind(tenant_id)
    .bind(address)
    .fetch_optional(db)
    .await?
    .ok_or_else(|| {
        AppError::NotFound("That address is not one of this workspace's mailboxes.".into())
    })?;

    use sqlx::Row;
    Ok(MailboxChoice {
        id: row.try_get("id")?,
        email_address: row.try_get("email_address")?,
        mode: row
            .try_get("signature_mode")
            .unwrap_or_else(|_| "inherit".into()),
        signature: Signature {
            full_name: row
                .try_get::<Option<String>, _>("signature_full_name")
                .ok()
                .flatten()
                .unwrap_or_default(),
            job_title: row
                .try_get::<Option<String>, _>("signature_job_title")
                .ok()
                .flatten()
                .unwrap_or_default(),
            company: row
                .try_get::<Option<String>, _>("signature_company")
                .ok()
                .flatten()
                .unwrap_or_default(),
            phone: row
                .try_get::<Option<String>, _>("signature_phone")
                .ok()
                .flatten()
                .unwrap_or_default(),
            line: row
                .try_get::<Option<String>, _>("signature_line")
                .ok()
                .flatten()
                .unwrap_or_default(),
        },
        legacy: row.try_get::<Option<String>, _>("signature").ok().flatten(),
    })
}

/// The `<img>` for an on-screen preview: the public logo URL, at the size it would be DRAWN.
fn preview_logo(workspace: &WorkspaceSignature, resolved: &Resolved) -> Option<LogoSrc> {
    let logo = workspace.logo.as_ref()?;
    if workspace.logo_url.trim().is_empty() || resolved.is_off() {
        return None;
    }
    let stored_w = logo.width_px.unwrap_or(0).max(0) as u32;
    let stored_h = logo.height_px.unwrap_or(0).max(0) as u32;
    let (w, h) = display_size(stored_w, stored_h);
    Some(LogoSrc {
        src: workspace.logo_url.clone(),
        width: w,
        height: h,
        alt: alt_text(workspace, resolved),
    })
}

/// The logo `<img>` for a REAL send: embedded bytes, addressed by Content-ID.
pub fn send_logo<'a>(
    workspace: &'a WorkspaceSignature,
    resolved: &Resolved,
) -> Option<(LogoSrc, &'a Logo)> {
    let logo = workspace.logo.as_ref()?;
    if resolved.is_off() {
        return None;
    }
    let stored_w = logo.width_px.unwrap_or(0).max(0) as u32;
    let stored_h = logo.height_px.unwrap_or(0).max(0) as u32;
    let (w, h) = display_size(stored_w, stored_h);
    Some((
        LogoSrc {
            src: format!("cid:{}", LOGO_CID),
            width: w,
            height: h,
            alt: alt_text(workspace, resolved),
        },
        logo,
    ))
}

fn alt_text(workspace: &WorkspaceSignature, resolved: &Resolved) -> String {
    let name = if !workspace.brand_name.trim().is_empty() {
        workspace.brand_name.trim().to_string()
    } else if !resolved.signature.company.trim().is_empty() {
        resolved.signature.company.trim().to_string()
    } else {
        "Logo".to_string()
    };
    name.chars().take(MAX_FIELD).collect()
}

/// Trim, drop control characters, and REFUSE an over-long field with a sentence a person can act on.
/// Truncating silently could cut a phone number in half.
fn clean_field(v: Option<String>, max: usize, label: &str) -> Result<String, AppError> {
    let raw = v.unwrap_or_default();
    let cleaned: String = raw
        .chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.chars().count() > max {
        return Err(AppError::Validation(format!(
            "{} is too long — {} characters at most.",
            label, max
        )));
    }
    Ok(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig(name: &str, company: &str) -> Signature {
        Signature {
            full_name: name.into(),
            company: company.into(),
            ..Default::default()
        }
    }

    #[test]
    fn an_inheriting_mailbox_uses_the_workspace_default_whole() {
        let ws = sig("Dana Reed", "Giraudy Capital");
        let r = resolve("inherit", &Signature::default(), None, &ws);
        assert_eq!(r.signature, ws);
        assert!(!r.is_empty());
    }

    #[test]
    fn a_custom_mailbox_falls_back_field_by_field() {
        let ws = sig("Dana Reed", "Giraudy Capital");
        let own = Signature {
            full_name: String::new(),
            job_title: "Sales".into(),
            ..Default::default()
        };
        let r = resolve("custom", &own, None, &ws);
        assert_eq!(r.signature.full_name, "Dana Reed");
        assert_eq!(r.signature.job_title, "Sales");
        assert_eq!(r.signature.company, "Giraudy Capital");
    }

    #[test]
    fn off_prints_nothing_even_with_a_workspace_default() {
        let ws = sig("Dana Reed", "Giraudy Capital");
        let r = resolve("off", &Signature::default(), Some("legacy"), &ws);
        assert!(r.is_off());
        assert!(r.is_empty());
        assert_eq!(html(&r.signature, r.legacy.as_deref(), None, "#000000"), "");
    }

    #[test]
    fn a_blank_signature_with_a_logo_still_prints_the_logo() {
        let logo = LogoSrc {
            src: "cid:coreswift-logo.png".into(),
            width: 100,
            height: 40,
            alt: "Acme".into(),
        };
        let h = html(&Signature::default(), None, Some(logo), "#4f46e5");
        assert!(h.contains("cid:coreswift-logo.png"));
        assert!(h.contains("width=\"100\""));
        assert!(h.contains("max-width:100%"));
    }

    #[test]
    fn every_field_is_escaped() {
        let s = Signature {
            full_name: "<script>alert(1)</script>".into(),
            company: "A & B".into(),
            phone: "javascript:alert(1)".into(),
            ..Default::default()
        };
        let h = html(&s, Some("<img src=x onerror=alert(1)>"), None, "#4f46e5");
        assert!(!h.contains("<script>"));
        assert!(!h.contains("<img"));
        // The visible text may say anything; what must never happen is a scheme reaching the HREF.
        assert!(!h.contains("href=\"javascript:"));
        assert!(h.contains("A &amp; B"));
    }

    #[test]
    fn a_phone_becomes_a_tel_link_with_dialable_characters_only() {
        let s = Signature {
            phone: "+1 (305) 555-0100 x12 <evil>".into(),
            ..Default::default()
        };
        let h = html(&s, None, None, "#4f46e5");
        assert!(h.contains("href=\"tel:+1 (305) 555-0100 12\""));
        assert!(h.contains("x12"));
        assert!(!h.contains("<evil>"));
    }

    #[test]
    fn stored_2x_is_drawn_at_half_and_a_small_logo_is_not_enlarged() {
        assert_eq!(display_size(440, 176), (220, 88));
        assert_eq!(display_size(120, 40), (120, 40));
        // 880x176 (a wide 2x logo) is drawn 220x44.
        assert_eq!(display_size(880, 176), (220, 44));
    }

    #[test]
    fn the_typed_body_is_escaped_and_keeps_its_line_breaks() {
        let h = body_to_html("Hi <b>there</b>\nsecond line");
        assert!(h.contains("&lt;b&gt;"));
        assert!(h.contains("<br>second line"));
        assert!(!h.contains("<b>"));
    }
}
