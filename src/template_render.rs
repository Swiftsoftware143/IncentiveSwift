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
//!
//! t_c8df11e6 closed the other half of that blind spot: markup the renderer cannot process at
//! all — mustache block markers (`{{#if prize_name}}` / `{{/if}}`) — used to be neither
//! substituted NOR reported (`#` and `/` are not identifier characters), so a stored row that
//! carried them mailed the raw markers and NOBODY was told. Markers are still not placeholders,
//! but they are now named by `unprocessed_scaffolding` / `warn_unsubstituted` so a row that
//! drifts into the landing-page vocabulary (`templates/*.html` really does use handlebars;
//! these email renderers do not) can never ship silently again.

use std::sync::OnceLock;

/// Matches `{name}`, `{{name}}` and `{a.b}` and captures the bare name.
///
/// A doubled brace is deliberately NOT special-cased: after substitution a surviving
/// `{{name}}` is exactly as unsubstituted as `{name}` is, and the name is what the log
/// needs. `{{#if x}}` / `{{/if}}` (mustache block markers) do not match — `#` and `/` are not
/// identifier characters — so conditional markup is never reported as a placeholder NAME; it is
/// reported as unprocessable MARKUP by `unprocessed_scaffolding` instead (kanban t_c8df11e6).
fn placeholder_re() -> Option<&'static regex_lite::Regex> {
    static RE: OnceLock<Option<regex_lite::Regex>> = OnceLock::new();
    RE.get_or_init(|| regex_lite::Regex::new(r"\{([A-Za-z_][A-Za-z0-9_.]*)\}").ok())
        .as_ref()
}

/// Matches mustache BLOCK MARKERS — `{{#if x}}`, `{{#each x}}`, `{{/if}}`, `{{/each}}`,
/// `{{^inverted}}`, `{{!comment}}`, `{{>partial}}`, `{{else}}`.
///
/// A marker whose first character is a sigil is not a merge field: nothing in this crate
/// substitutes or evaluates it, and no renderer here implements conditionals. It is markup the
/// renderer will pass through to the recipient verbatim (t_c8df11e6).
fn scaffolding_re() -> Option<&'static regex_lite::Regex> {
    static RE: OnceLock<Option<regex_lite::Regex>> = OnceLock::new();
    RE.get_or_init(|| regex_lite::Regex::new(r"\{\{else\}\}|\{\{[#/^!>][^}]*\}\}").ok())
        .as_ref()
}

/// Every distinct mustache block MARKER still present in `rendered` — deduplicated, in
/// first-seen order. Empty for every template written in the one vocabulary this crate speaks.
///
/// These are the tokens that used to be invisible: `unsubstituted` cannot see them (they hold no
/// identifier), so a stored template could mail `{{#if prize_name}}` and nothing logged a word.
pub fn unprocessed_scaffolding(rendered: &str) -> Vec<String> {
    let Some(re) = scaffolding_re() else {
        // Constant pattern: unreachable in practice; "no markup found" is the only
        // non-panicking answer (guardrail: no unwrap in library code).
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for m in re.find_iter(rendered) {
        let token = m.as_str().to_string();
        if !out.contains(&token) {
            out.push(token);
        }
    }
    out
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
/// deliberate copy. Since t_c8df11e6 it also names mustache block MARKERS
/// (`unprocessed_scaffolding`) — markup this crate cannot process at all, which used to reach
/// the recipient with no log line whatsoever.
pub fn warn_unsubstituted(rendered: &str, context: &str) -> Vec<String> {
    let left = unsubstituted(rendered);
    let scaffolding = unprocessed_scaffolding(rendered);
    if !left.is_empty() || !scaffolding.is_empty() {
        tracing::warn!(
            context = %context,
            placeholders = ?left,
            scaffolding = ?scaffolding,
            "template markup was NOT processed - the recipient receives it verbatim; use the \
             double-brace vocabulary (GET /api/v1/email-templates/merge-fields) and no mustache \
             conditionals (this renderer implements none)"
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
        // CSS / JSON / mustache scaffolding must not be reported as placeholder NAMES.
        assert!(unsubstituted("a { color: red; }").is_empty());
        assert!(unsubstituted(r#"{"a":1}"#).is_empty());
        assert!(unsubstituted("{{#if prize_name}}x{{/if}}").is_empty());
        // ... but the merge field INSIDE that scaffolding still is.
        assert_eq!(
            unsubstituted("{{#if prize_name}}{prize_name}{{/if}}"),
            vec!["prize_name"]
        );
    }

    #[test]
    fn names_mustache_markers_the_renderer_cannot_process() {
        // The exact stored shape of the global default `winner` row (kanban t_c8df11e6). On the
        // RAW row the two merge fields are placeholders (and the sender binds both), while the
        // markers are invisible to `unsubstituted`; on the WIRE the fields are substituted and the
        // markers are all that is left — which is why they have to be named separately.
        let stored = "<p>You won the <b>{{campaign_name}}</b> campaign.</p>\
                      {{#if prize_name}}<p>Prize: <b>{{prize_name}}</b></p>{{/if}}";
        assert_eq!(unsubstituted(stored), vec!["campaign_name", "prize_name"]);
        assert_eq!(
            unprocessed_scaffolding(stored),
            vec!["{{#if prize_name}}", "{{/if}}"],
            "both markers are named IN FULL, in first-seen order"
        );

        // What really left the box before this card: the fields rendered, the markers verbatim.
        let on_the_wire = "<p>You won the <b>T1ADE personality 5</b> campaign.</p>\
                           {{#if prize_name}}<p>Prize: <b>Scratch Ticket</b></p>{{/if}}";
        assert_eq!(
            unprocessed_scaffolding(on_the_wire),
            vec!["{{#if prize_name}}", "{{/if}}"]
        );
        assert!(
            unsubstituted(on_the_wire).is_empty(),
            "and the markers are NOT placeholder names - which is exactly why they used to ship \
             with no log line at all"
        );

        // A marker used twice is reported once. The merge field inside a marker's argument is
        // NOT reported here (it is not a placeholder either: it is an argument, not copy).
        assert_eq!(
            unprocessed_scaffolding("{{#if a}}x{{/if}}{{/if}}"),
            vec!["{{#if a}}", "{{/if}}"]
        );
        // The other sigils mustache uses, and the one sigil-less marker.
        assert_eq!(
            unprocessed_scaffolding("{{#each xs}}{{x}}{{else}}none{{/each}}"),
            vec!["{{#each xs}}", "{{else}}", "{{/each}}"]
        );
        assert_eq!(
            unprocessed_scaffolding("{{^empty}}n/a{{/empty}}{{!comment}}{{>partial}}"),
            vec!["{{^empty}}", "{{/empty}}", "{{!comment}}", "{{>partial}}"]
        );
        // NOTHING is reported for the vocabulary this crate really speaks, nor for CSS/JSON, nor
        // for a legitimate JSON body whose nested braces start with a non-sigil character.
        for clean in [
            "Welcome, {{first_name}}!",
            "a { color: red; }",
            r#"{"a": {"b": 1}}"#,
            "{{prize_name}}",
            "",
        ] {
            assert!(
                unprocessed_scaffolding(clean).is_empty(),
                "must stay silent on {clean:?}"
            );
        }
        // An unclosed marker is still markup, and the argument-free marker is still a marker.
        assert_eq!(unprocessed_scaffolding("{{#if}}"), vec!["{{#if}}"]);
    }
}
