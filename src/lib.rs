//! This library serves as the main crate for IncentiveSwift.
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
mod email;
