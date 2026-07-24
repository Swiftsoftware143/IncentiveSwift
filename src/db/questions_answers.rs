//! Questions and answers database operations.
//! Question text ALWAYS comes from the questions table, never from raw JSONB.

use crate::error::AppError;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

/// Full question record from DB (includes correct_answer for admin/backend use).
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Question {
    pub id: Uuid,
    pub campaign_id: Uuid,
    pub question_key: String,
    pub question_text: String,
    pub question_type: String,
    pub sort_order: i32,
    pub correct_answer: Option<String>,
    pub score_weight: i32,
    pub options: Option<serde_json::Value>,
    pub crm_field: Option<String>,
    pub crm_field_type: Option<String>,
}

/// Public question (no correct_answer exposed to frontend).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublicQuestion {
    pub id: Uuid,
    pub question_key: String,
    pub question_text: String,
    pub question_type: String,
    pub sort_order: i32,
    pub score_weight: i32,
    pub options: Option<serde_json::Value>,
}

impl From<Question> for PublicQuestion {
    fn from(q: Question) -> Self {
        PublicQuestion {
            id: q.id,
            question_key: q.question_key,
            question_text: q.question_text,
            question_type: q.question_type,
            sort_order: q.sort_order,
            score_weight: q.score_weight,
            options: q.options,
        }
    }
}

/// Input for creating a question.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateQuestionInput {
    pub question_key: String,
    pub question_text: String,
    pub question_type: String,
    pub sort_order: i32,
    pub correct_answer: Option<String>,
    pub score_weight: Option<i32>,
    pub options: Option<serde_json::Value>,
    pub crm_field: Option<String>,
    pub crm_field_type: Option<String>,
}

/// Input for updating a question.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateQuestionInput {
    pub question_text: Option<String>,
    pub question_type: Option<String>,
    pub sort_order: Option<i32>,
    pub correct_answer: Option<String>,
    pub score_weight: Option<i32>,
    pub options: Option<serde_json::Value>,
    pub crm_field: Option<String>,
    pub crm_field_type: Option<String>,
}

/// Create a question for a campaign.
pub async fn create_question(
    pool: &PgPool,
    campaign_id: &Uuid,
    input: &CreateQuestionInput,
) -> Result<Uuid, AppError> {
    let id = Uuid::new_v4();
    let score_weight = input.score_weight.unwrap_or(1);
    sqlx::query(
        r#"INSERT INTO questions (id, campaign_id, question_key, question_text, question_type,
            sort_order, correct_answer, score_weight, options, crm_field, crm_field_type)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)"#
    )
    .bind(id)
    .bind(campaign_id)
    .bind(&input.question_key)
    .bind(&input.question_text)
    .bind(&input.question_type)
    .bind(input.sort_order)
    .bind(&input.correct_answer)
    .bind(score_weight)
    .bind(&input.options)
    .bind(&input.crm_field)
    .bind(&input.crm_field_type)
    .execute(pool)
    .await?;

    Ok(id)
}

/// Update a question.
pub async fn update_question(
    pool: &PgPool,
    question_id: &Uuid,
    input: &UpdateQuestionInput,
) -> Result<(), AppError> {
    let mut sets = Vec::new();
    let mut bind_idx = 1usize;

    if let Some(ref v) = input.question_text {
        sets.push(format!("question_text = ${}", bind_idx));
        bind_idx += 1;
    }
    if let Some(ref v) = input.question_type {
        sets.push(format!("question_type = ${}", bind_idx));
        bind_idx += 1;
    }
    if let Some(v) = input.sort_order {
        sets.push(format!("sort_order = ${}", bind_idx));
        bind_idx += 1;
    }
    if let Some(ref v) = input.correct_answer {
        sets.push(format!("correct_answer = ${}", bind_idx));
        bind_idx += 1;
    }
    if let Some(v) = input.score_weight {
        sets.push(format!("score_weight = ${}", bind_idx));
        bind_idx += 1;
    }
    if let Some(ref v) = input.options {
        sets.push(format!("options = ${}", bind_idx));
        bind_idx += 1;
    }
    if let Some(ref v) = input.crm_field {
        sets.push(format!("crm_field = ${}", bind_idx));
        bind_idx += 1;
    }
    if let Some(ref v) = input.crm_field_type {
        sets.push(format!("crm_field_type = ${}", bind_idx));
        bind_idx += 1;
    }

    if sets.is_empty() {
        return Ok(());
    }

    let q = format!(
        "UPDATE questions SET {} WHERE id = ${}",
        sets.join(", "),
        bind_idx
    );

    let mut query = sqlx::query(&q);
    if let Some(ref v) = input.question_text {
        query = query.bind(v);
    }
    if let Some(ref v) = input.question_type {
        query = query.bind(v);
    }
    if let Some(v) = input.sort_order {
        query = query.bind(v);
    }
    if let Some(ref v) = input.correct_answer {
        query = query.bind(v);
    }
    if let Some(v) = input.score_weight {
        query = query.bind(v);
    }
    if let Some(ref v) = input.options {
        query = query.bind(v);
    }
    if let Some(ref v) = input.crm_field {
        query = query.bind(v);
    }
    if let Some(ref v) = input.crm_field_type {
        query = query.bind(v);
    }
    query = query.bind(question_id);

    query.execute(pool).await?;
    Ok(())
}

