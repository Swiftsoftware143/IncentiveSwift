//! Email-address syntax validation, in ONE place, at every boundary that writes `accounts.email`.
//!
//! `public.accounts.email` is an account's login identity AND the only address its credentials can
//! ever be mailed to. Before this module the app had no format check anywhere in the request path
//! and the column had no `CHECK`, so `POST /api/v1/auth/register` accepted the literal string
//! `bad` and minted a real account whose login was not an address at all — permanently unreachable
//! (no welcome/credentials mail can ever be delivered to it). That is the defect this module closes
//! (kanban t_d7ef4a88; the class was measured live under t_4722a331, the reference fix proven on
//! missedcallrespondr under t_54b1ffab where the retired row was users.email='bad').
//!
//! Five writers need the same answer, and they must not drift apart:
//!   * the public signup path (`handlers::auth_handler::register`),
//!   * (the referral/credit path — `handlers::external_grants::{grant_credits,register_member}` —
//!     was RETIRED with the external loyalty surface, kanban t_f76c9950; no such writer remains),
//!   * the internal business-create path (`handlers::business_handler::register_business`),
//!   * the paid-checkout path (`billing::webhooks::deliver_credentials`),
//!   * plus the read-only `login` / `forgot-password` lookups, which must refuse the same input the
//!     same way (they match, they never mint).
//!
//! Deliberately **syntax only**: trimming and lowercasing are the normalisations this fleet already
//! ships (FunnelSwift/CoreSwift-CRM register, `harness_marker`), and nothing here tightens what an
//! address may *mean*. Plus-aliases (`a+b@x.com`), dotted locals (`a.b@x.com`) and IDN domains
//! (`user@münchen.de`) stay valid. The mirror `CHECK` on the column is deliberately LOOSER than
//! this function (`migrations/zz_is8_accounts_email_format_check.sql`) so the database can never
//! refuse a value the application accepted.
//!
//! No `regex` crate in the dependency graph, and none is added: the checks are simple scans.

/// RFC 5321 forward-path limit — every real mailbox fits.
const MAX_ADDRESS_LEN: usize = 254;

/// Normalise an address for STORAGE and reject anything that is not syntactically an address.
///
/// Returns the trimmed, lowercased value the caller must persist and use in messages, or a
/// caller-safe reason (the handlers map it to a 4xx). Call this BEFORE any INSERT/UPDATE — never
/// after, and never let an unvalidated value reach the statement.
pub fn normalize(raw: &str) -> Result<String, String> {
    let value = raw.trim().to_lowercase();

    if value.is_empty() {
        return Err("email: is required".into());
    }
    if value.len() > MAX_ADDRESS_LEN {
        return Err(format!(
            "email: is longer than {MAX_ADDRESS_LEN} characters"
        ));
    }
    if value.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("email: must not contain whitespace or control characters".into());
    }

    let Some((local, domain)) = value.split_once('@') else {
        return Err("email: must look like name@example.com".into());
    };
    if domain.contains('@') {
        return Err("email: must contain exactly one @".into());
    }
    if local.is_empty() {
        return Err("email: is missing the part before @".into());
    }
    if local.starts_with('.') || local.ends_with('.') || local.contains("..") {
        return Err("email: has an empty dot-separated part before @".into());
    }
    if domain.is_empty() {
        return Err("email: is missing the domain after @".into());
    }
    if !domain.contains('.') {
        return Err("email: domain must contain a dot (e.g. example.com)".into());
    }
    if domain.starts_with('.') || domain.ends_with('.') || domain.split('.').any(|l| l.is_empty()) {
        return Err("email: domain has an empty dot-separated part".into());
    }

    Ok(value)
}

/// The lookup key for an address a caller only needs to MATCH against a stored row
/// (`login`, `forgot-password`). Same trim+lowercase as [`normalize`], with no failure arm: a
/// malformed value simply matches nothing, so credential endpoints keep answering their own
/// "invalid email or password" / "if the email exists…" response instead of becoming an
/// account-existence oracle. Pair it with `WHERE lower(email) = $1` so rows stored before the
/// normalisation existed still resolve.
pub fn lookup_key(raw: &str) -> String {
    raw.trim().to_lowercase()
}

/// Domains RFC 2606 reserves for documentation. They are never delegated and never deliver mail;
/// matched exactly or as a `.`-suffix, so `probe.example.com` is refused too.
const RESERVED_DOC_DOMAINS: [&str; 3] = ["example.com", "example.net", "example.org"];

/// Special-use TLDs that have no mail exchanger on the public internet (RFC 2606 §2 + RFC 6761:
/// `.invalid`, `.test`, `.example`, `.localhost`, and `.local` for mDNS names).
const RESERVED_TLDS: [&str; 5] = ["invalid", "test", "example", "local", "localhost"];

/// The marker every refusal from [`refuse_undeliverable_recipient`] carries.
///
/// The operator surfaces that show a send failure — the ticker's WARN/ERROR line,
/// `pending_emails.last_error` (rendered by the console's "Email queue" panel) and the
/// Settings → Email test-send answer — therefore all read `recipient-refused: …`, which cannot be
/// confused with a TRANSPORT failure (`did not answer within 10s`, `Mailgun returned 502`). Those
/// are retryable and mean "try again"; this one means "this can never arrive".
pub const RECIPIENT_REFUSED: &str = "recipient-refused:";

