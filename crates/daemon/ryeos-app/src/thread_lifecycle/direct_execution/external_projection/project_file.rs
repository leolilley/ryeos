//! One sealed evaluator-selected C member; no callback or serialized authority.

use crate::thread_lifecycle::{CandidateOperationPurpose, SealedRootExecutionRequest};
use anyhow::{Context as _, Result, bail, ensure};
use ryeos_engine::contracts::{ExecutionPlan, PlanArgument, PlanExecutionProjectFileInput};
use ryeos_engine::protocols::VerifiedProtocol;
use ryeos_external_execution_contract::{
    ExternalGuestInputProjection, GuestMountContentAuthority, GuestMountInput, GuestMountKind,
    GuestMountRole,
};
use ryeos_state::objects::{AdmittedLaunchCapsule, ExecutionProjectAuthority, ProjectFile};

/// Keeps the exact sealed descriptor and verified C materialization alive.
/// Only trusted preparation can construct this; copied projection facts cannot.
pub struct PreparedExternalDirectProjectFile {
    capsule_hash: String,
    snapshot_hash: String,
    declaration: PlanExecutionProjectFileInput,
    metadata: ProjectFile,
    authority: lillux::InheritedDescriptorAuthority,
    observation: lillux::OpenRegularFileObservation,
    _materialization: ryeos_state::PinnedProjectMaterialization,
}

impl PreparedExternalDirectProjectFile {
    pub fn input(&self) -> Result<GuestMountInput> {
        Ok(GuestMountInput {
            role: GuestMountRole::Configuration,
            authority_id: "execution-project-file".to_owned(),
            descriptor: self
                .authority
                .inherited_descriptor()
                .map_err(anyhow::Error::msg)?,
            destination: self.declaration.destination.clone(),
            kind: GuestMountKind::RegularFile,
            access: self.declaration.access,
            normalized_mode: Some(self.metadata.normalized_mode),
            content_authority: GuestMountContentAuthority::RawFile {
                sha256: self.metadata.blob_hash.clone(),
            },
            bytes: self.metadata.size,
        })
    }

    /// Arc clone of the SAME descriptor, not a reopened path or duplicated FD.
    pub fn input_authority(&self) -> lillux::InheritedDescriptorAuthority {
        self.authority.clone()
    }

    pub(super) fn require_capsule(&self, capsule: &AdmittedLaunchCapsule) -> Result<()> {
        ensure!(
            self.capsule_hash == capsule.content_hash()?
                && self.snapshot_hash == candidate_snapshot(capsule)?,
            "execution project file proof differs from born capsule or C snapshot"
        );
        Ok(())
    }

    pub(super) fn require_input(
        &self,
        declaration: &PlanExecutionProjectFileInput,
        input: &GuestMountInput,
    ) -> Result<()> {
        declaration.validate()?;
        self.metadata.validate()?;
        ensure!(
            &self.declaration == declaration
                && self.metadata.size <= declaration.max_bytes
                && self.metadata.normalized_mode == declaration.normalized_mode,
            "execution project file proof differs from signed declaration"
        );
        ensure!(
            *input == self.input()?,
            "execution project file mount changed its exact descriptor or facts"
        );
        ensure!(
            self.observation.permission_mode()? == declaration.normalized_mode
                && self.observation.size() == self.metadata.size
                && self
                    .authority
                    .digest_regular_file_stable_exact(&self.observation)?
                    == self.metadata.blob_hash,
            "execution project file sealed descriptor changed content or observation"
        );
        Ok(())
    }
}

fn candidate_snapshot(capsule: &AdmittedLaunchCapsule) -> Result<&str> {
    let sealed = SealedRootExecutionRequest::decode_from_admitted_capsule(capsule)?;
    let evaluation = sealed
        .candidate_evaluation_authority()
        .context("execution project file requires sealed Evaluate authority")?;
    evaluation.validate()?;
    ensure!(
        evaluation.purpose == CandidateOperationPurpose::Evaluate,
        "execution project file is restricted to Evaluate"
    );
    let ExecutionProjectAuthority::PinnedGeneration { snapshot_hash, .. } =
        &capsule.project_authority
    else {
        bail!("execution project file has no pinned C snapshot");
    };
    ensure!(
        evaluation.candidate_snapshot_hash == *snapshot_hash,
        "execution project file C snapshot differs from sealed evaluation"
    );
    Ok(snapshot_hash)
}

