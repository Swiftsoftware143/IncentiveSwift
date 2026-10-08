//! `image_store` — the ONE accept-store-serve path for a customer-supplied image.
//!
//! Kanban t_c06a32eb. Two surfaces hold an image the customer uploaded: the profile picture
//! (`user_avatars`, kanban t_ff948669) and the email-branding logo (`tenant_logos`). The card is
//! explicit that the capability must be built ONCE — "accept an image, store it, serve it" — and
//! used by both, so the three pieces of that capability live here and nowhere else:
//!
//! * [`read_uploaded_image`] — the multipart read (first part carrying a filename, size cap,
//!   MAGIC-BYTE sniff). Both upload handlers call this and then persist what it returns.
//! * [`sniff`] — recognise an image by its bytes, never by the caller's `Content-Type`.
//! * [`image_response`] — serve stored bytes back with the sniffed type pinned and caching that
//!   never invites a shared cache to keep one customer's picture.
//!
//! The BYTES themselves are stored in the database on both surfaces, for one measured reason: this
//! container binds only its release binary and `migrations/` (`docker inspect incentiveswift`), so a
//! file written at run time lives inside the container and dies with the next `docker restart`, and
//! no host webroot could serve it either.

use axum::{
    body::Body,
    extract::Multipart,
    http::{header, StatusCode},
    response::Response,
};

use crate::error::AppError;

/// The platform's request-body ceiling for these uploads: axum refuses a body over 2 MB by default.
///
/// The per-route `DefaultBodyLimit` (`api_router::IMAGE_BODY_LIMIT_BYTES`) does NOT raise it, and
/// that is measured, not assumed: the body is read by
/// `body_deadline::body_read_deadline_middleware`, which runs OUTSIDE the route layer and therefore
/// reads with axum's own default. Measured 2026-10-08 (kanban t_c06a32eb), a 2 MB + 4 KB multipart
/// body answers `413 Failed to buffer the request body: length limit exceeded` before any handler
/// runs. So the ENVELOPE — image plus multipart framing — is what has to fit in 2 MB.
pub const ENVELOPE_BYTES: usize = 2 * 1024 * 1024;

/// Multipart framing reserve: boundary lines, the part headers and a real filename. Kept out of the
/// envelope so a file the picker accepts can never be refused by the envelope instead of by the
/// handler (which is the difference between the friendly message and a bare 413).
pub const ENVELOPE_HEADROOM: usize = 64 * 1024;

/// The most image bytes an upload may carry.
///
/// `ENVELOPE_BYTES - ENVELOPE_HEADROOM`, because the framing has to fit in the same 2 MB envelope.
/// This is also the number the two pickers in the tenant console check before uploading, so the
/// client and the server refuse the SAME set of files. The handler still checks the decoded length
/// as the authoritative backstop.
pub const MAX_IMAGE_BYTES: usize = ENVELOPE_BYTES - ENVELOPE_HEADROOM;

/// The `Content-Type` an upload is served back as, recognised from the MAGIC BYTES.
///
/// These bytes are stored and later served from our own origin with the type this returns, so
/// trusting the client's header would let a caller upload HTML or JavaScript labelled `image/png`
/// and have the app serve it as an image on an IncentiveSwift URL. SVG is deliberately NOT accepted:
/// it is a scriptable document rather than a bitmap, and this surface carries no HTML sanitiser.
pub fn sniff(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        return Some("image/png");
    }
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some("image/jpeg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

/// Read the first multipart part that carries a filename, cap it, and sniff it.
///
/// Returns `(content_type, bytes)`. Every refusal is a 4xx: no part with a filename, an empty file,
/// one over [`MAX_IMAGE_BYTES`], or bytes that are not a PNG/JPEG/GIF/WebP. A form carrying extra
/// text parts still works — a part with no filename is skipped, not refused.
pub async fn read_uploaded_image(multipart: &mut Multipart) -> Result<(String, Vec<u8>), AppError> {
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::BadRequest(format!("Multipart error: {e}")))?
    {
        // Only a part that carries a filename is the image; a form with extra text parts still works.
        if field.file_name().is_none() {
            continue;
        }
        let data = field
            .bytes()
            .await
            .map_err(|e| AppError::BadRequest(format!("Failed to read the image: {e}")))?;
        if data.is_empty() {
            return Err(AppError::BadRequest("The image file is empty".into()));
        }
        if data.len() > MAX_IMAGE_BYTES {
            return Err(AppError::BadRequest(format!(
                "The image must be {} bytes or smaller (2 MB minus the upload envelope)",
                MAX_IMAGE_BYTES
            )));
        }
        let kind = sniff(&data)
            .ok_or_else(|| AppError::BadRequest("Upload a PNG, JPEG, GIF or WebP image".into()))?;
        return Ok((kind.to_string(), data.to_vec()));
    }
    Err(AppError::BadRequest("No image was uploaded".into()))
}

/// Serve stored image bytes back.
///
/// `private, max-age=300`: the browser may reuse it for the session; no shared cache is invited to
/// keep one customer's picture. `nosniff` pins the type to the one sniffed at upload time.
pub fn image_response(content_type: String, bytes: Vec<u8>) -> Result<Response, AppError> {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, "private, max-age=300")
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .body(Body::from(bytes))
        .map_err(|e| AppError::Internal(format!("Could not build the image response: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniff_recognises_the_four_allowed_formats_by_magic_bytes() {
        let png = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0];
        assert_eq!(sniff(&png), Some("image/png"));
        assert_eq!(sniff(&[0xff, 0xd8, 0xff, 0xe0]), Some("image/jpeg"));
        assert_eq!(sniff(b"GIF89a...."), Some("image/gif"));
        assert_eq!(sniff(b"GIF87a...."), Some("image/gif"));
        let mut webp = b"RIFF".to_vec();
        webp.extend_from_slice(&[0, 0, 0, 0]);
        webp.extend_from_slice(b"WEBP");
        assert_eq!(sniff(&webp), Some("image/webp"));
    }

    #[test]
    fn sniff_refuses_scriptable_and_unknown_payloads() {
        // SVG is a document, not a bitmap — and the label is not consulted either way.
        assert_eq!(
            sniff(b"<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>"),
            None
        );
        assert_eq!(sniff(b"<html><script>alert(1)</script></html>"), None);
        assert_eq!(sniff(b"RIFF1234WAVE"), None);
        assert_eq!(sniff(b""), None);
    }
}
