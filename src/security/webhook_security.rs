//! Webhook Security — domain allowlisting, private IP blocklist, and daily rate limiting.
//!
//! Any outbound webhook from the platform must pass three gates:
//! 1. Domain allowlist — the hostname must be in the target's allowlist (unless empty = allow all).
//! 2. Private IP blocklist — prevents SSRF attacks against internal infrastructure.
//! 3. Daily rate cap — each integration target has a configurable daily limit.

use crate::error::AppError;
use sqlx::PgPool;
use url::Url;
use std::net::IpAddr;

/// Check whether an IP address belongs to a private or reserved range.
/// Used to prevent SSRF attacks against internal infrastructure.
pub fn is_private_ip(addr: &IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => {
            let octets = v4.octets();
            match octets[0] {
                10 => true,                                          // 10.0.0.0/8
                127 => true,                                         // 127.0.0.0/8 (localhost)
                169 if octets[1] == 254 => true,                     // 169.254.0.0/16 (link-local)
                172 if (16..=31).contains(&octets[1]) => true,       // 172.16.0.0/12
                192 if octets[1] == 168 => true,                     // 192.168.0.0/16
                _ => false,
            }
        }
        IpAddr::V6(v6) => {
            // ::1 (IPv6 localhost)
            v6.octets() == [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]
        }
    }
}

/// Validate a webhook URL: checks domain allowlist AND resolves host to
/// reject private/reserved IPs (SSRF prevention).
/// Returns Ok(()) if the domain passes, Err with a descriptive message otherwise.
pub async fn validate_webhook_url(webhook_url: &str, allowed_domains: &[String]) -> Result<(), String> {
    // Parse the URL
    let parsed = Url::parse(webhook_url).map_err(|e| {
        format!("Invalid webhook URL '{}': {}", webhook_url, e)
    })?;

    let host = parsed.host_str().ok_or_else(|| {
        format!("Webhook URL '{}' has no host component", webhook_url)
    })?;

    // Resolve hostname to IP addresses and check against private blocklist (SSRF prevention)
    let addrs = tokio::net::lookup_host((host, 0)).await.map_err(|e| {
        format!("DNS resolution failed for '{}': {}", host, e)
    })?;

    for addr in addrs {
        if is_private_ip(&addr.ip()) {
            return Err(format!(
                concat!(
                    "Webhook URL resolves to a private/reserved IP address ({}). ",
                    "Outbound webhooks to internal infrastructure are blocked for security."
                ),
                addr.ip()
            ));
        }
    }

    // If the allowed_domains list is empty, all external domains are permitted
    if allowed_domains.is_empty() {
        return Ok(());
    }

    // Check if the hostname (or any subdomain of it) matches any allowed domain
    let host_lower = host.to_lowercase();
    for domain in allowed_domains {
        let domain_lower = domain.trim().to_lowercase();
        // Exact match or subdomain match (e.g., "hooks.example.com" matches "example.com")
        if host_lower == domain_lower || host_lower.ends_with(&format!(".{}", domain_lower)) {
            return Ok(());
        }
    }

    Err(format!(
        "Webhook URL domain '{}' is not in the allowed domains list: {:?}",
        host, allowed_domains
    ))
}

/// Check whether a given integration target has exceeded its daily webhook limit.
/// Returns Ok(true) if the target can fire, Ok(false) if over limit, or Err on DB failure.
pub async fn check_daily_limit(
    pool: &PgPool,
    target_id: &uuid::Uuid,
    daily_limit: i32,
) -> Result<bool, String> {
    if daily_limit <= 0 {
        return Err("Daily limit must be greater than 0".to_string());
    }

    // Count delivery_log entries for this target URL in the current UTC day
    let count: i64 = sqlx::query_scalar(
        r#"SELECT COUNT(*) FROM delivery_log
           WHERE target = (SELECT webhook_url FROM integration_targets WHERE id = $1)
             AND attempted_at >= date_trunc('day', now() AT TIME ZONE 'UTC')::timestamptz
             AND attempted_at < date_trunc('day', now() AT TIME ZONE 'UTC')::timestamptz + INTERVAL '1 day'"#
    )
    .bind(target_id)
    .fetch_one(pool)
    .await
    .map_err(|e| format!("DB error checking daily limit: {}", e))?;

    if count >= daily_limit as i64 {
        return Ok(false);
    }

    Ok(true)
}

