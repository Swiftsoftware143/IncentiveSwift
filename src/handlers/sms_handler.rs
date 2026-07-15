//! SMS/WhatsApp → Chat Funnel Bridge
//!
//! Routes inbound Telnyx messages into running chat funnel campaigns.
//!
//! Flow:
//!   1. User texts a keyword (e.g. "PLAY", "WIN", "QUIZ") to your Telnyx number
//!   2. This handler stores the message, then checks if any active `chat` campaign
//!      has a matching `chat_keyword` in its config
//!   3. If match: starts a chat session (tracks step, collected_data)
//!   4. Bot replies via Telnyx API with the next chat script step (or AI response)
//!   5. Subsequent messages from the same number route to the existing session
//!   6. When all steps complete or user submits their info, creates an entry
//!      and fires output actions (CoreSwift sync, email, webhook, n8n, etc.)

use crate::state::AppState;
use axum::{Json, extract::State};
use reqwest::Client;
use serde_json::{json, Value};
use uuid::Uuid;
use std::sync::Arc;

/// POST /api/v1/channels/sms/inbound — Telnyx inbound webhook with chat funnel routing
pub async fn channel_inbound_webhook(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let pool = &state.db;

    // Parse Telnyx webhook payload
    let event_type = body.pointer("/data/event_type")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    let message_id = body.pointer("/data/payload/id")
        .or_else(|| body.pointer("/data/id"))
        .and_then(|v| v.as_str());

    let from = body.pointer("/data/payload/from/phone_number")
        .or_else(|| body.pointer("/data/payload/from"))
        .and_then(|v| v.as_str());

    let to = body.pointer("/data/payload/to/0/phone_number")
        .or_else(|| body.pointer("/data/payload/to"))
        .and_then(|v| v.as_str());

    let text = body.pointer("/data/payload/text")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let direction = if event_type.contains("whatsapp") { "inbound_whatsapp" } else { "inbound" };

    if from.is_none() || to.is_none() {
        return Json(json!({"success": false, "error": "Missing from/to number"}));
    }

    let from_num = from.unwrap();
    let to_num = to.unwrap();
    let from_clean = from_num.trim_start_matches('+').to_string();
    let to_clean = to_num.trim_start_matches('+').to_string();
    let text_upper = text.to_uppercase().trim().to_string();

    // 1. Store inbound message
    if let Some(mid) = message_id {
        sqlx::query(
            "INSERT INTO inbound_messages (message_id, from_number, to_number, body, direction)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (message_id) DO NOTHING"
        )
        .bind(mid)
        .bind(&from_clean)
        .bind(&to_clean)
        .bind(text)
        .bind(direction)
        .execute(pool)
        .await
        .ok();
    }

    // 2. Check if there's an active chat session for this phone number
    let existing_session = sqlx::query_as::<_, (Uuid, String, String, i32, Option<Value>, Option<Uuid>)>(
        r#"SELECT id, phone, campaign_slug, step, collected_data, campaign_id
           FROM chat_sessions
           WHERE phone = $1 AND status = 'active'
           ORDER BY updated_at DESC
           LIMIT 1"#
    )
    .bind(&from_clean)
    .fetch_optional(pool)
    .await;

    match existing_session {
        Ok(Some((session_id, _, campaign_slug, step, collected_data, campaign_id_opt))) => {
            // We have an active session — route this message to the chat
            let cid = campaign_id_opt.unwrap_or(Uuid::nil());
            tokio::spawn({
                let state = state.clone();
                let sid = session_id;
                let slug = campaign_slug.clone();
                let cd = collected_data.clone();
                let msg = text.to_string();
                let phone = from_clean.clone();
                async move {
                    route_to_chat_session(&state, &sid, &cid, &slug, step, &cd, &msg, &phone).await;
                }
            });

            Json(json!({
                "success": true,
                "session": "active",
                "event_type": event_type,
            }))
        }
        _ => {
            // No active session — check if this is a keyword match
            if text_upper.is_empty() {
                return Json(json!({
                    "success": true,
                    "event_type": event_type,
                    "session": "none",
                    "note": "No keyword detected"
                }));
            }

            // Find a campaign where config->>'chat_keyword' matches (case-insensitive)
            let campaign = sqlx::query_as::<_, (Uuid, Uuid, String, String, Option<Value>)>(
                r#"SELECT id, account_id, slug, name, config
                   FROM campaigns
                   WHERE type = 'chat'
                     AND status = 'active'
                     AND config->>'chat_keyword' ILIKE $1
                   LIMIT 1"#
            )
            .bind(&text_upper)
            .fetch_optional(pool)
            .await;

            match campaign {
                Ok(Some((campaign_id, account_id, slug, name, config))) => {
                    let cfg = config.clone().unwrap_or_default();

                    // Create a new chat session
                    let session_id = Uuid::new_v4();
                    let step = 0;

                    sqlx::query(
                        "INSERT INTO chat_sessions (id, phone, campaign_slug, campaign_id, account_id, step, collected_data, status)
                         VALUES ($1, $2, $3, $4, $5, $6, '{}'::jsonb, 'active')"
                    )
                    .bind(session_id)
                    .bind(&from_clean)
                    .bind(&slug)
                    .bind(campaign_id)
                    .bind(account_id)
                    .bind(step)
                    .execute(pool)
                    .await
                    .ok();

                    // Update the inbound message with campaign info
                    if let Some(mid) = message_id {
                        sqlx::query(
                            "UPDATE inbound_messages SET campaign_slug = $1, account_id = $2, processed = true WHERE message_id = $3"
                        )
                        .bind(&slug)
                        .bind(account_id)
                        .bind(mid)
                        .execute(pool)
                        .await
                        .ok();
                    }

                    // Send first bot reply
                    let slug_spawn = slug.clone();
                    let slug_json = slug.clone();
                    tokio::spawn({
                        let state = state.clone();
                        let phone = from_clean.clone();
                        let cfg = cfg.clone();
                        let cid = campaign_id;
                        let sname = name.clone();
                        async move {
                            send_bot_reply(&state, &phone, &cid, &slug_spawn, step, &cfg, &[], &sname).await;
                        }
                    });

                    Json(json!({
                        "success": true,
                        "session": "new",
                        "campaign_slug": slug_json,
                        "campaign_name": name,
                        "event_type": event_type,
                    }))
                }
                _ => {
                    // No matching campaign — just acknowledge
                    Json(json!({
                        "success": true,
                        "event_type": event_type,
                        "session": "none",
                        "note": "No matching campaign found for keyword"
                    }))
                }
            }
        }
    }
}

