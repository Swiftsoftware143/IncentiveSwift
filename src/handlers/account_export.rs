//! `GET /api/v1/account/export` — the business's OWN copy of its data.
//!
//! WHY THIS EXISTS
//!
//! Every other door in this app hands the business one screen's worth of rows (a member list, an
//! analytics CSV). A business that wants to leave, move or audit its own account had no way to take
//! its data with it. This is the whole-account door, and it is scoped by the SESSION's claim — there
//! is no id in the path to spoof, so one business can never read another's rows through it. The
//! siblings on this box ship the same door (FunnelSwift, ADASwift, WorkflowSwift, MissedCall); this
//! app was the one missing it.
//!
//! SHAPE
//!
//! One JSON document, one key per collection (`account`, `team`, `loyalty_programs`, …). Postgres
//! builds each collection with `json_agg` so column types never have to be re-declared in Rust, and
//! the columns are listed EXPLICITLY per table: a `SELECT *` here would leak `accounts.password_hash`,
//! `accounts.purchase_pin`, the hashed/sealed API-key columns and a provider credential into a
//! downloadable file. Those classes are deliberately absent and the payload says so in `excluded`.
//!
//! SCOPING (two shapes, both anchored on the session's account)
//!
//!   * `account_id` tables (campaigns, loyalty_programs, tags, credit_transactions, contact links)
//!     filter on `account_id = $1`.
//!   * `tenant_id` tables (users, workspaces, offers, reviews, support_tickets, api_keys) filter on
//!     `tenant_id = (SELECT tenant_id FROM accounts WHERE id = $1)` — the same parent the rest of the
//!     app reads. An account with no tenant gets an empty list, never somebody else's rows.
//!
//! The download is bounded by nothing but the account's own size — it is the business's data, and a
//! truncated copy would be a lie. It reads only; nothing is written.

use axum::{
    extract::State,
    http::header::{self, HeaderValue},
    response::{IntoResponse, Response},
};
use serde_json::{json, Map, Value};
use uuid::Uuid;

use crate::error::AppError;
use crate::security::auth::AuthenticatedUser;
use crate::state::AppState;

/// `(key in the document, SQL returning a JSON array of the caller's rows)`.
///
/// Each query is wrapped as `SELECT coalesce(json_agg(x), '[]'::json)::text FROM (<inner>) x`, so
/// the column list lives in ONE place per table and never needs a Rust mirror type. `$1` is always
/// the caller's account id.
const COLLECTIONS: &[(&str, &str)] = &[
    (
        "account",
        "SELECT coalesce(json_agg(x), '[]'::json)::text FROM (
             SELECT id, name, email, slug, company, username, role, credits_balance,
                    credits_lifetime_used, credit_rate, loyalty_plan, loyalty_plan_status,
                    redemption_cap_pct, min_redemption_credits, created_at, updated_at,
                    last_login_at, closed_at
               FROM accounts WHERE id = $1) x",
    ),
    (
        "team",
        "SELECT coalesce(json_agg(x), '[]'::json)::text FROM (
             SELECT id, email, name, role, is_active, created_at, updated_at
               FROM users
              WHERE tenant_id = (SELECT tenant_id FROM accounts WHERE id = $1)) x",
    ),
    (
        "workspaces",
        "SELECT coalesce(json_agg(x), '[]'::json)::text FROM (
             SELECT id, name, is_default, created_at
               FROM workspaces
              WHERE tenant_id = (SELECT tenant_id FROM accounts WHERE id = $1)) x",
    ),
    (
        "loyalty_programs",
        "SELECT coalesce(json_agg(x), '[]'::json)::text FROM (
             SELECT id, campaign_id, name, slug, description, recognition_method,
                    points_per_checkin, max_checkins_per_day, point_decay_days, points_expire_days,
                    is_active, tiers_enabled, tier_type, milestones_enabled, streak_enabled,
                    streak_bonus, streak_days, referral_bonus, birthday_bonus, social_share_points,
                    points_per_visit, points_per_secret_code, purchase_credit_rate, scan_points,
                    referral_points, max_redeem_percent, currency_name, currency_icon,
                    currency_color, created_at
               FROM loyalty_programs WHERE account_id = $1) x",
    ),
    (
        "campaigns",
        "SELECT coalesce(json_agg(x), '[]'::json)::text FROM (
             SELECT id, name, slug, type, status, tag_namespace, outcome_tags, delivery_method,
                    loyalty_points_per_play, auto_enroll_loyalty, created_at
               FROM campaigns WHERE account_id = $1) x",
    ),
    (
        "members",
        "SELECT coalesce(json_agg(x), '[]'::json)::text FROM (
             SELECT c.id, c.first_name, c.last_name, c.email, c.phone, c.business_name,
                    c.website, c.total_entries, c.notes, c.first_seen_at, c.last_seen_at,
                    c.created_at, ct.first_linked_at AS linked_at, ct.source AS linked_source
               FROM contacts c
               JOIN contact_tenants ct ON ct.contact_id = c.id
              WHERE ct.account_id = $1) x",
    ),
    (
        "entries",
        "SELECT coalesce(json_agg(x), '[]'::json)::text FROM (
             SELECT e.id, e.contact_id, e.campaign_id, e.answers, e.score, e.outcome,
                    e.tags_applied, e.delivered, e.delivered_at, e.utm_source, e.utm_medium,
                    e.utm_campaign, e.created_at
               FROM entries e
               JOIN campaigns cm ON cm.id = e.campaign_id
              WHERE cm.account_id = $1) x",
    ),
    (
        "tags",
        "SELECT coalesce(json_agg(x), '[]'::json)::text FROM (
             SELECT id, name, color, created_at, updated_at
               FROM tags WHERE account_id = $1) x",
    ),
    (
        "offers",
        "SELECT coalesce(json_agg(x), '[]'::json)::text FROM (
             SELECT id, name, discount_percent, cap_dollars, active, created_at, updated_at
               FROM offers
              WHERE tenant_id = (SELECT tenant_id FROM accounts WHERE id = $1)) x",
    ),
    (
        "credit_transactions",
        "SELECT coalesce(json_agg(x), '[]'::json)::text FROM (
             SELECT id, amount, balance_after, action, reference_type, reference_id, description,
                    created_at
               FROM credit_transactions WHERE account_id = $1) x",
    ),
    (
        "reviews",
        "SELECT coalesce(json_agg(x), '[]'::json)::text FROM (
             SELECT id, campaign_id, contact_id, rating, title, body, reviewer_name, status,
                    created_at, updated_at
               FROM reviews
              WHERE tenant_id = (SELECT tenant_id FROM accounts WHERE id = $1)) x",
    ),
    (
        "support_tickets",
        "SELECT coalesce(json_agg(x), '[]'::json)::text FROM (
             SELECT id, campaign_id, contact_id, subject, description, status, priority, category,
                    created_at, updated_at, resolved_at, source
               FROM support_tickets
              WHERE tenant_id = (SELECT tenant_id FROM accounts WHERE id = $1)) x",
    ),
    (
        "api_keys",
        "SELECT coalesce(json_agg(x), '[]'::json)::text FROM (
             SELECT id, name, prefix, service_type, is_active, last_used_at, expires_at,
                    created_at
               FROM api_keys
              WHERE tenant_id = (SELECT tenant_id FROM accounts WHERE id = $1)) x",
    ),
];

