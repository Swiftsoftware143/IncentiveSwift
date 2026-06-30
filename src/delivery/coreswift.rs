//! WorkflowSwift push — send entry data to WorkflowSwift for routing.
//!
//! Instead of IncentiveSwift pushing directly to third-party tools,
//! it pushes to WorkflowSwift's incoming webhook. WorkflowSwift handles
//! the rest — integration dispatch with stored API keys, n8n workflow
//! triggers, workflow step execution, etc.
//!
//! This is the hands-off layer: users configure everything in WorkflowSwift.

use crate::delivery::payload::DeliveryPayload;
use crate::error::AppError;
use serde_json::json;

/// Push entry data to WorkflowSwift for orchestrated routing.
///
/// WorkflowSwift's POST /api/incoming handler will:
///   1. Match the incoming data to an active workflow
///   2. Create a workflow instance
///   3. Step through configured workflow steps
///   4. Dispatch to integration targets using stored API keys
///   5. Trigger n8n workflows as configured
///
/// Best-effort: won't fail the entry if WorkflowSwift is unreachable.
pub async fn push_to_workflowswift(
    client: &reqwest::Client,
    workflowswift_url: &str,
    payload: &DeliveryPayload,
) -> Result<(), AppError> {
    let url = format!("{}/api/incoming", workflowswift_url.trim_end_matches('/'));

    // Build the incoming payload for WorkflowSwift
    let mut qa_map = serde_json::Map::new();
    for qa in &payload.questions_and_answers {
        qa_map.insert(qa.question.clone(), json!(qa.answer));
    }

    let body = json!({
        "source": "incentiveswift",
        "campaign_slug": payload.campaign.tag_namespace,
        "contact": {
            "first_name": payload.contact.first_name,
            "last_name": payload.contact.last_name,
            "email": payload.contact.email,
            "phone": payload.contact.phone,
            "business_name": payload.contact.business_name,
        },
        "data": {
            "campaign": {
                "name": payload.campaign.name,
                "type": payload.campaign.campaign_type,
                "tag_namespace": payload.campaign.tag_namespace,
            },
            "outcome": payload.outcome,
            "tags": payload.tags_applied,
            "score": payload.score,
            "answers": serde_json::Value::Object(qa_map),
            "entry_id": payload.entry_id,
            "captured_at": payload.captured_at,
        },
        "source_entry_id": payload.entry_id,
    });

    let internal_key = std::env::var("INTERNAL_SYNC_KEY").unwrap_or_default();

    let mut req_builder = client
        .post(&url)
        .json(&body)
        .timeout(std::time::Duration::from_secs(15));

    if !internal_key.is_empty() {
        req_builder = req_builder.header("X-Internal-Key", &internal_key);
    }

    match req_builder.send().await
    {
        Ok(resp) => {
            let status = resp.status();
            if status.is_success() {
                tracing::info!(
                    "WorkflowSwift push successful for {}",
                    payload.contact.email.as_deref().unwrap_or("unknown")
                );
            } else {
                let body_text = resp.text().await.unwrap_or_default();
                tracing::warn!(
                    "WorkflowSwift push returned {}: {}",
                    status,
                    body_text
                );
            }
            Ok(())
        }
        Err(e) => {
            tracing::warn!("WorkflowSwift push failed: {}", e);
            // Best-effort — don't fail the entry
            Ok(())
        }
    }
}
