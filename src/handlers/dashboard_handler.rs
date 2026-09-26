//! Dashboard handler — aggregate stats for the authenticated user's campaigns.

use crate::error::AppError;
use crate::security::auth::AuthenticatedUser;
use crate::state::AppState;
use axum::{extract::State, Json};
use serde_json::{json, Value};
use uuid::Uuid;

/// GET /api/v1/dashboard/stats
/// Returns aggregate counts for the authenticated account's tenant.
pub async fn dashboard_stats(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;

    // Count campaigns for this tenant
    let total_campaigns: i64 = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT COUNT(*) FROM campaigns c
         WHERE c.account_id = $1",
    )
    .bind(account_id)
    .fetch_one(&state.db)
    .await?
    .unwrap_or(0);

    // Count contacts/participants for this tenant
    let total_participants: i64 = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT COUNT(DISTINCT e.contact_id) FROM entries e
         JOIN campaigns c ON c.id = e.campaign_id
         WHERE c.account_id = $1",
    )
    .bind(account_id)
    .fetch_one(&state.db)
    .await?
    .unwrap_or(0);

    // Count total entries (interactions)
    let total_entries: i64 = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT COUNT(*) FROM entries e
         JOIN campaigns c ON c.id = e.campaign_id
         WHERE c.account_id = $1",
    )
    .bind(account_id)
    .fetch_one(&state.db)
    .await?
    .unwrap_or(0);

    Ok(Json(json!({
        "total_campaigns": total_campaigns,
        "total_rewards": 0,
        "total_participants": total_participants,
        "total_points": 0,
        "total_entries": total_entries,
    })))
}

/// GET /api/v1/dashboard/activity
/// Returns recent activity feed for the authenticated user's campaigns.
pub async fn dashboard_activity(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;

    let rows = sqlx::query(
        r#"SELECT e.id, COALESCE(c.name, 'Unknown') as campaign_name,
                  COALESCE(ct.email, ct.phone, 'Anonymous') as participant,
                  e.created_at
           FROM entries e
           JOIN campaigns c ON c.id = e.campaign_id
           LEFT JOIN contacts ct ON ct.id = e.contact_id
           WHERE c.account_id = $1 ORDER BY e.created_at DESC LIMIT 20"#,
    )
    .bind(account_id)
    .fetch_all(&state.db)
    .await?;

    let mut activities: Vec<Value> = Vec::new();
    for row in &rows {
        use sqlx::Row;
        let id: Uuid = row.get("id");
        let campaign: String = row.get("campaign_name");
        let participant: String = row.get("participant");
        // `entries.created_at` is TIMESTAMPTZ (`information_schema` says so). Asking sqlx for a
        // `NaiveDateTime` (SQL type TIMESTAMP) is not a decode it can do, and `row.get` PANICS:
        // `mismatched types; Rust type chrono::naive::datetime::NaiveDateTime (as SQL type TIMESTAMP)
        // is not compatible with SQL type TIMESTAMPTZ` — measured this route live on the deployed
        // binary, where it killed the whole API process and restarted it (kanban t_04553fa6). The
        // crate's working precedent is `DateTime<Utc>` (offers_handler, campaign_integrations, …).
        let created_at: chrono::DateTime<chrono::Utc> = row.get("created_at");
        activities.push(json!({
            "id": id,
            "type": "entry",
            "campaign": campaign,
            "participant": participant,
            "created_at": created_at,
            "action": format!("New entry in {}", campaign),
        }));
    }

    Ok(Json(json!({ "activities": activities })))
}

