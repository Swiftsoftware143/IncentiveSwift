//! Campaign database operations.

use crate::error::AppError;
use sqlx::PgPool;
use serde_json::Value as JsonValue;
use uuid::Uuid;

/// Valid mechanic types.
pub const VALID_MECHANIC_TYPES: &[&str] = &[
    "score_reveal", "spin_wheel", "scratch_card", "personality",
    "calculator", "mystery", "countdown", "poll", "chat",
    "leaderboard", "raffle", "long_form_qualifier", "quiz",
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
}

/// Validate mechanic type string.
pub fn validate_mechanic_type(type_str: &str) -> bool {
    VALID_MECHANIC_TYPES.contains(&type_str)
}

/// Get a campaign by its slug.
pub async fn get_campaign_by_slug(
    pool: &PgPool,
    slug: &str,
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
                  auto_enroll_loyalty
           FROM campaigns WHERE slug = $1"#
    )
    .bind(slug)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::NotFound("Campaign not found".to_string()))?;

    Ok(campaign)
}

/// List campaigns scoped to an account.
pub async fn list_campaigns(
    pool: &PgPool,
    account_id: &Uuid,
) -> Result<Vec<Campaign>, AppError> {
    let campaigns = sqlx::query_as::<_, Campaign>(
        r#"SELECT id, name, slug, type as "type", status,
                  config, tag_namespace,
                  outcome_tags,
                  delivery_method, delivery_config,
                  created_at,
                  account_id,
                  loyalty_program_id,
                  loyalty_points_per_play,
                  auto_enroll_loyalty
           FROM campaigns WHERE account_id = $1
           ORDER BY created_at DESC"#
    )
    .bind(account_id)
    .fetch_all(pool)
    .await?;

    Ok(campaigns)
}

/// Look up an account by its subdomain slug.
pub async fn get_account_by_slug(
    pool: &PgPool,
    slug: &str,
) -> Result<Uuid, AppError> {
    let account_id: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM accounts WHERE slug = $1"
    )
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
    let delivery_method = input.delivery_method.clone().unwrap_or_else(|| "webhook".to_string());
    let config = input.config.clone().unwrap_or_else(|| serde_json::json!({}));
    let outcome_tags = input.outcome_tags.clone().unwrap_or_else(|| serde_json::json!({}));
    let delivery_config = input.delivery_config.clone().unwrap_or_else(|| serde_json::json!({}));

    let loyalty_program_id = input.loyalty_program_id;
    let loyalty_points_per_play = input.loyalty_points_per_play.unwrap_or(0);
    let auto_enroll_loyalty = input.auto_enroll_loyalty.unwrap_or(false);

    sqlx::query(
        r#"INSERT INTO campaigns (id, account_id, name, slug, type, status, config, tag_namespace, outcome_tags, delivery_method, delivery_config, loyalty_program_id, loyalty_points_per_play, auto_enroll_loyalty)
           VALUES ($1, $2, $3, $4, $5, 'active', $6, $7, $8, $9, $10, $11, $12, $13)"#
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
    .execute(pool)
    .await?;

    // Fetch back the created campaign
    get_campaign_by_slug(pool, &slug).await
}

/// Get a campaign by its id.
pub async fn get_campaign_by_id(
    pool: &PgPool,
    id: &Uuid,
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
                  auto_enroll_loyalty
           FROM campaigns WHERE id = $1"#
    )
    .bind(id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::NotFound("Campaign not found".to_string()))?;

    Ok(campaign)
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
) -> Result<Campaign, AppError> {
    let existing = get_campaign_by_id(pool, id).await?;

    let new_name = name.unwrap_or(&existing.name);
    let new_config = config.unwrap_or(&existing.config);
    let new_outcome_tags = outcome_tags.unwrap_or(&existing.outcome_tags);
    let new_delivery_method = delivery_method.unwrap_or(&existing.delivery_method);
    let new_delivery_config = delivery_config.unwrap_or(&existing.delivery_config);
    let new_loyalty_program_id = loyalty_program_id.unwrap_or(existing.loyalty_program_id);
    let new_loyalty_points_per_play = loyalty_points_per_play.unwrap_or(existing.loyalty_points_per_play);
    let new_auto_enroll_loyalty = auto_enroll_loyalty.unwrap_or(existing.auto_enroll_loyalty);

    sqlx::query(
        r#"UPDATE campaigns
           SET name = $1, config = $2, outcome_tags = $3,
               delivery_method = $4, delivery_config = $5,
               loyalty_program_id = $6,
               loyalty_points_per_play = $7,
               auto_enroll_loyalty = $8
           WHERE id = $9"#
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
    .execute(pool)
    .await?;

    get_campaign_by_id(pool, id).await
}

/// Delete a campaign by id.
pub async fn delete_campaign(
    pool: &PgPool,
    id: &Uuid,
) -> Result<bool, AppError> {
    let result = sqlx::query("DELETE FROM campaigns WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;

    Ok(result.rows_affected() > 0)
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
