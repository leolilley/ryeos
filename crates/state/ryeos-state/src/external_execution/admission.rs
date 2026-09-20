//! Program requirements for external candidate execution. These identities
//! never grant cloud allocation or substitute for a protected placement binding.

use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::external_content::products::composition::ResolvedExternalProductSelections;
use crate::objects::{EXTERNAL_CONTENT_MANIFEST_KIND, ExternalContentKind, canonical_value_digest};

pub const PROTOCOL: &str = "ryeos.external-candidate.stdio.v1";
/// The initial runtime must qualify the whole routed execution boundary.
pub const REQUIRED_CLAIMS: &[&str] = &[
    "bounded_candidate_capture",
    "candidate_only_execution",
    "controller_credential_exclusion",
    "native_writer_exclusion",
    "no_local_execution_fallback",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalCandidateRequirement {
    pub schema: u32,
    pub protocol: String,
    pub runtime_product_declaration_id: String,
}

impl ExternalCandidateRequirement {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == 1 && self.protocol == PROTOCOL,
            "external candidate protocol is not admitted"
        );
        crate::external_content::products::validate_name(&self.runtime_product_declaration_id)
    }

    pub fn resolve(
        &self,
        selections: Option<&ResolvedExternalProductSelections>,
    ) -> Result<AdmittedExternalCandidateProgram> {
        self.validate()?;
        let selections =
            selections.context("external candidate requires retained runtime products")?;
        selections.validate()?;
        let runtime = selections
            .get(&self.runtime_product_declaration_id)
            .context("external candidate runtime product is absent")?;
        ensure!(
            runtime.manifest_kind == EXTERNAL_CONTENT_MANIFEST_KIND
                && runtime.declaration.kind == ExternalContentKind::Tree,
            "external candidate runtime requires an exact content tree"
        );
        let qualification = runtime
            .qualification
            .as_ref()
            .context("external candidate runtime has no admitted qualification")?;
        // Selection validation joins the proof to its exact policy and subject.
        // Require these claims in the signed relationship too: an incidental
        // verifier result cannot widen the consumer's qualification allowance.
        ensure!(
            REQUIRED_CLAIMS.iter().all(|claim| runtime
                .relationship
                .qualification
                .required_claims
                .iter()
                .any(|required| required == claim)),
            "external candidate runtime qualification is insufficient"
        );
        Ok(AdmittedExternalCandidateProgram {
            requirement: self.clone(),
            runtime_manifest_hash: runtime.manifest_hash.clone(),
            runtime_witness_hash: runtime.witness_hash.clone(),
            qualification_attestation_hash: qualification.attestation_hash.clone(),
            selection_identity_digest: canonical_value_digest(&runtime.semantic_identity_value()?)?,
        })
    }
}

/// Projection of already retained product authority. All CAS edges remain
/// owned by the capsule's full product selections, which must be rejoined on
/// validation. No occurrence, capsule hash or operator credential enters it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmittedExternalCandidateProgram {
    pub requirement: ExternalCandidateRequirement,
    pub runtime_manifest_hash: String,
    pub runtime_witness_hash: String,
    pub qualification_attestation_hash: String,
    pub selection_identity_digest: String,
}

impl AdmittedExternalCandidateProgram {
    pub fn validate(&self) -> Result<()> {
        self.requirement.validate()?;
        for hash in [
            &self.runtime_manifest_hash,
            &self.runtime_witness_hash,
            &self.qualification_attestation_hash,
            &self.selection_identity_digest,
        ] {
            super::hash(hash)?;
        }
        Ok(())
    }

