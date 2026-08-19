//! Chat Funnel handler — conversational bubble quiz with progressive flow.
//!
//! Questions are served step-by-step from `config.chat_flow`. A pluggable LLM
//! (DeepSeek default, OpenAI fallback) is resolved from the account's provider
//! keys in the Integration Center — per-user key, no server-wide env key.
//! When no provider key is configured the flow falls back to scripted replies.

use crate::db::campaigns;
use crate::db::entries;
use crate::error::AppError;
use crate::handlers::mechanic_common::{
    extract_source, gate_mechanic, resolve_contact, MechanicContact,
};
use crate::state::AppState;
use axum::{
    extract::{Path, State},
    http::HeaderMap,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

/// Request body for a chat turn.
#[derive(Debug, Deserialize)]
pub struct ChatBody {
    pub contact: MechanicContact,
    /// The user's answer/message for the current step (empty on first turn).
    #[serde(default)]
    pub message: Option<String>,
    /// Zero-based index of the question being answered. Omit for the greeting.
    #[serde(default)]
    pub step: Option<usize>,
    pub utm_source: Option<String>,
    pub utm_medium: Option<String>,
    pub utm_campaign: Option<String>,
    pub referrer_url: Option<String>,
    pub page_url: Option<String>,
}

/// Extract the scripted bot-message flow from campaign config.
/// `config.chat_flow` = ["hello", "what's your goal?", "thanks!"]
/// Falls back to `config.questions` as an array of strings.
fn chat_flow(config: &Value) -> Vec<String> {
    if let Some(flow) = config.get("chat_flow").and_then(|f| f.as_array()) {
        let items: Vec<String> = flow
            .iter()
            .filter_map(|s| s.as_str().map(|x| x.to_string()))
            .collect();
        if !items.is_empty() {
            return items;
        }
    }
    if let Some(questions) = config.get("questions").and_then(|q| q.as_array()) {
        let items: Vec<String> = questions
            .iter()
            .filter_map(|s| s.as_str().map(|x| x.to_string()))
            .collect();
        if !items.is_empty() {
            return items;
        }
    }
    vec!["Welcome! How can we help you today?".to_string()]
}

/// Resolve an active DeepSeek (preferred) or OpenAI provider key for the account.
async fn resolve_llm_key(state: &AppState, account_id: &Uuid) -> Option<(String, String, String)> {
    for provider in ["deepseek", "openai"] {
        let row = sqlx::query(
            "SELECT api_key, base_url FROM provider_keys WHERE account_id = $1 AND provider = $2 AND is_active = true",
        )
        .bind(account_id)
        .bind(provider)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();

        if let Some(r) = row {
            let key: String = r.get("api_key");
            let base: Option<String> = r.get("base_url");
            if !key.is_empty() {
                return Some((provider.to_string(), key, base.unwrap_or_default()));
            }
        }
    }
    None
}

/// Best-effort LLM completion. Returns None (and logs) on any failure so the
/// conversation can always fall back to scripted replies.
async fn maybe_llm_reply(
    state: &AppState,
    account_id: &Uuid,
    system_prompt: &str,
    user_message: &str,
) -> Option<String> {
    let (provider, api_key, base_url) = resolve_llm_key(state, account_id).await?;

    let (url, model) = match provider.as_str() {
        "deepseek" => (
            if base_url.is_empty() {
                "https://api.deepseek.com/chat/completions".to_string()
            } else {
                format!("{}/chat/completions", base_url.trim_end_matches('/'))
            },
            "deepseek-chat".to_string(),
        ),
        _ => (
            if base_url.is_empty() {
                "https://api.openai.com/v1/chat/completions".to_string()
            } else {
                format!("{}/chat/completions", base_url.trim_end_matches('/'))
            },
            "gpt-4o-mini".to_string(),
        ),
    };

    let payload = json!({
        "model": model,
        "messages": [
            { "role": "system", "content": system_prompt },
            { "role": "user", "content": user_message }
        ],
        "max_tokens": 200,
        "temperature": 0.7
    });

    let resp = state
        .http_client
        .post(&url)
        .bearer_auth(&api_key)
        .json(&payload)
        .send()
        .await
        .ok()?;

    if !resp.status().is_success() {
        tracing::warn!("Chat LLM ({}) returned {}", provider, resp.status());
        return None;
    }

    let body: Value = resp.json().await.ok()?;
    let text = body
        .pointer("/choices/0/message/content")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    text.filter(|s| !s.trim().is_empty())
}

/// POST /api/v1/campaigns/:slug/chat
pub async fn chat(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    headers: HeaderMap,
    Json(body): Json<ChatBody>,
) -> Result<Json<Value>, AppError> {
    let campaign = campaigns::get_campaign_by_slug(&state.db, &slug).await?;
    if campaign.status != "active" {
        return Err(AppError::Forbidden("Campaign is not active".to_string()));
    }
    if campaign.r#type != "chat" {
        return Err(AppError::BadRequest(format!(
            "Campaign '{}' is type '{}', not 'chat'",
            campaign.slug, campaign.r#type
        )));
    }

    gate_mechanic(&state, &campaign.account_id, "chat").await?;

    let contact_id = resolve_contact(&state, &body.contact).await?;
    let flow = chat_flow(&campaign.config);

    let step = body.step.unwrap_or(0);
    let user_message = body.message.clone().unwrap_or_default();

    // The scripted prompt for this step (empty after the last step).
    let current_prompt = flow.get(step).cloned().unwrap_or_default();
    let completed = step >= flow.len();

    // LLM enrichment (per-user provider key). Fall back to scripted reply.
    let system_prompt = campaign
        .config
        .get("system_prompt")
        .and_then(|v| v.as_str())
        .unwrap_or("You are a friendly sales assistant running a conversational funnel. Keep replies short and helpful.");
    let llm_reply = if user_message.is_empty() {
        None
    } else {
        maybe_llm_reply(&state, &campaign.account_id, system_prompt, &user_message).await
    };
    let reply = llm_reply.unwrap_or_else(|| {
        if completed {
            "Thanks for chatting! We'll follow up shortly.".to_string()
        } else {
            current_prompt.clone()
        }
    });

    let (user_agent, ip_address) = extract_source(&headers);
    let entry_id = entries::create_entry(
        &state.db,
        &entries::CreateEntryInput {
            contact_id,
            campaign_id: campaign.id,
            answers: json!({
                "kind": "chat_step",
                "step": step,
                "message": user_message,
                "reply": reply,
            }),
            score: None,
            outcome: Some(
                if completed {
                    "completed"
                } else {
                    "in_progress"
                }
                .to_string(),
            ),
            tags_applied: Some(vec![]),
            utm_source: body.utm_source.clone(),
            utm_medium: body.utm_medium.clone(),
            utm_campaign: body.utm_campaign.clone(),
            referrer_url: body.referrer_url.clone(),
            page_url: body.page_url.clone(),
            user_agent,
            ip_address,
        },
    )
    .await?;

    let next_step = step + 1;
    let next_question = flow.get(next_step).cloned();

    Ok(Json(json!({
        "entry_id": entry_id,
        "contact_id": contact_id,
        "step": next_step,
        "completed": completed,
        "reply": reply,
        "next_question": next_question,
    })))
}