pub(super) fn declaration(plan: &ExecutionPlan) -> Result<Option<PlanExecutionProjectFileInput>> {
    plan.validate_endpoint_for_sealing()?;
    let spec = super::ordinary_direct_subprocess(plan)?;
    let selected = spec
        .args
        .iter()
        .filter_map(|arg| match arg {
            PlanArgument::AdmittedExecutionProjectFile { declaration } => Some(declaration),
            _ => None,
        })
        .collect::<Vec<_>>();
    match selected.as_slice() {
        [] => Ok(None),
        [selected] => {
            selected.validate()?;
            Ok(Some((*selected).clone()))
        }
        _ => bail!("execution project file declaration repeats"),
    }
}

/// Extract only from retained hashed plan/protocol, and capture only verified C CAS bytes.
/// This does not admit an allocation, expose a live checkout or create a witness.
pub fn prepare_external_direct_project_file(
    capsule: &AdmittedLaunchCapsule,
    protocol: &VerifiedProtocol,
    materialization: Option<&ryeos_state::PinnedProjectMaterialization>,
) -> Result<Option<PreparedExternalDirectProjectFile>> {
    capsule.validate()?;
    super::validate_external_direct_project_authority(&capsule.project_authority)?;
    let (plan, _) = super::retained_external_direct_plan(
        &capsule.execution_closure,
        &capsule.artifact_identity,
        protocol,
    )?;
    let Some(declaration) = declaration(&plan)? else {
        return Ok(None);
    };
    let snapshot_hash = candidate_snapshot(capsule)?;
    let materialization =
        materialization.context("execution project file has no verified C materialization")?;
    ensure!(
        materialization.snapshot_hash() == snapshot_hash,
        "execution project file materialization changed C snapshot"
    );
    let (metadata, bytes) = materialization
        .authoritative_file_with_metadata_bounded(
            &declaration.relative_path,
            declaration.max_bytes,
        )?
        .context("selected C execution project file is absent")?;
    metadata.validate()?;
    ensure!(
        metadata.normalized_mode == declaration.normalized_mode
            && metadata.size <= declaration.max_bytes
            && metadata.size == bytes.len() as u64
            && lillux::sha256_hex(&bytes) == metadata.blob_hash,
        "selected C execution project file mode/size/hash differs from signed input"
    );
    let authority = lillux::sealed_normalized_data_memfd(
        c"ryeos-execution-project-file",
        &bytes,
        declaration.normalized_mode,
    )
    .map_err(anyhow::Error::msg)?;
    let observation = authority.regular_file_observation()?;
    let prepared = PreparedExternalDirectProjectFile {
        capsule_hash: capsule.content_hash()?,
        snapshot_hash: snapshot_hash.to_owned(),
        declaration,
        metadata,
        authority,
        observation,
        _materialization: materialization.clone(),
    };
    prepared.require_input(&prepared.declaration, &prepared.input()?)?;
    Ok(Some(prepared))
}