/// Get all questions for a campaign (admin view — includes correct_answer).
pub async fn get_campaign_questions(
    pool: &PgPool,
    campaign_id: &Uuid,
) -> Result<Vec<Question>, AppError> {
    let rows = sqlx::query_as::<_, Question>(
        r#"SELECT id, campaign_id, question_key, question_text, question_type,
                  sort_order, correct_answer, score_weight, options, crm_field, crm_field_type
           FROM questions
           WHERE campaign_id = $1
           ORDER BY sort_order"#,
    )
    .bind(campaign_id)
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

/// Get public questions for a campaign (play view — no correct_answer).
pub async fn get_campaign_questions_public(
    pool: &PgPool,
    campaign_id: &Uuid,
) -> Result<Vec<PublicQuestion>, AppError> {
    let rows = sqlx::query_as::<_, Question>(
        r#"SELECT id, campaign_id, question_key, question_text, question_type,
                  sort_order, correct_answer, score_weight, options, crm_field, crm_field_type
           FROM questions
           WHERE campaign_id = $1
           ORDER BY sort_order"#,
    )
    .bind(campaign_id)
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().map(PublicQuestion::from).collect())
}

/// Delete a question.
pub async fn delete_question(
    pool: &PgPool,
    question_id: &Uuid,
) -> Result<(), AppError> {
    sqlx::query("DELETE FROM questions WHERE id = $1")
        .bind(question_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Score a quiz submission by comparing answers against correct_answer.
/// Returns (score_earned, max_score, percentage).
pub async fn score_quiz_submission(
    pool: &PgPool,
    campaign_id: &Uuid,
    answers: &[AnswerInput],
) -> Result<(i32, i32, f64), AppError> {
    let questions = get_campaign_questions(pool, campaign_id).await?;

    let mut score_earned = 0i32;
    let mut max_score = 0i32;

    for answer in answers {
        if let Some(q) = questions.iter().find(|q| q.id == answer.question_id) {
            max_score += q.score_weight;
            if let Some(ref correct) = q.correct_answer {
                if answer.value.trim().to_lowercase() == correct.trim().to_lowercase() {
                    score_earned += q.score_weight;
                }
            }
        }
    }

    let percentage = if max_score > 0 {
        (score_earned as f64 / max_score as f64) * 100.0
    } else {
        0.0
    };

    Ok((score_earned, max_score, percentage))
}

/// Generate a persona/tier based on quiz score percentage and outcome_tags config.
pub fn determine_persona(percentage: f64, outcome_tags: &serde_json::Value) -> (String, String) {
    // outcome_tags expected format: [{"label": "Beginner", "min_score": 0, "tag": "beginner"}, ...]
    if let Some(tags) = outcome_tags.as_array() {
        let mut best = ("General".to_string(), "".to_string());
        for tag in tags {
            let min = tag.get("min_score").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let max = tag.get("max_score").and_then(|v| v.as_f64()).unwrap_or(100.0);
            if percentage >= min && percentage <= max {
                let label = tag.get("label").and_then(|v| v.as_str()).unwrap_or("General");
                let tag_str = tag.get("tag").and_then(|v| v.as_str()).unwrap_or("");
                best = (label.to_string(), tag_str.to_string());
                break;
            }
        }
        best
    } else if percentage >= 80.0 {
        ("Expert".to_string(), "expert".to_string())
    } else if percentage >= 60.0 {
        ("Advanced".to_string(), "advanced".to_string())
    } else if percentage >= 40.0 {
        ("Intermediate".to_string(), "intermediate".to_string())
    } else {
        ("Beginner".to_string(), "beginner".to_string())
    }
}

/// Input for an answer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnswerInput {
    pub question_id: Uuid,
    pub value: String,
    pub raw_value: Option<serde_json::Value>,
}

/// Create a single answer for an entry.
pub async fn create_answer(
    pool: &PgPool,
    entry_id: &Uuid,
    question_id: &Uuid,
    value: &str,
    raw_value: Option<&serde_json::Value>,
) -> Result<Uuid, AppError> {
    let id = Uuid::new_v4();
    sqlx::query(
        r#"INSERT INTO answers (id, entry_id, question_id, value, raw_value)
           VALUES ($1, $2, $3, $4, $5)"#
    )
    .bind(id)
    .bind(entry_id)
    .bind(question_id)
    .bind(value)
    .bind(raw_value)
    .execute(pool)
    .await?;

    Ok(id)
}

/// A question-answer pair from normalized DB joins.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct QuestionAnswerPair {
    pub question_text: String,
    pub question_key: String,
    pub value: String,
    pub raw_value: Option<serde_json::Value>,
}

/// Get all questions and answers for an entry using normalized joins.
/// The question text ALWAYS comes from the questions table.
pub async fn get_questions_with_answers(
    pool: &PgPool,
    entry_id: &Uuid,
) -> Result<Vec<QuestionAnswerPair>, AppError> {
    let rows = sqlx::query_as::<_, QuestionAnswerPair>(
        r#"SELECT q.question_text, q.question_key,
                  a.value, a.raw_value
           FROM answers a
           JOIN questions q ON q.id = a.question_id
           WHERE a.entry_id = $1
           ORDER BY q.sort_order"#,
    )
    .bind(entry_id)
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

/// Batch insert answers for an entry.
pub async fn batch_insert_answers(
    pool: &PgPool,
    entry_id: &Uuid,
    answers: &[AnswerInput],
) -> Result<(), AppError> {
    for answer in answers {
        create_answer(
            pool,
            entry_id,
            &answer.question_id,
            &answer.value,
            answer.raw_value.as_ref(),
        )
        .await?;
    }

    Ok(())
}
