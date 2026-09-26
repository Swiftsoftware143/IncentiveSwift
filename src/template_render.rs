//! Placeholder-vocabulary enforcement for every template renderer in this crate.
//!
//! Every renderer here substitutes the DOUBLE-brace vocabulary (`{{key}}`), and that is also
//! what BOTH sides of the admin surface advertise: the in-app merge-field list
//! (`handlers::email_templates_handler::merge_field_list`, served as `{{token}}` from
//! `GET /api/v1/email-templates/merge-fields`) and the served console's system-mail buttons
//! (`MERGE_FIELDS` in `www-admin/index.html`). Kanban t_e43521d2 measured the one document
//! that disagreed with them: the shipped `email_templates` row for `welcome` spoke single
//! braces (`Welcome to {app_name}!`), and because no renderer binds that style, every one of
//! its 13 placeholders went out verbatim.
//!
//! The arm chosen is (b) — make the stored rows use the vocabulary the renderers and the UI
//! already speak — plus this module's second half: make a LEFTOVER placeholder LOUD instead
//! of teaching the renderers a second brace style. Tolerating `{key}` as well would hide the
//! drift that produced this defect (the next single-brace row would render by accident and
//! nobody would learn the vocabulary was still split) and would corrupt any HTML/CSS body,
//! where `{` and `}` are ordinary characters.

use std::sync::OnceLock;

/// Matches `{name}`, `{{name}}` and `{a.b}` and captures the bare name.
///
/// A doubled brace is deliberately NOT special-cased: after substitution a surviving
/// `{{name}}` is exactly as unsubstituted as `{name}` is, and the name is what the log
/// needs. `{{#if x}}` / `{{/if}}` (the mustache blocks the seeded winner row carries) do
/// not match — `#` and `/` are not identifier characters — so conditional scaffolding is
/// never reported as a placeholder.
fn placeholder_re() -> Option<&'static regex_lite::Regex> {
    static RE: OnceLock<Option<regex_lite::Regex>> = OnceLock::new();
    RE.get_or_init(|| regex_lite::Regex::new(r"\{([A-Za-z_][A-Za-z0-9_.]*)\}").ok())
        .as_ref()
}

/// Names of the placeholders still present in `rendered` — deduplicated, in first-seen
/// order. Empty for every correctly rendered template.
pub fn unsubstituted(rendered: &str) -> Vec<String> {
    let Some(re) = placeholder_re() else {
        // The pattern is a constant, so this arm is unreachable in practice; returning
        // "nothing left over" is the only non-panicking answer (guardrail: no unwrap).
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for caps in re.captures_iter(rendered) {
        if let Some(m) = caps.get(1) {
            let name = m.as_str().to_string();
            if !out.contains(&name) {
                out.push(name);
            }
        }
    }
    out
}

/// Warn — at `warn!`, with the placeholder NAMES — about everything a render left
/// unsubstituted, then return the names so a caller (or a probe) can assert on them.
///
/// This is the loud half of the fix: a placeholder whose name the sender never bound is a
/// defect the recipient would otherwise receive as literal braces, indistinguishable from
/// deliberate copy.
pub fn warn_unsubstituted(rendered: &str, context: &str) -> Vec<String> {
    let left = unsubstituted(rendered);
    if !left.is_empty() {
        tracing::warn!(
            context = %context,
            placeholders = ?left,
            "template placeholders were NOT substituted - the recipient receives the braces verbatim; \
             use the double-brace vocabulary (GET /api/v1/email-templates/merge-fields)"
        );
    }
    left
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_single_and_double_brace_leftovers_alike() {
        assert_eq!(unsubstituted("Welcome to {app_name}!"), vec!["app_name"]);
        assert_eq!(
            unsubstituted("{{app_name}} and {{login_url}}"),
            vec!["app_name", "login_url"]
        );
        assert_eq!(unsubstituted("{a.b}"), vec!["a.b"]);
        assert_eq!(
            unsubstituted("{x} {x} {y}"),
            vec!["x", "y"],
            "names are deduplicated"
        );
    }

    #[test]
    fn stays_quiet_on_rendered_copy_and_on_non_placeholder_braces() {
        assert!(unsubstituted("Welcome to IncentiveSwift, Dana!").is_empty());
        assert!(unsubstituted("").is_empty());
        // CSS / JSON / mustache scaffolding must not be reported.
        assert!(unsubstituted("a { color: red; }").is_empty());
        assert!(unsubstituted(r#"{"a":1}"#).is_empty());
        assert!(unsubstituted("{{#if prize_name}}x{{/if}}").is_empty());
        // ... but the merge field INSIDE that scaffolding still is.
        assert_eq!(
            unsubstituted("{{#if prize_name}}{prize_name}{{/if}}"),
            vec!["prize_name"]
        );
    }
}
