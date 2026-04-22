//! Crypto re-exports. All crates use these instead of importing ed25519_dalek directly.

use anyhow::Context;

pub use ed25519_dalek::{
    Signature, Signer, SigningKey, Verifier, VerifyingKey,
};
pub use ed25519_dalek::pkcs8::{DecodePrivateKey, EncodePrivateKey, EncodePublicKey};

/// Load a signing key from a PEM file.
pub fn load_signing_key(path: &std::path::Path) -> anyhow::Result<SigningKey> {
    let pem = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read signing key: {}", path.display()))?;
    SigningKey::from_pkcs8_pem(&pem)
        .with_context(|| format!("failed to decode signing key: {}", path.display()))
}

/// Compute the fingerprint (SHA256 hex) of a verifying key.
pub fn fingerprint(key: &VerifyingKey) -> String {
    crate::sha256_hex(key.as_bytes())
}
