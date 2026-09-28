//! Test-only provider-neutral lifecycle fixture.
//!
//! This executable is admitted through the ordinary signed lifecycle adapter
//! declaration. It models a remote occurrence in one owner-private local
//! directory, stages only descriptor-supplied bytes, and launches the real
//! supervisor/launcher chain. It is never installed by a production bundle.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context as _, Result, bail, ensure};
use ryeos_external_execution_contract::guest_supervisor_descriptors::{
    SUPERVISOR_BOOTSTRAP_FD, SUPERVISOR_CANDIDATE_RUNTIME_FD, SUPERVISOR_CONTENT_RECORD_FD_BASE,
    SUPERVISOR_LAUNCHER_FD, SUPERVISOR_PRIVATE_PARENT_FD, SUPERVISOR_RUNTIME_MOUNT_FD_BASE,
    SUPERVISOR_STATE_ROOT_FD, SUPERVISOR_WORKSPACE_OUTPUT_FD,
};
use ryeos_external_execution_contract::{
    BoundOccurrence, LIFECYCLE_ADAPTER_EXECUTABLE_FD_ENV, LIFECYCLE_ADAPTER_PROTOCOL,
    LIFECYCLE_BOOTSTRAP_FD_ENV, LIFECYCLE_CREDENTIAL_FD_ENV, LIFECYCLE_GUEST_PACKAGE_FD_ENV,
    LIFECYCLE_LAUNCHER_FD_ENV, LIFECYCLE_REQUEST_FD_ENV, LIFECYCLE_SETTINGS_FD_ENV,
    LIFECYCLE_SUPERVISOR_FD_ENV, LifecycleAdapterInspectionRequest,
    LifecycleAdapterInspectionResponse, LifecycleAdapterRequest, LifecycleAdapterResponse,
    LifecycleArtifactInspection, LifecycleArtifactRole, MAX_LIFECYCLE_REQUEST_BYTES,
    MAX_LIFECYCLE_RESPONSE_BYTES, from_json_slice_strict,
};
use ryeos_state::external_execution::transport::{
    ExternalSupervisorBootstrap, MAX_EXTERNAL_SUPERVISOR_BOOTSTRAP_BYTES,
};
use serde::{Deserialize, Serialize};

const ADAPTER_BUILD: &str = "ryeos-synthetic-external-lifecycle-adapter.2";
const MAX_SETTINGS_BYTES: usize = 64 * 1024;
const MAX_CREDENTIAL_BYTES: usize = 64 * 1024;
const MAX_ARTIFACT_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_RECORD_BYTES: u64 = 1024 * 1024;
const SUPERVISOR_EXECUTABLE_FD: u32 = 49;

const ALLOCATION_FILE: &str = "allocation.json";
const ACTIVATION_FILE: &str = "activation.json";
const READY_FILE: &str = "ready.json";
const TERMINATE_FILE: &str = "terminate.json";
const TERMINAL_FILE: &str = "terminal.json";
const FAILURE_FILE: &str = "failure.json";
const BOOTSTRAP_FILE: &str = "bootstrap.json";
const SUPERVISOR_FILE: &str = "supervisor";
const LAUNCHER_FILE: &str = "launcher";
const CANDIDATE_RUNTIME_DIR: &str = "candidate-runtime";
const SUPERVISOR_STATE_DIR: &str = "supervisor-state";
const CANDIDATE_PRIVATE_DIR: &str = "candidate-private";
const MOUNTS_DIR: &str = "mounts";
const WORKSPACE_OUTPUT_FILE: &str = "workspace-output.json";
const ALLOCATION_RESPONSE_LOST_FILE: &str = "fault-allocation-response-lost.json";
const ACTIVATION_RESPONSE_LOST_FILE: &str = "fault-activation-response-lost.json";
const LIFECYCLE_CONTACTS_FILE: &str = "lifecycle-contacts.json";
const MAX_LIFECYCLE_CONTACTS: usize = 256;
const MAX_LIFECYCLE_CONTACT_BYTES: usize = 256 * 1024;

/// Test-only invocation testimony, not execution or provider authority.
/// Qualification `inspect` has no provider contact and is outside this log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LifecycleContactLog {
    schema: u32,
    contacts: Vec<LifecycleContact>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LifecycleContact {
    sequence: u64,
    operation: LifecycleContactOperation,
    operation_id: String,
    request_sha256: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum LifecycleContactOperation {
    Allocate,
    ReconcileAllocation,
    ActivateSupervisor,
    ReconcileSupervisorActivation,
    Terminate,
    ReconcileTermination,
}

impl LifecycleContactLog {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == 1 && self.contacts.len() <= MAX_LIFECYCLE_CONTACTS,
            "synthetic lifecycle contact log has invalid schema or size"
        );
        for (index, contact) in self.contacts.iter().enumerate() {
            ensure!(
                contact.sequence == u64::try_from(index + 1)?
                    && !contact.operation_id.is_empty()
                    && contact.operation_id.len() <= 256
                    && lillux::valid_hash(&contact.request_sha256)
                    && !contact
                        .request_sha256
                        .bytes()
                        .any(|byte| byte.is_ascii_uppercase()),
                "synthetic lifecycle contact log changed its ordered metadata"
            );
        }
        Ok(())
    }
}