/// THE RECIPIENT-SIDE GUARD at the mail seam (kanban t_f56f4a79) — the mirror of the SSRF guard
/// that `delivery::sender` / `email_provider` already apply to the DESTINATION of an outbound call.
///
/// DECISION (chosen over "send anyway and take the bounce"): an address that PROVABLY cannot
/// receive mail is refused before any transport is touched, naming the reason. Measured before the
/// change: nothing on this path looked at the recipient at all, so a queued lifecycle mail to
/// `probe@example.com` was handed to the platform provider, accepted with a 2xx (Mailgun accepts
/// anything), counted `sent` on its row — and then bounced. That is a wasted provider send, a bounce
/// against the platform's own sending reputation, and a row that claims success for a message no
/// human can ever receive (kanban t_9d711589 retired 18 such rows off the live queue).
///
/// What is refused:
///   * a MALFORMED address — the same syntax rule as [`normalize`], i.e. exactly what the app
///     already refuses to store as an account identity;
///   * a RECIPIENT DOMAIN that provably cannot receive mail: RFC 2606 `example.com`/`.net`/`.org`
///     (and any subdomain) plus the special-use TLDs `.invalid`, `.test`, `.example`, `.local`,
///     `.localhost` (RFC 2606/6761 — reserved precisely so they can never resolve in public DNS).
///
/// What is deliberately NOT refused: anything routable — a typo'd but syntactically valid real
/// domain (`@gmial.com`), a plus-alias, an IDN domain, a subdomain, a domain whose LABEL merely
/// looks like a reserved word (`user@test.swiftsoftware.net`). This guard is about *provably
/// undeliverable*, never about guessing intent.
///
/// Scope: the SEND SEAM only. The app's account-creation paths still accept a reserved address —
/// `normalize` stays syntax-only, so a fixture/harness account can exist and log in — it simply can
/// never be mailed. Callers: `email_provider::deliver` (the platform arm, and the single leaf that
/// spends a provider send) and `delivery::sender::deliver_via` (the tenant's own SMTP arm), which
/// together cover the capture-time immediate send, the queue ticker, the lifecycle stages and the
/// Settings → Email test-send.
///
/// Returns the NORMALISED address (trimmed, lowercased — the same value [`normalize`] would store)
/// so the transport receives the canonical form, or a caller-safe reason. A refusal is an ordinary
/// `Err` at the seam: the caller surfaces it (the queue records it in `last_error` and retries like
/// any other failure, the Settings → Email pane shows the sentence).
pub fn refuse_undeliverable_recipient(raw: &str) -> Result<String, String> {
    let to = normalize(raw).map_err(|reason| format!("{RECIPIENT_REFUSED} {reason}"))?;
    let domain = to.split_once('@').map(|(_, d)| d).unwrap_or("");
    if let Some(reason) = reserved_domain_reason(domain) {
        return Err(format!("{RECIPIENT_REFUSED} {reason}"));
    }
    Ok(to)
}

/// Why `domain` provably cannot receive mail, or `None` when it can (and this guard has no
/// opinion). Names the DOMAIN, never the whole address: the fleet's log convention keeps the
/// recipient's local part out of log lines, and the domain is the thing that decides.
fn reserved_domain_reason(domain: &str) -> Option<String> {
    for d in RESERVED_DOC_DOMAINS {
        if domain == d || domain.ends_with(&format!(".{d}")) {
            return Some(format!(
                "the domain '{domain}' is reserved for documentation by RFC 2606 — it can never \
                 receive mail"
            ));
        }
    }
    if let Some(tld) = domain.rsplit('.').next() {
        if RESERVED_TLDS.contains(&tld) {
            return Some(format!(
                "'.{tld}' is a special-use TLD (RFC 2606/6761), so a '{domain}' recipient can \
                 never receive mail"
            ));
        }
    }
    None
}

