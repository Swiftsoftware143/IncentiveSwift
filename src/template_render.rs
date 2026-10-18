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
//!
//! t_cdc24339 gave the renderer exactly ONE conditional: `{{#if key}}…{{/if}}` (see
//! [`resolve_sections`]). Every other sigil (`#each`, `^`, `else`, `!`, `>`) is still markup this
//! crate cannot process and is still named loudly. The reason is copy, not convenience: the
//! shipped `win_voucher` row promises "Code: {{reward_code}}.", and a prize that comes from the
//! campaign config has no code — a flat renderer could only ship the promise verbatim or drop the
//! whole prize sentence. A section lets the ROW state its own condition, so nothing has to be
//! rewritten in code when the copy changes.

use serde_json::Value;
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

/// Is `key` present AND non-empty in `vars`? The one truthiness rule the section resolver uses.
///
/// A flat key (`reward_code`) wins over a pointer path so a caller that hands the renderer
/// pre-flattened keys (`output_actions`' `contact.email`) is answered from its own map, while a
/// nested caller (`campaigns.config`) is answered by the pointer. Absent, `null`, `false`, a blank
/// string and an empty array/object are all "not there" — that is what makes an optional clause
/// disappear rather than print an empty sentence.
fn section_is_truthy(vars: &Value, name: &str) -> bool {
    let direct = vars.get(name);
    let nested = if direct.is_none() && name.contains('.') {
        let ptr = format!("/{}", name.replace('.', "/"));
        vars.pointer(&ptr)
    } else {
        None
    };
    match direct.or(nested) {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => !s.trim().is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
        Some(Value::Number(_)) => true,
    }
}

