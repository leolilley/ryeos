//! Fixed-descriptor executable boundary for the protected supervisor.

use std::ffi::OsStr;
use std::path::PathBuf;

use anyhow::{Context as _, Result, ensure};
use ryeos_external_execution::guest_content::{
    BoundGuestMountedContent, open_verified_mounted_guest_content,
};
use ryeos_external_execution::guest_installation::{
    MAX_GUEST_SUPERVISOR_LAUNCH_RECORD_BYTES, decode_mounted_supervisor_handoff,
};
use ryeos_external_execution_contract::guest_supervisor_descriptors::{
    SUPERVISOR_BOOTSTRAP_FD, SUPERVISOR_CANDIDATE_RUNTIME_FD, SUPERVISOR_LAUNCH_INTENT_FD,
    SUPERVISOR_LAUNCHER_FD, SUPERVISOR_PRIVATE_PARENT_FD, SUPERVISOR_STAGE_MOUNT_DESTINATION,
    SUPERVISOR_STATE_ROOT_FD, SUPERVISOR_WORKSPACE_OUTPUT_FD,
    fixed_guest_supervisor_descriptor_plan,
};
use ryeos_external_execution_contract::{ExternalGuestInputProjection, GuestMountContentAuthority};
use ryeos_state::external_execution::transport::{
    ExternalSupervisorBootstrap, MAX_EXTERNAL_SUPERVISOR_BOOTSTRAP_BYTES,
};

use crate::runtime::{
    ExternalCandidateSupervisorInputs, ExternalCandidateSupervisorOutcome,
    run_external_candidate_supervisor,
};

/// Adopt source-resident content after the trusted outer owner has proved the
/// exact staged child is the sole read-only mount in this supervisor view.
/// This helper checks content and indexed scratch, but it cannot attest the
/// applied mount, occurrence writer fence, or whole-scope settlement itself.
/// It does not launch a candidate or contact the controller.
pub fn adopt_mounted_supervisor_content(
    staged_root: &lillux::PinnedDirectory,
    expected_stage: &lillux::PinnedDirectoryIdentity,
    private_parent: &lillux::PinnedDirectory,
    retained: &ExternalGuestInputProjection,
    expected_launcher_sha256: &str,
) -> Result<(
    lillux::InheritedDescriptorAuthority,
    BoundGuestMountedContent,
)> {
    retained.validate()?;
    staged_root.require_owner_private_directory()?;
    ensure!(
        staged_root.identity()? == *expected_stage,
        "mounted guest stage differs from retained generation"
    );
    private_parent.require_owner_private_directory()?;
    staged_root.require_disjoint_directory_tree(private_parent)?;
    let launcher = staged_root
        .open_inherited_regular(OsStr::new("launcher"), false)?
        .context("mounted guest launcher is absent")?;
    launcher.require_owned_executable()?;
    let observation = launcher.regular_file_observation()?;
    ensure!(
        launcher.digest_regular_file_stable_exact(&observation)? == expected_launcher_sha256,
        "mounted guest launcher changed admitted bytes"
    );
    let opened = open_verified_mounted_guest_content(staged_root, retained)?;
    let mut scratch = Vec::new();
    for (index, input) in retained.inputs.iter().enumerate() {
        if matches!(
            input.content_authority,
            GuestMountContentAuthority::PrivateScratch { .. }
        ) {
            let name = format!("guest-scratch-{index:02}");
            let directory = private_parent
                .open_child_directory(OsStr::new(&name))?
                .context("mounted supervisor private scratch is absent")?;
            scratch.push((index, directory.inherited_descriptor_authority()?));
        }
    }
    Ok((launcher, opened.bind_execution_inputs(retained, scratch)?))
}

