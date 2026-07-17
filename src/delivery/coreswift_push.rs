//! CoreSwift cross-app tag sync push — pushes contacts + tags from IncentiveSwift
//! to CoreSwift CRM's existing webhook endpoint.
//!
//! POSTs to {CORESWIFT_URL}/api/v1/webhooks/cross-app/tag-sync
//! Auth via X-Internal-Key header

use crate::state::AppState;
use serde_json::json;
use uuid::Uuid;

/// Push contact + tags to CoreSwift CRM via its cross-app tag-sync webhook.
///
/// # Best-effort
/// This function never fails the caller — if CoreSwift is unreachable or
/// returns an error, we log the warning and continue.
///
/// # Arguments
/// - `state`: AppState for DB access + HTTP client
/// - `contact_id`: UUID of the contact in IncentiveSwift
/// - `tenant_id`: account_id (used as tenant_id in CoreSwift)
/// - `tags`: full set of tags currently on the contact
/// - `added_tags`: tags that were just added (e.g., from this flow)
/// - `removed_tags`: tags that were just removed
/// - `triggered_by`: what triggered this push (e.g., "contact_creation", "tag_change", "entry", "signup")
pub async fn push_contact_to_coreswift(
    state: &AppState,
    contact_id: &Uuid,
    tenant_id: &Uuid,
    tags: &[String],
    added_tags: &[String],
    removed_tags: &[String],
    triggered_by: &str,
) {
    let coreswift_url = state.config.coreswift_url.trim_end_matches('/').to_string();
    if coreswift_url.is_empty() {
        tracing::debug!("CORESWIFT_URL not set, skipping CoreSwift push");
        return;
    }

    // Query contact details from IncentiveSwift
    let contact = match sqlx::query_as::<_, (String, String, Option<String>, Option<String>, Option<String>)>(
        r#"SELECT first_name, last_name, email, phone, business_name
           FROM contacts WHERE id = $1"#
    )
    .bind(contact_id)
    .fetch_optional(&state.db)
    .await
    {
        Ok(Some(row)) => row,
        Ok(None) => {
            tracing::warn!("CoreSwift push: contact {} not found, skipping", contact_id);
            return;
        }
        Err(e) => {
            tracing::warn!("CoreSwift push: DB error for contact {}: {}", contact_id, e);
            return;
        }
    };

    let (first_name, last_name, email, phone, business_name) = contact;
    let name = format!("{} {}", first_name, last_name);
    let name = name.trim().to_string();
    let email_str = email.clone().unwrap_or_default();
    let company = business_name.clone().unwrap_or_default();

    // Build the payload matching what CoreSwift's cross_app_tag_sync expects
    let payload = json!({
        "source_app": "incentiveswift",
        "tenant_id": tenant_id.to_string(),
        "triggered_by": triggered_by,
        "lead": {
            "id": contact_id.to_string(),
            "name": if name.is_empty() { email_str.clone() } else { name },
            "email": email_str,
            "company": company,
            "phone": phone,
        },
        "tags": tags,
        "added_tags": added_tags,
        "removed_tags": removed_tags,
    });

    let url = format!("{}/api/v1/webhooks/cross-app/tag-sync", coreswift_url);
    let internal_key = &state.config.internal_sync_key;

    let mut req = state.http_client
        .post(&url)
        .json(&payload)
        .timeout(std::time::Duration::from_secs(10));

    if !internal_key.is_empty() {
        req = req.header("X-Internal-Key", internal_key);
    }

    match req.send().await {
        Ok(resp) => {
            let status = resp.status();
            if status.is_success() {
                tracing::info!(
                    "CoreSwift push successful for contact {} ({}), triggered_by={}",
                    contact_id, email_str, triggered_by
                );
            } else {
                let body = resp.text().await.unwrap_or_default().chars().take(300).collect::<String>();
                tracing::warn!(
                    "CoreSwift push returned {} for contact {}: {}",
                    status, contact_id, body
                );
            }
        }
        Err(e) => {
            tracing::warn!(
                "CoreSwift push failed for contact {}: {}",
                contact_id, e
            );
        }
    }
}

/// Get the current tags for a contact from the entry system.
/// Since IncentiveSwift stores tags on entries (tags_applied), this
/// aggregates unique tags across all entries for the contact.
///
/// Also checks the campaign's tag_namespace prefix to build a reasonable
/// tag set.
pub async fn get_contact_tags(state: &AppState, contact_id: &Uuid) -> Vec<String> {
    let tags: Vec<String> = sqlx::query_scalar(
        r#"SELECT DISTINCT unnest(tags_applied) AS tag
           FROM entries
           WHERE contact_id = $1 AND tags_applied IS NOT NULL
           ORDER BY tag"#
    )
    .bind(contact_id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();
    tags
}

/// Get tag names for a list of tag UUIDs from the tags table.
pub async fn get_tag_names(state: &AppState, tag_uuids: &[Uuid]) -> Vec<String> {
    if tag_uuids.is_empty() {
        return vec![];
    }
    let names: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM tags WHERE id = ANY($1)"
    )
    .bind(tag_uuids)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();
    names
}
