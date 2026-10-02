//! The knowledge base — two sides, kept in line.
//!
//! David, 2026-10-02: *"Every software should have a knowledge base for the admin, for the users and
//! they should be in line respectively. So that way users can be walked through."*
//!
//! Measured before this module existed: `knowledge_base` held 0 rows, had 0 code references and 0
//! routes (`GET /api/v1/knowledge-base` answered 404). It was created by
//! `019_fix_phantom_tables.sql` — a table built to satisfy a schema expectation, never a feature.
//!
//! TWO AUDIENCES, TWO READERS:
//!   * `audience = 'user'`  -> the end user walking through the product. Read at `/guide.html`.
//!   * `audience = 'admin'` -> the account holder operating it. Read in the console's Knowledge Base
//!     view and at `/admin/guide.html`.
//! "In line respectively" is enforced by the shared shape rather than by convention: both sides are the
//! same rows, the same columns, the same ordering (`sort_order`), and the same editor — so an operator
//! writing the admin side and the user side is looking at one screen and cannot drift into two systems.
//!
//! TENANCY: `tenant_id IS NULL` is the platform-wide article that ships with the product and every
//! tenant reads. A row carrying a `tenant_id` is that tenant's own article and WINS for that tenant.
//! The list resolves per slug, so overriding one article does not delete the rest of the shipped set.

use axum::{
    extract::{Path, Query, State},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::error::AppError;
use crate::security::auth::AuthenticatedUser;
use crate::state::AppState;

/// One article as a READER sees it: no tenant_id, no draft flag — the list is already filtered.
const LIST_SQL: &str = r#"
    SELECT DISTINCT ON (slug)
           id, slug, title, category, sort_order, updated_at
      FROM knowledge_base
     WHERE audience = $1
       AND is_published
       AND (tenant_id IS NULL OR tenant_id = $2)
     ORDER BY slug, (tenant_id IS NOT NULL) DESC, sort_order
"#;

#[derive(Debug, Deserialize)]
pub struct AudienceQuery {
    #[serde(default)]
    pub audience: Option<String>,
}

fn audience_of(q: &AudienceQuery) -> Result<&'static str, AppError> {
    match q.audience.as_deref().unwrap_or("user") {
        "user" => Ok("user"),
        "admin" => Ok("admin"),
        other => Err(AppError::BadRequest(format!(
            "audience must be 'admin' or 'user', got '{other}'"
        ))),
    }
}

/// GET /api/v1/knowledge-base?audience=user|admin
///
/// The USER side is public: the people being walked through the product are frequently not signed in
/// (a player on a kiosk, someone who followed a link). The ADMIN side requires a caller, because it
/// describes how the account holder's own software works and names screens only they can see.
pub async fn list_articles(
    State(state): State<AppState>,
    user: Option<AuthenticatedUser>,
    Query(q): Query<AudienceQuery>,
) -> Result<Json<Value>, AppError> {
    let audience = audience_of(&q)?;
    if audience == "admin" && user.is_none() {
        return Err(AppError::Unauthorized(
            "The admin knowledge base requires a signed-in account".to_string(),
        ));
    }
    // A signed-in caller gets their own tenant's rows preferred; an anonymous one gets whatever the
    // query needs, which `resolve` handles by falling back to the platform rows below.
    // Option<Uuid>, not Option<String>: `tenant_id` is a uuid column, and binding text made Postgres
    // refuse the comparison outright (500 on every read that carried a tenant). Nothing about the
    // feature was wrong — the type going into the bind was.
    let tenant: Option<Uuid> = user
        .as_ref()
        .and_then(|u| Uuid::parse_str(&u.account_id).ok());

    let rows = resolve(&state, audience, tenant).await?;
    Ok(Json(json!({
        "audience": audience,
        "count": rows.len(),
        "articles": rows,
    })))
}