/// Route an inbound message to an existing chat session
async fn route_to_chat_session(
    state: &AppState,
    session_id: &Uuid,
    campaign_id: &Uuid,
    campaign_slug: &str,
    step: i32,
    collected_data: &Option<Value>,
    message: &str,
    phone: &str,
) {
    let pool = &state.db;

    // Get campaign details
    let campaign = sqlx::query_as::<_, (String, Option<Value>)>(
        "SELECT name, config FROM campaigns WHERE id = $1"
    )
    .bind(campaign_id)
    .fetch_optional(pool)
    .await;

    let Ok(Some((campaign_name, config))) = campaign else {
        // Campaign deleted — end session
        let _ = sqlx::query("UPDATE chat_sessions SET status = 'ended' WHERE id = $1")
            .bind(session_id)
            .execute(pool).await;
        return;
    };

    let cfg = config.unwrap_or_default();
    let chat_ai = cfg.get("chat_ai").and_then(|v| v.as_bool()).unwrap_or(false);
    let chat_script = cfg.get("chat_script").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let collect_name = cfg.get("chat_collect_name").and_then(|v| v.as_bool()).unwrap_or(true);
    let collect_email = cfg.get("chat_collect_email").and_then(|v| v.as_bool()).unwrap_or(true);
    let collect_phone = cfg.get("chat_collect_phone").and_then(|v| v.as_bool()).unwrap_or(false);

    // Extract existing collected data
    let mut collected = collected_data.clone().unwrap_or_else(|| json!({}));

    // Try to extract info from the user's message
    if collect_name && !collected.get("name").and_then(|v| v.as_str()).map(|s| !s.is_empty()).unwrap_or(false) {
        collected["name"] = json!(message);
    } else if collect_email && !collected.get("email").and_then(|v| v.as_str()).map(|s| !s.is_empty()).unwrap_or(false) {
        if message.contains('@') && message.contains('.') {
            collected["email"] = json!(message);
        }
    } else if collect_phone && !collected.get("phone").and_then(|v| v.as_str()).map(|s| !s.is_empty()).unwrap_or(false) && collect_email {
        // After email, phone is the next piece
        collected["phone"] = json!(message);
    }

    let new_step = step + 1;
    let mut convo_complete = false;

    // Check if we've collected enough to complete the funnel
    let has_essential = if collect_name && collect_email {
        collected.get("name").and_then(|v| v.as_str()).map(|s| !s.is_empty()).unwrap_or(false)
            && collected.get("email").and_then(|v| v.as_str()).map(|s| !s.is_empty()).unwrap_or(false)
    } else if collect_name {
        collected.get("name").and_then(|v| v.as_str()).map(|s| !s.is_empty()).unwrap_or(false)
    } else if collect_email {
        collected.get("email").and_then(|v| v.as_str()).map(|s| !s.is_empty()).unwrap_or(false)
    } else {
        true
    };

    // Complete once we have essential data — send thank you + create entry
    if has_essential {
        convo_complete = true;
    }

    // Update session
    let status = if convo_complete { "completed" } else { "active" };
    let _ = sqlx::query(
        "UPDATE chat_sessions SET step = $1, collected_data = $2::jsonb, status = $3 WHERE id = $4"
    )
    .bind(new_step)
    .bind(json!(&collected).to_string())
    .bind(status)
    .bind(session_id)
    .execute(pool)
    .await;

    if convo_complete {
        // Funnel complete — create entry and fire output actions
        complete_chat_funnel(state, campaign_id, &campaign_name, &cfg, &collected, phone, campaign_slug).await;
    } else {
        // Send the next bot message
        send_bot_reply(state, phone, campaign_id, campaign_slug, new_step, &cfg, &[], &campaign_name).await;
    }
}

