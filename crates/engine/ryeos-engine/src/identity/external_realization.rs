//! Captured realizations and their finalization proof.
//!
//! A realization is the admitted, immutable form of one declaration: a
//! manifest in CAS, the blobs it references, and the logical place the runtime
//! will see it. The set of realizations for a launch is written to the sealed
//! program's derived values, which is what carries it into executable
//! identity and into the capsule.
//!
//! The proof here deliberately does not re-observe live content. Once
//! execution reads CAS, what the live filesystem does between capture and
//! spawn cannot change what runs, so re-checking it would answer a question
//! that no longer matters — and a size/mtime/inode witness would not answer it
//! honestly anyway, since all three can be preserved across a content change.
//! What must hold is that the realization is *redeemable*: the manifest is
//! present, its blobs exist, and the candidate commits to exactly that
//! closure.

use serde::Serialize;

use crate::error::EngineError;
pub use ryeos_state::objects::{
    ExternalContentRealization as RealizedExternalContent,
    ExternalContentRealizationSet as RealizedExternalContentSet,
};

/// Whether a realization is redeemable at finalization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RealizationProofStatus {
    Current,
    /// The manifest or one of its blobs is missing. Fails closed: falling back
    /// to live content is the ambient behavior realization exists to remove.
    RealizationUnavailable,
}

/// Evidence that a candidate's realizations can actually be executed.
#[derive(Debug, Clone)]
pub struct ExternalRealizationProof {
    realized: RealizedExternalContentSet,
}

impl ExternalRealizationProof {
    fn new(realized: RealizedExternalContentSet) -> Result<Self, EngineError> {
        realized
            .validate()
            .map_err(|error| EngineError::Internal(error.to_string()))?;
        Ok(Self { realized })
    }

    pub fn realized(&self) -> &RealizedExternalContentSet {
        &self.realized
    }

    /// Stable identity folded into the finalization authority binding, so a
    /// finalized program cannot be paired with a different realization set
    /// than the one that was proved.
    pub fn identity_digest(&self) -> Result<String, EngineError> {
        #[derive(Serialize)]
        struct Seed<'a> {
            schema: &'static str,
            realized: &'a RealizedExternalContentSet,
        }
        let value = serde_json::to_value(Seed {
            schema: "ryeos.external_realization_proof.v1",
            realized: &self.realized,
        })
        .map_err(|error| {
            EngineError::Internal(format!("serialize external realization proof: {error}"))
        })?;
        let canonical = lillux::cas::canonical_json(&value).map_err(|error| {
            EngineError::Internal(format!("canonicalize external realization proof: {error}"))
        })?;
        Ok(lillux::cas::sha256_hex(canonical.as_bytes()))
    }

    /// Re-establish that every realization is redeemable.
    ///
    /// Deliberately narrow: presence and integrity in CAS, not a re-walk of
    /// live content.
    pub(crate) fn revalidate(&self, store: &dyn RealizationStore) -> RealizationProofStatus {
        for entry in self.realized.iter() {
            match store.realization_available(&entry.manifest_hash) {
                Ok(true) => {}
                Ok(false) | Err(_) => return RealizationProofStatus::RealizationUnavailable,
            }
        }
        RealizationProofStatus::Current
    }
}

/// Mint a realization proof only after the supplied store has verified every
/// manifest and blob in the exact set. Validation failure is fail-closed; an
/// unchecked set cannot be turned into finalization authority.
pub fn prove_external_realizations(
    realized: RealizedExternalContentSet,
    store: &dyn RealizationStore,
) -> Result<ExternalRealizationProof, EngineError> {
    let proof = ExternalRealizationProof::new(realized)?;
    if proof.revalidate(store) != RealizationProofStatus::Current {
        return Err(EngineError::Internal(
            "external content realization is unavailable from verified CAS authority".to_string(),
        ));
    }
    Ok(proof)
}

/// Presence oracle for realized content.
///
/// Abstracted so the engine can prove redeemability without depending on a
/// concrete store, and so tests can exercise the failure path without one.
pub trait RealizationStore {
    /// Is this manifest present, valid, and are all of its blobs present?
    fn realization_available(&self, manifest_hash: &str) -> anyhow::Result<bool>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn realized(id: &str, hash_seed: char) -> RealizedExternalContent {
        RealizedExternalContent {
            id: id.to_string(),
            kind: ryeos_state::objects::ExternalContentKind::Tree,
            mode: ryeos_state::objects::ExternalContentMode::Captured,
            manifest_hash: std::iter::repeat_n(hash_seed, 64).collect(),
            entry_count: 1,
            total_bytes: 1,
            mount: format!("mnt/{id}"),
        }
    }

    struct Present;
    impl RealizationStore for Present {
        fn realization_available(&self, _manifest_hash: &str) -> anyhow::Result<bool> {
            Ok(true)
        }
    }

    struct Absent;
    impl RealizationStore for Absent {
        fn realization_available(&self, _manifest_hash: &str) -> anyhow::Result<bool> {
            Ok(false)
        }
    }

    #[test]
    fn a_set_is_canonically_ordered_regardless_of_capture_order() {
        let first =
            RealizedExternalContentSet::new(vec![realized("b", 'b'), realized("a", 'a')]).unwrap();
        let second =
            RealizedExternalContentSet::new(vec![realized("a", 'a'), realized("b", 'b')]).unwrap();
        assert_eq!(first, second);
        assert_eq!(
            identity_digest_for_test(&first),
            identity_digest_for_test(&second)
        );
    }

    #[test]
    fn a_missing_realization_is_not_current() {
        let proof = prove_external_realizations(
            RealizedExternalContentSet::new(vec![realized("a", 'a')]).unwrap(),
            &Present,
        )
        .unwrap();
        assert_eq!(proof.revalidate(&Present), RealizationProofStatus::Current);
        assert_eq!(
            proof.revalidate(&Absent),
            RealizationProofStatus::RealizationUnavailable
        );
    }

    #[test]
    fn mount_target_participates_in_identity() {
        let mut moved = realized("a", 'a');
        moved.mount = "elsewhere".to_string();

        let original = prove_external_realizations(
            RealizedExternalContentSet::new(vec![realized("a", 'a')]).unwrap(),
            &Present,
        )
        .unwrap();
        let relocated = prove_external_realizations(
            RealizedExternalContentSet::new(vec![moved]).unwrap(),
            &Present,
        )
        .unwrap();

        // Identical bytes mounted elsewhere are a different program.
        assert_ne!(
            original.identity_digest().unwrap(),
            relocated.identity_digest().unwrap()
        );
    }

    fn identity_digest_for_test(value: &RealizedExternalContentSet) -> String {
        let canonical = lillux::cas::canonical_json(&serde_json::to_value(value).unwrap()).unwrap();
        lillux::cas::sha256_hex(canonical.as_bytes())
    }
}
