//! Campaign database operations.

use crate::error::AppError;
use serde_json::Value as JsonValue;
use sqlx::PgPool;
use uuid::Uuid;

/// Valid mechanic types.
pub const VALID_MECHANIC_TYPES: &[&str] = &[
    "score_reveal",
    "spin_wheel",
    "scratch_card",
    "personality",
    "calculator",
    "mystery",
    "countdown",
    "poll",
    "chat",
    "leaderboard",
    "raffle",
    "long_form_qualifier",
    "quiz",
    "loyalty",
    "b2b_loyalty",
];

/// Input for creating a campaign.
#[derive(Debug, serde::Deserialize)]
pub struct CreateCampaignInput {
    pub name: String,
    pub r#type: String,
    pub tag_namespace: String,
    pub config: Option<JsonValue>,
    pub outcome_tags: Option<JsonValue>,
    pub delivery_method: Option<String>,
    pub delivery_config: Option<JsonValue>,
    pub account_id: Uuid,
    pub loyalty_program_id: Option<Uuid>,
    pub loyalty_points_per_play: Option<i32>,
    pub auto_enroll_loyalty: Option<bool>,
    pub theme: Option<JsonValue>,
    /// IQS funnel to gate this campaign with at creation. `None` = no gate. Written in the same
    /// INSERT as the rest of the row (kanban t_6c8d8e40): the route used to ACCEPT this field and
    /// silently drop it, which is the same "write that cannot happen" the PUT clear was.
    pub iqs_funnel_id: Option<String>,
}

/// A campaign record.
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct Campaign {
    pub id: uuid::Uuid,
    pub name: String,
    pub slug: String,
    pub r#type: String,
    pub status: String,
    #[serde(rename = "config")]
    pub config: serde_json::Value,
    pub tag_namespace: String,
    pub outcome_tags: serde_json::Value,
    pub delivery_method: String,
    pub delivery_config: serde_json::Value,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub account_id: uuid::Uuid,
    /// Optional loyalty program linked to this campaign
    pub loyalty_program_id: Option<uuid::Uuid>,
    /// Points awarded per play that goes to the linked loyalty program
    pub loyalty_points_per_play: i32,
    /// Auto-enroll players into the loyalty program when they play
    pub auto_enroll_loyalty: bool,
    /// Optional IQS funnel linked to this campaign (stores funnel UUID as string; column is VARCHAR)
    pub iqs_funnel_id: Option<String>,
}

/// Validate mechanic type string.
pub fn validate_mechanic_type(type_str: &str) -> bool {
    VALID_MECHANIC_TYPES.contains(&type_str)
}

/// Is `tag_namespace` already taken by another campaign in this account? (kanban t_a56c03a1)
///
/// Case-INSENSITIVE on purpose: every outcome tag a campaign applies is `{tag_namespace}_*`
/// (`entries.rs` determine_outcome, `score_reveal_handler.rs`, `long_form_qualifier_handler.rs`,
/// `mystery_handler.rs`) and `public.tags` is `UNIQUE (account_id, lower(name))`, so `Summer` and
/// `summer` name the SAME tag vocabulary. Two campaigns sharing it makes their audiences
/// indistinguishable. The `campaigns_account_tag_namespace_uidx` index
/// (migrations/20261002_campaign_identity_not_blank.sql) is the class-wide backstop; this is the
/// check that turns the collision into a 400 instead of a unique-violation 500.
pub async fn tag_namespace_taken(
    pool: &PgPool,
    account_id: &Uuid,
    tag_namespace: &str,
) -> Result<bool, AppError> {
    let taken: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM campaigns \
         WHERE account_id = $1 AND lower(tag_namespace) = lower($2) LIMIT 1",
    )
    .bind(account_id)
    .bind(tag_namespace)
    .fetch_optional(pool)
    .await?;

    Ok(taken.is_some())
}

/// A tag namespace for a CLONE that cannot collide with its source (kanban t_a56c03a1).
///
/// `clone_campaign` used to copy `tag_namespace` verbatim, so EVERY clone produced a second campaign
/// whose outcome tags are the same strings as its source's — and since `tags` is unique per account
/// (and `entries.tags_applied` is what segments an audience), the copy and the original were
/// indistinguishable. Bounded retry against the live table; the unique index is the backstop.
pub async fn clone_tag_namespace(
    pool: &PgPool,
    account_id: &Uuid,
    source: &str,
) -> Result<String, AppError> {
    for _ in 0..8 {
        let candidate = format!("{}-clone-{}", source, &Uuid::new_v4().to_string()[..8]);
        if !tag_namespace_taken(pool, account_id, &candidate).await? {
            return Ok(candidate);
        }
    }
    Ok(Uuid::new_v4().to_string())
}