/// GET /api/v1/knowledge-base/article/:slug?audience=user|admin — one article WITH its body.
pub async fn get_article(
    State(state): State<AppState>,
    user: Option<AuthenticatedUser>,
    Path(slug): Path<String>,
    Query(q): Query<AudienceQuery>,
) -> Result<Json<Value>, AppError> {
    let audience = audience_of(&q)?;
    if audience == "admin" && user.is_none() {
        return Err(AppError::Unauthorized(
            "The admin knowledge base requires a signed-in account".to_string(),
        ));
    }
    // Option<Uuid> for the same reason as the list: `tenant_id` is a uuid column, and text made
    // Postgres refuse the comparison, so a missing article answered 500 instead of 404.
    let tenant: Option<Uuid> = user
        .as_ref()
        .and_then(|u| Uuid::parse_str(&u.account_id).ok());

    let row: Option<(
        Uuid,
        Option<Uuid>,
        String,
        Option<String>,
        Option<String>,
        bool,
        i32,
    )> = sqlx::query_as(
        r#"SELECT id, tenant_id, title, content, category, is_published, sort_order
                 FROM knowledge_base
                WHERE audience = $1
                  AND slug = $2
                  AND is_published
                  AND (tenant_id IS NULL OR tenant_id = $3)
                ORDER BY (tenant_id IS NOT NULL) DESC
                LIMIT 1"#,
    )
    .bind(audience)
    .bind(&slug)
    .bind(tenant)
    .fetch_optional(&state.db)
    .await?;

    match row {
        Some((id, tenant_id, title, content, category, published, order)) => Ok(Json(json!({
            "id": id,
            "slug": slug,
            "audience": audience,
            "title": title,
            "content": content.unwrap_or_default(),
            "category": category,
            "sort_order": order,
            "is_published": published,
            "tenant_specific": tenant_id.is_some(),
        }))),
        None => Err(AppError::NotFound(format!(
            "No {audience} article '{slug}'"
        ))),
    }
}

// ─────────────────────────────── authoring (the console) ───────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ArticleInput {
    pub audience: String,
    pub title: String,
    #[serde(default)]
    pub slug: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub sort_order: Option<i32>,
    #[serde(default = "default_true")]
    pub is_published: bool,
}

fn default_true() -> bool {
    true
}

/// Who may write WHAT.
///
/// The platform operator writes the SHIPPED set (`tenant_id IS NULL`) — the walkthrough that arrives
/// with the product for every tenant. An account holder writes their OWN rows (`tenant_id = their
/// account`), which win for them alone. Neither may edit the other's: the rule below is the single
/// place that decides it, so create, update and delete cannot drift apart on it.
fn is_platform_operator(user: &AuthenticatedUser) -> bool {
    user.role == "admin" || user.role == "super_admin"
}

/// The tenant_id a NEW article should carry, given who is writing.
fn owner_for_new(user: &AuthenticatedUser, fallback: Uuid) -> Option<Uuid> {
    if is_platform_operator(user) {
        None // platform-wide: ships to every tenant
    } else {
        Some(Uuid::parse_str(&user.account_id).unwrap_or(fallback))
    }
}

/// May this caller edit/delete a row owned by `owner`?
fn may_touch(user: &AuthenticatedUser, owner: Option<Uuid>) -> bool {
    match owner {
        None => is_platform_operator(user), // shipped content: operator only
        Some(t) => is_platform_operator(user) || Some(t) == Uuid::parse_str(&user.account_id).ok(),
    }
}

/// A slug an operator can type or omit. When omitted it is derived from the title, so a new article is
/// linkable the moment it is saved without anyone having to think about URL syntax.
fn slugify(title: &str, given: Option<&str>) -> String {
    if let Some(s) = given.map(str::trim).filter(|s| !s.is_empty()) {
        return s.to_lowercase().replace(' ', "-");
    }
    let mut out = String::new();
    let mut dash = false;
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        format!("article-{}", &Uuid::new_v4().to_string()[..8])
    } else {
        out
    }
}

/// Fetch a row's owner and refuse the edit if it is not the caller's to make.
///
/// Doing this BEFORE the write is what makes "someone else's article" answer 403 instead of the
/// ambiguous 404 an empty `rows_affected` would produce — and it is the only place the rule lives,
/// so update and delete cannot drift apart.
async fn authorise_edit(
    state: &AppState,
    user: &AuthenticatedUser,
    id: Uuid,
) -> Result<(), AppError> {
    let owner: Option<Option<Uuid>> =
        sqlx::query_scalar("SELECT tenant_id FROM knowledge_base WHERE id = $1")
            .bind(id)
            .fetch_optional(&state.db)
            .await?;
    match owner {
        None => Err(AppError::NotFound("No such article".to_string())),
        Some(None) if !is_platform_operator(user) => Err(AppError::Forbidden(
            "This is shipped platform content; only the platform operator can change it"
                .to_string(),
        )),
        Some(Some(owner)) if !may_touch(user, Some(owner)) => Err(AppError::Forbidden(
            "This article belongs to another account".to_string(),
        )),
        _ => Ok(()),
    }
}