/// Join the sealed post-import record, bootstrap, pinned private roots and
/// opened mounted source into the supervisor's existing runtime input type.
/// The outer owner must separately prove the exact applied read-only stage
/// mount, one-way release and continuous writer exclusion before invoking the
/// shipped entrypoint; this helper cannot manufacture those facts.
pub fn adopt_mounted_supervisor_inputs(
    bootstrap: ExternalSupervisorBootstrap,
    sealed_launch_record: &[u8],
    staged_root: &lillux::PinnedDirectory,
    state_root: lillux::PinnedDirectory,
    candidate_runtime: lillux::PinnedDirectory,
    candidate_private_parent: lillux::PinnedDirectory,
) -> Result<ExternalCandidateSupervisorInputs> {
    let handoff = decode_mounted_supervisor_handoff(
        sealed_launch_record,
        &bootstrap,
        &state_root,
        &candidate_runtime,
        &candidate_private_parent,
    )?;
    let (launcher, content) = adopt_mounted_supervisor_content(
        staged_root,
        &handoff.stage_directory_identity(),
        &candidate_private_parent,
        &bootstrap.guest_inputs,
        handoff.launcher_sha256(),
    )?;
    let (execution_guest_inputs, workspace_outputs, runtime_mounts, content_records) =
        content.into_parts();
    Ok(ExternalCandidateSupervisorInputs {
        bootstrap,
        execution_guest_inputs,
        state_root,
        candidate_runtime,
        candidate_private_parent,
        launcher,
        workspace_outputs,
        runtime_mounts,
        content_records,
    })
}

