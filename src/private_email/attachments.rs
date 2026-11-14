//! THE FILES THAT TRAVEL WITH A MESSAGE (ported from CoreSwift-CRM, kanban t_f919924f).
//!
//! Before this module the receiver counted a delivery's file parts, added their bytes to the
//! message's storage figure and then threw the bytes away: the inbox marked a message as carrying
//! an attachment and there was nothing to open. The app's own branded mail made that visible on
//! every send — the logo is an inline `cid:` part, so the reader had no image to draw.
//!
//! This port covers the INBOUND leg, which is the half that loses a customer's file: a receiver
//! parses the delivery's file parts into [`InboundFile`] and [`store_for_message`] writes one row
//! per file, keyed to the message and the account. Nothing here can refuse a delivery: inbound mail
//! is never refused (storage refuses NEW outbound files through its own 100% rule).
//!
//! STORAGE IS COUNTED ONCE. `private_email_messages.size_bytes` already includes a delivery's file
//! bytes (the meter, the banner and the 100% refusal all read that), so this table is the payload
//! and nothing sums it a second time.
//!
//! TENANCY. Every read takes the caller's account id; the download route resolves the file through
//! `(tenant_id, message_id, id)` in one statement, so a guessed attachment id from another account
//! finds no row — 404, never a 403 that confirms the id exists.

use std::collections::HashMap;

use base64::Engine as _;
use serde::Serialize;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::error::AppError;

/// One file part as it arrived, before it is written to the database.
#[derive(Debug, Clone)]
pub struct InboundFile {
    /// The sender's name for the part, already made safe for a header and a filename.
    pub filename: String,
    pub content_type: String,
    /// MIME Content-ID without angle brackets, when the delivery named one.
    pub content_id: Option<String>,
    /// Whether the body draws this part in place (`cid:`) rather than offering it as a download.
    pub is_inline: bool,
    pub bytes: Vec<u8>,
}

/// An inline image larger than this is NOT embedded into the message JSON; it stays a downloadable
/// part instead, so opening a message can never ship a 10 MB base64 blob into the browser.
pub const MAX_INLINE_EMBED_BYTES: usize = 1_500_000;
/// The total inline budget for one message body.
pub const MAX_INLINE_EMBED_TOTAL_BYTES: usize = 4 * 1024 * 1024;

