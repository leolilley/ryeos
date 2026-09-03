//! Crypto re-exports. All crates use these instead of importing ed25519_dalek directly.

use anyhow::Context;

pub use ed25519_dalek::pkcs8::{
    DecodePrivateKey, DecodePublicKey, EncodePrivateKey, EncodePublicKey,
};
pub use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};

/// Generate an Ed25519 signing key from the platform CSPRNG. Entropy source
/// selection stays inside Lillux rather than leaking OS mechanics to callers.
pub fn generate_signing_key() -> SigningKey {
    SigningKey::generate(&mut rand::rngs::OsRng)
}

/// Generate and create one private signing-key file without following links
/// or replacing an incumbent pathname.
pub fn create_signing_key(path: &std::path::Path) -> anyhow::Result<SigningKey> {
    let parent_path = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("signing key path has no parent: {}", path.display()))?;
    let name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("signing key path has no filename: {}", path.display()))?;
    let parent = crate::PinnedDirectory::open_or_create(parent_path)
        .with_context(|| format!("pin signing key parent {}", parent_path.display()))?;
    let signing_key = generate_signing_key();
    let pem = signing_key
        .to_pkcs8_pem(Default::default())
        .context("failed to serialize signing key")?;
    parent
        .atomic_write_if_same(name, None, pem.as_bytes(), 0o600)
        .with_context(|| format!("create signing key {}", path.display()))?;
    Ok(signing_key)
}

/// Load a signing key from a PEM file.
pub fn load_signing_key(path: &std::path::Path) -> anyhow::Result<SigningKey> {
    let bytes = crate::read_regular_file_bounded_no_follow(path, 64 * 1024)
        .with_context(|| format!("failed to read signing key: {}", path.display()))?;
    let pem = std::str::from_utf8(&bytes)
        .with_context(|| format!("signing key is not UTF-8: {}", path.display()))?;
    SigningKey::from_pkcs8_pem(&pem)
        .with_context(|| format!("failed to decode signing key: {}", path.display()))
}

/// Load a signing key from one exact pinned regular-file authority.
pub fn load_signing_key_from_pinned_file(
    file: &crate::PinnedRegularFile,
) -> anyhow::Result<SigningKey> {
    let observation = file
        .observation()
        .context("observe signing-key descriptor")?;
    let bytes = file
        .read_stable_bounded(&observation, 64 * 1024)
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
