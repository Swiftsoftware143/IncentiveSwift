//! Authentication primitives for the fleet-canonical integration-schema module.
//!
//! `crate::integration_schema` (byte-identical across the fleet) expects an `AuthUser` extractor
//! with a single `is_admin` flag. IncentiveSwift's own boundary is `security::auth::AuthenticatedUser`
//! (role-based), so this module is the thin adapter: it authenticates with the SAME credential
//! machinery (issued API key first, then session JWT) and reduces it to `is_admin`, where an admin
//! is a session whose role is `admin`/`super_admin` (still backed by the live `accounts` row) OR the
//! owner account of an issued API key.

pub mod middleware;
