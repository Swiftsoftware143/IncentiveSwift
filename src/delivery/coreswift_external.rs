//! CoreSwift external push — per-user (personal API key) delivery of captured
//! lead data points from IncentiveSwift into CoreSwift CRM.
//!
//! Replaces the old hardcoded `coreswift_push.rs` anti-pattern (hardcoded tenant
//! UUID + global X-Internal-Key). Uses the per-account personal API key (stored in
//! `provider_keys` with provider="coreswift") and pushes to CoreSwift's
//! `/api/external/contacts` endpoint.
//!
//! Field mapping is per-campaign via question `crm_field`:
//!   - crm_field set to a name  -> map that question's answer to that CoreSwift field
//!   - crm_field = null/empty   -> auto: use question_text as the field name
//!   - crm_field = "__ignore__" -> skip this question (don't push)

use crate::state::AppState;
use serde_json::{json, Map, Value};
use uuid::Uuid;

/// Resolve the account's CoreSwift connection (personal API key + base URL) from
/// provider_keys. Returns None if not connected.
pub async fn get_coreswift_connection(
    state: &AppState,
    account_id: &Uuid,
) -> Option<(String, String)> {
    let row = sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT api_key, base_url FROM provider_keys
         WHERE account_id = $1 AND provider = 'coreswift' AND is_active = true",
    )
    .bind(account_id)
    .fetch_optional(&state.db)
    .await
    .ok()??;

    let api_key = row.0;
    let base_url = row
        .1
        .filter(|u| !u.is_empty())
        .or_else(|| {
            let def = state.config.coreswift_url.trim().to_string();
            if def.is_empty() {
                None
            } else {
                Some(def)
            }
        })
        .map(|u| u.trim_end_matches('/').to_string())?;

    Some((api_key, base_url))
}

/// Fetch the campaign's per-campaign CoreSwift list id from delivery_config jsonb.
pub async fn get_campaign_coreswift_list(state: &AppState, campaign_id: &Uuid) -> Option<String> {
    let cfg: Value = sqlx::query_scalar::<_, Value>(
        "SELECT delivery_config FROM campaigns WHERE id = $1 AND delivery_config IS NOT NULL",
    )
    .bind(campaign_id)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten()?;

    cfg.get("coreswift")
        .and_then(|c| c.get("list_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Find the campaign id linked to an IQS funnel via campaigns.iqs_funnel_id.
pub async fn find_campaign_by_iqs_funnel(state: &AppState, funnel_id: &Uuid) -> Option<Uuid> {
    sqlx::query_scalar::<_, Uuid>("SELECT id FROM campaigns WHERE iqs_funnel_id = $1 LIMIT 1")
        .bind(funnel_id.to_string())
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()
}

/// Core push: contact + fields + tags + list -> POST /api/external/contacts.
///
/// Shared by the regular-entry path and the IQS path. Never fails the caller.
async fn do_push(
    state: &AppState,
    account_id: &Uuid,
    contact_id: &Uuid,
    tags: &[String],
    list_id: Option<String>,
    fields: Value,
    context_label: &str,
) -> bool {
    let (api_key, base_url) = match get_coreswift_connection(state, account_id).await {
        Some(c) => c,
        None => {
            tracing::debug!(
                "CoreSwift external push: account {account_id} not connected, skipping"
            );
            return false;
        }
    };

    let contact = sqlx::query_as::<
        _,
        (
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
        ),
    >(
        "SELECT first_name, last_name, email, phone, business_name FROM contacts WHERE id = $1",
    )
    .bind(contact_id)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();

    let (first_name, last_name, email, phone, business_name) = match contact {
        Some(c) => c,
        None => {
            tracing::warn!("CoreSwift external push: contact {} not found", contact_id);
            return false;
        }
    };

    let mut body = Map::new();
    body.insert("first_name".into(), json!(first_name));
    body.insert("last_name".into(), json!(last_name));
    if let Some(e) = email {
        body.insert("email".into(), json!(e));
    }
    if let Some(p) = phone {
        body.insert("phone".into(), json!(p));
    }
    if let Some(b) = business_name {
        body.insert("company".into(), json!(b));
    }
    body.insert("source".into(), json!("incentiveswift"));
    body.insert("source_app".into(), json!("incentiveswift"));
    if let Some(lid) = &list_id {
        body.insert("list_id".into(), json!(lid));
    }
    if !tags.is_empty() {
        body.insert("tags".into(), json!(tags));
    }
    body.insert("fields".into(), fields);

    let url = format!("{base_url}/api/external/contacts");

    let resp = match state
        .http_client
        .post(&url)
        .bearer_auth(&api_key)
        .json(&Value::Object(body))
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                "CoreSwift external push failed (network) for contact {contact_id}: {e}"
            );
            return false;
        }
    };

    let status = resp.status();
    if status.is_success() {
        tracing::info!(
            "CoreSwift external push OK: contact {contact_id} ({context_label}, list={})",
            list_id.unwrap_or_else(|| "-".into())
        );
        true
    } else {
        let body = resp.text().await.unwrap_or_default();
        let body: String = body.chars().take(300).collect();
        tracing::warn!(
            "CoreSwift external push returned {status} for contact {contact_id}: {body}"
        );
        false
    }
}

