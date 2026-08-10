//! IQS field validation — enforces field-type rules on submission.
//!
//! Each question can declare a field type that controls strict input validation:
//!
//! | Field Type         | Validation                                      | Config Keys                   |
//! |--------------------|-------------------------------------------------|-------------------------------|
//! | `field_text`       | Free text, no restrictions                      | —                             |
//! | `field_textarea`   | Free text (long form), no restrictions          | —                             |
//! | `field_email`      | Valid email format `user@domain.tld`            | —                             |
//! | `field_phone`      | E.164 phone, country code selector              | `country_code_default`        |
//! | `field_url`        | Valid URL `https://...`                         | —                             |
//! | `field_number`     | Integer or decimal within optional range        | `min`, `max`, `step`, `unit`  |
//! | `field_rating`     | Number 1-5 or 1-10                              | `max_rating` (5 or 10)        |
//! | `field_date`       | ISO date `YYYY-MM-DD` or `YYYY-MM-DD HH:MM`     | `include_time`                |
//! | `field_file`       | File upload URL (validated as URL)              | `allowed_extensions`, `max_size_mb` |
//! | `field_consent`    | Must be `"true"` (required checkbox)             | `consent_label`               |
//! | `field_select`     | Must match one of the option values              | —                             |
//! | `single_choice`    | Must match one of the option values              | —                             |
//! | `multiple_choice`  | Accepts comma-separated values, each must match  | —                             |

use crate::error::AppError;
use serde_json::Value;

/// Validate a single answer value against a question's field-type rules.
/// `question_type` – the `question_type` column on `iqs_questions`
/// `config`         – the `config` JSONB from `iqs_questions`
/// `value`          – the answer string submitted by the contact
pub fn validate_field(question_type: &str, config: &Value, value: &str) -> Result<(), AppError> {
    match question_type {
        "field_email" => validate_email(value),
        "field_phone" => validate_phone(value, config),
        "field_url" => validate_url(value),
        "field_number" => validate_number(value, config),
        "field_rating" => validate_rating(value, config),
        "field_date" => validate_date(value, config),
        "field_file" => validate_file(value, config),
        "field_consent" => validate_consent(value),
        "field_select" => validate_select(value, config),
        "single_choice" => validate_single_choice(value, config),
        "multiple_choice" => validate_multiple_choice(value, config),
        "field_slider" => validate_slider(value, config),
        "field_dropdown" => validate_select(value, config),
        "field_image_choice" => validate_single_choice(value, config),
        "field_matrix" => validate_matrix(value, config),
        "field_text" | "field_textarea" => Ok(()), // free text
        _ => Ok(()),                               // pass through unknown types
    }
}

/// Validate an email address.
fn validate_email(value: &str) -> Result<(), AppError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(AppError::BadRequest("Email is required".into()));
    }
    let re = regex_lite::Regex::new(r"^[^\s@]+@[^\s@]+\.[^\s@]+$").unwrap();
    if !re.is_match(trimmed) {
        return Err(AppError::BadRequest(format!(
            "'{}' is not a valid email address",
            trimmed
        )));
    }
    Ok(())
}

/// Validate a phone number (E.164).
/// Accepts + followed by 7-15 digits.
fn validate_phone(value: &str, _config: &Value) -> Result<(), AppError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(AppError::BadRequest("Phone number is required".into()));
    }
    let e164 = regex_lite::Regex::new(r"^\+\d{7,15}$").unwrap();
    let us = regex_lite::Regex::new(r"^\+?1?\d{10}$").unwrap();
    if !e164.is_match(trimmed) && !us.is_match(trimmed) {
        return Err(AppError::BadRequest(format!(
            "'{}' is not a valid phone number. Use E.164 format (e.g. +12025551234)",
            trimmed
        )));
    }
    Ok(())
}

/// Validate a URL.
fn validate_url(value: &str) -> Result<(), AppError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(AppError::BadRequest("URL is required".into()));
    }
    let re = regex_lite::Regex::new(r"^https?://[^\s/$.?#].[^\s]*$").unwrap();
    if !re.is_match(trimmed) {
        return Err(AppError::BadRequest(format!(
            "'{}' is not a valid URL. Must start with http:// or https://",
            trimmed
        )));
    }
    Ok(())
}

