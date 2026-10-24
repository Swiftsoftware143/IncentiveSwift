//! `probe_addr` — recipient addresses a fleet harness owns, which must never be handed to a real
//! mail relay (parity with FunnelSwift, kanban t_36b55ed2).
//!
//! WHY THIS EXISTS
//!
//! The fleet's probe suites mint throwaway accounts through the real signup route, and the signup
//! route mails the generated first password. Some of those probes still address the fleet's own
//! dev domains — `swiftsoftware.dev`, `swiftsoftware.net` — which are REAL, registrable domains.
//! Measured on the fleet's Mailgun domains (events API, 2026-10-09): a message to
//! `*@swiftsoftware.dev` is `accepted` and then `bounced` (552), while a message to a customer
//! address is `delivered`. So the send reaches no one, burns a delivery on the domain's sending
//! reputation, and — when the address happens to be routable — lands in the fleet's own mailbox.
//!
//! The policy (`/opt/swift/docs/fleet-probe-residue-policy-2026-09-28.md`) classes
//! `swiftsoftware.dev/.local` as FLEET-DEV — the harness class. This module is the OUTBOUND half:
//! a signup addressed into the harness class is created normally but its mail is never sent, so a
//! probe can never reach an inbox.
//!
//! WHAT IS *NOT* HERE, AND WHY
//!
//! The RFC 2606 / 6761 class (`example.com/.net/.org`, `*.invalid`, `*.test`, `*.local`,
//! `localhost`) is deliberately NOT suppressed. Those names cannot resolve to a mailbox, so a send
//! to them can only bounce — and that is exactly the address class the content-level harnesses use
//! on purpose, pointing the provider at a local SMTP sink and reading the credential message off
//! the wire. Suppressing that class would delete the only harness that can prove a send still works.

/// The fleet's own harness domains. Every one of them is fleet-controlled: a message addressed here
/// can only land in a fleet mailbox, never in a customer's.
pub const FLEET_HARNESS_DOMAINS: &[&str] = &[
    "swiftsoftware.dev",
    "swiftsoftware.net",
    "swiftsoftware.local",
    "example.invalid",
    "example.com",
    "example.net",
    "example.org",
    "invalid",
    "test",
];

/// Domains that make a SIGNUP fleet machinery WITHOUT being mail-suppressed.
///
/// `maxxspace.com` is the one domain served by the fleet's own disposable inbox (mail.tm): no
/// customer can own an address there, so a workspace minted on it is a probe by construction. Its
/// mail must still be DELIVERED, though — the credential-delivery proof reads the generated password
/// out of that inbox, so adding it to [`FLEET_HARNESS_DOMAINS`] would delete the only end-to-end
/// proof that a new customer's password reaches a mailbox. Hence a separate list, consulted by
/// [`attribution_domain`] and never by the mail layer.
///
/// Measured 2026-10-24 on ADASwift: `probe-swsink165312@maxxspace.com` minted the workspace "Sink
/// Probe swsink165312" with `probe_harness` NULL, so it sat in the operator's Tenants list until the
/// residue sweep deleted it. Ported from ADASwift commit 96a16b2.
pub const ATTRIBUTION_ONLY_DOMAINS: &[&str] = &["maxxspace.com"];

/// The domain that makes a SIGNUP a probe: the fleet-harness class of [`harness_domain`] plus the
/// attribution-only class above. The mail layer must keep using [`harness_domain`], never this.
pub fn attribution_domain(addr: &str) -> Option<&'static str> {
    if let Some(domain) = harness_domain(addr) {
        return Some(domain);
    }
    let (_, domain) = addr.rsplit_once('@')?;
    let domain = domain.trim().trim_end_matches('.').to_ascii_lowercase();
    if domain.is_empty() {
        return None;
    }
    ATTRIBUTION_ONLY_DOMAINS.iter().copied().find(|d| {
        domain == *d || (domain.len() > d.len() + 1 && domain.ends_with(&format!(".{d}")))
    })
}

