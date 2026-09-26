//! Fixed-descriptor executable boundary for the protected supervisor.

use std::path::PathBuf;

use anyhow::{Context as _, Result, ensure};
use ryeos_external_execution_contract::guest_supervisor_descriptors::{
    SUPERVISOR_BOOTSTRAP_FD, SUPERVISOR_CANDIDATE_RUNTIME_FD, SUPERVISOR_CONTENT_RECORD_FD_BASE,
    SUPERVISOR_LAUNCHER_FD, SUPERVISOR_PRIVATE_PARENT_FD, SUPERVISOR_RUNTIME_MOUNT_FD_BASE,
    SUPERVISOR_STATE_ROOT_FD, SUPERVISOR_WORKSPACE_OUTPUT_FD,
    fixed_guest_supervisor_descriptor_plan, rebind_fixed_guest_descriptors,
};
use ryeos_state::external_execution::transport::{
    ExternalSupervisorBootstrap, MAX_EXTERNAL_SUPERVISOR_BOOTSTRAP_BYTES,
};

use crate::runtime::{
    ExternalCandidateSupervisorInputs, ExternalCandidateSupervisorOutcome,
    run_external_candidate_supervisor,
};

/// Adopt the exact fixed descriptor contract and run one supervisor.
///
/// Keeping this boundary in the library lets test-scoped executable wrappers
/// traverse the same authority adoption as the shipped binary without copying
/// it or introducing a fixture-only protocol.
pub fn run_from_inherited() -> Result<ExternalCandidateSupervisorOutcome> {
    let bootstrap_bytes = lillux::read_sealed_inherited_descriptor(
        SUPERVISOR_BOOTSTRAP_FD,
        MAX_EXTERNAL_SUPERVISOR_BOOTSTRAP_BYTES,
    )
    .map_err(anyhow::Error::msg)?;
    let bootstrap: ExternalSupervisorBootstrap = serde_json::from_slice(&bootstrap_bytes)
        .context("decode sealed external supervisor bootstrap")?;
    ensure!(
        bootstrap.canonical_bytes()? == bootstrap_bytes,
        "external supervisor bootstrap is not canonical"
    );
    bootstrap.validate()?;
    // SAFETY: the admitted lifecycle adapter uniquely maps each fixed
    // descriptor and this executable adopts every coordinate exactly once.
    let state_root = unsafe {
        lillux::PinnedDirectory::take_inherited_directory(
            PathBuf::from("<external-supervisor-state>"),
            SUPERVISOR_STATE_ROOT_FD,
        )
    }?;
    let candidate_runtime = unsafe {
        lillux::PinnedDirectory::take_inherited_directory(
            PathBuf::from("<external-candidate-runtime>"),
            SUPERVISOR_CANDIDATE_RUNTIME_FD,
        )
    }?;
    let candidate_private_parent = unsafe {
        lillux::PinnedDirectory::take_inherited_directory(
            PathBuf::from("<external-candidate-private-parent>"),
            SUPERVISOR_PRIVATE_PARENT_FD,
        )
    }?;
    let launcher = unsafe { lillux::take_inherited_descriptor_authority(SUPERVISOR_LAUNCHER_FD) }
        .map_err(anyhow::Error::msg)?;
    let workspace_outputs = bootstrap
        .guest_inputs
        .workspace_outputs
        .as_ref()
        .map(|_| {
            // SAFETY: the admitted lifecycle adapter installs this exact
            // one-use authority at the fixed supervisor coordinate.
            unsafe { lillux::take_inherited_descriptor_authority(SUPERVISOR_WORKSPACE_OUTPUT_FD) }
                .map_err(anyhow::Error::msg)
        })
        .transpose()?;
    let plan = fixed_guest_supervisor_descriptor_plan(&bootstrap.guest_inputs)?;
    let mut runtime_mounts = Vec::with_capacity(plan.runtime_mount_descriptors.len());
    for descriptor in plan.runtime_mount_descriptors {
        runtime_mounts.push(
            unsafe { lillux::take_inherited_descriptor_authority(descriptor) }
                .map_err(anyhow::Error::msg)?,
        );
    }
    let mut content_records = Vec::with_capacity(plan.content_record_descriptors.len());
    for descriptor in plan.content_record_descriptors {
        // SAFETY: the admitted adapter installs each content record exactly
        // once in flattened input order (source binding before manifest).
        content_records.push(
            unsafe { lillux::take_inherited_descriptor_authority(descriptor) }
                .map_err(anyhow::Error::msg)?,
        );
    }
    run_external_candidate_supervisor(ExternalCandidateSupervisorInputs {
        bootstrap,
        execution_guest_inputs: plan.execution_inputs,
        state_root,
        candidate_runtime,
        candidate_private_parent,
        launcher,
        workspace_outputs,
        runtime_mounts,
        content_records,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use ryeos_external_execution_contract::guest_supervisor_descriptors::SUPERVISOR_CONSUMED_BASE_SNAPSHOT_FD;
    use ryeos_external_execution_contract::{
        ExternalGuestInputProjection, GuestBaseSnapshotInput, GuestMountAccess,
        GuestMountContentAuthority, GuestMountInput, GuestMountKind, GuestMountRole,
        GuestProductManifestKind,
    };

    use super::*;

    #[test]
    fn two_product_inputs_rebind_without_manifest_descriptor_collision() {
        let mut inputs = ExternalGuestInputProjection {
            schema: ryeos_external_execution_contract::EXTERNAL_GUEST_INPUT_PROJECTION_SCHEMA,
            base_snapshot: GuestBaseSnapshotInput {
                descriptor: SUPERVISOR_CONTENT_RECORD_FD_BASE,
                snapshot_hash: "a".repeat(64),
                closure_digest: "b".repeat(64),
                object_count: 3,
                blob_count: 1,
                total_bytes: 1,
            },
            workspace_outputs: None,
            inputs: [
                ("auxiliary", "/runtime", 126, 98),
                ("provider-runtime", "/ryeos/realizations/provider", 129, 101),
            ]
            .into_iter()
            .map(
                |(authority_id, destination, descriptor, manifest_descriptor)| GuestMountInput {
                    role: GuestMountRole::Product,
                    authority_id: authority_id.into(),
                    descriptor,
                    destination: destination.into(),
                    kind: GuestMountKind::Directory,
                    access: GuestMountAccess::ReadOnly,
                    normalized_mode: None,
                    content_authority: GuestMountContentAuthority::ProductManifest {
                        manifest_kind: GuestProductManifestKind::LargeContent,
                        manifest_hash: "c".repeat(64),
                        manifest_descriptor,
                        manifest_bytes: 1105,
                    },
                    bytes: 1,
                },
            )
            .collect(),
            executable_search: vec!["/runtime/bin".into()],
            environment: BTreeMap::new(),
        };
        inputs.validate().unwrap();
        let identity = inputs.identity_digest().unwrap();
        rebind_fixed_guest_descriptors(&mut inputs).unwrap();
        assert_eq!(inputs.identity_digest().unwrap(), identity);
        assert_eq!(
            inputs.base_snapshot.descriptor,
            SUPERVISOR_CONSUMED_BASE_SNAPSHOT_FD
        );
        assert_eq!(
            inputs.inputs[0].descriptor,
            SUPERVISOR_RUNTIME_MOUNT_FD_BASE
        );
        assert_eq!(
            inputs.inputs[1].descriptor,
            SUPERVISOR_RUNTIME_MOUNT_FD_BASE + 1
        );
        for (index, input) in inputs.inputs.iter().enumerate() {
            let GuestMountContentAuthority::ProductManifest {
                manifest_descriptor,
                ..
            } = &input.content_authority
            else {
                panic!("fixture product lost its manifest authority");
            };
            assert_eq!(
                *manifest_descriptor,
                SUPERVISOR_CONTENT_RECORD_FD_BASE + index as u32
            );
        }
    }

    #[test]
    fn source_records_and_products_rebind_in_flattened_order() {
        let mut inputs = ExternalGuestInputProjection {
            schema: ryeos_external_execution_contract::EXTERNAL_GUEST_INPUT_PROJECTION_SCHEMA,
            base_snapshot: GuestBaseSnapshotInput {
                descriptor: 200,
                snapshot_hash: "a".repeat(64),
                closure_digest: "b".repeat(64),
                object_count: 3,
                blob_count: 1,
                total_bytes: 1,
            },
            workspace_outputs: None,
            inputs: vec![
                GuestMountInput {
                    role: GuestMountRole::Configuration,
                    authority_id: "configuration".into(),
                    descriptor: 201,
                    destination: "/configuration".into(),
                    kind: GuestMountKind::RegularFile,
                    access: GuestMountAccess::ReadOnly,
                    normalized_mode: Some(0o644),
                    bytes: 1,
                    content_authority: GuestMountContentAuthority::RawFile {
                        sha256: "c".repeat(64),
                    },
                },
                GuestMountInput {
                    role: GuestMountRole::Source,
                    authority_id: "source".into(),
                    descriptor: 202,
                    destination: "/admitted-source".into(),
                    kind: GuestMountKind::Directory,
                    access: GuestMountAccess::ReadOnly,
                    normalized_mode: None,
                    bytes: 1,
                    content_authority: GuestMountContentAuthority::SourceClosure {
                        binding_hash: "d".repeat(64),
                        binding_descriptor: 203,
                        binding_bytes: 1024,
                        manifest_hash: "e".repeat(64),
                        manifest_descriptor: 204,
                        manifest_bytes: 256,
                    },
                },
                GuestMountInput {
                    role: GuestMountRole::Product,
                    authority_id: "runtime".into(),
                    descriptor: 205,
                    destination: "/runtime".into(),
                    kind: GuestMountKind::Directory,
                    access: GuestMountAccess::ReadOnly,
                    normalized_mode: None,
                    bytes: 1,
                    content_authority: GuestMountContentAuthority::ProductManifest {
                        manifest_kind: GuestProductManifestKind::Content,
                        manifest_hash: "f".repeat(64),
                        manifest_descriptor: 206,
                        manifest_bytes: 256,
                    },
                },
            ],
            executable_search: vec![],
            environment: BTreeMap::new(),
        };
        let identity = inputs.identity_digest().unwrap();
        rebind_fixed_guest_descriptors(&mut inputs).unwrap();
        assert_eq!(inputs.identity_digest().unwrap(), identity);
        assert_eq!(
            inputs
                .record_descriptors()
                .map(|(fd, _, _)| fd)
                .collect::<Vec<_>>(),
            (SUPERVISOR_CONTENT_RECORD_FD_BASE..SUPERVISOR_CONTENT_RECORD_FD_BASE + 3)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            inputs
                .inputs
                .iter()
                .map(|input| input.descriptor)
                .collect::<Vec<_>>(),
            (SUPERVISOR_RUNTIME_MOUNT_FD_BASE..SUPERVISOR_RUNTIME_MOUNT_FD_BASE + 3)
                .collect::<Vec<_>>()
        );
    }
}
