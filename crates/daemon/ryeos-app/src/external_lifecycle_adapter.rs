//! Closed executable runner for signed external occurrence lifecycle adapters.

pub(crate) mod lease_admission;
pub(crate) mod lease_ready;

use std::collections::BTreeMap;

use anyhow::{Context as _, Result, ensure};
use ryeos_engine::binary_resolver::BundlePayloadSourceProof;
use ryeos_external_execution::lifecycle_adapter::{
    LifecycleAdapterInvocation, run_lifecycle_adapter,
};
use ryeos_external_execution_contract::relay_file::{
    LIFECYCLE_RELAY_UPLOAD_FD_ENV, MAX_RELAY_RESPONSE_BYTES, RELAY_FILE_PROTOCOL,
    RelayFileAdapterRequest, RelayFileAdapterResponse, RelayFileDirection, RelayFileEffect,
    RelayFileKind,
};
use ryeos_external_execution_contract::restored_runtime_measurement::{
    MAX_RESTORED_VERIFIER_ADAPTER_REQUEST_BYTES, MAX_RESTORED_VERIFIER_ADAPTER_RESPONSE_BYTES,
    RestoredVerifierAdapterRequest, RestoredVerifierAdapterResponse,
};
use ryeos_external_execution_contract::runtime_snapshot::{
    MAX_RUNTIME_SNAPSHOT_ADAPTER_REQUEST_BYTES, RuntimeSnapshotAdapterRequest,
    RuntimeSnapshotAdapterResponse, RuntimeSnapshotCreateAdapterRequest,
    RuntimeSnapshotCreateResult, RuntimeSnapshotQualificationAdapterRequest,
    RuntimeSnapshotQualificationAdapterResponse,
    RuntimeSnapshotQualificationTerminationAdapterRequest,
    RuntimeSnapshotQualificationTerminationAdapterResponse, RuntimeSnapshotReadinessObservation,
    RuntimeSnapshotReadinessRequest, RuntimeSnapshotUploadAdapterRequest,
    RuntimeSnapshotUploadReceipt,
};
use ryeos_external_execution_contract::runtime_snapshot_bootstrap::{
    MAX_BOOTSTRAP_ADAPTER_REQUEST_BYTES, RuntimeSnapshotBootstrapAdapterRequest,
    RuntimeSnapshotBootstrapAdapterResponse, RuntimeSnapshotBootstrapIntent,
    RuntimeSnapshotBootstrapReadinessAdapterResponse, RuntimeSnapshotBootstrapReadinessRequest,
    RuntimeSnapshotBootstrapTerminationAdapterRequest,
    RuntimeSnapshotBootstrapTerminationAdapterResponse,
};
use ryeos_external_execution_contract::staging_package::GuestStagingExpected;
use ryeos_external_execution_contract::{
    AllocationReservation, BoundOccurrence, LIFECYCLE_ADAPTER_PROTOCOL, LIFECYCLE_BOOTSTRAP_FD_ENV,
    LIFECYCLE_CREDENTIAL_FD_ENV, LIFECYCLE_GUEST_FRAMES_FD_ENV, LIFECYCLE_GUEST_PACKAGE_FD_ENV,
    LIFECYCLE_GUEST_RUNTIME_FD_ENV,
    LIFECYCLE_HOSTS_FD_ENV,
    LIFECYCLE_HOSTS_SHA256_ENV, LIFECYCLE_LAUNCHER_FD_ENV, LIFECYCLE_NETWORK_POLICY_SHA256_ENV,
    LIFECYCLE_PROVIDER_SPEC_FD_ENV, LIFECYCLE_PROVIDER_SPEC_SHA256_ENV,
    LIFECYCLE_REMAINING_TIMEOUT_MS_ENV, LIFECYCLE_RESOLVER_FD_ENV, LIFECYCLE_RESOLVER_SHA256_ENV,
    LIFECYCLE_SETTINGS_FD_ENV, LIFECYCLE_SIGNED_ASSIGNMENT_FD_ENV, LIFECYCLE_SIGNED_IMPORT_FD_ENV,
    LEASE_ADMISSION_FD_ENV, LIFECYCLE_SUPERVISOR_FD_ENV, LifecycleAdapterInspectionRequest,
    LifecycleAdapterInspectionResponse, LifecycleAdapterRequest, LifecycleAdapterResponse,
    LifecycleArtifactInspection, LifecycleArtifactRole, LifecycleGuestPackageDelivery,
    LifecycleOperationCommon, LifecycleRuntimeProbeRequest, LifecycleRuntimeProbeResponse,
    LifecycleRuntimeProbeSource, MAX_LIFECYCLE_GUEST_FRAME_MESSAGE_BYTES,
    MAX_LIFECYCLE_REQUEST_BYTES, MAX_LIFECYCLE_RESPONSE_BYTES,
    SupervisorActivationIntent, TerminationIntent, from_json_slice_strict,
};
use ryeos_state::objects::RetainedLifecycleExecutableRole;

use crate::external_artifacts::{
    CapturedLifecycleProviderSpec, LifecycleEscrowCapture, LifecycleEscrowExecutable,
    ResolvedExternalLifecycleArtifacts,
};
use crate::external_placement::{
    ExternalAllocationResolution, ExternalLifecycleObservation, ExternalPlacementBackend,
    ExternalSupervisorActivation, ExternalSupervisorActivationResolution,
    ExternalTerminationResolution, RuntimeProbeInput, SupervisorGuestPackage,
};
use crate::node_config::sections::external_execution::ExternalPlacementBackendContract;
use crate::runtime_db::external_execution::{
    ClaimedRelayFileContact, ExternalAllocationOccurrence, ExternalAllocationReservation,
    ExternalGuestPackageDeliveryCommitment, ExternalSupervisorActivationIntent,
    ExternalTerminationIntent, RelayDirection, RelayKind, RelayTransferPhase,
};
use crate::vault::placement::PlacementCredential;

// Inspection re-reads every declared executable through the transferred
// descriptor so the adapter independently proves the artifact set it will
// execute.  Debug and otherwise unstripped admitted artifacts can be large;
// the ordinary five-second command/control budget is therefore not a valid
// ceiling for this startup-only, non-provider-contact operation.  Keep it
// finite and aligned with the maximum admitted lifecycle contact window.
const LIFECYCLE_ADAPTER_INSPECTION_TIMEOUT_SECONDS: f64 = 60.0;

struct CapturedLifecycleNetworkInputs {
    resolver: lillux::secure_fs::CapturedRegularFile,
    hosts: lillux::secure_fs::CapturedRegularFile,
    resolver_sha256: String,
    hosts_sha256: String,
    policy_sha256: String,
}

/// In-memory exact source for a future protected CAS escrow. This is
/// historical verification data, not permission to reuse the backend for
/// fresh allocation after its installed Bundle generation is replaced.
#[derive(Debug)]
pub(crate) struct CapturedLifecycleSourceEvidence {
    pub bundle_name: String,
    pub signed_bundle_manifest: Vec<u8>,
    pub bundle_verifying_key: [u8; 32],
    pub adapter: BundlePayloadSourceProof,
    pub supervisor: BundlePayloadSourceProof,
    pub launcher: BundlePayloadSourceProof,
    pub restoration_verifier: Option<BundlePayloadSourceProof>,
}

fn capture_lifecycle_network_inputs(
    policy: &ryeos_state::external_execution::transport::ExternalNetworkInputPolicy,
) -> Result<CapturedLifecycleNetworkInputs> {
    use ryeos_state::external_execution::transport::{
        ExternalCapturedNetworkInputs, ExternalNetworkInputSelection,
    };

    policy.validate()?;
    let capture = |selection: &ExternalNetworkInputSelection| -> Result<
        lillux::secure_fs::CapturedRegularFile,
    > {
        let path = lillux::canonicalize_existing_path(std::path::Path::new(&selection.source))
            .with_context(|| "canonicalize signed lifecycle network input")?;
        let file = lillux::secure_fs::open_pinned_regular_file_no_follow(&path)
            .with_context(|| "open pinned lifecycle network input")?;
        let observation = file.observation()?;
        file.capture_sealed_bounded(&observation, selection.max_bytes)
            .with_context(|| "capture bounded lifecycle network input")
    };

    let resolver = capture(&policy.resolver)?;
    let hosts = capture(&policy.hosts)?;
    lillux::network::NetworkContext::from_config_bytes(resolver.bytes(), hosts.bytes())
        .context("validate captured lifecycle network inputs")?;
    let captured =
        ExternalCapturedNetworkInputs::from_bytes(policy, resolver.bytes(), hosts.bytes())?;
    captured.validate_for(policy)?;
    let policy_sha256 = policy.digest()?;

    Ok(CapturedLifecycleNetworkInputs {
        resolver,
        hosts,
        resolver_sha256: captured.resolver_digest,
        hosts_sha256: captured.hosts_digest,
        policy_sha256,
    })
}

/// How many entries the credential-home identity check reads, the homes
/// under the root and the top-level entries of each, before it refuses the
/// upload rather than admit it unchecked. Files deeper in a home are not
/// read, so session state accumulating inside homes never exhausts it.
const MAX_CREDENTIAL_HOME_CHECK_ENTRIES: usize = 65_536;

/// Refuse an upload whose opened source file is a file of the node's
/// private artifact homes. Three checks run on the exact descriptor the
/// adapter will inherit. First, the kernel's current pathname for the open
/// file is compared with the homes root: a file linked anywhere under it,
/// or last linked there and since unlinked (a credential file replaced by
/// writing a new file and renaming it over the old one), is refused. That
/// pathname names only one of a file's links, so second, the file's device
/// and inode are compared with every regular file at the top level of each
/// home, where a credential profile keeps its credential files; this names
/// the credential file when the upload is a hard link to one. Files deeper
/// in a home are not compared, so third, a file with more than one link is
/// refused outright: a hard link outside the homes to any file inside them
/// therefore never passes. A symlink is resolved when the file is opened,
/// so the open file is the home's own file and the first check sees it.
pub(crate) fn refuse_credential_home_upload(
    credential_home_root: &std::path::Path,
    upload: &lillux::InheritedDescriptorAuthority,
) -> Result<()> {
    let identity = upload.file_identity()?;
    let Some(root) = lillux::PinnedDirectory::open(credential_home_root)
        .context("open the node's credential-profile homes to check an upload")?
    else {
        return Ok(());
    };
    let canonical_root = lillux::canonicalize_existing_path(credential_home_root)
        .context("resolve the node's credential-profile homes to check an upload")?;
    let opened = upload
        .opened_path()
        .context("upload refused: read where its source file is linked")?;
    if opened.path.starts_with(&canonical_root) || opened.path.starts_with(credential_home_root) {
        anyhow::bail!(
            "upload refused: its source is {}{}, under the node's credential-profile homes; \
             a credential home never leaves this node",
            opened.path.display(),
            if opened.unlinked { " (since unlinked)" } else { "" }
        );
    }
    if let Some(found) =
        find_home_credential_file(&root, (identity.device(), identity.inode()))?
    {
        anyhow::bail!(
            "upload refused: its source is {}, a file of the node's credential-profile homes; \
             a credential home never leaves this node",
            credential_home_root.join(found).display()
        );
    }
    let links = identity.link_count();
    if links > 1 {
        anyhow::bail!(
            "upload refused: its source file has {links} hard links; an upload must have \
             exactly one, so that it cannot be another name for a file of the node's \
             credential-profile homes"
        );
    }
    Ok(())
}

/// The path, relative to the homes root, of the regular file with
/// `identity` at the top level of any home, reading without following
/// links and within [`MAX_CREDENTIAL_HOME_CHECK_ENTRIES`].
fn find_home_credential_file(
    root: &lillux::PinnedDirectory,
    identity: (u64, u64),
) -> Result<Option<std::path::PathBuf>> {
    let homes = credential_home_entries(root, MAX_CREDENTIAL_HOME_CHECK_ENTRIES)?;
    let mut remaining = MAX_CREDENTIAL_HOME_CHECK_ENTRIES
        .checked_sub(homes.len())
        .ok_or_else(credential_home_check_exhausted)?;
    for home in homes {
        let at_root = std::path::Path::new(&home.name);
        match home.entry_type {
            lillux::PinnedEntryType::Regular
                if (home.containing_device, home.inode) == identity =>
            {
                return Ok(Some(at_root.to_path_buf()));
            }
            lillux::PinnedEntryType::Directory => {
                let Some(directory) = root.open_child_directory(&home.name)? else {
                    continue;
                };
                let entries = credential_home_entries(&directory, remaining)?;
                remaining = remaining
                    .checked_sub(entries.len())
                    .ok_or_else(credential_home_check_exhausted)?;
                if let Some(entry) = entries.into_iter().find(|entry| {
                    entry.entry_type == lillux::PinnedEntryType::Regular
                        && (entry.containing_device, entry.inode) == identity
                }) {
                    return Ok(Some(at_root.join(entry.name)));
                }
            }
            _ => {}
        }
    }
    Ok(None)
}

fn credential_home_check_exhausted() -> anyhow::Error {
    anyhow::anyhow!(
        "upload refused: the node's credential-profile homes hold more homes and top-level \
         entries than the upload check reads ({MAX_CREDENTIAL_HOME_CHECK_ENTRIES})"
    )
}

/// The entries of one directory of the homes, at most `max_entries` of them,
/// read without following links. An entry renamed or removed between the
/// listing and its inspection is skipped. That loses nothing the upload
/// check relies on: if the upload is that file, its own pathname still names
/// the homes when the file stayed under them, the file is no longer a home's
/// file when it left them, and a second link to it is refused by the
/// link-count check.
fn credential_home_entries(
    directory: &lillux::PinnedDirectory,
    max_entries: usize,
) -> Result<Vec<lillux::PinnedDirectoryEntryMetadata>> {
    let read_failed = || {
        format!(
            "upload refused: read the node's credential-profile homes within the upload \
             check's bound of {MAX_CREDENTIAL_HOME_CHECK_ENTRIES} entries"
        )
    };
    // One name past the bound tells a full directory from an oversized one.
    let names = directory
        .entry_names_bounded(max_entries.saturating_add(1))
        .with_context(read_failed)?;
    if names.len() > max_entries {
        return Err(credential_home_check_exhausted());
    }
    let mut entries = Vec::with_capacity(names.len());
    for name in names {
        if let Some(entry) = directory.entry_no_follow(&name).with_context(read_failed)? {
            entries.push(entry);
        }
    }
    Ok(entries)
}

#[derive(Debug)]
pub(crate) struct ExecutableExternalPlacementBackend {
    /// Root of the node's credential-profile homes; no upload this backend
    /// hands its adapter may be a file under it.
    credential_home_root: std::path::PathBuf,
    declaration: ryeos_external_execution_contract::ExternalLifecycleAdapterDeclaration,
    bundle_manifest_digest: String,
    signer_fingerprint: String,
    adapter_hash: String,
    adapter_bytes: u64,
    adapter: lillux::InheritedDescriptorAuthority,
    supervisor_hash: String,
    supervisor_bytes: u64,
    supervisor: lillux::InheritedDescriptorAuthority,
    launcher_hash: String,
    launcher_bytes: u64,
    launcher: lillux::InheritedDescriptorAuthority,
    restoration_verifier: Option<lillux::InheritedDescriptorAuthority>,
    restoration_verifier_hash: Option<String>,
    provider_spec: CapturedLifecycleProviderSpec,
    /// Whether the captured provider spec declares an activation that uploads
    /// the guest runtime itself. Read once from the exact captured bytes.
    stages_guest_runtime_upload: bool,
    snapshot_production_spec: Option<CapturedLifecycleProviderSpec>,
    inspection: LifecycleAdapterInspectionResponse,
    source_evidence: CapturedLifecycleSourceEvidence,
}

