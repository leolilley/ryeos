//! Closed identity for a qualification-only hosted structured session.
//!
//! This value has no ordinary Worker admission, provider contact, or export
//! authority. Its constructor compares an authenticated product witness and
//! retained Worker profile with the already sealed qualification purpose.

use anyhow::{Context as _, Result, ensure};
use ryeos_external_execution_contract::{
    ExternalGuestInputProjection, GuestMountAccess, GuestMountContentAuthority, GuestMountKind,
    GuestProductManifestKind,
};
use serde::{Deserialize, Serialize};

use crate::external_content::products::ProductShape;
use crate::external_content::products::publication::VerifiedProductWitness;
use crate::external_content::products::qualification::ProductQualificationConsumerRuntimeMemberIdentity;
use crate::external_content::qualification_purpose::QualificationLaunchPurpose;
use crate::objects::{
    AdmittedStructuredSessionProfile, EXTERNAL_CONTENT_MANIFEST_KIND,
    EXTERNAL_LARGE_CONTENT_MANIFEST_KIND, ExternalContentKind, ExternalContentRealizationSet,
    canonical_value_digest, validate_canonical_project_relative_path,
};

use super::admission::{
    ExternalCandidateQualificationUse, ExternalCandidateRequirement,
    ExternalCandidateRuntimeAuthority,
};

pub const QUALIFICATION_SESSION_PROGRAM_SCHEMA: u32 = 1;

/// One logical Worker qualification contact per accepted verifier root,
/// restored qualification operation and signed scenario. The key deliberately
/// excludes retry deadlines, transport representation, binding generation and
/// the member/program bytes: changing those cannot mint another allocation
/// opportunity for the same accepted operation.
pub fn qualification_candidate_placement_key(
    coordinate: &ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeVerificationCoordinate,
    qualification_operation_id: &str,
) -> Result<String> {
    coordinate.validate()?;
    super::hash(qualification_operation_id)?;
    let digest = canonical_value_digest(&serde_json::json!({
        "domain": "ryeos.external-qualification-candidate-placement.v1",
        "accepted_root_id": &coordinate.accepted_root_id,
        "qualification_operation_id": qualification_operation_id,
        "scenario_id": &coordinate.scenario_id,
    }))?;
    Ok(format!("Q-{digest}"))
}

/// A controller-owned projection of one exact accepted consumer use. It is
/// deliberately distinct from `AdmittedExternalCandidateProgram`, which
/// requires a prior runtime qualification and candidate-export authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmittedExternalQualificationSessionProgram {
    pub schema: u32,
    pub qualification_purpose_digest: String,
    pub consumer_content_digest: String,
    pub requirement: ExternalCandidateRequirement,
    pub qualification_use: ExternalCandidateQualificationUse,
    pub runtime_manifest_kind: String,
    pub runtime_content_kind: ExternalContentKind,
    pub runtime_manifest_hash: String,
    pub runtime_member: ProductQualificationConsumerRuntimeMemberIdentity,
    pub runtime_recipe_digest: String,
}

