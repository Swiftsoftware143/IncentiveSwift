//! Profile-field length validation, in ONE place, at the boundary that writes it.
//!
//! `accounts.company` and `accounts.username` are `character varying(255)`. Before this module
//! `update_profile` (PUT /api/v1/auth/profile) passed the caller's value straight into the `UPDATE`
//! with no length check, so a value the column cannot hold reached Postgres, came back as
//! `22001 value too long for type character varying(255)`, and [`crate::error::AppError::Database`]
//! answered the caller a bare
//!
//!   500 {"code":500,"error":"Internal server error"}
//!
//! — the reason discarded, the row unchanged, and nothing the operator could act on. Measured LIVE
//! 2026-10-16 on this app:
//!
//!   PUT /api/v1/auth/profile {"company":"<300 chars>"}   -> 500  (row.company unchanged)
//!   PUT /api/v1/auth/profile {"username":"<300 chars>"}  -> 500  (row.username NULL)
//!   PUT /api/v1/auth/profile {"company":"Acme Widgets"}   -> 200  (the control: the route works)
//!
//! `accounts.name` is `TEXT`, so the same over-long input did NOT fail there — it stored silently
//! (measured: a 5000-character name landed whole). That is the same defect with a quieter face: the
//! name is what the console header and every "signed in as" line print. All three fields are capped
//! here so one call answers the same shape for both.
//!
//! The cap is the fleet's display-name width ([`COLUMN_CHARS`] = 255, the same constant three
//! sibling apps carry in their own `security/display_name.rs`), not a new product rule: anything
//! that was storable before is still storable, and everything a console can render still fits.

/// `accounts.company` / `accounts.username` are `character varying(255)`; the fleet's display-name
/// width is the same 255 in ADASwift / FunnelSwift / WorkflowSwift.
pub const COLUMN_CHARS: usize = 255;

/// Longest value any profile field accepts.
pub const MAX_PROFILE_FIELD_CHARS: usize = COLUMN_CHARS;

/// Enforce ONLY the length limit, for a field that may legitimately be EMPTY (`company` and
/// `username` are clearable from the profile screen). A blank value is the caller's business; a
/// value the column cannot hold is not, and used to surface as a raw 500.
///
/// Counted in CHARACTERS, which is what `varchar(n)` measures: 200 accented letters are 200
/// characters (400 bytes) and fit a `varchar(255)` column.
pub fn check_len(raw: &str, max_chars: usize, label: &str) -> Result<(), String> {
    if raw.trim().chars().count() > max_chars {
        return Err(format!("{label}: is longer than {max_chars} characters"));
    }
    Ok(())
}

/// The same guard for a field that must also be NON-EMPTY and must not carry control characters
/// (the name is rendered into HTML and into mail subjects). Returns the trimmed value to persist —
/// the caller must bind THIS, never the raw input.
pub fn normalize_name(raw: &str) -> Result<String, String> {
    let value = raw.trim();

    if value.is_empty() {
        return Err("name: is required".to_string());
    }
    if value.chars().count() > MAX_PROFILE_FIELD_CHARS {
        return Err(format!(
            "name: is longer than {MAX_PROFILE_FIELD_CHARS} characters"
        ));
    }
    if value.chars().any(|c| c.is_control()) {
        return Err("name: must not contain control characters".to_string());
    }

    Ok(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_the_length_that_produced_a_500() {
        // Measured live 2026-10-16: 300 characters answered a bare 500 for company and username.
        assert_eq!(
            check_len(&"C".repeat(300), COLUMN_CHARS, "company"),
            Err("company: is longer than 255 characters".to_string())
        );
        assert_eq!(
            check_len(&"U".repeat(300), COLUMN_CHARS, "username"),
            Err("username: is longer than 255 characters".to_string())
        );
    }

    #[test]
    fn the_cap_itself_still_passes() {
        assert!(check_len(&"C".repeat(255), COLUMN_CHARS, "company").is_ok());
        assert!(check_len("Acme Widgets", COLUMN_CHARS, "company").is_ok());
        // An empty value is legal for the clearable fields.
        assert!(check_len("", COLUMN_CHARS, "company").is_ok());
    }

    #[test]
    fn the_name_arm_requires_a_value_and_fits_the_cap() {
        assert_eq!(normalize_name("   "), Err("name: is required".to_string()));
        assert_eq!(normalize_name("Ada Lovelace").unwrap(), "Ada Lovelace");
        assert!(normalize_name(&"N".repeat(5000)).is_err());
        assert!(normalize_name(&"N".repeat(MAX_PROFILE_FIELD_CHARS)).is_ok());
        assert!(normalize_name("Ada\nLovelace").is_err());
    }
}
