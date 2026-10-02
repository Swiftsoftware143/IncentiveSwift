//! Campaign handlers — list, get by slug, create.

use crate::access::feature_gate;
use crate::db::campaigns::{self, Campaign, IqsGate};
use crate::error::AppError;
use crate::handlers::tri_state::double_option;
use crate::security::auth::AuthenticatedUser;
use crate::state::AppState;
use axum::{
    extract::{Path, State},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

/// Resolve and validate an IQS-gate request into the write (kanban t_6c8d8e40).
///
/// Measured defect (live, 2026-10-01, kanban t_7f2d2995): `PUT /api/v1/campaigns/<slug>
/// {"iqs_funnel_id": null}` — the served IQS builder's own ungate arm (www-app/iqs.html,
/// `previouslyGated`) — answered **200** and kept the old value, so a campaign's survey gate could
/// never be detached. Two campaigns ended up pointing at one funnel, and because the submit path
/// (`delivery::coreswift_external::find_campaign_by_iqs_funnel`, `LIMIT 1`, no `ORDER BY`) then
/// resolved that funnel's campaign arbitrarily, IQS submissions were routed to whichever of the two
/// the planner happened to return first.
///
/// DECIDED here (the card's arm (a)) — one funnel gates at most one campaign. An absent field means
/// `Keep` (this request does not mention the column); `null` or a blank string means `Clear` (the
/// detach the builder sends); a funnel id means `Set`, but only when it parses as a UUID, names a
/// funnel that exists and belongs to THIS account, and no other campaign holds it (409).
/// A refusal rather than a silent move: no request may strip a survey from a campaign it never
/// mentioned, and the builder already detaches the old holder before attaching the new one, so the
/// UI flow is unchanged and its loop becomes belt-and-braces instead of the only guard.
///
/// The same rule exists as a partial unique index (`campaigns_iqs_funnel_id_uidx`,
/// migrations/20261002_iqs_one_campaign_per_funnel.sql), which is what closes the check-then-write
/// race; `db::campaigns::map_iqs_gate_unique_violation` turns that race into the identical 409.
async fn resolve_iqs_gate(
    state: &AppState,
    account_id: &uuid::Uuid,
    requested: Option<&Option<String>>,
    writing: Option<&uuid::Uuid>,
) -> Result<IqsGate, AppError> {
    let raw = match requested {
        None => return Ok(IqsGate::Keep),
        Some(None) => return Ok(IqsGate::Clear),
        Some(Some(raw)) => raw.trim(),
    };
    if raw.is_empty() {
        return Ok(IqsGate::Clear);
    }

    let funnel_id = uuid::Uuid::parse_str(raw).map_err(|_| {
        AppError::BadRequest(
            "iqs_funnel_id must be the id of an IQS funnel, or null to detach the survey."
                .to_string(),
        )
    })?;

    // A gate has to point at a funnel the caller can actually see: the column is a plain VARCHAR
    // with no FK, and `GET /campaigns/:slug/iqs-funnel-questions` then reads the funnel by id alone,
    // so an unvalidated write could attach a campaign to ANOTHER tenant's survey.
    let owned: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM iqs_funnels WHERE id = $1 AND account_id = $2")
            .bind(funnel_id)
            .bind(account_id)
            .fetch_one(&state.db)
            .await?;
    if owned == 0 {
        return Err(AppError::NotFound("IQS funnel not found".to_string()));
    }

    let holder: Option<String> = sqlx::query_scalar(
        "SELECT slug FROM campaigns WHERE iqs_funnel_id = $1 AND ($2::uuid IS NULL OR id <> $2) \
         ORDER BY created_at ASC, id ASC LIMIT 1",
    )
    .bind(funnel_id.to_string())
    .bind(writing.copied())
    .fetch_optional(&state.db)
    .await?;
    if let Some(other) = holder {
        return Err(AppError::Conflict(format!(
            "That survey is already attached to campaign '{other}'. A survey runs in one campaign \
             at a time — detach it there first."
        )));
    }

    Ok(IqsGate::Set(funnel_id))
}

/// Validate a caller-supplied `loyalty_program_id` BEFORE the write (kanban t_28431966).
///
/// Measured live 2026-10-02 (binary 5d0f5a81166787ed): `PUT /api/v1/campaigns/<slug>
/// {"loyalty_program_id":"<random uuid>"}` answered `500 {"error":"Internal server error"}` —
/// `campaigns_loyalty_program_id_fkey` raises SQLSTATE 23503 and nothing mapped it. Same rule as the
/// survey gate above (its sibling on this handler) and the same shape: the request boundary refuses
/// a bad id with a field-level 4xx BEFORE any write, so the row is left untouched.
///
/// The predicate is the console picker's OWN (src/handlers/loyalty.rs `list_programs`): the editor
/// offers `lp` where `c.account_id = $1 OR lp.campaign_id IS NULL`, so everything that screen offers
/// must stay writable and everything it does not offer is refused. That second clause matters: the
/// FK references `loyalty_programs(id)` GLOBALLY, so another tenant's real program SATISFIES it —
/// only a pre-write ownership check can refuse that (measured: it was accepted -> 200 before this).
///
/// Tri-state spelling is the writer's own (`Option<Option<Uuid>>`): only `Some(Some(id))` is a
/// caller-supplied pointer worth validating; `None` (absent) keeps and `Some(None)` (null) clears.
async fn resolve_loyalty_program_gate(
    state: &AppState,
    account_id: &uuid::Uuid,
    requested: Option<Option<uuid::Uuid>>,
) -> Result<(), AppError> {
    let Some(id) = requested.flatten() else {
        return Ok(());
    };
    let usable: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM loyalty_programs lp \
         LEFT JOIN campaigns c ON c.id = lp.campaign_id \
         WHERE lp.id = $1 AND (c.account_id = $2 OR lp.campaign_id IS NULL)",
    )
    .bind(id)
    .bind(account_id)
    .fetch_one(&state.db)
    .await?;
    if usable == 0 {
        // ONE message for "no such program" and "another account's program": a distinct response for
        // each would let a caller probe which ids exist (skill fk-write-refusal-field-level-4xx).
        return Err(AppError::NotFound(
            campaigns::LOYALTY_PROGRAM_REFUSAL.to_string(),
        ));
    }
    Ok(())
}

