//! Sealed descriptor bootstrap for the dedicated native candidate launcher.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context as _, Result, ensure};
use ryeos_external_execution_contract::{
    ExternalGuestInputProjection, GuestMountAccess, MAX_GUEST_INPUTS,
};
use ryeos_state::external_execution::ExecutionChannelBinding;
use ryeos_state::external_execution::admission::{
    AdmittedExternalExecutionProgram, ExternalCandidateProcFilesystem, ExternalDirectSealedInput,
};
use serde::{Deserialize, Serialize};

use crate::backends::linux::{NativeCandidateOutput, NativeExternalCandidate};

pub const LAUNCHER_CONTROL_FD: u32 = 40;
pub const LAUNCHER_BOOTSTRAP_FD: u32 = 41;
pub const LAUNCHER_RUNTIME_FD: u32 = 42;
pub const LAUNCHER_PRIVATE_PARENT_FD: u32 = 43;
pub const LAUNCHER_EXECUTABLE_FD: u32 = 44;
pub const LAUNCHER_RUNTIME_MOUNT_FD_BASE: u32 = 64;
pub const MAX_LAUNCHER_RUNTIME_MOUNTS: usize = MAX_GUEST_INPUTS;
pub const MAX_LAUNCHER_BOOTSTRAP_BYTES: usize = 512 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalCandidateRuntimeMountSpec {
    pub destination: String,
    pub layer: u32,
    pub access: GuestMountAccess,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalCandidateLauncherSpec {
    pub schema: u32,
    pub binding: ExecutionChannelBinding,
    pub candidate_program: AdmittedExternalExecutionProgram,
    pub guest_inputs: ExternalGuestInputProjection,
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
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub stdin: Option<ExternalDirectSealedInput>,
}

fn deserialize_required_nullable<'de, D, T>(
    deserializer: D,
) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

struct LauncherCommandFields {
    executable: String,
    argv0: String,
    arguments: Vec<String>,
    cwd: String,
    max_stdout_bytes: u64,
    max_stderr_bytes: u64,
    proc_filesystem: ExternalCandidateProcFilesystem,
    contain_process_group: bool,
    nested_sandbox: bool,
    stdin: Option<ExternalDirectSealedInput>,
}

fn command_fields(program: &AdmittedExternalExecutionProgram) -> Result<LauncherCommandFields> {
    program.validate()?;
    Ok(match program {
        AdmittedExternalExecutionProgram::StructuredSession(program) => {
            let recipe = &program.requirement.runtime_recipe;
            LauncherCommandFields {
                executable: recipe.namespace_executable()?,
                argv0: recipe.argv0.clone(),
                arguments: recipe.arguments.clone(),
                cwd: recipe.cwd.clone(),
                max_stdout_bytes: recipe.max_stdout_bytes,
                max_stderr_bytes: recipe.max_stderr_bytes,
                proc_filesystem: recipe.proc_filesystem,
                contain_process_group: recipe.contain_process_group,
                nested_sandbox: recipe.nested_sandbox,
                stdin: None,
            }
        }
        AdmittedExternalExecutionProgram::DirectCommand(program) => {
            let projection = program.projection();
            let ryeos_external_execution_contract::ExternalExecutionMode::DirectCommand {
                stdout_max_bytes,
                stderr_max_bytes,
            } = projection.execution_mode
            else {
                anyhow::bail!("external direct projection requires direct execution mode");
            };
            // This finite mechanism is required, not inferred from the controller.
            let ryeos_state::external_execution::admission::ExternalDirectNativeRequirements::LinuxIsolatedReadOnly { .. } = &projection.native;
            LauncherCommandFields {
                executable: program.namespace_executable()?,
                argv0: projection.argv0.clone(),
                arguments: projection.arguments.clone(),
                cwd: projection.cwd.clone(),
                max_stdout_bytes: stdout_max_bytes,
                max_stderr_bytes: stderr_max_bytes,
                proc_filesystem: ExternalCandidateProcFilesystem::Empty,
                contain_process_group: true,
                nested_sandbox: false,
                stdin: Some(projection.stdin.clone()),
            }
        }
    })
}