/// Get a campaign by its slug.
pub async fn get_campaign_by_slug(pool: &PgPool, slug: &str) -> Result<Campaign, AppError> {
    let campaign = sqlx::query_as::<_, Campaign>(
        r#"SELECT id, name, slug, type as "type", status,
                  config, tag_namespace,
                  outcome_tags,
                  delivery_method, delivery_config,
                  created_at,
                  account_id,
                  loyalty_program_id,
                  loyalty_points_per_play,
                  auto_enroll_loyalty,
                  iqs_funnel_id
           FROM campaigns WHERE slug = $1"#,
    )
    .bind(slug)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::NotFound("Campaign not found".to_string()))?;

    Ok(campaign)
}

/// Get a campaign by its slug, but ONLY when it belongs to `account_id` — otherwise 404.
///
/// SECURITY (kanban t_b7f3c191): `get_campaign_by_slug` above takes no caller, so every route that
/// resolves a campaign from a path slug alone answers about another account's campaign. This is the
/// scoped variant; the integration-binding routes use it so a foreign slug cannot reach the
/// `integration_targets` rows behind it. 404 (never 403) is this app's convention: a 403 would
/// confirm the slug exists somewhere.
pub async fn get_campaign_by_slug_for_account(
    pool: &PgPool,
    slug: &str,
    account_id: &Uuid,
) -> Result<Campaign, AppError> {
    let campaign = sqlx::query_as::<_, Campaign>(
        r#"SELECT id, name, slug, type as "type", status,
                  config, tag_namespace,
                  outcome_tags,
                  delivery_method, delivery_config,
                  created_at,
                  account_id,
                  loyalty_program_id,
                  loyalty_points_per_play,
                  auto_enroll_loyalty,
                  iqs_funnel_id
           FROM campaigns WHERE slug = $1 AND account_id = $2"#,
    )
    .bind(slug)
    .bind(account_id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::NotFound("Campaign not found".to_string()))?;

    Ok(campaign)
}

/// List campaigns scoped to an account.
pub async fn list_campaigns(pool: &PgPool, account_id: &Uuid) -> Result<Vec<Campaign>, AppError> {
    let campaigns = sqlx::query_as::<_, Campaign>(
        r#"SELECT id, name, slug, type as "type", status,
                  config, tag_namespace,
                  outcome_tags,
                  delivery_method, delivery_config,
                  created_at,
                  account_id,
                  loyalty_program_id,
                  loyalty_points_per_play,
                  auto_enroll_loyalty,
                  iqs_funnel_id
           FROM campaigns WHERE account_id = $1
           ORDER BY created_at DESC"#,
    )
    .bind(account_id)
    .fetch_all(pool)
    .await?;

    Ok(campaigns)
}

/// Look up an account by its subdomain slug.
pub async fn get_account_by_slug(pool: &PgPool, slug: &str) -> Result<Uuid, AppError> {
    let account_id: Option<Uuid> = sqlx::query_scalar("SELECT id FROM accounts WHERE slug = $1")
        .bind(slug)
        .fetch_optional(pool)
        .await?
        .flatten();

    account_id.ok_or_else(|| AppError::NotFound("Tenant not found".to_string()))
}