fn record_lifecycle_contact(
    root: &lillux::PinnedDirectory,
    request: &LifecycleAdapterRequest,
) -> Result<()> {
    request.validate()?;
    let operation = match request {
        LifecycleAdapterRequest::Allocate { .. } => LifecycleContactOperation::Allocate,
        LifecycleAdapterRequest::ReconcileAllocation { .. } => {
            LifecycleContactOperation::ReconcileAllocation
        }
        LifecycleAdapterRequest::ActivateSupervisor { .. } => {
            LifecycleContactOperation::ActivateSupervisor
        }
        LifecycleAdapterRequest::ReconcileSupervisorActivation { .. } => {
            LifecycleContactOperation::ReconcileSupervisorActivation
        }
        LifecycleAdapterRequest::Terminate { .. } => LifecycleContactOperation::Terminate,
        LifecycleAdapterRequest::ReconcileTermination { .. } => {
            LifecycleContactOperation::ReconcileTermination
        }
    };
    let guard = root.lock_exclusive_with_timeout(lillux::time::Duration::from_secs(5))?;
    guard.ensure_protects(root)?;
    let prior = root.open_pinned_regular(OsStr::new(LIFECYCLE_CONTACTS_FILE), false)?;
    let mut log = if let Some(file) = &prior {
        let bytes = file.read_bounded(MAX_LIFECYCLE_CONTACT_BYTES as u64)?;
        let log: LifecycleContactLog = from_json_slice_strict(&bytes, MAX_LIFECYCLE_CONTACT_BYTES)?;
        ensure!(
            canonical_bytes(&log)? == bytes,
            "synthetic lifecycle contact log is noncanonical"
        );
        log
    } else {
        LifecycleContactLog {
            schema: 1,
            contacts: Vec::new(),
        }
    };
    log.validate()?;
    ensure!(
        log.contacts.len() < MAX_LIFECYCLE_CONTACTS,
        "synthetic lifecycle contact log is full"
    );
    // Intentionally no deduplication: an exact retry is still a contact.
    log.contacts.push(LifecycleContact {
        sequence: u64::try_from(log.contacts.len() + 1)?,
        operation,
        operation_id: request.common().operation_id.clone(),
        request_sha256: lillux::sha256_hex(&request.canonical_bytes()?),
    });
    log.validate()?;
    let bytes = canonical_bytes(&log)?;
    ensure!(
        bytes.len() <= MAX_LIFECYCLE_CONTACT_BYTES,
        "synthetic lifecycle contact log exceeds byte bound"
    );
    root.atomic_write_pinned_if_same(
        OsStr::new(LIFECYCLE_CONTACTS_FILE),
        prior.as_ref(),
        &bytes,
        0o600,
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SyntheticSettings {
    schema: u32,
    state_root: PathBuf,
    expected_credential_sha256: String,
    maximum_copy_entries: usize,
    maximum_copy_depth: usize,
    startup_timeout_ms: u64,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    faults: BTreeSet<SyntheticFault>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SyntheticFault {
    LoseFirstAllocationResponse,
    LoseFirstActivationResponse,
    FailAfterStagingIntent,
    FailAfterSpawnIntent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InjectedFaultRecord {
    schema: u32,
    operation_id: String,
    request_digest: String,
    fault: SyntheticFault,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AllocationRecord {
    schema: u32,
    operation_id: String,
    request_digest: String,
    occurrence_id: String,
    binding_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ActivationRecord {
    schema: u32,
    operation_id: String,
    occurrence_id: String,
    activation_request_digest: String,
    server_nonce: String,
    server_process: Option<lillux::ExactProcessIdentity>,
    supervisor_digest: String,
    launcher_digest: String,
    guest_input_identity: String,
    phase: ActivationPhase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ActivationPhase {
    Staging,
    SpawnIntent,
    ServerBound,
    FencedBeforeSpawn,
}

impl ActivationRecord {
    fn validate(&self) -> Result<()> {
        ensure!(self.schema == 2, "unsupported synthetic activation schema");
        ensure!(
            [
                &self.activation_request_digest,
                &self.server_nonce,
                &self.supervisor_digest,
                &self.launcher_digest,
                &self.guest_input_identity,
            ]
            .into_iter()
            .all(|digest| lillux::valid_hash(digest)),
            "synthetic activation identity is invalid"
        );
        ensure!(
            self.server_process.is_some() == (self.phase == ActivationPhase::ServerBound),
            "synthetic activation phase contradicts its server authority"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadyRecord {
    schema: u32,
    activation_request_digest: String,
    server_process_digest: String,
    supervisor_process: lillux::ExactProcessIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TerminateRecord {
    schema: u32,
    termination_request_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TerminalRecord {
    schema: u32,
    activation_request_digest: String,
    server_process_digest: String,
    supervisor_process_digest: String,
    supervisor_success: bool,
    supervisor_exit_code: i32,
    supervisor_timed_out: bool,
    supervisor_stderr: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FailureRecord {
    schema: u32,
    message: String,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("ryeos-synthetic-external-lifecycle-adapter: {error:#}");
        std::process::exit(126);
    }
}

fn run() -> Result<()> {
    // SAFETY: the lifecycle runner (or this adapter's trusted detached
    // relaunch) installs this exact coordinate once before single-threaded
    // startup. Adoption consumes the environment transport immediately.
    let adapter = unsafe {
        lillux::take_inherited_descriptor_authority_from_env(LIFECYCLE_ADAPTER_EXECUTABLE_FD_ENV)
    }
    .map_err(anyhow::Error::msg)?;
    let adapter_descriptor = adapter.inherited_descriptor().map_err(anyhow::Error::msg)?;
    lillux::validate_current_executable_descriptor(adapter_descriptor)
        .map_err(anyhow::Error::msg)?;
    let mut arguments = std::env::args();
    let _argv0 = arguments.next();
    let operation = arguments
        .next()
        .context("lifecycle adapter operation is absent")?;
    match operation.as_str() {
        "inspect" => inspect(&adapter),
        "operate" => operate(&adapter),
        "serve-occurrence" => {
            let occurrence = PathBuf::from(
                arguments
                    .next()
                    .context("synthetic occurrence path is absent")?,
            );
            let nonce = arguments
                .next()
                .context("synthetic server nonce is absent")?;
            ensure!(
                arguments.next().is_none(),
                "synthetic server has extra arguments"
            );
            let result = serve_occurrence(&occurrence, &nonce);
            if let Err(error) = &result {
                if let Ok(Some(directory)) = lillux::PinnedDirectory::open(&occurrence) {
                    let message = format!("{error:#}");
                    if message.len() <= 64 * 1024 {
                        let _ = ensure_document(
                            &directory,
                            FAILURE_FILE,
                            &FailureRecord { schema: 1, message },
                        );
                    }
                }
            }
            result
        }
        _ => bail!("unsupported lifecycle adapter operation"),
    }
}

fn inspect(adapter: &lillux::InheritedDescriptorAuthority) -> Result<()> {
    let request_bytes = read_sealed_env(LIFECYCLE_REQUEST_FD_ENV, MAX_LIFECYCLE_REQUEST_BYTES)?;
    let request: LifecycleAdapterInspectionRequest =
        from_json_slice_strict(&request_bytes, MAX_LIFECYCLE_REQUEST_BYTES)?;
    request.validate()?;
    ensure!(
        ryeos_external_execution_contract::canonical_json(&request)? == request_bytes,
        "inspection request is noncanonical"
    );
    ensure!(
        request.protocol == LIFECYCLE_ADAPTER_PROTOCOL
            && request.target == lillux::platform::current_binary_target()?,
        "inspection target or protocol is unsupported"
    );

    verify_artifact(
        adapter,
        request.adapter_artifact_hash.as_str(),
        None,
        "adapter",
    )?;
    for (role, inspection) in &request.artifacts {
        // SAFETY: descriptor ownership is supplied by the signed lifecycle
        // runner and consumed once in this dedicated process.
        let authority =
            unsafe { lillux::take_inherited_descriptor_authority(inspection.descriptor) }
                .map_err(anyhow::Error::msg)?;
        verify_artifact(
            &authority,
            &inspection.digest,
            Some(inspection.bytes),
            match role {
                LifecycleArtifactRole::Supervisor => "supervisor",
                LifecycleArtifactRole::Launcher => "launcher",
            },
        )?;
    }
    let response = LifecycleAdapterInspectionResponse {
        schema: 1,
        protocol: LIFECYCLE_ADAPTER_PROTOCOL.into(),
        adapter_id: request.adapter_id.clone(),
        adapter_build: ADAPTER_BUILD.into(),
        observed_adapter_artifact_hash: request.adapter_artifact_hash.clone(),
        observed_settings_schema_digest: request.settings_schema_digest.clone(),
        target: request.target.clone(),
        effective_capabilities: request.declared_capabilities.clone(),
        observed_provider_spec_sha256: request.provider_spec.digest.clone(),
        observed_snapshot_production_spec_sha256: None,
        artifacts: request.artifacts.clone(),
    };
    response.validate_for(&request)?;
    write_response(&response)
}

fn inject_response_loss_once(
    root: &lillux::PinnedDirectory,
    settings: &SyntheticSettings,
    request: &LifecycleAdapterRequest,
) -> Result<()> {
    let (fault, occurrence_id, request_digest, marker) = match request {
        LifecycleAdapterRequest::Allocate { reservation, .. }
            if settings
                .faults
                .contains(&SyntheticFault::LoseFirstAllocationResponse) =>
        {
            (
                SyntheticFault::LoseFirstAllocationResponse,
                occurrence_id(&reservation.request_digest),
                reservation.request_digest.as_str(),
                ALLOCATION_RESPONSE_LOST_FILE,
            )
        }
        LifecycleAdapterRequest::ActivateSupervisor {
            occurrence,
            activation,
            ..
        } if settings
            .faults
            .contains(&SyntheticFault::LoseFirstActivationResponse) =>
        {
            (
                SyntheticFault::LoseFirstActivationResponse,
                occurrence.occurrence_id.clone(),
                activation.activation_request_digest.as_str(),
                ACTIVATION_RESPONSE_LOST_FILE,
            )
        }
        _ => return Ok(()),
    };
    let occurrence = root
        .open_child_directory(OsStr::new(&occurrence_id))?
        .context("synthetic fault target occurrence is absent")?;
    if occurrence
        .open_pinned_regular(OsStr::new(marker), false)?
        .is_some()
    {
        return Ok(());
    }
    ensure_document(
        &occurrence,
        marker,
        &InjectedFaultRecord {
            schema: 1,
            operation_id: request.common().operation_id.clone(),
            request_digest: request_digest.into(),
            fault,
        },
    )?;
    bail!("synthetic lifecycle fault lost the first {fault:?} response")
}

fn operate(adapter_executable: &lillux::InheritedDescriptorAuthority) -> Result<()> {
    let request_bytes = read_sealed_env(LIFECYCLE_REQUEST_FD_ENV, MAX_LIFECYCLE_REQUEST_BYTES)?;
    let request: LifecycleAdapterRequest =
        from_json_slice_strict(&request_bytes, MAX_LIFECYCLE_REQUEST_BYTES)?;
    request.validate()?;
    ensure!(
        request.canonical_bytes()? == request_bytes,
        "operation request is noncanonical"
    );
    let settings_bytes = read_sealed_env(LIFECYCLE_SETTINGS_FD_ENV, MAX_SETTINGS_BYTES)?;
    let observed_settings_digest = lillux::sha256_hex(&settings_bytes);
    ensure!(
        observed_settings_digest == request.common().settings_digest,
        "synthetic lifecycle settings changed: expected {}, observed {}",
        request.common().settings_digest,
        observed_settings_digest
    );
    let settings: SyntheticSettings = serde_json::from_slice(&settings_bytes)?;
    settings.validate()?;
    ensure!(
        canonical_bytes(&settings)? == settings_bytes,
        "synthetic settings are noncanonical"
    );
    let credential = read_sealed_env(LIFECYCLE_CREDENTIAL_FD_ENV, MAX_CREDENTIAL_BYTES)?;
    ensure!(
        lillux::sha256_hex(&credential) == settings.expected_credential_sha256,
        "synthetic lifecycle credential is not the admitted generation"
    );
    let root = open_state_root(&settings)?;
    // Fail closed before any lifecycle operation if its contact witness
    // cannot be retained. No credential or bootstrap bytes enter the log.
    record_lifecycle_contact(&root, &request)?;
    let response = match &request {
        LifecycleAdapterRequest::Allocate { reservation, .. } => {
            allocate(&root, &request, reservation.request_digest.as_str())?
        }
        LifecycleAdapterRequest::ReconcileAllocation { reservation, .. } => {
            reconcile_allocation(&root, &request, reservation.request_digest.as_str())?
        }
        LifecycleAdapterRequest::ActivateSupervisor {
            occurrence,
            activation,
            guest_input_identity,
            guest_input_projection,
            guest_package,
            import_ticket,
            ..
        } => activate(
            adapter_executable,
            &root,
            &settings,
            &request,
            occurrence,
            activation,
            guest_input_identity,
            guest_input_projection,
            guest_package,
            import_ticket,
        )?,
        LifecycleAdapterRequest::ReconcileSupervisorActivation {
            occurrence,
            activation,
            ..
        } => reconcile_activation(&root, &request, occurrence, activation)?,
        LifecycleAdapterRequest::Terminate {
            occurrence,
            termination,
            ..
        } => terminate(&root, &settings, &request, occurrence, termination)?,
        LifecycleAdapterRequest::ReconcileTermination {
            occurrence,
            termination,
            ..
        } => reconcile_termination(&root, &request, occurrence, termination)?,
    };
    response.validate_for(&request)?;
    inject_response_loss_once(&root, &settings, &request)?;
    write_response(&response)
}

impl SyntheticSettings {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == 1,
            "unsupported synthetic lifecycle settings schema"
        );
        require_absolute_normalized(&self.state_root)?;
        ensure!(
            lillux::valid_hash(&self.expected_credential_sha256)
                && (1..=500_000).contains(&self.maximum_copy_entries)
                && (1..=64).contains(&self.maximum_copy_depth)
                && (100..=30_000).contains(&self.startup_timeout_ms),
            "synthetic lifecycle settings exceed their bounds"
        );
        Ok(())
    }
}

fn allocate(
    root: &lillux::PinnedDirectory,
    request: &LifecycleAdapterRequest,
    request_digest: &str,
) -> Result<LifecycleAdapterResponse> {
    let occurrence_id = occurrence_id(request_digest);
    let record = AllocationRecord {
        schema: 1,
        operation_id: request.common().operation_id.clone(),
        request_digest: request_digest.into(),
        occurrence_id: occurrence_id.clone(),
        binding_hash: request.common().binding_hash.clone(),
    };
    let occurrence = match root.open_child_directory(OsStr::new(&occurrence_id))? {
        Some(existing) => existing,
        None => root.create_child(OsStr::new(&occurrence_id), 0o700)?,
    };
    occurrence.require_owner_private_directory()?;
    ensure_document(&occurrence, ALLOCATION_FILE, &record)?;
    Ok(LifecycleAdapterResponse::AllocationBound {
        operation_id: request.common().operation_id.clone(),
        request_digest: request_digest.into(),
        occurrence_id,
        provider_observation_digest: evidence_digest("allocation_bound", &record)?,
    })
}

fn reconcile_allocation(
    root: &lillux::PinnedDirectory,
    request: &LifecycleAdapterRequest,
    request_digest: &str,
) -> Result<LifecycleAdapterResponse> {
    let occurrence_id = occurrence_id(request_digest);
    let Some(occurrence) = root.open_child_directory(OsStr::new(&occurrence_id))? else {
        return Ok(LifecycleAdapterResponse::AllocationNoOccurrence {
            operation_id: request.common().operation_id.clone(),
            request_digest: request_digest.into(),
            provider_observation_digest: evidence_digest(
                "allocation_absent",
                &serde_json::json!({"request_digest": request_digest}),
            )?,
        });
    };
    let record: AllocationRecord = read_document(&occurrence, ALLOCATION_FILE)?;
    require_allocation(&record, request, request_digest, &occurrence_id)?;
    ensure!(
        record.operation_id == request.common().operation_id,
        "synthetic allocation operation identity changed"
    );
    Ok(LifecycleAdapterResponse::AllocationBound {
        operation_id: request.common().operation_id.clone(),
        request_digest: request_digest.into(),
        occurrence_id,
        provider_observation_digest: evidence_digest("allocation_bound", &record)?,
    })
}

fn activate(
    adapter_executable: &lillux::InheritedDescriptorAuthority,
    root: &lillux::PinnedDirectory,
    settings: &SyntheticSettings,
    request: &LifecycleAdapterRequest,
    occurrence: &BoundOccurrence,
    activation: &ryeos_external_execution_contract::SupervisorActivationIntent,
    guest_input_identity: &str,
    guest_input_projection: &ryeos_external_execution_contract::ExternalGuestInputProjection,
    guest_package: &ryeos_external_execution_contract::LifecycleGuestPackageDelivery,
    import_ticket: &ryeos_external_execution_contract::staging_package::GuestImportTicket,
) -> Result<LifecycleAdapterResponse> {
    let occurrence_root = require_occurrence(root, request, occurrence)?;
    let Some(phase_lock) = occurrence_root.try_lock_exclusive()? else {
        return Ok(LifecycleAdapterResponse::SupervisorPending {
            operation_id: request.common().operation_id.clone(),
            activation_request_digest: activation.activation_request_digest.clone(),
        });
    };
    phase_lock.ensure_protects(&occurrence_root)?;
    if occurrence_root
        .open_pinned_regular(OsStr::new(ACTIVATION_FILE), false)?
        .is_some()
    {
        drop(phase_lock);
        return reconcile_activation(root, request, occurrence, activation);
    }
    ensure!(
        occurrence_root
            .open_pinned_regular(OsStr::new(TERMINATE_FILE), false)?
            .is_none(),
        "synthetic occurrence is already fenced for termination"
    );
    let bootstrap_bytes = read_sealed_env(
        LIFECYCLE_BOOTSTRAP_FD_ENV,
        MAX_EXTERNAL_SUPERVISOR_BOOTSTRAP_BYTES,
    )?;
    let bootstrap: ExternalSupervisorBootstrap = serde_json::from_slice(&bootstrap_bytes)?;
    bootstrap.validate()?;
    ensure!(
        bootstrap.canonical_bytes()? == bootstrap_bytes,
        "supervisor bootstrap is noncanonical"
    );
    ensure!(
        bootstrap.occurrence_id == occurrence.occurrence_id
            && bootstrap.allocation_request_digest == occurrence.request_digest
            && bootstrap.execution_binding_hash == request.common().binding_hash
            && bootstrap.supervisor_runtime_hash == activation.supervisor_runtime_hash
            && bootstrap.launcher_artifact_hash == activation.launcher_artifact_hash
            && bootstrap.guest_input_identity == *guest_input_identity
            && bootstrap.guest_inputs == *guest_input_projection
            && bootstrap.guest_input_identity == guest_input_projection.identity_digest()?,
        "synthetic activation changed its retained occurrence or guest authority"
    );

    // SAFETY: each descriptor was installed once by the admitted runner and
    // activation consumes the only transferable authority.
    let supervisor = unsafe {
        lillux::take_inherited_descriptor_authority_from_env(LIFECYCLE_SUPERVISOR_FD_ENV)
    }
    .map_err(anyhow::Error::msg)?;
    let launcher =
        unsafe { lillux::take_inherited_descriptor_authority_from_env(LIFECYCLE_LAUNCHER_FD_ENV) }
            .map_err(anyhow::Error::msg)?;
    let supervisor_digest = measure_executable(&supervisor)?;
    let launcher_digest = measure_executable(&launcher)?;
    ensure!(
        launcher_digest == activation.launcher_artifact_hash,
        "synthetic activation launcher changed its admitted digest"
    );
    let mut retained = ActivationRecord {
        schema: 2,
        operation_id: request.common().operation_id.clone(),
        occurrence_id: occurrence.occurrence_id.clone(),
        activation_request_digest: activation.activation_request_digest.clone(),
        server_nonce: lillux::sha256_hex(&lillux::crypto::generate_random_bytes::<32>()),
        server_process: None,
        supervisor_digest,
        launcher_digest,
        guest_input_identity: bootstrap.guest_input_identity.clone(),
        phase: ActivationPhase::Staging,
    };
    retained.validate()?;
    ensure_document(&occurrence_root, ACTIVATION_FILE, &retained)?;
    ensure!(
        !settings
            .faults
            .contains(&SyntheticFault::FailAfterStagingIntent),
        "synthetic lifecycle fault after staging intent"
    );
    ensure!(
        stage_executable(&occurrence_root, SUPERVISOR_FILE, &supervisor)?
            == retained.supervisor_digest
            && stage_executable(&occurrence_root, LAUNCHER_FILE, &launcher)?
                == retained.launcher_digest,
        "synthetic executable changed after staging intent"
    );
    ensure_document_bytes(&occurrence_root, BOOTSTRAP_FILE, &bootstrap_bytes, 0o600)?;

    // The request carries the protected input projection as well as its
    // semantic identity. Descriptor numbers were allocated in the controller;
    // the adapter does not open them as local authority. Import the inherited
    // package under the exact joined projection and retained coordinates.
    let package = unsafe {
        lillux::take_inherited_descriptor_authority_from_env(LIFECYCLE_GUEST_PACKAGE_FD_ENV)
    }
    .map_err(anyhow::Error::msg)?;
    ensure!(
        package.inherited_descriptor().map_err(anyhow::Error::msg)? == guest_package.descriptor,
        "synthetic guest package descriptor changed at handoff"
    );
    let bootstrap_sha256 = lillux::sha256_hex(&bootstrap_bytes);
    let import_context = ryeos_external_execution_contract::staging_package::GuestImportContext {
        binding_hash: &request.common().binding_hash,
        allocation_request_digest: &occurrence.request_digest,
        occurrence_id: &retained.occurrence_id,
        activation_request_digest: &retained.activation_request_digest,
    };
    ensure!(
        import_ticket.bootstrap_sha256 == bootstrap_sha256
            && import_ticket.supervisor_sha256 == retained.supervisor_digest
            && import_ticket.launcher_sha256 == retained.launcher_digest,
        "synthetic import ticket changed retained bootstrap or executable authority"
    );
    let expected = import_ticket.staging_expected(&import_context, guest_input_projection)?;
    let mut reader = package.stable_regular_reader_exact(
        guest_package.framed_bytes,
        &guest_package.payload_sha256,
        guest_package.framed_bytes,
    )?;
    let staged = ryeos_external_execution::guest_staging::stage_guest_package(
        &mut reader,
        &occurrence_root,
        &expected,
    )?;
    if let Err(error) = reader.finish() {
        staged
            .discard()
            .context("discard package after failed stable read")?;
        return Err(error);
    }
    let installation = (|| -> Result<()> {
        import_ticket.validate_verified_manifest(
            &import_context,
            staged.manifest(),
            &guest_package.payload_sha256,
            guest_package.framed_bytes,
        )?;
        ryeos_external_execution::guest_content::recheck_staged_guest_content(
            &staged,
            guest_input_projection,
        )?;
        let guest_inputs = guest_input_projection;
        let staged_root = staged.root();
        let runtime = occurrence_root.create_child(OsStr::new(CANDIDATE_RUNTIME_DIR), 0o700)?;
        let base = staged_root
            .open_child_directory(OsStr::new("base"))?
            .context("staged guest base is absent")?;
        ryeos_project_capture::install_project_snapshot_transfer(
            &base,
            &runtime,
            &ryeos_project_capture::ProjectSnapshotTransferMeasurement {
                snapshot_hash: guest_inputs.base_snapshot.snapshot_hash.clone(),
                closure_digest: guest_inputs.base_snapshot.closure_digest.clone(),
                object_count: guest_inputs.base_snapshot.object_count,
                blob_count: guest_inputs.base_snapshot.blob_count,
                total_bytes: guest_inputs.base_snapshot.total_bytes,
            },
        )?;
        occurrence_root.create_child(OsStr::new(SUPERVISOR_STATE_DIR), 0o700)?;
        occurrence_root.create_child(OsStr::new(CANDIDATE_PRIVATE_DIR), 0o700)?;
        let mounts = occurrence_root.create_child(OsStr::new(MOUNTS_DIR), 0o700)?;
        if let Some(outputs) = &guest_inputs.workspace_outputs {
            let output = staged_root
                .open_pinned_regular(OsStr::new("workspace_outputs"), false)?
                .context("staged workspace-output authority is absent")?
                .inherited_descriptor_authority()?;
            stage_regular(
                &occurrence_root,
                WORKSPACE_OUTPUT_FILE,
                &output,
                outputs.bytes,
                &outputs.authority_hash,
                0o600,
            )?;
        }
        let mut record_index = 0;
        for (index, input) in guest_inputs.inputs.iter().enumerate() {
            let name = mount_name(index);
            for (_, digest, bytes) in input.content_authority.record_descriptors() {
                let source = staged_root
                    .open_pinned_regular(OsStr::new(&format!("record-{record_index:02}")), false)?
                    .context("staged guest content record is absent")?
                    .inherited_descriptor_authority()?;
                stage_regular(
                    &mounts,
                    &content_record_name(record_index),
                    &source,
                    bytes,
                    digest,
                    0o600,
                )?;
                record_index += 1;
            }
            match input.kind {
                ryeos_external_execution_contract::GuestMountKind::Directory => {
                    if matches!(
                        input.content_authority,
                        ryeos_external_execution_contract::GuestMountContentAuthority::PrivateScratch { .. }
                    ) {
                        mounts.create_child(OsStr::new(&name), 0o700)?;
                    } else {
                        let staged_name = format!("input-{index:02}");
                        let observed = staged_root
                            .entry_no_follow(OsStr::new(&staged_name))?
                            .context("staged guest directory input is absent")?;
                        ensure!(
                            staged_root.move_child_if_same_noreplace_to(&observed, &mounts)?,
                            "staged guest directory destination is occupied"
                        );
                        let moved = mounts
                            .open_child_directory(OsStr::new(&staged_name))?
                            .context("moved guest directory input is absent")?;
                        mounts.rename_child_directory_noreplace(
                            OsStr::new(&staged_name),
                            OsStr::new(&name),
                            &moved,
                        )?;
                    }
                }
                ryeos_external_execution_contract::GuestMountKind::RegularFile => {
                    ensure!(
                    !matches!(input.content_authority, ryeos_external_execution_contract::GuestMountContentAuthority::PrivateScratch { .. }),
                    "synthetic private scratch must be a directory"
                );
                    let staged_name = format!("input-{index:02}");
                    let observed = staged_root
                        .entry_no_follow(OsStr::new(&staged_name))?
                        .context("staged guest regular input is absent")?;
                    ensure!(
                        staged_root.move_child_if_same_noreplace_to(&observed, &mounts)?,
                        "staged guest regular destination is occupied"
                    );
                    let moved = mounts
                        .open_regular(OsStr::new(&staged_name), false)?
                        .context("moved guest regular input is absent")?;
                    mounts.rename_regular_child_noreplace_atomic(
                        OsStr::new(&staged_name),
                        OsStr::new(&name),
                        &moved,
                    )?;
                }
            }
        }
        Ok(())
    })();
    let cleanup = staged.discard();
    match (installation, cleanup) {
        (Ok(()), Ok(())) => {}
        (Err(error), Ok(())) => return Err(error),
        (Ok(()), Err(error)) => return Err(error.context("guest package cleanup failed")),
        (Err(error), Err(cleanup)) => {
            return Err(error.context(format!("guest package cleanup also failed: {cleanup:#}")));
        }
    }

    if !author_spawn_intent(&occurrence_root, &phase_lock, &mut retained)? {
        return Ok(LifecycleAdapterResponse::SupervisorNotStarted {
            operation_id: request.common().operation_id.clone(),
            activation_request_digest: activation.activation_request_digest.clone(),
            provider_observation_digest: evidence_digest(
                "supervisor_fenced_before_spawn",
                &retained,
            )?,
        });
    }
    ensure!(
        !settings
            .faults
            .contains(&SyntheticFault::FailAfterSpawnIntent),
        "synthetic lifecycle fault after spawn intent"
    );
    let occurrence_path = occurrence_root
        .path()
        .to_str()
        .context("occurrence path is not UTF-8")?;
    let adapter_descriptor = adapter_executable
        .inherited_descriptor()
        .map_err(anyhow::Error::msg)?;
    let spawned = lillux::spawn_detached_from_executable(
        adapter_executable,
        &[
            "serve-occurrence".into(),
            occurrence_path.into(),
            retained.server_nonce.clone(),
        ],
        None,
        &[(
            LIFECYCLE_ADAPTER_EXECUTABLE_FD_ENV.into(),
            adapter_descriptor.to_string(),
        )],
    )
    .map_err(anyhow::Error::msg)
    .context("launch synthetic occurrence server")?;
    let process = lillux::capture_exact_process_identity(spawned.pid, Some(spawned.pid))
        .map_err(anyhow::Error::msg)
        .context("capture synthetic occurrence server identity")?;
    retained.server_process = Some(process);
    retained.phase = ActivationPhase::ServerBound;
    retained.validate()?;
    replace_document(&occurrence_root, ACTIVATION_FILE, &retained)?;
    // The detached server acquires an independent lock before its own spawn.
    // Never hand off this lock or wait for Ready while retaining it.
    drop(phase_lock);

    wait_for_document::<ReadyRecord>(
        &occurrence_root,
        READY_FILE,
        lillux::time::Duration::from_millis(settings.startup_timeout_ms),
    )?;
    Ok(LifecycleAdapterResponse::SupervisorStarted {
        operation_id: request.common().operation_id.clone(),
        activation_request_digest: activation.activation_request_digest.clone(),
        provider_observation_digest: evidence_digest("supervisor_started", &retained)?,
    })
}

fn reconcile_activation(
    root: &lillux::PinnedDirectory,
    request: &LifecycleAdapterRequest,
    occurrence: &BoundOccurrence,
    activation: &ryeos_external_execution_contract::SupervisorActivationIntent,
) -> Result<LifecycleAdapterResponse> {
    let occurrence_root = require_occurrence(root, request, occurrence)?;
    let Some(phase_lock) = occurrence_root.try_lock_exclusive()? else {
        return Ok(LifecycleAdapterResponse::SupervisorPending {
            operation_id: request.common().operation_id.clone(),
            activation_request_digest: activation.activation_request_digest.clone(),
        });
    };
    phase_lock.ensure_protects(&occurrence_root)?;
    let Some(file) = occurrence_root.open_pinned_regular(OsStr::new(ACTIVATION_FILE), false)?
    else {
        return Ok(LifecycleAdapterResponse::SupervisorPending {
            operation_id: request.common().operation_id.clone(),
            activation_request_digest: activation.activation_request_digest.clone(),
        });
    };
    let mut retained: ActivationRecord = decode_document(&file)?;
    retained.validate()?;
    ensure!(
        retained.operation_id == request.common().operation_id
            && retained.occurrence_id == occurrence.occurrence_id
            && retained.activation_request_digest == activation.activation_request_digest,
        "synthetic activation reconciliation changed operation identity"
    );
    if fence_staging_for_termination(&occurrence_root, &phase_lock, &mut retained)? {
        return Ok(LifecycleAdapterResponse::SupervisorNotStarted {
            operation_id: request.common().operation_id.clone(),
            activation_request_digest: activation.activation_request_digest.clone(),
            provider_observation_digest: evidence_digest(
                "supervisor_fenced_before_spawn",
                &retained,
            )?,
        });
    }
    if let Some(file) = occurrence_root.open_pinned_regular(OsStr::new(READY_FILE), false)? {
        let ready: ReadyRecord = decode_document(&file)?;
        let server = retained
            .server_process
            .as_ref()
            .context("synthetic readiness has no exact server authority")?;
        ensure!(
            retained.phase == ActivationPhase::ServerBound
                && ready.schema == 1
                && ready.activation_request_digest == retained.activation_request_digest
                && ready.server_process_digest
                    == server.incarnation_digest().map_err(anyhow::Error::msg)?,
            "synthetic readiness contradicts its activation authority"
        );
        return Ok(LifecycleAdapterResponse::SupervisorStarted {
            operation_id: request.common().operation_id.clone(),
            activation_request_digest: activation.activation_request_digest.clone(),
            provider_observation_digest: evidence_digest("supervisor_started", &retained)?,
        });
    }
    // After spawn intent, a missing Ready or ended server is not evidence that
    // no supervisor/descendant started. Only the positive pre-spawn fence above
    // may report NotStarted.
    Ok(LifecycleAdapterResponse::SupervisorPending {
        operation_id: request.common().operation_id.clone(),
        activation_request_digest: activation.activation_request_digest.clone(),
    })
}

fn fence_staging_for_termination(
    occurrence: &lillux::PinnedDirectory,
    phase_lock: &lillux::PinnedDirectoryLock,
    activation: &mut ActivationRecord,
) -> Result<bool> {
    phase_lock.ensure_protects(occurrence)?;
    activation.validate()?;
    let incumbent = occurrence
        .open_pinned_regular(OsStr::new(ACTIVATION_FILE), false)?
        .context("synthetic activation transition lost its incumbent")?;
    let current: ActivationRecord = decode_document(&incumbent)?;
    current.validate()?;
    ensure!(
        current == *activation,
        "synthetic activation transition has stale authority"
    );
    let Some(file) = occurrence.open_pinned_regular(OsStr::new(TERMINATE_FILE), false)? else {
        ensure!(
            activation.phase != ActivationPhase::FencedBeforeSpawn,
            "synthetic pre-spawn fence lost its termination authority"
        );
        return Ok(false);
    };
    let termination: TerminateRecord = decode_document(&file)?;
    ensure!(
        termination.schema == 1 && lillux::valid_hash(&termination.termination_request_digest),
        "synthetic termination authority is invalid"
    );
    if activation.phase == ActivationPhase::Staging {
        activation.phase = ActivationPhase::FencedBeforeSpawn;
        occurrence.atomic_write_pinned_if_same(
            OsStr::new(ACTIVATION_FILE),
            Some(&incumbent),
            &canonical_bytes(activation)?,
            0o600,
        )?;
    }
    Ok(activation.phase == ActivationPhase::FencedBeforeSpawn)
}

fn author_spawn_intent(
    occurrence: &lillux::PinnedDirectory,
    phase_lock: &lillux::PinnedDirectoryLock,
    activation: &mut ActivationRecord,
) -> Result<bool> {
    if fence_staging_for_termination(occurrence, phase_lock, activation)? {
        return Ok(false);
    }
    ensure!(
        activation.phase == ActivationPhase::Staging,
        "synthetic activation cannot repeat its spawn intent"
    );
    let incumbent = occurrence
        .open_pinned_regular(OsStr::new(ACTIVATION_FILE), false)?
        .context("synthetic spawn transition lost its incumbent")?;
    let current: ActivationRecord = decode_document(&incumbent)?;
    ensure!(
        current == *activation,
        "synthetic spawn transition has stale authority"
    );
    activation.phase = ActivationPhase::SpawnIntent;
    occurrence.atomic_write_pinned_if_same(
        OsStr::new(ACTIVATION_FILE),
        Some(&incumbent),
        &canonical_bytes(activation)?,
        0o600,
    )?;
    Ok(true)
}

fn terminate(
    root: &lillux::PinnedDirectory,
    settings: &SyntheticSettings,
    request: &LifecycleAdapterRequest,
    occurrence: &BoundOccurrence,
    termination: &ryeos_external_execution_contract::TerminationIntent,
) -> Result<LifecycleAdapterResponse> {
    let occurrence_root = require_occurrence(root, request, occurrence)?;
    let intent = TerminateRecord {
        schema: 1,
        termination_request_digest: termination.termination_request_digest.clone(),
    };
    ensure_document(&occurrence_root, TERMINATE_FILE, &intent)?;
    let deadline = lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_millis(
        settings.startup_timeout_ms,
    ));
    loop {
        let response = reconcile_termination(root, request, occurrence, termination)?;
        if matches!(
            response,
            LifecycleAdapterResponse::OccurrenceTerminal { .. }
        ) {
            return Ok(response);
        }
        if deadline.has_elapsed() {
            return Ok(response);
        }
        lillux::time::sleep(
            deadline
                .remaining()
                .min(lillux::time::Duration::from_millis(10)),
        );
    }
}

fn reconcile_termination(
    root: &lillux::PinnedDirectory,
    request: &LifecycleAdapterRequest,
    occurrence: &BoundOccurrence,
    termination: &ryeos_external_execution_contract::TerminationIntent,
) -> Result<LifecycleAdapterResponse> {
    let occurrence_root = require_occurrence(root, request, occurrence)?;
    let Some(phase_lock) = occurrence_root.try_lock_exclusive()? else {
        return Ok(LifecycleAdapterResponse::TerminationPending {
            operation_id: request.common().operation_id.clone(),
            termination_request_digest: termination.termination_request_digest.clone(),
        });
    };
    phase_lock.ensure_protects(&occurrence_root)?;
    let Some(intent_file) =
        occurrence_root.open_pinned_regular(OsStr::new(TERMINATE_FILE), false)?
    else {
        return Ok(LifecycleAdapterResponse::TerminationPending {
            operation_id: request.common().operation_id.clone(),
            termination_request_digest: termination.termination_request_digest.clone(),
        });
    };
    let intent: TerminateRecord = decode_document(&intent_file)?;
    ensure!(
        intent.schema == 1
            && intent.termination_request_digest == termination.termination_request_digest,
        "synthetic termination intent changed"
    );
    let Some(activation_file) =
        occurrence_root.open_pinned_regular(OsStr::new(ACTIVATION_FILE), false)?
    else {
        return Ok(LifecycleAdapterResponse::TerminationPending {
            operation_id: request.common().operation_id.clone(),
            termination_request_digest: termination.termination_request_digest.clone(),
        });
    };
    let mut activation: ActivationRecord = decode_document(&activation_file)?;
    activation.validate()?;
    ensure!(
        activation.occurrence_id == occurrence.occurrence_id,
        "synthetic terminal activation changed occurrence"
    );
    if fence_staging_for_termination(&occurrence_root, &phase_lock, &mut activation)? {
        return Ok(LifecycleAdapterResponse::OccurrenceTerminal {
            operation_id: request.common().operation_id.clone(),
            termination_request_digest: termination.termination_request_digest.clone(),
            provider_observation_digest: evidence_digest(
                "occurrence_fenced_before_spawn",
                &serde_json::json!({"activation": activation, "termination": intent}),
            )?,
        });
    }
    let Some(terminal_file) =
        occurrence_root.open_pinned_regular(OsStr::new(TERMINAL_FILE), false)?
    else {
        return Ok(LifecycleAdapterResponse::TerminationPending {
            operation_id: request.common().operation_id.clone(),
            termination_request_digest: termination.termination_request_digest.clone(),
        });
    };
    let terminal: TerminalRecord = decode_document(&terminal_file)?;
    let server = activation
        .server_process
        .as_ref()
        .context("terminal occurrence has no exact server process")?;
    let ready: ReadyRecord = read_document(&occurrence_root, READY_FILE)?;
    ensure!(
        terminal.schema == 1
            && terminal.activation_request_digest == activation.activation_request_digest
            && terminal.server_process_digest
                == server.incarnation_digest().map_err(anyhow::Error::msg)?
            && ready.schema == 1
            && ready.activation_request_digest == terminal.activation_request_digest
            && ready.server_process_digest == terminal.server_process_digest
            && ready
                .supervisor_process
                .incarnation_digest()
                .map_err(anyhow::Error::msg)?
                == terminal.supervisor_process_digest,
        "synthetic terminal evidence changed its exact process authority"
    );
    if !server.has_ended().map_err(anyhow::Error::msg)? {
        return Ok(LifecycleAdapterResponse::TerminationPending {
            operation_id: request.common().operation_id.clone(),
            termination_request_digest: termination.termination_request_digest.clone(),
        });
    }
    Ok(LifecycleAdapterResponse::OccurrenceTerminal {
        operation_id: request.common().operation_id.clone(),
        termination_request_digest: termination.termination_request_digest.clone(),
        provider_observation_digest: evidence_digest(
            "occurrence_terminal",
            &serde_json::json!({"terminal": terminal, "termination": intent}),
        )?,
    })
}

fn serve_occurrence(path: &Path, nonce: &str) -> Result<()> {
    require_absolute_normalized(path)?;
    ensure!(
        lillux::valid_hash(nonce),
        "synthetic occurrence nonce is invalid"
    );
    let occurrence =
        lillux::PinnedDirectory::open(path)?.context("synthetic occurrence disappeared")?;
    occurrence.require_owner_private_directory()?;
    let phase_lock =
        occurrence.lock_exclusive_with_timeout(lillux::time::Duration::from_secs(30))?;
    phase_lock.ensure_protects(&occurrence)?;
    let activation: ActivationRecord = read_document(&occurrence, ACTIVATION_FILE)?;
    activation.validate()?;
    ensure!(
        activation.server_nonce == nonce && activation.phase == ActivationPhase::ServerBound,
        "synthetic occurrence has no exact armed server authority"
    );
    let server_process = lillux::capture_current_process_identity().map_err(anyhow::Error::msg)?;
    ensure!(
        activation.server_process.as_ref() == Some(&server_process),
        "synthetic occurrence server identity changed"
    );
    ensure!(
        occurrence
            .open_pinned_regular(OsStr::new(TERMINATE_FILE), false)?
            .is_none(),
        "synthetic occurrence was terminated before supervisor spawn"
    );
    let bootstrap_file = occurrence
        .open_pinned_regular(OsStr::new(BOOTSTRAP_FILE), false)?
        .context("synthetic supervisor bootstrap is absent")?;
    let bootstrap_bytes =
        bootstrap_file.read_bounded(MAX_EXTERNAL_SUPERVISOR_BOOTSTRAP_BYTES as u64)?;
    let bootstrap: ExternalSupervisorBootstrap = serde_json::from_slice(&bootstrap_bytes)?;
    bootstrap.validate()?;
    let supervisor = occurrence
        .open_pinned_regular(OsStr::new(SUPERVISOR_FILE), false)?
        .context("synthetic supervisor executable is absent")?;
    let launcher = occurrence
        .open_pinned_regular(OsStr::new(LAUNCHER_FILE), false)?
        .context("synthetic launcher executable is absent")?;
    let state = occurrence
        .open_child_directory(OsStr::new(SUPERVISOR_STATE_DIR))?
        .context("synthetic supervisor state is absent")?;
    let runtime = occurrence
        .open_child_directory(OsStr::new(CANDIDATE_RUNTIME_DIR))?
        .context("synthetic candidate runtime is absent")?;
    let private = occurrence
        .open_child_directory(OsStr::new(CANDIDATE_PRIVATE_DIR))?
        .context("synthetic candidate private root is absent")?;
    let mounts = occurrence
        .open_child_directory(OsStr::new(MOUNTS_DIR))?
        .context("synthetic candidate mounts are absent")?;
    let bootstrap_handle =
        lillux::sealed_memfd(c"synthetic-supervisor-bootstrap", &bootstrap_bytes)
            .map_err(anyhow::Error::msg)?;
    let supervisor_authority = supervisor.inherited_descriptor_authority()?;
    let launcher_authority = launcher.inherited_descriptor_authority()?;
    let mut process = lillux::SubprocessRequest {
        cmd: String::new(),
        argv0: Some("ryeos-external-candidate-supervisor".into()),
        args: Vec::new(),
        cwd: Some("/".into()),
        envs: Vec::new(),
        stdin_data: None,
        timeout: f64::from(
            bootstrap.execution_timeout_seconds + bootstrap.post_execution_timeout_seconds + 30,
        ),
        limits: Some(lillux::SubprocessLimits {
            max_open_files: Some(256),
            max_stdout_bytes: Some(64 * 1024),
            max_stderr_bytes: Some(256 * 1024),
            ..lillux::SubprocessLimits::default()
        }),
        inherited_fds: Vec::new(),
        inherited_fd_mappings: Vec::new(),
        supervised_status: None,
    };
    supervisor_authority
        .bind_as_subprocess_executable(&mut process, SUPERVISOR_EXECUTABLE_FD)
        .map_err(anyhow::Error::msg)?;
    bootstrap_handle
        .bind_to_subprocess_request(&mut process, SUPERVISOR_BOOTSTRAP_FD)
        .map_err(anyhow::Error::msg)?;
    state
        .inherited_descriptor_authority()?
        .bind_to_subprocess_request(&mut process, SUPERVISOR_STATE_ROOT_FD)
        .map_err(anyhow::Error::msg)?;
    runtime
        .inherited_descriptor_authority()?
        .bind_to_subprocess_request(&mut process, SUPERVISOR_CANDIDATE_RUNTIME_FD)
        .map_err(anyhow::Error::msg)?;
    private
        .inherited_descriptor_authority()?
        .bind_to_subprocess_request(&mut process, SUPERVISOR_PRIVATE_PARENT_FD)
        .map_err(anyhow::Error::msg)?;
    launcher_authority
        .bind_to_subprocess_request(&mut process, SUPERVISOR_LAUNCHER_FD)
        .map_err(anyhow::Error::msg)?;
    if bootstrap.guest_inputs.workspace_outputs.is_some() {
        let output = occurrence
            .open_pinned_regular(OsStr::new(WORKSPACE_OUTPUT_FILE), false)?
            .context("synthetic workspace-output authority is absent")?;
        output
            .inherited_descriptor_authority()?
            .bind_to_subprocess_request(&mut process, SUPERVISOR_WORKSPACE_OUTPUT_FD)
            .map_err(anyhow::Error::msg)?;
    }
    for (index, input) in bootstrap.guest_inputs.inputs.iter().enumerate() {
        let target = SUPERVISOR_RUNTIME_MOUNT_FD_BASE
            .checked_add(u32::try_from(index)?)
            .context("synthetic supervisor mount descriptor overflow")?;
        let name = mount_name(index);
        let authority = match input.kind {
            ryeos_external_execution_contract::GuestMountKind::Directory => mounts
                .open_child_directory(OsStr::new(&name))?
                .context("synthetic runtime directory mount is absent")?
                .inherited_descriptor_authority()?,
            ryeos_external_execution_contract::GuestMountKind::RegularFile => mounts
                .open_pinned_regular(OsStr::new(&name), false)?
                .context("synthetic runtime file mount is absent")?
                .inherited_descriptor_authority()?,
        };
        authority
            .bind_to_subprocess_request(&mut process, target)
            .map_err(anyhow::Error::msg)?;
    }
    for (index, _) in bootstrap.guest_inputs.record_descriptors().enumerate() {
        let target = SUPERVISOR_CONTENT_RECORD_FD_BASE
            .checked_add(u32::try_from(index)?)
            .context("synthetic supervisor content record descriptor overflow")?;
        let record = mounts
            .open_pinned_regular(OsStr::new(&content_record_name(index)), false)?
            .context("synthetic content record is absent")?;
        record
            .inherited_descriptor_authority()?
            .bind_to_subprocess_request(&mut process, target)
            .map_err(anyhow::Error::msg)?;
    }
    ensure!(
        occurrence
            .open_pinned_regular(OsStr::new(TERMINATE_FILE), false)?
            .is_none(),
        "synthetic occurrence was terminated during supervisor preparation"
    );
    let running = lillux::spawn(process).map_err(|failure| {
        anyhow::anyhow!("launch real external supervisor: {}", failure.stderr)
    })?;
    let supervisor_process = lillux::capture_exact_process_identity(running.pid, Some(running.pid))
        .map_err(anyhow::Error::msg)?;
    ensure_document(
        &occurrence,
        READY_FILE,
        &ReadyRecord {
            schema: 1,
            activation_request_digest: activation.activation_request_digest.clone(),
            server_process_digest: server_process
                .incarnation_digest()
                .map_err(anyhow::Error::msg)?,
            supervisor_process: supervisor_process.clone(),
        },
    )?;
    drop(phase_lock);
    let result = running.wait_interruptible(|| {
        occurrence
            .open_pinned_regular(OsStr::new(TERMINATE_FILE), false)
            .ok()
            .flatten()
            .is_some()
    });
    ensure_document(
        &occurrence,
        TERMINAL_FILE,
        &TerminalRecord {
            schema: 1,
            activation_request_digest: activation.activation_request_digest,
            server_process_digest: server_process
                .incarnation_digest()
                .map_err(anyhow::Error::msg)?,
            supervisor_process_digest: supervisor_process
                .incarnation_digest()
                .map_err(anyhow::Error::msg)?,
            supervisor_success: result.success,
            supervisor_exit_code: result.exit_code,
            supervisor_timed_out: result.timed_out,
            supervisor_stderr: result.stderr,
        },
    )
}

fn require_occurrence(
    root: &lillux::PinnedDirectory,
    request: &LifecycleAdapterRequest,
    occurrence: &BoundOccurrence,
) -> Result<lillux::PinnedDirectory> {
    let expected = occurrence_id(&occurrence.request_digest);
    ensure!(
        occurrence.occurrence_id == expected,
        "synthetic occurrence identity changed"
    );
    let directory = root
        .open_child_directory(OsStr::new(&expected))?
        .context("synthetic occurrence is absent")?;
    let allocation: AllocationRecord = read_document(&directory, ALLOCATION_FILE)?;
    require_allocation(&allocation, request, &occurrence.request_digest, &expected)?;
    Ok(directory)
}

fn require_allocation(
    record: &AllocationRecord,
    request: &LifecycleAdapterRequest,
    request_digest: &str,
    occurrence_id: &str,
) -> Result<()> {
    ensure!(
        record.schema == 1
            && record.request_digest == request_digest
            && record.occurrence_id == occurrence_id
            && record.binding_hash == request.common().binding_hash,
        "synthetic allocation record changed"
    );
    Ok(())
}

fn open_state_root(settings: &SyntheticSettings) -> Result<lillux::PinnedDirectory> {
    let root = lillux::PinnedDirectory::open(&settings.state_root)?
        .context("synthetic lifecycle state root is absent")?;
    root.require_owner_private_directory()?;
    Ok(root)
}

fn occurrence_id(request_digest: &str) -> String {
    format!("occ-{}", &request_digest[..48])
}

fn mount_name(index: usize) -> String {
    format!("mount-{index:02}")
}

fn content_record_name(index: usize) -> String {
    format!("content-record-{index:02}.json")
}

/// Preserve the existing authority records, not a new attestation. The real
/// supervisor verifies product manifests and source binding/manifest/tree joins
/// before Ready. Slots are flattened across mounts, with source binding first.
#[cfg(test)]
fn stage_content_records(
    mounts: &lillux::PinnedDirectory,
    content: &ryeos_external_execution_contract::GuestMountContentAuthority,
    record_index: &mut usize,
    mut adopt: impl FnMut(u32) -> Result<lillux::InheritedDescriptorAuthority>,
) -> Result<Vec<lillux::InheritedDescriptorAuthority>> {
    let mut records = Vec::new();
    for (descriptor, digest, bytes) in content.record_descriptors() {
        let record = adopt(descriptor)?;
        stage_regular(
            mounts,
            &content_record_name(*record_index),
            &record,
            bytes,
            digest,
            0o600,
        )?;
        *record_index = record_index
            .checked_add(1)
            .context("synthetic content record count overflow")?;
        records.push(record);
    }
    Ok(records)
}

fn stage_executable(
    directory: &lillux::PinnedDirectory,
    name: &str,
    authority: &lillux::InheritedDescriptorAuthority,
) -> Result<String> {
    authority.require_owned_executable()?;
    let observation = authority.regular_file_observation()?;
    ensure!(
        observation.size() <= MAX_ARTIFACT_BYTES,
        "synthetic executable exceeds its bound"
    );
    let digest = authority.digest_regular_file_stable_exact(&observation)?;
    let (bytes, after) = authority.read_regular_file_stable_bounded(MAX_ARTIFACT_BYTES)?;
    ensure!(
        after.size() == observation.size(),
        "synthetic executable changed during staging"
    );
    ensure_document_bytes(directory, name, &bytes, 0o700)?;
    Ok(digest)
}

fn measure_executable(authority: &lillux::InheritedDescriptorAuthority) -> Result<String> {
    authority.require_owned_executable()?;
    let observation = authority.regular_file_observation()?;
    ensure!(
        observation.size() <= MAX_ARTIFACT_BYTES,
        "synthetic executable exceeds its bound"
    );
    authority.digest_regular_file_stable_exact(&observation)
}

fn stage_regular(
    directory: &lillux::PinnedDirectory,
    name: &str,
    authority: &lillux::InheritedDescriptorAuthority,
    expected_bytes: u64,
    expected_digest: &str,
    mode: u32,
) -> Result<()> {
    let observation = authority.regular_file_observation()?;
    ensure!(
        observation.size() == expected_bytes
            && expected_bytes <= MAX_ARTIFACT_BYTES
            && authority.digest_regular_file_stable_exact(&observation)? == expected_digest,
        "synthetic regular input changed"
    );
    let (bytes, after) = authority.read_regular_file_stable_bounded(MAX_ARTIFACT_BYTES)?;
    ensure!(
        after.size() == expected_bytes && lillux::sha256_hex(&bytes) == expected_digest,
        "synthetic regular input changed during staging"
    );
    ensure_document_bytes(directory, name, &bytes, mode)
}

fn verify_artifact(
    authority: &lillux::InheritedDescriptorAuthority,
    expected_digest: &str,
    expected_bytes: Option<u64>,
    label: &str,
) -> Result<LifecycleArtifactInspection> {
    authority.require_owned_executable()?;
    let observation = authority.regular_file_observation()?;
    ensure!(
        expected_bytes.is_none_or(|bytes| bytes == observation.size())
            && authority.digest_regular_file_stable_exact(&observation)? == expected_digest,
        "synthetic lifecycle {label} artifact changed"
    );
    Ok(LifecycleArtifactInspection {
        descriptor: authority
            .inherited_descriptor()
            .map_err(anyhow::Error::msg)?,
        digest: expected_digest.into(),
        bytes: observation.size(),
    })
}

fn read_sealed_env(name: &str, maximum: usize) -> Result<Vec<u8>> {
    // SAFETY: the admitted lifecycle runner installs each named sealed
    // descriptor exactly once before this single-threaded startup boundary.
    unsafe { lillux::read_sealed_inherited_descriptor_from_env(name, maximum) }
        .map_err(anyhow::Error::msg)
}

fn ensure_document<T: Serialize>(
    directory: &lillux::PinnedDirectory,
    name: &str,
    value: &T,
) -> Result<()> {
    ensure_document_bytes(directory, name, &canonical_bytes(value)?, 0o600)
}

fn ensure_document_bytes(
    directory: &lillux::PinnedDirectory,
    name: &str,
    bytes: &[u8],
    mode: u32,
) -> Result<()> {
    if let Some(existing) = directory.open_pinned_regular(OsStr::new(name), false)? {
        ensure!(
            existing.read_bounded(MAX_RECORD_BYTES)? == bytes,
            "synthetic retained file changed"
        );
        return Ok(());
    }
    directory.atomic_write_pinned_if_same(OsStr::new(name), None, bytes, mode)
}

fn replace_document<T: Serialize>(
    directory: &lillux::PinnedDirectory,
    name: &str,
    value: &T,
) -> Result<()> {
    let existing = directory
        .open_pinned_regular(OsStr::new(name), false)?
        .context("synthetic replacement incumbent is absent")?;
    directory.atomic_write_pinned_if_same(
        OsStr::new(name),
        Some(&existing),
        &canonical_bytes(value)?,
        0o600,
    )
}

fn read_document<T: for<'de> Deserialize<'de> + Serialize>(
    directory: &lillux::PinnedDirectory,
    name: &str,
) -> Result<T> {
    let file = directory
        .open_pinned_regular(OsStr::new(name), false)?
        .with_context(|| format!("synthetic retained document {name} is absent"))?;
    decode_document(&file)
}

fn decode_document<T: for<'de> Deserialize<'de> + Serialize>(
    file: &lillux::PinnedRegularFile,
) -> Result<T> {
    let bytes = file.read_bounded(MAX_RECORD_BYTES)?;
    let value = serde_json::from_slice(&bytes)?;
    ensure!(
        canonical_bytes(&value)? == bytes,
        "synthetic retained document is noncanonical"
    );
    Ok(value)
}

fn wait_for_document<T: for<'de> Deserialize<'de> + Serialize>(
    directory: &lillux::PinnedDirectory,
    name: &str,
    timeout: lillux::time::Duration,
) -> Result<T> {
    let deadline = lillux::time::MonotonicDeadline::after(timeout);
    loop {
        if let Some(file) = directory.open_pinned_regular(OsStr::new(name), false)? {
            return decode_document(&file);
        }
        if let Some(file) = directory.open_pinned_regular(OsStr::new(FAILURE_FILE), false)? {
            let failure: FailureRecord = decode_document(&file)?;
            bail!(
                "synthetic occurrence failed before {name}: {}",
                failure.message
            );
        }
        ensure!(
            !deadline.has_elapsed(),
            "synthetic lifecycle observation timed out"
        );
        lillux::time::sleep(
            deadline
                .remaining()
                .min(lillux::time::Duration::from_millis(5)),
        );
    }
}

fn canonical_bytes<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    Ok(lillux::canonical_json(&serde_json::to_value(value)?)?.into_bytes())
}

fn evidence_digest<T: Serialize>(kind: &str, value: &T) -> Result<String> {
    Ok(lillux::sha256_hex(&canonical_bytes(&serde_json::json!({
        "domain": "ryeos.synthetic-external-lifecycle-evidence.v1",
        "kind": kind,
        "value": serde_json::to_value(value)?,
    }))?))
}

fn write_response<T: Serialize>(response: &T) -> Result<()> {
    let bytes = ryeos_external_execution_contract::canonical_json(response)?;
    ensure!(
        bytes.len() <= MAX_LIFECYCLE_RESPONSE_BYTES,
        "lifecycle response exceeds its bound"
    );
    println!("{}", String::from_utf8(bytes)?);
    Ok(())
}

fn require_absolute_normalized(path: &Path) -> Result<()> {
    ensure!(
        path.is_absolute()
            && path.components().all(|component| {
                matches!(component, Component::RootDir | Component::Normal(_))
            })
            && path.components().collect::<PathBuf>() == path,
        "synthetic lifecycle path is not normalized"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_contacts_count_exact_retries_and_preserve_request_identity() {
        let fixture = PhaseFixture::new(None);
        record_lifecycle_contact(&fixture.root, &fixture.request).unwrap();
        record_lifecycle_contact(&fixture.root, &fixture.request).unwrap();
        let log: LifecycleContactLog =
            read_document(&fixture.root, LIFECYCLE_CONTACTS_FILE).unwrap();
        log.validate().unwrap();
        assert_eq!(log.contacts.len(), 2);
        assert_eq!(log.contacts[0].sequence, 1);
        assert_eq!(log.contacts[1].sequence, 2);
        assert_eq!(
            log.contacts[0].operation,
            LifecycleContactOperation::ReconcileTermination
        );
        assert_eq!(
            log.contacts[0].operation_id,
            fixture.request.common().operation_id
        );
        assert_eq!(
            log.contacts[0].request_sha256,
            lillux::sha256_hex(&fixture.request.canonical_bytes().unwrap())
        );
        assert_eq!(
            log.contacts[0].request_sha256,
            log.contacts[1].request_sha256
        );
    }

    #[test]
    fn lifecycle_contacts_serialize_concurrent_invocations() {
        let fixture = PhaseFixture::new(None);
        let mut tasks = Vec::new();
        for _ in 0..4 {
            let path = fixture._temp.path().to_path_buf();
            let request = fixture.request.clone();
            tasks.push(
                lillux::task::spawn_host_task("synthetic-contact-test", move || {
                    let root = lillux::PinnedDirectory::open(&path).unwrap().unwrap();
                    for _ in 0..4 {
                        record_lifecycle_contact(&root, &request).unwrap();
                    }
                })
                .unwrap(),
            );
        }
        for task in tasks {
            task.join().unwrap();
        }
        let log: LifecycleContactLog =
            read_document(&fixture.root, LIFECYCLE_CONTACTS_FILE).unwrap();
        log.validate().unwrap();
        assert_eq!(log.contacts.len(), 16);
    }

    #[test]
    fn lifecycle_contacts_refuse_full_or_corrupt_history_without_replacement() {
        let fixture = PhaseFixture::new(None);
        record_lifecycle_contact(&fixture.root, &fixture.request).unwrap();
        let mut log: LifecycleContactLog =
            read_document(&fixture.root, LIFECYCLE_CONTACTS_FILE).unwrap();
        let first = log.contacts[0].clone();
        log.contacts = (1..=MAX_LIFECYCLE_CONTACTS)
            .map(|sequence| LifecycleContact {
                sequence: sequence as u64,
                ..first.clone()
            })
            .collect();
        replace_document(&fixture.root, LIFECYCLE_CONTACTS_FILE, &log).unwrap();
        let before = canonical_bytes(&log).unwrap();
        assert!(record_lifecycle_contact(&fixture.root, &fixture.request).is_err());
        assert_eq!(
            fixture
                .root
                .open_pinned_regular(OsStr::new(LIFECYCLE_CONTACTS_FILE), false)
                .unwrap()
                .unwrap()
                .read_bounded(MAX_RECORD_BYTES)
                .unwrap(),
            before
        );

        log.contacts.truncate(1);
        log.contacts[0].sequence = 3;
        replace_document(&fixture.root, LIFECYCLE_CONTACTS_FILE, &log).unwrap();
        assert!(record_lifecycle_contact(&fixture.root, &fixture.request).is_err());
        let retained: LifecycleContactLog =
            read_document(&fixture.root, LIFECYCLE_CONTACTS_FILE).unwrap();
        assert_eq!(retained, log);
    }

    #[test]
    fn lifecycle_contacts_refuse_noncanonical_or_oversized_history() {
        for bytes in [
            b"{\"schema\":1, \"contacts\":[]}".to_vec(),
            vec![b' '; MAX_LIFECYCLE_CONTACT_BYTES + 1],
        ] {
            let fixture = PhaseFixture::new(None);
            ensure_document_bytes(&fixture.root, LIFECYCLE_CONTACTS_FILE, &bytes, 0o600).unwrap();
            assert!(record_lifecycle_contact(&fixture.root, &fixture.request).is_err());
            assert_eq!(
                fixture
                    .root
                    .open_pinned_regular(OsStr::new(LIFECYCLE_CONTACTS_FILE), false)
                    .unwrap()
                    .unwrap()
                    .read_bounded(MAX_RECORD_BYTES)
                    .unwrap(),
                bytes
            );
        }
    }

    #[test]
    fn content_records_use_flattened_slots_and_preserve_source_pair() {
        use ryeos_external_execution_contract::{
            GuestMountContentAuthority as Content, GuestProductManifestKind,
        };
        let temp = tempfile::tempdir().unwrap();
        let root = lillux::PinnedDirectory::open(temp.path()).unwrap().unwrap();
        let mounts = root.create_child(OsStr::new("mounts"), 0o700).unwrap();
        // These bytes test descriptor transport, not source admission. Only the
        // real supervisor can accept their canonical records and tree relation.
        let records: [(u32, &[u8]); 3] = [
            (301, b"exact source binding"),
            (307, b"exact source manifest"),
            (311, b"exact product manifest"),
        ];
        for (descriptor, bytes) in records {
            ensure_document_bytes(&root, &format!("input-{descriptor}"), bytes, 0o600).unwrap();
        }
        let contents = [
            Content::RawFile {
                sha256: "a".repeat(64),
            },
            Content::SourceClosure {
                binding_hash: lillux::sha256_hex(records[0].1),
                binding_descriptor: records[0].0,
                binding_bytes: records[0].1.len() as u64,
                manifest_hash: lillux::sha256_hex(records[1].1),
                manifest_descriptor: records[1].0,
                manifest_bytes: records[1].1.len() as u64,
            },
            Content::PrivateScratch {
                binding_hash: "b".repeat(64),
            },
            Content::ProductManifest {
                manifest_kind: GuestProductManifestKind::Content,
                manifest_hash: lillux::sha256_hex(records[2].1),
                manifest_descriptor: records[2].0,
                manifest_bytes: records[2].1.len() as u64,
            },
        ];
        let mut index = 0;
        let mut adopted = Vec::new();
        for content in &contents {
            let staged = stage_content_records(&mounts, content, &mut index, |descriptor| {
                adopted.push(descriptor);
                Ok(root
                    .open_pinned_regular(OsStr::new(&format!("input-{descriptor}")), false)?
                    .context("fixture descriptor absent")?
                    .inherited_descriptor_authority()?)
            })
            .unwrap();
            assert_eq!(staged.len(), content.record_descriptors().count());
        }
        assert_eq!(index, 3);
        assert_eq!(adopted, vec![301, 307, 311]);
        // This is the same flattened order used by the fixed supervisor slots:
        // raw/scratch mounts consume no content-record descriptor.
        for (index, (_, expected)) in records.iter().enumerate() {
            let record = mounts
                .open_pinned_regular(OsStr::new(&content_record_name(index)), false)
                .unwrap()
                .unwrap()
                .inherited_descriptor_authority()
                .unwrap();
            let (bytes, _) = record.read_regular_file_stable_bounded(1024).unwrap();
            assert_eq!(bytes.as_slice(), *expected);
        }
        assert!(
            mounts
                .open_pinned_regular(OsStr::new(&content_record_name(3)), false)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn content_records_refuse_missing_changed_or_wrong_size_authority() {
        use ryeos_external_execution_contract::GuestMountContentAuthority as Content;
        let temp = tempfile::tempdir().unwrap();
        let root = lillux::PinnedDirectory::open(temp.path()).unwrap().unwrap();
        ensure_document_bytes(&root, "binding", b"binding", 0o600).unwrap();
        let content = Content::SourceClosure {
            binding_hash: lillux::sha256_hex(b"binding"),
            binding_descriptor: 101,
            binding_bytes: 7,
            manifest_hash: lillux::sha256_hex(b"manifest"),
            manifest_descriptor: 103,
            manifest_bytes: 8,
        };
        let mut missing_index = 0;
        let missing = root.create_child(OsStr::new("missing"), 0o700).unwrap();
        let result = stage_content_records(&missing, &content, &mut missing_index, |descriptor| {
            ensure!(descriptor == 101, "fixture manifest descriptor absent");
            Ok(root
                .open_pinned_regular(OsStr::new("binding"), false)?
                .unwrap()
                .inherited_descriptor_authority()?)
        });
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("manifest descriptor absent")
        );
        assert_eq!(missing_index, 1);
        for (name, wrong_bytes) in [
            ("changed", &b"changed"[..]),
            ("wrong-size", &b"long binding"[..]),
        ] {
            let mounts = root.create_child(OsStr::new(name), 0o700).unwrap();
            ensure_document_bytes(&mounts, "wrong", wrong_bytes, 0o600).unwrap();
            let mut index = 0;
            let result = stage_content_records(&mounts, &content, &mut index, |_| {
                Ok(mounts
                    .open_pinned_regular(OsStr::new("wrong"), false)?
                    .unwrap()
                    .inherited_descriptor_authority()?)
            });
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("regular input changed")
            );
            assert_eq!(index, 0);
            assert!(
                mounts
                    .open_pinned_regular(OsStr::new(&content_record_name(0)), false)
                    .unwrap()
                    .is_none()
            );
        }
    }

    // Harness-owned journal fixtures exercise authority transitions without
    // launching a process. Executable fault-injection acceptance remains a
    // separate gate; these records are not deployment testimony.
    struct PhaseFixture {
        _temp: tempfile::TempDir,
        root: lillux::PinnedDirectory,
        occurrence: lillux::PinnedDirectory,
        bound: BoundOccurrence,
        termination: ryeos_external_execution_contract::TerminationIntent,
        request: LifecycleAdapterRequest,
        activation: ActivationRecord,
    }

    impl PhaseFixture {
        fn new(phase: Option<ActivationPhase>) -> Self {
            let temp = tempfile::tempdir().unwrap();
            let root = lillux::PinnedDirectory::open(temp.path()).unwrap().unwrap();
            let bound = BoundOccurrence {
                request_digest: "a".repeat(64),
                occurrence_id: occurrence_id(&"a".repeat(64)),
            };
            let occurrence = root
                .create_child(OsStr::new(&bound.occurrence_id), 0o700)
                .unwrap();
            let common = ryeos_external_execution_contract::LifecycleOperationCommon {
                schema: 1,
                protocol: LIFECYCLE_ADAPTER_PROTOCOL.into(),
                operation_id: "fixture-termination".into(),
                binding_hash: "b".repeat(64),
                settings_digest: "c".repeat(64),
            };
            ensure_document(
                &occurrence,
                ALLOCATION_FILE,
                &AllocationRecord {
                    schema: 1,
                    operation_id: "fixture-allocation".into(),
                    request_digest: bound.request_digest.clone(),
                    occurrence_id: bound.occurrence_id.clone(),
                    binding_hash: common.binding_hash.clone(),
                },
            )
            .unwrap();
            let termination = ryeos_external_execution_contract::TerminationIntent {
                termination_request_digest: "d".repeat(64),
            };
            let request = LifecycleAdapterRequest::ReconcileTermination {
                common,
                occurrence: bound.clone(),
                termination: termination.clone(),
            };
            let activation = ActivationRecord {
                schema: 2,
                operation_id: "fixture-activation".into(),
                occurrence_id: bound.occurrence_id.clone(),
                activation_request_digest: "e".repeat(64),
                server_nonce: "f".repeat(64),
                server_process: None,
                supervisor_digest: "1".repeat(64),
                launcher_digest: "2".repeat(64),
                guest_input_identity: "3".repeat(64),
                phase: phase.unwrap_or(ActivationPhase::Staging),
            };
            if phase.is_some() {
                ensure_document(&occurrence, ACTIVATION_FILE, &activation).unwrap();
            }
            ensure_document(
                &occurrence,
                TERMINATE_FILE,
                &TerminateRecord {
                    schema: 1,
                    termination_request_digest: termination.termination_request_digest.clone(),
                },
            )
            .unwrap();
            Self {
                _temp: temp,
                root,
                occurrence,
                bound,
                termination,
                request,
                activation,
            }
        }

        fn reconcile(&self) -> LifecycleAdapterResponse {
            reconcile_termination(&self.root, &self.request, &self.bound, &self.termination)
                .unwrap()
        }

        fn reconcile_start(&self) -> LifecycleAdapterResponse {
            let mut common = self.request.common().clone();
            common.operation_id = self.activation.operation_id.clone();
            let activation = ryeos_external_execution_contract::SupervisorActivationIntent {
                activation_request_digest: self.activation.activation_request_digest.clone(),
                supervisor_runtime_hash: "4".repeat(64),
                launcher_artifact_hash: self.activation.launcher_digest.clone(),
                attachment_deadline_ms: i64::try_from(lillux::time::timestamp_millis()).unwrap()
                    + 60_000,
                execution_timeout_seconds: 60,
                post_execution_timeout_seconds: 1,
                channel_max_bytes: 1024,
            };
            let request = LifecycleAdapterRequest::ReconcileSupervisorActivation {
                common,
                occurrence: self.bound.clone(),
                activation: activation.clone(),
            };
            reconcile_activation(&self.root, &request, &self.bound, &activation).unwrap()
        }
    }

    #[test]
    fn pre_spawn_fence_waits_for_actor_lock_and_retains_exact_terminal_evidence() {
        let fixture = PhaseFixture::new(Some(ActivationPhase::Staging));
        let guard = fixture.occurrence.try_lock_exclusive().unwrap().unwrap();
        assert!(matches!(
            fixture.reconcile(),
            LifecycleAdapterResponse::TerminationPending { .. }
        ));
        assert!(matches!(
            fixture.reconcile_start(),
            LifecycleAdapterResponse::SupervisorPending { .. }
        ));
        drop(guard);
        let terminal = fixture.reconcile();
        assert!(matches!(
            terminal,
            LifecycleAdapterResponse::OccurrenceTerminal { .. }
        ));
        assert_eq!(terminal, fixture.reconcile());
        assert!(matches!(
            fixture.reconcile_start(),
            LifecycleAdapterResponse::SupervisorNotStarted { .. }
        ));
        let guard = fixture.occurrence.try_lock_exclusive().unwrap().unwrap();
        let mut retained: ActivationRecord =
            read_document(&fixture.occurrence, ACTIVATION_FILE).unwrap();
        assert_eq!(retained.phase, ActivationPhase::FencedBeforeSpawn);
        assert!(retained.server_process.is_none());
        assert!(!author_spawn_intent(&fixture.occurrence, &guard, &mut retained).unwrap());
        assert_eq!(retained.phase, ActivationPhase::FencedBeforeSpawn);
    }

    #[test]
    fn absent_or_spawn_intent_activation_never_proves_no_process() {
        for phase in [None, Some(ActivationPhase::SpawnIntent)] {
            let fixture = PhaseFixture::new(phase);
            assert!(matches!(
                fixture.reconcile(),
                LifecycleAdapterResponse::TerminationPending { .. }
            ));
            assert!(matches!(
                fixture.reconcile_start(),
                LifecycleAdapterResponse::SupervisorPending { .. }
            ));
            if let Some(phase) = phase {
                let retained: ActivationRecord =
                    read_document(&fixture.occurrence, ACTIVATION_FILE).unwrap();
                assert_eq!(retained.phase, phase);
            }
        }
    }

    #[test]
    fn pre_spawn_fence_refuses_wrong_lock_and_predecessor_record() {
        let fixture = PhaseFixture::new(Some(ActivationPhase::Staging));
        let wrong_guard = fixture.root.try_lock_exclusive().unwrap().unwrap();
        let mut retained = fixture.activation.clone();
        assert!(
            fence_staging_for_termination(&fixture.occurrence, &wrong_guard, &mut retained)
                .is_err()
        );
        assert_eq!(retained.phase, ActivationPhase::Staging);
        retained.schema = 1;
        assert!(retained.validate().is_err());
        let mut encoded = serde_json::to_value(&fixture.activation).unwrap();
        encoded.as_object_mut().unwrap().remove("phase");
        assert!(serde_json::from_value::<ActivationRecord>(encoded).is_err());
    }

    #[test]
    fn stale_staging_cannot_fence_spawn_intent_or_reopen_a_fenced_occurrence() {
        for phase in [
            ActivationPhase::SpawnIntent,
            ActivationPhase::FencedBeforeSpawn,
        ] {
            let fixture = PhaseFixture::new(Some(phase));
            let guard = fixture.occurrence.try_lock_exclusive().unwrap().unwrap();
            let mut stale = fixture.activation.clone();
            stale.phase = ActivationPhase::Staging;
            assert!(
                fence_staging_for_termination(&fixture.occurrence, &guard, &mut stale).is_err()
            );
            assert!(author_spawn_intent(&fixture.occurrence, &guard, &mut stale).is_err());
            let retained: ActivationRecord =
                read_document(&fixture.occurrence, ACTIVATION_FILE).unwrap();
            assert_eq!(retained, fixture.activation);
        }
    }

    #[test]
    fn settings_refuse_relative_or_unbounded_state() {
        let valid = SyntheticSettings {
            schema: 1,
            state_root: PathBuf::from("/tmp/ryeos-synthetic-external"),
            expected_credential_sha256: "a".repeat(64),
            maximum_copy_entries: 100,
            maximum_copy_depth: 8,
            startup_timeout_ms: 1_000,
            faults: BTreeSet::new(),
        };
        valid.validate().unwrap();
        let mut invalid = valid.clone();
        invalid.state_root = PathBuf::from("relative");
        assert!(invalid.validate().is_err());
        let mut invalid = valid;
        invalid.maximum_copy_entries = 0;
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn occurrence_identity_is_stable_and_bounded() {
        let request = "b".repeat(64);
        assert_eq!(occurrence_id(&request), format!("occ-{}", "b".repeat(48)));
    }
}
