//! IQS (Intelligent Qualifying Survey) — rule-based qualifying funnel system.
//!
//! Completely separate from campaigns. Funnels are survey/questionnaire flows
//! with field-type validation at submission time.
//!
//! ## Tagging System
//!
//! Each funnel has a `source_tag` applied to every submission.
//! Each question's options can declare an answer→tag mapping:
//!   `{"label": "Yes", "value": "yes", "tag": "interested_in_premium"}`
//! When the user selects that answer, the tag is applied to the contact.

use crate::delivery::coreswift_push::{get_contact_tags, push_contact_to_coreswift};
use crate::error::AppError;
use crate::security::auth::AuthenticatedUser;
use crate::state::AppState;
use axum::{
    extract::{Path, Query, State},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Models
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct IqsFunnel {
    pub id: Uuid,
    pub account_id: Uuid,
    pub name: String,
    pub funnel_type: String,
    pub description: Option<String>,
    pub status: String,
    pub source_tag: Option<String>,
    pub theme: Value,
    pub config: Value,
    pub slug: String,
    pub response_count: i32,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct IqsQuestion {
    pub id: Uuid,
    pub funnel_id: Uuid,
    pub question_key: String,
    pub question_text: String,
    pub question_type: String,
    pub sort_order: i32,
    pub required: bool,
    pub options: Value,
    pub config: Value,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct IqsRule {
    pub id: Uuid,
    pub funnel_id: Uuid,
    pub rule_type: String,
    pub priority: i32,
    pub conditions: Value,
    pub actions: Value,
    pub is_active: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct IqsSubmission {
    pub id: Uuid,
    pub funnel_id: Uuid,
    pub contact_id: Uuid,
    pub answers: Value,
    pub total_score: i32,
    pub outcome: Option<String>,
    pub tags_applied: Vec<String>,
    pub source: Value,
    pub classification: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

// ---------------------------------------------------------------------------
// Input structs
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct CreateFunnelInput {
    pub name: String,
    pub funnel_type: Option<String>,
    pub description: Option<String>,
    pub source_tag: Option<String>,
    pub theme: Option<Value>,
    pub config: Option<Value>,
    pub slug: Option<String>,
}

#[derive(Deserialize)]
pub struct UpdateFunnelInput {
    pub name: Option<String>,
    pub funnel_type: Option<String>,
    pub description: Option<String>,
    pub status: Option<String>,
    pub source_tag: Option<Option<String>>,
    pub theme: Option<Value>,
    pub config: Option<Value>,
    pub slug: Option<String>,
}

#[derive(Deserialize)]
pub struct CreateQuestionInput {
    pub question_key: String,
    pub question_text: String,
    pub question_type: Option<String>,
    pub sort_order: Option<i32>,
    pub required: Option<bool>,
    pub options: Option<Value>,
    pub config: Option<Value>,
}

#[derive(Deserialize)]
pub struct UpdateQuestionInput {
    pub question_key: Option<String>,
    pub question_text: Option<String>,
    pub question_type: Option<String>,
    pub sort_order: Option<i32>,
    pub required: Option<bool>,
    pub options: Option<Value>,
    pub config: Option<Value>,
}

#[derive(Deserialize)]
pub struct ReorderInput {
    pub question_ids: Vec<Uuid>,
}

#[derive(Deserialize)]
pub struct CreateRuleInput {
    pub rule_type: Option<String>,
    pub priority: Option<i32>,
    pub conditions: Option<Value>,
    pub actions: Option<Value>,
}

#[derive(Deserialize)]
pub struct UpdateRuleInput {
    pub rule_type: Option<String>,
    pub priority: Option<i32>,
    pub conditions: Option<Value>,
    pub actions: Option<Value>,
    pub is_active: Option<bool>,
}

/// One answer in a submission.
#[derive(Deserialize)]
pub struct AnswerBody {
    pub question_id: Uuid,
    pub value: String,
    pub score: Option<i32>,
}

#[derive(Deserialize)]
pub struct SubmitBody {
    pub contact: ContactBody,
    pub answers: Vec<AnswerBody>,
    pub source: Option<Value>,
}

#[derive(Deserialize)]
pub struct ContactBody {
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    pub email: Option<String>,
    pub phone: Option<String>,
}

#[derive(Deserialize)]
pub struct PaginationQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

// ---------------------------------------------------------------------------
// Funnel CRUD
// ---------------------------------------------------------------------------

/// GET /api/v1/iqs/funnels — list funnels for the authenticated account.
pub async fn list_funnels(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Query(pag): Query<PaginationQuery>,
) -> Result<Json<Value>, AppError> {
    let limit = pag.limit.unwrap_or(50).min(200);
    let offset = pag.offset.unwrap_or(0);

    let rows = sqlx::query_as::<_, IqsFunnel>(
        r#"SELECT id, account_id, name, funnel_type, description, status,
                   source_tag, theme, config, slug, response_count,
                   created_at, updated_at
            FROM iqs_funnels
           WHERE account_id = $1::uuid
           ORDER BY created_at DESC
           LIMIT $2 OFFSET $3"#,
    )
    .bind(&user.account_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;

    Ok(Json(json!({ "data": rows })))
}

/// POST /api/v1/iqs/funnels — create a new funnel.
pub async fn create_funnel(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<CreateFunnelInput>,
) -> Result<Json<Value>, AppError> {
    let id = Uuid::new_v4();
    let slug = body.slug.unwrap_or_else(|| slugify(&body.name));
    let funnel_type = body.funnel_type.unwrap_or_else(|| "survey".to_string());
    let theme = body.theme.unwrap_or_else(|| {
        json!({
            "preset": "dark_modern",
            "bg_gradient": "linear-gradient(135deg, #0f172a 0%, #1e293b 100%)",
            "accent_color": "#8b5cf6",
            "font_family": "Inter",
            "logo_url": null,
            "button_style": "rounded"
        })
    });
    let config = body.config.unwrap_or_else(|| {
        json!({
            "show_progress_bar": true,
            "allow_skip": false,
            "collect_email": true,
            "collect_name": true,
            "collect_phone": false,
            "redirect_url": null,
            "passing_score": 70,
            "max_attempts": 1
        })
    });

    // Check slug uniqueness
    let existing: Option<String> =
        sqlx::query_scalar("SELECT slug FROM iqs_funnels WHERE slug = $1")
            .bind(&slug)
            .fetch_optional(&state.db)
            .await?;

    if existing.is_some() {
        return Err(AppError::BadRequest(format!(
            "A funnel with slug '{}' already exists",
            slug
        )));
    }

    sqlx::query(
        r#"INSERT INTO iqs_funnels
           (id, account_id, name, funnel_type, description, status, source_tag,
            theme, config, slug, response_count)
           VALUES ($1, $2::uuid, $3, $4, $5, 'draft', $6, $7::jsonb, $8::jsonb, $9, 0)"#,
    )
    .bind(id)
    .bind(&user.account_id)
    .bind(&body.name)
    .bind(&funnel_type)
    .bind(&body.description)
    .bind(&body.source_tag)
    .bind(&theme)
    .bind(&config)
    .bind(&slug)
    .execute(&state.db)
    .await?;

    let funnel = sqlx::query_as::<_, IqsFunnel>("SELECT * FROM iqs_funnels WHERE id = $1")
        .bind(id)
        .fetch_one(&state.db)
        .await?;

    Ok(Json(json!({ "data": funnel, "slug": slug })))
}

/// GET /api/v1/iqs/funnels/:id — get a single funnel by ID.
pub async fn get_funnel(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, AppError> {
    let funnel = sqlx::query_as::<_, IqsFunnel>(
        "SELECT * FROM iqs_funnels WHERE id = $1 AND account_id = $2::uuid",
    )
    .bind(id)
    .bind(&user.account_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Funnel not found".into()))?;

    Ok(Json(json!({ "data": funnel })))
}

/// PUT /api/v1/iqs/funnels/:id — update a funnel.
pub async fn update_funnel(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateFunnelInput>,
) -> Result<Json<Value>, AppError> {
    // Fetch existing to merge
    let existing = sqlx::query_as::<_, IqsFunnel>(
        "SELECT * FROM iqs_funnels WHERE id = $1 AND account_id = $2::uuid",
    )
    .bind(id)
    .bind(&user.account_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Funnel not found".into()))?;

    let name = body.name.unwrap_or(existing.name);
    let funnel_type = body.funnel_type.unwrap_or(existing.funnel_type);
    let description = body.description.or(existing.description);
    let status = body.status.unwrap_or(existing.status);
    let source_tag = body.source_tag.unwrap_or(existing.source_tag);
    let theme = body.theme.unwrap_or(existing.theme);
    let config = body.config.unwrap_or(existing.config);
    let slug = body.slug.unwrap_or(existing.slug);

    sqlx::query(
        r#"UPDATE iqs_funnels
              SET name = $1, funnel_type = $2, description = $3, status = $4,
                  source_tag = $5, theme = $6::jsonb, config = $7::jsonb,
                  slug = $8, updated_at = NOW()
            WHERE id = $9"#,
    )
    .bind(&name)
    .bind(&funnel_type)
    .bind(&description)
    .bind(&status)
    .bind(&source_tag)
    .bind(&theme)
    .bind(&config)
    .bind(&slug)
    .bind(id)
    .execute(&state.db)
    .await?;

    let updated = sqlx::query_as::<_, IqsFunnel>("SELECT * FROM iqs_funnels WHERE id = $1")
        .bind(id)
        .fetch_one(&state.db)
        .await?;

    Ok(Json(json!({ "data": updated })))
}

/// DELETE /api/v1/iqs/funnels/:id — delete a funnel (cascades questions, rules, submissions).
pub async fn delete_funnel(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, AppError> {
    let result = sqlx::query("DELETE FROM iqs_funnels WHERE id = $1 AND account_id = $2::uuid")
        .bind(id)
        .bind(&user.account_id)
        .execute(&state.db)
        .await?;

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound("Funnel not found".into()));
    }

    Ok(Json(json!({ "deleted": true })))
}

// ---------------------------------------------------------------------------
// Questions CRUD
// ---------------------------------------------------------------------------

/// GET /api/v1/iqs/funnels/:id/questions — list questions for a funnel.
pub async fn list_questions(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, AppError> {
    // Verify ownership
    verify_funnel_ownership(&state.db, &user.account_id, id).await?;

    let questions = sqlx::query_as::<_, IqsQuestion>(
        r#"SELECT * FROM iqs_questions
            WHERE funnel_id = $1
            ORDER BY sort_order ASC, created_at ASC"#,
    )
    .bind(id)
    .fetch_all(&state.db)
    .await?;

    Ok(Json(json!({ "data": questions })))
}

/// POST /api/v1/iqs/funnels/:id/questions — create a new question.
pub async fn create_question(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(id): Path<Uuid>,
    Json(body): Json<CreateQuestionInput>,
) -> Result<Json<Value>, AppError> {
    verify_funnel_ownership(&state.db, &user.account_id, id).await?;

    let qid = Uuid::new_v4();
    let question_type = body
        .question_type
        .unwrap_or_else(|| "single_choice".to_string());
    let required = body.required.unwrap_or(true);
    let options = body.options.unwrap_or_else(|| json!([]));
    let question_config = body.config.unwrap_or_else(|| json!({}));

    // Auto-assign sort_order if not given
    let sort_order = match body.sort_order {
        Some(s) => s,
        None => {
            let max: Option<i32> = sqlx::query_scalar(
                "SELECT MAX(sort_order) FROM iqs_questions WHERE funnel_id = $1",
            )
            .bind(id)
            .fetch_one(&state.db)
            .await?;
            max.unwrap_or(-1) + 1
        }
    };

    sqlx::query(
        r#"INSERT INTO iqs_questions
           (id, funnel_id, question_key, question_text, question_type,
            sort_order, required, options, config)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8::jsonb, $9::jsonb)"#,
    )
    .bind(qid)
    .bind(id)
    .bind(&body.question_key)
    .bind(&body.question_text)
    .bind(&question_type)
    .bind(sort_order)
    .bind(required)
    .bind(&options)
    .bind(&question_config)
    .execute(&state.db)
    .await?;

    let question = sqlx::query_as::<_, IqsQuestion>("SELECT * FROM iqs_questions WHERE id = $1")
        .bind(qid)
        .fetch_one(&state.db)
        .await?;

    Ok(Json(json!({ "data": question })))
}

/// PUT /api/v1/iqs/funnels/:id/questions/reorder — batch reorder questions.
pub async fn reorder_questions(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(id): Path<Uuid>,
    Json(body): Json<ReorderInput>,
) -> Result<Json<Value>, AppError> {
    verify_funnel_ownership(&state.db, &user.account_id, id).await?;

    for (i, qid) in body.question_ids.iter().enumerate() {
        sqlx::query("UPDATE iqs_questions SET sort_order = $1 WHERE id = $2 AND funnel_id = $3")
            .bind(i as i32)
            .bind(qid)
            .bind(id)
            .execute(&state.db)
            .await?;
    }

    Ok(Json(json!({ "reordered": true })))
}

/// PUT /api/v1/iqs/funnels/:id/questions/:qid — update a question.
pub async fn update_question(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path((_fid, qid)): Path<(Uuid, Uuid)>,
    Json(body): Json<UpdateQuestionInput>,
) -> Result<Json<Value>, AppError> {
    // Fetch existing question (ownership verified through funnel)
    let existing = sqlx::query_as::<_, IqsQuestion>(
        r#"SELECT q.* FROM iqs_questions q
            JOIN iqs_funnels f ON f.id = q.funnel_id
           WHERE q.id = $1 AND f.account_id = $2::uuid"#,
    )
    .bind(qid)
    .bind(&user.account_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Question not found".into()))?;

    let question_key = body.question_key.unwrap_or(existing.question_key);
    let question_text = body.question_text.unwrap_or(existing.question_text);
    let question_type = body.question_type.unwrap_or(existing.question_type);
    let sort_order = body.sort_order.unwrap_or(existing.sort_order);
    let required = body.required.unwrap_or(existing.required);
    let options = body.options.unwrap_or(existing.options);
    let config = body.config.unwrap_or(existing.config);

    sqlx::query(
        r#"UPDATE iqs_questions
              SET question_key = $1, question_text = $2, question_type = $3,
                  sort_order = $4, required = $5, options = $6::jsonb,
                  config = $7::jsonb, updated_at = NOW()
            WHERE id = $8"#,
    )
    .bind(&question_key)
    .bind(&question_text)
    .bind(&question_type)
    .bind(sort_order)
    .bind(required)
    .bind(&options)
    .bind(&config)
    .bind(qid)
    .execute(&state.db)
    .await?;

    let updated = sqlx::query_as::<_, IqsQuestion>("SELECT * FROM iqs_questions WHERE id = $1")
        .bind(qid)
        .fetch_one(&state.db)
        .await?;

    Ok(Json(json!({ "data": updated })))
}

/// DELETE /api/v1/iqs/funnels/:id/questions/:qid — delete a question.
pub async fn delete_question(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path((_fid, qid)): Path<(Uuid, Uuid)>,
) -> Result<Json<Value>, AppError> {
    // Verify ownership through funnel join
    let existing: Option<Uuid> = sqlx::query_scalar(
        r#"SELECT q.id FROM iqs_questions q
            JOIN iqs_funnels f ON f.id = q.funnel_id
           WHERE q.id = $1 AND f.account_id = $2::uuid"#,
    )
    .bind(qid)
    .bind(&user.account_id)
    .fetch_optional(&state.db)
    .await?;

    if existing.is_none() {
        return Err(AppError::NotFound("Question not found".into()));
    }

    sqlx::query("DELETE FROM iqs_questions WHERE id = $1")
        .bind(qid)
        .execute(&state.db)
        .await?;

    Ok(Json(json!({ "deleted": true })))
}

// ---------------------------------------------------------------------------
// Rules CRUD
// ---------------------------------------------------------------------------

/// GET /api/v1/iqs/funnels/:id/rules — list rules for a funnel.
pub async fn list_rules(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, AppError> {
    verify_funnel_ownership(&state.db, &user.account_id, id).await?;

    let rules = sqlx::query_as::<_, IqsRule>(
        r#"SELECT * FROM iqs_rules
            WHERE funnel_id = $1
            ORDER BY priority ASC, created_at ASC"#,
    )
    .bind(id)
    .fetch_all(&state.db)
    .await?;

    Ok(Json(json!({ "data": rules })))
}

/// POST /api/v1/iqs/funnels/:id/rules — create a rule.
pub async fn create_rule(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(id): Path<Uuid>,
    Json(body): Json<CreateRuleInput>,
) -> Result<Json<Value>, AppError> {
    verify_funnel_ownership(&state.db, &user.account_id, id).await?;

    let rid = Uuid::new_v4();
    let rule_type = body.rule_type.unwrap_or_else(|| "always".to_string());
    let priority = body.priority.unwrap_or(0);
    let conditions = body.conditions.unwrap_or_else(|| json!([]));
    let actions = body.actions.unwrap_or_else(|| json!([]));

    sqlx::query(
        r#"INSERT INTO iqs_rules
           (id, funnel_id, rule_type, priority, conditions, actions)
           VALUES ($1, $2, $3, $4, $5::jsonb, $6::jsonb)"#,
    )
    .bind(rid)
    .bind(id)
    .bind(&rule_type)
    .bind(priority)
    .bind(&conditions)
    .bind(&actions)
    .execute(&state.db)
    .await?;

    let rule = sqlx::query_as::<_, IqsRule>("SELECT * FROM iqs_rules WHERE id = $1")
        .bind(rid)
        .fetch_one(&state.db)
        .await?;

    Ok(Json(json!({ "data": rule })))
}

/// PUT /api/v1/iqs/funnels/:id/rules/:rid — update a rule.
pub async fn update_rule(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path((fid, rid)): Path<(Uuid, Uuid)>,
    Json(body): Json<UpdateRuleInput>,
) -> Result<Json<Value>, AppError> {
    let existing = sqlx::query_as::<_, IqsRule>(
        r#"SELECT r.* FROM iqs_rules r
            JOIN iqs_funnels f ON f.id = r.funnel_id
           WHERE r.id = $1 AND f.id = $2 AND f.account_id = $3::uuid"#,
    )
    .bind(rid)
    .bind(fid)
    .bind(&user.account_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Rule not found".into()))?;

    let rule_type = body.rule_type.unwrap_or(existing.rule_type);
    let priority = body.priority.unwrap_or(existing.priority);
    let conditions = body.conditions.unwrap_or(existing.conditions);
    let actions = body.actions.unwrap_or(existing.actions);
    let is_active = body.is_active.unwrap_or(existing.is_active);

    sqlx::query(
        r#"UPDATE iqs_rules
              SET rule_type = $1, priority = $2, conditions = $3::jsonb,
                  actions = $4::jsonb, is_active = $5, updated_at = NOW()
            WHERE id = $6"#,
    )
    .bind(&rule_type)
    .bind(priority)
    .bind(&conditions)
    .bind(&actions)
    .bind(is_active)
    .bind(rid)
    .execute(&state.db)
    .await?;

    let updated = sqlx::query_as::<_, IqsRule>("SELECT * FROM iqs_rules WHERE id = $1")
        .bind(rid)
        .fetch_one(&state.db)
        .await?;

    Ok(Json(json!({ "data": updated })))
}

/// DELETE /api/v1/iqs/funnels/:id/rules/:rid — delete a rule.
pub async fn delete_rule(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path((fid, rid)): Path<(Uuid, Uuid)>,
) -> Result<Json<Value>, AppError> {
    // Verify ownership through funnel join
    let result = sqlx::query(
        r#"DELETE FROM iqs_rules r
            USING iqs_funnels f
           WHERE r.id = $1 AND r.funnel_id = f.id
             AND f.id = $2 AND f.account_id = $3::uuid"#,
    )
    .bind(rid)
    .bind(fid)
    .bind(&user.account_id)
    .execute(&state.db)
    .await?;

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound("Rule not found".into()));
    }

    Ok(Json(json!({ "deleted": true })))
}

// ---------------------------------------------------------------------------
// Public play endpoints (no auth required)
// ---------------------------------------------------------------------------

/// GET /api/v1/iqs/play/:slug — get funnel config + questions (public embed data).
pub async fn get_play_funnel(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> Result<Json<Value>, AppError> {
    let funnel = sqlx::query_as::<_, IqsFunnel>(
        r#"SELECT * FROM iqs_funnels WHERE slug = $1 AND status = 'active'"#,
    )
    .bind(&slug)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Funnel not found or not active".into()))?;

    let questions = sqlx::query_as::<_, IqsQuestion>(
        r#"SELECT * FROM iqs_questions
            WHERE funnel_id = $1
            ORDER BY sort_order ASC"#,
    )
    .bind(funnel.id)
    .fetch_all(&state.db)
    .await?;

    Ok(Json(json!({
        "funnel": {
            "id": funnel.id,
            "name": funnel.name,
            "funnel_type": funnel.funnel_type,
            "description": funnel.description,
            "theme": funnel.theme,
            "config": funnel.config,
            "slug": funnel.slug,
        },
        "questions": questions
    })))
}

/// POST /api/v1/iqs/play/:slug/submit — submit answers (public, with field validation).
pub async fn submit_funnel(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    Json(body): Json<SubmitBody>,
) -> Result<Json<Value>, AppError> {
    // 1. Fetch the funnel
    let funnel = sqlx::query_as::<_, IqsFunnel>(
        r#"SELECT * FROM iqs_funnels WHERE slug = $1 AND status = 'active'"#,
    )
    .bind(&slug)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Funnel not found or not active".into()))?;

    // 2. Fetch all questions for this funnel, keyed by id
    let questions = sqlx::query_as::<_, IqsQuestion>(
        r#"SELECT * FROM iqs_questions WHERE funnel_id = $1 ORDER BY sort_order ASC"#,
    )
    .bind(funnel.id)
    .fetch_all(&state.db)
    .await?;

    let question_map: std::collections::HashMap<Uuid, IqsQuestion> =
        questions.into_iter().map(|q| (q.id, q)).collect();

    // 3. Upsert contact
    let contact_id = crate::db::contacts::upsert_contact(
        &state.db,
        &crate::db::contacts::ContactInput {
            first_name: body.contact.first_name.clone(),
            last_name: body.contact.last_name.clone(),
            email: body.contact.email.clone(),
            phone: body.contact.phone.clone(),
            business_name: None,
            website: None,
        },
    )
    .await?;

    // 4. Validate each answer against field-type rules and calculate score
    let mut total_score = 0i32;
    let mut answer_records = Vec::new();
    let mut collected_tags: Vec<String> = Vec::new();
    let mut skipped_question_ids: std::collections::HashSet<Uuid> =
        std::collections::HashSet::new();

    // Add source_tag from funnel if present
    if let Some(ref tag) = funnel.source_tag {
        collected_tags.push(tag.clone());
    }

    // Build a map of question_key -> answer for show_if evaluation
    let mut answers_by_key: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for ans in &body.answers {
        if let Some(q) = question_map.get(&ans.question_id) {
            answers_by_key.insert(q.question_key.clone(), ans.value.clone());
        }
    }

    // Pre-check show_if conditions — mark questions that should be skipped
    for (_qid, question) in &question_map {
        if let Some(show_if) = question.config.get("show_if") {
            if let (Some(dep_key), Some(operator), Some(required_val)) = (
                show_if.get("depends_on").and_then(|v| v.as_str()),
                show_if.get("operator").and_then(|v| v.as_str()),
                show_if.get("value").and_then(|v| v.as_str()),
            ) {
                let dep_answer = answers_by_key
                    .get(dep_key)
                    .map(|s| s.as_str())
                    .unwrap_or("");
                let condition_met = match operator {
                    "equals" => dep_answer == required_val,
                    "not_equals" => dep_answer != required_val,
                    "contains" => dep_answer.contains(required_val),
                    _ => true, // unknown operator = show by default
                };
                if !condition_met {
                    skipped_question_ids.insert(*_qid);
                }
            }
        }
    }

    for ans in &body.answers {
        let question = question_map.get(&ans.question_id).ok_or_else(|| {
            AppError::BadRequest(format!(
                "Question {} not found in this funnel",
                ans.question_id
            ))
        })?;

        // Skip questions that fail show_if conditions
        if skipped_question_ids.contains(&ans.question_id) {
            answer_records.push(json!({
                "question_id": ans.question_id,
                "question_key": question.question_key,
                "value": ans.value,
                "score": 0,
                "skipped": true,
            }));
            continue;
        }
        let question = question_map.get(&ans.question_id).ok_or_else(|| {
            AppError::BadRequest(format!(
                "Question {} not found in this funnel",
                ans.question_id
            ))
        })?;

        // Validate field type — skip validation for empty answers on non-required questions
        if question.required || !ans.value.is_empty() || question.question_type == "field_consent" {
            let mut validation_config = question.config.clone();
            if question.options.as_array().is_some_and(|o| !o.is_empty()) {
                validation_config["options"] = question.options.clone();
            }
            crate::iqs_validation::validate_field(
                &question.question_type,
                &validation_config,
                &ans.value,
            )?;
        }

        // Calculate score for this answer
        let score = ans.score.unwrap_or_else(|| {
            // Auto-score: check if the selected value matches a scoring option
            if let Some(options) = question.options.as_array() {
                for opt in options {
                    if let (Some(val), Some(pts)) = (
                        opt.get("value").and_then(|v| v.as_str()),
                        opt.get("score").and_then(|s| s.as_i64()),
                    ) {
                        if val == ans.value {
                            return pts as i32;
                        }
                    }
                }
            }
            0
        });

        total_score += score;

        // Check for answer→tag mapping
        if let Some(options) = question.options.as_array() {
            for opt in options {
                if let (Some(val), Some(tag)) = (
                    opt.get("value").and_then(|v| v.as_str()),
                    opt.get("tag").and_then(|t| t.as_str()),
                ) {
                    if val == ans.value && !tag.is_empty() {
                        collected_tags.push(tag.to_string());
                    }
                }
            }
        }

        answer_records.push(json!({
            "question_id": ans.question_id,
            "question_key": question.question_key,
            "value": ans.value,
            "score": score,
        }));
    }

    // 5. Determine outcome based on passing_score
    let config = &funnel.config;
    let passing_score = config
        .get("passing_score")
        .and_then(|v| v.as_i64())
        .unwrap_or(70);
    // Calculate the maximum possible score — sum of highest score per non-skipped question
    let max_possible: i32 = question_map
        .values()
        .filter_map(|q| {
            if skipped_question_ids.contains(&q.id) {
                return None;
            }
            if let Some(options) = q.options.as_array() {
                options
                    .iter()
                    .filter_map(|o| o.get("score").and_then(|s| s.as_i64()))
                    .max()
                    .map(|v| v as i32)
            } else {
                Some(0)
            }
        })
        .sum();
    let percentage = if max_possible > 0 {
        (total_score as f64 / max_possible as f64) * 100.0
    } else {
        100.0
    };

    let outcome = if percentage >= passing_score as f64 {
        Some("qualified".to_string())
    } else {
        Some("disqualified".to_string())
    };

    // 6. Auto-Classification (Hot/Warm/Cold) based on classification_ranges
    let mut classification: Option<String> = None;
    if let Some(ranges) = config
        .get("classification_ranges")
        .and_then(|v| v.as_array())
    {
        for range in ranges {
            if let (Some(label), Some(min)) = (
                range.get("label").and_then(|v| v.as_str()),
                range.get("min").and_then(|v| v.as_f64()),
            ) {
                if percentage >= min {
                    classification = Some(label.to_string());
                    // Also add the range's tag if present
                    if let Some(tag) = range.get("tag").and_then(|v| v.as_str()) {
                        if !tag.is_empty() {
                            collected_tags.push(tag.to_string());
                        }
                    }
                    break;
                }
            }
        }
    }

    // 7. De-duplicate tags
    collected_tags.sort();
    collected_tags.dedup();

    // 8. Insert submission
    let submission_id = Uuid::new_v4();
    let source = body.source.unwrap_or_else(|| json!({}));

    sqlx::query(
        r#"INSERT INTO iqs_submissions
           (id, funnel_id, contact_id, answers, total_score, outcome, tags_applied, source, classification)
           VALUES ($1, $2, $3, $4::jsonb, $5, $6, $7::text[], $8::jsonb, $9)"#,
    )
    .bind(submission_id)
    .bind(funnel.id)
    .bind(contact_id)
    .bind(json!(answer_records))
    .bind(total_score)
    .bind(&outcome)
    .bind(&collected_tags)
    .bind(&source)
    .bind(&classification)
    .execute(&state.db)
    .await?;

    // 9. Increment response_count
    sqlx::query("UPDATE iqs_funnels SET response_count = response_count + 1 WHERE id = $1")
        .bind(funnel.id)
        .execute(&state.db)
        .await?;

    // 10. Push to CoreSwift
    let contact_tags = get_contact_tags(&state, &contact_id).await;
    push_contact_to_coreswift(
        &state,
        &contact_id,
        &funnel.account_id,
        &contact_tags,
        &collected_tags,
        &[],
        "iqs_submission",
    )
    .await;

    Ok(Json(json!({
        "data": {
            "submission_id": submission_id,
            "outcome": outcome,
            "score": total_score,
            "percentage": (percentage * 100.0).round() / 100.0,
            "tags_applied": collected_tags,
            "passed": percentage >= passing_score as f64,
            "classification": classification,
        }
    })))
}

// ---------------------------------------------------------------------------
// Campaign-linked IQS funnel questions
// ---------------------------------------------------------------------------

/// GET /api/v1/campaigns/:id/iqs-funnel-questions
/// Fetch questions from the IQS funnel attached to a campaign.
pub async fn get_campaign_iqs_questions(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(slug): Path<String>,
) -> Result<Json<Value>, AppError> {
    // Resolve campaign by slug or UUID
    let campaign = if let Ok(id) = uuid::Uuid::parse_str(&slug) {
        sqlx::query_as::<_, crate::db::campaigns::Campaign>(
            r#"SELECT id, name, slug, type as "type", status,
                      config, tag_namespace,
                      outcome_tags,
                      delivery_method, delivery_config,
                      created_at,
                      account_id,
                      loyalty_program_id,
                      loyalty_points_per_play,
                      auto_enroll_loyalty,
                      iqs_funnel_id
               FROM campaigns WHERE id = $1 AND account_id = $2::uuid"#,
        )
        .bind(id)
        .bind(&user.account_id)
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Campaign not found".into()))?
    } else {
        // Fetch by slug — but we need account context for ownership
        sqlx::query_as::<_, crate::db::campaigns::Campaign>(
            r#"SELECT id, name, slug, type as "type", status,
                      config, tag_namespace,
                      outcome_tags,
                      delivery_method, delivery_config,
                      created_at,
                      account_id,
                      loyalty_program_id,
                      loyalty_points_per_play,
                      auto_enroll_loyalty,
                      iqs_funnel_id
               FROM campaigns WHERE slug = $1 AND account_id = $2::uuid"#,
        )
        .bind(&slug)
        .bind(&user.account_id)
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Campaign not found".into()))?
    };

    let funnel_id = match campaign.iqs_funnel_id {
        Some(fid) => fid,
        None => {
            return Err(AppError::BadRequest(
                "Campaign has no IQS funnel attached".into(),
            ))
        }
    };

    // Fetch the funnel
    let funnel = sqlx::query_as::<_, IqsFunnel>("SELECT * FROM iqs_funnels WHERE id = $1")
        .bind(funnel_id)
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("IQS funnel not found".into()))?;

    // Fetch questions
    let questions = sqlx::query_as::<_, IqsQuestion>(
        r#"SELECT * FROM iqs_questions
            WHERE funnel_id = $1
            ORDER BY sort_order ASC, created_at ASC"#,
    )
    .bind(funnel_id)
    .fetch_all(&state.db)
    .await?;

    Ok(Json(json!({
        "funnel": {
            "id": funnel.id,
            "name": funnel.name,
            "description": funnel.description,
            "slug": funnel.slug,
            "config": funnel.config,
        },
        "questions": questions,
    })))
}

// ---------------------------------------------------------------------------
// Submissions list (authenticated)
// ---------------------------------------------------------------------------

/// GET /api/v1/iqs/funnels/:id/submissions — list submissions for a funnel.
pub async fn list_submissions(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(id): Path<Uuid>,
    Query(pag): Query<PaginationQuery>,
) -> Result<Json<Value>, AppError> {
    verify_funnel_ownership(&state.db, &user.account_id, id).await?;

    let limit = pag.limit.unwrap_or(50).min(200);
    let offset = pag.offset.unwrap_or(0);

    let submissions = sqlx::query_as::<_, IqsSubmission>(
        r#"SELECT * FROM iqs_submissions
            WHERE funnel_id = $1
            ORDER BY created_at DESC
            LIMIT $2 OFFSET $3"#,
    )
    .bind(id)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;

    Ok(Json(json!({ "data": submissions })))
}

// ---------------------------------------------------------------------------
// File upload
// ---------------------------------------------------------------------------

/// POST /api/v1/iqs/upload — upload a file for a file-type question.
/// Returns the public URL of the uploaded file.
pub async fn upload_file(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    mut multipart: axum::extract::Multipart,
) -> Result<Json<Value>, AppError> {
    use std::io::Write;
    use uuid::Uuid;

    let upload_dir = "/var/www/incentiveswift-app/uploads";
    let public_url_base = "https://app.incentiveswift.com/uploads";

    let mut file_urls = Vec::new();

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::BadRequest(format!("Multipart error: {}", e)))?
    {
        let file_name = field.file_name().unwrap_or("unknown").to_string();
        let content_type = field
            .content_type()
            .unwrap_or("application/octet-stream")
            .to_string();
        let data = field
            .bytes()
            .await
            .map_err(|e| AppError::BadRequest(format!("Failed to read file: {}", e)))?;

        // Validate file size (max 10MB)
        let max_size: usize = 10 * 1024 * 1024;
        if data.len() > max_size {
            return Err(AppError::BadRequest(format!(
                "File '{}' exceeds maximum size of 10MB",
                file_name
            )));
        }

        // Generate unique filename
        let ext = std::path::Path::new(&file_name)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("bin");
        let stored_name = format!(
            "{}-{}.{}",
            Uuid::new_v4(),
            slugify(file_name.trim_end_matches(&format!(".{}", ext))),
            ext
        );
        let file_path = format!("{}/{}", upload_dir, stored_name);
        let public_url = format!("{}/{}", public_url_base, stored_name);

        // Write file
        let mut f = std::fs::File::create(&file_path)
            .map_err(|e| AppError::Internal(format!("Failed to save file: {}", e)))?;
        f.write_all(&data)
            .map_err(|e| AppError::Internal(format!("Failed to write file: {}", e)))?;

        file_urls.push(json!({
            "original_name": file_name,
            "content_type": content_type,
            "size": data.len(),
            "url": public_url,
        }));
    }

    Ok(Json(json!({
        "data": if file_urls.len() == 1 { file_urls.into_iter().next().unwrap() } else { json!(file_urls) }
    })))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn verify_funnel_ownership(
    db: &sqlx::PgPool,
    account_id: &str,
    funnel_id: Uuid,
) -> Result<(), AppError> {
    let exists: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM iqs_funnels WHERE id = $1 AND account_id = $2::uuid")
            .bind(funnel_id)
            .bind(account_id)
            .fetch_optional(db)
            .await?;

    if exists.is_none() {
        return Err(AppError::NotFound("Funnel not found".into()));
    }

    Ok(())
}

fn slugify(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}