/// Send the next bot message in the chat funnel via Telnyx SMS
async fn send_bot_reply(
    state: &AppState,
    phone: &str,
    campaign_id: &Uuid,
    campaign_slug: &str,
    step: i32,
    config: &Value,
    _tags: &[String],
    campaign_name: &str,
) {
    let chat_ai = config.get("chat_ai").and_then(|v| v.as_bool()).unwrap_or(false);
    let chat_script = config.get("chat_script").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let chat_provider = config.get("chat_provider").and_then(|v| v.as_str()).unwrap_or("openai");
    let chat_prompt = config.get("chat_prompt").and_then(|v| v.as_str()).unwrap_or("");

    let message = if chat_ai {
        // Generate AI response (simplified — uses provider key)
        generate_ai_reply(state, campaign_id, chat_provider, chat_prompt, "").await
            .unwrap_or_else(|| "Thanks for chatting! Can you tell me your name?".to_string())
    } else {
        // Use scripted messages
        let idx = step as usize;
        if idx < chat_script.len() {
            chat_script[idx].as_str().unwrap_or("Thanks!").to_string()
        } else {
            // Script exhausted — last message
            "Thanks for participating! You'll receive a confirmation shortly.".to_string()
        }
    };

    // Send via Telnyx
    let _ = send_telnyx_sms(state, phone, &message).await;
}