/// Write the files a delivery carried against the message it created.
///
/// A failure here is LOGGED and swallowed: the message has already been accepted, and a provider
/// retry cannot be relied on to bring the bytes back. Losing the mail because one file row failed
/// would be the worse defect — the row is simply absent and the message says so.
pub async fn store_for_message(
    db: &PgPool,
    tenant_id: Uuid,
    message_id: Uuid,
    files: &[InboundFile],
) {
    if files.is_empty() || message_id == Uuid::nil() {
        return;
    }
    for file in files {
        insert_row(
            db,
            tenant_id,
            message_id,
            &file.filename,
            &file.content_type,
            file.content_id.as_deref(),
            file.is_inline || file.content_id.is_some(),
            &file.bytes,
        )
        .await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn insert_row(
    db: &PgPool,
    tenant_id: Uuid,
    message_id: Uuid,
    filename: &str,
    content_type: &str,
    content_id: Option<&str>,
    is_inline: bool,
    bytes: &[u8],
) {
    let result = sqlx::query(
        r#"
        INSERT INTO private_email_attachments
            (tenant_id, message_id, filename, content_type, content_id, is_inline, size_bytes, data)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        "#,
    )
    .bind(tenant_id)
    .bind(message_id)
    .bind(filename)
    .bind(content_type)
    .bind(content_id)
    .bind(is_inline)
    .bind(bytes.len() as i32)
    .bind(bytes)
    .execute(db)
    .await;
    if let Err(e) = result {
        tracing::warn!(
            tenant = %tenant_id,
            message = %message_id,
            filename = %filename,
            error = %e,
            "an attachment could not be stored with its message"
        );
    }
}

/// What the reader needs to draw a chip — never the bytes themselves.
#[derive(Debug, Clone, Serialize)]
pub struct AttachmentMeta {
    pub id: Uuid,
    pub filename: String,
    pub content_type: String,
    pub size_bytes: i32,
    pub is_inline: bool,
}

/// Every file attached to one message of this account, newest last.
pub async fn list_for_message(
    db: &PgPool,
    tenant_id: Uuid,
    message_id: Uuid,
) -> Result<Vec<AttachmentMeta>, AppError> {
    let rows = sqlx::query(
        r#"
        SELECT id, filename, content_type, size_bytes, is_inline
          FROM private_email_attachments
         WHERE tenant_id = $1 AND message_id = $2
         ORDER BY created_at ASC, id ASC
        "#,
    )
    .bind(tenant_id)
    .bind(message_id)
    .fetch_all(db)
    .await
    .map_err(AppError::Database)?;

    Ok(rows
        .into_iter()
        .map(|r| AttachmentMeta {
            id: r.try_get("id").unwrap_or_default(),
            filename: r.try_get("filename").unwrap_or_default(),
            content_type: r
                .try_get("content_type")
                .unwrap_or_else(|_| "application/octet-stream".into()),
            size_bytes: r.try_get("size_bytes").unwrap_or(0),
            is_inline: r.try_get("is_inline").unwrap_or(false),
        })
        .collect())
}

/// The bytes of ONE file, resolved through the account AND the message it belongs to.
pub async fn load_bytes(
    db: &PgPool,
    tenant_id: Uuid,
    message_id: Uuid,
    attachment_id: Uuid,
) -> Result<Option<(String, String, Vec<u8>)>, AppError> {
    let row = sqlx::query(
        r#"
        SELECT filename, content_type, data
          FROM private_email_attachments
         WHERE id = $1 AND tenant_id = $2 AND message_id = $3
        "#,
    )
    .bind(attachment_id)
    .bind(tenant_id)
    .bind(message_id)
    .fetch_optional(db)
    .await
    .map_err(AppError::Database)?;

    Ok(row.map(|r| {
        (
            r.try_get::<String, _>("filename").unwrap_or_default(),
            r.try_get::<String, _>("content_type")
                .unwrap_or_else(|_| "application/octet-stream".into()),
            r.try_get::<Vec<u8>, _>("data").unwrap_or_default(),
        )
    }))
}

/// The inline parts small enough to embed in the message JSON, keyed for `cid:` lookup.
///
/// Only `is_inline` rows are read, and only under [`MAX_INLINE_EMBED_BYTES`]: opening a message must
/// not ship a 10 MB PDF into the browser as base64. A part that is too large stays a chip.
pub async fn inline_parts_for_message(
    db: &PgPool,
    tenant_id: Uuid,
    message_id: Uuid,
) -> Result<Vec<(String, String, Vec<u8>)>, AppError> {
    let rows = sqlx::query(
        r#"
        SELECT filename, content_id, content_type, data
          FROM private_email_attachments
         WHERE tenant_id = $1 AND message_id = $2
           AND is_inline = true
           AND size_bytes <= $3
         ORDER BY created_at ASC, id ASC
        "#,
    )
    .bind(tenant_id)
    .bind(message_id)
    .bind(MAX_INLINE_EMBED_BYTES as i32)
    .fetch_all(db)
    .await
    .map_err(AppError::Database)?;

    Ok(rows
        .into_iter()
        .map(|r| {
            let filename: String = r.try_get("filename").unwrap_or_default();
            let content_id: Option<String> = r.try_get("content_id").unwrap_or(None);
            // The lookup key: the Content-ID when the delivery named one, else the filename.
            let key = content_id
                .map(|c| c.trim().trim_matches(['<', '>']).to_string())
                .filter(|c| !c.is_empty())
                .unwrap_or_else(|| filename.clone());
            (
                key,
                r.try_get::<String, _>("content_type")
                    .unwrap_or_else(|_| "application/octet-stream".into()),
                r.try_get::<Vec<u8>, _>("data").unwrap_or_default(),
            )
        })
        .collect())
}

/// A file part's name, made safe for a header and for a saved filename.
///
/// Never a path: separators, control characters and quotes are removed, a leading dot is dropped,
/// and an empty result becomes `attachment-<n>`. The name the customer sees is preserved otherwise.
pub fn safe_filename(raw: &str, ordinal: usize) -> String {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or(raw);
    let cleaned: String = base
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, '"' | '\'' | '\\' | '/' | ':' | '*' | '?'))
        .collect();
    let cleaned = cleaned.trim().trim_start_matches('.').trim().to_string();
    if cleaned.is_empty() {
        format!("attachment-{ordinal}")
    } else {
        cleaned.chars().take(180).collect()
    }
}

