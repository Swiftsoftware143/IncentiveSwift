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
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)"#,
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

/// Update a question that BELONGS to `campaign_id`.
///
/// SECURITY (kanban t_27e3e083): `campaign_id` is part of the statement, not a caller courtesy.
/// The handler had already resolved the campaign under the caller's account, but the UPDATE bound
/// only `id`, so a tenant who owned campaign X could PUT `/campaigns/X/questions/{id}` with a
/// question id belonging to ANOTHER tenant's campaign and rewrite that row (measured live on
/// 7870f1c3: HTTP 200, foreign `question_text` changed). Returns `false` when no row matched —
/// foreign id, absent id, or a question in the caller's own OTHER campaign — and the handler
/// answers 404. The ownership probe runs even for a body that sets nothing, so an empty PUT
/// cannot read as a successful write on a foreign id.
pub async fn update_question(
    pool: &PgPool,
    campaign_id: &Uuid,
    question_id: &Uuid,
    input: &UpdateQuestionInput,
) -> Result<bool, AppError> {
    // One complete compile-time statement (gate rule 5d / class 14, kanban t_563a3f10): the old
    // builder pushed `format!("col = ${n}")` fragments and joined them at run time. Every column now
    // sits at a FIXED slot wrapped in COALESCE($n, col), so a NULL bind leaves the column alone —
    // the same outcome the builder had, with the statement visible at the call site.
    let owned: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM questions WHERE id = $1 AND campaign_id = $2)",
    )
    .bind(question_id)
    .bind(campaign_id)
    .fetch_one(pool)
    .await?;
    if !owned {
        return Ok(false);
    }

    if input.question_text.is_none()
        && input.question_type.is_none()
        && input.sort_order.is_none()
        && input.correct_answer.is_none()
        && input.score_weight.is_none()
        && input.options.is_none()
        && input.crm_field.is_none()
        && input.crm_field_type.is_none()
    {
        return Ok(true);
    }

    sqlx::query(
        "UPDATE questions SET
            question_text = COALESCE($1, question_text),
            question_type = COALESCE($2, question_type),
            sort_order = COALESCE($3, sort_order),
            correct_answer = COALESCE($4, correct_answer),
            score_weight = COALESCE($5, score_weight),
            options = COALESCE($6, options),
            crm_field = COALESCE($7, crm_field),
            crm_field_type = COALESCE($8, crm_field_type)
         WHERE id = $9 AND campaign_id = $10",
    )
    .bind(&input.question_text)
    .bind(&input.question_type)
    .bind(input.sort_order)
    .bind(&input.correct_answer)
    .bind(input.score_weight)
    .bind(&input.options)
    .bind(&input.crm_field)
    .bind(&input.crm_field_type)
    .bind(question_id)
    .bind(campaign_id)
    .execute(pool)
    .await?;
    Ok(true)
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

/// Delete a question that BELONGS to `campaign_id`.
///
/// SECURITY (kanban t_27e3e083): the predicate is the whole fix — the old statement bound `id`
/// alone, so a tenant owning campaign X could DELETE `/campaigns/X/questions/{id}` and remove a
/// question from ANOTHER tenant's campaign (measured live on 7870f1c3: HTTP 200, foreign row gone).
/// `false` = nothing matched (foreign id, absent id) and the handler answers 404.
pub async fn delete_question(
    pool: &PgPool,
    campaign_id: &Uuid,
    question_id: &Uuid,
) -> Result<bool, AppError> {
    let result = sqlx::query("DELETE FROM questions WHERE id = $1 AND campaign_id = $2")
        .bind(question_id)
        .bind(campaign_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() == 1)
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
            let max = tag
                .get("max_score")
                .and_then(|v| v.as_f64())
                .unwrap_or(100.0);
            if percentage >= min && percentage <= max {
                let label = tag
                    .get("label")
                    .and_then(|v| v.as_str())
                    .unwrap_or("General");
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
           VALUES ($1, $2, $3, $4, $5)"#,
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
