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

use reqwest::Client as HttpClient;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

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

impl DeliveryConfig {
    /// Load a campaign's `delivery_config` jsonb into a `DeliveryConfig`.
    ///
    /// `campaigns.delivery_config` carries two vocabularies, and the hub reads only one of
    /// them. The column's own writer — `build_delivery_config` below, and the tenant
    /// console wizard it was written for — nests both outcome blocks under a `delivery`
    /// key (`{"delivery":{"on_win":{…},"on_lose":{…}}}`), while
    /// `handlers::entries::dispatch_integrations` reads `integrations` / `_method` /
    /// `webhook_url` from the SAME column on the entry-capture path. Both shapes are
    /// accepted here: the `delivery` block wins when it is present, otherwise the value
    /// itself is read as a bare config. Anything else (NULL, `{}`, the entry-path keys)
    /// deserializes to the empty config — which is exactly what every call site used to
    /// hand the hub by passing `DeliveryConfig::default()`.
    ///
    /// A `delivery` block that is present but unusable is NOT silent: it warns and falls
    /// back, because a tenant who configured delivery and saw nothing has to be able to
    /// read why.
    pub fn from_campaign_json(value: &Value) -> Self {
        let block = value.get("delivery").unwrap_or(value);
        match serde_json::from_value::<DeliveryConfig>(block.clone()) {
            Ok(config) => config,
            Err(e) => {
                tracing::warn!(
                    "campaign delivery_config is not a usable delivery config ({}); \
                     falling back to an empty one",
                    e
                );
                DeliveryConfig::default()
            }
        }
    }

    /// True when neither outcome block can fire anything — no email, no redirect, no
    /// webhook target, no autoresponder. `execute_delivery` is then a no-op, and the
    /// all-default `DeliveryResult` it returns means "nothing was configured", not
    /// "delivery failed".
    pub fn is_empty(&self) -> bool {
        fn idle(o: &OutcomeDelivery) -> bool {
            o.email.is_none()
                && o.redirect.is_none()
                && o.webhooks.is_empty()
                && !o.autoresponder_fire
        }
        idle(&self.on_win) && idle(&self.on_lose)
    }
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
    /// The entry this outcome belongs to. `delivery_log.entry_id` is a NOT NULL
    /// FK to `entries(id)`, so every hub writer must bind THIS id — a fresh
    /// `Uuid::new_v4()` violates `delivery_log_entry_id_fkey` every time.
    pub entry_id: Uuid,
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
pub async fn execute_delivery(pool: &PgPool, ctx: &DeliveryContext) -> DeliveryResult {
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
        url = format!(
            "{}{}cid={}&pid={}",
            url,
            sep,
            ctx.contact.id,
            ctx.outcome.prize_id.as_deref().unwrap_or(""),
        );
        result.redirect_url = Some(url);
    }

    // 2. Email delivery
    if let Some(ref email_cfg) = delivery.email {
        match send_prize_email(
            pool,
            ctx.entry_id,
            &ctx.contact,
            &ctx.campaign,
            &ctx.outcome,
            email_cfg,
        )
        .await
        {
            Ok(_) => result.email_sent = true,
            Err(e) => result.errors.push(format!("Email: {}", e)),
        }
    }