impl ExecutableExternalPlacementBackend {
    pub(crate) fn new(
        artifacts: ResolvedExternalLifecycleArtifacts,
        credential_home_root: std::path::PathBuf,
    ) -> Result<Self> {
        let adapter_bytes = artifact_bytes(&artifacts.adapter)?;
        let supervisor_bytes = artifact_bytes(&artifacts.supervisor)?;
        let launcher_bytes = artifact_bytes(&artifacts.launcher)?;
        let restoration_verifier_bytes = artifacts
            .restoration_verifier
            .as_ref()
            .map(artifact_bytes)
            .transpose()?;
        ensure!(
            artifacts.provider_spec.sha256 == artifacts.declaration.provider_spec.sha256,
            "captured lifecycle provider spec identity differs from its signed declaration"
        );
        let mut inspected_artifacts = BTreeMap::from([
            (
                LifecycleArtifactRole::Supervisor,
                LifecycleArtifactInspection {
                    descriptor: artifacts
                        .supervisor
                        .handle
                        .inherited_descriptor()
                        .map_err(anyhow::Error::msg)?,
                    digest: artifacts.supervisor.identity.content_hash.clone(),
                    bytes: supervisor_bytes,
                },
            ),
            (
                LifecycleArtifactRole::Launcher,
                LifecycleArtifactInspection {
                    descriptor: artifacts
                        .launcher
                        .handle
                        .inherited_descriptor()
                        .map_err(anyhow::Error::msg)?,
                    digest: artifacts.launcher.identity.content_hash.clone(),
                    bytes: launcher_bytes,
                },
            ),
        ]);
        if let Some(verifier) = &artifacts.restoration_verifier {
            inspected_artifacts.insert(
                LifecycleArtifactRole::RestorationVerifier,
                LifecycleArtifactInspection {
                    descriptor: verifier
                        .handle
                        .inherited_descriptor()
                        .map_err(anyhow::Error::msg)?,
                    digest: verifier.identity.content_hash.clone(),
                    bytes: restoration_verifier_bytes
                        .context("restoration verifier has no size")?,
                },
            );
        }
        let request = LifecycleAdapterInspectionRequest {
            schema: 1,
            protocol: LIFECYCLE_ADAPTER_PROTOCOL.into(),
            adapter_id: artifacts.declaration.id.clone(),
            adapter_artifact_hash: artifacts.adapter.identity.content_hash.clone(),
            settings_schema_digest: artifacts.declaration.settings_schema_digest.clone(),
            target: lillux::platform::current_binary_target()?.into(),
            declared_capabilities: artifacts.declaration.capabilities.clone(),
            provider_spec: LifecycleArtifactInspection {
                descriptor: artifacts
                    .provider_spec
                    .authority
                    .inherited_descriptor()
                    .map_err(anyhow::Error::msg)?,
                digest: artifacts.provider_spec.sha256.clone(),
                bytes: artifacts.provider_spec.bytes,
            },
            snapshot_production_spec: artifacts
                .snapshot_production_spec
                .as_ref()
                .map(|spec| -> Result<LifecycleArtifactInspection> {
                    Ok(LifecycleArtifactInspection {
                        descriptor: spec
                            .authority
                            .inherited_descriptor()
                            .map_err(anyhow::Error::msg)?,
                        digest: spec.sha256.clone(),
                        bytes: spec.bytes,
                    })
                })
                .transpose()?,
            artifacts: inspected_artifacts,
        };
        request.validate()?;
        let request_bytes = lifecycle_inspection_bytes(&request)?;
        let request_handle =
            lillux::sealed_memfd(c"ryeos-lifecycle-inspection-request", &request_bytes)
                .map_err(anyhow::Error::msg)?;
        let response = run_lifecycle_adapter(
            &artifacts.adapter.handle,
            LifecycleAdapterInvocation::Inspect,
            &request_handle,
            [
                Some(artifacts.supervisor.handle.clone()),
                Some(artifacts.launcher.handle.clone()),
                artifacts
                    .restoration_verifier
                    .as_ref()
                    .map(|item| item.handle.clone()),
                Some(artifacts.provider_spec.authority.clone()),
                artifacts
                    .snapshot_production_spec
                    .as_ref()
                    .map(|spec| spec.authority.clone()),
            ]
            .into_iter()
            .flatten()
            .collect(),
            Vec::new(),
            lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs_f64(
                LIFECYCLE_ADAPTER_INSPECTION_TIMEOUT_SECONDS,
            )),
        )?;
        let inspection = decode_inspection_response(&response.bytes, &request)?;
        let stages_guest_runtime_upload =
            provider_spec_stages_guest_runtime(&artifacts.provider_spec)?;

