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
//!   * the referral/credit path (`handlers::external_grants::{grant_credits,register_member}`,
//!     both funnelling through `find_or_create_account`),
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

    #[test]
    fn lookup_key_matches_what_normalize_stores() {
        assert_eq!(lookup_key("  Zaarhub@gmail.com "), "zaarhub@gmail.com");
        // A malformed value has no failure arm here — it just matches nothing.
        assert_eq!(lookup_key("bad"), "bad");
    }
}
