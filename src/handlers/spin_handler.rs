//! Spin wheel API handlers for IncentiveSwift prize pool draws.
//!
//! Includes prize delivery (email/redirect) and integration hub firing.

use crate::db::campaigns;
use crate::db::contacts;
use crate::error::AppError;
use crate::handlers::campaign_integrations;
use crate::mechanics::prize_draw;
use crate::state::AppState;
use axum::{
    extract::{Path, State},
    http::HeaderMap,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Available providers for prize delivery delivery_templates.
const DELIVERY_PROVIDERS: &[&str] = &[
    "mailgun",
    "sendgrid",
    "sendiio",
    "letterman",
    "nexweave",
    "sam_gov",
];

/// Generate a short random hex string for redemption codes.
fn generate_redemption_code() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let hex_bytes: String = (0..4).map(|_| format!("{:02x}", rng.gen::<u8>())).collect();
    format!("REDEEM-{}", hex_bytes.to_uppercase())
}

// ---------------------------------------------------------------------------
// Request / Response types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct SpinRequestBody {
    /// Existing contact UUID, or omit to create an anonymous contact
    pub contact_id: Option<Uuid>,
    /// When creating an anonymous contact, optionally provide email/phone
    pub email: Option<String>,
    pub phone: Option<String>,
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    pub website: Option<String>,
    pub answers: Option<Value>,
    /// Source tracking fields
    pub utm_source: Option<String>,
    pub utm_medium: Option<String>,
    pub utm_campaign: Option<String>,
    pub referrer_url: Option<String>,
    pub page_url: Option<String>,
}

#[derive(Deserialize)]
pub struct SpinStatusQuery {
    pub contact_id: Option<Uuid>,
    pub email: Option<String>,
    pub phone: Option<String>,
}

#[derive(Deserialize)]
pub struct WinsQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub redeemed_only: Option<bool>,
}

#[derive(serde::Serialize, sqlx::FromRow)]
pub struct CampaignWinRow {
    pub id: Uuid,
    pub entry_id: Option<Uuid>,
    pub contact_id: Uuid,
    pub campaign_id: Uuid,
    pub prize_id: String,
    pub prize_label: String,
    pub prize_type: String,
    pub streak_when_won: i32,
    pub was_pity: bool,
    pub redeemed: bool,
    pub redeemed_at: Option<chrono::DateTime<chrono::Utc>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub redemption_code: Option<String>,
}

#[derive(Deserialize)]
pub struct RedeemBody {
    pub contact_id: Option<Uuid>,
}

// ---------------------------------------------------------------------------
// Helper: resolve contact by id, email, or phone
// ---------------------------------------------------------------------------

async fn resolve_contact(state: &AppState, body: &SpinRequestBody) -> Result<Uuid, AppError> {
    if let Some(cid) = body.contact_id {
        // Verify contact exists
        contacts::get_contact(&state.db, &cid).await?;
        return Ok(cid);
    }

    let email = body.email.as_deref();
    let phone = body.phone.as_deref();

    if email.is_none() && phone.is_none() {
        return Err(AppError::BadRequest(
            "Either contact_id, email, or phone is required".to_string(),
        ));
    }

    // Create or find contact via upsert
    let input = contacts::ContactInput {
        first_name: body.first_name.clone(),
        last_name: body.last_name.clone(),
        email: body.email.clone(),
        phone: body.phone.clone(),
        website: body.website.clone(),
        business_name: None,
    };

    let contact_id = contacts::upsert_contact(&state.db, &input).await?;
    Ok(contact_id)
}

// ---------------------------------------------------------------------------
// Delivery config parsing
// ---------------------------------------------------------------------------

/// Prize delivery config stored in each prize's section of campaign config.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct PrizeDeliveryConfig {
    #[serde(default)]
    pub method: String, // "none", "email", "redirect"
    pub subject: Option<String>,
    pub body: Option<String>,
    pub redirect_url: Option<String>,
}