/// POST /api/v1/knowledge-base — author an article for either side.
pub async fn create_article(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<ArticleInput>,
) -> Result<Json<Value>, AppError> {
    let audience = match body.audience.as_str() {
        "admin" | "user" => body.audience.clone(),
        other => {
            return Err(AppError::BadRequest(format!(
                "audience must be 'admin' or 'user', got '{other}'"
            )))
        }
    };
    let title = body.title.trim().to_string();
    if title.is_empty() {
        return Err(AppError::BadRequest("A title is required".to_string()));
    }
    let slug = slugify(&title, body.slug.as_deref());
    let id = Uuid::new_v4();
    let tid: Option<Uuid> = owner_for_new(&user, id);

    let res = sqlx::query(
        r#"INSERT INTO knowledge_base
               (id, tenant_id, audience, slug, title, content, category, sort_order, is_published, updated_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, now())
           ON CONFLICT (audience, slug) WHERE slug IS NOT NULL
           DO NOTHING"#,
    )
    .bind(id)
    .bind(tid)
    .bind(&audience)
    .bind(&slug)
    .bind(&title)
    .bind(body.content.clone().unwrap_or_default())
    .bind(body.category.clone())
    .bind(body.sort_order.unwrap_or(100))
    .bind(body.is_published)
    .execute(&state.db)
    .await?;

    if res.rows_affected() == 0 {
        return Err(AppError::BadRequest(format!(
            "An {audience} article with the slug '{slug}' already exists"
        )));
    }
    Ok(Json(
        json!({ "id": id, "audience": audience, "slug": slug, "title": title, "created": true }),
    ))
}

/// PUT /api/v1/admin/knowledge-base/:id — edit any field, keeping the side it belongs to.
pub async fn update_article(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(id): Path<Uuid>,
    Json(body): Json<ArticleInput>,
) -> Result<Json<Value>, AppError> {
    authorise_edit(&state, &user, id).await?;
    let audience = match body.audience.as_str() {
        "admin" | "user" => body.audience.clone(),
        other => {
            return Err(AppError::BadRequest(format!(
                "audience must be 'admin' or 'user', got '{other}'"
            )))
        }
    };
    let res = sqlx::query(
        r#"UPDATE knowledge_base
              SET audience     = $2,
                  title        = COALESCE($3, title),
                  content      = COALESCE($4, content),
                  category     = COALESCE($5, category),
                  sort_order   = COALESCE($6, sort_order),
                  is_published = $7,
                  updated_at   = now()
            WHERE id = $1
              AND (tenant_id = $8 OR ($9 AND tenant_id IS NULL))"#,
    )
    .bind(id)
    .bind(&audience)
    .bind(body.title.trim().to_string())
    .bind(body.content.clone())
    .bind(body.category.clone())
    .bind(body.sort_order)
    .bind(body.is_published)
    .bind(Uuid::parse_str(&user.account_id).ok())
    .bind(is_platform_operator(&user))
    .execute(&state.db)
    .await?;

    if res.rows_affected() == 0 {
        return Err(AppError::NotFound(
            "No such article, or it belongs to someone else".to_string(),
        ));
    }
    Ok(Json(json!({ "id": id, "updated": true })))
}

/// DELETE /api/v1/admin/knowledge-base/:id
pub async fn delete_article(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, AppError> {
    authorise_edit(&state, &user, id).await?;
    let res = sqlx::query(
        "DELETE FROM knowledge_base
          WHERE id = $1
            AND (tenant_id = $2 OR ($3 AND tenant_id IS NULL))",
    )
    .bind(id)
    .bind(Uuid::parse_str(&user.account_id).ok())
    .bind(is_platform_operator(&user))
    .execute(&state.db)
    .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::NotFound(
            "No such article, or it belongs to someone else".to_string(),
        ));
    }
    Ok(Json(json!({ "id": id, "deleted": true })))
}

/// The reader's query, with the tenant fallback that a plain SQL `OR` cannot express: `tenant_id IS
/// NULL` rows are the shipped set, and a tenant row of the same slug replaces one of them.
async fn resolve(
    state: &AppState,
    audience: &str,
    tenant: Option<Uuid>,
) -> Result<Vec<Value>, AppError> {
    let rows: Vec<(
        Uuid,
        String,
        String,
        Option<String>,
        i32,
        Option<chrono::DateTime<chrono::Utc>>,
    )> = sqlx::query_as(LIST_SQL)
        .bind(audience)
        .bind(tenant)
        .fetch_all(&state.db)
        .await?;

    Ok(rows
        .into_iter()
        .map(|(id, slug, title, category, sort_order, updated_at)| {
            json!({
                "id": id,
                "slug": slug,
                "title": title,
                "category": category,
                "sort_order": sort_order,
                "updated_at": updated_at,
            })
        })
        .collect())
}
