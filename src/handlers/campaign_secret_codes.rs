use crate::security::auth::AuthenticatedUser;
use crate::state::AppState;
use axum::{
    extract::{Path, State},
    http::HeaderMap,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};
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
    /// Operator-shaped call (admin console panel 16) hands over the contact it picked.
    ///
    /// A participant surface cannot: `/play/<slug>` has no account and `GET /api/v1/play/<slug>`
    /// returns no contact id, so the customer identifies itself with email/phone + names exactly
    /// like `SpinRequestBody` (handlers/spin_handler.rs:47) and the loyalty sibling
    /// (`secret_codes_handler::verify_secret_code`, which builds a ContactInput) do. `contact_id`
    /// stays optional so panel 16's existing `{code, contact_id}` call is unchanged.
    #[serde(default)]
    pub contact_id: Option<Uuid>,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub first_name: Option<String>,
    pub last_name: Option<String>,
}

fn ok(d: Value) -> Json<Value> {
    Json(json!({"data": d, "error": null}))
}

// ---------------------------------------------------------------------------
// Public guessing guard for the redeem path
// ---------------------------------------------------------------------------
//
// `POST /api/v1/campaigns/:campaign_id/redeem-code` is PUBLIC (no AuthenticatedUser extractor;
// docs/inline-api-reference.md lists it `Auth: None`), and the code box on `/play/<slug>` is what
// makes wrong guesses reachable from the internet. Nothing else on this route bounds them:
// is_active / expires_at / max_uses only describe a VALID code, and the UNIQUE
// (secret_code_id, contact_id) row only stops ONE contact redeeming ONE code twice.
//
// So failed code lookups are counted per network origin in a rolling window, and every attempt
// from an origin that has burned the window is refused with 429 BEFORE any lookup -- that is the
// point of a brute-force guard (an attacker must not even get the DB read for free).
//
// Key: nginx's own `X-Real-IP` ($remote_addr) -- server-set, so it cannot be spoofed by the
// client; `X-Forwarded-For`'s LAST entry is the same value and is the fallback. In production
// the direct peer is the Cloudflare edge, so this is a ceiling on the network origin the request
// arrived from, not per-visitor policing: an individual customer typing one wrong code is never
// affected, and a window is at most 5 minutes wide.
//
// In-process state on purpose: this app runs ONE container (measured), the counter is a
// signup-flow guard, and a process restart clearing it is the right failure mode.
const REDEEM_FAILURE_LIMIT: u32 = 12;
const REDEEM_WINDOW: Duration = Duration::from_secs(300);

struct FailureWindow {
    started: Instant,
    failures: u32,
}

static REDEEM_FAILURES: LazyLock<Mutex<HashMap<String, FailureWindow>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The network origin a redeem attempt came from. Server-set headers only.
fn peer_key(headers: &HeaderMap) -> String {
    if let Some(ip) = headers.get("x-real-ip").and_then(|v| v.to_str().ok()) {
        let ip = ip.trim();
        if !ip.is_empty() {
            return ip.to_string();
        }
    }
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.rsplit(',').next())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

fn lock_failures() -> std::sync::MutexGuard<'static, HashMap<String, FailureWindow>> {
    // A poisoned mutex would otherwise panic the public route; the map is a best-effort counter.
    REDEEM_FAILURES.lock().unwrap_or_else(|e| e.into_inner())
}

/// True when this origin has already burned its window: refuse without touching the DB.
fn redeem_throttled(key: &str) -> bool {
    let now = Instant::now();
    let map = lock_failures();
    map.get(key)
        .map(|w| {
            now.duration_since(w.started) < REDEEM_WINDOW && w.failures >= REDEEM_FAILURE_LIMIT
        })
        .unwrap_or(false)
}

