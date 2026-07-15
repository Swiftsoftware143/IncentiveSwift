//! Custom domain handler — allow tenants to use their own domain/subdomain
//!
//! Endpoints:
//!   GET  /api/v1/custom-domains        — list domains for current tenant
//!   POST /api/v1/custom-domains        — add a new domain
//!   DELETE /api/v1/custom-domains/:id  — remove a domain
//!   POST /api/v1/custom-domains/:id/verify — trigger verification

use crate::state::AppState;
use crate::security::auth::AuthenticatedUser;
use axum::{extract::{Path, State}, Json};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use chrono::{DateTime, Utc};
use serde_json::{json, Value};

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct CustomDomain {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub domain: String,
    pub target_type: String,
    pub verification_token: String,
    pub verified_at: Option<DateTime<Utc>>,
    pub ssl_provisioned_at: Option<DateTime<Utc>>,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
pub struct AddDomainRequest {
    pub domain: String,
}

/// GET /api/v1/custom-domains
pub async fn list_domains(
    State(state): State<AppState>,
    auth: AuthenticatedUser,
) -> Json<Value> {
    let account_id = Uuid::parse_str(&auth.account_id).unwrap_or(Uuid::nil());

    let domains = sqlx::query_as::<_, CustomDomain>(
        "SELECT * FROM custom_domains WHERE tenant_id = $1 ORDER BY created_at DESC"
    )
    .bind(account_id)
    .fetch_all(&state.db)
    .await;

    match domains {
        Ok(domains) => Json(json!({"success": true, "domains": domains})),
        Err(e) => Json(json!({"success": false, "error": e.to_string()})),
    }
}

/// POST /api/v1/custom-domains
pub async fn add_domain(
    State(state): State<AppState>,
    auth: AuthenticatedUser,
    Json(body): Json<AddDomainRequest>,
) -> Json<Value> {
    let account_id = Uuid::parse_str(&auth.account_id).unwrap_or(Uuid::nil());
    let domain = body.domain.trim().to_lowercase();

    // Basic validation
    if !domain.contains('.') {
        return Json(json!({"success": false, "error": "Invalid domain format"}));
    }

    // Check if domain already exists
    let existing = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM custom_domains WHERE domain = $1"
    )
    .bind(&domain)
    .fetch_one(&state.db)
    .await
    .unwrap_or(0);

    if existing > 0 {
        return Json(json!({"success": false, "error": "Domain already registered by another account"}));
    }

    // Generate verification token
    let verification_token = Uuid::new_v4().to_string();

    let result = sqlx::query_as::<_, CustomDomain>(
        "INSERT INTO custom_domains (tenant_id, domain, verification_token) VALUES ($1, $2, $3) RETURNING *"
    )
    .bind(account_id)
    .bind(&domain)
    .bind(&verification_token)
    .fetch_one(&state.db)
    .await;

    match result {
        Ok(d) => {
            // Try to auto-provision: check if Cloudflare API key exists and domain is on Cloudflare
            let auto_result = try_auto_provision(&state.db, &d).await;

            let token_record = d.verification_token.clone();
            let zone_id = auto_result.as_ref().ok().map(|z| z.clone());

            Json(json!({
                "success": true,
                "domain": d,
                "verification_token": token_record,
                "dns_instructions": {
                    "cname": {
                        "name": domain,
                        "type": "CNAME",
                        "value": "app.incentiveswift.com.",
                        "ttl": 300
                    },
                    "txt": {
                        "name": format!("_verify.{}", domain),
                        "type": "TXT",
                        "value": token_record,
                        "ttl": 300
                    }
                },
                "auto_provision": auto_result.is_ok(),
                "cloudflare_zone_id": zone_id,
            }))
        }
        Err(e) => Json(json!({"success": false, "error": e.to_string()})),
    }
}