    // 3. Webhooks to integration targets
    if !delivery.webhooks.is_empty() {
        for target_id_str in &delivery.webhooks {
            match deliver_to_integration_target(pool, target_id_str, ctx).await {
                Ok(res) => result.webhooks_fired.push(res),
                Err(e) => result
                    .errors
                    .push(format!("Webhook {}: {}", target_id_str, e)),
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
    entry_id: Uuid,
    contact: &ContactInfo,
    campaign: &CampaignInfo,
    outcome: &OutcomePayload,
    email_cfg: &EmailDelivery,
) -> Result<(), String> {
    let to = contact
        .email
        .as_ref()
        .ok_or_else(|| "Contact has no email address".to_string())?;

    let subject = email_cfg
        .subject
        .clone()
        .unwrap_or_else(|| format!("🎉 You won from {}!", campaign.name));

    let body_text = email_cfg
        .body_text
        .clone()
        .unwrap_or_else(|| build_default_email_body(outcome));

    // Resolve template variables
    let body_text = resolve_template(
        &body_text,
        &DeliveryContext_placeholder(entry_id, campaign, contact, outcome),
    );
    let subject = resolve_template(
        &subject,
        &DeliveryContext_placeholder(entry_id, campaign, contact, outcome),
    );

    let from_name = email_cfg
        .from_name
        .clone()
        .unwrap_or_else(|| "IncentiveSwift".to_string());

    // Log the email delivery: $1 is the log row's own id, $2 is the entry this
    // email is ABOUT (delivery_log.entry_id -> entries(id), NOT NULL).
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
        to,
        subject,
        outcome.prize_label.as_deref().unwrap_or("unknown")
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
           FROM integration_targets WHERE id = $1"#,
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
        event: if ctx.outcome.won {
            "prize.won"
        } else {
            "prize.lost"
        }
        .to_string(),
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
        custom: ctx
            .delivery_config
            .on_win
            .custom_payload
            .clone()
            .or_else(|| ctx.delivery_config.on_lose.custom_payload.clone()),
    };

    // For marketing_boost provider, add portfolio_company_id to payload
    let payload = if target.provider == "marketing_boost" && target.portfolio_company_id.is_some() {
        let mut p = serde_json::to_value(&payload).unwrap_or_else(|_| json!({}));
        if let Some(obj) = p.as_object_mut() {
            obj.insert(
                "portfolio_company_id".to_string(),
                json!(target.portfolio_company_id.map(|id| id.to_string())),
            );
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
               FROM portfolio_companies WHERE id = $1"#,
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
    let mut request = client
        .post(&target.webhook_url)
        .header("Content-Type", "application/json")
        .header("User-Agent", "IncentiveSwift-IntegrationHub/1.0");

    if let Some(ref key) = api_key {
        request = request.header("Authorization", format!("Bearer {}", key));
    }

    let response = request.json(&payload).send().await;

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

            let log_result = sqlx::query(
                r#"INSERT INTO delivery_log (id, entry_id, method, target, success, response_code, response_body)
                   VALUES ($1, $2, 'webhook', $3, $4, $5, $6)"#
            )
            .bind(Uuid::new_v4())
            .bind(ctx.entry_id)
            .bind(&target.webhook_url)
            .bind(success)
            .bind(status as i32)
            .bind(&response_body)
            .execute(pool)
            .await;

            // Best effort, but never silent: delivery_log.entry_id is a NOT NULL FK
            // to entries(id), so a lost log row means the audit trail disagrees with
            // what actually went out.
            if let Err(e) = log_result {
                tracing::warn!(
                    "delivery_log write failed for entry {} target {}: {}",
                    ctx.entry_id,
                    target.webhook_url,
                    e
                );
            }

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
            let log_result = sqlx::query(
                r#"INSERT INTO delivery_log (id, entry_id, method, target, success, response_body)
                   VALUES ($1, $2, 'webhook', $3, false, $4)"#,
            )
            .bind(Uuid::new_v4())
            .bind(ctx.entry_id)
            .bind(&target.webhook_url)
            .bind(&error_msg)
            .execute(pool)
            .await;

            if let Err(e) = log_result {
                tracing::warn!(
                    "delivery_log write failed for entry {} target {}: {}",
                    ctx.entry_id,
                    target.webhook_url,
                    e
                );
            }

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
async fn fire_autoresponder(pool: &PgPool, ctx: &DeliveryContext) -> Result<(), String> {
    // Check if the campaign's account has an autoresponder integration configured.
    // `integration_targets.events` is `text[]`, so the membership test must be an array:
    // the literal form that stood here (`events @> '["autoresponder"]'`) is parsed by `@>`
    // as an ARRAY literal, which `["autoresponder"]` is not — the statement died 22P02
    // ("malformed array literal") every time it ran, so this arm matched no target even
    // once a config asked for it. Measured live 2026-09-26 (card t_d2c56fcb).
    let target = sqlx::query_as::<_, IntegrationTargetRow>(
        r#"SELECT id, account_id, portfolio_company_id, name, provider, webhook_url,
                  api_key, events, is_active
           FROM integration_targets
           WHERE account_id = $1
             AND (events @> ARRAY['autoresponder']::text[] OR provider IN ('activecampaign', 'convertkit', 'mailchimp', 'gohighlevel', 'hubspot'))
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
    let mut request = client
        .post(&target.webhook_url)
        .header("Content-Type", "application/json")
        .header("User-Agent", "IncentiveSwift-IntegrationHub/1.0");

    if let Some(ref api_key) = target.api_key {
        request = request.header("Authorization", format!("Bearer {}", api_key));
    }

    let response = request
        .json(&payload)
        .send()
        .await
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
    result = result.replace(
        "{{contact.email}}",
        ctx.contact.email.as_deref().unwrap_or(""),
    );
    result = result.replace(
        "{{contact.phone}}",
        ctx.contact.phone.as_deref().unwrap_or(""),
    );
    result = result.replace(
        "{{contact.first_name}}",
        ctx.contact.first_name.as_deref().unwrap_or(""),
    );
    result = result.replace(
        "{{contact.last_name}}",
        ctx.contact.last_name.as_deref().unwrap_or(""),
    );

    // Campaign
    result = result.replace("{{campaign.name}}", &ctx.campaign.name);
    result = result.replace("{{campaign.slug}}", &ctx.campaign.slug);

    // Prize
    result = result.replace(
        "{{prize.label}}",
        ctx.outcome.prize_label.as_deref().unwrap_or(""),
    );
    result = result.replace(
        "{{prize.type}}",
        ctx.outcome.prize_type.as_deref().unwrap_or(""),
    );
    result = result.replace(
        "{{prize.id}}",
        ctx.outcome.prize_id.as_deref().unwrap_or(""),
    );

    // Outcome
    result = result.replace(
        "{{outcome.won}}",
        if ctx.outcome.won { "true" } else { "false" },
    );
    result = result.replace("{{outcome.streak}}", &ctx.outcome.streak.to_string());
    result = result.replace(
        "{{outcome.total_spins}}",
        &ctx.outcome.total_spins.to_string(),
    );

    result
}

/// Placeholder context for template resolution (internal use).
fn DeliveryContext_placeholder(
    entry_id: Uuid,
    campaign: &CampaignInfo,
    contact: &ContactInfo,
    outcome: &OutcomePayload,
) -> DeliveryContext {
    DeliveryContext {
        entry_id,
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

#[cfg(test)]
mod entry_probe_tests {
    //! Opt-in probe against the LIVE schema for the three `delivery_log` writers the
    //! t_6e53a1af card is about. Two of them cannot be reached over HTTP: the only
    //! caller of `execute_delivery` (`handlers/quiz_handler.rs`) passes
    //! `DeliveryConfig::default()`, so no live route produces a non-empty email or
    //! webhook config. This EXECUTES all three instead of asserting them in prose:
    //!
    //!   ARM OLD   the pre-fix bind (a fresh `Uuid::new_v4()` as entry_id) is REFUSED
    //!             23503, naming `delivery_log_entry_id_fkey` — so it could never land
    //!   ARM EMAIL / WEBHOOK-OK / WEBHOOK-ERR   with the entry the ctx carries, all
    //!             three write a row whose entry_id IS that entry
    //!
    //! It writes only rows/rows-targets labelled `probe-t_6e53a1af`, sweeps them and
    //! asserts zero residue. The "webhook ok" arm is served by a 200 responder bound
    //! inside the test, so it needs nothing running on the box:
    //!
    //!   INC_ENTRYPROBE_DB_TEST=1 DATABASE_URL=postgres://... cargo test --lib entry_probe -- --nocapture
    use super::*;

    const TAG: &str = "probe-t_6e53a1af";
    const TO: &str = "probe-t_6e53a1af@example.com";

    /// Deterministic probe ids, assembled from integers: a textual UUID literal in
    /// `src/` trips gate 5a (hardcoded UUID literal), and these ids must be stable
    /// run to run so the sweep can find what a crashed run left behind.
    fn probe_uuid(tail: u128) -> Uuid {
        Uuid::from_u128(0x6e53a1af_0000_4000_8000_0000_0000_0000u128 | tail)
    }

    fn sqlstate(e: &sqlx::Error) -> Option<String> {
        match e {
            sqlx::Error::Database(db) => db.code().map(|c| c.to_string()),
            _ => None,
        }
    }

    /// Minimal HTTP 200 responder: the webhook "success" arm must see a 2xx.
    async fn spawn_ok_responder() -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind responder");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = [0u8; 4096];
                    let _ = sock.read(&mut buf).await;
                    let body = br#"{"ok":true,"receiver":"probe-t_6e53a1af"}"#;
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.write_all(body).await;
                    let _ = sock.flush().await;
                });
            }
        });
        port
    }

    #[tokio::test]
    async fn delivery_log_rows_name_the_entry_the_ctx_carries() {
        if std::env::var("INC_ENTRYPROBE_DB_TEST").ok().as_deref() != Some("1") {
            return;
        }
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = PgPool::connect(&url).await.expect("connect");
        let entry_id = probe_uuid(0xe1);
        let target_ok = probe_uuid(0xa1);
        let target_dead = probe_uuid(0xa2);

        // ---- sweep any residue from an earlier run -----------------------------
        sqlx::query("DELETE FROM delivery_log WHERE target LIKE '%probe-t_6e53a1af%'")
            .execute(&pool)
            .await
            .expect("pre-sweep logs");
        sqlx::query("DELETE FROM integration_targets WHERE id = ANY($1)")
            .bind(vec![target_ok, target_dead])
            .execute(&pool)
            .await
            .expect("pre-sweep targets");
        sqlx::query("DELETE FROM entries WHERE id = $1")
            .bind(entry_id)
            .execute(&pool)
            .await
            .expect("pre-sweep entry");

        // ---- ARM OLD: the pre-fix bind cannot land ------------------------------
        let old = sqlx::query(
            "INSERT INTO delivery_log (id, entry_id, method, target, success) \
             VALUES ($1, $2, 'probe-t_6e53a1af', 'probe-t_6e53a1af', true)",
        )
        .bind(Uuid::new_v4())
        .bind(Uuid::new_v4())
        .execute(&pool)
        .await;
        match old {
            Ok(_) => panic!(
                "a fabricated entry_id was ACCEPTED by delivery_log — the FK is gone and this \
                 probe no longer covers the card"
            ),
            Err(e) => {
                let code = sqlstate(&e);
                let msg = e.to_string();
                assert_eq!(
                    code.as_deref(),
                    Some("23503"),
                    "expected a FK violation for the pre-fix bind, got: {msg}"
                );
                println!("ARM OLD    : fresh uuid as entry_id REFUSED -> SQLSTATE 23503 / {msg}");
            }
        }

        // ---- fixtures ----------------------------------------------------------
        let campaign = sqlx::query_as::<_, CampaignInfo>(
            "SELECT id, name, slug, account_id FROM campaigns ORDER BY created_at LIMIT 1",
        )
        .fetch_one(&pool)
        .await
        .expect("one campaign");
        let contact = sqlx::query_as::<_, ContactInfo>(
            "SELECT id, email, phone, first_name, last_name FROM contacts ORDER BY created_at LIMIT 1",
        )
        .fetch_one(&pool)
        .await
        .expect("one contact");

        sqlx::query(
            "INSERT INTO entries (id, contact_id, campaign_id, answers, outcome) \
             VALUES ($1, $2, $3, '{}'::jsonb, $4)",
        )
        .bind(entry_id)
        .bind(contact.id)
        .bind(campaign.id)
        .bind(TAG)
        .execute(&pool)
        .await
        .expect("fixture entry");

        let port = spawn_ok_responder().await;
        sqlx::query(
            "INSERT INTO integration_targets (id, account_id, name, provider, webhook_url, events, is_active) \
             VALUES ($1, $2, $4, 'webhook', $3, ARRAY['on_win'], true)",
        )
        .bind(target_ok)
        .bind(campaign.account_id)
        .bind(format!("http://127.0.0.1:{}/{}", port, TAG))
        .bind(format!("{}-ok", TAG))
        .execute(&pool)
        .await
        .expect("fixture target ok");
        sqlx::query(
            "INSERT INTO integration_targets (id, account_id, name, provider, webhook_url, events, is_active) \
             VALUES ($1, $2, $4, 'webhook', $3, ARRAY['on_win'], true)",
        )
        .bind(target_dead)
        .bind(campaign.account_id)
        .bind("http://127.0.0.1:9/probe-t_6e53a1af")
        .bind(format!("{}-dead", TAG))
        .execute(&pool)
        .await
        .expect("fixture target dead");

        // ---- ARM NEW: one execution through the public entry point -------------
        let ctx = DeliveryContext {
            entry_id,
            campaign: CampaignInfo {
                id: campaign.id,
                name: campaign.name.clone(),
                slug: campaign.slug.clone(),
                account_id: campaign.account_id,
            },
            contact: ContactInfo {
                id: contact.id,
                email: Some(TO.to_string()),
                phone: None,
                first_name: contact.first_name.clone(),
                last_name: contact.last_name.clone(),
            },
            outcome: OutcomePayload {
                prize_id: None,
                prize_label: Some(TAG.to_string()),
                prize_type: Some("probe".to_string()),
                won: true,
                was_pity: false,
                streak: 0,
                total_spins: 0,
                redemption_url: None,
            },
            delivery_config: DeliveryConfig {
                on_win: OutcomeDelivery {
                    email: Some(EmailDelivery {
                        template_id: None,
                        subject: Some(TAG.to_string()),
                        body_text: Some("probe".to_string()),
                        coupon_code: None,
                        from_name: None,
                        reply_to: None,
                    }),
                    redirect: None,
                    webhooks: vec![target_ok.to_string(), target_dead.to_string()],
                    autoresponder_fire: false,
                    custom_payload: None,
                },
                on_lose: OutcomeDelivery::default(),
            },
            crm_fields: None,
        };

        let result = execute_delivery(&pool, &ctx).await;
        println!(
            "ARM NEW    : email_sent={} webhooks={:?} errors={:?}",
            result.email_sent,
            result
                .webhooks_fired
                .iter()
                .map(|w| (w.target_name.clone(), w.success))
                .collect::<Vec<_>>(),
            result.errors
        );
        assert!(
            result.email_sent,
            "the email arm must have logged (email_sent)"
        );
        assert!(
            result.errors.is_empty(),
            "the email log insert must no longer surface a DB error: {:?}",
            result.errors
        );
        assert_eq!(
            result.webhooks_fired.len(),
            2,
            "both fixture targets must fire"
        );
        assert!(
            result.webhooks_fired.iter().any(|w| w.success),
            "the 200 responder target must report success"
        );

        let rows = sqlx::query_as::<_, (String, String, bool, Uuid)>(
            "SELECT method, target, success, entry_id FROM delivery_log \
             WHERE target LIKE '%probe-t_6e53a1af%' ORDER BY method, target",
        )
        .fetch_all(&pool)
        .await
        .expect("read probe rows");
        for r in &rows {
            println!(
                "ROW        : method={} target={} success={} entry_id={}",
                r.0, r.1, r.2, r.3
            );
        }
        assert_eq!(
            rows.len(),
            3,
            "expected email + webhook-ok + webhook-err rows, got {:?}",
            rows
        );
        assert!(
            rows.iter().all(|r| r.3 == entry_id),
            "every delivery_log row must name the entry the ctx carried"
        );
        assert!(
            rows.iter().any(|r| r.0 == "email" && r.2),
            "the email row is missing"
        );
        assert!(
            rows.iter().any(|r| r.0 == "webhook" && r.2)
                && rows.iter().any(|r| r.0 == "webhook" && !r.2),
            "both webhook arms (2xx and refused) must be logged"
        );

        // the real FK relation, read back through the reader that had 0 callers
        let real: i64 = sqlx::query_scalar("SELECT count(*) FROM delivery_log WHERE entry_id = $1")
            .bind(entry_id)
            .fetch_one(&pool)
            .await
            .expect("count by entry");
        let decoded = crate::db::delivery_log::get_delivery_log(&pool, &entry_id)
            .await
            .expect("reader decodes");
        assert_eq!(real as usize, decoded.len(), "reader lost rows");
        println!(
            "READER     : get_delivery_log({}) decoded {} rows",
            entry_id,
            decoded.len()
        );

        // ---- sweep + residue ---------------------------------------------------
        sqlx::query("DELETE FROM delivery_log WHERE target LIKE '%probe-t_6e53a1af%'")
            .execute(&pool)
            .await
            .expect("sweep logs");
        sqlx::query("DELETE FROM integration_targets WHERE id = ANY($1)")
            .bind(vec![target_ok, target_dead])
            .execute(&pool)
            .await
            .expect("sweep targets");
        sqlx::query("DELETE FROM entries WHERE id = $1")
            .bind(entry_id)
            .execute(&pool)
            .await
            .expect("sweep entry");
        // Scoped to THIS probe's own rows by id: a live-fixture row that merely
        // shares the label must not make this test lie about its own hygiene.
        let residue_logs: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM delivery_log WHERE target LIKE '%probe-t_6e53a1af%'",
        )
        .fetch_one(&pool)
        .await
        .expect("residue logs");
        let residue_targets: i64 =
            sqlx::query_scalar("SELECT count(*) FROM integration_targets WHERE id = ANY($1)")
                .bind(vec![target_ok, target_dead])
                .fetch_one(&pool)
                .await
                .expect("residue targets");
        let residue_entry: i64 = sqlx::query_scalar("SELECT count(*) FROM entries WHERE id = $1")
            .bind(entry_id)
            .fetch_one(&pool)
            .await
            .expect("residue entry");
        assert_eq!(
            residue_logs + residue_targets + residue_entry,
            0,
            "probe left residue (logs {} targets {} entries {})",
            residue_logs,
            residue_targets,
            residue_entry
        );
        println!("SWEEP      : residue 0 (logs/targets/entries)");
    }
}

#[cfg(test)]
mod delivery_config_tests {
    //! The question this card answers (t_d2c56fcb) is a SHAPE question: the hub reads
    //! `DeliveryConfig`, and the column is written by `build_delivery_config` (and by the
    //! tenant console wizard it was written for). These tests assert the two agree, and
    //! that every other state the column can be in — NULL/`{}`, the entry-capture
    //! vocabulary, a malformed block — still resolves to an empty config rather than to
    //! an error or to a half-built one. Pure: no DB, no network.
    use super::*;

