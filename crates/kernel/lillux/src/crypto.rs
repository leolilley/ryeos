//! Crypto re-exports. All crates use these instead of importing ed25519_dalek directly.

use anyhow::Context;

pub use ed25519_dalek::pkcs8::{
    DecodePrivateKey, DecodePublicKey, EncodePrivateKey, EncodePublicKey,
};
pub use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};

/// Load a signing key from a PEM file.
pub fn load_signing_key(path: &std::path::Path) -> anyhow::Result<SigningKey> {
    let bytes = crate::read_regular_file_bounded_no_follow(path, 64 * 1024)
        .with_context(|| format!("failed to read signing key: {}", path.display()))?;
    let pem = std::str::from_utf8(&bytes)
        .with_context(|| format!("signing key is not UTF-8: {}", path.display()))?;
    SigningKey::from_pkcs8_pem(&pem)
        .with_context(|| format!("failed to decode signing key: {}", path.display()))
}

/// Load a signing key from one already-opened regular-file authority.
/// The descriptor is observed and read exactly once so callers that pin the
/// containing directory never fall back to pathname authority.
pub fn load_signing_key_from_open_file(mut file: std::fs::File) -> anyhow::Result<SigningKey> {
    let observation =
        crate::observe_open_regular_file(&file).context("observe signing-key descriptor")?;
    let bytes = crate::read_open_regular_file_stable_bounded(&mut file, &observation, 64 * 1024)
        .context("read stable signing-key descriptor")?;
    let pem = std::str::from_utf8(&bytes).context("signing key is not UTF-8")?;
    SigningKey::from_pkcs8_pem(pem).context("failed to decode signing key")
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
