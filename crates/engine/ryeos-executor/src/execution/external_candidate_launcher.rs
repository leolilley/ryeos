//! Sealed descriptor bootstrap for the dedicated native candidate launcher.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context as _, Result, ensure};
use ryeos_state::external_execution::ExecutionChannelBinding;
use serde::{Deserialize, Serialize};

use super::external_candidate::{NativeCandidateOutput, NativeExternalCandidate};

pub const LAUNCHER_CONTROL_FD: u32 = 40;
pub const LAUNCHER_BOOTSTRAP_FD: u32 = 41;
pub const LAUNCHER_RUNTIME_FD: u32 = 42;
pub const LAUNCHER_PRIVATE_PARENT_FD: u32 = 43;
pub const LAUNCHER_EXECUTABLE_FD: u32 = 44;
pub const LAUNCHER_RUNTIME_MOUNT_FD_BASE: u32 = 64;
pub const MAX_LAUNCHER_RUNTIME_MOUNTS: usize = 32;
pub const MAX_LAUNCHER_BOOTSTRAP_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalCandidateRuntimeMountSpec {
    pub destination: String,
    pub layer: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalCandidateLauncherSpec {
    pub schema: u32,
    pub binding: ExecutionChannelBinding,
    pub executable: String,
    pub argv0: String,
    pub arguments: Vec<String>,
    pub cwd: String,
    pub environment: BTreeMap<String, String>,
    pub runtime_mounts: Vec<ExternalCandidateRuntimeMountSpec>,
    pub max_stdout_bytes: u64,
    pub max_stderr_bytes: u64,
    pub proc_filesystem: ExternalCandidateProcFilesystem,
    pub contain_process_group: bool,
    pub nested_sandbox: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalCandidateProcFilesystem {
    Empty,
    PidNamespace,
    PidNamespaceNested,
}

impl ExternalCandidateLauncherSpec {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.schema == 1, "unsupported external launcher schema");
        self.binding.validate()?;
        ensure!(
            !self.executable.is_empty() && self.executable.len() <= 4096,
            "external launcher executable is invalid"
        );
        ensure!(
            !self.argv0.is_empty() && self.argv0.len() <= 4096,
            "external launcher argv0 is invalid"
        );
        ensure!(
            self.arguments.len() <= 256
                && self.arguments.iter().all(|value| value.len() <= 64 * 1024),
            "external launcher arguments exceed bounds"
        );
        ensure!(
            self.environment.len() <= 256
                && self.environment.iter().all(|(name, value)| {
                    !name.is_empty()
                        && !name.contains('=')
                        && !name.contains('\0')
                        && !value.contains('\0')
                        && name.len() <= 4096
                        && value.len() <= 64 * 1024
                }),
            "external launcher environment exceeds bounds"
        );
        ensure!(
            self.runtime_mounts.len() <= MAX_LAUNCHER_RUNTIME_MOUNTS,
            "external launcher has too many runtime mounts"
        );
        ensure!(
            (1..=64 * 1024 * 1024).contains(&self.max_stdout_bytes)
                && (1..=64 * 1024 * 1024).contains(&self.max_stderr_bytes),
            "external launcher output bounds are invalid"
        );
        require_absolute_normalized(Path::new(&self.executable), "executable")?;
        require_absolute_normalized(Path::new(&self.cwd), "cwd")?;
        ensure!(
            Path::new(&self.cwd).starts_with("/workspace"),
            "external launcher cwd is outside /workspace"
        );
        let mut destinations = std::collections::BTreeSet::new();
        for mount in &self.runtime_mounts {
            let destination = Path::new(&mount.destination);
            require_absolute_normalized(destination, "runtime mount destination")?;
            ensure!(
                !destination.starts_with("/workspace")
                    && !Path::new("/workspace").starts_with(destination),
                "external runtime mount overlaps /workspace"
            );
            ensure!(
                destinations.insert(mount.destination.clone()),
                "external runtime mount destination is duplicated"
            );
        }
        Ok(())
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        Ok(lillux::canonical_json(&serde_json::to_value(self)?)?.into_bytes())
    }

    pub fn digest(&self) -> Result<String> {
        Ok(lillux::sha256_hex(&self.canonical_bytes()?))
    }
}

fn require_absolute_normalized(path: &Path, label: &str) -> Result<()> {
    ensure!(
        path.is_absolute(),
        "external launcher {label} is not absolute"
    );
    ensure!(
        path.components()
            .all(|component| { matches!(component, Component::RootDir | Component::Normal(_)) }),
        "external launcher {label} is not normalized"
    );
    Ok(())
}