impl AdmittedExternalQualificationSessionProgram {
    /// Caller must authenticate the born root, the retained profile source,
    /// and the product witness before supplying these values. This pure
    /// projection cannot turn matching historical bytes into contact rights.
    pub fn from_authenticated_inputs(
        purpose: &QualificationLaunchPurpose,
        profile: &AdmittedStructuredSessionProfile,
        product: &VerifiedProductWitness,
    ) -> Result<Self> {
        purpose.validate()?;
        profile.validate()?;
        product.evidence.validate()?;
        let (witness_hash, _, _) = purpose.subject.captured_product()?;
        let content = purpose
            .consumer_content
            .as_ref()
            .context("qualification-only session has no accepted consumer content")?;
        let qualification_use = content
            .qualification_use
            .as_ref()
            .context("qualification-only session has no admitted use")?;
        qualification_use
            .require_qualified_use(&purpose.policy_source.policy.verifier_parameters)?;
        let requirement = profile
            .external_candidate_requirement()?
            .context("qualification-only Worker has no external candidate requirement")?;
        ensure!(
            requirement.runtime_authority == ExternalCandidateRuntimeAuthority::CapturedProduct,
            "qualification-only product session requires captured-product authority"
        );
        ensure!(
            witness_hash == product.attestation_hash
                && purpose.subject_manifest_hash == product.evidence.manifest_hash
                && product.evidence.declaration.shape == ProductShape::Tree
                && content.runtime_realization.kind == ExternalContentKind::Tree
                && content.runtime_realization.manifest_hash == product.evidence.manifest_hash
                && content.runtime_realization.entry_count == product.evidence.entry_count
                && content.runtime_realization.total_bytes == product.evidence.total_bytes,
            "qualification-only session differs from authenticated product witness"
        );
        let realizations = ExternalContentRealizationSet::new(
            content
                .worker_literals
                .iter()
                .chain(content.environment_realizations.iter())
                .chain(std::iter::once(&content.runtime_realization))
                .cloned()
                .collect(),
        )?;
        let reproduced = ExternalCandidateQualificationUse::from_admitted_inputs(
            &requirement,
            profile,
            &content.worker_source,
            &realizations,
            &content.executable_search,
            &content.process_environment,
        )?;
        ensure!(
            reproduced == *qualification_use && profile.profile_hash == content.worker_profile_hash,
            "qualification-only session differs from accepted Worker use"
        );
        let program = Self {
            schema: QUALIFICATION_SESSION_PROGRAM_SCHEMA,
            qualification_purpose_digest: canonical_value_digest(&serde_json::to_value(purpose)?)?,
            consumer_content_digest: canonical_value_digest(&serde_json::to_value(content)?)?,
            requirement: requirement.clone(),
            qualification_use: qualification_use.clone(),
            runtime_manifest_kind: product.evidence.manifest_kind.clone(),
            runtime_content_kind: content.runtime_realization.kind,
            runtime_manifest_hash: product.evidence.manifest_hash.clone(),
            runtime_member: content.runtime_member.clone(),
            runtime_recipe_digest: requirement.runtime_recipe.digest()?,
        };
        program.validate()?;
        program.validate_against_purpose(purpose)?;
        Ok(program)
    }