/// DELETE /api/v1/custom-domains/:id
pub async fn remove_domain(
    State(state): State<AppState>,
    auth: AuthenticatedUser,
    Path(id): Path<Uuid>,
) -> Json<Value> {
    let account_id = Uuid::parse_str(&auth.account_id).unwrap_or(Uuid::nil());

    let result = sqlx::query(
        "DELETE FROM custom_domains WHERE id = $1 AND tenant_id = $2"
    )
    .bind(id)
    .bind(account_id)
    .execute(&state.db)
    .await;

    match result {
        Ok(r) => {
            if r.rows_affected() > 0 {
                // Regenerate nginx config
                let _ = regenerate_nginx_config(&state.db).await;
                Json(json!({"success": true, "message": "Domain removed"}))
            } else {
                Json(json!({"success": false, "error": "Domain not found"}))
            }
        }
        Err(e) => Json(json!({"success": false, "error": e.to_string()})),
    }
}

/// POST /api/v1/custom-domains/:id/verify
pub async fn verify_domain(
    State(state): State<AppState>,
    auth: AuthenticatedUser,
    Path(id): Path<Uuid>,
) -> Json<Value> {
    let account_id = Uuid::parse_str(&auth.account_id).unwrap_or(Uuid::nil());

    // Get the domain
    let domain = sqlx::query_as::<_, CustomDomain>(
        "SELECT * FROM custom_domains WHERE id = $1 AND tenant_id = $2"
    )
    .bind(id)
    .bind(account_id)
    .fetch_optional(&state.db)
    .await;

    match domain {
        Ok(Some(d)) => {
            // Verify TXT record
            match verify_dns_txt(&d.domain, &d.verification_token).await {
                Ok(true) => {
                    // Mark as verified
                    sqlx::query(
                        "UPDATE custom_domains SET verified_at = now(), is_active = true WHERE id = $1"
                    )
                    .bind(id)
                    .execute(&state.db)
                    .await
                    .ok();

                    // Auto-provision SSL via Cloudflare if available
                    let _ = try_auto_ssl(&state.db, &d).await;

                    // Regenerate nginx config
                    let _ = regenerate_nginx_config(&state.db).await;

                    Json(json!({
                        "success": true,
                        "message": "Domain verified! DNS records confirmed. Your campaign is now live on this domain.",
                        "verified_at": Utc::now(),
                    }))
                }
                Ok(false) => Json(json!({
                    "success": false,
                    "message": "DNS verification failed. Add TXT record _verify.{domain} with value \"{token}\" and ensure CNAME points to app.incentiveswift.com.",
                    "verification_token": d.verification_token,
                    "dns_type": "TXT",
                    "dns_name": format!("_verify.{}", d.domain),
                    "dns_value": d.verification_token,
                })),
                Err(e) => Json(json!({"success": false, "error": e})),
            }
        }
        Ok(None) => Json(json!({"success": false, "error": "Domain not found"})),
        Err(e) => Json(json!({"success": false, "error": e.to_string()})),
    }
}

/// Verify DNS TXT record exists for domain verification
async fn verify_dns_txt(domain: &str, expected_token: &str) -> Result<bool, String> {
    let txt_name = format!("_verify.{}", domain);
    // Use Google DNS over HTTPS to check
    let url = format!("https://dns.google/resolve?name={}&type=TXT", txt_name);
    let client = reqwest::Client::new();

    match client.get(&url).send().await {
        Ok(resp) => {
            if let Ok(data) = resp.json::<Value>().await {
                if let Some(answer) = data.get("Answer").and_then(|a| a.as_array()) {
                    for ans in answer {
                        if let Some(txt) = ans.get("data").and_then(|d| d.as_str()) {
                            let cleaned = txt.trim_matches('"').to_string();
                            if cleaned.contains(expected_token) {
                                return Ok(true);
                            }
                        }
                    }
                }
            }
            Ok(false)
        }
        Err(_) => Err("Failed to query DNS".to_string()),
    }
}