    pub fn verify_selections(
        &self,
        selections: Option<&ResolvedExternalProductSelections>,
    ) -> Result<()> {
        self.validate()?;
        ensure!(
            self.requirement.resolve(selections)? == *self,
            "external candidate program contradicts retained product authority"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn requirement() -> ExternalCandidateRequirement {
        ExternalCandidateRequirement {
            schema: 1,
            protocol: PROTOCOL.into(),
            runtime_product_declaration_id: "auxiliary".into(),
        }
    }

    fn selections() -> ResolvedExternalProductSelections {
        let evidence = crate::external_content::products::qualification::tests::dynamic_evidence_with_auxiliary_proof();
        let mut selections = evidence.verifier_root_selections.unwrap().into_inner();
        let runtime = selections.get_mut("auxiliary").unwrap();
        let claims: Vec<String> = REQUIRED_CLAIMS
            .iter()
            .map(|claim| (*claim).into())
            .collect();
        runtime.relationship.qualification.required_claims = claims.clone();
        let proof = &mut runtime.qualification.as_mut().unwrap().evidence;
        proof.policy_source.policy.allowed_claims = claims.clone();
        proof.result.claims = claims;
        proof.verifier.result_digest = proof.result.digest().unwrap();
        ResolvedExternalProductSelections::new(selections).unwrap()
    }

    #[test]
    fn external_program_requires_exact_qualified_runtime() {
        let requirement = requirement();
        let selections = selections();
        let program = requirement.resolve(Some(&selections)).unwrap();
        program.verify_selections(Some(&selections)).unwrap();
        assert!(requirement.resolve(None).is_err());
        let mut missing = requirement.clone();
        missing.runtime_product_declaration_id = "absent".into();
        assert!(missing.resolve(Some(&selections)).is_err());
        let mut unqualified = requirement.clone();
        unqualified.runtime_product_declaration_id = "runtime".into();
        assert!(unqualified.resolve(Some(&selections)).is_err());
        let mut weaker = selections.clone().into_inner();
        weaker
            .get_mut("auxiliary")
            .unwrap()
            .relationship
            .qualification
            .required_claims
            .pop();
        let weaker = ResolvedExternalProductSelections::new(weaker).unwrap();
        assert!(requirement.resolve(Some(&weaker)).is_err());
        for field in [
            "runtime_manifest_hash",
            "runtime_witness_hash",
            "qualification_attestation_hash",
            "selection_identity_digest",
        ] {
            let mut wire = serde_json::to_value(&program).unwrap();
            wire[field] = serde_json::json!("0".repeat(64));
            let changed: AdmittedExternalCandidateProgram = serde_json::from_value(wire).unwrap();
            assert!(
                changed.verify_selections(Some(&selections)).is_err(),
                "{field}"
            );
        }
    }

    #[test]
    fn external_capsule_roundtrip_rejoins_profile_and_retained_selection() {
        use crate::external_content::products::composition::EXTERNAL_PRODUCT_SELECTIONS_DERIVED_KEY;
        use crate::objects::*;
        use serde_json::json;
        use std::collections::BTreeMap;
        let selections = selections();
        let requirement = requirement();
        let contract = json!({"external_candidate":requirement,
            "auxiliary_configs":[], "runtime_configs":[]});
        let exact_program = json!({"resolution_output":{"composed":{"derived":{
            (EXTERNAL_PRODUCT_SELECTIONS_DERIVED_KEY):selections.semantic_identity_value().unwrap()
        }}}});
        let artifact = crate::external_content::products::qualification::tests::artifact_identity();
        let capsule = AdmittedPersistentSessionCapsule {
            schema: PERSISTENT_SESSION_CAPSULE_SCHEMA_VERSION,
            kind: PERSISTENT_SESSION_CAPSULE_KIND.into(),
            external_candidate: Some(requirement.resolve(Some(&selections)).unwrap()),
            exact_program_hash: canonical_value_digest(&exact_program).unwrap(),
            exact_program,
            retained_product_selections: Some(selections),
            lifecycle: PersistentSessionLifecycleContract {
                max_processes: 1,
                max_inflight_per_process: 1,
                max_address_space_bytes: 64 * 1024 * 1024,
                max_cpu_seconds: 1,
                real_uid_process_limit: 1,
                ready_timeout_ms: 1,
                request_timeout_ms: 1,
                idle_timeout_ms: 1,
            },
            wire: PersistentSessionWireContract {
                channel_env: "RYEOS_SESSION_FD".into(),
                wire_protocol: "ryeos.structured-session".into(),
                wire_version: 2,
                max_frame_bytes: 1024,
            },
            executor_ref: artifact.executor_ref().into(),
            artifact_identity: artifact,
            execution_closure: AdmittedExecutionClosure::DirectItemExecutor {
                execution_plan: json!({}),
                protocol_descriptor_document: "fixture".into(),
                command: AdmittedDirectCommandClosure::ContentAddressed {
                    executable_blob_hash: "6".repeat(64),
                    execution_path: admitted_direct_command_execution_path(
                        &"6".repeat(64),
                        std::path::Path::new("fixture"),
                    )
                    .unwrap(),
                },
                admitted_project_root: None,
            },
            execution_realization_hash: "8".repeat(64),
            source_binding_hash: None,
            structured_session_profile: Some(AdmittedStructuredSessionProfile {
                profile_hash: canonical_value_digest(&contract).unwrap(),
                contract,
                schema_hashes: BTreeMap::from([("response.json".into(), "9".repeat(64))]),
                baseline_source: "baseline.conf".into(),
                baseline_destination: "config.conf".into(),
                auxiliary_configs: vec![],
                runtime_configs: vec![],
            }),
            executable_search: vec![],
            process_environment: BTreeMap::new(),
            runtime_ref: "runtime:fixtures/qualified".into(),
        };
        capsule.validate().unwrap();
        let wire = capsule.to_value().unwrap();
        assert_eq!(
            AdmittedPersistentSessionCapsule::from_current_value(&wire).unwrap(),
            capsule
        );
        let mut changed = capsule.clone();
        changed.external_candidate = None;
        assert!(changed.validate().is_err());
        let mut changed = capsule.clone();
        let profile = changed.structured_session_profile.as_mut().unwrap();
        profile.contract["external_candidate"]["runtime_product_declaration_id"] = json!("runtime");
        profile.profile_hash = canonical_value_digest(&profile.contract).unwrap();
        assert!(changed.validate().is_err());
        // Keep the semantic program consistent with a changed retained proof;
        // the independent external projection must still refuse that change.
        let mut changed = capsule.clone();
        let mut selections = changed
            .retained_product_selections
            .take()
            .unwrap()
            .into_inner();
        selections
            .get_mut("auxiliary")
            .unwrap()
            .relationship_raw_content_digest = "0".repeat(64);
        let selections = ResolvedExternalProductSelections::new(selections).unwrap();
        changed.exact_program["resolution_output"]["composed"]["derived"]
            [EXTERNAL_PRODUCT_SELECTIONS_DERIVED_KEY] =
            selections.semantic_identity_value().unwrap();
        changed.exact_program_hash = canonical_value_digest(&changed.exact_program).unwrap();
        changed.retained_product_selections = Some(selections);
        assert!(
            changed
                .validate()
                .unwrap_err()
                .to_string()
                .contains("external candidate program")
        );
    }

    #[test]
    fn external_requirement_refuses_ambient_or_occurrence_authority() {
        for (field, value) in [
            ("schema", serde_json::json!(2)),
            ("protocol", serde_json::json!("local_fallback")),
            (
                "runtime_product_declaration_id",
                serde_json::json!("../runtime"),
            ),
        ] {
            let mut wire = serde_json::to_value(requirement()).unwrap();
            wire[field] = value;
            let changed: ExternalCandidateRequirement = serde_json::from_value(wire).unwrap();
            assert!(changed.validate().is_err());
        }
        for field in [
            "url",
            "credential",
            "occurrence_id",
            "execution_binding_hash",
            "capsule_hash",
        ] {
            let mut wire = serde_json::to_value(requirement()).unwrap();
            wire[field] = serde_json::json!("unexpected");
            assert!(serde_json::from_value::<ExternalCandidateRequirement>(wire).is_err());
        }
    }
}