    /// Rejoin a retained program to the exact CAS-authenticated accepted
    /// purpose before reservation or recovery. This checks data agreement;
    /// the caller still owns root authentication, product member custody and
    /// current provider-contact authority.
    pub fn validate_against_purpose(&self, purpose: &QualificationLaunchPurpose) -> Result<()> {
        self.validate()?;
        purpose.validate()?;
        purpose.subject.captured_product()?;
        let content = purpose
            .consumer_content
            .as_ref()
            .context("qualification-only accepted purpose has no consumer content")?;
        let admitted_use = content
            .qualification_use
            .as_ref()
            .context("qualification-only accepted purpose has no admitted use")?;
        admitted_use.require_qualified_use(&purpose.policy_source.policy.verifier_parameters)?;
        ensure!(
            self.qualification_purpose_digest
                == canonical_value_digest(&serde_json::to_value(purpose)?)?
                && self.consumer_content_digest
                    == canonical_value_digest(&serde_json::to_value(content)?)?
                && self.qualification_use == *admitted_use
                && self.qualification_use.profile_hash == content.worker_profile_hash
                && self.runtime_manifest_hash == purpose.subject_manifest_hash
                && self.runtime_manifest_hash == content.runtime_realization.manifest_hash
                && self.runtime_member == content.runtime_member,
            "qualification-only program differs from its accepted purpose"
        );
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == QUALIFICATION_SESSION_PROGRAM_SCHEMA,
            "unsupported qualification-only session program"
        );
        self.requirement.validate()?;
        self.qualification_use.validate()?;
        ensure!(
            self.requirement.runtime_authority
                == ExternalCandidateRuntimeAuthority::CapturedProduct
                && self.qualification_use.requirement_digest
                    == self.requirement.qualification_requirement_digest()?,
            "qualification-only program differs from its admitted use"
        );
        ensure!(
            matches!(
                self.runtime_manifest_kind.as_str(),
                EXTERNAL_CONTENT_MANIFEST_KIND | EXTERNAL_LARGE_CONTENT_MANIFEST_KIND
            ) && self.runtime_content_kind == ExternalContentKind::Tree,
            "qualification-only runtime manifest kind is invalid"
        );
        for hash in [
            &self.qualification_purpose_digest,
            &self.consumer_content_digest,
            &self.runtime_manifest_hash,
            &self.runtime_member.executable_sha256,
            &self.runtime_recipe_digest,
        ] {
            super::hash(hash)?;
        }
        validate_canonical_project_relative_path(&self.runtime_member.relative_path)?;
        ensure!(
            self.runtime_member.product_declaration_id
                == self.requirement.runtime_product_declaration_id
                && self.runtime_member.relative_path
                    == self.requirement.runtime_recipe.executable_relative_path
                && self.runtime_recipe_digest == self.requirement.runtime_recipe.digest()?,
            "qualification-only runtime differs from accepted executable recipe"
        );
        Ok(())
    }

    pub fn digest(&self) -> Result<String> {
        self.validate()?;
        canonical_value_digest(&serde_json::json!({
            "domain": "ryeos.external-qualification-session-program.v1",
            "program": self,
        }))
    }

    /// The selected runtime product must be one exact read-only guest mount.
    /// This checks the closed projection; descriptor and manifest bytes are
    /// authenticated separately by the guest-input owner.
    pub fn validate_guest_inputs(&self, inputs: &ExternalGuestInputProjection) -> Result<()> {
        self.validate()?;
        inputs.validate()?;
        let expected_kind = match self.runtime_manifest_kind.as_str() {
            EXTERNAL_CONTENT_MANIFEST_KIND => GuestProductManifestKind::Content,
            EXTERNAL_LARGE_CONTENT_MANIFEST_KIND => GuestProductManifestKind::LargeContent,
            _ => unreachable!("validated qualification manifest kind"),
        };
        ensure!(
            inputs.inputs.iter().any(|input| {
                input.authority_id == self.requirement.runtime_product_declaration_id
                    && input.destination
                        == self.requirement.runtime_recipe.runtime_mount_destination
                    && input.kind == GuestMountKind::Directory
                    && input.access == GuestMountAccess::ReadOnly
                    && matches!(
                        &input.content_authority,
                        GuestMountContentAuthority::ProductManifest {
                            manifest_kind,
                            manifest_hash,
                            ..
                        } if *manifest_kind == expected_kind
                            && manifest_hash == &self.runtime_manifest_hash
                    )
            }),
            "qualification-only guest inputs lost their exact read-only runtime"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeVerificationCoordinate;

    #[test]
    fn one_logical_qualification_contact_survives_evidence_and_binding_changes() {
        let mut coordinate = ConsumerRuntimeVerificationCoordinate {
            schema: 1,
            accepted_root_id: "T-accepted".into(),
            accepted_capsule_hash: "a".repeat(64),
            qualification_purpose_digest: "b".repeat(64),
            scenario_id: "hosted_worker".into(),
            scenario_source_digest: "c".repeat(64),
            subject_digest: "d".repeat(64),
            use_digest: "e".repeat(64),
            prerequisite_measurement_attempt_id: "f".repeat(64),
            prerequisite_measurement_observation_digest: "0".repeat(64),
        };
        let key = qualification_candidate_placement_key(&coordinate, &"1".repeat(64)).unwrap();
        assert!(key.starts_with("Q-") && key.len() == 66);
        coordinate.prerequisite_measurement_observation_digest = "2".repeat(64);
        coordinate.scenario_source_digest = "3".repeat(64);
        assert_eq!(
            key,
            qualification_candidate_placement_key(&coordinate, &"1".repeat(64)).unwrap()
        );
        coordinate.scenario_id = "other_scenario".into();
        assert_ne!(
            key,
            qualification_candidate_placement_key(&coordinate, &"1".repeat(64)).unwrap()
        );
        assert!(qualification_candidate_placement_key(&coordinate, "invalid").is_err());
    }
}
