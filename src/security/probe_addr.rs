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

// ─────────────────────────────────────────────────────────────────────────────
// SYNTHETIC REQUESTERS — a harness must never page a human (kanban t_f17b5802)
//
// Measured 2026-11-10: one day of audit walks produced 49 operator alert emails
// ("A support request needs an answer: …", "A customer asked to buy …") because the
// plan-request and support-request doors notify the OPERATOR by design — the recipient is
// the operator whatever address the requester gave. The probe rows were all minted on
// reserved addresses (`@example.invalid`, `p3-smoke@x.com`), and the app dutifully paged a
// person for every one of them.
//
// The row is the record: it still lands in the console queue, the request still works, and
// the caller still answers 200/201. Only the ALERT to a human is withheld — the same
// treatment the signup and password-reset paths already give mail that can only bounce.
//
// The predicate is deliberately on the REQUESTER, never on the route: the same door serves
// real customers, and a real customer's request must still reach a human.
// ─────────────────────────────────────────────────────────────────────────────

/// Domains that can only belong to a harness. `x.com` / `test.com` are added on top of the
/// RFC-reserved class because the fleet's own proof scripts address them and they are not
/// mail-suppressed (they are real, registrable domains), so nothing else would stop the alert.
pub const SYNTHETIC_REQUESTER_DOMAINS: &[&str] = &[
    "example.invalid",
    "example.test",
    "example.com",
    "x.com",
    "test.com",
];

/// Local parts a harness mints. `bounce+` is the shape a deliverability probe uses.
pub const SYNTHETIC_LOCAL_PREFIXES: &[&str] = &["probe-", "canary-", "bounce+", "verify"];

/// Workspace/account name fragments that name fleet machinery in words.
pub const SYNTHETIC_NAME_FRAGMENTS: &[&str] =
    &["probe", "canary", "synthetic", "smoke test", "test farm"];

/// Why `email`/`name` is a synthetic (harness) identity, or `None` for a real customer.
///
/// Returns the offending domain, local-part prefix or name fragment, so the caller can log a
/// reason instead of a bare boolean. Matches on the label boundary (`x@notexample.com` is a
/// customer), normalises case/whitespace/trailing root dot, and never treats an empty string
/// as a match.
pub fn synthetic_requester(email: &str, name: &str) -> Option<&'static str> {
    let addr = email.trim();
    // The fleet's own harness domains (swiftsoftware.dev/.net/.local) are already classified.
    if let Some(domain) = harness_domain(addr) {
        return Some(domain);
    }
    if let Some((local, domain)) = addr.rsplit_once('@') {
        let domain = domain.trim().trim_end_matches('.').to_ascii_lowercase();
        if !domain.is_empty() {
            if let Some(d) = SYNTHETIC_REQUESTER_DOMAINS.iter().copied().find(|d| {
                domain == *d || (domain.len() > d.len() + 1 && domain.ends_with(&format!(".{d}")))
            }) {
                return Some(d);
            }
        }
        let local = local.trim().to_ascii_lowercase();
        if let Some(p) = SYNTHETIC_LOCAL_PREFIXES
            .iter()
            .copied()
            .find(|p| local.starts_with(p))
        {
            return Some(p);
        }
    }
    let name = name.trim().to_ascii_lowercase();
    if !name.is_empty() {
        if let Some(f) = SYNTHETIC_NAME_FRAGMENTS
            .iter()
            .copied()
            .find(|f| name.contains(f))
        {
            return Some(f);
        }
    }
    None
}

#[cfg(test)]
mod synthetic_requester_tests {
    use super::*;

    #[test]
    fn harness_identities_are_synthetic() {
        // The exact shapes that produced the 49 alerts.
        assert_eq!(
            synthetic_requester("probe-ctrl-1111@example.invalid", ""),
            Some("example.invalid")
        );
        assert_eq!(synthetic_requester("p3-smoke@x.com", ""), Some("x.com"));
        assert_eq!(synthetic_requester("walk@test.com", ""), Some("test.com"));
        // `test` is the wider harness label, so it wins the match; either way it is refused.
        assert!(synthetic_requester("someone@mail.example.test", "").is_some());
        assert_eq!(
            synthetic_requester("probe-abc@real-customer.com", ""),
            Some("probe-")
        );
        assert_eq!(synthetic_requester("canary-7@acme.io", ""), Some("canary-"));
        assert_eq!(synthetic_requester("bounce+x@acme.io", ""), Some("bounce+"));
        assert_eq!(synthetic_requester("verify1@acme.io", ""), Some("verify"));
        // The name alone is enough: a harness workspace on a plausible address.
        assert_eq!(synthetic_requester("a@b.com", "Probe Walk"), Some("probe"));
        assert_eq!(
            synthetic_requester("a@b.com", "Smoke Test 4"),
            Some("smoke test")
        );
        assert_eq!(
            synthetic_requester("a@b.com", "Synthetic Co"),
            Some("synthetic")
        );
        assert_eq!(
            synthetic_requester("a@b.com", "Test Farm 2"),
            Some("test farm")
        );
    }

    #[test]
    fn real_customers_are_not_synthetic() {
        assert_eq!(
            synthetic_requester("owner@albert-tire.com", "Albert Tire"),
            None
        );
        assert_eq!(synthetic_requester("certifiedtb143@yahoo.com", ""), None);
        assert_eq!(
            synthetic_requester("dev@notexample.com", "Not Example Ltd"),
            None
        );
        assert_eq!(synthetic_requester("x@notx.com", ""), None);
        assert_eq!(synthetic_requester("", ""), None);
        assert_eq!(synthetic_requester("no-at-sign", "Real Co"), None);
    }
}
