//! A provider-free placement backend for an ordinary relay session.
//!
//! Allocation names a local occurrence, activation hands the controller's
//! real guest package, signed import and node-signed assignment to a local
//! rehearsal host that starts the real guest owner in a root enclosure, and
//! every relay file effect is a file move in that host's mailbox. The relay
//! request each effect serves is the exact one an installed lifecycle adapter
//! receives, and the response is checked the same way.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use ryeos_external_execution::guest_import_authorization::GuestOwnerRuntimeProfile;
use ryeos_external_execution::guest_relay_loopback::{
    GuestRelayLoopbackEnd, GuestRelayLoopbackHost, GuestRelayLoopbackRequest,
};
use ryeos_external_execution::guest_start_rehearsal::{
    GuestStartRehearsalBase, GuestStartRehearsalFileProduct, RehearsalInputsRequest,
    rehearsal_guest_inputs,
};
use ryeos_external_execution_contract::relay_file::{
    RelayDeliveryAccount, RelayFileAdapterResponse, RelayFileEffect, RelayFileKind,
};
use ryeos_state::external_execution::admission::ExternalCandidateRequirement;
use ryeos_state::external_execution::transport::{
    ExternalControllerFileRelayContract, ExternalControllerTransportSelection,
    ExternalNetworkInputPolicy,
};

use super::*;
use crate::runtime_db::external_execution::ClaimedRelayFileContact;

/// A fault the loopback injects into its own transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuestRelayLoopbackFault {
    /// The attach response is delivered with a corrupted signature.
    CorruptAttachResponse,
}

/// One relay file effect the loopback served.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestRelayLoopbackContact {
    pub kind: RelayFileKind,
    pub sequence: u32,
    pub effect: RelayFileEffect,
    /// Download only: whether the file was present.
    pub found: bool,
    pub bytes: u64,
    /// Milliseconds since the backend was created.
    pub at_ms: u64,
}

#[derive(Default)]
struct Guest {
    host: Option<GuestRelayLoopbackHost>,
    end: Option<GuestRelayLoopbackEnd>,
    activated_at_ms: Option<u64>,
}

pub struct GuestRelayLoopbackBackend {
    backend_id: String,
    artifact_hash: String,
    settings_schema_digest: String,
    supervisor: lillux::InheritedDescriptorAuthority,
    supervisor_hash: String,
    supervisor_bytes: u64,
    launcher: lillux::InheritedDescriptorAuthority,
    launcher_hash: String,
    launcher_bytes: u64,
    runtime: lillux::PinnedDirectory,
    runtime_manifest_hash: String,
    owner_deadline_ms: u64,
    work_parent: PathBuf,
    host_executable: PathBuf,
    fault: Option<GuestRelayLoopbackFault>,
    created: std::time::Instant,
    guest: Mutex<Guest>,
    contacts: Mutex<Vec<GuestRelayLoopbackContact>>,
}

impl std::fmt::Debug for GuestRelayLoopbackBackend {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("GuestRelayLoopbackBackend")
    }
}

