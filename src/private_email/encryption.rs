//! AES-256-GCM for tenant-supplied credentials at rest (the private-inbox half of IncentiveSwift).
//!
//! Key = `SHA256(master || ":" || tenant_id)`: the same plaintext under two tenants is two different
//! keys, so a row copied between accounts does not yield a usable credential.
//!
//! WHERE THE MASTER COMES FROM. `INCENTIVESWIFT_SECRET` when the deployment sets it (>= 32 chars), else a
//! DOMAIN-SEPARATED derivation of this app's own signing secret (`JWT_SECRET`): an inbox credential
//! is exactly as sensitive as a session token, both already live outside the repository, and
//! deriving one from the other means the inbox needs no new environment variable. The prefix
//! `incentiveswift::private-email::v1` keeps the two keys independent.
//!
//! Writes FAIL CLOSED when neither is configured — a credential is never written in the clear — and
//! reads stay tolerant (`decrypt_api_key_legacy`) so a row sealed under an earlier key still
//! resolves instead of locking a customer out.
use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use rand::Rng;
use sha2::{Digest, Sha256};
use uuid::Uuid;

const ENV_VAR: &str = "INCENTIVESWIFT_SECRET";
const FALLBACK_ENV_VAR: &str = "JWT_SECRET";
const MIN_SECRET_LEN: usize = 32;
const DOMAIN: &str = "incentiveswift::private-email::v1";

/// Is a usable master secret configured? Writes are refused when this is false.
pub fn is_configured() -> bool {
    env_secret().is_some()
}

fn env_secret() -> Option<String> {
    if let Ok(v) = std::env::var(ENV_VAR) {
        if v.len() >= MIN_SECRET_LEN {
            return Some(v);
        }
        tracing::error!(
            len = v.len(),
            min = MIN_SECRET_LEN,
            "{} is too short to be a master key — refusing to use it",
            ENV_VAR
        );
    }
    match std::env::var(FALLBACK_ENV_VAR) {
        Ok(v) if v.len() >= MIN_SECRET_LEN => Some(format!("{DOMAIN}::{v}")),
        _ => None,
    }
}

/// Encrypt a credential for storage. Fails closed: with no configured master key there is no safe
/// encryption, and a key that lives in the source tree is not encryption at rest.
pub fn encrypt_api_key(tenant_id: Uuid, plaintext: &str) -> Result<String, String> {
    let secret = env_secret().ok_or_else(|| {
        format!(
            "neither {ENV_VAR} nor {FALLBACK_ENV_VAR} is set to a value of at least \
             {MIN_SECRET_LEN} characters: refusing to encrypt a credential"
        )
    })?;
    encrypt_with(&secret, tenant_id, plaintext)
}

pub fn encrypt_with(secret: &str, tenant_id: Uuid, plaintext: &str) -> Result<String, String> {
    let key = derive_key_from(secret, tenant_id);
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|e| format!("cipher init: {}", e))?;

    let mut nonce_bytes = [0u8; 12];
    rand::thread_rng().fill(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, plaintext.as_bytes())
        .map_err(|e| format!("encrypt: {}", e))?;

    let mut combined = nonce_bytes.to_vec();
    combined.extend_from_slice(&ciphertext);

    Ok(BASE64.encode(&combined))
}

/// Decrypt a stored credential with the CONFIGURED master key.
pub fn decrypt_api_key(tenant_id: Uuid, encrypted: &str) -> Result<String, String> {
    let secret = env_secret().ok_or_else(|| format!("{ENV_VAR} is not set"))?;
    decrypt_with(&secret, tenant_id, encrypted)
}

/// Read a value sealed under an earlier key source. IncentiveSwift has no such history (the inbox arrives
/// with this module), so this is the configured path: it exists so the ported callers compile and a
/// future key move has ONE function to teach.
pub fn decrypt_api_key_legacy(tenant_id: Uuid, encrypted: &str) -> Result<String, String> {
    decrypt_api_key(tenant_id, encrypted)
}

pub fn decrypt_with(secret: &str, tenant_id: Uuid, encrypted: &str) -> Result<String, String> {
    let key = derive_key_from(secret, tenant_id);
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|e| format!("cipher init: {}", e))?;

    let combined = BASE64
        .decode(encrypted)
        .map_err(|e| format!("base64 decode: {}", e))?;

    if combined.len() < 12 {
        return Err("ciphertext too short".into());
    }

    let (nonce_bytes, ciphertext) = combined.split_at(12);
    let nonce = Nonce::from_slice(nonce_bytes);

    let plaintext = cipher
        .decrypt(nonce, ciphertext)
        .map_err(|e| format!("decrypt: {}", e))?;

    String::from_utf8(plaintext).map_err(|e| format!("utf8: {}", e))
}

fn derive_key_from(secret: &str, tenant_id: Uuid) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(secret.as_bytes());
    hasher.update(b":");
    hasher.update(tenant_id.as_bytes());
    let hash = hasher.finalize();

    let mut key = [0u8; 32];
    key.copy_from_slice(&hash);
    key
}