/// Extract delivery config from a prize's config, or return default.
fn get_prize_delivery(prize_json: &serde_json::Value) -> PrizeDeliveryConfig {
    prize_json
        .get("delivery")
        .and_then(|d| serde_json::from_value(d.clone()).ok())
        .unwrap_or(PrizeDeliveryConfig {
            method: "none".to_string(),
            subject: None,
            body: None,
            redirect_url: None,
        })
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// POST /api/v1/campaigns/{slug}/spin
/// Execute a spin for a contact on a campaign. Creates an anonymous contact if
/// no contact_id is provided (must have email or phone).
/// After a win, generates a redemption code, handles delivery (email/redirect),
/// and fires campaign integrations.
pub async fn spin(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    headers: HeaderMap,
    Json(body): Json<SpinRequestBody>,
) -> Result<Json<Value>, AppError> {
    // Resolve campaign by slug
    let campaign = campaigns::get_campaign_by_slug(&state.db, &slug).await?;

    if campaign.status != "active" {
        return Err(AppError::Forbidden("Campaign is not active".to_string()));
    }

    if campaign.r#type != "spin_wheel" {
        return Err(AppError::BadRequest(format!(
            "Campaign '{}' is type '{}', not 'spin_wheel'",
            campaign.slug, campaign.r#type
        )));
    }

    let contact_id = resolve_contact(&state, &body).await?;

    // Extract source tracking from headers
    let user_agent = headers
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let ip_address = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .or_else(|| headers.get("x-real-ip").and_then(|v| v.to_str().ok()))
        .map(|s| s.to_string());

    // Clone source tracking fields before move into apply_prize_draw
    let utm_source = body.utm_source.clone();
    let utm_medium = body.utm_medium.clone();
    let utm_campaign = body.utm_campaign.clone();
    let referrer_url = body.referrer_url.clone();
    let page_url = body.page_url.clone();

    // Get contact info for delivery
    let contact = contacts::get_contact(&state.db, &contact_id).await?;

    // Build contact JSON for entry webhook (used after prize draw)
    let contact_val = json!({
        "first_name": contact.first_name,
        "last_name": contact.last_name,
        "email": contact.email,
        "phone": contact.phone,
        "website": contact.website,
        "business_name": contact.business_name,
    });

    // Execute the prize draw
    let result = prize_draw::apply_prize_draw(
        &state.db,
        &campaign.id,
        &contact_id,
        &campaign.config,
        utm_source,
        utm_medium,
        utm_campaign,
        referrer_url,
        page_url,
        user_agent,
        ip_address,
    )
    .await?;

    tracing::info!(
        "Spin: contact={} campaign={} prize={} won={} pity={} streak={} total={}",
        contact_id,
        campaign.slug,
        result.prize_label,
        result.won,
        result.was_pity,
        result.streak,
        result.total_spins
    );

    // If won, handle delivery + redemption code + integrations
    let mut redemption_code: Option<String> = None;
    let mut redirect_url: Option<String> = None;
    let mut delivery_info: Option<Value> = None;

    // If custom field answers were provided, store them on the entry
    if let Some(ref answers) = body.answers {
        // Update the most recent entry for this contact+campaign with custom answers
        let _ = sqlx::query(
            r#"UPDATE entries SET answers = answers || $1::jsonb
               WHERE id = (
                   SELECT id FROM entries
                   WHERE contact_id = $2 AND campaign_id = $3
                   ORDER BY created_at DESC LIMIT 1
               )"#,
        )
        .bind(answers)
        .bind(contact_id)
        .bind(campaign.id)
        .execute(&state.db)
        .await;
    }

    // Execute all configured output actions (webhooks, CoreSwift sync, emails, SMS, etc.)
    let outcome = if result.won { "winner" } else { "loss" };
    let campaign_config = campaign.config.clone();
    let campaign_name = campaign.name.clone();
    let campaign_slug = campaign.slug.clone();
    let campaign_type = campaign.r#type.clone();

    let fn1 = contact.first_name.as_deref().unwrap_or("").to_string();
    let ln1 = contact.last_name.as_deref().unwrap_or("").to_string();
    let em1 = contact.email.as_deref().unwrap_or("").to_string();
    let ph1 = contact.phone.as_deref().unwrap_or("").to_string();
    let ws1 = contact.website.as_deref().unwrap_or("").to_string();
    let bn1 = contact.business_name.as_deref().unwrap_or("").to_string();
    let outcome_str = outcome.to_string();

    tokio::spawn({
        let state = state.clone();
        let campaign_id = campaign.id;
        let contact_id = contact_id;
        let account_id = campaign.account_id;
        let answers = body.answers.clone();
        let utm_source = body.utm_source.clone();
        let utm_medium = body.utm_medium.clone();
        let utm_campaign = body.utm_campaign.clone();
        let referrer_url = body.referrer_url.clone();
        let page_url = body.page_url.clone();
        async move {
            crate::delivery::output_actions::execute_output_actions(
                &state,
                &campaign_id,
                &campaign_name,
                &campaign_slug,
                &campaign_type,
                &campaign_config,
                &contact_id,
                &fn1,
                &ln1,
                &em1,
                &ph1,
                &ws1,
                &bn1,
                &account_id,
                &outcome_str,
                &[],
                None,
                answers.as_ref(),
                utm_source.as_deref(),
                utm_medium.as_deref(),
                utm_campaign.as_deref(),
                referrer_url.as_deref(),
                page_url.as_deref(),
            )
            .await;
        }
    });

    if result.won {
        // Generate redemption code
        let code = generate_redemption_code();
        redemption_code = Some(code.clone());

        // Update the win record with the redemption code
        let _ = prize_draw::set_redemption_code(
            &state.db,
            &campaign.id,
            &contact_id,
            &result.prize_id,
            &code,
        )
        .await;

        // Check prize delivery config from campaign config
        if let Some(prize_pool) = campaign.config.get("prize_pool") {
            if let Some(prizes) = prize_pool.get("prizes").and_then(|p| p.as_array()) {
                if let Some(prize_json) = prizes
                    .iter()
                    .find(|p| p.get("id").and_then(|id| id.as_str()) == Some(&result.prize_id))
                {
                    let delivery = get_prize_delivery(prize_json);

                    match delivery.method.as_str() {
                        "email" => {
                            // Fire email delivery via webhook to configured email provider
                            let subject = delivery
                                .subject
                                .unwrap_or_else(|| format!("You won: {}!", result.prize_label));
                            let body_template = delivery.body
                                .unwrap_or_else(|| "Congratulations! You won {prize_label}. Use code {code} to redeem.".to_string());

                            // Substitute placeholders
                            let body = body_template
                                .replace("{code}", &code)
                                .replace("{prize_label}", &result.prize_label)
                                .replace(
                                    "{contact_name}",
                                    contact.first_name.as_deref().unwrap_or("there"),
                                )
                                .replace("{prize_type}", &result.prize_type);

                            let subject_final = subject
                                .replace("{prize_label}", &result.prize_label)
                                .replace("{code}", &code)
                                .replace(
                                    "{contact_name}",
                                    contact.first_name.as_deref().unwrap_or("there"),
                                );

                            delivery_info = Some(json!({
                                "method": "email",
                                "subject": subject_final,
                                "body": body,
                                "redemption_code": code,
                            }));

                            // Fire integrations for delivery
                            // In a full implementation, this would use a provider key to send email
                            tracing::info!(
                                "Prize email delivery: contact={} slug={} prize={} code={}",
                                contact_id,
                                slug,
                                result.prize_label,
                                code
                            );
                        }
                        "redirect" => {
                            redirect_url = delivery.redirect_url.clone();
                            if let Some(ref url) = redirect_url {
                                // Substitute placeholders in URL
                                let url_with_code = url
                                    .replace("{code}", &code)
                                    .replace("{prize_label}", &result.prize_label)
                                    .replace("{contact_id}", &contact_id.to_string());

                                delivery_info = Some(json!({
                                    "method": "redirect",
                                    "redirect_url": url_with_code,
                                    "redemption_code": code,
                                }));
                            }
                        }
                        _ => {
                            // No delivery method configured — just include redemption code
                            delivery_info = Some(json!({
                                "method": "none",
                                "redemption_code": code,
                            }));
                        }
                    }
                } else {
                    delivery_info = Some(json!({
                        "method": "none",
                        "redemption_code": code,
                    }));
                }
            } else {
                delivery_info = Some(json!({
                    "method": "none",
                    "redemption_code": code,
                }));
            }
        } else {
            delivery_info = Some(json!({
                "method": "none",
                "redemption_code": code,
            }));
        }

        // Fire campaign integrations on win event
        let contact_email = contact.email.clone();
        let contact_phone = contact.phone.clone();
        let first_name = contact.first_name.clone();
        let last_name = contact.last_name.clone();
        let contact_email_for_mb = contact.email.clone();
        let contact_phone_for_mb = contact.phone.clone();
        let first_name_for_mb = contact.first_name.clone();
        let last_name_for_mb = contact.last_name.clone();

        let prize_info = campaign_integrations::PrizeInfo {
            id: result.prize_id.clone(),
            label: result.prize_label.clone(),
            prize_type: result.prize_type.clone(),
            redemption_code: redemption_code.clone(),
        };

        let contact_info = campaign_integrations::ContactInfo {
            email: contact_email,
            phone: contact_phone,
            first_name,
            last_name,
        };

        // Look up prize-level marketing_boost from campaign_prize_inventory
        let prize_marketing_boost: Option<serde_json::Value> = sqlx::query_scalar(
            r#"SELECT marketing_boost FROM campaign_prize_inventory
               WHERE campaign_id = $1 AND prize_id = $2"#,
        )
        .bind(campaign.id)
        .bind(&result.prize_id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();

        // Fire asynchronously so we don't block the response
        let state_for_mb = state.clone();
        let campaign_id_for_mb = campaign.id;
        let campaign_name_for_mb = campaign.name.clone();
        let mb_override = prize_marketing_boost.clone();
        tokio::spawn(async move {
            campaign_integrations::fire_campaign_integrations(
                &state,
                &campaign.slug,
                &campaign.name,
                &campaign.r#type,
                &campaign.id,
                "on_win",
                contact_info,
                prize_info,
                result.was_pity,
                result.streak,
                result.total_spins,
            )
            .await;

            // Also fire Marketing Boost direct API send if configured
            // Per-prize marketing_boost takes priority over campaign-level config
            let mb_payload = json!({
                "first_name": first_name_for_mb,
                "last_name": last_name_for_mb,
                "email": contact_email_for_mb,
                "phone": contact_phone_for_mb,
                "campaign_name": campaign_name_for_mb,
                "event": "on_win",
            });
            campaign_integrations::fire_marketing_boost_with_override(
                &state_for_mb,
                &campaign_id_for_mb,
                "on_win",
                &mb_payload,
                mb_override.as_ref(),
            )
            .await;
        });
    }

    // Build response
    let mut response = json!({
        "result": result,
        "contact_id": contact_id,
        "campaign_id": campaign.id,
    });

    if let Some(code) = redemption_code {
        response["redemption_code"] = json!(code);
    }

    if let Some(url) = redirect_url {
        response["redirect_url"] = json!(url);
    }

    if let Some(info) = delivery_info {
        response["delivery"] = info;
    }

    Ok(Json(response))
}

/// GET /api/v1/campaigns/{slug}/spin-status
/// Check current spin status for a contact (streak, remaining spins, etc.).
/// Uses query parameters for GET request.
pub async fn spin_status(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    query: axum::extract::Query<SpinStatusQuery>,
) -> Result<Json<Value>, AppError> {
    let campaign = campaigns::get_campaign_by_slug(&state.db, &slug).await?;

    let contact_id = if let Some(cid) = query.contact_id {
        contacts::get_contact(&state.db, &cid).await?;
        cid
    } else if let Some(ref email) = query.email {
        let input = contacts::ContactInput {
            first_name: None,
            last_name: None,
            email: Some(email.clone()),
            phone: None,
            website: None,
            business_name: None,
        };
        contacts::upsert_contact(&state.db, &input).await?
    } else if let Some(ref phone) = query.phone {
        let input = contacts::ContactInput {
            first_name: None,
            last_name: None,
            email: None,
            phone: Some(phone.clone()),
            website: None,
            business_name: None,
        };
        contacts::upsert_contact(&state.db, &input).await?
    } else {
        return Err(AppError::BadRequest(
            "Either contact_id, email, or phone query parameter is required".to_string(),
        ));
    };

    let status =
        prize_draw::get_spin_status(&state.db, &campaign.id, &contact_id, &campaign.config).await?;

    Ok(Json(json!({
        "status": status,
        "contact_id": contact_id,
        "campaign_id": campaign.id,
    })))
}

/// GET /api/v1/campaigns/{slug}/wins
/// List all wins for a campaign (admin).
pub async fn list_wins(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    query: axum::extract::Query<WinsQuery>,
) -> Result<Json<Value>, AppError> {
    let campaign = campaigns::get_campaign_by_slug(&state.db, &slug).await?;

    let limit = query.limit.unwrap_or(50).min(200);
    let offset = query.offset.unwrap_or(0);

    let wins: Vec<CampaignWinRow> = if query.redeemed_only.unwrap_or(false) {
        sqlx::query_as::<_, CampaignWinRow>(
            r#"SELECT id, entry_id, contact_id, campaign_id, prize_id, prize_label,
                      prize_type, streak_when_won, was_pity, redeemed, redeemed_at, created_at,
                      redemption_code
               FROM campaign_wins
               WHERE campaign_id = $1 AND redeemed = true
               ORDER BY created_at DESC
               LIMIT $2 OFFSET $3"#,
        )
        .bind(campaign.id)
        .bind(limit)
        .bind(offset)
        .fetch_all(&state.db)
        .await?
    } else {
        sqlx::query_as::<_, CampaignWinRow>(
            r#"SELECT id, entry_id, contact_id, campaign_id, prize_id, prize_label,
                      prize_type, streak_when_won, was_pity, redeemed, redeemed_at, created_at,
                      redemption_code
               FROM campaign_wins
               WHERE campaign_id = $1
               ORDER BY created_at DESC
               LIMIT $2 OFFSET $3"#,
        )
        .bind(campaign.id)
        .bind(limit)
        .bind(offset)
        .fetch_all(&state.db)
        .await?
    };

    let total_count: i64 =
        sqlx::query_scalar(r#"SELECT COUNT(*) FROM campaign_wins WHERE campaign_id = $1"#)
            .bind(campaign.id)
            .fetch_one(&state.db)
            .await?;

    Ok(Json(json!({
        "wins": wins,
        "total": total_count,
        "limit": limit,
        "offset": offset,
    })))
}

/// POST /api/v1/campaigns/{slug}/wins/{win_id}/redeem
/// Mark a win as redeemed.
pub async fn redeem_win(
    State(state): State<AppState>,
    Path((slug, win_id)): Path<(String, String)>,
    Json(_body): Json<RedeemBody>,
) -> Result<Json<Value>, AppError> {
    let campaign = campaigns::get_campaign_by_slug(&state.db, &slug).await?;
    let win_uuid = Uuid::parse_str(&win_id)
        .map_err(|_| AppError::BadRequest("Invalid win ID format".to_string()))?;

    let result = sqlx::query(
        r#"UPDATE campaign_wins
           SET redeemed = true, redeemed_at = now()
           WHERE id = $1 AND campaign_id = $2 AND redeemed = false"#,
    )
    .bind(win_uuid)
    .bind(campaign.id)
    .execute(&state.db)
    .await?;

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound(
            "Win not found or already redeemed".to_string(),
        ));
    }

    Ok(Json(json!({ "status": "redeemed" })))
}
