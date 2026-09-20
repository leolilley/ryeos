//! Sealed descriptor bootstrap for the dedicated native candidate launcher.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context as _, Result, ensure};
use ryeos_state::external_execution::ExecutionChannelBinding;
use ryeos_state::external_execution::admission::{
    AdmittedExternalCandidateProgram, ExternalCandidateProcFilesystem,
};
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
    pub candidate_program: AdmittedExternalCandidateProgram,
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

impl ExternalCandidateLauncherSpec {
    /// Compile the exact profile-owned recipe after channel attachment. The
    /// runtime tree itself is supplied independently as one exact descriptor;
    /// this conversion cannot select an ambient executable or extra mount.
    pub fn from_admitted_program(
        binding: ExecutionChannelBinding,
        program: &AdmittedExternalCandidateProgram,
    ) -> Result<Self> {
        program.validate()?;
        ensure!(
            binding.candidate_program_digest == program.digest()?
                && binding.supervisor_runtime_hash == program.runtime_manifest_hash,
            "external launcher program contradicts its attached channel"
        );
        let recipe = &program.requirement.runtime_recipe;
        let spec = Self {
            schema: 1,
            binding,
            candidate_program: program.clone(),
            executable: recipe.namespace_executable()?,
            argv0: recipe.argv0.clone(),
            arguments: recipe.arguments.clone(),
            cwd: recipe.cwd.clone(),
            environment: recipe.environment.clone(),
            runtime_mounts: vec![ExternalCandidateRuntimeMountSpec {
                destination: recipe.runtime_mount_destination.clone(),
                layer: 0,
            }],
            max_stdout_bytes: recipe.max_stdout_bytes,
            max_stderr_bytes: recipe.max_stderr_bytes,
            proc_filesystem: recipe.proc_filesystem,
            contain_process_group: recipe.contain_process_group,
            nested_sandbox: recipe.nested_sandbox,
        };
        spec.validate()?;
        Ok(spec)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(self.schema == 1, "unsupported external launcher schema");
        self.binding.validate()?;
        self.candidate_program.validate()?;
        ensure!(
            self.binding.candidate_program_digest == self.candidate_program.digest()?
                && self.binding.supervisor_runtime_hash
                    == self.candidate_program.runtime_manifest_hash,
            "external launcher program contradicts its attached channel"
        );
        let recipe = &self.candidate_program.requirement.runtime_recipe;
        ensure!(
            self.executable == recipe.namespace_executable()?
                && self.argv0 == recipe.argv0
                && self.arguments == recipe.arguments
                && self.cwd == recipe.cwd
                && self.environment == recipe.environment
                && self.runtime_mounts
                    == [ExternalCandidateRuntimeMountSpec {
                        destination: recipe.runtime_mount_destination.clone(),
                        layer: 0,
                    }]
                && self.max_stdout_bytes == recipe.max_stdout_bytes
                && self.max_stderr_bytes == recipe.max_stderr_bytes
                && self.proc_filesystem == recipe.proc_filesystem
                && self.contain_process_group == recipe.contain_process_group
                && self.nested_sandbox == recipe.nested_sandbox,
            "external launcher specification changed its admitted runtime recipe"
        );
        ensure!(
            !self.executable.is_empty() && self.executable.len() <= 4096,
            "external launcher executable is invalid"
        );
        ensure!(
            !self.argv0.is_empty() && self.argv0.len() <= 4096 && !self.argv0.contains('\0'),
            "external launcher argv0 is invalid"
        );
        ensure!(
            self.arguments.len() <= 256
                && self
                    .arguments
                    .iter()
                    .all(|value| value.len() <= 64 * 1024 && !value.contains('\0')),
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
        ensure!(
            lillux::canonical_json(&serde_json::to_value(self)?)?.len()
                <= MAX_LAUNCHER_BOOTSTRAP_BYTES,
            "external launcher bootstrap exceeds its encoded byte bound"
        );
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
            .all(|component| { matches!(component, Component::RootDir | Component::Normal(_)) })
            && path.components().collect::<PathBuf>().as_os_str() == path.as_os_str()
            && path.as_os_str().as_encoded_bytes().len() <= 4096
            && !path.as_os_str().as_encoded_bytes().contains(&0),
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
    use ryeos_state::external_execution::admission::{
        ExternalCandidateRequirement, ExternalCandidateRuntimeRecipe,
        MAX_EXTERNAL_RUNTIME_RECIPE_BYTES, PROTOCOL,
    };

    fn program() -> AdmittedExternalCandidateProgram {
        let runtime_recipe = ExternalCandidateRuntimeRecipe {
            schema: 1,
            runtime_mount_destination: "/runtime".into(),
            executable_relative_path: "bin/codex".into(),
            argv0: "codex".into(),
            arguments: vec!["exec-server".into()],
            cwd: "/workspace".into(),
            environment: BTreeMap::from([("HOME".into(), "/workspace/.home".into())]),
            max_stdout_bytes: 1024 * 1024,
            max_stderr_bytes: 1024 * 1024,
            proc_filesystem: ExternalCandidateProcFilesystem::PidNamespaceNested,
            contain_process_group: true,
            nested_sandbox: true,
        };
        let runtime_recipe_digest = runtime_recipe.digest().unwrap();
        AdmittedExternalCandidateProgram {
            requirement: ExternalCandidateRequirement {
                schema: 3,
                protocol: PROTOCOL.into(),
                connector_protocol:
                    ryeos_state::external_execution::admission::CONNECTOR_PROTOCOL.into(),
                execution_route: ryeos_state::external_execution::admission::ExternalCandidateExecutionRoute::ConnectorOnly,
                runtime_product_declaration_id: "runtime".into(),
                runtime_recipe,
            },
            runtime_manifest_hash: "e".repeat(64),
            runtime_witness_hash: "1".repeat(64),
            qualification_attestation_hash: "2".repeat(64),
            selection_identity_digest: "3".repeat(64),
            runtime_recipe_digest,
        }
    }

    fn binding() -> ExecutionChannelBinding {
        let owner = lillux::crypto::generate_signing_key();
        let supervisor = lillux::crypto::generate_signing_key();
        let now = lillux::time::timestamp_millis();
        let program = program();
        ExecutionChannelBinding {
            schema: 3,
            placement_thread_id: "T-external-launcher-bootstrap".into(),
            allocation_request_digest: "a".repeat(64),
            occurrence_id: "occurrence-external-launcher-bootstrap".into(),
            admitted_capsule_hash: "b".repeat(64),
            base_snapshot_hash: "c".repeat(64),
            execution_binding_hash: "d".repeat(64),
            supervisor_runtime_hash: "e".repeat(64),
            candidate_program_digest: program.digest().unwrap(),
            channel_nonce: "f".repeat(64),
            owner_public_key: STANDARD.encode(owner.verifying_key().as_bytes()),
            supervisor_public_key: STANDARD.encode(supervisor.verifying_key().as_bytes()),
            issued_at_ms: now - 1_000,
            execution_deadline_ms: now + 60_000,
            expires_at_ms: now + 120_000,
            candidate_export_max_bytes: 512 * 1024,
            max_frames: 16,
            max_bytes: 1024 * 1024,
        }
    }

    fn spec() -> ExternalCandidateLauncherSpec {
        ExternalCandidateLauncherSpec {
            schema: 1,
            binding: binding(),
            candidate_program: program(),
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
            proc_filesystem: ExternalCandidateProcFilesystem::PidNamespaceNested,
            contain_process_group: true,
            nested_sandbox: true,
        }
    }

    #[test]
    fn launcher_spec_is_the_exact_admitted_program_projection() {
        let program = program();
        let binding = binding();
        let compiled =
            ExternalCandidateLauncherSpec::from_admitted_program(binding.clone(), &program)
                .unwrap();
        let mut expected = spec();
        expected.binding = binding;
        assert_eq!(compiled, expected);

        for mutation in [
            "binding",
            "program",
            "executable",
            "argv0",
            "arguments",
            "cwd",
            "environment",
            "mount_destination",
            "mount_layer",
            "mount_count",
            "stdout",
            "stderr",
            "proc",
            "process_group",
            "nested",
        ] {
            let mut changed = compiled.clone();
            match mutation {
                "binding" => changed.binding.candidate_program_digest = "0".repeat(64),
                "program" => changed
                    .candidate_program
                    .requirement
                    .runtime_recipe
                    .arguments
                    .push("--changed".into()),
                "executable" => changed.executable = "/runtime/bin/other".into(),
                "argv0" => changed.argv0 = "other".into(),
                "arguments" => changed.arguments.push("--changed".into()),
                "cwd" => changed.cwd = "/workspace/other".into(),
                "environment" => {
                    changed.environment.insert("OTHER".into(), "value".into());
                }
                "mount_destination" => {
                    changed.runtime_mounts[0].destination = "/other-runtime".into()
                }
                "mount_layer" => changed.runtime_mounts[0].layer = 1,
                "mount_count" => changed.runtime_mounts.clear(),
                "stdout" => changed.max_stdout_bytes += 1,
                "stderr" => changed.max_stderr_bytes += 1,
                "proc" => changed.proc_filesystem = ExternalCandidateProcFilesystem::Empty,
                "process_group" => changed.contain_process_group = false,
                "nested" => changed.nested_sandbox = false,
                _ => unreachable!(),
            }
            assert!(changed.validate().is_err(), "{mutation}");
        }
    }

    #[test]
    fn admitted_recipe_ceiling_is_representable_in_launcher_bootstrap() {
        fn compile_with_argument_bytes(total: usize) -> Result<ExternalCandidateLauncherSpec> {
            let mut remaining = total;
            let mut arguments = Vec::new();
            while remaining > 0 {
                let bytes = remaining.min(64 * 1024);
                arguments.push("a".repeat(bytes));
                remaining -= bytes;
            }
            let mut program = program();
            program.requirement.runtime_recipe.arguments = arguments;
            program.runtime_recipe_digest = program.requirement.runtime_recipe.digest()?;
            let mut binding = binding();
            binding.candidate_program_digest = program.digest()?;
            ExternalCandidateLauncherSpec::from_admitted_program(binding, &program)
        }

        let mut accepted = 0_usize;
        let mut refused = MAX_EXTERNAL_RUNTIME_RECIPE_BYTES + 1;
        while accepted + 1 < refused {
            let candidate = accepted + (refused - accepted) / 2;
            if compile_with_argument_bytes(candidate)
                .and_then(|spec| spec.canonical_bytes())
                .is_ok()
            {
                accepted = candidate;
            } else {
                refused = candidate;
            }
        }
        let boundary = compile_with_argument_bytes(accepted).unwrap();
        let recipe_bytes = lillux::canonical_json(
            &serde_json::to_value(&boundary.candidate_program.requirement.runtime_recipe).unwrap(),
        )
        .unwrap()
        .len();
        assert!(recipe_bytes <= MAX_EXTERNAL_RUNTIME_RECIPE_BYTES);
        assert!(MAX_EXTERNAL_RUNTIME_RECIPE_BYTES - recipe_bytes <= 2);
        let encoded = boundary.canonical_bytes().unwrap();
        assert!(encoded.len() <= MAX_LAUNCHER_BOOTSTRAP_BYTES);
        serde_json::from_slice::<ExternalCandidateLauncherSpec>(&encoded)
            .unwrap()
            .validate()
            .unwrap();
        assert!(
            compile_with_argument_bytes(accepted + 1)
                .and_then(|spec| spec.canonical_bytes())
                .is_err()
        );

        // Every argument fits its individual limit, but JSON escaping makes
        // the recipe exceed the preallocation ceiling before it can become a
        // launcher bootstrap.
        let mut changed = program();
        changed.requirement.runtime_recipe.arguments = vec!["\n".repeat(64 * 1024); 2];
        assert!(
            changed
                .requirement
                .runtime_recipe
                .arguments
                .iter()
                .all(|value| value.len() <= 64 * 1024)
        );
        assert!(
            changed
                .validate()
                .unwrap_err()
                .to_string()
                .contains("encoded bound")
        );
        assert!(changed.digest().is_err());

        let valid = spec();
        assert!(valid.canonical_bytes().unwrap().len() <= MAX_LAUNCHER_BOOTSTRAP_BYTES);
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