/// Validate a number against optional min/max config.
/// Config: `{"min": 0, "max": 100, "step": 0.5}`
fn validate_number(value: &str, config: &Value) -> Result<(), AppError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(AppError::BadRequest("Number is required".into()));
    }
    let parsed: f64 = trimmed
        .parse()
        .map_err(|_| AppError::BadRequest(format!("'{}' is not a valid number", trimmed)))?;
    if let Some(min) = config.get("min").and_then(|v| v.as_f64()) {
        if parsed < min {
            return Err(AppError::BadRequest(format!(
                "Value {} is below minimum {}",
                parsed, min
            )));
        }
    }
    if let Some(max) = config.get("max").and_then(|v| v.as_f64()) {
        if parsed > max {
            return Err(AppError::BadRequest(format!(
                "Value {} exceeds maximum {}",
                parsed, max
            )));
        }
    }
    Ok(())
}

/// Validate a rating (1–max_rating, default 5).
/// Config: `{"max_rating": 10}`
fn validate_rating(value: &str, config: &Value) -> Result<(), AppError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(AppError::BadRequest("Rating is required".into()));
    }
    let max_rating = config
        .get("max_rating")
        .and_then(|v| v.as_i64())
        .unwrap_or(5);
    let parsed: i64 = trimmed
        .parse()
        .map_err(|_| AppError::BadRequest(format!("'{}' is not a valid rating number", trimmed)))?;
    if parsed < 1 || parsed > max_rating {
        return Err(AppError::BadRequest(format!(
            "Rating must be between 1 and {}",
            max_rating
        )));
    }
    Ok(())
}

/// Validate a date string.
/// Config: `{"include_time": true}` allows datetime format.
/// Accepts YYYY-MM-DD (date) or YYYY-MM-DD HH:MM (datetime).
fn validate_date(value: &str, config: &Value) -> Result<(), AppError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(AppError::BadRequest("Date is required".into()));
    }
    let include_time = config
        .get("include_time")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if include_time {
        // Accept both date-only (YYYY-MM-DD) and datetime formats
        let is_valid = chrono::NaiveDate::parse_from_str(trimmed, "%Y-%m-%d").is_ok()
            || chrono::NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%d %H:%M").is_ok()
            || chrono::NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%dT%H:%M").is_ok();
        if !is_valid {
            return Err(AppError::BadRequest(format!(
                "'{}' is not a valid date/datetime. Use format YYYY-MM-DD or YYYY-MM-DD HH:MM",
                trimmed
            )));
        }
    } else if chrono::NaiveDate::parse_from_str(trimmed, "%Y-%m-%d").is_err() {
        return Err(AppError::BadRequest(format!(
            "'{}' is not a valid date. Use format YYYY-MM-DD",
            trimmed
        )));
    }
    Ok(())
}

/// Validate a file upload URL.
/// Config: `{"allowed_extensions": [".pdf",".jpg"], "max_size_mb": 10}`
/// We validate that the value is a valid URL (files are uploaded separately and
/// the URL is stored in the answer). Future: add MIME-type checking.
fn validate_file(value: &str, _config: &Value) -> Result<(), AppError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        // File uploads are optional unless required is set
        return Ok(());
    }
    // Validate it's a URL
    validate_url(trimmed)?;
    Ok(())
}

/// Validate consent — must be exactly "true".
fn validate_consent(value: &str) -> Result<(), AppError> {
    let trimmed = value.trim().to_lowercase();
    if trimmed != "true" {
        return Err(AppError::BadRequest("You must agree to proceed".into()));
    }
    Ok(())
}

/// Validate a select field — must match one of the option values.
fn validate_select(value: &str, config: &Value) -> Result<(), AppError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(AppError::BadRequest("Selection is required".into()));
    }
    let options = match config.get("options").and_then(|v| v.as_array()) {
        Some(o) => o,
        None => return Ok(()), // no options defined → pass through
    };
    let valid = options
        .iter()
        .any(|o| o.get("value").and_then(|v| v.as_str()) == Some(trimmed));
    if !valid {
        return Err(AppError::BadRequest(format!(
            "'{}' is not a valid selection",
            trimmed
        )));
    }
    Ok(())
}