        Ok(Self {
            credential_home_root,
            declaration: artifacts.declaration,
            bundle_manifest_digest: artifacts.bundle_manifest_digest,
            signer_fingerprint: artifacts.signer_fingerprint,
            adapter_hash: artifacts.adapter.identity.content_hash,
            adapter_bytes,
            adapter: artifacts.adapter.handle,
            supervisor_hash: artifacts.supervisor.identity.content_hash,
            supervisor_bytes,
            supervisor: artifacts.supervisor.handle,
            launcher_hash: artifacts.launcher.identity.content_hash,
            launcher_bytes,
            launcher: artifacts.launcher.handle,
            restoration_verifier_hash: artifacts
                .restoration_verifier
                .as_ref()
                .map(|item| item.identity.content_hash.clone()),
            restoration_verifier: artifacts
                .restoration_verifier
                .as_ref()
                .map(|item| item.handle.clone()),
            provider_spec: artifacts.provider_spec,
            stages_guest_runtime_upload,
            snapshot_production_spec: artifacts.snapshot_production_spec,
            inspection,
            source_evidence: CapturedLifecycleSourceEvidence {
                bundle_name: artifacts.bundle_name,
                signed_bundle_manifest: artifacts.signed_bundle_manifest,
                bundle_verifying_key: artifacts.bundle_verifying_key,
                adapter: artifacts.adapter.source_proof,
                supervisor: artifacts.supervisor.source_proof,
                launcher: artifacts.launcher.source_proof,
                restoration_verifier: artifacts.restoration_verifier.map(|item| item.source_proof),
            },
        })
    }

    fn invoke(
        &self,
        contract: &ExternalPlacementBackendContract,
        credential: &PlacementCredential,
        request: &LifecycleAdapterRequest,
        bootstrap: Option<&ryeos_state::external_execution::transport::ExternalSupervisorBootstrap>,
        guest_inputs: Option<&ryeos_external_execution::guest_inputs::ExternalGuestInputAuthority>,
        guest_package: Option<&lillux::InheritedDescriptorAuthority>,
        guest_runtime: Option<&lillux::InheritedDescriptorAuthority>,
        signed_import: Option<&ryeos_external_execution_contract::guest_import_authorization::SignedGuestImportAuthorization>,
        signed_assignment: Option<&ryeos_external_execution_contract::guest_import_authorization::SignedGuestOccurrenceAssignment>,
        admitted_adapter_request: Option<&[u8]>,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<ExternalLifecycleObservation<LifecycleAdapterResponse>> {
        let deadline = deadline.min(lillux::time::MonotonicDeadline::after(
            lillux::time::Duration::from_secs(u64::from(contract.contact_timeout_seconds)),
        ));
        ensure!(
            !deadline.has_elapsed(),
            "external lifecycle contact deadline expired"
        );
        // Sealed request/credential preparation also acquires descriptor
        // authority. Keep the bounded outer lease so nested preparation
        // cannot queue behind a new exclusive fork after budget admission.
        let descriptors = lillux::retain_fork_sensitive_descriptors_until(deadline)?;
        self.qualify_offline(contract, credential)?;
        let network_inputs = capture_lifecycle_network_inputs(&contract.provider_network_inputs)?;
        let request_handle = lillux::sealed_memfd(
            c"ryeos-lifecycle-operation-request",
            &request.canonical_bytes()?,
        )
        .map_err(anyhow::Error::msg)?;
        let settings = lillux::canonical_json(&contract.settings)?;
        let settings_handle =
            lillux::sealed_memfd(c"ryeos-lifecycle-settings", settings.as_bytes())
                .map_err(anyhow::Error::msg)?;
        let credential_handle = lillux::sealed_memfd(
            c"ryeos-lifecycle-credential",
            credential.secret().as_bytes(),
        )
        .map_err(anyhow::Error::msg)?;
        let mut inherited = vec![
            self.supervisor.clone(),
            self.launcher.clone(),
            self.provider_spec.authority.clone(),
            settings_handle.clone(),
            credential_handle.clone(),
            network_inputs.resolver.authority().clone(),
            network_inputs.hosts.authority().clone(),
        ];
        let mut envs = vec![
            (
                LIFECYCLE_SUPERVISOR_FD_ENV.into(),
                self.supervisor
                    .inherited_descriptor()
                    .map_err(anyhow::Error::msg)?
                    .to_string(),
            ),
            (
                LIFECYCLE_LAUNCHER_FD_ENV.into(),
                self.launcher
                    .inherited_descriptor()
                    .map_err(anyhow::Error::msg)?
                    .to_string(),
            ),
            (
                LIFECYCLE_SETTINGS_FD_ENV.into(),
                settings_handle
                    .inherited_descriptor()
                    .map_err(anyhow::Error::msg)?
                    .to_string(),
            ),
            (
                LIFECYCLE_CREDENTIAL_FD_ENV.into(),
                credential_handle
                    .inherited_descriptor()
                    .map_err(anyhow::Error::msg)?
                    .to_string(),
            ),
            (
                LIFECYCLE_PROVIDER_SPEC_FD_ENV.into(),
                self.provider_spec
                    .authority
                    .inherited_descriptor()
                    .map_err(anyhow::Error::msg)?
                    .to_string(),
            ),
            (
                LIFECYCLE_PROVIDER_SPEC_SHA256_ENV.into(),
                self.provider_spec.sha256.clone(),
            ),
            (
                LIFECYCLE_RESOLVER_FD_ENV.into(),
                network_inputs
                    .resolver
                    .authority()
                    .inherited_descriptor()
                    .map_err(anyhow::Error::msg)?
                    .to_string(),
            ),
            (
                LIFECYCLE_HOSTS_FD_ENV.into(),
                network_inputs
                    .hosts
                    .authority()
                    .inherited_descriptor()
                    .map_err(anyhow::Error::msg)?
                    .to_string(),
            ),
            (
                LIFECYCLE_RESOLVER_SHA256_ENV.into(),
                network_inputs.resolver_sha256.clone(),
            ),
            (
                LIFECYCLE_HOSTS_SHA256_ENV.into(),
                network_inputs.hosts_sha256.clone(),
            ),
            (
                LIFECYCLE_NETWORK_POLICY_SHA256_ENV.into(),
                network_inputs.policy_sha256.clone(),
            ),
        ];
        if let Some(bootstrap) = bootstrap {
            bootstrap.validate()?;
            let encoded = bootstrap.canonical_bytes()?;
            let handle = lillux::sealed_memfd(c"ryeos-lifecycle-supervisor-bootstrap", &encoded)
                .map_err(anyhow::Error::msg)?;
            envs.push((
                LIFECYCLE_BOOTSTRAP_FD_ENV.into(),
                handle
                    .inherited_descriptor()
                    .map_err(anyhow::Error::msg)?
                    .to_string(),
            ));
            inherited.push(handle);
        }
        if let Some(guest_inputs) = guest_inputs {
            ensure!(
                guest_inputs.projection().identity_digest()? == guest_inputs.identity_digest()?,
                "external guest input identity changed before adapter launch"
            );
            if let LifecycleAdapterRequest::ActivateSupervisor {
                guest_input_identity,
                guest_input_projection,
                ..
            } = request
            {
                ensure!(
                    guest_inputs.identity_digest()? == *guest_input_identity
                        && guest_inputs.projection() == guest_input_projection.as_ref(),
                    "external guest input identity changed at package handoff"
                );
            }
        }
        if let Some(package) = guest_package {
            // The guest package crosses to the lease like any upload, so it
            // takes the same credential-home identity check.
            refuse_credential_home_upload(&self.credential_home_root, package)?;
            envs.push((
                LIFECYCLE_GUEST_PACKAGE_FD_ENV.into(),
                package
                    .inherited_descriptor()
                    .map_err(anyhow::Error::msg)?
                    .to_string(),
            ));
            inherited.push(package.clone());
        }
        ensure!(
            signed_import.is_some() == guest_package.is_some()
                && signed_assignment.is_some() == signed_import.is_some()
                && signed_import.is_some()
                    == matches!(request, LifecycleAdapterRequest::ActivateSupervisor { .. }),
            "signed guest import and exact package must cross one activation contact together"
        );
        if let Some((name, descriptor, runtime)) = guest_runtime_delivery(
            &self.credential_home_root,
            self.stages_guest_runtime_upload,
            guest_package.is_some(),
            guest_runtime,
        )? {
            envs.push((name, descriptor));
            inherited.push(runtime.clone());
        }
        if let Some(signed) = signed_import {
            signed.validate_shape()?;
            let bytes = ryeos_external_execution_contract::canonical_json(signed)?;
            let handle = lillux::sealed_memfd(c"ryeos-signed-guest-import", &bytes)
                .map_err(anyhow::Error::msg)?;
            envs.push((
                LIFECYCLE_SIGNED_IMPORT_FD_ENV.into(),
                handle
                    .inherited_descriptor()
                    .map_err(anyhow::Error::msg)?
                    .to_string(),
            ));
            inherited.push(handle);
        }
        if let Some(signed) = signed_assignment {
            signed.validate_shape()?;
            let bytes = ryeos_external_execution_contract::canonical_json(signed)?;
            let handle = lillux::sealed_memfd(c"ryeos-signed-guest-assignment", &bytes)
                .map_err(anyhow::Error::msg)?;
            envs.push((
                LIFECYCLE_SIGNED_ASSIGNMENT_FD_ENV.into(),
                handle
                    .inherited_descriptor()
                    .map_err(anyhow::Error::msg)?
                    .to_string(),
            ));
            inherited.push(handle);
        }
        if let Some(bytes) = admitted_adapter_request {
            let (handle, env) = sealed_admitted_adapter_request(bytes)?;
            envs.push(env);
            inherited.push(handle);
        }
        drop(descriptors);
        let remaining_timeout_ms = deadline.remaining().as_millis();
        ensure!(
            remaining_timeout_ms > 0,
            "external lifecycle contact deadline expired before spawn"
        );
        envs.push((
            LIFECYCLE_REMAINING_TIMEOUT_MS_ENV.into(),
            u64::try_from(remaining_timeout_ms)
                .context("external lifecycle remaining timeout exceeds its bound")?
                .to_string(),
        ));
        let response = run_lifecycle_adapter(
            &self.adapter,
            LifecycleAdapterInvocation::Operate,
            &request_handle,
            inherited,
            envs,
            deadline,
        )?;
        let deadline_exceeded = response.deadline_exceeded;
        let response = decode_operation_response(&response.bytes, request)?;
        Ok(ExternalLifecycleObservation {
            value: response,
            deadline_exceeded,
        })
    }
    fn invoke_relay_file(
        &self,
        contract: &ExternalPlacementBackendContract,
        credential: &PlacementCredential,
        bootstrap: &ryeos_state::external_execution::transport::ExternalSupervisorBootstrap,
        claim: ClaimedRelayFileContact,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<ExternalLifecycleObservation<RelayFileAdapterResponse>> {
        let record = claim.record();
        let intent = &record.intent;
        bootstrap.validate()?;
        let relay = bootstrap.controller.relay()?;
        ensure!(
            bootstrap.digest()? == intent.bootstrap_digest
                && bootstrap.placement_thread_id == intent.placement_thread_id
                && bootstrap.occurrence_id == intent.occurrence_id
                && bootstrap.allocation_request_digest == intent.allocation_request_digest
                && bootstrap.relay_mailbox_root()? == intent.mailbox_root
                && relay.maximum_file_bytes == intent.maximum_file_bytes,
            "claimed relay differs from original signed placement"
        );
        ensure!(
            match claim.effect() {
                RelayFileEffect::UploadOnce =>
                    intent.direction == RelayDirection::ControllerToGuest
                        && record.phase == RelayTransferPhase::UploadPossible
                        && record.read_attempts == 0,
                RelayFileEffect::Download =>
                    (intent.direction == RelayDirection::GuestToController
                        && record.phase == RelayTransferPhase::Reading
                        && record.read_attempts > 0)
                        || (intent.direction == RelayDirection::ControllerToGuest
                            && record.phase == RelayTransferPhase::UploadPossible
                            && record.read_attempts > 0),
            },
            "relay adapter received no durable contact claim"
        );
        let request = RelayFileAdapterRequest {
            schema: 1,
            protocol: RELAY_FILE_PROTOCOL.into(),
            binding_hash: bootstrap.execution_binding_hash.clone(),
            settings_digest: contract.settings_digest.clone(),
            placement_thread_id: intent.placement_thread_id.clone(),
            occurrence_id: intent.occurrence_id.clone(),
            allocation_request_digest: intent.allocation_request_digest.clone(),
            bootstrap_digest: intent.bootstrap_digest.clone(),
            mailbox_root: intent.mailbox_root.clone(),
            kind: match intent.kind {
                RelayKind::Attach => RelayFileKind::Attach,
                RelayKind::Exchange => RelayFileKind::Exchange,
            },
            sequence: intent.sequence,
            direction: match intent.direction {
                RelayDirection::GuestToController => RelayFileDirection::GuestToController,
                RelayDirection::ControllerToGuest => RelayFileDirection::ControllerToGuest,
            },
            remote_path: intent.remote_path.clone(),
            maximum_file_bytes: intent.maximum_file_bytes,
            deadline_ms: intent.deadline_ms,
            effect: claim.effect(),
            expected_sha256: intent.expected_sha256.clone(),
            expected_bytes: intent.expected_bytes,
        };
        let request_bytes = request.canonical_bytes()?;
        let deadline = deadline.min(lillux::time::MonotonicDeadline::after(
            lillux::time::Duration::from_secs(u64::from(contract.contact_timeout_seconds)),
        ));
        ensure!(!deadline.has_elapsed(), "relay contact deadline expired");
        let descriptors = lillux::retain_fork_sensitive_descriptors_until(deadline)?;
        self.qualify_offline(contract, credential)?;
        let network = capture_lifecycle_network_inputs(&contract.provider_network_inputs)?;
        let request_handle = lillux::sealed_memfd(c"ryeos-relay-file-request", &request_bytes)
            .map_err(anyhow::Error::msg)?;
        let bootstrap_handle = lillux::sealed_memfd(
            c"ryeos-original-relay-bootstrap",
            &bootstrap.canonical_bytes()?,
        )
        .map_err(anyhow::Error::msg)?;
        let settings_bytes = lillux::canonical_json(&contract.settings)?;
        ensure!(
            lillux::sha256_hex(settings_bytes.as_bytes()) == contract.settings_digest,
            "relay signed settings changed before adapter contact"
        );
        let settings_handle =
            lillux::sealed_memfd(c"ryeos-relay-settings", settings_bytes.as_bytes())
                .map_err(anyhow::Error::msg)?;
        let credential_handle =
            lillux::sealed_memfd(c"ryeos-relay-credential", credential.secret().as_bytes())
                .map_err(anyhow::Error::msg)?;
        let fd = |handle: &lillux::InheritedDescriptorAuthority| -> Result<String> {
            Ok(handle
                .inherited_descriptor()
                .map_err(anyhow::Error::msg)?
                .to_string())
        };
        let mut inherited = vec![
            self.provider_spec.authority.clone(),
            bootstrap_handle.clone(),
            settings_handle.clone(),
            credential_handle.clone(),
            network.resolver.authority().clone(),
            network.hosts.authority().clone(),
        ];
        let mut envs = vec![
            (LIFECYCLE_BOOTSTRAP_FD_ENV.into(), fd(&bootstrap_handle)?),
            (LIFECYCLE_SETTINGS_FD_ENV.into(), fd(&settings_handle)?),
            (LIFECYCLE_CREDENTIAL_FD_ENV.into(), fd(&credential_handle)?),
            (
                LIFECYCLE_PROVIDER_SPEC_FD_ENV.into(),
                fd(&self.provider_spec.authority)?,
            ),
            (
                LIFECYCLE_PROVIDER_SPEC_SHA256_ENV.into(),
                self.provider_spec.sha256.clone(),
            ),
            (
                LIFECYCLE_RESOLVER_FD_ENV.into(),
                fd(network.resolver.authority())?,
            ),
            (
                LIFECYCLE_HOSTS_FD_ENV.into(),
                fd(network.hosts.authority())?,
            ),
            (
                LIFECYCLE_RESOLVER_SHA256_ENV.into(),
                network.resolver_sha256,
            ),
            (LIFECYCLE_HOSTS_SHA256_ENV.into(), network.hosts_sha256),
            (
                LIFECYCLE_NETWORK_POLICY_SHA256_ENV.into(),
                network.policy_sha256,
            ),
        ];
        if claim.effect() == RelayFileEffect::UploadOnce {
            let bytes = record
                .canonical_bytes
                .as_deref()
                .context("claimed relay upload lost retained bytes")?;
            ensure!(
                intent.expected_sha256.as_deref() == Some(lillux::sha256_hex(bytes).as_str())
                    && intent.expected_bytes == Some(bytes.len() as u64),
                "claimed relay upload changed its committed bytes"
            );
            // The relay upload is a memfd sealed here from the journal's
            // committed bytes, never a file on disk, so it cannot be a
            // credential-home file and takes no identity check.
            let handle =
                lillux::sealed_memfd(c"ryeos-relay-upload", bytes).map_err(anyhow::Error::msg)?;
            envs.push((LIFECYCLE_RELAY_UPLOAD_FD_ENV.into(), fd(&handle)?));
            inherited.push(handle);
        }
        let remaining = deadline.remaining().as_millis();
        ensure!(remaining > 0, "relay contact deadline expired before spawn");
        envs.push((
            LIFECYCLE_REMAINING_TIMEOUT_MS_ENV.into(),
            u64::try_from(remaining)?.to_string(),
        ));
        drop(descriptors);
        let output = run_lifecycle_adapter(
            &self.adapter,
            LifecycleAdapterInvocation::RelayFile,
            &request_handle,
            inherited,
            envs,
            deadline,
        )?;
        let response: RelayFileAdapterResponse =
            from_json_slice_strict(&output.bytes, MAX_RELAY_RESPONSE_BYTES)?;
        let mut canonical = ryeos_external_execution_contract::canonical_json(&response)?;
        canonical.push(b'\n');
        ensure!(
            output.bytes == canonical,
            "relay adapter response is not exact canonical protocol output"
        );
        response.validate_for(&request, &lillux::sha256_hex(&request_bytes))?;
        Ok(ExternalLifecycleObservation {
            value: response,
            deadline_exceeded: output.deadline_exceeded,
        })
    }

    fn invoke_snapshot_adapter(
        &self,
        binding: &crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
        credential: &PlacementCredential,
        request_bytes: &[u8],
        invocation: LifecycleAdapterInvocation,
        upload: Option<&lillux::InheritedDescriptorAuthority>,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<ryeos_external_execution::lifecycle_adapter::LifecycleAdapterOutput> {
        let spec = self
            .snapshot_production_spec
            .as_ref()
            .context("installed lifecycle adapter has no signed snapshot production profile")?;
        self.invoke_sealed_snapshot_operation(
            binding.network_inputs(),
            spec,
            binding.settings(),
            binding.settings_digest(),
            credential,
            request_bytes,
            invocation,
            upload,
            deadline,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn invoke_sealed_snapshot_operation(
        &self,
        network_policy: &ryeos_state::external_execution::transport::ExternalNetworkInputPolicy,
        spec: &CapturedLifecycleProviderSpec,
        settings: &serde_json::Value,
        settings_digest: &str,
        credential: &PlacementCredential,
        request_bytes: &[u8],
        invocation: LifecycleAdapterInvocation,
        upload: Option<&lillux::InheritedDescriptorAuthority>,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<ryeos_external_execution::lifecycle_adapter::LifecycleAdapterOutput> {
        ensure!(!deadline.has_elapsed(), "snapshot adapter deadline expired");
        ensure!(
            request_bytes.len()
                <= if invocation == LifecycleAdapterInvocation::QualifySnapshotVerify {
                    MAX_RESTORED_VERIFIER_ADAPTER_REQUEST_BYTES
                } else {
                    MAX_RUNTIME_SNAPSHOT_ADAPTER_REQUEST_BYTES
                },
            "snapshot adapter request exceeds its bound"
        );
        let descriptors = lillux::retain_fork_sensitive_descriptors_until(deadline)?;
        let network_inputs = capture_lifecycle_network_inputs(network_policy)?;
        let request_handle = lillux::sealed_memfd(c"ryeos-runtime-snapshot-request", request_bytes)
            .map_err(anyhow::Error::msg)?;
        let settings_bytes = lillux::canonical_json(settings)?;
        ensure!(
            lillux::sha256_hex(settings_bytes.as_bytes()) == settings_digest,
            "snapshot operation settings changed after node admission"
        );
        let settings_handle = lillux::sealed_memfd(
            c"ryeos-runtime-snapshot-settings",
            settings_bytes.as_bytes(),
        )
        .map_err(anyhow::Error::msg)?;
        let credential_handle = lillux::sealed_memfd(
            c"ryeos-runtime-snapshot-credential",
            credential.secret().as_bytes(),
        )
        .map_err(anyhow::Error::msg)?;
        let fd = |handle: &lillux::InheritedDescriptorAuthority| -> Result<String> {
            Ok(handle
                .inherited_descriptor()
                .map_err(anyhow::Error::msg)?
                .to_string())
        };
        let mut inherited = vec![
            spec.authority.clone(),
            settings_handle.clone(),
            credential_handle.clone(),
            network_inputs.resolver.authority().clone(),
            network_inputs.hosts.authority().clone(),
        ];
        if let Some(upload) = upload {
            refuse_credential_home_upload(&self.credential_home_root, upload)?;
            inherited.push(upload.clone());
        }
        let environment = vec![
            (LIFECYCLE_PROVIDER_SPEC_FD_ENV.into(), fd(&spec.authority)?),
            (
                LIFECYCLE_PROVIDER_SPEC_SHA256_ENV.into(),
                spec.sha256.clone(),
            ),
            (LIFECYCLE_SETTINGS_FD_ENV.into(), fd(&settings_handle)?),
            (LIFECYCLE_CREDENTIAL_FD_ENV.into(), fd(&credential_handle)?),
            (
                LIFECYCLE_RESOLVER_FD_ENV.into(),
                fd(network_inputs.resolver.authority())?,
            ),
            (
                LIFECYCLE_HOSTS_FD_ENV.into(),
                fd(network_inputs.hosts.authority())?,
            ),
            (
                LIFECYCLE_RESOLVER_SHA256_ENV.into(),
                network_inputs.resolver_sha256,
            ),
            (
                LIFECYCLE_HOSTS_SHA256_ENV.into(),
                network_inputs.hosts_sha256,
            ),
            (
                LIFECYCLE_NETWORK_POLICY_SHA256_ENV.into(),
                network_inputs.policy_sha256,
            ),
            (
                LIFECYCLE_REMAINING_TIMEOUT_MS_ENV.into(),
                u64::try_from(deadline.remaining().as_millis())?.to_string(),
            ),
        ];
        drop(descriptors);
        run_lifecycle_adapter(
            &self.adapter,
            invocation,
            &request_handle,
            inherited,
            environment,
            deadline,
        )
    }
}

impl ExternalPlacementBackend for ExecutableExternalPlacementBackend {
    fn relay_file_contact(
        &self,
        contract: &ExternalPlacementBackendContract,
        credential: &PlacementCredential,
        bootstrap: &ryeos_state::external_execution::transport::ExternalSupervisorBootstrap,
        claim: ClaimedRelayFileContact,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<ExternalLifecycleObservation<RelayFileAdapterResponse>> {
        self.invoke_relay_file(contract, credential, bootstrap, claim, deadline)
    }

    fn lifecycle_escrow_capture(&self) -> Result<Option<LifecycleEscrowCapture<'_>>> {
        let mut executables = vec![
            LifecycleEscrowExecutable {
                role: RetainedLifecycleExecutableRole::Adapter,
                hash: &self.adapter_hash,
                bytes: self.adapter_bytes,
                handle: &self.adapter,
                proof: &self.source_evidence.adapter,
            },
            LifecycleEscrowExecutable {
                role: RetainedLifecycleExecutableRole::Supervisor,
                hash: &self.supervisor_hash,
                bytes: self.supervisor_bytes,
                handle: &self.supervisor,
                proof: &self.source_evidence.supervisor,
            },
            LifecycleEscrowExecutable {
                role: RetainedLifecycleExecutableRole::Launcher,
                hash: &self.launcher_hash,
                bytes: self.launcher_bytes,
                handle: &self.launcher,
                proof: &self.source_evidence.launcher,
            },
        ];
        match (
            &self.restoration_verifier,
            &self.restoration_verifier_hash,
            &self.source_evidence.restoration_verifier,
        ) {
            (Some(handle), Some(hash), Some(proof)) => {
                executables.push(LifecycleEscrowExecutable {
                    role: RetainedLifecycleExecutableRole::RestorationVerifier,
                    hash,
                    bytes: handle.regular_file_observation()?.size(),
                    handle,
                    proof,
                })
            }
            (None, None, None) => {}
            _ => anyhow::bail!("captured restoration verifier authority is incomplete"),
        }
        Ok(Some(LifecycleEscrowCapture {
            declaration: &self.declaration,
            bundle_name: &self.source_evidence.bundle_name,
            bundle_manifest_digest: &self.bundle_manifest_digest,
            signer_fingerprint: &self.signer_fingerprint,
            signed_bundle_manifest: &self.source_evidence.signed_bundle_manifest,
            bundle_verifying_key: self.source_evidence.bundle_verifying_key,
            executables,
            provider_spec: &self.provider_spec,
            snapshot_production_spec: self.snapshot_production_spec.as_ref(),
        }))
    }
    fn bootstrap_profile_digest(&self) -> Option<&str> {
        self.inspection.observed_bootstrap_profile_digest.as_deref()
    }
    fn bootstrap_provider_spec_digest(&self) -> Option<&str> {
        Some(&self.inspection.observed_provider_spec_sha256)
    }
    fn backend_id(&self) -> &str {
        &self.declaration.id
    }

    fn stages_guest_runtime_upload(&self) -> bool {
        self.stages_guest_runtime_upload
    }

    fn admit_lease(
        &self,
        inputs: &lease_admission::LeaseAdmissionInputs,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<lease_admission::AdmittedLease> {
        self.invoke_admit_lease(inputs, deadline)
    }

    fn await_lease_ready(
        &self,
        request: &[u8],
        ready_frame: &[u8],
        report_frame: &[u8],
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<Vec<u8>> {
        self.invoke_await_lease_ready(request, ready_frame, report_frame, deadline)
    }

    fn artifact_hash(&self) -> &str {
        &self.adapter_hash
    }

    fn artifact_bytes(&self) -> u64 {
        self.adapter_bytes
    }

    fn supervisor_artifact(&self) -> (&str, u64) {
        (&self.supervisor_hash, self.supervisor_bytes)
    }

    fn launcher_artifact(&self) -> (&str, u64) {
        (&self.launcher_hash, self.launcher_bytes)
    }

    fn settings_schema_digest(&self) -> &str {
        &self.declaration.settings_schema_digest
    }

    fn preflight_runtime_snapshot(
        &self,
        binding: &crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
        credential: &PlacementCredential,
    ) -> Result<()> {
        let spec = self
            .snapshot_production_spec
            .as_ref()
            .context("installed lifecycle adapter has no signed snapshot production profile")?;
        ensure!(
            binding.backend() == self.declaration.id
                && binding.adapter_artifact_hash() == self.adapter_hash
                && binding.snapshot_spec_sha256() == spec.sha256
                && self
                    .inspection
                    .observed_snapshot_production_spec_sha256
                    .as_deref()
                    == Some(spec.sha256.as_str())
                && credential.backend() == binding.backend()
                && credential.account() == binding.account()
                && lillux::sha256_hex(lillux::canonical_json(binding.settings())?.as_bytes())
                    == binding.settings_digest(),
            "snapshot producer differs from its inspected signed authority"
        );
        Ok(())
    }

    fn preflight_bootstrap_recovery(
        &self,
        binding: &crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
        credential: &PlacementCredential,
        intent: &RuntimeSnapshotBootstrapIntent,
    ) -> Result<()> {
        intent.validate()?;
        ensure!(
            intent.production_binding_digest == binding.digest()
                && intent.provider_id == self.declaration.id
                && intent.provider_id == binding.backend()
                && intent.provider_group_id == binding.provider_group_id()
                && intent.adapter_artifact_hash == self.adapter_hash
                && intent.bootstrap_profile_digest
                    == self
                        .inspection
                        .observed_bootstrap_profile_digest
                        .as_deref()
                        .unwrap_or_default()
                && intent.provider_spec_digest == self.provider_spec.sha256
                && self.inspection.observed_provider_spec_sha256 == self.provider_spec.sha256
                && intent.settings_digest == binding.settings_digest()
                && credential.backend() == binding.backend()
                && credential.account() == binding.account()
                && lillux::sha256_hex(lillux::canonical_json(binding.settings())?.as_bytes())
                    == binding.settings_digest(),
            "bootstrap recovery differs from its retained signed adapter and provider authority"
        );
        Ok(())
    }

    fn preflight_snapshot_qualification_create(
        &self,
        producer: &crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
        qualification: &crate::node_config::sections::runtime_snapshot_qualification::InstalledRuntimeSnapshotQualificationBinding,
        credential: &PlacementCredential,
    ) -> Result<()> {
        ensure!(
            producer.backend() == self.declaration.id
                && producer.adapter_artifact_hash() == self.adapter_hash
                && qualification.production_binding_id() == producer.id()
                && qualification.production_binding_digest() == producer.digest()
                && qualification.provider_spec_digest() == self.provider_spec.sha256
                && self.restoration_verifier_hash.as_deref()
                    == Some(qualification.verifier_artifact_hash())
                && self.restoration_verifier.is_some()
                && self.inspection.observed_provider_spec_sha256 == self.provider_spec.sha256
                && credential.backend() == producer.backend()
                && credential.account() == producer.account()
                && lillux::sha256_hex(lillux::canonical_json(qualification.settings())?.as_bytes())
                    == qualification.settings_digest(),
            "snapshot qualification differs from its exact signed producer or provider profile"
        );
        Ok(())
    }

    fn create_snapshot_bootstrap_source(
        &self,
        binding: &crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
        credential: &PlacementCredential,
        request: &RuntimeSnapshotBootstrapAdapterRequest,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<ExternalLifecycleObservation<RuntimeSnapshotBootstrapAdapterResponse>> {
        self.preflight_runtime_snapshot(binding, credential)?;
        request.validate()?;
        ensure!(
            request.intent.production_binding_digest == binding.digest()
                && request.intent.bootstrap_profile_digest
                    == self
                        .inspection
                        .observed_bootstrap_profile_digest
                        .as_deref()
                        .unwrap_or_default()
                && request.intent.provider_id == binding.backend()
                && request.intent.provider_group_id == binding.provider_group_id()
                && request.intent.adapter_artifact_hash == self.adapter_hash
                && request.intent.provider_spec_digest == self.provider_spec.sha256
                && request.intent.settings_digest == binding.settings_digest(),
            "bootstrap create differs from inspected signed production authority"
        );
        let remaining_ms = request
            .intent
            .attempt_deadline_ms
            .saturating_sub(lillux::time::timestamp_millis());
        ensure!(
            remaining_ms > 0,
            "bootstrap create expired before adapter contact"
        );
        let deadline = deadline
            .min(lillux::time::MonotonicDeadline::after(
                lillux::time::Duration::from_secs(u64::from(binding.contact_timeout_seconds())),
            ))
            .min(lillux::time::MonotonicDeadline::after(
                lillux::time::Duration::from_millis(u64::try_from(remaining_ms)?),
            ));
        let bytes = ryeos_external_execution_contract::canonical_json(request)?;
        ensure!(
            bytes.len() <= MAX_BOOTSTRAP_ADAPTER_REQUEST_BYTES,
            "bootstrap adapter request exceeds sealed bound"
        );
        let output = self.invoke_sealed_snapshot_operation(
            binding.network_inputs(),
            &self.provider_spec,
            binding.settings(),
            binding.settings_digest(),
            credential,
            &bytes,
            LifecycleAdapterInvocation::BootstrapSourceCreate,
            None,
            deadline,
        )?;
        let value: RuntimeSnapshotBootstrapAdapterResponse =
            from_json_slice_strict(&output.bytes, MAX_LIFECYCLE_RESPONSE_BYTES)
                .map_err(|_| anyhow::anyhow!("invalid bootstrap create adapter response"))?;
        value
            .validate_for(request)
            .map_err(|_| anyhow::anyhow!("invalid bootstrap create adapter response"))?;
        if let RuntimeSnapshotBootstrapAdapterResponse::OccurrenceBound { occurrence } = &value {
            ensure!(
                !occurrence.contact_deadline_exceeded,
                "bootstrap adapter claimed daemon-only deadline evidence"
            );
        }
        Ok(ExternalLifecycleObservation {
            value,
            deadline_exceeded: output.deadline_exceeded,
        })
    }

    fn observe_snapshot_bootstrap_source(
        &self,
        binding: &crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
        credential: &PlacementCredential,
        request: &RuntimeSnapshotBootstrapReadinessRequest,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<ExternalLifecycleObservation<RuntimeSnapshotBootstrapReadinessAdapterResponse>>
    {
        self.preflight_bootstrap_recovery(binding, credential, &request.intent)?;
        request.validate()?;
        ensure!(
            request.intent.production_binding_digest == binding.digest()
                && request.intent.bootstrap_profile_digest
                    == self
                        .inspection
                        .observed_bootstrap_profile_digest
                        .as_deref()
                        .unwrap_or_default()
                && request.intent.provider_id == binding.backend()
                && request.intent.provider_group_id == binding.provider_group_id()
                && request.intent.adapter_artifact_hash == self.adapter_hash
                && request.intent.provider_spec_digest == self.provider_spec.sha256
                && request.intent.settings_digest == binding.settings_digest(),
            "bootstrap readiness differs from inspected signed production authority"
        );
        let deadline = deadline.min(lillux::time::MonotonicDeadline::after(
            lillux::time::Duration::from_secs(u64::from(binding.contact_timeout_seconds())),
        ));
        let bytes = ryeos_external_execution_contract::canonical_json(request)?;
        ensure!(
            bytes.len() <= MAX_BOOTSTRAP_ADAPTER_REQUEST_BYTES,
            "bootstrap readiness request exceeds sealed bound"
        );
        let output = self.invoke_sealed_snapshot_operation(
            binding.network_inputs(),
            &self.provider_spec,
            binding.settings(),
            binding.settings_digest(),
            credential,
            &bytes,
            LifecycleAdapterInvocation::BootstrapSourceReadiness,
            None,
            deadline,
        )?;
        let value: RuntimeSnapshotBootstrapReadinessAdapterResponse =
            from_json_slice_strict(&output.bytes, MAX_LIFECYCLE_RESPONSE_BYTES)
                .map_err(|_| anyhow::anyhow!("invalid bootstrap readiness adapter response"))?;
        value.validate_for(request)?;
        Ok(ExternalLifecycleObservation {
            value,
            deadline_exceeded: output.deadline_exceeded,
        })
    }

    fn terminate_snapshot_bootstrap_source(
        &self,
        binding: &crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
        credential: &PlacementCredential,
        request: &RuntimeSnapshotBootstrapTerminationAdapterRequest,
        first_contact: bool,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<ExternalLifecycleObservation<RuntimeSnapshotBootstrapTerminationAdapterResponse>>
    {
        self.preflight_bootstrap_recovery(binding, credential, &request.bootstrap_intent)?;
        request.validate()?;
        ensure!(
            request.bootstrap_intent.production_binding_digest == binding.digest()
                && request.bootstrap_intent.bootstrap_profile_digest
                    == self
                        .inspection
                        .observed_bootstrap_profile_digest
                        .as_deref()
                        .unwrap_or_default()
                && request.bootstrap_intent.provider_id == binding.backend()
                && request.bootstrap_intent.provider_group_id == binding.provider_group_id()
                && request.bootstrap_intent.adapter_artifact_hash == self.adapter_hash
                && request.provider_spec_digest == self.provider_spec.sha256
                && request.bootstrap_intent.settings_digest == binding.settings_digest(),
            "bootstrap termination differs from inspected signed production authority"
        );
        let mut deadline = deadline.min(lillux::time::MonotonicDeadline::after(
            lillux::time::Duration::from_secs(u64::from(binding.contact_timeout_seconds())),
        ));
        if first_contact {
            let remaining_ms = request
                .intent
                .attempt_deadline_ms
                .saturating_sub(lillux::time::timestamp_millis());
            ensure!(
                remaining_ms > 0,
                "bootstrap termination expired before first contact"
            );
            deadline = deadline.min(lillux::time::MonotonicDeadline::after(
                lillux::time::Duration::from_millis(u64::try_from(remaining_ms)?),
            ));
        }
        let bytes = ryeos_external_execution_contract::canonical_json(request)?;
        ensure!(
            bytes.len() <= MAX_BOOTSTRAP_ADAPTER_REQUEST_BYTES,
            "bootstrap termination request exceeds sealed bound"
        );
        let output = self.invoke_sealed_snapshot_operation(
            binding.network_inputs(),
            &self.provider_spec,
            binding.settings(),
            binding.settings_digest(),
            credential,
            &bytes,
            if first_contact {
                LifecycleAdapterInvocation::BootstrapSourceTerminate
            } else {
                LifecycleAdapterInvocation::BootstrapSourceObserveTermination
            },
            None,
            deadline,
        )?;
        let value: RuntimeSnapshotBootstrapTerminationAdapterResponse =
            from_json_slice_strict(&output.bytes, MAX_LIFECYCLE_RESPONSE_BYTES)
                .map_err(|_| anyhow::anyhow!("invalid bootstrap termination adapter response"))?;
        value.validate_for(request)?;
        if let RuntimeSnapshotBootstrapTerminationAdapterResponse::Terminal { observation } = &value
        {
            ensure!(
                !observation.contact_deadline_exceeded,
                "bootstrap adapter claimed daemon-only deadline evidence"
            );
        }
        Ok(ExternalLifecycleObservation {
            value,
            deadline_exceeded: output.deadline_exceeded,
        })
    }

    fn create_snapshot_qualification_occurrence(
        &self,
        producer: &crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
        qualification: &crate::node_config::sections::runtime_snapshot_qualification::InstalledRuntimeSnapshotQualificationBinding,
        credential: &PlacementCredential,
        request: &RuntimeSnapshotQualificationAdapterRequest,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<ExternalLifecycleObservation<RuntimeSnapshotQualificationAdapterResponse>> {
        self.preflight_snapshot_qualification_create(producer, qualification, credential)?;
        request.validate()?;
        ensure!(
            request.intent.qualification_profile_digest == qualification.digest()
                && request.intent.snapshot_operation_id == request.source_intent.operation_id
                && request.intent.provider_id == producer.backend()
                && request.intent.adapter_artifact_hash == self.adapter_hash
                && request.intent.provider_spec_digest == self.provider_spec.sha256
                && request.intent.settings_digest == qualification.settings_digest()
                && request.intent.verifier_artifact_hash == qualification.verifier_artifact_hash()
                && request.intent.maximum_lifetime_seconds
                    == qualification.maximum_lifetime_seconds()
                && request.source_intent.production_profile_digest == producer.digest(),
            "qualification create differs from inspected signed authority"
        );
        let remaining_ms = request
            .intent
            .attempt_deadline_ms
            .saturating_sub(lillux::time::timestamp_millis());
        ensure!(
            remaining_ms > 0,
            "qualification create expired before adapter contact"
        );
        let deadline = deadline
            .min(lillux::time::MonotonicDeadline::after(
                lillux::time::Duration::from_secs(u64::from(
                    qualification.contact_timeout_seconds(),
                )),
            ))
            .min(lillux::time::MonotonicDeadline::after(
                lillux::time::Duration::from_millis(u64::try_from(remaining_ms)?),
            ));
        let bytes = ryeos_external_execution_contract::canonical_json(request)?;
        let output = self.invoke_sealed_snapshot_operation(
            producer.network_inputs(),
            &self.provider_spec,
            qualification.settings(),
            qualification.settings_digest(),
            credential,
            &bytes,
            LifecycleAdapterInvocation::QualifySnapshotCreate,
            None,
            deadline,
        )?;
        let value: RuntimeSnapshotQualificationAdapterResponse =
            from_json_slice_strict(&output.bytes, MAX_LIFECYCLE_RESPONSE_BYTES)
                .map_err(|_| anyhow::anyhow!("invalid qualification create adapter response"))?;
        value
            .validate_for(request)
            .map_err(|_| anyhow::anyhow!("invalid qualification create adapter response"))?;
        if let RuntimeSnapshotQualificationAdapterResponse::OccurrenceBound { occurrence } = &value
        {
            ensure!(
                !occurrence.contact_deadline_exceeded,
                "qualification adapter claimed daemon-only deadline evidence"
            );
        }
        Ok(ExternalLifecycleObservation {
            value,
            deadline_exceeded: output.deadline_exceeded,
        })
    }

    fn terminate_snapshot_qualification_occurrence(
        &self,
        producer: &crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
        qualification: &crate::node_config::sections::runtime_snapshot_qualification::InstalledRuntimeSnapshotQualificationBinding,
        credential: &PlacementCredential,
        request: &RuntimeSnapshotQualificationTerminationAdapterRequest,
        first_contact: bool,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<ExternalLifecycleObservation<RuntimeSnapshotQualificationTerminationAdapterResponse>>
    {
        self.preflight_snapshot_qualification_create(producer, qualification, credential)?;
        request.validate()?;
        ensure!(
            request.qualification_intent.qualification_profile_digest == qualification.digest()
                && request.qualification_intent.provider_id == producer.backend()
                && request.qualification_intent.adapter_artifact_hash == self.adapter_hash
                && request.qualification_intent.provider_spec_digest == self.provider_spec.sha256
                && request.qualification_intent.settings_digest == qualification.settings_digest()
                && request.qualification_intent.verifier_artifact_hash
                    == qualification.verifier_artifact_hash()
                && request.qualification_intent.maximum_lifetime_seconds
                    == qualification.maximum_lifetime_seconds()
                && request.provider_spec_digest == self.provider_spec.sha256,
            "qualification termination differs from inspected signed authority"
        );
        let deadline = deadline.min(lillux::time::MonotonicDeadline::after(
            lillux::time::Duration::from_secs(u64::from(qualification.contact_timeout_seconds())),
        ));
        let deadline = if first_contact {
            let remaining_ms = request
                .intent
                .attempt_deadline_ms
                .saturating_sub(lillux::time::timestamp_millis());
            ensure!(
                remaining_ms > 0,
                "qualification termination expired before first contact"
            );
            deadline.min(lillux::time::MonotonicDeadline::after(
                lillux::time::Duration::from_millis(u64::try_from(remaining_ms)?),
            ))
        } else {
            deadline
        };
        let bytes = ryeos_external_execution_contract::canonical_json(request)?;
        let invocation = if first_contact {
            LifecycleAdapterInvocation::QualifySnapshotTerminate
        } else {
            LifecycleAdapterInvocation::ObserveSnapshotTermination
        };
        let output = self.invoke_sealed_snapshot_operation(
            producer.network_inputs(),
            &self.provider_spec,
            qualification.settings(),
            qualification.settings_digest(),
            credential,
            &bytes,
            invocation,
            None,
            deadline,
        )?;
        let value: RuntimeSnapshotQualificationTerminationAdapterResponse =
            from_json_slice_strict(&output.bytes, MAX_LIFECYCLE_RESPONSE_BYTES).map_err(|_| {
                anyhow::anyhow!("invalid qualification termination adapter response")
            })?;
        value
            .validate_for(request)
            .map_err(|_| anyhow::anyhow!("invalid qualification termination adapter response"))?;
        if let RuntimeSnapshotQualificationTerminationAdapterResponse::Terminal { observation } =
            &value
        {
            ensure!(
                !observation.contact_deadline_exceeded,
                "qualification adapter claimed daemon-only deadline evidence"
            );
        }
        Ok(ExternalLifecycleObservation {
            value,
            deadline_exceeded: output.deadline_exceeded,
        })
    }

    fn seal_restoration_verifier_upload(
        &self,
        producer: &crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
        qualification: &crate::node_config::sections::runtime_snapshot_qualification::InstalledRuntimeSnapshotQualificationBinding,
    ) -> Result<
        ryeos_external_execution::restoration_verifier_delivery::SealedQualificationVerifierUpload,
    > {
        ensure!(
            producer.backend() == self.declaration.id
                && producer.adapter_artifact_hash() == self.adapter_hash
                && qualification.production_binding_id() == producer.id()
                && qualification.production_binding_digest() == producer.digest()
                && qualification.provider_spec_digest() == self.provider_spec.sha256
                && self.restoration_verifier_hash.as_deref()
                    == Some(qualification.verifier_artifact_hash())
                && self.inspection.observed_provider_spec_sha256 == self.provider_spec.sha256,
            "restoration verifier differs from exact signed product authority"
        );
        let verifier = self
            .restoration_verifier
            .as_ref()
            .context("exact admitted restoration verifier executable is absent")?;
        ryeos_external_execution::restoration_verifier_delivery::seal_restoration_verifier_upload(
            verifier,
            qualification.verifier_artifact_hash(),
        )
    }

    fn verify_restored_snapshot_once(
        &self,
        producer: &crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
        qualification: &crate::node_config::sections::runtime_snapshot_qualification::InstalledRuntimeSnapshotQualificationBinding,
        credential: &PlacementCredential,
        request: &RestoredVerifierAdapterRequest,
        upload: &lillux::InheritedDescriptorAuthority,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<ExternalLifecycleObservation<RestoredVerifierAdapterResponse>> {
        self.preflight_snapshot_qualification_create(producer, qualification, credential)?;
        request.validate()?;
        let verifier_selection_matches = match &request.intent.purpose {
            ryeos_external_execution_contract::restored_runtime_measurement::RemoteVerificationPurpose::OwnerMeasurement { .. } => {
                request.consumer_selection.is_none()
                    && request.intent.verifier_artifact_hash == qualification.verifier_artifact_hash()
            }
            ryeos_external_execution_contract::restored_runtime_measurement::RemoteVerificationPurpose::ConsumerRuntime { coordinate, .. } => {
                request.consumer_selection.as_ref() == Some(qualification.consumer_verifier(coordinate)?)
            }
        };
        ensure!(
            request.qualification_intent.qualification_profile_digest == qualification.digest()
                && request.qualification_intent.adapter_artifact_hash == self.adapter_hash
                && request.qualification_intent.provider_spec_digest == self.provider_spec.sha256
                && request.qualification_intent.settings_digest == qualification.settings_digest()
                && verifier_selection_matches
                && request.source_intent.production_profile_digest == producer.digest()
                && request.upload_descriptor
                    == upload.inherited_descriptor().map_err(anyhow::Error::msg)?,
            "restored verifier contact differs from inspected signed authority"
        );
        upload.require_owned_regular()?;
        let observation = upload.regular_file_observation()?;
        ensure!(
            observation.full_permission_mode()? == request.required_upload_mode()
                && observation.size() == request.upload_bytes
                && upload.digest_regular_file_stable_exact(&observation)? == request.upload_sha256,
            "restored verifier upload changed before adapter invocation"
        );
        let remaining_ms = request
            .intent
            .attempt_deadline_ms
            .saturating_sub(lillux::time::timestamp_millis());
        ensure!(
            remaining_ms > 0,
            "restored verifier expired before adapter contact"
        );
        let deadline = deadline
            .min(lillux::time::MonotonicDeadline::after(
                lillux::time::Duration::from_secs(u64::from(
                    qualification.contact_timeout_seconds(),
                )),
            ))
            .min(lillux::time::MonotonicDeadline::after(
                lillux::time::Duration::from_millis(u64::try_from(remaining_ms)?),
            ));
        let bytes = ryeos_external_execution_contract::canonical_json(request)?;
        let output = self.invoke_sealed_snapshot_operation(
            producer.network_inputs(),
            &self.provider_spec,
            qualification.settings(),
            qualification.settings_digest(),
            credential,
            &bytes,
            LifecycleAdapterInvocation::QualifySnapshotVerify,
            Some(upload),
            deadline,
        )?;
        let value: RestoredVerifierAdapterResponse =
            from_json_slice_strict(&output.bytes, MAX_RESTORED_VERIFIER_ADAPTER_RESPONSE_BYTES)
                .map_err(|_| anyhow::anyhow!("invalid restored verifier adapter response"))?;
        value
            .validate_for(request)
            .map_err(|_| anyhow::anyhow!("invalid restored verifier adapter response"))?;
        if let RestoredVerifierAdapterResponse::Observed { observation } = &value {
            ensure!(
                !observation.contact_deadline_exceeded,
                "restored verifier adapter claimed daemon-only deadline evidence"
            );
        }
        if let RestoredVerifierAdapterResponse::ConsumerObserved { observation, .. } = &value {
            ensure!(
                !observation.contact_deadline_exceeded,
                "consumer verifier adapter claimed daemon-only deadline evidence"
            );
        }
        Ok(ExternalLifecycleObservation {
            value,
            deadline_exceeded: output.deadline_exceeded,
        })
    }

    fn produce_runtime_snapshot(
        &self,
        binding: &crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
        credential: &PlacementCredential,
        request: &RuntimeSnapshotAdapterRequest,
        upload: &lillux::InheritedDescriptorAuthority,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<ExternalLifecycleObservation<RuntimeSnapshotAdapterResponse>> {
        self.preflight_runtime_snapshot(binding, credential)?;
        request.validate()?;
        let spec = self
            .snapshot_production_spec
            .as_ref()
            .context("installed lifecycle adapter has no signed snapshot production profile")?;
        ensure!(
            binding.backend() == self.declaration.id
                && binding.adapter_artifact_hash() == self.adapter_hash
                && binding.snapshot_spec_sha256() == spec.sha256
                && self
                    .inspection
                    .observed_snapshot_production_spec_sha256
                    .as_deref()
                    == Some(spec.sha256.as_str())
                && request.intent.provider_id == self.declaration.id
                && request.intent.adapter_artifact_hash == self.adapter_hash
                && request.provider_spec_digest == spec.sha256
                && request.intent.production_profile_digest == binding.digest()
                && request.intent.settings_digest == binding.settings_digest()
                && credential.backend() == binding.backend()
                && credential.account() == binding.account()
                && request.upload_bytes <= binding.maximum_upload_bytes()
                && request.upload_descriptor
                    == upload.inherited_descriptor().map_err(anyhow::Error::msg)?,
            "snapshot attempt differs from its inspected signed producer authority"
        );
        let now = lillux::time::timestamp_millis();
        let remaining_ms = request.intent.attempt_deadline_ms.saturating_sub(now);
        ensure!(
            remaining_ms > 0,
            "snapshot attempt expired before adapter contact"
        );
        let deadline = deadline
            .min(lillux::time::MonotonicDeadline::after(
                lillux::time::Duration::from_secs(u64::from(binding.contact_timeout_seconds())),
            ))
            .min(lillux::time::MonotonicDeadline::after(
                lillux::time::Duration::from_millis(u64::try_from(remaining_ms)?),
            ));
        let request_bytes = ryeos_external_execution_contract::canonical_json(request)?;
        let output = self.invoke_snapshot_adapter(
            binding,
            credential,
            &request_bytes,
            LifecycleAdapterInvocation::ProduceSnapshot,
            Some(upload),
            deadline,
        )?;
        let value: RuntimeSnapshotAdapterResponse =
            from_json_slice_strict(&output.bytes, MAX_LIFECYCLE_RESPONSE_BYTES)
                .map_err(|_| anyhow::anyhow!("invalid runtime snapshot adapter response"))?;
        value
            .validate_for(request)
            .map_err(|_| anyhow::anyhow!("invalid runtime snapshot adapter response"))?;
        Ok(ExternalLifecycleObservation {
            value,
            deadline_exceeded: output.deadline_exceeded,
        })
    }

    fn upload_runtime_snapshot(
        &self,
        binding: &crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
        credential: &PlacementCredential,
        request: &RuntimeSnapshotUploadAdapterRequest,
        upload: &lillux::InheritedDescriptorAuthority,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<ExternalLifecycleObservation<RuntimeSnapshotUploadReceipt>> {
        self.preflight_runtime_snapshot(binding, credential)?;
        request.validate()?;
        ensure!(
            request.intent.provider_id == self.declaration.id
                && request.intent.adapter_artifact_hash == self.adapter_hash
                && request.provider_spec_digest == binding.snapshot_spec_sha256()
                && request.intent.production_profile_digest == binding.digest()
                && request.intent.settings_digest == binding.settings_digest()
                && request.upload_bytes <= binding.maximum_upload_bytes()
                && request.upload_descriptor
                    == upload.inherited_descriptor().map_err(anyhow::Error::msg)?,
            "snapshot upload differs from its inspected signed producer authority"
        );
        let remaining_ms = request
            .stage
            .attempt_deadline_ms
            .saturating_sub(lillux::time::timestamp_millis());
        ensure!(
            remaining_ms > 0,
            "snapshot upload expired before adapter contact"
        );
        let deadline = deadline
            .min(lillux::time::MonotonicDeadline::after(
                lillux::time::Duration::from_secs(u64::from(binding.contact_timeout_seconds())),
            ))
            .min(lillux::time::MonotonicDeadline::after(
                lillux::time::Duration::from_millis(u64::try_from(remaining_ms)?),
            ));
        let output = self.invoke_snapshot_adapter(
            binding,
            credential,
            &ryeos_external_execution_contract::canonical_json(request)?,
            LifecycleAdapterInvocation::ProduceSnapshotUpload,
            Some(upload),
            deadline,
        )?;
        let value: RuntimeSnapshotUploadReceipt =
            from_json_slice_strict(&output.bytes, MAX_LIFECYCLE_RESPONSE_BYTES)
                .map_err(|_| anyhow::anyhow!("invalid snapshot upload adapter receipt"))?;
        value.validate_observation_for_stage(&request.intent, &request.stage)?;
        Ok(ExternalLifecycleObservation {
            value,
            deadline_exceeded: output.deadline_exceeded,
        })
    }

    fn create_runtime_snapshot(
        &self,
        binding: &crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
        credential: &PlacementCredential,
        request: &RuntimeSnapshotCreateAdapterRequest,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<ExternalLifecycleObservation<RuntimeSnapshotCreateResult>> {
        self.preflight_runtime_snapshot(binding, credential)?;
        request.validate()?;
        ensure!(
            request.intent.provider_id == self.declaration.id
                && request.intent.adapter_artifact_hash == self.adapter_hash
                && request.provider_spec_digest == binding.snapshot_spec_sha256()
                && request.intent.production_profile_digest == binding.digest()
                && request.intent.settings_digest == binding.settings_digest(),
            "snapshot create differs from its inspected signed producer authority"
        );
        let remaining_ms = request
            .create_stage
            .attempt_deadline_ms
            .saturating_sub(lillux::time::timestamp_millis());
        ensure!(
            remaining_ms > 0,
            "snapshot create expired before adapter contact"
        );
        let deadline = deadline
            .min(lillux::time::MonotonicDeadline::after(
                lillux::time::Duration::from_secs(u64::from(binding.contact_timeout_seconds())),
            ))
            .min(lillux::time::MonotonicDeadline::after(
                lillux::time::Duration::from_millis(u64::try_from(remaining_ms)?),
            ));
        let output = self.invoke_snapshot_adapter(
            binding,
            credential,
            &ryeos_external_execution_contract::canonical_json(request)?,
            LifecycleAdapterInvocation::ProduceSnapshotCreate,
            None,
            deadline,
        )?;
        let value: RuntimeSnapshotCreateResult =
            from_json_slice_strict(&output.bytes, MAX_LIFECYCLE_RESPONSE_BYTES)
                .map_err(|_| anyhow::anyhow!("invalid snapshot create adapter result"))?;
        value.validate_for(request)?;
        Ok(ExternalLifecycleObservation {
            value,
            deadline_exceeded: output.deadline_exceeded,
        })
    }

    fn observe_runtime_snapshot_readiness(
        &self,
        binding: &crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
        credential: &PlacementCredential,
        request: &RuntimeSnapshotReadinessRequest,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<ExternalLifecycleObservation<RuntimeSnapshotReadinessObservation>> {
        self.preflight_runtime_snapshot(binding, credential)?;
        request.validate()?;
        ensure!(
            request.intent.provider_id == self.declaration.id
                && request.intent.adapter_artifact_hash == self.adapter_hash
                && request.intent.production_profile_digest == binding.digest()
                && request.intent.settings_digest == binding.settings_digest()
                && request.provider_spec_digest == binding.snapshot_spec_sha256(),
            "snapshot readiness differs from its inspected signed producer authority"
        );
        let deadline = deadline.min(lillux::time::MonotonicDeadline::after(
            lillux::time::Duration::from_secs(u64::from(binding.contact_timeout_seconds())),
        ));
        let request_bytes = ryeos_external_execution_contract::canonical_json(request)?;
        let output = self.invoke_snapshot_adapter(
            binding,
            credential,
            &request_bytes,
            LifecycleAdapterInvocation::ObserveSnapshotReadiness,
            None,
            deadline,
        )?;
        let value: RuntimeSnapshotReadinessObservation =
            from_json_slice_strict(&output.bytes, MAX_LIFECYCLE_RESPONSE_BYTES)
                .map_err(|_| anyhow::anyhow!("invalid runtime snapshot readiness response"))?;
        value
            .validate_for(request)
            .map_err(|_| anyhow::anyhow!("invalid runtime snapshot readiness response"))?;
        Ok(ExternalLifecycleObservation {
            value,
            deadline_exceeded: output.deadline_exceeded,
        })
    }

    fn lifecycle_capabilities(
        &self,
    ) -> std::collections::BTreeSet<ryeos_external_execution_contract::LifecycleCapability> {
        self.inspection.effective_capabilities.clone()
    }

    fn qualify_offline(
        &self,
        contract: &ExternalPlacementBackendContract,
        credential: &PlacementCredential,
    ) -> Result<()> {
        ensure!(
            contract.backend == self.declaration.id
                && contract.backend_artifact_hash == self.adapter_hash
                && contract.backend_artifact_bytes == self.adapter_bytes
                && contract.supervisor_artifact_hash == self.supervisor_hash
                && contract.supervisor_artifact_bytes == self.supervisor_bytes
                && contract.launcher_artifact_hash == self.launcher_hash
                && contract.launcher_artifact_bytes == self.launcher_bytes
                && contract.settings_schema_digest == self.declaration.settings_schema_digest
                && self.inspection.effective_capabilities == self.declaration.capabilities
                && credential.backend() == contract.backend
                && credential.account() == contract.account,
            "signed external lifecycle generation contradicts the protected placement binding"
        );
        Ok(())
    }

    fn verify_runtime_probe(
        &self,
        contract: &ExternalPlacementBackendContract,
        proof: &RuntimeProbeInput,
        source: &ryeos_external_execution::guest_runtime_product::GuestOwnerRuntimeManifestIdentity,
        binding_hash: &str,
    ) -> Result<()> {
        ensure!(
            contract.backend == self.declaration.id
                && contract.backend_artifact_hash == self.adapter_hash
                && contract.backend_artifact_bytes == self.adapter_bytes
                && contract.settings_schema_digest == self.declaration.settings_schema_digest
                && source.manifest_hash == contract.guest_runtime_manifest_hash
                && source.manifest_hash == proof.subject_manifest_hash,
            "runtime probe changed its signed adapter or authenticated source"
        );
        let settings_bytes = ryeos_external_execution_contract::canonical_json(&contract.settings)?;
        ensure!(
            lillux::sha256_hex(&settings_bytes) == contract.settings_digest,
            "runtime probe settings changed their signed digest"
        );
        let request = LifecycleRuntimeProbeRequest {
            schema: 2,
            protocol: LIFECYCLE_ADAPTER_PROTOCOL.into(),
            adapter_id: self.declaration.id.clone(),
            adapter_artifact_hash: self.adapter_hash.clone(),
            settings_digest: contract.settings_digest.clone(),
            binding_hash: binding_hash.to_owned(),
            qualification_attestation_hash: proof.qualification_attestation_hash.clone(),
            runtime_source: proof.runtime_source.clone(),
            account: contract.account.clone(),
            source: LifecycleRuntimeProbeSource {
                manifest_hash: source.manifest_hash.clone(),
                owner_executable_sha256: source.owner_executable_sha256.clone(),
                controller_root_blob_sha256: source.controller_root_blob_sha256.clone(),
                controller_public_root: source.controller_public_root.clone(),
            },
            probe_evidence: proof.probe_evidence.clone(),
        };
        let request_handle = lillux::sealed_memfd(
            c"ryeos-lifecycle-runtime-probe-request",
            &request.canonical_bytes()?,
        )
        .map_err(anyhow::Error::msg)?;
        let settings_handle =
            lillux::sealed_memfd(c"ryeos-lifecycle-runtime-probe-settings", &settings_bytes)
                .map_err(anyhow::Error::msg)?;
        let deadline = lillux::time::MonotonicDeadline::after(
            lillux::time::Duration::from_secs_f64(LIFECYCLE_ADAPTER_INSPECTION_TIMEOUT_SECONDS),
        );
        let response = run_lifecycle_adapter(
            &self.adapter,
            LifecycleAdapterInvocation::VerifyRuntimeProbe,
            &request_handle,
            vec![settings_handle.clone()],
            vec![(
                LIFECYCLE_SETTINGS_FD_ENV.into(),
                settings_handle
                    .inherited_descriptor()
                    .map_err(anyhow::Error::msg)?
                    .to_string(),
            )],
            deadline,
        )?;
        let decoded: LifecycleRuntimeProbeResponse =
            from_json_slice_strict(&response.bytes, MAX_LIFECYCLE_RESPONSE_BYTES)
                .map_err(|_| anyhow::anyhow!("invalid lifecycle runtime probe response"))?;
        decoded
            .validate_for(&request)
            .map_err(|_| anyhow::anyhow!("invalid lifecycle runtime probe response"))
    }

    #[allow(clippy::too_many_arguments)]
    fn prepare_guest_package(
        &self,
        contract: &ExternalPlacementBackendContract,
        reservation: &ExternalAllocationReservation,
        occurrence: &ExternalAllocationOccurrence,
        bootstrap: &ryeos_state::external_execution::transport::ExternalSupervisorBootstrap,
        guest_inputs: &ryeos_external_execution::guest_inputs::ExternalGuestInputAuthority,
        activation_request_digest: &str,
        parent: &lillux::PinnedDirectory,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<SupervisorGuestPackage> {
        let bootstrap_bytes = bootstrap.canonical_bytes()?;
        let guest_input_identity = guest_inputs.identity_digest()?;
        let bootstrap_sha256 = lillux::sha256_hex(&bootstrap_bytes);
        let bootstrap_file =
            lillux::sealed_memfd(c"ryeos-external-guest-bootstrap", &bootstrap_bytes)
                .map_err(anyhow::Error::msg)?;
        let expected = GuestStagingExpected {
            inputs: guest_inputs.projection(),
            activation_request_digest,
            bootstrap_sha256: &bootstrap_sha256,
            supervisor_sha256: &self.supervisor_hash,
            launcher_sha256: &self.launcher_hash,
            maximum_regular_bytes: contract.max_guest_package_regular_bytes,
            maximum_framed_bytes: contract.max_guest_package_framed_bytes,
        };
        let package =
            ryeos_external_execution::guest_package_producer::prepare_private_guest_package(
                parent,
                guest_inputs,
                &bootstrap_file,
                &self.supervisor,
                &self.launcher,
                &expected,
                deadline,
            )?;
        let commitment = ExternalGuestPackageDeliveryCommitment {
            schema: 1,
            binding_hash: reservation.binding_hash.clone(),
            request_digest: reservation.request_digest.clone(),
            occurrence_id: occurrence.occurrence_id.clone(),
            activation_request_digest: activation_request_digest.to_owned(),
            guest_input_identity,
            manifest_sha256: package.manifest_sha256().to_owned(),
            payload_sha256: package.sha256().to_owned(),
            regular_bytes: package.manifest().total_regular_bytes,
            framed_bytes: package.bytes(),
        };
        Ok(SupervisorGuestPackage::Prepared {
            package,
            commitment,
        })
    }

    fn allocate(
        &self,
        contract: &ExternalPlacementBackendContract,
        credential: &PlacementCredential,
        reservation: &ExternalAllocationReservation,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<ExternalLifecycleObservation<ExternalAllocationResolution>> {
        self.allocation_operation(contract, credential, reservation, false, deadline)
    }

    fn reconcile_allocation(
        &self,
        contract: &ExternalPlacementBackendContract,
        credential: &PlacementCredential,
        reservation: &ExternalAllocationReservation,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<ExternalLifecycleObservation<ExternalAllocationResolution>> {
        self.allocation_operation(contract, credential, reservation, true, deadline)
    }

    fn activate_supervisor(
        &self,
        contract: &ExternalPlacementBackendContract,
        credential: &PlacementCredential,
        reservation: &ExternalAllocationReservation,
        occurrence: &ExternalAllocationOccurrence,
        intent: &ExternalSupervisorActivationIntent,
        activation: &ExternalSupervisorActivation,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<ExternalLifecycleObservation<ExternalSupervisorActivationResolution>> {
        self.activation_operation(
            contract,
            credential,
            reservation,
            occurrence,
            intent,
            Some(activation),
            false,
            deadline,
        )
    }

    fn reconcile_supervisor_activation(
        &self,
        contract: &ExternalPlacementBackendContract,
        credential: &PlacementCredential,
        reservation: &ExternalAllocationReservation,
        occurrence: &ExternalAllocationOccurrence,
        intent: &ExternalSupervisorActivationIntent,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<ExternalLifecycleObservation<ExternalSupervisorActivationResolution>> {
        self.activation_operation(
            contract,
            credential,
            reservation,
            occurrence,
            intent,
            None,
            true,
            deadline,
        )
    }

    fn terminate(
        &self,
        contract: &ExternalPlacementBackendContract,
        credential: &PlacementCredential,
        reservation: &ExternalAllocationReservation,
        occurrence: &ExternalAllocationOccurrence,
        intent: &ExternalTerminationIntent,
    ) -> Result<ExternalTerminationResolution> {
        self.termination_operation(contract, credential, reservation, occurrence, intent, false)
    }

    fn reconcile_termination(
        &self,
        contract: &ExternalPlacementBackendContract,
        credential: &PlacementCredential,
        reservation: &ExternalAllocationReservation,
        occurrence: &ExternalAllocationOccurrence,
        intent: &ExternalTerminationIntent,
    ) -> Result<ExternalTerminationResolution> {
        self.termination_operation(contract, credential, reservation, occurrence, intent, true)
    }
}

impl ExecutableExternalPlacementBackend {
    /// Stage-one lease admission. The adapter receives the sealed request and
    /// the captured signed provider spec, nothing else: no credential,
    /// settings, network inputs or activation material, so its verdict can
    /// cost no provider contact.
    fn invoke_admit_lease(
        &self,
        inputs: &lease_admission::LeaseAdmissionInputs,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<lease_admission::AdmittedLease> {
        let request = lease_admission::canonical_request(
            &self.declaration.id,
            &self.provider_spec.sha256,
            inputs,
        )?;
        let request_handle = lillux::sealed_memfd(c"ryeos-lease-admission-request", &request)
            .map_err(anyhow::Error::msg)?;
        let envs = vec![
            (
                LIFECYCLE_PROVIDER_SPEC_FD_ENV.into(),
                self.provider_spec
                    .authority
                    .inherited_descriptor()
                    .map_err(anyhow::Error::msg)?
                    .to_string(),
            ),
            (
                LIFECYCLE_PROVIDER_SPEC_SHA256_ENV.into(),
                self.provider_spec.sha256.clone(),
            ),
        ];
        let response = run_lifecycle_adapter(
            &self.adapter,
            LifecycleAdapterInvocation::AdmitLease,
            &request_handle,
            vec![self.provider_spec.authority.clone()],
            envs,
            deadline,
        )
        .with_context(|| {
            format!(
                "lease admission refused: lifecycle adapter `{}` gave no verdict on workload `{}`",
                self.declaration.id, inputs.workload
            )
        })?;
        let budget_weight = lease_admission::require_admitted(
            &response.bytes,
            &request,
            &self.provider_spec.sha256,
            &self.declaration.id,
        )?;
        Ok(lease_admission::AdmittedLease {
            request,
            budget_weight,
        })
    }

    /// Lease readiness judgement. The adapter receives the sealed readiness
    /// request, the captured signed provider spec and one inherited duplex
    /// descriptor carrying exactly the guest's Ready and capability report,
    /// each length-prefixed, then end of input. It holds no credential,
    /// settings or activation material and the enclosure denies it process
    /// creation. Returns the adapter's raw verdict bytes for the caller to
    /// read against the lease record; a failure to obtain any verdict is an
    /// error, never a verdict.
    fn invoke_await_lease_ready(
        &self,
        request: &[u8],
        ready_frame: &[u8],
        report_frame: &[u8],
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<Vec<u8>> {
        let ready = ryeos_external_execution_contract::length_prefixed::encode(
            ready_frame,
            MAX_LIFECYCLE_GUEST_FRAME_MESSAGE_BYTES,
        )?;
        let report = ryeos_external_execution_contract::length_prefixed::encode(
            report_frame,
            MAX_LIFECYCLE_GUEST_FRAME_MESSAGE_BYTES,
        )?;
        let request_handle = lillux::sealed_memfd(c"ryeos-lease-ready-request", request)
            .map_err(anyhow::Error::msg)?;
        let (mut frames, child) =
            lillux::inherited_duplex_channel_pair().map_err(anyhow::Error::msg)?;
        let mut inherited = vec![self.provider_spec.authority.clone()];
        child.retain_for_child(&mut inherited);
        let envs = vec![
            (
                LIFECYCLE_PROVIDER_SPEC_FD_ENV.into(),
                self.provider_spec
                    .authority
                    .inherited_descriptor()
                    .map_err(anyhow::Error::msg)?
                    .to_string(),
            ),
            (
                LIFECYCLE_PROVIDER_SPEC_SHA256_ENV.into(),
                self.provider_spec.sha256.clone(),
            ),
            (
                LIFECYCLE_REMAINING_TIMEOUT_MS_ENV.into(),
                u64::try_from(deadline.remaining().as_millis())?.to_string(),
            ),
            (
                LIFECYCLE_GUEST_FRAMES_FD_ENV.into(),
                child.inherited_descriptor().map_err(anyhow::Error::msg)?.to_string(),
            ),
        ];
        // The child's copy travels in `inherited`; dropping the parent's copy
        // leaves the adapter the only reader of its end.
        drop(child);
        let result = std::thread::scope(|scope| {
            // The writer runs beside the adapter so neither waits on the
            // other: it writes both frames, then closes its direction so the
            // adapter sees end of input. A write error after the adapter has
            // answered and gone is not a failure; only the verdict counts.
            scope.spawn(move || {
                use std::io::Write as _;
                let mut stream = frames.with_deadline(deadline);
                if stream.write_all(&ready).is_ok() && stream.write_all(&report).is_ok() {
                    let _ = stream.flush();
                }
                let _ = frames.shutdown_write();
            });
            run_lifecycle_adapter(
                &self.adapter,
                LifecycleAdapterInvocation::AwaitLeaseReady,
                &request_handle,
                inherited,
                envs,
                deadline,
            )
        });
        let response = result.with_context(|| {
            format!(
                "lease readiness refused: lifecycle adapter `{}` gave no verdict",
                self.declaration.id
            )
        })?;
        ensure!(
            !response.deadline_exceeded,
            "lease readiness refused: lifecycle adapter `{}` answered after its deadline",
            self.declaration.id
        );
        Ok(response.bytes)
    }

    fn common(
        contract: &ExternalPlacementBackendContract,
        binding_hash: &str,
        operation_id: &str,
    ) -> LifecycleOperationCommon {
        LifecycleOperationCommon {
            schema: 1,
            protocol: LIFECYCLE_ADAPTER_PROTOCOL.into(),
            operation_id: operation_id.into(),
            binding_hash: binding_hash.into(),
            settings_digest: contract.settings_digest.clone(),
        }
    }

    fn allocation_operation(
        &self,
        contract: &ExternalPlacementBackendContract,
        credential: &PlacementCredential,
        reservation: &ExternalAllocationReservation,
        reconcile: bool,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<ExternalLifecycleObservation<ExternalAllocationResolution>> {
        let reservation_wire = AllocationReservation {
            placement_thread_id: reservation.placement_thread_id.clone(),
            admitted_capsule_hash: reservation.admitted_capsule_hash.clone(),
            base_snapshot_hash: reservation.base_snapshot_hash.clone(),
            request_digest: reservation.request_digest.clone(),
            maximum_lifetime_seconds: reservation.timeout_seconds,
            contact_deadline_ms: reservation.contact_deadline_ms,
        };
        let common = Self::common(
            contract,
            &reservation.binding_hash,
            &reservation.request_digest,
        );
        let request = if reconcile {
            LifecycleAdapterRequest::ReconcileAllocation {
                common,
                reservation: reservation_wire,
            }
        } else {
            LifecycleAdapterRequest::Allocate {
                common,
                reservation: reservation_wire,
            }
        };
        let admitted_adapter_request = reservation.admitted_adapter_request()?;
        let response = self.invoke(
            contract,
            credential,
            &request,
            None,
            None,
            None,
            None,
            None,
            None,
            admitted_adapter_request.as_deref(),
            deadline,
        )?;
        let value = match response.value {
            LifecycleAdapterResponse::AllocationBound {
                occurrence_id,
                provider_observation_digest,
                ..
            } => ExternalAllocationResolution::Bound {
                occurrence_id,
                provider_observation_digest,
            },
            LifecycleAdapterResponse::AllocationNoOccurrence {
                provider_observation_digest,
                ..
            } => ExternalAllocationResolution::NoOccurrence {
                provider_observation_digest,
            },
            LifecycleAdapterResponse::AllocationPending { .. } => {
                ExternalAllocationResolution::Pending
            }
            _ => anyhow::bail!("lifecycle adapter returned a non-allocation response"),
        };
        Ok(ExternalLifecycleObservation {
            value,
            deadline_exceeded: response.deadline_exceeded,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn activation_operation(
        &self,
        contract: &ExternalPlacementBackendContract,
        credential: &PlacementCredential,
        reservation: &ExternalAllocationReservation,
        occurrence: &ExternalAllocationOccurrence,
        intent: &ExternalSupervisorActivationIntent,
        activation_authority: Option<&ExternalSupervisorActivation>,
        reconcile: bool,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<ExternalLifecycleObservation<ExternalSupervisorActivationResolution>> {
        let occurrence_wire = BoundOccurrence {
            request_digest: reservation.request_digest.clone(),
            occurrence_id: occurrence.occurrence_id.clone(),
        };
        let guest_runtime = contact_guest_runtime(
            activation_authority.and_then(ExternalSupervisorActivation::guest_runtime_upload),
            reconcile,
        );
        let activation = SupervisorActivationIntent {
            activation_request_digest: intent.activation_request_digest.clone(),
            supervisor_runtime_hash: intent.supervisor_runtime_hash.clone(),
            launcher_artifact_hash: contract.launcher_artifact_hash.clone(),
            guest_runtime_artifact_hash: guest_runtime.map(|upload| upload.sha256().to_owned()),
            guest_runtime_artifact_bytes: guest_runtime.map(|upload| upload.bytes()),
            attachment_deadline_ms: intent.attachment_deadline_ms,
            execution_timeout_seconds: intent.execution_timeout_seconds,
            post_execution_timeout_seconds: intent.post_execution_timeout_seconds,
            channel_max_bytes: intent.channel_max_bytes,
        };
        let common = Self::common(
            contract,
            &reservation.binding_hash,
            &intent.activation_request_digest,
        );
        let (package_authority, import_ticket) = if reconcile {
            (None, None)
        } else {
            let activation_authority = activation_authority
                .context("first lifecycle activation has no guest package authority")?;
            let package = activation_authority.prepared_guest_package()?;
            ensure!(
                package.sha256() == intent.delivery.payload_sha256
                    && package.manifest_sha256() == intent.delivery.manifest_sha256
                    && package.bytes() == intent.delivery.framed_bytes
                    && package.manifest().total_regular_bytes == intent.delivery.regular_bytes,
                "prepared guest package changed after durable activation claim"
            );
            let signed = activation_authority.signed_import_authorization()?;
            let authorization = &signed.authorization;
            ensure!(
                authorization.placement_thread_id == reservation.placement_thread_id
                    && authorization.admitted_capsule_hash == reservation.admitted_capsule_hash
                    && authorization.base_snapshot_hash == reservation.base_snapshot_hash
                    && authorization.execution_binding_hash == reservation.binding_hash
                    && authorization.activation_request_digest == intent.activation_request_digest
                    && authorization.supervisor_runtime_hash == intent.supervisor_runtime_hash
                    && authorization.guest_runtime_manifest_hash
                        == contract.guest_runtime_manifest_hash
                    && authorization.guest_inputs
                        == *activation_authority.guest_inputs().projection(),
                "signed guest import changed the admitted activation"
            );
            let ticket = authorization.ticket.clone();
            ensure!(
                ticket.guest_input_identity == intent.guest_input_identity
                    && ticket.payload_sha256 == intent.delivery.payload_sha256
                    && ticket.manifest_sha256 == intent.delivery.manifest_sha256
                    && ticket.regular_bytes == intent.delivery.regular_bytes
                    && ticket.framed_bytes == intent.delivery.framed_bytes
                    && ticket.bootstrap_sha256
                        == lillux::sha256_hex(&activation_authority.bootstrap().canonical_bytes()?)
                    && ticket.supervisor_sha256 == contract.supervisor_artifact_hash
                    && ticket.launcher_sha256 == contract.launcher_artifact_hash,
                "signed guest import ticket changed durable delivery or executable roots"
            );
            ticket.validate_verified_manifest(
                &ryeos_external_execution_contract::staging_package::GuestImportContext {
                    binding_hash: &reservation.binding_hash,
                    allocation_request_digest: &reservation.request_digest,
                    occurrence_id: &occurrence.occurrence_id,
                    activation_request_digest: &intent.activation_request_digest,
                },
                package.manifest(),
                package.sha256(),
                package.bytes(),
            )?;
            (Some(package.delivery_descriptor()?), Some(ticket))
        };
        let request = if reconcile {
            LifecycleAdapterRequest::ReconcileSupervisorActivation {
                common,
                occurrence: occurrence_wire,
                activation,
            }
        } else {
            LifecycleAdapterRequest::ActivateSupervisor {
                common,
                occurrence: occurrence_wire,
                activation,
                guest_input_identity: intent.guest_input_identity.clone(),
                guest_input_projection: Box::new(
                    activation_authority
                        .context("first activation lost its guest input authority")?
                        .guest_inputs()
                        .projection()
                        .clone(),
                ),
                import_ticket: Box::new(
                    import_ticket.context("first activation lost its import ticket")?,
                ),
                guest_package: LifecycleGuestPackageDelivery {
                    descriptor: package_authority
                        .as_ref()
                        .expect("first activation owns a prepared package")
                        .inherited_descriptor()
                        .map_err(anyhow::Error::msg)?,
                    payload_sha256: intent.delivery.payload_sha256.clone(),
                    manifest_sha256: intent.delivery.manifest_sha256.clone(),
                    regular_bytes: intent.delivery.regular_bytes,
                    framed_bytes: intent.delivery.framed_bytes,
                },
            }
        };
        let bootstrap = activation_authority.map(ExternalSupervisorActivation::bootstrap);
        let guest_inputs = activation_authority.map(ExternalSupervisorActivation::guest_inputs);
        let response = self.invoke(
            contract,
            credential,
            &request,
            bootstrap,
            guest_inputs,
            package_authority.as_ref(),
            guest_runtime.map(|upload| upload.descriptor()),
            activation_authority
                .map(ExternalSupervisorActivation::signed_import_authorization)
                .transpose()?,
            activation_authority
                .map(ExternalSupervisorActivation::signed_assignment)
                .transpose()?,
            reservation.admitted_adapter_request()?.as_deref(),
            deadline,
        )?;
        let value = match response.value {
            LifecycleAdapterResponse::SupervisorStarted {
                provider_observation_digest,
                ..
            } => ExternalSupervisorActivationResolution::Started {
                provider_observation_digest,
            },
            LifecycleAdapterResponse::SupervisorNotStarted {
                provider_observation_digest,
                ..
            } => ExternalSupervisorActivationResolution::NotStarted {
                provider_observation_digest,
            },
            LifecycleAdapterResponse::SupervisorPending { .. } => {
                ExternalSupervisorActivationResolution::Pending
            }
            _ => anyhow::bail!("lifecycle adapter returned a non-activation response"),
        };
        Ok(ExternalLifecycleObservation {
            value,
            deadline_exceeded: response.deadline_exceeded,
        })
    }

    fn termination_operation(
        &self,
        contract: &ExternalPlacementBackendContract,
        credential: &PlacementCredential,
        reservation: &ExternalAllocationReservation,
        occurrence: &ExternalAllocationOccurrence,
        intent: &ExternalTerminationIntent,
        reconcile: bool,
    ) -> Result<ExternalTerminationResolution> {
        let common = Self::common(
            contract,
            &reservation.binding_hash,
            &intent.termination_request_digest,
        );
        let occurrence_wire = BoundOccurrence {
            request_digest: reservation.request_digest.clone(),
            occurrence_id: occurrence.occurrence_id.clone(),
        };
        let termination = TerminationIntent {
            termination_request_digest: intent.termination_request_digest.clone(),
        };
        let request = if reconcile {
            LifecycleAdapterRequest::ReconcileTermination {
                common,
                occurrence: occurrence_wire,
                termination,
            }
        } else {
            LifecycleAdapterRequest::Terminate {
                common,
                occurrence: occurrence_wire,
                termination,
            }
        };
        let deadline = lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(
            u64::from(contract.cleanup_timeout_seconds),
        ));
        let admitted_adapter_request = reservation.admitted_adapter_request()?;
        // Late terminal evidence may settle cleanup, never start execution.
        match self
            .invoke(
                contract,
                credential,
                &request,
                None,
                None,
                None,
                None,
                None,
                None,
                admitted_adapter_request.as_deref(),
                deadline,
            )?
            .value
        {
            LifecycleAdapterResponse::OccurrenceTerminal {
                provider_observation_digest,
                ..
            } => Ok(ExternalTerminationResolution::Terminal {
                provider_observation_digest,
            }),
            LifecycleAdapterResponse::TerminationPending { .. } => {
                Ok(ExternalTerminationResolution::Pending)
            }
            _ => anyhow::bail!("lifecycle adapter returned a non-termination response"),
        }
    }
}

/// Provider spec operation kind under which activation uploads the guest
/// runtime itself. The daemon keys on this string alone and reads no other
/// meaning from the spec.
const STAGE_THEN_UPLOAD_ACTIVATION_KIND: &str = "stage_then_upload_then_run_once";

fn provider_spec_stages_guest_runtime(spec: &CapturedLifecycleProviderSpec) -> Result<bool> {
    let bytes = spec
        .authority
        .read_regular_file_stable_bounded(spec.bytes)?
        .0;
    Ok(spec_declares_staged_activation(&bytes))
}

/// An unparsable spec declares nothing; the adapter refuses it itself.
fn spec_declares_staged_activation(spec_bytes: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(spec_bytes)
        .ok()
        .is_some_and(|spec| {
            spec.pointer("/operations/activate_supervisor/kind")
                .and_then(serde_json::Value::as_str)
                == Some(STAGE_THEN_UPLOAD_ACTIVATION_KIND)
        })
}

/// Only a first activation uploads a runtime; reconciliation carries none,
/// even if the authority it was handed still holds one.
fn contact_guest_runtime<T>(held: Option<T>, reconcile: bool) -> Option<T> {
    held.filter(|_| !reconcile)
}

/// The runtime descriptor crosses exactly when the provider declares a staged
/// activation and the guest package crosses with it. Any other descriptor is
/// refused rather than ignored, and a delivered one takes the same
/// credential-home identity check as the package.
fn guest_runtime_delivery<'a>(
    credential_home_root: &std::path::Path,
    declared: bool,
    activating: bool,
    runtime: Option<&'a lillux::InheritedDescriptorAuthority>,
) -> Result<Option<(String, String, &'a lillux::InheritedDescriptorAuthority)>> {
    ensure!(
        runtime.is_some() == (declared && activating),
        "guest runtime upload must cross one activation contact exactly when the provider \
         declares a staged activation"
    );
    let Some(runtime) = runtime else {
        return Ok(None);
    };
    refuse_credential_home_upload(credential_home_root, runtime)?;
    let descriptor = runtime
        .inherited_descriptor()
        .map_err(anyhow::Error::msg)?
        .to_string();
    Ok(Some((LIFECYCLE_GUEST_RUNTIME_FD_ENV.into(), descriptor, runtime)))
}

/// Seal the admitted adapter request a placement retains for one `operate`
/// contact. Every `operate` contact for that placement (allocation,
/// activation, termination and their reconciliations) receives the exact
/// bytes the adapter admitted, so every provider contact for the placement
/// works from the same admitted request; the controller never interprets them.
fn sealed_admitted_adapter_request(
    bytes: &[u8],
) -> Result<(lillux::InheritedDescriptorAuthority, (String, String))> {
    ensure!(
        !bytes.is_empty()
            && bytes.len() <= ryeos_external_execution_contract::MAX_LEASE_ADMISSION_BYTES,
        "admitted adapter request is empty or exceeds its bound"
    );
    let handle = lillux::sealed_memfd(c"ryeos-lifecycle-admitted-request", bytes)
        .map_err(anyhow::Error::msg)?;
    let descriptor = handle
        .inherited_descriptor()
        .map_err(anyhow::Error::msg)?
        .to_string();
    Ok((handle, (LEASE_ADMISSION_FD_ENV.into(), descriptor)))
}

fn decode_inspection_response(
    bytes: &[u8],
    request: &LifecycleAdapterInspectionRequest,
) -> Result<LifecycleAdapterInspectionResponse> {
    let response: LifecycleAdapterInspectionResponse =
        from_json_slice_strict(bytes, MAX_LIFECYCLE_RESPONSE_BYTES)
            .map_err(|_| anyhow::anyhow!("invalid lifecycle adapter inspection response"))?;
    response
        .validate_for(request)
        .map_err(|_| anyhow::anyhow!("invalid lifecycle adapter inspection response"))?;
    Ok(response)
}

fn decode_operation_response(
    bytes: &[u8],
    request: &LifecycleAdapterRequest,
) -> Result<LifecycleAdapterResponse> {
    let response: LifecycleAdapterResponse =
        from_json_slice_strict(bytes, MAX_LIFECYCLE_RESPONSE_BYTES)
            .map_err(|_| anyhow::anyhow!("invalid lifecycle adapter operation response"))?;
    response
        .validate_for(request)
        .map_err(|_| anyhow::anyhow!("invalid lifecycle adapter operation response"))?;
    Ok(response)
}

fn lifecycle_inspection_bytes(request: &LifecycleAdapterInspectionRequest) -> Result<Vec<u8>> {
    request.validate()?;
    let bytes = ryeos_external_execution_contract::canonical_json(request)?;
    ensure!(
        bytes.len() <= MAX_LIFECYCLE_REQUEST_BYTES,
        "lifecycle adapter inspection request exceeds its byte bound"
    );
    Ok(bytes)
}

fn artifact_bytes(artifact: &ryeos_engine::binary_resolver::CapturedExecutable) -> Result<u64> {
    let observation = artifact.handle.regular_file_observation()?;
    let bytes = observation.size();
    ensure!(
        (1..=1024 * 1024 * 1024).contains(&bytes),
        "external lifecycle artifact exceeds its byte bound"
    );
    ensure!(
        artifact
            .handle
            .digest_regular_file_stable_exact(&observation)?
            == artifact.identity.content_hash,
        "external lifecycle artifact changed after signed resolution"
    );
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opened(path: &std::path::Path) -> lillux::InheritedDescriptorAuthority {
        let resolved = lillux::canonicalize_existing_path(path).unwrap();
        lillux::secure_fs::open_pinned_regular_file_no_follow(&resolved)
            .unwrap()
            .inherited_descriptor_authority()
            .unwrap()
    }

    #[test]
    fn an_upload_from_a_credential_home_is_refused_by_file_identity() {
        let state = tempfile::tempdir().unwrap();
        let home = crate::private_artifact_home::create(
            state.path(),
            "credential-fixture",
            &BTreeMap::from([("auth.json".to_owned(), b"credential".to_vec())]),
        )
        .unwrap();
        let root = crate::private_artifact_home::root(state.path());
        let credential = home.join("auth.json");

        // Directly, through a symlink outside the homes, and through a hard
        // link outside them: each opens the same credential file.
        let outside = state.path().join("outside");
        let outside_directory = lillux::PinnedDirectory::open_or_create(&outside).unwrap();
        outside_directory
            .create_symlink(
                std::ffi::OsStr::new("link.json"),
                credential.as_os_str().as_encoded_bytes(),
            )
            .unwrap();
        std::fs::hard_link(&credential, outside.join("hard.json")).unwrap();
        for path in [credential.clone(), outside.join("link.json"), outside.join("hard.json")] {
            let error = refuse_credential_home_upload(&root, &opened(&path)).unwrap_err();
            let text = format!("{error:#}");
            assert!(text.starts_with("upload refused: its source is "), "{text}");
            assert!(text.contains("credential-fixture/auth.json"), "{text}");
        }

        // A workload file elsewhere, and sealed relay bytes, pass.
        std::fs::write(outside.join("workload.json"), b"workload").unwrap();
        refuse_credential_home_upload(&root, &opened(&outside.join("workload.json"))).unwrap();
        // Session state accumulating below a home's top level is never
        // read by the inode comparison, so it never exhausts its bound.
        let sessions = home.join("sessions").join("2026").join("10");
        std::fs::create_dir_all(&sessions).unwrap();
        for index in 0..64 {
            std::fs::write(sessions.join(format!("rollout-{index}.jsonl")), b"log").unwrap();
        }
        refuse_credential_home_upload(&root, &opened(&outside.join("workload.json"))).unwrap();
        // A file below a home's top level is refused by where it is linked.
        let error =
            refuse_credential_home_upload(&root, &opened(&sessions.join("rollout-0.jsonl")))
                .unwrap_err();
        assert!(format!("{error:#}").contains("rollout-0.jsonl"), "{error:#}");
        let error = refuse_credential_home_upload(&root, &opened(&credential)).unwrap_err();
        assert!(format!("{error:#}").contains("credential-fixture/auth.json"), "{error:#}");
        // A hard link outside the homes to a file below a home's top level
        // is named by neither the pathname nor the inode comparison; its
        // extra link refuses it.
        std::fs::hard_link(sessions.join("rollout-1.jsonl"), outside.join("rollout.jsonl"))
            .unwrap();
        let error = refuse_credential_home_upload(&root, &opened(&outside.join("rollout.jsonl")))
            .unwrap_err();
        assert!(
            format!("{error:#}").starts_with("upload refused: its source file has 2 hard links"),
            "{error:#}"
        );
        // So is any other multiply linked file, wherever its links are.
        std::fs::hard_link(outside.join("workload.json"), outside.join("workload-2.json"))
            .unwrap();
        let error = refuse_credential_home_upload(&root, &opened(&outside.join("workload.json")))
            .unwrap_err();
        assert!(format!("{error:#}").contains("2 hard links"), "{error:#}");
        let sealed = lillux::sealed_memfd(c"ryeos-relay-upload", b"envelope").unwrap();
        refuse_credential_home_upload(&root, &sealed).unwrap();
        // A node with no homes root has nothing to protect.
        refuse_credential_home_upload(&state.path().join("absent"), &sealed).unwrap();
    }

    #[test]
    fn a_credential_home_file_replaced_or_moved_after_opening_is_still_refused() {
        let state = tempfile::tempdir().unwrap();
        let home = crate::private_artifact_home::create(
            state.path(),
            "credential-fixture",
            &BTreeMap::from([("auth.json".to_owned(), b"credential".to_vec())]),
        )
        .unwrap();
        let root = crate::private_artifact_home::root(state.path());
        let credential = home.join("auth.json");

        // Opened, then replaced by a new file renamed over it: the opened
        // inode is in no home any more, and the kernel marks its name unlinked.
        let replaced = opened(&credential);
        std::fs::write(home.join("auth.json.new"), b"rotated").unwrap();
        std::fs::rename(home.join("auth.json.new"), &credential).unwrap();
        let error = refuse_credential_home_upload(&root, &replaced).unwrap_err();
        let text = format!("{error:#}");
        assert!(text.starts_with("upload refused: its source is "), "{text}");
        assert!(text.contains("credential-fixture/auth.json (since unlinked)"), "{text}");

        // Opened, then renamed below the home's top level, where the inode
        // comparison does not read.
        let moved = opened(&credential);
        std::fs::create_dir(home.join("archive")).unwrap();
        std::fs::rename(&credential, home.join("archive").join("auth.json")).unwrap();
        let error = refuse_credential_home_upload(&root, &moved).unwrap_err();
        assert!(format!("{error:#}").contains("archive/auth.json"), "{error:#}");
    }

    #[test]
    fn credential_home_listing_refuses_past_its_bound_and_reads_up_to_it() {
        let state = tempfile::tempdir().unwrap();
        for name in ["a", "b"] {
            std::fs::write(state.path().join(name), b"x").unwrap();
        }
        let directory = lillux::PinnedDirectory::open(state.path()).unwrap().unwrap();
        let entries = credential_home_entries(&directory, 2).unwrap();
        assert_eq!(entries.len(), 2);
        std::fs::write(state.path().join("c"), b"x").unwrap();
        let error = credential_home_entries(&directory, 2).unwrap_err();
        assert!(
            format!("{error:#}").contains("hold more homes and top-level entries"),
            "{error:#}"
        );
    }

    fn require_private_decode_error(error: anyhow::Error) {
        let error = error.context("lifecycle dispatch failed");
        for rendered in [
            format!("{error}"),
            format!("{error:#}"),
            format!("{error:?}"),
        ] {
            assert!(!rendered.contains("ADAPTER_SECRET_SENTINEL"), "{rendered}");
        }
        assert_eq!(
            error.chain().count(),
            2,
            "decoder source must not escape closed boundary"
        );
    }

    #[test]
    fn only_the_staged_activation_kind_declares_a_runtime_upload() {
        let spec = |kind: &str| {
            serde_json::to_vec(&serde_json::json!({
                "operations": { "activate_supervisor": { "kind": kind } }
            }))
            .unwrap()
        };
        assert!(spec_declares_staged_activation(&spec(
            "stage_then_upload_then_run_once"
        )));
        assert!(!spec_declares_staged_activation(&spec("unsupported_pending")));
        assert!(!spec_declares_staged_activation(b"{}"));
        assert!(!spec_declares_staged_activation(
            br#"{"operations":{"activate_supervisor":"stage_then_upload_then_run_once"}}"#
        ));
        assert!(!spec_declares_staged_activation(b"not json"));
    }

    #[test]
    fn reconciliation_declares_no_guest_runtime_even_if_one_is_held() {
        let scratch = tempfile::tempdir().unwrap();
        let upload = crate::guest_runtime_upload::test_upload(scratch.path());
        assert!(contact_guest_runtime(Some(&upload), true).is_none());
        let first = contact_guest_runtime(Some(&upload), false).unwrap();
        assert_eq!(first.sha256(), upload.sha256());
        assert!(contact_guest_runtime::<&lillux::InheritedDescriptorAuthority>(None, false).is_none());
    }

    #[test]
    fn the_runtime_descriptor_crosses_only_when_declared_and_activating() {
        let root = tempfile::tempdir().unwrap();
        let tar = lillux::sealed_memfd(c"ryeos-test-guest-runtime", b"runtime tar").unwrap();

        let (name, descriptor, runtime) =
            guest_runtime_delivery(root.path(), true, true, Some(&tar))
                .unwrap()
                .expect("declared activation delivers the runtime");
        assert_eq!(name, LIFECYCLE_GUEST_RUNTIME_FD_ENV);
        assert_eq!(descriptor, tar.inherited_descriptor().unwrap().to_string());
        let read = lillux::read_sealed_inherited_descriptor(descriptor.parse().unwrap(), 64)
            .unwrap();
        assert_eq!(read, b"runtime tar");
        assert_eq!(runtime.inherited_descriptor().unwrap().to_string(), descriptor);

        // Not declared: nothing crosses, and a descriptor is refused.
        assert!(
            guest_runtime_delivery(root.path(), false, true, None)
                .unwrap()
                .is_none()
        );
        assert!(guest_runtime_delivery(root.path(), false, true, Some(&tar)).is_err());
        // Declared but not activating (every other operation), or absent
        // from an activation: refused.
        assert!(guest_runtime_delivery(root.path(), true, false, Some(&tar)).is_err());
        assert!(guest_runtime_delivery(root.path(), true, true, None).is_err());
        assert!(
            guest_runtime_delivery(root.path(), true, false, None)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_runtime_upload_from_a_credential_home_is_refused_at_delivery() {
        let state = tempfile::tempdir().unwrap();
        let home = crate::private_artifact_home::create(
            state.path(),
            "credential-fixture",
            &BTreeMap::from([("auth.json".to_owned(), b"credential".to_vec())]),
        )
        .unwrap();
        let root = crate::private_artifact_home::root(state.path());
        let credential = opened(&home.join("auth.json"));
        let error = guest_runtime_delivery(&root, true, true, Some(&credential)).unwrap_err();
        assert!(
            format!("{error:#}").starts_with("upload refused: its source is "),
            "{error:#}"
        );
    }

    #[test]
    fn admitted_adapter_request_reaches_its_descriptor_exactly_and_bounded() {
        let bytes = br#"{"admitted":"opaque"}"#;
        let (handle, (name, descriptor)) = sealed_admitted_adapter_request(bytes).unwrap();
        assert_eq!(name, LEASE_ADMISSION_FD_ENV);
        assert_eq!(
            descriptor,
            handle.inherited_descriptor().unwrap().to_string()
        );
        let read = lillux::read_sealed_inherited_descriptor(
            descriptor.parse().unwrap(),
            ryeos_external_execution_contract::MAX_LEASE_ADMISSION_BYTES,
        )
        .unwrap();
        assert_eq!(read, bytes);

        assert!(sealed_admitted_adapter_request(&[]).is_err());
        let over = vec![b'x'; ryeos_external_execution_contract::MAX_LEASE_ADMISSION_BYTES + 1];
        assert!(sealed_admitted_adapter_request(&over).is_err());
    }

    #[test]
    fn operation_decode_and_validation_never_expose_adapter_values() {
        let request = LifecycleAdapterRequest::Allocate {
            common: LifecycleOperationCommon {
                schema: 1,
                protocol: LIFECYCLE_ADAPTER_PROTOCOL.into(),
                operation_id: "a".repeat(64),
                binding_hash: "b".repeat(64),
                settings_digest: "c".repeat(64),
            },
            reservation: AllocationReservation {
                placement_thread_id: "T-fixture".into(),
                admitted_capsule_hash: "d".repeat(64),
                base_snapshot_hash: "e".repeat(64),
                request_digest: "a".repeat(64),
                maximum_lifetime_seconds: 60,
                contact_deadline_ms: 100_000,
            },
        };
        let valid = serde_json::json!({"outcome":"allocation_pending", "operation_id":"a".repeat(64), "request_digest":"a".repeat(64)});
        decode_operation_response(&serde_json::to_vec(&valid).unwrap(), &request).unwrap();
        for fault in ["variant", "field", "identity"] {
            let mut value = valid.clone();
            match fault {
                "variant" => value["outcome"] = "ADAPTER_SECRET_SENTINEL".into(),
                "field" => value["ADAPTER_SECRET_SENTINEL"] = true.into(),
                "identity" => value["operation_id"] = "ADAPTER_SECRET_SENTINEL".into(),
                _ => unreachable!(),
            }
            require_private_decode_error(
                decode_operation_response(&serde_json::to_vec(&value).unwrap(), &request)
                    .unwrap_err(),
            );
        }
    }

    #[test]
    fn inspection_decode_and_validation_never_expose_adapter_values() {
        let request = LifecycleAdapterInspectionRequest {
            schema: 1, protocol: LIFECYCLE_ADAPTER_PROTOCOL.into(), adapter_id: "fixture".into(),
            adapter_artifact_hash: "a".repeat(64), settings_schema_digest: "b".repeat(64),
            target: "x86_64-unknown-linux-gnu".into(),
            declared_capabilities: std::collections::BTreeSet::from([ryeos_external_execution_contract::LifecycleCapability::ExactAllocationReconciliation]),
            provider_spec: LifecycleArtifactInspection {
                descriptor: 22,
                digest: "e".repeat(64),
                bytes: 128,
            },
            snapshot_production_spec: Some(LifecycleArtifactInspection {
                descriptor: 23,
                digest: "f".repeat(64),
                bytes: 256,
            }),
            artifacts: BTreeMap::from([
                (LifecycleArtifactRole::Supervisor, LifecycleArtifactInspection { descriptor: 20, digest: "c".repeat(64), bytes: 4096 }),
                (LifecycleArtifactRole::Launcher, LifecycleArtifactInspection { descriptor: 21, digest: "d".repeat(64), bytes: 4096 }),
            ]),
        };
        let response = LifecycleAdapterInspectionResponse {
            schema: 1,
            protocol: request.protocol.clone(),
            adapter_id: request.adapter_id.clone(),
            adapter_build: "fixture-build".into(),
            observed_adapter_artifact_hash: request.adapter_artifact_hash.clone(),
            observed_settings_schema_digest: request.settings_schema_digest.clone(),
            target: request.target.clone(),
            effective_capabilities: request.declared_capabilities.clone(),
            observed_provider_spec_sha256: request.provider_spec.digest.clone(),
            observed_snapshot_production_spec_sha256: request
                .snapshot_production_spec
                .as_ref()
                .map(|spec| spec.digest.clone()),
            observed_bootstrap_profile_digest: None,
            artifacts: request.artifacts.clone(),
        };
        let valid = serde_json::to_value(response).unwrap();
        decode_inspection_response(&serde_json::to_vec(&valid).unwrap(), &request).unwrap();
        let mut wrong_snapshot = valid.clone();
        wrong_snapshot["observed_snapshot_production_spec_sha256"] =
            serde_json::json!("0".repeat(64));
        require_private_decode_error(
            decode_inspection_response(&serde_json::to_vec(&wrong_snapshot).unwrap(), &request)
                .unwrap_err(),
        );
        for fault in ["variant", "field", "identity"] {
            let mut value = valid.clone();
            match fault {
                "variant" => {
                    value["effective_capabilities"] = serde_json::json!(["ADAPTER_SECRET_SENTINEL"])
                }
                "field" => value["ADAPTER_SECRET_SENTINEL"] = true.into(),
                "identity" => value["adapter_id"] = "ADAPTER_SECRET_SENTINEL".into(),
                _ => unreachable!(),
            }
            require_private_decode_error(
                decode_inspection_response(&serde_json::to_vec(&value).unwrap(), &request)
                    .unwrap_err(),
            );
        }
    }
}
