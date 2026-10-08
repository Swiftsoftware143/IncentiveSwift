//! `branding` — per-TENANT email branding (kanban t_feab8aff, port of FunnelSwift t_c06a32eb).
//!
//! David 2026-10-08: *"even though these are transactional emails he wants the branding uniform,
//! with somewhere to put a logo or similar to personalise them"* — per TENANT, not per app. In this
//! app a tenant IS an account (`email_templates.aid`), which is exactly the axis the template
//! override already uses, so branding is keyed by the same id the senders pass as `aid`.
//!
//! # Where it lives
//!
//! * `brand_name` / `brand_color` / `logo_url` are one jsonb document in the tenant's OWN
//!   `tenant_settings` row under key [`SETTINGS_KEY`] — the store the tenant Settings screen already
//!   reads and writes (`GET`/`PUT /api/v1/settings`), so the text half needs no new endpoint and no
//!   new column.
//! * The logo's BYTES live in `tenant_logos` (migration 099) and are streamed back by
//!   `GET /api/v1/branding/logo/:tenant_id`. See `crate::image_store` for why the bytes are in the
//!   database rather than on disk.
//!
//! # The one-writer-per-field rule
//!
//! `logo_url` is owned by the logo endpoints: `POST|DELETE /api/v1/settings/branding/logo` write it
//! and `settings_handler::update_settings` PRESERVES the stored value when the tenant saves
//! name/colour (the panel echoes the document it was given, and a stale echo must not un-reference a
//! logo that is still there). `brand_name` / `brand_color` are owned by the settings write, which
//! validates them.
//!
//! # Nothing configured means nothing changes
//!
//! A tenant with no branding gets byte-identical mail to before this module existed — the header
//! block is only ever added when a brand name or a logo is actually set (`Branding::from_value`
//! returns `None` otherwise). That is what makes the change additive for every existing tenant.

use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

/// The `tenant_settings` key holding this tenant's email-branding document.
pub const SETTINGS_KEY: &str = "email_branding";

/// The longest brand name accepted. Long enough for a legal name, short enough that it cannot be
/// used as a header-injection or layout-breaking payload.
pub const MAX_BRAND_NAME: usize = 60;

/// The rule colour used when the tenant set a name/logo but no colour of their own.
pub const DEFAULT_BRAND_COLOR: &str = "#4f46e5";

/// One tenant's email branding, as stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Branding {
    /// The display name to print above the message. Empty = not set.
    pub brand_name: String,
    /// `#rgb` / `#rrggbb` / `#rrggbbaa`, or empty for [`DEFAULT_BRAND_COLOR`].
    pub brand_color: String,
    /// Where the logo is served from, as stored. Written by the logo endpoints only.
    pub logo_url: String,
}

impl Branding {
    /// Read a stored document. `None` when there is nothing to render with — i.e. neither a brand
    /// name nor a logo — which is what keeps the change additive for every unbranded tenant.
    pub fn from_value(v: &Value) -> Option<Branding> {
        let s = |k: &str| {
            v.get(k)
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string()
        };
        let b = Branding {
            brand_name: s("brand_name").chars().take(MAX_BRAND_NAME).collect(),
            brand_color: s("brand_color"),
            logo_url: s("logo_url"),
        };
        if b.brand_name.is_empty() && b.logo_url.is_empty() {
            return None;
        }
        Some(b)
    }

    /// The colour to actually paint with: the tenant's when it is a well-formed hex colour, else the
    /// default. Re-checked at RENDER time as well as at write time — a value written straight into
    /// the row (a migration, a psql session) must not be able to inject into a `style` attribute.
    pub fn effective_color(&self) -> &str {
        if is_hex_color(&self.brand_color) {
            &self.brand_color
        } else {
            DEFAULT_BRAND_COLOR
        }
    }

    /// The logo's absolute URL for a mail client, or `None` when there is no logo.
    ///
    /// A mail client resolves nothing relative to the app, so the path stored by the console
    /// (`/api/v1/branding/logo/<id>?v=<n>`) is prefixed with the app origin. A stored absolute URL is
    /// passed through untouched.
    pub fn resolve_logo_url(&self, origin: &str) -> Option<String> {
        if self.logo_url.is_empty() {
            return None;
        }
        if self.logo_url.starts_with("http://") || self.logo_url.starts_with("https://") {
            Some(self.logo_url.clone())
        } else {
            Some(format!("{origin}{}", self.logo_url))
        }
    }

