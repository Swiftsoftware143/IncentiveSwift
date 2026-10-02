//! IncentiveSwift site settings — the DB row is the SOURCE OF TRUTH; the static pages are
//! materialized by a HOST-side applier, never by the request path.
//!
//! WHY (kanban t_3fb0d3d2; same class ADASwift closed in t_1f427190, evidence
//! /opt/swift/audits/t_3fb0d3d2/)
//! --------------------------------------------------------------------------------------------
//! The marketing page + the three legal pages are plain files under
//! `/opt/swift/nginx/www/incentiveswift/` (served by nginx for incentiveswift.com). The service runs
//! in a container with ZERO mounts (`docker inspect incentiveswift --format '{{json .Mounts}}'` ->
//! `[]`), so `PUT /api/v1/admin/site` could never write them: it UPSERTed the `admin_settings` row
//! FIRST, then died in `regenerate_html`, answering 500 *after* the write had landed (measured
//! live: PUT `{}` -> 500 while the row's `updated_at` moved, log line "Failed to read
//! /opt/swift/nginx/www/incentiveswift/index.html: No such file or directory"). An admin saw a red
//! toast for a save that had in fact committed, and `regenerate_legal` could never run at all.
//!
//! The two arms were (a) mount the served root into the container and let the request path write it,
//! or (b) retire the request-path write. Arm (b), the way ADASwift proved it (t_1f427190) and
//! CoreSwift-CRM before that (t_02986434): the row is the source of truth, `update_site` writes
//! ONLY the row and answers 2xx naming the applier, and `/opt/swift/bin/is-site-apply.sh` runs
//! `incentiveswift-api apply-site-settings` on the HOST (where the files actually are) from cron.
//! Arm (a) was rejected because the served root is also the target of the repo->served publish gate
//! (/opt/swift/fleet/marketing-www-parity.py): a container that writes it makes two writers for one
//! tree with no reconciliation, and it would require a root-owned host path mounted read-write into
//! a service running as an unprivileged uid.
//!
//! INVARIANTS THIS MODULE KEEPS
//! --------------------------------------------------------------------------------------------
//! * No request path writes a file. `update_site` = one UPSERT + 2xx.
//! * A blank `legal_*` can never downgrade a published page: the applier guards on the VALUE
//!   (`trim().is_empty()` -> skip + reason), and `preserve_nonblank` refuses the same blank at the
//!   STORE, so the panel's GET-then-PUT round trip (GET merges the code defaults, which carry `""`)
//!   cannot blank the row either.
//! * The applier is idempotent: a file is only rewritten when its bytes would change, so a scheduled
//!   run is free and the repo/served parity gate stays quiet.
//! * Addresses are escaped at RENDER time (`@` -> `&#64;`), the fleet's served-page convention
//!   (docs/fleet-marketing-www.md §6.1, kanban t_d4347fb5 / t_cda04aec): the DB holds the human form,
//!   the rendered page carries the entity. The three published legal pages carry `&#64;` and the
//!   DB carries `@`, so this is also what makes `render(DB)` byte-identical to what is served.
use axum::extract::{Json, State};
use serde_json::json;
use std::fs;
use uuid::Uuid;

use sqlx::Row;

use crate::error::AppError;
use crate::security::auth::AuthenticatedUser;
use crate::state::AppState;

const SITE_KEY: &str = "incentiveswift_site";

/// Where the static marketing page and the three legal pages live. These are HOST paths: the app
/// runs in a container with ZERO mounts, so the only process that can write them is the same binary
/// executed on the host (`incentiveswift-api apply-site-settings`, driven by
/// /opt/swift/bin/is-site-apply.sh).
pub(crate) const SITE_ROOT: &str = "/opt/swift/nginx/www/incentiveswift/";
pub(crate) const SITE_INDEX: &str = "/opt/swift/nginx/www/incentiveswift/index.html";

