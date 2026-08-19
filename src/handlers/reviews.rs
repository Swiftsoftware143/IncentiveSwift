//! Reviews & ratings handlers — CRUD + moderation for reviews table.
use crate::error::AppError;
use crate::security::auth::AuthenticatedUser;
use crate::state::AppState;
use axum::{
    extract::{Path, State},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Review {
    pub id: Uuid,
    pub tenant_id: Option<Uuid>,
    pub campaign_id: Option<Uuid>,
    pub contact_id: Option<Uuid>,
    pub rating: i32,
    pub title: Option<String>,
    pub body: Option<String>,
    pub reviewer_name: Option<String>,
    pub status: String,
    pub moderation_note: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Deserialize)]
pub struct CreateReviewInput {
    pub campaign_id: Option<Uuid>,
    pub contact_id: Option<Uuid>,
    pub rating: i32,
    pub title: Option<String>,
    pub body: Option<String>,
    pub reviewer_name: Option<String>,
    pub status: Option<String>,
}

#[derive(Deserialize)]
pub struct UpdateReviewInput {
    pub status: Option<String>,
    pub moderation_note: Option<String>,
    pub title: Option<String>,
    pub body: Option<String>,
    pub rating: Option<i32>,
}

const REVIEW_COLS: &str = "id, tenant_id, campaign_id, contact_id, rating, title, body, reviewer_name, status, moderation_note, created_at, updated_at";

/// GET /api/v1/reviews
pub async fn list_reviews(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let account = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;
    let rows = sqlx::query_as::<_, Review>(&format!(
        "SELECT {REVIEW_COLS} FROM reviews WHERE tenant_id = $1 ORDER BY created_at DESC"
    ))
    .bind(account)
    .fetch_all(&state.db)
    .await?;

    // compute aggregate
    let agg = sqlx::query_as::<_, (i64, f64)>(
        "SELECT COUNT(*), COALESCE(AVG(rating),0)::float8 FROM reviews WHERE tenant_id = $1 AND status = 'approved'",
    )
    .bind(account)
    .fetch_one(&state.db)
    .await?;

    Ok(Json(
        json!({ "reviews": rows, "count": agg.0, "average_rating": agg.1 }),
    ))
}

/// POST /api/v1/reviews
pub async fn create_review(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<CreateReviewInput>,
) -> Result<Json<Value>, AppError> {
    let account = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;
    if !(1..=5).contains(&body.rating) {
        return Err(AppError::BadRequest(
            "Rating must be between 1 and 5".to_string(),
        ));
    }
    let id = Uuid::new_v4();
    let status = body.status.unwrap_or_else(|| "pending".to_string());
    sqlx::query(
        r#"INSERT INTO reviews
             (id, tenant_id, campaign_id, contact_id, rating, title, body, reviewer_name, status)
           VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)"#,
    )
    .bind(id)
    .bind(account)
    .bind(body.campaign_id)
    .bind(body.contact_id)
    .bind(body.rating)
    .bind(&body.title)
    .bind(&body.body)
    .bind(&body.reviewer_name)
    .bind(&status)
    .execute(&state.db)
    .await?;
    let row =
        sqlx::query_as::<_, Review>(&format!("SELECT {REVIEW_COLS} FROM reviews WHERE id = $1"))
            .bind(id)
            .fetch_one(&state.db)
            .await?;
    Ok(Json(json!({ "review": row })))
}

/// PUT /api/v1/reviews/:id — update + moderation
pub async fn update_review(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateReviewInput>,
) -> Result<Json<Value>, AppError> {
    let account = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;
    let exists =
        sqlx::query_scalar::<_, Uuid>("SELECT id FROM reviews WHERE id = $1 AND tenant_id = $2")
            .bind(id)
            .bind(account)
            .fetch_optional(&state.db)
            .await?
            .ok_or_else(|| AppError::NotFound("Review not found".to_string()))?;
    let _ = exists;

    sqlx::query(
        r#"UPDATE reviews SET
             status = COALESCE($2, status),
             moderation_note = COALESCE($3, moderation_note),
             title = COALESCE($4, title),
             body = COALESCE($5, body),
             rating = COALESCE($6, rating),
             updated_at = now()
           WHERE id = $1"#,
    )
    .bind(id)
    .bind(&body.status)
    .bind(&body.moderation_note)
    .bind(&body.title)
    .bind(&body.body)
    .bind(body.rating)
    .execute(&state.db)
    .await?;
    let row =
        sqlx::query_as::<_, Review>(&format!("SELECT {REVIEW_COLS} FROM reviews WHERE id = $1"))
            .bind(id)
            .fetch_one(&state.db)
            .await?;
    Ok(Json(json!({ "review": row })))
}

/// DELETE /api/v1/reviews/:id
pub async fn delete_review(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, AppError> {
    let account = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;
    sqlx::query("DELETE FROM reviews WHERE id = $1 AND tenant_id = $2")
        .bind(id)
        .bind(account)
        .execute(&state.db)
        .await?;
    Ok(Json(json!({ "deleted": true })))
}
