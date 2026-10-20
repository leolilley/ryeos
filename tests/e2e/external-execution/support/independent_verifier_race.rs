//! The reserved START and RESUME race variant of the independent verifier
//! scenario, driven only by the daemon restart test.

use std::collections::BTreeMap;

use anyhow::Result;
use lillux::crypto::SigningKey;

use super::independent_verifier_scenario::IndependentVerifierScenario;

impl IndependentVerifierScenario {
    /// Test-only signed variant: the same selected producer and exact inputs,
    /// but the root verifier concurrently requests START and exact RESUME.
    /// It still issues no qualification claims.
    pub fn signed_sources_for_reserved_resume_race(
        &self,
        publisher: &SigningKey,
        signed_at: &str,
    ) -> Result<BTreeMap<String, Vec<u8>>> {
        self.signed_sources_with_mode(publisher, signed_at, true)
    }
}