/// The legal pages this applier owns: (slug, <h1>/<title> text, settings key, bytes after the body).
///
/// The trailing field is the exact closing bytes of the published page. It is a per-page constant
/// and not a shared literal because the reconciliation must make `render(DB)` byte-identical to what
/// is SERVED - a one-byte difference would make the first applier run rewrite a live legal page.
pub(crate) const LEGAL_PAGES: [(&str, &str, &str, &str); 3] = [
    (
        "terms",
        "Terms of Service",
        "legal_tos",
        "</div></body></html>\n",
    ),
    (
        "privacy",
        "Privacy Policy",
        "legal_privacy",
        "</div></body></html>\n",
    ),
    (
        "refunds",
        "Refund & Cancellation Policy",
        "legal_refunds",
        "</div></body></html>\n",
    ),
];

/// The legal keys `preserve_nonblank` protects at the store.
const LEGAL_KEYS: [&str; 3] = ["legal_tos", "legal_privacy", "legal_refunds"];

/// GET /api/v1/admin/site — get site settings (SEO, tracking, homepage, legal)
pub async fn get_site(
    State(state): State<AppState>,
    _auth: AuthenticatedUser,
) -> Result<Json<serde_json::Value>, AppError> {
    let settings = load_settings(&state.db).await?;
    Ok(Json(settings))
}

/// The stored settings merged over the code defaults — the same value `get_site` serves, and the
/// input to the host-side applier.
pub(crate) async fn load_settings(db: &sqlx::PgPool) -> Result<serde_json::Value, AppError> {
    let defaults = default_site_settings();

    let row = sqlx::query("SELECT value FROM admin_settings WHERE key = $1")
        .bind(SITE_KEY)
        .fetch_optional(db)
        .await?;

    Ok(match row {
        Some(r) => {
            let val: serde_json::Value = r.try_get("value")?;
            merge_json(defaults, val)
        }
        None => defaults,
    })
}