/// Validate a slider value (number within min/max/step range).
/// Config: `{"min": 0, "max": 100, "step": 1}`
fn validate_slider(value: &str, config: &Value) -> Result<(), AppError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(AppError::BadRequest("Slider value is required".into()));
    }
    let parsed: i64 = trimmed
        .parse()
        .map_err(|_| AppError::BadRequest(format!("'{}' is not a valid number", trimmed)))?;
    let min = config.get("min").and_then(|v| v.as_i64()).unwrap_or(0);
    let max = config.get("max").and_then(|v| v.as_i64()).unwrap_or(100);
    let step = config.get("step").and_then(|v| v.as_i64()).unwrap_or(1);
    if parsed < min || parsed > max {
        return Err(AppError::BadRequest(format!(
            "Value must be between {} and {}",
            min, max
        )));
    }
    if (parsed - min) % step != 0 {
        return Err(AppError::BadRequest(format!(
            "Value must be in steps of {}",
            step
        )));
    }
    Ok(())
}

/// Validate a matrix/grid answer.
/// Value format: JSON object `{"row_key": "col_value", ...}`
/// Config: `{"rows": [{"key": "q", "label": "Quality"}, ...], "columns": [{"label": "1", "value": "1"}, ...]}`
fn validate_matrix(value: &str, config: &Value) -> Result<(), AppError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(AppError::BadRequest("Matrix is required".into()));
    }
    let parsed: Value = serde_json::from_str(trimmed)
        .map_err(|_| AppError::BadRequest("Invalid matrix format".into()))?;
    let obj = parsed
        .as_object()
        .ok_or_else(|| AppError::BadRequest("Matrix must be a JSON object".into()))?;
    let rows = config
        .get("rows")
        .and_then(|v| v.as_array())
        .ok_or_else(|| AppError::BadRequest("Matrix config missing rows".into()))?;
    let columns = config
        .get("columns")
        .and_then(|v| v.as_array())
        .ok_or_else(|| AppError::BadRequest("Matrix config missing columns".into()))?;
    let valid_col_values: Vec<&str> = columns
        .iter()
        .filter_map(|c| c.get("value").and_then(|v| v.as_str()))
        .collect();
    for row in rows {
        let key = row
            .get("key")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::BadRequest("Matrix row missing 'key'".into()))?;
        let answer = obj.get(key).and_then(|v| v.as_str()).unwrap_or("");
        if answer.is_empty() {
            return Err(AppError::BadRequest(format!(
                "Matrix row '{}' is unanswered",
                key
            )));
        }
        if !valid_col_values.contains(&answer) {
            return Err(AppError::BadRequest(format!(
                "'{}' is not a valid option for row '{}'",
                answer, key
            )));
        }
    }
    Ok(())
}

/// Validate single_choice — must match one of the question's options.
/// Options come from the `options` JSONB column, not config.
fn validate_single_choice(value: &str, config: &Value) -> Result<(), AppError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(AppError::BadRequest("Selection is required".into()));
    }
    let options = match config.get("options").and_then(|v| v.as_array()) {
        Some(o) => o,
        None => return Ok(()),
    };
    let valid = options
        .iter()
        .any(|o| o.get("value").and_then(|v| v.as_str()) == Some(trimmed));
    if !valid {
        return Err(AppError::BadRequest(format!(
            "'{}' is not a valid option",
            trimmed
        )));
    }
    Ok(())
}

