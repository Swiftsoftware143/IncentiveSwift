//! A malformed request body answers the app's own JSON, not a framework sentence.
//!
//! Every `#[derive(Deserialize)]` body on this app is read by axum's `Json` extractor, which
//! refuses a bad body BEFORE the handler runs. Its refusal is `text/plain`, so the console's
//! `await r.json()` throws and the operator sees a JavaScript parse error instead of a sentence.
//! Measured live 2026-10-26 on the three public auth forms (`/api/v1/auth/register`,
//! `/login`, `/forgot-password`) with seven malformed bodies each — every one answered
//! `text/plain` carrying the framework's own words:
//!
//! ```text
//! Failed to deserialize the JSON body into the target type: missing field `email` at line 1 column 2
//! Failed to parse the request body as JSON: EOF while parsing a value at line 1 column 0
//! Failed to deserialize the JSON body into the target type: invalid type: null, expected struct RegisterInput
//! ```
//!
//! Two problems in each: the content type breaks `r.json()`, and the sentence names our structs
//! and the JSON grammar. FunnelSwift, ADASwift and missedcallrespondr already answer their own
//! JSON here (missedcallrespondr `error::rejection_as_json`); this is the same fix for this app.
//!
//! [`as_app_json`] rewrites exactly the responses an EXTRACTOR produced: status 400/413/415/422
//! whose body is `text/plain`. Everything else is untouched — handler-written errors are already
//! `application/json`, so they pass through byte-identical (including this app's
//! `{"error": …, "code": …}` envelope), and no successful response is buffered.
//!
//! The status is kept where it means something to the caller (413 stays 413: the upload was not
//! malformed, it was too big) and collapsed onto 400 otherwise, so the whole API has one
//! bad-request shape.

use axum::body::Body;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::Response;

/// Statuses an extractor can produce for a request it cannot read.
const EXTRACTOR_STATUSES: [StatusCode; 4] = [
    StatusCode::BAD_REQUEST,
    StatusCode::UNSUPPORTED_MEDIA_TYPE,
    StatusCode::UNPROCESSABLE_ENTITY,
    StatusCode::PAYLOAD_TOO_LARGE,
];

/// A rejection body is one short sentence; anything larger is not ours and is passed through.
const INSPECT_LIMIT: usize = 8 * 1024;

/// The framing axum puts around the part the caller needs (the offending field, and the byte
/// position for malformed JSON). Dropped first so `leading_field` can see the field name;
/// anything unrecognised is left as it is.
const EXTRACTOR_PREFIXES: [&str; 2] = [
    "Failed to deserialize the JSON body into the target type: ",
    "Failed to parse the request body as JSON: ",
];

fn strip_framing(raw: &str) -> &str {
    EXTRACTOR_PREFIXES
        .iter()
        .find_map(|prefix| raw.strip_prefix(prefix))
        .map(str::trim)
        .unwrap_or(raw)
}

/// Rewrite an extractor's `text/plain` refusal into the app's own JSON error envelope.
pub async fn as_app_json(response: Response) -> Response {
    if !EXTRACTOR_STATUSES.contains(&response.status()) {
        return response;
    }
    let plain = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.starts_with("text/plain"))
        .unwrap_or(false);
    if !plain {
        return response;
    }

    let status = response.status();
    let (mut parts, body) = response.into_parts();
    let bytes = match axum::body::to_bytes(body, INSPECT_LIMIT).await {
        Ok(b) => b,
        // Unreadable body: still answer JSON rather than hand back a truncated stream.
        Err(_) => {
            return json_refusal(
                StatusCode::BAD_REQUEST,
                "The request body could not be read.",
            );
        }
    };

    let raw = String::from_utf8_lossy(&bytes);
    let stripped = strip_framing(raw.trim());
    let message = plain_english(stripped, status);
    // 415/422 are the extractor's own distinctions, not ours: this app answers one bad-request
    // shape (400 `{"error": …}`). A 413 is kept — the upload was not malformed, it was too big.
    let status = if status == StatusCode::PAYLOAD_TOO_LARGE {
        StatusCode::PAYLOAD_TOO_LARGE
    } else {
        StatusCode::BAD_REQUEST
    };

    // Keep the headers the inner layers set (CORS, security, trace); replace status and body.
    parts.status = status;
    parts.headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    let out = serde_json::json!({ "error": message, "code": status.as_u16() }).to_string();
    if let Ok(len) = HeaderValue::from_str(&out.len().to_string()) {
        parts.headers.insert(header::CONTENT_LENGTH, len);
    }
    Response::from_parts(parts, Body::from(out))
}

