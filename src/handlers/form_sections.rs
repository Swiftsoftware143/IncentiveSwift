//! Section overlay for the long-form qualifier form.
//!
//! `config.sections` is an ORDERING-ONLY overlay:
//!
//! ```json
//! [{ "title": "About you", "questions": ["q1", "q2"] }]
//! ```
//!
//! It decides the presentation order of the public form and nothing else. Scoring,
//! outcomes, tags, redirects and the outcome webhook all keep reading `config.rules`
//! exactly as they did before this overlay existed — `long_form_qualifier_handler.rs`
//! never looks at `sections`.
//!
//! Invariants (proven live, not asserted):
//!   * unknown / typo'd question keys are IGNORED, never an error;
//!   * a question not named in any section still appears, in a trailing group,
//!     so it can never be silently dropped;
//!   * a config with no `sections` resolves to the flat `config.rules` order.

use serde_json::{json, Value};

/// Question keys in `config.rules` order (the source of truth for scoring).
fn rule_question_keys(config: &Value) -> Vec<String> {
    config
        .get("rules")
        .and_then(|r| r.as_array())
        .map(|rules| {
            rules
                .iter()
                .filter_map(|r| r.get("question").and_then(|q| q.as_str()))
                .filter(|q| !q.is_empty())
                .map(|q| q.to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// Resolve the ordering-only `config.sections` overlay into concrete display groups.
///
/// Returns at most one group per declared section (empty ones are dropped) plus, when
/// anything is left over, a single trailing group with `title: ""` holding every
/// question that no section named. Never fails: malformed input degrades to the flat
/// rule order.
pub fn resolve_form_sections(config: &Value) -> Vec<Value> {
    let keys = rule_question_keys(config);
    let mut seen: Vec<String> = Vec::new();
    let mut groups: Vec<Value> = Vec::new();

    if let Some(sections) = config.get("sections").and_then(|s| s.as_array()) {
        for section in sections {
            let title = section
                .get("title")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string();

            let mut questions: Vec<String> = Vec::new();
            if let Some(qs) = section.get("questions").and_then(|q| q.as_array()) {
                for q in qs {
                    let Some(key) = q.as_str() else { continue };
                    // Unknown keys are ignored (not an error); duplicates are dropped so a
                    // question can never render twice.
                    if keys.iter().any(|k| k == key)
                        && !seen.iter().any(|s| s == key)
                        && !questions.iter().any(|s| s == key)
                    {
                        questions.push(key.to_string());
                        seen.push(key.to_string());
                    }
                }
            }

            if !questions.is_empty() {
                groups.push(json!({ "title": title, "questions": questions }));
            }
        }
    }

    // Anything no section named still appears — trailing, never silently dropped.
    let leftover: Vec<String> = keys
        .iter()
        .filter(|k| !seen.iter().any(|s| s == *k))
        .cloned()
        .collect();
    if !leftover.is_empty() {
        groups.push(json!({ "title": "", "questions": leftover }));
    }

    groups
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn no_sections_is_flat_rule_order() {
        let cfg = json!({ "rules": [
            { "question": "q1" }, { "question": "q2" }, { "question": "q3" }]});
        assert_eq!(
            resolve_form_sections(&cfg),
            vec![json!({ "title": "", "questions": ["q1", "q2", "q3"] })]
        );
    }

    #[test]
    fn unknown_keys_ignored_and_ungrouped_trailing() {
        let cfg = json!({
            "rules": [{ "question": "q1" }, { "question": "q2" }, { "question": "q3" }],
            "sections": [
                { "title": "A", "questions": ["q1", "typo_nope"] },
                { "title": "B", "questions": ["q2"] }
            ]
        });
        assert_eq!(
            resolve_form_sections(&cfg),
            vec![
                json!({ "title": "A", "questions": ["q1"] }),
                json!({ "title": "B", "questions": ["q2"] }),
                json!({ "title": "", "questions": ["q3"] }),
            ]
        );
    }

    #[test]
    fn malformed_sections_degrade_to_flat() {
        let cfg = json!({ "rules": [{ "question": "q1" }], "sections": "not-an-array" });
        assert_eq!(
            resolve_form_sections(&cfg),
            vec![json!({ "title": "", "questions": ["q1"] })]
        );
    }
}