    /// The header block that opens every rendered HTML mail, or `""` for a tenant with no branding.
    ///
    /// Inline styles only, no external assets, no `<style>` block: mail clients strip or ignore all
    /// three. `alt` carries the brand name so a blocked image still says whose mail this is.
    pub fn header_html(&self, logo_absolute: Option<&str>) -> String {
        let color = self.effective_color();
        let name = escape_html(&self.brand_name);
        let inner = match logo_absolute {
            Some(url) => format!(
                "<img src=\"{}\" alt=\"{}\" style=\"max-height:56px;max-width:240px;display:inline-block\">",
                escape_html(url),
                name
            ),
            None => {
                if name.is_empty() {
                    return String::new();
                }
                format!(
                    "<span style=\"font:700 20px/1.3 -apple-system,'Segoe UI',Roboto,Helvetica,Arial,sans-serif;color:#111827\">{}</span>",
                    name
                )
            }
        };
        format!(
            "<div style=\"text-align:center;padding:18px 0 6px\">{inner}</div>\
             <div style=\"border-top:3px solid {color};margin:0 0 18px\"></div>"
        )
    }

    /// The one-line header for the PLAIN-TEXT part: the brand name and nothing else (a text part
    /// cannot carry an image). `""` when no brand name is set.
    pub fn text_header(&self) -> String {
        if self.brand_name.is_empty() {
            String::new()
        } else {
            format!("{}\n\n", self.brand_name)
        }
    }
}

/// Load this tenant's branding. `None` on no row, an unreadable row, or nothing configured.
///
/// A read failure is NOT fatal to a send: an email that cannot read its branding still goes out with
/// the app's own identity, exactly as every mail did before this module existed.
pub async fn load(pool: &PgPool, tenant_id: Uuid) -> Option<Branding> {
    let stored: Option<Value> =
        sqlx::query_scalar("SELECT value FROM tenant_settings WHERE tenant_id = $1 AND key = $2")
            .bind(tenant_id)
            .bind(SETTINGS_KEY)
            .fetch_optional(pool)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(
                    tenant = %tenant_id,
                    error = %e,
                    "email branding could not be read — sending with the app's own identity"
                );
                None
            });
    // An unreadable row means "unbranded" here: `?` on the Option, not on the read error.
    Branding::from_value(stored.as_ref()?)
}

/// Validate an incoming `email_branding` document, for the settings write.
///
/// Refuses (with the reason) rather than silently storing something the renderer would have to
/// defend against: an over-long name, a name carrying control characters, or a colour that is not a
/// hex colour. An ABSENT key is fine — that is "leave this field alone".
pub fn validate_value(v: &Value) -> Result<(), String> {
    let obj = v
        .as_object()
        .ok_or_else(|| "Branding must be an object with brand_name / brand_color".to_string())?;
    for (key, label) in [
        ("brand_name", "Brand display name"),
        ("brand_color", "Brand colour"),
        ("logo_url", "Logo URL"),
    ] {
        if let Some(raw) = obj.get(key) {
            if !raw.is_null() && !raw.is_string() {
                return Err(format!("{label} must be text"));
            }
        }
    }
    if let Some(name) = obj.get("brand_name").and_then(Value::as_str) {
        let n = name.trim();
        if n.chars().count() > MAX_BRAND_NAME {
            return Err(format!(
                "Brand display name must be {MAX_BRAND_NAME} characters or fewer"
            ));
        }
        if n.chars().any(|c| c.is_control()) {
            return Err("Brand display name must not contain control characters".into());
        }
    }
    if let Some(color) = obj.get("brand_color").and_then(Value::as_str) {
        let c = color.trim();
        if !c.is_empty() && !is_hex_color(c) {
            return Err("Brand colour must be a hex colour such as #4f46e5".into());
        }
    }
    Ok(())
}

/// `#rgb` / `#rrggbb` / `#rrggbbaa`.
pub fn is_hex_color(s: &str) -> bool {
    let body = match s.strip_prefix('#') {
        Some(b) => b,
        None => return false,
    };
    matches!(body.len(), 3 | 6 | 8) && body.chars().all(|c| c.is_ascii_hexdigit())
}

