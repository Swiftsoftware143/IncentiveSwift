//! Surface handlers — widget embed snippets, play, embed views, and domain management.

use crate::error::AppError;
use crate::handlers::api_keys::resolve_owner_account_id;
use crate::security::auth::AuthenticatedUser;
use crate::state::AppState;
use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap},
    response::{IntoResponse, Response},
    Json,
};

fn esc_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

/// A widget snippet record.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct WidgetSnippet {
    pub id: Uuid,
    pub campaign_id: Uuid,
    pub snippet_hash: String,
    pub is_active: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// A custom domain record.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct CustomDomain {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub domain: String,
    pub target_type: String,
    pub verification_token: String,
    pub verified_at: Option<chrono::DateTime<chrono::Utc>>,
    pub ssl_provisioned_at: Option<chrono::DateTime<chrono::Utc>>,
    pub is_active: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// A loyalty member record for the dashboard.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct LoyaltyMemberRow {
    pub id: Uuid,
    pub contact_id: Option<Uuid>,
    pub points_balance: i32,
    pub lifetime_points: i32,
    pub member_since: chrono::DateTime<chrono::Utc>,
    pub last_checkin_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// A reward tier record for the dashboard.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct RewardTierRow {
    pub id: Uuid,
    pub name: String,
    pub points_required: i32,
    pub requires_approval: bool,
    pub reward_tag: String,
    pub sort_order: i32,
}

/// Input for registering a domain.
#[derive(Deserialize)]
pub struct RegisterDomainInput {
    pub domain: String,
    pub target_type: Option<String>,
}

/// Input for updating surface config.
#[derive(Deserialize)]
pub struct UpdateSurfaceConfigInput {
    pub surface_config: Value,
}

/// The embeddable popup widget runtime served by `GET /api/v1/widget/{hash}`.
///
/// Served as `text/javascript` because that is what the embed code the app hands a
/// customer points at (`<script src=".../api/v1/widget/{hash}" async>`).  It used to be
/// answered as JSON from a route the app's own embed options told customers to load as a
/// script, and `X-Content-Type-Options: nosniff` makes a browser refuse a JSON body for a
/// `<script src>` — i.e. the widget could never render anywhere (kanban t_e3a33d15).
///
/// Placeholders are substituted with `.replace()` rather than `format!` so the JS braces
/// stay readable: __HASH__, __SLUG__, __ORIGIN__, __LABEL__, __TITLE__, __CSS__.
const WIDGET_RUNTIME_JS: &str = r#"(function () {
  var HASH = __HASH__, SLUG = __SLUG__, ORIGIN = __ORIGIN__, LABEL = __LABEL__, TITLE = __TITLE__;
  var CSS = __CSS__;
  function boot() {
    if (window.__incentiveswiftWidget) { return; }
    window.__incentiveswiftWidget = true;
    if (!document.body) { return; }

    var style = document.createElement('style');
    style.setAttribute('data-incentiveswift-theme', HASH);
    style.appendChild(document.createTextNode(CSS));
    document.head.appendChild(style);

    // Source tracking: the HOST page's own UTM parameters, its referrer and its URL are
    // handed to the play page inside the overlay, which forwards them onto the entry.
    var host = window.location.search || '';
    function playUrl() {
      var q = [];
      var params = new URLSearchParams(host);
      ['utm_source', 'utm_medium', 'utm_campaign'].forEach(function (k) {
        var v = params.get(k);
        if (v) { q.push(k + '=' + encodeURIComponent(v)); }
      });
      q.push('page_url=' + encodeURIComponent(window.location.href));
      if (document.referrer) { q.push('referrer_url=' + encodeURIComponent(document.referrer)); }
      return ORIGIN + '/play/' + encodeURIComponent(SLUG) + '?' + q.join('&');
    }

    var trigger = document.createElement('button');
    trigger.type = 'button';
    trigger.id = 'incentiveswift-widget-trigger';
    trigger.setAttribute('data-incentiveswift-widget', HASH);
    trigger.setAttribute('aria-label', TITLE);
    trigger.textContent = LABEL;
    trigger.setAttribute('style', 'position:fixed;right:20px;bottom:20px;z-index:2147483000;cursor:pointer;' +
      'padding:14px 20px;border:0;font:600 15px/1.2 var(--is-font,Inter,system-ui,sans-serif);' +
      'background:var(--is-primary,#2563eb);color:var(--is-btn-text,#ffffff);' +
      'border-radius:var(--is-radius,12px);box-shadow:0 10px 30px rgba(15,21,48,.28)');

    var overlay = null;
    function build() {
      overlay = document.createElement('div');
      overlay.id = 'incentiveswift-widget-overlay';
      overlay.setAttribute('data-is-theme', HASH);
      overlay.setAttribute('style', 'position:fixed;inset:0;z-index:2147483001;display:flex;' +
        'align-items:center;justify-content:center;padding:16px;background:rgba(15,21,48,.72)');
      var frame = document.createElement('iframe');
      frame.setAttribute('data-incentiveswift-widget-frame', HASH);
      frame.setAttribute('title', TITLE);
      frame.setAttribute('allow', 'geolocation');
      frame.src = playUrl();
      frame.setAttribute('style', 'width:min(560px,100%);height:min(760px,92vh);border:0;' +
        'border-radius:var(--is-radius,12px);background:var(--is-bg,#ffffff)');
      var close = document.createElement('button');
      close.type = 'button';
      close.setAttribute('data-incentiveswift-widget-close', HASH);
      close.setAttribute('aria-label', 'Close');
      close.textContent = '✕';
      close.setAttribute('style', 'position:absolute;top:14px;right:18px;cursor:pointer;border:0;' +
        'width:36px;height:36px;border-radius:50%;font-size:16px;' +
        'background:var(--is-primary,#2563eb);color:var(--is-btn-text,#ffffff)');
      close.addEventListener('click', function () { overlay.style.display = 'none'; });
      overlay.addEventListener('click', function (e) { if (e.target === overlay) { overlay.style.display = 'none'; } });
      overlay.appendChild(frame);
      overlay.appendChild(close);
      document.body.appendChild(overlay);
    }
    trigger.addEventListener('click', function () {
      if (!overlay) { build(); } else { overlay.style.display = 'flex'; }
    });
    document.body.appendChild(trigger);
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', boot);
  } else {
    boot();
  }
})();
"#;