pub(super) fn require_inputs(
    plan: &ExecutionPlan,
    inputs: &ExternalGuestInputProjection,
    proof: Option<&PreparedExternalDirectProjectFile>,
) -> Result<Option<String>> {
    let selected = declaration(plan)?;
    let configuration = inputs
        .inputs
        .iter()
        .filter(|input| input.role == GuestMountRole::Configuration)
        .collect::<Vec<_>>();
    match (selected, proof, configuration.as_slice()) {
        (None, None, []) => Ok(None),
        (Some(declaration), Some(proof), [input]) => {
            proof.require_input(&declaration, input)?;
            Ok(Some(declaration.destination))
        }
        _ => bail!("execution project file declaration/proof/configuration presence differs"),
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn compiler_project_file_join_refuses_copied_facts_or_source_impersonation() {
        // Component proof checker only. The real born capsule/claim is a
        // separate outer check; this fixture grants no launch authority.
        let root = tempfile::tempdir().unwrap();
        let bytes = b"exact C member";
        std::fs::write(root.path().join("member.txt"), bytes).unwrap();
        let metadata = ProjectFile {
            blob_hash: lillux::sha256_hex(bytes),
            size: bytes.len() as u64,
            normalized_mode: ProjectFile::REGULAR_MODE,
        };
        let materialization =
            ryeos_state::PinnedProjectMaterialization::from_observed_tree_for_test(
                "a".repeat(64),
                root.path(),
                BTreeMap::from([("member.txt".to_owned(), metadata.clone())]),
            )
            .unwrap();
        let authority =
            lillux::sealed_normalized_data_memfd(c"ryeos-project-file-control", bytes, 0o644)
                .unwrap();
        let observation = authority.regular_file_observation().unwrap();
        let declaration = PlanExecutionProjectFileInput {
            relative_path: "member.txt".to_owned(),
            destination: "/candidate-input".to_owned(),
            max_bytes: 4096,
            normalized_mode: 0o644,
            access: ryeos_external_execution_contract::GuestMountAccess::ReadOnly,
        };
        let proof = PreparedExternalDirectProjectFile {
            capsule_hash: "b".repeat(64),
            snapshot_hash: "a".repeat(64),
            declaration: declaration.clone(),
            metadata,
            authority,
            observation,
            _materialization: materialization,
        };
        let (mut plan, mut inputs) = super::super::tests::project_file_plan_fixture();
        let ryeos_engine::contracts::PlanNode::DispatchSubprocess { spec, .. } = &mut plan.nodes[0]
        else {
            unreachable!();
        };
        spec.args = vec![PlanArgument::AdmittedExecutionProjectFile {
            declaration: declaration.clone(),
        }];
        let input = proof.input().unwrap();
        // Storage fixture descriptors are inert numbers. Keep them distinct
        // from this actual sealed descriptor for the structural validator.
        inputs.base_snapshot.descriptor = input.descriptor + 10;
        inputs.inputs[0].descriptor = input.descriptor + 11;
        let GuestMountContentAuthority::ProductManifest {
            manifest_descriptor,
            ..
        } = &mut inputs.inputs[0].content_authority
        else {
            unreachable!();
        };
        *manifest_descriptor = input.descriptor + 12;
        inputs.inputs.push(input.clone());
        inputs.validate().unwrap();
        assert_eq!(
            require_inputs(&plan, &inputs, Some(&proof)).unwrap(),
            Some(declaration.destination.clone())
        );
        assert!(require_inputs(&plan, &inputs, None).is_err());
        for field in [
            "descriptor",
            "hash",
            "size",
            "mode",
            "destination",
            "access",
            "role",
        ] {
            let mut changed = inputs.clone();
            let file = changed.inputs.last_mut().unwrap();
            match field {
                "descriptor" => {
                    // Even identical bytes in a SECOND sealed file cannot
                    // replace the original proof-owned descriptor.
                    let other =
                        lillux::sealed_memfd(c"ryeos-project-file-substitution", bytes).unwrap();
                    file.descriptor = other.inherited_descriptor().unwrap();
                    assert!(require_inputs(&plan, &changed, Some(&proof)).is_err());
                    continue;
                }
                "hash" => {
                    file.content_authority = GuestMountContentAuthority::RawFile {
                        sha256: "c".repeat(64),
                    }
                }
                "size" => file.bytes += 1,
                "mode" => file.normalized_mode = Some(0o755),
                "destination" => file.destination = "/different-input".to_owned(),
                "access" => {
                    file.access =
                        ryeos_external_execution_contract::GuestMountAccess::PrivateWritable
                }
                "role" => file.role = GuestMountRole::Source,
                _ => unreachable!(),
            }
            assert!(
                require_inputs(&plan, &changed, Some(&proof)).is_err(),
                "{field}"
            );
        }
        let mut extra = inputs.clone();
        extra.inputs.push(input);
        assert!(require_inputs(&plan, &extra, Some(&proof)).is_err());
        let mut unselected = plan.clone();
        let ryeos_engine::contracts::PlanNode::DispatchSubprocess { spec, .. } =
            &mut unselected.nodes[0]
        else {
            unreachable!();
        };
        spec.args = vec![PlanArgument::literal("/candidate-input")];
        assert!(require_inputs(&unselected, &inputs, Some(&proof)).is_err());
        assert!(require_inputs(&unselected, &inputs, None).is_err());
        let mut substituted = declaration;
        substituted.relative_path = "other.txt".to_owned();
        let ryeos_engine::contracts::PlanNode::DispatchSubprocess { spec, .. } = &mut plan.nodes[0]
        else {
            unreachable!();
        };
        spec.args = vec![PlanArgument::AdmittedExecutionProjectFile {
            declaration: substituted,
        }];
        assert!(require_inputs(&plan, &inputs, Some(&proof)).is_err());
    }
}
