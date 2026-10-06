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
// The ONE mint of a self-serve signup unit (kanban t_3724204f): the signup door and the FunnelSwift
// tag door both call `account_mint::mint_account`.
pub mod account_mint;
// Billing is also compiled into the LIB since 2026-10-01: the self-signup handler lives in
// `handlers` (a lib module) and now MINTS a credential with `billing::webhooks::generate_temp_password`
// — one generator, not a second copy of the charset. It was previously declared only in `main.rs`, so
// the lib could not see it and `cargo check` said "cannot find `billing` in `crate`".
pub mod billing;
pub mod body_deadline;
pub mod config;
pub mod db;
pub mod delivery;
mod email;
mod email_provider;
mod email_queue;
pub mod error;
pub mod features;
pub mod handlers;
pub mod iqs_validation;
mod lifecycle_emails;
pub mod mechanics;
pub mod security;
mod smtp;
pub mod state;
mod template_render;
pub mod template_types;
pub mod theme;
