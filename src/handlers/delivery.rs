//! Delivery handlers — resend a delivery by entry ID.

use crate::db::questions_answers;
use crate::delivery::{
    payload::CampaignPayload, payload::ContactPayload, payload::DeliveryPayload,
    payload::QuestionAnswerPair,
};
use crate::error::AppError;
use crate::security::auth::AuthenticatedUser;
use crate::state::AppState;
use axum::{
    extract::{Path, State},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

/// Body for resending delivery.
#[derive(Deserialize)]
pub struct ResendBody {
    pub entry_id: String,
}

/// POST /api/v1/delivery/resend — authenticated, and scoped to the CALLER's own entry.
///
/// SECURITY (kanban t_373f10c0). The lookup bound only `e.id = $1` while reading
/// `cam.account_id` — the caller was never compared with it. Measured live 2026-10-02 on the pre-fix
/// binary: a throwaway account that merely knew an entry id made the platform re-deliver another
/// account's entry, with real impact (the owner's configured endpoint received the contact payload
/// and the owner's `entries` row was marked delivered with its attempt counter incremented) while
/// the handler answered `200 {"status":"resent"}`.
///
/// The predicate binds the PARENT's account — the delivery arm of this class resolves ownership from
/// the campaign the entry hangs off (`entries` itself has no `account_id`) — so foreign/absent is the
/// same `404 Entry not found` this route already returned for a missing id. 404 (not 403) is the
/// app's convention: a 403 would confirm the entry exists somewhere.
///
/// Scoped UNCONDITIONALLY — no `role == "admin"` bypass. Measured: this app has NO operator route for
/// `entries` or delivery (`grep -n "delivery/resend" src/main.rs` -> this route only; there is no
/// `/api/v1/admin/entries`), and the `ops` panel entry that calls this (`{"id": "email-ops", …,
/// "label": "Resend an entry"}`) is the same generic API exerciser whose `/provider-keys` arm the
/// card names as correctly per-account (`WHERE pk.account_id = $1`, no role branch). A cross-account
/// resend, if ever wanted, belongs on an `admin_guard`-protected `/api/v1/admin/*` route, not here.
pub async fn resend(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<ResendBody>,
) -> Result<Json<Value>, AppError> {
    let entry_id = Uuid::parse_str(&body.entry_id)
        .map_err(|_| AppError::BadRequest("Invalid entry ID".to_string()))?;
    let caller_account_id = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;

    // Get the entry — the caller's OWN entry only (`cam.account_id` is the parent's owner).
    let row = sqlx::query(
        r#"SELECT e.id, e.contact_id, e.campaign_id, e.score, e.outcome,
                  e.tags_applied, e.created_at,
                  c.first_name, c.last_name, c.email, c.phone, c.business_name,
                  cam.name, cam.type, cam.tag_namespace, cam.delivery_method,
                  cam.delivery_config, cam.account_id
           FROM entries e
           JOIN contacts c ON c.id = e.contact_id
           JOIN campaigns cam ON cam.id = e.campaign_id
           WHERE e.id = $1 AND cam.account_id = $2"#,
    )
    .bind(entry_id)
    .bind(caller_account_id)
    .fetch_optional(&state.db)
    .await?;

    let row = match row {
        Some(r) => r,
        None => return Err(AppError::NotFound("Entry not found".to_string())),
    };

    use sqlx::Row;
    let contact_id: Uuid = row.get("contact_id");
    let campaign_id: Uuid = row.get("campaign_id");
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
    let campaign_account_id: Uuid = row.get("account_id");

    // CRITICAL: Get Q&A from normalized join (questions table), not from raw JSONB
    let normalized_qa = questions_answers::get_questions_with_answers(&state.db, &entry_id).await?;
    let qa_pairs: Vec<QuestionAnswerPair> = normalized_qa
        .iter()
        .map(|qa| QuestionAnswerPair {
            question: qa.question_text.clone(),
            answer: qa.value.clone(),
        })
        .collect();

    // Build payload
    let payload = DeliveryPayload::build(
        ContactPayload {
            first_name,
            last_name,
            email,
            phone,
            website: None,
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

    // Trigger integrations using the shared dispatch logic
    // Unused variable kept for backwards compat in the query
    let _ = delivery_method;
    crate::handlers::entries::dispatch_integrations(
        &state.http_client,
        &delivery_config,
        &payload,
        &state.db,
        &entry_id,
        &campaign_account_id,
    )
    .await?;

    Ok(Json(json!({
        "status": "resent",
        "entry_id": entry_id.to_string(),
    })))
}

/// GET /api/v1/campaigns/{slug}/delivery-attempts — what the app actually did for this campaign's
/// outcomes, newest first.
///
/// The guide tells the operator that "the email and webhooks are recorded in the delivery log", and
/// until this route existed there was no screen (and no read route) behind that sentence: a failing
/// Mailchimp / HubSpot / webhook push wrote a `delivery_log` row nobody could see, so the only way to
/// discover a broken integration was to read the server log. The console's Delivery modal now lists
/// these rows.
///
/// SECURITY: scoped exactly like the console's other campaign reads — the campaign is resolved for
/// the CALLER's account (`get_campaign_by_slug_for_account`), so a foreign or unknown slug is the
/// same 404 and no `role == "admin"` bypass exists.
pub async fn list_delivery_attempts(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(slug): Path<String>,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;
    let campaign =
        crate::db::campaigns::get_campaign_by_slug_for_account(&state.db, &slug, &account_id)
            .await?;

    let rows = sqlx::query(
        r#"SELECT dl.method, dl.target, dl.success, dl.response_code,
                  left(coalesce(dl.response_body, ''), 400) AS response_body,
                  dl.attempted_at, dl.entry_id
           FROM delivery_log dl
           JOIN entries e ON e.id = dl.entry_id
           WHERE e.campaign_id = $1
           ORDER BY dl.attempted_at DESC
           LIMIT 50"#,
    )
    .bind(campaign.id)
    .fetch_all(&state.db)
    .await?;

    use sqlx::Row;
    let attempts: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "method": r.try_get::<String, _>("method").unwrap_or_default(),
                "target": r.try_get::<String, _>("target").unwrap_or_default(),
                "success": r.try_get::<bool, _>("success").unwrap_or(false),
                "response_code": r.try_get::<Option<i32>, _>("response_code").ok().flatten(),
                "response_body": r.try_get::<Option<String>, _>("response_body").ok().flatten(),
                "attempted_at": r.try_get::<chrono::DateTime<chrono::Utc>, _>("attempted_at")
                    .map(|t| t.to_rfc3339())
                    .unwrap_or_default(),
                "entry_id": r.try_get::<Uuid, _>("entry_id").map(|u| u.to_string()).unwrap_or_default(),
            })
        })
        .collect();

    Ok(Json(json!({
        "campaign_id": campaign.id,
        "campaign_slug": slug,
        "attempts": attempts,
    })))
}
