//! This library serves as the main crate for IncentiveSwift.
#![allow(unused_variables, dead_code)]
#![allow(clippy::too_many_arguments)]
#![allow(clippy::redundant_locals)]
#![allow(clippy::doc_lazy_continuation)]
#![allow(clippy::if_same_then_else)]
#![allow(clippy::collapsible_match)]
#![allow(clippy::needless_borrows_for_generic_args)]
#![allow(clippy::type_complexity)]
#![allow(clippy::incompatible_msrv)]
#![allow(non_snake_case)]
//!
//! Integration tests use `incentiveswift_api::*` to access public types and functions.
//! Keep the module structure identical to main.rs so tests can reference everything.

pub mod access;
/// The fleet-standard auth boundary the canonical `integration_schema` module reads (`AuthUser`
/// with `is_admin`, accepting an admin session OR the admin's API key).
pub mod auth;

/// The describe config for `GET /api/v1/integration/schema` (CoreSwift data-mapping spec
/// 2026-11-13 §2). This is the ONE app-specific datum the byte-identical
/// `crate::integration_schema` module reads.
pub const SCHEMA: &integration_schema::AppSchema = &integration_schema::AppSchema {
    app: "incentiveswift",
    version: 1,
    entities: &[
        integration_schema::EntityConfig {
            key: "contacts",
            label: "Contacts",
            table: "contacts",
        },
        integration_schema::EntityConfig {
            key: "members",
            label: "Members",
            table: "loyalty_members",
        },
        integration_schema::EntityConfig {
            key: "entries",
            label: "Campaign entries",
            table: "entries",
        },
    ],
};
// The ONE mint of a self-serve signup unit (kanban t_3724204f): the signup door and the FunnelSwift
// tag door both call `account_mint::mint_account`.
pub mod account_mint;
// Billing is also compiled into the LIB since 2026-10-01: the self-signup handler lives in
// `handlers` (a lib module) and now MINTS a credential with `billing::webhooks::generate_temp_password`
// — one generator, not a second copy of the charset. It was previously declared only in `main.rs`, so
// the lib could not see it and `cargo check` said "cannot find `billing` in `crate`".
pub mod billing;
pub mod body_deadline;
pub mod branding;
pub mod config;
pub mod db;
pub mod delivery;
mod email;
mod email_provider;
mod email_queue;
pub mod error;
pub mod features;
pub mod handlers;
pub mod image_store;
/// Fleet-canonical `GET /api/v1/integration/schema` describe module (CoreSwift data-mapping spec
/// 2026-11-13 §2). Byte-identical across every SwiftSoftware app; only the `EntitySpec` list in
/// `handlers::integration_schema` and the route registration in `main.rs` are app-specific.
pub mod integration_schema;
pub mod iqs_validation;
mod lifecycle_emails;
mod login_throttle;
pub mod mechanics;
// Attribution for a workspace a test harness mints through the public signup
// (`X-Swift-Harness`, kanban t_4a744754). Declared here as well as in `main.rs` because the
// integration tests build the LIB, and the module structure of the two must stay identical.
pub mod probe_harness;
pub mod security;
mod smtp;
pub mod state;
mod template_render;
pub mod template_types;
pub mod theme;
