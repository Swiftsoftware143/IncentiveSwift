use crate::security::auth::AuthenticatedUser;
use crate::state::AppState;
use axum::{
    extract::{Path, State},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct CampaignSecretCode {
    pub id: Uuid,
    pub campaign_id: Uuid,
    pub code: String,
    pub points: i32,
    pub max_uses: Option<i32>,
    pub uses_count: i32,
    pub is_active: bool,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Deserialize)]
pub struct CreateSecretCodeBody {
    pub code: String,
    pub points: Option<i32>,
    pub max_uses: Option<i32>,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateSecretCodeBody {
    pub code: Option<String>,
    pub points: Option<i32>,
    pub max_uses: Option<i32>,
    pub is_active: Option<bool>,
    pub expires_at: Option<Option<chrono::DateTime<chrono::Utc>>>,
}

#[derive(Debug, Deserialize)]
pub struct RedeemSecretCodeBody {
    pub code: String,
    pub contact_id: Uuid,
}

fn ok(d: Value) -> Json<Value> {
    Json(json!({"data": d, "error": null}))
}

pub async fn list_secret_codes(
    State(app): State<AppState>,
    user: AuthenticatedUser,
    Path(campaign_id): Path<Uuid>,
) -> Result<Json<Value>, crate::error::AppError> {
    let codes = sqlx::query_as::<_, CampaignSecretCode>(
        "SELECT * FROM campaign_secret_codes WHERE campaign_id = $1 ORDER BY created_at DESC",
    )
    .bind(campaign_id)
    .fetch_all(&app.db)
    .await
    .map_err(|e| crate::error::AppError::Internal(format!("DB: {}", e)))?;
    Ok(ok(json!({"secret_codes": codes})))
}

pub async fn create_secret_code(
    State(app): State<AppState>,
    user: AuthenticatedUser,
    Path(campaign_id): Path<Uuid>,
    Json(body): Json<CreateSecretCodeBody>,
) -> Result<Json<Value>, crate::error::AppError> {
    let code = body.code.trim().to_uppercase();
    let points = body.points.unwrap_or(100);
    match sqlx::query_as::<_, CampaignSecretCode>(
        "INSERT INTO campaign_secret_codes (campaign_id,code,points,max_uses,expires_at)
         VALUES ($1,$2,$3,$4,$5) RETURNING *",
    )
    .bind(campaign_id)
    .bind(&code)
    .bind(points)
    .bind(body.max_uses)
    .bind(body.expires_at)
    .fetch_one(&app.db)
    .await
    {
        Ok(sc) => Ok(ok(json!({"secret_code": sc}))),
        Err(sqlx::Error::Database(ref d))
            if d.constraint() == Some("campaign_secret_codes_code_campaign_id_key") =>
        {
            Err(crate::error::AppError::BadRequest(
                "Code already exists for this campaign".into(),
            ))
        }
        Err(e) => Err(crate::error::AppError::Internal(format!("DB: {}", e))),
    }
}

pub async fn update_secret_code(
    State(app): State<AppState>,
    user: AuthenticatedUser,
    Path((_cid, code_id)): Path<(Uuid, Uuid)>,
    Json(b): Json<UpdateSecretCodeBody>,
) -> Result<Json<Value>, crate::error::AppError> {
    let sc = sqlx::query_as::<_, CampaignSecretCode>(
        "UPDATE campaign_secret_codes SET
         code=COALESCE($1,code),points=COALESCE($2,points),
         max_uses=COALESCE($3,max_uses),is_active=COALESCE($4,is_active),
         expires_at=COALESCE($5,expires_at) WHERE id=$6 RETURNING *",
    )
    .bind(&b.code)
    .bind(b.points)
    .bind(b.max_uses)
    .bind(b.is_active)
    .bind(b.expires_at)
    .bind(code_id)
    .fetch_one(&app.db)
    .await
    .map_err(|e| crate::error::AppError::Internal(format!("DB: {}", e)))?;
    Ok(ok(json!({"secret_code": sc})))
}

pub async fn delete_secret_code(
    State(app): State<AppState>,
    user: AuthenticatedUser,
    Path((_cid, code_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<Value>, crate::error::AppError> {
    sqlx::query("DELETE FROM campaign_secret_codes WHERE id=$1")
        .bind(code_id)
        .execute(&app.db)
        .await
        .map_err(|e| crate::error::AppError::Internal(format!("DB: {}", e)))?;
    Ok(ok(json!({"deleted": true})))
}

pub async fn redeem_secret_code(
    State(app): State<AppState>,
    Path(campaign_id): Path<Uuid>,
    Json(body): Json<RedeemSecretCodeBody>,
) -> Result<Json<Value>, crate::error::AppError> {
    let cu = body.code.trim().to_uppercase();
    let sc = match sqlx::query_as::<_, CampaignSecretCode>(
        "SELECT * FROM campaign_secret_codes WHERE campaign_id=$1 AND code=$2
         AND is_active=true AND (expires_at IS NULL OR expires_at>now())
         AND (max_uses IS NULL OR uses_count<max_uses)",
    )
    .bind(campaign_id)
    .bind(&cu)
    .fetch_optional(&app.db)
    .await
    .map_err(|e| crate::error::AppError::Internal(format!("DB: {}", e)))?
    {
        Some(s) => s,
        None => {
            return Ok(ok(json!({"success":false,"points_awarded":0,
            "message":"Invalid or expired secret code"})))
        }
    };

    let already = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM campaign_secret_code_redemptions
         WHERE secret_code_id=$1 AND contact_id=$2",
    )
    .bind(sc.id)
    .bind(body.contact_id)
    .fetch_one(&app.db)
    .await
    .map_err(|e| crate::error::AppError::Internal(format!("DB: {}", e)))?;

    if already > 0 {
        return Ok(ok(json!({"success":false,"points_awarded":0,
            "message":"You have already redeemed this code"})));
    }

    sqlx::query(
        "INSERT INTO campaign_points_balance (campaign_id,contact_id,points_balance)
         VALUES ($1,$2,$3)
         ON CONFLICT (campaign_id,contact_id)
         DO UPDATE SET points_balance = campaign_points_balance.points_balance + $3",
    )
    .bind(campaign_id)
    .bind(body.contact_id)
    .bind(sc.points)
    .execute(&app.db)
    .await
    .map_err(|e| crate::error::AppError::Internal(format!("DB: {}", e)))?;

    let _ = sqlx::query(
        "INSERT INTO campaign_secret_code_redemptions
         (secret_code_id,contact_id,campaign_id,points_awarded)
         VALUES ($1,$2,$3,$4)",
    )
    .bind(sc.id)
    .bind(body.contact_id)
    .bind(campaign_id)
    .bind(sc.points)
    .execute(&app.db)
    .await;

    let _ = sqlx::query("UPDATE campaign_secret_codes SET uses_count = uses_count + 1 WHERE id=$1")
        .bind(sc.id)
        .execute(&app.db)
        .await;

    let cur_pts = sqlx::query_scalar::<_, i32>(
        "SELECT COALESCE(points,0) FROM campaign_points_balance
         WHERE campaign_id=$1 AND contact_id=$2",
    )
    .bind(campaign_id)
    .bind(body.contact_id)
    .fetch_one(&app.db)
    .await
    .unwrap_or(0);

    let _ = crate::mechanics::milestone_engine::check_milestones(
        &app,
        &campaign_id,
        &body.contact_id,
        cur_pts,
    )
    .await;

    Ok(ok(json!({"success":true,"points_awarded":sc.points,
        "message":format!("You earned {} points!",sc.points)})))
}

pub async fn list_redemptions(
    State(app): State<AppState>,
    user: AuthenticatedUser,
    Path(campaign_id): Path<Uuid>,
) -> Result<Json<Value>, crate::error::AppError> {
    let rows = sqlx::query(
        "SELECT r.id, sc.code AS secret_code, sc.points, r.contact_id,
                r.points_awarded, r.redeemed_at
         FROM campaign_secret_code_redemptions r
         JOIN campaign_secret_codes sc ON r.secret_code_id=sc.id
         WHERE r.campaign_id=$1 ORDER BY r.redeemed_at DESC LIMIT 100",
    )
    .bind(campaign_id)
    .fetch_all(&app.db)
    .await
    .map_err(|e| crate::error::AppError::Internal(format!("DB: {}", e)))?;
    let mut redemptions: Vec<Value> = Vec::with_capacity(rows.len());
    for row in &rows {
        use sqlx::Row;
        redemptions.push(json!({
            "id": row.get::<Uuid,_>("id"),
            "secret_code": row.get::<String,_>("secret_code"),
            "points": row.get::<i32,_>("points"),
            "contact_id": row.get::<Uuid,_>("contact_id"),
            "points_awarded": row.get::<i32,_>("points_awarded"),
            "redeemed_at": row.get::<chrono::DateTime<chrono::Utc>,_>("redeemed_at"),
        }));
    }
    Ok(ok(json!({"redemptions": redemptions})))
}