/// The fleet harness domain `addr` belongs to, if any. A subdomain counts
/// (`probe@mail.swiftsoftware.dev` is still the harness class); a domain that merely CONTAINS the
/// name does not (`x@notswiftsoftware.dev` is refused — the match is on the label boundary).
///
/// Everything is normalised first (trim, lowercase, trailing dot), so `Probe@SwiftSoftware.NET.`
/// classifies like the exact form. A value with no `@`, or with an empty domain, is not an address
/// and returns `None`.
pub fn harness_domain(addr: &str) -> Option<&'static str> {
    let (_, domain) = addr.rsplit_once('@')?;
    let domain = domain.trim().trim_end_matches('.').to_ascii_lowercase();
    if domain.is_empty() {
        return None;
    }
    FLEET_HARNESS_DOMAINS.iter().copied().find(|d| {
        domain == *d || (domain.len() > d.len() + 1 && domain.ends_with(&format!(".{d}")))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fleet_harness_domains_are_detected() {
        assert_eq!(
            harness_domain("isprof386014@swiftsoftware.dev"),
            Some("swiftsoftware.dev")
        );
        assert_eq!(
            harness_domain("probe@swiftsoftware.net"),
            Some("swiftsoftware.net")
        );
        assert_eq!(
            harness_domain("x@swiftsoftware.local"),
            Some("swiftsoftware.local")
        );
        // Case, whitespace and a trailing root dot are normalised away.
        assert_eq!(
            harness_domain(" Probe@SwiftSoftware.NET. "),
            Some("swiftsoftware.net")
        );
        // A subdomain is still the harness class.
        assert_eq!(
            harness_domain("probe@mail.swiftsoftware.dev"),
            Some("swiftsoftware.dev")
        );
    }

    #[test]
    fn real_customer_addresses_are_not_suppressed() {
        assert_eq!(harness_domain("certifiedtb143@yahoo.com"), None);
        assert_eq!(harness_domain("someone@gmail.com"), None);
        assert_eq!(harness_domain("david@swiftsoftware.com"), None);
        assert_eq!(harness_domain("support@incentiveswift.com"), None);
        // A domain that merely CONTAINS a harness domain is not a match.
        assert_eq!(harness_domain("x@notswiftsoftware.dev"), None);
        assert_eq!(harness_domain("x@swiftsoftware.dev.evil.com"), None);
        // Reserved non-routable names are not this rule's business (see the module doc).
        assert_eq!(harness_domain("cr1sink0a1b@probe.local"), None);
        assert_eq!(harness_domain("x@example.com"), Some("example.com"));
        // Reserved names are suppressed too (2026-10-09): they can never resolve, so the
        // only possible outcomes are a bounce and lost sending reputation.
    }

    #[test]
    fn malformed_values_never_classify() {
        assert_eq!(harness_domain("no-at-sign"), None);
        assert_eq!(harness_domain(""), None);
        assert_eq!(harness_domain("trailing@"), None);
    }
}

/// The funnel's disposable inbox is a SIGNUP tell, never a mail-suppression tell.
#[cfg(test)]
mod attribution_only_tests {
    use super::*;

    #[test]
    fn the_disposable_inbox_attributes_a_signup_but_does_not_suppress_its_mail() {
        assert_eq!(
            attribution_domain("probe-swsink165312@maxxspace.com"),
            Some("maxxspace.com")
        );
        assert_eq!(
            attribution_domain(" Probe@MAXXSPACE.com. "),
            Some("maxxspace.com")
        );
        assert_eq!(
            attribution_domain("x@mail.maxxspace.com"),
            Some("maxxspace.com")
        );
        // The mail layer's own predicate must NOT learn this domain.
        assert_eq!(harness_domain("probe-swsink165312@maxxspace.com"), None);
        // A real customer is untouched by either arm.
        assert_eq!(attribution_domain("certifiedtb143@yahoo.com"), None);
        assert_eq!(attribution_domain("x@notmaxxspace.com"), None);
    }
}