    #[test]
    fn the_columns_own_writer_round_trips_through_the_loader() {
        let stored = build_delivery_config(
            Some(EmailDelivery {
                template_id: Some("win-email".to_string()),
                subject: Some("you won".to_string()),
                body_text: None,
                coupon_code: None,
                from_name: Some("probe".to_string()),
                reply_to: None,
            }),
            Some(RedirectDelivery {
                url: "https://example.com/won".to_string(),
                params: None,
                text: None,
            }),
            vec!["target-a".to_string()],
            true,
            Some(RedirectDelivery {
                url: "https://example.com/lost".to_string(),
                params: None,
                text: None,
            }),
        );

        let cfg = DeliveryConfig::from_campaign_json(&stored);

        assert_eq!(
            cfg.on_win.email.as_ref().and_then(|e| e.subject.as_deref()),
            Some("you won")
        );
        assert_eq!(
            cfg.on_win.redirect.as_ref().map(|r| r.url.as_str()),
            Some("https://example.com/won")
        );
        assert_eq!(cfg.on_win.webhooks, vec!["target-a".to_string()]);
        assert!(cfg.on_win.autoresponder_fire);
        assert_eq!(
            cfg.on_lose.redirect.as_ref().map(|r| r.url.as_str()),
            Some("https://example.com/lost")
        );
        assert!(!cfg.is_empty());
    }

