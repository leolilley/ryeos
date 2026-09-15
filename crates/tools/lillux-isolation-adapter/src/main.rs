//! Thin protocol adapter for Lillux's native Linux confinement primitive.
//!
//! RyeOS owns the signed plan and descriptor roles. Lillux owns every raw OS
//! operation. This crate only validates and translates between those typed
//! contracts; it does not discover paths, tools, projects, or policy.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::io::Write as _;
use std::path::PathBuf;

use ryeos_isolation_protocol::{
    AdapterInspectionRequest, AdapterInspectionResponse, AdapterLaunchLifecycle,
    AdapterLaunchRequest, AdapterWorkspaceRequest, AdapterWorkspaceResponse,
    IsolationAdapterProtocolVersion, IsolationAuthorityPurpose, IsolationCapability,
    IsolationDiagnostic, IsolationDiagnosticCode, IsolationMountAccess, IsolationNetwork,
    IsolationTargetTriple, LauncherRefusalDocument, MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES,
    MAX_WORKSPACE_MUTATIONS, MAX_WORKSPACE_RESPONSE_BYTES, MAX_WORKSPACE_VIEW_RECEIPT_BYTES,
    WorkspaceLifecycleOperation, WorkspaceMutation, WorkspaceMutationKind,
    WorkspaceViewTransferReceipt, from_json_slice_strict,
};

const BACKEND_ID: &str = "linux-lillux";
const ADAPTER_BUILD: &str = env!("CARGO_PKG_VERSION");
// A dedicated child bounds its local transfer wait. The invoking parent owns
// the original operation-wide deadline, including launch/reap/settlement.
const WORKSPACE_TRANSFER_ENTRY_TIMEOUT: lillux::time::Duration =
    lillux::time::Duration::from_secs(30);

fn main() {
    let mut arguments = std::env::args_os();
    let _program = arguments.next();
    let Some(operation) = arguments.next() else {
        fail("missing adapter operation");
    };
    let Some(request_fd) = arguments.next() else {
        fail("missing request descriptor");
    };
    if arguments.next().is_some() {
        fail("unexpected adapter argument");
    }
    let request_fd = parse_descriptor(&request_fd).unwrap_or_else(|error| fail(&error));
    match operation.to_str() {
        Some("inspect") => match inspect(request_fd) {
            Ok(response) => write_json_response(&response, MAX_RESPONSE_BYTES),
            Err(error) => fail(&error),
        },
        Some("workspace") => match workspace(request_fd) {
            Ok(response) => write_json_response(&response, MAX_WORKSPACE_RESPONSE_BYTES),
            Err(error) => fail(&error),
        },
        Some("launch") => launch(request_fd),
        _ => fail("unsupported adapter operation"),
    }
}

fn inspect(request_fd: u32) -> Result<AdapterInspectionResponse, String> {
    let request: AdapterInspectionRequest = read_request(request_fd)?;
    request
        .validate()
        .map_err(|error| format!("invalid inspection request: {error}"))?;
    if request.backend_id != BACKEND_ID {
        return Err(format!(
            "inspection requested backend {}, adapter implements {BACKEND_ID}",
            request.backend_id
        ));
    }
    if request.target != host_target()? {
        return Err("inspection target does not match this adapter build".to_string());
    }
    if !request.artifacts.is_empty() {
        return Err("the self-contained Lillux adapter accepts no external artifacts".to_string());
    }
    let inspection = lillux::inspect_linux_sandbox()?;
    if inspection.aggregate_resource_isolation {
        return Err(
            "adapter build unexpectedly claims unconfigured aggregate resource isolation"
                .to_string(),
        );
    }
    let response = AdapterInspectionResponse {
        protocol: IsolationAdapterProtocolVersion::Current,
        adapter_build: ADAPTER_BUILD.to_string(),
        effective_capabilities: supported_capabilities(&inspection),
        artifacts: BTreeMap::new(),
    };
    response
        .validate()
        .map_err(|error| format!("invalid inspection response: {error}"))?;
    Ok(response)
}