impl GuestRelayLoopbackBackend {
    /// Milliseconds since the backend was created.
    pub fn elapsed_ms(&self) -> u64 {
        u64::try_from(self.created.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// Every relay effect served so far, in order.
    pub fn contacts(&self) -> Vec<GuestRelayLoopbackContact> {
        self.contacts.lock().map(|c| c.clone()).unwrap_or_default()
    }

    /// Milliseconds from backend creation to the start of activation.
    pub fn activated_at_ms(&self) -> Option<u64> {
        self.guest
            .lock()
            .ok()
            .and_then(|guest| guest.activated_at_ms)
    }

    /// Owner stdout forwarded so far.
    pub fn owner_stdout(&self) -> String {
        let Ok(mut guest) = self.guest.lock() else {
            return String::new();
        };
        if let Some(end) = &guest.end {
            return end.owner_stdout.clone();
        }
        match guest.host.as_mut() {
            Some(host) => {
                host.poll();
                host.owner_stdout().to_owned()
            }
            None => String::new(),
        }
    }

    /// End the guest host, asking the owner to cancel first. Idempotent.
    pub fn stop_guest(&self) -> Option<GuestRelayLoopbackEnd> {
        let mut guest = self.guest.lock().ok()?;
        if let Some(host) = guest.host.take() {
            guest.end = host.stop().ok();
        }
        guest.end.clone()
    }

    fn record(&self, contact: GuestRelayLoopbackContact) {
        if let Ok(mut contacts) = self.contacts.lock() {
            contacts.push(contact);
        }
    }
}

fn corrupt_signature(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut envelope: serde_json::Value = serde_json::from_slice(bytes)?;
    let signature = envelope["signature"]
        .as_str()
        .context("relay response has no signature")?
        .to_owned();
    let mut chars: Vec<char> = signature.chars().collect();
    let first = chars
        .first_mut()
        .context("relay response signature is empty")?;
    *first = if *first == 'A' { 'B' } else { 'A' };
    envelope["signature"] = serde_json::Value::String(chars.into_iter().collect());
    Ok(lillux::canonical_json(&envelope)?.into_bytes())
}

impl ExternalPlacementBackend for GuestRelayLoopbackBackend {
    fn backend_id(&self) -> &str {
        &self.backend_id
    }

    fn lifecycle_capabilities(&self) -> BTreeSet<LifecycleCapability> {
        BTreeSet::from([
            LifecycleCapability::SupervisorActivation,
            LifecycleCapability::ControllerFileRelay,
            LifecycleCapability::ExactAllocationReconciliation,
            LifecycleCapability::AuthoritativeNoOccurrence,
            LifecycleCapability::ExactActivationReconciliation,
            LifecycleCapability::IdempotentTermination,
            LifecycleCapability::ExactTerminalObservation,
        ])
    }

    fn artifact_hash(&self) -> &str {
        &self.artifact_hash
    }

    fn artifact_bytes(&self) -> u64 {
        4096
    }

    fn supervisor_artifact(&self) -> (&str, u64) {
        (&self.supervisor_hash, self.supervisor_bytes)
    }

    fn launcher_artifact(&self) -> (&str, u64) {
        (&self.launcher_hash, self.launcher_bytes)
    }

    fn settings_schema_digest(&self) -> &str {
        &self.settings_schema_digest
    }

    /// The key the composed settings carry the relay's workspace scope under.
    fn relay_scope_setting(&self) -> Option<&str> {
        Some("owner_id")
    }

    fn qualify_offline(
        &self,
        _contract: &ExternalPlacementBackendContract,
        _credential: &PlacementCredential,
    ) -> Result<()> {
        Ok(())
    }

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
        let bootstrap_sha256 = lillux::sha256_hex(&bootstrap_bytes);
        let bootstrap_file =
            lillux::sealed_memfd(c"ryeos-loopback-guest-bootstrap", &bootstrap_bytes)
                .map_err(anyhow::Error::msg)?;
        let expected = ryeos_external_execution_contract::staging_package::GuestStagingExpected {
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
            guest_input_identity: guest_inputs.identity_digest()?,
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
        self.reconcile_allocation(contract, credential, reservation, deadline)
    }

    fn reconcile_allocation(
        &self,
        _contract: &ExternalPlacementBackendContract,
        _credential: &PlacementCredential,
        reservation: &ExternalAllocationReservation,
        _deadline: lillux::time::MonotonicDeadline,
    ) -> Result<ExternalLifecycleObservation<ExternalAllocationResolution>> {
        Ok(ExternalLifecycleObservation {
            value: ExternalAllocationResolution::Bound {
                occurrence_id: format!("loopback-{}", &reservation.request_digest[..32]),
                provider_observation_digest: lillux::sha256_hex(
                    format!("loopback-allocation:{}", reservation.request_digest).as_bytes(),
                ),
            },
            deadline_exceeded: false,
        })
    }

    fn activate_supervisor(
        &self,
        _contract: &ExternalPlacementBackendContract,
        _credential: &PlacementCredential,
        _reservation: &ExternalAllocationReservation,
        occurrence: &ExternalAllocationOccurrence,
        _intent: &ExternalSupervisorActivationIntent,
        activation: &ExternalSupervisorActivation,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<ExternalLifecycleObservation<ExternalSupervisorActivationResolution>> {
        let mut guest = self
            .guest
            .lock()
            .map_err(|_| anyhow::anyhow!("loopback guest is poisoned"))?;
        ensure!(
            guest.host.is_none() && guest.end.is_none(),
            "loopback supervisor activation was already contacted"
        );
        guest.activated_at_ms = Some(self.elapsed_ms());
        let signed_import = ryeos_external_execution_contract::canonical_json(
            activation.signed_import_authorization()?,
        )?;
        let signed_assignment =
            ryeos_external_execution_contract::canonical_json(activation.signed_assignment()?)?;
        let mailbox_root = activation.bootstrap().relay_mailbox_root()?;
        let mut host = GuestRelayLoopbackHost::start(&GuestRelayLoopbackRequest {
            runtime: &self.runtime,
            runtime_manifest_hash: &self.runtime_manifest_hash,
            signed_import: &signed_import,
            package: activation.prepared_guest_package()?.payload(),
            signed_assignment: &signed_assignment,
            occurrence_id: &occurrence.occurrence_id,
            mailbox_root: &mailbox_root,
            owner_deadline_ms: self.owner_deadline_ms,
            work_parent: &self.work_parent,
            host_executable: &self.host_executable,
        })
        .context("start the loopback guest host")?;
        // The owner reports held once it has released the supervisor; as
        // with a provider's run stream, held is not Ready.
        let report = host.await_owner_report(deadline);
        guest.host = Some(host);
        Ok(ExternalLifecycleObservation {
            value: ExternalSupervisorActivationResolution::Pending {
                guest_owner: Some(report),
                contact_failure: None,
            },
            deadline_exceeded: false,
        })
    }

    fn reconcile_supervisor_activation(
        &self,
        _contract: &ExternalPlacementBackendContract,
        _credential: &PlacementCredential,
        _reservation: &ExternalAllocationReservation,
        _occurrence: &ExternalAllocationOccurrence,
        _intent: &ExternalSupervisorActivationIntent,
        _deadline: lillux::time::MonotonicDeadline,
    ) -> Result<ExternalLifecycleObservation<ExternalSupervisorActivationResolution>> {
        let mut guest = self
            .guest
            .lock()
            .map_err(|_| anyhow::anyhow!("loopback guest is poisoned"))?;
        let host = guest
            .host
            .as_mut()
            .context("loopback activation has no running guest")?;
        host.poll();
        // Only an owner that ended before releasing its supervisor proves the
        // supervisor never started: after release the owner reports held, and
        // its later exit is the supervisor's end, which the relay learns from
        // the owner's report file.
        if let Some(code) = host.owner_exited()
            && !host.owner_released_supervisor()
        {
            return Ok(ExternalLifecycleObservation {
                value: ExternalSupervisorActivationResolution::NotStarted {
                    provider_observation_digest: lillux::sha256_hex(
                        format!("loopback-owner-exited:{code:?}").as_bytes(),
                    ),
                },
                deadline_exceeded: false,
            });
        }
        Ok(ExternalLifecycleObservation {
            value: ExternalSupervisorActivationResolution::Pending {
                guest_owner: None,
                contact_failure: None,
            },
            deadline_exceeded: false,
        })
    }

    fn relay_file_contact(
        &self,
        contract: &ExternalPlacementBackendContract,
        _credential: &PlacementCredential,
        bootstrap: &ryeos_state::external_execution::transport::ExternalSupervisorBootstrap,
        claim: ClaimedRelayFileContact,
        delivery_account: Option<RelayDeliveryAccount>,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<ExternalLifecycleObservation<RelayFileAdapterResponse>> {
        let request = crate::external_lifecycle_adapter::relay_file_adapter_request(
            contract,
            bootstrap,
            &claim,
            delivery_account,
        )?;
        let request_digest = lillux::sha256_hex(&request.canonical_bytes()?);
        let deadline = deadline.min(lillux::time::MonotonicDeadline::after(
            lillux::time::Duration::from_secs(u64::from(contract.contact_timeout_seconds)),
        ));
        let mut guest = self
            .guest
            .lock()
            .map_err(|_| anyhow::anyhow!("loopback guest is poisoned"))?;
        let host = guest
            .host
            .as_mut()
            .context("loopback relay has no running guest")?;
        let (response, found, bytes) = match request.effect {
            RelayFileEffect::Download => match host.download(&request.remote_path, deadline)? {
                Some(bytes) => (
                    RelayFileAdapterResponse::Downloaded {
                        request_digest: request_digest.clone(),
                        bytes_base64: STANDARD.encode(&bytes),
                        sha256: lillux::sha256_hex(&bytes),
                        bytes: bytes.len() as u64,
                    },
                    true,
                    bytes.len() as u64,
                ),
                None => (
                    RelayFileAdapterResponse::DownloadHttpNotFound {
                        request_digest: request_digest.clone(),
                    },
                    false,
                    0,
                ),
            },
            RelayFileEffect::UploadOnce => {
                let mut bytes = claim
                    .record()
                    .canonical_bytes
                    .clone()
                    .context("claimed relay upload lost retained bytes")?;
                if self.fault == Some(GuestRelayLoopbackFault::CorruptAttachResponse)
                    && request.kind == RelayFileKind::Attach
                {
                    bytes = corrupt_signature(&bytes)?;
                }
                let RelayDeliveryAccount::Unix { uid, gid } = request
                    .delivery_account
                    .context("relay upload names no delivery account")?;
                host.upload(&request.remote_path, &bytes, uid, gid, deadline)?;
                (
                    RelayFileAdapterResponse::UploadTransportObserved {
                        request_digest: request_digest.clone(),
                    },
                    true,
                    bytes.len() as u64,
                )
            }
        };
        drop(guest);
        self.record(GuestRelayLoopbackContact {
            kind: request.kind,
            sequence: request.sequence,
            effect: request.effect,
            found,
            bytes,
            at_ms: self.elapsed_ms(),
        });
        response.validate_for(&request, &request_digest)?;
        Ok(ExternalLifecycleObservation {
            value: response,
            deadline_exceeded: false,
        })
    }

    fn terminate(
        &self,
        _contract: &ExternalPlacementBackendContract,
        _credential: &PlacementCredential,
        _reservation: &ExternalAllocationReservation,
        occurrence: &ExternalAllocationOccurrence,
        _intent: &ExternalTerminationIntent,
    ) -> Result<ExternalTerminationResolution> {
        self.stop_guest();
        Ok(ExternalTerminationResolution::Terminal {
            provider_observation_digest: lillux::sha256_hex(
                format!("loopback-terminated:{}", occurrence.occurrence_id).as_bytes(),
            ),
        })
    }

    fn reconcile_termination(
        &self,
        contract: &ExternalPlacementBackendContract,
        credential: &PlacementCredential,
        reservation: &ExternalAllocationReservation,
        occurrence: &ExternalAllocationOccurrence,
        intent: &ExternalTerminationIntent,
    ) -> Result<ExternalTerminationResolution> {
        self.terminate(contract, credential, reservation, occurrence, intent)
    }
}

/// Read one placement's original sealed relay bootstrap and signed import
/// from a node's protected vault without writing to it. The bootstrap
/// capability is blanked; the documents are for local rehearsal only.
pub fn read_retained_relay_bootstrap(
    app_root: &Path,
    placement: &str,
    request_digest: &str,
) -> Result<(serde_json::Value, serde_json::Value)> {
    let secret = lillux::vault::read_secret_key(
        &ryeos_vault::paths::default_vault_secret_key_path(app_root),
    )
    .map_err(|error| anyhow::anyhow!("read the node vault key: {error:#}"))?;
    let vault = crate::vault::SealedEnvelopeVault::new(
        ryeos_vault::paths::default_sealed_store_path(app_root),
        secret,
    );
    let sealed = vault.external_relay_bootstrap(&ExternalRelayBootstrapAccess::new(
        placement,
        request_digest,
    )?)?;
    let mut bootstrap = serde_json::to_value(sealed.bootstrap())?;
    bootstrap["bootstrap_capability"] = serde_json::Value::String(String::new());
    Ok((bootstrap, serde_json::to_value(sealed.signed_import())?))
}

/// Direction and payload kind of every channel frame the controller has
/// journalled, in journal order.
pub fn journalled_frame_kinds(state: &AppState) -> Result<Vec<(String, String)>> {
    state.state_store.with_runtime_sql_for_test(|connection| {
        let mut statement = connection.prepare(
            "SELECT direction, json_extract(frame_json,'$.frame.payload.kind')
               FROM external_execution_frame ORDER BY rowid",
        )?;
        let kinds = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(kinds)
    })
}

/// Relay transfers the controller's journal accepted, as kind, sequence and
/// direction, in journal order.
pub fn accepted_relay_transfers(state: &AppState) -> Result<Vec<(String, u32, String)>> {
    state.state_store.with_runtime_sql_for_test(|connection| {
        let mut statement = connection.prepare(
            "SELECT kind, sequence, direction FROM external_execution_relay_transfer
              WHERE phase='accepted' ORDER BY rowid",
        )?;
        let accepted = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(accepted)
    })
}

/// What a loopback session starts from. Paths are copied before use.
pub struct GuestRelayLoopbackSetup<'a> {
    /// The backend the composed binding names; the loopback stands in for it.
    pub backend_id: &'a str,
    pub owner_executable: &'a Path,
    pub profile: GuestOwnerRuntimeProfile,
    pub supervisor_executable: &'a Path,
    pub launcher_executable: &'a Path,
    /// The structured-session requirement whose recipe the candidate runs.
    pub requirement: ExternalCandidateRequirement,
    /// Directory mounted at the recipe's runtime destination.
    pub product: &'a Path,
    pub product_manifest_record: Option<&'a Path>,
    pub file_products: &'a [GuestStartRehearsalFileProduct<'a>],
    pub base_snapshot: Option<GuestStartRehearsalBase<'a>>,
    pub relay: ExternalControllerFileRelayContract,
    pub network_inputs: ExternalNetworkInputPolicy,
    /// Private parent for the session's and the guest host's work.
    pub work_parent: &'a Path,
    /// Executable that runs the rehearsal host entry first thing.
    pub host_executable: &'a Path,
    pub fault: Option<GuestRelayLoopbackFault>,
}

/// A private work directory removed with everything under it on drop.
struct WorkDirectory(Option<(lillux::PinnedDirectory, OsString, lillux::PinnedDirectory)>);

impl WorkDirectory {
    fn directory(&self) -> &lillux::PinnedDirectory {
        &self
            .0
            .as_ref()
            .expect("work directory is held until drop")
            .2
    }
}

impl Drop for WorkDirectory {
    fn drop(&mut self) {
        if let Some((parent, name, work)) = self.0.take() {
            let _ = work
                .remove_contents_recursive_bounded(lillux::DirectoryTraversalBudget::new(
                    1_000_000, 64,
                ))
                .and_then(|_| parent.remove_empty_child_if_same(&name, &work).map(|_| ()));
        }
    }
}

/// A reserved session whose start owner will contact the loopback backend.
pub struct GuestRelayLoopbackSession {
    pub placement: String,
    pub backend: Arc<GuestRelayLoopbackBackend>,
    /// Consumed by the first activation.
    pub guest_inputs: Option<ryeos_external_execution::guest_inputs::ExternalGuestInputAuthority>,
    // Dropped last: the inputs and runtime above live under it.
    _work: WorkDirectory,
}

impl Drop for GuestRelayLoopbackSession {
    fn drop(&mut self) {
        self.backend.stop_guest();
        self.guest_inputs = None;
    }
}

fn copy_executable(work: &lillux::PinnedDirectory, name: &str, source: &Path) -> Result<PathBuf> {
    let bytes = std::fs::read(source).with_context(|| format!("read {}", source.display()))?;
    let path = work.path().join(name);
    lillux::atomic_write_with_mode(&path, &bytes, 0o755)?;
    Ok(path)
}

fn executable_authority(
    path: &Path,
) -> Result<(lillux::InheritedDescriptorAuthority, String, u64)> {
    let pinned = lillux::secure_fs::open_pinned_regular_file_no_follow(path)?;
    pinned.require_executable()?;
    let observation = pinned.observation()?;
    let hash = pinned.digest_stable_exact(&observation)?;
    Ok((
        pinned.inherited_descriptor_authority()?,
        hash,
        observation.size(),
    ))
}

/// Install the loopback backend, its node-config binding and a reserved
/// session for a capsule whose program runs `setup.requirement` over
/// `setup.product`. The guest owner runtime names no controller; its manifest
/// is held in the node's CAS.
pub fn compose_guest_relay_loopback_session(
    state: &mut AppState,
    setup: &GuestRelayLoopbackSetup<'_>,
) -> Result<GuestRelayLoopbackSession> {
    let parent = lillux::PinnedDirectory::open(setup.work_parent)?.with_context(|| {
        format!(
            "loopback work parent {} is absent",
            setup.work_parent.display()
        )
    })?;
    let (name, work) = parent.create_unique_child("guest-relay-loopback-controller", 0o700)?;
    let guard = WorkDirectory(Some((parent, name, work)));
    let work = guard.directory();
    test_support::install_test_placement_vault(state);
    // The node's signed maintenance policy funds the supervisor journal; these
    // are the signed profiles' values.
    let maintenance: ryeos_scheduler_contract::NodeMaintenancePolicy =
        serde_json::from_value(serde_json::json!({
            "schema": 5,
            "schedules": [],
            "offline_reset": null,
            "offline_native": {
                (crate::external_supervisor_native::EXTERNAL_SUPERVISOR_JOURNAL_CLASS): {
                    "max_cleanup_millis": 120000,
                    "max_cleanup_steps": 3000000,
                    "max_native_allocator_steps": 2000000000u64,
                    "max_native_connections": 16,
                    "max_native_storage_bytes": 67108864,
                    "max_operation_millis": 4860000,
                },
            },
            "remote_offline_operations": [],
        }))?;
    // Issuing windows consults the node's execution admission policy; none
    // of its optional requirements apply to a loopback.
    let execution =
        ryeos_node_runtime::node_policy::sections::execution::NodeExecutionAdmissionPolicy {
            schema: 4,
            max_live_fanout: 1,
            max_private_materialization_copy_bytes: 1024 * 1024,
            host_env_passthrough: vec![],
            workload_client: None,
            resource_authority: None,
            producer_resource_ceiling: None,
            external_lease_budget: None,
            external_wall_clock_requirement: None,
        };
    state.node_policy = Arc::new(
        ryeos_node_runtime::node_policy::NodePolicySnapshot::from_test_records(vec![
            Arc::new(maintenance) as Arc<dyn ryeos_node_runtime::node_policy::ErasedNodePolicy>,
            Arc::new(execution),
        ]),
    );

    let inputs_directory = work.create_child(std::ffi::OsStr::new("inputs"), 0o700)?;
    let inputs = rehearsal_guest_inputs(
        &inputs_directory,
        &RehearsalInputsRequest {
            product: setup.product,
            product_manifest_record: setup.product_manifest_record,
            product_mount: &setup.requirement.runtime_product_declaration_id,
            product_destination: &setup.requirement.runtime_recipe.runtime_mount_destination,
            base_snapshot: setup.base_snapshot,
            file_products: setup.file_products,
        },
    )
    .context("assemble the loopback guest inputs")?;

    // The owner runtime names no controller; this node's key reaches the
    // guest only inside the occurrence assignment it signs.
    let owner = copy_executable(work, "owner", setup.owner_executable)?;
    let (owner_authority, owner_sha256, owner_bytes) = executable_authority(&owner)?;
    let runtime = ryeos_external_execution::guest_runtime_product::produce_guest_owner_runtime(
        work,
        std::ffi::OsStr::new("owner-runtime"),
        &owner_authority,
        owner_bytes,
        &owner_sha256,
        &setup.profile,
    )?;
    let runtime_manifest = ryeos_state::observe_external_content_tree_exact(runtime.root())?;
    {
        let authority = state.state_store.pinned_state_authority()?;
        let guard = authority.acquire_shared_guard()?;
        let cas = authority.cas_store()?;
        ensure!(
            cas.store_object(&serde_json::to_value(&runtime_manifest)?)? == runtime.manifest_hash(),
            "loopback owner runtime manifest differs from its produced hash"
        );
        cas.store_blob(&serde_json::to_vec(&setup.profile)?)?;
        let profile_entry = runtime_manifest
            .entries
            .iter()
            .find(|entry| entry.path == "guest-owner-profile.json")
            .and_then(|entry| entry.blob_hash.clone())
            .context("loopback owner runtime has no profile entry")?;
        let profile_bytes = runtime
            .root()
            .open_pinned_regular(std::ffi::OsStr::new("guest-owner-profile.json"), false)?
            .context("loopback owner runtime lost its profile")?
            .read_bounded(64 * 1024)?;
        ensure!(
            cas.store_blob(&profile_bytes)? == profile_entry,
            "loopback owner profile differs from its manifest entry"
        );
        authority.ensure_guard(&guard)?;
    }

    let supervisor = copy_executable(work, "supervisor", setup.supervisor_executable)?;
    let launcher = copy_executable(work, "launcher", setup.launcher_executable)?;
    let (supervisor, supervisor_hash, supervisor_bytes) = executable_authority(&supervisor)?;
    let (launcher_authority, launcher_hash, launcher_bytes) = executable_authority(&launcher)?;
    let backend = Arc::new(GuestRelayLoopbackBackend {
        backend_id: setup.backend_id.to_owned(),
        artifact_hash: lillux::sha256_hex(b"ryeos.guest-relay-loopback.adapter"),
        settings_schema_digest: lillux::sha256_hex(b"ryeos.guest-relay-loopback.settings"),
        supervisor,
        supervisor_hash: supervisor_hash.clone(),
        supervisor_bytes,
        launcher: launcher_authority,
        launcher_hash: launcher_hash.clone(),
        launcher_bytes,
        runtime: runtime.root().try_clone()?,
        runtime_manifest_hash: runtime.manifest_hash().to_owned(),
        owner_deadline_ms: u64::from(setup.profile.owner_timeout_seconds) * 1_000,
        work_parent: setup.work_parent.to_owned(),
        host_executable: setup.host_executable.to_owned(),
        fault: setup.fault,
        created: std::time::Instant::now(),
        guest: Mutex::new(Guest::default()),
        contacts: Mutex::new(Vec::new()),
    });
    state.external_placement_backends =
        Arc::new(ExternalPlacementBackendRegistry::from_backends(vec![
            Arc::clone(&backend) as Arc<dyn ExternalPlacementBackend>,
        ])?);
    // Connector and provider configuration are only coordinates during
    // startup; any captured executables stand in for them.
    state.external_candidate_connectors =
        Arc::new(ExternalCandidateConnectorRegistry::from_paths([
            launcher.clone()
        ])?);
    let declaration = ryeos_external_execution_contract::ExternalProviderDeclaration {
        id: setup.requirement.provider_declaration_id.clone(),
        protocol: ryeos_external_execution_contract::PROVIDER_DECLARATION_PROTOCOL.to_owned(),
        targets: vec![lillux::platform::current_binary_target()?.to_owned()],
        connector: "provider-connector".to_owned(),
        connector_descendants:
            ryeos_external_execution_contract::ExternalProviderConnectorDescendants::InProcessGroup,
        configuration_adapter: "provider-configuration".to_owned(),
        configuration_destination: setup.requirement.provider_configuration_destination.clone(),
        qualification_request_binding_protocol: None,
    };
    declaration.validate()?;
    let adapter = lillux::secure_fs::open_pinned_regular_file_no_follow(&launcher)?;
    let observation = adapter.observation()?;
    state.external_provider_configurations =
        Arc::new(ExternalProviderConfigurationRegistry::from_artifacts(
            vec![InstalledExternalProviderConfiguration {
                declaration,
                bundle_manifest_digest: "0".repeat(64),
                signer_fingerprint: "guest-relay-loopback".to_owned(),
                artifact_hash: adapter.digest_stable_exact(&observation)?,
                artifact_bytes: observation.size(),
                executable: adapter.inherited_descriptor_authority()?,
            }],
        )?);

    let mut capsule =
        ryeos_state::external_execution::admission::test_support::qualified_external_candidate_capsule(
            setup.requirement.clone(),
            &inputs.product_manifest_hash,
        )?;
    // Startup covers producing and staging the real guest package and the
    // owner's whole install; the fixture's ten seconds cover neither.
    capsule.lifecycle.ready_timeout_ms = 300_000;
    capsule.validate()?;
    let placement = test_support::bind_composed_external_session(
        state,
        capsule,
        &inputs.base_snapshot_hash,
        crate::node_config::sections::external_execution::ComposedTestTransport {
            selection: ExternalControllerTransportSelection::ControllerFileRelay(
                setup.relay.clone(),
            ),
            network_inputs: setup.network_inputs.clone(),
            relay_scope_setting: backend.relay_scope_setting().map(str::to_owned),
        },
        supervisor_hash,
        launcher_hash,
        // A relay binding's settings name the mailbox owner workspace.
        serde_json::json!({
            "owner_id": setup.relay.owner_workspace_id,
            "region": "loopback",
            "plan": "loopback",
        }),
        &test_support::PlacementQualificationSelection {
            guest_runtime_manifest_hash: runtime.manifest_hash().to_owned(),
            guest: None,
            content: None,
        },
    )?;
    Ok(GuestRelayLoopbackSession {
        placement,
        backend,
        guest_inputs: Some(inputs.authority),
        _work: guard,
    })
}