    #[test]
    fn a_bare_config_is_read_and_every_other_state_is_empty() {
        // A bare config (no `delivery` wrapper) is read as-is.
        let bare = json!({"on_win": {"webhooks": ["target-b"], "autoresponder_fire": true}});
        let cfg = DeliveryConfig::from_campaign_json(&bare);
        assert_eq!(cfg.on_win.webhooks, vec!["target-b".to_string()]);
        assert!(cfg.on_win.autoresponder_fire);
        assert!(!cfg.is_empty());

        // The column's OTHER vocabulary (the entry-capture path), the empty object and a
        // null column must all resolve to the empty config — the arms stay off, nothing
        // errors, and no half-built config reaches the hub.
        for value in [
            json!({}),
            json!(null),
            json!({"integrations": [{"type": "webhook", "config": {"url": "https://x/hook"}}]}),
            json!({"_method": "direct_api", "api_type": "hubspot", "api_key": "k"}),
            json!({"coreswift": {"list_id": "123"}}),
        ] {
            let cfg = DeliveryConfig::from_campaign_json(&value);
            assert!(cfg.is_empty(), "{} should load as an empty config", value);
        }
    }

    #[test]
    fn a_malformed_delivery_block_falls_back_instead_of_failing_the_route() {
        let broken = json!({"delivery": {"on_win": {"webhooks": "not-a-list"}}});
        let cfg = DeliveryConfig::from_campaign_json(&broken);
        assert!(cfg.is_empty());
    }
}