/// Sole shipped mounted-source entrypoint. The trusted outer owner must have
/// selected the exact sealed-source child as the read-only stage mount and
/// applied the schema-2 control-channel profile before target release.
pub fn run_from_mounted_inherited() -> Result<ExternalCandidateSupervisorOutcome> {
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
    let launch_record = lillux::read_sealed_inherited_descriptor(
        SUPERVISOR_LAUNCH_INTENT_FD,
        MAX_GUEST_SUPERVISOR_LAUNCH_RECORD_BYTES,
    )
    .map_err(anyhow::Error::msg)?;
    // SAFETY: the held outer owner maps only the four non-source controls at
    // their fixed coordinates. Each directory descriptor is adopted once.
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
    let staged_root =
        lillux::PinnedDirectory::open(std::path::Path::new(SUPERVISOR_STAGE_MOUNT_DESTINATION))?
            .context("mounted guest stage is absent")?;
    let inputs = adopt_mounted_supervisor_inputs(
        bootstrap,
        &launch_record,
        &staged_root,
        state_root,
        candidate_runtime,
        candidate_private_parent,
    )?;
    run_external_candidate_supervisor(inputs)
}

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
    use std::io::Write as _;

    use ryeos_external_execution_contract::guest_supervisor_descriptors::{
        SUPERVISOR_CONSUMED_BASE_SNAPSHOT_FD, SUPERVISOR_CONTENT_RECORD_FD_BASE,
        SUPERVISOR_RUNTIME_MOUNT_FD_BASE, rebind_fixed_guest_descriptors,
    };
    use ryeos_external_execution_contract::{
        ExternalGuestInputProjection, GuestBaseSnapshotInput, GuestMountAccess,
        GuestMountContentAuthority, GuestMountInput, GuestMountKind, GuestMountRole,
        GuestProductManifestKind,
    };

    use super::*;

    #[test]
    fn mounted_content_adoption_binds_launcher_config_and_indexed_scratch() {
        let staged_dir = tempfile::tempdir().unwrap();
        let staged = lillux::PinnedDirectory::open(staged_dir.path())
            .unwrap()
            .unwrap();
        staged.tighten_owner_private_directory().unwrap();
        let launcher_bytes = b"exact launcher fixture";
        let mut launcher = staged
            .open_regular_create(OsStr::new("launcher"), true, true, 0o755)
            .unwrap();
        launcher.write_all(launcher_bytes).unwrap();
        launcher.sync_all().unwrap();
        drop(launcher);
        let config_bytes = b"exact configuration";
        let mut config = staged
            .open_regular_create(OsStr::new("input-00"), true, true, 0o644)
            .unwrap();
        config.write_all(config_bytes).unwrap();
        config.sync_all().unwrap();
        drop(config);
        let private_dir = tempfile::tempdir().unwrap();
        let private = lillux::PinnedDirectory::open(private_dir.path())
            .unwrap()
            .unwrap();
        private.tighten_owner_private_directory().unwrap();
        let scratch = private
            .create_child(OsStr::new("guest-scratch-01"), 0o700)
            .unwrap();
        let inputs = ExternalGuestInputProjection {
            schema: ryeos_external_execution_contract::EXTERNAL_GUEST_INPUT_PROJECTION_SCHEMA,
            base_snapshot: GuestBaseSnapshotInput {
                descriptor: 10,
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
                    authority_id: "config".into(),
                    descriptor: 11,
                    destination: "/config/farm".into(),
                    kind: GuestMountKind::RegularFile,
                    access: GuestMountAccess::ReadOnly,
                    normalized_mode: Some(0o644),
                    content_authority: GuestMountContentAuthority::RawFile {
                        sha256: lillux::sha256_hex(config_bytes),
                    },
                    bytes: config_bytes.len() as u64,
                },
                GuestMountInput {
                    role: GuestMountRole::PrivateScratch,
                    authority_id: "scratch".into(),
                    descriptor: 12,
                    destination: "/scratch".into(),
                    kind: GuestMountKind::Directory,
                    access: GuestMountAccess::PrivateWritable,
                    normalized_mode: None,
                    content_authority: GuestMountContentAuthority::PrivateScratch {
                        binding_hash: "c".repeat(64),
                    },
                    bytes: 0,
                },
            ],
            executable_search: vec![],
            environment: BTreeMap::new(),
        };
        inputs.validate().unwrap();
        let launcher_hash = lillux::sha256_hex(launcher_bytes);
        let stage_identity = staged.identity().unwrap();
        let (opened_launcher, bound) = adopt_mounted_supervisor_content(
            &staged,
            &stage_identity,
            &private,
            &inputs,
            &launcher_hash,
        )
        .unwrap();
        assert_eq!(
            opened_launcher
                .digest_regular_file_stable_exact(
                    &opened_launcher.regular_file_observation().unwrap()
                )
                .unwrap(),
            launcher_hash
        );
        let (rebound, outputs, mounts, records) = bound.into_parts();
        assert_eq!(
            rebound.identity_digest().unwrap(),
            inputs.identity_digest().unwrap()
        );
        assert!(outputs.is_none() && records.is_empty());
        assert_eq!(mounts.len(), 2);
        assert_eq!(
            mounts[1].directory_identity().unwrap(),
            scratch.identity().unwrap()
        );
        for (mount, input) in mounts.iter().zip(&rebound.inputs) {
            assert_eq!(mount.inherited_descriptor().unwrap(), input.descriptor);
        }
        assert!(
            adopt_mounted_supervisor_content(
                &staged,
                &stage_identity,
                &private,
                &inputs,
                &"0".repeat(64),
            )
            .is_err()
        );
        let other_stage_dir = tempfile::tempdir().unwrap();
        let other_stage = lillux::PinnedDirectory::open(other_stage_dir.path())
            .unwrap()
            .unwrap();
        other_stage.tighten_owner_private_directory().unwrap();
        assert!(
            adopt_mounted_supervisor_content(
                &staged,
                &other_stage.identity().unwrap(),
                &private,
                &inputs,
                &launcher_hash,
            )
            .is_err()
        );
        let missing_private_dir = tempfile::tempdir().unwrap();
        let missing_private = lillux::PinnedDirectory::open(missing_private_dir.path())
            .unwrap()
            .unwrap();
        missing_private.tighten_owner_private_directory().unwrap();
        assert!(
            adopt_mounted_supervisor_content(
                &staged,
                &stage_identity,
                &missing_private,
                &inputs,
                &launcher_hash,
            )
            .is_err()
        );
        let ambient = scratch.create_child(OsStr::new("ambient"), 0o700).unwrap();
        assert!(
            adopt_mounted_supervisor_content(
                &staged,
                &stage_identity,
                &private,
                &inputs,
                &launcher_hash,
            )
            .is_err()
        );
        assert!(
            scratch
                .remove_empty_child_if_same(OsStr::new("ambient"), &ambient)
                .unwrap()
        );
    }

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