/// Generate an AI reply using configured provider
async fn generate_ai_reply(
    state: &AppState,
    account_id: &Uuid,
    provider: &str,
    system_prompt: &str,
    user_message: &str,
) -> Option<String> {
    // Get provider API key
    let key = sqlx::query_as::<_, (String,)>( // Fixed: query_as for single column
        r#"SELECT api_key FROM provider_keys
           WHERE provider = $1 AND account_id = $2 AND is_active = true
           LIMIT 1"#
    )
    .bind(provider)
    .bind(account_id)
    .fetch_optional(&state.db)
    .await
    .ok()
    .and_then(|r| r)
    .map(|(k,)| k)?;

    if key.is_empty() { return None; }

    let url = match provider {
        "anthropic" => "https://api.anthropic.com/v1/messages",
        "deepseek" => "https://api.deepseek.com/v1/chat/completions",
        _ => "https://api.openai.com/v1/chat/completions",
    };

    let messages = json!([
        {"role": "system", "content": system_prompt},
        {"role": "user", "content": user_message}
    ]);

    let body = json!({
        "model": if provider == "anthropic" { "claude-3-haiku-20240307" } else if provider == "deepseek" { "deepseek-chat" } else { "gpt-4o-mini" },
        "messages": messages,
        "max_tokens": 200,
    });

    let client = Client::new();
    let resp = client
        .post(url)
        .header("Authorization", if provider == "anthropic" {
            format!("Bearer {}", &key)
        } else {
            format!("Bearer {}", &key)
        })
        .header("Content-Type", "application/json")
        .header("anthropic-version", "2023-06-01")
        .json(&body)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .ok()?;

    let result: Value = resp.json().await.ok()?;

    if provider == "anthropic" {
        result.pointer("/content/0/text")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    } else {
        result.pointer("/choices/0/message/content")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    }
}

