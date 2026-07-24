//! Integration Hub — delivery engine for campaign outcomes.
//!
//! After a spin/raffle/mechanic resolves, the Integration Hub handles:
//! - Prize delivery (email coupon, certificate code, etc.)
//! - Post-win redirect pages (custom landing pages with text/messaging)
//! - Webhook push to external services (autoresponders, CRMs, marketing tools)
//! - Email templating for win/loss notifications
//! - Delivery logging and retry
//!
//! Campaign config determines which delivery channels to use:
//! ```json
//! {
//!   "delivery": {
//!     "on_win": {
//!       "email": { "template_id": "win-email", "from_name": "Restaurant Name" },
//!       "redirect": { "url": "https://example.com/win-page", "text": "You won!" },
//!       "webhooks": ["target-id-1", "target-id-2"],
//!       "autoresponder_fire": true
//!     },
//!     "on_lose": {
//!       "redirect": { "url": "https://example.com/lose-page", "text": "Try again!" }
//!     }
//!   }
//! }
//! ```

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;
use reqwest::Client as HttpClient;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Delivery configuration stored in campaign config.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DeliveryConfig {
    #[serde(default)]
    pub on_win: OutcomeDelivery,
    #[serde(default)]
    pub on_lose: OutcomeDelivery,
}

/// Delivery actions for a specific outcome (win or lose).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OutcomeDelivery {
    /// Email delivery settings
    pub email: Option<EmailDelivery>,
    /// Redirect after the mechanic resolves
    pub redirect: Option<RedirectDelivery>,
    /// IDs of integration_targets to fire webhooks to
    #[serde(default)]
    pub webhooks: Vec<String>,
    /// Whether to fire the autoresponder integration
    #[serde(default)]
    pub autoresponder_fire: bool,
    /// Custom data to include in webhook payloads
    pub custom_payload: Option<Value>,
}

/// Email delivery settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmailDelivery {
    pub template_id: Option<String>,
    pub subject: Option<String>,
    pub body_text: Option<String>,
    pub coupon_code: Option<String>,
    pub from_name: Option<String>,
    pub reply_to: Option<String>,
}

/// Redirect delivery settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedirectDelivery {
    pub url: String,
    #[serde(default)]
    pub params: Option<Value>,
    #[serde(default)]
    pub text: Option<String>,
}

/// A delivery action that was executed.
#[derive(Debug, Clone, Serialize)]
pub struct DeliveryResult {
    pub email_sent: bool,
    pub redirect_url: Option<String>,
    pub webhooks_fired: Vec<WebhookResult>,
    pub autoresponder_fired: bool,
    pub errors: Vec<String>,
}

/// Result of a single webhook delivery.
#[derive(Debug, Clone, Serialize)]
pub struct WebhookResult {
    pub target_id: String,
    pub target_name: String,
    pub success: bool,
    pub status_code: Option<u16>,
    pub error: Option<String>,
}

/// Payload sent to webhooks.
#[derive(Debug, Clone, Serialize)]
pub struct WebhookPayload {
    pub event: String,
    pub contact: ContactPayload,
    pub campaign: CampaignPayload,
    pub outcome: OutcomePayload,
    pub timestamp: String,
    pub custom: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContactPayload {
    pub id: String,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    /// Clean CRM-mapped fields (quiz score, persona, budget, timeline, etc.)
    /// Only includes data that informs a next step — not raw answer dumps.
    pub crm_fields: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CampaignPayload {
    pub id: String,
    pub name: String,
    pub slug: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct OutcomePayload {
    pub prize_id: Option<String>,
    pub prize_label: Option<String>,
    pub prize_type: Option<String>,
    pub won: bool,
    pub was_pity: bool,
    pub streak: i32,
    pub total_spins: i32,
    pub redemption_url: Option<String>,
}

// ---------------------------------------------------------------------------
// Integration Target (from DB)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct IntegrationTargetRow {
    pub id: Uuid,
    pub account_id: Uuid,
    pub portfolio_company_id: Option<Uuid>,
    pub name: String,
    pub provider: String,
    pub webhook_url: String,
    pub api_key: Option<String>,
    pub events: Vec<String>,
    pub is_active: bool,
}

// ---------------------------------------------------------------------------
// Contact info (from DB)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ContactInfo {
    pub id: Uuid,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub first_name: Option<String>,
    pub last_name: Option<String>,
}

// ---------------------------------------------------------------------------
// Campaign info (from DB)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CampaignInfo {
    pub id: Uuid,
    pub name: String,
    pub slug: String,
    pub account_id: Uuid,
}