/// Map an answer's target field name from question_text + optional crm_field override.
fn resolve_field_key(question_text: &str, crm_field: Option<&str>) -> Option<String> {
    if crm_field == Some("__ignore__") {
        return None;
    }
    match crm_field {
        Some(cf) if !cf.trim().is_empty() => Some(cf.trim().to_string()),
        _ => {
            let t = question_text.trim();
            if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Path 1: regular campaign entries
// ---------------------------------------------------------------------------

/// Build the mapped `fields` object from an entry's answers + the campaign's questions.
pub async fn build_field_mapping(state: &AppState, entry_id: &Uuid) -> Value {
    let rows = sqlx::query_as::<_, (String, Option<String>, Option<String>, String)>(
        r#"SELECT q.question_text, q.crm_field, q.question_type, a.value
           FROM answers a
           JOIN questions q ON q.id = a.question_id
           WHERE a.entry_id = $1
           ORDER BY q.sort_order"#,
    )
    .bind(entry_id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    let mut fields = Map::new();
    for (question_text, crm_field, _qtype, value) in rows {
        let Some(key) = resolve_field_key(&question_text, crm_field.as_deref()) else {
            continue;
        };
        if fields.contains_key(&key) {
            continue;
        }
        fields.insert(key, json!(value));
    }

    Value::Object(fields)
}

/// Best-effort push of a regular campaign entry into CoreSwift.
pub async fn push_entry_to_coreswift(
    state: &AppState,
    contact_id: &Uuid,
    campaign_id: &Uuid,
    entry_id: &Uuid,
    tags: &[String],
) -> bool {
    let campaign =
        sqlx::query_as::<_, (Uuid, String)>("SELECT account_id, name FROM campaigns WHERE id = $1")
            .bind(campaign_id)
            .fetch_optional(&state.db)
            .await
            .ok()
            .flatten();

    let (account_id, campaign_name) = match campaign {
        Some((a, n)) => (a, n),
        None => {
            tracing::warn!(
                "CoreSwift external push: campaign {} not found",
                campaign_id
            );
            return false;
        }
    };

    let list_id = get_campaign_coreswift_list(state, campaign_id).await;
    let fields = build_field_mapping(state, entry_id).await;

    do_push(
        state,
        &account_id,
        contact_id,
        tags,
        list_id,
        fields,
        &format!("campaign '{campaign_name}'"),
    )
    .await
}

// ---------------------------------------------------------------------------
// Path 2: IQS funnel submissions
// ---------------------------------------------------------------------------

/// One answer record from an IQS submission (question question_key/question_text + value).
#[derive(Debug, Clone)]
pub struct IqsAnswerField {
    pub question_key: String,
    pub question_text: String,
    pub value: String,
    pub crm_field: Option<String>,
}

/// Build the mapped `fields` object from IQS answer records (already resolved from
/// the funnel's `iqs_questions` with crm_field baked in).
pub fn build_iqs_field_mapping(answers: &[IqsAnswerField]) -> Value {
    let mut fields = Map::new();
    for a in answers {
        let Some(key) = resolve_field_key(&a.question_text, a.crm_field.as_deref()) else {
            continue;
        };
        if fields.contains_key(&key) {
            continue;
        }
        fields.insert(key, json!(a.value));
    }
    Value::Object(fields)
}

/// Best-effort push of an IQS submission into CoreSwift.
///
/// `footer`: resolved field mapping (question -> crm_field) already applied to `answers`.
/// The list is resolved by finding the campaign linked to this funnel.
pub async fn push_iqs_submission_to_coreswift(
    state: &AppState,
    account_id: &Uuid,
    funnel_id: &Uuid,
    contact_id: &Uuid,
    tags: &[String],
    answers: &[IqsAnswerField],
) -> bool {
    // Find the campaign linked to this funnel (for its list id)
    let campaign_id = find_campaign_by_iqs_funnel(state, funnel_id).await;
    let list_id = match campaign_id {
        Some(cid) => get_campaign_coreswift_list(state, &cid).await,
        None => None,
    };

    let fields = build_iqs_field_mapping(answers);

    do_push(
        state,
        account_id,
        contact_id,
        tags,
        list_id,
        fields,
        "iqs_submission",
    )
    .await
}
