//! Immutable effective-program finalization boundary.

use serde::Serialize;
use std::sync::Arc;

use crate::error::EngineError;
use crate::item_resolution::ResolutionRoots;
use crate::launch_config::{LaunchConfigDependencyProof, LaunchConfigProofStatus};
use crate::project_content::AuthoritativeProjectContent;
use crate::resolution::{EffectiveDefinitionDigest, ResolutionOutput};

/// The only production value from which a managed launch or sealed root
/// request may be constructed.
#[derive(Debug, Clone)]
pub struct FinalizedEffectiveProgram {
    resolution: ResolutionOutput,
    effective_definition_digest: EffectiveDefinitionDigest,
}

impl FinalizedEffectiveProgram {
    pub fn resolution(&self) -> &ResolutionOutput {
        &self.resolution
    }

    pub fn effective_definition_digest(&self) -> &EffectiveDefinitionDigest {
        &self.effective_definition_digest
    }

    pub fn into_parts(self) -> (ResolutionOutput, EffectiveDefinitionDigest) {
        (self.resolution, self.effective_definition_digest)
    }
}

#[cfg(test)]
pub(crate) fn finalized_test_fixture(
    resolution: ResolutionOutput,
) -> Result<FinalizedEffectiveProgram, EngineError> {
    let effective_definition_digest = resolution
        .effective_definition_digest()
        .map_err(|error| EngineError::Internal(error.to_string()))?;
    Ok(FinalizedEffectiveProgram {
        resolution,
        effective_definition_digest,
    })
}

/// Immutable, engine-locked output of kind-declared semantic validation.
/// Fields and construction remain private so downstream crates cannot mutate
/// or fabricate a validated resolution.
#[derive(Debug)]
pub struct ValidatedEffectiveProgramCandidate {
    resolution: ResolutionOutput,
    binding: String,
    instance: Arc<()>,
}

/// Candidate-bound proof that all mutable capture dependencies were current
/// after semantic validation and before hashing/sealing.
#[derive(Debug)]
pub struct FinalizationAuthorityProof {
    candidate_instance: Arc<()>,
    candidate_binding: String,
    authority_binding: String,
}

/// Engine-owned semantic-validation success token. Effective-validator
/// dispatch constructs this only after a strict `valid` response.
#[derive(Debug)]
pub struct EffectiveValidationSuccess {
    normalized_digest: String,
}

impl EffectiveValidationSuccess {
    pub(crate) fn from_normalized(normalized: &serde_json::Value) -> Result<Self, EngineError> {
        let canonical = lillux::cas::canonical_json(normalized).map_err(|error| {
            EngineError::Internal(format!("canonicalize validation result: {error}"))
        })?;
        Ok(Self {
            normalized_digest: lillux::cas::sha256_hex(canonical.as_bytes()),
        })
    }

    /// Kinds with no declared semantic validator still pass through the same
    /// engine gate using a fixed, explicit validation result.
    pub(crate) fn no_declared_validator() -> Self {
        Self {
            normalized_digest: lillux::cas::sha256_hex(b"ryeos.no_declared_effective_validator.v1"),
        }
    }
}

/// Lock a fully augmented resolution after its kind-declared semantic
/// validator has succeeded.
pub fn lock_validated_effective_program(
    resolution: ResolutionOutput,
    validation: EffectiveValidationSuccess,
) -> Result<ValidatedEffectiveProgramCandidate, EngineError> {
    let binding = candidate_binding(&resolution, &validation.normalized_digest)?;
    Ok(ValidatedEffectiveProgramCandidate {
        resolution,
        binding,
        instance: Arc::new(()),
    })
}