/// Try to add DNS records via Cloudflare API automatically
async fn try_auto_provision(pool: &sqlx::PgPool, domain: &CustomDomain) -> Result<String, String> {
    // Get Cloudflare API key
    let cf_key = sqlx::query_scalar::<_, Option<String>>(
        "SELECT api_key FROM provider_keys WHERE provider = 'cloudflare' AND (account_id IS NULL OR scope = 'account') AND is_active = true LIMIT 1"
    )
    .fetch_optional(pool)
    .await
    .map_err(|_| "DB error".to_string())?
    .flatten()
    .ok_or_else(|| "No Cloudflare API key configured".to_string())?;

    // Get Cloudflare email from metadata
    let cf_email = sqlx::query_scalar::<_, Option<String>>(
        "SELECT metadata->>'messaging_profile_id' FROM provider_keys WHERE provider = 'cloudflare' AND (account_id IS NULL OR scope = 'account') AND is_active = true LIMIT 1"
    )
    .fetch_optional(pool)
    .await
    .map_err(|_| "DB error".to_string())?
    .flatten()
    .ok_or_else(|| "No Cloudflare email configured".to_string())?;

    // Find the zone for this domain's root
    let domain_parts: Vec<&str> = domain.domain.split('.').collect();
    let root_domain = if domain_parts.len() >= 2 {
        format!("{}.{}", domain_parts[domain_parts.len() - 2], domain_parts[domain_parts.len() - 1])
    } else {
        return Err("Invalid domain".to_string());
    };

    let client = reqwest::Client::new();
    let auth_header = if cf_key.starts_with("cfat_") {
        format!("Bearer {}", cf_key)
    } else {
        return Err("Only API tokens supported for auto-provision".to_string());
    };

    // Find zone
    let zones_resp = client.get(&format!("https://api.cloudflare.com/client/v4/zones?name={}", root_domain))
        .header("Authorization", &auth_header)
        .header("Content-Type", "application/json")
        .send()
        .await
        .map_err(|e| e.to_string())?;

    let zones = zones_resp.json::<Value>().await.map_err(|e| e.to_string())?;
    let zone_id = zones["result"][0]["id"].as_str()
        .ok_or_else(|| format!("Zone {} not found on Cloudflare", root_domain))?
        .to_string();

    // Add CNAME record
    let cname_body = json!({
        "type": "CNAME",
        "name": &domain.domain,
        "content": "app.incentiveswift.com",
        "ttl": 120,
        "proxied": true
    });

    let cname_resp = client.post(&format!("https://api.cloudflare.com/client/v4/zones/{}/dns_records", zone_id))
        .header("Authorization", &auth_header)
        .header("Content-Type", "application/json")
        .body(cname_body.to_string())
        .send()
        .await
        .map_err(|e| e.to_string())?;

    let _cname_result = cname_resp.json::<Value>().await.unwrap_or_default();

    // Add TXT verification record
    let txt_body = json!({
        "type": "TXT",
        "name": format!("_verify.{}", domain.domain),
        "content": &domain.verification_token,
        "ttl": 120,
        "proxied": false
    });

    let txt_resp = client.post(&format!("https://api.cloudflare.com/client/v4/zones/{}/dns_records", zone_id))
        .header("Authorization", &auth_header)
        .header("Content-Type", "application/json")
        .body(txt_body.to_string())
        .send()
        .await
        .map_err(|e| e.to_string())?;

    let _txt_result = txt_resp.json::<Value>().await.unwrap_or_default();

    Ok(zone_id)
}