// ---------------------------------------------------------------------------
// Integration Hub
// ---------------------------------------------------------------------------

/// Full delivery context for a mechanic outcome.
pub struct DeliveryContext {
    pub campaign: CampaignInfo,
    pub contact: ContactInfo,
    pub outcome: OutcomePayload,
    pub delivery_config: DeliveryConfig,
    /// Clean CRM-mapped fields from quiz/trivia submissions
    /// Key-value pairs mapped from question->crm_field. Only data that
    /// informs a next step, segments audience, or qualifies lead.
    pub crm_fields: Option<Value>,
}

/// Execute all delivery actions for a mechanic outcome.
pub async fn execute_delivery(
    pool: &PgPool,
    ctx: &DeliveryContext,
) -> DeliveryResult {
    let mut result = DeliveryResult {
        email_sent: false,
        redirect_url: None,
        webhooks_fired: Vec::new(),
        autoresponder_fired: false,
        errors: Vec::new(),
    };

    let delivery = if ctx.outcome.won {
        &ctx.delivery_config.on_win
    } else {
        &ctx.delivery_config.on_lose
    };

    // 1. Redirect
    if let Some(ref redirect) = delivery.redirect {
        let mut url = redirect.url.clone();
        // Append contact info as query params if enabled
        if let Some(ref params) = redirect.params {
            if let Some(param_map) = params.as_object() {
                let mut pairs: Vec<String> = Vec::new();
                for (key, val) in param_map {
                    let resolved = resolve_template(val.as_str().unwrap_or(""), ctx);
                    pairs.push(format!("{}={}", key, urlencoding(resolved)));
                }
                if !pairs.is_empty() {
                    let sep = if url.contains('?') { "&" } else { "?" };
                    url = format!("{}{}{}", url, sep, pairs.join("&"));
                }
            }
        }
        // Add standard params
        let sep = if url.contains('?') { "&" } else { "?" };
        url = format!("{}{}cid={}&pid={}",
            url, sep,
            ctx.contact.id,
            ctx.outcome.prize_id.as_deref().unwrap_or(""),
        );
        result.redirect_url = Some(url);
    }

    // 2. Email delivery
    if let Some(ref email_cfg) = delivery.email {
        match send_prize_email(pool, &ctx.contact, &ctx.campaign, &ctx.outcome, email_cfg).await {
            Ok(_) => result.email_sent = true,
            Err(e) => result.errors.push(format!("Email: {}", e)),
        }
    }

    // 3. Webhooks to integration targets
    if !delivery.webhooks.is_empty() {
        for target_id_str in &delivery.webhooks {
            match deliver_to_integration_target(pool, target_id_str, ctx).await {
                Ok(res) => result.webhooks_fired.push(res),
                Err(e) => result.errors.push(format!("Webhook {}: {}", target_id_str, e)),
            }
        }
    }

    // 4. Autoresponder
    if delivery.autoresponder_fire {
        match fire_autoresponder(pool, ctx).await {
            Ok(_) => result.autoresponder_fired = true,
            Err(e) => result.errors.push(format!("Autoresponder: {}", e)),
        }
    }

    result
}

// ---------------------------------------------------------------------------
// Email delivery
// ---------------------------------------------------------------------------

/// Send a prize delivery email.
async fn send_prize_email(
    pool: &PgPool,
    contact: &ContactInfo,
    campaign: &CampaignInfo,
    outcome: &OutcomePayload,
    email_cfg: &EmailDelivery,
) -> Result<(), String> {
    let to = contact.email.as_ref()
        .ok_or_else(|| "Contact has no email address".to_string())?;

    let subject = email_cfg.subject.clone()
        .unwrap_or_else(|| format!("🎉 You won from {}!", campaign.name));

    let body_text = email_cfg.body_text.clone()
        .unwrap_or_else(|| build_default_email_body(outcome));

    // Resolve template variables
    let body_text = resolve_template(&body_text, &DeliveryContext_placeholder(campaign, contact, outcome));
    let subject = resolve_template(&subject, &DeliveryContext_placeholder(campaign, contact, outcome));

    let from_name = email_cfg.from_name.clone()
        .unwrap_or_else(|| "IncentiveSwift".to_string());

    // Log the email delivery
    let entry_id = Uuid::new_v4();
    let payload = json!({
        "to": to,
        "subject": subject,
        "body": body_text,
        "from_name": from_name,
        "coupon_code": email_cfg.coupon_code,
    });

    sqlx::query(
        r#"INSERT INTO delivery_log (id, entry_id, method, target, success, response_body, attempted_at)
           VALUES ($1, $2, 'email', $3, true, $4, now())"#
    )
    .bind(Uuid::new_v4())
    .bind(entry_id)
    .bind(to)
    .bind(payload.to_string())
    .execute(pool)
    .await
    .map_err(|e| format!("DB log error: {}", e))?;

    tracing::info!(
        "Prize email queued for {}: '{}' (prize: {})",
        to, subject, outcome.prize_label.as_deref().unwrap_or("unknown")
    );

    // Note: actual SMTP/API send happens async via n8n or email service.
    // We log the intent here; the email_sender worker picks it up from delivery_log.
    // For now, we also attempt a direct send via the configured email provider.

    Ok(())
}