/// A `Content-Disposition` value that survives a non-ASCII filename: an ASCII fallback plus
/// RFC 5987 `filename*` when the name is not plain ASCII.
pub fn content_disposition(disposition: &str, filename: &str) -> String {
    let ascii: String = filename
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ' ') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let ascii = if ascii.trim().is_empty() {
        "attachment".to_string()
    } else {
        ascii
    };
    if filename.is_ascii() {
        format!("{disposition}; filename=\"{ascii}\"")
    } else {
        let encoded: String = filename
            .bytes()
            .map(|b| {
                if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b'~') {
                    (b as char).to_string()
                } else {
                    format!("%{b:02X}")
                }
            })
            .collect();
        format!("{disposition}; filename=\"{ascii}\"; filename*=UTF-8''{encoded}")
    }
}

/// Replace `cid:<name>` references in a message body with `data:` URIs built from the stored
/// inline parts, so a branded message draws its own logo without a second, unauthenticated route.
///
/// Matching is by Content-ID first, then the filename, then its basename. If the body carries a
/// single `cid:` reference this delivery did not name and there is exactly ONE inline part, that
/// part is used — the shape a provider produces when it rewrites the Content-ID in transit.
/// A part over the inline budget is left as `cid:` and appears as a download chip instead.
pub fn rewrite_inline_images(html: &str, parts: &[(String, String, Vec<u8>)]) -> String {
    if parts.is_empty() || !html.to_ascii_lowercase().contains("cid:") {
        return html.to_string();
    }

    struct Part<'a> {
        content_type: &'a str,
        bytes: &'a [u8],
    }
    let mut by_key: HashMap<String, Part<'_>> = HashMap::new();
    let mut inline_count = 0usize;
    let mut single: Option<Part<'_>> = None;
    for (key, content_type, bytes) in parts {
        if bytes.len() > MAX_INLINE_EMBED_BYTES {
            continue;
        }
        inline_count += 1;
        single = Some(Part {
            content_type,
            bytes,
        });
        let key = key.trim().trim_matches(['<', '>']).to_ascii_lowercase();
        if key.is_empty() {
            continue;
        }
        let part = Part {
            content_type,
            bytes,
        };
        if let Some(base) = key.rsplit('/').next() {
            by_key.entry(base.to_string()).or_insert(Part {
                content_type: part.content_type,
                bytes: part.bytes,
            });
        }
        by_key.entry(key).or_insert(Part {
            content_type: part.content_type,
            bytes: part.bytes,
        });
    }

    let mut out = String::with_capacity(html.len());
    let bytes = html.as_bytes();
    let mut i = 0usize;
    let mut budget = MAX_INLINE_EMBED_TOTAL_BYTES;
    while i < bytes.len() {
        if i + 4 <= bytes.len() && bytes[i..i + 4].eq_ignore_ascii_case(b"cid:") {
            let mut j = i + 4;
            while j < bytes.len() {
                let b = bytes[j];
                let ok = b.is_ascii_alphanumeric()
                    || matches!(b, b'.' | b'_' | b'@' | b'-' | b'+' | b'%' | b'~');
                if !ok {
                    break;
                }
                j += 1;
            }
            let token = &html[i + 4..j];
            let token_key = token.trim_matches(['<', '>']).to_ascii_lowercase();
            let chosen = by_key.get(&token_key).or(if inline_count == 1 {
                single.as_ref()
            } else {
                None
            });
            if let Some(part) = chosen {
                if part.bytes.len() <= budget {
                    budget -= part.bytes.len();
                    out.push_str("data:");
                    out.push_str(part.content_type);
                    out.push_str(";base64,");
                    out.push_str(&base64::engine::general_purpose::STANDARD.encode(part.bytes));
                    i = j;
                    continue;
                }
            }
        }
        // Advance one CHARACTER, so multi-byte text is copied intact.
        let ch = html[i..].chars().next().unwrap_or('\u{fffd}');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Is this delivery's file part referenced from the body as an inline image?
///
/// Mailgun does not always publish a Content-ID on the inbound leg, so the body's own `cid:`
/// reference is the reliable signal: a part whose filename is named there is embedded, everything
/// else is a downloadable attachment.
pub fn looks_inline(filename: &str, body_html: Option<&str>) -> bool {
    let Some(html) = body_html else {
        return false;
    };
    let lower = html.to_ascii_lowercase();
    let name = filename.to_ascii_lowercase();
    if name.is_empty() {
        return false;
    }
    lower.contains(&format!("cid:{name}"))
        || name
            .rsplit('/')
            .next()
            .map(|base| !base.is_empty() && lower.contains(&format!("cid:{base}")))
            .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(key: &str, ctype: &str, bytes: &[u8]) -> (String, String, Vec<u8>) {
        (key.to_string(), ctype.to_string(), bytes.to_vec())
    }

    #[test]
    fn a_cid_reference_becomes_a_data_uri() {
        let html = r#"<p>Regards<img src="cid:logo.png"></p>"#;
        let out = rewrite_inline_images(html, &[part("logo.png", "image/png", b"PNG")]);
        assert!(out.contains("src=\"data:image/png;base64,UE5H\""), "{out}");
        assert!(!out.contains("cid:"), "{out}");
    }

    #[test]
    fn a_content_id_with_angle_brackets_matches() {
        let html = r#"<img src="cid:abc@mailgun">"#;
        let out = rewrite_inline_images(html, &[part("<abc@mailgun>", "image/png", b"PNG")]);
        assert!(out.contains("data:image/png;base64,"), "{out}");
    }

    #[test]
    fn the_only_inline_part_covers_an_unnamed_reference() {
        let html = r#"<img src="cid:i-did-not-name-this">"#;
        let out = rewrite_inline_images(html, &[part("logo.png", "image/jpeg", b"JPG")]);
        assert!(out.contains("data:image/jpeg;base64,"), "{out}");
    }

    #[test]
    fn an_unmatched_reference_is_left_alone_when_several_parts_exist() {
        let html = r#"<img src="cid:other.png">"#;
        let out = rewrite_inline_images(
            html,
            &[
                part("logo.png", "image/png", b"A"),
                part("second.png", "image/png", b"B"),
            ],
        );
        assert!(out.contains("cid:other.png"), "{out}");
    }

    #[test]
    fn plain_text_with_no_cid_is_unchanged() {
        let html = "<p>hello — ünïcode</p>";
        assert_eq!(rewrite_inline_images(html, &[]), html);
        assert_eq!(
            rewrite_inline_images(html, &[part("logo.png", "image/png", b"A")]),
            html
        );
    }

    #[test]
    fn a_filename_is_never_a_path() {
        assert_eq!(safe_filename("../../etc/passwd", 1), "passwd");
        assert_eq!(
            safe_filename("invoice \"march\".pdf", 1),
            "invoice march.pdf"
        );
        assert_eq!(safe_filename("   ", 3), "attachment-3");
        assert_eq!(safe_filename("..", 2), "attachment-2");
    }

    #[test]
    fn a_disposition_survives_a_non_ascii_name() {
        let d = content_disposition("attachment", "faktura-märz.pdf");
        assert!(d.starts_with("attachment; filename=\"faktura-m_rz.pdf\""));
        assert!(d.contains("filename*=UTF-8''faktura-m%C3%A4rz.pdf"), "{d}");
        // A quote or a newline can never break out of the header.
        let d = content_disposition("inline", "a\"b\r\nc.png");
        assert!(!d.contains('\n') && !d.contains('\r'));
        assert!(!d.contains("a\"b"));
    }

    #[test]
    fn a_body_that_names_the_file_is_inline() {
        assert!(looks_inline(
            "logo.png",
            Some(r#"<img src="cid:logo.png">"#)
        ));
        assert!(!looks_inline(
            "invoice.pdf",
            Some(r#"<img src="cid:logo.png">"#)
        ));
        assert!(!looks_inline("invoice.pdf", None));
    }
}