/// Does this caller act ACROSS accounts? (kanban t_734f1f94)
///
/// The operator audience is `admin`/`super_admin` — the same pair `security::auth::admin_guard`
/// admits. The role comes from a signed JWT claim or an `api_keys` row (`security::auth`), so a
/// tenant cannot mint it; an API-key caller carries the role `api_key` and is scoped like any tenant.
pub(crate) fn is_operator(user: &AuthenticatedUser) -> bool {
    user.role == "admin" || user.role == "super_admin"
}

/// Resolve the campaign named in the path for THIS caller (kanban t_734f1f94).
///
/// A tenant is scoped to its own `account_id`; a foreign slug/uuid is a 404. The operator audience
/// keeps acting across accounts, which is what the console's operator catalogue and the pre-existing
/// behaviour rely on. The scope value is the caller's RAW `account_id` — the value `list_campaigns`
/// filters on and `create_campaign` writes into the row — deliberately NOT
/// `resolve_owner_account_id`, which maps the multi-account tenants to their `accounts.tenant_id`
/// and would 404 a tenant on its own campaign (measured on a tenant's own
/// `campaigns.account_id` is its own id while its `accounts.tenant_id` names another row).
pub(crate) async fn campaign_for_caller(
    state: &AppState,
    ident: &str,
    user: &AuthenticatedUser,
) -> Result<Campaign, AppError> {
    if is_operator(user) {
        return if let Ok(id) = uuid::Uuid::parse_str(ident) {
            campaigns::get_campaign_by_id(&state.db, &id).await
        } else {
            campaigns::get_campaign_by_slug(&state.db, ident).await
        };
    }
    let account_id = uuid::Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;
    campaigns::resolve_campaign_for_account(&state.db, ident, &account_id).await
}