/// Revalidate every mutable launch-config proof against the same resolution
/// roots and project authority, then bind that proof to the immutable
/// candidate. A changed dependency never produces a finalization token.
pub fn prove_finalization_authority(
    candidate: &ValidatedEffectiveProgramCandidate,
    proofs: &[LaunchConfigDependencyProof],
    roots: &ResolutionRoots,
    project: Option<(&std::path::Path, &dyn AuthoritativeProjectContent)>,
    external_realization: Option<(
        &crate::external_realization::ExternalRealizationProof,
        &dyn crate::external_realization::RealizationStore,
    )>,
) -> Result<FinalizationAuthorityProof, EngineError> {
    let mut identities = Vec::with_capacity(proofs.len());
    for proof in proofs {
        match proof.revalidate_under_authority_status(roots, project) {
            LaunchConfigProofStatus::Current => identities.push(proof.identity_digest()?),
            LaunchConfigProofStatus::MutableAuthorityChanged => {
                return Err(EngineError::MutableEffectiveProgramAuthorityChanged);
            }
            LaunchConfigProofStatus::ImmutableAuthorityMismatch => {
                return Err(EngineError::Internal(
                    "immutable effective-program authority mismatched during finalization"
                        .to_string(),
                ));
            }
        }
    }
    let captured = candidate
        .resolution
        .composed
        .derived
        .get(crate::external_content::EXTERNAL_REALIZATIONS_DERIVED_KEY)
        .map(ryeos_state::objects::ExternalContentRealizationSet::from_value)
        .transpose()
        .map_err(|error| EngineError::Internal(format!(
            "invalid external realization set at finalization: {error}"
        )))?;
    // A composed view that declares external content commits that declaration
    // into identity. Finalizing without a realization would admit a program
    // whose identity names dependencies it never captured — the launch path
    // must realize the declaration or refuse, never silently read live.
    let declared_external = candidate
        .resolution
        .composed
        .composed
        .get("external_content")
        .and_then(|value| value.as_array())
        .is_some_and(|entries| !entries.is_empty());
    if declared_external && captured.is_none() {
        return Err(EngineError::Internal(
            "effective program declares external content that was never realized; \
             this launch path cannot execute external declarations"
                .to_string(),
        ));
    }
    match (captured.as_ref(), external_realization) {
        (None, None) => {}
        (Some(_), None) => {
            return Err(EngineError::Internal(
                "effective program carries external realizations without a realization proof"
                    .to_string(),
            ));
        }
        (None, Some(_)) => {
            return Err(EngineError::Internal(
                "external realization proof does not belong to this effective program"
                    .to_string(),
            ));
        }
        (Some(captured), Some((proof, store))) => {
            if proof.realized() != captured {
                return Err(EngineError::Internal(
                    "external realization proof belongs to a different effective program"
                        .to_string(),
                ));
            }
            if proof.revalidate(store)
                != crate::external_realization::RealizationProofStatus::Current
            {
                return Err(EngineError::MutableEffectiveProgramAuthorityChanged);
            }
            identities.push(format!("external:{}", proof.identity_digest()?));
        }
    }
    identities.sort();
    let authority_binding = lillux::cas::sha256_hex(
        format!("ryeos.finalization_authority.v1\n{}", identities.join("\n")).as_bytes(),
    );
    Ok(FinalizationAuthorityProof {
        candidate_instance: Arc::clone(&candidate.instance),
        candidate_binding: candidate.binding.clone(),
        authority_binding,
    })
}

/// Consume the candidate and its exact proof. This is the one production
/// constructor for `FinalizedEffectiveProgram` and the one call site that
/// computes the effective-definition digest.
pub fn finalize_effective_program(
    candidate: ValidatedEffectiveProgramCandidate,
    proof: FinalizationAuthorityProof,
) -> Result<FinalizedEffectiveProgram, EngineError> {
    if !Arc::ptr_eq(&candidate.instance, &proof.candidate_instance)
        || candidate.binding != proof.candidate_binding
        || proof.authority_binding.is_empty()
    {
        return Err(EngineError::Internal(
            "finalization authority proof is forged or belongs to another candidate".to_string(),
        ));
    }
    let effective_definition_digest = candidate
        .resolution
        .effective_definition_digest()
        .map_err(|error| EngineError::Internal(error.to_string()))?;
    Ok(FinalizedEffectiveProgram {
        resolution: candidate.resolution,
        effective_definition_digest,
    })
}

