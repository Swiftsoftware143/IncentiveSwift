//! Delivery payload struct — exact contract for FunnelSwift/webhook pushes.
//! This shape MUST NOT change without updating FunnelSwift's ingest endpoint.

use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct DeliveryPayload {
    pub event: &'static str, // always "entry.captured"
    pub contact: ContactPayload,
    pub campaign: CampaignPayload,
    pub outcome: String,
    pub tags_applied: Vec<String>,
    pub score: Option<i32>,
    pub questions_and_answers: Vec<QuestionAnswerPair>,
    pub entry_id: String,
    pub captured_at: String, // ISO 8601
}

#[derive(Debug, Serialize)]
pub struct ContactPayload {
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub business_name: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CampaignPayload {
    pub name: String,
    #[serde(rename = "type")]
    pub campaign_type: String,
    pub tag_namespace: String,
}

#[derive(Debug, Serialize)]
pub struct QuestionAnswerPair {
    pub question: String,
    pub answer: String,
}

impl DeliveryPayload {
    /// Build payload from normalized answers+questions join.
    /// NEVER reconstruct from raw entries.answers JSONB — use the join.
    pub fn build(
        contact: ContactPayload,
        campaign: CampaignPayload,
        outcome: String,
        tags_applied: Vec<String>,
        score: Option<i32>,
        questions_and_answers: Vec<QuestionAnswerPair>,
        entry_id: String,
    ) -> Self {
        Self {
            event: "entry.captured",
            contact,
            campaign,
            outcome,
            tags_applied,
            score,
            questions_and_answers,
            entry_id,
            captured_at: chrono::Utc::now().to_rfc3339(),
        }
    }

    /// Build payload from entry_id by doing normalized DB joins.
    /// The question text ALWAYS comes from the questions table, never from raw JSONB.
    pub async fn build_from_entry(
        pool: &sqlx::PgPool,
        entry_id: &uuid::Uuid,
    ) -> Result<Self, crate::error::AppError> {
        use sqlx::Row;

        // Get entry with contact and campaign info
        let row = sqlx::query(
            r#"SELECT e.id, e.score, e.outcome, e.tags_applied,
                      c.first_name, c.last_name, c.email, c.phone, c.business_name,
                      cam.name, cam.type, cam.tag_namespace
               FROM entries e
               JOIN contacts c ON c.id = e.contact_id
               JOIN campaigns cam ON cam.id = e.campaign_id
               WHERE e.id = $1"#
        )
        .bind(entry_id)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| crate::error::AppError::NotFound("Entry not found".to_string()))?;

        let score: Option<i32> = row.get("score");
        let outcome: Option<String> = row.get("outcome");
        let tags_applied: Option<Vec<String>> = row.get("tags_applied");

        // Get Q&A from normalized join (questions table)
        let qa_rows = crate::db::questions_answers::get_questions_with_answers(pool, entry_id).await?;
        let qa_pairs: Vec<QuestionAnswerPair> = qa_rows.iter().map(|qa| {
            QuestionAnswerPair {
                question: qa.question_text.clone(),
                answer: qa.value.clone(),
            }
        }).collect();

        Ok(Self::build(
            ContactPayload {
                first_name: row.get("first_name"),
                last_name: row.get("last_name"),
                email: row.get("email"),
                phone: row.get("phone"),
                business_name: row.get("business_name"),
            },
            CampaignPayload {
                name: row.get("name"),
                campaign_type: row.get("type"),
                tag_namespace: row.get("tag_namespace"),
            },
            outcome.unwrap_or_else(|| "entrant".to_string()),
            tags_applied.unwrap_or_default(),
            score,
            qa_pairs,
            entry_id.to_string(),
        ))
    }
}
