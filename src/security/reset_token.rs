//! Reset-token-at-rest (2026-11-19).
//!
//! A password-reset token is a BEARER credential: whoever can read the value out of
//! `password_resets` can take the account over for as long as the row is live. The table used to
//! hold that value verbatim, so a database read — a backup, a replica, a support export, an
//! injection — was replayable as a login. ADASwift already stores `token_hash`; this is the same
//! rule on this app's schema without a column rename: `password_resets.token` now holds the hex
//! digest, and the mailed link keeps the raw value. The lookup hashes what the caller presents
//! before it compares, so the two ends can never drift.
//!
//! The row is keyed on `account_id` here, not `user_id`. Writers that must hash:
//! `handlers::auth_handler::forgot_password`, `handlers::auth_handler::reset_password` (the
//! lookup and the consume) and the operator "sign-in help" mailer in
//! `handlers::email_settings_handler`.
//!
//! `migrations/zz_reset_tokens_hashed.sql` retires any row still holding a raw token (it is
//! marked used): a customer who needs a link asks again and receives a hashed one.

use sha2::{Digest, Sha256};

/// The stored form of a reset token: lowercase hex SHA-256 (64 characters).
pub fn hash(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect()
}

/// True when a stored value is shaped like a hash rather than a raw uuid token.
pub fn looks_hashed(stored: &str) -> bool {
    stored.len() == 64 && stored.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_64_lowercase_hex_and_stable() {
        let digest = hash("a-known-token");
        assert_eq!(digest.len(), 64);
        assert!(looks_hashed(&digest));
        assert_eq!(digest, hash("a-known-token"));
    }

    #[test]
    fn a_raw_uuid_token_does_not_look_hashed() {
        // Built at run time on purpose: the gate rejects UUID literals in src/ (they are the
        // hardcoded-id bug class), and a real reset token is exactly this shape.
        assert!(!looks_hashed(&uuid::Uuid::new_v4().to_string()));
    }

    #[test]
    fn known_sha256_vector() {
        // sha256("abc") — a fixed vector, so a change of algorithm cannot pass silently.
        assert_eq!(
            hash("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