/// Escape text for interpolation into an HTML attribute or text node.
///
/// Small and local on purpose: this module is the only place a tenant-controlled string is written
/// into mail markup, and the app carries no HTML sanitiser.
pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// The document the console writes when a tenant saves name/colour. Kept here so the shape the
/// renderer reads and the shape the panel writes cannot drift.
pub fn document(brand_name: &str, brand_color: &str, logo_url: &str) -> Value {
    json!({
        "brand_name": brand_name,
        "brand_color": brand_color,
        "logo_url": logo_url,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(name: &str, color: &str, logo: &str) -> Branding {
        Branding {
            brand_name: name.to_string(),
            brand_color: color.to_string(),
            logo_url: logo.to_string(),
        }
    }

    #[test]
    fn nothing_configured_is_none_so_the_change_is_additive() {
        assert_eq!(Branding::from_value(&json!({})), None);
        assert_eq!(Branding::from_value(&json!({"brand_name": "  "})), None);
        // a colour alone renders nothing, so it is not "configured"
        assert_eq!(Branding::from_value(&json!({"brand_color": "#fff"})), None);
        assert!(Branding::from_value(&json!({"logo_url": "/x.png"})).is_some());
        assert!(Branding::from_value(&json!({"brand_name": "Acme"})).is_some());
    }

    #[test]
    fn the_stored_document_round_trips() {
        let v = document(
            "Giraudy Capital",
            "#0ea5e9",
            "/api/v1/branding/logo/abc?v=7",
        );
        let got = Branding::from_value(&v).expect("configured");
        assert_eq!(got.brand_name, "Giraudy Capital");
        assert_eq!(got.effective_color(), "#0ea5e9");
        assert_eq!(
            got.resolve_logo_url("https://app.incentiveswift.com")
                .as_deref(),
            Some("https://app.incentiveswift.com/api/v1/branding/logo/abc?v=7")
        );
    }

    #[test]
    fn a_stored_absolute_url_is_not_prefixed_again() {
        let got = b("", "", "https://cdn.example.com/logo.png");
        assert_eq!(
            got.resolve_logo_url("https://app.incentiveswift.com")
                .as_deref(),
            Some("https://cdn.example.com/logo.png")
        );
        // ...and no logo at all is None, not an empty <img src="">
        assert_eq!(
            b("Acme", "", "").resolve_logo_url("https://x").as_deref(),
            None
        );
    }

    #[test]
    fn the_header_block_carries_the_logo_and_the_colour() {
        let got = b("Acme & Sons", "#0ea5e9", "/api/v1/branding/logo/abc?v=7");
        let html = got.header_html(
            got.resolve_logo_url("https://app.incentiveswift.com")
                .as_deref(),
        );
        assert!(html.contains("https://app.incentiveswift.com/api/v1/branding/logo/abc?v=7"));
        assert!(html.contains("border-top:3px solid #0ea5e9"));
        // the name is escaped everywhere it appears
        assert!(html.contains("Acme &amp; Sons"));
        assert!(!html.contains("Acme & Sons"));
    }

    #[test]
    fn a_name_only_branding_renders_the_name_not_a_broken_image() {
        let got = b("Acme", "#ffffff", "");
        let html = got.header_html(
            got.resolve_logo_url("https://app.incentiveswift.com")
                .as_deref(),
        );
        assert!(!html.contains("<img"));
        assert!(html.contains(">Acme<"));
    }

    #[test]
    fn an_invalid_stored_colour_falls_back_instead_of_being_painted() {
        // A row written straight into the database bypasses the write-time check, so the renderer
        // decides again: `red;background:url(javascript:…)` must never reach a style attribute.
        let got = b("Acme", "red;background:url(javascript:alert(1))", "");
        let html = got.header_html(None);
        assert!(!html.contains("javascript"));
        assert!(html.contains(DEFAULT_BRAND_COLOR));
    }

    #[test]
    fn an_empty_brand_name_renders_no_header_at_all() {
        assert_eq!(b("", "", "").header_html(None), "");
        assert_eq!(b("", "", "").text_header(), "");
        assert_eq!(b("Acme", "", "").text_header(), "Acme\n\n");
    }

    #[test]
    fn validation_refuses_what_the_renderer_would_have_to_defend_against() {
        assert!(validate_value(&json!({"brand_name": "Acme", "brand_color": "#4f46e5"})).is_ok());
        assert!(validate_value(&json!({})).is_ok());
        assert!(validate_value(&json!({"brand_name": "A".repeat(61)})).is_err());
        assert!(validate_value(&json!({"brand_name": "A\u{0}B"})).is_err());
        assert!(validate_value(&json!({"brand_color": "red"})).is_err());
        assert!(validate_value(&json!({"brand_color": "#ggg"})).is_err());
        assert!(validate_value(&json!({"brand_color": ""})).is_ok());
        assert!(validate_value(&json!({"brand_name": 12})).is_err());
        assert!(validate_value(&json!("nope")).is_err());
    }

    #[test]
    fn a_brand_name_longer_than_the_cap_is_truncated_not_dropped() {
        let long = "A".repeat(200);
        let got = Branding::from_value(&json!({"brand_name": long})).unwrap();
        assert_eq!(got.brand_name.chars().count(), MAX_BRAND_NAME);
    }
}