/// Run both security checks before delivering a webhook.
/// Returns Ok(()) if all checks pass, AppError with descriptive message otherwise.
pub async fn check_webhook_security(
    pool: &PgPool,
    target_id: &uuid::Uuid,
    webhook_url: &str,
    allowed_domains: &[String],
    daily_limit: i32,
) -> Result<(), AppError> {
    // 1. Domain allowlist + private IP blocklist check
    validate_webhook_url(webhook_url, allowed_domains).await
        .map_err(|msg| AppError::Forbidden(format!("Webhook blocked by security policy: {}", msg)))?;

    // 2. Daily limit check
    let within_limit = check_daily_limit(pool, target_id, daily_limit).await
        .map_err(|msg| AppError::Internal(format!("Security check error: {}", msg)))?;

    if !within_limit {
        return Err(AppError::TooManyRequests(format!(
            "Webhook blocked by daily limit ({} calls/day). Reset at midnight UTC.",
            daily_limit
        )));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_validate_webhook_url_empty_allowlist() {
        assert!(validate_webhook_url("https://example.com/hook", &[]).await.is_ok());
        assert!(validate_webhook_url("http://evil.net/callback", &[]).await.is_ok());
    }

    #[tokio::test]
    async fn test_validate_webhook_url_exact_match() {
        let domains = vec!["example.com".to_string(), "api.good.com".to_string()];
        assert!(validate_webhook_url("https://example.com/hook", &domains).await.is_ok());
        assert!(validate_webhook_url("https://api.good.com/v1/callback", &domains).await.is_ok());
    }

    #[tokio::test]
    async fn test_validate_webhook_url_subdomain_match() {
        let domains = vec!["example.com".to_string()];
        assert!(validate_webhook_url("https://hooks.example.com/path", &domains).await.is_ok());
        assert!(validate_webhook_url("https://sub.hooks.example.com/path", &domains).await.is_ok());
    }

    #[tokio::test]
    async fn test_validate_webhook_url_rejected() {
        let domains = vec!["example.com".to_string()];
        assert!(validate_webhook_url("https://evil.com/hook", &domains).await.is_err());
        assert!(validate_webhook_url("https://example.evil.com/hook", &domains).await.is_err());
    }

    #[tokio::test]
    async fn test_validate_webhook_url_case_insensitive() {
        let domains = vec!["EXAMPLE.COM".to_string()];
        assert!(validate_webhook_url("https://example.com/hook", &domains).await.is_ok());
        assert!(validate_webhook_url("https://Example.COM/Hook", &domains).await.is_ok());
    }

    #[tokio::test]
    async fn test_validate_webhook_url_invalid_url() {
        let domains = vec!["example.com".to_string()];
        assert!(validate_webhook_url("not-a-url", &domains).await.is_err());
        assert!(validate_webhook_url("", &domains).await.is_err());
    }

    #[test]
    fn test_is_private_ip_v4() {
        // 10.0.0.0/8
        assert!(is_private_ip(&"10.0.0.1".parse::<IpAddr>().unwrap()));
        assert!(is_private_ip(&"10.255.255.255".parse::<IpAddr>().unwrap()));
        // 172.16.0.0/12
        assert!(is_private_ip(&"172.16.0.1".parse::<IpAddr>().unwrap()));
        assert!(is_private_ip(&"172.31.255.255".parse::<IpAddr>().unwrap()));
        // 192.168.0.0/16
        assert!(is_private_ip(&"192.168.1.1".parse::<IpAddr>().unwrap()));
        assert!(is_private_ip(&"192.168.255.255".parse::<IpAddr>().unwrap()));
        // 127.0.0.0/8 (localhost)
        assert!(is_private_ip(&"127.0.0.1".parse::<IpAddr>().unwrap()));
        assert!(is_private_ip(&"127.0.0.2".parse::<IpAddr>().unwrap()));
        // 169.254.0.0/16 (link-local)
        assert!(is_private_ip(&"169.254.0.1".parse::<IpAddr>().unwrap()));
        assert!(is_private_ip(&"169.254.255.255".parse::<IpAddr>().unwrap()));
        // Public IPs should NOT be private
        assert!(!is_private_ip(&"8.8.8.8".parse::<IpAddr>().unwrap()));
        assert!(!is_private_ip(&"1.1.1.1".parse::<IpAddr>().unwrap()));
    }

    #[test]
    fn test_is_private_ip_v6() {
        // ::1 (IPv6 localhost)
        assert!(is_private_ip(&"::1".parse::<IpAddr>().unwrap()));
        // Public IPv6 should NOT be private
        assert!(!is_private_ip(&"2001:4860:4860::8888".parse::<IpAddr>().unwrap()));
    }
}
