pub mod none;
pub mod rye_signed;

use std::sync::Arc;

use crate::routes::compile::AuthVerifier;

pub struct AuthVerifierRegistry {
    verifiers: Vec<Arc<dyn AuthVerifier>>,
}

impl AuthVerifierRegistry {
    pub fn new() -> Self {
        Self {
            verifiers: Vec::new(),
        }
    }

    pub fn register(&mut self, verifier: Arc<dyn AuthVerifier>) {
        let key = verifier.key();
        if self.verifiers.iter().any(|v| v.key() == key) {
            panic!("AuthVerifierRegistry: duplicate verifier `{key}`");
        }
        self.verifiers.push(verifier);
    }

    pub fn get(&self, key: &str) -> Option<&dyn AuthVerifier> {
        self.verifiers
            .iter()
            .find(|v| v.key() == key)
            .map(|v| v.as_ref())
    }

    pub fn with_builtins() -> Self {
        let mut r = Self::new();
        r.register(Arc::new(none::NoneVerifier));
        r.register(Arc::new(rye_signed::RyeSignedVerifier));
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[should_panic(expected = "duplicate verifier")]
    fn duplicate_registration_panics() {
        let mut r = AuthVerifierRegistry::new();
        r.register(Arc::new(none::NoneVerifier));
        r.register(Arc::new(none::NoneVerifier));
    }

    #[test]
    fn builtins_has_none_and_rye_signed() {
        let r = AuthVerifierRegistry::with_builtins();
        assert!(r.get("none").is_some());
        assert!(r.get("rye_signed").is_some());
        // hmac_sha256_v1 was deferred (F.17) — Phase E only needs rye_signed.
        // If/when it's reintroduced it must implement timestamp window,
        // anti-replay, and constant-time compare per spec §6.
        assert!(r.get("hmac_sha256_v1").is_none());
    }
}
