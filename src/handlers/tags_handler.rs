//! Tags handlers — the tenant's own tag library (`tags` + `tag_groups`).
//!
//! WHY THIS MODULE EXISTS (kanban t_286aead1): until this change the whole crate held exactly ONE
//! statement against `tags` — the reader `dashboard_handler::list_tags` — and none at all against
//! `tag_groups`. Yet the served Operator Console has shipped a `Tags` screen all along
//! (`www-admin/index.html`, nav id `tags`: view `Tags` + `TagModal`, lines 1096-1150) that calls
//! `GET /tags`, `POST /tags`, `PUT /tags/:id` and `DELETE /tags/:id`. Measured on the deployed
//! binary before this file: GET 200, POST 405, PUT 404, DELETE 404 — a live screen whose three write
//! verbs did not exist. So this is the PRODUCER for a shipped surface, not a new feature: same
//! screen, same payload shape (`{name, color, group}`), same list shape (`{tags:[{id,name,color,group}]}`).
//!
//! Two rules every statement here follows:
//!
//!   * TENANT SCOPE comes from the JWT (`AuthenticatedUser::account_id`), never from the body, and
//!     an id that belongs to another account is a 404 — so the response cannot be used to probe
//!     whose tag an id is.
//!   * The allowance is the canonical one: `features::enforce_feature_limit` for the key `max_tags`,
//!     i.e. `tier_features.limit_value` on the account's OWN `plan_tiers` row
//!     (`accounts.plan_tier_id`), seated by `migrations/20260927_max_tags_entitlement.sql`. It is
//!     NOT read from `plans.max_tags`, which has no reader and no writer in this crate.

use crate::error::AppError;
use crate::features;
use crate::security::auth::AuthenticatedUser;
use crate::state::AppState;
use axum::{
    extract::{Path, State},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

/// `tags.name` and `tag_groups.name` are both `VARCHAR(255)`.
const MAX_NAME_LEN: usize = 255;
/// The column default of `tags.color` — what an empty or absent colour means.
const DEFAULT_COLOR: &str = "#6366f1";

/// Request body of both write verbs. The served console's modal sends exactly these three keys
/// (`TagModal`, `www-admin/index.html:1140`: `{name, color, group}`), where `group` is the group's
/// NAME (that is also how the list route reports it).
#[derive(Deserialize)]
pub struct TagBody {
    pub name: Option<String>,
    pub color: Option<String>,
    pub group: Option<String>,
}

fn account_uuid(user: &AuthenticatedUser) -> Result<Uuid, AppError> {
    Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))
}

fn parse_tag_id(id: &str) -> Result<Uuid, AppError> {
    Uuid::parse_str(id).map_err(|_| AppError::BadRequest("Invalid tag ID".to_string()))
}

/// A tag name is required and must fit the column. Rejecting it here is the difference between the
/// app's own message and a raw Postgres error surfacing as `500 Database error`.
fn normalize_name(name: Option<&str>) -> Result<String, AppError> {
    let name = name.unwrap_or("").trim();
    if name.is_empty() {
        return Err(AppError::BadRequest("Tag name is required".to_string()));
    }
    if name.chars().count() > MAX_NAME_LEN {
        return Err(AppError::BadRequest(format!(
            "Tag name must be {MAX_NAME_LEN} characters or fewer"
        )));
    }
    Ok(name.to_string())
}

/// `color` is `VARCHAR(7)` and the console's input is `type=color`, so anything that is not
/// `#rrggbb` is a caller error. Empty/absent means "the column default".
fn normalize_color(color: Option<&str>) -> Result<String, AppError> {
    let color = color.unwrap_or("").trim();
    if color.is_empty() {
        return Ok(DEFAULT_COLOR.to_string());
    }
    let hex = color.strip_prefix('#').unwrap_or("");
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(AppError::BadRequest(
            "Tag color must be a hex value like #6366f1".to_string(),
        ));
    }
    Ok(format!("#{}", hex.to_ascii_lowercase()))
}

/// The account's tag with this name, case-insensitively — the idempotency key, which exists as a
/// UNIQUE expression index (`tags_account_lower_name_uidx`).
async fn find_tag_id(
    state: &AppState,
    account_id: Uuid,
    name: &str,
) -> Result<Option<Uuid>, AppError> {
    let id: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM tags WHERE account_id = $1 AND lower(name) = lower($2)")
            .bind(account_id)
            .bind(name)
            .fetch_optional(&state.db)
            .await?;
    Ok(id)
}