/// Does this address sit on a domain that provably cannot receive the credentials mail?
///
/// The account-creation doors call this BEFORE minting (kanban t_a8bd2860): a reserved address
/// must never become a real, unreachable login. The SEND SEAM's
/// [`refuse_undeliverable_recipient`] is the other half — same predicate, different boundary.
pub fn is_reserved_address(email: &str) -> bool {
    match email.rsplit_once('@') {
        Some((_, domain)) => reserved_domain_reason(domain).is_some(),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_the_literal_that_minted_the_dead_account() {
        // The row the class census was looking for: accounts.email = 'bad'.
        assert_eq!(
            normalize("bad"),
            Err("email: must look like name@example.com".to_string())
        );
    }

    #[test]
    fn accepts_real_addresses_and_normalises_them() {
        assert_eq!(
            normalize("  Ada.Lovelace+trial@Example.COM  ").unwrap(),
            "ada.lovelace+trial@example.com"
        );
        assert_eq!(normalize("a@b.co").unwrap(), "a@b.co");
        // IDN domain, unicode local part, long-but-legal address.
        assert_eq!(normalize("User@München.DE").unwrap(), "user@münchen.de");
        assert!(normalize("öhn@example.com").is_ok());
        let long = format!("{}@example.com", "a".repeat(240));
        assert!(normalize(&long).is_ok());
    }

    #[test]
    fn plus_aliases_dots_and_subdomains_stay_valid() {
        for ok in [
            "a+b@x.com",
            "a.b.c@x.com",
            "user@mail.co.uk",
            "user@sub.domain.example.org",
            "user_1-2%3@x-y.com",
        ] {
            assert!(normalize(ok).is_ok(), "{ok} must stay valid");
        }
    }

    #[test]
    fn rejects_shapes_that_are_not_addresses() {
        for bad in [
            "",
            "   ",
            "bad",
            "@x.com",
            "user@",
            "user@nodot",
            "user@@x.com",
            "us er@x.com",
            "user@x .com",
            ".user@x.com",
            "user.@x.com",
            "us..er@x.com",
            "user@.x.com",
            "user@x..com",
            "user@x.com.",
            "user@x@y.com",
        ] {
            assert!(normalize(bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn normalisation_is_idempotent() {
        let once = normalize("  Zed+1@Example.COM ").unwrap();
        assert_eq!(normalize(&once).unwrap(), once);
    }

    /// The guard this app did not have (kanban t_f56f4a79): every address below is provably
    /// undeliverable and used to be handed to the provider anyway.
    #[test]
    fn the_send_seam_refuses_a_recipient_that_can_never_receive_mail() {
        for bad in [
            "customer@example.com",
            "Customer@EXAMPLE.COM",
            "  customer@Example.Org  ",
            "a.b+c@sub.example.com",
            "customer@example.net",
            "customer@host.invalid",
            "customer@probe.test",
            "customer@box.example",
            "customer@box.local",
            "customer@mail.localhost",
        ] {
            let err = refuse_undeliverable_recipient(bad)
                .expect_err("this recipient must be refused at the send seam");
            assert!(
                err.starts_with(RECIPIENT_REFUSED),
                "{bad}: the refusal must be greppable, got {err}"
            );
            assert!(
                err.contains("can never receive mail"),
                "{bad}: the refusal must name WHY, got {err}"
            );
        }
    }

    /// The positive side, which is what keeps the guard honest: everything routable still goes to
    /// the transport, and a reserved WORD in a label or a subdomain is not a reserved domain.
    #[test]
    fn the_send_seam_still_hands_a_routable_address_to_the_transport() {
        assert_eq!(
            refuse_undeliverable_recipient("  Real.Customer+trial@Gmail.COM ").unwrap(),
            "real.customer+trial@gmail.com",
            "the seam must hand on the normalised address"
        );
        for ok in [
            "customer@swiftsoftware.dev",
            "customer@mail.co.uk",
            "a@b.co",
            "user@test.swiftsoftware.net",
            "user@münchen.de",
            "cust.omer@example.commercial.net",
        ] {
            assert!(
                refuse_undeliverable_recipient(ok).is_ok(),
                "{ok} is routable and must still send"
            );
        }
    }

    /// A malformed recipient is refused by the same syntax rule that already guards
    /// `accounts.email` — one answer to "is this an address", not two.
    #[test]
    fn a_malformed_recipient_is_refused_at_the_send_seam() {
        for bad in [
            "",
            "   ",
            "bad",
            "@x.com",
            "user@",
            "user@nodot",
            "user@@x.com",
            "us er@x.com",
            "user@x..com",
            // No dot at all: refused here by the SYNTAX rule (a domain-less host is not an
            // address), which is why it does not need the reserved-domain arm.
            "customer@localhost",
        ] {
            let err = refuse_undeliverable_recipient(bad)
                .expect_err("a malformed recipient must be refused at the send seam");
            assert!(err.starts_with(RECIPIENT_REFUSED), "{bad:?}: got {err}");
        }
    }

    #[test]
    fn lookup_key_matches_what_normalize_stores() {
        assert_eq!(lookup_key("  Mixed@Case.COM "), "mixed@case.com");
        // A malformed value has no failure arm here — it just matches nothing.
        assert_eq!(lookup_key("bad"), "bad");
    }

    #[test]
    fn is_reserved_address_flags_rfc_2606_and_allows_routable() {
        for bad in [
            "someone@example.com",
            "a@example.net",
            "b@example.org",
            "c@sub.example.com",
            "d@foo.invalid",
            "e@foo.test",
            "f@foo.example",
            "g@host.local",
            "h@localhost",
        ] {
            assert!(is_reserved_address(bad), "{bad} must be flagged reserved");
        }
        // Routable addresses whose LABEL merely looks reserved stay allowed.
        for ok in [
            "david@swiftsoftware.dev",
            "user@test.swiftsoftware.net",
            "a@real.example.io",
        ] {
            assert!(
                !is_reserved_address(ok),
                "{ok} must NOT be flagged reserved"
            );
        }
    }
}