/// Record one WRONG code (a valid code, or a code already redeemed by this contact, is not a
/// guess and is not counted).
fn note_redeem_failure(key: &str) {
    let now = Instant::now();
    let mut map = lock_failures();
    if map.len() > 4096 {
        map.retain(|_, w| now.duration_since(w.started) < REDEEM_WINDOW);
    }
    let w = map.entry(key.to_string()).or_insert(FailureWindow {
        started: now,
        failures: 0,
    });
    if now.duration_since(w.started) >= REDEEM_WINDOW {
        w.started = now;
        w.failures = 0;
    }
    w.failures += 1;
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
    headers: HeaderMap,
    Json(body): Json<RedeemSecretCodeBody>,
) -> Result<Json<Value>, crate::error::AppError> {
    let peer = peer_key(&headers);
    if redeem_throttled(&peer) {
        return Err(crate::error::AppError::TooManyRequests(
            "Too many code attempts from this network — please try again in a few minutes.".into(),
        ));
    }

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
            // A WRONG code is the guess this route has to bound; count it, then answer in the
            // family's own shape so the served code box can render the message.
            note_redeem_failure(&peer);
            return Ok(ok(json!({"success":false,"points_awarded":0,
            "message":"Invalid or expired secret code"})));
        }
    };

    // Who is redeeming? The operator console (panel 16) hands over the contact it picked; a
    // participant surface cannot, so it sends email/phone + optional names and the contact is
    // upserted here -- the same contract as POST /campaigns/<slug>/spin (SpinRequestBody) and the
    // loyalty sibling (secret_codes_handler::verify_secret_code). Resolved only AFTER the code
    // matched, so a wrong code never writes a contacts row (measured: the contacts count does not
    // move on a guess).
    let contact_id = if let Some(cid) = body.contact_id {
        // Verify the id exists, exactly as spin_handler::resolve_contact does.
        crate::db::contacts::get_contact(&app.db, &cid).await?;
        cid
    } else if body.email.is_some() || body.phone.is_some() {
        let input = crate::db::contacts::ContactInput {
            first_name: body.first_name.clone(),
            last_name: body.last_name.clone(),
            email: body.email.clone(),
            phone: body.phone.clone(),
            website: None,
            business_name: None,
        };
        crate::db::contacts::upsert_contact(&app.db, &input).await?
    } else {
        return Err(crate::error::AppError::BadRequest(
            "Either contact_id, or email/phone to identify the customer, is required".into(),
        ));
    };

    // Claim the redemption FIRST, in the same transaction as the award.
    //
    // The UNIQUE (secret_code_id, contact_id) constraint is the real gate: the old
    // COUNT-then-INSERT shape let two simultaneous submits of one code by one contact both pass
    // the count and both award (measured t_958b9880: an 11-point code left points_balance = 22
    // with a single redemption row), and the discarded `let _ =` on the INSERT meant the loser
    // of that race still answered success:true without a row of its own. With the claim inside
    // the transaction, exactly one caller can win it, and only the winner awards.
    let mut tx = app
        .db
        .begin()
        .await
        .map_err(|e| crate::error::AppError::Internal(format!("DB: {}", e)))?;

    let claimed = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO campaign_secret_code_redemptions
         (secret_code_id,contact_id,campaign_id,points_awarded)
         VALUES ($1,$2,$3,$4)
         ON CONFLICT (secret_code_id, contact_id) DO NOTHING
         RETURNING id",
    )
    .bind(sc.id)
    .bind(contact_id)
    .bind(campaign_id)
    .bind(sc.points)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| crate::error::AppError::Internal(format!("DB: {}", e)))?;

    if claimed.is_none() {
        // Dropping the transaction rolls the no-op INSERT back.
        return Ok(ok(json!({"success":false,"points_awarded":0,
            "message":"You have already redeemed this code"})));
    }

    // Award through the shared upsert so points_balance AND lifetime_points stay in sync --
    // db/viral.rs:get_campaign_leaderboard() filters on lifetime_points > 0, so a hand-rolled
    // points_balance-only UPDATE keeps the winner off the leaderboard. (The previous version also
    // read "COALESCE(points,0)" from a column that has never existed.)
    let cur_pts =
        crate::db::viral::upsert_campaign_points(&mut *tx, &campaign_id, &contact_id, sc.points)
            .await
            .map_err(|e| crate::error::AppError::Internal(format!("DB: {}", e)))?;

    sqlx::query("UPDATE campaign_secret_codes SET uses_count = uses_count + 1 WHERE id=$1")
        .bind(sc.id)
        .execute(&mut *tx)
        .await
        .map_err(|e| crate::error::AppError::Internal(format!("DB: {}", e)))?;

    tx.commit()
        .await
        .map_err(|e| crate::error::AppError::Internal(format!("DB: {}", e)))?;

    // Milestones stay best-effort (a reward-side failure must not roll back a points award).
    let _ = crate::mechanics::milestone_engine::check_milestones(
        &app,
        &campaign_id,
        &contact_id,
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
