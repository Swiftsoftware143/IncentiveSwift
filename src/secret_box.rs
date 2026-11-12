//! Secret-at-rest for tenant-supplied credentials (ported with IncentiveSwift's private inbox).
//!
//! Same envelope and the same tenant-scoped key derivation as CoreSwift's `secret_box`
//! (`enc:v1:<base64(nonce || ciphertext)>`, AES-256-GCM under
//! `SHA256(master || ":" || tenant_id)`), so a row's shape is identical across the fleet and a
//! credential copied to another tenant's row cannot be opened.
//!
//! WHERE THE MASTER KEY COMES FROM. CoreSwift reads `CORESWIFT_SECRET`. This app already carries a
//! long-lived signing secret in its environment (`JWT_SECRET`), and an inbox credential is exactly
//! as sensitive as a session token; deriving the AES master from it means the inbox needs no new
//! environment variable (and no container recreate to add one) while the key still lives outside
//! the repository. The derivation is domain-separated (`incentiveswift::private-email::v1`), so the
//! session-signing key cannot be recovered from it.
//!
//! Two halves, both required: writes FAIL CLOSED when no master key is configured, and reads never
//! fail (a legacy plaintext row still resolves) so the rollout cannot lock a customer out.

use crate::error::AppError;
use uuid::Uuid;

const PREFIX: &str = "enc:v1:";

/// Is this value already sealed?
pub fn is_sealed(stored: &str) -> bool {
    stored.starts_with(PREFIX)
}

/// Encrypt a credential for storage. An EMPTY value stays empty: callers use
/// `api_key <> ''` to mean "no credential configured", and sealing the empty string would turn
/// every empty slot into a configured one.
pub fn seal(tenant_id: Uuid, plaintext: &str) -> Result<String, AppError> {
    if plaintext.is_empty() {
        return Ok(String::new());
    }
    let ct =
        crate::private_email::encryption::encrypt_api_key(tenant_id, plaintext).map_err(|e| {
            AppError::Internal(format!(
            "Refusing to store a credential: the server's encryption key is not configured ({e})."
        ))
        })?;
    Ok(format!("{PREFIX}{ct}"))
}

/// Decrypt a stored credential. Never fails: a sealed value opens, a legacy plaintext value is
/// passed through (rows written before the seal existed), and an un-authenticatable ciphertext
/// reads as empty so the caller reports "no credential" instead of a 500.
pub fn open(tenant_id: Uuid, stored: &str) -> String {
    if stored.is_empty() {
        return String::new();
    }
    let body = stored.strip_prefix(PREFIX).unwrap_or(stored);
    crate::private_email::encryption::decrypt_api_key(tenant_id, body).unwrap_or_default()
}

/// Is the stored value effectively missing (empty, or a ciphertext this deployment cannot open)?
pub fn open_is_empty(tenant_id: Uuid, stored: &str) -> bool {
    open(tenant_id, stored).trim().is_empty()
}