pub struct PreparedExternalCandidateLauncherRequest {
    pub request: lillux::SubprocessRequest,
    pub bootstrap_digest: String,
    pub authority: ryeos_state::PinnedStateAuthority,
}

/// Bind exact node-owned descriptors into a credential-free dedicated
/// launcher request. No ambient project or runtime pathname is placed in the
/// child environment.
pub fn prepare_launcher_subprocess_request(
    launcher: &lillux::InheritedDescriptorAuthority,
    runtime: &lillux::PinnedDirectory,
    private_parent: &lillux::PinnedDirectory,
    runtime_mounts: &[lillux::InheritedDescriptorAuthority],
    spec: &ExternalCandidateLauncherSpec,
    timeout_seconds: f64,
) -> Result<PreparedExternalCandidateLauncherRequest> {
    spec.validate()?;
    ensure!(
        runtime_mounts.len() == spec.runtime_mounts.len(),
        "external launcher runtime mount authority count changed"
    );
    ensure!(
        timeout_seconds.is_finite() && timeout_seconds > 0.0,
        "external launcher timeout is invalid"
    );
    let bytes = spec.canonical_bytes()?;
    let bootstrap_digest = lillux::sha256_hex(&bytes);
    let bootstrap = lillux::sealed_memfd(c"ryeos-external-candidate-bootstrap", &bytes)
        .map_err(anyhow::Error::msg)?;
    let authority =
        ryeos_state::PinnedStateAuthority::from_external_candidate_runtime(runtime.try_clone()?)?;
    let runtime = runtime.inherited_descriptor_authority()?;
    let private_parent = private_parent.inherited_descriptor_authority()?;
    let mut request = lillux::SubprocessRequest {
        cmd: String::new(),
        argv0: Some("ryeos-external-candidate-launcher".to_owned()),
        args: vec![],
        cwd: None,
        envs: vec![],
        stdin_data: None,
        timeout: timeout_seconds,
        limits: None,
        inherited_fds: vec![],
        inherited_fd_mappings: vec![],
        supervised_status: None,
    };
    launcher
        .bind_as_subprocess_executable(&mut request, LAUNCHER_EXECUTABLE_FD)
        .map_err(anyhow::Error::msg)?;
    bootstrap
        .bind_to_subprocess_request(&mut request, LAUNCHER_BOOTSTRAP_FD)
        .map_err(anyhow::Error::msg)?;
    runtime
        .bind_to_subprocess_request(&mut request, LAUNCHER_RUNTIME_FD)
        .map_err(anyhow::Error::msg)?;
    private_parent
        .bind_to_subprocess_request(&mut request, LAUNCHER_PRIVATE_PARENT_FD)
        .map_err(anyhow::Error::msg)?;
    for (index, mount) in runtime_mounts.iter().enumerate() {
        let target = LAUNCHER_RUNTIME_MOUNT_FD_BASE
            .checked_add(u32::try_from(index)?)
            .context("external runtime mount descriptor overflow")?;
        mount
            .bind_to_subprocess_request(&mut request, target)
            .map_err(anyhow::Error::msg)?;
    }
    Ok(PreparedExternalCandidateLauncherRequest {
        request,
        bootstrap_digest,
        authority,
    })
}

