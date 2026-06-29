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
}
