//! Credit system handler — balance, deduction, top-up, history

use axum::{
    extract::{Query, State},
    Json,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use crate::state::AppState;
use crate::security::auth::AuthenticatedUser;

#[derive(Debug, Serialize, Deserialize)]
pub struct CreditTransaction {
    pub id: Uuid,
    pub amount: i32,
    pub balance_after: i32,
    pub action: String,
    pub reference_type: Option<String>,
    pub reference_id: Option<String>,
    pub description: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CreditHistoryQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub action: Option<String>,
}

/// GET /api/v1/credits/balance — get current credit balance
pub async fn get_balance(
    State(state): State<AppState>,
    auth: AuthenticatedUser,
) -> Json<serde_json::Value> {
    let account_id = auth.account_id.parse::<Uuid>().unwrap_or(Uuid::nil());
    let pool = &state.db;

    let row = sqlx::query_as::<_, (i32, i32)>(
        "SELECT credits_balance, credits_lifetime_used FROM accounts WHERE id = $1"
    )
    .bind(account_id)
    .fetch_optional(pool)
    .await;

    match row {
        Ok(Some((balance, lifetime_used))) => {
            // Get plan info for monthly + overdraft limits
            let plan_info = sqlx::query_as::<_, (Option<i32>, Option<i32>, Option<String>)>(
                "SELECT (p.features->>'credits_monthly')::int, (p.features->>'credits_overdraft')::int, p.name
                 FROM accounts a JOIN plans p ON a.plan_tier_id = p.id WHERE a.id = $1"
            )
            .bind(account_id)
            .fetch_optional(pool)
            .await;

            let (credits_monthly, credits_overdraft, plan_name) = match plan_info {
                Ok(Some((cm, co, pn))) => (cm.unwrap_or(0), co.unwrap_or(0), pn.unwrap_or_else(|| "Unknown".to_string())),
                _ => (0, 0, "Unknown".to_string()),
            };

            Json(serde_json::json!({
                "success": true,
                "balance": balance,
                "lifetime_used": lifetime_used,
                "plan_name": plan_name,
                "credits_monthly": credits_monthly,
                "credits_overdraft": credits_overdraft,
            }))
        }
        Ok(None) => Json(serde_json::json!({"success": false, "error": "Account not found"})),
        Err(e) => Json(serde_json::json!({"success": false, "error": e.to_string()})),
    }
}

/// GET /api/v1/credits/history — get credit transaction history
pub async fn get_history(
    State(state): State<AppState>,
    auth: AuthenticatedUser,
    Query(q): Query<CreditHistoryQuery>,
) -> Json<serde_json::Value> {
    let account_id = auth.account_id.parse::<Uuid>().unwrap_or(Uuid::nil());
    let pool = &state.db;
    let limit = q.limit.unwrap_or(50).min(200);
    let offset = q.offset.unwrap_or(0);

    let rows = if let Some(ref action) = q.action {
        sqlx::query_as::<_, (Uuid, i32, i32, String, Option<String>, Option<String>, Option<String>, chrono::DateTime<chrono::Utc>)>(
            "SELECT id, amount, balance_after, action, reference_type, reference_id, description, created_at
             FROM credit_transactions WHERE account_id = $1 AND action = $2
             ORDER BY created_at DESC LIMIT $3 OFFSET $4"
        )
        .bind(account_id)
        .bind(action)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await
    } else {
        sqlx::query_as::<_, (Uuid, i32, i32, String, Option<String>, Option<String>, Option<String>, chrono::DateTime<chrono::Utc>)>(
            "SELECT id, amount, balance_after, action, reference_type, reference_id, description, created_at
             FROM credit_transactions WHERE account_id = $1
             ORDER BY created_at DESC LIMIT $2 OFFSET $3"
        )
        .bind(account_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await
    };

    let total = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM credit_transactions WHERE account_id = $1"
    )
    .bind(account_id)
    .fetch_one(pool)
    .await
    .unwrap_or(0);

    match rows {
        Ok(rows) => {
            let transactions: Vec<CreditTransaction> = rows.into_iter().map(|(id, amount, balance_after, action, reference_type, reference_id, description, created_at)| {
                CreditTransaction { id, amount, balance_after, action, reference_type, reference_id, description, created_at }
            }).collect();

            Json(serde_json::json!({
                "success": true,
                "transactions": transactions,
                "total": total,
            }))
        }
        Err(e) => Json(serde_json::json!({"success": false, "error": e.to_string()})),
    }
}

/// Deduct credits for a specific action
pub async fn deduct_credits(
    pool: &sqlx::PgPool,
    account_id: Uuid,
    action: &str,
    reference_type: Option<&str>,
    reference_id: Option<&str>,
    description: Option<&str>,
) -> Result<(bool, i32, i32), String> {
    // Get plan + credit costs
    let plan_info = sqlx::query_as::<_, (i32, i32, i32)>(
        "SELECT COALESCE(a.credits_balance, 0),
                COALESCE((p.features->>'credits_monthly')::int, 0),
                COALESCE((p.features->>'credits_overdraft')::int, 0)
         FROM accounts a JOIN plans p ON a.plan_tier_id = p.id WHERE a.id = $1"
    )
    .bind(account_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| e.to_string())?
    .ok_or_else(|| "Account not found".to_string())?;

    let (balance, _credits_monthly, _credits_overdraft) = plan_info;

    // Get cost for this action from plan features
    let cost_key = format!("cost_{}", action.replace("usage_", ""));
    let cost = sqlx::query_scalar::<_, Option<i32>>(
        &format!("SELECT (p.features->>'{}')::int FROM accounts a JOIN plans p ON a.plan_tier_id = p.id WHERE a.id = $1", cost_key)
    )
    .bind(account_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| e.to_string())?
    .flatten()
    .unwrap_or(1);

    // Check if user has enough credits
    if balance < cost {
        return Ok((false, balance, cost));
    }

    // Deduct
    let new_balance = balance - cost;
    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;

    sqlx::query("UPDATE accounts SET credits_balance = $1, credits_lifetime_used = credits_lifetime_used + $2 WHERE id = $3")
        .bind(new_balance)
        .bind(cost)
        .bind(account_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;

    sqlx::query(
        "INSERT INTO credit_transactions (account_id, amount, balance_after, action, reference_type, reference_id, description)
         VALUES ($1, $2, $3, $4, $5, $6, $7)"
    )
    .bind(account_id)
    .bind(-cost)
    .bind(new_balance)
    .bind(action)
    .bind(reference_type.map(|s| s.to_string()))
    .bind(reference_id.map(|s| s.to_string()))
    .bind(description.map(|s| s.to_string()))
    .execute(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;

    tx.commit().await.map_err(|e| e.to_string())?;

    Ok((true, new_balance, cost))
}

/// Check if account has enough credits (without deducting)
pub async fn check_credits(
    pool: &sqlx::PgPool,
    account_id: Uuid,
    action: &str,
) -> Result<(bool, i32, i32), String> {
    let info = sqlx::query_as::<_, (i32, Option<i32>)>(
        "SELECT a.credits_balance, (p.features->>$1)::int
         FROM accounts a JOIN plans p ON a.plan_tier_id = p.id WHERE a.id = $2"
    )
    .bind(format!("cost_{}", action))
    .bind(account_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| e.to_string())?
    .ok_or_else(|| "Account not found".to_string())?;

    let (balance, cost_raw) = info;
    let cost = cost_raw.unwrap_or(1);

    Ok((balance >= cost, balance, cost))
}

// --- Credit top-up via Stripe ---

/// POST /api/v1/credits/topup — create a Stripe checkout session for credit top-up
pub async fn create_topup_checkout(
    State(state): State<AppState>,
    auth: AuthenticatedUser,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let amount = body.get("amount").and_then(|v| v.as_i64()).unwrap_or(0);
    let credits = body.get("credits").and_then(|v| v.as_i64()).unwrap_or(0);

    if amount <= 0 || credits <= 0 {
        return Json(serde_json::json!({
            "success": false, "error": "Invalid amount or credits"
        }));
    }

    // Get Stripe key
    let stripe_key = sqlx::query_scalar::<_, String>(
        "SELECT api_key FROM provider_keys WHERE provider = 'stripe' AND (account_id IS NULL OR scope = 'account') AND is_active = true LIMIT 1"
    )
    .fetch_optional(&state.db)
    .await;

    match stripe_key {
        Ok(Some(key)) => {
            let account_id = auth.account_id.parse::<Uuid>().unwrap_or(Uuid::nil());
            let success_url = format!("https://app.incentiveswift.com/admin/credits?checkout=success&credits={}", credits);
            let cancel_url = "https://app.incentiveswift.com/admin/credits?checkout=cancel".to_string();

            match create_stripe_session(&key, amount as i64, &success_url, &cancel_url, account_id, credits as i32).await {
                Some(session_url) => {
                    // Store pending checkout
                    let stripe_session_id = session_url.split('/').last().unwrap_or("").to_string();
                    sqlx::query(
                        "INSERT INTO stripe_checkout_sessions (account_id, stripe_session_id, amount, credits, status)
                         VALUES ($1, $2, $3, $4, 'pending')"
                    )
                    .bind(account_id)
                    .bind(&stripe_session_id)
                    .bind(amount as i32)
                    .bind(credits as i32)
                    .execute(&state.db)
                    .await
                    .ok();

                    Json(serde_json::json!({
                        "success": true,
                        "url": session_url,
                        "credits": credits,
                        "amount_cents": amount,
                    }))
                }
                None => Json(serde_json::json!({
                    "success": false, "error": "Failed to create Stripe checkout session"
                })),
            }
        }
        Ok(None) => Json(serde_json::json!({
            "success": false, "error": "Stripe not configured. Ask the admin to add Stripe API key in Provider Keys."
        })),
        Err(e) => Json(serde_json::json!({"success": false, "error": e.to_string()})),
    }
}

/// POST /api/v1/stripe/webhook — Stripe webhook handler (public, no auth)
pub async fn stripe_webhook(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> Json<serde_json::Value> {
    let pool = &state.db;

    match serde_json::from_slice::<serde_json::Value>(&body) {
        Ok(event) => {
            let event_type = event.get("type").and_then(|v| v.as_str()).unwrap_or("unknown");

            if let Some(session_id) = extract_session_id(&event) {
                let amount = event.pointer("/data/object/amount_total").and_then(|v| v.as_i64()).unwrap_or(0);
                let _credits = (amount / 100) as i32 * 10;

                // Update session and credit account
                if let Ok(Some((acc_id, cr))) = sqlx::query_as::<_, (Uuid, i32)>(
                    "UPDATE stripe_checkout_sessions SET status = 'completed', completed_at = now()
                     WHERE stripe_session_id = $1 AND status = 'pending' RETURNING account_id, credits"
                )
                .bind(&session_id)
                .fetch_optional(pool)
                .await
                {
                    let _ = add_credits_internal(pool, acc_id, cr, "top_up", Some("stripe"), Some(&session_id),
                        &Some(format!("Stripe top-up: ${}", amount as f64 / 100.0))).await;
                }
            }

            Json(serde_json::json!({"success": true, "event_type": event_type}))
        }
        Err(e) => Json(serde_json::json!({"success": false, "error": e.to_string()})),
    }
}

fn extract_session_id(event: &serde_json::Value) -> Option<String> {
    event.pointer("/data/object/id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

async fn create_stripe_session(
    api_key: &str,
    amount_cents: i64,
    success_url: &str,
    cancel_url: &str,
    _account_id: Uuid,
    _credits: i32,
) -> Option<String> {
    let client = reqwest::Client::new();
    let params = [
        ("mode", "payment"),
        ("payment_method_types[]", "card"),
        ("line_items[0][price_data][currency]", "usd"),
        ("line_items[0][price_data][unit_amount]", &amount_cents.to_string()),
        ("line_items[0][price_data][product_data][name]", &format!("{} Credits", _credits)),
        ("line_items[0][quantity]", "1"),
        ("success_url", success_url),
        ("cancel_url", cancel_url),
    ];

    let resp = client.post("https://api.stripe.com/v1/checkout/sessions")
        .header("Authorization", format!("Bearer {}", api_key))
        .form(&params)
        .send()
        .await
        .ok()?;

    let json: serde_json::Value = resp.json().await.ok()?;
    json.get("url").and_then(|v| v.as_str()).map(|s| s.to_string())
}

/// POST /api/v1/admin/credits/adjust — admin adjusts a user's credits
pub async fn admin_adjust_credits(
    State(state): State<AppState>,
    auth: AuthenticatedUser,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    // Verify admin role
    let is_admin = sqlx::query_scalar::<_, String>(
        "SELECT role FROM accounts WHERE id = $1"
    )
    .bind(auth.account_id.parse::<Uuid>().unwrap_or(Uuid::nil()))
    .fetch_optional(&state.db)
    .await;

    match is_admin {
        Ok(Some(ref role)) if role == "admin" || role == "super_admin" => {
            let target_id = body.get("account_id")
                .and_then(|v| v.as_str())
                .and_then(|s| Uuid::parse_str(s).ok());
            let amount = body.get("amount").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
            let reason = body.get("reason").and_then(|v| v.as_str()).unwrap_or("Admin adjustment");

            match (target_id, amount) {
                (Some(tid), amt) if amt != 0 => {
                    let result = add_credits_internal(
                        &state.db, tid, amt, "admin_adjust", None, None,
                        &Some(reason.to_string())
                    ).await;
                    match result {
                        Ok(balance) => Json(serde_json::json!({
                            "success": true,
                            "new_balance": balance,
                            "adjusted_by": amt,
                        })),
                        Err(e) => Json(serde_json::json!({"success": false, "error": e})),
                    }
                }
                _ => Json(serde_json::json!({"success": false, "error": "Invalid account_id or amount"})),
            }
        }
        _ => Json(serde_json::json!({"success": false, "error": "Admin access required"})),
    }
}

// Internal: add credits to an account
pub async fn add_credits_internal(
    pool: &sqlx::PgPool,
    account_id: Uuid,
    amount: i32,
    action: &str,
    reference_type: Option<&str>,
    reference_id: Option<&str>,
    description: &Option<String>,
) -> Result<i32, String> {
    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;

    let current = sqlx::query_scalar::<_, i32>(
        "SELECT credits_balance FROM accounts WHERE id = $1 FOR UPDATE"
    )
    .bind(account_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| e.to_string())?
    .unwrap_or(0);

    let new_balance = current + amount;

    sqlx::query("UPDATE accounts SET credits_balance = $1 WHERE id = $2")
        .bind(new_balance)
        .bind(account_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;

    sqlx::query(
        "INSERT INTO credit_transactions (account_id, amount, balance_after, action, reference_type, reference_id, description)
         VALUES ($1, $2, $3, $4, $5, $6, $7)"
    )
    .bind(account_id)
    .bind(amount)
    .bind(new_balance)
    .bind(action)
    .bind(reference_type.map(|s| s.to_string()))
    .bind(reference_id.map(|s| s.to_string()))
    .bind(description.as_ref().map(|s| s.to_string()))
    .execute(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;

    tx.commit().await.map_err(|e| e.to_string())?;

    Ok(new_balance)
}

// --- SMS Inbound Webhook ---

/// POST /api/v1/channels/sms/inbound — Telnyx SMS/WhatsApp inbound webhook (public, no auth)
pub async fn sms_inbound_webhook(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let pool = &state.db;

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
        .and_then(|v| v.as_str());

    let direction = if event_type.contains("whatsapp") { "inbound_whatsapp" } else { "inbound" };

    if let (Some(from_num), Some(to_num)) = (from, to) {
        let from_clean = from_num.trim_start_matches('+').to_string();
        let to_clean = to_num.trim_start_matches('+').to_string();

        sqlx::query(
            "INSERT INTO inbound_messages (message_id, from_number, to_number, body, direction)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (message_id) DO NOTHING"
        )
        .bind(message_id)
        .bind(&from_clean)
        .bind(&to_clean)
        .bind(text)
        .bind(direction)
        .execute(pool)
        .await
        .ok();
    }

    Json(serde_json::json!({
        "success": true,
        "event_type": event_type,
    }))
}