/// Try to auto-provision SSL via Cloudflare
async fn try_auto_ssl(pool: &sqlx::PgPool, domain: &CustomDomain) -> Result<(), String> {
    let cf_key = sqlx::query_scalar::<_, Option<String>>(
        "SELECT api_key FROM provider_keys WHERE provider = 'cloudflare' AND (account_id IS NULL OR scope = 'account') AND is_active = true LIMIT 1"
    )
    .fetch_optional(pool)
    .await
    .map_err(|_| "DB error".to_string())?
    .flatten()
    .ok_or_else(|| "No Cloudflare key".to_string())?;

    let auth_header = if cf_key.starts_with("cfat_") {
        format!("Bearer {}", cf_key)
    } else {
        return Err("Only API tokens supported".to_string());
    };

    let client = reqwest::Client::new();
    let domain_parts: Vec<&str> = domain.domain.split('.').collect();
    let root_domain = format!("{}.{}", domain_parts[domain_parts.len() - 2], domain_parts[domain_parts.len() - 1]);

    let zones_resp = client.get(&format!("https://api.cloudflare.com/client/v4/zones?name={}", root_domain))
        .header("Authorization", &auth_header)
        .header("Content-Type", "application/json")
        .send()
        .await
        .map_err(|e| e.to_string())?;

    let zones = zones_resp.json::<Value>().await.map_err(|e| e.to_string())?;
    let zone_id = zones["result"][0]["id"].as_str()
        .ok_or_else(|| "Zone not found".to_string())?
        .to_string();

    // Enable SSL for the zone (it's already on by default with proxied CNAME)
    // Just mark as provisioned
    sqlx::query(
        "UPDATE custom_domains SET ssl_provisioned_at = now() WHERE id = $1"
    )
    .bind(domain.id)
    .execute(pool)
    .await
    .ok();

    Ok(())
}

/// Regenerate nginx config for all active custom domains
async fn regenerate_nginx_config(pool: &sqlx::PgPool) -> Result<(), String> {
    let domains = sqlx::query_as::<_, CustomDomain>(
        "SELECT * FROM custom_domains WHERE is_active = true AND verified_at IS NOT NULL ORDER BY domain"
    )
    .fetch_all(pool)
    .await
    .map_err(|e| e.to_string())?;

    // Build nginx config block
    let mut config = String::new();
    config.push_str("# Auto-generated by IncentiveSwift custom domains\n");
    config.push_str("# DO NOT EDIT — changes will be overwritten\n\n");

    for d in &domains {
        config.push_str(&format!(
            r#"
server {{
    listen 443 ssl http2;
    server_name {domain};

    ssl_certificate /etc/letsencrypt/live/app.incentiveswift.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/app.incentiveswift.com/privkey.pem;

    # Serve Play SPA for all routes
    root /var/www/incentiveswift;
    index play.html;

    # API proxy to backend
    location /api/ {{
        proxy_pass http://127.0.0.1:8083;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
    }}

    # Serve the Play SPA
    location / {{
        try_files /play.html =404;
        add_header X-IncentiveSwift-Domain {domain} always;
    }}

    # Redirect HTTP → HTTPS
}}
server {{
    listen 80;
    server_name {domain};
    return 301 https://$host$request_uri;
}}
"#,
            domain = d.domain
        ));
    }

    // Write config to disk
    if !domains.is_empty() {
        tokio::fs::write("/etc/nginx/sites-enabled/incentiveswift-custom-domains.conf", &config)
            .await
            .map_err(|e| format!("Failed to write nginx config: {}", e))?;

        // Reload nginx
        std::process::Command::new("nginx")
            .arg("-t")
            .output()
            .map_err(|e| format!("nginx test failed: {}", e))?;

        std::process::Command::new("systemctl")
            .args(&["reload", "nginx"])
            .output()
            .map_err(|e| format!("nginx reload failed: {}", e))?;
    }

    Ok(())
}

/// POST /api/v1/custom-domains/regenerate-nginx — force regenerate nginx config
pub async fn regenerate_handler(
    State(state): State<AppState>,
) -> Json<Value> {
    match regenerate_nginx_config(&state.db).await {
        Ok(()) => Json(json!({"success": true, "message": "Nginx config regenerated"})),
        Err(e) => Json(json!({"success": false, "error": e})),
    }
}
