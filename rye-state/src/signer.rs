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

/// Deterministic test signer that returns zeros.
///
/// Only available in test builds. The fingerprint is the fixed string
/// `"test-signer-fingerprint"` and signatures are all-zero bytes of the
/// same length as an Ed25519 signature (64 bytes).
#[cfg(test)]
pub struct TestSigner {
    fingerprint: String,
}

#[cfg(test)]
impl TestSigner {
    /// Create a new test signer with the default fingerprint.
    pub fn new() -> Self {
        Self {
            fingerprint: "test-signer-fingerprint".to_string(),
        }
    }

    /// Create a test signer with a custom fingerprint.
    pub fn with_fingerprint(fingerprint: impl Into<String>) -> Self {
        Self {
            fingerprint: fingerprint.into(),
        }
    }
}

#[cfg(test)]
impl Signer for TestSigner {
    fn sign(&self, _data: &[u8]) -> Vec<u8> {
        vec![0u8; 64] // Ed25519 signature length
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

    #[test]
    fn test_signer_returns_zeros() {
        let signer = TestSigner::new();
        let sig = signer.sign(b"hello");
        assert_eq!(sig.len(), 64);
        assert!(sig.iter().all(|&b| b == 0));
    }

    #[test]
    fn test_signer_fingerprint() {
        let signer = TestSigner::new();
        assert_eq!(signer.fingerprint(), "test-signer-fingerprint");
    }

    #[test]
    fn test_signer_custom_fingerprint() {
        let signer = TestSigner::with_fingerprint("custom-fp");
        assert_eq!(signer.fingerprint(), "custom-fp");
    }

    #[test]
    fn test_signer_default() {
        let signer = TestSigner::default();
        assert_eq!(signer.fingerprint(), "test-signer-fingerprint");
    }

    #[test]
    fn test_signer_deterministic() {
        let signer = TestSigner::new();
        let sig1 = signer.sign(b"data1");
        let sig2 = signer.sign(b"data2");
        assert_eq!(sig1, sig2, "TestSigner should return same signature for all data");
    }
}
