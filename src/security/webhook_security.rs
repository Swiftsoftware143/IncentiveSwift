//! Webhook Security — domain allowlisting, private IP blocklist, and daily rate limiting.
//!
//! Any outbound webhook from the platform must pass three gates:
//! 1. Domain allowlist — the hostname must be in the target's allowlist (unless empty = allow all).
//! 2. Private IP blocklist — prevents SSRF attacks against internal infrastructure.
//! 3. Daily rate cap — each integration target has a configurable daily limit.

use crate::error::AppError;
use sqlx::PgPool;
// `url` is not a direct dependency of this crate (it is reqwest's), so the type comes in through
// reqwest's own re-export — same type, same version pivot, no manifest change (kanban t_016c839c,
// which is also what first compiled this module: `pub mod webhook_security` had never been
// declared, so this file was dead source until the test-webhook route became its caller).
use reqwest::Url;
use std::net::IpAddr;

/// Check whether an IP address belongs to a private or reserved range.
/// Used to prevent SSRF attacks against internal infrastructure.
pub fn is_private_ip(addr: &IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => {
            let octets = v4.octets();
            match octets[0] {
                10 => true,                                    // 10.0.0.0/8
                127 => true,                                   // 127.0.0.0/8 (localhost)
                169 if octets[1] == 254 => true,               // 169.254.0.0/16 (link-local)
                172 if (16..=31).contains(&octets[1]) => true, // 172.16.0.0/12
                192 if octets[1] == 168 => true,               // 192.168.0.0/16
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
pub async fn validate_webhook_url(
    webhook_url: &str,
    allowed_domains: &[String],
) -> Result<(), String> {
    // Parse the URL
    let parsed = Url::parse(webhook_url)
        .map_err(|e| format!("Invalid webhook URL '{}': {}", webhook_url, e))?;

    let host = parsed
        .host_str()
        .ok_or_else(|| format!("Webhook URL '{}' has no host component", webhook_url))?;

    // Resolve hostname to IP addresses and check against private blocklist (SSRF prevention)
    let addrs = tokio::net::lookup_host((host, 0))
        .await
        .map_err(|e| format!("DNS resolution failed for '{}': {}", host, e))?;

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

    // Check if the hostname (or any subdomain of it) matches any allowed domain
    if !host_matches_allowlist(host, allowed_domains) {
        return Err(format!(
            "Webhook URL domain '{}' is not in the allowed domains list: {:?}",
            host, allowed_domains
        ));
    }

    Ok(())
}

/// Does `host` pass the allowlist? An EMPTY allowlist permits every host (the caller has already
/// been through the private/reserved-IP gate above). Otherwise it is an exact match or a subdomain
/// of an allowed domain, case-insensitively: `hooks.example.com` matches `example.com`.
///
/// Split out of `validate_webhook_url` so the allowlist rule can be proven without a network: the
/// URL gate resolves the host FIRST and fails closed on an unresolvable one, which makes any
/// hostname-based assertion a DNS-dependent test (kanban t_016c839c — the module's original tests
/// asserted `api.good.com`/`hooks.example.com` were allowed, and neither name resolves).
pub fn host_matches_allowlist(host: &str, allowed_domains: &[String]) -> bool {
    if allowed_domains.is_empty() {
        return true;
    }
    let host_lower = host.to_lowercase();
    allowed_domains.iter().any(|domain| {
        let domain_lower = domain.trim().to_lowercase();
        !domain_lower.is_empty()
            && (host_lower == domain_lower || host_lower.ends_with(&format!(".{}", domain_lower)))
    })
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

/// The gate for a TENANT-supplied outbound webhook destination that has no `integration_targets`
/// row of its own (a campaign's `config.entry_webhook_url`, an `output_actions` webhook, a reward
/// webhook, a campaign `delivery_config` webhook).
///
/// The destination is looked up in `integration_targets`: a matching active row brings its own
/// allowlist and daily cap along, so those deliveries get the full three-arm gate. With no
/// matching row the private/reserved-IP refusal still applies (an empty allowlist permits every
/// *public* destination, which is the module's own documented semantics). This is the same
/// resolution the test-webhook route uses (`handlers::entries::test_entry_webhook`), given a name
/// so every production delivery site can call one function instead of re-deriving it.
pub async fn gate_outbound_webhook(
    pool: &PgPool,
    account_id: &uuid::Uuid,
    webhook_url: &str,
) -> Result<(), AppError> {
    let target: Option<(uuid::Uuid, Vec<String>, i32)> = sqlx::query_as(
        "SELECT id, COALESCE(allowed_domains, '{}'), daily_limit FROM integration_targets \
         WHERE account_id = $1 AND webhook_url = $2 AND is_active = true LIMIT 1",
    )
    .bind(account_id)
    .bind(webhook_url)
    .fetch_optional(pool)
    .await
    .map_err(|e| AppError::Internal(format!("Security check error: {}", e)))?;

    match target {
        Some((target_id, allowed_domains, daily_limit)) => {
            check_webhook_security(pool, &target_id, webhook_url, &allowed_domains, daily_limit)
                .await
        }
        None => validate_webhook_url(webhook_url, &[]).await.map_err(|msg| {
            AppError::Forbidden(format!("Webhook blocked by security policy: {}", msg))
        }),
    }
}

/// `gate_outbound_webhook` for a BEST-EFFORT delivery path, where the entry must not fail because
/// its destination was refused. Returns `true` when the destination may be contacted and `false`
/// when the gate refused it — the caller must then NOT send. The refusal is logged with its
/// reason, so a blocked delivery is never silent.
pub async fn outbound_webhook_allowed(
    pool: &PgPool,
    account_id: &uuid::Uuid,
    webhook_url: &str,
) -> bool {
    match gate_outbound_webhook(pool, account_id, webhook_url).await {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(
                "Outbound webhook to '{}' refused by the security gate: {}",
                webhook_url,
                e
            );
            false
        }
    }
}

/// A shared client for outbound delivery (webhooks AND provider endpoints) that follows NO
/// redirects.
///
/// `reqwest::Client::new()` follows up to 10 redirects, so a destination that passed the gate
/// could answer `302 Location: http://169.254.169.254/…` and be a free hop past it.
/// `AppState::http_client` carries the same policy — prefer it wherever a state handle is in
/// scope; this exists for the delivery modules, which are handed only a pool.
///
/// Returns `None` when the client cannot be built at all (TLS backend failure). Callers must then
/// SKIP the delivery rather than fall back to a redirect-following default: a gate that degrades to
/// an ungated send is not a gate.
pub fn delivery_client() -> Option<reqwest::Client> {
    static DELIVERY_CLIENT: std::sync::OnceLock<Option<reqwest::Client>> =
        std::sync::OnceLock::new();
    DELIVERY_CLIENT
        .get_or_init(|| {
            match reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .redirect(reqwest::redirect::Policy::none())
                .user_agent("IncentiveSwift/0.1.0")
                .build()
            {
                Ok(c) => Some(c),
                Err(e) => {
                    tracing::error!("outbound-webhook client could not be built ({e}); webhook delivery stays disabled");
                    None
                }
            }
        })
        .clone()
}

// ---------------------------------------------------------------------------------------------
// Provider-ENDPOINT gate (a BYOK `base_url` / `api_url`, not a webhook) — kanban t_f3c75b2a
// ---------------------------------------------------------------------------------------------

/// The platform's OWN base URL for a provider (`integration_provider_presets`), when one is set.
/// This is the carve-out key for the gate below: the one destination a tenant may still reach
/// even when it is loopback/private, because it is the platform's own first-party bridge.
pub async fn provider_preset_base_url(pool: &PgPool, provider: &str) -> Option<String> {
    sqlx::query_scalar::<_, String>(
        "SELECT base_url FROM integration_provider_presets \
         WHERE key = $1 AND base_url IS NOT NULL AND base_url <> ''",
    )
    .bind(provider)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
}

/// Is `url` the platform's own preset endpoint for the provider? Same scheme, same host
/// (case-insensitively), same port, and a path at or below the preset's own path. Pure, so the
/// carve-out rule is testable without a DB or a network.
pub fn is_platform_preset_url(url: &str, preset: &str) -> bool {
    let (Ok(u), Ok(p)) = (Url::parse(url.trim()), Url::parse(preset.trim())) else {
        return false;
    };
    if u.scheme() != p.scheme() {
        return false;
    }
    let host_eq = match (u.host_str(), p.host_str()) {
        (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
        _ => false,
    };
    if !host_eq || u.port_or_known_default() != p.port_or_known_default() {
        return false;
    }
    let up = u.path().trim_end_matches('/');
    let pp = p.path().trim_end_matches('/');
    up == pp || up.starts_with(&format!("{}/", pp))
}

/// Host-only flavour of the carve-out, for a transport whose destination is a bare host:port
/// (SMTP): a `smtp://127.0.0.1:25` preset admits the host `127.0.0.1`. Pure.
pub fn host_is_platform_preset(host: &str, preset: &str) -> bool {
    let Ok(p) = Url::parse(preset.trim()) else {
        return false;
    };
    match p.host_str() {
        Some(h) => h.eq_ignore_ascii_case(host.trim()),
        None => false,
    }
}

/// The gate for a TENANT- or ADMIN-settable provider ENDPOINT: `provider_keys.base_url`
/// (written by `POST /api/v1/provider-keys`), the LLM `base_url` in `chat_handler`, and
/// `email_provider`'s `api_url` (writable by a tenant through `PUT /api/v1/settings`).
///
/// This is NOT the webhook gate: the destination is a BYOK endpoint, and the platform's own
/// endpoint for a provider may legitimately be loopback — `integration_provider_presets` carries
/// `coreswift -> http://127.0.0.1:8084` (the first-party CoreSwift bridge) and every live
/// `provider_keys` row for it carries exactly that value. A blanket private-IP refusal would turn
/// the live lead pipeline off for every account, so the arms are:
///
///   1. the platform preset for this provider                  -> ALLOW (the carve-out),
///   2. anything else that resolves to a private/reserved IP   -> REFUSE,
///   3. an empty value                                         -> ALLOW (nothing to contact; the
///      caller's own literal default applies).
pub async fn gate_provider_endpoint(
    pool: &PgPool,
    provider: &str,
    base_url: &str,
) -> Result<(), String> {
    let url = base_url.trim();
    if url.is_empty() {
        return Ok(());
    }
    if let Some(preset) = provider_preset_base_url(pool, provider).await {
        if is_platform_preset_url(url, &preset) {
            return Ok(());
        }
    }
    validate_webhook_url(url, &[]).await.map_err(|msg| {
        format!(
            "Provider endpoint '{}' for provider '{}' refused: {}",
            url, provider, msg
        )
    })
}

/// `gate_provider_endpoint` for a transport whose destination is a bare host (SMTP): the same
/// carve-out (the provider's preset host) plus the same private/reserved refusal, resolved the
/// same way.
pub async fn gate_provider_endpoint_host(
    pool: &PgPool,
    provider: &str,
    host: &str,
) -> Result<(), String> {
    let h = host.trim();
    if h.is_empty() {
        return Ok(());
    }
    if let Some(preset) = provider_preset_base_url(pool, provider).await {
        if host_is_platform_preset(h, &preset) {
            return Ok(());
        }
    }
    let addrs = tokio::net::lookup_host((h, 0))
        .await
        .map_err(|e| format!("DNS resolution failed for '{}': {}", h, e))?;
    for addr in addrs {
        if is_private_ip(&addr.ip()) {
            return Err(format!(
                concat!(
                    "Provider host '{}' for provider '{}' resolves to a private/reserved IP ",
                    "address ({}). Outbound connections to internal infrastructure are blocked ",
                    "for security."
                ),
                h,
                provider,
                addr.ip()
            ));
        }
    }
    Ok(())
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
    validate_webhook_url(webhook_url, allowed_domains)
        .await
        .map_err(|msg| {
            AppError::Forbidden(format!("Webhook blocked by security policy: {}", msg))
        })?;

    // 2. Daily limit check
    let within_limit = check_daily_limit(pool, target_id, daily_limit)
        .await
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

    // ---- the allowlist rule, exercised as the pure predicate (no DNS, no network) -----------
    #[test]
    fn empty_allowlist_allows_every_external_host() {
        assert!(host_matches_allowlist("example.com", &[]));
        assert!(host_matches_allowlist("evil.net", &[]));
        assert!(host_matches_allowlist("8.8.8.8", &[]));
    }

    #[test]
    fn allowlist_exact_match() {
        let domains = vec!["example.com".to_string(), "api.good.com".to_string()];
        assert!(host_matches_allowlist("example.com", &domains));
        assert!(host_matches_allowlist("api.good.com", &domains));
    }

    #[test]
    fn allowlist_subdomain_match() {
        let domains = vec!["example.com".to_string()];
        assert!(host_matches_allowlist("hooks.example.com", &domains));
        assert!(host_matches_allowlist("sub.hooks.example.com", &domains));
    }

    #[test]
    fn allowlist_rejects_lookalikes() {
        let domains = vec!["example.com".to_string()];
        assert!(!host_matches_allowlist("evil.com", &domains));
        assert!(!host_matches_allowlist("example.evil.com", &domains));
        assert!(!host_matches_allowlist("notexample.com", &domains));
        // an empty allowlist ENTRY must not become a match-everything wildcard
        assert!(!host_matches_allowlist("evil.com", &["".to_string()]));
    }

    #[test]
    fn allowlist_is_case_insensitive() {
        let domains = vec!["EXAMPLE.COM".to_string()];
        assert!(host_matches_allowlist("Example.COM", &domains));
        assert!(host_matches_allowlist("hooks.Example.com", &domains));
    }

    // ---- the URL gate: literal IPs keep these legs network-free ------------------------------
    #[tokio::test]
    async fn refuses_private_and_link_local_addresses() {
        for url in [
            "http://127.0.0.1:8083/api/v1/health",
            "http://127.0.0.2/x",
            "http://169.254.169.254/latest/meta-data/",
            "http://10.0.0.5/x",
            "http://172.16.0.9/x",
            "http://192.168.1.10/x",
        ] {
            let err = validate_webhook_url(url, &[])
                .await
                .unwrap_err_or_panic(url);
            assert!(err.contains("private/reserved"), "{url} -> {err}");
        }
    }

    #[tokio::test]
    async fn allows_a_public_address_and_applies_the_allowlist_to_it() {
        // A literal IP is a public destination when it is not in a reserved range…
        assert!(validate_webhook_url("https://8.8.8.8/hook", &[])
            .await
            .is_ok());
        // …and the allowlist still has to name it.
        assert!(
            validate_webhook_url("https://8.8.8.8/hook", &["8.8.8.8".to_string()])
                .await
                .is_ok()
        );
        let err = validate_webhook_url("https://8.8.8.8/hook", &["example.com".to_string()])
            .await
            .unwrap_err_or_panic("8.8.8.8 vs example.com");
        assert!(err.contains("allowed domains"), "{err}");
    }

    #[tokio::test]
    async fn fails_closed_on_an_unresolvable_host() {
        // A destination we cannot resolve is not a delivery: the gate refuses instead of trying.
        let err = validate_webhook_url("https://does-not-resolve.invalid/hook", &[])
            .await
            .unwrap_err_or_panic("unresolvable host");
        assert!(err.contains("DNS resolution failed"), "{err}");
    }

    #[tokio::test]
    async fn refuses_a_malformed_url() {
        assert!(validate_webhook_url("not-a-url", &[]).await.is_err());
        assert!(validate_webhook_url("", &[]).await.is_err());
    }

    /// `Result::expect_err` needs `T: Debug`; this keeps the failure message the URL.
    trait UnwrapErrOrPanic<T, E> {
        fn unwrap_err_or_panic(self, what: &str) -> E;
    }

    impl<T: std::fmt::Debug, E> UnwrapErrOrPanic<T, E> for Result<T, E> {
        fn unwrap_err_or_panic(self, what: &str) -> E {
            match self {
                Err(e) => e,
                Ok(v) => panic!("expected Err for {what}, got Ok({v:?})"),
            }
        }
    }

    // ---- the provider-endpoint carve-out, on the LIVE preset value (kanban t_f3c75b2a) --------
    #[test]
    fn platform_preset_admits_exactly_the_preset_origin() {
        // `integration_provider_presets` -> coreswift = this value, and 5/5 live
        // `provider_keys` rows for coreswift carry exactly it. It must stay admitted.
        let preset = "http://127.0.0.1:8084";
        assert!(is_platform_preset_url("http://127.0.0.1:8084", preset));
        assert!(is_platform_preset_url("http://127.0.0.1:8084/", preset));
        assert!(is_platform_preset_url(
            "http://127.0.0.1:8084/api/external/contacts",
            preset
        ));
        // the first-party bridge is loopback, so the carve-out has to survive case-insensitivity
        assert!(is_platform_preset_url(
            "HTTP://127.0.0.1:8084/api/external/lists",
            preset
        ));

        // …and NOTHING else that is internal is the preset: another loopback port, another
        // loopback host, a lookalike name, another scheme.
        assert!(!is_platform_preset_url("http://127.0.0.1:18084/x", preset));
        assert!(!is_platform_preset_url("http://127.0.0.2:8084/x", preset));
        assert!(!is_platform_preset_url(
            "http://127.0.0.1.evil.com:8084/x",
            preset
        ));
        assert!(!is_platform_preset_url("https://127.0.0.1:8084/x", preset));
        // 80 is the http default: an explicit :8084 is a different port
        assert!(!is_platform_preset_url("http://127.0.0.1:80/x", preset));

        // a preset with its own path admits what hangs below it, not what sits above it
        assert!(is_platform_preset_url(
            "http://127.0.0.1:8084/v1/x",
            "http://127.0.0.1:8084/v1"
        ));
        assert!(!is_platform_preset_url(
            "http://127.0.0.1:8084/v2/x",
            "http://127.0.0.1:8084/v1"
        ));
        // malformed / unroutable input is never "the preset"
        assert!(!is_platform_preset_url("not-a-url", preset));
        assert!(!is_platform_preset_url("http://127.0.0.1:8084", ""));
    }

    #[test]
    fn preset_host_matches_a_bare_transport_host() {
        let preset = "smtp://127.0.0.1:25";
        assert!(host_is_platform_preset("127.0.0.1", preset));
        assert!(host_is_platform_preset(" 127.0.0.1 ", preset));
        assert!(!host_is_platform_preset("127.0.0.2", preset));
        assert!(!host_is_platform_preset("127.0.0.1", ""));
        assert!(!host_is_platform_preset("127.0.0.1", "not-a-url"));
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
        assert!(!is_private_ip(
            &"2001:4860:4860::8888".parse::<IpAddr>().unwrap()
        ));
    }
}