/// Validate multiple_choice — comma-separated values, each must match an option.
fn validate_multiple_choice(value: &str, config: &Value) -> Result<(), AppError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(AppError::BadRequest(
            "At least one selection is required".into(),
        ));
    }
    let options = match config.get("options").and_then(|v| v.as_array()) {
        Some(o) => o,
        None => return Ok(()),
    };
    let values: Vec<&str> = trimmed.split(',').map(|s| s.trim()).collect();
    for val in &values {
        let valid = options
            .iter()
            .any(|o| o.get("value").and_then(|v| v.as_str()) == Some(val));
        if !valid {
            return Err(AppError::BadRequest(format!(
                "'{}' is not a valid option in the list",
                val
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // --- Email ---
    #[test]
    fn test_valid_emails() {
        assert!(validate_email("user@example.com").is_ok());
        assert!(validate_email("a.b@c.co").is_ok());
        assert!(validate_email("test+label@domain.org").is_ok());
    }
    #[test]
    fn test_invalid_emails() {
        assert!(validate_email("").is_err());
        assert!(validate_email("notanemail").is_err());
        assert!(validate_email("@domain.com").is_err());
    }

    // --- Phone ---
    #[test]
    fn test_valid_phones() {
        assert!(validate_phone("+12025551234", &json!({})).is_ok());
        assert!(validate_phone("+447911123456", &json!({})).is_ok());
    }
    #[test]
    fn test_invalid_phones() {
        assert!(validate_phone("", &json!({})).is_err());
        assert!(validate_phone("abc", &json!({})).is_err());
    }

    // --- URL ---
    #[test]
    fn test_valid_urls() {
        assert!(validate_url("https://example.com").is_ok());
        assert!(validate_url("http://example.com/file.pdf").is_ok());
    }
    #[test]
    fn test_invalid_urls() {
        assert!(validate_url("").is_err());
        assert!(validate_url("not-a-url").is_err());
        assert!(validate_url("ftp://bad").is_err());
    }

    // --- Number ---
    #[test]
    fn test_valid_numbers() {
        assert!(validate_number("42", &json!({})).is_ok());
        assert!(validate_number("3.14", &json!({})).is_ok());
        assert!(validate_number("-5", &json!({})).is_ok());
        assert!(validate_number("50", &json!({"min": 0, "max": 100})).is_ok());
    }
    #[test]
    fn test_invalid_numbers() {
        assert!(validate_number("", &json!({})).is_err());
        assert!(validate_number("abc", &json!({})).is_err());
        assert!(validate_number("150", &json!({"max": 100})).is_err());
        assert!(validate_number("-1", &json!({"min": 0})).is_err());
    }

    // --- Rating ---
    #[test]
    fn test_valid_ratings() {
        assert!(validate_rating("3", &json!({})).is_ok());
        assert!(validate_rating("10", &json!({"max_rating": 10})).is_ok());
        assert!(validate_rating("1", &json!({"max_rating": 5})).is_ok());
        assert!(validate_rating("5", &json!({"max_rating": 5})).is_ok());
    }
    #[test]
    fn test_invalid_ratings() {
        assert!(validate_rating("", &json!({})).is_err());
        assert!(validate_rating("0", &json!({})).is_err());
        assert!(validate_rating("6", &json!({"max_rating": 5})).is_err());
        assert!(validate_rating("abc", &json!({})).is_err());
    }

    // --- Date ---
    #[test]
    fn test_valid_dates() {
        assert!(validate_date("2026-07-16", &json!({})).is_ok());
        assert!(validate_date("2026-07-16 14:30", &json!({"include_time": true})).is_ok());
        assert!(validate_date("2026-07-16T14:30", &json!({"include_time": true})).is_ok());
    }
    #[test]
    fn test_invalid_dates() {
        assert!(validate_date("", &json!({})).is_err());
        assert!(validate_date("07-16-2026", &json!({})).is_err());
        assert!(validate_date("2026-07-16", &json!({"include_time": true})).is_ok());
        // date-only is OK with include_time
    }

    // --- Consent ---
    #[test]
    fn test_consent() {
        assert!(validate_consent("true").is_ok());
        assert!(validate_consent("True").is_ok());
        assert!(validate_consent("").is_err());
        assert!(validate_consent("false").is_err());
        assert!(validate_consent("yes").is_err());
    }

    // --- Select ---
    #[test]
    fn test_select() {
        let cfg = json!({"options": [{"value": "a", "label": "A"}, {"value": "b", "label": "B"}]});
        assert!(validate_select("a", &cfg).is_ok());
        assert!(validate_select("b", &cfg).is_ok());
        assert!(validate_select("c", &cfg).is_err());
        assert!(validate_select("", &json!({})).is_err());
    }
}