fn launch(request_fd: u32) -> ! {
    let request = match read_request::<AdapterLaunchRequest>(request_fd) {
        Ok(request) => request,
        Err(error) => fail(&error),
    };
    let status_fd = request.status_fd;
    let result = translate_launch(&request).and_then(lillux::launch_linux_sandbox);
    let process = match result {
        Ok(process) => process,
        Err(error) => emit_refusal(status_fd, error),
    };
    let target = serde_json::json!({ "child-pid": process.child_pid() });
    let mut bytes = match serde_json::to_vec(&target) {
        Ok(bytes) => bytes,
        Err(error) => emit_refusal(status_fd, format!("serialize target status: {error}")),
    };
    bytes.push(b'\n');
    if let Err(error) = lillux::write_inherited_descriptor(status_fd, &bytes) {
        emit_refusal(status_fd, format!("publish target status: {error}"));
    }
    match process.wait() {
        Ok(status) => lillux::exit_with_linux_sandbox_status(status),
        Err(error) => fail(&error),
    }
}

fn translate_launch(request: &AdapterLaunchRequest) -> Result<lillux::LinuxSandboxRequest, String> {
    let required = request
        .validate()
        .map_err(|error| format!("invalid launch request: {error}"))?;
    if !request.artifacts.is_empty() {
        return Err("the self-contained Lillux adapter accepts no external artifacts".to_string());
    }
    lillux::validate_current_executable_descriptor(request.adapter_fd)?;
    // Backend selection already ran the real kernel probe through `inspect`.
    // Launch translates against the same fixed Lillux contract and lets each
    // namespace/mount/seccomp operation fail closed; it must not create a
    // second throwaway sandbox for every target process.
    let supported =
        supported_capabilities(&lillux::LinuxSandboxInspection::declared_native_contract());
    let missing = required.difference(&supported).collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(format!(
            "native Lillux backend lacks capabilities {missing:?}"
        ));
    }
    let authorities = request
        .authorities
        .iter()
        .map(|authority| (authority.id.clone(), authority))
        .collect::<BTreeMap<_, _>>();
    let target_mount = request
        .plan
        .mounts
        .iter()
        .find(|mount| mount.source == request.plan.target.executable)
        .ok_or_else(|| "target executable has no mount".to_string())?;
    let mounts = request
        .plan
        .mounts
        .iter()
        .map(|mount| {
            let authority = authorities
                .get(&mount.source)
                .ok_or_else(|| "mount authority disappeared after validation".to_string())?;
            Ok(lillux::LinuxSandboxMount {
                source_fd: authority.inherited_fd,
                destination: PathBuf::from(mount.destination.as_str()),
                access: match mount.access {
                    IsolationMountAccess::ReadOnly => lillux::LinuxSandboxMountAccess::ReadOnly,
                    IsolationMountAccess::Writable => lillux::LinuxSandboxMountAccess::Writable,
                },
                layer: mount.layer,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let overlay = request
        .plan
        .project_workspace
        .as_ref()
        .map(|workspace| {
            let view = authorities
                .get(&workspace.view)
                .ok_or_else(|| "workspace view authority disappeared".to_string())?;
            // SAFETY: validated descriptor roles are unique, and this dedicated
            // adapter consumes each inherited template exactly once.
            let template = unsafe {
                lillux::LinuxOverlayTemplate::take_inherited_descriptor(view.inherited_fd)
            }?;
            if canonical_digest(
                &template
                    .inherited_authority()
                    .directory_identity()
                    .map_err(|error| error.to_string())?,
            )? != workspace.view_descriptor_identity
            {
                return Err(
                    "workspace view descriptor differs from the admitted identity".to_string(),
                );
            }
            Ok::<_, String>(lillux::LinuxSandboxOverlay {
                template,
                destination: PathBuf::from(workspace.destination.as_str()),
                writable_descendant_mounts: workspace
                    .writable_descendant_mounts
                    .iter()
                    .map(|mount| {
                        let source = authorities.get(&mount.source).ok_or_else(|| {
                            "workspace descendant authority disappeared".to_string()
                        })?;
                        if source.purpose != IsolationAuthorityPurpose::WorkspaceViewDescendant {
                            return Err("workspace descendant has the wrong authority role".into());
                        }
                        Ok(lillux::LinuxSandboxOverlayDescendantMount {
                            source_fd: source.inherited_fd,
                            relative_path: PathBuf::from(&mount.relative_path),
                            destination: PathBuf::from(mount.destination.as_str()),
                        })
                    })
                    .collect::<Result<Vec<_>, String>>()?,
            })
        })
        .transpose()?;
    let target_channels = request
        .plan
        .target_channels
        .iter()
        .map(|channel| {
            let authority = authorities
                .get(&channel.source)
                .ok_or_else(|| "target-channel authority disappeared".to_string())?;
            if authority.purpose != IsolationAuthorityPurpose::TargetDuplexChannel {
                return Err("target-channel authority has the wrong purpose".to_string());
            }
            lillux::validate_connected_unix_stream_descriptor(authority.inherited_fd)?;
            Ok((authority.inherited_fd, channel.target_fd))
        })
        .collect::<Result<Vec<_>, String>>()?;
    let lifecycle = match request.lifecycle {
        AdapterLaunchLifecycle::Run => lillux::LinuxSandboxLifecycle::Run,
        AdapterLaunchLifecycle::AwaitAttachment {
            release_fd,
            release_keepalive_fd,
        } => lillux::LinuxSandboxLifecycle::AwaitRelease {
            release_fd,
            release_keepalive_fd,
        },
    };
    Ok(lillux::LinuxSandboxRequest {
        executable: PathBuf::from(target_mount.destination.as_str()),
        argv0: OsString::from(&request.plan.target.argv0),
        arguments: request
            .plan
            .target
            .arguments
            .iter()
            .map(OsString::from)
            .collect(),
        cwd: PathBuf::from(request.plan.target.cwd.as_str()),
        environment: request
            .plan
            .environment
            .values
            .iter()
            .map(|(name, value)| (OsString::from(name), OsString::from(value)))
            .collect(),
        mounts,
        fixed_parent_views: request
            .plan
            .fixed_parent_views
            .iter()
            .map(|view| lillux::LinuxSandboxFixedParentView {
                destination: PathBuf::from(view.destination.as_str()),
                denied_paths: view.denied_paths.iter().map(PathBuf::from).collect(),
                max_entries: view.limits.max_entries,
                max_depth: view.limits.max_depth,
            })
            .collect(),
        overlay,
        network: match request.plan.network {
            IsolationNetwork::Host => lillux::LinuxSandboxNetwork::Host,
            IsolationNetwork::Isolated => lillux::LinuxSandboxNetwork::Isolated,
        },
        private_tmp: request.plan.private_tmp,
        proc_filesystem: match request.plan.proc_filesystem {
            ryeos_isolation_protocol::IsolationProcFilesystem::Empty => {
                lillux::LinuxSandboxProcFilesystem::Empty
            }
            ryeos_isolation_protocol::IsolationProcFilesystem::PidNamespace => {
                lillux::LinuxSandboxProcFilesystem::PidNamespace
            }
            ryeos_isolation_protocol::IsolationProcFilesystem::PidNamespaceNested => {
                lillux::LinuxSandboxProcFilesystem::PidNamespaceNested
            }
        },
        minimal_devices: true,
        target_channels,
        lifecycle,
        contain_process_group: request.plan.shared_process_group,
        nested_sandbox: request.plan.nested_sandbox,
        // Aggregate resource limits are separate from the launch owner's
        // retained whole-execution lifetime scope. Scope control authority
        // stays outside this adapter/target namespace. The current protocol
        // admits no aggregate-limit request; ordinary rlimits do not supply it.
        aggregate_limits: None,
    })
}

fn workspace(request_fd: u32) -> Result<AdapterWorkspaceResponse, String> {
    let request: AdapterWorkspaceRequest = read_request(request_fd)?;
    request
        .validate()
        .map_err(|error| format!("invalid workspace request: {error}"))?;
    if request.transfer_fd == Some(request_fd)
        || request
            .authorities
            .iter()
            .any(|authority| authority.inherited_fd == request_fd)
    {
        return Err("workspace role aliases the sealed request descriptor".to_string());
    }
    let deadline = lillux::time::MonotonicDeadline::after(WORKSPACE_TRANSFER_ENTRY_TIMEOUT);
    let sender = request
        .transfer_fd
        .map(|fd| {
            // SAFETY: the validated sealed request assigns this unique inherited
            // endpoint solely to this dedicated Create invocation.
            unsafe { lillux::take_inherited_descriptor_transfer_sender(fd) }
                .map_err(|error| error.to_string())
        })
        .transpose()?;
    let descriptor_for = |purpose| {
        request
            .authorities
            .iter()
            .find(|authority| authority.purpose == purpose)
            .map(|authority| authority.inherited_fd)
            .ok_or_else(|| "workspace request is missing an authority".to_string())
    };
    let operation = match request.operation {
        WorkspaceLifecycleOperation::Create => lillux::LinuxOverlayWorkspaceOperation::Create,
        WorkspaceLifecycleOperation::FreezeAndDiff => {
            lillux::LinuxOverlayWorkspaceOperation::Observe
        }
        WorkspaceLifecycleOperation::Destroy => lillux::LinuxOverlayWorkspaceOperation::Destroy,
    };
    let observation = lillux::operate_linux_overlay_workspace(
        descriptor_for(IsolationAuthorityPurpose::WorkspaceProject)?,
        descriptor_for(IsolationAuthorityPurpose::WorkspaceBackendState)?,
        operation,
        MAX_WORKSPACE_MUTATIONS,
    )?;
    let template = if request.operation == WorkspaceLifecycleOperation::Create {
        Some(lillux::create_linux_overlay_template(
            descriptor_for(IsolationAuthorityPurpose::WorkspaceProject)?,
            descriptor_for(IsolationAuthorityPurpose::WorkspaceBackendState)?,
        )?)
    } else {
        None
    };
    let view_descriptor_identity = template
        .as_ref()
        .map(|template| {
            canonical_digest(
                &template
                    .inherited_authority()
                    .directory_identity()
                    .map_err(|error| error.to_string())?,
            )
        })
        .transpose()?;
    let mutations = observation
        .mutations
        .into_iter()
        .map(workspace_mutation)
        .collect::<Vec<_>>();
    let mut pinned_root_identities = BTreeMap::new();
    pinned_root_identities.insert("project".to_string(), observation.project_identity.clone());
    pinned_root_identities.insert(
        "backend_state".to_string(),
        observation.state_identity.clone(),
    );
    let mut response = AdapterWorkspaceResponse {
        protocol: IsolationAdapterProtocolVersion::Current,
        operation: request.operation,
        workspace_id: request.workspace_id.clone(),
        launch_owner: request.launch_owner.clone(),
        backend_id: BACKEND_ID.to_string(),
        backend_version: ADAPTER_BUILD.to_string(),
        mount_identity: request.mount_identity.clone(),
        view_descriptor_identity,
        pinned_root_identities,
        mutation_content_root: observation.mutation_content_root,
        mutations,
        destroyed: request.operation == WorkspaceLifecycleOperation::Destroy,
    };
    if request.operation == WorkspaceLifecycleOperation::Create {
        response.mount_identity = Some(canonical_digest(
            &response
                .mount_identity_value(&request)
                .map_err(|error| error.to_string())?,
        )?);
    }
    response
        .validate_for(&request)
        .map_err(|error| format!("invalid workspace response: {error}"))?;
    if serde_json::to_vec(&response)
        .map_err(|error| error.to_string())?
        .len()
        > MAX_WORKSPACE_RESPONSE_BYTES
    {
        return Err("workspace response exceeds its stdout limit".to_string());
    }
    match (sender, template) {
        (Some(sender), Some(template)) => {
            let bytes = workspace_view_receipt(&request, &response)?;
            sender
                .send(
                    &bytes,
                    std::slice::from_ref(template.inherited_authority()),
                    lillux::DescriptorTransferBounds::new(MAX_WORKSPACE_VIEW_RECEIPT_BYTES, 1)
                        .map_err(|error| error.to_string())?,
                    deadline,
                )
                .map_err(|error| format!("transfer created workspace view: {error}"))?;
        }
        (None, None) => {}
        _ => return Err("workspace transfer and created-view ownership disagree".to_string()),
    }
    Ok(response)
}

fn workspace_mutation(mutation: lillux::LinuxOverlayMutation) -> WorkspaceMutation {
    let (kind, normalized_mode, size, content_hash, target) = match mutation.kind {
        lillux::LinuxOverlayMutationKind::UpsertRegular {
            normalized_mode,
            size,
            sha256,
        } => (
            WorkspaceMutationKind::UpsertRegular,
            Some(normalized_mode),
            Some(size),
            Some(sha256),
            None,
        ),
        lillux::LinuxOverlayMutationKind::UpsertSymlink { target } => (
            WorkspaceMutationKind::UpsertSymlink,
            None,
            None,
            None,
            Some(target),
        ),
        lillux::LinuxOverlayMutationKind::DeletePath => {
            (WorkspaceMutationKind::DeletePath, None, None, None, None)
        }
        lillux::LinuxOverlayMutationKind::EnsureDirectory => (
            WorkspaceMutationKind::EnsureDirectory,
            None,
            None,
            None,
            None,
        ),
        lillux::LinuxOverlayMutationKind::OpaqueDirectory => (
            WorkspaceMutationKind::OpaqueDirectory,
            None,
            None,
            None,
            None,
        ),
    };
    WorkspaceMutation {
        path: mutation.path,
        kind,
        normalized_mode,
        size,
        content_hash,
        target,
    }
}

fn workspace_view_receipt(
    request: &AdapterWorkspaceRequest,
    response: &AdapterWorkspaceResponse,
) -> Result<Vec<u8>, String> {
    if request.operation != WorkspaceLifecycleOperation::Create {
        return Err("only Create may deliver a workspace view receipt".to_string());
    }
    response
        .validate_for(request)
        .map_err(|error| error.to_string())?;
    let receipt = WorkspaceViewTransferReceipt {
        protocol: IsolationAdapterProtocolVersion::Current,
        request_digest: canonical_digest(request)?,
        response_digest: canonical_digest(response)?,
    };
    receipt.validate().map_err(|error| error.to_string())?;
    let bytes = canonical_bytes(&receipt)?;
    if bytes.len() > MAX_WORKSPACE_VIEW_RECEIPT_BYTES {
        return Err("workspace view receipt exceeds the protocol bound".to_string());
    }
    Ok(bytes)
}

fn canonical_bytes<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, String> {
    let value = serde_json::to_value(value).map_err(|error| error.to_string())?;
    lillux::canonical_json(&value)
        .map(String::into_bytes)
        .map_err(|error| error.to_string())
}

fn canonical_digest<T: serde::Serialize>(value: &T) -> Result<String, String> {
    Ok(lillux::sha256_hex(&canonical_bytes(value)?))
}

fn supported_capabilities(
    inspection: &lillux::LinuxSandboxInspection,
) -> BTreeSet<IsolationCapability> {
    let mut capabilities = BTreeSet::new();
    if inspection.private_root {
        capabilities.insert(IsolationCapability::FilesystemPrivateRoot);
    }
    if inspection.descriptor_mounts {
        capabilities.extend([
            IsolationCapability::FilesystemFdReadOnly,
            IsolationCapability::FilesystemFdWritable,
            IsolationCapability::FilesystemOrderedOverlays,
        ]);
    }
    if inspection.overlay_workspace {
        capabilities.extend([
            IsolationCapability::FilesystemProjectWorkspaceCow,
            IsolationCapability::FilesystemWorkspaceDelta,
        ]);
    }
    if inspection.fixed_parent_views {
        capabilities.insert(IsolationCapability::FilesystemFixedParentViews);
    }
    if inspection.private_tmp {
        capabilities.insert(IsolationCapability::FilesystemPrivateTmp);
    }
    if inspection.pid_namespace_proc {
        capabilities.insert(IsolationCapability::FilesystemPidNamespaceProc);
    }
    if inspection.nested_sandbox {
        capabilities.insert(IsolationCapability::ProcessNestedSandbox);
    }
    if inspection.minimal_devices {
        capabilities.insert(IsolationCapability::DevicesMinimal);
    }
    if inspection.exact_environment {
        capabilities.insert(IsolationCapability::EnvironmentExact);
    }
    capabilities.insert(IsolationCapability::NetworkHost);
    if inspection.isolated_network {
        capabilities.insert(IsolationCapability::NetworkIsolated);
    }
    if inspection.isolated_pid_namespace {
        capabilities.insert(IsolationCapability::ProcessIsolatedPidNamespace);
    }
    capabilities.extend([
        IsolationCapability::ProcessTargetPidReporting,
        IsolationCapability::IpcTargetUnixStream,
    ]);
    if inspection.process_group_containment {
        capabilities.insert(IsolationCapability::LifecycleSharedProcessGroup);
    }
    capabilities
}

fn host_target() -> Result<IsolationTargetTriple, String> {
    if cfg!(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_env = "gnu"
    )) {
        Ok(IsolationTargetTriple::X86_64UnknownLinuxGnu)
    } else if cfg!(all(
        target_arch = "aarch64",
        target_os = "linux",
        target_env = "gnu"
    )) {
        Ok(IsolationTargetTriple::Aarch64UnknownLinuxGnu)
    } else {
        Err("native Lillux isolation does not support this target".to_string())
    }
}

fn read_request<T: serde::de::DeserializeOwned>(fd: u32) -> Result<T, String> {
    let bytes = lillux::read_sealed_inherited_descriptor(fd, MAX_REQUEST_BYTES)?;
    from_json_slice_strict(&bytes).map_err(|error| format!("parse strict request JSON: {error}"))
}

fn parse_descriptor(value: &OsStr) -> Result<u32, String> {
    value
        .to_str()
        .ok_or_else(|| "request descriptor is not UTF-8".to_string())?
        .parse::<u32>()
        .map_err(|_| "request descriptor is not numeric".to_string())
}

fn write_json_response<T: serde::Serialize>(response: &T, max_bytes: usize) -> ! {
    let bytes = serde_json::to_vec(response)
        .unwrap_or_else(|error| fail(&format!("serialize adapter response: {error}")));
    if bytes.len() > max_bytes {
        fail("adapter response exceeds protocol limit");
    }
    if std::io::stdout().write_all(&bytes).is_err() {
        std::process::exit(1);
    }
    std::process::exit(0)
}

fn emit_refusal(status_fd: u32, message: String) -> ! {
    let document = LauncherRefusalDocument {
        refused: IsolationDiagnostic {
            code: IsolationDiagnosticCode::LaunchRefused,
            message,
            details: BTreeMap::new(),
        },
    };
    if let Ok(mut bytes) = serde_json::to_vec(&document) {
        bytes.push(b'\n');
        let _ = lillux::write_inherited_descriptor(status_fd, &bytes);
    }
    std::process::exit(126)
}

fn fail(message: &str) -> ! {
    eprintln!("ryeos-lillux-isolation-adapter: {message}");
    std::process::exit(125)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_translation_preserves_each_normalized_mutation() {
        assert_eq!(
            lillux::sandbox::MAX_LINUX_OVERLAY_SYMLINK_TARGET_BYTES,
            ryeos_isolation_protocol::MAX_WORKSPACE_SYMLINK_TARGET_BYTES,
        );
        let cases = [
            (
                lillux::LinuxOverlayMutationKind::UpsertRegular {
                    normalized_mode: 0o755,
                    size: 7,
                    sha256: "a".repeat(64),
                },
                WorkspaceMutationKind::UpsertRegular,
            ),
            (
                lillux::LinuxOverlayMutationKind::UpsertSymlink {
                    target: "../lib/interpreter".into(),
                },
                WorkspaceMutationKind::UpsertSymlink,
            ),
            (
                lillux::LinuxOverlayMutationKind::DeletePath,
                WorkspaceMutationKind::DeletePath,
            ),
            (
                lillux::LinuxOverlayMutationKind::EnsureDirectory,
                WorkspaceMutationKind::EnsureDirectory,
            ),
            (
                lillux::LinuxOverlayMutationKind::OpaqueDirectory,
                WorkspaceMutationKind::OpaqueDirectory,
            ),
        ];
        for (kind, expected) in cases {
            let mutation = workspace_mutation(lillux::LinuxOverlayMutation {
                path: "output/member".into(),
                kind,
            });
            assert_eq!(mutation.path, "output/member");
            assert_eq!(mutation.kind, expected);
            mutation.validate().unwrap();
            let wire = serde_json::to_value(&mutation).unwrap();
            let decoded: WorkspaceMutation = serde_json::from_value(wire.clone()).unwrap();
            assert_eq!(decoded, mutation);
            match expected {
                WorkspaceMutationKind::UpsertRegular => {
                    assert_eq!(mutation.normalized_mode, Some(0o755));
                    assert_eq!(mutation.size, Some(7));
                    assert_eq!(
                        mutation.content_hash.as_deref(),
                        Some("a".repeat(64).as_str())
                    );
                    assert!(wire["target"].is_null());
                }
                WorkspaceMutationKind::UpsertSymlink => {
                    assert_eq!(mutation.target.as_deref(), Some("../lib/interpreter"));
                    assert!(wire["normalized_mode"].is_null());
                    assert!(wire["size"].is_null());
                    assert!(wire["content_hash"].is_null());
                }
                _ => assert!(wire["target"].is_null()),
            }
        }
    }

    #[test]
    fn capability_projection_does_not_claim_aggregate_resources() {
        let capabilities =
            supported_capabilities(&lillux::LinuxSandboxInspection::declared_native_contract());
        assert!(capabilities.contains(&IsolationCapability::FilesystemPrivateRoot));
        assert!(capabilities.contains(&IsolationCapability::NetworkIsolated));
        assert!(capabilities.contains(&IsolationCapability::FilesystemWorkspaceDelta));
        assert!(capabilities.contains(&IsolationCapability::ProcessIsolatedPidNamespace));
        assert!(!capabilities.contains(&IsolationCapability::ProcessHostPidNamespace));
    }

    #[test]
    fn adapter_is_self_contained() {
        let request = AdapterInspectionRequest {
            protocol: IsolationAdapterProtocolVersion::Current,
            target: host_target().unwrap(),
            backend_id: BACKEND_ID.to_string(),
            artifacts: BTreeMap::new(),
        };
        request.validate().unwrap();
        assert!(request.artifacts.is_empty());
    }

    #[test]
    fn workspace_receipt_commits_exact_canonical_request_and_full_response() {
        let request: AdapterWorkspaceRequest = serde_json::from_value(serde_json::json!({
            "protocol": IsolationAdapterProtocolVersion::Current,
            "operation": "create", "workspace_id": "workspace-one",
            "launch_owner": "{\"attempt\":1}", "base_snapshot": "a".repeat(64),
            "authorities": [
                {"id":"project", "inherited_fd":10, "purpose":"workspace_project"},
                {"id":"state", "inherited_fd":11, "purpose":"workspace_backend_state"}
            ], "transfer_fd":12, "mount_identity":null
        }))
        .unwrap();
        request.validate().unwrap();
        let mut response = AdapterWorkspaceResponse {
            protocol: request.protocol,
            operation: request.operation,
            workspace_id: request.workspace_id.clone(),
            launch_owner: request.launch_owner.clone(),
            backend_id: BACKEND_ID.to_string(),
            backend_version: ADAPTER_BUILD.to_string(),
            pinned_root_identities: BTreeMap::from([
                ("project".to_string(), "dev1-ino2".to_string()),
                ("backend_state".to_string(), "dev1-ino3".to_string()),
            ]),
            mount_identity: None,
            view_descriptor_identity: Some("b".repeat(64)),
            mutation_content_root: None,
            mutations: Vec::new(),
            destroyed: false,
        };
        response.mount_identity =
            Some(canonical_digest(&response.mount_identity_value(&request).unwrap()).unwrap());
        let bytes = workspace_view_receipt(&request, &response).unwrap();
        assert!(bytes.len() <= MAX_WORKSPACE_VIEW_RECEIPT_BYTES);
        let receipt: WorkspaceViewTransferReceipt = from_json_slice_strict(&bytes).unwrap();
        assert_eq!(bytes, canonical_bytes(&receipt).unwrap());
        assert_eq!(receipt.request_digest, canonical_digest(&request).unwrap());
        assert_eq!(
            receipt.response_digest,
            canonical_digest(&response).unwrap()
        );
        let mut changed = response.clone();
        changed.backend_version.push_str("-changed");
        assert_ne!(canonical_digest(&changed).unwrap(), receipt.response_digest);
        let mut changed = request.clone();
        changed.transfer_fd = Some(13);
        assert_ne!(canonical_digest(&changed).unwrap(), receipt.request_digest);
        changed.operation = WorkspaceLifecycleOperation::Destroy;
        assert!(workspace_view_receipt(&changed, &response).is_err());
    }
}
