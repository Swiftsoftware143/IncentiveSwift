//! Campaign handlers — list, get by slug, create.

use crate::access::feature_gate;
use crate::db::campaigns::{self, IqsGate};
use crate::error::AppError;
use crate::security::auth::AuthenticatedUser;
use crate::state::AppState;
use axum::{
    extract::{Path, State},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

/// Deserialize a tri-state field: absent -> `None`, JSON `null` -> `Some(None)`, value ->
/// `Some(Some(v))` (kanban t_6c8d8e40).
///
/// `Option<Option<T>>` on its own does NOT give this: serde's `Option` impl answers the OUTER `None`
/// for `null`, so `{"iqs_funnel_id": null}` — the served IQS builder's own ungate arm — was
/// indistinguishable from "the field was not sent" and `db::campaigns::update_campaign` read it as
/// "keep what is there". There was no spelling of that request which cleared the column.
///
/// `deserialize_with` is only invoked when the key IS present, and the inner `Option` answers `None`
/// for `null`; the outer `Some` is what records "the caller spoke". `default` covers the absent key.
fn double_option<'de, T, D>(de: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Deserialize::deserialize(de).map(Some)
}

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

/// GET /api/v1/campaigns/:slug — public, cacheable.
pub async fn get_campaign(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> Result<Json<Value>, AppError> {
    let campaign = campaigns::get_campaign_by_slug(&state.db, &slug).await?;
    Ok(Json(json!({ "campaign": campaign })))
}

/// GET /api/v1/campaigns/by-subdomain/:slug — public campaigns for a tenant subdomain.
pub async fn get_campaigns_by_subdomain(
    State(state): State<AppState>,
    Path(t_slug): Path<String>,
) -> Result<Json<Value>, AppError> {
    let account_id = campaigns::get_account_by_slug(&state.db, &t_slug).await?;
    let campaigns = campaigns::list_campaigns(&state.db, &account_id).await?;
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
    // Resolve campaign by slug or UUID
    let campaign = if let Ok(id) = uuid::Uuid::parse_str(&slug) {
        campaigns::get_campaign_by_id(&state.db, &id).await
    } else {
        campaigns::get_campaign_by_slug(&state.db, &slug).await
    };
    let campaign = campaign?;

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
    // Try as UUID first, then as slug
    let campaign = if let Ok(id) = uuid::Uuid::parse_str(&slug) {
        campaigns::get_campaign_by_id(&state.db, &id).await
    } else {
        campaigns::get_campaign_by_slug(&state.db, &slug).await
    };

    let campaign = campaign?;
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
    let original = campaigns::get_campaign_by_slug(&state.db, &slug).await?;
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