/// Create a new campaign.
pub async fn create_campaign(
    pool: &PgPool,
    input: &CreateCampaignInput,
) -> Result<Campaign, AppError> {
    // Validate mechanic type
    if !validate_mechanic_type(&input.r#type) {
        return Err(AppError::BadRequest(format!(
            "Invalid mechanic type: {}. Must be one of: {:?}",
            input.r#type, VALID_MECHANIC_TYPES
        )));
    }

    // Generate slug from name
    let slug = generate_slug(&input.name);

    let id = Uuid::new_v4();
    let delivery_method = input
        .delivery_method
        .clone()
        .unwrap_or_else(|| "webhook".to_string());
    let config = input
        .config
        .clone()
        .unwrap_or_else(|| serde_json::json!({}));
    let outcome_tags = input
        .outcome_tags
        .clone()
        .unwrap_or_else(|| serde_json::json!({}));
    let delivery_config = input
        .delivery_config
        .clone()
        .unwrap_or_else(|| serde_json::json!({}));

    let loyalty_program_id = input.loyalty_program_id;
    let loyalty_points_per_play = input.loyalty_points_per_play.unwrap_or(0);
    let auto_enroll_loyalty = input.auto_enroll_loyalty.unwrap_or(false);
    let surface_config = match &input.theme {
        Some(t) => serde_json::json!({ "theme": t }),
        None => serde_json::json!({}),
    };

    sqlx::query(
        r#"INSERT INTO campaigns (id, account_id, name, slug, type, status, config, tag_namespace, outcome_tags, delivery_method, delivery_config, loyalty_program_id, loyalty_points_per_play, auto_enroll_loyalty, surface_config, iqs_funnel_id)
           VALUES ($1, $2, $3, $4, $5, 'active', $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)"#
    )
    .bind(id)
    .bind(input.account_id)
    .bind(&input.name)
    .bind(&slug)
    .bind(&input.r#type)
    .bind(&config)
    .bind(&input.tag_namespace)
    .bind(&outcome_tags)
    .bind(&delivery_method)
    .bind(&delivery_config)
    .bind(loyalty_program_id)
    .bind(loyalty_points_per_play)
    .bind(auto_enroll_loyalty)
    .bind(&surface_config)
    .bind(input.iqs_funnel_id.as_deref())
    .execute(pool)
    .await
    .map_err(map_iqs_gate_unique_violation)?;

    // Fetch back the created campaign
    get_campaign_by_slug(pool, &slug).await
}

/// Get a campaign by its id.
pub async fn get_campaign_by_id(pool: &PgPool, id: &Uuid) -> Result<Campaign, AppError> {
    let campaign = sqlx::query_as::<_, Campaign>(
        r#"SELECT id, name, slug, type as "type", status,
                  config, tag_namespace,
                  outcome_tags,
                  delivery_method, delivery_config,
                  created_at,
                  account_id,
                  loyalty_program_id,
                  loyalty_points_per_play,
                  auto_enroll_loyalty,
                  iqs_funnel_id
           FROM campaigns WHERE id = $1"#,
    )
    .bind(id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::NotFound("Campaign not found".to_string()))?;

    Ok(campaign)
}

/// The IQS gate a campaign write asked for (kanban t_6c8d8e40).
///
/// Tri-state on purpose: "the field was absent" and "the field was explicitly null" are different
/// instructions, and collapsing them is exactly the bug — `{"iqs_funnel_id": null}` was read as
/// "keep", so a campaign's survey could never be detached.
#[derive(Debug, Clone, Copy)]
pub enum IqsGate {
    /// The request did not mention the gate — leave `campaigns.iqs_funnel_id` untouched.
    Keep,
    /// Explicit JSON null / blank string — detach the survey.
    Clear,
    /// Attach this funnel (already validated by the handler: it exists, it belongs to the campaign's
    /// account, and no other campaign holds it).
    Set(Uuid),
}

/// Turn the DB backstop for one-funnel-one-campaign into the same 409 the handler's pre-check
/// answers (kanban t_6c8d8e40).
///
/// `campaigns_iqs_funnel_id_uidx` (migrations/20261002_iqs_one_campaign_per_funnel.sql) can only
/// fire when two writers race past the pre-check; without this mapping the loser would see a 500 for
/// a request the API already knows how to explain.
fn map_iqs_gate_unique_violation(e: sqlx::Error) -> AppError {
    if let sqlx::Error::Database(db) = &e {
        if db.constraint() == Some("campaigns_iqs_funnel_id_uidx") {
            return AppError::Conflict(
                "That survey is already attached to another campaign. A survey runs in one campaign \
                 at a time — detach it from the other campaign first."
                    .to_string(),
            );
        }
    }
    AppError::from(e)
}

/// Update a campaign's name, config, and other fields.
pub async fn update_campaign(
    pool: &PgPool,
    id: &Uuid,
    name: Option<&str>,
    config: Option<&JsonValue>,
    outcome_tags: Option<&JsonValue>,
    delivery_method: Option<&str>,
    delivery_config: Option<&JsonValue>,
    loyalty_program_id: Option<Option<Uuid>>,
    loyalty_points_per_play: Option<i32>,
    auto_enroll_loyalty: Option<bool>,
    iqs_funnel_id: IqsGate,
) -> Result<Campaign, AppError> {
    let existing = get_campaign_by_id(pool, id).await?;

    let new_name = name.unwrap_or(&existing.name);
    let new_config = config.unwrap_or(&existing.config);
    let new_outcome_tags = outcome_tags.unwrap_or(&existing.outcome_tags);
    let new_delivery_method = delivery_method.unwrap_or(&existing.delivery_method);
    let new_delivery_config = delivery_config.unwrap_or(&existing.delivery_config);
    let new_loyalty_program_id = loyalty_program_id.unwrap_or(existing.loyalty_program_id);
    let new_loyalty_points_per_play =
        loyalty_points_per_play.unwrap_or(existing.loyalty_points_per_play);
    let new_auto_enroll_loyalty = auto_enroll_loyalty.unwrap_or(existing.auto_enroll_loyalty);
    // Keep / Clear / Set — the three instructions the request boundary now distinguishes
    // (kanban t_6c8d8e40). `unwrap_or(existing)` used to collapse "null" into "absent" here.
    let new_iqs_funnel_id: Option<String> = match iqs_funnel_id {
        IqsGate::Keep => existing.iqs_funnel_id.clone(),
        IqsGate::Clear => None,
        IqsGate::Set(fid) => Some(fid.to_string()),
    };

    sqlx::query(
        r#"UPDATE campaigns
           SET name = $1, config = $2, outcome_tags = $3,
               delivery_method = $4, delivery_config = $5,
               loyalty_program_id = $6,
               loyalty_points_per_play = $7,
               auto_enroll_loyalty = $8,
               iqs_funnel_id = $10
           WHERE id = $9"#,
    )
    .bind(new_name)
    .bind(new_config)
    .bind(new_outcome_tags)
    .bind(new_delivery_method)
    .bind(new_delivery_config)
    .bind(new_loyalty_program_id)
    .bind(new_loyalty_points_per_play)
    .bind(new_auto_enroll_loyalty)
    .bind(id)
    .bind(new_iqs_funnel_id)
    .execute(pool)
    .await
    .map_err(map_iqs_gate_unique_violation)?;

    get_campaign_by_id(pool, id).await
}

/// Delete a campaign by id.
pub async fn delete_campaign(pool: &PgPool, id: &Uuid) -> Result<bool, AppError> {
    let result = sqlx::query("DELETE FROM campaigns WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;

    Ok(result.rows_affected() > 0)
}

/// Merge a theme object into a campaign's `surface_config.theme` (deep merge),
/// preserving other surface_config keys and any existing theme keys not
/// present in the incoming object.
pub async fn merge_campaign_theme(
    pool: &PgPool,
    id: &Uuid,
    theme: &JsonValue,
) -> Result<(), AppError> {
    let existing: Option<JsonValue> =
        sqlx::query_scalar("SELECT surface_config FROM campaigns WHERE id = $1")
            .bind(id)
            .fetch_optional(pool)
            .await?
            .flatten();

    let mut surface = existing.unwrap_or_else(|| serde_json::json!({}));
    let mut theme_obj = surface
        .get("theme")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    crate::theme::deep_merge(&mut theme_obj, theme);

    if let Some(obj) = surface.as_object_mut() {
        obj.insert("theme".to_string(), theme_obj);
    }

    sqlx::query("UPDATE campaigns SET surface_config = $1 WHERE id = $2")
        .bind(&surface)
        .bind(id)
        .execute(pool)
        .await?;

    Ok(())
}

/// Generate a clone slug by appending a short id to a slugified name.
pub fn generate_clone_slug(name: &str) -> String {
    let base: String = name
        .to_lowercase()
        .chars()
        .map(|c| match c {
            'a'..='z' | '0'..='9' | '-' => c,
            ' ' | '_' => '-',
            _ => '-',
        })
        .collect();
    let base: String = base
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if base.is_empty() {
        Uuid::new_v4().to_string()
    } else {
        format!("{}-clone-{}", base, &Uuid::new_v4().to_string()[..6])
    }
}

/// Generate a URL-safe slug from a name.
fn generate_slug(name: &str) -> String {
    let slug: String = name
        .to_lowercase()
        .chars()
        .map(|c| match c {
            'a'..='z' | '0'..='9' | '-' => c,
            ' ' | '_' => '-',
            _ => '-',
        })
        .collect();

    // Trim leading/trailing hyphens and collapse multiple hyphens
    let slug: String = slug
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");

    if slug.is_empty() {
        Uuid::new_v4().to_string()
    } else {
        format!("{}-{}", slug, &Uuid::new_v4().to_string()[..8])
    }
}