/// Resolve `{{#if key}}…{{/if}}` SECTIONS against `vars`, leaving every other byte alone.
///
/// The renderers here substitute FLAT placeholders and nothing else, so a stored row that promises
/// something optional could only ship its promise verbatim to a recipient who does not have the
/// datum. Measured live 2026-10-18 (kanban t_cdc24339): a `spin_wheel` voucher whose prize came
/// from `campaigns.config` — a prize with no redemption code — mailed the recipient
/// `Code: {{reward_code}}.` literally. The renderer implements no mustache sections, and the card
/// that added the loud warn (t_c8df11e6) deliberately did not teach it any.
///
/// So this is ONE conditional, with a deliberately small surface:
///
///   * `{{#if key}}…{{/if}}` — the content is kept only when `key` resolves to a present, non-empty
///     value in `vars` ([`section_is_truthy`]); nested sections are allowed and inherit their
///     parent's decision.
///   * `{{#each …}}`, `{{^…}}`, `{{else}}`, `{{!…}}`, `{{>…}}` are NOT processed and are passed
///     through verbatim, so `unprocessed_scaffolding` still names them.
///   * an UNCLOSED `{{#if}}` (or a stray `{{/if}}`) is left in place, byte for byte, rather than
///     being swallowed: the recipient's copy is never deleted on a guess, and the leftover marker
///     is still reported.
///
/// The caller runs this BEFORE substituting placeholders and BEFORE
/// [`warn_unsubstituted`], so a dropped clause reports nothing (there is nothing left to
/// substitute) while a kept one is still checked like any other copy.
pub fn resolve_sections(template: &str, vars: &Value) -> String {
    struct Frame {
        keep: bool,
        opener: String,
        buf: String,
    }

    let mut stack: Vec<Frame> = vec![Frame {
        keep: true,
        opener: String::new(),
        buf: String::new(),
    }];
    let mut rest = template;

    loop {
        let Some(open) = rest.find("{{") else {
            if let Some(top) = stack.last_mut() {
                top.buf.push_str(rest);
            }
            break;
        };
        let (before, tail) = rest.split_at(open);
        if let Some(top) = stack.last_mut() {
            top.buf.push_str(before);
        }
        let Some(close) = tail.find("}}") else {
            if let Some(top) = stack.last_mut() {
                top.buf.push_str(tail);
            }
            break;
        };
        let token = &tail[..close + 2];
        let inner = tail[2..close].trim();

        if let Some(cond) = inner.strip_prefix("#if") {
            let name = cond.trim();
            let parent_keep = stack.last().map(|f| f.keep).unwrap_or(true);
            let keep = parent_keep && !name.is_empty() && section_is_truthy(vars, name);
            stack.push(Frame {
                keep,
                opener: token.to_string(),
                buf: String::new(),
            });
        } else if inner == "/if" && stack.len() > 1 {
            // A closing marker with no opener is NOT ours to interpret: fall through and keep it.
            if let Some(frame) = stack.pop() {
                if frame.keep {
                    if let Some(top) = stack.last_mut() {
                        top.buf.push_str(&frame.buf);
                    }
                }
            }
        } else if let Some(top) = stack.last_mut() {
            top.buf.push_str(token);
        }

        rest = &tail[close + 2..];
    }

    // Unclosed sections: hand the author's bytes back (opener included) instead of guessing where
    // the block ended. `unprocessed_scaffolding` then names the marker, which is the loud failure.
    while stack.len() > 1 {
        if let Some(frame) = stack.pop() {
            let mut text = frame.opener;
            text.push_str(&frame.buf);
            if let Some(top) = stack.last_mut() {
                top.buf.push_str(&text);
            }
        }
    }

    stack.pop().map(|f| f.buf).unwrap_or_default()
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
             double-brace vocabulary (GET /api/v1/email-templates/merge-fields), and only the \
             #if-key conditional (this renderer implements no other markup)"
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
    fn resolves_an_optional_clause_and_leaves_every_other_byte_alone() {
        // The SHIPPED voucher copy after zz_is17 (kanban t_cdc24339). Sections only decide
        // presence; substitution is the caller's job, so the placeholders survive this step.
        let tpl = "You won {{prize_name}} on {{campaign_name}}!\
                   {{#if reward_code}} Code: {{reward_code}}.{{/if}}";
        let with_code = serde_json::json!({"prize_name": "Free coffee", "reward_code": "ABC123"});
        let without = serde_json::json!({"prize_name": "Free coffee"});
        assert_eq!(
            resolve_sections(tpl, &with_code),
            "You won {{prize_name}} on {{campaign_name}}! Code: {{reward_code}}."
        );
        assert_eq!(
            resolve_sections(tpl, &without),
            "You won {{prize_name}} on {{campaign_name}}!"
        );

        // Blank, null, false and empty containers are "not there" — an optional clause must never
        // render as an empty sentence (" Code: .").
        for empty in [
            serde_json::json!({"reward_code": ""}),
            serde_json::json!({"reward_code": "   "}),
            serde_json::json!({"reward_code": null}),
            serde_json::json!({"reward_code": false}),
            serde_json::json!({"reward_code": []}),
            serde_json::json!({"reward_code": {}}),
            serde_json::json!({}),
        ] {
            assert_eq!(
                resolve_sections("A{{#if reward_code}} code{{/if}}B", &empty),
                "AB",
                "expected {empty} to read as absent"
            );
        }
        // A number is a value (a 0 is mustache-truthy: it was PRESENT).
        assert_eq!(
            resolve_sections("A{{#if x}} code{{/if}}B", &serde_json::json!({"x": 0})),
            "A codeB"
        );
        // A dotted name is answered from a nested var map (output_actions' payload shape).
        assert_eq!(
            resolve_sections(
                "A{{#if contact.email}} email{{/if}}B",
                &serde_json::json!({"contact": {"email": "a@b.test"}})
            ),
            "A emailB"
        );
    }

    #[test]
    fn nested_sections_inherit_their_parent_and_unsupported_markup_is_untouched() {
        let vars = serde_json::json!({"a": "yes"});
        assert_eq!(
            resolve_sections("{{#if a}}1{{#if b}}2{{/if}}3{{/if}}", &vars),
            "13",
            "an inner section with no value drops only its own content"
        );
        assert_eq!(
            resolve_sections(
                "{{#if a}}1{{#if b}}2{{/if}}3{{/if}}",
                &serde_json::json!({"b": 1})
            ),
            "",
            "a dropped parent drops the whole block"
        );
        // Only #if is implemented: every other sigil is left for `unprocessed_scaffolding`.
        let other = "{{#each xs}}{{x}}{{else}}none{{/each}}{{^empty}}n/a{{/empty}}";
        assert_eq!(resolve_sections(other, &vars), other);
        assert!(!unprocessed_scaffolding(other).is_empty());
        // An unclosed #if is handed back verbatim (copy is never deleted on a guess) and named.
        let unclosed = "A{{#if b}}B";
        assert_eq!(resolve_sections(unclosed, &vars), unclosed);
        assert_eq!(
            unprocessed_scaffolding(&resolve_sections(unclosed, &vars)),
            vec!["{{#if b}}"]
        );
        // A stray close marker is not interpreted either.
        assert_eq!(resolve_sections("A{{/if}}B", &vars), "A{{/if}}B");
        // No braces at all, and CSS/JSON braces, are untouched.
        assert_eq!(
            resolve_sections("a { color: red; }", &vars),
            "a { color: red; }"
        );
        assert_eq!(resolve_sections("", &vars), "");
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