/// GET /api/v1/leads - list all entries as leads
pub async fn list_leads(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;
    let rows = sqlx::query(
        r#"SELECT e.id, ct.email, ct.phone, ct.first_name, ct.last_name, c.name as campaign_name, e.created_at
           FROM entries e JOIN campaigns c ON c.id = e.campaign_id
           LEFT JOIN contacts ct ON ct.id = e.contact_id
           WHERE c.account_id = $1 ORDER BY e.created_at DESC LIMIT 100"#,
    ).bind(account_id).fetch_all(&state.db).await?;
    let mut leads: Vec<Value> = Vec::new();
    for row in &rows {
        use sqlx::Row;
        leads.push(json!({
            "id": row.get::<Uuid,_>("id"),
            "email": row.get::<Option<String>,_>("email"),
            "phone": row.get::<Option<String>,_>("phone"),
            "first_name": row.get::<Option<String>,_>("first_name"),
            "last_name": row.get::<Option<String>,_>("last_name"),
            "campaign": row.get::<String,_>("campaign_name"),
            // TIMESTAMPTZ, not TIMESTAMP — same decode defect as `dashboard_activity` above: a
            // `NaiveDateTime` here PANICS the worker (measured live: it restarted the API process,
            // kanban t_04553fa6). The admin console's Leads view renders this route
            // (www-admin/index.html:1086-1108), so the panic was a console-visible outage.
            "created_at": row.get::<chrono::DateTime<chrono::Utc>,_>("created_at"),
        }));
    }
    Ok(Json(json!({"leads": leads})))
}

/// GET /api/v1/tags - list all tags for tenant
pub async fn list_tags(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let account_id = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;
    let rows = sqlx::query(
        r#"SELECT t.id, t.name, t.color, tg.name as group_name FROM tags t
           LEFT JOIN tag_groups tg ON tg.id = t.group_id
           WHERE t.account_id = $1 ORDER BY tg.name, t.name"#,
    )
    .bind(account_id)
    .fetch_all(&state.db)
    .await?;
    let mut tags: Vec<Value> = Vec::new();
    for row in &rows {
        use sqlx::Row;
        tags.push(json!({
            "id": row.get::<Uuid,_>("id"),
            "name": row.get::<String,_>("name"),
            "color": row.get::<Option<String>,_>("color"),
            "group": row.get::<Option<String>,_>("group_name"),
        }));
    }
    Ok(Json(json!({"tags": tags})))
}

/// GET /api/v1/plans (public - no auth required)
pub async fn list_public_plans(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let rows = sqlx::query(
        r#"SELECT id, name, slug, price::int as monthly_price, 0 as monthly_zc_pool, NULL::text as description, features, NULL::text as how_it_works
           FROM plans ORDER BY name"#,
    ).fetch_all(&state.db).await?;
    let mut plans: Vec<Value> = Vec::new();
    for row in &rows {
        use sqlx::Row;
        plans.push(json!({
            "id": row.get::<Uuid,_>("id"),
            "name": row.get::<String,_>("name"),
            "slug": row.get::<String,_>("slug"),
            "monthly_price": row.get::<i32,_>("monthly_price"),
            "monthly_zc_pool": row.get::<i32,_>("monthly_zc_pool"),
            "description": row.get::<Option<String>,_>("description"),
            "features": row.get::<Option<serde_json::Value>,_>("features"),
            "how_it_works": row.get::<Option<String>,_>("how_it_works"),
        }));
    }
    Ok(Json(json!({"plans": plans})))
}

#[cfg(test)]
mod timestamp_decode_tests {
    //! RED/GREEN guard for the TIMESTAMPTZ-as-NaiveDateTime decode that PANICKED two live routes
    //! (kanban t_04553fa6), driven against the real schema.
    //!
    //! Measured on the deployed binary: `GET /api/v1/leads` (the route the admin console's Leads view
    //! renders) and `GET /api/v1/dashboard/activity` each killed the API process —
    //!
    //! ```text
    //! thread 'tokio-rt-worker' panicked at src/handlers/dashboard_handler.rs:125:31:
    //! called `Result::unwrap()` on an `Err` value: ColumnDecode { index: "\"created_at\"",
    //! source: "mismatched types; Rust type `chrono::naive::datetime::NaiveDateTime` (as SQL type
    //! `TIMESTAMP`) is not compatible with SQL type `TIMESTAMPTZ`" }
    //! ```
    //!
    //! `entries.created_at` is TIMESTAMPTZ, so the decode must be `DateTime<Utc>` (the crate's own
    //! precedent). This test executes both routes' exact statements, decodes `created_at` the way the
    //! handlers do, and pins the column's type — so the type and the decode cannot drift apart again
    //! without this failing.
    //!
    //! DB-backed and read-only, OPT-IN on the same two variables as `features::usage_arm_tests`:
    //! `INC_ARM_DB_TEST=1` and `DATABASE_URL`.
    use sqlx::{PgPool, Row};
    use uuid::Uuid;

