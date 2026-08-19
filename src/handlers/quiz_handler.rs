//! Quiz/Trivia handler — question CRUD, quiz submission, scoring, CRM field mapping.

use crate::db::{campaigns, questions_answers};
use crate::error::AppError;
use crate::state::AppState;
use axum::{
    extract::{Path, State},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

/// GET /api/v1/campaigns/{slug}/questions — admin view (includes correct_answer)
pub async fn list_campaign_questions(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> Result<Json<Value>, AppError> {
    let campaign = campaigns::get_campaign_by_slug(&state.db, &slug).await?;
    let questions = questions_answers::get_campaign_questions(&state.db, &campaign.id).await?;
    Ok(Json(
        json!({ "questions": questions, "campaign_id": campaign.id }),
    ))
}

/// GET /api/v1/play/{campaign_id}/questions — public view (no correct_answer)
pub async fn play_campaign_questions(
    State(state): State<AppState>,
    Path(campaign_id): Path<Uuid>,
) -> Result<Json<Value>, AppError> {
    // Play-time gate: loading the quiz questions is the first step of play, so gate
    // on the campaign owner's tier (402 before questions are served when subtracted).
    let campaign = campaigns::get_campaign_by_id(&state.db, &campaign_id).await?;
    crate::access::feature_gate::enforce_mechanic_feature(
        &state,
        &campaign.account_id.to_string(),
        &campaign.r#type,
    )
    .await?;

    let questions =
        questions_answers::get_campaign_questions_public(&state.db, &campaign_id).await?;
    Ok(Json(json!({ "questions": questions })))
}

/// POST /api/v1/campaigns/{slug}/questions — create a question
pub async fn create_question(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    Json(input): Json<questions_answers::CreateQuestionInput>,
) -> Result<Json<Value>, AppError> {
    let campaign = campaigns::get_campaign_by_slug(&state.db, &slug).await?;
    let id = questions_answers::create_question(&state.db, &campaign.id, &input).await?;
    Ok(Json(
        json!({ "id": id, "question_key": input.question_key }),
    ))
}

/// PUT /api/v1/campaigns/{slug}/questions/{question_id} — update a question
pub async fn update_question(
    State(state): State<AppState>,
    Path((slug, question_id)): Path<(String, Uuid)>,
    Json(input): Json<questions_answers::UpdateQuestionInput>,
) -> Result<Json<Value>, AppError> {
    let _campaign = campaigns::get_campaign_by_slug(&state.db, &slug).await?;
    questions_answers::update_question(&state.db, &question_id, &input).await?;
    Ok(Json(json!({ "status": "updated" })))
}

/// DELETE /api/v1/campaigns/{slug}/questions/{question_id}
pub async fn delete_question(
    State(state): State<AppState>,
    Path((slug, question_id)): Path<(String, Uuid)>,
) -> Result<Json<Value>, AppError> {
    let _campaign = campaigns::get_campaign_by_slug(&state.db, &slug).await?;
    questions_answers::delete_question(&state.db, &question_id).await?;
    Ok(Json(json!({ "status": "deleted" })))
}

/// Input for quiz submission
#[derive(Debug, Deserialize)]
pub struct QuizSubmitInput {
    pub contact: QuizContact,
    pub answers: Vec<QuizAnswer>,
    pub source: Option<QuizSource>,
}

#[derive(Debug, Deserialize)]
pub struct QuizContact {
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    pub email: String,
    pub phone: Option<String>,
    pub company: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct QuizAnswer {
    pub question_id: Uuid,
    pub value: String,
}

#[derive(Debug, Deserialize)]
pub struct QuizSource {
    pub utm_source: Option<String>,
    pub utm_medium: Option<String>,
    pub utm_campaign: Option<String>,
    pub referrer_url: Option<String>,
    pub page_url: Option<String>,
}

/// Response from quiz submission
#[derive(Debug, Serialize)]
pub struct QuizResult {
    pub score: i32,
    pub max_score: i32,
    pub percentage: f64,
    pub passed: bool,
    pub persona: String,
    pub persona_tag: String,
    pub entry_id: Uuid,
    pub crm_fields: Value,
}

/// POST /api/v1/quiz/{campaign_id}/submit — submit quiz answers, score, create entry
pub async fn submit_quiz(
    State(state): State<AppState>,
    Path(campaign_id): Path<Uuid>,
    Json(input): Json<QuizSubmitInput>,
) -> Result<Json<Value>, AppError> {
    // Get campaign
    let campaign = campaigns::get_campaign_by_id(&state.db, &campaign_id).await?;

    // Play-time gate: enforce the campaign owner's plan tier includes this mechanic
    // (402 before any quiz is scored/recorded for free/subtracted tiers).
    crate::access::feature_gate::enforce_mechanic_feature(
        &state,
        &campaign.account_id.to_string(),
        &campaign.r#type,
    )
    .await?;

    let passing_score = campaign
        .config
        .get("passing_score")
        .and_then(|v| v.as_f64())
        .unwrap_or(70.0);

    // Score the answers
    let answer_inputs: Vec<questions_answers::AnswerInput> = input
        .answers
        .iter()
        .map(|a| questions_answers::AnswerInput {
            question_id: a.question_id,
            value: a.value.clone(),
            raw_value: None,
        })
        .collect();

    let (score, max_score, percentage) =
        questions_answers::score_quiz_submission(&state.db, &campaign.id, &answer_inputs).await?;

    let passed = percentage >= passing_score;

    // Determine persona from outcome_tags
    let (persona, persona_tag) =
        questions_answers::determine_persona(percentage, &campaign.outcome_tags);

    // Build CRM fields from question mappings
    let questions = questions_answers::get_campaign_questions(&state.db, &campaign.id).await?;
    let mut crm_fields = json!({
        "quiz_score": score,
        "quiz_max_score": max_score,
        "quiz_percentage": percentage,
        "persona": persona,
        "persona_tag": persona_tag,
        "passed": passed,
    });

    // Map answer values to CRM fields where configured
    for answer in &input.answers {
        if let Some(q) = questions.iter().find(|q| q.id == answer.question_id) {
            if let Some(ref crm_field) = q.crm_field {
                if let Some(ref crm_type) = q.crm_field_type {
                    let key = format!("{}_{}", crm_type, crm_field);
                    crm_fields[key] = json!(answer.value);
                }
            }
        }
    }

    // Add source/UTM
    if let Some(ref source) = input.source {
        if let Some(ref v) = source.utm_source {
            crm_fields["utm_source"] = json!(v);
        }
        if let Some(ref v) = source.utm_medium {
            crm_fields["utm_medium"] = json!(v);
        }
        if let Some(ref v) = source.utm_campaign {
            crm_fields["utm_campaign"] = json!(v);
        }
        if let Some(ref v) = source.referrer_url {
            crm_fields["referrer_url"] = json!(v);
        }
    }

    // Create/upsert contact — email is required, handle conflict with ON CONFLICT
    let contact_id = sqlx::query_scalar::<_, Uuid>(
        r#"INSERT INTO contacts (id, first_name, last_name, email, phone, business_name)
           VALUES (gen_random_uuid(), $1, $2, $3, $4, $5)
           ON CONFLICT (email) WHERE email IS NOT NULL AND email <> ''
           DO UPDATE SET
               first_name = COALESCE(NULLIF($1, ''), contacts.first_name),
               last_name = COALESCE(NULLIF($2, ''), contacts.last_name),
               phone = COALESCE(NULLIF($4, ''), contacts.phone)
           RETURNING id"#,
    )
    .bind(&input.contact.first_name)
    .bind(&input.contact.last_name)
    .bind(&input.contact.email)
    .bind(&input.contact.phone)
    .bind(&input.contact.company)
    .fetch_one(&state.db)
    .await
    .map_err(|e| AppError::Database(format!("Contact upsert failed: {}", e)))?;

    // Create entry (entries table has no account_id column, only contact_id + campaign_id)
    let entry_id = Uuid::new_v4();
    sqlx::query(
        r#"INSERT INTO entries (id, campaign_id, contact_id, answers, score, outcome,
            utm_source, utm_medium, utm_campaign, referrer_url, page_url)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)"#,
    )
    .bind(entry_id)
    .bind(campaign.id)
    .bind(contact_id)
    .bind(json!({
        "persona": persona,
        "persona_tag": persona_tag,
        "crm_fields": crm_fields
    }))
    .bind(score)
    .bind(if passed { "won" } else { "lost" })
    .bind(input.source.as_ref().and_then(|s| s.utm_source.as_ref()))
    .bind(input.source.as_ref().and_then(|s| s.utm_medium.as_ref()))
    .bind(input.source.as_ref().and_then(|s| s.utm_campaign.as_ref()))
    .bind(input.source.as_ref().and_then(|s| s.referrer_url.as_ref()))
    .bind(input.source.as_ref().and_then(|s| s.page_url.as_ref()))
    .execute(&state.db)
    .await?;

    // Store individual answers
    for answer in &input.answers {
        questions_answers::create_answer(
            &state.db,
            &entry_id,
            &answer.question_id,
            &answer.value,
            None,
        )
        .await?;
    }

    // Fire delivery integration with clean CRM payload (only crm_fields, not raw answers)
    use crate::delivery::integration_hub::{
        self, CampaignInfo, ContactInfo, DeliveryConfig, DeliveryContext, OutcomePayload,
    };
    let _ = integration_hub::execute_delivery(
        &state.db,
        &DeliveryContext {
            campaign: CampaignInfo {
                id: campaign.id,
                name: campaign.name.clone(),
                slug: campaign.slug.clone(),
                account_id: campaign.account_id,
            },
            contact: ContactInfo {
                id: contact_id,
                email: Some(input.contact.email.clone()),
                phone: input.contact.phone.clone(),
                first_name: input.contact.first_name.clone(),
                last_name: input.contact.last_name.clone(),
            },
            outcome: OutcomePayload {
                prize_id: None,
                prize_label: if passed { Some(persona.clone()) } else { None },
                prize_type: Some("quiz".to_string()),
                won: passed,
                was_pity: false,
                streak: 0,
                total_spins: 0,
                redemption_url: None,
            },
            delivery_config: DeliveryConfig::default(),
            crm_fields: Some(crm_fields.clone()),
        },
    )
    .await;

    let result = QuizResult {
        score,
        max_score,
        percentage: (percentage * 100.0).round() / 100.0,
        passed,
        persona,
        persona_tag,
        entry_id,
        crm_fields,
    };

    Ok(Json(json!(result)))
}