/// Find-or-create the account's tag group by name — `tag_groups` had NO writer at all before this,
/// and the console posts the group as a NAME, so this is the only way its Group field can mean
/// anything. An empty or absent name is `None` = "no group".
async fn resolve_group(
    state: &AppState,
    account_id: Uuid,
    name: Option<&str>,
) -> Result<Option<Uuid>, AppError> {
    let Some(name) = name.map(str::trim).filter(|n| !n.is_empty()) else {
        return Ok(None);
    };
    if name.chars().count() > MAX_NAME_LEN {
        return Err(AppError::BadRequest(format!(
            "Tag group name must be {MAX_NAME_LEN} characters or fewer"
        )));
    }

    let existing: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM tag_groups WHERE account_id = $1 AND lower(name) = lower($2)",
    )
    .bind(account_id)
    .bind(name)
    .fetch_optional(&state.db)
    .await?;
    if let Some(id) = existing {
        return Ok(Some(id));
    }

    let created: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO tag_groups (account_id, name) VALUES ($1, $2)
         ON CONFLICT DO NOTHING RETURNING id",
    )
    .bind(account_id)
    .bind(name)
    .fetch_optional(&state.db)
    .await?;

    // `None` here means a concurrent caller created the same group between the SELECT and the
    // INSERT — the row that won is the answer, not an error.
    match created {
        Some(id) => Ok(Some(id)),
        None => {
            let id: Option<Uuid> = sqlx::query_scalar(
                "SELECT id FROM tag_groups WHERE account_id = $1 AND lower(name) = lower($2)",
            )
            .bind(account_id)
            .bind(name)
            .fetch_optional(&state.db)
            .await?;
            Ok(id)
        }
    }
}

/// Every tag of the account (or just one of them), in the payload shape the served console reads.
async fn fetch_tags(
    state: &AppState,
    account_id: Uuid,
    tag_id: Option<Uuid>,
) -> Result<Vec<Value>, AppError> {
    use sqlx::Row;
    let rows = sqlx::query(
        r#"SELECT t.id, t.name, t.color, tg.name as group_name FROM tags t
           LEFT JOIN tag_groups tg ON tg.id = t.group_id
           WHERE t.account_id = $1 AND ($2::uuid IS NULL OR t.id = $2)
           ORDER BY tg.name, t.name"#,
    )
    .bind(account_id)
    .bind(tag_id)
    .fetch_all(&state.db)
    .await?;

    let mut tags: Vec<Value> = Vec::new();
    for row in &rows {
        tags.push(json!({
            "id": row.get::<Uuid, _>("id"),
            "name": row.get::<String, _>("name"),
            "color": row.get::<Option<String>, _>("color"),
            "group": row.get::<Option<String>, _>("group_name"),
        }));
    }
    Ok(tags)
}

async fn tag_json(state: &AppState, account_id: Uuid, tag_id: Uuid) -> Result<Value, AppError> {
    let mut rows = fetch_tags(state, account_id, Some(tag_id)).await?;
    rows.pop()
        .ok_or_else(|| AppError::Internal("tag row vanished between write and read".to_string()))
}

/// GET /api/v1/tags — the caller's own tags. Moved here from `dashboard_handler` (route and payload
/// unchanged) so the whole tag family lives in one module.
pub async fn list_tags(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let account_id = account_uuid(&user)?;
    let tags = fetch_tags(&state, account_id, None).await?;
    Ok(Json(json!({ "tags": tags })))
}

