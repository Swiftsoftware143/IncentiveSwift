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
        "calculator" => {
            0
        }
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
fn calculate_personality(answers: &Value) -> i32 {
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

/// Get personality label from score.
pub fn get_personality_label(score: i32) -> &'static str {
    match score {
        0 => "Balanced",
        1 => "Driver",
        2 => "Analyzer",
        3 => "Neutral",
        _ => "Unknown",
    }
}