/// GET /api/v1/campaigns — list campaigns scoped to authenticated user's account.
pub async fn list_campaigns(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Result<Json<Value>, AppError> {
    let account_id = uuid::Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;
    let campaigns = campaigns::list_campaigns(&state.db, &account_id).await?;
    Ok(Json(json!({ "campaigns": campaigns })))
}

/// GET /api/v1/campaigns/:slug — the caller's OWN campaign (kanban t_734f1f94).
///
/// This route used to be registered without an `AuthenticatedUser` extractor, so it answered
/// ANONYMOUSLY about any campaign (`GET /campaigns/<slug>` -> 200 with the whole row, measured live
/// on the pre-fix binary) — the widest arm of the class. It is a console read: the served shells
/// read the public per-campaign contracts instead (`/api/v1/embed/campaign/:slug`, `/api/v1/play/:id`),
/// which are untouched and still answer anonymous callers.
pub async fn get_campaign(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(slug): Path<String>,
) -> Result<Json<Value>, AppError> {
    let campaign = campaign_for_caller(&state, &slug, &user).await?;
    Ok(Json(json!({ "campaign": campaign })))
}

/// GET /api/v1/campaigns/by-subdomain/:slug — public campaigns for a tenant subdomain.
///
/// Listed from the callers', not the tenant's, point of view — but the ROWS are the tenant's, so the
/// projection is redacted exactly like the other anonymous surface arms (kanban t_eeed1bba): the
/// whole `campaigns.config` blob used to answer here, carrying the tenant's Marketing Boost
/// credential to any caller with no token at all.
pub async fn get_campaigns_by_subdomain(
    State(state): State<AppState>,
    Path(t_slug): Path<String>,
) -> Result<Json<Value>, AppError> {
    let account_id = campaigns::get_account_by_slug(&state.db, &t_slug).await?;
    let mut campaigns = campaigns::list_campaigns(&state.db, &account_id).await?;
    for campaign in campaigns.iter_mut() {
        campaign.config = crate::security::public_projection::public_config(&campaign.config);
        // Same projection, second credential column (kanban t_10559717): the row serialises the
        // whole `delivery_config` jsonb, which carries the direct-API `api_key` / `webhook_url`
        // vocabulary. Anonymous route, so it is projected; the tenant's own authenticated list
        // (`GET /api/v1/campaigns`) keeps the raw column, as the IQS / delivery panels expect.
        campaign.delivery_config =
            crate::security::public_projection::public_delivery_config(&campaign.delivery_config);
    }
    Ok(Json(json!({ "campaigns": campaigns })))
}

/// Input for creating a campaign.
#[derive(Deserialize)]
pub struct CreateCampaignBody {
    pub name: String,
    pub r#type: String,
    pub tag_namespace: String,
    pub config: Option<Value>,
    pub outcome_tags: Option<Value>,
    pub delivery_method: Option<String>,
    pub delivery_config: Option<Value>,
    pub loyalty_program_id: Option<uuid::Uuid>,
    pub loyalty_points_per_play: Option<i32>,
    pub auto_enroll_loyalty: Option<bool>,
    pub theme: Option<Value>,
    /// IQS survey gate, tri-state: absent = leave it, `null` or `""` = detach, an id = attach.
    /// See `double_option` for why the plain `Option<Option<String>>` shape was not enough.
    #[serde(default, deserialize_with = "double_option")]
    pub iqs_funnel_id: Option<Option<String>>,
}

/// POST /api/v1/campaigns — create campaign (authenticated + feature-gated).
pub async fn create_campaign(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(body): Json<CreateCampaignBody>,
) -> Result<Json<Value>, AppError> {
    // Feature gate: check if account can create campaigns of this mechanic type.
    // Honors the `all_mechanics` catch-all with explicit-disable override.
    let account_id = uuid::Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;

    // ── Identity validation (kanban t_a56c03a1) ────────────────────────────────────────────
    // Measured live 2026-10-02 on binary f03d7fe8: this route answered 200 for `name: ""` and
    // created a campaign whose console row renders as a blank <td>, and 200 for `tag_namespace: ""`,
    // whose outcome tags are `_entrant`/`_winner` — one shared vocabulary for the whole account.
    // Both columns are NOT NULL with no default, so an empty string is the only way a caller can
    // omit them, and nothing rejected it.
    //
    // Arm picked by measurement: REJECT (400). The fleet's only caller, www-admin/index.html,
    // always sends `name` and turns a 400 into `alert(e.message)` — the same path the mechanic-403
    // takes (API.handle throws `new Error(d.error)`, Campaigns.save's catch alerts it) — so the
    // operator is told. Defaulting would silently create rows an operator still cannot tell apart.
    // The DB keeps the same invariant (campaigns_name_not_blank / campaigns_tag_namespace_not_blank
    // + campaigns_account_tag_namespace_uidx, migrations/20261002_campaign_identity_not_blank.sql).
    let name = body.name.trim();
    if name.is_empty() {
        return Err(AppError::BadRequest(
            "Campaign name is required.".to_string(),
        ));
    }
    let tag_namespace = body.tag_namespace.trim();
    if tag_namespace.is_empty() {
        return Err(AppError::BadRequest(
            "Campaign tag namespace is required.".to_string(),
        ));
    }
    if campaigns::tag_namespace_taken(&state.db, &account_id, tag_namespace).await? {
        return Err(AppError::BadRequest(format!(
            "Tag namespace '{}' is already used by another campaign in this account. \
             Every outcome tag of this campaign is prefixed with it, so it must be unique per account.",
            tag_namespace
        )));
    }

    let has_access =
        feature_gate::has_mechanic_access(&state, &user.account_id, &body.r#type).await?;
    if !has_access {
        return Err(AppError::Forbidden(format!(
            "Your plan does not include the '{}' mechanic. Upgrade to access this feature.",
            body.r#type
        )));
    }

    // A creation request may gate the new campaign in the same INSERT (kanban t_6c8d8e40). This
    // field used to be accepted and silently dropped — the same "write that cannot happen" the PUT
    // clear was. A campaign that does not exist yet can never be the funnel's current holder, so
    // only the id/ownership rules can refuse here.
    let iqs_gate = resolve_iqs_gate(&state, &account_id, body.iqs_funnel_id.as_ref(), None).await?;

    // The same rule applies on the creation door (kanban t_28431966): this INSERT binds the same
    // campaign.loyalty_program_id FK, so an id this caller cannot use must be refused here too
    // instead of surfacing the INSERT's 23503 as a 500. Reached only after the plan gate above, so
    // the mechanic-403 a plan-less tenant sees is unchanged.
    resolve_loyalty_program_gate(&state, &account_id, Some(body.loyalty_program_id)).await?;

    let input = campaigns::CreateCampaignInput {
        name: name.to_string(),
        r#type: body.r#type,
        tag_namespace: tag_namespace.to_string(),
        config: body.config,
        outcome_tags: body.outcome_tags,
        delivery_method: body.delivery_method,
        delivery_config: body.delivery_config,
        account_id,
        loyalty_program_id: body.loyalty_program_id,
        loyalty_points_per_play: body.loyalty_points_per_play,
        auto_enroll_loyalty: body.auto_enroll_loyalty,
        theme: body.theme,
        iqs_funnel_id: match iqs_gate {
            IqsGate::Set(fid) => Some(fid.to_string()),
            IqsGate::Keep | IqsGate::Clear => None,
        },
    };

    let campaign = campaigns::create_campaign(&state.db, &input).await?;
    Ok(Json(json!({ "campaign": campaign })))
}

/// Input for updating a campaign.
#[derive(Deserialize)]
pub struct UpdateCampaignBody {
    pub name: Option<String>,
    pub config: Option<Value>,
    pub outcome_tags: Option<Value>,
    pub delivery_method: Option<String>,
    pub delivery_config: Option<Value>,
    pub branding: Option<Value>,
    /// Tri-state loyalty program (kanban t_2371942d): absent = leave it, `null` = detach, an id =
    /// attach. The plain `Option<Option<Uuid>>` read `null` as "keep", so a campaign's loyalty
    /// program could never be detached. See `double_option`.
    #[serde(default, deserialize_with = "double_option")]
    pub loyalty_program_id: Option<Option<uuid::Uuid>>,
    pub loyalty_points_per_play: Option<i32>,
    pub auto_enroll_loyalty: Option<bool>,
    pub theme: Option<Value>,
    /// IQS survey gate, tri-state: absent = leave it, `null` or `""` = detach, an id = attach.
    /// See `double_option` for why the plain `Option<Option<String>>` shape was not enough.
    #[serde(default, deserialize_with = "double_option")]
    pub iqs_funnel_id: Option<Option<String>>,
}

/// PUT /api/v1/campaigns/:slug — update campaign (authenticated).
pub async fn update_campaign(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(slug): Path<String>,
    Json(body): Json<UpdateCampaignBody>,
) -> Result<Json<Value>, AppError> {
    // Resolve the campaign by slug or UUID, scoped to the caller (kanban t_734f1f94): a tenant may
    // only write its own row, the operator audience still acts across accounts.
    let campaign = campaign_for_caller(&state, &slug, &user).await?;

    // A rename to "" (or to whitespace) left the row exactly as blank as an empty create did:
    // measured live 2026-10-02 (binary f03d7fe8) `PUT /api/v1/campaigns/:slug {"name":""}` -> 200
    // with `name=''` in the DB, and the console's Edit form sends whatever the operator typed
    // (www-admin/index.html doSave: `{ name: cf.name, config }`), so clearing the Name box and
    // saving produced a nameless campaign. Same refusal and same shape as the create path
    // (kanban t_a56c03a1). A non-empty rename is still trimmed and stored.
    let new_name: Option<String> = match body.name.as_deref() {
        Some(n) => {
            let trimmed = n.trim();
            if trimmed.is_empty() {
                return Err(AppError::BadRequest(
                    "Campaign name is required.".to_string(),
                ));
            }
            Some(trimmed.to_string())
        }
        None => None,
    };

    // Merge branding into existing campaign config if provided
    let config: Option<Value> = if let Some(ref branding) = body.branding {
        let mut merged = body.config.clone().unwrap_or(campaign.config);
        if let Some(obj) = merged.as_object_mut() {
            obj.insert("branding".to_string(), branding.clone());
        }
        Some(merged)
    } else {
        body.config.clone()
    };

    // Validate the requested survey gate BEFORE any write: a refusal here (bad id / foreign funnel /
    // funnel already held) must leave the row untouched (kanban t_6c8d8e40).
    let iqs_gate = resolve_iqs_gate(
        &state,
        &campaign.account_id,
        body.iqs_funnel_id.as_ref(),
        Some(&campaign.id),
    )
    .await?;

    // Validate the requested loyalty program BEFORE any write (kanban t_28431966), same shape as the
    // survey gate above: an id this caller cannot use must never reach the FK (a 500), and the
    // refusal must leave the row untouched.
    resolve_loyalty_program_gate(&state, &campaign.account_id, body.loyalty_program_id).await?;

    let campaign = campaigns::update_campaign(
        &state.db,
        &campaign.id,
        new_name.as_deref(),
        config.as_ref(),
        body.outcome_tags.as_ref(),
        body.delivery_method.as_deref(),
        body.delivery_config.as_ref(),
        body.loyalty_program_id,
        body.loyalty_points_per_play,
        body.auto_enroll_loyalty,
        iqs_gate,
    )
    .await?;

    // Persist an optional theme into surface_config.theme (deep merge).
    if let Some(ref theme) = body.theme {
        campaigns::merge_campaign_theme(&state.db, &campaign.id, theme).await?;
    }

    Ok(Json(json!({ "campaign": campaign })))
}

/// DELETE /api/v1/campaigns/:slug — delete campaign by slug (authenticated).
pub async fn delete_campaign_by_id(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(slug): Path<String>,
) -> Result<Json<Value>, AppError> {
    // Slug or UUID, scoped to the caller (kanban t_734f1f94) — a foreign campaign is a 404, so a
    // guessed slug can no longer delete another tenant's row.
    let campaign = campaign_for_caller(&state, &slug, &user).await?;
    let deleted = campaigns::delete_campaign(&state.db, &campaign.id).await?;
    if !deleted {
        return Err(AppError::NotFound("Campaign not found".to_string()));
    }

    Ok(Json(json!({ "status": "deleted" })))
}

/// POST /api/v1/campaigns/:slug/clone — Clone a campaign with all config
pub async fn clone_campaign(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Path(slug): Path<String>,
) -> Result<Json<Value>, AppError> {
    // The SOURCE must be the caller's own campaign (kanban t_734f1f94): before this check any
    // tenant could clone another tenant's campaign — config, delivery_config, theme and all — into
    // its own account (measured live: a foreign slug cloned -> 200, the copy landed under the
    // caller's account_id).
    let original = campaign_for_caller(&state, &slug, &user).await?;
    let new_name = format!("{} (Copy)", original.name);
    let new_slug = campaigns::generate_clone_slug(&original.name);
    let account_id = uuid::Uuid::parse_str(&user.account_id)
        .map_err(|_| AppError::BadRequest("Invalid account ID".to_string()))?;
    let new_id = uuid::Uuid::new_v4();

    // The clone must NOT inherit its source's `tag_namespace` (kanban t_a56c03a1): every outcome tag
    // is `{tag_namespace}_*` and `tags` is UNIQUE per account, so copying it made the copy's audience
    // the same tag rows as the original's — the two campaigns were indistinguishable downstream.
    // Derive a fresh, collision-checked namespace (the unique index is the backstop).
    let new_tag_namespace =
        campaigns::clone_tag_namespace(&state.db, &account_id, &original.tag_namespace).await?;

    sqlx::query(
        r#"INSERT INTO campaigns (id, account_id, name, slug, type, status, config, tag_namespace, outcome_tags, delivery_method, delivery_config, loyalty_program_id, loyalty_points_per_play, auto_enroll_loyalty)
           VALUES ($1, $2, $3, $4, $5, 'draft', $6, $7, $8, $9, $10, $11, $12, $13)"#
    )
    .bind(new_id)
    .bind(account_id)
    .bind(&new_name)
    .bind(&new_slug)
    .bind(&original.r#type)
    .bind(&original.config)
    .bind(&new_tag_namespace)
    .bind(&original.outcome_tags)
    .bind(&original.delivery_method)
    .bind(&original.delivery_config)
    .bind(original.loyalty_program_id)
    .bind(original.loyalty_points_per_play)
    .bind(original.auto_enroll_loyalty)
    .execute(&state.db)
    .await?;

    let cloned = campaigns::get_campaign_by_slug(&state.db, &new_slug).await?;
    Ok(Json(json!({ "campaign": cloned })))
}