    async fn pool_or_skip() -> Option<PgPool> {
        if std::env::var("INC_ARM_DB_TEST").as_deref() != Ok("1") {
            return None;
        }
        let url = std::env::var("DATABASE_URL").ok()?;
        Some(
            PgPool::connect(&url)
                .await
                .expect("connect to DATABASE_URL"),
        )
    }

    #[tokio::test]
    async fn leads_and_activity_rows_decode_their_timestamptz() {
        let Some(pool) = pool_or_skip().await else {
            return;
        };

        // The column must be TIMESTAMPTZ. A `NaiveDateTime` decode of it is not a slow path — it is
        // the panic above, for every row, on every account.
        let column_type: String = sqlx::query_scalar(
            "SELECT data_type FROM information_schema.columns
             WHERE table_name = 'entries' AND column_name = 'created_at'",
        )
        .fetch_one(&pool)
        .await
        .expect("entries.created_at must exist");
        assert_eq!(
            column_type, "timestamp with time zone",
            "entries.created_at changed type; revisit the decode in dashboard_activity / list_leads"
        );

        let acct: Option<Uuid> = sqlx::query_scalar(
            "SELECT c.account_id FROM entries e JOIN campaigns c ON c.id = e.campaign_id LIMIT 1",
        )
        .fetch_optional(&pool)
        .await
        .expect("pick an account with an entry");
        let Some(acct) = acct else {
            eprintln!("SKIP: no entry row in the database to decode");
            return;
        };

        // list_leads' own statement (`GET /api/v1/leads`).
        let rows = sqlx::query(
            r#"SELECT e.id, ct.email, ct.phone, ct.first_name, ct.last_name, c.name as campaign_name, e.created_at
               FROM entries e JOIN campaigns c ON c.id = e.campaign_id
               LEFT JOIN contacts ct ON ct.id = e.contact_id
               WHERE c.account_id = $1 ORDER BY e.created_at DESC LIMIT 100"#,
        )
        .bind(acct)
        .fetch_all(&pool)
        .await
        .expect("list_leads statement");
        assert!(!rows.is_empty(), "the chosen account owns >= 1 entry");
        let decoded: chrono::DateTime<chrono::Utc> = rows[0].get("created_at");
        println!(
            "list_leads decoded created_at = {decoded} ({} row(s))",
            rows.len()
        );

        // dashboard_activity's own statement (`GET /api/v1/dashboard/activity`).
        let rows = sqlx::query(
            r#"SELECT e.id, COALESCE(c.name, 'Unknown') as campaign_name,
                      COALESCE(ct.email, ct.phone, 'Anonymous') as participant,
                      e.created_at
               FROM entries e
               JOIN campaigns c ON c.id = e.campaign_id
               LEFT JOIN contacts ct ON ct.id = e.contact_id
               WHERE c.account_id = $1 ORDER BY e.created_at DESC LIMIT 20"#,
        )
        .bind(acct)
        .fetch_all(&pool)
        .await
        .expect("dashboard_activity statement");
        assert!(!rows.is_empty());
        let decoded: chrono::DateTime<chrono::Utc> = rows[0].get("created_at");
        println!(
            "dashboard_activity decoded created_at = {decoded} ({} row(s))",
            rows.len()
        );
    }
}
