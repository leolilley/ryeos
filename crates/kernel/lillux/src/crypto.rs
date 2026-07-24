//! Crypto re-exports. All crates use these instead of importing ed25519_dalek directly.

use anyhow::Context;

pub use ed25519_dalek::pkcs8::{
    DecodePrivateKey, DecodePublicKey, EncodePrivateKey, EncodePublicKey,
};
pub use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};

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

/// HMAC-SHA256 of `message` under `key`, rendered as lowercase hex.
pub fn hmac_sha256_hex(key: &[u8], message: &[u8]) -> String {
    use hmac::{Hmac, Mac};
    let mut mac =
        <Hmac<sha2::Sha256>>::new_from_slice(key).expect("HMAC-SHA256 accepts keys of any length");
    mac.update(message);
    mac.finalize()
        .into_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn hmac_sha256_hex_matches_rfc_4231_test_case_2() {
        assert_eq!(
            super::hmac_sha256_hex(b"Jefe", b"what do ya want for nothing?"),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }
}