fn json_refusal(status: StatusCode, message: &str) -> Response {
    use axum::response::IntoResponse;
    let body = serde_json::json!({ "error": message, "code": status.as_u16() });
    (status, axum::Json(body)).into_response()
}

/// Turn the extractor's words into a sentence a customer can act on.
///
/// Deliberately specific where the framework already told us which field is at fault, and
/// deliberately silent about the rest: the struct names, the byte offsets and the JSON grammar
/// are ours, not the caller's.
fn plain_english(raw: &str, status: StatusCode) -> String {
    let lower = raw.to_ascii_lowercase();

    if status == StatusCode::PAYLOAD_TOO_LARGE || lower.contains("length limit") {
        return "That upload is too large. Choose a smaller file.".to_string();
    }
    if status == StatusCode::UNSUPPORTED_MEDIA_TYPE {
        return "Send this form as JSON with a Content-Type of application/json.".to_string();
    }

    // `missing field \`email\`` — name the field in plain words when the form has one.
    if let Some(rest) = lower.split("missing field").nth(1) {
        let field = rest
            .trim_start_matches(|c: char| !c.is_alphanumeric() && c != '_')
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .next()
            .unwrap_or("")
            .to_string();
        return match field.as_str() {
            "email" => "Enter your email address.".to_string(),
            "password" => "Enter your password.".to_string(),
            "new_password" => "Choose a new password.".to_string(),
            "name" => "Enter your name.".to_string(),
            "token" | "code" => "Paste the reset code from your email.".to_string(),
            "" => "A required field is missing from this form.".to_string(),
            other => format!("The form is missing {other}."),
        };
    }

    // `invalid type: integer \`123\`, expected a string` — the value, not the field, is wrong.
    if lower.contains("invalid type")
        || lower.contains("invalid value")
        || lower.contains("invalid length")
    {
        // No leading field means the BODY itself is the wrong shape (null, or an array instead
        // of an object): the fix is to send an object with the form's fields.
        let shape = lower.contains("expected struct")
            || lower.starts_with("invalid type: null")
            || lower.contains("invalid length");
        return match leading_field(raw) {
            Some(field) => format!("{field}: that value is not valid for this form."),
            None if shape => "Send the form's fields as a JSON object.".to_string(),
            None => "One of the values in this form is not valid.".to_string(),
        };
    }

    // Unparsable JSON: EOF, a stray token, a bad escape.
    if lower.contains("eof while parsing")
        || lower.contains("expected value")
        || lower.contains("invalid unicode")
        || lower.contains("key must be a string")
        || lower.contains("control character")
        || lower.contains("expected `,`")
        || lower.contains("expected ident")
    {
        return "The request body was not valid JSON.".to_string();
    }

    if let Some(field) = leading_field(raw) {
        return format!("{field}: that value is not valid for this form.");
    }

    "The request body could not be read.".to_string()
}

