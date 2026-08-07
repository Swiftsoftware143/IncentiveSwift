
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

pub mod config;
pub mod features;
pub mod state;
pub mod error;
pub mod db;
pub mod handlers;
pub mod delivery;
pub mod mechanics;
pub mod access;
pub mod security;
pub mod iqs_validation;
mod email;
