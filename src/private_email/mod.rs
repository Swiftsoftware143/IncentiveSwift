//! The private inbox (ported from CoreSwift-CRM, kanban t_cd522551).
//!
//! WHAT THIS IS. A customer's own mailboxes on their OWN domain: they bring a domain and a Mailgun
//! (or any SMTP) account, add the DNS records this module prints, and then create as many addresses
//! as their plan sells. Mail to any of those addresses is received by the provider's inbound webhook
//! and stored as a `private_email_messages` row, which is what the mail client
//! (`inbox.incentiveswift.com`) reads and answers.
//!
//! WHY IT LIVES HERE AND NOT IN CoreSwift. The model and the screens are CoreSwift's; this is the
//! port to IncentiveSwift so every Swift app offers the same multi-address inbox. The API shape, the
//! table names and the mail client are deliberately IDENTICAL, so one client can be served from
//! each app without a translation layer — and an operator who learns one app has learned them all.
//!
//! WHAT WAS LEFT BEHIND, on purpose:
//!   * ticket creation from a message — IncentiveSwift's queues answer on `support_tickets`, and the
//!     bridge between mail and that queue is separate work, not part of this port;
//!   * AI thread summaries — CoreSwift routes those through its own provider registry;
//!   * the platform admin limits console and the trial meter — CoreSwift's billing model, not this
//!     app's. The plan ceilings that DO apply here are read from `plans.features` (see
//!     [`crate::module_registry`]).
//!
//! Every route sits behind the app's one credential boundary (`auth::boundary`): an anonymous caller
//! is refused before routing, and every handler additionally scopes its SQL by the SESSION's tenant.

use crate::state::AppState;
use axum::{middleware, Router};

pub mod api_keys_handler;
pub mod domain_handler;
pub mod encryption;
pub mod feature_gate;
pub mod mailbox_handler;
pub mod messages_handler;
pub mod models;
pub mod providers;
#[allow(dead_code)]
pub mod purge;
pub mod send_handler;
pub mod signature;
#[allow(dead_code)]
pub mod storage;
pub mod webhook_handler;

pub fn router(state: AppState) -> Router<AppState> {
    Router::new()
        // ── Domains: the customer's own domain, its records, and its verification ──────────────
        .route("/domains", axum::routing::get(domain_handler::list_domains))
        .route("/domains", axum::routing::post(domain_handler::add_domain))
        .route(
            "/domains/:id",
            axum::routing::patch(domain_handler::update_domain),
        )
        .route(
            "/domains/:id",
            axum::routing::delete(domain_handler::delete_domain),
        )
        // Re-read the provider's own state for this domain: what makes a domain the provider
        // activates AFTER it was added become usable without re-entering the credential.
        .route(
            "/domains/:id/verify",
            axum::routing::post(domain_handler::verify_domain),
        )
        // The records the customer must add, in plain English. Values come from the provider
        // itself, never from a doc.
        .route(
            "/domains/:id/records",
            axum::routing::get(domain_handler::domain_records),
        )
        // ── Mailboxes: the addresses the inbox receives on ─────────────────────────────────────
        .route(
            "/boxes",
            axum::routing::get(mailbox_handler::list_mailboxes),
        )
        .route(
            "/boxes",
            axum::routing::post(mailbox_handler::provision_mailbox),
        )
        .route(
            "/boxes/:id",
            axum::routing::patch(mailbox_handler::update_mailbox),
        )
        .route(
            "/boxes/:id",
            axum::routing::delete(mailbox_handler::delete_mailbox),
        )
        // ── Messages: the inbox's own records ─────────────────────────────────────────────────
        .route(
            "/messages",
            axum::routing::get(messages_handler::list_messages),
        )
        // Static segment before the capture, so /messages/counts is never read as an id.
        .route(
            "/messages/counts",
            axum::routing::get(messages_handler::message_counts),
        )
        // The composer's own store — Drafts has to be able to hold something.
        .route(
            "/messages/drafts",
            axum::routing::post(messages_handler::save_draft),
        )
        .route(
            "/messages/:id",
            axum::routing::get(messages_handler::get_message)
                .patch(messages_handler::update_message)
                .delete(messages_handler::delete_message),
        )
        .route(
            "/messages/:id/reply",
            axum::routing::post(messages_handler::reply_to_message),
        )
        // The people a message can be assigned to — this account's members only.
        .route(
            "/members",
            axum::routing::get(messages_handler::list_members),
        )
        // The address filter's source of truth: every address this account receives on, plus MISC.
        .route(
            "/addresses",
            axum::routing::get(messages_handler::list_addresses),
        )
        // The rail: system folders with live counts plus real subfolders.
        .route(
            "/folders",
            axum::routing::get(messages_handler::list_folders)
                .post(messages_handler::create_folder),
        )
        // Mark all read / unread, and empty Spam or Trash (destructive, always reports a count).
        .route(
            "/folders/action",
            axum::routing::post(messages_handler::folder_action),
        )
        // Labels: name + colour, per account, applied to one or many messages.
        .route(
            "/labels",
            axum::routing::get(messages_handler::list_labels).post(messages_handler::create_label),
        )
        .route(
            "/labels/:id",
            axum::routing::patch(messages_handler::update_label)
                .delete(messages_handler::delete_label),
        )
        .route(
            "/labels/:id/apply",
            axum::routing::post(messages_handler::apply_label)
                .delete(messages_handler::remove_label),
        )
        // Send email
        .route("/send", axum::routing::post(send_handler::send_email))
        // ── The company signature ───────────────────────────────────────────────────────────────
        // `GET /signature?mailbox=` is what Compose reads to show the block BEFORE sending;
        // `PUT` saves that address's own choice; `/signature/workspace` is the ONE writer of the
        // workspace default edited on the branding screen.
        .route(
            "/signature",
            axum::routing::get(signature::get_signature).put(signature::put_signature),
        )
        .route(
            "/signature/workspace",
            axum::routing::get(signature::get_workspace_signature)
                .put(signature::put_workspace_signature),
        )
        .layer(axum::middleware::from_fn_with_state(
            crate::body_deadline::BodyReadDeadline::from_secs(state.config.body_read_deadline_secs),
            crate::body_deadline::body_read_deadline_middleware,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            crate::access::claims::claims_middleware,
        ))
}