/// Adopt the exact fixed descriptors and prepare one native candidate. This
/// function is called only by the dedicated launcher executable.
///
/// # Safety
/// The fixed descriptors must have been installed uniquely by Lillux from the
/// exact authorities named by the sealed bootstrap.
#[cfg(unix)]
pub unsafe fn prepare_from_inherited_bootstrap() -> Result<(
    ExternalCandidateLauncherSpec,
    ryeos_state::PinnedStateAuthority,
    NativeExternalCandidate,
    NativeCandidateOutput,
)> {
    let bytes = lillux::read_sealed_inherited_descriptor(
        LAUNCHER_BOOTSTRAP_FD,
        MAX_LAUNCHER_BOOTSTRAP_BYTES,
    )
    .map_err(anyhow::Error::msg)?;
    let spec: ExternalCandidateLauncherSpec = serde_json::from_slice(&bytes)?;
    spec.validate()?;
    ensure!(
        spec.canonical_bytes()? == bytes,
        "external launcher bootstrap is not canonical"
    );
    let runtime = unsafe {
        lillux::PinnedDirectory::take_inherited_directory(
            PathBuf::from("<external-candidate-runtime>"),
            LAUNCHER_RUNTIME_FD,
        )
    }?;
    let private_parent = unsafe {
        lillux::PinnedDirectory::take_inherited_directory(
            PathBuf::from("<external-candidate-private-parent>"),
            LAUNCHER_PRIVATE_PARENT_FD,
        )
    }?;
    let authority = ryeos_state::PinnedStateAuthority::from_external_candidate_runtime(runtime)?;
    let mut mount_authorities = Vec::with_capacity(spec.runtime_mounts.len());
    let mut mounts = Vec::with_capacity(spec.runtime_mounts.len());
    for (index, mount) in spec.runtime_mounts.iter().enumerate() {
        let fd = LAUNCHER_RUNTIME_MOUNT_FD_BASE
            .checked_add(u32::try_from(index)?)
            .context("external runtime mount descriptor overflow")?;
        let inherited = unsafe { lillux::take_inherited_descriptor_authority(fd) }
            .map_err(anyhow::Error::msg)?;
        let source_fd = inherited
            .inherited_descriptor()
            .map_err(anyhow::Error::msg)?;
        ensure!(
            inherited.mount_entry_kind()? != lillux::OpenMountEntryKind::UnixSocket,
            "external runtime mount is not file content"
        );
        mounts.push(lillux::LinuxSandboxMount {
            source_fd,
            destination: PathBuf::from(&mount.destination),
            access: lillux::LinuxSandboxMountAccess::ReadOnly,
            layer: mount.layer,
        });
        mount_authorities.push(inherited);
    }
    let request = lillux::LinuxSandboxRequest {
        executable: PathBuf::from(&spec.executable),
        argv0: spec.argv0.clone().into(),
        arguments: spec.arguments.iter().cloned().map(Into::into).collect(),
        cwd: PathBuf::from(&spec.cwd),
        environment: spec
            .environment
            .iter()
            .map(|(name, value)| (name.clone().into(), value.clone().into()))
            .collect(),
        mounts,
        fixed_parent_views: vec![],
        overlay: None,
        network: lillux::LinuxSandboxNetwork::Isolated,
        private_tmp: true,
        proc_filesystem: match spec.proc_filesystem {
            ExternalCandidateProcFilesystem::Empty => lillux::LinuxSandboxProcFilesystem::Empty,
            ExternalCandidateProcFilesystem::PidNamespace => {
                lillux::LinuxSandboxProcFilesystem::PidNamespace
            }
            ExternalCandidateProcFilesystem::PidNamespaceNested => {
                lillux::LinuxSandboxProcFilesystem::PidNamespaceNested
            }
        },
        minimal_devices: true,
        character_devices: vec![],
        target_channels: vec![],
        lifecycle: lillux::LinuxSandboxLifecycle::Run,
        contain_process_group: spec.contain_process_group,
        nested_sandbox: spec.nested_sandbox,
        aggregate_limits: None,
    };
    let (candidate, output) = NativeExternalCandidate::prepare(
        spec.binding.clone(),
        &authority,
        &private_parent,
        request,
    )?;
    // The registered mount owners intentionally remain live until preparation
    // has cloned them into the held native sandbox process.
    drop(mount_authorities);
    Ok((spec, authority, candidate, output))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine as _, engine::general_purpose::STANDARD};

    fn binding() -> ExecutionChannelBinding {
        let owner = lillux::crypto::generate_signing_key();
        let supervisor = lillux::crypto::generate_signing_key();
        let now = lillux::time::timestamp_millis();
        ExecutionChannelBinding {
            schema: 1,
            placement_thread_id: "T-external-launcher-bootstrap".into(),
            allocation_request_digest: "a".repeat(64),
            occurrence_id: "occurrence-external-launcher-bootstrap".into(),
            admitted_capsule_hash: "b".repeat(64),
            base_snapshot_hash: "c".repeat(64),
            execution_binding_hash: "d".repeat(64),
            supervisor_runtime_hash: "e".repeat(64),
            channel_nonce: "f".repeat(64),
            owner_public_key: STANDARD.encode(owner.verifying_key().as_bytes()),
            supervisor_public_key: STANDARD.encode(supervisor.verifying_key().as_bytes()),
            issued_at_ms: now - 1_000,
            execution_deadline_ms: now + 60_000,
            expires_at_ms: now + 120_000,
            max_frames: 16,
            max_bytes: 1024 * 1024,
        }
    }

    fn spec() -> ExternalCandidateLauncherSpec {
        ExternalCandidateLauncherSpec {
            schema: 1,
            binding: binding(),
            executable: "/runtime/bin/codex".into(),
            argv0: "codex".into(),
            arguments: vec!["exec-server".into()],
            cwd: "/workspace".into(),
            environment: BTreeMap::from([("HOME".into(), "/workspace/.home".into())]),
            runtime_mounts: vec![ExternalCandidateRuntimeMountSpec {
                destination: "/runtime".into(),
                layer: 0,
            }],
            max_stdout_bytes: 1024 * 1024,
            max_stderr_bytes: 1024 * 1024,
            proc_filesystem: ExternalCandidateProcFilesystem::PidNamespace,
            contain_process_group: true,
            nested_sandbox: true,
        }
    }

    #[test]
    fn launcher_spec_refuses_ambiguous_or_workspace_overlapping_paths() {
        let valid = spec();
        valid.validate().unwrap();

        let mut changed = valid.clone();
        changed.cwd = "workspace".into();
        assert!(
            changed
                .validate()
                .unwrap_err()
                .to_string()
                .contains("not absolute")
        );

        let mut changed = valid.clone();
        changed.executable = "/runtime/../bin/codex".into();
        assert!(
            changed
                .validate()
                .unwrap_err()
                .to_string()
                .contains("not normalized")
        );

        let mut changed = valid.clone();
        changed.runtime_mounts[0].destination = "/workspace/runtime".into();
        assert!(
            changed
                .validate()
                .unwrap_err()
                .to_string()
                .contains("overlaps /workspace")
        );

        let mut changed = valid.clone();
        changed
            .runtime_mounts
            .push(changed.runtime_mounts[0].clone());
        assert!(
            changed
                .validate()
                .unwrap_err()
                .to_string()
                .contains("duplicated")
        );

        let mut changed = valid;
        changed.max_stdout_bytes = 0;
        assert!(
            changed
                .validate()
                .unwrap_err()
                .to_string()
                .contains("output bounds")
        );
    }

    #[test]
    fn launcher_spec_decode_refuses_unknown_fields_and_noncanonical_bootstrap() {
        let value = serde_json::to_value(spec()).unwrap();
        let mut object = value.as_object().unwrap().clone();
        object.insert(
            "ambient_project_path".into(),
            serde_json::json!("/host/project"),
        );
        assert!(
            serde_json::from_value::<ExternalCandidateLauncherSpec>(serde_json::Value::Object(
                object
            ))
            .is_err()
        );

        let spec = spec();
        let canonical = spec.canonical_bytes().unwrap();
        assert_eq!(canonical, spec.canonical_bytes().unwrap());
        assert!(!canonical.windows(2).any(|window| window == b"  "));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sealed_request_has_only_fixed_descriptor_authority_and_no_ambient_environment() {
        use std::ffi::OsStr;
        use std::os::unix::fs::PermissionsExt as _;

        let temporary = tempfile::tempdir().unwrap();
        let launcher_path = temporary.path().join("launcher");
        std::fs::write(&launcher_path, b"#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&launcher_path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let runtime_path = temporary.path().join("runtime");
        let private_path = temporary.path().join("private");
        let mount_path = temporary.path().join("mount");
        std::fs::create_dir(&private_path).unwrap();
        std::fs::create_dir(&mount_path).unwrap();
        let state = ryeos_state::StateDb::open(
            &runtime_path,
            std::sync::Arc::new(ryeos_state::TrustStore::new()),
        )
        .unwrap();
        drop(state);

        let root = lillux::PinnedDirectory::open(temporary.path())
            .unwrap()
            .unwrap();
        let launcher = root
            .open_inherited_regular(OsStr::new("launcher"), false)
            .unwrap()
            .unwrap();
        let runtime = lillux::PinnedDirectory::open(&runtime_path)
            .unwrap()
            .unwrap();
        let private = lillux::PinnedDirectory::open(&private_path)
            .unwrap()
            .unwrap();
        let mount = lillux::PinnedDirectory::open(&mount_path)
            .unwrap()
            .unwrap()
            .inherited_descriptor_authority()
            .unwrap();
        let launch_spec = spec();
        let prepared = prepare_launcher_subprocess_request(
            &launcher,
            &runtime,
            &private,
            &[mount],
            &launch_spec,
            30.0,
        )
        .unwrap();
        assert_eq!(prepared.request.cmd, "/proc/self/fd/44");
        assert_eq!(
            prepared.request.argv0.as_deref(),
            Some("ryeos-external-candidate-launcher")
        );
        assert!(prepared.request.envs.is_empty());
        assert!(prepared.request.inherited_fds.is_empty());
        assert_eq!(prepared.request.inherited_fd_mappings.len(), 5);
        assert_eq!(prepared.bootstrap_digest, launch_spec.digest().unwrap());
    }
}
