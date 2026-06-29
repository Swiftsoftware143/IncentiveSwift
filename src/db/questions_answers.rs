//! Questions and answers database operations.
//! Question text ALWAYS comes from the questions table, never from raw JSONB.

use crate::error::AppError;
use sqlx::PgPool;
use uuid::Uuid;

/// Create a question for a campaign.
pub async fn create_question(
    pool: &PgPool,
    campaign_id: &Uuid,
    question_key: &str,
    question_text: &str,
    question_type: &str,
    sort_order: i32,
) -> Result<Uuid, AppError> {
    let id = Uuid::new_v4();
    sqlx::query(
        r#"INSERT INTO questions (id, campaign_id, question_key, question_text, question_type, sort_order)
           VALUES ($1, $2, $3, $4, $5, $6)"#
    )
    .bind(id)
    .bind(campaign_id)
    .bind(question_key)
    .bind(question_text)
    .bind(question_type)
    .bind(sort_order)
    .execute(pool)
    .await?;

    Ok(id)
}

/// Input for an answer.
#[derive(Debug, Clone)]
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
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
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
