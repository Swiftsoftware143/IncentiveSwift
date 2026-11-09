//! Scoring engine — calculate score based on campaign type and answers.
//!
//! For score_reveal: sum of answer scores from question config.
//! For calculator: evaluate formula with answer values.
//! For personality: match answer patterns to predefined personality types.
//! Default: return 0.

use serde_json::Value;

/// Calculate a score based on campaign type and answers.
pub fn calculate_score(campaign_type: &str, answers: &Value) -> i32 {
    match campaign_type {
        "score_reveal" => calculate_score_reveal(answers),
        "calculator" => 0,
        "personality" => calculate_personality(answers),
        _ => 0,
    }
}

/// For score_reveal: sum scores from answer values if they are numeric.
fn calculate_score_reveal(answers: &Value) -> i32 {
    let mut total = 0;

    if let Some(obj) = answers.as_object() {
        for value in obj.values() {
            if let Some(n) = value.as_i64() {
                total += n as i32;
            } else if let Some(f) = value.as_f64() {
                total += f as i32;
            } else if let Some(s) = value.as_str() {
                if let Ok(n) = s.parse::<i32>() {
                    total += n;
                }
            }
        }
    }

    total
}

/// For personality: simple pattern matching to determine personality type.
/// Returns personality index (0-3) mapped to predefined types.
///
/// Public because the personality mechanic now HAS a play path (`handlers::entries`), which calls
/// it directly; before that this function was compiled and never executed (kanban t_3292150a).
pub fn calculate_personality(answers: &Value) -> i32 {
    let mut score_a = 0;
    let mut score_b = 0;

    if let Some(obj) = answers.as_object() {
        for value in obj.values() {
            let val_str = match value {
                Value::String(s) => s.to_lowercase(),
                Value::Number(n) => n.to_string(),
                _ => continue,
            };

            match val_str.as_str() {
                "a" | "yes" | "true" | "agree" | "strongly_agree" => score_a += 1,
                "b" | "no" | "false" | "disagree" | "strongly_disagree" => score_b += 1,
                _ => {}
            }
        }
    }

    if score_a >= 3 && score_b >= 3 {
        0 // "Balanced"
    } else if score_a > score_b {
        1 // "Type A - Driver"
    } else if score_b > score_a {
        2 // "Type B - Analyzer"
    } else {
        3 // "Neutral"
    }
}

/// The four result labels the engine can produce, indexed by the value `calculate_personality`
/// returns (0 Balanced, 1 Driver, 2 Analyzer, 3 Neutral).
pub const PERSONALITY_LABELS: [&str; 4] = ["Balanced", "Driver", "Analyzer", "Neutral"];

/// Get personality label from score.
pub fn get_personality_label(score: i32) -> &'static str {
    if score < 0 {
        return "Unknown";
    }
    PERSONALITY_LABELS
        .get(score as usize)
        .copied()
        .unwrap_or("Unknown")
}

/// Resolve a personality index to the (label, description) the VISITOR is shown.
///
/// The four result labels are the campaign OWNER's, not ours: `config.outcomes` carries them in
/// the engine's own index order —
///
/// ```json
/// "outcomes": [
///   { "label": "The Steady Hand", "description": "…" },   // 0 — Balanced
///   { "label": "The Trailblazer", "description": "…" },   // 1 — Driver
///   { "label": "The Analyst",     "description": "…" },   // 2 — Analyzer
///   { "label": "The Free Spirit", "description": "…" }    // 3 — Neutral
/// ]
/// ```
///
/// An entry may also carry an explicit `"type"`/`"index"` instead of relying on position. A
/// campaign that names none keeps the engine labels, so a personality quiz works out of the box
/// and the owner's four labels are a pure rename (kanban t_3292150a).
pub fn personality_outcome(index: i32, config: &Value) -> (String, String) {
    let fallback = get_personality_label(index).to_string();
    let Some(outcomes) = config.get("outcomes").and_then(Value::as_array) else {
        return (fallback, String::new());
    };
    let explicit = outcomes.iter().find(|o| {
        o.get("type")
            .or_else(|| o.get("index"))
            .and_then(Value::as_i64)
            == Some(index as i64)
    });
    let picked = explicit.or_else(|| usize::try_from(index).ok().and_then(|i| outcomes.get(i)));
    let Some(outcome) = picked else {
        return (fallback, String::new());
    };
    let label = outcome
        .get("label")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(fallback.as_str())
        .to_string();
    let description = outcome
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    (label, description)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn personality_counts_both_buckets_and_balances() {
        let driver = json!({"q1": "a", "q2": "A", "q3": "yes"});
        assert_eq!(calculate_personality(&driver), 1);
        let analyzer = json!({"q1": "b", "q2": "no", "q3": "disagree"});
        assert_eq!(calculate_personality(&analyzer), 2);
        // One of each: neither bucket wins. "Balanced" needs BOTH buckets at 3+, so this is Neutral.
        let neutral = json!({"q1": "a", "q2": "b"});
        assert_eq!(calculate_personality(&neutral), 3);
        let balanced = json!({"a1": "a", "a2": "a", "a3": "a", "b1": "b", "b2": "b", "b3": "b"});
        assert_eq!(calculate_personality(&balanced), 0);
    }

    #[test]
    fn personality_outcome_uses_owner_labels_by_position() {
        let config = json!({"outcomes": [
            {"label": "Steady", "description": "Level head"},
            {"label": "Trailblazer", "description": "First to move"},
            {"label": "Analyst", "description": "Checks twice"},
            {"label": "Free Spirit", "description": "Goes with it"}
        ]});
        assert_eq!(
            personality_outcome(1, &config),
            ("Trailblazer".to_string(), "First to move".to_string())
        );
        assert_eq!(personality_outcome(3, &config).0, "Free Spirit");
    }

    #[test]
    fn personality_outcome_falls_back_to_engine_labels() {
        assert_eq!(
            personality_outcome(2, &json!({})),
            ("Analyzer".to_string(), String::new())
        );
        // A slot with no usable label falls back too, instead of showing the visitor an empty type.
        let blank = json!({"outcomes": [{"description": "no label"}, {"label": "  "}]});
        assert_eq!(personality_outcome(1, &blank).0, "Driver");
        // An explicit index wins over position.
        let explicit = json!({"outcomes": [{"type": 3, "label": "Fourth"}]});
        assert_eq!(personality_outcome(3, &explicit).0, "Fourth");
    }
}
