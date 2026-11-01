//! Host-name normalisation for customer-supplied domains in `POST /api/v1/admin/domains`.
//!
//! MEASURED DEFECT, 2026-11-01 (cron `default`). The route bound `body.domain` straight into
//! `INSERT INTO custom_domains (…, domain, …)`, so three live POSTs as a real signed-in customer
//! were all answered **200** and written to the table:
//!
//!     https://Evil.Example.COM/some/path?x=1     -> stored verbatim
//!     asdf not a site                            -> stored verbatim
//!     ""                                         -> stored as an empty host
//!
//! None of the three can ever be verified: the row is rendered in the customer's console, and the
//! response's own verification instructions read
//! `Add a TXT record for _swift-verify.https://Evil.Example.COM/some/path?x=1 with value '…'` —
//! a sentence the customer cannot act on. The empty value is worse than useless: `domain` carries
//! the `custom_domains_domain_key` UNIQUE index, so the first `""` succeeds and the second answers
//! a bare 500 instead of telling the caller the host is taken.
//!
//! ADASwift fixed this class for its `clients.domain` column on 2026-10-30 (`normalise_website_domain`);
//! this is the sibling door, and nothing on this side ever validated.
//!
//! The rule: salvage a pasted URL down to the host it names (`https://Shop.Example.COM/pricing` ->
//! `shop.example.com`), keep only ASCII alphanumerics, `-` and `.`, require at least two non-empty
//! labels ending in an alphabetic TLD, and refuse anything else with a sentence a customer can act
//! on. Refusal happens BEFORE the insert, so a rejected value writes no row.

/// Longest host name the DNS allows (`253` characters in presentation form).
const MAX_HOST_CHARS: usize = 253;
/// Longest single label the DNS allows.
const MAX_LABEL_CHARS: usize = 63;

/// Normalise a customer-typed domain to the bare host it names, or explain why it is not one.
pub fn normalise_host(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("Enter a domain name, for example shop.example.com".to_string());
    }
    let lower = trimmed.to_ascii_lowercase();

    // Drop an explicit scheme. A bare `://` with no scheme, or one with characters a scheme
    // cannot carry, is not a URL we can trust — refuse rather than guess.
    let after_scheme = match lower.split_once("://") {
        Some((scheme, rest)) => {
            let scheme_ok = !scheme.is_empty()
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
            if !scheme_ok {
                return Err(not_a_domain(trimmed));
            }
            rest
        }
        None => lower.as_str(),
    };

    // The authority ends at the first path, query or fragment character.
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .trim();

    // `user:pass@host` — only the host is ours. `[::1]`-style literals are refused below.
    let authority = authority.rsplit('@').next().unwrap_or("");

    // A numeric port is not part of the host.
    let host = match authority.rsplit_once(':') {
        Some((h, port)) if port.bytes().all(|c| c.is_ascii_digit()) => h,
        _ => authority,
    };

    let host = host.strip_suffix('.').unwrap_or(host);
    let host = host.strip_prefix("www.").unwrap_or(host);

    if host.is_empty() || host.len() > MAX_HOST_CHARS {
        return Err(not_a_domain(trimmed));
    }

    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() < 2 {
        return Err(not_a_domain(trimmed));
    }
    for label in &labels {
        if label.is_empty() || label.len() > MAX_LABEL_CHARS {
            return Err(not_a_domain(trimmed));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(not_a_domain(trimmed));
        }
        if !label
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-')
        {
            return Err(not_a_domain(trimmed));
        }
    }
    // A numeric-only last label is an IP address or a mistake, never a domain a customer can verify.
    let tld = labels[labels.len() - 1];
    if tld.len() < 2 || !tld.bytes().all(|c| c.is_ascii_alphabetic()) {
        return Err(not_a_domain(trimmed));
    }

    Ok(host.to_string())
}

fn not_a_domain(raw: &str) -> String {
    format!(
        "`{raw}` is not a domain name. Enter just the host you want to serve, for example shop.example.com"
    )
}

#[cfg(test)]
mod tests {
    use super::normalise_host;

    #[test]
    fn a_pasted_url_is_reduced_to_its_host() {
        assert_eq!(
            normalise_host("https://Shop.Example.COM/pricing?ref=mail#top").unwrap(),
            "shop.example.com"
        );
        assert_eq!(
            normalise_host("http://www.example.com").unwrap(),
            "example.com"
        );
        assert_eq!(normalise_host("example.com:8443").unwrap(), "example.com");
        // A bare host followed by a path is the same paste with the scheme lost in transit: the
        // authority still ends at the first '/', so it is salvaged like the full URL.
        assert_eq!(normalise_host("example.com/path").unwrap(), "example.com");
        assert_eq!(
            normalise_host("https://user:pw@example.com/").unwrap(),
            "example.com"
        );
        assert_eq!(normalise_host("  Example.COM.  ").unwrap(), "example.com");
    }

    #[test]
    fn what_is_not_a_host_is_refused_with_a_sentence() {
        for bad in [
            "",
            "   ",
            "asdf not a site",
            "localhost",
            "example",
            "https://",
            "example..com",
            "-example.com",
            "example-.com",
            "example.c0m",
            "192.168.1.1",
            "exa mple.com",
        ] {
            let err = normalise_host(bad)
                .err()
                .unwrap_or_else(|| panic!("{bad:?} must not be accepted"));
            assert!(
                err.contains("shop.example.com"),
                "{bad:?} refusal must tell the caller what a good value looks like: {err}"
            );
        }
    }

    #[test]
    fn a_deep_host_with_a_four_label_tld_is_kept() {
        assert_eq!(
            normalise_host("app.shop.example.co.uk").unwrap(),
            "app.shop.example.co.uk"
        );
    }
}