fn candidate_binding(
    resolution: &ResolutionOutput,
    validation_digest: &str,
) -> Result<String, EngineError> {
    #[derive(Serialize)]
    struct CandidateSeed<'a> {
        schema: &'static str,
        resolution: &'a ResolutionOutput,
        validation_digest: &'a str,
    }
    let value = serde_json::to_value(CandidateSeed {
        schema: "ryeos.validated_effective_program_candidate.v1",
        resolution,
        validation_digest,
    })
    .map_err(|error| EngineError::Internal(format!("encode validation candidate: {error}")))?;
    let canonical = lillux::cas::canonical_json(&value).map_err(|error| {
        EngineError::Internal(format!("canonicalize validation candidate: {error}"))
    })?;
    Ok(lillux::cas::sha256_hex(canonical.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::ItemSpace;
    use crate::resolution::{KindComposedView, ResolutionStepName, ResolvedAncestor, TrustClass};
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn resolution(start: &str) -> ResolutionOutput {
        ResolutionOutput {
            root: ResolvedAncestor {
                requested_id: "test/program".to_string(),
                resolved_ref: "graph:test/program".to_string(),
                source_path: PathBuf::from("/diagnostic/program.yaml"),
                source_space: ItemSpace::Bundle,
                trust_class: TrustClass::TrustedBundle,
                signer_fingerprint: Some("f".repeat(64)),
                alias_resolution: None,
                added_by: ResolutionStepName::PipelineInit,
                raw_content: "fixture".to_string(),
                source_content_digest: "b".repeat(64),
                raw_content_digest: "a".repeat(64),
            },
            ancestors: Vec::new(),
            references_edges: Vec::new(),
            referenced_items: Vec::new(),
            step_outputs: HashMap::new(),
            effective_trust_class: TrustClass::TrustedBundle,
            composed: KindComposedView {
                composed: serde_json::json!({"config": {"start": start}}),
                derived: HashMap::new(),
                policy_facts: HashMap::new(),
            },
        }
    }

    #[test]
    fn finalization_proof_is_bound_to_the_exact_validated_candidate() {
        let candidate_a = lock_validated_effective_program(
            resolution("a"),
            EffectiveValidationSuccess::no_declared_validator(),
        )
        .unwrap();
        let proof_a = prove_finalization_authority(
            &candidate_a,
            &[],
            &ResolutionRoots::from_flat(None, Vec::new()),
            None,
            None,
        )
        .unwrap();
        let candidate_b = lock_validated_effective_program(
            resolution("b"),
            EffectiveValidationSuccess::no_declared_validator(),
        )
        .unwrap();

        let error = finalize_effective_program(candidate_b, proof_a).unwrap_err();
        assert!(error.to_string().contains("belongs to another candidate"));
    }

    #[test]
    fn identical_content_does_not_make_proofs_cross_candidate_reusable() {
        let candidate_a = lock_validated_effective_program(
            resolution("same"),
            EffectiveValidationSuccess::no_declared_validator(),
        )
        .unwrap();
        let proof_a = prove_finalization_authority(
            &candidate_a,
            &[],
            &ResolutionRoots::from_flat(None, Vec::new()),
            None,
            None,
        )
        .unwrap();
        let candidate_b = lock_validated_effective_program(
            resolution("same"),
            EffectiveValidationSuccess::no_declared_validator(),
        )
        .unwrap();

        let error = finalize_effective_program(candidate_b, proof_a).unwrap_err();
        assert!(error.to_string().contains("belongs to another candidate"));
    }

    #[test]
    fn finalized_program_exposes_the_digest_of_its_exact_resolution() {
        let candidate = lock_validated_effective_program(
            resolution("a"),
            EffectiveValidationSuccess::no_declared_validator(),
        )
        .unwrap();
        let proof = prove_finalization_authority(
            &candidate,
            &[],
            &ResolutionRoots::from_flat(None, Vec::new()),
            None,
            None,
        )
        .unwrap();
        let finalized = finalize_effective_program(candidate, proof).unwrap();

        assert_eq!(
            finalized.effective_definition_digest(),
            &finalized
                .resolution()
                .effective_definition_digest()
                .unwrap()
        );
    }

    #[test]
    fn finalization_refuses_a_stale_mutable_capture_proof() {
        let temp = tempfile::tempdir().unwrap();
        let node_root = crate::item_resolution::ResolutionRoot {
            space: ItemSpace::Node,
            label: "node".to_string(),
            ai_root: temp.path().join(".ai"),
        };
        let roots = ResolutionRoots {
            ordered: vec![node_root.clone()],
        };
        let proof = crate::launch_config::node_dependency_proof_test_fixture(
            0,
            &node_root,
            "config/ryeos-runtime/hooks/operator.yaml",
        )
        .unwrap();
        std::fs::create_dir_all(node_root.ai_root.join("config/ryeos-runtime/hooks")).unwrap();
        std::fs::write(
            node_root
                .ai_root
                .join("config/ryeos-runtime/hooks/operator.yaml"),
            "changed after capture\n",
        )
        .unwrap();
        let candidate = lock_validated_effective_program(
            resolution("a"),
            EffectiveValidationSuccess::no_declared_validator(),
        )
        .unwrap();

        let error =
            prove_finalization_authority(&candidate, &[proof], &roots, None, None).unwrap_err();
        assert!(error.to_string().contains("authority changed"));
    }

    #[test]
    fn a_declared_but_unrealized_external_set_cannot_finalize() {
        // A non-empty declaration with no captured realization must refuse on
        // every launch path — silently reading live content while identity
        // commits to the declaration is the state this layer exists to remove.
        let mut declaring = resolution("declares-external");
        declaring.composed.composed = serde_json::json!({
            "config": {"start": "declares-external"},
            "external_content": [{"id": "sim", "kind": "tree"}],
        });
        let candidate = lock_validated_effective_program(
            declaring,
            EffectiveValidationSuccess::no_declared_validator(),
        )
        .unwrap();
        let error = prove_finalization_authority(
            &candidate,
            &[],
            &ResolutionRoots::from_flat(None, Vec::new()),
            None,
            None,
        )
        .unwrap_err();
        assert!(error.to_string().contains("never realized"));

        // An explicit empty declaration means "captures nothing" and needs no
        // realization to finalize.
        let mut declares_none = resolution("declares-none");
        declares_none.composed.composed = serde_json::json!({
            "config": {"start": "declares-none"},
            "external_content": [],
        });
        let candidate = lock_validated_effective_program(
            declares_none,
            EffectiveValidationSuccess::no_declared_validator(),
        )
        .unwrap();
        prove_finalization_authority(
            &candidate,
            &[],
            &ResolutionRoots::from_flat(None, Vec::new()),
            None,
            None,
        )
        .unwrap();
    }
}
