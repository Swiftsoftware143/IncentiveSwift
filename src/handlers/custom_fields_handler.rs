//! Campaign custom fields — CRUD for per-campaign entry form fields

use crate::error::AppError;
use crate::state::AppState;
use crate::security::auth::AuthenticatedUser;
use axum::{extract::{Path, State}, Json};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct CustomField {
    pub id: Uuid,
    pub campaign_id: Uuid,
    pub field_key: String,
    pub field_label: String,
    pub field_type: String,
    pub sort_order: i32,
    pub required: bool,
    pub options: Vec<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Deserialize)]
pub struct CreateFieldInput {
    pub field_key: String,
    pub field_label: String,
    pub field_type: Option<String>,
    pub sort_order: Option<i32>,
    pub required: Option<bool>,
    pub options: Option<Vec<String>>,
}

#[derive(Deserialize)]
pub struct UpdateFieldInput {
    pub field_label: Option<String>,
    pub field_type: Option<String>,
    pub sort_order: Option<i32>,
    pub required: Option<bool>,
    pub options: Option<Vec<String>>,
}

/// GET /api/v1/campaigns/:slug/custom-fields
pub async fn list_custom_fields(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> Result<Json<Value>, AppError> {
    let campaign = crate::db::campaigns::get_campaign_by_slug(&state.db, &slug).await?;
    let fields = sqlx::query_as::<_, CustomField>(
        r#"SELECT id, campaign_id, field_key, field_label, field_type, sort_order, required, options, created_at
           FROM campaign_custom_fields WHERE campaign_id = $1
           ORDER BY sort_order"#
    )
    .bind(campaign.id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(json!({ "fields": fields })))
}

/// POST /api/v1/campaigns/:slug/custom-fields
pub async fn create_custom_field(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(slug): Path<String>,
    Json(body): Json<CreateFieldInput>,
) -> Result<Json<Value>, AppError> {
    let campaign = crate::db::campaigns::get_campaign_by_slug(&state.db, &slug).await?;
    let id = Uuid::new_v4();
    let field_type = body.field_type.unwrap_or_else(|| "text".to_string());
    let sort_order = body.sort_order.unwrap_or(0);
    let required = body.required.unwrap_or(false);
    let options = body.options.unwrap_or_default();

    sqlx::query(
        r#"INSERT INTO campaign_custom_fields (id, campaign_id, field_key, field_label, field_type, sort_order, required, options)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8)"#
    )
    .bind(id)
    .bind(campaign.id)
    .bind(&body.field_key)
    .bind(&body.field_label)
    .bind(&field_type)
    .bind(sort_order)
    .bind(required)
    .bind(&options)
    .execute(&state.db)
    .await?;

    let field = sqlx::query_as::<_, CustomField>(
        r#"SELECT id, campaign_id, field_key, field_label, field_type, sort_order, required, options, created_at
           FROM campaign_custom_fields WHERE id = $1"#
    )
    .bind(id)
    .fetch_one(&state.db)
    .await?;

    Ok(Json(json!({ "field": field })))
}

/// PUT /api/v1/campaigns/:slug/custom-fields/:field_id
pub async fn update_custom_field(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path((slug, field_id)): Path<(String, String)>,
    Json(body): Json<UpdateFieldInput>,
) -> Result<Json<Value>, AppError> {
    let campaign = crate::db::campaigns::get_campaign_by_slug(&state.db, &slug).await?;
    let fid = Uuid::parse_str(&field_id)
        .map_err(|_| AppError::BadRequest("Invalid field ID".to_string()))?;

    let existing = sqlx::query_as::<_, CustomField>(
        r#"SELECT id, campaign_id, field_key, field_label, field_type, sort_order, required, options, created_at
           FROM campaign_custom_fields WHERE id = $1 AND campaign_id = $2"#
    )
    .bind(fid)
    .bind(campaign.id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Custom field not found".to_string()))?;

    let label = body.field_label.unwrap_or(existing.field_label);
    let ftype = body.field_type.unwrap_or(existing.field_type);
    let sort_order = body.sort_order.unwrap_or(existing.sort_order);
    let required = body.required.unwrap_or(existing.required);
    let options = body.options.unwrap_or(existing.options);

    sqlx::query(
        r#"UPDATE campaign_custom_fields SET field_label = $1, field_type = $2, sort_order = $3, required = $4, options = $5
           WHERE id = $6"#
    )
    .bind(&label)
    .bind(&ftype)
    .bind(sort_order)
    .bind(required)
    .bind(&options)
    .bind(fid)
    .execute(&state.db)
    .await?;

    let field = sqlx::query_as::<_, CustomField>(
        r#"SELECT id, campaign_id, field_key, field_label, field_type, sort_order, required, options, created_at
           FROM campaign_custom_fields WHERE id = $1"#
    )
    .bind(fid)
    .fetch_one(&state.db)
    .await?;

    Ok(Json(json!({ "field": field })))
}

/// DELETE /api/v1/campaigns/:slug/custom-fields/:field_id
pub async fn delete_custom_field(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path((slug, field_id)): Path<(String, String)>,
) -> Result<Json<Value>, AppError> {
    let campaign = crate::db::campaigns::get_campaign_by_slug(&state.db, &slug).await?;
    let fid = Uuid::parse_str(&field_id)
        .map_err(|_| AppError::BadRequest("Invalid field ID".to_string()))?;

    sqlx::query("DELETE FROM campaign_custom_fields WHERE id = $1 AND campaign_id = $2")
        .bind(fid)
        .bind(campaign.id)
        .execute(&state.db)
        .await?;

    Ok(Json(json!({ "status": "deleted" })))
}

/// PUT /api/v1/campaigns/:slug/custom-fields/reorder
pub async fn reorder_custom_fields(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(slug): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, AppError> {
    let campaign = crate::db::campaigns::get_campaign_by_slug(&state.db, &slug).await?;

    if let Some(ids) = body.get("field_ids").and_then(|v| v.as_array()) {
        for (i, id_val) in ids.iter().enumerate() {
            if let Some(id_str) = id_val.as_str() {
                if let Ok(fid) = Uuid::parse_str(id_str) {
                    sqlx::query(
                        "UPDATE campaign_custom_fields SET sort_order = $1 WHERE id = $2 AND campaign_id = $3"
                    )
                    .bind(i as i32)
                    .bind(fid)
                    .bind(campaign.id)
                    .execute(&state.db)
                    .await?;
                }
            }
        }
    }

    Ok(Json(json!({ "status": "reordered" })))
}
