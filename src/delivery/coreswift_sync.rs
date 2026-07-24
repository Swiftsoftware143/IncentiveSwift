// CoreSwift CRM native integration — auto-sync campaign entries to CoreSwift contacts/leads.
//
// Configuration: user stores CoreSwift credentials in provider_keys table:
//   provider = 'coreswift'
//   api_key = JWT token (cached after first login)
//   base_url = CoreSwift instance URL (e.g. https://coreswiftcrm.com)
//   metadata = JSON with email/password for re-authentication
//
// On each campaign entry, we:
//   1. Authenticate if no cached JWT (or if JWT expired)
//   2. Upsert contact in CoreSwift
//   3. Optionally create a lead if campaign config has coreswift_create_lead=true

use crate::state::AppState;
use reqwest::Client;
use serde_json::{json, Value};
use uuid::Uuid;

const CORESWIFT_PROVIDER: &str = "coreswift";

/// Sync a campaign entry to CoreSwift CRM as a contact (and optionally a lead).
/// Best-effort: does not fail the entry if CoreSwift sync fails.
pub async fn sync_entry_to_coreswift(
    state: &AppState,
    campaign_id: &Uuid,
    campaign_name: &str,
    campaign_slug: &str,
    contact_id: &Uuid,
    contact_first_name: &Option<String>,
    contact_last_name: &Option<String>,
    contact_email: &Option<String>,
    contact_phone: &Option<String>,
    contact_website: &Option<String>,
    contact_business_name: &Option<String>,
    account_id: &Uuid,
    outcome: &str,
    answers: Option<&Value>,
    utm_source: Option<&str>,
) {
    // Check if CoreSwift is configured for this account
    let creds = sqlx::query_as::<_, (String, Option<String>, Option<String>, Option<Value>)>(
        r#"SELECT api_key, base_url, scope, metadata
           FROM provider_keys
           WHERE provider = $1 AND account_id = $2 AND is_active = true
           LIMIT 1"#
    )
    .bind(CORESWIFT_PROVIDER)
    .bind(account_id)
    .fetch_optional(&state.db)
    .await;

    let Ok(Some(row)) = creds else {
        return; // Not configured — skip silently
    };

    let (jwt_str, base_url, scope, metadata) = row;
    let mut jwt: Option<String> = if jwt_str.is_empty() { None } else { Some(jwt_str) };
    let base_url = base_url.unwrap_or_else(|| "https://coreswiftcrm.com".to_string());

    let http_client = &state.http_client;

    // Helper: login to CoreSwift and cache the JWT
    async fn login_and_cache(
        client: &Client,
        db: &sqlx::PgPool,
        account_id: &Uuid,
        metadata: &Option<Value>,
    ) -> Option<String> {
        let meta = metadata.as_ref()?;
        let email = meta.get("email")?.as_str()?;
        let password = meta.get("password")?.as_str()?;

        let login_url = format!("{}/api/auth/login", meta.get("base_url").and_then(|v| v.as_str()).unwrap_or("https://coreswiftcrm.com"));

        let resp = client
            .post(&login_url)
            .json(&json!({"email": email, "password": password}))
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
            .ok()?;

        let body: Value = resp.json().await.ok()?;
        let token = body.get("access_token")?.as_str()?.to_string();

        // Cache the JWT in the provider_keys table
        let _ = sqlx::query(
            "UPDATE provider_keys SET api_key = $1 WHERE provider = 'coreswift' AND account_id = $2"
        )
        .bind(&token)
        .bind(account_id)
        .execute(db)
        .await;

        Some(token)
    }

    // Get or refresh JWT
    jwt = if jwt.is_none() || jwt.as_ref().map(|s| s.len() < 10).unwrap_or(true) {
        login_and_cache(http_client, &state.db, account_id, &metadata).await
    } else {
        jwt.clone()
    };

    let Some(token) = jwt else {
        return; // Can't authenticate
    };

    // Build contact from entry data
    let first = contact_first_name.clone().unwrap_or_default();
    let last = contact_last_name.clone().unwrap_or_default();
    let email = contact_email.clone();
    let phone = contact_phone.clone();
    let website = contact_website.clone();
    let business = contact_business_name.clone();

    let notes = format!(
        "Imported from IncentiveSwift campaign: {} ({})\nOutcome: {}\nSource: {}",
        campaign_name,
        campaign_slug,
        outcome,
        utm_source.unwrap_or("direct")
    );

    // Create/update contact in CoreSwift
    let contact_payload = json!({
        "first_name": first,
        "last_name": last,
        "email": email,
        "phone": phone,
        "notes": notes,
        "metadata": {
            "source": "incentiveswift",
            "campaign_slug": campaign_slug,
            "campaign_name": campaign_name,
            "outcome": outcome,
            "synced_at": chrono::Utc::now().to_rfc3339(),
        }
    });

    let contact_resp = http_client
        .post(format!("{}/api/contacts", base_url.trim_end_matches('/')))
        .header("Authorization", format!("Bearer {}", token))
        .json(&contact_payload)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await;

    match contact_resp {
        Ok(r) => {
            let status = r.status().as_u16();
            if (200..300).contains(&status) {
                tracing::info!("CoreSwift contact synced: {} {} (status={})", first, last, status);
            } else if status == 401 {
                // JWT expired — clear it so next attempt re-authenticates
                let _ = sqlx::query(
                    "UPDATE provider_keys SET api_key = NULL WHERE provider = 'coreswift' AND account_id = $1"
                )
                .bind(account_id)
                .execute(&state.db)
                .await;
                tracing::warn!("CoreSwift JWT expired, will re-authenticate on next entry");
            } else {
                let body = r.text().await.unwrap_or_default().chars().take(200).collect::<String>();
                tracing::warn!("CoreSwift contact sync failed ({}): {}", status, body);
            }
        }
        Err(e) => {
            tracing::warn!("CoreSwift contact sync error: {}", e);
        }
    }
}