/// The framework prefixes a field-scoped complaint with `field: `. Only take a token that looks
/// like a field name, so an unrecognised sentence never becomes `Failed: that value is not…`.
fn leading_field(raw: &str) -> Option<String> {
    let (head, _) = raw.split_once(':')?;
    let head = head.trim();
    let looks_like_field = !head.is_empty()
        && head.len() <= 40
        && head
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.');
    looks_like_field.then(|| head.replace('_', " "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;

    fn rejection(status: StatusCode, body: &str) -> Response {
        (
            status,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            body.to_string(),
        )
            .into_response()
    }

    async fn read(res: Response) -> (StatusCode, String, String) {
        let status = res.status();
        let ctype = res
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let body = axum::body::to_bytes(res.into_body(), 8192).await.unwrap();
        (status, ctype, String::from_utf8_lossy(&body).to_string())
    }

    // The exact bodies axum 0.7 produced, measured live on this app 2026-10-26.
    const MISSING_EMAIL: &str =
        "Failed to deserialize the JSON body into the target type: missing field `email` at line 1 column 2";
    const WRONG_TYPE: &str = "Failed to deserialize the JSON body into the target type: email: invalid type: integer `123`, expected a string at line 1 column 45";
    const EMPTY: &str =
        "Failed to parse the request body as JSON: EOF while parsing a value at line 1 column 0";
    const NULL_BODY: &str =
        "Failed to deserialize the JSON body into the target type: invalid type: null, expected struct RegisterInput";

    #[tokio::test]
    async fn a_missing_field_names_the_field_in_plain_words() {
        let (status, ctype, body) =
            read(as_app_json(rejection(StatusCode::UNPROCESSABLE_ENTITY, MISSING_EMAIL)).await)
                .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "body was {body}");
        assert!(ctype.starts_with("application/json"), "ctype was {ctype}");
        assert!(
            body.contains("Enter your email address."),
            "body was {body}"
        );
        for leak in ["deserialize", "struct", "line 1 column", "missing field"] {
            assert!(!body.contains(leak), "leaked {leak}: {body}");
        }
    }

    #[tokio::test]
    async fn a_wrong_type_names_the_field_but_not_the_grammar() {
        let (status, _, body) =
            read(as_app_json(rejection(StatusCode::UNPROCESSABLE_ENTITY, WRONG_TYPE)).await).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body.contains("email: that value is not valid"),
            "body was {body}"
        );
        assert!(!body.contains("invalid type"), "body was {body}");
        assert!(!body.contains("expected a string"), "body was {body}");
    }

    #[tokio::test]
    async fn unparsable_json_says_so() {
        let (status, _, body) =
            read(as_app_json(rejection(StatusCode::BAD_REQUEST, EMPTY)).await).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("not valid JSON"), "body was {body}");
        assert!(!body.contains("EOF"), "body was {body}");

        // A body that is JSON but not an OBJECT (null, or an array) names the fix, not the type.
        let (status, _, body) =
            read(as_app_json(rejection(StatusCode::UNPROCESSABLE_ENTITY, NULL_BODY)).await).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body.contains("Send the form's fields as a JSON object."),
            "body was {body}"
        );
        for leak in ["struct", "RegisterInput", "invalid type"] {
            assert!(!body.contains(leak), "leaked {leak}: {body}");
        }
    }

    #[tokio::test]
    async fn a_too_large_upload_stays_a_413() {
        let (status, _, body) = read(
            as_app_json(rejection(
                StatusCode::PAYLOAD_TOO_LARGE,
                "length limit exceeded",
            ))
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert!(body.contains("too large"), "body was {body}");
        assert!(body.contains("\"code\":413"), "body was {body}");
    }

    #[tokio::test]
    async fn a_handlers_own_json_error_is_untouched() {
        let res = (
            StatusCode::BAD_REQUEST,
            [(header::CONTENT_TYPE, "application/json")],
            r#"{"error":"Invalid credentials","code":400}"#.to_string(),
        )
            .into_response();
        let (status, _, body) = read(as_app_json(res).await).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("Invalid credentials"), "body was {body}");
    }

    #[tokio::test]
    async fn a_successful_response_is_untouched() {
        let res = (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            r#"{"ok":true}"#.to_string(),
        )
            .into_response();
        let (status, _, body) = read(as_app_json(res).await).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("\"ok\":true"), "body was {body}");
    }
}