/// Send an SMS via Telnyx API
async fn send_telnyx_sms(
    state: &AppState,
    to: &str,
    message: &str,
) -> Result<(), String> {
    // Find Telnyx SMS credentials for any account (use first active one)
    let creds = sqlx::query_as::<_, (String, Option<Value>)>(
        r#"SELECT api_key, metadata FROM provider_keys
           WHERE provider = 'telnyx_sms' AND is_active = true
           LIMIT 1"#
    )
    .fetch_optional(&state.db)
    .await
    .map_err(|e| format!("DB error: {}", e))?
    .ok_or_else(|| "No Telnyx key configured".to_string())?;

    let (api_key, meta) = creds;

    let from_number = meta.as_ref()
        .and_then(|m| m.get("from_number"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if from_number.is_empty() {
        return Err("No Telnyx from_number in metadata".to_string());
    }

    let payload = json!({
        "from": from_number,
        "to": if to.starts_with('+') { to.to_string() } else { format!("+{}", to) },
        "text": message,
    });

    let client = Client::new();
    let resp = client
        .post("https://api.telnyx.com/v2/messages")
        .header("Authorization", format!("Bearer {}", &api_key))
        .header("Content-Type", "application/json")
        .json(&payload)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| format!("Telnyx send failed: {}", e))?;

    let status = resp.status();
    tracing::info!("Telnyx SMS sent to {}: status={}", to, status);
    Ok(())
}

/// Complete the chat funnel — create entry + fire output actions
async fn complete_chat_funnel(
    state: &AppState,
    campaign_id: &Uuid,
    campaign_name: &str,
    config: &Value,
    collected_data: &Value,
    phone: &str,
    campaign_slug: &str,
) {
    let pool = &state.db;

    let name = collected_data.get("name").and_then(|v| v.as_str()).unwrap_or("SMS Lead");
    let email = collected_data.get("email").and_then(|v| v.as_str()).unwrap_or("");
    let phone_val = collected_data.get("phone").and_then(|v| v.as_str()).unwrap_or(phone);

    // Find or create contact
    let contact_id = if !email.is_empty() {
        let existing = sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM contacts WHERE email = $1 LIMIT 1"
        )
        .bind(email)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten();

        match existing {
            Some(eid) => {
                // Update existing contact
                let _ = sqlx::query(
                    "UPDATE contacts SET first_name = COALESCE(NULLIF($1, ''), first_name), phone = COALESCE(NULLIF($2, ''), phone), last_seen_at = NOW(), total_entries = total_entries + 1 WHERE id = $3"
                )
                .bind(name)
                .bind(phone_val)
                .bind(eid)
                .execute(pool)
                .await;
                eid
            }
            None => {
                // Insert new contact
                let new_id = Uuid::new_v4();
                let _ = sqlx::query(
                    "INSERT INTO contacts (id, first_name, email, phone, first_seen_at, last_seen_at, total_entries) VALUES ($1, $2, $3, $4, NOW(), NOW(), 1) ON CONFLICT (id) DO NOTHING"
                )
                .bind(new_id)
                .bind(name)
                .bind(email)
                .bind(phone_val)
                .execute(pool)
                .await;
                new_id
            }
        }
    } else {
        let new_id = Uuid::new_v4();
        let _ = sqlx::query(
            "INSERT INTO contacts (id, first_name, email, phone, first_seen_at, last_seen_at, total_entries) VALUES ($1, $2, $3, $4, NOW(), NOW(), 1) ON CONFLICT (id) DO NOTHING"
        )
        .bind(new_id)
        .bind(name)
        .bind(email)
        .bind(phone_val)
        .execute(pool)
        .await;
        new_id
    };

    // Create entry
    let entry_id = Uuid::new_v4();
    let _ = sqlx::query(
        "INSERT INTO entries (id, contact_id, campaign_id, outcome, answers, tags_applied, created_at) VALUES ($1, $2, $3, 'completed', $4::jsonb, '{chat_funnel}', NOW()) ON CONFLICT (id) DO NOTHING"
    )
    .bind(entry_id)
    .bind(contact_id)
    .bind(campaign_id)
    .bind(json!(collected_data).to_string())
    .execute(pool)
    .await;

    // Update session
    let _ = sqlx::query(
        "UPDATE chat_sessions SET status = 'completed', entry_id = $1 WHERE campaign_id = $2 AND phone = $3 AND status = 'completed'"
    )
    .bind(entry_id)
    .bind(campaign_id)
    .bind(phone.trim_start_matches('+'))
    .execute(pool)
    .await;

    // Find the account_id for output actions
    let account_info = sqlx::query_as::<_, (Uuid,)>(
        "SELECT account_id FROM campaigns WHERE id = $1"
    )
    .bind(campaign_id)
    .fetch_optional(pool)
    .await;

    let account_id = match account_info {
        Ok(Some((aid,))) => aid,
        _ => Uuid::nil(),
    };

    // Fire output actions
    let fn1_owned = name.to_string();
    let campaign_slug_owned = campaign_slug.to_string();
    let cname_owned = campaign_name.to_string();
    let cfg_owned = config.clone();
    let cd_owned = collected_data.clone();
    let email_owned = email.to_string();
    let phone_owned = phone_val.to_string();
    let campaign_id_owned = *campaign_id;

    tokio::spawn({
        let state = state.clone();
        async move {
            crate::delivery::output_actions::execute_output_actions(
                &state, &campaign_id_owned, &cname_owned, &campaign_slug_owned,
                &"chat".to_string(), &cfg_owned,
                &contact_id,
                &fn1_owned, "", &email_owned, &phone_owned, "", "",
                &account_id, &"completed".to_string(), &[],
                None,
                Some(&cd_owned),
                None, None, None,
                None, None,
            ).await;
        }
    });
}
