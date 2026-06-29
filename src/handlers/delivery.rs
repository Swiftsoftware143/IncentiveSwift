//! Delivery handlers — resend a delivery by entry ID.

use crate::error::AppError;
use crate::state::AppState;
use crate::security::auth::AuthenticatedUser;
use crate::db::questions_answers;
use crate::delivery::{payload::DeliveryPayload, payload::ContactPayload, payload::CampaignPayload, payload::QuestionAnswerPair, webhook};
use axum::{extract::State, Json};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

/// Body for resending delivery.
#[derive(Deserialize)]
pub struct ResendBody {
    pub entry_id: String,
}

/// POST /api/v1/delivery/resend — authenticated.
/// Rebuilds payload from normalized Q&A join (NEVER from raw JSONB), repushes.
pub async fn resend(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Json(body): Json<ResendBody>,
) -> Result<Json<Value>, AppError> {
    let entry_id = Uuid::parse_str(&body.entry_id)
        .map_err(|_| AppError::BadRequest("Invalid entry ID".to_string()))?;

    // Get the entry
    let row = sqlx::query(
        r#"SELECT e.id, e.contact_id, e.campaign_id, e.score, e.outcome,
                  e.tags_applied, e.created_at,
                  c.first_name, c.last_name, c.email, c.phone, c.business_name,
                  cam.name, cam.type, cam.tag_namespace, cam.delivery_method,
                  cam.delivery_config
           FROM entries e
           JOIN contacts c ON c.id = e.contact_id
           JOIN campaigns cam ON cam.id = e.campaign_id
           WHERE e.id = $1"#
    )
    .bind(entry_id)
    .fetch_optional(&state.db)
    .await?;

    let row = match row {
        Some(r) => r,
        None => return Err(AppError::NotFound("Entry not found".to_string())),
    };

    use sqlx::Row;
    let _contact_id: Uuid = row.get("contact_id");
    let _campaign_id: Uuid = row.get("campaign_id");
    let score: Option<i32> = row.get("score");
    let outcome: Option<String> = row.get("outcome");
    let tags_applied: Option<Vec<String>> = row.get("tags_applied");
    let first_name: Option<String> = row.get("first_name");
    let last_name: Option<String> = row.get("last_name");
    let email: Option<String> = row.get("email");
    let phone: Option<String> = row.get("phone");
    let business_name: Option<String> = row.get("business_name");
    let campaign_name: String = row.get("name");
    let campaign_type: String = row.get("type");
    let tag_namespace: String = row.get("tag_namespace");
    let delivery_method: String = row.get("delivery_method");
    let delivery_config: serde_json::Value = row.get("delivery_config");

    // CRITICAL: Get Q&A from normalized join (questions table), not from raw JSONB
    let normalized_qa = questions_answers::get_questions_with_answers(&state.db, &entry_id).await?;
    let qa_pairs: Vec<QuestionAnswerPair> = normalized_qa.iter().map(|qa| {
        QuestionAnswerPair {
            question: qa.question_text.clone(),
            answer: qa.value.clone(),
        }
    }).collect();

    // Build payload
    let payload = DeliveryPayload::build(
        ContactPayload {
            first_name,
            last_name,
            email,
            phone,
            business_name,
        },
        CampaignPayload {
            name: campaign_name,
            campaign_type,
            tag_namespace,
        },
        outcome.unwrap_or_else(|| "entrant".to_string()),
        tags_applied.unwrap_or_default(),
        score,
        qa_pairs,
        entry_id.to_string(),
    );

    // Trigger delivery based on campaign delivery method
    match delivery_method.as_str() {
        "direct_api" => {
            let api_type = delivery_config.get("api_type")
                .and_then(|v| v.as_str())
                .unwrap_or("webhook");
            let api_key = delivery_config.get("api_key")
                .and_then(|v| v.as_str())
                .unwrap_or("");

            match api_type {
                "hubspot" => {
                    crate::delivery::direct_api::hubspot::push_to_hubspot(&state.http_client, api_key, &payload).await?;
                }
                "activecampaign" => {
                    crate::delivery::direct_api::activecampaign::push_to_activecampaign(&state.http_client, api_key, &payload).await?;
                }
                "gohighlevel" => {
                    crate::delivery::direct_api::gohighlevel::push_to_gohighlevel(&state.http_client, api_key, &payload).await?;
                }
                _ => {
                    let url = delivery_config.get("webhook_url")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if !url.is_empty() {
                        webhook::push_to_webhook(&state.http_client, url, &payload, &state.db, &entry_id).await?;
                    }
                }
            }
        }
        _ => {
            let url = delivery_config.get("webhook_url")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if !url.is_empty() {
                webhook::push_to_webhook(&state.http_client, url, &payload, &state.db, &entry_id).await?;
            }
        }
    }

    Ok(Json(json!({
        "status": "resent",
        "entry_id": entry_id.to_string(),
    })))
}
