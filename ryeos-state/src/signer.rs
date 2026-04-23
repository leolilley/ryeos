//! Signer trait for chain head refs.
//!
//! The crate doesn't hold the signing key. The daemon passes a signer
//! that implements this trait. The signing key never leaves daemon memory.

/// Trait for signing chain head refs.
///
/// Implementations must be `Send + Sync` so they can be used across
/// threads in the daemon.
pub trait Signer: Send + Sync {
    /// Sign the given data bytes, returning the raw signature bytes.
    fn sign(&self, data: &[u8]) -> Vec<u8>;

    /// Return the fingerprint of the signing key (e.g. SHA-256 of the
    /// public key in hex).
    fn fingerprint(&self) -> &str;
}

/// Deterministic test signer with real Ed25519 cryptography.
///
/// Only available in test builds. Uses a fixed seed (all 42s) to generate
/// a deterministic keypair. Produces real Ed25519 signatures that can be
/// verified using the contained public key.
#[cfg(test)]
pub struct TestSigner {
    signing_key: lillux::crypto::SigningKey,
    fingerprint: String,
}

#[cfg(test)]
impl TestSigner {
    /// Create a new test signer with a deterministic keypair.
    pub fn new() -> Self {
        // Deterministic seed: 32 bytes of 42
        let seed = [42u8; 32];
        let signing_key = lillux::crypto::SigningKey::from_bytes(&seed);
        let verifying_key = signing_key.verifying_key();
        let fingerprint = lillux::sha256_hex(verifying_key.as_bytes());
        
        Self {
            signing_key,
            fingerprint,
        }
    }

    /// Get the verifying key for signature verification.
    pub fn verifying_key(&self) -> lillux::crypto::VerifyingKey {
        self.signing_key.verifying_key()
    }

    /// Create a test signer with a custom fingerprint (for testing fingerprint mismatches).
    pub fn with_fingerprint(fingerprint: impl Into<String>) -> Self {
        let mut signer = Self::new();
        signer.fingerprint = fingerprint.into();
        signer
    }
}

#[cfg(test)]
impl Signer for TestSigner {
    fn sign(&self, data: &[u8]) -> Vec<u8> {
        use lillux::crypto::Signer as Ed25519Signer;
        self.signing_key.sign(data).to_bytes().to_vec()
    }

    fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
}

#[cfg(test)]
impl Default for TestSigner {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lillux::crypto::Verifier;

    #[test]
    fn test_signer_produces_valid_signatures() {
        let signer = TestSigner::new();
        let data = b"hello world";
        let sig = signer.sign(data);
        
        // Verify the signature using the public key
        let verifying_key = signer.verifying_key();
        let signature = lillux::crypto::Signature::from_bytes(
            &sig.as_slice()[..64].try_into().unwrap()
        );
        assert!(
            verifying_key.verify(data, &signature).is_ok(),
            "Signature must be valid"
        );
    }

    #[test]
    fn test_signer_fingerprint_is_stable() {
        let signer1 = TestSigner::new();
        let signer2 = TestSigner::new();
        assert_eq!(
            signer1.fingerprint(),
            signer2.fingerprint(),
            "Deterministic signers should have identical fingerprints"
        );
    }

    #[test]
    fn test_signer_custom_fingerprint() {
        let signer = TestSigner::with_fingerprint("custom-fp");
        assert_eq!(signer.fingerprint(), "custom-fp");
    }

    #[test]
    fn test_signer_default() {
        let signer = TestSigner::default();
        let signer2 = TestSigner::new();
        assert_eq!(signer.fingerprint(), signer2.fingerprint());
    }

    #[test]
    fn test_signer_signatures_are_deterministic() {
        let signer = TestSigner::new();
        let data = b"test data";
        let sig1 = signer.sign(data);
        let sig2 = signer.sign(data);
        assert_eq!(sig1, sig2, "Same data should produce same signature");
    }

    #[test]
    fn test_signer_different_data_different_signatures() {
        let signer = TestSigner::new();
        let sig1 = signer.sign(b"data1");
        let sig2 = signer.sign(b"data2");
        assert_ne!(sig1, sig2, "Different data should produce different signatures");
    }
}
