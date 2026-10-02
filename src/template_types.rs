//! The SENDABLE email-template vocabulary (kanban t_0eed3151).
//!
//! One list, derived from the senders themselves, that three callers share:
//!
//!   * `GET /api/v1/email-templates/types` — the served console's Type picker renders exactly this
//!     list, so a tenant can no longer mint a row of a type no sender selects, and the picker can
//!     REPRESENT the type of every row it offers to edit;
//!   * `POST`/`PUT /api/v1/email-templates` — a write whose `template_type` is not in this list is
//!     refused with 400, so the same dead-data class cannot be minted by curl either;
//!   * this module's own tests — the drift guard.
//!
//! Why the list is DERIVED rather than typed out: the previous picker was a hand-written
//! `<select>` of four strings that had drifted away from the senders in BOTH directions. It offered
//! `notification` and `general`, which appear in no sender (`get_default_subject`'s `_` arm and
//! `send_inline`'s `_` arm are fallbacks for a template_type that was never named anywhere), and it
//! could not express `purchase_confirmed` (asked for by `billing::webhooks::deliver_credentials`),
//! `welcome_credentials` (the checkout credential mail), any `{mechanic}_winner` type, or any of the
//! lifecycle types — so the two mails this card is about could not be authored from the console at
//! all.
//!
//! The three producer families, and where each is read from:
//!
//! | group             | producer                                                              |
//! |-------------------|-----------------------------------------------------------------------|
//! | Account emails    | `email::send_welcome_email` / `send_template_email` callers in `handlers::auth_handler` (welcome, welcome_credentials, purchase_confirmed, password_reset) |
//! | Campaign lifecycle| `lifecycle_emails::lifecycle_templates` (entry + follow-up arm)       |
//! | Mechanic winners  | `handlers::entries` step 8: `format!("{campaign_type}_winner")`, with `winner` as the fallback row |

use serde::Serialize;

/// The rows `handlers::entries` step 8 can resolve: `{mechanic}_winner` for every creatable
/// mechanic, plus the generic `winner` row every other mechanic falls back to.
pub const GENERIC_WINNER: &str = "winner";

/// The account mails: the `template_type` literals of `email::send_*` (`src/email.rs`), which is
/// also the vocabulary its inline fallback (`send_inline`) carries arms for.
pub const ACCOUNT_MAILS: &[(&str, &str)] = &[
    ("welcome", "Welcome email (self-signup)"),
    (
        "welcome_credentials",
        "Welcome email with the generated password (checkout)",
    ),
    (
        "purchase_confirmed",
        "Purchase confirmation (checkout / plan upgrade)",
    ),
    ("password_reset", "Password reset (forgot password)"),
];

pub const GROUP_ACCOUNT: &str = "Account emails";
pub const GROUP_LIFECYCLE: &str = "Campaign lifecycle";
pub const GROUP_WINNER: &str = "Mechanic winners";

/// One offered template type, in the order the picker renders it.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SendableTemplate {
    pub key: String,
    pub label: String,
    pub group: &'static str,
}

fn humanise(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    for (i, word) in key.split('_').enumerate() {
        if i > 0 {
            out.push(' ');
        }
        let mut chars = word.chars();
        if let Some(c) = chars.next() {
            out.extend(c.to_uppercase());
            out.push_str(chars.as_str());
        }
    }
    out
}

/// Every `template_type` a sender can select, grouped for the picker. Deduplicated: a key offered
/// twice would make the `<select>` lie about which row it will edit.
pub fn sendable_types() -> Vec<SendableTemplate> {
    let mut out: Vec<SendableTemplate> = Vec::new();
    let mut push = |key: String, label: String, group: &'static str| {
        if !out.iter().any(|t| t.key == key) {
            out.push(SendableTemplate { key, label, group });
        }
    };

    for (key, label) in ACCOUNT_MAILS {
        push((*key).to_string(), (*label).to_string(), GROUP_ACCOUNT);
    }
    for key in crate::lifecycle_emails::lifecycle_template_names() {
        push(key.to_string(), humanise(key), GROUP_LIFECYCLE);
    }
    for mechanic in crate::db::campaigns::VALID_MECHANIC_TYPES {
        let key = format!("{mechanic}_winner");
        let label = format!("{} winner", humanise(mechanic));
        push(key, label, GROUP_WINNER);
    }
    push(
        GENERIC_WINNER.to_string(),
        "Winner (fallback for mechanics with no row of their own)".to_string(),
        GROUP_WINNER,
    );

    out
}

/// Is `key` a type some sender can select? This is the write-side gate: a row whose type is not in
/// the list is a row nothing will ever look up.
pub fn is_sendable(key: &str) -> bool {
    let key = key.trim();
    !key.is_empty() && sendable_types().iter().any(|t| t.key == key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// The mirror defect this module closes: two real callers ask for a type with no row, and the
    /// old picker could not author either one. Both must be in the offered vocabulary.
    #[test]
    fn offers_every_type_a_sender_asks_for() {
        let keys: HashSet<String> = sendable_types().into_iter().map(|t| t.key).collect();
        for needed in [
            "welcome",
            "welcome_credentials",
            "purchase_confirmed",
            "password_reset",
        ] {
            assert!(
                keys.contains(needed),
                "account-mail type {needed} not offered"
            );
        }
        assert!(keys.contains(GENERIC_WINNER));
        for mechanic in crate::db::campaigns::VALID_MECHANIC_TYPES {
            assert!(
                keys.contains(&format!("{mechanic}_winner")),
                "winner type for {mechanic} not offered"
            );
        }
        for name in crate::lifecycle_emails::lifecycle_template_names() {
            assert!(keys.contains(name), "lifecycle type {name} not offered");
        }
    }

    /// The dead half the old picker offered: `notification` / `general` are `_` arms in
    /// `get_default_subject` / `send_inline`, not template types any sender names, so they must
    /// never be offered (nor accepted by the write path) again.
    #[test]
    fn never_offers_a_type_no_sender_names() {
        for dead in ["notification", "general", "", "  "] {
            assert!(!is_sendable(dead), "{dead:?} must not be sendable");
        }
        assert_eq!(
            sendable_types()
                .iter()
                .filter(|t| t.key == "notification" || t.key == "general")
                .count(),
            0
        );
    }

    #[test]
    fn keys_are_unique_and_always_land_in_a_known_group() {
        let types = sendable_types();
        let unique: HashSet<&str> = types.iter().map(|t| t.key.as_str()).collect();
        assert_eq!(unique.len(), types.len(), "a key is offered twice");
        for t in &types {
            assert!(
                matches!(t.group, GROUP_ACCOUNT | GROUP_LIFECYCLE | GROUP_WINNER),
                "unknown group {}",
                t.group
            );
            assert!(!t.label.trim().is_empty(), "{} has no label", t.key);
        }
    }
}