/// Optional query for the widget route: `?format=json` keeps the pre-existing JSON payload.
#[derive(Deserialize)]
pub struct WidgetJsQuery {
    pub format: Option<String>,
}

/// The origin a customer's embed must name: the CUSTOMER-FACING app host, never the operator
/// console's own host.  Two properties matter and both were measured on this deployment:
///
///   * the url must be ABSOLUTE — the tag is pasted onto someone else's site, where a relative
///     `/api/v1/...` (or `/play/...`) would resolve to the customer's own domain;
///   * it must be the APP host, not this request's host.  The console serves the same API on
///     admin.<domain>, and an embed minted from there inherited `admin.<domain>` — where
///     `/play/<slug>` is not served, so the widget's overlay 404s (found by
///     /opt/swift/audits/t_e3a33d15/proof-console-panel.cjs, kanban t_e3a33d15).
///
/// Loopback keeps `http` and its own host so a local probe stays honest about what it measured;
/// anything else is `https`, because TLS terminates upstream (Cloudflare -> nginx) and the
/// origin only ever sees plain http.
fn public_origin(headers: &HeaderMap) -> String {
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(header::HOST))
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "app.incentiveswift.com".to_string());
    let loopback = host.starts_with("127.0.0.1")
        || host.starts_with("localhost")
        || host.starts_with("[::1]")
        || host.starts_with("0.0.0.0");
    if loopback {
        return format!("http://{}", host);
    }
    // admin.<domain> is the operator console; the embed belongs to app.<domain>.
    let app_host = match host.split_once('.') {
        Some(("admin", rest)) => format!("app.{}", rest),
        _ => host,
    };
    format!("https://{}", app_host)
}

/// GET /api/v1/widget/{hash}
/// Serves the embeddable widget runtime as JavaScript — what an embed can execute.
/// `?format=json` returns the same snippet as JSON, the payload this route served before.
pub async fn get_widget_js(
    State(state): State<AppState>,
    Path(hash): Path<String>,
    Query(query): Query<WidgetJsQuery>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let snippet = sqlx::query_as::<_, WidgetSnippet>(
        r#"SELECT id, campaign_id, snippet_hash, is_active, created_at
           FROM widget_snippets WHERE snippet_hash = $1 AND is_active = true"#,
    )
    .bind(&hash)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Widget snippet not found".to_string()))?;

    // Resolve the campaign's theme (and label) so the runtime brands itself live.
    let campaign = sqlx::query(
        r#"SELECT name, slug, config, COALESCE(surface_config, '{}'::jsonb) AS surface_config
           FROM campaigns WHERE id = $1"#,
    )
    .bind(snippet.campaign_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Campaign not found".to_string()))?;
    let slug: String = campaign.get("slug");
    let name: String = campaign.get("name");
    let config: Value = campaign.get("config");
    let surface_config: Value = campaign.get("surface_config");
    let theme = crate::theme::resolve_theme(&surface_config);
    let label = config
        .get("cta_text")
        .and_then(|v| v.as_str())
        .unwrap_or("Enter now")
        .to_string();
    let runtime = widget_runtime_js(&hash, &slug, &name, &label, &theme, &headers);

    if query.format.as_deref() == Some("json") {
        return Ok(Json(json!({
            "hash": hash,
            "campaign_id": snippet.campaign_id,
            "campaign_slug": slug,
            "javascript": runtime,
            "theme": theme,
            "status": "active",
        }))
        .into_response());
    }

    Ok((
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        runtime,
    )
        .into_response())
}

/// Render the widget runtime for one campaign, with every dynamic value JSON-escaped.
fn widget_runtime_js(
    hash: &str,
    slug: &str,
    name: &str,
    label: &str,
    theme: &Value,
    headers: &HeaderMap,
) -> String {
    let js_str = |s: &str| serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string());
    let css = crate::theme::js_string_literal(&crate::theme::theme_css(theme));
    WIDGET_RUNTIME_JS
        .replace("__HASH__", &js_str(hash))
        .replace("__SLUG__", &js_str(slug))
        .replace("__ORIGIN__", &js_str(&public_origin(headers)))
        .replace("__LABEL__", &js_str(label))
        .replace("__TITLE__", &js_str(name))
        .replace("__CSS__", &format!("'{}'", css))
}