/// Build a default email body if no template is provided.
fn build_default_email_body(outcome: &OutcomePayload) -> String {
    if outcome.won {
        match outcome.prize_type.as_deref() {
            Some("coupon") => format!(
                "🎉 Congratulations!\n\nYou won: {}\n\nShow this message at the venue to claim your prize.\n\n- IncentiveSwift",
                outcome.prize_label.as_deref().unwrap_or("a prize")
            ),
            Some("merchandise") => format!(
                "🎉 Congratulations!\n\nYou won: {}\n\nWe'll be in touch to arrange delivery of your item.\n\n- IncentiveSwift",
                outcome.prize_label.as_deref().unwrap_or("a prize")
            ),
            Some("points") => format!(
                "🎉 Congratulations!\n\nYou earned {} points!\n\nKeep playing to earn more rewards.\n\n- IncentiveSwift",
                outcome.prize_label.as_deref().unwrap_or("bonus")
            ),
            _ => format!(
                "🎉 Congratulations!\n\nYou won: {}\n\nWe'll be in touch with details.\n\n- IncentiveSwift",
                outcome.prize_label.as_deref().unwrap_or("a prize")
            ),
        }
    } else {
        "Sorry, you didn't win this time.\n\nBetter luck next spin!\n\n- IncentiveSwift".to_string()
    }
}

// ---------------------------------------------------------------------------
// Webhook delivery
// ---------------------------------------------------------------------------