/// PUT /api/v1/admin/site — save site settings.
///
/// Deliberately NO file writes on this path. The static pages are HOST paths and this service runs
/// in a container with no mount for them, so writing them here could only ever answer 500 — after
/// the row above had already committed, and an admin then saw a failure for a save that had in fact
/// landed (kanban t_3fb0d3d2: measured PUT `{}` -> 500 with the row's `updated_at` already moved).
/// The row IS the source of truth (`get_site` reads it); the pages are materialized by the
/// host-side applier, which is their only writer.
pub async fn update_site(
    State(state): State<AppState>,
    _auth: AuthenticatedUser,
    Json(req): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, AppError> {
    // Merge with existing
    let existing_row = sqlx::query("SELECT value FROM admin_settings WHERE key = $1")
        .bind(SITE_KEY)
        .fetch_optional(&state.db)
        .await?;

    let existing: Option<serde_json::Value> = match existing_row {
        Some(r) => Some(r.try_get("value")?),
        None => None,
    };

    let merged = match &existing {
        Some(val) => merge_json(val.clone(), req),
        None => req,
    };
    // The store guard over every RENDERED text field (the three legal bodies).
    let merged = preserve_nonblank(existing.as_ref(), merged, &LEGAL_KEYS);

    let admin_id = Uuid::parse_str(&_auth.account_id).unwrap_or(Uuid::nil());

    sqlx::query(
        r#"INSERT INTO admin_settings (key, value, description, updated_at, updated_by)
           VALUES ($1, $2::jsonb, 'IncentiveSwift site settings (SEO, tracking, homepage, legal)', NOW(), $3)
           ON CONFLICT (key) DO UPDATE SET value = $2::jsonb, updated_at = NOW(), updated_by = $3"#,
    )
    .bind(SITE_KEY)
    .bind(merged.to_string())
    .bind(admin_id)
    .execute(&state.db)
    .await?;

    Ok(Json(json!({
        "message": "Site settings saved",
        "settings": merged,
        "static_pages": {
            "writer": "/opt/swift/bin/is-site-apply.sh (incentiveswift-api apply-site-settings)",
            "within_minutes": 5
        }
    })))
}

/// A blank string (or null, or a missing key) is how "the operator cleared this field" and
/// "this form was rendered from a GET that merged the code defaults" look identical on the wire —
/// and the defaults carry `""` for all three legal keys. Refuse the blank at the STORE when the row
/// already holds text, so the panel's GET-then-PUT round trip can never blank a live policy's source
/// text. (The applier guards the same value at the render, so the page is protected twice over.)
fn preserve_nonblank(
    existing: Option<&serde_json::Value>,
    mut merged: serde_json::Value,
    keys: &[&str],
) -> serde_json::Value {
    for key in keys {
        let stored_has_text = existing
            .and_then(|e| e.get(key))
            .map(|v| !is_blank(Some(v)))
            .unwrap_or(false);
        if stored_has_text && is_blank(merged.get(key)) {
            if let (Some(dst), Some(src)) = (merged.get_mut(key), existing.and_then(|e| e.get(key)))
            {
                *dst = src.clone();
                tracing::warn!(key, "blank value refused: the stored text was preserved");
            }
        }
    }
    merged
}

fn is_blank(v: Option<&serde_json::Value>) -> bool {
    match v {
        None | Some(serde_json::Value::Null) => true,
        Some(serde_json::Value::String(s)) => s.trim().is_empty(),
        Some(_) => false,
    }
}

/// `(path, rendered bytes)` for every file the applier owns and has a value for.
pub(crate) type PlanTargets = Vec<(String, String)>;
/// `(path, reason)` for every file deliberately left alone.
pub(crate) type PlanSkips = Vec<(String, String)>;

/// Render every file this applier owns, WITHOUT touching the disk.
///
/// Returns `(targets, skipped)`: `targets` is `(path, rendered bytes)` for every file whose source
/// value exists, `skipped` is `(path, reason)` for the ones deliberately left alone — a blank legal
/// value lands here, never in `targets`.
pub(crate) fn plan(settings: &serde_json::Value) -> (PlanTargets, PlanSkips) {
    let mut targets: PlanTargets = Vec::new();
    let mut skipped: PlanSkips = Vec::new();

    if !std::path::Path::new(SITE_ROOT).is_dir() {
        skipped.push((
            SITE_ROOT.to_string(),
            "directory is not present in this runtime (host-only path)".to_string(),
        ));
        return (targets, skipped);
    }

    match fs::read_to_string(SITE_INDEX) {
        Ok(before) => targets.push((SITE_INDEX.to_string(), render_index(&before, settings))),
        Err(e) => skipped.push((SITE_INDEX.to_string(), format!("unreadable: {}", e))),
    }

    for (slug, title, key, tail) in LEGAL_PAGES {
        let path = format!("{}{}.html", SITE_ROOT, slug);
        match settings.get(key).and_then(|v| v.as_str()) {
            // Blank or absent means "no policy text configured" -> leave the published page alone.
            Some(text) if !text.trim().is_empty() => {
                targets.push((path, legal_page(title, &escape_addresses(text), tail)));
            }
            _ => skipped.push((
                path,
                format!(
                    "{} is blank or absent - the published page is left alone",
                    key
                ),
            )),
        }
    }

    (targets, skipped)
}

/// Materialize `settings` into the static marketing page + the three legal pages.
///
/// Returns `(written, skipped)`; every skipped entry is `(path, reason)`. Idempotent: a file is only
/// rewritten when its bytes would change, and nothing here is fatal — the caller reports the outcome
/// so no surface can claim a regeneration that did not happen.
pub(crate) fn apply_to_disk(settings: &serde_json::Value) -> (Vec<String>, PlanSkips) {
    let (targets, mut skipped) = plan(settings);
    let mut written: Vec<String> = Vec::new();

    for (path, rendered) in targets {
        match fs::read_to_string(&path) {
            Ok(before) if before == rendered => skipped.push((path, "unchanged".to_string())),
            _ => match fs::write(&path, rendered.as_bytes()) {
                Ok(_) => written.push(path),
                Err(e) => skipped.push((path, e.to_string())),
            },
        }
    }

    (written, skipped)
}

/// The marketing page, as a pure function of the bytes already on disk (so the applier can compare
/// before writing).
fn render_index(before: &str, settings: &serde_json::Value) -> String {
    inject_site_settings(before, settings)
}

/// One legal page, as a pure function so the applier can compare before writing.
///
/// The wrapper is the PUBLISHED template — it is reproduced here byte-for-byte (including the second
/// `<style>` block that carries `.updated`/`.back`, which the older in-repo template did not have).
/// Reconciliation note (kanban t_3fb0d3d2): the in-repo template was STALE relative to the pages that
/// are actually served, so a verbatim port would have rewritten all three live legal pages on the
/// first apply. The served bytes won; the template and the reconciled row both follow them.
fn legal_page(title: &str, body: &str, tail: &str) -> String {
    format!(
        r#"<!DOCTYPE html><html lang="en">
<head><meta charset="UTF-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>{} — IncentiveSwift</title>
<style>body{{font-family:system-ui,sans-serif;background:#0f0f0f;color:#e5e5e5;line-height:1.7;margin:0;padding:0}}
.container{{max-width:800px;margin:0 auto;padding:60px 24px}}
h1{{font-size:2rem;color:#8b5cf6}}a{{color:#8b5cf6}}</style>
<style>h1{{margin-bottom:8px}}
h2{{font-size:1.2rem;color:#ffffff;margin:28px 0 10px}}
p,li{{color:#9ca3af;margin:10px 0}}
.updated{{color:#6b7280;font-size:13px;margin-bottom:24px}}
.back{{margin-top:40px;padding-top:20px;border-top:1px solid rgba(255,255,255,.08);font-size:14px}}</style>
</head><body><div class="container">
<h1>{}</h1>
{}{}"#,
        title,
        escape_ampersand(title),
        body,
        tail
    )
}

/// Escape `&` as `&amp;` for an HTML text node. The published legal pages carry their `<h1>` escaped
/// (`<h1>Refund &amp; Cancellation Policy</h1>`) while the `<title>` element carries the same text
/// raw, so the two substitutions are deliberately NOT the same string — that is what reproduces the
/// served bytes.
fn escape_ampersand(text: &str) -> String {
    text.replace('&', "&amp;")
}

/// Write every address in an HTML body as the entity `&#64;`.
///
/// The served-page convention for this fleet: a literal address in a served HTML page is rewritten
/// per-request by the edge (Cloudflare) and is flagged as a hazard by the repo/served parity gate, so
/// an address in page TEXT is authored as the entity, which renders identically and is left alone
/// (docs/fleet-marketing-www.md §6.1, kanban t_d4347fb5 / t_cda04aec). The DB holds the human form
/// (`support@swiftsoftware.net`) because that is what an operator types and reads in the panel; the
/// applier is the single enforcement point that turns it into the entity on the page.
///
/// The shape matched is the gate's own ADDRESS regex: `[A-Za-z0-9._%+-]+@([A-Za-z0-9-]+\.)+[A-Za-z]{2,}`.
fn escape_addresses(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    while i < b.len() {
        if b[i] == b'@' && looks_like_address(b, i) {
            out.push_str("&#64;");
            i += 1;
        } else {
            // Copy one full UTF-8 char, never a byte: the legal text is prose and may carry any
            // character.
            let ch = match text[i..].chars().next() {
                Some(c) => c,
                None => break,
            };
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

fn looks_like_address(b: &[u8], at: usize) -> bool {
    // Local part: at least one local char immediately before the '@'.
    let mut ls = at;
    while ls > 0 && is_local_char(b[ls - 1]) {
        ls -= 1;
    }
    if ls == at {
        return false;
    }

    // Domain: a run of domain chars, at least one dot, and an alphabetic TLD of 2+ chars.
    //
    // The run may end in sentence punctuation that IS a domain char — a policy sentence ends
    // `...contact support@swiftsoftware.net.` and the '.' belongs to the sentence, not the domain.
    // The gate's own regex ends on an alphabetic label, so the trailing dots/hyphens are trimmed
    // before the labels are validated; otherwise the address would be skipped and a literal '@'
    // would be published (measured on ADASwift terms/privacy, kanban t_1f427190).
    let mut de = at + 1;
    while de < b.len() && is_domain_char(b[de]) {
        de += 1;
    }
    while de > at + 1 && (b[de - 1] == b'.' || b[de - 1] == b'-') {
        de -= 1;
    }
    let domain = match std::str::from_utf8(&b[at + 1..de]) {
        Ok(d) => d,
        Err(_) => return false,
    };
    let parts: Vec<&str> = domain.split('.').collect();
    if parts.len() < 2 {
        return false;
    }
    let tld = parts[parts.len() - 1];
    if tld.len() < 2 || !tld.bytes().all(|c| c.is_ascii_alphabetic()) {
        return false;
    }
    parts
        .iter()
        .all(|p| !p.is_empty() && !p.starts_with('-') && !p.ends_with('-'))
}

fn is_local_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'%' | b'+' | b'-')
}

fn is_domain_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'.' || c == b'-'
}

fn inject_site_settings(html: &str, s: &serde_json::Value) -> String {
    let mut result = html.to_string();

    if let Some(title) = s.get("title").and_then(|v| v.as_str()) {
        replace_title(&mut result, title);
    }
    if let Some(desc) = s.get("description").and_then(|v| v.as_str()) {
        upsert_meta(&mut result, "description", desc);
    }
    if let Some(kw) = s.get("keywords").and_then(|v| v.as_str()) {
        upsert_meta(&mut result, "keywords", kw);
    }
    upsert_meta_prop(
        &mut result,
        "og:title",
        s.get("og_title").and_then(|v| v.as_str()),
    );
    upsert_meta_prop(
        &mut result,
        "og:description",
        s.get("og_description").and_then(|v| v.as_str()),
    );
    upsert_meta_prop(
        &mut result,
        "og:image",
        s.get("og_image_url").and_then(|v| v.as_str()),
    );

    if let Some(schema_json) = s.get("schema_json").and_then(|v| v.as_str()) {
        if !schema_json.is_empty() {
            upsert_schema(&mut result, schema_json);
        }
    }

    let ga_id = s.get("ga_id").and_then(|v| v.as_str()).unwrap_or("");
    let gtm_id = s.get("gtm_id").and_then(|v| v.as_str()).unwrap_or("");
    remove_ga_gtm(&mut result);

    if !ga_id.is_empty() {
        let ga_script = format!(
            r#"<script async src="https://www.googletagmanager.com/gtag/js?id={}"></script><script>window.dataLayer=window.dataLayer||[];function gtag(){{dataLayer.push(arguments);}}gtag('js',new Date());gtag('config','{}');</script>"#,
            ga_id, ga_id
        );
        inject_before_head_end(&mut result, &ga_script);
    }
    if !gtm_id.is_empty() {
        let gtm_head = format!(
            r#"<script>(function(w,d,s,l,i){{w[l]=w[l]||[];w[l].push({{'gtm.start':new Date().getTime(),event:'gtm.js'}});var f=d.getElementsByTagName(s)[0],j=d.createElement(s);j.async=true;j.src='https://www.googletagmanager.com/gtm.js?id='+i;f.parentNode.insertBefore(j,f);}})(window,document,'script','dataLayer','{}');</script>"#,
            gtm_id
        );
        inject_before_head_end(&mut result, &gtm_head);
    }
    if let Some(head_scripts) = s.get("head_scripts").and_then(|v| v.as_str()) {
        if !head_scripts.is_empty() {
            inject_before_head_end(&mut result, head_scripts);
        }
    }
    if let Some(body_scripts) = s.get("body_scripts").and_then(|v| v.as_str()) {
        if !body_scripts.is_empty() {
            inject_before_body_end(&mut result, body_scripts);
        }
    }

    result
}

fn replace_title(result: &mut String, new_title: &str) {
    let open = "<title>";
    let close = "</title>";
    if let Some(start) = result.find(open) {
        let after_open = start + open.len();
        if let Some(end) = result[after_open..].find(close) {
            result.replace_range(after_open..after_open + end, new_title);
        }
    } else {
        inject_before_head_end(result, &format!("<title>{}</title>", new_title));
    }
}

fn upsert_meta(result: &mut String, name: &str, content: &str) {
    let pattern = format!(r#"<meta name="{}""#, name);
    if let Some(pos) = result.find(&pattern) {
        let after = &result[pos..];
        if let Some(end) = after.find('>') {
            result.replace_range(
                pos..pos + end + 1,
                &format!(r#"<meta name="{}" content="{}">"#, name, content),
            );
        }
    } else {
        inject_before_head_end(
            result,
            &format!(r#"<meta name="{}" content="{}">"#, name, content),
        );
    }
}

fn upsert_meta_prop(result: &mut String, property: &str, content: Option<&str>) {
    if let Some(c) = content {
        let pattern = format!(r#"<meta property="{}""#, property);
        if let Some(pos) = result.find(&pattern) {
            let after = &result[pos..];
            if let Some(end) = after.find('>') {
                result.replace_range(
                    pos..pos + end + 1,
                    &format!(r#"<meta property="{}" content="{}">"#, property, c),
                );
            }
        } else {
            inject_before_head_end(
                result,
                &format!(r#"<meta property="{}" content="{}">"#, property, c),
            );
        }
    }
}

fn upsert_schema(result: &mut String, schema_json: &str) {
    let open = r#"<script type="application/ld+json">"#;
    let close = r#"</script>"#;
    if let Some(start) = result.find(open) {
        let after_open = start + open.len();
        if let Some(end) = result[after_open..].find(close) {
            result.replace_range(after_open..after_open + end, schema_json);
        }
    } else {
        inject_before_head_end(
            result,
            &format!(
                r#"<script type="application/ld+json">{}</script>"#,
                schema_json
            ),
        );
    }
}

fn remove_ga_gtm(result: &mut String) {
    let patterns = [
        (
            r#"<script async src="https://www.googletagmanager.com/gtag/js"#,
            "</script>",
        ),
        (r#"<script>window.dataLayer=window.dataLayer"#, "</script>"),
        (
            r#"<script>(function(w,d,s,l,i){w[l]=w[l]||[];w[l].push"#,
            "</script>",
        ),
        (
            r#"<noscript><iframe src="https://www.googletagmanager.com/ns.html"#,
            "</noscript>",
        ),
    ];
    for (start_pat, end_pat) in &patterns {
        loop {
            if let Some(pos) = result.find(start_pat) {
                if let Some(end) = result[pos..].find(end_pat) {
                    result.replace_range(pos..pos + end + end_pat.len(), "");
                    continue;
                }
            }
            break;
        }
    }
    while result.contains("\n\n\n") {
        *result = result.replace("\n\n\n", "\n\n");
    }
}

/// Insert `content` immediately before the closing tag at `pos`.
///
/// IDEMPOTENCY (kanban t_3fb0d3d2): the first cut always prefixed the content with `"\n  "`, which is
/// only correct when the closing tag starts a fresh line. On the PUBLISHED page the tag is preceded
/// by the whitespace remnant of the GA block a previous write removed (`...</style>\n\n  \n  \n  </head>`),
/// so that prefix turned the remnant into its own whitespace-only line and every write ADDED one more
/// byte of drift - `apply-site-settings --check` on the untouched live page read `would-write` with a
/// ONE-LINE diff. The rule now: if everything between the newline and `pos` is spaces/tabs, that
/// indentation is kept and the content goes right after it, so a head this renderer already wrote is
/// reproduced byte-for-byte on the next run (and the published bytes are reproduced exactly as they
/// are today). Only a tag with no indentation in front of it gets the `"\n  "` prefix.
fn insert_before_tag(result: &mut String, pos: usize, content: &str) {
    let mut ws = pos;
    while ws > 0 && matches!(result.as_bytes()[ws - 1], b' ' | b'\t') {
        ws -= 1;
    }
    if ws < pos {
        result.insert_str(pos, content);
    } else {
        result.insert_str(pos, &format!("\n  {}", content));
    }
}

fn inject_before_head_end(result: &mut String, content: &str) {
    if let Some(pos) = result.rfind("</head>") {
        insert_before_tag(result, pos, content);
    }
}

fn inject_before_body_end(result: &mut String, content: &str) {
    if let Some(pos) = result.rfind("</body>") {
        insert_before_tag(result, pos, content);
    }
}

fn merge_json(a: serde_json::Value, b: serde_json::Value) -> serde_json::Value {
    match (a, b) {
        (serde_json::Value::Object(mut a_map), serde_json::Value::Object(b_map)) => {
            for (k, v) in b_map {
                a_map.insert(k, v);
            }
            serde_json::Value::Object(a_map)
        }
        (_a, b) => b,
    }
}

fn default_site_settings() -> serde_json::Value {
    json!({
        "title": "IncentiveSwift | Viral Campaigns & Loyalty Rewards Platform",
        "description": "Create viral incentive campaigns, loyalty programs, raffles, and sweepstakes with IncentiveSwift. Boost engagement and retention with gamified rewards.",
        "keywords": "incentive platform, viral campaigns, loyalty rewards, raffle software, sweepstakes, gamification",
        "og_title": "IncentiveSwift — Viral Campaigns & Loyalty Rewards",
        "og_description": "Create viral incentive campaigns, loyalty programs, raffles, and sweepstakes that drive engagement.",
        "og_image_url": "",
        "favicon_url": "",
        "canonical_url": "https://incentiveswift.com",
        "ga_id": "",
        "gtm_id": "",
        "head_scripts": "",
        "body_scripts": "",
        "schema_json": "{\"@context\":\"https://schema.org\",\"@type\":\"SoftwareApplication\",\"name\":\"IncentiveSwift\",\"operatingSystem\":\"All\",\"applicationCategory\":\"BusinessApplication\",\"offers\":{\"@type\":\"Offer\",\"price\":\"0.00\",\"priceCurrency\":\"USD\"},\"description\":\"Viral incentive campaigns, loyalty programs, raffles, and sweepstakes platform.\"}",
        "legal_tos": "",
        "legal_privacy": "",
        "legal_refunds": "",
        "homepage": {
            "logo_text": "IncentiveSwift",
            "headline": "Create Viral Campaigns That Drive Results",
            "subheadline": "Loyalty rewards, raffles, sweepstakes, and more — all in one platform.",
            "button_text": "Get Started Free",
            "secondary_button_text": "View Demo"
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // The served-page convention: an address in page TEXT is published as the entity, so the
    // applier — not the operator — is the enforcement point. Synthetic addresses only.
    #[test]
    fn an_address_in_legal_text_is_published_as_the_entity() {
        assert_eq!(
            escape_addresses("Email: support@example.com"),
            "Email: support&#64;example.com"
        );
        assert_eq!(
            escape_addresses("write to a.b+tag@ex-ample.co.uk."),
            "write to a.b+tag&#64;ex-ample.co.uk."
        );
    }

    #[test]
    fn an_address_at_the_end_of_a_sentence_is_still_escaped() {
        // The sentence's full stop sits inside the domain char run, so a naive cut would skip the
        // address and publish a literal '@' that the edge then rewrites (kanban t_1f427190).
        assert_eq!(
            escape_addresses("written notice to support@swiftsoftware.net."),
            "written notice to support&#64;swiftsoftware.net."
        );
        assert_eq!(
            escape_addresses("Email: legal@swiftsoftware.net\nWebsite: x.com"),
            "Email: legal&#64;swiftsoftware.net\nWebsite: x.com"
        );
    }

    #[test]
    fn a_plain_at_sign_that_is_not_an_address_is_left_alone() {
        assert_eq!(
            escape_addresses("@media (max-width: 600px)"),
            "@media (max-width: 600px)"
        );
        assert_eq!(
            escape_addresses("cost is 5@ 10 units"),
            "cost is 5@ 10 units"
        );
        assert_eq!(escape_addresses("a@b"), "a@b"); // no dotted domain
        assert_eq!(escape_addresses("v1@2.3"), "v1@2.3"); // numeric TLD
        assert_eq!(escape_addresses("@example.com"), "@example.com"); // no local part
    }

    #[test]
    fn an_already_escaped_address_is_not_double_escaped() {
        assert_eq!(
            escape_addresses("support&#64;example.com"),
            "support&#64;example.com"
        );
    }

    // The VALUE guard the card requires: a blank legal_* must never reach a target. With SITE_ROOT
    // present the index is also planned (read-only); with it absent the plan is empty. Either way the
    // refunds page must NOT be a target while its value is blank.
    #[test]
    fn a_blank_legal_value_is_never_a_write_target() {
        let settings = json!({"legal_refunds": "", "legal_tos": "   ", "legal_privacy": "text"});
        let (targets, skipped) = plan(&settings);
        assert!(targets.iter().all(|(p, _)| !p.ends_with("refunds.html")));
        assert!(targets.iter().all(|(p, _)| !p.ends_with("terms.html")));
        assert!(skipped
            .iter()
            .any(|(p, r)| p.ends_with("refunds.html") && r.contains("blank or absent")));
    }

    // The store guard: the panel's GET-then-PUT round trip cannot blank a stored policy.
    #[test]
    fn a_blank_legal_value_cannot_blank_the_stored_row() {
        let existing = json!({"legal_tos": "Terms body", "legal_privacy": "Privacy body"});
        let out = preserve_nonblank(
            Some(&existing),
            json!({"legal_tos": "", "legal_privacy": "  "}),
            &LEGAL_KEYS,
        );
        assert_eq!(out["legal_tos"], "Terms body");
        assert_eq!(out["legal_privacy"], "Privacy body");
        // ... and a real edit still lands
        let out = preserve_nonblank(
            Some(&existing),
            json!({"legal_tos": "New terms body"}),
            &LEGAL_KEYS,
        );
        assert_eq!(out["legal_tos"], "New terms body");
    }

    // The template must reproduce the PUBLISHED wrapper exactly, including the escaped <h1> that the
    // <title> does not carry (kanban t_3fb0d3d2 reconciliation).
    #[test]
    fn the_legal_template_matches_the_published_wrapper() {
        let page = legal_page(
            "Refund & Cancellation Policy",
            "BODY",
            "</div></body></html>\n",
        );
        assert!(page.contains("<title>Refund & Cancellation Policy — IncentiveSwift</title>"));
        assert!(
            page.contains("<h1>Refund &amp; Cancellation Policy</h1>\nBODY</div></body></html>\n")
        );
        assert!(page.starts_with("<!DOCTYPE html><html lang=\"en\">\n<head>"));
    }

    // The renderer must be a no-op on the head that is actually PUBLISHED (kanban t_3fb0d3d2): the
    // published `index.html` carries the whitespace remnant of the GA block a previous write removed,
    // and the first cut of `insert_before_tag` turned that remnant into a fresh whitespace-only line,
    // so `--check` on the untouched live page read `would-write` and every later write drifted the
    // file by one more byte. Synthetic GA id; the shape is the published one.
    #[test]
    fn the_head_injection_is_idempotent_on_a_published_head() {
        let head = "<style>x</style>\n\n  \n  \n  <script async src=\"https://www.googletagmanager.com/gtag/js?id=G-TEST\"></script><script>window.dataLayer=window.dataLayer||[];function gtag(){dataLayer.push(arguments);}gtag('js',new Date());gtag('config','G-TEST');</script></head>\n";
        let s = json!({"ga_id": "G-TEST", "gtm_id": ""});
        let once = inject_site_settings(head, &s);
        assert_eq!(
            once, head,
            "the published head must be reproduced byte-for-byte"
        );
        assert_eq!(
            inject_site_settings(&once, &s),
            once,
            "a second render of the same settings must be a no-op"
        );

        // ... and on a head with no indentation before the tag the block still lands on its own line.
        let clean = "<html><head></head><body></body></html>\n";
        let injected = inject_site_settings(clean, &s);
        assert!(injected.contains("<head>\n  <script async src="));
        assert_eq!(inject_site_settings(&injected, &s), injected);
    }
}