/// POST /api/v1/tags — create one tag for the caller's own account.
///
/// IDEMPOTENT on `(account_id, lower(name))`: the key the unique index enforces, and the key the
/// console's screen already shows. Re-submitting a name that exists returns the EXISTING tag with
/// `created: false` — no duplicate row, no 23505, and no consumption of the allowance.
///
/// The plan allowance is checked on the CREATE path only, through `enforce_feature_limit` for the
/// key `max_tags`. An account at its cap gets the app's own 402 message
/// (`Tags limit reached (85/10). Upgrade to increase your limit.`) and no row is written.
pub async fn create_tag(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<TagBody>,
) -> Result<Json<Value>, AppError> {
    let account_id = account_uuid(&user)?;
    let name = normalize_name(body.name.as_deref())?;
    let color = normalize_color(body.color.as_deref())?;

    // Already there? That is the answer — and a re-POST of an existing tag must not 402 just
    // because the account is at (or over) its cap.
    if let Some(existing) = find_tag_id(&state, account_id, &name).await? {
        return Ok(Json(json!({
            "tag": tag_json(&state, account_id, existing).await?,
            "created": false,
        })));
    }

    features::enforce_feature_limit(&state.db, &user.account_id, "max_tags", "Tags").await?;

    let group_id = resolve_group(&state, account_id, body.group.as_deref()).await?;

    let created: Option<Uuid> = sqlx::query_scalar(
        r#"INSERT INTO tags (account_id, name, color, group_id) VALUES ($1, $2, $3, $4)
           ON CONFLICT DO NOTHING RETURNING id"#,
    )
    .bind(account_id)
    .bind(&name)
    .bind(&color)
    .bind(group_id)
    .fetch_optional(&state.db)
    .await?;

    let tag_id = match created {
        Some(id) => id,
        // Lost a race with a concurrent create of the same name: return the row that won.
        None => find_tag_id(&state, account_id, &name)
            .await?
            .ok_or_else(|| {
                AppError::Internal("tag insert conflicted but no row exists".to_string())
            })?,
    };

    Ok(Json(json!({
        "tag": tag_json(&state, account_id, tag_id).await?,
        "created": created.is_some(),
    })))
}

/// PUT /api/v1/tags/:id — rename / recolour / regroup one of the caller's own tags.
///
/// `group_id` is SET, never COALESCEd: an empty Group box CLEARS the group, which is the only way
/// the console's Group field can express "no group" (a partial update could never clear it).
pub async fn update_tag(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(id): Path<String>,
    Json(body): Json<TagBody>,
) -> Result<Json<Value>, AppError> {
    let account_id = account_uuid(&user)?;
    let tag_id = parse_tag_id(&id)?;
    let name = normalize_name(body.name.as_deref())?;
    let color = normalize_color(body.color.as_deref())?;

    // Ownership first: 404 for a tag of another account (never 403 — a 403 would confirm the id
    // exists somewhere), and no group row is minted for an update that cannot happen.
    let owned: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM tags WHERE id = $1 AND account_id = $2")
            .bind(tag_id)
            .bind(account_id)
            .fetch_optional(&state.db)
            .await?;
    if owned.is_none() {
        return Err(AppError::NotFound("Tag not found".to_string()));
    }

    // Renaming onto another tag of the same account would violate the unique key: answer with a
    // message instead of letting Postgres raise 23505 into a 500.
    if let Some(other) = find_tag_id(&state, account_id, &name).await? {
        if other != tag_id {
            return Err(AppError::BadRequest(
                "A tag with that name already exists".to_string(),
            ));
        }
    }

    let group_id = resolve_group(&state, account_id, body.group.as_deref()).await?;

    let updated: Option<Uuid> = sqlx::query_scalar(
        r#"UPDATE tags SET name = $3, color = $4, group_id = $5, updated_at = NOW()
           WHERE id = $1 AND account_id = $2 RETURNING id"#,
    )
    .bind(tag_id)
    .bind(account_id)
    .bind(&name)
    .bind(&color)
    .bind(group_id)
    .fetch_optional(&state.db)
    .await?;

    if updated.is_none() {
        return Err(AppError::NotFound("Tag not found".to_string()));
    }

    Ok(Json(json!({
        "tag": tag_json(&state, account_id, tag_id).await?,
        "updated": true,
    })))
}

/// DELETE /api/v1/tags/:id — delete one of the caller's own tags. Idempotent in the sense that a
/// second delete answers 404 with the app's own message rather than 500.
pub async fn delete_tag(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    let account_id = account_uuid(&user)?;
    let tag_id = parse_tag_id(&id)?;

    let res = sqlx::query("DELETE FROM tags WHERE id = $1 AND account_id = $2")
        .bind(tag_id)
        .bind(account_id)
        .execute(&state.db)
        .await?;

    if res.rows_affected() == 0 {
        return Err(AppError::NotFound("Tag not found".to_string()));
    }

    Ok(Json(json!({ "status": "deleted", "deleted": true })))
}