impl ExternalCandidateLauncherSpec {
    /// Compile the exact profile-owned recipe after channel attachment. The
    /// runtime tree itself is supplied independently as one exact descriptor;
    /// this conversion cannot select an ambient executable or extra mount.
    pub fn from_admitted_program(
        binding: ExecutionChannelBinding,
        program: &AdmittedExternalExecutionProgram,
        guest_inputs: &ExternalGuestInputProjection,
    ) -> Result<Self> {
        program.validate_guest_inputs(guest_inputs)?;
        ensure!(
            binding.candidate_program_digest == program.digest()?
                && binding.supervisor_runtime_hash == program.runtime_manifest_hash()?
                && binding.execution_mode == program.execution_mode(),
            "external launcher program contradicts its attached channel"
        );
        if let AdmittedExternalExecutionProgram::StructuredSession(program) = program {
            let recipe = &program.requirement.runtime_recipe;
            ensure!(
            guest_inputs.inputs.iter().any(|input| {
                input.authority_id == program.requirement.runtime_product_declaration_id
                    && input.destination == recipe.runtime_mount_destination
                    && input.kind == ryeos_external_execution_contract::GuestMountKind::Directory
                    && matches!(&input.content_authority,
                        ryeos_external_execution_contract::GuestMountContentAuthority::ProductManifest {
                            manifest_kind,
                            manifest_hash,
                            ..
                        } if manifest_hash == &program.runtime_manifest_hash
                            && match manifest_kind {
                                ryeos_external_execution_contract::GuestProductManifestKind::Content =>
                                    program.runtime_manifest_kind == ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND,
                                ryeos_external_execution_contract::GuestProductManifestKind::LargeContent =>
                                    program.runtime_manifest_kind == ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND,
                            })
                    && input.access == GuestMountAccess::ReadOnly
            }),
            "external launcher guest inputs lost the qualified runtime"
        );
        }
        let command = command_fields(program)?;
        let spec = Self {
            schema: 3,
            binding,
            candidate_program: program.clone(),
            guest_inputs: guest_inputs.clone(),
            executable: command.executable,
            argv0: command.argv0,
            arguments: command.arguments,
            cwd: command.cwd,
            environment: guest_inputs.environment.clone(),
            runtime_mounts: guest_inputs
                .inputs
                .iter()
                .map(|input| ExternalCandidateRuntimeMountSpec {
                    destination: input.destination.clone(),
                    layer: u32::from(Path::new(&input.destination).starts_with("/workspace")),
                    access: input.access,
                })
                .collect(),
            max_stdout_bytes: command.max_stdout_bytes,
            max_stderr_bytes: command.max_stderr_bytes,
            proc_filesystem: command.proc_filesystem,
            contain_process_group: command.contain_process_group,
            nested_sandbox: command.nested_sandbox,
            stdin: command.stdin,
        };
        spec.validate()?;
        Ok(spec)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(self.schema == 3, "unsupported external launcher schema");
        self.binding.validate()?;
        ensure!(
            self.binding.execution_mode == self.candidate_program.execution_mode(),
            "external launcher mode contradicts its admitted program"
        );
        self.candidate_program
            .validate_guest_inputs(&self.guest_inputs)?;
        if let AdmittedExternalExecutionProgram::DirectCommand(program) = &self.candidate_program {
            let interval = self
                .binding
                .execution_deadline_ms
                .checked_sub(self.binding.issued_at_ms)
                .context("external direct execution interval overflow")?;
            ensure!(
                program.projection().endpoint_binding_digest == self.binding.execution_binding_hash
                    && interval > 0
                    && u64::try_from(interval)? <= program.projection().timeout_seconds * 1_000,
                "external launcher changed its direct endpoint or widened its tool timeout"
            );
        }
        ensure!(
            self.binding.candidate_program_digest == self.candidate_program.digest()?
                && self.binding.supervisor_runtime_hash
                    == self.candidate_program.runtime_manifest_hash()?,
            "external launcher program contradicts its attached channel"
        );
        let command = command_fields(&self.candidate_program)?;
        ensure!(
            self.executable == command.executable
                && self.argv0 == command.argv0
                && self.arguments == command.arguments
                && self.cwd == command.cwd
                && self.environment == self.guest_inputs.environment
                && self.runtime_mounts
                    == self
                        .guest_inputs
                        .inputs
                        .iter()
                        .map(|input| ExternalCandidateRuntimeMountSpec {
                            destination: input.destination.clone(),
                            layer: u32::from(
                                Path::new(&input.destination).starts_with("/workspace")
                            ),
                            access: input.access,
                        })
                        .collect::<Vec<_>>()
                && self.max_stdout_bytes == command.max_stdout_bytes
                && self.max_stderr_bytes == command.max_stderr_bytes
                && self.proc_filesystem == command.proc_filesystem
                && self.contain_process_group == command.contain_process_group
                && self.nested_sandbox == command.nested_sandbox
                && self.stdin == command.stdin,
            "external launcher specification changed its admitted command projection"
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
                mount.destination != "/workspace"
                    && !Path::new("/workspace").starts_with(destination),
                "external runtime mount replaces or contains /workspace"
            );
            ensure!(
                destinations.insert(mount.destination.clone()),
                "external runtime mount destination is duplicated"
            );
            ensure!(
                mount.layer == u32::from(destination.starts_with("/workspace"))
                    && (!destination.starts_with("/workspace")
                        || mount.access == GuestMountAccess::ReadOnly),
                "external runtime mount layer or access is invalid"
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

    pub(crate) fn validate_native_target(&self) -> Result<()> {
        self.validate()?;
        if let AdmittedExternalExecutionProgram::DirectCommand(program) = &self.candidate_program {
            let ryeos_state::external_execution::admission::ExternalDirectNativeRequirements::LinuxIsolatedReadOnly { required_arch } = &program.projection().native;
            let current = lillux::platform::current_target();
            ensure!(
                current.os == "linux" && current.arch == required_arch,
                "external direct native guest does not match its admitted target"
            );
        }
        Ok(())
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
    workspace_outputs: Option<&lillux::InheritedDescriptorAuthority>,
    runtime_mounts: &[lillux::InheritedDescriptorAuthority],
    spec: &ExternalCandidateLauncherSpec,
    timeout_seconds: f64,
) -> Result<PreparedExternalCandidateLauncherRequest> {
    spec.validate()?;
    ensure!(
        runtime_mounts.len() == spec.runtime_mounts.len(),
        "external launcher runtime mount authority count changed"
    );
    match (workspace_outputs, &spec.guest_inputs.workspace_outputs) {
        (None, None) => {}
        (Some(authority), Some(outputs)) => {
            ensure!(
                authority
                    .inherited_descriptor()
                    .map_err(anyhow::Error::msg)?
                    == outputs.descriptor,
                "external launcher workspace-output descriptor changed"
            );
            let observation = authority.regular_file_observation()?;
            ensure!(
                observation.size() == outputs.bytes
                    && authority.digest_regular_file_stable_exact(&observation)?
                        == outputs.authority_hash,
                "external launcher workspace-output authority changed"
            );
        }
        _ => anyhow::bail!("external launcher workspace-output authority presence changed"),
    }
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
    if let Some(workspace_outputs) = workspace_outputs {
        workspace_outputs.retain_for_child(&mut request.inherited_fds);
    }
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
    for index in 0..spec.runtime_mounts.len() {
        let fd = LAUNCHER_RUNTIME_MOUNT_FD_BASE
            .checked_add(u32::try_from(index)?)
            .context("external runtime mount descriptor overflow")?;
        let inherited = unsafe { lillux::take_inherited_descriptor_authority(fd) }
            .map_err(anyhow::Error::msg)?;
        mount_authorities.push(inherited);
    }
    let (candidate, output) = crate::backends::linux::prepare_candidate(
        &spec,
        &authority,
        &private_parent,
        &mount_authorities,
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
        AdmittedExternalCandidateProgram, ExternalCandidateRequirement,
        ExternalCandidateRuntimeRecipe, MAX_EXTERNAL_RUNTIME_RECIPE_BYTES, PROTOCOL,
    };

    fn program() -> AdmittedExternalCandidateProgram {
        let runtime_recipe = ExternalCandidateRuntimeRecipe {
            schema: 2,
            runtime_mount_destination: "/runtime".into(),
            executable_relative_path: "bin/codex".into(),
            argv0: "codex".into(),
            arguments: vec!["exec-server".into()],
            cwd: "/workspace".into(),
            environment: BTreeMap::from([("HOME".into(), "/workspace/.home".into())]),
            max_stdout_bytes: 1024 * 1024,
            max_stderr_bytes: 1024 * 1024,
            proc_filesystem: ExternalCandidateProcFilesystem::PidNamespaceNested,
            contain_process_group: false,
            nested_sandbox: true,
        };
        let runtime_recipe_digest = runtime_recipe.digest().unwrap();
        let requirement = ExternalCandidateRequirement {
            schema: 6,
            required_lifecycle_capabilities: Default::default(),
            protocol: PROTOCOL.into(),
            connector_protocol:
                ryeos_state::external_execution::admission::CONNECTOR_PROTOCOL.into(),
            execution_route: ryeos_state::external_execution::admission::ExternalCandidateExecutionRoute::ConnectorOnly,
            provider_declaration_id: "codex-hosted".into(),
            provider_configuration_destination: "environments.toml".into(),
            runtime_product_declaration_id: "runtime".into(),
            runtime_recipe,
        };
        let qualification_use =
            ryeos_state::external_execution::admission::test_support::fixture_qualification_use(
                &requirement,
            )
            .unwrap();
        AdmittedExternalCandidateProgram {
            requirement,
            qualification_use,
            runtime_manifest_kind: ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND.into(),
            runtime_manifest_hash: "e".repeat(64),
            runtime_source: ryeos_state::external_execution::admission::ExternalCandidateRuntimeSource::CapturedProduct { witness_hash: "1".repeat(64) },
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
            schema: ryeos_state::external_execution::EXECUTION_CHANNEL_BINDING_SCHEMA,
            execution_mode:
                ryeos_external_execution_contract::ExternalExecutionMode::StructuredSession {},
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

    fn guest_inputs() -> ExternalGuestInputProjection {
        ExternalGuestInputProjection {
            schema: ryeos_external_execution_contract::EXTERNAL_GUEST_INPUT_PROJECTION_SCHEMA,
            base_snapshot: ryeos_external_execution_contract::GuestBaseSnapshotInput {
                descriptor: 55,
                snapshot_hash: "c".repeat(64),
                closure_digest: "5".repeat(64),
                object_count: 3,
                blob_count: 1,
                total_bytes: 1,
            },
            workspace_outputs: None,
            inputs: vec![ryeos_external_execution_contract::GuestMountInput {
                role: ryeos_external_execution_contract::GuestMountRole::Product,
                authority_id: "runtime".into(),
                descriptor: 64,
                destination: "/runtime".into(),
                kind: ryeos_external_execution_contract::GuestMountKind::Directory,
                access: GuestMountAccess::ReadOnly,
                normalized_mode: None,
                content_authority:
                    ryeos_external_execution_contract::GuestMountContentAuthority::ProductManifest {
                        manifest_kind:
                            ryeos_external_execution_contract::GuestProductManifestKind::Content,
                        manifest_hash: "e".repeat(64),
                        manifest_descriptor: 65,
                        manifest_bytes: 256,
                    },
                bytes: 1,
            }],
            executable_search: Vec::new(),
            environment: BTreeMap::from([("HOME".into(), "/workspace/.home".into())]),
        }
    }

    fn spec() -> ExternalCandidateLauncherSpec {
        ExternalCandidateLauncherSpec {
            schema: 3,
            binding: binding(),
            candidate_program: program().into(),
            guest_inputs: guest_inputs(),
            executable: "/runtime/bin/codex".into(),
            argv0: "codex".into(),
            arguments: vec!["exec-server".into()],
            cwd: "/workspace".into(),
            environment: BTreeMap::from([("HOME".into(), "/workspace/.home".into())]),
            runtime_mounts: vec![ExternalCandidateRuntimeMountSpec {
                destination: "/runtime".into(),
                layer: 0,
                access: GuestMountAccess::ReadOnly,
            }],
            max_stdout_bytes: 1024 * 1024,
            max_stderr_bytes: 1024 * 1024,
            proc_filesystem: ExternalCandidateProcFilesystem::PidNamespaceNested,
            contain_process_group: false,
            nested_sandbox: true,
            stdin: None,
        }
    }

    #[test]
    fn direct_transport_projection_preserves_exact_mode_and_native_authority() {
        use ryeos_state::external_execution::admission::{
            AdmittedExternalDirectProgram, ExternalDirectSealedInput,
        };
        let mut inputs = guest_inputs();
        inputs.inputs[0].destination = "/ryeos/realizations/runtime".into();
        inputs.executable_search = vec![];
        let mut binding = binding();
        binding.execution_mode =
            ryeos_external_execution_contract::ExternalExecutionMode::DirectCommand {
                stdout_max_bytes: 1024,
                stderr_max_bytes: 2048,
            };
        binding.candidate_export_max_bytes = 0;
        // Wire-validation fixture only: no claim that this JSON passed the app
        // compiler or authorizes allocation or native execution.
        let direct:AdmittedExternalDirectProgram=serde_json::from_value(serde_json::json!({
            "execution_plan_hash":"1".repeat(64), "execution_closure_digest":"2".repeat(64),
            "command":{"authority":"realization_member","executable_blob_hash":"3".repeat(64),
                "realization_id":"runtime","realization_manifest_hash":binding.supervisor_runtime_hash,
                "realization_mount_root":"execution_runtime","realization_mount":"runtime","relative_path":"bin/evaluate",
                "execution_path":"/ryeos/realizations/runtime/bin/evaluate"},
            "projection":{"argv0":"/ryeos/realizations/runtime/bin/evaluate","arguments":["--check"],"cwd":"/workspace",
                "environment":inputs.environment,"stdin":ExternalDirectSealedInput::from_bytes(b"one shot\n").unwrap(),
                "endpoint_binding_id":"farm-direct","endpoint_binding_digest":binding.execution_binding_hash,
                "execution_mode":binding.execution_mode,"timeout_seconds":3600,
                "native":{"kind":"linux_isolated_read_only","required_arch":"x86_64"}},
            "guest_input_identity":inputs.identity_digest().unwrap()
        })).unwrap();
        let program = AdmittedExternalExecutionProgram::DirectCommand(direct);
        binding.candidate_program_digest = program.digest().unwrap();
        let exact =
            ExternalCandidateLauncherSpec::from_admitted_program(binding, &program, &inputs)
                .unwrap();
        assert_eq!(
            exact.stdin.as_ref().unwrap().decoded_bytes().unwrap(),
            b"one shot\n"
        );
        exact.validate().unwrap();
        // A structurally exact capsule is not evidence that this guest can
        // execute its architecture. Native validation is independently required
        // before the held launcher can produce Ready.
        let mut wrong_target = exact.clone();
        let mut wrong_program = serde_json::to_value(&wrong_target.candidate_program).unwrap();
        let wrong_arch = if lillux::platform::current_target().arch == "x86_64" {
            "aarch64"
        } else {
            "x86_64"
        };
        wrong_program["program"]["projection"]["native"]["required_arch"] =
            serde_json::json!(wrong_arch);
        wrong_target.candidate_program = serde_json::from_value(wrong_program).unwrap();
        wrong_target.binding.candidate_program_digest =
            wrong_target.candidate_program.digest().unwrap();
        wrong_target.validate().unwrap();
        assert!(
            wrong_target
                .validate_native_target()
                .unwrap_err()
                .to_string()
                .contains("does not match its admitted target")
        );
        assert_eq!(exact.executable, "/ryeos/realizations/runtime/bin/evaluate");
        // The same exact wire projection also supports a Project realization.
        // Its stable retained path is not the guest's execution coordinate.
        let mut project_inputs = inputs.clone();
        project_inputs.inputs[0].destination = "/workspace/runtime".into();
        let mut project_wire = serde_json::to_value(&program).unwrap();
        project_wire["program"]["command"]["realization_mount_root"] = serde_json::json!("project");
        project_wire["program"]["command"]["execution_path"] =
            serde_json::json!("/ryeos/admitted-project/runtime/bin/evaluate");
        project_wire["program"]["projection"]["argv0"] =
            serde_json::json!("/workspace/runtime/bin/evaluate");
        project_wire["program"]["guest_input_identity"] =
            serde_json::json!(project_inputs.identity_digest().unwrap());
        let project_program: AdmittedExternalExecutionProgram =
            serde_json::from_value(project_wire).unwrap();
        let mut project_binding = exact.binding.clone();
        project_binding.candidate_program_digest = project_program.digest().unwrap();
        let project = ExternalCandidateLauncherSpec::from_admitted_program(
            project_binding,
            &project_program,
            &project_inputs,
        )
        .unwrap();
        project.validate().unwrap();
        assert_eq!(project.executable, "/workspace/runtime/bin/evaluate");
        assert_eq!(project.argv0, project.executable);
        assert_eq!(project.runtime_mounts[0].destination, "/workspace/runtime");
        assert_eq!(
            serde_json::to_value(&project.candidate_program).unwrap()["program"]["command"]["execution_path"],
            "/ryeos/admitted-project/runtime/bin/evaluate"
        );
        let mut wrong_guest = project.clone();
        wrong_guest.executable = "/ryeos/admitted-project/runtime/bin/evaluate".into();
        assert!(wrong_guest.validate().is_err());
        let mut wrong_mount = project;
        wrong_mount.runtime_mounts[0].destination = "/ryeos/admitted-project/runtime".into();
        assert!(wrong_mount.validate().is_err());
        for mutation in [
            "stdin",
            "mode",
            "binding",
            "timeout",
            "executable",
            "native",
        ] {
            let mut changed = exact.clone();
            match mutation {
                "stdin" => {
                    changed.stdin = Some(ExternalDirectSealedInput::from_bytes(b"changed").unwrap())
                }
                "mode" => changed.binding.execution_mode =
                    ryeos_external_execution_contract::ExternalExecutionMode::StructuredSession {},
                "binding" => changed.binding.execution_binding_hash = "0".repeat(64),
                "timeout" => {
                    changed.binding.execution_deadline_ms = changed.binding.issued_at_ms + 3_600_001
                }
                "executable" => changed.executable = "/bin/echo".into(),
                "native" => changed.nested_sandbox = true,
                _ => unreachable!(),
            }
            assert!(changed.validate().is_err(), "accepted {mutation}");
        }
        let wire = serde_json::to_value(&exact).unwrap();
        let mut missing = wire.clone();
        missing.as_object_mut().unwrap().remove("stdin");
        assert!(serde_json::from_value::<ExternalCandidateLauncherSpec>(missing).is_err());
        let mut predecessor = exact;
        predecessor.schema = 2;
        assert!(predecessor.validate().is_err());
        let mut worker = spec();
        worker.stdin = Some(ExternalDirectSealedInput::from_bytes(b"").unwrap());
        assert!(worker.validate().is_err());
    }

    #[test]
    fn launcher_spec_is_the_exact_admitted_program_projection() {
        let program = program();
        let binding = binding();
        let compiled = ExternalCandidateLauncherSpec::from_admitted_program(
            binding.clone(),
            &program.into(),
            &guest_inputs(),
        )
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
                    .worker_mut()
                    .unwrap()
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
                "process_group" => changed.contain_process_group = true,
                "nested" => changed.nested_sandbox = false,
                _ => unreachable!(),
            }
            assert!(changed.validate().is_err(), "{mutation}");
        }
    }

    #[test]
    fn worker_recipe_cannot_be_relabelled_as_a_direct_evaluator() {
        let mut changed = spec();
        changed.binding.execution_mode =
            ryeos_external_execution_contract::ExternalExecutionMode::DirectCommand {
                stdout_max_bytes: 1024,
                stderr_max_bytes: 1024,
            };
        changed.binding.candidate_export_max_bytes = 0;
        changed.binding.validate().unwrap();
        assert!(
            changed
                .validate()
                .unwrap_err()
                .to_string()
                .contains("mode contradicts its admitted program")
        );
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
            program.qualification_use =
                ryeos_state::external_execution::admission::test_support::fixture_qualification_use(
                    &program.requirement,
                )?;
            let mut binding = binding();
            binding.candidate_program_digest = program.digest()?;
            ExternalCandidateLauncherSpec::from_admitted_program(
                binding,
                &program.into(),
                &guest_inputs(),
            )
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
            &serde_json::to_value(
                &boundary
                    .candidate_program
                    .worker()
                    .unwrap()
                    .requirement
                    .runtime_recipe,
            )
            .unwrap(),
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
        std::fs::write(temporary.path().join("workspace-output.json"), b"{}").unwrap();
        let workspace_output = root
            .open_inherited_regular(OsStr::new("workspace-output.json"), false)
            .unwrap()
            .unwrap();
        let mut launch_spec = spec();
        let output_observation = workspace_output.regular_file_observation().unwrap();
        let output_descriptor = workspace_output.inherited_descriptor().unwrap();
        // This is a process-local descriptor chosen by the OS, so the
        // fixture's synthetic base coordinate must not assume it is unused.
        launch_spec.guest_inputs.base_snapshot.descriptor =
            output_descriptor.max(65).checked_add(1).unwrap();
        launch_spec.guest_inputs.workspace_outputs = Some(
            ryeos_external_execution_contract::GuestWorkspaceOutputAuthorityInput {
                descriptor: output_descriptor,
                authority_hash: workspace_output
                    .digest_regular_file_stable_exact(&output_observation)
                    .unwrap(),
                bytes: output_observation.size(),
                producer_chain_root_id: "T-root".into(),
                producer_thread_id: "T-worker".into(),
                admitted_launch_capsule_hash: "8".repeat(64),
            },
        );
        let prepared = prepare_launcher_subprocess_request(
            &launcher,
            &runtime,
            &private,
            Some(&workspace_output),
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
        assert_eq!(prepared.request.inherited_fds.len(), 1);
        assert_eq!(
            prepared.request.inherited_fds[0]
                .inherited_descriptor()
                .unwrap(),
            workspace_output.inherited_descriptor().unwrap()
        );
        assert_eq!(prepared.request.inherited_fd_mappings.len(), 5);
        assert_eq!(prepared.bootstrap_digest, launch_spec.digest().unwrap());
    }
}