/// Resolve a campaign the caller's own account owns, or 404 — never leak another tenant's
/// campaign by answering 403.
async fn owned_campaign(
    state: &AppState,
    slug: &str,
    user: &AuthenticatedUser,
) -> Result<(Uuid, Uuid), AppError> {
    let account_id = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID value".to_string()))?;
    let row = sqlx::query("SELECT id, account_id FROM campaigns WHERE slug = $1")
        .bind(slug)
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Campaign not found".to_string()))?;
    let campaign_id: Uuid = row.get("id");
    let owner: Uuid = row.get("account_id");
    if owner != account_id {
        return Err(AppError::NotFound("Campaign not found".to_string()));
    }
    Ok((campaign_id, account_id))
}

/// POST /api/v1/campaigns/{slug}/widget-snippet
///
/// The producer for `widget_snippets`: mints (or re-uses) the campaign's active embed
/// snippet and returns the copy-paste tag.  Before this route existed nothing in the app,
/// the migrations or anywhere on the box could insert a `widget_snippets` row, so
/// `GET /api/v1/widget/{hash}` could only ever serve hand-made rows and
/// `GET /api/v1/embed/campaign/{slug}` handed customers a "Widget Script" URL that 404s.
pub async fn create_widget_snippet(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    user: AuthenticatedUser,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    let (campaign_id, _account_id) = owned_campaign(&state, &slug, &user).await?;

    let existing: Option<String> = sqlx::query_scalar::<_, String>(
        "SELECT snippet_hash FROM widget_snippets WHERE campaign_id = $1 AND is_active = true \
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(campaign_id)
    .fetch_optional(&state.db)
    .await?;

    let snippet_hash = match existing {
        Some(hash) => hash,
        None => {
            // The campaign slug IS the hash: it is UNIQUE on campaigns, it is what the
            // app's own embed option and the retired admin SPA already assumed
            // (`/api/v1/widget/<slug>`), and it keeps the snippet URL recognisable.
            let hash = slug.clone();
            let id = Uuid::new_v4();
            sqlx::query(
                "INSERT INTO widget_snippets (id, campaign_id, snippet_hash, is_active) \
                 VALUES ($1, $2, $3, true) \
                 ON CONFLICT (snippet_hash) DO UPDATE SET is_active = true, campaign_id = EXCLUDED.campaign_id",
            )
            .bind(id)
            .bind(campaign_id)
            .bind(&hash)
            .execute(&state.db)
            .await?;
            hash
        }
    };

    let campaign = sqlx::query("SELECT name, status FROM campaigns WHERE id = $1")
        .bind(campaign_id)
        .fetch_one(&state.db)
        .await?;
    let name: String = campaign.get("name");
    let status: String = campaign.get("status");

    let origin = public_origin(&headers);
    let embed_code = format!(
        "<script src=\"{origin}/api/v1/widget/{snippet_hash}\" async data-campaign-hash=\"{snippet_hash}\"></script>"
    );

    Ok(Json(json!({
        "campaign": { "id": campaign_id, "slug": slug, "name": name, "status": status },
        "snippet_hash": snippet_hash,
        "is_active": true,
        "widget_url": format!("{origin}/api/v1/widget/{snippet_hash}"),
        "config_url": format!("{origin}/api/v1/widget/{snippet_hash}/config"),
        "play_url": format!("{origin}/play/{slug}"),
        "embed_code": embed_code,
    })))
}

/// DELETE /api/v1/campaigns/{slug}/widget-snippet
/// Stops serving the campaign's embed (the row is deactivated so the trail survives).
pub async fn disable_widget_snippet(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let (campaign_id, _account_id) = owned_campaign(&state, &slug, &user).await?;
    let result = sqlx::query(
        "UPDATE widget_snippets SET is_active = false WHERE campaign_id = $1 AND is_active = true",
    )
    .bind(campaign_id)
    .execute(&state.db)
    .await?;
    Ok(Json(json!({
        "campaign_slug": slug,
        "deactivated": result.rows_affected(),
    })))
}

/// GET /api/v1/widget/{hash}/config
/// Returns the widget configuration for a campaign.
pub async fn get_widget_config(
    State(state): State<AppState>,
    Path(hash): Path<String>,
) -> Result<Json<Value>, AppError> {
    let snippet = sqlx::query_as::<_, WidgetSnippet>(
        r#"SELECT ws.id, ws.campaign_id, ws.snippet_hash, ws.is_active, ws.created_at
           FROM widget_snippets ws WHERE ws.snippet_hash = $1 AND ws.is_active = true"#,
    )
    .bind(&hash)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Widget snippet not found".to_string()))?;

    // Get the campaign config for widget settings
    let campaign = sqlx::query(
        r#"SELECT id, name, slug, type, config, surface_config, outcome_tags
           FROM campaigns WHERE id = $1"#,
    )
    .bind(snippet.campaign_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Campaign not found".to_string()))?;

    let campaign_id: Uuid = campaign.get("id");
    let name: String = campaign.get("name");
    let slug: String = campaign.get("slug");
    let campaign_type: String = campaign.get("type");
    let config: Value = campaign.get("config");
    let surface_config: Value = campaign.get("surface_config");
    let outcome_tags: Value = campaign.get("outcome_tags");
    let theme = crate::theme::resolve_theme(&surface_config);

    Ok(Json(json!({
        "campaign": {
            "id": campaign_id,
            "name": name,
            "slug": slug,
            "type": campaign_type,
        },
        "config": config,
        "surface_config": surface_config,
        "theme": theme,
        "outcome_tags": outcome_tags,
    })))
}

/// GET /api/v1/play/{id}
/// Optional query param: ?company=subdomain to scope to a portfolio company
pub async fn get_play_view(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<Value>,
) -> Result<Json<Value>, AppError> {
    // Resolve account_id from subdomain if provided
    let account_filter = if let Some(company) = params.get("company").and_then(|v| v.as_str()) {
        let filter = sqlx::query_scalar::<_, Uuid>(
            "SELECT account_id FROM portfolio_companies WHERE subdomain = $1 OR domain = $1",
        )
        .bind(company)
        .fetch_optional(&state.db)
        .await?;
        filter
    } else {
        None
    };

    // Fetch company info for branding
    let company_info: Option<serde_json::Value> = if let Some(company) =
        params.get("company").and_then(|v| v.as_str())
    {
        let row = sqlx::query_scalar(
            "SELECT jsonb_build_object('name', name, 'slug', slug, 'subdomain', subdomain, 'domain', domain, 'settings', COALESCE(settings, '{}'::jsonb)) FROM portfolio_companies WHERE subdomain = $1 OR domain = $1"
        )
        .bind(company)
        .fetch_optional(&state.db).await.unwrap_or(None);
        row
    } else {
        None
    };

    // Try as UUID first, then as slug
    let campaign = match Uuid::parse_str(&id) {
        Ok(cid) => {
            let mut q = String::from(
                "SELECT id, name, slug, type, status, config, surface_config,
                       tag_namespace, outcome_tags, delivery_method, delivery_config, created_at
                FROM campaigns WHERE id = $1 AND status = 'active'",
            );
            if let Some(aid) = account_filter {
                q.push_str(" AND account_id = $2");
                sqlx::query(&q)
                    .bind(cid)
                    .bind(aid)
                    .fetch_optional(&state.db)
                    .await?
            } else {
                sqlx::query(&q).bind(cid).fetch_optional(&state.db).await?
            }
        }
        Err(_) => {
            let mut q = String::from(
                "SELECT id, name, slug, type, status, config, surface_config,
                       tag_namespace, outcome_tags, delivery_method, delivery_config, created_at
                FROM campaigns WHERE slug = $1 AND status = 'active'",
            );
            if let Some(aid) = account_filter {
                q.push_str(" AND account_id = $2");
                sqlx::query(&q)
                    .bind(&id)
                    .bind(aid)
                    .fetch_optional(&state.db)
                    .await?
            } else {
                sqlx::query(&q).bind(&id).fetch_optional(&state.db).await?
            }
        }
    };
    let campaign =
        campaign.ok_or_else(|| AppError::NotFound("Active campaign not found".to_string()))?;

    let cid: Uuid = campaign.get("id");
    let name: String = campaign.get("name");
    let slug: String = campaign.get("slug");
    let campaign_type: String = campaign.get("type");
    let status: String = campaign.get("status");
    let config: Value = campaign.get("config");
    let surface_config: Value = campaign.get("surface_config");
    let tag_namespace: String = campaign.get("tag_namespace");
    let outcome_tags: Value = campaign.get("outcome_tags");
    let delivery_method: String = campaign.get("delivery_method");
    let delivery_config: Value = campaign.get("delivery_config");
    let created_at: chrono::DateTime<chrono::Utc> = campaign.get("created_at");
    let theme = crate::theme::resolve_theme(&surface_config);

    let mut payload = json!({
        "campaign": {
            "id": cid,
            "name": name,
            "slug": slug,
            "type": campaign_type,
            "status": status,
            "tag_namespace": tag_namespace,
            "created_at": created_at,
        },
        "config": config,
        "surface_config": surface_config,
        "theme": theme,
        "outcome_tags": outcome_tags,
        "delivery_method": delivery_method,
        "delivery_config": delivery_config,
        "company": company_info,
    });

    // Grouped presentation order for the long-form qualifier. `config.sections` is an
    // ORDERING-ONLY overlay: it never touches scoring, outcomes, tags, redirects or the
    // webhook. It is exposed ONLY when the campaign actually defines sections, so a
    // campaign without them returns exactly the payload it returned before.
    if config
        .get("sections")
        .and_then(|s| s.as_array())
        .map(|s| !s.is_empty())
        .unwrap_or(false)
    {
        payload["form_sections"] = json!(super::form_sections::resolve_form_sections(&config));
    }

    Ok(Json(payload))
}

/// GET /api/v1/play/{id}/dashboard
pub async fn get_loyalty_dashboard(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    let campaign_id = Uuid::parse_str(&id)
        .map_err(|_| AppError::BadRequest("Invalid campaign ID".to_string()))?;

    // Get the loyalty program for this campaign
    let program_row = sqlx::query(
        r#"SELECT id, name, recognition_method, points_per_checkin,
                  max_checkins_per_day, point_decay_days, is_active
           FROM loyalty_programs WHERE campaign_id = $1 AND is_active = true"#,
    )
    .bind(campaign_id)
    .fetch_optional(&state.db)
    .await?;

    let program_data = match program_row {
        Some(p) => {
            let program_id: Uuid = p.get("id");
            let program_name: String = p.get("name");
            let recognition_method: String = p.get("recognition_method");
            let points_per_checkin: i32 = p.get("points_per_checkin");
            let max_checkins_per_day: i32 = p.get("max_checkins_per_day");

            // Get reward tiers
            let reward_tiers: Vec<RewardTierRow> = sqlx::query_as(
                r#"SELECT id, name, points_required, requires_approval, reward_tag, sort_order
                   FROM loyalty_reward_tiers WHERE program_id = $1
                   ORDER BY sort_order, points_required"#,
            )
            .bind(program_id)
            .fetch_all(&state.db)
            .await?;

            // Get member count
            let member_count: i64 = sqlx::query_scalar::<_, Option<i64>>(
                "SELECT COUNT(*) FROM loyalty_members WHERE program_id = $1",
            )
            .bind(program_id)
            .fetch_one(&state.db)
            .await?
            .unwrap_or(0);

            // Get checkin stats
            let total_checkins: i64 = sqlx::query_scalar::<_, Option<i64>>(
                "SELECT COUNT(*) FROM loyalty_checkins lc JOIN loyalty_members lm ON lm.id = lc.member_id WHERE lm.program_id = $1"
            )
            .bind(program_id)
            .fetch_one(&state.db)
            .await?
            .unwrap_or(0);

            // Top members by points
            let top_members: Vec<LoyaltyMemberRow> = sqlx::query_as(
                r#"SELECT lm.id, lm.contact_id, lm.member_since, lm.last_checkin_at,
                          COALESCE(lm.points_balance, 0) AS points_balance, COALESCE(lm.lifetime_points, 0) AS lifetime_points
                   FROM loyalty_members lm
                   WHERE lm.program_id = $1
                   ORDER BY lm.points_balance DESC
                   LIMIT 10"#,
            )
            .bind(program_id)
            .fetch_all(&state.db)
            .await?;

            json!({
                "id": program_id,
                "name": program_name,
                "recognition_method": recognition_method,
                "points_per_checkin": points_per_checkin,
                "max_checkins_per_day": max_checkins_per_day,
                "reward_tiers": reward_tiers,
                "member_count": member_count,
                "total_checkins": total_checkins,
                "top_members": top_members,
            })
        }
        None => json!(null),
    };

    Ok(Json(json!({
        "campaign_id": campaign_id,
        "program": program_data,
    })))
}

/// GET /api/v1/embed/{id}
pub async fn get_embed_view(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    let campaign_id = Uuid::parse_str(&id)
        .map_err(|_| AppError::BadRequest("Invalid campaign ID".to_string()))?;

    let campaign = sqlx::query(
        r#"SELECT id, name, slug, type, config, surface_config, outcome_tags
           FROM campaigns WHERE id = $1 AND status = 'active'"#,
    )
    .bind(campaign_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Active campaign not found".to_string()))?;

    let cid: Uuid = campaign.get("id");
    let name: String = campaign.get("name");
    let slug: String = campaign.get("slug");
    let campaign_type: String = campaign.get("type");
    let config: Value = campaign.get("config");
    let surface_config: Value = campaign.get("surface_config");
    let outcome_tags: Value = campaign.get("outcome_tags");
    let theme = crate::theme::resolve_theme(&surface_config);
    let theme_css = crate::theme::theme_css(&theme);

    // Build embed HTML snippet with campaign config + injected theme CSS vars.
    let embed_html = format!(
        r#"<div id="is-embed-{}" data-campaign="{}" data-type="{}" data-is-theme></div>
<style data-incentiveswift-theme="{}">{}</style>
<script src="/api/v1/widget/{}/config" async></script>"#,
        &cid.to_string()[..8],
        slug,
        campaign_type,
        slug,
        theme_css,
        slug
    );

    Ok(Json(json!({
        "campaign": {
            "id": cid,
            "name": name,
            "slug": slug,
            "type": campaign_type,
        },
        "embed_html": embed_html,
        "config": config,
        "surface_config": surface_config,
        "theme": theme,
        "outcome_tags": outcome_tags,
    })))
}

/// GET /api/v1/admin/campaigns/{id}/surface
pub async fn get_surface_config(
    State(state): State<AppState>,
    Path(id): Path<String>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let campaign_id = Uuid::parse_str(&id)
        .map_err(|_| AppError::BadRequest("Invalid campaign ID".to_string()))?;

    let surface_config: Option<Value> =
        sqlx::query_scalar("SELECT surface_config FROM campaigns WHERE id = $1")
            .bind(campaign_id)
            .fetch_optional(&state.db)
            .await?
            .ok_or_else(|| AppError::NotFound("Campaign not found".to_string()))?;

    let sc = surface_config.clone().unwrap_or_else(|| json!({}));
    let theme = crate::theme::resolve_theme(&sc);

    Ok(Json(
        json!({ "surface_config": surface_config, "theme": theme }),
    ))
}

/// GET /api/v1/embed/campaign/all — List all active campaigns for public embed lobby
pub async fn get_embed_campaign_list(
    State(state): State<AppState>,
) -> Result<Json<Value>, AppError> {
    let rows = sqlx::query(
        r#"SELECT c.id, c.name, c.slug, c.type, c.config, c.created_at,
                  a.name as company_name
           FROM campaigns c
           JOIN accounts a ON a.id = c.account_id
           WHERE c.status = 'active'
           ORDER BY c.created_at DESC"#,
    )
    .fetch_all(&state.db)
    .await?;

    let mut campaigns = Vec::new();
    for row in rows {
        let cid: Uuid = row.get("id");
        let name: String = row.get("name");
        let slug: String = row.get("slug");
        let campaign_type: String = row.get("type");
        let config: serde_json::Value = row.get("config");
        let company: Option<String> = row.get("company_name");

        campaigns.push(serde_json::json!({
            "id": cid,
            "name": name,
            "slug": slug,
            "type": campaign_type,
            "config": config,
            "company_name": company,
        }));
    }

    Ok(Json(serde_json::json!({
        "success": true,
        "campaigns": campaigns,
    })))
}

/// GET /api/v1/embed/campaign/{slug} — Get embed info for a campaign by slug (public)
pub async fn get_campaign_embed(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    let campaign = sqlx::query(
        r#"SELECT id, name, slug, type, config, surface_config, created_at
           FROM campaigns WHERE slug = $1"#,
    )
    .bind(&slug)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Active campaign not found".to_string()))?;

    let cid: Uuid = campaign.get("id");
    let name: String = campaign.get("name");
    let slug_str: String = campaign.get("slug");
    let campaign_type: String = campaign.get("type");
    let config: Value = campaign.get("config");
    let surface_config: Value = campaign.get("surface_config");
    let theme = crate::theme::resolve_theme(&surface_config);
    let theme_css = crate::theme::theme_css(&theme);

    // Absolute URLs: this payload is pasted onto the CUSTOMER's site, where a relative
    // `/play/...` would resolve to the customer's own domain.
    let origin = public_origin(&headers);
    let play_url = format!("{origin}/play/{slug_str}");
    let play_url_full = play_url.clone();

    // The "Widget Script" option is offered only when the campaign really has an active
    // snippet (mint one with POST /api/v1/campaigns/{slug}/widget-snippet). It used to be
    // emitted unconditionally, so every customer who copied it got a 404 script URL
    // (measured, kanban t_e3a33d15).
    let snippet_hash: Option<String> = sqlx::query_scalar::<_, String>(
        "SELECT snippet_hash FROM widget_snippets WHERE campaign_id = $1 AND is_active = true ORDER BY created_at DESC LIMIT 1",
    )
    .bind(cid)
    .fetch_optional(&state.db)
    .await?;
    let widget_snippet = snippet_hash.as_ref().map(|h| {
        format!(
            r#"<script src="{origin}/api/v1/widget/{h}" async data-campaign-hash="{h}"></script>"#
        )
    });
    let embed_code = format!(
        r##"<!-- IncentiveSwift Campaign: {} -->
<style data-incentiveswift-theme="{}">{}</style>
<iframe src="{}/play/{}" width="100%" height="600" frameborder="0" style="border:none;border-radius:var(--is-radius,12px);" allow="geolocation" data-is-theme></iframe>
<script>
(function(){{
  var iframe = document.querySelector('iframe[src*="/play/{}"]');
  if(!iframe || !window.addEventListener) return;
  var utmParams = ['utm_source','utm_medium','utm_campaign'];
  var params = new URLSearchParams(window.location.search);
  var msg = {{}};
  utmParams.forEach(function(p){{ var v=params.get(p); if(v) msg[p]=v; }});
  msg.referrer = document.referrer || '';
  msg.page_url = window.location.href;
  iframe.addEventListener('load', function(){{
    iframe.contentWindow.postMessage({{type:'incentiveswift:source',data:msg}}, '*');
  }});
}})();
</script>
"##,
        esc_html(&name),
        slug_str,
        theme_css,
        origin,
        slug_str,
        slug_str
    );

    let r = serde_json::json!({
        "campaign": {
            "id": cid,
            "name": name,
            "slug": slug_str,
            "type": campaign_type,
        },
        "config": config,
        "surface_config": surface_config,
        "theme": theme,
        "play_url": play_url,
        "play_url_full": play_url_full,
        "embed_code": embed_code,
        "widget_snippet": widget_snippet,
        "widget_snippet_hash": snippet_hash,
        "widget_snippet_available": widget_snippet.is_some(),
    });
    Ok(Json(r))
}

/// PUT /api/v1/admin/campaigns/{id}/surface
pub async fn update_surface_config(
    State(state): State<AppState>,
    Path(id): Path<String>,
    user: AuthenticatedUser,
    Json(body): Json<UpdateSurfaceConfigInput>,
) -> Result<Json<Value>, AppError> {
    let campaign_id = Uuid::parse_str(&id)
        .map_err(|_| AppError::BadRequest("Invalid campaign ID".to_string()))?;

    // Deep-merge the incoming surface_config into the existing one so we never
    // clobber unrelated surface keys (tablet/widget/full_page) when only a
    // theme update is sent.
    let existing: Option<Value> =
        sqlx::query_scalar("SELECT surface_config FROM campaigns WHERE id = $1")
            .bind(campaign_id)
            .fetch_optional(&state.db)
            .await?
            .ok_or_else(|| AppError::NotFound("Campaign not found".to_string()))?;

    let mut merged = existing.unwrap_or_else(|| json!({}));
    crate::theme::deep_merge(&mut merged, &body.surface_config);

    sqlx::query("UPDATE campaigns SET surface_config = $1 WHERE id = $2")
        .bind(&merged)
        .bind(campaign_id)
        .execute(&state.db)
        .await?;

    let theme = crate::theme::resolve_theme(&merged);

    Ok(Json(json!({
        "status": "updated",
        "surface_config": merged,
        "theme": theme,
    })))
}

/// GET /api/v1/admin/domains
pub async fn list_domains(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let user_id = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid user ID".to_string()))?;

    // The accounts row that owns this account's custom domains. `accounts.tenant_id` is a legacy
    // free-form uuid that does NOT always name an accounts row, so the shared resolver is the only
    // way to get here: it keeps `accounts.tenant_id` when it really names an account and otherwise
    // falls back to the account's own id (kanban t_9a9ba67a).
    let tenant_id = resolve_owner_account_id(&state.db, user_id).await?;

    let domains = sqlx::query_as::<_, CustomDomain>(
        r#"SELECT id, tenant_id, domain, target_type, verification_token,
                      verified_at, ssl_provisioned_at, is_active, created_at, updated_at
               FROM custom_domains WHERE tenant_id = $1
               ORDER BY created_at DESC"#,
    )
    .bind(tenant_id)
    .fetch_all(&state.db)
    .await?;

    Ok(Json(json!({ "domains": domains })))
}

/// POST /api/v1/admin/domains
pub async fn register_domain(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<RegisterDomainInput>,
) -> Result<Json<Value>, AppError> {
    let user_id = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid user ID".to_string()))?;

    // Owner resolution is shared with the api_keys path — see `resolve_owner_account_id`
    // (kanban t_9a9ba67a). Binding `accounts.tenant_id` raw would file the row under a uuid no
    // account resolves to, so the account's own list would come back empty.
    let tenant_id = resolve_owner_account_id(&state.db, user_id).await?;

    let id = Uuid::new_v4();
    let target_type = body
        .target_type
        .unwrap_or_else(|| "incentiveswift".to_string());

    sqlx::query(
        r#"INSERT INTO custom_domains (id, tenant_id, domain, target_type)
           VALUES ($1, $2, $3, $4)"#,
    )
    .bind(id)
    .bind(tenant_id)
    .bind(&body.domain)
    .bind(&target_type)
    .execute(&state.db)
    .await?;

    let domain = sqlx::query_as::<_, CustomDomain>(
        r#"SELECT id, tenant_id, domain, target_type, verification_token,
                  verified_at, ssl_provisioned_at, is_active, created_at, updated_at
           FROM custom_domains WHERE id = $1"#,
    )
    .bind(id)
    .fetch_one(&state.db)
    .await?;

    Ok(Json(json!({
        "domain": domain,
        "verification_instructions": format!(
            "Add a TXT record for _swift-verify.{} with value '{}'",
            body.domain, domain.verification_token
        ),
    })))
}

/// DELETE /api/v1/admin/domains/{id}
pub async fn remove_domain(
    State(state): State<AppState>,
    Path(id): Path<String>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let domain_id =
        Uuid::parse_str(&id).map_err(|_| AppError::BadRequest("Invalid domain ID".to_string()))?;
    let user_id = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid user ID".to_string()))?;

    let tenant_id = resolve_owner_account_id(&state.db, user_id).await?;

    let result = sqlx::query("DELETE FROM custom_domains WHERE id = $1 AND tenant_id = $2")
        .bind(domain_id)
        .bind(tenant_id)
        .execute(&state.db)
        .await?;

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound("Domain not found".to_string()));
    }

    Ok(Json(json!({ "status": "removed", "id": id })))
}

/// POST /api/v1/admin/domains/{id}/verify
pub async fn verify_domain(
    State(state): State<AppState>,
    Path(id): Path<String>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let domain_id =
        Uuid::parse_str(&id).map_err(|_| AppError::BadRequest("Invalid domain ID".to_string()))?;
    let user_id = Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid user ID".to_string()))?;

    let tenant_id = resolve_owner_account_id(&state.db, user_id).await?;

    let domain = sqlx::query(
        r#"SELECT id, domain, verification_token, verified_at, is_active
           FROM custom_domains WHERE id = $1 AND tenant_id = $2"#,
    )
    .bind(domain_id)
    .bind(tenant_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::NotFound("Domain not found".to_string()))?;

    let domain_name: String = domain.get("domain");
    let verification_token: String = domain.get("verification_token");
    let verified_at: Option<chrono::DateTime<chrono::Utc>> = domain.get("verified_at");

    if verified_at.is_some() {
        return Ok(Json(json!({
            "status": "already_verified",
            "message": "This domain has already been verified"
        })));
    }

    // Try to verify via DNS TXT record lookup
    let verified = check_dns_verification(&domain_name, &verification_token).await;

    if verified {
        sqlx::query(
            r#"UPDATE custom_domains
               SET verified_at = now(), is_active = true, updated_at = now()
               WHERE id = $1"#,
        )
        .bind(domain_id)
        .execute(&state.db)
        .await?;

        Ok(Json(json!({
            "status": "verified",
            "domain": domain_name,
            "verified_at": chrono::Utc::now(),
        })))
    } else {
        Ok(Json(json!({
            "status": "pending",
            "domain": domain_name,
            "message": format!(
                "DNS verification record not found. Please add a TXT record for _swift-verify.{} with value '{}'",
                domain_name, verification_token
            ),
            "verification_token": verification_token,
        })))
    }
}

/// Attempt DNS TXT record lookup for domain verification.
async fn check_dns_verification(domain: &str, token: &str) -> bool {
    let lookup_name = format!("_swift-verify.{}", domain);
    let result = tokio::net::lookup_host(&lookup_name).await;
    match result {
        Ok(_) => {
            // DNS lookup succeeded — use system `dig` to check TXT records
            let output = std::process::Command::new("dig")
                .arg("TXT")
                .arg(&lookup_name)
                .arg("+short")
                .output();

            match output {
                Ok(out) => {
                    let stdout = String::from_utf8_lossy(&out.stdout);
                    stdout.contains(token)
                }
                Err(_) => {
                    // dig not available, assume verification is manual
                    false
                }
            }
        }
        Err(_) => false,
    }
}

/// GET /api/v1/admin/plans/{id}/domains
/// Check how many domains a plan tier allows.
pub async fn check_plan_domains(
    State(state): State<AppState>,
    Path(id): Path<String>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let plan_id =
        Uuid::parse_str(&id).map_err(|_| AppError::BadRequest("Invalid plan ID".to_string()))?;

    // Get plan to find matching plan_tier
    let plan = sqlx::query("SELECT slug FROM plans WHERE id = $1")
        .bind(plan_id)
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound("Plan not found".to_string()))?;

    let plan_slug: String = plan.get("slug");

    // Entitlements live in `tier_features` joined to `features` — the `feature_limits` table this
    // used to read does not exist in the live schema (`features.rs` documents its removal), so the
    // route 500'd on every call with `42P01 relation "feature_limits" does not exist`. Same shape
    // as `loyalty.rs::check_plan_loyalty` (fixed in t_cf7469bb) and `features::enforce_feature_limit`:
    // the tier is `plan_tiers`, and `plans.slug` is the join key (`plans` is the marketing/checkout
    // table; plan_tiers.slug == plans.slug for every plan row).
    //
    // Key: `custom_domains` — the key this statement already named, registered in `features` as the
    // surface domain gate. `surface_custom_domains` is a surface-flavoured duplicate with no reader
    // anywhere in the code, and `branding_custom_domain` is a branding entitlement, so neither is
    // the one being asked about here.
    //
    // Two conventions meet here and they agree: the GATE direction comes from
    // `access::feature_gate::has_feature_access` ("Missing row = false — feature not assigned to
    // that tier") and from `migrations-manual/register_feature_keys.sql` ("Assign nothing to free —
    // surface features require upgrade", which assigns `custom_domains` to enterprise only); the
    // NUMERIC direction comes from `features.rs`: limit_value NULL or -1 = no cap, 0 = not
    // available, positive = the cap. An explicit `enabled = false` overrides either.
    let row: Option<(Option<bool>, Option<i32>)> = sqlx::query_as(
        "SELECT tf.enabled, tf.limit_value
           FROM plan_tiers pt
           JOIN tier_features tf ON tf.tier_id = pt.id
           JOIN features f ON f.id = tf.feature_id
          WHERE pt.slug = $1 AND f.key = 'custom_domains'",
    )
    .bind(&plan_slug)
    .fetch_optional(&state.db)
    .await?;

    let feature_configured = row.is_some();
    let (allowed, limit) = match row {
        // Assigned to the tier, explicitly switched off — the disable wins over any cap.
        Some((Some(false), _)) => (false, 0_i64),
        // Assigned and enabled: the numeric cap refines it.
        // (`limit_value` is INT4, so it decodes as i32 — decoding it as i64 is a
        // `mismatched types; Rust type Option<i64> (as SQL type INT8) is not compatible with SQL
        // type INT4` 500, measured on this route.)
        Some((_, Some(l))) if l > 0 => (true, l as i64),
        // Assigned and enabled with no cap configured.
        Some((_, None)) | Some((_, Some(-1))) => (true, -1_i64),
        // Assigned, enabled, cap of 0 or less than -1: not available on this tier.
        Some((_, Some(_))) => (false, 0_i64),
        // No row for this tier: the feature is not assigned to it (upgrade required).
        None => (false, 0_i64),
    };

    Ok(Json(json!({
        "plan_id": plan_id,
        "plan_slug": plan_slug,
        "plan_tier": plan_slug,
        "custom_domains_allowed": allowed,
        "custom_domain_limit": limit,
        "feature_configured": feature_configured,
        "message": if allowed {
            if limit == -1 {
                "Unlimited custom domains".to_string()
            } else {
                format!("Up to {} custom domains allowed", limit)
            }
        } else {
            "Custom domains not included in this plan".to_string()
        },
    })))
}