/// The one thing a business must be told about its own copy: what is NOT in it, and why.
const EXCLUDED: &str = "Nothing that could be replayed to sign in as somebody is included: stored \
     password hashes, the scan PIN, API-key hashes and sealed key copies, mail-server passwords, \
     provider credentials and uploaded logo files are never exported. Ask support for a copy of \
     those if you need them.";

/// `GET /api/v1/account/export`
pub async fn export_account(
    auth: AuthenticatedUser,
    State(state): State<AppState>,
) -> Result<Response, AppError> {
    let account_id: Uuid = Uuid::parse_str(&auth.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account".into()))?;

    let mut out = Map::new();
    let mut counts = Map::new();

    for (key, sql) in COLLECTIONS {
        let raw: String = sqlx::query_scalar::<_, String>(sql)
            .bind(account_id)
            .fetch_one(&state.db)
            .await
            .map_err(|e| AppError::Internal(format!("export query `{key}` failed: {e}")))?;
        let rows: Value = serde_json::from_str(&raw).unwrap_or_else(|_| json!([]));
        counts.insert(
            (*key).to_string(),
            json!(rows.as_array().map(|a| a.len()).unwrap_or(0)),
        );
        out.insert((*key).to_string(), rows);
    }

    out.insert("counts".to_string(), Value::Object(counts));
    out.insert("excluded".to_string(), json!(EXCLUDED));
    out.insert(
        "exported_at".to_string(),
        json!(chrono::Utc::now().to_rfc3339()),
    );
    out.insert(
        "format".to_string(),
        json!("incentiveswift-account-export/1"),
    );

    let body = serde_json::to_string_pretty(&Value::Object(out))
        .map_err(|e| AppError::Internal(format!("export encode error: {e}")))?;

    let filename = format!(
        "incentiveswift-account-{}.json",
        chrono::Utc::now().format("%Y%m%d-%H%M%S")
    );
    let disposition = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
        .map_err(|e| AppError::Internal(format!("export header error: {e}")))?;

    Ok((
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            ),
            (header::CONTENT_DISPOSITION, disposition),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
        ],
        body,
    )
        .into_response())
}