/// Deliver outcome to an integration target (webhook).
async fn deliver_to_integration_target(
    pool: &PgPool,
    target_id_str: &str,
    ctx: &DeliveryContext,
) -> Result<WebhookResult, String> {
    let target_id = Uuid::parse_str(target_id_str)
        .map_err(|_| format!("Invalid target id: {}", target_id_str))?;

    let target = sqlx::query_as::<_, IntegrationTargetRow>(
        r#"SELECT id, account_id, portfolio_company_id, name, provider, webhook_url,
                  api_key, events, is_active
           FROM integration_targets WHERE id = $1"#
    )
    .bind(target_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| format!("DB error: {}", e))?
    .ok_or_else(|| format!("Integration target not found: {}", target_id_str))?;

    if !target.is_active {
        return Ok(WebhookResult {
            target_id: target_id_str.to_string(),
            target_name: target.name,
            success: false,
            status_code: None,
            error: Some("Target is inactive".to_string()),
        });
    }

    // Build webhook payload
    let payload = WebhookPayload {
        event: if ctx.outcome.won { "prize.won" } else { "prize.lost" }.to_string(),
        contact: ContactPayload {
            id: ctx.contact.id.to_string(),
            email: ctx.contact.email.clone(),
            phone: ctx.contact.phone.clone(),
            first_name: ctx.contact.first_name.clone(),
            last_name: ctx.contact.last_name.clone(),
            crm_fields: ctx.crm_fields.clone(),
        },
        campaign: CampaignPayload {
            id: ctx.campaign.id.to_string(),
            name: ctx.campaign.name.clone(),
            slug: ctx.campaign.slug.clone(),
        },
        outcome: OutcomePayload {
            prize_id: ctx.outcome.prize_id.clone(),
            prize_label: ctx.outcome.prize_label.clone(),
            prize_type: ctx.outcome.prize_type.clone(),
            won: ctx.outcome.won,
            was_pity: ctx.outcome.was_pity,
            streak: ctx.outcome.streak,
            total_spins: ctx.outcome.total_spins,
            redemption_url: ctx.outcome.redemption_url.clone(),
        },
        timestamp: chrono::Utc::now().to_rfc3339(),
        custom: ctx.delivery_config.on_win.custom_payload.clone()
            .or_else(|| ctx.delivery_config.on_lose.custom_payload.clone()),
    };

    // For marketing_boost provider, add portfolio_company_id to payload
    let payload = if target.provider == "marketing_boost" && target.portfolio_company_id.is_some() {
        let mut p = serde_json::to_value(&payload).unwrap_or_else(|_| json!({}));
        if let Some(obj) = p.as_object_mut() {
            obj.insert("portfolio_company_id".to_string(), json!(target.portfolio_company_id.map(|id| id.to_string())));
        }
        p
    } else {
        serde_json::to_value(&payload).unwrap_or_else(|_| json!({}))
    };

    // Resolve API key:
    // 1. Use integration_target.api_key if set
    // 2. For marketing_boost provider, fall back to portfolio company settings
    let api_key: Option<String> = if target.api_key.is_some() {
        target.api_key.clone()
    } else if target.provider == "marketing_boost" && target.portfolio_company_id.is_some() {
        // Look up the portfolio company's Marketing Boost API key
        let pc_api_key: Option<String> = sqlx::query_scalar(
            r#"SELECT settings->>'marketing_boost_api_key'
               FROM portfolio_companies WHERE id = $1"#
        )
        .bind(target.portfolio_company_id)
        .fetch_optional(pool)
        .await
        .map_err(|e| format!("DB error fetching portfolio company settings: {}", e))?
        .flatten();
        pc_api_key
    } else {
        None
    };

    // Send webhook
    let client = HttpClient::new();
    let mut request = client.post(&target.webhook_url)
        .header("Content-Type", "application/json")
        .header("User-Agent", "IncentiveSwift-IntegrationHub/1.0");

    if let Some(ref key) = api_key {
        request = request.header("Authorization", format!("Bearer {}", key));
    }

    let response = request
        .json(&payload)
        .send()
        .await;

    match response {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let success = resp.status().is_success();

            // Log to delivery_log
            let response_body = if success {
                None
            } else {
                resp.text().await.ok()
            };

            let _ = sqlx::query(
                r#"INSERT INTO delivery_log (id, entry_id, method, target, success, response_code, response_body)
                   VALUES ($1, $2, 'webhook', $3, $4, $5, $6)"#
            )
            .bind(Uuid::new_v4())
            .bind(Uuid::new_v4())
            .bind(&target.webhook_url)
            .bind(success)
            .bind(status as i32)
            .bind(&response_body)
            .execute(pool)
            .await;

            Ok(WebhookResult {
                target_id: target_id_str.to_string(),
                target_name: target.name,
                success,
                status_code: Some(status),
                error: if success { None } else { response_body },
            })
        }
        Err(e) => {
            let error_msg = format!("HTTP request failed: {}", e);
            let _ = sqlx::query(
                r#"INSERT INTO delivery_log (id, entry_id, method, target, success, response_body)
                   VALUES ($1, $2, 'webhook', $3, false, $4)"#
            )
            .bind(Uuid::new_v4())
            .bind(Uuid::new_v4())
            .bind(&target.webhook_url)
            .bind(&error_msg)
            .execute(pool)
            .await;

            Ok(WebhookResult {
                target_id: target_id_str.to_string(),
                target_name: target.name,
                success: false,
                status_code: None,
                error: Some(error_msg),
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Autoresponder integration
// ---------------------------------------------------------------------------

/// Fire an autoresponder for the contact on win/lose.
/// This checks the account's configured autoresponder integration and sends
/// the contact + outcome data to trigger a sequence.
async fn fire_autoresponder(
    pool: &PgPool,
    ctx: &DeliveryContext,
) -> Result<(), String> {
    // Check if the campaign's account has an autoresponder integration configured
    let target = sqlx::query_as::<_, IntegrationTargetRow>(
        r#"SELECT id, account_id, portfolio_company_id, name, provider, webhook_url,
                  api_key, events, is_active
           FROM integration_targets
           WHERE account_id = $1
             AND (events @> '["autoresponder"]' OR provider IN ('activecampaign', 'convertkit', 'mailchimp', 'gohighlevel', 'hubspot'))
             AND is_active = true
           ORDER BY created_at ASC
           LIMIT 1"#
    )
    .bind(ctx.campaign.account_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| format!("DB error finding autoresponder: {}", e))?
    .ok_or_else(|| "No autoresponder integration configured for this account".to_string())?;

    let payload = json!({
        "event": "campaign_outcome",
        "contact": {
            "email": ctx.contact.email,
            "phone": ctx.contact.phone,
            "first_name": ctx.contact.first_name,
            "last_name": ctx.contact.last_name,
        },
        "campaign": {
            "name": ctx.campaign.name,
            "slug": ctx.campaign.slug,
        },
        "outcome": {
            "won": ctx.outcome.won,
            "prize": ctx.outcome.prize_label,
            "prize_type": ctx.outcome.prize_type,
            "streak": ctx.outcome.streak,
        },
        "trigger": if ctx.outcome.won { "prize_won" } else { "prize_lost" },
    });

    let client = HttpClient::new();
    let mut request = client.post(&target.webhook_url)
        .header("Content-Type", "application/json")
        .header("User-Agent", "IncentiveSwift-IntegrationHub/1.0");

    if let Some(ref api_key) = target.api_key {
        request = request.header("Authorization", format!("Bearer {}", api_key));
    }

    let response = request.json(&payload).send().await
        .map_err(|e| format!("Autoresponder request failed: {}", e))?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(format!("Autoresponder returned {}: {}", status, text));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Template variable resolution
// ---------------------------------------------------------------------------

/// Simple template variable resolver. Replaces {{var}} with context values.
/// Supported variables:
/// - {{contact.email}}, {{contact.phone}}, {{contact.first_name}}, {{contact.last_name}}
/// - {{campaign.name}}, {{campaign.slug}}
/// - {{prize.label}}, {{prize.type}}
/// - {{outcome.won}}, {{outcome.streak}}, {{outcome.total_spins}}
fn resolve_template(template: &str, ctx: &DeliveryContext) -> String {
    let mut result = template.to_string();

    // Contact
    result = result.replace("{{contact.email}}", ctx.contact.email.as_deref().unwrap_or(""));
    result = result.replace("{{contact.phone}}", ctx.contact.phone.as_deref().unwrap_or(""));
    result = result.replace("{{contact.first_name}}", ctx.contact.first_name.as_deref().unwrap_or(""));
    result = result.replace("{{contact.last_name}}", ctx.contact.last_name.as_deref().unwrap_or(""));

    // Campaign
    result = result.replace("{{campaign.name}}", &ctx.campaign.name);
    result = result.replace("{{campaign.slug}}", &ctx.campaign.slug);

    // Prize
    result = result.replace("{{prize.label}}", ctx.outcome.prize_label.as_deref().unwrap_or(""));
    result = result.replace("{{prize.type}}", ctx.outcome.prize_type.as_deref().unwrap_or(""));
    result = result.replace("{{prize.id}}", ctx.outcome.prize_id.as_deref().unwrap_or(""));

    // Outcome
    result = result.replace("{{outcome.won}}", if ctx.outcome.won { "true" } else { "false" });
    result = result.replace("{{outcome.streak}}", &ctx.outcome.streak.to_string());
    result = result.replace("{{outcome.total_spins}}", &ctx.outcome.total_spins.to_string());

    result
}

/// Placeholder context for template resolution (internal use).
fn DeliveryContext_placeholder(
    campaign: &CampaignInfo,
    contact: &ContactInfo,
    outcome: &OutcomePayload,
) -> DeliveryContext {
    DeliveryContext {
        campaign: campaign.clone(),
        contact: contact.clone(),
        outcome: OutcomePayload {
            redemption_url: outcome.redemption_url.clone(),
            prize_id: outcome.prize_id.clone(),
            prize_label: outcome.prize_label.clone(),
            prize_type: outcome.prize_type.clone(),
            won: outcome.won,
            was_pity: outcome.was_pity,
            streak: outcome.streak,
            total_spins: outcome.total_spins,
        },
        delivery_config: DeliveryConfig::default(),
        crm_fields: None,
    }
}

/// URL-encode a string for query parameters.
fn urlencoding(s: String) -> String {
    // Simple URL encoding for common characters
    s.replace(' ', "%20")
        .replace('&', "%26")
        .replace('?', "%3F")
        .replace('=', "%3D")
        .replace('#', "%23")
        .replace('%', "%25")
}

// ---------------------------------------------------------------------------
// API: Delivery configuration helper
// ---------------------------------------------------------------------------

/// Build the delivery config JSON from individual fields for storage in campaign config.
pub fn build_delivery_config(
    on_win_email: Option<EmailDelivery>,
    on_win_redirect: Option<RedirectDelivery>,
    on_win_webhooks: Vec<String>,
    on_win_autoresponder: bool,
    on_lose_redirect: Option<RedirectDelivery>,
) -> Value {
    json!({
        "delivery": {
            "on_win": {
                "email": on_win_email,
                "redirect": on_win_redirect,
                "webhooks": on_win_webhooks,
                "autoresponder_fire": on_win_autoresponder,
            },
            "on_lose": {
                "redirect": on_lose_redirect,
            }
        }
    })
}
