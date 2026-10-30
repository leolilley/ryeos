//! Sole application-owned admission boundary for external candidate placement.
//!
//! An admitted signed definition may select an installed node binding, never a
//! raw endpoint, credential, account or backend request. Candidate data supplies
//! none of those authorities. This owner rejoins the exact session/thread and capsule to
//! one node-signed binding, an installed adapter artifact and a protected vault
//! generation before reserving capacity. Only the winner of the durable contact
//! claim receives a non-cloneable credential-bearing permit.

use std::collections::{BTreeMap, BTreeSet};
#[cfg(test)]
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use anyhow::{Context, Result, bail, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use ryeos_external_execution_contract::LifecycleCapability;
use ryeos_external_execution_contract::guest_import_authorization::SignedGuestImportAuthorization;
use ryeos_external_execution_contract::guest_import_authorization::SignedGuestOccurrenceAssignment;
use ryeos_state::external_execution::admission::AdmittedExternalExecutionProgram;
use subtle::ConstantTimeEq as _;

use crate::node_config::sections::external_execution::RetainedExternalExecutionBinding;
use crate::node_config::sections::external_execution::{
    ExternalPlacementBackendContract, InstalledExternalExecutionBinding,
};
use crate::runtime_db::external_execution::{
    EXTERNAL_ALLOCATION_RESERVATION_SCHEMA, ExternalAllocationContactClaim,
    ExternalAllocationOccurrence, ExternalAllocationOwner, ExternalAllocationPhase,
    ExternalAllocationRecord, ExternalAllocationReservation, ExternalDedicatedSessionOwner,
    ExternalGuestPackageDeliveryCommitment, ExternalNoOccurrenceEvidence,
    ExternalObservationTiming, ExternalRelayBootstrapCommitment,
    ExternalSupervisorActivationIntent, ExternalSupervisorActivationObservation,
    ExternalSupervisorActivationRecord, ExternalTerminalObservation, ExternalTerminationIntent,
};
use crate::runtime_db::{WorkspaceRecord, WorkspaceState};
use crate::state::AppState;
use crate::vault::NodeVault;
use crate::vault::external_channel::{ExternalChannelAuthority, ExternalChannelAuthorityAccess};
use crate::vault::external_relay_bootstrap::ExternalRelayBootstrapAccess;
use crate::vault::placement::PlacementCredential;
use ryeos_node_runtime::state_lock::StateLockLease;

/// Trusted controller adapter metadata and offline contract verification. This
/// method must not perform provider I/O; all provider mutation is fenced by the
/// later contact permit.
pub(crate) struct ExternalLifecycleObservation<T> {
    pub value: T,
    pub deadline_exceeded: bool,
}

/// Which independent runtime witness the later contact owner must recheck.
/// Offline adapter qualification never creates that witness itself.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RuntimeAdmissionBasis {
    OrdinarySignedRelationship,
    RestoredOwnerMeasurement,
}

/// Adapter interpretation input, not a qualification publication or grant.
/// The caller must independently authenticate the exact source, attestation,
/// retained journal and policy before constructing this value.
pub(crate) struct RuntimeProbeInput {
    pub qualification_attestation_hash: String,
    pub runtime_source: ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotSource,
    pub subject_manifest_hash: String,
    pub probe_evidence: serde_json::Value,
}

impl RuntimeProbeInput {
    fn from_product(
        proof: &ryeos_state::external_content::products::composition::AdmittedProductQualification,
    ) -> Self {
        Self {
            qualification_attestation_hash: proof.attestation_hash.clone(),
            runtime_source: ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotSource::CapturedProduct {
                product_witness_hash: proof.evidence.product_witness_hash.clone(),
            },
            subject_manifest_hash: proof.evidence.result.subject_manifest_hash.clone(),
            probe_evidence: proof.evidence.result.probe_evidence.clone(),
        }
    }
}

pub(crate) trait ExternalPlacementBackend: Send + Sync + std::fmt::Debug {
    fn backend_id(&self) -> &str;
    /// Stage-one lease admission, offline and before any reservation or
    /// provider contact: the backend's lifecycle adapter judges one lease
    /// request against its signed provider spec while holding no credential,
    /// network or activation authority. Returns the exact request bytes it
    /// admitted with the budget weight its signed provider spec declares; a
    /// refusal is an error carrying the adapter's plain reason.
    fn admit_lease(
        &self,
        _inputs: &crate::external_lifecycle_adapter::lease_admission::LeaseAdmissionInputs,
        _deadline: lillux::time::MonotonicDeadline,
    ) -> Result<crate::external_lifecycle_adapter::lease_admission::AdmittedLease> {
        bail!(
            "lease admission refused: external placement backend `{}` admits no leases",
            self.backend_id()
        )
    }
    /// Lease readiness judgement: the backend's lifecycle adapter judges the
    /// guest's first two frames, held un-journalled by the controller,
    /// against the lease's retained expectation. `request` is the canonical
    /// readiness request; the adapter receives it with the captured signed
    /// provider spec and the two frames, holding no credential or activation
    /// authority. Returns the adapter's raw verdict bytes; a failure to obtain
    /// one is an error, never a verdict.
    fn await_lease_ready(
        &self,
        _request: &[u8],
        _ready_frame: &[u8],
        _report_frame: &[u8],
        _deadline: lillux::time::MonotonicDeadline,
    ) -> Result<Vec<u8>> {
        bail!(
            "lease readiness refused: external placement backend `{}` judges no lease readiness",
            self.backend_id()
        )
    }
    /// Transport only. The private claimed contact follows a durable journal
    /// transition and the original sealed bootstrap is joined again below.
    fn relay_file_contact(
        &self,
        _contract: &ExternalPlacementBackendContract,
        _credential: &PlacementCredential,
        _bootstrap: &ryeos_state::external_execution::transport::ExternalSupervisorBootstrap,
        _claim: crate::runtime_db::external_execution::ClaimedRelayFileContact,
        _deadline: lillux::time::MonotonicDeadline,
    ) -> Result<
        ExternalLifecycleObservation<
            ryeos_external_execution_contract::relay_file::RelayFileAdapterResponse,
        >,
    > {
        bail!("external placement backend has no claimed relay file effect")
    }
    /// Already-captured signed source for cleanup-only retention. This does
    /// not grant a new allocation or provider operation.
    fn lifecycle_escrow_capture(
        &self,
    ) -> Result<Option<crate::external_artifacts::LifecycleEscrowCapture<'_>>> {
        Ok(None)
    }
    fn bootstrap_profile_digest(&self) -> Option<&str> {
        None
    }
    fn bootstrap_provider_spec_digest(&self) -> Option<&str> {
        None
    }
    fn artifact_hash(&self) -> &str;
    fn artifact_bytes(&self) -> u64;
    fn supervisor_artifact(&self) -> (&str, u64);
    fn launcher_artifact(&self) -> (&str, u64);
    fn settings_schema_digest(&self) -> &str;
    fn preflight_runtime_snapshot(
        &self,
        _binding: &crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
        _credential: &PlacementCredential,
    ) -> Result<()> {
        bail!("external placement backend has no inspected snapshot producer")
    }
    fn preflight_bootstrap_recovery(
        &self,
        _binding: &crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
        _credential: &PlacementCredential,
        _intent: &ryeos_external_execution_contract::runtime_snapshot_bootstrap::RuntimeSnapshotBootstrapIntent,
    ) -> Result<()> {
        bail!("external placement backend has no inspected bootstrap recovery authority")
    }
    fn create_snapshot_bootstrap_source(
        &self,
        _binding: &crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
        _credential: &PlacementCredential,
        _request: &ryeos_external_execution_contract::runtime_snapshot_bootstrap::RuntimeSnapshotBootstrapAdapterRequest,
        _deadline: lillux::time::MonotonicDeadline,
    ) -> Result<ExternalLifecycleObservation<ryeos_external_execution_contract::runtime_snapshot_bootstrap::RuntimeSnapshotBootstrapAdapterResponse>>{
        bail!("external placement backend has no inspected snapshot bootstrap profile")
    }
    fn observe_snapshot_bootstrap_source(
        &self,
        _binding: &crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
        _credential: &PlacementCredential,
        _request: &ryeos_external_execution_contract::runtime_snapshot_bootstrap::RuntimeSnapshotBootstrapReadinessRequest,
        _deadline: lillux::time::MonotonicDeadline,
    ) -> Result<ExternalLifecycleObservation<ryeos_external_execution_contract::runtime_snapshot_bootstrap::RuntimeSnapshotBootstrapReadinessAdapterResponse>>{
        bail!("external placement backend has no inspected bootstrap observer")
    }
    fn terminate_snapshot_bootstrap_source(
        &self,
        _binding: &crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
        _credential: &PlacementCredential,
        _request: &ryeos_external_execution_contract::runtime_snapshot_bootstrap::RuntimeSnapshotBootstrapTerminationAdapterRequest,
        _first_contact: bool,
        _deadline: lillux::time::MonotonicDeadline,
    ) -> Result<ExternalLifecycleObservation<ryeos_external_execution_contract::runtime_snapshot_bootstrap::RuntimeSnapshotBootstrapTerminationAdapterResponse>>{
        bail!("external placement backend has no inspected bootstrap terminator")
    }
    /// One already-claimed snapshot attempt. The caller must establish the
    /// current signed producer binding, product witness and durable CAS before
    /// invoking this contact path; backend inspection alone grants no contact.
    fn produce_runtime_snapshot(
        &self,
        _binding: &crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
        _credential: &PlacementCredential,
        _request: &ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotAdapterRequest,
        _upload: &lillux::InheritedDescriptorAuthority,
        _deadline: lillux::time::MonotonicDeadline,
    ) -> Result<
        ExternalLifecycleObservation<
            ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotAdapterResponse,
        >,
    > {
        bail!("external placement backend does not produce runtime snapshots")
    }
    fn upload_runtime_snapshot(
        &self,
        _binding: &crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
        _credential: &PlacementCredential,
        _request: &ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotUploadAdapterRequest,
        _upload: &lillux::InheritedDescriptorAuthority,
        _deadline: lillux::time::MonotonicDeadline,
    ) -> Result<
        ExternalLifecycleObservation<
            ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotUploadReceipt,
        >,
    > {
        bail!("external placement backend does not support staged snapshot upload")
    }
    fn create_runtime_snapshot(
        &self,
        _binding: &crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
        _credential: &PlacementCredential,
        _request: &ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotCreateAdapterRequest,
        _deadline: lillux::time::MonotonicDeadline,
    ) -> Result<
        ExternalLifecycleObservation<
            ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotCreateResult,
        >,
    > {
        bail!("external placement backend does not support staged snapshot creation")
    }
    fn observe_runtime_snapshot_readiness(
        &self,
        _binding: &crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
        _credential: &PlacementCredential,
        _request: &ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotReadinessRequest,
        _deadline: lillux::time::MonotonicDeadline,
    ) -> Result<
        ExternalLifecycleObservation<
            ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotReadinessObservation,
        >,
    >{
        bail!("external placement backend cannot observe runtime snapshot readiness")
    }
    fn preflight_snapshot_qualification_create(
        &self,
        _producer: &crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
        _qualification: &crate::node_config::sections::runtime_snapshot_qualification::InstalledRuntimeSnapshotQualificationBinding,
        _credential: &PlacementCredential,
    ) -> Result<()> {
        bail!("external placement backend has no inspected qualification create profile")
    }
    fn create_snapshot_qualification_occurrence(
        &self,
        _producer: &crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
        _qualification: &crate::node_config::sections::runtime_snapshot_qualification::InstalledRuntimeSnapshotQualificationBinding,
        _credential: &PlacementCredential,
        _request: &ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotQualificationAdapterRequest,
        _deadline: lillux::time::MonotonicDeadline,
    ) -> Result<ExternalLifecycleObservation<ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotQualificationAdapterResponse>>{
        bail!("external placement backend cannot create a qualification occurrence")
    }
    /// The caller owns the one-shot journal claim. `first_contact=false` is
    /// strictly observation-only and may not repeat a termination mutation.
    fn terminate_snapshot_qualification_occurrence(
        &self,
        _producer: &crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
        _qualification: &crate::node_config::sections::runtime_snapshot_qualification::InstalledRuntimeSnapshotQualificationBinding,
        _credential: &PlacementCredential,
        _request: &ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotQualificationTerminationAdapterRequest,
        _first_contact: bool,
        _deadline: lillux::time::MonotonicDeadline,
    ) -> Result<ExternalLifecycleObservation<ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotQualificationTerminationAdapterResponse>>{
        bail!("external placement backend cannot terminate a qualification occurrence")
    }
    fn seal_restoration_verifier_upload(
        &self,
        _producer: &crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
        _qualification: &crate::node_config::sections::runtime_snapshot_qualification::InstalledRuntimeSnapshotQualificationBinding,
    ) -> Result<
        ryeos_external_execution::restoration_verifier_delivery::SealedQualificationVerifierUpload,
    > {
        bail!("external placement backend has no admitted restoration verifier")
    }
    fn verify_restored_snapshot_once(
        &self,
        _producer: &crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
        _qualification: &crate::node_config::sections::runtime_snapshot_qualification::InstalledRuntimeSnapshotQualificationBinding,
        _credential: &PlacementCredential,
        _request: &ryeos_external_execution_contract::restored_runtime_measurement::RestoredVerifierAdapterRequest,
        _upload: &lillux::InheritedDescriptorAuthority,
        _deadline: lillux::time::MonotonicDeadline,
    ) -> Result<ExternalLifecycleObservation<ryeos_external_execution_contract::restored_runtime_measurement::RestoredVerifierAdapterResponse>>{
        bail!("external placement backend cannot verify restored snapshot")
    }
    /// Effective capabilities from the exact installed adapter inspection.
    /// A declaration alone is not permission to invent stronger observations.
    fn lifecycle_capabilities(&self) -> BTreeSet<LifecycleCapability>;
    fn qualify_offline(
        &self,
        contract: &ExternalPlacementBackendContract,
        credential: &PlacementCredential,
    ) -> Result<()>;

    /// Interpret an already-authenticated, CAS-owned runtime probe
    /// against this exact signed placement contract. This operation must be
    /// credential-free and provider-contact-free. It cannot authenticate the
    /// qualification attestation, grant lifecycle capability, or permit allocation.
    fn verify_runtime_probe(
        &self,
        _contract: &ExternalPlacementBackendContract,
        _proof: &RuntimeProbeInput,
        _source: &ryeos_external_execution::guest_runtime_product::GuestOwnerRuntimeManifestIdentity,
        _binding_hash: &str,
    ) -> Result<()> {
        bail!("external lifecycle adapter does not verify runtime qualification probes")
    }

    /// Whether this backend's signed provider spec declares that activation
    /// uploads the guest runtime itself, so the controller must produce that
    /// runtime's sealed tar alongside the guest package. Fixed by the captured
    /// spec; never consulted for any other operation.
    fn stages_guest_runtime_upload(&self) -> bool {
        false
    }

    /// Prepare and locally re-import the exact secret-bearing guest package
    /// before the durable activation/contact claim. No provider contact is
    /// permitted here. The returned commitment is inserted with that claim.
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
    ) -> Result<SupervisorGuestPackage>;

    /// The only allocator mutation. It is reachable solely by consuming the
    /// process-local contact permit after the durable contact CAS.
    fn allocate(
        &self,
        _contract: &ExternalPlacementBackendContract,
        _credential: &PlacementCredential,
        _reservation: &ExternalAllocationReservation,
        _deadline: lillux::time::MonotonicDeadline,
    ) -> Result<ExternalLifecycleObservation<ExternalAllocationResolution>> {
        bail!("external placement backend does not implement allocation")
    }

    /// Reconcile the exact original request. This must never create a second
    /// occurrence. A negative answer must be the backend's authoritative
    /// no-occurrence proof, not absence from a list or a 404.
    fn reconcile_allocation(
        &self,
        _contract: &ExternalPlacementBackendContract,
        _credential: &PlacementCredential,
        _reservation: &ExternalAllocationReservation,
        _deadline: lillux::time::MonotonicDeadline,
    ) -> Result<ExternalLifecycleObservation<ExternalAllocationResolution>> {
        bail!("external placement backend does not implement allocation reconciliation")
    }

    /// The one supervisor-start mutation for an already bound occurrence.
    /// Allocation and activation are deliberately distinct durable effects.
    fn activate_supervisor(
        &self,
        _contract: &ExternalPlacementBackendContract,
        _credential: &PlacementCredential,
        _reservation: &ExternalAllocationReservation,
        _occurrence: &ExternalAllocationOccurrence,
        _intent: &ExternalSupervisorActivationIntent,
        _activation: &ExternalSupervisorActivation,
        _deadline: lillux::time::MonotonicDeadline,
    ) -> Result<ExternalLifecycleObservation<ExternalSupervisorActivationResolution>> {
        bail!("external placement backend does not implement supervisor activation")
    }

    /// Reconcile the exact retained supervisor-start request. It may observe
    /// the original mutation but may never create a replacement supervisor.
    fn reconcile_supervisor_activation(
        &self,
        _contract: &ExternalPlacementBackendContract,
        _credential: &PlacementCredential,
        _reservation: &ExternalAllocationReservation,
        _occurrence: &ExternalAllocationOccurrence,
        _intent: &ExternalSupervisorActivationIntent,
        _deadline: lillux::time::MonotonicDeadline,
    ) -> Result<ExternalLifecycleObservation<ExternalSupervisorActivationResolution>> {
        bail!("external placement backend does not implement supervisor activation reconciliation")
    }

    /// Issue the exact controller-authored termination request once.
    fn terminate(
        &self,
        _contract: &ExternalPlacementBackendContract,
        _credential: &PlacementCredential,
        _reservation: &ExternalAllocationReservation,
        _occurrence: &ExternalAllocationOccurrence,
        _intent: &ExternalTerminationIntent,
    ) -> Result<ExternalTerminationResolution> {
        bail!("external placement backend does not implement termination")
    }

    /// Observe/reconcile cleanup after the one termination mutation. This may
    /// not treat request acknowledgement, timeout or absence as terminal proof.
    fn reconcile_termination(
        &self,
        _contract: &ExternalPlacementBackendContract,
        _credential: &PlacementCredential,
        _reservation: &ExternalAllocationReservation,
        _occurrence: &ExternalAllocationOccurrence,
        _intent: &ExternalTerminationIntent,
    ) -> Result<ExternalTerminationResolution> {
        bail!("external placement backend does not implement termination reconciliation")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExternalAllocationResolution {
    Bound {
        occurrence_id: String,
        provider_observation_digest: String,
    },
    NoOccurrence {
        provider_observation_digest: String,
    },
    Pending,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExternalTerminationResolution {
    Terminal { provider_observation_digest: String },
    Pending,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExternalSupervisorActivationResolution {
    Started { provider_observation_digest: String },
    NotStarted { provider_observation_digest: String },
    Pending,
}

/// Secret-bearing, one-contact bootstrap material passed only to the selected
/// protected lifecycle adapter.  It contains no node/operator signing key and
/// has no serialization or cloning surface.
pub(crate) struct ExternalSupervisorActivation {
    bootstrap: ryeos_state::external_execution::transport::ExternalSupervisorBootstrap,
    guest_inputs: ryeos_external_execution::guest_inputs::ExternalGuestInputAuthority,
    guest_package: Option<SupervisorGuestPackage>,
    /// Sealed tar of the qualified guest runtime, present exactly when the
    /// selected provider's activation uploads the runtime itself.
    guest_runtime:
        Option<ryeos_external_execution::guest_runtime_product::GuestOwnerSnapshotUpload>,
    import_authorization: Option<SignedGuestImportAuthorization>,
    signed_assignment: Option<SignedGuestOccurrenceAssignment>,
}

pub(crate) enum SupervisorGuestPackage {
    Prepared {
        package: ryeos_external_execution::guest_package_producer::PreparedGuestPackage,
        commitment: ExternalGuestPackageDeliveryCommitment,
    },
    /// Pure in-process fault fixtures exercise the journal without a provider.
    /// Installed adapters always use the prepared package variant.
    #[cfg(test)]
    Fixture(ExternalGuestPackageDeliveryCommitment),
}

impl SupervisorGuestPackage {
    fn commitment(&self) -> &ExternalGuestPackageDeliveryCommitment {
        match self {
            Self::Prepared { commitment, .. } => commitment,
            #[cfg(test)]
            Self::Fixture(commitment) => commitment,
        }
    }

    fn discard(self) -> Result<()> {
        match self {
            Self::Prepared { package, .. } => package.discard(),
            #[cfg(test)]
            Self::Fixture(_) => Ok(()),
        }
    }
}

/// Non-secret result of occurrence/bootstrap authentication.  The raw
/// capability is deliberately not retained in route principals or logs.
#[derive(Debug, PartialEq, Eq)]
pub struct AuthenticatedExternalOccurrence {
    placement_thread_id: String,
    occurrence_id: String,
    allocation_request_digest: String,
}

/// Signature-authenticated supervisor exchange coordinate. It carries no
/// signing key, capability or general node principal.
pub struct AuthenticatedExternalChannelFrame {
    placement_thread_id: String,
    occurrence_id: String,
    sequence: u64,
    frame_digest: String,
}

pub struct ExternalChannelOutboundFrame {
    sequence: u64,
    frame_digest: String,
    canonical_wire: Vec<u8>,
}

/// Controller-applied Q native evidence. This carries no process, provider or
/// product-qualification permission; a final claim must rejoin every owner and
/// terminal record under its own writer transaction.
pub struct ExternalQualificationNativeSettlement {
    pub placement_thread_id: String,
    pub binding_digest: String,
    pub completion_frame_digest: String,
    pub runtime_applied_frame_digest: String,
    pub protocol_eof_frame_digest: String,
    pub settlement_frame_digest: String,
    pub observation: ryeos_state::external_execution::NativeQualificationScopeSettlementObservation,
}

pub struct ExternalChannelExchangeResult {
    incoming_new: bool,
    acknowledgement_digest: Option<String>,
    outbound: Vec<ExternalChannelOutboundFrame>,
    urgent_revocation: Option<ExternalChannelOutboundFrame>,
}

/// Exact installed controller-side connector generation. The open descriptor
/// pins the bytes used for admission; the pathname is only the launch spelling
/// supplied to the provider and must still select this inode when used. A
/// connected process is independently checked against the exact hash/size.
pub struct InstalledExternalCandidateConnector {
    executable: lillux::PinnedRegularFile,
    captured_executable: lillux::InheritedDescriptorAuthority,
    artifact_hash: String,
    artifact_bytes: u64,
}

impl std::fmt::Debug for InstalledExternalCandidateConnector {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InstalledExternalCandidateConnector")
            .field("artifact_hash", &self.artifact_hash)
            .field("artifact_bytes", &self.artifact_bytes)
            .finish_non_exhaustive()
    }
}

impl InstalledExternalCandidateConnector {
    fn from_captured(captured: ryeos_engine::binary_resolver::CapturedExecutable) -> Result<Self> {
        let executable =
            lillux::secure_fs::open_pinned_regular_file_no_follow(&captured.identity.absolute_path)
                .context("pin signed external candidate connector launch spelling")?;
        executable.require_executable()?;
        let observation = executable.observation()?;
        let artifact_bytes = observation.size();
        ensure!(
            (1..=1024 * 1024 * 1024).contains(&artifact_bytes)
                && executable.digest_stable_exact(&observation)? == captured.identity.content_hash,
            "signed external candidate connector changed during capture"
        );
        Ok(Self {
            executable,
            captured_executable: captured.handle,
            artifact_hash: captured.identity.content_hash,
            artifact_bytes,
        })
    }

    #[cfg(test)]
    fn open(path: &Path) -> Result<Self> {
        let executable = lillux::secure_fs::open_pinned_regular_file_no_follow(path)
            .context("open installed external candidate connector")?;
        executable.require_executable()?;
        let observation = executable.observation()?;
        let artifact_bytes = observation.size();
        ensure!(
            (1..=1024 * 1024 * 1024).contains(&artifact_bytes),
            "installed external candidate connector exceeds its byte bound"
        );
        let artifact_hash = executable.digest_stable_exact(&observation)?;
        let captured_executable = executable.inherited_descriptor_authority()?;
        Ok(Self {
            executable,
            captured_executable,
            artifact_hash,
            artifact_bytes,
        })
    }

    fn ensure_path_binding(&self) -> Result<()> {
        let current = lillux::secure_fs::open_pinned_regular_file_no_follow(self.executable.path())
            .context("reopen installed external candidate connector")?;
        ensure!(
            lillux::secure_fs::same_open_file_identity(
                &self.executable.try_clone_descriptor()?,
                &current.try_clone_descriptor()?,
            )?,
            "installed external candidate connector pathname changed"
        );
        let observation = current.observation()?;
        ensure!(
            observation.size() == self.artifact_bytes
                && current.digest_stable_exact(&observation)? == self.artifact_hash,
            "installed external candidate connector bytes changed"
        );
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn executable_path(&self) -> Result<PathBuf> {
        self.ensure_path_binding()?;
        Ok(self.executable.path().to_path_buf())
    }

    pub(crate) fn captured_executable(&self) -> lillux::InheritedDescriptorAuthority {
        self.captured_executable.clone()
    }

    pub(crate) fn verify_peer(
        &self,
        peer: &lillux::local_ipc::AuthenticatedUnixPeer,
    ) -> Result<()> {
        // Bundle capture executes sealed anonymous bytes, whose kernel name
        // differs from the installed filename. Authenticate the pidfd-pinned
        // peer's exact image, not that diagnostic launch spelling.
        ensure!(
            peer.executable_digest_exact(self.artifact_bytes)? == self.artifact_hash,
            "external candidate connector peer has the wrong executable bytes"
        );
        Ok(())
    }
}

#[derive(Debug, Default)]
pub struct ExternalCandidateConnectorRegistry {
    artifacts: BTreeMap<(String, u64), Arc<InstalledExternalCandidateConnector>>,
}

impl ExternalCandidateConnectorRegistry {
    pub(crate) fn from_captured(
        captured: Vec<ryeos_engine::binary_resolver::CapturedExecutable>,
    ) -> Result<Self> {
        let mut artifacts = BTreeMap::new();
        for executable in captured {
            let artifact = Arc::new(InstalledExternalCandidateConnector::from_captured(
                executable,
            )?);
            let coordinate = (artifact.artifact_hash.clone(), artifact.artifact_bytes);
            ensure!(
                artifacts.insert(coordinate, artifact).is_none(),
                "signed external candidate connector generation is duplicated"
            );
        }
        Ok(Self { artifacts })
    }

    /// Test-only constructor. Production composition must supply connector
    /// artifacts captured from signed bundle authority; a sibling of the
    /// daemon executable is not an admitted artifact source.
    #[cfg(test)]
    fn from_paths(paths: impl IntoIterator<Item = PathBuf>) -> Result<Self> {
        let mut artifacts = BTreeMap::new();
        for path in paths {
            let artifact = Arc::new(InstalledExternalCandidateConnector::open(&path)?);
            let coordinate = (artifact.artifact_hash.clone(), artifact.artifact_bytes);
            ensure!(
                artifacts.insert(coordinate, artifact).is_none(),
                "installed external candidate connector generation is duplicated"
            );
        }
        Ok(Self { artifacts })
    }

    /// Exact artifact coordinate of the installed connector with this content
    /// identity, for composing a signed placement binding.
    pub(crate) fn coordinate_for_hash(&self, hash: &str) -> Result<(String, u64)> {
        let mut matching = self
            .artifacts
            .keys()
            .filter(|(candidate, _)| candidate == hash);
        let coordinate = matching
            .next()
            .context("installed external candidate connector is absent")?;
        ensure!(
            matching.next().is_none(),
            "installed external candidate connector identity is ambiguous"
        );
        Ok(coordinate.clone())
    }

    pub(crate) fn qualify(
        &self,
        contract: &ExternalPlacementBackendContract,
    ) -> Result<Arc<InstalledExternalCandidateConnector>> {
        let selected = contract.workload.session_artifacts()?;
        ensure!(
            selected.connector_protocol
                == ryeos_state::external_execution::admission::CONNECTOR_PROTOCOL,
            "signed external connector protocol is unsupported"
        );
        let artifact = self
            .artifacts
            .get(&(
                selected.connector_artifact_hash.to_owned(),
                selected.connector_artifact_bytes,
            ))
            .cloned()
            .context("exact signed external candidate connector is not installed")?;
        artifact.ensure_path_binding()?;
        Ok(artifact)
    }

    #[cfg(test)]
    fn from_test_path(path: &Path) -> Result<Self> {
        Self::from_paths([path.to_path_buf()])
    }
}

/// Exact signed provider-configuration adapter generation. The declaration
/// owns provider syntax and destination; the placement binding must still name
/// the observed executable identity before it is usable.
pub(crate) struct InstalledExternalProviderConfiguration {
    declaration: ryeos_external_execution_contract::ExternalProviderDeclaration,
    bundle_manifest_digest: String,
    signer_fingerprint: String,
    artifact_hash: String,
    artifact_bytes: u64,
    executable: lillux::InheritedDescriptorAuthority,
}

impl std::fmt::Debug for InstalledExternalProviderConfiguration {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InstalledExternalProviderConfiguration")
            .field("declaration", &self.declaration.id)
            .field("bundle_manifest_digest", &self.bundle_manifest_digest)
            .field("signer_fingerprint", &self.signer_fingerprint)
            .field("artifact_hash", &self.artifact_hash)
            .field("artifact_bytes", &self.artifact_bytes)
            .finish_non_exhaustive()
    }
}

impl InstalledExternalProviderConfiguration {
    pub(crate) fn new(
        declaration: ryeos_external_execution_contract::ExternalProviderDeclaration,
        bundle_manifest_digest: String,
        signer_fingerprint: String,
        captured: ryeos_engine::binary_resolver::CapturedExecutable,
    ) -> Result<Self> {
        declaration.validate()?;
        let observation = captured.handle.regular_file_observation()?;
        let artifact_bytes = observation.size();
        ensure!(
            (1..=1024 * 1024 * 1024).contains(&artifact_bytes)
                && captured
                    .handle
                    .digest_regular_file_stable_exact(&observation)?
                    == captured.identity.content_hash,
            "signed provider configuration adapter changed during capture"
        );
        Ok(Self {
            declaration,
            bundle_manifest_digest,
            signer_fingerprint,
            artifact_hash: captured.identity.content_hash,
            artifact_bytes,
            executable: captured.handle,
        })
    }

    pub(crate) fn declaration(
        &self,
    ) -> &ryeos_external_execution_contract::ExternalProviderDeclaration {
        &self.declaration
    }

    pub(crate) fn executable(&self) -> &lillux::InheritedDescriptorAuthority {
        &self.executable
    }
}

#[derive(Debug, Default)]
pub struct ExternalProviderConfigurationRegistry {
    artifacts: BTreeMap<String, Arc<InstalledExternalProviderConfiguration>>,
}

impl ExternalProviderConfigurationRegistry {
    pub(crate) fn from_artifacts(
        artifacts: Vec<InstalledExternalProviderConfiguration>,
    ) -> Result<Self> {
        let mut by_id = BTreeMap::new();
        for artifact in artifacts {
            let id = artifact.declaration.id.clone();
            ensure!(
                by_id.insert(id.clone(), Arc::new(artifact)).is_none(),
                "external provider configuration `{id}` is duplicated"
            );
        }
        Ok(Self { artifacts: by_id })
    }

    /// Exact artifact coordinate of the installed configuration adapter for
    /// one provider declaration, for composing a signed placement binding.
    pub(crate) fn coordinate_for_provider(
        &self,
        provider_declaration_id: &str,
    ) -> Result<(String, u64)> {
        let artifact = self
            .artifacts
            .get(provider_declaration_id)
            .context("external provider declaration is not installed")?;
        Ok((artifact.artifact_hash.clone(), artifact.artifact_bytes))
    }

    /// The installed signed declaration of one provider, for composing a
    /// binding that must agree with it.
    pub(crate) fn declaration_for_provider(
        &self,
        provider_declaration_id: &str,
    ) -> Result<&ryeos_external_execution_contract::ExternalProviderDeclaration> {
        Ok(self
            .artifacts
            .get(provider_declaration_id)
            .context("external provider declaration is not installed")?
            .declaration())
    }

    pub(crate) fn qualify(
        &self,
        contract: &ExternalPlacementBackendContract,
    ) -> Result<Arc<InstalledExternalProviderConfiguration>> {
        let selected = contract.workload.session_artifacts()?;
        let artifact = self
            .artifacts
            .get(selected.provider_declaration_id)
            .cloned()
            .context("exact signed external provider declaration is not installed")?;
        ensure!(
            artifact.artifact_hash == selected.configuration_adapter_artifact_hash
                && artifact.artifact_bytes == selected.configuration_adapter_artifact_bytes
                && artifact.declaration.configuration_destination
                    == selected.provider_configuration_destination,
            "external provider configuration adapter contradicts its placement binding"
        );
        Ok(artifact)
    }
}

/// One bounded advance of the controller-owned external start state machine.
///
/// This is deliberately not a generic provider result.  Each call may consume
/// at most one durable lifecycle decision and at most one corresponding
/// adapter mutation/observation.  Callers may poll the retained state, but
/// cannot turn a pending or ambiguous result into another allocation or
/// supervisor start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalCandidateStartProgress {
    /// The unique allocator request may have been accepted; only exact
    /// reconciliation may advance it.
    AllocationPending,
    /// One exact occurrence is retained.  A later call owns the separately
    /// journaled supervisor activation decision.
    OccurrenceBound,
    /// The activation request is retained but the adapter has not yet supplied
    /// authoritative started/not-started evidence.
    SupervisorPending,
    /// The adapter proved the supervisor start, but the occurrence has not yet
    /// authenticated and attached its exact execution channel.
    AttachmentPending,
    /// The exact occurrence-authenticated channel exists, but the supervisor's
    /// signed readiness observation has not yet been applied.  Candidate
    /// protocol I/O remains closed and no connector may be exposed.
    ChannelAttached,
    /// The signed supervisor readiness observation was atomically applied and
    /// the controller retained the exact Release which opens candidate
    /// protocol I/O.  Only this state may expose the protected connector.
    Ready(ryeos_state::external_execution::ExecutionChannelBinding),
    /// No external occurrence can remain: the allocation was never contacted,
    /// exact no-occurrence evidence was retained, or exact termination was
    /// independently observed.
    CleanupProved,
    /// An occurrence or possible occurrence is quarantined and must complete
    /// the independent termination/reconciliation path before capacity or the
    /// dedicated-session credential fence can be released.
    CleanupRequired,
}

/// One bounded advance of the independent external cleanup obligation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalCandidateCleanupProgress {
    /// The original allocation/termination operation remains unresolved. Its
    /// capacity and credential obligations stay retained for reconciliation.
    Pending,
    /// Exact no-contact, no-occurrence, or terminal evidence released the
    /// external occurrence obligation.
    Proved,
}

/// Completion-side selection for an admitted external candidate. The local
/// controller workspace is deliberately absent from this result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalCandidateCompletionProgress {
    AwaitingImport,
    Imported(ryeos_state::objects::WorkspaceGenerationPair),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalCandidateStartCleanup {
    Proved,
    Unproved,
}

/// Typed start failure.  The cleanup classification is derived from durable
/// placement state inside this module; daemon callers must never infer it from
/// an `anyhow` type or message.
#[derive(Debug)]
pub struct ExternalCandidateStartFailure {
    source: anyhow::Error,
    cleanup: ExternalCandidateStartCleanup,
}

/// Exact result of polling remote exec-server stdout for the protected local
/// connector. `Uncertain` is terminal for connector recovery: the bytes may
/// already have crossed the prior local transport and must never be replayed.
pub enum ExternalProtocolOutput {
    Idle,
    Claimed(ExternalProtocolOutputPermit),
    Uncertain { sequence: u64, frame_digest: String },
}

/// Non-cloneable application authority for one exact remote stdout frame.
/// Dropping it leaves the durable claim uncertain. Only a successful complete
/// write to the connector's local byte stream may call `finish`.
pub struct ExternalProtocolOutputPermit {
    state_store: Arc<crate::state_store::StateStore>,
    // A claimed frame may remain in a local socket write after the async
    // controller starts shutting down. Keep the exact controller generation's
    // OS-backed exclusion live until that write is either finished or dropped.
    _controller_lifetime: Arc<StateLockLease>,
    placement: String,
    sequence: u64,
    frame_digest: String,
    bytes: Vec<u8>,
    eof: bool,
}

impl ExternalProtocolOutputPermit {
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    pub fn frame_digest(&self) -> &str {
        &self.frame_digest
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn is_eof(&self) -> bool {
        self.eof
    }

    pub fn finish(self) -> Result<()> {
        self.state_store.finish_external_protocol_output(
            &self.placement,
            self.sequence,
            &self.frame_digest,
        )
    }
}

impl ExternalCandidateStartFailure {
    pub fn cleanup(&self) -> ExternalCandidateStartCleanup {
        self.cleanup
    }
}

impl std::fmt::Display for ExternalCandidateStartFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{:#}", self.source)
    }
}

impl std::error::Error for ExternalCandidateStartFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.source()
    }
}

impl AuthenticatedExternalChannelFrame {
    pub fn placement_thread_id(&self) -> &str {
        &self.placement_thread_id
    }

    pub fn occurrence_id(&self) -> &str {
        &self.occurrence_id
    }

    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    pub fn frame_digest(&self) -> &str {
        &self.frame_digest
    }
}

impl ExternalChannelOutboundFrame {
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    pub fn frame_digest(&self) -> &str {
        &self.frame_digest
    }

    pub fn canonical_wire(&self) -> &[u8] {
        &self.canonical_wire
    }
}

impl ExternalChannelExchangeResult {
    pub fn incoming_new(&self) -> bool {
        self.incoming_new
    }

    pub fn acknowledgement_digest(&self) -> Option<&str> {
        self.acknowledgement_digest.as_deref()
    }

    pub fn outbound(&self) -> &[ExternalChannelOutboundFrame] {
        &self.outbound
    }

    pub fn urgent_revocation(&self) -> Option<&ExternalChannelOutboundFrame> {
        self.urgent_revocation.as_ref()
    }
}

impl AuthenticatedExternalOccurrence {
    pub fn placement_thread_id(&self) -> &str {
        &self.placement_thread_id
    }
    pub fn occurrence_id(&self) -> &str {
        &self.occurrence_id
    }
    pub fn allocation_request_digest(&self) -> &str {
        &self.allocation_request_digest
    }
}

impl ExternalSupervisorActivation {
    pub(crate) fn bootstrap(
        &self,
    ) -> &ryeos_state::external_execution::transport::ExternalSupervisorBootstrap {
        &self.bootstrap
    }

    pub(crate) fn guest_inputs(
        &self,
    ) -> &ryeos_external_execution::guest_inputs::ExternalGuestInputAuthority {
        &self.guest_inputs
    }

    pub(crate) fn prepared_guest_package(
        &self,
    ) -> Result<&ryeos_external_execution::guest_package_producer::PreparedGuestPackage> {
        match self.guest_package.as_ref() {
            Some(SupervisorGuestPackage::Prepared { package, .. }) => Ok(package),
            #[cfg(test)]
            Some(SupervisorGuestPackage::Fixture(_)) => {
                bail!("in-process fixture has no provider guest package")
            }
            None => bail!("external guest package already discarded"),
        }
    }

    pub(crate) fn guest_runtime_upload(
        &self,
    ) -> Option<&ryeos_external_execution::guest_runtime_product::GuestOwnerSnapshotUpload> {
        self.guest_runtime.as_ref()
    }

    pub(crate) fn signed_import_authorization(&self) -> Result<&SignedGuestImportAuthorization> {
        self.import_authorization
            .as_ref()
            .context("first activation has no signed guest import authorization")
    }

    pub(crate) fn signed_assignment(&self) -> Result<&SignedGuestOccurrenceAssignment> {
        self.signed_assignment
            .as_ref()
            .context("first activation has no node-signed guest occurrence assignment")
    }

    fn retain_signed_assignment(&mut self, signed: SignedGuestOccurrenceAssignment) -> Result<()> {
        signed.validate_shape()?;
        let import = &self.signed_import_authorization()?.authorization;
        let assignment = &signed.assignment;
        ensure!(
            self.signed_assignment.is_none()
                && assignment.placement_thread_id == import.placement_thread_id
                && assignment.admitted_capsule_hash == import.admitted_capsule_hash
                && assignment.base_snapshot_hash == import.base_snapshot_hash
                && assignment.execution_binding_hash == import.execution_binding_hash
                && assignment.allocation_request_digest == import.allocation_request_digest
                && assignment.occurrence_id == import.occurrence_id
                && assignment.activation_request_digest == import.activation_request_digest
                && assignment.supervisor_runtime_hash == import.supervisor_runtime_hash
                && assignment.guest_runtime_manifest_hash == import.guest_runtime_manifest_hash
                && assignment.attachment_deadline_ms == import.attachment_deadline_ms
                && assignment.owner_public_key_hex
                    == hex::encode(STANDARD.decode(&self.bootstrap.owner_public_key)?),
            "node-signed guest assignment changed its import or channel-owner authority"
        );
        self.signed_assignment = Some(signed);
        Ok(())
    }

    fn discard_guest_package(&mut self) -> Result<()> {
        // The runtime tar is a sealed in-memory descriptor; dropping it is
        // its whole cleanup.
        self.guest_runtime = None;
        self.guest_package
            .take()
            .context("external activation lost its prepared guest package")?
            .discard()
    }
}

impl Drop for ExternalSupervisorActivation {
    fn drop(&mut self) {
        use zeroize::Zeroize as _;
        self.bootstrap.bootstrap_capability.zeroize();
    }
}

/// Installed artifact coordinates of one lifecycle adapter generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InstalledLifecycleGeneration {
    pub(crate) backend_artifact_hash: String,
    pub(crate) backend_artifact_bytes: u64,
    pub(crate) settings_schema_digest: String,
    pub(crate) supervisor_artifact_hash: String,
    pub(crate) supervisor_artifact_bytes: u64,
    pub(crate) launcher_artifact_hash: String,
    pub(crate) launcher_artifact_bytes: u64,
}

#[derive(Debug, Default)]
pub struct ExternalPlacementBackendRegistry {
    backends: BTreeMap<(String, String), Arc<dyn ExternalPlacementBackend>>,
    contact_gates: Mutex<BTreeMap<String, Weak<AtomicBool>>>,
    guest_runtime_upload: Option<Arc<dyn crate::guest_runtime_upload::GuestRuntimeUploadSource>>,
}

impl ExternalPlacementBackendRegistry {
    /// Install the composition root's source of guest runtime uploads. Absent,
    /// a provider that stages its runtime refuses activation before any claim.
    #[must_use]
    pub fn with_guest_runtime_upload_source(
        mut self,
        source: Arc<dyn crate::guest_runtime_upload::GuestRuntimeUploadSource>,
    ) -> Self {
        self.guest_runtime_upload = Some(source);
        self
    }

    fn guest_runtime_upload_binding(
        &self,
        state: &AppState,
    ) -> Option<crate::guest_runtime_upload::GuestRuntimeUploadBinding> {
        self.guest_runtime_upload.as_ref().map(|source| {
            crate::guest_runtime_upload::GuestRuntimeUploadBinding::new(source.clone(), state)
        })
    }

    pub(crate) fn seal_restoration_verifier_upload(
        &self,
        producer: &crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
        qualification: &crate::node_config::sections::runtime_snapshot_qualification::InstalledRuntimeSnapshotQualificationBinding,
    ) -> Result<
        ryeos_external_execution::restoration_verifier_delivery::SealedQualificationVerifierUpload,
    > {
        let backend = self
            .backends
            .get(&(
                producer.backend().to_owned(),
                producer.adapter_artifact_hash().to_owned(),
            ))
            .context("exact signed restoration verifier adapter generation is not installed")?;
        backend.seal_restoration_verifier_upload(producer, qualification)
    }

    pub(crate) fn verify_restored_snapshot_once(
        &self,
        producer: &crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
        qualification: &crate::node_config::sections::runtime_snapshot_qualification::InstalledRuntimeSnapshotQualificationBinding,
        credential: &PlacementCredential,
        request: &ryeos_external_execution_contract::restored_runtime_measurement::RestoredVerifierAdapterRequest,
        upload: &lillux::InheritedDescriptorAuthority,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<ExternalLifecycleObservation<ryeos_external_execution_contract::restored_runtime_measurement::RestoredVerifierAdapterResponse>>{
        let backend = self
            .backends
            .get(&(
                producer.backend().to_owned(),
                producer.adapter_artifact_hash().to_owned(),
            ))
            .context("exact signed restoration verifier adapter generation is not installed")?;
        backend.verify_restored_snapshot_once(
            producer,
            qualification,
            credential,
            request,
            upload,
            deadline,
        )
    }

    pub(crate) fn preflight_snapshot_qualification_create(
        &self,
        producer: &crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
        qualification: &crate::node_config::sections::runtime_snapshot_qualification::InstalledRuntimeSnapshotQualificationBinding,
        credential: &PlacementCredential,
    ) -> Result<()> {
        let backend = self
            .backends
            .get(&(
                producer.backend().to_owned(),
                producer.adapter_artifact_hash().to_owned(),
            ))
            .context("exact signed qualification adapter generation is not installed")?;
        backend.preflight_snapshot_qualification_create(producer, qualification, credential)
    }

    pub(crate) fn create_snapshot_qualification_occurrence(
        &self,
        producer: &crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
        qualification: &crate::node_config::sections::runtime_snapshot_qualification::InstalledRuntimeSnapshotQualificationBinding,
        credential: &PlacementCredential,
        request: &ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotQualificationAdapterRequest,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<ExternalLifecycleObservation<ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotQualificationAdapterResponse>>{
        let backend = self
            .backends
            .get(&(
                producer.backend().to_owned(),
                producer.adapter_artifact_hash().to_owned(),
            ))
            .context("exact signed qualification adapter generation is not installed")?;
        backend.create_snapshot_qualification_occurrence(
            producer,
            qualification,
            credential,
            request,
            deadline,
        )
    }

    pub(crate) fn terminate_snapshot_qualification_occurrence(
        &self,
        producer: &crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
        qualification: &crate::node_config::sections::runtime_snapshot_qualification::InstalledRuntimeSnapshotQualificationBinding,
        credential: &PlacementCredential,
        request: &ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotQualificationTerminationAdapterRequest,
        first_contact: bool,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<ExternalLifecycleObservation<ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotQualificationTerminationAdapterResponse>>{
        let backend = self
            .backends
            .get(&(
                producer.backend().to_owned(),
                producer.adapter_artifact_hash().to_owned(),
            ))
            .context(
                "exact signed qualification termination adapter generation is not installed",
            )?;
        backend.terminate_snapshot_qualification_occurrence(
            producer,
            qualification,
            credential,
            request,
            first_contact,
            deadline,
        )
    }

    pub(crate) fn preflight_runtime_snapshot(
        &self,
        binding: &crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
        credential: &PlacementCredential,
    ) -> Result<()> {
        let backend = self
            .backends
            .get(&(
                binding.backend().to_owned(),
                binding.adapter_artifact_hash().to_owned(),
            ))
            .context("exact signed snapshot producer adapter generation is not installed")?;
        backend.preflight_runtime_snapshot(binding, credential)
    }

    pub(crate) fn bootstrap_source_profile(
        &self,
        binding: &crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
    ) -> Result<(String, String)> {
        let backend = self
            .backends
            .get(&(
                binding.backend().to_owned(),
                binding.adapter_artifact_hash().to_owned(),
            ))
            .context("exact signed bootstrap adapter generation is not installed")?;
        Ok((
            backend
                .bootstrap_profile_digest()
                .context("signed adapter has no inspected bootstrap profile")?
                .to_owned(),
            backend
                .bootstrap_provider_spec_digest()
                .context("signed adapter has no inspected bootstrap provider spec")?
                .to_owned(),
        ))
    }

    pub(crate) fn lifecycle_escrow_capture(
        &self,
        binding: &crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
    ) -> Result<crate::external_artifacts::LifecycleEscrowCapture<'_>> {
        let backend = self
            .backends
            .get(&(
                binding.backend().to_owned(),
                binding.adapter_artifact_hash().to_owned(),
            ))
            .context("exact signed lifecycle generation is not installed")?;
        backend
            .lifecycle_escrow_capture()?
            .context("installed backend has no captured signed lifecycle source")
    }

    pub(crate) fn create_snapshot_bootstrap_source(
        &self,
        binding: &crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
        credential: &PlacementCredential,
        request: &ryeos_external_execution_contract::runtime_snapshot_bootstrap::RuntimeSnapshotBootstrapAdapterRequest,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<ExternalLifecycleObservation<ryeos_external_execution_contract::runtime_snapshot_bootstrap::RuntimeSnapshotBootstrapAdapterResponse>>{
        let backend = self
            .backends
            .get(&(
                binding.backend().to_owned(),
                binding.adapter_artifact_hash().to_owned(),
            ))
            .context("exact signed bootstrap adapter generation is not installed")?;
        backend.create_snapshot_bootstrap_source(binding, credential, request, deadline)
    }

    /// Resolve the exact inspected adapter generation selected by a signed
    /// producer binding. The caller must have won the durable snapshot-attempt
    /// claim; this lookup by itself never authorizes provider contact.
    pub(crate) fn produce_runtime_snapshot(
        &self,
        binding: &crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
        credential: &PlacementCredential,
        request: &ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotAdapterRequest,
        upload: &lillux::InheritedDescriptorAuthority,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<
        ExternalLifecycleObservation<
            ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotAdapterResponse,
        >,
    > {
        let backend = self
            .backends
            .get(&(
                binding.backend().to_owned(),
                binding.adapter_artifact_hash().to_owned(),
            ))
            .context("exact signed snapshot producer adapter generation is not installed")?;
        backend.produce_runtime_snapshot(binding, credential, request, upload, deadline)
    }

    pub(crate) fn observe_runtime_snapshot_readiness(
        &self,
        binding: &crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
        credential: &PlacementCredential,
        request: &ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotReadinessRequest,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<
        ExternalLifecycleObservation<
            ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotReadinessObservation,
        >,
    >{
        let backend = self
            .backends
            .get(&(
                binding.backend().to_owned(),
                binding.adapter_artifact_hash().to_owned(),
            ))
            .context("exact signed snapshot producer adapter generation is not installed")?;
        backend.observe_runtime_snapshot_readiness(binding, credential, request, deadline)
    }

    fn verify_runtime_probe(
        &self,
        contract: &ExternalPlacementBackendContract,
        proof: &RuntimeProbeInput,
        source: &ryeos_external_execution::guest_runtime_product::GuestOwnerRuntimeManifestIdentity,
        binding_hash: &str,
    ) -> Result<()> {
        let backend = self
            .backends
            .get(&(
                contract.backend.clone(),
                contract.backend_artifact_hash.clone(),
            ))
            .context("exact signed external placement backend generation is not installed")?;
        backend.verify_runtime_probe(contract, proof, source, binding_hash)
    }

    /// Exact installed artifact coordinates of the one lifecycle generation
    /// registered under this backend id, for composing a signed placement
    /// binding.
    pub(crate) fn installed_generation(
        &self,
        backend_id: &str,
    ) -> Result<InstalledLifecycleGeneration> {
        let mut matching = self
            .backends
            .iter()
            .filter(|((id, _), _)| id == backend_id)
            .map(|(_, backend)| backend);
        let backend = matching.next().with_context(|| {
            format!("external lifecycle backend `{backend_id}` is not installed")
        })?;
        ensure!(
            matching.next().is_none(),
            "external lifecycle backend `{backend_id}` has more than one installed generation"
        );
        let (supervisor_hash, supervisor_bytes) = backend.supervisor_artifact();
        let (launcher_hash, launcher_bytes) = backend.launcher_artifact();
        Ok(InstalledLifecycleGeneration {
            backend_artifact_hash: backend.artifact_hash().to_owned(),
            backend_artifact_bytes: backend.artifact_bytes(),
            settings_schema_digest: backend.settings_schema_digest().to_owned(),
            supervisor_artifact_hash: supervisor_hash.to_owned(),
            supervisor_artifact_bytes: supervisor_bytes,
            launcher_artifact_hash: launcher_hash.to_owned(),
            launcher_artifact_bytes: launcher_bytes,
        })
    }

    pub(crate) fn from_backends(backends: Vec<Arc<dyn ExternalPlacementBackend>>) -> Result<Self> {
        let mut by_id = BTreeMap::new();
        for backend in backends {
            let id = backend.backend_id();
            ensure!(
                !id.is_empty()
                    && id.len() <= 128
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.')),
                "external placement backend id is invalid"
            );
            ensure!(
                lillux::valid_hash(backend.artifact_hash()),
                "external placement backend artifact identity is invalid"
            );
            let coordinate = (id.to_owned(), backend.artifact_hash().to_owned());
            ensure!(
                by_id.insert(coordinate, backend).is_none(),
                "external placement backend generation is duplicated"
            );
        }
        Ok(Self {
            backends: by_id,
            contact_gates: Mutex::new(BTreeMap::new()),
            guest_runtime_upload: None,
        })
    }

    fn qualify(
        &self,
        contract: &ExternalPlacementBackendContract,
        credential: &PlacementCredential,
        program: &AdmittedExternalExecutionProgram,
    ) -> Result<Arc<dyn ExternalPlacementBackend>> {
        self.qualify_for_purpose(contract, credential, program, false)
    }

    fn qualify_for_cleanup(
        &self,
        contract: &ExternalPlacementBackendContract,
        credential: &PlacementCredential,
        program: &AdmittedExternalExecutionProgram,
    ) -> Result<Arc<dyn ExternalPlacementBackend>> {
        self.qualify_for_purpose(contract, credential, program, true)
    }

    /// Live relay authority is the signed ControllerFileRelay selection plus
    /// the exact signed provider route spec checked by the adapter. It is
    /// separate from cleanup qualification, which deliberately relaxes
    /// startup capabilities and cannot authorize a new channel file contact.
    fn qualify_for_relay(
        &self,
        contract: &ExternalPlacementBackendContract,
        credential: &PlacementCredential,
        program: &AdmittedExternalExecutionProgram,
    ) -> Result<Arc<dyn ExternalPlacementBackend>> {
        contract.controller_transport.relay()?;
        ensure!(
            matches!(
                program,
                AdmittedExternalExecutionProgram::QualificationStructuredSession(_)
            ),
            "external relay file requires a qualification-only session"
        );
        // Full live qualification retains SupervisorActivation and every
        // signed Q workload capability requirement. In particular, today's
        // unsupported_pending Render profile cannot contact a relay file.
        self.qualify(contract, credential, program)
    }

    fn qualify_for_ordinary_relay(
        &self,
        contract: &ExternalPlacementBackendContract,
        credential: &PlacementCredential,
        program: &AdmittedExternalExecutionProgram,
    ) -> Result<Arc<dyn ExternalPlacementBackend>> {
        contract.controller_transport.relay()?;
        let ordinary = contract.workload.structured_session()?;
        let AdmittedExternalExecutionProgram::StructuredSession(program) = program else {
            bail!("ordinary relay requires an admitted ordinary structured session");
        };
        ensure!(
            program.runtime_manifest_hash == ordinary.runtime_manifest_hash
                && program.selection_identity_digest == ordinary.runtime_selection_identity,
            "ordinary relay changed its retained structured-session runtime selection"
        );
        // This is the full current provider/runtime capability qualification.
        // An authored ordinary binding does not upgrade unsupported adapter
        // capability evidence.
        self.qualify(
            contract,
            credential,
            &AdmittedExternalExecutionProgram::StructuredSession(program.clone()),
        )
    }

    fn qualify_for_purpose(
        &self,
        contract: &ExternalPlacementBackendContract,
        credential: &PlacementCredential,
        program: &AdmittedExternalExecutionProgram,
        cleanup_only: bool,
    ) -> Result<Arc<dyn ExternalPlacementBackend>> {
        program.validate()?;
        let (required, runtime_basis) = match program {
            AdmittedExternalExecutionProgram::StructuredSession(program) => {
                let session = contract.workload.structured_session()?;
                ensure!(
                    program.runtime_manifest_hash == session.runtime_manifest_hash
                        && program.selection_identity_digest == session.runtime_selection_identity,
                    "external qualification changed its exact session runtime"
                );
                (
                    program.requirement.required_lifecycle_capabilities.clone(),
                    RuntimeAdmissionBasis::OrdinarySignedRelationship,
                )
            }
            AdmittedExternalExecutionProgram::QualificationStructuredSession(program) => {
                ensure!(
                    contract.max_export_bytes == 0
                        && contract.runtime_qualification.is_none()
                        && contract.runtime_content_qualification.is_none(),
                    "qualification-only session cannot borrow ordinary runtime or export authority"
                );
                let session = contract.workload.qualification_structured_session()?;
                ensure!(
                    program.requirement.qualification_requirement_digest()?
                        == session.requirement_digest
                        && program.runtime_manifest_hash == session.runtime_manifest_hash,
                    "qualification session changed its exact signed runtime"
                );
                (
                    program.requirement.required_lifecycle_capabilities.clone(),
                    RuntimeAdmissionBasis::RestoredOwnerMeasurement,
                )
            }
            AdmittedExternalExecutionProgram::DirectCommand(program) => {
                ensure!(matches!(contract.workload,
                    crate::node_config::sections::external_execution::ExternalWorkloadBinding::DirectCommand {})
                    && contract.max_export_bytes == 0,
                    "external direct qualification requires its direct workload without export");
                ensure!(
                    (1..=u64::from(contract.timeout_seconds))
                        .contains(&program.projection().timeout_seconds),
                    "external direct qualification exceeds its signed execution budget"
                );
                // This is lifecycle qualification only. Neither controller
                // isolation nor adapter metadata proves guest-native readiness.
                (
                    BTreeSet::new(),
                    RuntimeAdmissionBasis::OrdinarySignedRelationship,
                )
            }
        };
        // Retained cleanup must remain possible when startup capability is
        // absent. Individual reconciliation claims are checked at settlement.
        self.qualify_dependencies(
            contract,
            credential,
            if cleanup_only {
                BTreeSet::new()
            } else {
                required
            },
            !cleanup_only,
            runtime_basis,
        )
    }

    /// Offline installed lifecycle qualification, independent of born-thread
    /// program compilation. This grants neither contact nor guest execution.
    fn qualify_dependencies(
        &self,
        contract: &ExternalPlacementBackendContract,
        credential: &PlacementCredential,
        mut required: BTreeSet<LifecycleCapability>,
        require_activation: bool,
        runtime_basis: RuntimeAdmissionBasis,
    ) -> Result<Arc<dyn ExternalPlacementBackend>> {
        let backend = self.installed(contract)?;
        ensure!(
            credential.backend() == contract.backend && credential.account() == contract.account,
            "protected placement credential has the wrong canonical account"
        );
        backend.qualify_offline(contract, credential)?;
        ensure!(
            contract.cleanup_proof == "provider_terminal_occurrence_v1",
            "external placement has an unsupported cleanup evidence contract"
        );
        // Allocation without an installed activation operation strands a paid
        // occurrence. Require startup capability before any provider contact.
        if require_activation {
            required.insert(LifecycleCapability::SupervisorActivation);
        }
        // This is inherent to the admitted cleanup proof. Reconciliation is
        // optional: an unknown outcome may remain quarantined with its capacity
        // reserved, but cannot be converted into an unsupported stronger fact.
        required.insert(LifecycleCapability::ExactTerminalObservation);
        let effective = backend.lifecycle_capabilities();
        if require_activation {
            match runtime_basis {
                RuntimeAdmissionBasis::OrdinarySignedRelationship => ensure!(
                    contract.runtime_qualification.is_some()
                        == effective
                            .contains(&LifecycleCapability::IndependentGuestRuntimeAdmission),
                    "external runtime qualification relationship differs from installed adapter admission"
                ),
                // A provider whose activation stages the runtime itself admits
                // it independently of any prior qualification; the session
                // being qualified is then the product it stages.
                RuntimeAdmissionBasis::RestoredOwnerMeasurement => ensure!(
                    backend.stages_guest_runtime_upload()
                        || !effective
                            .contains(&LifecycleCapability::IndependentGuestRuntimeAdmission),
                    "qualification-only startup cannot satisfy adapter independent guest runtime admission"
                ),
            }
        }
        let missing: BTreeSet<_> = required.difference(&effective).copied().collect();
        ensure!(
            missing.is_empty(),
            "external lifecycle adapter lacks required capabilities: {missing:?}"
        );
        Ok(backend)
    }

    /// The exact installed backend generation a binding names, found without
    /// any credential. This qualifies nothing: it serves only the offline
    /// steps that must precede the credential read, such as lease admission,
    /// and full qualification still follows before any provider contact.
    fn installed(
        &self,
        contract: &ExternalPlacementBackendContract,
    ) -> Result<Arc<dyn ExternalPlacementBackend>> {
        self.backends
            .get(&(
                contract.backend.clone(),
                contract.backend_artifact_hash.clone(),
            ))
            .cloned()
            .context("exact signed external placement backend generation is not installed")
    }

    fn contact_gate(&self, placement: &str) -> Result<Arc<AtomicBool>> {
        let mut gates = self
            .contact_gates
            .lock()
            .map_err(|_| anyhow::anyhow!("external placement contact gate is poisoned"))?;
        if let Some(gate) = gates.get(placement).and_then(Weak::upgrade) {
            return Ok(gate);
        }
        gates.retain(|_, gate| gate.strong_count() > 0);
        let gate = Arc::new(AtomicBool::new(false));
        gates.insert(placement.to_owned(), Arc::downgrade(&gate));
        Ok(gate)
    }
}

/// Offline prebirth check of an already-finalized ordinary external endpoint.
/// Success is not a dispatch permit, guest attestation, or born-thread program.
/// The placement owner must independently rejoin retained authority at contact.
pub(crate) fn preflight_external_direct_endpoint(
    state: &AppState,
    requirement: &ryeos_engine::contracts::ExecutionEndpointRequirement,
    identity: &ryeos_engine::contracts::ExternalEndpointBindingIdentity,
    timeout_seconds: u64,
    required_feature_claims: &[&str],
) -> Result<RetainedExternalExecutionBinding> {
    preflight_external_direct_dependencies(
        &state.node_config.external_execution,
        &state.external_placement_backends,
        requirement,
        identity,
        timeout_seconds,
        |binding| {
            let access = binding.credential_access()?;
            access.decode(
                state
                    .vault
                    .placement_credential(&access)
                    .context("read protected external placement credential")?,
            )
        },
        |binding| {
            require_guest_support_claim_selection(
                &binding.backend_contract(),
                required_feature_claims,
            )?;
            require_current_runtime_qualification_for_start(
                state,
                &binding.backend_contract(),
                binding.digest(),
                None,
            )
        },
    )
}

/// Recheck the current published witness under its actual product owner's
/// grant and run the exact installed adapter's offline probe before fresh
/// contact. The signed relationship alone is never a startup permit.
///
/// A structured session passes its admitted program, whose runtime lane
/// decides the content-testimony slot: an activation-sourced session has its
/// current published acquired-content testimony rechecked (head, revocation
/// and claims), and a product-sourced session must leave the slot empty. A
/// start with no program (a direct command) still runs the workload-shape
/// refusals, so a direct or qualification-only workload can never inherit
/// structured-session testimony.
fn require_current_runtime_qualification_for_start(
    state: &AppState,
    contract: &ExternalPlacementBackendContract,
    binding_hash: &str,
    program: Option<&ryeos_state::external_execution::admission::AdmittedExternalCandidateProgram>,
) -> Result<()> {
    let lane = program.map_or(RuntimeContentLane::Product, RuntimeContentLane::of);
    let _ = require_current_runtime_content_qualification(state, contract, lane)?;
    let _ = admit_current_runtime_qualification(state, contract, binding_hash)?;
    Ok(())
}

/// The current runtime content qualification a launch or admission rests on.
/// A remote testimony is never returned as a published content qualification:
/// it keeps the origin's evidence and attestation hash beside the explicit
/// marker of the local acceptance that admitted it.
#[derive(Debug, Clone)]
struct CurrentRuntimeContentQualification {
    coordinate_id: String,
    attestation_hash: String,
    evidence: ryeos_state::external_content::qualification_evidence::ContentQualificationEvidence,
    remote_testimony: Option<ryeos_state::objects::RetainedRemoteQualificationTestimony>,
}

impl CurrentRuntimeContentQualification {
    fn into_retained(
        self,
        binding_hash: String,
        runtime_manifest_hash: String,
        activation_ref: String,
    ) -> ryeos_state::objects::RetainedExternalRuntimeContentQualification {
        ryeos_state::objects::RetainedExternalRuntimeContentQualification {
            binding_hash,
            runtime_manifest_hash,
            activation_ref,
            coordinate_id: self.coordinate_id,
            attestation_hash: self.attestation_hash,
            evidence: self.evidence,
            remote_testimony: self.remote_testimony,
        }
    }
}

/// Which source a structured session's runtime came from, as far as the
/// content-testimony slot is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeContentLane {
    /// The runtime is an admitted product selection; its compatibility claims
    /// ride that selection and the slot stays empty.
    Product,
    /// The runtime is activated acquired content; the slot carries its
    /// acquired-content testimony.
    Activation,
}

impl RuntimeContentLane {
    fn of(
        program: &ryeos_state::external_execution::admission::AdmittedExternalCandidateProgram,
    ) -> Self {
        use ryeos_state::external_execution::admission::ExternalCandidateRuntimeSource;
        match program.runtime_source {
            ExternalCandidateRuntimeSource::CapturedProduct { .. } => Self::Product,
            ExternalCandidateRuntimeSource::ActivatedContent { .. } => Self::Activation,
        }
    }
}

/// The current acquired-content testimony a structured session rests on. The
/// slot carries testimony for activation-sourced runtimes only: a
/// product-sourced session (`RuntimeContentLane::Product`) must leave it empty
/// and yields `None`, because its runtime authority is its admitted product
/// selection. Acquired-runtime compatibility does not substitute for the
/// separate guest-owner snapshot proof. Missing testimony is not an activation
/// permit. A binding that selects a remote testimony is checked against the
/// local acceptance of it; it never falls back to, or is mistaken for, a local
/// published qualification.
fn require_current_runtime_content_qualification(
    state: &AppState,
    contract: &ExternalPlacementBackendContract,
    lane: RuntimeContentLane,
) -> Result<Option<CurrentRuntimeContentQualification>> {
    let session = match &contract.workload {
        crate::node_config::sections::external_execution::ExternalWorkloadBinding::StructuredSession(session) => session,
        crate::node_config::sections::external_execution::ExternalWorkloadBinding::QualificationStructuredSession(_) => {
            ensure!(contract.runtime_content_qualification.is_none(),
                "qualification-only workload cannot inherit ordinary runtime compatibility testimony");
            return Ok(None);
        }
        crate::node_config::sections::external_execution::ExternalWorkloadBinding::DirectCommand {} => {
            ensure!(contract.runtime_content_qualification.is_none(),
                "direct workload cannot inherit structured-session compatibility testimony");
            return Ok(None);
        }
    };
    if lane == RuntimeContentLane::Product {
        ensure!(
            contract.runtime_content_qualification.is_none(),
            "a product-sourced structured session carries no acquired-content testimony; its runtime authority is its admitted product selection"
        );
        return Ok(None);
    }
    let selected = contract
        .runtime_content_qualification
        .as_ref()
        .context("external structured session has no qualified runtime content selection")?;
    let operator = crate::operator_authority::admitted_operator_authority_for_principal(
        state,
        &selected.owner_principal,
    )?;
    let context = operator.handler_context();
    let current = match selected.remote_testimony.as_ref() {
        None => {
            let published =
                crate::operator_external_content::content_qualification::load_current_qualified_content(
                    state,
                    &context,
                    &selected.activation_ref,
                    &selected.coordinate_id,
                    &selected.attestation_hash,
                    &session.runtime_manifest_hash,
                )?;
            CurrentRuntimeContentQualification {
                coordinate_id: published.coordinate_id,
                attestation_hash: published.attestation_hash,
                evidence: published.evidence,
                remote_testimony: None,
            }
        }
        Some(_) => {
            let remote = crate::operator_external_content::qualification_testimony::verify_current_remote_runtime_content_testimony(
                state,
                &context,
                selected,
                &session.runtime_manifest_hash,
            )?;
            CurrentRuntimeContentQualification {
                coordinate_id: remote.coordinate_id,
                attestation_hash: remote.qualification_attestation_hash,
                evidence: remote.evidence,
                remote_testimony: Some(
                    ryeos_state::objects::RetainedRemoteQualificationTestimony {
                        acceptance_hash: remote.acceptance_hash,
                        acceptance: remote.acceptance,
                    },
                ),
            }
        }
    };
    current.evidence.result.validate_claims_for(
        &current.evidence.purpose.policy_source.policy,
        &selected.required_claims,
    )?;
    Ok(Some(current))
}

/// A signed selection must demand the mechanisms this controller will send.
/// Actual testimony, exact guest identity and probe are verified below through
/// the existing qualification owner; this selection check grants nothing.
fn require_guest_support_claim_selection(
    contract: &ExternalPlacementBackendContract,
    required: &[&str],
) -> Result<()> {
    let selected = contract
        .runtime_qualification
        .as_ref()
        .context("ordinary external execution requires a selected guest runtime qualification")?;
    for claim in required {
        ensure!(
            selected
                .qualification
                .required_claims
                .iter()
                .any(|selected| selected.as_str() == *claim),
            "signed guest runtime qualification does not require mechanism claim {claim}"
        );
    }
    Ok(())
}

fn require_ordinary_guest_wire_support_selection(
    contract: &ExternalPlacementBackendContract,
) -> Result<()> {
    use crate::node_config::sections::external_execution::ExternalWorkloadBinding;
    use ryeos_external_execution_contract::{
        DIRECT_PROGRAM_SUPPORT_CLAIM, GUEST_WIRE_SUPPORT_CLAIM,
    };
    match &contract.workload {
        ExternalWorkloadBinding::StructuredSession(_) => {
            require_guest_support_claim_selection(contract, &[GUEST_WIRE_SUPPORT_CLAIM])
        }
        ExternalWorkloadBinding::DirectCommand {} => require_guest_support_claim_selection(
            contract,
            &[GUEST_WIRE_SUPPORT_CLAIM, DIRECT_PROGRAM_SUPPORT_CLAIM],
        ),
        ExternalWorkloadBinding::QualificationStructuredSession(_) => {
            bail!("qualification-only measurement cannot use ordinary guest testimony admission")
        }
    }
}

fn require_direct_feature_support_selection(
    contract: &ExternalPlacementBackendContract,
    program: &ryeos_state::external_execution::admission::AdmittedExternalDirectProgram,
) -> Result<()> {
    use ryeos_external_execution_contract::{
        CAPTURED_INPUTS_SUPPORT_CLAIM, GuestWorkspaceView, TARGET_LIMITS_SUPPORT_CLAIM,
    };
    if program.projection().workspace_view == GuestWorkspaceView::CapturedInputs {
        require_guest_support_claim_selection(contract, &[CAPTURED_INPUTS_SUPPORT_CLAIM])?;
    }
    if program.projection().target_limits.is_some() {
        require_guest_support_claim_selection(contract, &[TARGET_LIMITS_SUPPORT_CLAIM])?;
    }
    Ok(())
}

fn admit_current_runtime_qualification(
    state: &AppState,
    contract: &ExternalPlacementBackendContract,
    binding_hash: &str,
) -> Result<Option<ryeos_state::objects::RetainedExternalRuntimeQualification>> {
    require_ordinary_guest_wire_support_selection(contract)?;
    let qualification = contract
        .runtime_qualification
        .as_ref()
        .context("ordinary external execution lost its selected guest qualification")?;
    let (attestation_hash, evidence, remote_testimony) = if qualification.remote_testimony.is_some()
    {
        let remote = crate::operator_external_content::qualification_testimony::verify_current_remote_runtime_testimony(
            state,
            qualification,
            &contract.guest_runtime_manifest_hash,
        )?;
        // The probe snapshot locator is rejoined to this node's own provider
        // journal, which a remote testimony cannot satisfy. Refuse rather
        // than skip the rejoin.
        ensure!(
            remote
                .evidence
                .result
                .probe_evidence
                .get("runtime_snapshot_locator")
                .is_none(),
            "remote qualification testimony cannot carry a runtime snapshot locator"
        );
        (
            remote.qualification_attestation_hash,
            remote.evidence,
            Some(ryeos_state::objects::RetainedRemoteQualificationTestimony {
                acceptance_hash: remote.acceptance_hash,
                acceptance: remote.acceptance,
            }),
        )
    } else {
        let witness = crate::operator_external_content::product_qualification::verify_current_external_runtime_qualification(
            state,
            qualification,
            &contract.guest_runtime_manifest_hash,
        )?;
        (witness.attestation_hash, witness.evidence, None)
    };
    let retained = ryeos_state::objects::RetainedExternalRuntimeQualification {
        binding_hash: binding_hash.to_owned(),
        guest_runtime_manifest_hash: contract.guest_runtime_manifest_hash.clone(),
        owner_principal: qualification.owner_principal.clone(),
        proof: ryeos_state::external_content::products::composition::AdmittedProductQualification {
            attestation_hash,
            evidence,
        },
        remote_testimony,
    };
    retained.validate()?;
    let source = verify_retained_runtime_proof(state, &retained)?;
    crate::operator_runtime_snapshot::verify_probe_snapshot_locator(
        state,
        &retained.proof,
        &source,
        &contract.backend,
        &retained.owner_principal,
    )?;
    state.external_placement_backends.verify_runtime_probe(
        contract,
        &RuntimeProbeInput::from_product(&retained.proof),
        &source,
        binding_hash,
    )?;
    Ok(Some(retained))
}

/// Rejoin the session's CAS-owned guest-runtime proof to the exact signed
/// binding before opening credentials. Historical recovery authenticates this
/// retained proof, never a newly selected product head.
fn require_retained_session_runtime_qualification(
    state: &AppState,
    capsule: &ryeos_state::objects::AdmittedPersistentSessionCapsule,
    binding: &RetainedExternalExecutionBinding,
) -> Result<()> {
    let contract = binding.backend_contract();
    let program = capsule
        .external_candidate
        .as_ref()
        .context("external session capsule has no admitted candidate program")?;
    program.verify_runtime_authority(
        capsule.retained_product_selections.as_ref(),
        capsule
            .retained_external_runtime_content_qualification
            .as_ref(),
    )?;
    let lane = RuntimeContentLane::of(program);
    if lane == RuntimeContentLane::Product {
        require_product_sourced_retention(
            capsule
                .retained_external_runtime_content_qualification
                .is_some(),
            capsule.retained_product_selections.is_some(),
        )?;
        ensure!(
            contract.runtime_content_qualification.is_none(),
            "a product-sourced session binding carries no content qualification selection"
        );
    } else {
        require_retained_session_content_testimony(state, capsule, binding, &contract)?;
    }
    let Some(retained) = match_retained_session_runtime_qualification(
        capsule.retained_external_runtime_qualification.as_ref(),
        &contract,
        binding.digest(),
    )?
    else {
        return Ok(());
    };
    let source = verify_retained_runtime_proof(state, retained)?;
    crate::operator_runtime_snapshot::verify_probe_snapshot_locator(
        state,
        &retained.proof,
        &source,
        &contract.backend,
        &retained.owner_principal,
    )?;
    state.external_placement_backends.verify_runtime_probe(
        &contract,
        &RuntimeProbeInput::from_product(&retained.proof),
        &source,
        binding.digest(),
    )
}

/// What a retained product-sourced session must and must not carry: its
/// runtime authority is the admitted product selection, never acquired-content
/// testimony.
fn require_product_sourced_retention(has_content: bool, has_selections: bool) -> Result<()> {
    ensure!(
        !has_content,
        "a product-sourced session retains no acquired-content testimony"
    );
    ensure!(
        has_selections,
        "a product-sourced session retains its admitted product selection"
    );
    Ok(())
}

/// The activation-sourced half of retained session qualification: the
/// retained acquired-content testimony must match the signed binding.
fn require_retained_session_content_testimony(
    state: &AppState,
    capsule: &ryeos_state::objects::AdmittedPersistentSessionCapsule,
    binding: &RetainedExternalExecutionBinding,
    contract: &ExternalPlacementBackendContract,
) -> Result<()> {
    let content = capsule
        .retained_external_runtime_content_qualification
        .as_ref()
        .context("external session has no retained content qualification")?;
    let selected = contract
        .runtime_content_qualification
        .as_ref()
        .context("external session binding has no content qualification selection")?;
    let session = contract.workload.structured_session()?;
    ensure!(
        content.binding_hash == binding.digest()
            && content.activation_ref == selected.activation_ref
            && content.coordinate_id == selected.coordinate_id
            && content.attestation_hash == selected.attestation_hash
            && content.runtime_manifest_hash == session.runtime_manifest_hash
            && content.evidence.purpose.owner_fingerprint == selected.owner_principal
            && content.remote_testimony.as_ref().map(|remote| (
                &remote.acceptance_hash,
                &remote.acceptance.acceptance_policy_ref
            )) == selected
                .remote_testimony
                .as_ref()
                .map(|remote| { (&remote.acceptance_hash, &remote.acceptance_policy_ref) }),
        "retained content testimony differs from signed session binding"
    );
    content.evidence.result.validate_claims_for(
        &content.evidence.purpose.policy_source.policy,
        &selected.required_claims,
    )?;
    verify_retained_runtime_content(state, content)
}

/// Recovery validates only the capsule's historical CAS-owned witness. First
/// contact separately requires the current binding and current qualification;
/// already-contacted occurrences must not be rebound to a later product head.
pub fn verify_retained_external_candidate_capsule(
    state: &AppState,
    capsule: &ryeos_state::objects::AdmittedPersistentSessionCapsule,
) -> Result<()> {
    let Some(program) = capsule.external_candidate.as_ref() else {
        bail!("retained external candidate verification requires its admitted program");
    };
    program.verify_runtime_authority(
        capsule.retained_product_selections.as_ref(),
        capsule
            .retained_external_runtime_content_qualification
            .as_ref(),
    )?;
    match RuntimeContentLane::of(program) {
        RuntimeContentLane::Activation => {
            let content = capsule
                .retained_external_runtime_content_qualification
                .as_ref()
                .context("external candidate capsule has no retained content testimony")?;
            program.require_content_qualification(&content.evidence)?;
            ensure!(
                content.runtime_manifest_hash == program.runtime_manifest_hash,
                "external candidate content proof differs from admitted runtime"
            );
            verify_retained_runtime_content(state, content)?;
        }
        RuntimeContentLane::Product => {
            require_product_sourced_retention(
                capsule
                    .retained_external_runtime_content_qualification
                    .is_some(),
                capsule.retained_product_selections.is_some(),
            )?;
        }
    }
    if let Some(retained) = capsule.retained_external_runtime_qualification.as_ref() {
        verify_retained_runtime_proof(state, retained)?;
    }
    Ok(())
}

fn verify_retained_runtime_content(
    state: &AppState,
    retained: &ryeos_state::objects::RetainedExternalRuntimeContentQualification,
) -> Result<()> {
    retained.validate()?;
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let limits = state
        .node_policy
        .require::<ryeos_node_runtime::node_policy::sections::object_closure::NodeObjectClosurePolicy>()?
        .closure_limits()?;
    if retained.remote_testimony.is_some() {
        return crate::operator_external_content::qualification_testimony::verify_retained_remote_content_testimony_guarded(
            state,
            &authority,
            &guard,
            limits,
            retained,
        );
    }
    let value = ryeos_state::object_closure::load_exact_cas_object_with_cas(
        &authority.cas_store()?,
        &retained.attestation_hash,
        limits.max_object_bytes,
    )?;
    let attestation = ryeos_state::objects::Attestation::from_value(&value)?;
    let verified = ryeos_state::external_content::qualification_publication::verify_retained(
        &authority,
        &attestation,
        &retained.evidence.purpose.owner_fingerprint,
        &retained.coordinate_id,
        state.identity.verifying_key(),
        limits,
        &guard,
    )?;
    ensure!(
        verified.attestation_hash == retained.attestation_hash
            && verified.evidence == retained.evidence,
        "retained session content differs from authenticated historical testimony"
    );
    Ok(())
}

fn verify_retained_runtime_proof(
    state: &AppState,
    retained: &ryeos_state::objects::RetainedExternalRuntimeQualification,
) -> Result<ryeos_external_execution::guest_runtime_product::GuestOwnerRuntimeManifestIdentity> {
    retained.validate()?;
    let limits = state
        .node_policy
        .require::<ryeos_node_runtime::node_policy::sections::object_closure::NodeObjectClosurePolicy>()?
        .closure_limits()?;
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let product = match &retained.remote_testimony {
        None => {
            crate::operator_external_content::product_qualification::verify_retained_qualification_guarded(
                state,
                &authority,
                &guard,
                limits,
                &retained.proof,
                &retained.owner_principal,
            )?;
            crate::operator_external_content::product_receipt::load_product_source(
                state,
                &authority,
                &guard,
                limits,
                &retained.owner_principal,
                &retained.proof.evidence.product_witness_hash,
                &retained.proof.evidence.witness_source,
                crate::operator_external_content::product_receipt::ProductSourceVerification::Retained,
            )?
        }
        Some(remote) => {
            crate::operator_external_content::qualification_testimony::verify_retained_remote_testimony_guarded(
                state,
                &authority,
                &guard,
                limits,
                remote,
                &retained.proof,
                &retained.owner_principal,
            )?
        }
    };
    let manifest_hash = &retained.proof.evidence.result.subject_manifest_hash;
    ensure!(
        product.attestation_hash == retained.proof.evidence.product_witness_hash
            && product.evidence.manifest_hash == *manifest_hash
            && ryeos_state::external_content::products::publication::ProductCaptureCoordinate::from_evidence(&product.evidence)?
                == retained.proof.evidence.product_coordinate,
        "retained guest runtime qualification differs from its product source"
    );
    let manifest = ryeos_state::object_closure::load_exact_cas_object_with_cas(
        &authority.cas_store()?,
        manifest_hash,
        (ryeos_state::objects::MAX_LARGE_CONTENT_MANIFEST_BYTES as u64)
            .min(limits.max_object_bytes),
    )?;
    let source = ryeos_external_execution::guest_runtime_product::derive_guest_owner_runtime_manifest_identity(
        &manifest,
        state.identity.verifying_key(),
    )?;
    ensure!(
        source.manifest_hash == *manifest_hash
            && source.manifest_hash == retained.guest_runtime_manifest_hash,
        "retained guest runtime manifest differs from the qualified product"
    );
    Ok(source)
}

fn match_retained_session_runtime_qualification<'a>(
    retained: Option<&'a ryeos_state::objects::RetainedExternalRuntimeQualification>,
    contract: &ExternalPlacementBackendContract,
    binding_hash: &str,
) -> Result<Option<&'a ryeos_state::objects::RetainedExternalRuntimeQualification>> {
    match (retained, contract.runtime_qualification.as_ref()) {
        (None, None) => Ok(None),
        (Some(retained), Some(selected)) => {
            retained.validate()?;
            ensure!(
                retained.binding_hash == binding_hash
                    && retained.guest_runtime_manifest_hash == contract.guest_runtime_manifest_hash
                    && retained.owner_principal == selected.owner_principal
                    && retained.proof.attestation_hash == selected.attestation_hash
                    && retained
                        .remote_testimony
                        .as_ref()
                        .map(|remote| &remote.acceptance_hash)
                        == selected
                            .remote_testimony
                            .as_ref()
                            .map(|remote| &remote.acceptance_hash)
                    && retained
                        .remote_testimony
                        .as_ref()
                        .map(|remote| &remote.acceptance.acceptance_policy_ref)
                        == selected
                            .remote_testimony
                            .as_ref()
                            .map(|remote| &remote.acceptance_policy_ref),
                "retained external runtime proof differs from signed placement binding"
            );
            Ok(Some(retained))
        }
        _ => bail!("session runtime proof presence differs from signed placement binding"),
    }
}

fn preflight_external_direct_dependencies(
    bindings: &[InstalledExternalExecutionBinding],
    backends: &ExternalPlacementBackendRegistry,
    requirement: &ryeos_engine::contracts::ExecutionEndpointRequirement,
    identity: &ryeos_engine::contracts::ExternalEndpointBindingIdentity,
    timeout_seconds: u64,
    load_credential: impl FnOnce(&InstalledExternalExecutionBinding) -> Result<PlacementCredential>,
    qualify_runtime: impl FnOnce(&RetainedExternalExecutionBinding) -> Result<()>,
) -> Result<RetainedExternalExecutionBinding> {
    requirement.validate()?;
    identity.validate()?;
    let ryeos_engine::contracts::ExecutionEndpointRequirement::External { binding_id, .. } =
        requirement
    else {
        bail!("external direct preflight requires an external endpoint");
    };
    ensure!(
        binding_id == &identity.binding_id,
        "external direct endpoint changed its finalized binding id"
    );
    let binding = bindings
        .iter()
        .find(|binding| binding.id() == binding_id)
        .context("signed direct execution endpoint is not installed")?;
    ensure!(
        binding.digest() == identity.binding_digest,
        "external direct endpoint changed its finalized binding generation"
    );
    let retained = binding.retained_generation()?;
    let contract = retained.backend_contract();
    ensure!(
        matches!(contract.workload,
        crate::node_config::sections::external_execution::ExternalWorkloadBinding::DirectCommand {})
            && contract.max_export_bytes == 0,
        "external direct preflight requires its direct workload without export"
    );
    ensure!(
        (1..=u64::from(contract.timeout_seconds)).contains(&timeout_seconds),
        "external direct preflight exceeds its signed execution budget"
    );
    qualify_runtime(&retained)?;
    let credential = load_credential(binding)?;
    backends.qualify_dependencies(
        &contract,
        &credential,
        BTreeSet::new(),
        true,
        RuntimeAdmissionBasis::OrdinarySignedRelationship,
    )?;
    Ok(retained)
}

/// Admission-only check used while sealing a session capsule. It performs no
/// provider I/O and grants no allocation/contact authority.
pub fn preflight_external_candidate_program(
    state: &AppState,
    program: &ryeos_state::external_execution::admission::AdmittedExternalCandidateProgram,
) -> Result<(
    Option<ryeos_state::objects::RetainedExternalRuntimeQualification>,
    Option<ryeos_state::objects::RetainedExternalRuntimeContentQualification>,
)> {
    let binding = select_binding(&state.node_config.external_execution, program)?;
    let contract = binding.backend_contract();
    let runtime_proof = admit_current_runtime_qualification(state, &contract, binding.digest())?;
    let retained_content = match require_current_runtime_content_qualification(
        state,
        &contract,
        RuntimeContentLane::of(program),
    )? {
        None => None,
        Some(content) => {
            program.require_content_qualification(&content.evidence)?;
            let selected = contract
                .runtime_content_qualification
                .as_ref()
                .context("external candidate has no signed content selection")?;
            let retained_content = content.into_retained(
                binding.digest().to_owned(),
                program.runtime_manifest_hash.clone(),
                selected.activation_ref.clone(),
            );
            retained_content.validate()?;
            Some(retained_content)
        }
    };
    preflight_external_candidate_dependencies(
        &state.node_config.external_execution,
        &state.external_candidate_connectors,
        &state.external_provider_configurations,
        &state.external_placement_backends,
        &state.isolation,
        program,
        |binding| {
            let access = binding.credential_access()?;
            access.decode(
                state
                    .vault
                    .placement_credential(&access)
                    .context("read protected external placement credential")?,
            )
        },
    )?;
    Ok((runtime_proof, retained_content))
}

/// The channel journal bounds a qualification session derives from a signed
/// transfer budget. Composing a binding and admitting a qualification launch
/// both apply this, so a binding whose budget the launch would refuse is not
/// signed.
pub(crate) fn require_qualification_channel_limits(max_transfer_bytes: u64) -> Result<()> {
    let channel_max_bytes = max_transfer_bytes.min(64 * 1024 * 1024);
    let channel_max_frames = u32::try_from(
        channel_max_bytes
            .div_ceil(ryeos_state::external_execution::MAX_CHUNK_BYTES as u64)
            .saturating_mul(4)
            .clamp(64, 65_536),
    )?;
    ryeos_state::external_execution::validate_channel_journal_limits(
        ryeos_state::external_execution::ExternalExecutionMode::QualificationStructuredSession {},
        channel_max_frames,
        channel_max_bytes,
    )
}

/// Offline placement admission for the accepted qualification-only Worker.
/// The caller must authenticate the purpose and product member from the born
/// root. This checks the current signed node selection and installed
/// dependencies, but gives no reservation, credential or provider-contact
/// permit. StateStore reauthenticates the root and binding at each first-contact
/// transition.
pub(crate) fn preflight_external_qualification_program(
    state: &AppState,
    program: &ryeos_state::external_execution::qualification_session::AdmittedExternalQualificationSessionProgram,
    purpose: &ryeos_state::external_content::qualification_purpose::QualificationLaunchPurpose,
    journal_funding: crate::external_supervisor_native::SupervisorJournalFunding,
) -> Result<RetainedExternalExecutionBinding> {
    program.validate_against_purpose(purpose)?;
    let matched = state
        .node_config
        .external_execution
        .iter()
        .filter(|binding| {
            binding
                .check_qualification_program(program, purpose)
                .is_ok()
        })
        .collect::<Vec<_>>();
    let binding = match matched.as_slice() {
        [binding] => *binding,
        [] => bail!("no signed placement binding admits the exact qualification program"),
        _ => bail!("qualification program matches multiple signed placement bindings"),
    };
    let contract = binding.backend_contract();
    // Validate before allocation/provider contact, not only when the already
    // contacted occurrence attaches its channel. Earlier traffic cannot use
    // the signed allowance reserved for native Q settlement and capture.
    require_qualification_channel_limits(contract.max_transfer_bytes)?;
    state.external_candidate_connectors.qualify(&contract)?;
    let provider = state.external_provider_configurations.qualify(&contract)?;
    let invocation = purpose
        .consumer_invocation
        .as_ref()
        .context("Q binder preflight has no original sealed invocation")?;
    let request = purpose
        .policy_source
        .policy
        .producer_scenarios
        .get(&invocation.scenario_id)
        .and_then(|scenario| scenario.consumer_worker_request.as_ref())
        .context("Q binder preflight has no original signed Worker request")?;
    request.validate()?;
    if request.controller_canary.is_some() {
        // The exact captured signed provider opts in before allocator/provider
        // contact. Do not probe an older adapter or try another implementation.
        provider
            .declaration()
            .require_qualification_request_binding()?;
    }

    require_connector_process_group_authority(
        provider.declaration().connector_process_group,
        state.isolation.is_enforced(),
        state
            .isolation
            .inspection()
            .process_scope_readiness
            .trusted_exclusive_session
            .ready,
    )?;
    let access = binding.credential_access()?;
    let credential = access.decode(
        state
            .vault
            .placement_credential(&access)
            .context("read protected qualification placement credential")?,
    )?;
    let backend = state.external_placement_backends.qualify(
        &contract,
        &credential,
        &AdmittedExternalExecutionProgram::QualificationStructuredSession(program.clone()),
    )?;
    require_guest_package_artifact_floor(&contract, backend.as_ref())?;
    // A new occurrence's supervisor journals must be funded for its whole
    // lifetime before any provider contact. A qualification reservation
    // carries the contract timeout, the value its activation later checks.
    journal_funding.require(
        state.node_policy.as_ref(),
        &contract,
        contract.timeout_seconds,
    )?;
    binding.retained_generation()
}

/// An early offline check for callers that have authenticated the accepted
/// root but have not yet materialized its guest descriptors. Reservation must
/// repeat this check against the then-current signed binding.
pub fn preflight_external_qualification_guest(
    state: &AppState,
    root: &crate::operator_external_content::product_qualification::PreparedExternalQualificationRoot,
) -> Result<()> {
    preflight_external_qualification_program(
        state,
        root.program(),
        root.purpose(),
        crate::external_supervisor_native::SupervisorJournalFunding::New,
    )?;
    Ok(())
}

/// Resolve one explicitly content-backed requirement against uniquely matching
/// signed placement configuration. No credential access or provider operation.
pub fn prepare_content_candidate_program(
    state: &AppState,
    requirement: &ryeos_state::external_execution::admission::ExternalCandidateRequirement,
    qualification_use: &ryeos_state::external_execution::admission::ExternalCandidateQualificationUse,
    realizations: &ryeos_state::objects::ExternalContentRealizationSet,
) -> Result<(
    ryeos_state::external_execution::admission::AdmittedExternalCandidateProgram,
    ryeos_state::objects::RetainedExternalRuntimeContentQualification,
)> {
    requirement.validate()?;
    ensure!(requirement.runtime_authority == ryeos_state::external_execution::admission::ExternalCandidateRuntimeAuthority::ActivatedContent,
        "content preparation requires the signed acquired-runtime lane");
    realizations.validate()?;
    let runtime = realizations
        .iter()
        .find(|entry| entry.id == requirement.runtime_product_declaration_id)
        .context("activated candidate runtime has no admitted realization")?;
    let eligible = state.node_config.external_execution.iter().filter(|binding| {
        let contract = binding.backend_contract();
        match &contract.workload {
            crate::node_config::sections::external_execution::ExternalWorkloadBinding::StructuredSession(session) =>
                session.provider_declaration_id == requirement.provider_declaration_id
                && session.provider_configuration_destination == requirement.provider_configuration_destination
                && session.connector_protocol == requirement.connector_protocol
                && session.runtime_manifest_hash == runtime.manifest_hash,
            _ => false,
        }
    }).collect::<Vec<_>>();
    // Refuse ambiguous configuration before resolving testimony. A failing
    // qualification cannot cause selection of another apparently convenient
    // binding; there is no fallback after an authority failure.
    let binding = match eligible.as_slice() {
        [binding] => *binding,
        [] => bail!("no signed placement binding selects this activated runtime"),
        _ => bail!("multiple signed placement bindings select this activated runtime"),
    };
    let contract = binding.backend_contract();
    let qualified = require_current_runtime_content_qualification(
        state,
        &contract,
        RuntimeContentLane::Activation,
    )?
    .context("signed activated-runtime binding has no authenticated content proof")?;
    let selected = contract
        .runtime_content_qualification
        .as_ref()
        .context("signed activated-runtime binding has no content selection")?;
    let retained = qualified.into_retained(
        binding.digest().to_owned(),
        runtime.manifest_hash.clone(),
        selected.activation_ref.clone(),
    );
    let ryeos_state::external_content::qualification_purpose::QualificationSubject::ActivatedContent { content } =
        &retained.evidence.purpose.subject else { bail!("runtime content proof lost its activated source"); };
    content
        .verify_verifier_realizations(realizations, &requirement.runtime_product_declaration_id)?;
    let program = requirement.resolve_for_content(&retained, qualification_use)?;
    binding.check_program(&program)?;
    Ok((program, retained))
}

fn preflight_external_candidate_dependencies(
    bindings: &[InstalledExternalExecutionBinding],
    connectors: &ExternalCandidateConnectorRegistry,
    provider_configurations: &ExternalProviderConfigurationRegistry,
    backends: &ExternalPlacementBackendRegistry,
    isolation: &ryeos_engine::isolation::IsolationRuntime,
    program: &ryeos_state::external_execution::admission::AdmittedExternalCandidateProgram,
    load_credential: impl FnOnce(&InstalledExternalExecutionBinding) -> Result<PlacementCredential>,
) -> Result<()> {
    let binding = select_binding(bindings, program)?;
    let contract = binding.backend_contract();
    // The exact installed connector is an admission prerequisite, not an
    // observation made after credential access or provider qualification.
    connectors.qualify(&contract)?;
    let provider = provider_configurations.qualify(&contract)?;
    require_connector_process_group_authority(
        provider.declaration().connector_process_group,
        isolation.is_enforced(),
        isolation
            .inspection()
            .process_scope_readiness
            .trusted_exclusive_session
            .ready,
    )?;
    let credential = load_credential(binding)?;
    backends.qualify(
        &contract,
        &credential,
        &AdmittedExternalExecutionProgram::StructuredSession(program.clone()),
    )?;
    Ok(())
}

fn require_connector_process_group_authority(
    mode: ryeos_external_execution_contract::ExternalProviderConnectorProcessGroup,
    enforced_isolation: bool,
    trusted_session_ready: bool,
) -> Result<()> {
    use ryeos_external_execution_contract::ExternalProviderConnectorProcessGroup;
    if mode == ExternalProviderConnectorProcessGroup::New {
        ensure!(
            !enforced_isolation && trusted_session_ready,
            "signed provider connector requires a trusted controller process-group session; enforced strict-group isolation has no retained controller scope"
        );
    }
    Ok(())
}

/// Authenticate the one-use occurrence attachment capability.  This does not
/// register a key or apply any execution frame.  Callers must still pass the
/// returned exact occurrence into `attach_external_execution_channel`.
pub fn authenticate_external_channel_bootstrap(
    state: &AppState,
    placement: &str,
    occurrence_id: &str,
    bootstrap_capability: &str,
) -> Result<AuthenticatedExternalOccurrence> {
    let controller_lifetime = Arc::clone(&state.controller_lifetime);
    controller_lifetime
        .ensure_protects_app_root(&state.config.app_root)
        .context("external channel controller lease has the wrong app root")?;
    let allocation = state
        .state_store
        .external_allocation(placement)?
        .context("external channel has no retained allocation")?;
    ensure!(
        allocation.phase == ExternalAllocationPhase::Bound,
        "external channel attachment requires one bound occurrence"
    );
    let occurrence = allocation
        .occurrence
        .as_ref()
        .context("bound external allocation lost its occurrence")?;
    ensure!(
        occurrence.occurrence_id == occurrence_id,
        "external channel attachment changed its occurrence"
    );
    let retained = state
        .state_store
        .retained_external_binding(&allocation.reservation.binding_hash)?
        .context("external channel attachment lost its binding generation")?;
    let contract = retained.backend_contract();
    let program = retained_external_channel_program(state, &allocation.reservation, &retained)?;
    let now = lillux::time::timestamp_millis();
    let attach_deadline = allocation
        .reservation
        .contact_deadline_ms
        .checked_add(i64::from(contract.observation_timeout_seconds) * 1_000)
        .context("external channel attachment deadline overflow")?;
    let activation = state
        .state_store
        .external_supervisor_activation(placement)?
        .context("external channel attachment has no durable supervisor activation")?;
    require_external_activation_runtime(&program, &activation.intent.supervisor_runtime_hash)?;
    ensure!(
        activation.intent.binding_hash == allocation.reservation.binding_hash
            && activation.intent.request_digest == allocation.reservation.request_digest
            && activation.intent.occurrence_id == occurrence.occurrence_id
            && activation.intent.attachment_deadline_ms == attach_deadline
            && activation
                .observation
                .as_ref()
                .is_none_or(|value| value.activation_state == "started"),
        "external channel attachment contradicts its supervisor activation"
    );
    let existing = state
        .state_store
        .optional_external_execution_channel(placement)?;
    let exact_replay =
        existing_external_channel_matches(existing.as_ref(), &allocation.reservation, occurrence)?;
    ensure!(
        exact_replay || now < attach_deadline,
        "external channel bootstrap capability expired before attachment"
    );
    let access =
        ExternalChannelAuthorityAccess::new(&allocation.reservation.channel_authority_generation)?;
    let authority = access.decode(
        state
            .vault
            .external_channel_authority(&access)
            .context("read protected external channel authority")?,
    )?;
    ensure!(
        authority.owner_public_key() == allocation.reservation.channel_owner_public_key
            && authority.bootstrap_capability_hash()
                == allocation.reservation.channel_bootstrap_capability_hash,
        "protected external channel authority changed its reservation"
    );
    let presented_hash = lillux::sha256_hex(bootstrap_capability.as_bytes());
    ensure!(
        bool::from(
            presented_hash.as_bytes().ct_eq(
                allocation
                    .reservation
                    .channel_bootstrap_capability_hash
                    .as_bytes()
            )
        ),
        "external channel bootstrap authentication failed"
    );
    Ok(AuthenticatedExternalOccurrence {
        placement_thread_id: placement.to_owned(),
        occurrence_id: occurrence_id.to_owned(),
        allocation_request_digest: allocation.reservation.request_digest,
    })
}

fn existing_external_channel_matches(
    existing: Option<&ryeos_state::external_execution::ExecutionChannelBinding>,
    reservation: &ExternalAllocationReservation,
    occurrence: &ExternalAllocationOccurrence,
) -> Result<bool> {
    let Some(existing) = existing else {
        return Ok(false);
    };
    ensure!(
        existing.placement_thread_id == reservation.placement_thread_id
            && existing.allocation_request_digest == reservation.request_digest
            && existing.occurrence_id == occurrence.occurrence_id
            && existing.admitted_capsule_hash == reservation.admitted_capsule_hash
            && existing.base_snapshot_hash == reservation.base_snapshot_hash
            && existing.execution_binding_hash == reservation.binding_hash
            && existing.owner_public_key == reservation.channel_owner_public_key,
        "retained external channel contradicts its bootstrap authority"
    );
    Ok(true)
}

/// Authenticate an attached supervisor solely through its exact channel key.
/// This is not node enrollment and does not reuse the expiring bootstrap
/// capability after attachment.
pub fn authenticate_external_channel_frame(
    state: &AppState,
    placement: &str,
    occurrence_id: &str,
    wire: &[u8],
) -> Result<AuthenticatedExternalChannelFrame> {
    let controller_lifetime = Arc::clone(&state.controller_lifetime);
    controller_lifetime
        .ensure_protects_app_root(&state.config.app_root)
        .context("external channel controller lease has the wrong app root")?;
    let binding = state.state_store.external_execution_channel(placement)?;
    ensure!(
        binding.occurrence_id == occurrence_id,
        "external channel exchange changed its occurrence"
    );
    let verified = ryeos_state::external_execution::SignedExecutionFrame::decode_and_verify(
        wire,
        &binding,
        lillux::time::timestamp_millis(),
    )?;
    ensure!(
        verified.frame().direction
            == ryeos_state::external_execution::ChannelDirection::SupervisorToOwner,
        "external channel exchange requires a supervisor-authored frame"
    );
    Ok(AuthenticatedExternalChannelFrame {
        placement_thread_id: placement.to_owned(),
        occurrence_id: occurrence_id.to_owned(),
        sequence: verified.frame().sequence,
        frame_digest: verified.digest().to_owned(),
    })
}

/// What a channel stream may carry for one occurrence, settled before the
/// stream is opened. This authenticates nothing: the stream's first message
/// must still be a supervisor frame that verifies against the binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalChannelStreamAdmission {
    maximum_response_bytes: u64,
}

impl ExternalChannelStreamAdmission {
    /// The negotiated per-response bound the request/response route applies;
    /// a stream applies the same bound to each outbound message.
    pub fn maximum_response_bytes(&self) -> u64 {
        self.maximum_response_bytes
    }
}

/// Why a channel stream was refused before it carried anything. The two
/// are told apart because a guest that dialed the wrong carrier has a
/// configuration fault, while a channel that is not live has simply ended.
#[derive(Debug)]
pub enum ExternalChannelStreamRefusal {
    /// No live attached binding for this occurrence.
    NoLiveChannel(anyhow::Error),
    /// The signed binding carries its exchange on the request/response route.
    NotSelected,
    /// The occurrence's lease was durably refused. The stream does not
    /// reopen for it, whatever else a restart or a retry would find.
    OccurrenceRefused(String),
}

/// Whether the lease record of an occurrence leaves its channel stream
/// admissible, settled before the allocation's phase is consulted: a
/// durable refusal outlives the allocation that carried it and stays
/// refused. A placement with no lease record has no lease to refuse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalChannelStreamStanding {
    Live,
    Refused(String),
}

impl ExternalChannelStreamStanding {
    pub(crate) fn of(
        record: Option<&crate::runtime_db::external_execution::ExternalLeaseRecord>,
    ) -> Self {
        match record {
            Some(record)
                if record.state
                    == crate::runtime_db::external_execution::ExternalLeaseRecordState::Refused =>
            {
                Self::Refused(
                    record
                        .refusal_reason
                        .clone()
                        .unwrap_or_else(|| "the lease was refused".to_owned()),
                )
            }
            _ => Self::Live,
        }
    }
}

impl std::fmt::Display for ExternalChannelStreamRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoLiveChannel(error) => {
                write!(
                    formatter,
                    "external channel stream has no live channel: {error:#}"
                )
            }
            Self::NotSelected => formatter.write_str(
                "external channel stream refused: the signed binding selects the \
                 request/response route",
            ),
            Self::OccurrenceRefused(reason) => write!(
                formatter,
                "external channel stream refused: the occurrence's lease was refused: {reason}"
            ),
        }
    }
}

/// Whether a signed controller contract carries the exchange on the channel
/// stream. Both carriers ask this one question: the stream admits only a
/// binding that selects it, and the request/response exchange refuses one.
pub fn controller_exchanges_on_stream(
    controller: &ryeos_state::external_execution::transport::ExternalControllerTransportSelection,
) -> bool {
    controller
        .http()
        .is_ok_and(|http| http.exchanges_on_stream())
}

/// Whether the placement's retained signed binding selects the channel
/// stream for its exchange.
pub fn external_channel_exchanges_on_stream(state: &AppState, placement: &str) -> Result<bool> {
    let allocation = state
        .state_store
        .external_allocation(placement)?
        .context("external channel has no retained allocation")?;
    let retained = state
        .state_store
        .retained_external_binding(&allocation.reservation.binding_hash)?
        .context("external channel lost its retained signed binding")?;
    Ok(controller_exchanges_on_stream(
        &retained.backend_contract().controller_transport,
    ))
}

/// The retained facts a stream admission decides on.
pub(crate) struct ChannelStreamFacts<'a> {
    pub(crate) phase: ExternalAllocationPhase,
    pub(crate) bound_occurrence_id: Option<&'a str>,
    pub(crate) binding: Option<&'a ryeos_state::external_execution::ExecutionChannelBinding>,
    pub(crate) controller:
        &'a ryeos_state::external_execution::transport::ExternalControllerTransportSelection,
}

/// Admit a channel stream for `occurrence_id` from retained facts at `now_ms`:
/// an allocation that is not terminated, bound to the same occurrence, an
/// attached channel for it, a time inside the binding's admitted lifetime,
/// and a signed contract that selects the stream.
pub(crate) fn check_channel_stream(
    facts: ChannelStreamFacts<'_>,
    occurrence_id: &str,
    now_ms: i64,
) -> std::result::Result<ExternalChannelStreamAdmission, ExternalChannelStreamRefusal> {
    let live = || -> Result<()> {
        ensure!(
            facts.phase != ExternalAllocationPhase::Terminated,
            "the allocation is terminated"
        );
        ensure!(
            facts
                .bound_occurrence_id
                .context("the allocation has no bound occurrence")?
                == occurrence_id,
            "the allocation is bound to a different occurrence"
        );
        let binding = facts
            .binding
            .context("the occurrence has no attached channel")?;
        ensure!(
            binding.occurrence_id == occurrence_id,
            "the attached channel names a different occurrence"
        );
        ensure!(
            now_ms >= binding.issued_at_ms && now_ms < binding.expires_at_ms,
            "the time is outside the binding's admitted lifetime"
        );
        Ok(())
    };
    live().map_err(ExternalChannelStreamRefusal::NoLiveChannel)?;
    if !controller_exchanges_on_stream(facts.controller) {
        return Err(ExternalChannelStreamRefusal::NotSelected);
    }
    let http = facts
        .controller
        .http()
        .map_err(ExternalChannelStreamRefusal::NoLiveChannel)?;
    Ok(ExternalChannelStreamAdmission {
        maximum_response_bytes: http.maximum_response_bytes,
    })
}

/// Refuse a channel stream unless the occurrence has a live attached binding
/// whose signed route contract selects the stream. See `check_channel_stream`.
pub fn external_channel_stream_admission(
    state: &AppState,
    placement: &str,
    occurrence_id: &str,
) -> std::result::Result<ExternalChannelStreamAdmission, ExternalChannelStreamRefusal> {
    let standing = state
        .state_store
        .external_lease_record(placement)
        .map(|record| ExternalChannelStreamStanding::of(record.as_ref()))
        .map_err(ExternalChannelStreamRefusal::NoLiveChannel)?;
    if let ExternalChannelStreamStanding::Refused(reason) = standing {
        return Err(ExternalChannelStreamRefusal::OccurrenceRefused(reason));
    }
    let load = || -> Result<_> {
        let controller_lifetime = Arc::clone(&state.controller_lifetime);
        controller_lifetime
            .ensure_protects_app_root(&state.config.app_root)
            .context("external channel controller lease has the wrong app root")?;
        let allocation = state
            .state_store
            .external_allocation(placement)?
            .context("external channel stream has no retained allocation")?;
        let binding = state
            .state_store
            .optional_external_execution_channel(placement)?;
        let retained = state
            .state_store
            .retained_external_binding(&allocation.reservation.binding_hash)?
            .context("external channel stream lost its retained signed binding")?;
        Ok((allocation, binding, retained.backend_contract()))
    };
    let (allocation, binding, contract) =
        load().map_err(ExternalChannelStreamRefusal::NoLiveChannel)?;
    check_channel_stream(
        ChannelStreamFacts {
            phase: allocation.phase,
            bound_occurrence_id: allocation
                .occurrence
                .as_ref()
                .map(|occurrence| occurrence.occurrence_id.as_str()),
            binding: binding.as_ref(),
            controller: &contract.controller_transport,
        },
        occurrence_id,
        lillux::time::timestamp_millis(),
    )
}
/// Retain one authenticated supervisor frame and return only exact signed
/// owner frames from the durable backlog. HTTP completion is not application
/// evidence; the supervisor must send a later signed acknowledgement.
pub fn exchange_external_channel_frame(
    state: &AppState,
    authenticated: &AuthenticatedExternalChannelFrame,
    wire: &[u8],
) -> Result<ExternalChannelExchangeResult> {
    let allocation = state
        .state_store
        .external_allocation(&authenticated.placement_thread_id)?
        .context("external channel exchange lost its allocation")?;
    let occurrence = allocation
        .occurrence
        .as_ref()
        .context("external channel exchange lost its occurrence")?;
    ensure!(
        occurrence.occurrence_id == authenticated.occurrence_id,
        "external channel exchange authentication is stale"
    );
    let access =
        ExternalChannelAuthorityAccess::new(&allocation.reservation.channel_authority_generation)?;
    let authority = access.decode(
        state
            .vault
            .external_channel_authority(&access)
            .context("read protected external channel signer")?,
    )?;
    let binding = state
        .state_store
        .external_execution_channel(&authenticated.placement_thread_id)?;
    ensure!(
        authority.owner_public_key() == binding.owner_public_key,
        "protected external channel signer changed its binding"
    );
    let verified = ryeos_state::external_execution::SignedExecutionFrame::decode_and_verify(
        wire,
        &binding,
        lillux::time::timestamp_millis(),
    )?;
    ensure!(
        verified.frame().sequence == authenticated.sequence
            && verified.digest() == authenticated.frame_digest,
        "external channel frame changed after authentication"
    );
    if let lease_ready_gate::LeaseReadyGate::Held =
        lease_ready_gate::admit_frame(state, &allocation, &binding, verified.frame(), wire)?
    {
        return Ok(ExternalChannelExchangeResult {
            incoming_new: false,
            acknowledgement_digest: None,
            outbound: Vec::new(),
            urgent_revocation: None,
        });
    }
    let exchanged = exchange_external_supervisor_frame_and_wake_imports(
        &state.state_store,
        &state.external_candidate_imports,
        &authenticated.placement_thread_id,
        wire,
        authority.owner_signing_key(),
    )?;
    let acknowledgement_digest = exchanged
        .acknowledgement
        .as_ref()
        .map(|frame| frame.digest().to_owned());
    let mut outbound = Vec::with_capacity(exchanged.outbound.len());
    for frame in exchanged.outbound {
        ensure!(
            frame.direction()
                == ryeos_state::external_execution::ChannelDirection::OwnerToSupervisor,
            "external channel backlog changed direction"
        );
        outbound.push(ExternalChannelOutboundFrame {
            sequence: frame.sequence(),
            frame_digest: frame.digest().to_owned(),
            canonical_wire: frame.wire().to_vec(),
        });
    }
    let urgent_revocation = exchanged
        .urgent_revocation
        .map(|frame| {
            ensure!(
                frame.direction()
                    == ryeos_state::external_execution::ChannelDirection::OwnerToSupervisor,
                "external urgent revocation changed direction"
            );
            Ok(ExternalChannelOutboundFrame {
                sequence: frame.sequence(),
                frame_digest: frame.digest().to_owned(),
                canonical_wire: frame.wire().to_vec(),
            })
        })
        .transpose()?;
    Ok(ExternalChannelExchangeResult {
        incoming_new: exchanged.incoming_new,
        acknowledgement_digest,
        outbound,
        urgent_revocation,
    })
}

/// Retain one already-authenticated supervisor frame and discover any import
/// made eligible by that exact transcript transition. Keeping these actions at
/// one boundary ensures Ready/export arrival, a later Quiesce acknowledgement,
/// and idempotent reconnect polls all drive the same durable recovery path.
pub(crate) fn exchange_external_supervisor_frame_and_wake_imports(
    state_store: &Arc<crate::state_store::StateStore>,
    imports: &Arc<crate::external_candidate_import::ExternalCandidateImportPool>,
    placement: &str,
    wire: &[u8],
    owner_signing_key: &lillux::crypto::SigningKey,
) -> Result<crate::runtime_db::external_execution::ExternalSupervisorExchange> {
    let exchanged = state_store.exchange_external_supervisor_frame(
        placement,
        wire,
        owner_signing_key,
        16,
        1024 * 1024,
    )?;
    // Every authenticated exchange may advance an import prerequisite. In
    // particular, the guest exports before it later acknowledges owner
    // Quiesce application. Discover from durable state instead of assuming
    // that seal arrival itself is the only useful wake edge.
    imports.wake_recoverable(Arc::clone(state_store), Some(placement))?;
    crate::dedicated_session_service::notify_projection_change(placement);
    Ok(exchanged)
}

/// Author one controller command from protected authority. This function is an
/// internal runtime boundary, not a workload service: candidates and external
/// supervisors never receive the owner signer.
pub fn author_external_channel_command(
    state: &AppState,
    placement: &str,
    payload: ryeos_state::external_execution::ExecutionChannelPayload,
) -> Result<ExternalChannelOutboundFrame> {
    ensure!(
        matches!(
            &payload,
            ryeos_state::external_execution::ExecutionChannelPayload::ProtocolBytes { .. }
                | ryeos_state::external_execution::ExecutionChannelPayload::Quiesce { .. }
                | ryeos_state::external_execution::ExecutionChannelPayload::Cancel
        ),
        "external controller cannot author a supervisor observation or acknowledgement"
    );
    let controller_lifetime = Arc::clone(&state.controller_lifetime);
    controller_lifetime
        .ensure_protects_app_root(&state.config.app_root)
        .context("external channel controller lease has the wrong app root")?;
    let allocation = state
        .state_store
        .external_allocation(placement)?
        .context("external channel command lost its allocation")?;
    let access =
        ExternalChannelAuthorityAccess::new(&allocation.reservation.channel_authority_generation)?;
    let authority = access.decode(
        state
            .vault
            .external_channel_authority(&access)
            .context("read protected external channel signer")?,
    )?;
    let binding = state.state_store.external_execution_channel(placement)?;
    ensure!(
        authority.owner_public_key() == binding.owner_public_key,
        "protected external channel signer changed its binding"
    );
    let frame = if matches!(
        &payload,
        ryeos_state::external_execution::ExecutionChannelPayload::Cancel
    ) {
        state
            .state_store
            .author_external_owner_revocation(placement, authority.owner_signing_key())?
    } else {
        state.state_store.author_external_owner_frame(
            placement,
            authority.owner_signing_key(),
            payload,
        )?
    };
    Ok(ExternalChannelOutboundFrame {
        sequence: frame.frame().sequence,
        frame_digest: frame.digest().to_owned(),
        canonical_wire: frame.canonical().as_bytes().to_vec(),
    })
}

/// Request the Q guest's native scope stop only after the retained local
/// Worker process and connector have settled. This is a controller operation
/// under the same restored-occurrence exclusion as provider termination; the
/// runtime journal derives its completion coordinate from exact durable rows.
pub fn request_external_qualification_native_completion(
    state: &AppState,
    placement: &str,
) -> Result<ExternalChannelOutboundFrame> {
    use crate::runtime_db::external_execution::ExternalAllocationOwner;
    state
        .controller_lifetime
        .ensure_protects_app_root(&state.config.app_root)?;
    let allocation = state
        .state_store
        .external_allocation(placement)?
        .context("Q native completion has no retained allocation")?;
    let ExternalAllocationOwner::QualificationCandidate(owner) = &allocation.reservation.owner
    else {
        bail!("Q native completion requires a qualification candidate")
    };
    let operation_id = owner.qualification_operation_id.clone();
    crate::hosted_operation::with_qualification_occurrence_contact(&operation_id, || {
        let access = ExternalChannelAuthorityAccess::new(
            &allocation.reservation.channel_authority_generation,
        )?;
        let authority = access.decode(
            state
                .vault
                .external_channel_authority(&access)
                .context("read protected Q channel signer")?,
        )?;
        let binding = state.state_store.external_execution_channel(placement)?;
        ensure!(
            binding.owner_public_key == authority.owner_public_key()
                && binding.execution_binding_hash == allocation.reservation.binding_hash,
            "Q native completion changed protected channel authority"
        );
        let frame = state
            .state_store
            .ensure_external_qualification_complete(placement, authority.owner_signing_key())?;
        Ok(ExternalChannelOutboundFrame {
            sequence: frame.frame().sequence,
            frame_digest: frame.digest().to_owned(),
            canonical_wire: frame.canonical().as_bytes().to_vec(),
        })
    })
}

/// Reconcile only the exact signed Q scope observation and mark its local
/// no-effect application. Missing evidence remains pending; cancellation or
/// changed frame identities are errors, never normal settlement.
pub fn apply_external_qualification_native_settlement(
    state: &AppState,
    placement: &str,
) -> Result<Option<ExternalQualificationNativeSettlement>> {
    use crate::runtime_db::external_execution::ExternalAllocationOwner;
    use ryeos_state::external_execution::ExecutionChannelPayload;
    state
        .controller_lifetime
        .ensure_protects_app_root(&state.config.app_root)?;
    let allocation = state
        .state_store
        .external_allocation(placement)?
        .context("Q native settlement has no retained allocation")?;
    let ExternalAllocationOwner::QualificationCandidate(owner) = &allocation.reservation.owner
    else {
        bail!("Q native settlement requires a qualification candidate")
    };
    let operation_id = owner.qualification_operation_id.clone();
    crate::hosted_operation::with_qualification_occurrence_contact(&operation_id, || {
        let Some(retained) = state
            .state_store
            .apply_external_qualification_scope_settlement(placement)?
        else {
            return Ok(None);
        };
        let ExecutionChannelPayload::QualificationScopeSettled { observation } =
            &retained.settlement.frame().payload
        else {
            bail!("applied Q native settlement lost its signed observation")
        };
        ensure!(
            retained.binding.placement_thread_id == placement
                && retained.binding.execution_binding_hash == allocation.reservation.binding_hash,
            "applied Q native settlement changed its original allocation"
        );
        Ok(Some(ExternalQualificationNativeSettlement {
            placement_thread_id: placement.to_owned(),
            binding_digest: retained.binding.digest()?,
            completion_frame_digest: retained.completion.digest().to_owned(),
            runtime_applied_frame_digest: retained.runtime_applied.digest().to_owned(),
            protocol_eof_frame_digest: retained.protocol_eof.digest().to_owned(),
            settlement_frame_digest: retained.settlement.digest().to_owned(),
            observation: observation.observation.clone(),
        }))
    })
}

/// Finalize exactly one occurrence-scoped channel after bootstrap
/// authentication.  All mutable limits and identities are derived from the
/// retained session, allocation and signed binding rather than request data.
pub fn attach_external_execution_channel(
    state: &AppState,
    authenticated: &AuthenticatedExternalOccurrence,
    supervisor_public_key: &str,
) -> Result<ryeos_state::external_execution::ExecutionChannelBinding> {
    let controller_lifetime = Arc::clone(&state.controller_lifetime);
    controller_lifetime
        .ensure_protects_app_root(&state.config.app_root)
        .context("external channel controller lease has the wrong app root")?;
    ryeos_state::external_execution::validate_channel_public_key(supervisor_public_key)?;
    let allocation = state
        .state_store
        .external_allocation(&authenticated.placement_thread_id)?
        .context("external channel allocation disappeared before attachment")?;
    ensure!(
        allocation.phase == ExternalAllocationPhase::Bound
            && allocation.reservation.request_digest == authenticated.allocation_request_digest,
        "external channel authentication no longer names the bound allocation"
    );
    let occurrence = allocation
        .occurrence
        .as_ref()
        .context("bound external allocation lost its occurrence")?;
    ensure!(
        occurrence.occurrence_id == authenticated.occurrence_id,
        "external channel authentication no longer names the occurrence"
    );
    let retained = state
        .state_store
        .retained_external_binding(&allocation.reservation.binding_hash)?
        .context("external channel lost its retained binding generation")?;
    let program = retained_external_channel_program(state, &allocation.reservation, &retained)?;
    let contract = retained.backend_contract();
    if let Some(existing) = state
        .state_store
        .optional_external_execution_channel(&authenticated.placement_thread_id)?
    {
        ensure!(
            existing.supervisor_public_key == supervisor_public_key
                && existing.candidate_program_digest == program.digest()?
                && existing.execution_mode == program.execution_mode(),
            "external channel attachment replay changed its exact supervisor or program"
        );
        return Ok(existing);
    }
    let nonce = lillux::crypto::generate_random_bytes::<32>();
    let binding = build_external_execution_channel_binding(
        &allocation.reservation,
        occurrence,
        &program,
        &contract,
        supervisor_public_key,
        lillux::time::timestamp_millis(),
        lillux::sha256_hex(&nonce),
    )?;
    state
        .state_store
        .register_external_execution_channel(&binding)?;
    state
        .state_store
        .external_execution_channel(&authenticated.placement_thread_id)
}

/// Compare activation evidence to the resolved workload's exact runtime.
fn require_external_activation_runtime(
    program: &AdmittedExternalExecutionProgram,
    runtime_manifest_hash: &str,
) -> Result<()> {
    ensure!(
        program.runtime_manifest_hash()? == runtime_manifest_hash,
        "external channel activation changed its admitted program runtime"
    );
    Ok(())
}

/// Resolve the same retained owner/program for bootstrap authentication and
/// attachment. No request-selected runtime or current binding replaces it.
fn retained_external_channel_program(
    state: &AppState,
    reservation: &ExternalAllocationReservation,
    retained: &RetainedExternalExecutionBinding,
) -> Result<AdmittedExternalExecutionProgram> {
    let program = match &reservation.owner {
        ExternalAllocationOwner::DedicatedSession(_) => {
            let capsule = state
                .state_store
                .admitted_persistent_session_capsule(&reservation.admitted_capsule_hash)?;
            capsule.validate()?;
            let program = capsule
                .external_candidate
                .as_ref()
                .context("external channel capsule has no admitted candidate program")?;
            program.verify_runtime_authority(
                capsule.retained_product_selections.as_ref(),
                capsule
                    .retained_external_runtime_content_qualification
                    .as_ref(),
            )?;
            require_retained_session_runtime_qualification(state, &capsule, retained)?;
            retained.check_program(program)?;
            AdmittedExternalExecutionProgram::StructuredSession(program.clone())
        }
        ExternalAllocationOwner::DirectThread {
            chain_root_id,
            program,
            ..
        } => {
            let (stored_chain, capsule_hash, capsule) = state
                .state_store
                .admitted_launch_capsule_with_coordinates(&reservation.placement_thread_id)?
                .context("external direct channel lost its born-thread capsule")?;
            ensure!(
                &stored_chain == chain_root_id && capsule_hash == reservation.admitted_capsule_hash,
                "external direct channel changed its exact capsule coordinate"
            );
            let ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
                snapshot_hash,
                realization: ryeos_state::objects::PinnedProjectRealization::ReadOnly,
                environment: ryeos_state::objects::EnvironmentAuthority::None,
                ..
            } = &capsule.project_authority
            else {
                bail!("external direct channel requires its immutable pinned project");
            };
            ensure!(
                snapshot_hash == &reservation.base_snapshot_hash,
                "external direct channel changed its pinned snapshot"
            );
            let endpoint = crate::thread_lifecycle::validate_retained_external_direct_program(
                &capsule, program,
            )?;
            ensure!(
                endpoint.binding_digest == reservation.binding_hash,
                "external direct channel changed its sealed endpoint"
            );
            retained.check_direct_program(program)?;
            AdmittedExternalExecutionProgram::DirectCommand(program.clone())
        }
        ExternalAllocationOwner::QualificationCandidate(owner) => {
            let (stored_chain, capsule_hash, capsule) = state
                .state_store
                .admitted_launch_capsule_with_coordinates(&owner.coordinate.accepted_root_id)?
                .context("external qualification channel lost its accepted root capsule")?;
            ensure!(
                stored_chain == owner.coordinate.accepted_root_id
                    && capsule_hash == reservation.admitted_capsule_hash
                    && capsule_hash == owner.coordinate.accepted_capsule_hash,
                "external qualification channel changed its accepted root coordinate"
            );
            let ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
                snapshot_hash,
                realization: ryeos_state::objects::PinnedProjectRealization::ReadOnly,
                environment: ryeos_state::objects::EnvironmentAuthority::None,
                workspace_outputs: None,
                ..
            } = &capsule.project_authority
            else {
                bail!("external qualification channel requires its pinned read-only root snapshot");
            };
            ensure!(
                snapshot_hash == &reservation.base_snapshot_hash,
                "external qualification channel changed its accepted root snapshot"
            );
            let sealed =
                crate::thread_lifecycle::SealedRootExecutionRequest::decode_from_admitted_capsule(
                    &capsule,
                )?;
            let purpose = sealed
                .qualification_purpose()
                .context("external qualification channel root has no sealed purpose")?;
            owner.program.validate_against_purpose(purpose)?;
            retained.check_qualification_program(&owner.program, purpose)?;
            AdmittedExternalExecutionProgram::QualificationStructuredSession(owner.program.clone())
        }
    };
    let contract = retained.backend_contract();
    validate_external_placement_program(&program, reservation, &contract)?;
    Ok(program)
}

/// Read the exact original sealed bootstrap only for the retained Q relay
/// occurrence. Its guest descriptor numbers are historical import coordinates:
/// they must never be reopened to recreate guest inputs or retry activation.
/// This method performs no provider contact or relay reservation.
pub(crate) fn recover_original_external_relay_bootstrap(
    state: &AppState,
    placement: &str,
) -> Result<crate::vault::external_relay_bootstrap::ProtectedExternalRelayBootstrap> {
    recover_external_relay_bootstrap(state, placement, ExternalRelayOwnerLane::Qualification)
}

pub(crate) fn recover_ordinary_external_relay_bootstrap(
    state: &AppState,
    placement: &str,
) -> Result<crate::vault::external_relay_bootstrap::ProtectedExternalRelayBootstrap> {
    recover_external_relay_bootstrap(state, placement, ExternalRelayOwnerLane::OrdinarySession)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ExternalRelayOwnerLane {
    Qualification,
    OrdinarySession,
}

fn recover_external_relay_bootstrap(
    state: &AppState,
    placement: &str,
    lane: ExternalRelayOwnerLane,
) -> Result<crate::vault::external_relay_bootstrap::ProtectedExternalRelayBootstrap> {
    state
        .controller_lifetime
        .ensure_protects_app_root(&state.config.app_root)?;
    let allocation = state
        .state_store
        .external_allocation(placement)?
        .context("original relay bootstrap has no allocation")?;
    ensure!(
        allocation.phase == ExternalAllocationPhase::Bound
            && match lane {
                ExternalRelayOwnerLane::Qualification => matches!(
                    &allocation.reservation.owner,
                    ExternalAllocationOwner::QualificationCandidate(_)
                ),
                ExternalRelayOwnerLane::OrdinarySession => matches!(
                    &allocation.reservation.owner,
                    ExternalAllocationOwner::DedicatedSession(_)
                ),
            },
        "original relay bootstrap requires its exact live owner lane"
    );
    let occurrence = allocation
        .occurrence
        .as_ref()
        .context("original relay bootstrap lost its occurrence")?;
    let retained = state
        .state_store
        .retained_external_binding(&allocation.reservation.binding_hash)?
        .context("original relay bootstrap lost its signed binding")?;
    let contract = retained.backend_contract();
    contract.controller_transport.relay()?;
    ensure!(
        match lane {
            ExternalRelayOwnerLane::Qualification => matches!(
                &contract.workload,
                crate::node_config::sections::external_execution::ExternalWorkloadBinding::QualificationStructuredSession(_)
            ),
            ExternalRelayOwnerLane::OrdinarySession => matches!(
                &contract.workload,
                crate::node_config::sections::external_execution::ExternalWorkloadBinding::StructuredSession(_)
            ),
        },
        "original relay bootstrap binding changed its workload lane"
    );
    if lane == ExternalRelayOwnerLane::OrdinarySession {
        ensure!(
            state
                .node_config
                .external_execution
                .iter()
                .any(|installed| installed.digest() == retained.digest()),
            "ordinary relay recovery requires its exact currently installed signed binding"
        );
    }
    let program = retained_external_channel_program(state, &allocation.reservation, &retained)?;
    let activation = state
        .state_store
        .external_supervisor_activation(placement)?
        .context("original relay bootstrap has no activation intent")?;
    ensure!(
        activation
            .observation
            .as_ref()
            .is_none_or(|observation| observation.activation_state == "started"),
        "original relay bootstrap has supervisor non-start evidence"
    );
    let commitment = activation
        .relay_bootstrap
        .as_ref()
        .context("original relay bootstrap has no immutable activation commitment")?;
    commitment.validate_for(
        &allocation.reservation,
        occurrence,
        &activation.intent,
        &contract,
    )?;
    let authority_access =
        ExternalChannelAuthorityAccess::new(&allocation.reservation.channel_authority_generation)?;
    let channel_authority =
        authority_access.decode(state.vault.external_channel_authority(&authority_access)?)?;
    ensure!(
        channel_authority.owner_public_key() == allocation.reservation.channel_owner_public_key
            && channel_authority.bootstrap_capability_hash()
                == allocation.reservation.channel_bootstrap_capability_hash,
        "original relay bootstrap changed its protected channel authority"
    );
    let access =
        ExternalRelayBootstrapAccess::new(placement, &allocation.reservation.request_digest)?;
    let sealed = state.vault.external_relay_bootstrap(&access)?;
    let bootstrap = sealed.bootstrap();
    let signed_import = sealed.signed_import();
    let signed_import_bytes = zeroize::Zeroizing::new(
        ryeos_external_execution_contract::canonical_json(signed_import)?,
    );
    let owner_bytes: [u8; 32] = STANDARD
        .decode(&allocation.reservation.channel_owner_public_key)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("original relay owner key changed length"))?;
    let owner_key = lillux::crypto::VerifyingKey::from_bytes(&owner_bytes)?;
    let signature_bytes: [u8; 64] = hex::decode(&signed_import.signature_hex)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("original signed import signature changed length"))?;
    use lillux::crypto::Verifier as _;
    let signing_bytes = zeroize::Zeroizing::new(signed_import.authorization.signing_bytes()?);
    owner_key.verify(
        &signing_bytes,
        &lillux::crypto::Signature::from_bytes(&signature_bytes),
    )?;
    let ticket = &signed_import.authorization.ticket;
    ensure!(
        bootstrap.digest()? == commitment.bootstrap_sha256
            && ryeos_external_execution_contract::lease_ready_inputs::signed_import_sha256(
                &signed_import_bytes,
            ) == commitment.signed_import_sha256
            && bootstrap.placement_thread_id == allocation.reservation.placement_thread_id
            && bootstrap.occurrence_id == occurrence.occurrence_id
            && bootstrap.allocation_request_digest == allocation.reservation.request_digest
            && bootstrap.admitted_capsule_hash == allocation.reservation.admitted_capsule_hash
            && bootstrap.base_snapshot_hash == allocation.reservation.base_snapshot_hash
            && bootstrap.execution_binding_hash == allocation.reservation.binding_hash
            && bootstrap.supervisor_runtime_hash == activation.intent.supervisor_runtime_hash
            && bootstrap.launcher_artifact_hash == contract.launcher_artifact_hash
            && bootstrap.candidate_program == program
            && bootstrap.guest_input_identity == activation.intent.guest_input_identity
            && bootstrap.owner_public_key == allocation.reservation.channel_owner_public_key
            && lillux::sha256_hex(bootstrap.bootstrap_capability.as_bytes())
                == allocation.reservation.channel_bootstrap_capability_hash
            && bootstrap.controller == contract.controller_transport
            && bootstrap.tls_root_certificates_der_base64
                == contract.controller_tls_root_certificates_der_base64
            && bootstrap.attachment_deadline_ms == activation.intent.attachment_deadline_ms
            && bootstrap.execution_timeout_seconds == activation.intent.execution_timeout_seconds
            && bootstrap.post_execution_timeout_seconds
                == activation.intent.post_execution_timeout_seconds
            && bootstrap.channel_max_bytes == activation.intent.channel_max_bytes
            && bootstrap.candidate_export_max_bytes
                == match program.execution_mode() {
                    ryeos_external_execution_contract::ExternalExecutionMode::StructuredSession {} => {
                        contract.max_export_bytes.min(activation.intent.channel_max_bytes)
                    }
                    ryeos_external_execution_contract::ExternalExecutionMode::QualificationStructuredSession {} => 0,
                    ryeos_external_execution_contract::ExternalExecutionMode::DirectCommand { .. } => {
                        bail!("direct command cannot recover a structured-session relay bootstrap")
                    }
                },
        "sealed original relay bootstrap contradicts its signed activation"
    );
    ensure!(
        signed_import.authorization.activation_request_digest
            == activation.intent.activation_request_digest
            && signed_import.authorization.guest_runtime_manifest_hash
                == contract.guest_runtime_manifest_hash
            && ticket.payload_sha256 == activation.intent.delivery.payload_sha256
            && ticket.manifest_sha256 == activation.intent.delivery.manifest_sha256
            && ticket.framed_bytes == activation.intent.delivery.framed_bytes
            && ticket.regular_bytes == activation.intent.delivery.regular_bytes
            && ticket.supervisor_sha256 == contract.supervisor_artifact_hash
            && ticket.launcher_sha256 == contract.launcher_artifact_hash,
        "original signed guest import changed durable package delivery"
    );
    Ok(sealed)
}

/// A 404 from the signed file GET is only a pending transport observation.
/// The retained read claim has still consumed one attempt and its deadline;
/// no channel application, absent-file proof, or replacement PUT follows.
#[derive(Debug)]
pub(crate) enum ExternalRelayFileContactProgress {
    Accepted(crate::runtime_db::external_execution::RelayTransferRecord),
    PendingHttpNotFound(crate::runtime_db::external_execution::RelayTransferRecord),
}

/// Read-only transport choice for the exact retained Q activation. `None`
/// means activation has not yet retained an intent; it is not an HTTP choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalQualificationControllerTransport {
    Http,
    Relay {
        poll_interval_ms: u32,
        poll_deadline_ms: u32,
        maximum_frames: u32,
    },
}

/// Read-only transport choice for the exact retained ordinary session. Relay
/// parameters are returned only after its signed schema-19 binding and schema-9
/// bootstrap commitment have been rejoined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalSessionControllerTransport {
    Http,
    Relay {
        poll_interval_ms: u32,
        poll_deadline_ms: u32,
        maximum_frames: u32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalSessionRelayStep {
    Attach,
    Exchange { sequence: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExternalSessionRelayPending {
    pub read_attempts: u32,
    pub deadline_ms: i64,
    pub poll_interval_ms: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalSessionRelayStepProgress {
    GuestRequestPending(ExternalSessionRelayPending),
    ControllerResponsePending(ExternalSessionRelayPending),
    ControllerResponseAccepted,
}

pub fn inspect_external_session_controller_transport(
    state: &AppState,
    placement: &str,
) -> Result<Option<ExternalSessionControllerTransport>> {
    let Some(allocation) = state.state_store.external_allocation(placement)? else {
        return Ok(None);
    };
    ensure!(
        allocation.phase == ExternalAllocationPhase::Bound
            && matches!(
                &allocation.reservation.owner,
                ExternalAllocationOwner::DedicatedSession(_)
            ),
        "ordinary session transport inspection requires its bound dedicated-session owner"
    );
    let retained = state
        .state_store
        .retained_external_binding(&allocation.reservation.binding_hash)?
        .context("ordinary session transport lost its retained signed binding")?;
    ensure!(
        state
            .node_config
            .external_execution
            .iter()
            .any(|installed| installed.digest() == retained.digest()),
        "ordinary session transport requires its exact currently installed signed binding"
    );
    let contract = retained.backend_contract();
    contract.workload.structured_session()?;
    let Some(activation) = state
        .state_store
        .external_supervisor_activation(placement)?
    else {
        return Ok(None);
    };
    let occurrence = allocation
        .occurrence
        .as_ref()
        .context("ordinary session transport lost its exact occurrence")?;
    activation
        .intent
        .validate_contract(&allocation.reservation, occurrence, &contract)?;
    ensure!(
        activation
            .observation
            .as_ref()
            .is_none_or(|observation| observation.activation_state == "started"),
        "ordinary session transport has supervisor non-start evidence"
    );
    match &contract.controller_transport {
        ryeos_state::external_execution::transport::ExternalControllerTransportSelection::HttpJson(_) => {
            ensure!(
                activation.relay_bootstrap.is_none(),
                "ordinary HTTP activation changed relay bootstrap custody"
            );
            Ok(Some(ExternalSessionControllerTransport::Http))
        }
        ryeos_state::external_execution::transport::ExternalControllerTransportSelection::ControllerFileRelay(relay) => {
            recover_ordinary_external_relay_bootstrap(state, placement)?;
            ensure!(
                activation.relay_bootstrap.is_some(),
                "ordinary relay activation has no original bootstrap commitment"
            );
            Ok(Some(ExternalSessionControllerTransport::Relay {
                poll_interval_ms: relay.poll_interval_ms,
                poll_deadline_ms: relay.poll_deadline_ms,
                maximum_frames: relay.maximum_frames,
            }))
        }
    }
}

pub fn advance_external_session_relay_step(
    state: &AppState,
    placement: &str,
    step: ExternalSessionRelayStep,
) -> Result<ExternalSessionRelayStepProgress> {
    use crate::runtime_db::external_execution::{RelayDirection, RelayKind, RelayTransferPhase};
    let protected = recover_ordinary_external_relay_bootstrap(state, placement)?;
    let poll_interval_ms = protected.bootstrap().controller.relay()?.poll_interval_ms;
    let (kind, sequence) = match step {
        ExternalSessionRelayStep::Attach => (RelayKind::Attach, 0),
        ExternalSessionRelayStep::Exchange { sequence } => (RelayKind::Exchange, sequence),
    };
    match contact_ordinary_external_relay_transfer(
        state,
        placement,
        kind,
        sequence,
        RelayDirection::GuestToController,
        None,
    )? {
        ExternalRelayFileContactProgress::PendingHttpNotFound(record) => {
            ensure!(
                record.phase == RelayTransferPhase::Reading,
                "ordinary relay missing request lost its bounded read claim"
            );
            Ok(ExternalSessionRelayStepProgress::GuestRequestPending(
                ExternalSessionRelayPending {
                    read_attempts: record.read_attempts,
                    deadline_ms: record.intent.deadline_ms,
                    poll_interval_ms,
                },
            ))
        }
        ExternalRelayFileContactProgress::Accepted(record) => {
            ensure!(
                record.phase == RelayTransferPhase::Accepted,
                "ordinary relay accepted request has an invalid durable phase"
            );
            match apply_and_contact_ordinary_external_relay_request(
                state, placement, kind, sequence,
            )? {
                ExternalRelayFileContactProgress::PendingHttpNotFound(response) => {
                    ensure!(
                        response.phase == RelayTransferPhase::UploadPossible,
                        "ordinary relay response ambiguity lost its one-shot upload claim"
                    );
                    Ok(ExternalSessionRelayStepProgress::ControllerResponsePending(
                        ExternalSessionRelayPending {
                            read_attempts: response.read_attempts,
                            deadline_ms: response.intent.deadline_ms,
                            poll_interval_ms,
                        },
                    ))
                }
                ExternalRelayFileContactProgress::Accepted(response) => {
                    ensure!(
                        response.phase == RelayTransferPhase::Accepted,
                        "ordinary relay accepted response has an invalid durable phase"
                    );
                    Ok(ExternalSessionRelayStepProgress::ControllerResponseAccepted)
                }
            }
        }
    }
}

/// Inspect only the original bound Q occurrence and its currently installed
/// signed transport. Relay parameters are returned only after the activation
/// commitment rejoins the sealed bootstrap; this creates no contact permit.
pub fn inspect_external_qualification_controller_transport(
    state: &AppState,
    placement: &str,
) -> Result<Option<ExternalQualificationControllerTransport>> {
    use ryeos_state::external_execution::transport::ExternalControllerTransportSelection;

    state
        .controller_lifetime
        .ensure_protects_app_root(&state.config.app_root)?;
    let allocation = state
        .state_store
        .external_allocation(placement)?
        .context("Q transport selection has no original allocation")?;
    ensure!(
        matches!(
            &allocation.reservation.owner,
            ExternalAllocationOwner::QualificationCandidate(_)
        ),
        "Q transport selection requires its original qualification owner"
    );
    ensure!(
        matches!(
            allocation.phase,
            ExternalAllocationPhase::Reserved
                | ExternalAllocationPhase::ContactPending
                | ExternalAllocationPhase::Bound
        ),
        "Q transport selection has no live allocation"
    );
    if allocation.phase != ExternalAllocationPhase::Bound {
        return Ok(None);
    }
    let Some(activation) = state
        .state_store
        .external_supervisor_activation(placement)?
    else {
        return Ok(None);
    };
    ensure!(
        activation
            .observation
            .as_ref()
            .is_none_or(|observation| observation.activation_state == "started"),
        "Q transport selection has supervisor non-start evidence"
    );
    let retained = state
        .state_store
        .retained_external_binding(&allocation.reservation.binding_hash)?
        .context("Q transport selection lost its signed binding")?;
    ensure!(
        state
            .node_config
            .external_execution
            .iter()
            .any(|installed| installed.digest() == retained.digest()),
        "Q transport selection lost its currently installed signed binding"
    );
    match &retained.backend_contract().controller_transport {
        ExternalControllerTransportSelection::HttpJson(_) => {
            ensure!(
                activation.relay_bootstrap.is_none(),
                "HTTP Q activation changed relay bootstrap custody"
            );
            Ok(Some(ExternalQualificationControllerTransport::Http))
        }
        ExternalControllerTransportSelection::ControllerFileRelay(_) => {
            ensure!(
                activation.relay_bootstrap.is_some(),
                "relay Q activation has no original bootstrap commitment"
            );
            let sealed = recover_original_external_relay_bootstrap(state, placement)?;
            let relay = sealed.bootstrap().controller.relay()?;
            Ok(Some(ExternalQualificationControllerTransport::Relay {
                poll_interval_ms: relay.poll_interval_ms,
                poll_deadline_ms: relay.poll_deadline_ms,
                maximum_frames: relay.maximum_frames,
            }))
        }
    }
}

/// The only guest file coordinate a Q relay caller may ask the controller to
/// advance. Paths, bytes, deadlines and limits come from the sealed bootstrap
/// and durable journal, never from this request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalQualificationRelayStep {
    Attach,
    Exchange { sequence: u32 },
}

/// A missing exact file consumes one of the journal's bounded GET attempts.
/// The caller must use the signed poll interval rather than its own loop delay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExternalQualificationRelayPending {
    pub read_attempts: u32,
    pub deadline_ms: i64,
    pub poll_interval_ms: u32,
}

/// One bounded slot advancement may do a guest GET, semantic application,
/// controller PUT and readback GET. These are transport states, not Ready,
/// ACK, or native/provider settlement evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalQualificationRelayStepProgress {
    GuestRequestPending(ExternalQualificationRelayPending),
    ControllerResponsePending(ExternalQualificationRelayPending),
    ControllerResponseAccepted,
}

/// Advance exactly one Q relay slot. The caller owns polling pace and the
/// process-local Q lifeline. The file contact itself takes the per-placement
/// gate and rechecks live authority in its journal transaction. This method
/// cannot take the occurrence mutex: a local Worker may hold it while waiting
/// for the guest exchange that this method must transport. An exact 404 only
/// yields Pending; all other contact errors remain errors. The current signed
/// Render profile keeps this unreachable from a live Q loop.
pub fn advance_external_qualification_relay_step(
    state: &AppState,
    placement: &str,
    step: ExternalQualificationRelayStep,
) -> Result<ExternalQualificationRelayStepProgress> {
    use crate::runtime_db::external_execution::{RelayDirection, RelayKind, RelayTransferPhase};

    state
        .controller_lifetime
        .ensure_protects_app_root(&state.config.app_root)?;
    // This rejoins the live Q owner, original activation and sealed signed
    // relay selection before any contact. The contact path repeats those DB
    // checks after it obtains the per-placement gate.
    let original = recover_original_external_relay_bootstrap(state, placement)?;
    let poll_interval_ms = original.bootstrap().controller.relay()?.poll_interval_ms;
    let (kind, sequence) = match step {
        ExternalQualificationRelayStep::Attach => (RelayKind::Attach, 0),
        ExternalQualificationRelayStep::Exchange { sequence } => (RelayKind::Exchange, sequence),
    };
    match contact_original_external_relay_transfer(
        state,
        placement,
        kind,
        sequence,
        RelayDirection::GuestToController,
        None,
    )? {
        ExternalRelayFileContactProgress::PendingHttpNotFound(record) => {
            ensure!(
                record.phase == RelayTransferPhase::Reading,
                "Q relay missing request lost its bounded read claim"
            );
            Ok(ExternalQualificationRelayStepProgress::GuestRequestPending(
                ExternalQualificationRelayPending {
                    read_attempts: record.read_attempts,
                    deadline_ms: record.intent.deadline_ms,
                    poll_interval_ms,
                },
            ))
        }
        ExternalRelayFileContactProgress::Accepted(record) => {
            ensure!(
                record.phase == RelayTransferPhase::Accepted,
                "Q relay guest request has no exact accepted bytes"
            );
            match apply_and_contact_original_external_relay_request(
                state, placement, kind, sequence,
            )? {
                ExternalRelayFileContactProgress::PendingHttpNotFound(response) => {
                    ensure!(
                        response.phase == RelayTransferPhase::UploadPossible,
                        "Q relay response ambiguity lost its one-shot upload claim"
                    );
                    Ok(
                        ExternalQualificationRelayStepProgress::ControllerResponsePending(
                            ExternalQualificationRelayPending {
                                read_attempts: response.read_attempts,
                                deadline_ms: response.intent.deadline_ms,
                                poll_interval_ms,
                            },
                        ),
                    )
                }
                ExternalRelayFileContactProgress::Accepted(response) => {
                    ensure!(
                        response.phase == RelayTransferPhase::Accepted,
                        "Q relay response has no exact transport readback"
                    );
                    Ok(ExternalQualificationRelayStepProgress::ControllerResponseAccepted)
                }
            }
        }
    }
}

/// Apply one accepted guest file at the existing channel boundary, then
/// reserve and contact only the exact owner-signed committed response. No
/// operator Q loop calls this yet. An indeterminate application is deliberately
/// refused: restart fences the Q occurrence and proceeds to cleanup, rather
/// than reapplying a frame whose first transaction may already have committed.
pub(crate) fn apply_and_contact_original_external_relay_request(
    state: &AppState,
    placement: &str,
    kind: crate::runtime_db::external_execution::RelayKind,
    sequence: u32,
) -> Result<ExternalRelayFileContactProgress> {
    apply_and_contact_external_relay_request(
        state,
        placement,
        kind,
        sequence,
        ExternalRelayOwnerLane::Qualification,
    )
}

pub(crate) fn apply_and_contact_ordinary_external_relay_request(
    state: &AppState,
    placement: &str,
    kind: crate::runtime_db::external_execution::RelayKind,
    sequence: u32,
) -> Result<ExternalRelayFileContactProgress> {
    apply_and_contact_external_relay_request(
        state,
        placement,
        kind,
        sequence,
        ExternalRelayOwnerLane::OrdinarySession,
    )
}

fn apply_and_contact_external_relay_request(
    state: &AppState,
    placement: &str,
    kind: crate::runtime_db::external_execution::RelayKind,
    sequence: u32,
    lane: ExternalRelayOwnerLane,
) -> Result<ExternalRelayFileContactProgress> {
    use crate::runtime_db::external_execution::{
        RelayApplicationDecision, RelayDirection, RelayTransferIntent,
    };
    use ryeos_state::external_execution::transport::{
        EXTERNAL_CHANNEL_TRANSPORT_SCHEMA, ExternalChannelAttachRequest,
        ExternalChannelAttachResponse, ExternalChannelExchangeRequest,
        ExternalChannelExchangeResponse, ExternalChannelResponseFrame,
        ExternalRelayResponseEnvelope, ExternalRelayResponseKind,
    };

    let protected = recover_external_relay_bootstrap(state, placement, lane)?;
    let bootstrap = protected.bootstrap();
    let intent = RelayTransferIntent::from_bootstrap(
        bootstrap,
        kind,
        sequence,
        RelayDirection::GuestToController,
        None,
    )?;
    let accepted = state.state_store.accepted_external_relay_request(&intent)?;
    let request_bytes = accepted
        .canonical_bytes
        .context("accepted external relay request lost its exact bytes")?;
    let request_digest = accepted
        .observed_sha256
        .context("accepted external relay request lost its digest")?;
    ensure!(
        lillux::sha256_hex(&request_bytes) == request_digest,
        "accepted external relay request changed its retained digest"
    );

    // Parse and validate canonical guest syntax before taking the one-use
    // semantic claim. Time-sensitive authentication happens only for a new
    // application; committed recovery uses its exact retained response.
    let attach = if kind == crate::runtime_db::external_execution::RelayKind::Attach {
        let request: ExternalChannelAttachRequest = serde_json::from_slice(&request_bytes)?;
        request.validate_for_bootstrap(bootstrap)?;
        ensure!(
            request.canonical_bytes()? == request_bytes,
            "external relay attach request is noncanonical"
        );
        Some(request)
    } else {
        None
    };
    let exchange = if kind == crate::runtime_db::external_execution::RelayKind::Exchange {
        let request: ExternalChannelExchangeRequest = serde_json::from_slice(&request_bytes)?;
        request.validate_shape()?;
        ensure!(
            request.placement_thread_id == bootstrap.placement_thread_id
                && request.occurrence_id == bootstrap.occurrence_id
                && lillux::canonical_json(&serde_json::to_value(&request)?)?.as_bytes()
                    == request_bytes,
            "external relay exchange request changed its sealed occurrence"
        );
        let wire = request.decode_frame()?;
        Some((request, wire))
    } else {
        None
    };
    let decision = state
        .state_store
        .begin_external_relay_application(bootstrap, kind, sequence)?;
    let envelope = match decision {
        RelayApplicationDecision::Committed { response_bytes } => {
            serde_json::from_slice::<ExternalRelayResponseEnvelope>(&response_bytes)?
        }
        RelayApplicationDecision::Apply {
            request_digest: claimed_digest,
        } => {
            ensure!(
                claimed_digest == request_digest,
                "external relay semantic claim changed accepted bytes"
            );
            let allocation = state
                .state_store
                .external_allocation(placement)?
                .context("external relay semantic application lost its allocation")?;
            let access = ExternalChannelAuthorityAccess::new(
                &allocation.reservation.channel_authority_generation,
            )?;
            let authority = access.decode(state.vault.external_channel_authority(&access)?)?;
            ensure!(
                authority.owner_public_key() == bootstrap.owner_public_key,
                "external relay semantic signer changed its sealed owner"
            );
            let envelope = if let Some(request) = attach {
                let authenticated = authenticate_external_channel_bootstrap(
                    state,
                    placement,
                    &bootstrap.occurrence_id,
                    &request.bootstrap_capability,
                )?;
                let binding = attach_external_execution_channel(
                    state,
                    &authenticated,
                    &request.supervisor_public_key,
                )?;
                let response = ExternalChannelAttachResponse {
                    schema: EXTERNAL_CHANNEL_TRANSPORT_SCHEMA,
                    binding_digest: binding.digest()?,
                    binding,
                };
                ExternalRelayResponseEnvelope::sign(
                    bootstrap,
                    ExternalRelayResponseKind::Attach,
                    &request_digest,
                    None,
                    &response,
                    authority.owner_signing_key(),
                )?
            } else if let Some((_request, wire)) = exchange {
                let authenticated = authenticate_external_channel_frame(
                    state,
                    placement,
                    &bootstrap.occurrence_id,
                    &wire,
                )?;
                ensure!(
                    authenticated.sequence() == u64::from(sequence),
                    "external relay file sequence differs from its signed frame"
                );
                let binding_digest = state
                    .state_store
                    .external_execution_channel(placement)?
                    .digest()?;
                let result = exchange_external_channel_frame(state, &authenticated, &wire)?;
                let outbound_frames = result
                    .outbound()
                    .iter()
                    .map(|frame| {
                        ExternalChannelResponseFrame::new(
                            frame.sequence(),
                            frame.frame_digest().to_owned(),
                            frame.canonical_wire(),
                        )
                    })
                    .collect::<Result<Vec<_>>>()?;
                let urgent_revocation_frame = result
                    .urgent_revocation()
                    .map(|frame| {
                        ExternalChannelResponseFrame::new(
                            frame.sequence(),
                            frame.frame_digest().to_owned(),
                            frame.canonical_wire(),
                        )
                    })
                    .transpose()?;
                let response = ExternalChannelExchangeResponse {
                    schema: EXTERNAL_CHANNEL_TRANSPORT_SCHEMA,
                    incoming_new: result.incoming_new(),
                    incoming_sequence: authenticated.sequence(),
                    incoming_frame_digest: authenticated.frame_digest().to_owned(),
                    acknowledgement_frame_digest: result
                        .acknowledgement_digest()
                        .map(str::to_owned),
                    outbound_frames,
                    urgent_revocation_frame,
                };
                response.validate_shape()?;
                ExternalRelayResponseEnvelope::sign(
                    bootstrap,
                    ExternalRelayResponseKind::Exchange,
                    &request_digest,
                    Some(&binding_digest),
                    &response,
                    authority.owner_signing_key(),
                )?
            } else {
                bail!("external relay semantic application has no typed request")
            };
            state
                .state_store
                .commit_external_relay_application(bootstrap, kind, sequence, &envelope)?;
            envelope
        }
    };
    contact_external_relay_transfer(
        state,
        placement,
        kind,
        sequence,
        RelayDirection::ControllerToGuest,
        Some(&envelope),
        lane,
    )
}

/// Contact only an immutable file slot derived from the protected original
/// bootstrap. This records transport bytes, never Ready, ACK, or settlement.
/// The outbound reservation itself requires a committed semantic application.
pub(crate) fn contact_original_external_relay_transfer(
    state: &AppState,
    placement: &str,
    kind: crate::runtime_db::external_execution::RelayKind,
    sequence: u32,
    direction: crate::runtime_db::external_execution::RelayDirection,
    outbound: Option<&ryeos_state::external_execution::transport::ExternalRelayResponseEnvelope>,
) -> Result<ExternalRelayFileContactProgress> {
    contact_external_relay_transfer(
        state,
        placement,
        kind,
        sequence,
        direction,
        outbound,
        ExternalRelayOwnerLane::Qualification,
    )
}

pub(crate) fn contact_ordinary_external_relay_transfer(
    state: &AppState,
    placement: &str,
    kind: crate::runtime_db::external_execution::RelayKind,
    sequence: u32,
    direction: crate::runtime_db::external_execution::RelayDirection,
    outbound: Option<&ryeos_state::external_execution::transport::ExternalRelayResponseEnvelope>,
) -> Result<ExternalRelayFileContactProgress> {
    contact_external_relay_transfer(
        state,
        placement,
        kind,
        sequence,
        direction,
        outbound,
        ExternalRelayOwnerLane::OrdinarySession,
    )
}

fn contact_external_relay_transfer(
    state: &AppState,
    placement: &str,
    kind: crate::runtime_db::external_execution::RelayKind,
    sequence: u32,
    direction: crate::runtime_db::external_execution::RelayDirection,
    outbound: Option<&ryeos_state::external_execution::transport::ExternalRelayResponseEnvelope>,
    lane: ExternalRelayOwnerLane,
) -> Result<ExternalRelayFileContactProgress> {
    use crate::runtime_db::external_execution::{
        RelayDirection, RelayTransferIntent, RelayTransferPhase,
    };
    use ryeos_state::external_execution::transport::{
        ExternalRelayResponseEnvelope, ExternalRelayResponseKind,
    };

    state
        .controller_lifetime
        .ensure_protects_app_root(&state.config.app_root)?;
    let gate = state.external_placement_backends.contact_gate(placement)?;
    ensure!(
        gate.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok(),
        "external relay file contact is already in progress"
    );
    let _lease = ExternalContactLease { gate };
    let protected = recover_external_relay_bootstrap(state, placement, lane)?;
    let bootstrap = protected.bootstrap();
    let outbound_bytes = match direction {
        RelayDirection::GuestToController => {
            ensure!(
                outbound.is_none(),
                "guest relay read cannot carry controller response"
            );
            None
        }
        RelayDirection::ControllerToGuest => {
            let bytes = if let Some(envelope) = outbound {
                envelope.canonical_bytes()?
            } else {
                // Recovery after a possible PUT uses only the journal's
                // precommitted bytes. The caller cannot replace the slot.
                state
                    .state_store
                    .retained_external_relay_outbound(bootstrap, kind, sequence)?
            };
            let envelope: ExternalRelayResponseEnvelope = serde_json::from_slice(&bytes)?;
            ensure!(
                envelope.canonical_bytes()? == bytes,
                "retained relay response envelope is noncanonical"
            );
            let predecessor = RelayTransferIntent::from_bootstrap(
                bootstrap,
                kind,
                sequence,
                RelayDirection::GuestToController,
                None,
            )?;
            let accepted = state
                .state_store
                .accepted_external_relay_request(&predecessor)?;
            let request_digest = accepted
                .observed_sha256
                .as_deref()
                .context("accepted relay request has no exact digest")?;
            let (response_kind, binding_digest) = match kind {
                crate::runtime_db::external_execution::RelayKind::Attach => {
                    (ExternalRelayResponseKind::Attach, None)
                }
                crate::runtime_db::external_execution::RelayKind::Exchange => (
                    ExternalRelayResponseKind::Exchange,
                    Some(
                        state
                            .state_store
                            .external_execution_channel(placement)?
                            .digest()?,
                    ),
                ),
            };
            let _: serde_json::Value = ExternalRelayResponseEnvelope::decode_verified(
                &bytes,
                bootstrap,
                response_kind,
                request_digest,
                binding_digest.as_deref(),
            )?;
            Some(bytes)
        }
    };
    let intent = RelayTransferIntent::from_bootstrap(
        bootstrap,
        kind,
        sequence,
        direction,
        outbound_bytes.as_deref(),
    )?;
    let allocation = state
        .state_store
        .external_allocation(placement)?
        .context("external relay lost its exact allocation")?;
    let retained = state
        .state_store
        .retained_external_binding(&allocation.reservation.binding_hash)?
        .context("external relay lost its exact signed binding")?;
    let contract = retained.backend_contract();
    let program = retained_external_channel_program(state, &allocation.reservation, &retained)?;
    validate_external_placement_program(&program, &allocation.reservation, &contract)?;
    ensure!(
        state
            .node_config
            .external_execution
            .iter()
            .any(|installed| installed.digest() == retained.digest()),
        "live relay contact requires its exact currently installed signed binding"
    );
    let credential_access = retained.credential_access()?;
    let credential =
        credential_access.decode(state.vault.placement_credential(&credential_access)?)?;
    // Live file contact requires the current signed relay selection; terminal
    // cleanup uses a separate recovery path after this permission is withdrawn.
    let backend = match lane {
        ExternalRelayOwnerLane::Qualification => state
            .external_placement_backends
            .qualify_for_relay(&contract, &credential, &program)?,
        ExternalRelayOwnerLane::OrdinarySession => state
            .external_placement_backends
            .qualify_for_ordinary_relay(&contract, &credential, &program)?,
    };
    let reserved = state.state_store.reserve_external_relay_transfer(
        bootstrap,
        &intent,
        outbound_bytes.as_deref(),
    )?;
    if reserved.phase == RelayTransferPhase::Accepted {
        return Ok(ExternalRelayFileContactProgress::Accepted(reserved));
    }
    ensure!(
        matches!(
            reserved.phase,
            RelayTransferPhase::Reserved
                | RelayTransferPhase::Reading
                | RelayTransferPhase::UploadPossible
        ),
        "external relay slot is no longer contactable"
    );
    let remaining_ms = intent
        .deadline_ms
        .saturating_sub(lillux::time::timestamp_millis());
    ensure!(
        remaining_ms > 0,
        "external relay deadline expired before contact"
    );
    let deadline = lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_millis(
        u64::try_from(remaining_ms)?,
    ))
    .min(lillux::time::MonotonicDeadline::after(
        lillux::time::Duration::from_secs(u64::from(contract.contact_timeout_seconds)),
    ));

    if direction == RelayDirection::ControllerToGuest
        && reserved.phase == RelayTransferPhase::Reserved
    {
        let claim = state
            .state_store
            .claim_external_relay_upload_contact(&intent)?;
        // Even a transport error might follow a completed PUT. Never mint a
        // second upload; only the same-slot bounded GET can reconcile it.
        let _upload_observation =
            backend.relay_file_contact(&contract, &credential, bootstrap, claim, deadline);
    }
    let claim = state
        .state_store
        .claim_external_relay_read_contact(&intent)?;
    let claimed_record = claim.record().clone();
    let observed =
        backend.relay_file_contact(&contract, &credential, bootstrap, claim, deadline)?;
    ensure!(
        !observed.deadline_exceeded,
        "external relay download finished after its contact deadline"
    );
    match observed.value {
        ryeos_external_execution_contract::relay_file::RelayFileAdapterResponse::DownloadHttpNotFound { .. } => {
            // For an outbound UploadPossible slot, 404 does not prove the
            // previous PUT failed. A later attempt can only GET this slot.
            Ok(ExternalRelayFileContactProgress::PendingHttpNotFound(claimed_record))
        }
        ryeos_external_execution_contract::relay_file::RelayFileAdapterResponse::Downloaded {
            bytes_base64,
            ..
        } => {
            let bytes = STANDARD.decode(bytes_base64)?;
            Ok(ExternalRelayFileContactProgress::Accepted(
                state.state_store.accept_external_relay_readback(&intent, &bytes)?,
            ))
        }
        _ => bail!("external relay read returned a non-read transport observation"),
    }
}

fn build_external_execution_channel_binding(
    reservation: &ExternalAllocationReservation,
    occurrence: &ExternalAllocationOccurrence,
    program: &AdmittedExternalExecutionProgram,
    contract: &ExternalPlacementBackendContract,
    supervisor_public_key: &str,
    issued_at_ms: i64,
    channel_nonce: String,
) -> Result<ryeos_state::external_execution::ExecutionChannelBinding> {
    ensure!(
        occurrence.request_digest == reservation.request_digest
            && occurrence.binding_hash == reservation.binding_hash,
        "external channel occurrence changed its reserved authority"
    );
    ryeos_state::external_execution::validate_channel_public_key(supervisor_public_key)?;
    validate_external_placement_program(program, reservation, contract)?;
    let execution_deadline_ms = issued_at_ms
        .checked_add(i64::from(reservation.timeout_seconds) * 1_000)
        .context("external channel execution deadline overflow")?;
    let expires_at_ms = execution_deadline_ms
        .checked_add(
            i64::from(
                contract
                    .observation_timeout_seconds
                    .saturating_add(contract.cleanup_timeout_seconds),
            ) * 1_000,
        )
        .context("external channel expiry overflow")?;
    let max_bytes = contract.max_transfer_bytes.min(64 * 1024 * 1024);
    let candidate_export_max_bytes = contract.max_export_bytes.min(max_bytes);
    let max_frames = u32::try_from(
        max_bytes
            .div_ceil(ryeos_state::external_execution::MAX_CHUNK_BYTES as u64)
            .saturating_mul(4)
            .clamp(64, 65_536),
    )?;
    let binding = ryeos_state::external_execution::ExecutionChannelBinding {
        schema: ryeos_state::external_execution::EXECUTION_CHANNEL_BINDING_SCHEMA,
        execution_mode: program.execution_mode(),
        placement_thread_id: reservation.placement_thread_id.clone(),
        allocation_request_digest: reservation.request_digest.clone(),
        occurrence_id: occurrence.occurrence_id.clone(),
        admitted_capsule_hash: reservation.admitted_capsule_hash.clone(),
        base_snapshot_hash: reservation.base_snapshot_hash.clone(),
        execution_binding_hash: reservation.binding_hash.clone(),
        supervisor_runtime_hash: program.runtime_manifest_hash()?.to_owned(),
        candidate_program_digest: program.digest()?,
        channel_nonce,
        owner_public_key: reservation.channel_owner_public_key.clone(),
        supervisor_public_key: supervisor_public_key.to_owned(),
        issued_at_ms,
        execution_deadline_ms,
        expires_at_ms,
        candidate_export_max_bytes,
        max_frames,
        max_bytes,
    };
    binding.validate()?;
    Ok(binding)
}

/// Mechanical workload join shared by channel construction and activation.
/// The placement owner separately verifies the exact retained signed binding
/// and the live session/born-thread claim before any provider contact.
fn validate_external_placement_program(
    program: &AdmittedExternalExecutionProgram,
    reservation: &ExternalAllocationReservation,
    contract: &ExternalPlacementBackendContract,
) -> Result<()> {
    program.validate()?;
    match program {
        AdmittedExternalExecutionProgram::StructuredSession(program) => {
            ensure!(
                matches!(
                    reservation.owner,
                    ExternalAllocationOwner::DedicatedSession(_)
                ),
                "external structured session requires its dedicated-session owner"
            );
            let session = contract.workload.structured_session()?;
            ensure!(
                program.runtime_manifest_hash == session.runtime_manifest_hash
                    && program.selection_identity_digest == session.runtime_selection_identity,
                "external supervisor program contradicts its protected lifecycle binding"
            );
        }
        AdmittedExternalExecutionProgram::QualificationStructuredSession(program) => {
            let ExternalAllocationOwner::QualificationCandidate(owner) = &reservation.owner else {
                bail!("qualification-only session has no qualification allocation owner");
            };
            let selected = contract.workload.qualification_structured_session()?;
            ensure!(
                program == &owner.program
                    && contract.max_export_bytes == 0
                    && contract.runtime_content_qualification.is_none()
                    && program.requirement.provider_declaration_id
                        == selected.provider_declaration_id
                    && program.requirement.provider_configuration_destination
                        == selected.provider_configuration_destination
                    && program.requirement.connector_protocol == selected.connector_protocol
                    && program.requirement.qualification_requirement_digest()?
                        == selected.requirement_digest
                    && program.qualification_use.profile_hash == selected.worker_profile_hash
                    && program.runtime_manifest_hash == selected.runtime_manifest_hash
                    && program.runtime_member.executable_sha256 == selected.runtime_member_sha256
                    && reservation.timeout_seconds <= contract.timeout_seconds,
                "qualification-only program changed its owner or signed zero-export placement"
            );
        }
        AdmittedExternalExecutionProgram::DirectCommand(program) => {
            let ExternalAllocationOwner::DirectThread {
                program: retained_program,
                ..
            } = &reservation.owner
            else {
                bail!("external direct command requires its born-thread owner");
            };
            ensure!(
                retained_program == program,
                "external direct command changed its retained owner program"
            );
            ensure!(matches!(contract.workload,
                crate::node_config::sections::external_execution::ExternalWorkloadBinding::DirectCommand {})
                    && contract.max_export_bytes == 0,
                "external direct program requires a direct workload without candidate export");
            let projection = program.projection();
            ensure!(
                projection.endpoint_binding_digest == reservation.binding_hash,
                "external direct program changed its reserved endpoint binding"
            );
            ensure!(
                projection.timeout_seconds == u64::from(reservation.timeout_seconds)
                    && reservation.timeout_seconds <= contract.timeout_seconds,
                "external direct program changed its exact reserved execution budget"
            );
        }
    }
    Ok(())
}

pub(crate) struct ExternalPlacementOwner<'a> {
    state: &'a AppState,
}

impl<'a> ExternalPlacementOwner<'a> {
    pub(crate) fn new(state: &'a AppState) -> Self {
        Self { state }
    }

    /// Reopen only the retained Q allocation and its exact signed binding for
    /// this process-local start owner. This cannot reserve or claim contact;
    /// the caller supplies a fresh accepted-root proof to the Q-specific claim.
    fn prepare_retained_qualification_for_start(
        &self,
        placement: &str,
    ) -> Result<PreparedExternalPlacement> {
        let controller_lifetime = Arc::clone(&self.state.controller_lifetime);
        controller_lifetime.ensure_protects_app_root(&self.state.config.app_root)?;
        let record = self
            .state
            .state_store
            .external_allocation(placement)?
            .context("external qualification start lost its retained allocation")?;
        ensure!(
            matches!(
                &record.reservation.owner,
                ExternalAllocationOwner::QualificationCandidate(_)
            ),
            "external qualification start cannot reopen another allocation owner"
        );
        let binding = self
            .state
            .state_store
            .retained_external_binding(&record.reservation.binding_hash)?
            .context("external qualification start lost its retained binding")?;
        let program = retained_external_channel_program(self.state, &record.reservation, &binding)?;
        let contract = binding.backend_contract();
        record
            .reservation
            .validate_startup_budget(qualification_startup_budget_ms(&contract)?)?;
        let access = binding.credential_access()?;
        let credential = access.decode(self.state.vault.placement_credential(&access)?)?;
        let backend =
            self.state
                .external_placement_backends
                .qualify(&contract, &credential, &program)?;
        let authority_access =
            ExternalChannelAuthorityAccess::new(&record.reservation.channel_authority_generation)?;
        let channel_authority = authority_access.decode(
            self.state
                .vault
                .external_channel_authority(&authority_access)?,
        )?;
        ensure!(
            channel_authority.owner_public_key() == record.reservation.channel_owner_public_key
                && channel_authority.bootstrap_capability_hash()
                    == record.reservation.channel_bootstrap_capability_hash,
            "external qualification start changed its retained channel authority"
        );
        Ok(PreparedExternalPlacement {
            state_store: self.state.state_store.clone(),
            vault: self.state.vault.clone(),
            backend,
            contract,
            credential,
            contact_gate: self
                .state
                .external_placement_backends
                .contact_gate(placement)?,
            controller_lifetime,
            channel_authority,
            program,
            guest_inputs: None,
            qualification_activation: None,
            runtime_upload: None,
            native_journal:
                crate::external_supervisor_native::SupervisorJournalEnvelopeSource::Policy(
                    Arc::clone(&self.state.node_policy),
                ),
            record,
        })
    }

    /// Recover only the original qualification candidate's observation or
    /// cleanup obligation. This path cannot create the first provider contact;
    /// that requires the accepted-root proof and the Q-specific journal CAS.
    fn prepare_retained_qualification_for_cleanup(
        &self,
        placement: &str,
    ) -> Result<PreparedExternalPlacement> {
        let controller_lifetime = Arc::clone(&self.state.controller_lifetime);
        controller_lifetime.ensure_protects_app_root(&self.state.config.app_root)?;
        let record = self
            .state
            .state_store
            .external_allocation(placement)?
            .context("external qualification cleanup has no retained allocation")?;
        ensure!(
            matches!(
                &record.reservation.owner,
                ExternalAllocationOwner::QualificationCandidate(_)
            ),
            "external qualification cleanup cannot recover another allocation owner"
        );
        let binding = self
            .state
            .state_store
            .retained_external_binding(&record.reservation.binding_hash)?
            .context("external qualification cleanup lost its retained binding")?;
        let program = retained_external_channel_program(self.state, &record.reservation, &binding)?;
        let contract = binding.backend_contract();
        let access = binding.credential_access()?;
        let credential = access.decode(self.state.vault.placement_credential(&access)?)?;
        let backend = self.state.external_placement_backends.qualify_for_cleanup(
            &contract,
            &credential,
            &program,
        )?;
        let authority_access =
            ExternalChannelAuthorityAccess::new(&record.reservation.channel_authority_generation)?;
        let channel_authority = authority_access.decode(
            self.state
                .vault
                .external_channel_authority(&authority_access)?,
        )?;
        ensure!(
            channel_authority.owner_public_key() == record.reservation.channel_owner_public_key
                && channel_authority.bootstrap_capability_hash()
                    == record.reservation.channel_bootstrap_capability_hash,
            "external qualification cleanup lost its exact channel authority"
        );
        let record = self
            .state
            .state_store
            .reserve_external_allocation(&record.reservation, &binding)?;
        Ok(PreparedExternalPlacement {
            state_store: self.state.state_store.clone(),
            vault: self.state.vault.clone(),
            backend,
            contract,
            credential,
            contact_gate: self
                .state
                .external_placement_backends
                .contact_gate(placement)?,
            controller_lifetime,
            channel_authority,
            program,
            guest_inputs: None,
            qualification_activation: None,
            runtime_upload: None,
            native_journal:
                crate::external_supervisor_native::SupervisorJournalEnvelopeSource::Policy(
                    Arc::clone(&self.state.node_policy),
                ),
            record,
        })
    }

    /// Cleanup of a placed session's allocation works from its retained
    /// reservation alone. The obligation outlives the session's worker and
    /// launch claim: after a controller restart both are gone, and the lease
    /// must still be terminated by its occurrence id.
    fn prepare_retained_session_for_cleanup(
        &self,
        placement: &str,
    ) -> Result<PreparedExternalPlacement> {
        let controller_lifetime = Arc::clone(&self.state.controller_lifetime);
        controller_lifetime.ensure_protects_app_root(&self.state.config.app_root)?;
        let record = self
            .state
            .state_store
            .external_allocation(placement)?
            .context("external session cleanup has no retained allocation")?;
        ensure!(
            matches!(
                &record.reservation.owner,
                ExternalAllocationOwner::DedicatedSession(_)
            ),
            "external session cleanup cannot recover another allocation owner"
        );
        let binding = self
            .state
            .state_store
            .retained_external_binding(&record.reservation.binding_hash)?
            .context("external session cleanup lost its retained binding")?;
        let capsule = self
            .state
            .state_store
            .admitted_persistent_session_capsule(&record.reservation.admitted_capsule_hash)?;
        capsule.validate()?;
        let program = capsule
            .external_candidate
            .as_ref()
            .context("external session cleanup capsule has no admitted candidate program")?;
        program.verify_runtime_authority(
            capsule.retained_product_selections.as_ref(),
            capsule
                .retained_external_runtime_content_qualification
                .as_ref(),
        )?;
        binding.check_program(program)?;
        let program = AdmittedExternalExecutionProgram::StructuredSession(program.clone());
        let contract = binding.backend_contract();
        let credential = read_placement_credential(self.state, &binding.credential_access()?)?;
        let backend = self.state.external_placement_backends.qualify_for_cleanup(
            &contract,
            &credential,
            &program,
        )?;
        let authority_access =
            ExternalChannelAuthorityAccess::new(&record.reservation.channel_authority_generation)?;
        let channel_authority = authority_access.decode(
            self.state
                .vault
                .external_channel_authority(&authority_access)
                .context("read protected external cleanup channel authority")?,
        )?;
        ensure!(
            channel_authority.owner_public_key() == record.reservation.channel_owner_public_key
                && channel_authority.bootstrap_capability_hash()
                    == record.reservation.channel_bootstrap_capability_hash,
            "external session cleanup lost its exact channel authority"
        );
        let record = self
            .state
            .state_store
            .reserve_external_allocation(&record.reservation, &binding)?;
        Ok(PreparedExternalPlacement {
            state_store: self.state.state_store.clone(),
            vault: self.state.vault.clone(),
            backend,
            contract,
            credential,
            contact_gate: self
                .state
                .external_placement_backends
                .contact_gate(placement)?,
            controller_lifetime,
            channel_authority,
            program,
            guest_inputs: None,
            qualification_activation: None,
            runtime_upload: None,
            native_journal:
                crate::external_supervisor_native::SupervisorJournalEnvelopeSource::Policy(
                    Arc::clone(&self.state.node_policy),
                ),
            record,
        })
    }

    /// Reentry uses the original journal owner and program, never a replacement
    /// compiler claim or reconstructed descriptor inventory.
    fn prepare_retained_direct(&self, placement: &str) -> Result<PreparedExternalPlacement> {
        self.prepare_retained_direct_for_purpose(placement, false)
    }

    fn prepare_retained_direct_for_cleanup(
        &self,
        placement: &str,
    ) -> Result<PreparedExternalPlacement> {
        self.prepare_retained_direct_for_purpose(placement, true)
    }

    fn prepare_retained_direct_for_purpose(
        &self,
        placement: &str,
        cleanup_only: bool,
    ) -> Result<PreparedExternalPlacement> {
        let controller_lifetime = Arc::clone(&self.state.controller_lifetime);
        controller_lifetime.ensure_protects_app_root(&self.state.config.app_root)?;
        let record = self
            .state
            .state_store
            .external_allocation(placement)?
            .context("external direct startup has no retained allocation")?;
        let ExternalAllocationOwner::DirectThread { program, .. } = &record.reservation.owner
        else {
            bail!("external direct startup cannot recover a session allocation");
        };
        let binding = self
            .state
            .state_store
            .retained_external_binding(&record.reservation.binding_hash)?
            .context("external direct startup lost its retained binding")?;
        binding.check_direct_program(program)?;
        if record.phase == ExternalAllocationPhase::Reserved {
            let installed = self
                .state
                .node_config
                .external_execution
                .iter()
                .find(|binding| binding.id() == program.projection().endpoint_binding_id)
                .context("external direct endpoint is no longer installed")?;
            ensure!(
                installed.digest() == binding.digest(),
                "external direct endpoint changed before first contact"
            );
        }
        let contract = binding.backend_contract();
        record
            .reservation
            .validate_startup_budget(contract.direct_startup_budget_ms()?)?;
        if !cleanup_only && record.phase == ExternalAllocationPhase::Reserved {
            require_direct_feature_support_selection(&contract, program)?;
            require_current_runtime_qualification_for_start(
                self.state,
                &contract,
                binding.digest(),
                None,
            )?;
        }
        let program = AdmittedExternalExecutionProgram::DirectCommand(program.clone());
        let access = binding.credential_access()?;
        let credential = access.decode(self.state.vault.placement_credential(&access)?)?;
        let backend = if cleanup_only {
            self.state.external_placement_backends.qualify_for_cleanup(
                &contract,
                &credential,
                &program,
            )?
        } else {
            self.state
                .external_placement_backends
                .qualify(&contract, &credential, &program)?
        };
        let authority_access =
            ExternalChannelAuthorityAccess::new(&record.reservation.channel_authority_generation)?;
        let channel_authority = authority_access.decode(
            self.state
                .vault
                .external_channel_authority(&authority_access)?,
        )?;
        ensure!(
            channel_authority.owner_public_key() == record.reservation.channel_owner_public_key
                && channel_authority.bootstrap_capability_hash()
                    == record.reservation.channel_bootstrap_capability_hash,
            "external direct startup lost its exact channel authority"
        );
        // Reserved replay rechecks the real born owner; contacted replay keeps
        // the original immutable obligation even if its launch claim is stale.
        let record = self
            .state
            .state_store
            .reserve_external_allocation(&record.reservation, &binding)?;
        Ok(PreparedExternalPlacement {
            state_store: self.state.state_store.clone(),
            vault: self.state.vault.clone(),
            backend,
            contract,
            credential,
            contact_gate: self
                .state
                .external_placement_backends
                .contact_gate(placement)?,
            controller_lifetime,
            channel_authority,
            program,
            guest_inputs: None,
            qualification_activation: None,
            runtime_upload: None,
            native_journal:
                crate::external_supervisor_native::SupervisorJournalEnvelopeSource::Policy(
                    Arc::clone(&self.state.node_policy),
                ),
            record,
        })
    }

    /// Join compiler evidence to the real born launch before any contact.
    /// Startup shares the signed contact-plus-observation horizon; recovery
    /// preserves the original timestamps and cannot renew execution authority.
    pub(crate) fn prepare_direct(
        &self,
        compiled: crate::thread_lifecycle::CompiledExternalDirectProgram,
    ) -> Result<PreparedExternalPlacement> {
        let placement = compiled.thread_id();
        let controller_lifetime = Arc::clone(&self.state.controller_lifetime);
        controller_lifetime.ensure_protects_app_root(&self.state.config.app_root)?;
        let capsule = self
            .state
            .state_store
            .admitted_launch_capsule(placement)?
            .context("external direct placement has no born capsule")?;
        let (thread, _, _) = self
            .state
            .state_store
            .get_authoritative_thread_snapshot_with_last_event(compiled.chain_root_id(), placement)?
            .context("external direct placement has no born thread")?;
        let capsule_hash = capsule.content_hash()?;
        ensure!(
            capsule_hash == compiled.capsule_hash()
                && thread.admitted_launch_capsule_hash.as_deref() == Some(capsule_hash.as_str()),
            "external direct compiler changed its born capsule"
        );
        let program = compiled.program().clone();
        let endpoint =
            crate::thread_lifecycle::validate_retained_external_direct_program(&capsule, &program)?;
        let generic_program = AdmittedExternalExecutionProgram::DirectCommand(program.clone());
        let ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
            snapshot_hash,
            realization: ryeos_state::objects::PinnedProjectRealization::ReadOnly,
            environment: ryeos_state::objects::EnvironmentAuthority::None,
            workspace_outputs: None,
            ..
        } = &capsule.project_authority
        else {
            bail!("external direct placement requires immutable read-only project authority");
        };
        ensure!(
            thread.project_authority == capsule.project_authority,
            "external direct capsule changed the born project authority"
        );
        let existing = self.state.state_store.external_allocation(placement)?;
        let fresh = existing
            .as_ref()
            .is_none_or(|record| record.phase == ExternalAllocationPhase::Reserved);
        let binding = if fresh {
            let claim = self
                .state
                .state_store
                .get_launch_claim(placement)?
                .context("external direct placement has no current launch claim")?;
            ensure!(
                &claim.owner == compiled.launch_owner(),
                "external direct launch claim changed"
            );
            let installed = self
                .state
                .node_config
                .external_execution
                .iter()
                .find(|binding| binding.id() == endpoint.binding_id)
                .context("external direct endpoint is no longer installed")?;
            ensure!(
                installed.digest() == endpoint.binding_digest,
                "external direct endpoint generation changed before first contact"
            );
            installed.retained_generation()?
        } else {
            let record = existing.as_ref().unwrap();
            ensure!(
                !record.phase.is_settled(),
                "settled external direct placement has no startup authority"
            );
            self.state
                .state_store
                .retained_external_binding(&record.reservation.binding_hash)?
                .context("external direct placement lost its retained binding")?
        };
        binding.check_direct_program(&program)?;
        let contract = binding.backend_contract();
        if fresh {
            require_direct_feature_support_selection(&contract, &program)?;
            require_current_runtime_qualification_for_start(
                self.state,
                &contract,
                binding.digest(),
                None,
            )?;
        }
        let access = binding.credential_access()?;
        let credential = access.decode(self.state.vault.placement_credential(&access)?)?;
        let backend = self.state.external_placement_backends.qualify(
            &contract,
            &credential,
            &generic_program,
        )?;
        if existing.is_none() {
            require_guest_package_artifact_floor(&contract, backend.as_ref())?;
            // Fund the exact execution timeout the reservation below carries
            // and the supervisor activation later checks.
            crate::external_supervisor_native::external_supervisor_journal_quota(
                self.state.node_policy.as_ref(),
                &contract,
                u32::try_from(program.projection().timeout_seconds)?,
            )?;
        }
        let authority_generation = ryeos_state::objects::canonical_value_digest(
            &serde_json::json!({
                "domain":"ryeos.external-direct-channel-authority.v1", "placement_thread_id":placement,
                "admitted_capsule_hash":capsule_hash, "base_snapshot_hash":snapshot_hash,
                "binding_hash":binding.digest(), "program":program,
            }),
        )?;
        let authority_access = ExternalChannelAuthorityAccess::new(&authority_generation)?;
        let channel_authority = authority_access.decode(if existing.is_some() {
            self.state
                .vault
                .external_channel_authority(&authority_access)?
        } else {
            self.state
                .vault
                .ensure_external_channel_authority(&authority_access)?
        })?;
        let owner = if !fresh {
            let prior = &existing.as_ref().unwrap().reservation.owner;
            ensure!(
                matches!(prior, ExternalAllocationOwner::DirectThread { program: retained, .. } if retained == &program),
                "external direct recovery changed its retained program"
            );
            prior.clone()
        } else {
            ExternalAllocationOwner::DirectThread {
                chain_root_id: compiled.chain_root_id().to_owned(),
                launch_owner: compiled.launch_owner().clone(),
                program: program.clone(),
            }
        };
        // No adapter admission precedes this placement kind, so it retains no
        // admitted adapter request; the request digest still names the field.
        let admitted_adapter_request_base64: Option<String> = None;
        let request_digest = ryeos_state::objects::canonical_value_digest(&serde_json::json!({
            "domain":"ryeos.external-direct-placement-request.v1", "placement_thread_id":placement,
            "admitted_capsule_hash":capsule_hash, "base_snapshot_hash":snapshot_hash,
            "binding_hash":binding.digest(), "owner":owner, "backend_contract":contract,
            "channel_authority_generation":authority_generation,
            "channel_owner_public_key":channel_authority.owner_public_key(),
            "channel_bootstrap_capability_hash":channel_authority.bootstrap_capability_hash(),
            "admitted_adapter_request_base64":admitted_adapter_request_base64,
        }))?;
        let startup_budget = contract.direct_startup_budget_ms()?;
        let (started, contact_deadline, startup_deadline) = if let Some(prior) = &existing {
            prior.reservation.validate_startup_budget(startup_budget)?;
            (
                prior.reservation.startup_started_at_ms,
                prior.reservation.contact_deadline_ms,
                prior.reservation.startup_deadline_ms,
            )
        } else {
            let now = lillux::time::timestamp_millis();
            (
                now,
                now.checked_add(i64::from(contract.contact_timeout_seconds) * 1000)
                    .context("external direct contact deadline overflow")?,
                now.checked_add(i64::try_from(startup_budget)?)
                    .context("external direct startup deadline overflow")?,
            )
        };
        let reservation = ExternalAllocationReservation {
            schema: EXTERNAL_ALLOCATION_RESERVATION_SCHEMA,
            placement_thread_id: placement.to_owned(),
            admitted_capsule_hash: capsule_hash,
            owner,
            base_snapshot_hash: snapshot_hash.clone(),
            binding_hash: binding.digest().to_owned(),
            capacity_owner: binding.capacity_owner().to_owned(),
            channel_authority_generation: authority_generation,
            channel_owner_public_key: channel_authority.owner_public_key(),
            channel_bootstrap_capability_hash: channel_authority.bootstrap_capability_hash(),
            admitted_adapter_request_base64,
            request_digest,
            max_active: contract.max_active,
            timeout_seconds: u32::try_from(program.projection().timeout_seconds)?,
            contact_deadline_ms: contact_deadline,
            startup_started_at_ms: started,
            startup_deadline_ms: startup_deadline,
        };
        if let Some(prior) = &existing {
            ensure!(
                prior.reservation == reservation,
                "external direct recovery changed its reservation"
            );
        }
        let contact_gate = self
            .state
            .external_placement_backends
            .contact_gate(placement)?;
        let record = if fresh {
            self.state.state_store.reserve_external_direct_allocation(
                &reservation,
                &binding,
                compiled,
            )?
        } else {
            self.state
                .state_store
                .reserve_external_allocation(&reservation, &binding)?
        };
        Ok(PreparedExternalPlacement {
            state_store: self.state.state_store.clone(),
            vault: self.state.vault.clone(),
            backend,
            contract,
            credential,
            contact_gate,
            controller_lifetime,
            channel_authority,
            program: generic_program,
            guest_inputs: None,
            qualification_activation: None,
            runtime_upload: None,
            native_journal:
                crate::external_supervisor_native::SupervisorJournalEnvelopeSource::Policy(
                    Arc::clone(&self.state.node_policy),
                ),
            record,
        })
    }

    /// Prepare or exactly recover one placement. The placement id is an
    /// already-born dedicated session; no caller-authored allocation shape is
    /// accepted at this boundary.
    pub(crate) fn prepare(&self, placement: &str) -> Result<PreparedExternalPlacement> {
        let controller_lifetime = Arc::clone(&self.state.controller_lifetime);
        controller_lifetime
            .ensure_protects_app_root(&self.state.config.app_root)
            .context("external placement controller lease has the wrong app root")?;
        let session = self
            .state
            .state_store
            .dedicated_session(placement)?
            .context("external placement has no dedicated-session owner")?;
        let (thread, _, _) = self
            .state
            .state_store
            .get_authoritative_thread_snapshot_with_last_event(&session.chain_root_id, placement)?
            .context("external placement has no authoritative born thread")?;
        let launch_capsule = self
            .state
            .state_store
            .admitted_launch_capsule(placement)?
            .context("external placement thread has no admitted launch capsule")?;
        let ryeos_state::objects::AdmittedExecutionClosure::ManagedRuntime {
            prepared_runtime_launch,
            ..
        } = &launch_capsule.execution_closure
        else {
            bail!("external placement requires a managed runtime launch closure");
        };
        let retained_sessions = prepared_runtime_launch
            .get("admitted_sessions")
            .and_then(serde_json::Value::as_object)
            .context("external placement launch closure has no admitted session inventory")?;
        let launch_capsule_hash = launch_capsule.content_hash()?;
        ensure!(
            thread.thread_id == placement
                && thread.chain_root_id == session.chain_root_id
                && thread.admitted_launch_capsule_hash.as_deref()
                    == Some(launch_capsule_hash.as_str())
                && retained_sessions
                    .values()
                    .any(|hash| { hash.as_str() == Some(session.admitted_capsule_hash.as_str()) }),
            "external placement session contradicts its authoritative born thread"
        );
        let worker_instance_id = session
            .worker_instance_id
            .as_deref()
            .context("external placement session has no worker owner")?;
        let worker_boot_epoch = session
            .worker_boot_epoch
            .context("external placement session has no worker epoch")?;
        let capsule = self
            .state
            .state_store
            .admitted_persistent_session_capsule(&session.admitted_capsule_hash)?;
        capsule.validate()?;
        let program = capsule
            .external_candidate
            .as_ref()
            .context("dedicated session did not admit external candidate execution")?;
        program.verify_runtime_authority(
            capsule.retained_product_selections.as_ref(),
            capsule
                .retained_external_runtime_content_qualification
                .as_ref(),
        )?;
        let workspace = self
            .state
            .state_store
            .execution_workspace(&session.workspace_id)?
            .context("external placement workspace disappeared")?;
        let launch_claim = self
            .state
            .state_store
            .get_launch_claim(placement)?
            .context("external placement has no current launch owner")?;
        ensure!(
            workspace.workspace_id == session.workspace_id
                && workspace.thread_id.as_deref() == Some(placement)
                && workspace.launch_owner.as_deref() == Some(launch_claim.claimed_by.as_str())
                && thread.base_project_snapshot_hash.as_deref()
                    == Some(workspace.base_snapshot.as_str()),
            "external placement workspace is not the exact session owner"
        );
        let existing = self.state.state_store.external_allocation(placement)?;
        if let Some(existing) = &existing {
            lease_lifecycle::refuse_ended_session_lease(
                self.state,
                existing,
                &session.workspace_id,
                worker_instance_id,
                worker_boot_epoch,
            )?;
        }

        let (binding, contract, credential_access) = if let Some(existing) = &existing {
            let binding = self
                .state
                .state_store
                .retained_external_binding(&existing.reservation.binding_hash)?
                .context("external placement lost its retained binding generation")?;
            binding.check_program(program)?;
            if existing.phase == ExternalAllocationPhase::Reserved {
                require_fresh_contact_owner(&session, &workspace)?;
                require_current_contact_binding(
                    &self.state.node_config.external_execution,
                    program,
                    binding.digest(),
                )?;
            }
            let contract = binding.backend_contract();
            let access = binding.credential_access()?;
            (binding, contract, access)
        } else {
            require_fresh_contact_owner(&session, &workspace)?;
            let installed = select_binding(&self.state.node_config.external_execution, program)?;
            let binding = installed.retained_generation()?;
            let contract = installed.backend_contract();
            let access = installed.credential_access()?;
            (binding, contract, access)
        };
        require_retained_session_runtime_qualification(self.state, &capsule, &binding)?;
        if existing
            .as_ref()
            .is_none_or(|record| record.phase == ExternalAllocationPhase::Reserved)
        {
            require_current_runtime_qualification_for_start(
                self.state,
                &contract,
                binding.digest(),
                Some(program),
            )?;
        }
        // A recovered placement carries the lease request its first
        // reservation admitted. A fresh one whose profile names lease or
        // guest requirements is first measured against the lease it would
        // get, its deadline plus the node's turn-end margin against now plus
        // the lease lifetime, and then asks the binding's adapter, offline,
        // before the placement credential is read: a refused lease leaves no
        // vault read, reservation, channel authority or provider contact
        // behind.
        let (admitted_adapter_request_base64, lease_charge, credential) = match &existing {
            Some(existing) => (
                existing.reservation.admitted_adapter_request_base64.clone(),
                None,
                read_placement_credential(self.state, &credential_access)?,
            ),
            None => {
                let (lease, credential) = admit_fresh_lease_then_read_credential(
                    self.state,
                    capsule
                        .structured_session_profile
                        .as_ref()
                        .map(|profile| &profile.contract),
                    capsule.lifecycle.ready_timeout_ms,
                    || {
                        crate::dedicated_session_service::ensure_hosted_contact_deadline(
                            self.state, &session,
                        )
                    },
                    &contract,
                    &credential_access,
                )?;
                match lease {
                    Some((admitted, charge)) => (Some(admitted), Some(charge), credential),
                    None => (None, None, credential),
                }
            }
        };
        let admitted = AdmittedExternalExecutionProgram::StructuredSession(program.clone());
        let backend =
            self.state
                .external_placement_backends
                .qualify(&contract, &credential, &admitted)?;
        if existing.is_none() {
            require_guest_package_artifact_floor(&contract, backend.as_ref())?;
            crate::external_supervisor_native::external_supervisor_journal_quota(
                self.state.node_policy.as_ref(),
                &contract,
                contract.timeout_seconds,
            )?;
        }
        let contact_gate = self
            .state
            .external_placement_backends
            .contact_gate(placement)?;
        let authority_generation =
            ryeos_state::objects::canonical_value_digest(&serde_json::json!({
                "domain":"ryeos.external-channel-authority.v1",
                "placement_thread_id":placement,
                "admitted_capsule_hash":session.admitted_capsule_hash,
                "base_snapshot_hash":workspace.base_snapshot,
                "binding_hash":binding.digest(),
            }))?;
        let authority_access = ExternalChannelAuthorityAccess::new(&authority_generation)?;
        let channel_authority = if let Some(existing) = &existing {
            ensure!(
                existing.reservation.channel_authority_generation == authority_generation,
                "external placement recovery changed its channel authority generation"
            );
            authority_access.decode(
                self.state
                    .vault
                    .external_channel_authority(&authority_access)
                    .context("read protected external channel authority")?,
            )?
        } else {
            authority_access.decode(
                self.state
                    .vault
                    .ensure_external_channel_authority(&authority_access)
                    .context("seal protected external channel authority")?,
            )?
        };
        let channel_owner_public_key = channel_authority.owner_public_key();
        let channel_bootstrap_capability_hash = channel_authority.bootstrap_capability_hash();
        let request_digest = ryeos_state::objects::canonical_value_digest(&serde_json::json!({
            "domain":"ryeos.external-placement-request.v1",
            "placement_thread_id":placement,
            "admitted_capsule_hash":session.admitted_capsule_hash,
            "workspace_id":session.workspace_id,
            "worker_instance_id":worker_instance_id,
            "worker_boot_epoch":worker_boot_epoch,
            "base_snapshot_hash":workspace.base_snapshot,
            "binding_hash":binding.digest(),
            "capacity_owner":binding.capacity_owner(),
            "channel_authority_generation":authority_generation,
            "channel_owner_public_key":channel_owner_public_key,
            "channel_bootstrap_capability_hash":channel_bootstrap_capability_hash,
            "admitted_adapter_request_base64":admitted_adapter_request_base64,
            "program":program,
            "backend_contract":contract,
        }))?;
        let reservation = if let Some(existing) = existing {
            existing
                .reservation
                .validate_startup_budget(capsule.lifecycle.ready_timeout_ms)?;
            let expected = ExternalAllocationReservation {
                schema: EXTERNAL_ALLOCATION_RESERVATION_SCHEMA,
                placement_thread_id: placement.to_owned(),
                admitted_capsule_hash: session.admitted_capsule_hash.clone(),
                owner: ExternalAllocationOwner::DedicatedSession(ExternalDedicatedSessionOwner {
                    workspace_id: session.workspace_id.clone(),
                    worker_instance_id: worker_instance_id.to_owned(),
                    worker_boot_epoch,
                }),
                base_snapshot_hash: workspace.base_snapshot.clone(),
                binding_hash: binding.digest().to_owned(),
                capacity_owner: binding.capacity_owner().to_owned(),
                channel_authority_generation: authority_generation.clone(),
                channel_owner_public_key: channel_owner_public_key.clone(),
                channel_bootstrap_capability_hash: channel_bootstrap_capability_hash.clone(),
                admitted_adapter_request_base64,
                request_digest,
                max_active: contract.max_active,
                timeout_seconds: contract.timeout_seconds,
                contact_deadline_ms: existing.reservation.contact_deadline_ms,
                startup_started_at_ms: existing.reservation.startup_started_at_ms,
                startup_deadline_ms: existing.reservation.startup_deadline_ms,
            };
            ensure!(
                existing.reservation == expected,
                "external placement recovery contradicts its exact reservation"
            );
            expected
        } else {
            let now = lillux::time::timestamp_millis();
            let contact_deadline_ms = now
                .checked_add(i64::from(contract.contact_timeout_seconds) * 1_000)
                .context("external placement contact deadline overflow")?;
            ExternalAllocationReservation {
                schema: EXTERNAL_ALLOCATION_RESERVATION_SCHEMA,
                placement_thread_id: placement.to_owned(),
                admitted_capsule_hash: session.admitted_capsule_hash.clone(),
                owner: ExternalAllocationOwner::DedicatedSession(ExternalDedicatedSessionOwner {
                    workspace_id: session.workspace_id.clone(),
                    worker_instance_id: worker_instance_id.to_owned(),
                    worker_boot_epoch,
                }),
                base_snapshot_hash: workspace.base_snapshot.clone(),
                binding_hash: binding.digest().to_owned(),
                capacity_owner: binding.capacity_owner().to_owned(),
                channel_authority_generation: authority_generation,
                channel_owner_public_key,
                channel_bootstrap_capability_hash,
                admitted_adapter_request_base64,
                request_digest,
                max_active: contract.max_active,
                timeout_seconds: contract.timeout_seconds,
                contact_deadline_ms,
                startup_started_at_ms: now,
                startup_deadline_ms: now
                    .checked_add(i64::try_from(capsule.lifecycle.ready_timeout_ms)?)
                    .context("external placement startup deadline overflow")?,
            }
        };
        let record = match &lease_charge {
            Some(charge) => self.state.state_store.reserve_external_lease_allocation(
                &reservation,
                &binding,
                charge,
            )?,
            None => self
                .state
                .state_store
                .reserve_external_allocation(&reservation, &binding)?,
        };
        Ok(PreparedExternalPlacement {
            state_store: self.state.state_store.clone(),
            vault: self.state.vault.clone(),
            backend,
            contract,
            credential,
            contact_gate,
            controller_lifetime,
            channel_authority,
            program: AdmittedExternalExecutionProgram::StructuredSession(program.clone()),
            guest_inputs: None,
            qualification_activation: None,
            runtime_upload: None,
            native_journal:
                crate::external_supervisor_native::SupervisorJournalEnvelopeSource::Policy(
                    Arc::clone(&self.state.node_policy),
                ),
            record,
        })
    }
}

/// A qualification startup funds new supervisor journals until its supervisor
/// activation is retained; recovering a retained activation keeps the envelope
/// sealed at activation.
fn qualification_start_journal_funding(
    phase: ExternalAllocationPhase,
    activation_exists: bool,
) -> crate::external_supervisor_native::SupervisorJournalFunding {
    if phase == ExternalAllocationPhase::Bound && activation_exists {
        crate::external_supervisor_native::SupervisorJournalFunding::Retained
    } else {
        crate::external_supervisor_native::SupervisorJournalFunding::New
    }
}

fn qualification_startup_budget_ms(contract: &ExternalPlacementBackendContract) -> Result<u64> {
    contract.workload.qualification_structured_session()?;
    let budget = u64::from(contract.contact_timeout_seconds)
        .checked_add(u64::from(contract.observation_timeout_seconds))
        .and_then(|seconds| seconds.checked_mul(1_000))
        .context("external qualification startup budget overflow")?;
    ensure!(
        (1..=600_000).contains(&budget),
        "external qualification startup exceeds the reservation budget"
    );
    Ok(budget)
}

/// One process-local qualification startup owner. The accepted root and its
/// original descriptor lifelines remain together after the durable reservation.
/// First contact freshly authenticates the root, signed generation, exact
/// program and live occurrence before using StateStore's Q-specific claim.
pub struct ExternalQualificationStart<'a> {
    state: &'a AppState,
    invocation: crate::external_qualification_invocation::QualificationInvocation,
    coordinate: ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeVerificationCoordinate,
    qualification_operation_id: String,
    limits: ryeos_state::object_closure::ObjectClosureLimits,
    placement: String,
    deadline: lillux::time::MonotonicDeadline,
    root: crate::operator_external_content::product_qualification::PreparedExternalQualificationRoot,
    guest_inputs: Option<ryeos_external_execution::guest_inputs::ExternalGuestInputAuthority>,
    // Original descriptors AND their same materialization holds survive the
    // one-use activation transfer, for the controller exclusion check at Ready.
    original_guest_exclusion: ryeos_external_execution::guest_inputs::ExternalGuestInputExclusionWitness,
    cleanup: Option<ExternalCandidateCleanupLifeline>,
    failed: bool,
}

impl ExternalQualificationStart<'_> {
    pub fn placement(&self) -> &str {
        &self.placement
    }

    pub fn deadline(&self) -> lillux::time::MonotonicDeadline {
        self.deadline
    }

    /// Carry the authenticated accepted root into the local Worker owner after
    /// startup reaches Ready. This root is retained identity, not a new contact
    /// permit; the local plan and effectful spawn must recheck current authority.
    pub fn into_authenticated_ready_parts(
        self,
    ) -> (
        crate::operator_external_content::product_qualification::PreparedExternalQualificationRoot,
        ryeos_external_execution::guest_inputs::ExternalGuestInputExclusionWitness,
    ) {
        (self.root, self.original_guest_exclusion)
    }

    /// Move the original cleanup owner to the next process-local owner. If it
    /// is never transferred, dropping this start requests durable cleanup.
    pub fn take_cleanup_lifeline(&mut self) -> Result<ExternalCandidateCleanupLifeline> {
        self.cleanup
            .take()
            .context("external qualification cleanup owner was already transferred")
    }

    /// Advance the original Q allocation through its retained journal. The
    /// restored-occurrence gate remains held across the durable first-contact
    /// claim, provider allocation and first supervisor activation. A failure
    /// fences this process-local owner; cleanup uses the retained allocation.
    pub fn advance(
        &mut self,
    ) -> std::result::Result<ExternalCandidateStartProgress, ExternalCandidateStartFailure> {
        let operation_id = self.qualification_operation_id.clone();
        let result =
            crate::hosted_operation::with_qualification_occurrence_contact(&operation_id, || {
                self.advance_under_occurrence_gate()
            });
        result.map_err(|error| {
            self.failed = true;
            classify_external_start_failure(self.state, &self.placement, error)
        })
    }

    fn advance_under_occurrence_gate(&mut self) -> Result<ExternalCandidateStartProgress> {
        ensure!(
            !self.failed,
            "external qualification startup owner is already fenced"
        );
        let mut prepared = ExternalPlacementOwner::new(self.state)
            .prepare_retained_qualification_for_start(&self.placement)?;
        let ExternalAllocationOwner::QualificationCandidate(owner) =
            &prepared.record.reservation.owner
        else {
            bail!("external qualification startup changed its retained owner");
        };
        ensure!(
            owner.coordinate == self.coordinate
                && owner.qualification_operation_id == self.qualification_operation_id
                && owner.operator_principal == self.root.purpose().owner_fingerprint
                && owner.program == *self.root.program()
                && prepared.record.reservation.admitted_capsule_hash
                    == self.coordinate.accepted_capsule_hash,
            "external qualification startup differs from its original accepted allocation"
        );
        let qualified = self
            .state
            .state_store
            .snapshot_qualification_operation(&self.qualification_operation_id)?
            .context("external qualification startup lost its restored operation")?;
        ensure!(
            qualified.phase
                == crate::runtime_db::runtime_snapshot_qualification::SnapshotQualificationPhase::OccurrenceBound
                && qualified.intent.owner_principal == self.root.purpose().owner_fingerprint
                && qualified.occurrence.as_ref() == Some(&owner.qualification_occurrence),
            "external qualification startup lost its exact operator-owned occurrence"
        );
        let activation_exists = if prepared.record.phase == ExternalAllocationPhase::Bound {
            self.state
                .state_store
                .external_supervisor_activation(&self.placement)?
                .is_some()
        } else {
            false
        };
        let first_contact = prepared.record.phase == ExternalAllocationPhase::Reserved;
        let first_activation =
            prepared.record.phase == ExternalAllocationPhase::Bound && !activation_exists;
        let needs_live_execution =
            first_contact || prepared.record.phase == ExternalAllocationPhase::Bound;
        let (refreshed_root, binding) = if needs_live_execution {
            prepared.record.reservation.startup_deadline()?;
            ensure!(
                !self.deadline.has_elapsed(),
                "external qualification startup deadline expired before first contact"
            );
            let refreshed = crate::operator_external_content::product_qualification::prepare_external_qualification_root_for_invocation(
                self.state,
                &self.invocation,
                &self.coordinate,
                &self.qualification_operation_id,
                self.limits,
            )?;
            ensure!(
                refreshed.capsule() == self.root.capsule()
                    && refreshed.purpose() == self.root.purpose()
                    && refreshed.program() == self.root.program(),
                "external qualification startup changed its current accepted Worker program"
            );
            let binding = preflight_external_qualification_program(
                self.state,
                self.root.program(),
                self.root.purpose(),
                qualification_start_journal_funding(prepared.record.phase, activation_exists),
            )?;
            ensure!(
                binding.digest() == prepared.record.reservation.binding_hash
                    && binding.capacity_owner() == prepared.record.reservation.capacity_owner,
                "external qualification startup changed its signed placement generation"
            );
            (Some(refreshed), Some(binding))
        } else {
            (None, None)
        };
        if first_activation {
            let inputs = self.guest_inputs.take().context(
                "external qualification startup lost original activation input lifelines; cleanup required",
            )?;
            prepared = prepared.with_guest_inputs(
                inputs,
                self.state
                    .external_placement_backends
                    .guest_runtime_upload_binding(self.state),
            )?;
        } else if activation_exists {
            // A retained activation can only be reconciled; it cannot accept
            // another descriptor inventory, even from the same local owner.
            self.guest_inputs = None;
        }
        let admission = if first_contact {
            let authority = self.state.state_store.pinned_state_authority()?;
            let guard = authority.acquire_shared_guard()?;
            let admission = self.invocation.authenticate(
                self.state,
                &authority,
                &guard,
                self.limits,
                &self.coordinate,
                &self.qualification_operation_id,
            )?;
            ensure!(
                admission.purpose() == self.root.purpose()
                    && admission.accepted_project_authority()
                        == &self.root.capsule().project_authority,
                "external qualification contact changed its accepted root"
            );
            authority.ensure_guard(&guard)?;
            drop(guard);
            Some(admission)
        } else {
            None
        };
        let state = self.state;
        let deadline = self.deadline;
        let advance = move || {
            let decision = if first_contact {
                prepared.claim_qualification(
                    state,
                    admission.context("external qualification first contact lost admission")?,
                    binding
                        .as_ref()
                        .context("external qualification contact lost its current binding")?,
                )?
            } else {
                if prepared.record.phase == ExternalAllocationPhase::Bound {
                    prepared.qualification_activation = Some(QualificationActivationAdmission {
                        state: state.clone(),
                        root: refreshed_root
                            .context("external qualification startup lost its invoking root")?,
                    });
                }
                prepared.claim()?
            };
            advance_claimed_external_start(decision, deadline)
        };
        if needs_live_execution {
            let expected_generation = &self
                .root
                .purpose()
                .consumer_content
                .as_ref()
                .context("external qualification contact lost its accepted Worker content")?
                .definitions
                .bundle_generation_identity;
            // The root refresh above established exact definition equality.
            // Keep that generation excluded from replacement until the fresh
            // allocation or supervisor contact has returned.
            state.engine.with_checked_bundle_generation(|generation| {
                ensure!(
                    generation.request_engine_generation_identity() == expected_generation,
                    "external qualification changed Bundle generation before contact"
                );
                advance()
            })
        } else {
            advance()
        }
    }
}

/// Reserve one fresh qualification-only allocation. The caller passes the
/// original input lifelines; all admission and placement authority is
/// reconstructed from the accepted root, current signed Bundle generation,
/// installed node binding and StateStore's occurrence/owner proof. No provider
/// contact, channel attachment or activation occurs here.
pub fn prepare_external_qualification_start<'a>(
    state: &'a AppState,
    context: &crate::handler_context::HandlerContext,
    coordinate: &ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeVerificationCoordinate,
    qualification_operation_id: &str,
    limits: ryeos_state::object_closure::ObjectClosureLimits,
    root: crate::operator_external_content::product_qualification::PreparedExternalQualificationRoot,
    guest_inputs: ryeos_external_execution::guest_inputs::ExternalGuestInputAuthority,
) -> Result<ExternalQualificationStart<'a>> {
    prepare_external_qualification_start_for_invocation(
        state,
        &crate::external_qualification_invocation::QualificationInvocation::operator(context),
        coordinate,
        qualification_operation_id,
        limits,
        root,
        guest_inputs,
    )
}

pub fn prepare_external_qualification_start_for_invocation<'a>(
    state: &'a AppState,
    invocation: &crate::external_qualification_invocation::QualificationInvocation,
    coordinate: &ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeVerificationCoordinate,
    qualification_operation_id: &str,
    limits: ryeos_state::object_closure::ObjectClosureLimits,
    root: crate::operator_external_content::product_qualification::PreparedExternalQualificationRoot,
    guest_inputs: ryeos_external_execution::guest_inputs::ExternalGuestInputAuthority,
) -> Result<ExternalQualificationStart<'a>> {
    let refreshed = crate::operator_external_content::product_qualification::prepare_external_qualification_root_for_invocation(
        state,
        invocation,
        coordinate,
        qualification_operation_id,
        limits,
    )?;
    ensure!(
        refreshed.capsule() == root.capsule()
            && refreshed.purpose() == root.purpose()
            && refreshed.program() == root.program(),
        "external qualification start changed its authenticated accepted root"
    );
    let root = refreshed;
    let placement = ryeos_state::external_execution::qualification_session::qualification_candidate_placement_key(
        coordinate,
        qualification_operation_id,
    )?;
    ensure!(
        state.state_store.external_allocation(&placement)?.is_none(),
        "external qualification startup already exists; recover its original allocation"
    );
    state
        .controller_lifetime
        .ensure_protects_app_root(&state.config.app_root)?;
    let snapshot_hash = validate_qualification_guest_input_authority(state, &root, &guest_inputs)?;
    let binding = preflight_external_qualification_program(
        state,
        root.program(),
        root.purpose(),
        crate::external_supervisor_native::SupervisorJournalFunding::New,
    )?;
    let contract = binding.backend_contract();
    let qualified = state
        .state_store
        .snapshot_qualification_operation(qualification_operation_id)?
        .context("external qualification start lost its restored operation")?;
    ensure!(
        qualified.phase
            == crate::runtime_db::runtime_snapshot_qualification::SnapshotQualificationPhase::OccurrenceBound
            && qualified.intent.owner_principal == root.purpose().owner_fingerprint,
        "external qualification start has no operator-owned bound occurrence"
    );
    let occurrence = qualified
        .occurrence
        .context("external qualification start has no bound occurrence")?;
    let owner = ExternalAllocationOwner::QualificationCandidate(
        crate::runtime_db::external_execution::ExternalQualificationCandidateOwner {
            coordinate: coordinate.clone(),
            qualification_operation_id: qualification_operation_id.to_owned(),
            qualification_occurrence: occurrence,
            operator_principal: root.purpose().owner_fingerprint.clone(),
            program: root.program().clone(),
        },
    );
    let authority_generation = ryeos_state::objects::canonical_value_digest(&serde_json::json!({
        "domain": "ryeos.external-qualification-channel-authority.v1",
        "placement_thread_id": &placement,
        "admitted_capsule_hash": &coordinate.accepted_capsule_hash,
        "base_snapshot_hash": &snapshot_hash,
        "binding_hash": binding.digest(),
        "program": root.program(),
    }))?;
    let access = ExternalChannelAuthorityAccess::new(&authority_generation)?;
    let channel_authority =
        access.decode(state.vault.ensure_external_channel_authority(&access)?)?;
    // No adapter admission precedes this placement kind, so it retains no
    // admitted adapter request; the request digest still names the field.
    let admitted_adapter_request_base64: Option<String> = None;
    let request_digest = ryeos_state::objects::canonical_value_digest(&serde_json::json!({
        "domain": "ryeos.external-qualification-placement-request.v1",
        "placement_thread_id": &placement,
        "admitted_capsule_hash": &coordinate.accepted_capsule_hash,
        "base_snapshot_hash": &snapshot_hash,
        "binding_hash": binding.digest(),
        "capacity_owner": binding.capacity_owner(),
        "owner": &owner,
        "backend_contract": &contract,
        "channel_authority_generation": &authority_generation,
        "channel_owner_public_key": channel_authority.owner_public_key(),
        "channel_bootstrap_capability_hash": channel_authority.bootstrap_capability_hash(),
        "admitted_adapter_request_base64": &admitted_adapter_request_base64,
    }))?;
    let startup_budget_ms = qualification_startup_budget_ms(&contract)?;
    let now = lillux::time::timestamp_millis();
    let reservation = ExternalAllocationReservation {
        schema: EXTERNAL_ALLOCATION_RESERVATION_SCHEMA,
        placement_thread_id: placement.clone(),
        admitted_capsule_hash: coordinate.accepted_capsule_hash.clone(),
        owner,
        base_snapshot_hash: snapshot_hash,
        binding_hash: binding.digest().to_owned(),
        capacity_owner: binding.capacity_owner().to_owned(),
        channel_authority_generation: authority_generation,
        channel_owner_public_key: channel_authority.owner_public_key(),
        channel_bootstrap_capability_hash: channel_authority.bootstrap_capability_hash(),
        admitted_adapter_request_base64,
        request_digest,
        max_active: contract.max_active,
        timeout_seconds: contract.timeout_seconds,
        contact_deadline_ms: now
            .checked_add(i64::from(contract.contact_timeout_seconds) * 1_000)
            .context("external qualification contact deadline overflow")?,
        startup_started_at_ms: now,
        startup_deadline_ms: now
            .checked_add(i64::try_from(startup_budget_ms)?)
            .context("external qualification startup deadline overflow")?,
    };
    let program =
        AdmittedExternalExecutionProgram::QualificationStructuredSession(root.program().clone());
    validate_external_placement_program(&program, &reservation, &contract)?;
    reservation.validate()?;
    let deadline = root.cap_execution_deadline(reservation.startup_deadline()?);
    let admission = {
        let authority = state.state_store.pinned_state_authority()?;
        let guard = authority.acquire_shared_guard()?;
        let admitted = invocation.authenticate(
            state,
            &authority,
            &guard,
            limits,
            coordinate,
            qualification_operation_id,
        )?;
        ensure!(
            admitted.purpose() == root.purpose()
                && admitted.accepted_project_authority() == &root.capsule().project_authority,
            "external qualification reservation changed its accepted owner"
        );
        authority.ensure_guard(&guard)?;
        admitted
    };
    let expected_generation = &root
        .purpose()
        .consumer_content
        .as_ref()
        .context("external qualification reservation lost its accepted Worker content")?
        .definitions
        .bundle_generation_identity;
    state.engine.with_checked_bundle_generation(|generation| {
        ensure!(
            generation.request_engine_generation_identity() == expected_generation,
            "external qualification changed Bundle generation before reservation"
        );
        state.state_store.reserve_external_qualification_allocation(
            state,
            &reservation,
            &binding,
            admission,
        )
    })?;
    let cleanup = ExternalCandidateCleanupLifeline::new(state, &placement);
    let original_guest_exclusion = guest_inputs.retain_exclusion_witness()?;
    Ok(ExternalQualificationStart {
        state,
        invocation: invocation.clone(),
        coordinate: coordinate.clone(),
        qualification_operation_id: qualification_operation_id.to_owned(),
        limits,
        placement,
        deadline,
        root,
        guest_inputs: Some(guest_inputs),
        original_guest_exclusion,
        cleanup: Some(cleanup),
        failed: false,
    })
}

fn validate_qualification_guest_input_authority(
    state: &AppState,
    root: &crate::operator_external_content::product_qualification::PreparedExternalQualificationRoot,
    inputs: &ryeos_external_execution::guest_inputs::ExternalGuestInputAuthority,
) -> Result<String> {
    use ryeos_external_execution_contract::{
        GuestMountAccess, GuestMountContentAuthority, GuestMountKind, GuestMountRole,
    };
    use ryeos_state::objects::{
        EnvironmentAuthority, ExecutionProjectAuthority, ExternalContentKind,
        ExternalContentRealizationSet, PinnedProjectRealization, SessionProcessEnvironmentValue,
    };
    let ExecutionProjectAuthority::PinnedGeneration {
        snapshot_hash,
        realization: PinnedProjectRealization::ReadOnly,
        workspace_outputs: None,
        environment: EnvironmentAuthority::None,
        ..
    } = &root.capsule().project_authority
    else {
        bail!("external qualification guest requires its output-free pinned root");
    };
    let content = root
        .purpose()
        .consumer_content
        .as_ref()
        .context("external qualification guest has no accepted Worker content")?;
    let projection = inputs.projection();
    root.program().validate_guest_inputs(projection)?;
    ensure!(
        projection.base_snapshot.snapshot_hash.as_str() == snapshot_hash.as_str()
            && projection.workspace_outputs.is_none(),
        "external qualification guest changed its accepted base snapshot"
    );
    let realizations = ExternalContentRealizationSet::new(
        content
            .worker_literals
            .iter()
            .chain(content.environment_realizations.iter())
            .chain(std::iter::once(&content.runtime_realization))
            .cloned()
            .collect(),
    )?;
    let destinations = ryeos_state::external_execution::admission::ExternalCandidateGuestEnvironment::expected_destinations(
        &root.program().requirement,
        &realizations,
    )?;
    let logical =
        ryeos_state::external_execution::admission::ExternalCandidateGuestEnvironment::derive(
            &root.program().requirement.runtime_recipe,
            &content.process_environment,
            &content.executable_search,
            &destinations,
        )?;
    ensure!(
        projection.environment == logical.environment
            && projection.executable_search == logical.executable_search,
        "external qualification guest changed its accepted environment"
    );
    let mut products = realizations
        .iter()
        .map(|entry| (entry.id.as_str(), entry))
        .collect::<BTreeMap<_, _>>();
    let mut scratch = content
        .process_environment
        .iter()
        .filter_map(|(name, value)| match value {
            SessionProcessEnvironmentValue::RuntimeViewDirectory { relative_path } => {
                Some((name.as_str(), relative_path.as_str()))
            }
            _ => None,
        })
        .collect::<BTreeMap<_, _>>();
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let cas = authority.cas_store()?;
    for input in &projection.inputs {
        match input.role {
            GuestMountRole::Product => {
                let entry = products
                    .remove(input.authority_id.as_str())
                    .context("external qualification guest added an unaccepted product")?;
                let (manifest_kind, normalized_mode) =
                    qualification_product_record_identity(&cas, entry)?;
                ensure!(
                    destinations.get(&entry.id) == Some(&input.destination)
                        && input.access == GuestMountAccess::ReadOnly
                        && input.bytes == entry.total_bytes
                        && input.normalized_mode == normalized_mode
                        && matches!(
                            (entry.kind, input.kind),
                            (ExternalContentKind::Tree, GuestMountKind::Directory)
                                | (ExternalContentKind::File, GuestMountKind::RegularFile)
                        )
                        && matches!(
                            &input.content_authority,
                            GuestMountContentAuthority::ProductManifest {
                                manifest_kind: supplied_kind,
                                manifest_hash,
                                ..
                            } if *supplied_kind == manifest_kind
                                && manifest_hash == &entry.manifest_hash
                        ),
                    "external qualification guest changed an accepted product mount"
                );
            }
            GuestMountRole::PrivateScratch => {
                let name = input
                    .authority_id
                    .strip_prefix("scratch-")
                    .context("external qualification guest changed its scratch identity")?;
                let relative_path = scratch
                    .remove(name)
                    .context("external qualification guest added unaccepted scratch")?;
                let mut relative = std::path::PathBuf::from("scratch");
                if relative_path != "." {
                    relative.push(relative_path);
                }
                let expected_hash =
                    ryeos_state::objects::canonical_value_digest(&serde_json::json!({
                        "domain": "ryeos.qualification-guest-private-scratch.v1",
                        "purpose": root.program().qualification_purpose_digest,
                        "snapshot": snapshot_hash,
                        "environment_name": name,
                        "relative_path": relative,
                    }))?;
                let expected_destination =
                    ryeos_state::objects::runtime_view_mount_destination(name)?;
                ensure!(
                    input.destination
                        == expected_destination
                            .to_str()
                            .context("external qualification scratch destination is not UTF-8")?
                        && input.kind == GuestMountKind::Directory
                        && input.access == GuestMountAccess::PrivateWritable
                        && input.normalized_mode.is_none()
                        && input.bytes == 0
                        && matches!(
                            &input.content_authority,
                            GuestMountContentAuthority::PrivateScratch { binding_hash }
                                if binding_hash == &expected_hash
                        ),
                    "external qualification guest changed accepted private scratch"
                );
            }
            _ => bail!("external qualification guest added non-product authority"),
        }
    }
    ensure!(
        products.is_empty() && scratch.is_empty(),
        "external qualification guest omitted accepted input authority"
    );
    authority.ensure_guard(&guard)?;
    Ok(snapshot_hash.clone())
}

fn qualification_product_record_identity(
    cas: &lillux::CasStore,
    entry: &ryeos_state::objects::ExternalContentRealization,
) -> Result<(
    ryeos_external_execution_contract::GuestProductManifestKind,
    Option<u32>,
)> {
    use ryeos_external_execution_contract::GuestProductManifestKind;
    use ryeos_state::objects::{
        EXTERNAL_CONTENT_MANIFEST_KIND, EXTERNAL_LARGE_CONTENT_MANIFEST_KIND, ExternalContentKind,
        ExternalContentManifestObject, ExternalLargeContentManifestObject,
        MAX_LARGE_CONTENT_MANIFEST_BYTES,
    };
    let value = ryeos_state::object_closure::load_exact_cas_object_with_cas(
        cas,
        &entry.manifest_hash,
        MAX_LARGE_CONTENT_MANIFEST_BYTES as u64,
    )?;
    ensure!(
        lillux::sha256_hex(lillux::canonical_json(&value)?.as_bytes()) == entry.manifest_hash,
        "external qualification product record changed its CAS identity"
    );
    let (kind, entry_count, total_bytes, file_shaped, file_mode) =
        match value.get("kind").and_then(serde_json::Value::as_str) {
            Some(EXTERNAL_CONTENT_MANIFEST_KIND) => {
                let manifest = ExternalContentManifestObject::from_value(&value)?;
                (
                    GuestProductManifestKind::Content,
                    manifest.entry_count,
                    manifest.total_bytes,
                    manifest.is_file_shaped(),
                    manifest.entries.first().and_then(|file| file.mode),
                )
            }
            Some(EXTERNAL_LARGE_CONTENT_MANIFEST_KIND) => {
                let manifest = ExternalLargeContentManifestObject::from_value(&value)?;
                (
                    GuestProductManifestKind::LargeContent,
                    manifest.entry_count,
                    manifest.total_bytes,
                    manifest.is_file_shaped(),
                    manifest.entries.first().and_then(|file| file.mode),
                )
            }
            _ => bail!("external qualification product has an unsupported manifest kind"),
        };
    ensure!(
        entry_count == entry.entry_count
            && total_bytes == entry.total_bytes
            && (entry.kind != ExternalContentKind::File || file_shaped),
        "external qualification product record contradicts its accepted realization"
    );
    Ok((
        kind,
        if entry.kind == ExternalContentKind::File {
            file_mode
        } else {
            None
        },
    ))
}

/// One process-local ordinary startup owner. Descriptor lifelines stay here
/// across allocation polls and transfer only into the Bound activation step.
/// Dropping this owner never proves external cleanup or permits replacement.
pub struct ExternalDirectStart<'a> {
    state: &'a AppState,
    placement: String,
    deadline: lillux::time::MonotonicDeadline,
    guest_inputs: Option<ryeos_external_execution::guest_inputs::ExternalGuestInputAuthority>,
    failed: bool,
}

fn require_direct_start_lifelines(
    phase: ExternalAllocationPhase,
    activation_exists: bool,
    has_inputs: bool,
) -> Result<()> {
    ensure!(
        has_inputs
            || (activation_exists && phase != ExternalAllocationPhase::Reserved)
            || !matches!(
                phase,
                ExternalAllocationPhase::Reserved | ExternalAllocationPhase::Bound
            ),
        "external direct startup lost original activation lifelines; cleanup required"
    );
    Ok(())
}

impl ExternalDirectStart<'_> {
    /// Uses the same bounded lifecycle advance as structured sessions. A local
    /// error fences this owner; recovery must observe the retained obligation.
    pub fn advance(
        &mut self,
    ) -> std::result::Result<ExternalCandidateStartProgress, ExternalCandidateStartFailure> {
        let result = (|| {
            ensure!(
                !self.failed,
                "external direct startup owner is already fenced"
            );
            let mut prepared =
                ExternalPlacementOwner::new(self.state).prepare_retained_direct(&self.placement)?;
            let activation_exists = self
                .state
                .state_store
                .external_supervisor_activation(&self.placement)?
                .is_some();
            require_direct_start_lifelines(
                prepared.record.phase,
                activation_exists,
                self.guest_inputs.is_some(),
            )?;
            if prepared.record.phase == ExternalAllocationPhase::Bound {
                if !activation_exists {
                    let inputs = self.guest_inputs.take()
                        .context("external direct recovery has no original activation input lifelines; cleanup required")?;
                    prepared = prepared.with_guest_inputs(
                        inputs,
                        self.state
                            .external_placement_backends
                            .guest_runtime_upload_binding(self.state),
                    )?;
                } else {
                    // Retained activation can only be reconciled. Never inject
                    // even an exact-looking replacement descriptor inventory.
                    self.guest_inputs = None;
                }
            }
            advance_prepared_external_start(prepared, self.deadline)
        })();
        result.map_err(|error| {
            self.failed = true;
            classify_external_start_failure(self.state, &self.placement, error)
        })
    }
}

/// Compile the exact retained ordinary capsule and reserve it through the sole
/// born-owner boundary. This performs no provider contact. The caller supplies
/// verified protocol/source evidence and retained input descriptors, not a
/// serialized program, claim, credential, or alternate execution workflow.
pub fn prepare_external_direct_start<'a>(
    state: &'a AppState,
    placement: &str,
    chain_root_id: &str,
    protocol: &ryeos_engine::protocols::VerifiedProtocol,
    source: Option<&ryeos_state::source_verification::VerifiedAdmittedSourceRecords>,
    guest_inputs: ryeos_external_execution::guest_inputs::ExternalGuestInputAuthority,
    project_file: Option<&crate::thread_lifecycle::PreparedExternalDirectProjectFile>,
) -> Result<ExternalDirectStart<'a>> {
    ensure!(
        state.state_store.external_allocation(placement)?.is_none(),
        "external direct startup already exists; recover without replacement inputs"
    );
    let capsule = state
        .state_store
        .admitted_launch_capsule(placement)?
        .context("external direct startup has no admitted born capsule")?;
    let claim = state
        .state_store
        .get_launch_claim(placement)?
        .context("external direct startup has no current launch owner")?;
    let compiled = crate::thread_lifecycle::compile_external_direct_program(
        &capsule,
        protocol,
        placement,
        chain_root_id,
        guest_inputs.projection(),
        source,
        claim.owner,
        project_file,
    )?;
    let prepared = ExternalPlacementOwner::new(state).prepare_direct(compiled)?;
    let deadline = prepared.record.reservation.startup_deadline()?;
    Ok(ExternalDirectStart {
        state,
        placement: placement.to_owned(),
        deadline,
        guest_inputs: Some(guest_inputs),
        failed: false,
    })
}

/// Reentry never recreates compiler evidence, a launch claim, or input
/// descriptors. A bound occurrence lacking an activation intent cannot resume
/// startup after its original lifelines were lost; `advance` explicitly fails
/// with unproved cleanup rather than indefinitely reporting OccurrenceBound.
pub fn recover_external_direct_start<'a>(
    state: &'a AppState,
    placement: &str,
) -> Result<ExternalDirectStart<'a>> {
    let prepared = ExternalPlacementOwner::new(state).prepare_retained_direct(placement)?;
    Ok(ExternalDirectStart {
        state,
        placement: placement.to_owned(),
        deadline: prepared.record.reservation.startup_deadline()?,
        guest_inputs: None,
        failed: false,
    })
}

/// Reserve startup once from the admitted capsule and project its retained
/// expiry into the caller's live clock. No allocator is contacted here.
pub fn external_candidate_startup_deadline(
    state: &AppState,
    placement: &str,
) -> Result<lillux::time::MonotonicDeadline> {
    ExternalPlacementOwner::new(state)
        .prepare(placement)?
        .record
        .reservation
        .startup_deadline()
}

/// Advance one exact external candidate placement from the authoritative
/// dedicated-session owner.  This is the only public start surface: raw
/// prepared/contact/reconciliation capabilities remain crate-private.
pub fn advance_external_candidate_start(
    state: &AppState,
    placement: &str,
    live_deadline: lillux::time::MonotonicDeadline,
) -> std::result::Result<ExternalCandidateStartProgress, ExternalCandidateStartFailure> {
    let prepared = ExternalPlacementOwner::new(state)
        .prepare(placement)
        .map_err(|error| classify_external_start_failure(state, placement, error))?;
    advance_prepared_external_start(prepared, live_deadline)
        .map_err(|error| classify_external_start_failure(state, placement, error))
}

/// Advance the one activation mutation with its exact non-cloneable guest
/// input authority. This surface is valid only after allocation has bound an
/// occurrence and before any activation intent exists. Recovery deliberately
/// uses `advance_external_candidate_start` and can only reconcile.
pub fn advance_external_candidate_start_with_guest_inputs(
    state: &AppState,
    placement: &str,
    guest_inputs: ryeos_external_execution::guest_inputs::ExternalGuestInputAuthority,
    live_deadline: lillux::time::MonotonicDeadline,
) -> std::result::Result<ExternalCandidateStartProgress, ExternalCandidateStartFailure> {
    let prepared = ExternalPlacementOwner::new(state)
        .prepare(placement)
        .and_then(|prepared| {
            prepared.with_guest_inputs(
                guest_inputs,
                state
                    .external_placement_backends
                    .guest_runtime_upload_binding(state),
            )
        })
        .map_err(|error| classify_external_start_failure(state, placement, error))?;
    advance_prepared_external_start(prepared, live_deadline)
        .map_err(|error| classify_external_start_failure(state, placement, error))
}

/// Commit the one completion-bound quiesce request and select C only after the
/// authenticated export has been reconstructed and revalidated under the node
/// CAS authority. Re-entry returns the exact existing quiesce/import; it never
/// mints a later completion coordinate.
pub fn advance_external_candidate_completion(
    state: &AppState,
    placement: &str,
    completion_request_digest: &str,
) -> Result<ExternalCandidateCompletionProgress> {
    ensure!(
        lillux::valid_hash(completion_request_digest)
            && !completion_request_digest
                .bytes()
                .any(|byte| byte.is_ascii_uppercase()),
        "external completion request digest is not canonical"
    );
    let allocation = state
        .state_store
        .external_allocation(placement)?
        .context("external completion has no retained allocation")?;
    let binding = state.state_store.external_execution_channel(placement)?;
    if let Some(retained) = state
        .state_store
        .retained_external_candidate_import(placement)?
    {
        ensure!(
            retained.completion_request_digest == completion_request_digest
                && retained.binding == binding,
            "external imported candidate changed its completion authority"
        );
        return validated_external_candidate_generation(state, retained);
    }
    ensure!(
        allocation.phase == ExternalAllocationPhase::Bound,
        "external completion requires its live bound occurrence"
    );
    let access =
        ExternalChannelAuthorityAccess::new(&allocation.reservation.channel_authority_generation)?;
    let authority = access.decode(
        state
            .vault
            .external_channel_authority(&access)
            .context("read protected external completion signer")?,
    )?;
    ensure!(
        binding.owner_public_key == authority.owner_public_key(),
        "external completion signer changed its channel binding"
    );
    state.state_store.ensure_external_candidate_quiesce(
        placement,
        completion_request_digest,
        authority.owner_signing_key(),
    )?;
    let Some(retained) = state
        .state_store
        .retained_external_candidate_import(placement)?
    else {
        return Ok(ExternalCandidateCompletionProgress::AwaitingImport);
    };
    ensure!(
        retained.completion_request_digest == completion_request_digest
            && retained.binding == binding,
        "external imported candidate changed its completion authority"
    );
    validated_external_candidate_generation(state, retained)
}

fn validated_external_candidate_generation(
    state: &AppState,
    retained: crate::runtime_db::external_execution::RetainedExternalCandidateImport,
) -> Result<ExternalCandidateCompletionProgress> {
    let state_authority = state.state_store.pinned_state_authority()?;
    let guard = state_authority.acquire_shared_guard()?;
    ryeos_state::external_execution::export::validate_retained_candidate_coordinates(
        &state_authority,
        &guard,
        &retained.binding,
        &retained.candidate_snapshot_hash,
        retained.candidate_output_capture_hash.as_deref(),
        &retained.completion_request_digest,
        &retained.writer_exclusion_evidence_hash,
    )?;
    drop(guard);
    drop(state_authority);
    Ok(ExternalCandidateCompletionProgress::Imported(
        ryeos_state::objects::WorkspaceGenerationPair {
            snapshot_hash: retained.candidate_snapshot_hash,
            output_capture_hash: retained.candidate_output_capture_hash,
        },
    ))
}

/// Exact upper bound for completion observation plus provider cleanup under
/// the retained binding generation. Lillux remains the clock owner; this value
/// is only workflow policy for the caller's monotonic deadline.
pub fn external_candidate_settlement_timeout(
    state: &AppState,
    placement: &str,
) -> Result<lillux::time::Duration> {
    let allocation = state
        .state_store
        .external_allocation(placement)?
        .context("external settlement has no retained allocation")?;
    let binding = state
        .state_store
        .retained_external_binding(&allocation.reservation.binding_hash)?
        .context("external settlement lost its retained binding generation")?;
    let contract = binding.backend_contract();
    let seconds = contract
        .observation_timeout_seconds
        .checked_add(contract.cleanup_timeout_seconds)
        .context("external settlement timeout overflow")?;
    Ok(lillux::time::Duration::from_secs(u64::from(seconds)))
}

pub fn external_candidate_cleanup_is_proved(state: &AppState, placement: &str) -> Result<bool> {
    Ok(state
        .state_store
        .external_allocation(placement)?
        .is_none_or(|record| record.phase.is_settled()))
}

/// Select the exact imported generation for the ordinary dedicated-candidate
/// freeze owner. Absence means this is not an external candidate. Presence is
/// exposed only after completed-session authority, exact import validation and
/// provider cleanup all agree; a local workspace generation can never stand in
/// for the remotely produced C.
pub fn completed_external_candidate_generation(
    state: &AppState,
    placement: &str,
) -> Result<Option<ryeos_state::objects::WorkspaceGenerationPair>> {
    let Some(session) = state.state_store.dedicated_session(placement)? else {
        return Ok(None);
    };
    let capsule = state
        .state_store
        .admitted_persistent_session_capsule(&session.admitted_capsule_hash)?;
    capsule.validate()?;
    if capsule.external_candidate.is_none() {
        return Ok(None);
    }
    ensure!(
        session.terminal_reason.as_deref() == Some("completed")
            && matches!(session.state.as_str(), "freezing" | "frozen"),
        "external candidate generation is not at its completed freeze boundary"
    );
    let completion = session
        .completion_fence
        .as_ref()
        .context("completed external candidate has no durable completion fence")?;
    ensure!(
        external_candidate_cleanup_is_proved(state, placement)?,
        "completed external candidate retains unresolved provider cleanup"
    );
    let retained = state
        .state_store
        .retained_external_candidate_import(placement)?
        .context("completed external candidate has no retained imported generation")?;
    ensure!(
        retained.completion_request_digest == completion.request_digest,
        "external candidate import changed its completion fence"
    );
    let ExternalCandidateCompletionProgress::Imported(generation) =
        validated_external_candidate_generation(state, retained)?
    else {
        unreachable!("validated retained import always selects a generation")
    };
    ensure!(
        state
            .state_store
            .settle_completed_external_workspace_owned(placement)?,
        "completed external candidate retains unresolved workspace descendants"
    );
    Ok(Some(generation))
}

/// Advance occurrence teardown after the ordinary target's exact complete
/// terminal observation. This is not thread success or fresh execution
/// authority. The runtime writer admits the first intent from applied evidence
/// and the original live owner; retained intents permit reconciliation only.
pub fn advance_external_direct_settlement(
    state: &AppState,
    placement: &str,
) -> Result<ExternalCandidateCleanupProgress> {
    let record = state
        .state_store
        .external_allocation(placement)?
        .context("external direct settlement has no retained allocation")?;
    ensure!(
        matches!(
            &record.reservation.owner,
            ExternalAllocationOwner::DirectThread { .. }
        ),
        "external direct settlement cannot settle a structured session"
    );
    ensure!(
        matches!(
            record.phase,
            ExternalAllocationPhase::Bound
                | ExternalAllocationPhase::Quarantined
                | ExternalAllocationPhase::Terminated
        ),
        "external direct settlement requires an existing occurrence"
    );
    if record.phase == ExternalAllocationPhase::Terminated {
        return Ok(ExternalCandidateCleanupProgress::Proved);
    }
    let prepared =
        ExternalPlacementOwner::new(state).prepare_retained_direct_for_cleanup(placement)?;
    let reconciliation = match prepared.claim()? {
        ExternalPlacementContactDecision::Reconcile(reconciliation) => reconciliation,
        ExternalPlacementContactDecision::Settled(_) => {
            return Ok(ExternalCandidateCleanupProgress::Proved);
        }
        ExternalPlacementContactDecision::Contact(_) => {
            bail!("external settlement cannot allocate a replacement occurrence")
        }
    };
    // No app-side successful-output boolean authorizes this mutation. The
    // shared begin_external_termination transaction owns first-intent admission.
    let after = reconciliation.terminate_or_reconcile()?;
    Ok(if after.phase.is_settled() {
        ExternalCandidateCleanupProgress::Proved
    } else {
        ExternalCandidateCleanupProgress::Pending
    })
}

/// Advance the original Q provider occurrence toward exact terminal proof
/// after its immutable transcript, local teardown and signed native whole
/// scope death have been retained. The allocation writer rechecks those facts
/// in the same transaction that grants the one provider termination contact.
/// This settles capacity and grants no Worker semantic or qualification claim.
pub fn advance_external_qualification_normal_settlement(
    state: &AppState,
    placement: &str,
) -> Result<ExternalCandidateCleanupProgress> {
    state
        .controller_lifetime
        .ensure_protects_app_root(&state.config.app_root)?;
    let initial = state
        .state_store
        .external_allocation(placement)?
        .context("Q normal settlement has no retained allocation")?;
    let ExternalAllocationOwner::QualificationCandidate(owner) = &initial.reservation.owner else {
        bail!("Q normal settlement changed its accepted owner");
    };
    let operation_id = owner.qualification_operation_id.clone();
    crate::hosted_operation::with_qualification_occurrence_contact(&operation_id, || {
        let current = state
            .state_store
            .external_allocation(placement)?
            .context("Q normal settlement lost its original allocation")?;
        ensure!(
            current.reservation == initial.reservation
                && matches!(
                    current.phase,
                    ExternalAllocationPhase::Bound
                        | ExternalAllocationPhase::Quarantined
                        | ExternalAllocationPhase::Terminated
                ),
            "Q normal settlement changed its exact occurrence"
        );
        let channel = state.state_store.external_execution_channel(placement)?;
        ensure!(
            channel.execution_binding_hash == current.reservation.binding_hash
                && matches!(
                    channel.execution_mode,
                    ryeos_state::external_execution::ExternalExecutionMode::QualificationStructuredSession {}
                )
                && channel.candidate_export_max_bytes == 0
                && !state.state_store.external_execution_revoked(placement)?,
            "Q normal settlement lost its unrevoked zero-export channel"
        );
        if current.phase == ExternalAllocationPhase::Terminated {
            state
                .state_store
                .ensure_external_qualification_normal_terminal(placement)?;
            return Ok(ExternalCandidateCleanupProgress::Proved);
        }
        let prepared = ExternalPlacementOwner::new(state)
            .prepare_retained_qualification_for_cleanup(placement)?;
        let reconciliation = match prepared.claim()? {
            ExternalPlacementContactDecision::Reconcile(reconciliation) => reconciliation,
            ExternalPlacementContactDecision::Settled(_) => {
                bail!("Q normal settlement changed phase before its provider observation")
            }
            ExternalPlacementContactDecision::Contact(_) => {
                bail!("Q normal settlement cannot allocate a replacement occurrence")
            }
        };
        let after = reconciliation.terminate_or_reconcile()?;
        ensure!(
            !state.state_store.external_execution_revoked(placement)?,
            "Q normal settlement was cancelled during provider termination"
        );
        if after.phase == ExternalAllocationPhase::Terminated {
            state
                .state_store
                .ensure_external_qualification_normal_terminal(placement)?;
        }
        Ok(if after.phase == ExternalAllocationPhase::Terminated {
            ExternalCandidateCleanupProgress::Proved
        } else {
            ExternalCandidateCleanupProgress::Pending
        })
    })
}

/// Historical predicate for automatic recovery only. Explicit operator/task
/// cancellation must bypass this exception and retain sticky revocation.
fn preserves_external_direct_settlement(state: &AppState, placement: &str) -> Result<bool> {
    let Some(record) = state.state_store.external_allocation(placement)? else {
        return Ok(false);
    };
    if !matches!(
        &record.reservation.owner,
        ExternalAllocationOwner::DirectThread { .. }
    ) {
        return Ok(false);
    }
    Ok(state
        .state_store
        .external_direct_normal_settlement_output(placement)?
        .is_some())
}

/// A first Q provider termination contact may have lost its response. Only
/// its durable normal intent and the same immutable local/native prerequisites
/// preserve reconciliation across owner drop or controller restart.
fn preserves_external_qualification_normal_settlement(
    state: &AppState,
    placement: &str,
) -> Result<bool> {
    state
        .state_store
        .has_external_qualification_normal_termination_intent(placement)
}

fn fence_external_candidate_automatically(state: &AppState, placement: &str) -> Result<bool> {
    if preserves_external_direct_settlement(state, placement)?
        || preserves_external_qualification_normal_settlement(state, placement)?
    {
        return Ok(false);
    }
    request_external_candidate_cleanup(state, placement)?;
    Ok(true)
}

/// Durably revoke further guest input and advance at most one provider
/// reconciliation/termination operation. This owns no polling policy: callers
/// retain the original session/root while repeatedly advancing `Pending`.
pub fn advance_external_candidate_cleanup(
    state: &AppState,
    placement: &str,
) -> Result<ExternalCandidateCleanupProgress> {
    let Some(record) = state.state_store.external_allocation(placement)? else {
        return Ok(ExternalCandidateCleanupProgress::Proved);
    };
    if record.phase.is_settled() {
        return Ok(ExternalCandidateCleanupProgress::Proved);
    }
    request_external_candidate_cleanup(state, placement)?;
    let current = state
        .state_store
        .external_allocation(placement)?
        .context("external allocation disappeared during cleanup")?;
    if current.phase.is_settled() {
        return Ok(ExternalCandidateCleanupProgress::Proved);
    }
    let owner = ExternalPlacementOwner::new(state);
    let prepared = match &current.reservation.owner {
        ExternalAllocationOwner::DedicatedSession(_) => {
            owner.prepare_retained_session_for_cleanup(placement)?
        }
        ExternalAllocationOwner::DirectThread { .. } => {
            owner.prepare_retained_direct_for_cleanup(placement)?
        }
        ExternalAllocationOwner::QualificationCandidate(_) => {
            owner.prepare_retained_qualification_for_cleanup(placement)?
        }
    };
    let reconciliation = match prepared.claim()? {
        ExternalPlacementContactDecision::Reconcile(reconciliation) => reconciliation,
        ExternalPlacementContactDecision::Settled(_) => {
            return Ok(ExternalCandidateCleanupProgress::Proved);
        }
        ExternalPlacementContactDecision::Contact(_) => {
            bail!("external cleanup recovered a fresh allocation contact permit")
        }
    };
    let current = state
        .state_store
        .external_allocation(placement)?
        .context("external allocation disappeared before cleanup contact")?;
    let after = if current.occurrence.is_none() {
        reconciliation.reconcile()?
    } else {
        reconciliation.terminate_or_reconcile()?
    };
    Ok(if after.phase.is_settled() {
        ExternalCandidateCleanupProgress::Proved
    } else {
        ExternalCandidateCleanupProgress::Pending
    })
}

/// Commit cancellation/revocation without performing provider I/O. This is
/// safe in failure and owner-drop paths: later recovery can only reconcile the
/// original allocation or termination operation.
pub fn request_external_candidate_cleanup(state: &AppState, placement: &str) -> Result<()> {
    let Some(record) = state.state_store.external_allocation(placement)? else {
        return Ok(());
    };
    if record.phase.is_settled() {
        return Ok(());
    }
    state
        .state_store
        .cancel_uncontacted_external_allocation(placement)?;
    let current = state
        .state_store
        .external_allocation(placement)?
        .context("external allocation disappeared while requesting cleanup")?;
    if current.phase.is_settled()
        || state
            .state_store
            .optional_external_execution_channel(placement)?
            .is_none()
    {
        return Ok(());
    }
    let access =
        ExternalChannelAuthorityAccess::new(&current.reservation.channel_authority_generation)?;
    let authority = access.decode(
        state
            .vault
            .external_channel_authority(&access)
            .context("read protected external cleanup signer")?,
    )?;
    state
        .state_store
        .author_external_owner_revocation(placement, authority.owner_signing_key())?;
    Ok(())
}

/// Process-lifetime carrier for the remote cleanup obligation. Destruction is
/// deliberately non-contacting: it preserves an already-committed normal
/// settlement, otherwise commits sticky cancellation. Provider observation
/// remains with the retained recovery owner.
pub struct ExternalCandidateCleanupLifeline {
    state: AppState,
    placement: String,
    disarmed: bool,
}

#[derive(Debug, Default)]
pub struct ExternalCandidateCleanupRecovery {
    pub discovered: usize,
    pub proved: usize,
    pub pending: usize,
    pub failures: Vec<(String, String)>,
}

/// A replacement daemon cannot recreate the predecessor's connector/process
/// occurrence. Fence incomplete external placements before ingress, preserving
/// already-committed normal direct settlement. No provider I/O occurs here.
pub fn fence_external_candidates_after_controller_restart(state: &AppState) -> Result<usize> {
    let placements = state
        .state_store
        .unsettled_external_allocation_placements()?;
    let mut fenced = 0;
    for placement in &placements {
        if fence_external_candidate_automatically(state, placement)
            .with_context(|| format!("fence predecessor external placement {placement}"))?
        {
            fenced += 1;
        }
    }
    Ok(fenced)
}

/// Advance every already-quarantined occurrence once. Provider failures stay
/// typed by placement and do not erase the retained obligation or prevent the
/// daemon from serving observation/control paths needed for recovery.
pub fn recover_external_candidate_cleanups(
    state: &AppState,
) -> Result<ExternalCandidateCleanupRecovery> {
    // A lease whose guest never reported ready is refused first, so the
    // cleanup below finds it quarantined and terminates it in this same pass.
    let overdue_failures = lease_lifecycle::refuse_overdue_external_leases(state)?;
    let placements = state
        .state_store
        .recoverable_external_cleanup_placements()?;
    let mut report = ExternalCandidateCleanupRecovery {
        discovered: placements.len(),
        failures: overdue_failures,
        ..Default::default()
    };
    for placement in placements {
        let result = (|| {
            if preserves_external_direct_settlement(state, &placement)? {
                advance_external_direct_settlement(state, &placement)
            } else if preserves_external_qualification_normal_settlement(state, &placement)? {
                advance_external_qualification_normal_settlement(state, &placement)
            } else {
                advance_external_candidate_cleanup(state, &placement)
            }
        })();
        match result {
            Ok(ExternalCandidateCleanupProgress::Proved) => report.proved += 1,
            Ok(ExternalCandidateCleanupProgress::Pending) => report.pending += 1,
            Err(error) => report.failures.push((placement, format!("{error:#}"))),
        }
    }
    Ok(report)
}

impl ExternalCandidateCleanupLifeline {
    pub fn new(state: &AppState, placement: &str) -> Self {
        Self {
            state: state.clone(),
            placement: placement.to_owned(),
            disarmed: false,
        }
    }

    /// Consume this exact Q cleanup owner only after immutable local and
    /// signed native evidence joins an independently observed provider
    /// terminal. A failed check leaves Drop's sticky cancellation armed.
    pub fn disarm_after_qualification_normal_terminal(mut self) -> Result<()> {
        self.state
            .state_store
            .ensure_external_qualification_normal_terminal(&self.placement)?;
        self.disarmed = true;
        Ok(())
    }
}

impl Drop for ExternalCandidateCleanupLifeline {
    fn drop(&mut self) {
        if self.disarmed {
            return;
        }
        if let Err(error) = fence_external_candidate_automatically(&self.state, &self.placement) {
            tracing::error!(
                placement = %self.placement,
                error = %format!("{error:#}"),
                "external candidate owner-drop cancellation could not be retained"
            );
        }
    }
}

/// Claim at most one exact remote stdout frame for the protected connector.
/// The caller must retain the returned permit across the complete local write.
pub fn claim_external_protocol_output(
    state: &AppState,
    placement: &str,
) -> Result<ExternalProtocolOutput> {
    let controller_lifetime = Arc::clone(&state.controller_lifetime);
    controller_lifetime
        .ensure_protects_app_root(&state.config.app_root)
        .context("external connector controller lease has the wrong app root")?;
    match state
        .state_store
        .claim_next_external_protocol_output(placement)?
    {
        crate::runtime_db::external_execution::ExternalProtocolOutputClaim::Idle => {
            Ok(ExternalProtocolOutput::Idle)
        }
        crate::runtime_db::external_execution::ExternalProtocolOutputClaim::Uncertain {
            sequence,
            frame_digest,
        } => Ok(ExternalProtocolOutput::Uncertain {
            sequence,
            frame_digest,
        }),
        crate::runtime_db::external_execution::ExternalProtocolOutputClaim::Claimed(frame) => {
            let (bytes, eof) = match &frame.frame().payload {
                ryeos_state::external_execution::ExecutionChannelPayload::ProtocolBytes {
                    bytes_base64: _,
                } => (frame.protocol_bytes()?.to_vec(), false),
                ryeos_state::external_execution::ExecutionChannelPayload::ProtocolEof => {
                    (Vec::new(), true)
                }
                _ => bail!("external protocol output claim returned a non-protocol frame"),
            };
            Ok(ExternalProtocolOutput::Claimed(
                ExternalProtocolOutputPermit {
                    state_store: Arc::clone(&state.state_store),
                    _controller_lifetime: controller_lifetime,
                    placement: placement.to_owned(),
                    sequence: frame.frame().sequence,
                    frame_digest: frame.digest().to_owned(),
                    bytes,
                    eof,
                },
            ))
        }
    }
}

/// Retain one bounded local-provider stdin chunk for exact transport to the
/// remote exec-server. A local commit error is terminal ambiguity; callers
/// must not retry reconstructed bytes or fall back to local execution.
pub fn author_external_protocol_input(
    state: &AppState,
    placement: &str,
    bytes: &[u8],
) -> Result<ExternalChannelOutboundFrame> {
    let payload = external_protocol_input_payload(bytes)?;
    author_external_channel_command(state, placement, payload)
}

fn external_protocol_input_payload(
    bytes: &[u8],
) -> Result<ryeos_state::external_execution::ExecutionChannelPayload> {
    ensure!(
        !bytes.is_empty() && bytes.len() <= ryeos_state::external_execution::MAX_CHUNK_BYTES,
        "external connector input chunk is empty or exceeds its bound"
    );
    Ok(
        ryeos_state::external_execution::ExecutionChannelPayload::ProtocolBytes {
            bytes_base64: STANDARD.encode(bytes),
        },
    )
}

fn advance_prepared_external_start(
    prepared: PreparedExternalPlacement,
    live_deadline: lillux::time::MonotonicDeadline,
) -> Result<ExternalCandidateStartProgress> {
    // Do not consume the one allocation-contact claim on an already expired
    // live call. Existing bound Release replay is checked by its journal owner.
    if prepared.record.phase == ExternalAllocationPhase::Reserved {
        ensure!(
            !live_deadline.has_elapsed(),
            "external startup live deadline expired before contact"
        );
    }
    advance_claimed_external_start(prepared.claim()?, live_deadline)
}

fn advance_claimed_external_start(
    decision: ExternalPlacementContactDecision,
    live_deadline: lillux::time::MonotonicDeadline,
) -> Result<ExternalCandidateStartProgress> {
    match decision {
        ExternalPlacementContactDecision::Contact(permit) => {
            let record = permit.contact(live_deadline)?;
            progress_from_allocation_record(&record)
        }
        ExternalPlacementContactDecision::Reconcile(reconciliation) => {
            match reconciliation.record.phase {
                ExternalAllocationPhase::ContactPending => {
                    let deadline = reconciliation
                        .record
                        .reservation
                        .startup_deadline()?
                        .min(live_deadline);
                    ensure!(
                        !deadline.has_elapsed(),
                        "external startup live deadline expired before reconciliation"
                    );
                    let observation = reconciliation.reconcile_with_deadline(
                        deadline,
                        ExternalObservationTiming::Startup {
                            deadline_exceeded: false,
                            live_deadline: deadline,
                        },
                    )?;
                    let record = observation.value;
                    require_timely_lifecycle_observation(observation.deadline_exceeded, deadline)?;
                    record.reservation.startup_deadline()?;
                    progress_from_allocation_record(&record)
                }
                ExternalAllocationPhase::Bound => {
                    let activation_exists = reconciliation
                        .state_store
                        .external_supervisor_activation(
                            &reconciliation.record.reservation.placement_thread_id,
                        )?
                        .is_some();
                    if reconciliation.guest_inputs.is_none() && !activation_exists {
                        reconciliation.record.reservation.startup_deadline()?;
                        ensure!(
                            !live_deadline.has_elapsed(),
                            "external startup live deadline expired before activation"
                        );
                        Ok(ExternalCandidateStartProgress::OccurrenceBound)
                    } else {
                        reconciliation.advance_start(live_deadline)
                    }
                }
                ExternalAllocationPhase::Quarantined => {
                    Ok(ExternalCandidateStartProgress::CleanupRequired)
                }
                phase => bail!(
                    "external placement reconciliation returned invalid start phase {phase:?}"
                ),
            }
        }
        ExternalPlacementContactDecision::Settled(record) => {
            progress_from_allocation_record(&record)
        }
    }
}

fn progress_from_allocation_record(
    record: &ExternalAllocationRecord,
) -> Result<ExternalCandidateStartProgress> {
    match record.phase {
        ExternalAllocationPhase::ContactPending => {
            Ok(ExternalCandidateStartProgress::AllocationPending)
        }
        ExternalAllocationPhase::Bound => Ok(ExternalCandidateStartProgress::OccurrenceBound),
        ExternalAllocationPhase::Quarantined => Ok(ExternalCandidateStartProgress::CleanupRequired),
        ExternalAllocationPhase::NoContact
        | ExternalAllocationPhase::ContactedNoOccurrence
        | ExternalAllocationPhase::Terminated => Ok(ExternalCandidateStartProgress::CleanupProved),
        ExternalAllocationPhase::Reserved => {
            bail!("external placement start retained an unclaimed reservation")
        }
    }
}

fn classify_external_start_failure(
    state: &AppState,
    placement: &str,
    source: anyhow::Error,
) -> ExternalCandidateStartFailure {
    let cleanup = classify_external_start_cleanup(&state.state_store, placement);
    ExternalCandidateStartFailure { source, cleanup }
}

fn classify_external_start_cleanup(
    state_store: &crate::state_store::StateStore,
    placement: &str,
) -> ExternalCandidateStartCleanup {
    match state_store.external_allocation(placement) {
        Ok(None) => ExternalCandidateStartCleanup::Proved,
        Ok(Some(record)) if record.phase == ExternalAllocationPhase::Reserved => {
            match state_store
                .cancel_uncontacted_external_allocation(placement)
                .and_then(|()| {
                    state_store
                        .external_allocation(placement)?
                        .context("cancelled external reservation disappeared")
                }) {
                Ok(settled) if settled.phase == ExternalAllocationPhase::NoContact => {
                    ExternalCandidateStartCleanup::Proved
                }
                _ => ExternalCandidateStartCleanup::Unproved,
            }
        }
        Ok(Some(record)) if record.phase.is_settled() => ExternalCandidateStartCleanup::Proved,
        Ok(Some(_)) | Err(_) => ExternalCandidateStartCleanup::Unproved,
    }
}

fn require_fresh_contact_owner(
    session: &crate::runtime_db::DedicatedSessionRecord,
    workspace: &WorkspaceRecord,
) -> Result<()> {
    ensure!(
        session.state == "admitted"
            && session.send_boundary == "none"
            && matches!(
                workspace.state,
                WorkspaceState::Ready | WorkspaceState::Active
            ),
        "new external contact requires an unreleased admitted session and contactable workspace; \
         session_state={}, send_boundary={}, workspace_state={}",
        session.state,
        session.send_boundary,
        workspace.state,
    );
    Ok(())
}

fn select_binding<'a>(
    bindings: &'a [InstalledExternalExecutionBinding],
    program: &ryeos_state::external_execution::admission::AdmittedExternalCandidateProgram,
) -> Result<&'a InstalledExternalExecutionBinding> {
    let matched = bindings
        .iter()
        .filter(|binding| binding.check_program(program).is_ok())
        .collect::<Vec<_>>();
    match matched.as_slice() {
        [binding] => Ok(*binding),
        [] => bail!("no installed external placement binding admits the exact program"),
        _ => bail!("external placement program matches multiple installed bindings"),
    }
}

fn require_current_contact_binding<'a>(
    bindings: &'a [InstalledExternalExecutionBinding],
    program: &ryeos_state::external_execution::admission::AdmittedExternalCandidateProgram,
    retained_binding_hash: &str,
) -> Result<&'a InstalledExternalExecutionBinding> {
    let installed = select_binding(bindings, program)?;
    ensure!(
        installed.digest() == retained_binding_hash,
        "removed or rotated placement binding cannot authorize first contact"
    );
    Ok(installed)
}

struct QualificationActivationAdmission {
    state: AppState,
    root:
        crate::operator_external_content::product_qualification::PreparedExternalQualificationRoot,
}

pub(crate) struct PreparedExternalPlacement {
    state_store: Arc<crate::state_store::StateStore>,
    vault: Arc<dyn NodeVault>,
    backend: Arc<dyn ExternalPlacementBackend>,
    contract: ExternalPlacementBackendContract,
    credential: PlacementCredential,
    contact_gate: Arc<AtomicBool>,
    controller_lifetime: Arc<StateLockLease>,
    channel_authority: ExternalChannelAuthority,
    // Sessions reload their capsule and retained product selections. Direct
    // recovery uses compiler-attested program bytes in the immutable allocation
    // and rejoins the original born capsule; it cannot reconstruct fresh inputs.
    program: AdmittedExternalExecutionProgram,
    guest_inputs: Option<ryeos_external_execution::guest_inputs::ExternalGuestInputAuthority>,
    qualification_activation: Option<QualificationActivationAdmission>,
    runtime_upload: Option<crate::guest_runtime_upload::GuestRuntimeUploadBinding>,
    native_journal: crate::external_supervisor_native::SupervisorJournalEnvelopeSource,
    record: ExternalAllocationRecord,
}

impl PreparedExternalPlacement {
    fn with_guest_inputs(
        mut self,
        guest_inputs: ryeos_external_execution::guest_inputs::ExternalGuestInputAuthority,
        runtime_upload: Option<crate::guest_runtime_upload::GuestRuntimeUploadBinding>,
    ) -> Result<Self> {
        ensure!(
            self.record.phase == ExternalAllocationPhase::Bound,
            "external guest inputs may be supplied only for one bound occurrence"
        );
        ensure!(
            guest_inputs.projection().base_snapshot.snapshot_hash
                == self.record.reservation.base_snapshot_hash,
            "external guest inputs changed the reserved base snapshot"
        );
        self.program
            .validate_guest_inputs(guest_inputs.projection())?;
        ensure!(
            self.state_store
                .external_supervisor_activation(&self.record.reservation.placement_thread_id)?
                .is_none(),
            "external guest inputs cannot be supplied to activation reconciliation"
        );
        self.guest_inputs = Some(guest_inputs);
        self.runtime_upload = runtime_upload;
        Ok(self)
    }

    /// Consume preparation and durably decide whether this process owns the
    /// one allowed allocator call. A false claim is reconciliation authority,
    /// never permission to allocate again.
    pub(crate) fn claim(self) -> Result<ExternalPlacementContactDecision> {
        let qualification_recovery = matches!(
            &self.record.reservation.owner,
            ExternalAllocationOwner::QualificationCandidate(_)
        );
        ensure!(
            !qualification_recovery || self.record.phase != ExternalAllocationPhase::Reserved,
            "qualification first contact requires its accepted-root claim"
        );
        self.claim_with(|prepared| {
            let claim = prepared.state_store.claim_external_allocation_contact(
                &prepared.record.reservation.placement_thread_id,
                &prepared.record.reservation.request_digest,
            )?;
            if qualification_recovery {
                let record = match &claim {
                    ExternalAllocationContactClaim::Reconcile(record)
                    | ExternalAllocationContactClaim::Settled(record) => record,
                    ExternalAllocationContactClaim::Contact(_) => {
                        bail!("qualification recovery cannot create a provider contact")
                    }
                };
                ensure!(
                    record.reservation == prepared.record.reservation,
                    "qualification recovery changed its retained request"
                );
            }
            Ok(claim)
        })
    }

    /// Claim first contact for the exact accepted qualification root and
    /// current signed placement binding. Later phases only recover the retained
    /// journal obligation. The caller must hold the qualification occurrence
    /// contact gate throughout this claim and any returned
    /// `ExternalPlacementContactPermit::contact` call.
    pub(crate) fn claim_qualification(
        self,
        state: &AppState,
        admission: crate::operator_external_content::product_qualification::AuthenticatedConsumerRoot,
        binding: &RetainedExternalExecutionBinding,
    ) -> Result<ExternalPlacementContactDecision> {
        ensure!(
            matches!(
                &self.record.reservation.owner,
                ExternalAllocationOwner::QualificationCandidate(_)
            ),
            "qualification contact claim requires a qualification allocation"
        );
        self.record.reservation.validate()?;
        binding.validate()?;
        ensure!(
            binding.digest() == self.record.reservation.binding_hash
                && binding.capacity_owner() == self.record.reservation.capacity_owner,
            "qualification contact claim changed its retained signed binding"
        );
        self.claim_with(|prepared| {
            let reservation = &prepared.record.reservation;
            let claim = if prepared.record.phase == ExternalAllocationPhase::Reserved {
                prepared
                    .state_store
                    .claim_external_qualification_allocation_contact(
                        state,
                        &reservation.placement_thread_id,
                        &reservation.request_digest,
                        binding,
                        admission,
                    )?
            } else {
                // The ordinary journal claim cannot grant first Q contact:
                // this branch starts from a retained, already-contacted Q owner.
                prepared.state_store.claim_external_allocation_contact(
                    &reservation.placement_thread_id,
                    &reservation.request_digest,
                )?
            };
            let current = match &claim {
                ExternalAllocationContactClaim::Contact(record)
                | ExternalAllocationContactClaim::Reconcile(record)
                | ExternalAllocationContactClaim::Settled(record) => record,
            };
            ensure!(
                current.reservation == *reservation,
                "qualification contact claim changed its exact reserved request"
            );
            ensure!(
                prepared.record.phase == ExternalAllocationPhase::Reserved
                    || !matches!(&claim, ExternalAllocationContactClaim::Contact(_)),
                "qualification recovery cannot create a second provider contact"
            );
            Ok(claim)
        })
    }

    fn claim_with(
        self,
        claim: impl FnOnce(&Self) -> Result<ExternalAllocationContactClaim>,
    ) -> Result<ExternalPlacementContactDecision> {
        // A Reserved record can be re-entered after daemon restart without
        // passing through fresh preparation. Check the exact retained
        // executable floor at the final pre-contact boundary as well. Never
        // impose a new startup refusal on an already-contacted obligation;
        // those paths must remain able to reconcile and clean up.
        if self.record.phase == ExternalAllocationPhase::Reserved {
            require_guest_package_artifact_floor(&self.contract, self.backend.as_ref())?;
        }
        ensure!(
            self.contact_gate
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok(),
            "external placement contact decision is already in progress"
        );
        let claim = match claim(&self) {
            Ok(claim) => claim,
            Err(error) => {
                self.contact_gate.store(false, Ordering::Release);
                return Err(error);
            }
        };
        match claim {
            ExternalAllocationContactClaim::Contact(record) => Ok(
                ExternalPlacementContactDecision::Contact(ExternalPlacementContactPermit {
                    state_store: self.state_store,
                    backend: self.backend,
                    contract: self.contract,
                    credential: self.credential,
                    reservation: record.reservation,
                    contact_lease: ExternalContactLease {
                        gate: self.contact_gate,
                    },
                    _controller_lifetime: self.controller_lifetime,
                }),
            ),
            ExternalAllocationContactClaim::Reconcile(record) => {
                self.contact_gate.store(false, Ordering::Release);
                Ok(ExternalPlacementContactDecision::Reconcile(
                    ExternalPlacementReconciliation {
                        state_store: self.state_store,
                        vault: self.vault,
                        backend: self.backend,
                        contract: self.contract,
                        credential: self.credential,
                        contact_gate: self.contact_gate,
                        channel_authority: self.channel_authority,
                        program: self.program,
                        guest_inputs: self.guest_inputs,
                        qualification_activation: self.qualification_activation,
                        runtime_upload: self.runtime_upload,
                        native_journal: self.native_journal,
                        _controller_lifetime: self.controller_lifetime,
                        record,
                    },
                ))
            }
            ExternalAllocationContactClaim::Settled(record) => {
                self.contact_gate.store(false, Ordering::Release);
                Ok(ExternalPlacementContactDecision::Settled(record))
            }
        }
    }
}

pub(crate) enum ExternalPlacementContactDecision {
    Contact(ExternalPlacementContactPermit),
    Reconcile(ExternalPlacementReconciliation),
    Settled(ExternalAllocationRecord),
}

/// Process-local, non-cloneable and non-serializable proof of the single
/// durable contact claim. Provider adapters consume it; workers never see it.
pub(crate) struct ExternalPlacementContactPermit {
    state_store: Arc<crate::state_store::StateStore>,
    backend: Arc<dyn ExternalPlacementBackend>,
    contract: ExternalPlacementBackendContract,
    credential: PlacementCredential,
    reservation: ExternalAllocationReservation,
    contact_lease: ExternalContactLease,
    // Keep the exact OS-backed controller exclusion live across synchronous
    // provider I/O. Tokio's bounded shutdown cannot cancel spawn_blocking.
    _controller_lifetime: Arc<StateLockLease>,
}

/// Existing or uncertain contact must be observed through this exact retained
/// authority. It deliberately has no conversion into a new contact permit.
pub(crate) struct ExternalPlacementReconciliation {
    state_store: Arc<crate::state_store::StateStore>,
    vault: Arc<dyn NodeVault>,
    backend: Arc<dyn ExternalPlacementBackend>,
    contract: ExternalPlacementBackendContract,
    credential: PlacementCredential,
    contact_gate: Arc<AtomicBool>,
    channel_authority: ExternalChannelAuthority,
    program: AdmittedExternalExecutionProgram,
    guest_inputs: Option<ryeos_external_execution::guest_inputs::ExternalGuestInputAuthority>,
    qualification_activation: Option<QualificationActivationAdmission>,
    runtime_upload: Option<crate::guest_runtime_upload::GuestRuntimeUploadBinding>,
    native_journal: crate::external_supervisor_native::SupervisorJournalEnvelopeSource,
    // Reconciliation and termination are provider mutations/observations too;
    // they must not overlap a replacement controller generation.
    _controller_lifetime: Arc<StateLockLease>,
    record: ExternalAllocationRecord,
}

struct ExternalContactLease {
    gate: Arc<AtomicBool>,
}

impl Drop for ExternalContactLease {
    fn drop(&mut self) {
        self.gate.store(false, Ordering::Release);
    }
}

// Access remains inside this module until a lifecycle adapter consumes these
// exact types. Keeping the fields live now prevents a future adapter from
// reconstructing authority from public hashes or caller input.
fn require_timely_lifecycle_observation(
    deadline_exceeded: bool,
    deadline: lillux::time::MonotonicDeadline,
) -> Result<()> {
    if deadline_exceeded || deadline.has_elapsed() {
        // The state owner has atomically retained evidence and fenced reentry.
        bail!("external lifecycle observation exceeded its admitted contact deadline");
    }
    Ok(())
}

impl ExternalPlacementContactPermit {
    pub(crate) fn contact(
        self,
        live_deadline: lillux::time::MonotonicDeadline,
    ) -> Result<ExternalAllocationRecord> {
        let deadline = self.reservation.startup_deadline()?.min(live_deadline);
        ensure!(
            !deadline.has_elapsed(),
            "external startup live deadline expired before allocation"
        );
        let ExternalLifecycleObservation {
            value: resolution,
            deadline_exceeded,
        } = self.backend.allocate(
            &self.contract,
            &self.credential,
            &self.reservation,
            deadline,
        )?;
        require_allocation_resolution_capability(self.backend.as_ref(), &resolution, false)?;
        let contact_gate = self.contact_lease.gate.clone();
        let mut contact_lease = Some(self.contact_lease);
        // Positive or pending observations retain exclusion through durable
        // settlement. Only an exact negative response consumes the lease first:
        // the original request has returned and the negative-settlement owner
        // must still refuse if a successor has since acquired the gate.
        if matches!(
            &resolution,
            ExternalAllocationResolution::NoOccurrence { .. }
        ) {
            drop(contact_lease.take());
        }
        let record = apply_reconciliation_resolution(
            &self.state_store,
            &contact_gate,
            &self.reservation,
            resolution,
            ExternalObservationTiming::Startup {
                deadline_exceeded,
                live_deadline: deadline,
            },
        )?;
        // Preserve exact late observations before refusing any continuation.
        require_timely_lifecycle_observation(deadline_exceeded, deadline)?;
        record.reservation.startup_deadline()?;
        Ok(record)
    }
}

impl ExternalPlacementReconciliation {
    pub(crate) fn reconcile(self) -> Result<ExternalAllocationRecord> {
        // Cleanup reconciliation deliberately has its own contact allowance.
        let deadline = lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(
            u64::from(self.contract.contact_timeout_seconds),
        ));
        // A complete late observation still identifies the existing cleanup
        // obligation (or proves no occurrence). It does not authorize startup.
        Ok(self
            .reconcile_with_deadline(deadline, ExternalObservationTiming::Cleanup)?
            .value)
    }

    fn reconcile_with_deadline(
        self,
        deadline: lillux::time::MonotonicDeadline,
        timing: ExternalObservationTiming,
    ) -> Result<ExternalLifecycleObservation<ExternalAllocationRecord>> {
        ensure!(
            self.contact_gate
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok(),
            "external allocation reconciliation raced an active contact permit"
        );
        let mut contact_lease = Some(ExternalContactLease {
            gate: self.contact_gate.clone(),
        });
        let ExternalLifecycleObservation {
            value: resolution,
            deadline_exceeded,
        } = self.backend.reconcile_allocation(
            &self.contract,
            &self.credential,
            &self.record.reservation,
            deadline,
        )?;
        require_allocation_resolution_capability(self.backend.as_ref(), &resolution, true)?;
        if matches!(
            &resolution,
            ExternalAllocationResolution::NoOccurrence { .. }
        ) {
            // Consuming the lease prevents a later destructor from clearing a
            // successor's gate after the negative-settlement check.
            drop(contact_lease.take());
        }
        let record = apply_reconciliation_resolution(
            &self.state_store,
            &self.contact_gate,
            &self.record.reservation,
            resolution,
            timing.with_deadline_exceeded(deadline_exceeded),
        )?;
        Ok(ExternalLifecycleObservation {
            value: record,
            deadline_exceeded,
        })
    }

    /// Start the protected supervisor exactly once after allocation has bound
    /// an occurrence. An exact replay or daemon restart observes the retained
    /// request and can only reconcile that mutation.
    #[cfg(test)]
    pub(crate) fn activate_or_reconcile(mut self) -> Result<ExternalSupervisorActivationRecord> {
        self.activate_or_reconcile_retained(self.record.reservation.startup_deadline()?)
    }

    /// Advance activation and readiness while retaining the non-serializable
    /// owner signer inside this placement owner.  An attached channel is not
    /// executable authority: only an applied signed Ready plus the exact
    /// controller Release may produce `Ready`.
    fn advance_start(
        mut self,
        live_deadline: lillux::time::MonotonicDeadline,
    ) -> Result<ExternalCandidateStartProgress> {
        let activation = self.activate_or_reconcile_retained(live_deadline)?;
        let placement = &self.record.reservation.placement_thread_id;
        let Some(binding) = self
            .state_store
            .optional_external_execution_channel(placement)?
        else {
            return match activation
                .observation
                .as_ref()
                .map(|observation| observation.activation_state.as_str())
            {
                Some("started") => Ok(ExternalCandidateStartProgress::AttachmentPending),
                Some("not_started") => Ok(ExternalCandidateStartProgress::CleanupRequired),
                None => Ok(ExternalCandidateStartProgress::SupervisorPending),
                Some(_) => bail!("external supervisor activation retained an unknown observation"),
            };
        };
        ensure!(
            activation
                .observation
                .as_ref()
                .is_none_or(|observation| observation.activation_state == "started"),
            "attached external channel contradicts supervisor activation"
        );
        let release = if matches!(
            &self.record.reservation.owner,
            ExternalAllocationOwner::QualificationCandidate(_)
        ) {
            let proof = self
                .qualification_activation
                .as_ref()
                .context("qualification Release has no live invoking owner")?;
            self.state_store
                .admit_external_qualification_ready_and_author_release(
                    &proof.state,
                    placement,
                    self.channel_authority.owner_signing_key(),
                    live_deadline,
                    proof.root.authenticate_invocation(&proof.state)?,
                )?
        } else {
            self.state_store.admit_external_ready_and_author_release(
                placement,
                self.channel_authority.owner_signing_key(),
                live_deadline,
            )?
        };
        if release.is_some() {
            Ok(ExternalCandidateStartProgress::Ready(binding))
        } else {
            Ok(ExternalCandidateStartProgress::ChannelAttached)
        }
    }

    fn activate_or_reconcile_retained(
        &mut self,
        live_deadline: lillux::time::MonotonicDeadline,
    ) -> Result<ExternalSupervisorActivationRecord> {
        ensure!(
            self.contact_gate
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok(),
            "external supervisor activation decision is already in progress"
        );
        let _contact_lease = ExternalContactLease {
            gate: self.contact_gate.clone(),
        };
        let placement = &self.record.reservation.placement_thread_id;
        let current = self
            .state_store
            .external_allocation(placement)?
            .context("external allocation disappeared before supervisor activation")?;
        ensure!(
            matches!(
                current.phase,
                ExternalAllocationPhase::Bound | ExternalAllocationPhase::Quarantined
            ),
            "external supervisor activation requires a retained bound occurrence"
        );
        let occurrence = current
            .occurrence
            .as_ref()
            .context("external supervisor activation has no exact occurrence")?;
        validate_external_placement_program(&self.program, &current.reservation, &self.contract)?;
        if let AdmittedExternalExecutionProgram::DirectCommand(program) = &self.program {
            let binding = self
                .state_store
                .retained_external_binding(&current.reservation.binding_hash)?
                .context("external direct activation lost its exact signed binding")?;
            binding.check_direct_program(program)?;
            ensure!(
                binding.backend_contract() == self.contract,
                "external direct activation changed its retained backend contract"
            );
        }
        let retained = self.state_store.external_supervisor_activation(placement)?;
        let (intent, mut activation, owns_contact, deadline) = if let Some(record) = retained {
            record
                .intent
                .validate_contract(&current.reservation, occurrence, &self.contract)?;
            ensure!(
                self.guest_inputs.is_none(),
                "activation reconciliation cannot receive replacement guest input authority"
            );
            if record.observation.is_some() {
                return Ok(record);
            }
            (
                record.intent,
                None,
                false,
                current.reservation.startup_deadline()?.min(live_deadline),
            )
        } else {
            let deadline = current.reservation.startup_deadline()?.min(live_deadline);
            ensure!(
                !deadline.has_elapsed(),
                "external startup live deadline expired before activation intent"
            );
            // Seal the journal envelope and resolve the package parent before
            // consuming guest input authority, so a policy or store refusal
            // leaves that authority with its owner.
            let native_journal = self
                .native_journal
                .envelope(&self.contract, current.reservation.timeout_seconds)?;
            let package_parent = self.state_store.external_guest_package_parent()?;
            let guest_runtime = if self.backend.stages_guest_runtime_upload() {
                let binding = self.runtime_upload.as_ref().context(
                    "external provider stages its guest runtime but no upload source is installed",
                )?;
                let basis = match (
                    &current.reservation.owner,
                    self.qualification_activation.as_ref(),
                ) {
                    (ExternalAllocationOwner::DedicatedSession(_), _) => {
                        crate::guest_runtime_upload::GuestRuntimeBasis::Ordinary
                    }
                    (ExternalAllocationOwner::QualificationCandidate(_), Some(admission)) => {
                        crate::guest_runtime_upload::GuestRuntimeBasis::QualificationCandidate(
                            &admission.root,
                        )
                    }
                    (ExternalAllocationOwner::QualificationCandidate(_), None) => bail!(
                        "qualification activation has no live invoking owner to name its guest runtime"
                    ),
                    (ExternalAllocationOwner::DirectThread { .. }, _) => {
                        bail!("a direct thread has no guest runtime to stage")
                    }
                };
                Some(binding.produce(&self.contract, basis)?)
            } else {
                None
            };
            let guest_inputs = self
                .guest_inputs
                .take()
                .context("first supervisor activation has no exact guest input authority")?;
            let (intent, mut activation) = supervisor_activation(
                &self.contract,
                &current.reservation,
                occurrence,
                &self.channel_authority,
                &self.program,
                guest_inputs,
                self.backend.as_ref(),
                guest_runtime,
                &package_parent,
                deadline,
                native_journal,
            )?;
            if activation.import_authorization.is_some() {
                let assignment = self.state_store.author_external_guest_assignment(
                    &current.reservation,
                    occurrence,
                    &intent,
                    &self.contract,
                );
                let retained =
                    assignment.and_then(|signed| activation.retain_signed_assignment(signed));
                if let Err(error) = retained {
                    if let Err(cleanup) = activation.discard_guest_package() {
                        return Err(error.context(format!(
                            "guest assignment refusal also failed package cleanup: {cleanup:#}"
                        )));
                    }
                    return Err(error);
                }
            }
            let claim = (|| {
                let relay = if matches!(
                    &self.contract.controller_transport,
                    ryeos_state::external_execution::transport::ExternalControllerTransportSelection::ControllerFileRelay(_)
                ) {
                    let bootstrap = activation.bootstrap();
                    let signed = activation.signed_import_authorization()?;
                    signed.validate_shape()?;
                    let bootstrap_sha256 = bootstrap.digest()?;
                    ensure!(
                        signed.authorization.ticket.bootstrap_sha256 == bootstrap_sha256
                            && signed.authorization.ticket.guest_input_identity
                                == intent.guest_input_identity
                            && signed.authorization.ticket.activation_request_digest
                                == intent.activation_request_digest,
                        "original signed guest import changed its relay bootstrap"
                    );
                    let signed_import_bytes = zeroize::Zeroizing::new(
                        ryeos_external_execution_contract::canonical_json(signed)?,
                    );
                    let commitment = ExternalRelayBootstrapCommitment {
                        schema: 1,
                        placement_thread_id: current.reservation.placement_thread_id.clone(),
                        allocation_request_digest: current.reservation.request_digest.clone(),
                        occurrence_id: occurrence.occurrence_id.clone(),
                        activation_request_digest: intent.activation_request_digest.clone(),
                        guest_input_identity: intent.guest_input_identity.clone(),
                        bootstrap_sha256,
                        signed_import_sha256:
                            ryeos_external_execution_contract::lease_ready_inputs::signed_import_sha256(
                                &signed_import_bytes,
                            ),
                    };
                    commitment.validate_for(
                        &current.reservation,
                        occurrence,
                        &intent,
                        &self.contract,
                    )?;
                    let access = ExternalRelayBootstrapAccess::new(
                        placement,
                        &current.reservation.request_digest,
                    )?;
                    self.vault
                        .retain_external_relay_bootstrap(&access, bootstrap, signed)?;
                    Some(commitment)
                } else {
                    None
                };
                if matches!(
                    &current.reservation.owner,
                    ExternalAllocationOwner::QualificationCandidate(_)
                ) {
                    let proof = self
                        .qualification_activation
                        .as_ref()
                        .context("fresh qualification activation has no live invoking owner")?;
                    self.state_store
                        .begin_external_qualification_supervisor_activation(
                            &proof.state,
                            placement,
                            &intent,
                            relay.as_ref(),
                            proof.root.authenticate_invocation(&proof.state)?,
                        )
                } else if let Some(commitment) = relay.as_ref() {
                    self.state_store
                        .begin_external_relay_supervisor_activation(placement, &intent, commitment)
                } else {
                    self.state_store
                        .begin_external_supervisor_activation(placement, &intent)
                }
            })();
            if !matches!(claim.as_ref(), Ok(true)) {
                let cleanup = activation.discard_guest_package();
                if let Err(cleanup) = cleanup {
                    return Err(anyhow::anyhow!(
                        "external activation claim failed and guest package cleanup failed: {claim:?}; {cleanup:#}"
                    ));
                }
                ensure!(
                    claim?,
                    "fresh external activation lost its unique contact claim"
                );
            }
            if let Err(error) = begin_external_lease_supervisor_activation(
                &self.state_store,
                &current.reservation,
                occurrence,
                &intent,
                &activation,
            ) {
                return Err(match activation.discard_guest_package() {
                    Ok(()) => error,
                    Err(cleanup) => error.context(format!(
                        "lease activation refusal also failed guest package cleanup: {cleanup:#}"
                    )),
                });
            }
            (intent, Some(activation), true, deadline)
        };
        if deadline.has_elapsed() {
            if let Some(activation) = activation.as_mut() {
                activation.discard_guest_package().context(
                    "external activation deadline expired and guest package cleanup failed",
                )?;
            }
            bail!("external activation deadline expired before contact");
        }
        let ExternalLifecycleObservation {
            value: resolution,
            deadline_exceeded,
        } = if owns_contact {
            let activation = activation
                .as_mut()
                .expect("fresh activation retains its guest authority");
            let result = self.backend.activate_supervisor(
                &self.contract,
                &self.credential,
                &current.reservation,
                occurrence,
                &intent,
                activation,
                deadline,
            );
            let cleanup = activation.discard_guest_package();
            match (result, cleanup) {
                (Ok(result), Ok(())) => result,
                (Err(error), Ok(())) => return Err(error),
                (Ok(_), Err(error)) => {
                    return Err(error.context(
                        "external guest package cleanup failed after activation contact",
                    ));
                }
                (Err(error), Err(cleanup)) => {
                    return Err(error.context(format!(
                        "external guest package cleanup also failed after activation contact: {cleanup:#}"
                    )));
                }
            }
        } else {
            self.backend.reconcile_supervisor_activation(
                &self.contract,
                &self.credential,
                &current.reservation,
                occurrence,
                &intent,
                deadline,
            )?
        };
        if !owns_contact && !matches!(resolution, ExternalSupervisorActivationResolution::Pending) {
            require_lifecycle_capability(
                self.backend.as_ref(),
                LifecycleCapability::ExactActivationReconciliation,
            )?;
        }
        if let Some((activation_state, provider_observation_digest)) = match resolution {
            ExternalSupervisorActivationResolution::Started {
                provider_observation_digest,
            } => Some(("started", provider_observation_digest)),
            ExternalSupervisorActivationResolution::NotStarted {
                provider_observation_digest,
            } => Some(("not_started", provider_observation_digest)),
            ExternalSupervisorActivationResolution::Pending => None,
        } {
            self.state_store.settle_external_supervisor_activation(
                placement,
                &ExternalSupervisorActivationObservation {
                    schema: 1,
                    binding_hash: current.reservation.binding_hash.clone(),
                    request_digest: current.reservation.request_digest.clone(),
                    occurrence_id: occurrence.occurrence_id.clone(),
                    activation_request_digest: intent.activation_request_digest.clone(),
                    activation_state: activation_state.into(),
                    provider_observation_digest,
                },
                ExternalObservationTiming::Startup {
                    deadline_exceeded,
                    live_deadline: deadline,
                },
            )?;
        } else {
            self.state_store.observe_external_lifecycle_pending(
                placement,
                ExternalObservationTiming::Startup {
                    deadline_exceeded,
                    live_deadline: deadline,
                },
            )?;
        }
        // A late response is still durable lifecycle evidence, never proof
        // that no supervisor was started. Retain it before refusing Release.
        require_timely_lifecycle_observation(deadline_exceeded, deadline)?;
        current.reservation.startup_deadline()?;
        self.state_store
            .external_supervisor_activation(placement)?
            .context("external supervisor activation intent disappeared")
    }

    /// Start the exact termination request once, or reconcile it after
    /// restart. The durable intent is committed before the adapter mutation.
    pub(crate) fn terminate_or_reconcile(self) -> Result<ExternalAllocationRecord> {
        ensure!(
            self.contact_gate
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok(),
            "external placement lifecycle decision is already in progress"
        );
        let _contact_lease = ExternalContactLease {
            gate: self.contact_gate.clone(),
        };
        let placement = &self.record.reservation.placement_thread_id;
        let current = self
            .state_store
            .external_allocation(placement)?
            .context("external allocation disappeared before cleanup")?;
        if current.phase == ExternalAllocationPhase::Terminated {
            return Ok(current);
        }
        let occurrence = current
            .occurrence
            .as_ref()
            .context("external cleanup has no exact occurrence")?;
        let termination_request_digest =
            ryeos_state::objects::canonical_value_digest(&serde_json::json!({
                "domain":"ryeos.external-placement-termination.v1",
                "binding_hash":current.reservation.binding_hash,
                "request_digest":current.reservation.request_digest,
                "occurrence_id":occurrence.occurrence_id,
            }))?;
        let intent = ExternalTerminationIntent {
            schema: 1,
            binding_hash: current.reservation.binding_hash.clone(),
            request_digest: current.reservation.request_digest.clone(),
            occurrence_id: occurrence.occurrence_id.clone(),
            termination_request_digest,
        };
        let owns_contact = self
            .state_store
            .begin_external_termination(placement, &intent)?;
        let resolution = if owns_contact {
            self.backend.terminate(
                &self.contract,
                &self.credential,
                &current.reservation,
                occurrence,
                &intent,
            )?
        } else {
            self.backend.reconcile_termination(
                &self.contract,
                &self.credential,
                &current.reservation,
                occurrence,
                &intent,
            )?
        };
        if let ExternalTerminationResolution::Terminal {
            provider_observation_digest,
        } = resolution
        {
            require_lifecycle_capability(
                self.backend.as_ref(),
                LifecycleCapability::ExactTerminalObservation,
            )?;
            self.state_store.settle_external_terminal(
                placement,
                &ExternalTerminalObservation {
                    schema: 1,
                    binding_hash: current.reservation.binding_hash.clone(),
                    request_digest: current.reservation.request_digest.clone(),
                    occurrence_id: occurrence.occurrence_id.clone(),
                    termination_request_digest: intent.termination_request_digest,
                    terminal_state: "terminated".into(),
                    provider_observation_digest,
                },
            )?;
        }
        self.state_store
            .external_allocation(placement)?
            .context("external allocation disappeared after cleanup observation")
    }
}

/// Read and decode the binding's protected placement credential.
fn read_placement_credential(
    state: &AppState,
    access: &crate::vault::placement::PlacementCredentialAccess,
) -> Result<PlacementCredential> {
    access.decode(
        state
            .vault
            .placement_credential(access)
            .context("read protected external placement credential")?,
    )
}

/// A fresh placement's lease admission, then its placement credential, in
/// that order. Admission runs through the exact installed backend generation
/// the binding names and needs only the session's profile, its deadline, the
/// binding and node policy, so a refused lease returns before the vault is
/// touched. `session_deadline` is asked only when a lease is needed. The
/// caller still fully qualifies the backend with the credential afterwards.
fn admit_fresh_lease_then_read_credential(
    state: &AppState,
    profile_contract: Option<&serde_json::Value>,
    ready_timeout_ms: u64,
    session_deadline: impl FnOnce() -> Result<Option<i64>>,
    contract: &ExternalPlacementBackendContract,
    credential_access: &crate::vault::placement::PlacementCredentialAccess,
) -> Result<(
    Option<(
        String,
        crate::runtime_db::external_execution::ExternalLeaseCharge,
    )>,
    PlacementCredential,
)> {
    let installed = state.external_placement_backends.installed(contract)?;
    let lease = admit_session_lease(
        state,
        profile_contract,
        ready_timeout_ms,
        session_deadline,
        contract,
        installed.as_ref(),
    )?;
    let credential = read_placement_credential(state, credential_access)?;
    Ok((lease, credential))
}

/// Stage-one lease admission for a fresh placed session: the binding's
/// adapter judges the lease [`session_lease_request`] builds, offline. Returns
/// the admitted request bytes, encoded for the reservation, and the lease's
/// charge against the node budget at the budget weight of the provider spec
/// that admitted it, which the reservation takes in its own transaction. A
/// session with no structured-session profile needs no lease. A session whose
/// deadline the lease could not cover is refused before the adapter is asked.
fn admit_session_lease(
    state: &AppState,
    profile_contract: Option<&serde_json::Value>,
    ready_timeout_ms: u64,
    session_deadline: impl FnOnce() -> Result<Option<i64>>,
    contract: &ExternalPlacementBackendContract,
    backend: &dyn ExternalPlacementBackend,
) -> Result<
    Option<(
        String,
        crate::runtime_db::external_execution::ExternalLeaseCharge,
    )>,
> {
    let Some(profile_contract) = profile_contract else {
        return Ok(None);
    };
    let budget = state
        .node_policy
        .require::<ryeos_node_runtime::node_policy::sections::execution::NodeExecutionAdmissionPolicy>()?
        .external_lease_budget
        .as_ref();
    let Some((inputs, charge)) =
        session_lease_request(profile_contract, ready_timeout_ms, contract, budget)?
    else {
        return Ok(None);
    };
    let margin_seconds = budget
        .map(|budget| budget.turn_end_margin_seconds)
        .context("lease refused: this node's execution policy declares no external lease budget")?;
    lease_lifecycle::refuse_session_unfit_for_fresh_lease(
        session_deadline()?,
        charge.lifetime_seconds,
        margin_seconds,
        lillux::time::timestamp_millis(),
    )?;
    let admitted = backend.admit_lease(
        &inputs,
        lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(u64::from(
            contract.contact_timeout_seconds,
        ))),
    )?;
    let charge = charge
        .weighted_by(admitted.budget_weight)
        .with_context(|| format!("lease refused for workload `{}`", inputs.workload))?;
    Ok(Some((
        ExternalAllocationReservation::encode_admitted_adapter_request(&admitted.request)?,
        charge,
    )))
}

/// The lease a placed session needs, if any, and its charge before the
/// admitting provider spec's budget weight is known.
///
/// Only a session whose admitted structured-session profile names `lease`
/// or `guest` requirements needs a lease; any other is placed without one.
/// The request is built from signed data alone: the profile's workload class
/// and requirement lists, the one host the guest may reach (the binding's
/// HTTPS controller origin), the binding's execution timeout as the lease
/// lifetime and the session's readiness budget as its activation deadline.
/// The node's lease budget must exist and allow that lifetime.
fn session_lease_request(
    profile_contract: &serde_json::Value,
    ready_timeout_ms: u64,
    contract: &ExternalPlacementBackendContract,
    budget: Option<&ryeos_node_runtime::node_policy::sections::execution::ExternalLeaseBudget>,
) -> Result<
    Option<(
        crate::external_lifecycle_adapter::lease_admission::LeaseAdmissionInputs,
        crate::runtime_db::external_execution::UnweightedLeaseCharge,
    )>,
> {
    let Some(requirements) =
        ryeos_engine::structured_session_profile::placement_requirements(profile_contract)?
    else {
        return Ok(None);
    };
    if requirements.lease.is_empty() && requirements.guest.is_empty() {
        return Ok(None);
    }
    let workload = requirements.workload_class;
    let budget = budget.with_context(|| {
        format!(
            "lease refused: workload `{workload}` needs a remote lease and this node's \
             execution policy declares no external lease budget"
        )
    })?;
    let charge = crate::runtime_db::external_execution::UnweightedLeaseCharge::under(
        budget,
        contract.timeout_seconds,
    )
    .with_context(|| format!("lease refused for workload `{workload}`"))?;
    let origin = &contract
        .controller_transport
        .http()
        .with_context(|| {
            format!(
                "lease refused: workload `{workload}` needs a lease guest that reaches its \
                 controller, and the binding names no HTTPS controller origin"
            )
        })?
        .https_origin;
    let controller_host =
        crate::external_lifecycle_adapter::lease_admission::LeaseAdmissionInputs::controller_host_of(
            origin,
        )?;
    let inputs = crate::external_lifecycle_adapter::lease_admission::LeaseAdmissionInputs {
        workload,
        requires_before_spend: requirements.lease,
        requires_from_guest: requirements.guest,
        controller_host,
        maximum_lifetime_seconds: contract.timeout_seconds,
        activation_deadline_seconds: u32::try_from(ready_timeout_ms.div_ceil(1_000))
            .context("lease activation deadline exceeds its bound")?,
    };
    Ok(Some((inputs, charge)))
}

/// Refuse a fresh allocation that cannot possibly carry its two exact
/// executable roots. The occurrence-bound bootstrap and complete inventory
/// are still checked when the final package is prepared after allocation;
/// this lower bound is only an early, provider-contact-free refusal.
fn require_guest_package_artifact_floor(
    contract: &ExternalPlacementBackendContract,
    backend: &dyn ExternalPlacementBackend,
) -> Result<()> {
    let (_, supervisor_bytes) = backend.supervisor_artifact();
    let (_, launcher_bytes) = backend.launcher_artifact();
    require_guest_package_artifact_floor_bytes(
        contract.max_guest_package_regular_bytes,
        supervisor_bytes,
        launcher_bytes,
    )
}

pub(crate) fn require_guest_package_artifact_floor_bytes(
    maximum_regular_bytes: u64,
    supervisor_bytes: u64,
    launcher_bytes: u64,
) -> Result<()> {
    let executable_bytes = supervisor_bytes
        .checked_add(launcher_bytes)
        .context("external guest executable byte floor overflow")?;
    ensure!(
        executable_bytes <= maximum_regular_bytes,
        "external guest executable roots exceed the signed regular package budget before allocation"
    );
    Ok(())
}

#[cfg(test)]
mod session_lease_tests;

#[cfg(test)]
pub(crate) mod compose_test_support;

#[cfg(test)]
mod channel_stream_admission_tests;

#[cfg(test)]
mod lease_ready_loopback_tests;

mod lease_lifecycle;
mod lease_ready_gate;
pub(crate) use lease_lifecycle::admit_turn_on_external_lease;
pub use lease_lifecycle::{
    PREDECESSOR_LEASE_INTERRUPTION, PredecessorLeaseTermination,
    terminate_predecessor_external_leases,
};
pub use lease_ready_gate::ExternalChannelExchangeUnavailable;

#[cfg(test)]
mod guest_package_artifact_floor_tests {
    use super::require_guest_package_artifact_floor_bytes;

    #[test]
    fn refuses_impossible_signed_budget_before_allocation() {
        assert!(require_guest_package_artifact_floor_bytes(9, 4, 5).is_ok());
        assert!(require_guest_package_artifact_floor_bytes(8, 4, 5).is_err());
        assert!(require_guest_package_artifact_floor_bytes(u64::MAX, u64::MAX, 1).is_err());
    }
}

/// Mark the lease record of a placement that holds an admitted lease request
/// activated, binding the lease's identity and the activation's digests. The
/// lease is the bound occurrence; the guest package digest is the payload
/// the supervisor's activation delivers, and the signed import digest is the
/// canonical digest of the import authorization the guest verifies. The ready
/// deadline is the session's startup deadline. A placement without an
/// admitted lease request has no lease record and nothing to mark.
fn begin_external_lease_supervisor_activation(
    state_store: &Arc<crate::state_store::StateStore>,
    reservation: &ExternalAllocationReservation,
    occurrence: &ExternalAllocationOccurrence,
    intent: &ExternalSupervisorActivationIntent,
    activation: &ExternalSupervisorActivation,
) -> Result<()> {
    if reservation.admitted_adapter_request_base64.is_none() {
        return Ok(());
    }
    state_store.activate_external_lease_record(
        &reservation.placement_thread_id,
        &external_lease_activation(reservation, occurrence, intent, activation)?,
    )
}

/// The digests and deadline the lease record binds at activation. The signed
/// import digest covers the exact canonical bytes delivered to the guest,
/// which the guest accepts only in the contract's canonical encoding (struct
/// field order), not the key-sorted encoding used for content addressing.
fn external_lease_activation(
    reservation: &ExternalAllocationReservation,
    occurrence: &ExternalAllocationOccurrence,
    intent: &ExternalSupervisorActivationIntent,
    activation: &ExternalSupervisorActivation,
) -> Result<crate::runtime_db::external_execution::ExternalLeaseActivation> {
    let signed_import = ryeos_external_execution_contract::canonical_json(
        activation.signed_import_authorization()?,
    )?;
    Ok(
        crate::runtime_db::external_execution::ExternalLeaseActivation {
            lease_id: occurrence.occurrence_id.clone(),
            activation_request_digest: intent.activation_request_digest.clone(),
            signed_import_sha256:
                ryeos_external_execution_contract::lease_ready_inputs::signed_import_sha256(
                    &signed_import,
                ),
            guest_package_sha256: intent.delivery.payload_sha256.clone(),
            ready_deadline_ms: reservation.startup_deadline_ms,
        },
    )
}

fn supervisor_activation(
    contract: &ExternalPlacementBackendContract,
    reservation: &ExternalAllocationReservation,
    occurrence: &ExternalAllocationOccurrence,
    authority: &ExternalChannelAuthority,
    program: &AdmittedExternalExecutionProgram,
    guest_inputs: ryeos_external_execution::guest_inputs::ExternalGuestInputAuthority,
    backend: &dyn ExternalPlacementBackend,
    guest_runtime: Option<
        ryeos_external_execution::guest_runtime_product::GuestOwnerSnapshotUpload,
    >,
    package_parent: &lillux::PinnedDirectory,
    deadline: lillux::time::MonotonicDeadline,
    native_journal: ryeos_state::external_execution::transport::ExternalSupervisorNativeEnvelope,
) -> Result<(
    ExternalSupervisorActivationIntent,
    ExternalSupervisorActivation,
)> {
    ensure!(
        authority.generation() == reservation.channel_authority_generation
            && authority.owner_public_key() == reservation.channel_owner_public_key
            && authority.bootstrap_capability_hash()
                == reservation.channel_bootstrap_capability_hash,
        "external supervisor activation changed its retained channel authority"
    );
    let attachment_deadline_ms = reservation
        .contact_deadline_ms
        .checked_add(i64::from(contract.observation_timeout_seconds) * 1_000)
        .context("external supervisor attachment deadline overflow")?;
    let channel_max_bytes = contract.max_transfer_bytes.min(64 * 1024 * 1024);
    let post_execution_timeout_seconds = contract
        .observation_timeout_seconds
        .checked_add(contract.cleanup_timeout_seconds)
        .context("external supervisor post-execution timeout overflow")?;
    validate_external_placement_program(program, reservation, contract)?;
    program.validate_guest_inputs(guest_inputs.projection())?;
    let supervisor_runtime_hash = program.runtime_manifest_hash()?.to_owned();
    let guest_input_identity = guest_inputs.identity_digest()?;
    ensure!(
        guest_inputs.projection().base_snapshot.snapshot_hash == reservation.base_snapshot_hash,
        "external supervisor guest inputs changed the reserved base snapshot"
    );
    let bootstrap = ryeos_state::external_execution::transport::ExternalSupervisorBootstrap {
        schema: match &contract.controller_transport {
            ryeos_state::external_execution::transport::ExternalControllerTransportSelection::HttpJson(_) => ryeos_state::external_execution::transport::EXTERNAL_SUPERVISOR_HTTP_BOOTSTRAP_SCHEMA,
            ryeos_state::external_execution::transport::ExternalControllerTransportSelection::ControllerFileRelay(_) => match program.execution_mode() {
                ryeos_external_execution_contract::ExternalExecutionMode::QualificationStructuredSession {} => ryeos_state::external_execution::transport::EXTERNAL_SUPERVISOR_RELAY_BOOTSTRAP_SCHEMA,
                ryeos_external_execution_contract::ExternalExecutionMode::StructuredSession {} => ryeos_state::external_execution::transport::EXTERNAL_SUPERVISOR_ORDINARY_RELAY_BOOTSTRAP_SCHEMA,
                ryeos_external_execution_contract::ExternalExecutionMode::DirectCommand { .. } => 0,
            },
        },
        controller: contract.controller_transport.clone(),
        tls_root_certificates_der_base64: contract
            .controller_tls_root_certificates_der_base64
            .clone(),
        placement_thread_id: reservation.placement_thread_id.clone(),
        occurrence_id: occurrence.occurrence_id.clone(),
        allocation_request_digest: reservation.request_digest.clone(),
        admitted_capsule_hash: reservation.admitted_capsule_hash.clone(),
        base_snapshot_hash: reservation.base_snapshot_hash.clone(),
        execution_binding_hash: reservation.binding_hash.clone(),
        supervisor_runtime_hash: supervisor_runtime_hash.clone(),
        launcher_artifact_hash: contract.launcher_artifact_hash.clone(),
        candidate_program: program.clone(),
        guest_input_identity: guest_input_identity.clone(),
        guest_inputs: guest_inputs.projection().clone(),
        owner_public_key: reservation.channel_owner_public_key.clone(),
        bootstrap_capability: authority.bootstrap_capability().to_owned(),
        attachment_deadline_ms,
        execution_timeout_seconds: reservation.timeout_seconds,
        post_execution_timeout_seconds,
        candidate_export_max_bytes: contract.max_export_bytes.min(channel_max_bytes),
        channel_max_bytes,
        native_journal,
        // A guest on a remote lease sends a capability report the lease's
        // lifecycle adapter judges, so exactly a reservation that holds an
        // admitted lease request asks for one.
        report_capabilities: reservation.admitted_adapter_request_base64.is_some(),
    };
    bootstrap.validate()?;
    let activation_request_digest =
        crate::runtime_db::external_execution::external_supervisor_activation_request_digest(
            reservation,
            occurrence,
            contract,
            attachment_deadline_ms,
            post_execution_timeout_seconds,
            channel_max_bytes,
            &guest_input_identity,
        )?;
    // A provider that uploads the runtime itself needs its sealed tar, produced
    // by the caller before the claim and before any input lifeline is taken.
    ensure!(
        backend.stages_guest_runtime_upload() == guest_runtime.is_some(),
        "external activation guest runtime upload differs from the provider's declaration"
    );
    let guest_package = backend.prepare_guest_package(
        contract,
        reservation,
        occurrence,
        &bootstrap,
        &guest_inputs,
        &activation_request_digest,
        package_parent,
        deadline,
    )?;
    let delivery = guest_package.commitment().clone();
    let intent = ExternalSupervisorActivationIntent {
        schema: 3,
        binding_hash: reservation.binding_hash.clone(),
        request_digest: reservation.request_digest.clone(),
        occurrence_id: occurrence.occurrence_id.clone(),
        supervisor_runtime_hash,
        guest_input_identity,
        activation_request_digest,
        attachment_deadline_ms,
        execution_timeout_seconds: reservation.timeout_seconds,
        post_execution_timeout_seconds,
        channel_max_bytes,
        delivery,
    };
    if let Err(error) = intent.validate_contract(reservation, occurrence, contract) {
        if let Err(cleanup) = guest_package.discard() {
            return Err(error.context(format!(
                "prepared guest package cleanup also failed after intent refusal: {cleanup:#}"
            )));
        }
        return Err(error);
    }
    let import_authorization = match &guest_package {
        SupervisorGuestPackage::Prepared { package, .. } => {
            let signed = sign_prepared_guest_import(
                contract,
                reservation,
                occurrence,
                &intent,
                &bootstrap,
                guest_inputs.projection(),
                package,
                authority,
            );
            match signed {
                Ok(signed) => Some(signed),
                Err(error) => {
                    if let Err(cleanup) = guest_package.discard() {
                        return Err(error.context(format!(
                            "guest import refusal also failed package cleanup: {cleanup:#}"
                        )));
                    }
                    return Err(error);
                }
            }
        }
        #[cfg(test)]
        SupervisorGuestPackage::Fixture(_) => None,
    };
    Ok((
        intent,
        ExternalSupervisorActivation {
            bootstrap,
            guest_inputs,
            guest_package: Some(guest_package),
            guest_runtime,
            import_authorization,
            signed_assignment: None,
        },
    ))
}

fn sign_prepared_guest_import(
    contract: &ExternalPlacementBackendContract,
    reservation: &ExternalAllocationReservation,
    occurrence: &ExternalAllocationOccurrence,
    intent: &ExternalSupervisorActivationIntent,
    bootstrap: &ryeos_state::external_execution::transport::ExternalSupervisorBootstrap,
    inputs: &ryeos_external_execution_contract::ExternalGuestInputProjection,
    package: &ryeos_external_execution::guest_package_producer::PreparedGuestPackage,
    authority: &ExternalChannelAuthority,
) -> Result<SignedGuestImportAuthorization> {
    use ryeos_external_execution_contract::staging_package::{
        GUEST_IMPORT_TICKET_SCHEMA, GuestImportTicket,
    };

    let ticket = GuestImportTicket {
        schema: GUEST_IMPORT_TICKET_SCHEMA,
        binding_hash: reservation.binding_hash.clone(),
        allocation_request_digest: reservation.request_digest.clone(),
        occurrence_id: occurrence.occurrence_id.clone(),
        activation_request_digest: intent.activation_request_digest.clone(),
        guest_input_identity: intent.guest_input_identity.clone(),
        payload_sha256: package.sha256().to_owned(),
        manifest_sha256: package.manifest_sha256().to_owned(),
        framed_bytes: package.bytes(),
        regular_bytes: package.manifest().total_regular_bytes,
        bootstrap_sha256: lillux::sha256_hex(&bootstrap.canonical_bytes()?),
        supervisor_sha256: contract.supervisor_artifact_hash.clone(),
        launcher_sha256: contract.launcher_artifact_hash.clone(),
        maximum_regular_bytes: contract.max_guest_package_regular_bytes,
        maximum_framed_bytes: contract.max_guest_package_framed_bytes,
    };
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
    sign_guest_import_ticket(
        contract,
        reservation,
        occurrence,
        intent,
        inputs,
        ticket,
        authority,
    )
}

fn sign_guest_import_ticket(
    contract: &ExternalPlacementBackendContract,
    reservation: &ExternalAllocationReservation,
    occurrence: &ExternalAllocationOccurrence,
    intent: &ExternalSupervisorActivationIntent,
    inputs: &ryeos_external_execution_contract::ExternalGuestInputProjection,
    ticket: ryeos_external_execution_contract::staging_package::GuestImportTicket,
    authority: &ExternalChannelAuthority,
) -> Result<SignedGuestImportAuthorization> {
    use ryeos_external_execution_contract::guest_import_authorization::{
        GUEST_IMPORT_AUTHORIZATION_SCHEMA, GuestImportAuthorization, GuestOccurrenceAssignment,
    };

    let assignment = GuestOccurrenceAssignment {
        placement_thread_id: &reservation.placement_thread_id,
        admitted_capsule_hash: &reservation.admitted_capsule_hash,
        base_snapshot_hash: &reservation.base_snapshot_hash,
        execution_binding_hash: &reservation.binding_hash,
        allocation_request_digest: &reservation.request_digest,
        occurrence_id: &occurrence.occurrence_id,
        activation_request_digest: &intent.activation_request_digest,
        supervisor_runtime_hash: &intent.supervisor_runtime_hash,
        guest_runtime_manifest_hash: &contract.guest_runtime_manifest_hash,
        attachment_deadline_ms: intent.attachment_deadline_ms,
    };
    let authorization = GuestImportAuthorization {
        schema: GUEST_IMPORT_AUTHORIZATION_SCHEMA,
        placement_thread_id: reservation.placement_thread_id.clone(),
        admitted_capsule_hash: reservation.admitted_capsule_hash.clone(),
        base_snapshot_hash: reservation.base_snapshot_hash.clone(),
        execution_binding_hash: reservation.binding_hash.clone(),
        allocation_request_digest: reservation.request_digest.clone(),
        occurrence_id: occurrence.occurrence_id.clone(),
        activation_request_digest: intent.activation_request_digest.clone(),
        supervisor_runtime_hash: intent.supervisor_runtime_hash.clone(),
        guest_runtime_manifest_hash: contract.guest_runtime_manifest_hash.clone(),
        attachment_deadline_ms: intent.attachment_deadline_ms,
        admission_deadline_ms: intent.attachment_deadline_ms,
        nonce_sha256: lillux::sha256_hex(&lillux::crypto::generate_random_bytes::<32>()),
        ticket,
        guest_inputs: inputs.clone(),
    };
    ryeos_external_execution::guest_import_authorization::sign_guest_import_authorization(
        authorization,
        authority.owner_signing_key(),
        &assignment,
    )
}

/// Validate the meaning of an adapter observation before durable settlement.
/// Pending is always truthful without a reconciliation capability; a stronger
/// result must be supported by the exact inspected adapter generation.
fn require_lifecycle_capability(
    backend: &dyn ExternalPlacementBackend,
    required: LifecycleCapability,
) -> Result<()> {
    ensure!(
        backend.lifecycle_capabilities().contains(&required),
        "external lifecycle observation requires undeclared capability: {required:?}"
    );
    Ok(())
}

fn require_allocation_resolution_capability(
    backend: &dyn ExternalPlacementBackend,
    resolution: &ExternalAllocationResolution,
    reconciliation: bool,
) -> Result<()> {
    match resolution {
        ExternalAllocationResolution::Bound { .. } if reconciliation => {
            require_lifecycle_capability(
                backend,
                LifecycleCapability::ExactAllocationReconciliation,
            )
        }
        ExternalAllocationResolution::NoOccurrence { .. } => {
            require_lifecycle_capability(backend, LifecycleCapability::AuthoritativeNoOccurrence)
        }
        ExternalAllocationResolution::Bound { .. } | ExternalAllocationResolution::Pending => {
            Ok(())
        }
    }
}

fn apply_allocation_resolution(
    state_store: &crate::state_store::StateStore,
    reservation: &ExternalAllocationReservation,
    resolution: ExternalAllocationResolution,
    timing: ExternalObservationTiming,
) -> Result<ExternalAllocationRecord> {
    match resolution {
        ExternalAllocationResolution::Bound {
            occurrence_id,
            provider_observation_digest,
        } => state_store.bind_external_allocation(
            &reservation.placement_thread_id,
            &ExternalAllocationOccurrence {
                schema: 1,
                binding_hash: reservation.binding_hash.clone(),
                request_digest: reservation.request_digest.clone(),
                occurrence_id,
                provider_observation_digest,
            },
            timing,
        )?,
        ExternalAllocationResolution::NoOccurrence { .. } => {
            // A negative provider observation cannot settle while the unique
            // original create permit is still capable of delayed contact.
            // Once that lease is absent no new create permit can exist because
            // the durable phase is already contact_pending.
            //
            // Callers from reconciliation pass their shared gate below; the
            // contact-permit path can never safely return NoOccurrence.
            bail!("allocator contact cannot directly settle no-occurrence evidence")
        }
        ExternalAllocationResolution::Pending => state_store
            .observe_external_lifecycle_pending(&reservation.placement_thread_id, timing)?,
    }
    state_store
        .external_allocation(&reservation.placement_thread_id)?
        .context("external allocation disappeared after lifecycle observation")
}

fn apply_reconciliation_resolution(
    state_store: &crate::state_store::StateStore,
    contact_gate: &AtomicBool,
    reservation: &ExternalAllocationReservation,
    resolution: ExternalAllocationResolution,
    timing: ExternalObservationTiming,
) -> Result<ExternalAllocationRecord> {
    match resolution {
        ExternalAllocationResolution::NoOccurrence {
            provider_observation_digest,
        } => {
            ensure!(
                !contact_gate.load(Ordering::Acquire),
                "external no-occurrence proof raced an active contact permit"
            );
            state_store.settle_external_no_occurrence(
                &reservation.placement_thread_id,
                &ExternalNoOccurrenceEvidence {
                    schema: 1,
                    binding_hash: reservation.binding_hash.clone(),
                    request_digest: reservation.request_digest.clone(),
                    provider_observation_digest,
                },
            )?;
        }
        other => return apply_allocation_resolution(state_store, reservation, other, timing),
    }
    state_store
        .external_allocation(&reservation.placement_thread_id)?
        .context("external allocation disappeared after lifecycle observation")
}

/// Exact pre-contact authority for composed external-execution tests. This
/// surface exists only when the repository test-support feature is selected;
/// it establishes the state a successfully allocated and activated occurrence
/// would already retain, but deliberately does not attach the channel, author
/// Ready, release the candidate, import C, or settle cleanup.
#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    use std::sync::Arc;

    use anyhow::{Context as _, Result, ensure};

    use super::*;

    /// Install one private test vault generation before any placement
    /// credential is provisioned. Subsequent binding rotations must reuse it.
    pub fn install_test_placement_vault(state: &mut AppState) {
        state.vault = Arc::new(SealedEnvelopeVault::new(
            state
                .config
                .app_root
                .join(".ai/state/test-external-placement-secrets.enc"),
            lillux::vault::VaultSecretKey::generate(),
        ));
    }
    use crate::node_config::sections::external_execution::RetainedExternalExecutionBinding;
    use crate::runtime_db::external_execution::{
        ExternalAllocationOccurrence, ExternalAllocationReservation,
        ExternalSupervisorActivationIntent, ExternalSupervisorActivationObservation,
        external_supervisor_activation_request_digest,
    };
    use crate::vault::SealedEnvelopeVault;

    pub use crate::node_config::sections::external_execution::{
        ContentRuntimeQualificationSelection, GuestRuntimeQualificationSelection,
        PlacementQualificationSelection,
    };

    fn installed_artifact_coordinates(
        state: &AppState,
        provider_declaration_id: &str,
    ) -> Result<(
        String,
        String,
        u64,
        String,
        String,
        u64,
        String,
        u64,
        String,
        u64,
        String,
        u64,
    )> {
        let mut backends = state.external_placement_backends.backends.values();
        let backend = backends
            .next()
            .context("composed fixture has no installed lifecycle backend")?;
        ensure!(
            backends.next().is_none(),
            "composed fixture requires exactly one lifecycle backend"
        );
        let (supervisor_hash, supervisor_bytes) = backend.supervisor_artifact();
        let (launcher_hash, launcher_bytes) = backend.launcher_artifact();
        let mut connectors = state.external_candidate_connectors.artifacts.values();
        let connector = connectors
            .next()
            .context("composed fixture has no installed connector")?;
        ensure!(
            connectors.next().is_none(),
            "composed fixture requires exactly one connector"
        );
        let configuration = state
            .external_provider_configurations
            .artifacts
            .get(provider_declaration_id)
            .context("composed fixture has no installed provider configuration")?;
        Ok((
            backend.backend_id().to_owned(),
            backend.artifact_hash().to_owned(),
            backend.artifact_bytes(),
            backend.settings_schema_digest().to_owned(),
            supervisor_hash.to_owned(),
            supervisor_bytes,
            launcher_hash.to_owned(),
            launcher_bytes,
            configuration.artifact_hash.clone(),
            configuration.artifact_bytes,
            connector.artifact_hash.clone(),
            connector.artifact_bytes,
        ))
    }

    /// Install the exact node-config placement authority needed to admit one
    /// composed external program. This does not reserve capacity, contact a
    /// provider, or create an occurrence; those remain owned by the ordinary
    /// placement path after capsule admission.
    pub fn install_precontact_binding(
        state: &mut AppState,
        program: &ryeos_state::external_execution::admission::AdmittedExternalCandidateProgram,
        controller: ryeos_state::external_execution::transport::ExternalControllerTransportContract,
        tls_root_certificates_der_base64: Vec<String>,
        qualification: &PlacementQualificationSelection,
    ) -> Result<()> {
        install_precontact_binding_with_settings(
            state,
            program,
            controller,
            tls_root_certificates_der_base64,
            serde_json::json!({"region":"composed-test", "plan":"bounded-fixture"}),
            b"fixture-secret",
            qualification,
        )
    }

    /// Test-only installation of one exact signed placement generation whose
    /// settings and sealed credential are consumed by the ordinary lifecycle
    /// adapter path. It still performs no reservation or provider contact.
    pub fn install_precontact_binding_with_settings(
        state: &mut AppState,
        program: &ryeos_state::external_execution::admission::AdmittedExternalCandidateProgram,
        controller: ryeos_state::external_execution::transport::ExternalControllerTransportContract,
        tls_root_certificates_der_base64: Vec<String>,
        settings: serde_json::Value,
        credential: &[u8],
        qualification: &PlacementQualificationSelection,
    ) -> Result<()> {
        let (
            backend,
            backend_hash,
            backend_bytes,
            settings_schema_digest,
            supervisor_hash,
            supervisor_bytes,
            launcher_hash,
            launcher_bytes,
            configuration_hash,
            configuration_bytes,
            connector_hash,
            connector_bytes,
        ) = installed_artifact_coordinates(state, &program.requirement.provider_declaration_id)?;
        let binding = crate::node_config::sections::external_execution::InstalledExternalExecutionBinding::composed_test_fixture_with_settings(
            program,
            controller,
            tls_root_certificates_der_base64,
            backend,
            backend_hash,
            backend_bytes,
            settings_schema_digest,
            supervisor_hash,
            supervisor_bytes,
            launcher_hash,
            launcher_bytes,
            configuration_hash,
            configuration_bytes,
            connector_hash,
            connector_bytes,
            settings,
            qualification,
        )?;
        let credential_access = binding.credential_access()?;
        state.vault.provision_placement_credential(
            &credential_access,
            &credential_access.test_value(
                std::str::from_utf8(credential)
                    .context("fixture placement credential must be UTF-8")?,
            ),
        )?;
        state.node_config = Arc::new(crate::node_config::NodeConfigSnapshot {
            external_execution: vec![binding],
            runtime_snapshot_production: state.node_config.runtime_snapshot_production.clone(),
            guest_runtime_materialization: state.node_config.guest_runtime_materialization.clone(),
            runtime_snapshot_qualification: state
                .node_config
                .runtime_snapshot_qualification
                .clone(),
            bundles: state.node_config.bundles.clone(),
            routes: state.node_config.routes.clone(),
            commands: state.node_config.commands.clone(),
        });
        Ok(())
    }

    /// Prepare only the durable root, dedicated session, and exact retained
    /// workspace needed by `start_exclusive_capsule`. Allocation remains
    /// absent so the production placement owner must reserve and contact the
    /// lifecycle adapter itself.
    pub fn prepare_ordinary_external_session(
        state: &mut AppState,
        placement_thread_id: &str,
        workspace_id: &str,
        worker_instance_id: &str,
        admitted_capsule_hash: &str,
        base_snapshot_hash: &str,
        prepared_runtime_launch: serde_json::Value,
        turn_start_payload: serde_json::Value,
        project_root: &std::path::Path,
        workspace_lifeline: &Arc<crate::temp_dir_guard::TempDirGuard>,
    ) -> Result<String> {
        let retained_for_review = match prepared_runtime_launch
            .pointer("/runtime_data/worker_execution/candidate_disposition")
            .and_then(serde_json::Value::as_str)
        {
            Some("retained_for_review") => true,
            Some("owner_decision") => false,
            _ => anyhow::bail!(
                "ordinary external fixture has no closed signed candidate disposition"
            ),
        };
        let now = lillux::time::timestamp_millis();
        let reservation = ExternalAllocationReservation {
            schema: EXTERNAL_ALLOCATION_RESERVATION_SCHEMA,
            placement_thread_id: placement_thread_id.to_owned(),
            admitted_capsule_hash: admitted_capsule_hash.to_owned(),
            owner: ExternalAllocationOwner::DedicatedSession(ExternalDedicatedSessionOwner {
                workspace_id: workspace_id.to_owned(),
                worker_instance_id: worker_instance_id.to_owned(),
                worker_boot_epoch: 1,
            }),
            base_snapshot_hash: base_snapshot_hash.to_owned(),
            binding_hash: "0".repeat(64),
            capacity_owner: "1".repeat(64),
            channel_authority_generation: "2".repeat(64),
            channel_owner_public_key: "3".repeat(64),
            channel_bootstrap_capability_hash: "4".repeat(64),
            admitted_adapter_request_base64: None,
            request_digest: "5".repeat(64),
            max_active: 1,
            timeout_seconds: 60,
            startup_started_at_ms: now,
            startup_deadline_ms: now
                .checked_add(60_000)
                .context("fixture startup deadline overflow")?,
            contact_deadline_ms: lillux::time::timestamp_millis()
                .checked_add(30_000)
                .context("ordinary external fixture contact deadline overflow")?,
        };
        let operator = ryeos_node_runtime::identity::NodeIdentity::load(
            &state.config.operator_signing_key_path,
        )?
        .principal_id();
        let project_authority = ryeos_state::objects::ExecutionProjectAuthority::pinned(
            "project:external-placement-test".to_owned(),
            Some(project_root.to_path_buf()),
            base_snapshot_hash.to_owned(),
            ryeos_state::objects::PinnedProjectRealization::Cow {
                terminal_publication: ryeos_state::objects::PinnedTerminalPublication::RetainResult,
            },
            ryeos_state::objects::EnvironmentAuthority::None,
            Vec::new(),
        )?
        .with_child_policy(ryeos_state::objects::ChildProjectAuthorityPolicy::Inherit)?;
        let mut launch_metadata = crate::state_store::external_fixture_launch_metadata(
            project_authority,
            prepared_runtime_launch,
            turn_start_payload,
        );
        let provisional = launch_metadata
            .admitted_launch_capsule()?
            .context("ordinary external fixture has no provisional launch capsule")?;
        let authority = provisional.launch_authority();
        let execution_identity = state
            .extensions
            .get::<crate::execution_identity_probe::NodeExecutionIdentity>()
            .context("ordinary external fixture has no rooted execution identity")?;
        let effective_definition_digest = provisional
            .exact_program
            .get("effective_definition_digest")
            .and_then(serde_json::Value::as_str)
            .context("ordinary external fixture exact program has no definition digest")?
            .to_owned();
        let (contract_ref, contract_digest) = match &provisional.artifact_identity {
            ryeos_state::objects::AdmittedLaunchArtifactIdentity::ManagedRuntime {
                runtime_ref,
                runtime_content_hash,
                ..
            } => (runtime_ref.clone(), runtime_content_hash.clone()),
            ryeos_state::objects::AdmittedLaunchArtifactIdentity::DirectItemExecutor {
                runtime_identity,
                ..
            } => (
                runtime_identity.runtime_ref.clone(),
                runtime_identity.runtime_content_hash.clone(),
            ),
        };
        let realization = ryeos_state::objects::AdmittedExecutionRealization {
            schema: ryeos_state::objects::EXECUTION_REALIZATION_SCHEMA_VERSION,
            kind: ryeos_state::objects::ADMITTED_EXECUTION_REALIZATION_KIND.to_owned(),
            substrate_identity_hash: execution_identity.identity_hash.clone(),
            substrate_attestation_hash: execution_identity.attestation_hash.clone(),
            launch_authority_digest: authority.digest()?,
            effective_definition_digest,
            artifact_identity_digest: authority.artifact_identity_digest()?,
            execution_closure_digest: authority.execution_closure_digest()?,
            contract_ref,
            contract_digest,
            components: Vec::new(),
            properties: BTreeMap::new(),
        };
        let state_authority = state.state_store.pinned_state_authority()?;
        let guard = state_authority.acquire_shared_guard()?;
        let realization_hash = state_authority
            .cas_store()?
            .store_object(&realization.to_value()?)?;
        state_authority.ensure_guard(&guard)?;
        drop(guard);
        drop(state_authority);
        launch_metadata.execution_realization_hash = Some(realization_hash);
        state
            .state_store
            .install_external_session_test_fixture_with_workspace_owner(
                &reservation,
                project_root,
                &operator,
                retained_for_review,
                &launch_metadata,
                |launch_owner| {
                    let (backend_id, backend_version) = state
                        .isolation
                        .workspace_backend_identity()
                        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
                    state.state_store.prepare_execution_workspace_backend(
                        workspace_id,
                        placement_thread_id,
                        launch_owner,
                        backend_id,
                        backend_version,
                    )?;
                    let created = state
                        .isolation
                        .create_workspace(
                            ryeos_engine::isolation::WorkspaceLifecycleInvocation {
                                operation: ryeos_isolation_protocol::WorkspaceLifecycleOperation::Create,
                                workspace_id,
                                launch_owner,
                                base_snapshot: base_snapshot_hash,
                                project_path: project_root,
                                mount_identity: None,
                            },
                            &|held| {
                                let identity = crate::process::execution_process_identity_from_lillux(
                                    held.exact_process_identity().map_err(|error| {
                                        format!("capture ordinary workspace creator identity: {error}")
                                    })?,
                                    None,
                                )
                                .map_err(|error| error.to_string())?;
                                state
                                    .state_store
                                    .attach_workspace_creator(
                                        workspace_id,
                                        placement_thread_id,
                                        launch_owner,
                                        &identity,
                                    )
                                    .map_err(|error| error.to_string())
                            },
                        )
                        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
                    if state.isolation.is_enforced() {
                        state.state_store.assert_execution_workspace_creator_reaped(
                            workspace_id,
                            placement_thread_id,
                            launch_owner,
                        )?;
                    }
                    let evidence = created.evidence;
                    workspace_lifeline.install_workspace_view(
                        &evidence,
                        created
                            .created_view
                            .context("ordinary workspace Create omitted its retained view")?,
                    )?;
                    Ok(crate::state_store::TestWorkspaceBinding {
                        workspace_id: evidence.workspace_id,
                        thread_id: placement_thread_id.to_owned(),
                        launch_owner: Some(evidence.launch_owner),
                        backend_id: Some(evidence.backend_id),
                        backend_version: Some(evidence.backend_version),
                        pinned_root_identities: Some(lillux::canonical_json(
                            &serde_json::to_value(&evidence.pinned_root_identities)?,
                        )?),
                        mount_identity: evidence.mount_identity,
                        workspace_output_partition_identity: None,
                        base_output_capture_hash: None,
                    })
                },
            )?;
        Ok(operator)
    }

    pub fn external_occurrence_id(state: &AppState, placement: &str) -> Result<String> {
        Ok(state
            .state_store
            .external_allocation(placement)?
            .context("external fixture has no retained allocation")?
            .occurrence
            .context("external fixture allocation has no bound occurrence")?
            .occurrence_id)
    }

    pub fn prepare_bound_external_supervisor(
        state: &mut AppState,
        capsule: ryeos_state::objects::AdmittedPersistentSessionCapsule,
        base_snapshot_hash: String,
        guest_inputs: ryeos_external_execution_contract::ExternalGuestInputProjection,
        controller: ryeos_state::external_execution::transport::ExternalControllerTransportContract,
        tls_root_certificates_der_base64: Vec<String>,
        supervisor_artifact_hash: String,
        launcher_artifact_hash: String,
        qualification: &PlacementQualificationSelection,
    ) -> Result<ryeos_state::external_execution::transport::ExternalSupervisorBootstrap> {
        prepare_bound_external_supervisor_inner(
            state,
            capsule,
            base_snapshot_hash,
            guest_inputs,
            controller,
            tls_root_certificates_der_base64,
            supervisor_artifact_hash,
            launcher_artifact_hash,
            None,
            qualification,
        )
    }

    pub fn prepare_bound_external_supervisor_in_workspace(
        state: &mut AppState,
        capsule: ryeos_state::objects::AdmittedPersistentSessionCapsule,
        base_snapshot_hash: String,
        guest_inputs: ryeos_external_execution_contract::ExternalGuestInputProjection,
        controller: ryeos_state::external_execution::transport::ExternalControllerTransportContract,
        tls_root_certificates_der_base64: Vec<String>,
        supervisor_artifact_hash: String,
        launcher_artifact_hash: String,
        project_root: &std::path::Path,
        workspace_lifeline: &Arc<crate::temp_dir_guard::TempDirGuard>,
        qualification: &PlacementQualificationSelection,
    ) -> Result<ryeos_state::external_execution::transport::ExternalSupervisorBootstrap> {
        prepare_bound_external_supervisor_inner(
            state,
            capsule,
            base_snapshot_hash,
            guest_inputs,
            controller,
            tls_root_certificates_der_base64,
            supervisor_artifact_hash,
            launcher_artifact_hash,
            Some((project_root, workspace_lifeline)),
            qualification,
        )
    }

    /// Root launch metadata a composed external placement would have been
    /// admitted with: a managed-runtime closure whose session inventory names
    /// the stored persistent-session capsule, and an execution realization
    /// rooted in this node's booted execution identity.
    fn composed_fixture_launch_metadata(
        state: &AppState,
        project_root: &std::path::Path,
        base_snapshot_hash: &str,
        admitted_capsule_hash: &str,
    ) -> Result<crate::launch_metadata::RuntimeLaunchMetadata> {
        let project_authority = ryeos_state::objects::ExecutionProjectAuthority::pinned(
            "project:external-placement-test".to_owned(),
            Some(project_root.to_path_buf()),
            base_snapshot_hash.to_owned(),
            ryeos_state::objects::PinnedProjectRealization::Cow {
                terminal_publication: ryeos_state::objects::PinnedTerminalPublication::RetainResult,
            },
            ryeos_state::objects::EnvironmentAuthority::None,
            Vec::new(),
        )?
        .with_child_policy(ryeos_state::objects::ChildProjectAuthorityPolicy::Inherit)?;
        let mut launch_metadata = crate::state_store::external_fixture_launch_metadata(
            project_authority,
            serde_json::json!({
                "binding_records": {},
                "required_secrets": [],
                "admitted_sessions": {"external_candidate": admitted_capsule_hash},
            }),
            serde_json::json!({}),
        );
        let provisional = launch_metadata
            .admitted_launch_capsule()?
            .context("composed external fixture has no provisional launch capsule")?;
        let authority = provisional.launch_authority();
        let execution_identity = crate::execution_identity_probe::boot_node_execution_identity(
            &state.state_store,
            &state.identity,
            &crate::build_info::get(),
        )?;
        let effective_definition_digest = provisional
            .exact_program
            .get("effective_definition_digest")
            .and_then(serde_json::Value::as_str)
            .context("composed external fixture exact program has no definition digest")?
            .to_owned();
        let ryeos_state::objects::AdmittedLaunchArtifactIdentity::ManagedRuntime {
            runtime_ref,
            runtime_content_hash,
            ..
        } = &provisional.artifact_identity
        else {
            anyhow::bail!("composed external fixture requires a managed runtime launch identity");
        };
        let realization = ryeos_state::objects::AdmittedExecutionRealization {
            schema: ryeos_state::objects::EXECUTION_REALIZATION_SCHEMA_VERSION,
            kind: ryeos_state::objects::ADMITTED_EXECUTION_REALIZATION_KIND.to_owned(),
            substrate_identity_hash: execution_identity.identity_hash.clone(),
            substrate_attestation_hash: execution_identity.attestation_hash.clone(),
            launch_authority_digest: authority.digest()?,
            effective_definition_digest,
            artifact_identity_digest: authority.artifact_identity_digest()?,
            execution_closure_digest: authority.execution_closure_digest()?,
            contract_ref: runtime_ref.clone(),
            contract_digest: runtime_content_hash.clone(),
            components: Vec::new(),
            properties: BTreeMap::new(),
        };
        let state_authority = state.state_store.pinned_state_authority()?;
        let guard = state_authority.acquire_shared_guard()?;
        let realization_hash = state_authority
            .cas_store()?
            .store_object(&realization.to_value()?)?;
        state_authority.ensure_guard(&guard)?;
        drop(guard);
        drop(state_authority);
        launch_metadata.execution_realization_hash = Some(realization_hash);
        Ok(launch_metadata)
    }

    fn prepare_bound_external_supervisor_inner(
        state: &mut AppState,
        mut capsule: ryeos_state::objects::AdmittedPersistentSessionCapsule,
        base_snapshot_hash: String,
        guest_inputs: ryeos_external_execution_contract::ExternalGuestInputProjection,
        controller: ryeos_state::external_execution::transport::ExternalControllerTransportContract,
        tls_root_certificates_der_base64: Vec<String>,
        supervisor_artifact_hash: String,
        launcher_artifact_hash: String,
        workspace: Option<(&std::path::Path, &Arc<crate::temp_dir_guard::TempDirGuard>)>,
        qualification: &PlacementQualificationSelection,
    ) -> Result<ryeos_state::external_execution::transport::ExternalSupervisorBootstrap> {
        capsule.validate()?;
        let program = capsule
            .external_candidate
            .clone()
            .context("composed external fixture capsule has no candidate program")?;
        program.verify_runtime_authority(
            capsule.retained_product_selections.as_ref(),
            capsule
                .retained_external_runtime_content_qualification
                .as_ref(),
        )?;
        guest_inputs.validate()?;
        ensure!(
            guest_inputs.base_snapshot.snapshot_hash == base_snapshot_hash,
            "composed external fixture changed its base snapshot"
        );

        let vault = Arc::new(SealedEnvelopeVault::new(
            state
                .config
                .app_root
                .join(".ai/state/test-external-secrets.enc"),
            lillux::vault::VaultSecretKey::generate(),
        ));
        state.vault = vault.clone();

        let (
            backend,
            backend_hash,
            backend_bytes,
            settings_schema_digest,
            installed_supervisor_hash,
            supervisor_bytes,
            installed_launcher_hash,
            launcher_bytes,
            configuration_hash,
            configuration_bytes,
            connector_hash,
            connector_bytes,
        ) = installed_artifact_coordinates(state, &program.requirement.provider_declaration_id)?;
        ensure!(
            supervisor_artifact_hash == installed_supervisor_hash
                && launcher_artifact_hash == installed_launcher_hash,
            "composed fixture artifacts disagree with installed lifecycle authority"
        );
        let retained = RetainedExternalExecutionBinding::composed_test_fixture(
            &program,
            controller.clone(),
            tls_root_certificates_der_base64.clone(),
            backend,
            backend_hash,
            backend_bytes,
            settings_schema_digest,
            installed_supervisor_hash,
            supervisor_bytes,
            installed_launcher_hash,
            launcher_bytes,
            configuration_hash,
            configuration_bytes,
            connector_hash,
            connector_bytes,
            qualification,
        )?;
        let credential_access = retained.credential_access()?;
        vault.provision_placement_credential(
            &credential_access,
            &credential_access.test_value("fixture-secret"),
        )?;
        let contract = retained.backend_contract();
        // A binding that selects runtime proofs yields the same retained
        // testimony the ordinary session admission seals into its capsule.
        if RuntimeContentLane::of(&program) == RuntimeContentLane::Product {
            ensure!(
                contract.runtime_content_qualification.is_none(),
                "a product-sourced composed fixture carries no acquired-content testimony"
            );
        }
        if let Some(selected) = contract.runtime_content_qualification.as_ref() {
            let published = require_current_runtime_content_qualification(
                state,
                &contract,
                RuntimeContentLane::of(&program),
            )?
            .context("composed fixture content selection has no authenticated content")?;
            program.require_content_qualification(&published.evidence)?;
            capsule.retained_external_runtime_content_qualification =
                Some(published.into_retained(
                    retained.digest().to_owned(),
                    program.runtime_manifest_hash.clone(),
                    selected.activation_ref.clone(),
                ));
        }
        if contract.runtime_qualification.is_some() {
            capsule.retained_external_runtime_qualification =
                admit_current_runtime_qualification(state, &contract, retained.digest())?;
        }
        capsule.validate()?;
        let state_authority = state.state_store.pinned_state_authority()?;
        let guard = state_authority.acquire_shared_guard()?;
        let capsule_value = capsule.to_value()?;
        let stored_capsule_hash = state_authority.cas_store()?.store_object(&capsule_value)?;
        ensure!(
            stored_capsule_hash == capsule.content_hash()?,
            "composed external fixture changed its admitted capsule hash"
        );
        drop(guard);
        drop(state_authority);
        let placement_thread_id = "T-00000000-0000-0000-0000-000000000004".to_owned();
        let workspace_id = "W-external-composed-controller".to_owned();
        let worker_instance_id = "worker-external-composed-controller".to_owned();
        let authority_generation =
            ryeos_state::objects::canonical_value_digest(&serde_json::json!({
                "domain":"ryeos.external-channel-authority.v1",
                "placement_thread_id":&placement_thread_id,
                "admitted_capsule_hash":&stored_capsule_hash,
                "base_snapshot_hash":&base_snapshot_hash,
                "binding_hash":retained.digest(),
            }))?;
        let authority_access = ExternalChannelAuthorityAccess::new(&authority_generation)?;
        let channel_authority = authority_access.decode(
            vault
                .ensure_external_channel_authority(&authority_access)
                .context("seal composed external channel authority")?,
        )?;
        let channel_owner_public_key = channel_authority.owner_public_key();
        let channel_bootstrap_capability_hash = channel_authority.bootstrap_capability_hash();
        let bootstrap_capability = channel_authority.bootstrap_capability().to_owned();
        let request_digest = ryeos_state::objects::canonical_value_digest(&serde_json::json!({
            "domain":"ryeos.external-placement-request.v1",
            "placement_thread_id":&placement_thread_id,
            "admitted_capsule_hash":&stored_capsule_hash,
            "workspace_id":&workspace_id,
            "worker_instance_id":&worker_instance_id,
            "worker_boot_epoch":1,
            "base_snapshot_hash":&base_snapshot_hash,
            "binding_hash":retained.digest(),
            "capacity_owner":retained.capacity_owner(),
            "channel_authority_generation":&authority_generation,
            "channel_owner_public_key":&channel_owner_public_key,
            "channel_bootstrap_capability_hash":&channel_bootstrap_capability_hash,
            "admitted_adapter_request_base64":serde_json::Value::Null,
            "program":&program,
            "backend_contract":&contract,
        }))?;
        let now = lillux::time::timestamp_millis();
        let reservation = ExternalAllocationReservation {
            schema: EXTERNAL_ALLOCATION_RESERVATION_SCHEMA,
            placement_thread_id: placement_thread_id.clone(),
            admitted_capsule_hash: stored_capsule_hash.clone(),
            owner: ExternalAllocationOwner::DedicatedSession(ExternalDedicatedSessionOwner {
                workspace_id,
                worker_instance_id,
                worker_boot_epoch: 1,
            }),
            base_snapshot_hash: base_snapshot_hash.clone(),
            binding_hash: retained.digest().to_owned(),
            capacity_owner: retained.capacity_owner().to_owned(),
            channel_authority_generation: authority_generation,
            channel_owner_public_key: channel_owner_public_key.clone(),
            channel_bootstrap_capability_hash,
            admitted_adapter_request_base64: None,
            request_digest,
            max_active: contract.max_active,
            timeout_seconds: contract.timeout_seconds,
            startup_started_at_ms: now,
            startup_deadline_ms: now
                .checked_add(i64::try_from(capsule.lifecycle.ready_timeout_ms)?)
                .context("composed fixture startup deadline overflow")?,
            contact_deadline_ms: now
                .checked_add(i64::from(contract.contact_timeout_seconds) * 1_000)
                .context("composed fixture contact deadline overflow")?,
        };
        let occurrence_id = format!("occ-{}", &reservation.request_digest[..48]);
        if let Some((project_root, lifeline)) = workspace {
            let session_owner = reservation.owner.dedicated_session()?;
            let operator = ryeos_node_runtime::identity::NodeIdentity::load(
                &state.config.operator_signing_key_path,
            )?
            .principal_id();
            let launch_metadata = composed_fixture_launch_metadata(
                state,
                project_root,
                &base_snapshot_hash,
                &stored_capsule_hash,
            )?;
            state
                .state_store
                .install_external_placement_test_fixture_with_workspace_owner(
                    &reservation,
                    &retained,
                    project_root,
                    &operator,
                    &launch_metadata,
                    |launch_owner| {
                        let (backend_id, backend_version) = state
                            .isolation
                            .workspace_backend_identity()
                            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
                        state.state_store.prepare_execution_workspace_backend(
                            &session_owner.workspace_id,
                            &reservation.placement_thread_id,
                            launch_owner,
                            backend_id,
                            backend_version,
                        )?;
                        let created = state
                            .isolation
                            .create_workspace(
                                ryeos_engine::isolation::WorkspaceLifecycleInvocation {
                                    operation: ryeos_isolation_protocol::WorkspaceLifecycleOperation::Create,
                                    workspace_id: &session_owner.workspace_id,
                                    launch_owner,
                                    base_snapshot: &reservation.base_snapshot_hash,
                                    project_path: project_root,
                                    mount_identity: None,
                                },
                                &|held| {
                                    let identity = crate::process::execution_process_identity_from_lillux(
                                        held.exact_process_identity().map_err(|error| {
                                            format!("capture composed workspace creator identity: {error}")
                                        })?,
                                        None,
                                    )
                                    .map_err(|error| error.to_string())?;
                                    state
                                        .state_store
                                        .attach_workspace_creator(
                                            &session_owner.workspace_id,
                                            &reservation.placement_thread_id,
                                            launch_owner,
                                            &identity,
                                        )
                                        .map_err(|error| error.to_string())
                                },
                            )
                            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
                        if state.isolation.is_enforced() {
                            state
                                .state_store
                                .assert_execution_workspace_creator_reaped(
                                    &session_owner.workspace_id,
                                    &reservation.placement_thread_id,
                                    launch_owner,
                                )?;
                        }
                        let evidence = created.evidence;
                        lifeline.install_workspace_view(
                            &evidence,
                            created.created_view.context(
                                "composed workspace Create omitted its retained view",
                            )?,
                        )?;
                        Ok(crate::state_store::TestWorkspaceBinding {
                            workspace_id: evidence.workspace_id,
                            thread_id: reservation.placement_thread_id.clone(),
                            launch_owner: Some(evidence.launch_owner),
                            backend_id: Some(evidence.backend_id),
                            backend_version: Some(evidence.backend_version),
                            pinned_root_identities: Some(lillux::canonical_json(
                                &serde_json::to_value(&evidence.pinned_root_identities)?,
                            )?),
                            mount_identity: evidence.mount_identity,
                            workspace_output_partition_identity: None,
                            base_output_capture_hash: None,
                        })
                    },
                )?;
        } else {
            let launch_metadata = composed_fixture_launch_metadata(
                state,
                std::path::Path::new(crate::state_store::EXTERNAL_PLACEMENT_TEST_PROJECT_ROOT),
                &base_snapshot_hash,
                &stored_capsule_hash,
            )?;
            state
                .state_store
                .install_admitted_external_placement_test_fixture(
                    &reservation,
                    &retained,
                    &launch_metadata,
                )?;
        }
        let contact = state
            .state_store
            .claim_external_allocation_contact(&placement_thread_id, &reservation.request_digest)?;
        ensure!(
            matches!(contact, ExternalAllocationContactClaim::Contact(_)),
            "composed external fixture did not own first allocator contact"
        );
        let occurrence = ExternalAllocationOccurrence {
            schema: 1,
            binding_hash: reservation.binding_hash.clone(),
            request_digest: reservation.request_digest.clone(),
            occurrence_id,
            provider_observation_digest: "f".repeat(64),
        };
        state.state_store.bind_external_allocation(
            &placement_thread_id,
            &occurrence,
            ExternalObservationTiming::Startup {
                deadline_exceeded: false,
                live_deadline: reservation.startup_deadline()?,
            },
        )?;
        let attachment_deadline_ms = reservation
            .contact_deadline_ms
            .checked_add(i64::from(contract.observation_timeout_seconds) * 1_000)
            .context("composed fixture attachment deadline overflow")?;
        let post_execution_timeout_seconds = contract
            .observation_timeout_seconds
            .checked_add(contract.cleanup_timeout_seconds)
            .context("composed fixture post-execution timeout overflow")?;
        let channel_max_bytes = contract.max_transfer_bytes.min(64 * 1024 * 1024);
        let guest_input_identity = guest_inputs.identity_digest()?;
        let activation_request_digest = external_supervisor_activation_request_digest(
            &reservation,
            &occurrence,
            &contract,
            attachment_deadline_ms,
            post_execution_timeout_seconds,
            channel_max_bytes,
            &guest_input_identity,
        )?;
        let delivery = crate::runtime_db::external_execution::fixture_guest_package_delivery(
            &reservation.binding_hash,
            &reservation.request_digest,
            &occurrence.occurrence_id,
            &activation_request_digest,
            &guest_input_identity,
        );
        let activation = ExternalSupervisorActivationIntent {
            schema: 3,
            binding_hash: reservation.binding_hash.clone(),
            request_digest: reservation.request_digest.clone(),
            occurrence_id: occurrence.occurrence_id.clone(),
            supervisor_runtime_hash: contract
                .workload
                .structured_session()?
                .runtime_manifest_hash
                .clone(),
            guest_input_identity: guest_input_identity.clone(),
            activation_request_digest,
            attachment_deadline_ms,
            execution_timeout_seconds: reservation.timeout_seconds,
            post_execution_timeout_seconds,
            channel_max_bytes,
            delivery,
        };
        ensure!(
            state
                .state_store
                .begin_external_supervisor_activation(&placement_thread_id, &activation)?,
            "composed external fixture activation was not new"
        );
        state.state_store.settle_external_supervisor_activation(
            &placement_thread_id,
            &ExternalSupervisorActivationObservation {
                schema: 1,
                binding_hash: reservation.binding_hash.clone(),
                request_digest: reservation.request_digest.clone(),
                occurrence_id: occurrence.occurrence_id.clone(),
                activation_request_digest: activation.activation_request_digest,
                activation_state: "started".into(),
                provider_observation_digest: "7".repeat(64),
            },
            ExternalObservationTiming::Startup {
                deadline_exceeded: false,
                live_deadline: reservation.startup_deadline()?,
            },
        )?;

        let bootstrap = ryeos_state::external_execution::transport::ExternalSupervisorBootstrap {
            schema: ryeos_state::external_execution::transport::EXTERNAL_SUPERVISOR_HTTP_BOOTSTRAP_SCHEMA,
            controller: controller.into(),
            tls_root_certificates_der_base64,
            placement_thread_id,
            occurrence_id: occurrence.occurrence_id,
            allocation_request_digest: reservation.request_digest,
            admitted_capsule_hash: stored_capsule_hash,
            base_snapshot_hash,
            execution_binding_hash: reservation.binding_hash,
            supervisor_runtime_hash: program.runtime_manifest_hash.clone(),
            launcher_artifact_hash,
            candidate_program: program.into(),
            guest_input_identity,
            guest_inputs,
            owner_public_key: channel_owner_public_key,
            bootstrap_capability,
            attachment_deadline_ms,
            execution_timeout_seconds: reservation.timeout_seconds,
            post_execution_timeout_seconds,
            candidate_export_max_bytes: contract.max_export_bytes.min(channel_max_bytes),
            channel_max_bytes,
            // The signed profiles' `external-supervisor-journal` values. This
            // module also builds under the `test-support` feature, where the
            // `cfg(test)` envelope helper does not exist.
            native_journal: ryeos_state::external_execution::transport::ExternalSupervisorNativeEnvelope {
                storage_bytes: 67_108_864,
                allocator_steps: 2_000_000_000,
                connections: 16,
                operation_millis: 4_860_000,
                cleanup_millis: 120_000,
                cleanup_steps: 3_000_000,
            },
            report_capabilities: false,
        };
        bootstrap.validate()?;
        Ok(bootstrap)
    }

    /// Perform only the production start-owner transition that follows an
    /// authenticated Ready frame. The frame itself must already exist in the
    /// durable transcript; this helper cannot manufacture or apply it.
    pub fn admit_retained_ready_and_author_release(
        state: &AppState,
        placement: &str,
        live_deadline: lillux::time::MonotonicDeadline,
    ) -> Result<bool> {
        let allocation = state
            .state_store
            .external_allocation(placement)?
            .context("composed start-owner beat lost its allocation")?;
        let access = ExternalChannelAuthorityAccess::new(
            &allocation.reservation.channel_authority_generation,
        )?;
        let authority = access.decode(
            state
                .vault
                .external_channel_authority(&access)
                .context("read composed start-owner channel authority")?,
        )?;
        Ok(state
            .state_store
            .admit_external_ready_and_author_release(
                placement,
                authority.owner_signing_key(),
                live_deadline,
            )?
            .is_some())
    }

    /// Advance the composed fixture through the ordinary worker completion
    /// and independent external-occurrence settlement owners. Candidate C
    /// must already have arrived through the authenticated production import
    /// path; this helper manufactures neither export nor import evidence.
    pub fn settle_composed_session_completion(
        state: &AppState,
        placement: &str,
        completion_request_digest: &str,
        provider_termination_request_digest: &str,
        provider_observation_digest: &str,
    ) -> Result<()> {
        use crate::process::{ExecutionProcessIdentity, PROCESS_IDENTITY_SCHEMA_VERSION};
        use crate::runtime_db::{
            NewDedicatedSessionCommand, WorkerProcessRecord, WorkerProcessState,
        };

        let session = state
            .state_store
            .dedicated_session(placement)?
            .context("composed completion lost its dedicated session")?;
        let worker_instance_id = "worker-external-composed-controller";
        let now = lillux::time::timestamp_millis();
        state
            .state_store
            .attach_worker_process(&WorkerProcessRecord {
                worker_instance_id: worker_instance_id.into(),
                boot_identity_hash: "8".repeat(64),
                session_capsule_hash: session.admitted_capsule_hash.clone(),
                boot_epoch: 1,
                lifecycle_generation: 1,
                process_identity: ExecutionProcessIdentity {
                    schema_version: PROCESS_IDENTITY_SCHEMA_VERSION,
                    process_scope: None,
                    boot_id: "composed-external-test-boot".into(),
                    target_pid: 41,
                    target_start_time_ticks: 100,
                    group_leader_pid: 41,
                    group_leader_start_time_ticks: 100,
                    resource_selections: Vec::new(),
                    resource_settlement_authority: None,
                    resource_operations: Vec::new(),
                    resource_allocation_limit: None,
                    resource_occupancy_start: None,
                    resource_occupancy_limit: None,
                    resource_cleanup_allowance_ms: None,
                },
                control_channel_identity: "fd:test-composed-external".into(),
                state: WorkerProcessState::Attached,
                daemon_generation_id: "composed-external-test-daemon".into(),
                placement_thread_id: placement.into(),
                cleanup_state: "owned".into(),
                created_at_ms: now,
                updated_at_ms: now,
            })?;
        state
            .state_store
            .complete_worker_binding(worker_instance_id, placement, 1)?;
        let command =
            state
                .state_store
                .reserve_dedicated_session_command(NewDedicatedSessionCommand {
                    placement_thread_id: placement,
                    idempotency_key: "composed-external-test-turn",
                    worker_boot_epoch: 1,
                    command_kind: "route",
                    request_digest: completion_request_digest,
                    payload: &serde_json::json!({"route_id":"turn.start","payload":{}}),
                })?;
        state.state_store.mark_dedicated_command_contacted(
            placement,
            command.command_sequence,
            1,
        )?;
        state.state_store.settle_dedicated_command(
            placement,
            command.command_sequence,
            1,
            true,
            &serde_json::json!({"completed":true}),
        )?;
        let completion = ryeos_runtime::callback::HostedCommandCompletionFence {
            placement_thread_id: placement.into(),
            admitted_capsule_hash: session.admitted_capsule_hash,
            worker_boot_epoch: 1,
            command_sequence: command.command_sequence,
            request_digest: completion_request_digest.into(),
            turn_id: "composed-external-test-turn".into(),
            completion_operation_id: lillux::sha256_hex(
                b"composed-external-test-completion-operation",
            ),
        };
        state
            .state_store
            .reserve_dedicated_session_completion(placement, 1, &completion)?;
        request_external_candidate_cleanup(state, placement)?;
        let allocation = state
            .state_store
            .external_allocation(placement)?
            .context("composed completion lost its external allocation")?;
        let occurrence = allocation
            .occurrence
            .as_ref()
            .context("composed completion has no exact occurrence")?;
        let termination_request_digest =
            ryeos_state::objects::canonical_value_digest(&serde_json::json!({
                "domain":"ryeos.external-placement-termination.v1",
                "binding_hash":allocation.reservation.binding_hash,
                "request_digest":allocation.reservation.request_digest,
                "occurrence_id":occurrence.occurrence_id,
            }))?;
        ensure!(
            termination_request_digest == provider_termination_request_digest,
            "provider terminal observation changed its termination request identity"
        );
        let intent = ExternalTerminationIntent {
            schema: 1,
            binding_hash: allocation.reservation.binding_hash.clone(),
            request_digest: allocation.reservation.request_digest.clone(),
            occurrence_id: occurrence.occurrence_id.clone(),
            termination_request_digest: termination_request_digest.clone(),
        };
        state
            .state_store
            .begin_external_termination(placement, &intent)?;
        state.state_store.settle_external_terminal(
            placement,
            &ExternalTerminalObservation {
                schema: 1,
                binding_hash: allocation.reservation.binding_hash,
                request_digest: allocation.reservation.request_digest,
                occurrence_id: occurrence.occurrence_id.clone(),
                termination_request_digest,
                terminal_state: "terminated".into(),
                provider_observation_digest: provider_observation_digest.to_owned(),
            },
        )?;
        state.state_store.settle_worker_process(
            worker_instance_id,
            placement,
            1,
            "reaped",
            "completed",
        )?;
        state.state_store.terminalize_dedicated_session(
            placement,
            worker_instance_id,
            1,
            "completed",
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    fn fixture_guest_package(
        contract: &ExternalPlacementBackendContract,
        reservation: &ExternalAllocationReservation,
        occurrence: &ExternalAllocationOccurrence,
        guest_inputs: &ryeos_external_execution::guest_inputs::ExternalGuestInputAuthority,
        activation_request_digest: &str,
    ) -> Result<SupervisorGuestPackage> {
        let commitment = ExternalGuestPackageDeliveryCommitment {
            schema: 1,
            binding_hash: reservation.binding_hash.clone(),
            request_digest: reservation.request_digest.clone(),
            occurrence_id: occurrence.occurrence_id.clone(),
            activation_request_digest: activation_request_digest.into(),
            guest_input_identity: guest_inputs.identity_digest()?,
            manifest_sha256: "1".repeat(64),
            payload_sha256: "2".repeat(64),
            regular_bytes: 1,
            framed_bytes: 21,
        };
        ensure!(
            commitment.regular_bytes <= contract.max_guest_package_regular_bytes
                && commitment.framed_bytes <= contract.max_guest_package_framed_bytes,
            "fixture package exceeds signed budget"
        );
        Ok(SupervisorGuestPackage::Fixture(commitment))
    }
    fn test_observation_timing() -> ExternalObservationTiming {
        ExternalObservationTiming::Startup {
            deadline_exceeded: false,
            live_deadline: lillux::time::MonotonicDeadline::after(
                lillux::time::Duration::from_secs(60),
            ),
        }
    }
    fn test_startup_deadline() -> lillux::time::MonotonicDeadline {
        lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(60))
    }
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn guest_input_authority(
        root: &std::path::Path,
        base_snapshot_hash: &str,
    ) -> ryeos_external_execution::guest_inputs::ExternalGuestInputAuthority {
        let base_path = root.join("base-snapshot");
        let runtime_path = root.join("runtime");
        std::fs::create_dir_all(&base_path).unwrap();
        std::fs::create_dir_all(&runtime_path).unwrap();
        let base = lillux::PinnedDirectory::open(&base_path)
            .unwrap()
            .unwrap()
            .inherited_descriptor_authority()
            .unwrap();
        let runtime = lillux::PinnedDirectory::open(&runtime_path)
            .unwrap()
            .unwrap()
            .inherited_descriptor_authority()
            .unwrap();
        let manifest = ryeos_state::observe_external_content_tree_exact(
            &lillux::PinnedDirectory::open(&runtime_path)
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        let manifest_bytes =
            lillux::canonical_json(&serde_json::to_value(&manifest).unwrap()).unwrap();
        let manifest_hash = lillux::sha256_hex(manifest_bytes.as_bytes());
        let manifest_authority =
            lillux::sealed_memfd(c"test-runtime-manifest", manifest_bytes.as_bytes()).unwrap();
        let projection = ryeos_external_execution_contract::ExternalGuestInputProjection {
            workspace_view: ryeos_external_execution_contract::GuestWorkspaceView::FullSnapshot,
            schema: ryeos_external_execution_contract::EXTERNAL_GUEST_INPUT_PROJECTION_SCHEMA,
            base_snapshot: ryeos_external_execution_contract::GuestBaseSnapshotInput {
                descriptor: base.inherited_descriptor().unwrap(),
                snapshot_hash: base_snapshot_hash.into(),
                closure_digest: "5".repeat(64),
                object_count: 3,
                blob_count: 1,
                total_bytes: 1,
            },
            workspace_outputs: None,
            inputs: vec![ryeos_external_execution_contract::GuestMountInput {
                role: ryeos_external_execution_contract::GuestMountRole::Product,
                authority_id: "runtime".into(),
                descriptor: runtime.inherited_descriptor().unwrap(),
                destination: "/runtime".into(),
                kind: ryeos_external_execution_contract::GuestMountKind::Directory,
                access: ryeos_external_execution_contract::GuestMountAccess::ReadOnly,
                normalized_mode: None,
                content_authority:
                    ryeos_external_execution_contract::GuestMountContentAuthority::ProductManifest {
                        manifest_kind:
                            ryeos_external_execution_contract::GuestProductManifestKind::Content,
                        manifest_hash,
                        manifest_descriptor: manifest_authority.inherited_descriptor().unwrap(),
                        manifest_bytes: manifest_bytes.len() as u64,
                    },
                bytes: manifest.total_bytes,
            }],
            executable_search: vec!["/runtime/bin".into()],
            environment: std::collections::BTreeMap::new(),
        };
        ryeos_external_execution::guest_inputs::ExternalGuestInputAuthority::new(
            projection,
            base,
            None,
            vec![runtime],
            vec![manifest_authority],
            Vec::new(),
        )
        .unwrap()
    }

    #[test]
    fn protocol_input_payload_enforces_the_exact_transport_bound() {
        assert!(external_protocol_input_payload(&[]).is_err());
        let exact = vec![0x5a; ryeos_state::external_execution::MAX_CHUNK_BYTES];
        let payload = external_protocol_input_payload(&exact).unwrap();
        let ryeos_state::external_execution::ExecutionChannelPayload::ProtocolBytes {
            bytes_base64,
        } = payload
        else {
            panic!("protocol input validation produced the wrong payload kind");
        };
        assert_eq!(STANDARD.decode(bytes_base64).unwrap(), exact);
        assert!(
            external_protocol_input_payload(&vec![
                0x5a;
                ryeos_state::external_execution::MAX_CHUNK_BYTES
                    + 1
            ])
            .is_err()
        );
    }

    #[derive(Debug)]
    pub(super) struct FixtureBackend {
        pub(super) artifact: String,
    }

    fn all_lifecycle_capabilities() -> BTreeSet<LifecycleCapability> {
        BTreeSet::from([
            LifecycleCapability::SupervisorActivation,
            LifecycleCapability::ExactAllocationReconciliation,
            LifecycleCapability::AuthoritativeNoOccurrence,
            LifecycleCapability::ExactActivationReconciliation,
            LifecycleCapability::IdempotentTermination,
            LifecycleCapability::ExactTerminalObservation,
        ])
    }

    #[derive(Debug)]
    struct FaultBackend {
        artifact: String,
        capabilities: BTreeSet<LifecycleCapability>,
        qualification_calls: AtomicUsize,
        allocate_calls: AtomicUsize,
        allocation_observations: AtomicUsize,
        reconciled_allocation: &'static str,
        activation_calls: AtomicUsize,
        activation_observations: AtomicUsize,
        reconciled_activation: &'static str,
        terminate_calls: AtomicUsize,
        termination_observations: AtomicUsize,
        reconciled_termination: &'static str,
        late_allocation: bool,
        late_activation: bool,
        stages_runtime: bool,
        prepare_calls: AtomicUsize,
    }

    impl FaultBackend {
        fn new() -> Self {
            Self {
                artifact: "d".repeat(64),
                capabilities: all_lifecycle_capabilities(),
                qualification_calls: AtomicUsize::new(0),
                allocate_calls: AtomicUsize::new(0),
                allocation_observations: AtomicUsize::new(0),
                reconciled_allocation: "bound",
                activation_calls: AtomicUsize::new(0),
                activation_observations: AtomicUsize::new(0),
                reconciled_activation: "started",
                terminate_calls: AtomicUsize::new(0),
                termination_observations: AtomicUsize::new(0),
                reconciled_termination: "terminal",
                late_allocation: false,
                late_activation: false,
                stages_runtime: false,
                prepare_calls: AtomicUsize::new(0),
            }
        }

        fn with_reconciled_activation(reconciled_activation: &'static str) -> Self {
            Self {
                reconciled_activation,
                ..Self::new()
            }
        }
    }

    impl ExternalPlacementBackend for FaultBackend {
        fn lifecycle_capabilities(&self) -> BTreeSet<LifecycleCapability> {
            self.capabilities.clone()
        }

        fn stages_guest_runtime_upload(&self) -> bool {
            self.stages_runtime
        }

        fn backend_id(&self) -> &str {
            "fixture"
        }

        fn artifact_hash(&self) -> &str {
            &self.artifact
        }

        fn artifact_bytes(&self) -> u64 {
            4096
        }

        fn supervisor_artifact(&self) -> (&str, u64) {
            (
                "1111111111111111111111111111111111111111111111111111111111111111",
                4096,
            )
        }

        fn launcher_artifact(&self) -> (&str, u64) {
            (
                "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
                4096,
            )
        }

        fn settings_schema_digest(&self) -> &str {
            "3333333333333333333333333333333333333333333333333333333333333333"
        }

        fn qualify_offline(
            &self,
            _contract: &ExternalPlacementBackendContract,
            _credential: &PlacementCredential,
        ) -> Result<()> {
            self.qualification_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn prepare_guest_package(
            &self,
            contract: &ExternalPlacementBackendContract,
            reservation: &ExternalAllocationReservation,
            occurrence: &ExternalAllocationOccurrence,
            _bootstrap: &ryeos_state::external_execution::transport::ExternalSupervisorBootstrap,
            guest_inputs: &ryeos_external_execution::guest_inputs::ExternalGuestInputAuthority,
            activation_request_digest: &str,
            _parent: &lillux::PinnedDirectory,
            _deadline: lillux::time::MonotonicDeadline,
        ) -> Result<SupervisorGuestPackage> {
            self.prepare_calls.fetch_add(1, Ordering::SeqCst);
            fixture_guest_package(
                contract,
                reservation,
                occurrence,
                guest_inputs,
                activation_request_digest,
            )
        }

        fn allocate(
            &self,
            _contract: &ExternalPlacementBackendContract,
            _credential: &PlacementCredential,
            _reservation: &ExternalAllocationReservation,
            _deadline: lillux::time::MonotonicDeadline,
        ) -> Result<ExternalLifecycleObservation<ExternalAllocationResolution>> {
            self.allocate_calls.fetch_add(1, Ordering::SeqCst);
            if self.late_allocation {
                return Ok(ExternalLifecycleObservation {
                    value: ExternalAllocationResolution::Bound {
                        occurrence_id: "late-occurrence".into(),
                        provider_observation_digest: "f".repeat(64),
                    },
                    deadline_exceeded: true,
                });
            }
            bail!("fixture lost the create response after provider mutation")
        }

        fn reconcile_allocation(
            &self,
            _contract: &ExternalPlacementBackendContract,
            _credential: &PlacementCredential,
            _reservation: &ExternalAllocationReservation,
            _deadline: lillux::time::MonotonicDeadline,
        ) -> Result<ExternalLifecycleObservation<ExternalAllocationResolution>> {
            self.allocation_observations.fetch_add(1, Ordering::SeqCst);
            Ok(ExternalLifecycleObservation {
                value: match self.reconciled_allocation {
                    "bound" => ExternalAllocationResolution::Bound {
                        occurrence_id: "fixture-occurrence".into(),
                        provider_observation_digest: "f".repeat(64),
                    },
                    "no_occurrence" => ExternalAllocationResolution::NoOccurrence {
                        provider_observation_digest: "f".repeat(64),
                    },
                    "pending" => ExternalAllocationResolution::Pending,
                    _ => bail!("fixture selected an unknown allocation resolution"),
                },
                deadline_exceeded: false,
            })
        }

        fn activate_supervisor(
            &self,
            _contract: &ExternalPlacementBackendContract,
            _credential: &PlacementCredential,
            reservation: &ExternalAllocationReservation,
            occurrence: &ExternalAllocationOccurrence,
            intent: &ExternalSupervisorActivationIntent,
            activation: &ExternalSupervisorActivation,
            _deadline: lillux::time::MonotonicDeadline,
        ) -> Result<ExternalLifecycleObservation<ExternalSupervisorActivationResolution>> {
            let bootstrap = activation.bootstrap();
            ensure!(
                bootstrap.placement_thread_id == reservation.placement_thread_id
                    && bootstrap.occurrence_id == occurrence.occurrence_id
                    && bootstrap.owner_public_key == reservation.channel_owner_public_key
                    && lillux::sha256_hex(bootstrap.bootstrap_capability.as_bytes())
                        == reservation.channel_bootstrap_capability_hash
                    && intent.activation_request_digest.len() == 64,
                "fixture adapter received wrong supervisor activation"
            );
            self.activation_calls.fetch_add(1, Ordering::SeqCst);
            if self.late_activation {
                return Ok(ExternalLifecycleObservation {
                    value: ExternalSupervisorActivationResolution::Started {
                        provider_observation_digest: "3".repeat(64),
                    },
                    deadline_exceeded: true,
                });
            }
            bail!("fixture lost the supervisor-start response after provider mutation")
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
            self.activation_observations.fetch_add(1, Ordering::SeqCst);
            Ok(ExternalLifecycleObservation {
                value: match self.reconciled_activation {
                    "started" => ExternalSupervisorActivationResolution::Started {
                        provider_observation_digest: "3".repeat(64),
                    },
                    "not_started" => ExternalSupervisorActivationResolution::NotStarted {
                        provider_observation_digest: "4".repeat(64),
                    },
                    "pending" => ExternalSupervisorActivationResolution::Pending,
                    _ => bail!("fixture selected an unknown activation resolution"),
                },
                deadline_exceeded: false,
            })
        }

        fn terminate(
            &self,
            _contract: &ExternalPlacementBackendContract,
            _credential: &PlacementCredential,
            _reservation: &ExternalAllocationReservation,
            _occurrence: &ExternalAllocationOccurrence,
            _intent: &ExternalTerminationIntent,
        ) -> Result<ExternalTerminationResolution> {
            self.terminate_calls.fetch_add(1, Ordering::SeqCst);
            bail!("fixture lost the termination response after provider mutation")
        }

        fn reconcile_termination(
            &self,
            _contract: &ExternalPlacementBackendContract,
            _credential: &PlacementCredential,
            _reservation: &ExternalAllocationReservation,
            _occurrence: &ExternalAllocationOccurrence,
            _intent: &ExternalTerminationIntent,
        ) -> Result<ExternalTerminationResolution> {
            self.termination_observations.fetch_add(1, Ordering::SeqCst);
            Ok(match self.reconciled_termination {
                "terminal" => ExternalTerminationResolution::Terminal {
                    provider_observation_digest: "2".repeat(64),
                },
                "pending" => ExternalTerminationResolution::Pending,
                _ => bail!("fixture selected an unknown termination resolution"),
            })
        }
    }

    impl ExternalPlacementBackend for FixtureBackend {
        fn lifecycle_capabilities(&self) -> BTreeSet<LifecycleCapability> {
            all_lifecycle_capabilities()
        }

        fn backend_id(&self) -> &str {
            "fixture"
        }

        fn artifact_hash(&self) -> &str {
            &self.artifact
        }

        fn artifact_bytes(&self) -> u64 {
            4096
        }

        fn supervisor_artifact(&self) -> (&str, u64) {
            (
                "1111111111111111111111111111111111111111111111111111111111111111",
                4096,
            )
        }

        fn launcher_artifact(&self) -> (&str, u64) {
            (
                "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
                4096,
            )
        }

        fn settings_schema_digest(&self) -> &str {
            "3333333333333333333333333333333333333333333333333333333333333333"
        }

        fn qualify_offline(
            &self,
            contract: &ExternalPlacementBackendContract,
            credential: &PlacementCredential,
        ) -> Result<()> {
            ensure!(
                contract.settings["region"] == "fixture-region"
                    && contract.settings["plan"] == "fixture-plan"
                    && contract.network_policy
                        == "supervisor_pinned_owner_only_candidate_denied_v1"
                    && contract.storage_policy == "ephemeral_private_candidate_v1"
                    && contract.cleanup_proof == "provider_terminal_occurrence_v1"
                    && credential.secret() == "fixture-secret",
                "fixture backend received widened or wrong authority"
            );
            Ok(())
        }

        fn prepare_guest_package(
            &self,
            contract: &ExternalPlacementBackendContract,
            reservation: &ExternalAllocationReservation,
            occurrence: &ExternalAllocationOccurrence,
            _bootstrap: &ryeos_state::external_execution::transport::ExternalSupervisorBootstrap,
            guest_inputs: &ryeos_external_execution::guest_inputs::ExternalGuestInputAuthority,
            activation_request_digest: &str,
            _parent: &lillux::PinnedDirectory,
            _deadline: lillux::time::MonotonicDeadline,
        ) -> Result<SupervisorGuestPackage> {
            fixture_guest_package(
                contract,
                reservation,
                occurrence,
                guest_inputs,
                activation_request_digest,
            )
        }
    }

    fn program() -> ryeos_state::external_execution::admission::AdmittedExternalCandidateProgram {
        let runtime_recipe =
            ryeos_state::external_execution::admission::ExternalCandidateRuntimeRecipe {
                schema: 2,
                runtime_mount_destination: "/runtime".into(),
                executable_relative_path: "bin/codex".into(),
                argv0: "codex".into(),
                arguments: vec!["exec-server".into(), "--listen".into(), "stdio".into()],
                cwd: "/workspace".into(),
                environment: BTreeMap::new(),
                max_stdout_bytes: 1024 * 1024,
                max_stderr_bytes: 1024 * 1024,
                proc_filesystem:
                    ryeos_state::external_execution::admission::ExternalCandidateProcFilesystem::PidNamespaceNested,
                contain_process_group: false,
                nested_sandbox: true,
            };
        let runtime_recipe_digest = runtime_recipe.digest().unwrap();
        let requirement =
            ryeos_state::external_execution::admission::ExternalCandidateRequirement {
                schema: 7,
                protocol: ryeos_state::external_execution::admission::PROTOCOL.into(),
                required_lifecycle_capabilities: BTreeSet::new(),
                connector_protocol:
                    ryeos_state::external_execution::admission::CONNECTOR_PROTOCOL.into(),
                execution_route: ryeos_state::external_execution::admission::ExternalCandidateExecutionRoute::ConnectorOnly,
                provider_declaration_id: "codex-hosted".into(),
                provider_configuration_destination: "environments.toml".into(),
                runtime_product_declaration_id: "runtime".into(),
                runtime_authority: ryeos_state::external_execution::admission::ExternalCandidateRuntimeAuthority::CapturedProduct,
                runtime_recipe,
            };
        let qualification_use =
            ryeos_state::external_execution::admission::test_support::fixture_qualification_use(
                &requirement,
            )
            .unwrap();
        ryeos_state::external_execution::admission::AdmittedExternalCandidateProgram {
            requirement,
            qualification_use,
            runtime_manifest_kind: ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND.into(),
            runtime_manifest_hash: "b".repeat(64),
            runtime_source: ryeos_state::external_execution::admission::ExternalCandidateRuntimeSource::CapturedProduct { witness_hash: "1".repeat(64) },
            qualification_attestation_hash: "2".repeat(64),
            selection_identity_digest: "c".repeat(64),
            runtime_recipe_digest,
        }
    }

    fn credential(binding: &InstalledExternalExecutionBinding) -> PlacementCredential {
        let access = binding.credential_access().unwrap();
        let value = lillux::canonical_json(&serde_json::json!({
            "schema":1,
            "backend":"fixture",
            "account":"account",
            "generation":"a".repeat(64),
            "secret":"fixture-secret",
        }))
        .unwrap();
        access.decode(zeroize::Zeroizing::new(value)).unwrap()
    }

    /// The Runtime store opens through the native test gate; the returned
    /// owner dereferences to it and closes it when dropped.
    fn lifecycle_fixture(
        path: &std::path::Path,
    ) -> (
        crate::runtime_db::test_native::TestRuntime,
        ExternalAllocationReservation,
        RetainedExternalExecutionBinding,
    ) {
        use crate::runtime_db::{
            DedicatedCandidateDisposition, NewCredentialProfile, NewDedicatedSession,
            WorkspaceBinding, WorkspaceState,
        };
        let db = crate::runtime_db::test_native::TestRuntime::open(path);
        db.create_credential_profile(NewCredentialProfile {
            profile_id: "P-one",
            owner_principal: "fp:operator",
            home_id: "home-one",
        })
        .unwrap();
        db.acquire_credential_profile("P-one", "fp:operator", "worker-one")
            .unwrap();
        db.admit_dedicated_session(NewDedicatedSession {
            placement_thread_id: "T-one",
            chain_root_id: "T-root",
            owner_principal: "fp:operator",
            admitted_capsule_hash: &"a".repeat(64),
            workspace_id: "W-one",
            candidate_required: true,
            candidate_disposition: DedicatedCandidateDisposition::OwnerDecision,
            credential_profile_id: "P-one",
            credential_generation: 1,
            credential_lock_owner: "worker-one",
        })
        .unwrap();
        assert_eq!(
            db.fixture_claim_thread_launch("T-one", "claim-one", "daemon:external-placement-test")
                .unwrap(),
            crate::runtime_db::LaunchClaimOutcome::Claimed,
        );
        let launch_owner = db
            .get_launch_claim("T-one")
            .unwrap()
            .expect("external placement fixture launch claim")
            .claimed_by;
        db.reserve_workspace("W-one", &"b".repeat(64), "/fixture")
            .unwrap();
        db.transition_workspace(
            "W-one",
            &[WorkspaceState::Reserved],
            WorkspaceState::Constructing,
            None,
        )
        .unwrap();
        db.claim_workspace_construction("W-one", "T-one", &launch_owner)
            .unwrap();
        db.bind_workspace(WorkspaceBinding {
            workspace_id: "W-one",
            thread_id: "T-one",
            launch_owner: Some(&launch_owner),
            backend_id: Some("fixture"),
            backend_version: Some("fixture"),
            pinned_root_identities: Some("fixture"),
            mount_identity: Some("fixture"),
            workspace_output_partition_identity: None,
            base_output_capture_hash: None,
        })
        .unwrap();
        let binding = RetainedExternalExecutionBinding::test_fixture();
        let channel_authority = ExternalChannelAuthority::test_fixture(&"3".repeat(64));
        let channel_owner_public_key = channel_authority.owner_public_key();
        let channel_bootstrap_capability_hash = channel_authority.bootstrap_capability_hash();
        let now = lillux::time::timestamp_millis();
        let reservation = ExternalAllocationReservation {
            schema: EXTERNAL_ALLOCATION_RESERVATION_SCHEMA,
            placement_thread_id: "T-one".into(),
            admitted_capsule_hash: "a".repeat(64),
            owner: ExternalAllocationOwner::DedicatedSession(ExternalDedicatedSessionOwner {
                workspace_id: "W-one".into(),
                worker_instance_id: "worker-one".into(),
                worker_boot_epoch: 1,
            }),
            base_snapshot_hash: "b".repeat(64),
            binding_hash: binding.digest().into(),
            capacity_owner: binding.capacity_owner().into(),
            channel_authority_generation: "3".repeat(64),
            channel_owner_public_key,
            channel_bootstrap_capability_hash,
            admitted_adapter_request_base64: None,
            request_digest: "e".repeat(64),
            max_active: 1,
            timeout_seconds: 60,
            startup_started_at_ms: now,
            startup_deadline_ms: now + 60_000,
            contact_deadline_ms: lillux::time::timestamp_millis() + 60_000,
        };
        db.reserve_external_allocation(&reservation, &binding)
            .unwrap();
        (db, reservation, binding)
    }

    fn placement_store_fixture() -> (
        crate::state_store::test_native::TestStateStore,
        Arc<crate::state_store::StateStore>,
        ExternalAllocationReservation,
        RetainedExternalExecutionBinding,
        Arc<StateLockLease>,
        std::path::PathBuf,
    ) {
        placement_store_fixture_with_startup(60_000)
    }

    fn placement_store_fixture_with_startup(
        startup_ms: i64,
    ) -> (
        crate::state_store::test_native::TestStateStore,
        Arc<crate::state_store::StateStore>,
        ExternalAllocationReservation,
        RetainedExternalExecutionBinding,
        Arc<StateLockLease>,
        std::path::PathBuf,
    ) {
        let root = tempfile::tempdir().unwrap().keep();
        let lock_path = ryeos_node_runtime::state_lock::default_lock_path(&root);
        let identity =
            ryeos_node_runtime::identity::NodeIdentity::create(&root.join("node-key.pem")).unwrap();
        let signer = Arc::new(crate::state_store::NodeIdentitySigner::from_identity(
            &identity,
        ));
        let mut trust = ryeos_state::refs::TrustStore::new();
        trust.insert(identity.fingerprint().to_owned(), *identity.verifying_key());
        // The owner holds this root's operator lock; the controller lifetime
        // is a lease on that same lock.
        let store_owner =
            crate::state_store::test_native::TestStateStore::open(&root, signer, Arc::new(trust))
                .unwrap();
        let controller_lifetime = store_owner.controller_lifetime();
        let store = Arc::clone(store_owner.store());
        let binding = RetainedExternalExecutionBinding::test_fixture();
        let channel_authority = ExternalChannelAuthority::test_fixture(&"3".repeat(64));
        let now = lillux::time::timestamp_millis();
        let reservation = ExternalAllocationReservation {
            schema: EXTERNAL_ALLOCATION_RESERVATION_SCHEMA,
            placement_thread_id: "T-one".into(),
            admitted_capsule_hash: "a".repeat(64),
            owner: ExternalAllocationOwner::DedicatedSession(ExternalDedicatedSessionOwner {
                workspace_id: "W-one".into(),
                worker_instance_id: "worker-one".into(),
                worker_boot_epoch: 1,
            }),
            base_snapshot_hash: "b".repeat(64),
            binding_hash: binding.digest().into(),
            capacity_owner: binding.capacity_owner().into(),
            channel_authority_generation: "3".repeat(64),
            channel_owner_public_key: channel_authority.owner_public_key(),
            channel_bootstrap_capability_hash: channel_authority.bootstrap_capability_hash(),
            admitted_adapter_request_base64: None,
            request_digest: "e".repeat(64),
            max_active: 1,
            timeout_seconds: 60,
            startup_started_at_ms: now,
            startup_deadline_ms: now + startup_ms,
            contact_deadline_ms: lillux::time::timestamp_millis() + 60_000,
        };
        store
            .install_external_placement_test_fixture(&reservation, &binding)
            .unwrap();
        (
            store_owner,
            store,
            reservation,
            binding,
            controller_lifetime,
            lock_path,
        )
    }

    #[test]
    fn guest_assignment_is_signed_by_retained_node_for_only_the_bound_occurrence() {
        let (_store_owner, store, reservation, binding, _lifetime, lock_path) =
            placement_store_fixture();
        assert!(matches!(
            store
                .claim_external_allocation_contact("T-one", &reservation.request_digest)
                .unwrap(),
            ExternalAllocationContactClaim::Contact(_)
        ));
        let occurrence = ExternalAllocationOccurrence {
            schema: 1,
            binding_hash: reservation.binding_hash.clone(),
            request_digest: reservation.request_digest.clone(),
            occurrence_id: "assigned-occurrence".into(),
            provider_observation_digest: "f".repeat(64),
        };
        store
            .bind_external_allocation("T-one", &occurrence, test_observation_timing())
            .unwrap();
        let temporary = tempfile::tempdir().unwrap();
        let inputs = guest_input_authority(
            &temporary.path().join("guest-inputs"),
            &reservation.base_snapshot_hash,
        );
        let package_parent = lillux::PinnedDirectory::open(temporary.path())
            .unwrap()
            .unwrap();
        let channel =
            ExternalChannelAuthority::test_fixture(&reservation.channel_authority_generation);
        let contract = binding.backend_contract();
        let (intent, _activation) = supervisor_activation(
            &contract,
            &reservation,
            &occurrence,
            &channel,
            &AdmittedExternalExecutionProgram::StructuredSession(program()),
            inputs,
            &FaultBackend::new(),
            None,
            &package_parent,
            reservation.startup_deadline().unwrap(),
            crate::external_supervisor_native::test_supervisor_journal_envelope(),
        )
        .unwrap();
        let signed = store
            .author_external_guest_assignment(&reservation, &occurrence, &intent, &contract)
            .unwrap();
        let root = lock_path.ancestors().nth(3).unwrap();
        let identity =
            ryeos_node_runtime::identity::NodeIdentity::load(&root.join("node-key.pem")).unwrap();
        let verified = ryeos_external_execution::guest_import_authorization::verify_signed_guest_occurrence_assignment(
            signed.clone(),
            identity.verifying_key(),
        )
        .unwrap();
        assert_eq!(
            verified.assignment().occurrence_id,
            occurrence.occurrence_id
        );
        assert_eq!(
            verified.assignment().guest_runtime_manifest_hash,
            contract.guest_runtime_manifest_hash
        );
        let mut changed = occurrence.clone();
        changed.occurrence_id = "substituted-occurrence".into();
        assert!(
            store
                .author_external_guest_assignment(&reservation, &changed, &intent, &contract)
                .is_err()
        );
        let mut changed_contract = contract.clone();
        changed_contract.guest_runtime_manifest_hash = "7".repeat(64);
        assert!(
            store
                .author_external_guest_assignment(
                    &reservation,
                    &occurrence,
                    &intent,
                    &changed_contract
                )
                .is_err()
        );
        assert!(
            store
                .begin_external_supervisor_activation("T-one", &intent)
                .unwrap()
        );
        assert!(
            store
                .author_external_guest_assignment(&reservation, &occurrence, &intent, &contract)
                .is_err()
        );
    }

    #[test]
    fn normal_direct_settlement_does_not_admit_absent_or_session_allocations() {
        // Storage-only negative admission test, not a real born direct launch.
        let (_store_owner, store, reservation, _, lifetime, _) = placement_store_fixture();
        let app_root = tempfile::tempdir().unwrap();
        // One test Store per thread: compose the application over the
        // fixture's Store instead of opening another.
        let state =
            crate::state::test_support::build_with_store(app_root.path(), store.clone(), lifetime)
                .unwrap();
        assert!(advance_external_direct_settlement(&state, "absent").is_err());
        assert!(!preserves_external_direct_settlement(&state, "absent").unwrap());
        assert!(
            advance_external_direct_settlement(&state, &reservation.placement_thread_id).is_err()
        );
        assert!(
            !preserves_external_direct_settlement(&state, &reservation.placement_thread_id)
                .unwrap()
        );
        assert_eq!(
            store
                .external_allocation(&reservation.placement_thread_id)
                .unwrap()
                .unwrap()
                .phase,
            ExternalAllocationPhase::Reserved
        );
        // Automatic fencing still cancels incomplete session work.
        assert!(
            fence_external_candidate_automatically(&state, &reservation.placement_thread_id)
                .unwrap()
        );
        assert_eq!(
            store
                .external_allocation(&reservation.placement_thread_id)
                .unwrap()
                .unwrap()
                .phase,
            ExternalAllocationPhase::NoContact
        );
    }

    fn replacement_controller_fence_fixture(contacted: bool) {
        let (predecessor_owner, predecessor, reservation, _, predecessor_lifetime, lock_path) =
            placement_store_fixture();
        assert_eq!(
            predecessor
                .external_allocation(&reservation.placement_thread_id)
                .unwrap()
                .unwrap()
                .phase,
            ExternalAllocationPhase::Reserved
        );
        if contacted {
            assert!(matches!(
                predecessor
                    .claim_external_allocation_contact(
                        &reservation.placement_thread_id,
                        &reservation.request_digest,
                    )
                    .unwrap(),
                ExternalAllocationContactClaim::Contact(_)
            ));
        }
        drop(predecessor);
        drop(predecessor_lifetime);
        drop(predecessor_owner);

        let (_reopened_owner, reopened, replacement_lifetime) = reopen_placement_store(&lock_path);
        let app_root = tempfile::tempdir().unwrap();
        let replacement = crate::state::test_support::build_with_store(
            app_root.path(),
            reopened.clone(),
            replacement_lifetime.clone(),
        )
        .unwrap();

        assert_eq!(
            fence_external_candidates_after_controller_restart(&replacement).unwrap(),
            1
        );
        assert_eq!(
            fence_external_candidates_after_controller_restart(&replacement).unwrap(),
            usize::from(contacted)
        );
        assert_eq!(
            reopened
                .external_allocation(&reservation.placement_thread_id)
                .unwrap()
                .unwrap()
                .phase,
            if contacted {
                ExternalAllocationPhase::Quarantined
            } else {
                ExternalAllocationPhase::NoContact
            }
        );
        assert_eq!(
            reopened.recoverable_external_cleanup_placements().unwrap(),
            if contacted {
                vec![reservation.placement_thread_id]
            } else {
                Vec::new()
            }
        );
    }

    fn reopen_placement_store(
        lock_path: &std::path::Path,
    ) -> (
        crate::state_store::test_native::TestStateStore,
        Arc<crate::state_store::StateStore>,
        Arc<StateLockLease>,
    ) {
        let root = lock_path.ancestors().nth(3).unwrap().to_path_buf();
        assert_eq!(
            ryeos_node_runtime::state_lock::default_lock_path(&root),
            lock_path
        );
        let identity =
            ryeos_node_runtime::identity::NodeIdentity::load(&root.join("node-key.pem")).unwrap();
        let signer = Arc::new(crate::state_store::NodeIdentitySigner::from_identity(
            &identity,
        ));
        let mut trust = ryeos_state::refs::TrustStore::new();
        trust.insert(identity.fingerprint().to_owned(), *identity.verifying_key());
        let reopened_owner =
            crate::state_store::test_native::TestStateStore::open(&root, signer, Arc::new(trust))
                .unwrap();
        let replacement_lifetime = reopened_owner.controller_lifetime();
        let reopened = Arc::clone(reopened_owner.store());
        (reopened_owner, reopened, replacement_lifetime)
    }

    #[test]
    fn replacement_controller_fences_uncontacted_allocation_without_provider_io() {
        replacement_controller_fence_fixture(false);
    }

    #[test]
    fn replacement_controller_quarantines_ambiguous_contact_without_provider_io() {
        replacement_controller_fence_fixture(true);
    }

    #[test]
    fn reopened_reserved_allocation_refuses_impossible_package_before_contact() {
        let (predecessor_owner, predecessor, reservation, binding, predecessor_lifetime, lock_path) =
            placement_store_fixture();
        drop(predecessor);
        drop(predecessor_lifetime);
        drop(predecessor_owner);

        let (_reopened_owner, reopened, replacement_lifetime) = reopen_placement_store(&lock_path);
        let backend = Arc::new(FaultBackend::new());
        let gate = Arc::new(AtomicBool::new(false));
        let mut prepared = prepared_fixture(
            reopened.clone(),
            backend.clone(),
            &reservation,
            &binding,
            gate.clone(),
            replacement_lifetime,
        );
        assert_eq!(prepared.record.phase, ExternalAllocationPhase::Reserved);
        // The exact two executable roots are 4096 bytes each. An older
        // retained reservation must not bypass the new pre-contact check.
        prepared.contract.max_guest_package_regular_bytes = 8191;
        let error = prepared
            .claim()
            .err()
            .expect("undersized package was admitted");
        assert!(error.to_string().contains("before allocation"), "{error:#}");
        assert_eq!(backend.allocate_calls.load(Ordering::SeqCst), 0);
        assert!(!gate.load(Ordering::SeqCst));
        assert_eq!(
            reopened
                .external_allocation(&reservation.placement_thread_id)
                .unwrap()
                .unwrap()
                .phase,
            ExternalAllocationPhase::Reserved
        );
    }

    #[test]
    fn replacement_controller_reconciles_original_allocation_without_duplicate_contact() {
        let (predecessor_owner, predecessor, reservation, binding, predecessor_lifetime, lock_path) =
            placement_store_fixture();
        let backend = Arc::new(FaultBackend::new());
        let decision = prepared_fixture(
            predecessor.clone(),
            backend.clone(),
            &reservation,
            &binding,
            Arc::new(AtomicBool::new(false)),
            predecessor_lifetime.clone(),
        )
        .claim()
        .unwrap();
        let ExternalPlacementContactDecision::Contact(contact) = decision else {
            panic!("predecessor did not own the first allocation contact");
        };
        assert!(contact.contact(test_startup_deadline()).is_err());
        assert_eq!(backend.allocate_calls.load(Ordering::SeqCst), 1);
        drop(predecessor);
        drop(predecessor_lifetime);
        drop(predecessor_owner);

        let (_reopened_owner, reopened, replacement_lifetime) = reopen_placement_store(&lock_path);
        let app_root = tempfile::tempdir().unwrap();
        let replacement = crate::state::test_support::build_with_store(
            app_root.path(),
            reopened.clone(),
            replacement_lifetime.clone(),
        )
        .unwrap();
        assert_eq!(
            fence_external_candidates_after_controller_restart(&replacement).unwrap(),
            1
        );
        assert_eq!(
            reopened
                .external_allocation(&reservation.placement_thread_id)
                .unwrap()
                .unwrap()
                .phase,
            ExternalAllocationPhase::Quarantined
        );
        let decision = prepared_fixture(
            reopened.clone(),
            backend.clone(),
            &reservation,
            &binding,
            Arc::new(AtomicBool::new(false)),
            replacement_lifetime.clone(),
        )
        .claim()
        .unwrap();
        let ExternalPlacementContactDecision::Reconcile(recovery) = decision else {
            panic!("replacement did not retain reconciliation of the original request");
        };
        let settled = recovery.reconcile().unwrap();
        assert_eq!(settled.phase, ExternalAllocationPhase::Quarantined);
        assert_eq!(
            settled.occurrence.as_ref().unwrap().occurrence_id,
            "fixture-occurrence"
        );
        assert_eq!(backend.allocate_calls.load(Ordering::SeqCst), 1);
        assert_eq!(backend.allocation_observations.load(Ordering::SeqCst), 1);

        let decision = prepared_fixture(
            reopened.clone(),
            backend.clone(),
            &reservation,
            &binding,
            Arc::new(AtomicBool::new(false)),
            replacement_lifetime.clone(),
        )
        .claim()
        .unwrap();
        let ExternalPlacementContactDecision::Reconcile(cleanup) = decision else {
            panic!("replacement lost cleanup authority for the quarantined occurrence");
        };
        assert!(cleanup.terminate_or_reconcile().is_err());
        assert_eq!(backend.terminate_calls.load(Ordering::SeqCst), 1);

        let decision = prepared_fixture(
            reopened,
            backend.clone(),
            &reservation,
            &binding,
            Arc::new(AtomicBool::new(false)),
            replacement_lifetime,
        )
        .claim()
        .unwrap();
        let ExternalPlacementContactDecision::Reconcile(cleanup) = decision else {
            panic!("replacement lost exact termination reconciliation authority");
        };
        assert_eq!(
            cleanup.terminate_or_reconcile().unwrap().phase,
            ExternalAllocationPhase::Terminated
        );
        assert_eq!(backend.terminate_calls.load(Ordering::SeqCst), 1);
        assert_eq!(backend.termination_observations.load(Ordering::SeqCst), 1);
    }

    fn prepared_fixture(
        store: Arc<crate::state_store::StateStore>,
        backend: Arc<FaultBackend>,
        reservation: &ExternalAllocationReservation,
        binding: &RetainedExternalExecutionBinding,
        gate: Arc<AtomicBool>,
        controller_lifetime: Arc<StateLockLease>,
    ) -> PreparedExternalPlacement {
        PreparedExternalPlacement {
            state_store: store.clone(),
            vault: Arc::new(crate::vault::EmptyVault),
            backend,
            contract: binding.backend_contract(),
            credential: credential(&InstalledExternalExecutionBinding::test_fixture()),
            contact_gate: gate,
            controller_lifetime,
            channel_authority: ExternalChannelAuthority::test_fixture(
                &reservation.channel_authority_generation,
            ),
            program: AdmittedExternalExecutionProgram::StructuredSession(program()),
            guest_inputs: None,
            qualification_activation: None,
            runtime_upload: None,
            native_journal:
                crate::external_supervisor_native::SupervisorJournalEnvelopeSource::Fixed(
                    crate::external_supervisor_native::test_supervisor_journal_envelope(),
                ),
            record: store
                .external_allocation(&reservation.placement_thread_id)
                .unwrap()
                .unwrap(),
        }
    }

    fn prepared_fixture_with_guest_inputs(
        store: Arc<crate::state_store::StateStore>,
        backend: Arc<FaultBackend>,
        reservation: &ExternalAllocationReservation,
        binding: &RetainedExternalExecutionBinding,
        gate: Arc<AtomicBool>,
        controller_lifetime: Arc<StateLockLease>,
        guest_root: &std::path::Path,
    ) -> PreparedExternalPlacement {
        prepared_fixture(
            store,
            backend,
            reservation,
            binding,
            gate,
            controller_lifetime,
        )
        .with_guest_inputs(
            guest_input_authority(guest_root, &reservation.base_snapshot_hash),
            None,
        )
        .unwrap()
    }

    #[test]
    fn exact_program_account_and_adapter_artifact_are_all_required() {
        let binding = InstalledExternalExecutionBinding::test_fixture();
        let selected = select_binding(std::slice::from_ref(&binding), &program()).unwrap();
        let credential = credential(selected);
        let registry =
            ExternalPlacementBackendRegistry::from_backends(vec![Arc::new(FixtureBackend {
                artifact: "d".repeat(64),
            })])
            .unwrap();
        registry
            .qualify(
                &selected.backend_contract(),
                &credential,
                &AdmittedExternalExecutionProgram::StructuredSession(program()),
            )
            .unwrap();

        let wrong_artifact =
            ExternalPlacementBackendRegistry::from_backends(vec![Arc::new(FixtureBackend {
                artifact: "e".repeat(64),
            })])
            .unwrap();
        assert!(
            wrong_artifact
                .qualify(
                    &selected.backend_contract(),
                    &credential,
                    &AdmittedExternalExecutionProgram::StructuredSession(program())
                )
                .is_err()
        );
        assert!(select_binding(&[], &program()).is_err());
        assert!(select_binding(&[binding.clone(), binding], &program()).is_err());

        let current = InstalledExternalExecutionBinding::test_fixture();
        require_current_contact_binding(
            std::slice::from_ref(&current),
            &program(),
            current.digest(),
        )
        .unwrap();
        assert!(require_current_contact_binding(&[], &program(), current.digest()).is_err());
        assert!(
            require_current_contact_binding(
                std::slice::from_ref(&current),
                &program(),
                &"e".repeat(64),
            )
            .is_err()
        );
    }

    fn content_selection_fixture()
    -> crate::node_config::sections::external_execution::ExternalRuntimeContentQualificationBinding
    {
        crate::node_config::sections::external_execution::ExternalRuntimeContentQualificationBinding {
            activation_ref: "config:codex/runtime-activation".into(),
            coordinate_id: "1".repeat(64),
            attestation_hash: "2".repeat(64),
            owner_principal: format!("fp:{}", "3".repeat(64)),
            required_claims: vec!["runtime_compatible".into()],
            remote_testimony: None,
        }
    }

    #[test]
    fn a_product_sourced_session_admits_no_content_testimony() {
        let root = tempfile::tempdir().unwrap();
        let (_owner, state) = crate::state::test_support::build(root.path()).unwrap();
        let mut contract = InstalledExternalExecutionBinding::test_fixture().backend_contract();
        contract.runtime_content_qualification = None;
        assert!(
            require_current_runtime_content_qualification(
                &state,
                &contract,
                RuntimeContentLane::Product
            )
            .unwrap()
            .is_none()
        );
        contract.runtime_content_qualification = Some(content_selection_fixture());
        let error = require_current_runtime_content_qualification(
            &state,
            &contract,
            RuntimeContentLane::Product,
        )
        .err()
        .unwrap();
        assert!(
            format!("{error:#}").contains("carries no acquired-content testimony"),
            "{error:#}"
        );
    }

    #[test]
    fn an_activation_sourced_session_still_requires_content_testimony() {
        let root = tempfile::tempdir().unwrap();
        let (_owner, state) = crate::state::test_support::build(root.path()).unwrap();
        let mut contract = InstalledExternalExecutionBinding::test_fixture().backend_contract();
        contract.runtime_content_qualification = None;
        let error = require_current_runtime_content_qualification(
            &state,
            &contract,
            RuntimeContentLane::Activation,
        )
        .err()
        .unwrap();
        assert!(
            format!("{error:#}").contains("no qualified runtime content selection"),
            "{error:#}"
        );
    }

    #[test]
    fn the_start_path_keeps_the_workload_and_lane_content_refusals() {
        let root = tempfile::tempdir().unwrap();
        let (_owner, state) = crate::state::test_support::build(root.path()).unwrap();
        let mut contract =
            InstalledExternalExecutionBinding::direct_test_fixture(30).backend_contract();
        contract.runtime_content_qualification = Some(content_selection_fixture());
        let error = require_current_runtime_qualification_for_start(&state, &contract, "0", None)
            .err()
            .unwrap();
        assert!(
            format!("{error:#}").contains("direct workload cannot inherit"),
            "{error:#}"
        );
        let mut session = InstalledExternalExecutionBinding::test_fixture().backend_contract();
        session.runtime_content_qualification = Some(content_selection_fixture());
        // The fixture program takes its runtime from a captured product.
        let product = program();
        let error =
            require_current_runtime_qualification_for_start(&state, &session, "0", Some(&product))
                .err()
                .unwrap();
        assert!(
            format!("{error:#}").contains("carries no acquired-content testimony"),
            "{error:#}"
        );
    }

    #[test]
    fn a_retained_product_sourced_session_carries_selections_and_no_content() {
        require_product_sourced_retention(false, true).unwrap();
        assert!(require_product_sourced_retention(true, true).is_err());
        assert!(require_product_sourced_retention(false, false).is_err());
    }

    #[test]
    fn the_lane_follows_the_programs_runtime_source() {
        let program = program();
        let expected = match program.runtime_source {
            ryeos_state::external_execution::admission::ExternalCandidateRuntimeSource::CapturedProduct { .. } => RuntimeContentLane::Product,
            _ => RuntimeContentLane::Activation,
        };
        assert_eq!(RuntimeContentLane::of(&program), expected);
    }

    #[test]
    fn ordinary_guest_wire_floor_refuses_absent_and_older_claim_selection() {
        use ryeos_external_execution_contract::{
            DIRECT_PROGRAM_SUPPORT_CLAIM, GUEST_WIRE_SUPPORT_CLAIM,
        };
        let binding = InstalledExternalExecutionBinding::direct_test_fixture(30);
        let mut contract = binding.backend_contract();
        contract.runtime_qualification = None;
        assert!(require_ordinary_guest_wire_support_selection(&contract).is_err());
        contract.runtime_qualification = Some(
            crate::node_config::sections::external_execution::ExternalRuntimeQualificationBinding {
                attestation_hash: "1".repeat(64),
                owner_principal: format!("fp:{}", "2".repeat(64)),
                qualification: ryeos_state::external_content::products::composition::ProductRelationshipQualification {
                    policy_ref: Some("config:fixture/guest-mechanisms".into()),
                    required_claims: vec!["render_snapshot_v1".into()],
                },
                remote_testimony: None,
            },
        );
        assert!(require_ordinary_guest_wire_support_selection(&contract).is_err());
        let selected = contract.runtime_qualification.as_mut().unwrap();
        selected
            .qualification
            .required_claims
            .push(GUEST_WIRE_SUPPORT_CLAIM.into());
        assert!(require_ordinary_guest_wire_support_selection(&contract).is_err());
        contract
            .runtime_qualification
            .as_mut()
            .unwrap()
            .qualification
            .required_claims
            .push(DIRECT_PROGRAM_SUPPORT_CLAIM.into());
        let selected = contract.runtime_qualification.as_mut().unwrap();
        selected.qualification.required_claims.sort();
        selected.validate().unwrap();
        // Selection alone is not evidence: current witness and probe remain
        // mandatory in admit_current_runtime_qualification.
        require_ordinary_guest_wire_support_selection(&contract).unwrap();
    }

    #[test]
    fn structured_session_guest_wire_floor_accepts_exactly_the_wire_claim() {
        use ryeos_external_execution_contract::{
            DIRECT_PROGRAM_SUPPORT_CLAIM, GUEST_WIRE_SUPPORT_CLAIM,
        };
        let binding = InstalledExternalExecutionBinding::test_fixture();
        let mut contract = binding.backend_contract();
        assert!(matches!(
            contract.workload,
            crate::node_config::sections::external_execution::ExternalWorkloadBinding::StructuredSession(_)
        ));
        contract.runtime_qualification = Some(
            crate::node_config::sections::external_execution::ExternalRuntimeQualificationBinding {
                attestation_hash: "1".repeat(64),
                owner_principal: format!("fp:{}", "2".repeat(64)),
                qualification: ryeos_state::external_content::products::composition::ProductRelationshipQualification {
                    policy_ref: Some("config:codex/guest-wire-qualification".into()),
                    required_claims: vec![GUEST_WIRE_SUPPORT_CLAIM.into()],
                },
                remote_testimony: None,
            },
        );
        contract
            .runtime_qualification
            .as_ref()
            .unwrap()
            .validate()
            .unwrap();
        // A structured session needs the wire claim and nothing more.
        require_ordinary_guest_wire_support_selection(&contract).unwrap();
        // The direct-program claim alone does not satisfy the wire floor.
        contract
            .runtime_qualification
            .as_mut()
            .unwrap()
            .qualification
            .required_claims = vec![DIRECT_PROGRAM_SUPPORT_CLAIM.into()];
        assert!(require_ordinary_guest_wire_support_selection(&contract).is_err());
    }

    #[test]
    fn retained_guest_runtime_proof_matches_only_its_signed_binding() {
        let binding = RetainedExternalExecutionBinding::test_fixture();
        let mut contract = binding.backend_contract();
        let selections = ryeos_state::external_content::products::qualification::test_support::qualified_runtime_selections(
            &contract.guest_runtime_manifest_hash,
        )
        .unwrap();
        let proof = selections
            .get("auxiliary")
            .unwrap()
            .qualification
            .clone()
            .unwrap();
        let owner_principal = format!("fp:{}", "7".repeat(64));
        contract.runtime_qualification = Some(
            crate::node_config::sections::external_execution::ExternalRuntimeQualificationBinding {
                attestation_hash: proof.attestation_hash.clone(),
                owner_principal: owner_principal.clone(),
                qualification: ryeos_state::external_content::products::composition::ProductRelationshipQualification {
                    policy_ref: Some("config:render/snapshot-qualification".into()),
                    required_claims: vec!["render_snapshot_v1".into()],
                },
                remote_testimony: None,
            },
        );
        let retained = ryeos_state::objects::RetainedExternalRuntimeQualification {
            binding_hash: binding.digest().to_owned(),
            guest_runtime_manifest_hash: contract.guest_runtime_manifest_hash.clone(),
            owner_principal,
            proof,
            remote_testimony: None,
        };
        assert!(
            match_retained_session_runtime_qualification(
                Some(&retained),
                &contract,
                binding.digest()
            )
            .unwrap()
            .is_some()
        );
        assert!(
            match_retained_session_runtime_qualification(None, &contract, binding.digest())
                .is_err()
        );
        let mut changed = retained.clone();
        changed.binding_hash = "8".repeat(64);
        assert!(
            match_retained_session_runtime_qualification(
                Some(&changed),
                &contract,
                binding.digest()
            )
            .is_err()
        );
        changed = retained.clone();
        changed.proof.attestation_hash = "9".repeat(64);
        assert!(
            match_retained_session_runtime_qualification(
                Some(&changed),
                &contract,
                binding.digest()
            )
            .is_err()
        );
        changed = retained.clone();
        changed.owner_principal = format!("fp:{}", "a".repeat(64));
        assert!(
            match_retained_session_runtime_qualification(
                Some(&changed),
                &contract,
                binding.digest()
            )
            .is_err()
        );
        changed = retained.clone();
        changed.guest_runtime_manifest_hash = "b".repeat(64);
        assert!(
            match_retained_session_runtime_qualification(
                Some(&changed),
                &contract,
                binding.digest()
            )
            .is_err()
        );
        contract.runtime_qualification = None;
        assert!(
            match_retained_session_runtime_qualification(
                Some(&retained),
                &contract,
                binding.digest()
            )
            .is_err()
        );
    }

    fn remote_testimony_selection(
        acceptance_hash: &str,
    ) -> crate::node_config::sections::external_execution::ExternalRuntimeQualificationBinding {
        use ryeos_external_execution_contract::{
            DIRECT_PROGRAM_SUPPORT_CLAIM, GUEST_WIRE_SUPPORT_CLAIM,
        };
        let mut claims = vec![
            GUEST_WIRE_SUPPORT_CLAIM.to_owned(),
            DIRECT_PROGRAM_SUPPORT_CLAIM.to_owned(),
        ];
        claims.sort();
        crate::node_config::sections::external_execution::ExternalRuntimeQualificationBinding {
            attestation_hash: "1".repeat(64),
            owner_principal: format!("fp:{}", "2".repeat(64)),
            qualification: ryeos_state::external_content::products::composition::ProductRelationshipQualification {
                policy_ref: Some("config:fixture/guest-mechanisms".into()),
                required_claims: claims,
            },
            remote_testimony: Some(
                crate::node_config::sections::external_execution::ExternalRemoteQualificationTestimonyBinding {
                    acceptance_hash: acceptance_hash.to_owned(),
                    acceptance_policy_ref: "config:fixture/guest-acceptance".into(),
                },
            ),
        }
    }

    #[test]
    fn a_remote_testimony_selection_is_refused_without_a_current_attester_grant() {
        let binding = InstalledExternalExecutionBinding::direct_test_fixture(30);
        let mut contract = binding.backend_contract();
        contract.runtime_qualification = Some(remote_testimony_selection(&"3".repeat(64)));
        contract
            .runtime_qualification
            .as_ref()
            .unwrap()
            .validate()
            .unwrap();
        require_ordinary_guest_wire_support_selection(&contract).unwrap();
        let root = tempfile::tempdir().unwrap();
        let (_owner, state) = crate::state::test_support::build(root.path()).unwrap();
        let error =
            admit_current_runtime_qualification(&state, &contract, binding.digest()).unwrap_err();
        assert!(
            format!("{error:#}").contains("grant was revoked"),
            "{error:#}"
        );
    }

    #[test]
    fn a_retained_proof_must_agree_with_the_binding_on_remote_testimony() {
        let binding = RetainedExternalExecutionBinding::test_fixture();
        let mut contract = binding.backend_contract();
        let selections = ryeos_state::external_content::products::qualification::test_support::qualified_runtime_selections(
            &contract.guest_runtime_manifest_hash,
        )
        .unwrap();
        let proof = selections
            .get("auxiliary")
            .unwrap()
            .qualification
            .clone()
            .unwrap();
        let owner_principal = format!("fp:{}", "7".repeat(64));
        let mut selected = remote_testimony_selection(&"3".repeat(64));
        selected.attestation_hash = proof.attestation_hash.clone();
        selected.owner_principal = owner_principal.clone();
        contract.runtime_qualification = Some(selected);
        let retained = ryeos_state::objects::RetainedExternalRuntimeQualification {
            binding_hash: binding.digest().to_owned(),
            guest_runtime_manifest_hash: contract.guest_runtime_manifest_hash.clone(),
            owner_principal,
            proof,
            remote_testimony: None,
        };
        // A binding that selects a remote testimony never matches a retained
        // proof that carries none, and the reverse holds too.
        assert!(
            match_retained_session_runtime_qualification(
                Some(&retained),
                &contract,
                binding.digest()
            )
            .is_err()
        );
        contract
            .runtime_qualification
            .as_mut()
            .unwrap()
            .remote_testimony = None;
        assert!(
            match_retained_session_runtime_qualification(
                Some(&retained),
                &contract,
                binding.digest()
            )
            .unwrap()
            .is_some()
        );
    }

    #[test]
    fn runtime_probe_interpretation_requires_an_exact_installed_backend() {
        let binding = RetainedExternalExecutionBinding::test_fixture();
        let contract = binding.backend_contract();
        let selections = ryeos_state::external_content::products::qualification::test_support::qualified_runtime_selections(
            &contract.guest_runtime_manifest_hash,
        )
        .unwrap();
        let proof = selections
            .get("auxiliary")
            .unwrap()
            .qualification
            .as_ref()
            .unwrap();
        let input = RuntimeProbeInput::from_product(proof);
        assert_eq!(
            input.runtime_source,
            ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotSource::CapturedProduct {
                product_witness_hash: proof.evidence.product_witness_hash.clone(),
            }
        );
        assert_eq!(input.qualification_attestation_hash, proof.attestation_hash);
        assert_eq!(
            input.subject_manifest_hash,
            proof.evidence.result.subject_manifest_hash
        );
        assert_eq!(input.probe_evidence, proof.evidence.result.probe_evidence);
        let source =
            ryeos_external_execution::guest_runtime_product::GuestOwnerRuntimeManifestIdentity {
                manifest_hash: contract.guest_runtime_manifest_hash.clone(),
                owner_executable_sha256: "7".repeat(64),
                controller_root_blob_sha256: "8".repeat(64),
                controller_public_root: format!("ed25519:{}", "A".repeat(44)),
            };
        assert!(
            ExternalPlacementBackendRegistry::default()
                .verify_runtime_probe(&contract, &input, &source, binding.digest(),)
                .is_err()
        );
        let registry =
            ExternalPlacementBackendRegistry::from_backends(vec![Arc::new(FixtureBackend {
                artifact: contract.backend_artifact_hash.clone(),
            })])
            .unwrap();
        assert!(
            registry
                .verify_runtime_probe(&contract, &input, &source, binding.digest(),)
                .is_err()
        );
    }

    #[test]
    fn lifecycle_capabilities_are_required_before_contact() {
        let binding = InstalledExternalExecutionBinding::test_fixture();
        let contract = binding.backend_contract();
        let credential = credential(&binding);
        let activation_and_terminal = Arc::new(FaultBackend {
            capabilities: BTreeSet::from([
                LifecycleCapability::SupervisorActivation,
                LifecycleCapability::ExactTerminalObservation,
            ]),
            ..FaultBackend::new()
        });
        let registry =
            ExternalPlacementBackendRegistry::from_backends(vec![activation_and_terminal.clone()])
                .unwrap();
        // No reconciliation demand is legitimate, but conveys no right to
        // resolve an unknown allocation or release its reservation.
        registry
            .qualify(
                &contract,
                &credential,
                &AdmittedExternalExecutionProgram::StructuredSession(program()),
            )
            .unwrap();
        let mut unverified = contract.clone();
        unverified.runtime_qualification = Some(
            crate::node_config::sections::external_execution::ExternalRuntimeQualificationBinding {
                attestation_hash: "6".repeat(64),
                owner_principal: format!("fp:{}", "7".repeat(64)),
                qualification: ryeos_state::external_content::products::composition::ProductRelationshipQualification {
                    policy_ref: Some("config:render/snapshot-qualification".into()),
                    required_claims: vec!["render_snapshot_v1".into()],
                },
                remote_testimony: None,
            },
        );
        assert!(
            registry
                .qualify(
                    &unverified,
                    &credential,
                    &AdmittedExternalExecutionProgram::StructuredSession(program()),
                )
                .is_err()
        );
        let qualified_backend = Arc::new(FaultBackend {
            capabilities: BTreeSet::from([
                LifecycleCapability::SupervisorActivation,
                LifecycleCapability::IndependentGuestRuntimeAdmission,
                LifecycleCapability::ExactTerminalObservation,
            ]),
            ..FaultBackend::new()
        });
        let qualified_registry =
            ExternalPlacementBackendRegistry::from_backends(vec![qualified_backend]).unwrap();
        assert!(
            qualified_registry
                .qualify(
                    &contract,
                    &credential,
                    &AdmittedExternalExecutionProgram::StructuredSession(program()),
                )
                .is_err(),
            "an active independent-runtime adapter cannot admit a missing relationship"
        );
        // The registry checks the shape. Session admission and first contact
        // independently authenticate the exact witness and adapter probe.
        qualified_registry
            .qualify(
                &unverified,
                &credential,
                &AdmittedExternalExecutionProgram::StructuredSession(program()),
            )
            .unwrap();
        registry
            .qualify_for_cleanup(
                &unverified,
                &credential,
                &AdmittedExternalExecutionProgram::StructuredSession(program()),
            )
            .unwrap();
        assert_eq!(
            activation_and_terminal
                .allocate_calls
                .load(Ordering::SeqCst),
            0
        );

        let terminal_only = Arc::new(FaultBackend {
            capabilities: BTreeSet::from([LifecycleCapability::ExactTerminalObservation]),
            ..FaultBackend::new()
        });
        let cleanup_registry =
            ExternalPlacementBackendRegistry::from_backends(vec![terminal_only.clone()]).unwrap();
        let admitted = AdmittedExternalExecutionProgram::StructuredSession(program());
        assert!(
            cleanup_registry
                .qualify(&contract, &credential, &admitted)
                .is_err()
        );
        cleanup_registry
            .qualify_for_cleanup(&contract, &credential, &admitted)
            .unwrap();
        assert_eq!(terminal_only.allocate_calls.load(Ordering::SeqCst), 0);

        for missing in all_lifecycle_capabilities() {
            let mut required_program = program();
            // Activation and terminal evidence are required by placement
            // itself, even when the workload requests no additional claim.
            if !matches!(
                missing,
                LifecycleCapability::SupervisorActivation
                    | LifecycleCapability::ExactTerminalObservation
            ) {
                required_program
                    .requirement
                    .required_lifecycle_capabilities
                    .insert(missing);
                required_program.qualification_use =
                    ryeos_state::external_execution::admission::test_support::fixture_qualification_use(
                        &required_program.requirement,
                    )
                    .unwrap();
            }
            let mut capabilities = all_lifecycle_capabilities();
            capabilities.remove(&missing);
            let backend = Arc::new(FaultBackend {
                capabilities,
                ..FaultBackend::new()
            });
            let registry =
                ExternalPlacementBackendRegistry::from_backends(vec![backend.clone()]).unwrap();
            let error = registry
                .qualify(
                    &contract,
                    &credential,
                    &AdmittedExternalExecutionProgram::StructuredSession(required_program),
                )
                .unwrap_err();
            assert!(
                error.to_string().contains("lacks required capabilities"),
                "{error:#}"
            );
            assert_eq!(backend.allocate_calls.load(Ordering::SeqCst), 0);
            assert_eq!(backend.allocation_observations.load(Ordering::SeqCst), 0);
            assert_eq!(backend.activation_calls.load(Ordering::SeqCst), 0);
            assert_eq!(backend.terminate_calls.load(Ordering::SeqCst), 0);
        }
    }

    #[test]
    fn lifecycle_capabilities_do_not_allow_stronger_allocation_settlement() {
        for resolution in ["bound", "no_occurrence", "pending"] {
            let (_store_owner, store, reservation, binding, lifetime, _) =
                placement_store_fixture();
            let backend = Arc::new(FaultBackend {
                capabilities: BTreeSet::from([LifecycleCapability::ExactTerminalObservation]),
                reconciled_allocation: resolution,
                ..FaultBackend::new()
            });
            let gate = Arc::new(AtomicBool::new(false));
            let prepared = || {
                prepared_fixture(
                    store.clone(),
                    backend.clone(),
                    &reservation,
                    &binding,
                    gate.clone(),
                    lifetime.clone(),
                )
            };
            let ExternalPlacementContactDecision::Contact(contact) = prepared().claim().unwrap()
            else {
                panic!("first attempt lost its exact contact permit")
            };
            assert!(contact.contact(test_startup_deadline()).is_err());
            let before = store
                .external_allocation(&reservation.placement_thread_id)
                .unwrap()
                .unwrap();
            let ExternalPlacementContactDecision::Reconcile(reconcile) =
                prepared().claim().unwrap()
            else {
                panic!("ambiguous allocation produced a replacement contact permit")
            };
            let result = reconcile.reconcile();
            if resolution == "pending" {
                assert!(result.is_ok());
            } else {
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("undeclared capability")
                );
            }
            let after = store
                .external_allocation(&reservation.placement_thread_id)
                .unwrap()
                .unwrap();
            if resolution == "pending" {
                // Cleanup observes the original uncertainty without granting
                // startup permission, even when no stronger capability exists.
                assert_eq!(after.reservation, before.reservation);
                assert_eq!(after.occurrence, before.occurrence);
                assert_eq!(after.phase, ExternalAllocationPhase::Quarantined);
            } else {
                assert_eq!(after, before);
            }
            assert!(!after.phase.is_settled());
            assert!(after.occurrence.is_none());
            assert_eq!(backend.allocate_calls.load(Ordering::SeqCst), 1);
            assert_eq!(backend.allocation_observations.load(Ordering::SeqCst), 1);
        }
    }

    #[test]
    fn lifecycle_capabilities_do_not_allow_stronger_activation_settlement() {
        for resolution in ["started", "not_started", "pending"] {
            let (_store_owner, store, reservation, binding, lifetime, _) =
                placement_store_fixture();
            let mut capabilities = all_lifecycle_capabilities();
            capabilities.remove(&LifecycleCapability::ExactActivationReconciliation);
            let backend = Arc::new(FaultBackend {
                capabilities,
                reconciled_activation: resolution,
                ..FaultBackend::new()
            });
            let gate = Arc::new(AtomicBool::new(false));
            let prepared = || {
                prepared_fixture(
                    store.clone(),
                    backend.clone(),
                    &reservation,
                    &binding,
                    gate.clone(),
                    lifetime.clone(),
                )
            };
            assert!(advance_prepared_external_start(prepared(), test_startup_deadline()).is_err());
            assert_eq!(
                advance_prepared_external_start(prepared(), test_startup_deadline()).unwrap(),
                ExternalCandidateStartProgress::OccurrenceBound,
            );
            let guest_root = tempfile::tempdir().unwrap();
            let ExternalPlacementContactDecision::Reconcile(activation) =
                prepared_fixture_with_guest_inputs(
                    store.clone(),
                    backend.clone(),
                    &reservation,
                    &binding,
                    gate.clone(),
                    lifetime.clone(),
                    guest_root.path(),
                )
                .claim()
                .unwrap()
            else {
                panic!("bound occurrence lost activation authority")
            };
            assert!(activation.activate_or_reconcile().is_err());
            let before = store
                .external_supervisor_activation(&reservation.placement_thread_id)
                .unwrap()
                .unwrap();
            assert!(before.observation.is_none());
            let allocation = store
                .external_allocation(&reservation.placement_thread_id)
                .unwrap()
                .unwrap();

            for observation_count in 1..=2 {
                let ExternalPlacementContactDecision::Reconcile(activation) =
                    prepared().claim().unwrap()
                else {
                    panic!("ambiguous activation produced replacement contact authority")
                };
                let result = activation.activate_or_reconcile();
                if resolution == "pending" {
                    assert_eq!(result.unwrap(), before);
                } else {
                    assert!(
                        result
                            .unwrap_err()
                            .to_string()
                            .contains("undeclared capability")
                    );
                }
                assert_eq!(
                    store
                        .external_supervisor_activation(&reservation.placement_thread_id)
                        .unwrap()
                        .unwrap(),
                    before,
                );
                assert_eq!(
                    store
                        .external_allocation(&reservation.placement_thread_id)
                        .unwrap()
                        .unwrap(),
                    allocation,
                );
                assert!(!allocation.phase.is_settled());
                assert!(
                    store
                        .optional_external_execution_channel(&reservation.placement_thread_id)
                        .unwrap()
                        .is_none()
                );
                assert!(!gate.load(Ordering::Acquire));
                assert_eq!(backend.allocate_calls.load(Ordering::SeqCst), 1);
                assert_eq!(backend.allocation_observations.load(Ordering::SeqCst), 1);
                assert_eq!(backend.activation_calls.load(Ordering::SeqCst), 1);
                assert_eq!(
                    backend.activation_observations.load(Ordering::SeqCst),
                    observation_count
                );
                assert_eq!(backend.terminate_calls.load(Ordering::SeqCst), 0);
            }
        }
    }

    #[test]
    fn lifecycle_capabilities_do_not_allow_stronger_terminal_settlement() {
        for resolution in ["terminal", "pending"] {
            let (_store_owner, store, reservation, binding, lifetime, _) =
                placement_store_fixture();
            let mut capabilities = all_lifecycle_capabilities();
            capabilities.remove(&LifecycleCapability::ExactTerminalObservation);
            let backend = Arc::new(FaultBackend {
                capabilities,
                reconciled_termination: resolution,
                ..FaultBackend::new()
            });
            let gate = Arc::new(AtomicBool::new(false));
            // Deliberately bypass registry admission to exercise the production
            // settlement guard. This backend cannot qualify for a fresh launch.
            let prepared = || {
                prepared_fixture(
                    store.clone(),
                    backend.clone(),
                    &reservation,
                    &binding,
                    gate.clone(),
                    lifetime.clone(),
                )
            };
            assert!(advance_prepared_external_start(prepared(), test_startup_deadline()).is_err());
            assert_eq!(
                advance_prepared_external_start(prepared(), test_startup_deadline()).unwrap(),
                ExternalCandidateStartProgress::OccurrenceBound,
            );
            let ExternalPlacementContactDecision::Reconcile(cleanup) = prepared().claim().unwrap()
            else {
                panic!("bound occurrence lost cleanup authority")
            };
            assert!(cleanup.terminate_or_reconcile().is_err());
            let before = store
                .external_allocation(&reservation.placement_thread_id)
                .unwrap()
                .unwrap();
            assert_eq!(before.phase, ExternalAllocationPhase::Quarantined);
            let occurrence = before.occurrence.as_ref().unwrap();
            let intent = ExternalTerminationIntent {
                schema: 1,
                binding_hash: reservation.binding_hash.clone(),
                request_digest: reservation.request_digest.clone(),
                occurrence_id: occurrence.occurrence_id.clone(),
                termination_request_digest: ryeos_state::objects::canonical_value_digest(
                    &serde_json::json!({
                        "domain":"ryeos.external-placement-termination.v1",
                        "binding_hash":reservation.binding_hash,
                        "request_digest":reservation.request_digest,
                        "occurrence_id":occurrence.occurrence_id,
                    }),
                )
                .unwrap(),
            };

            for observation_count in 1..=2 {
                let ExternalPlacementContactDecision::Reconcile(cleanup) =
                    prepared().claim().unwrap()
                else {
                    panic!("ambiguous cleanup produced replacement contact authority")
                };
                let result = cleanup.terminate_or_reconcile();
                if resolution == "pending" {
                    assert_eq!(result.unwrap(), before);
                } else {
                    assert!(
                        result
                            .unwrap_err()
                            .to_string()
                            .contains("undeclared capability")
                    );
                }
                assert_eq!(
                    store
                        .external_allocation(&reservation.placement_thread_id)
                        .unwrap()
                        .unwrap(),
                    before,
                );
                // Exact replay returns false only when the same original
                // termination intent remains durable; it cannot create another.
                assert!(
                    !store
                        .begin_external_termination(&reservation.placement_thread_id, &intent)
                        .unwrap()
                );
                assert_eq!(
                    classify_external_start_cleanup(&store, &reservation.placement_thread_id),
                    ExternalCandidateStartCleanup::Unproved
                );
                assert!(!gate.load(Ordering::Acquire));
                assert_eq!(backend.allocate_calls.load(Ordering::SeqCst), 1);
                assert_eq!(backend.allocation_observations.load(Ordering::SeqCst), 1);
                assert_eq!(backend.activation_calls.load(Ordering::SeqCst), 0);
                assert_eq!(backend.terminate_calls.load(Ordering::SeqCst), 1);
                assert_eq!(
                    backend.termination_observations.load(Ordering::SeqCst),
                    observation_count
                );
            }
        }
    }

    #[test]
    fn lifecycle_capabilities_distinguish_first_response_from_reconciliation() {
        let backend = FaultBackend {
            capabilities: BTreeSet::new(),
            ..FaultBackend::new()
        };
        let bound = ExternalAllocationResolution::Bound {
            occurrence_id: "fixture-occurrence".into(),
            provider_observation_digest: "f".repeat(64),
        };
        require_allocation_resolution_capability(&backend, &bound, false).unwrap();
        assert!(require_allocation_resolution_capability(&backend, &bound, true).is_err());
        for reconciliation in [false, true] {
            assert!(
                require_allocation_resolution_capability(
                    &backend,
                    &ExternalAllocationResolution::NoOccurrence {
                        provider_observation_digest: "f".repeat(64)
                    },
                    reconciliation,
                )
                .is_err()
            );
            require_allocation_resolution_capability(
                &backend,
                &ExternalAllocationResolution::Pending,
                reconciliation,
            )
            .unwrap();
        }
    }

    #[test]
    fn registry_rejects_duplicate_or_noncanonical_adapter_identity() {
        assert!(
            ExternalPlacementBackendRegistry::from_backends(vec![
                Arc::new(FixtureBackend {
                    artifact: "d".repeat(64),
                }),
                Arc::new(FixtureBackend {
                    artifact: "d".repeat(64),
                }),
            ])
            .is_err()
        );
        assert!(
            ExternalPlacementBackendRegistry::from_backends(vec![Arc::new(FixtureBackend {
                artifact: "not-a-hash".into(),
            })])
            .is_err()
        );

        let rotated = ExternalPlacementBackendRegistry::from_backends(vec![
            Arc::new(FixtureBackend {
                artifact: "d".repeat(64),
            }),
            Arc::new(FixtureBackend {
                artifact: "e".repeat(64),
            }),
        ])
        .unwrap();
        let binding = InstalledExternalExecutionBinding::test_fixture();
        let credential = credential(&binding);
        rotated
            .qualify(
                &binding.backend_contract(),
                &credential,
                &AdmittedExternalExecutionProgram::StructuredSession(program()),
            )
            .unwrap();
        let mut newer = binding.backend_contract();
        newer.backend_artifact_hash = "e".repeat(64);
        rotated
            .qualify(
                &newer,
                &credential,
                &AdmittedExternalExecutionProgram::StructuredSession(program()),
            )
            .unwrap();
    }

    #[test]
    fn direct_start_lifeline_policy_refuses_replacement_activation() {
        // Pure handoff policy only: no fixture claims to prove born authority.
        for phase in [
            ExternalAllocationPhase::Reserved,
            ExternalAllocationPhase::Bound,
        ] {
            assert!(require_direct_start_lifelines(phase, false, false).is_err());
            assert!(require_direct_start_lifelines(phase, false, true).is_ok());
        }
        assert!(
            require_direct_start_lifelines(ExternalAllocationPhase::Bound, true, false).is_ok()
        );
        assert!(
            require_direct_start_lifelines(ExternalAllocationPhase::Reserved, true, false).is_err()
        );
        for phase in [
            ExternalAllocationPhase::ContactPending,
            ExternalAllocationPhase::Quarantined,
            ExternalAllocationPhase::Terminated,
        ] {
            assert!(require_direct_start_lifelines(phase, false, false).is_ok());
        }
    }

    #[test]
    fn direct_prebirth_preflight_joins_exact_endpoint_without_contact() {
        use ryeos_engine::contracts::{
            ExecutionEndpointRequirement, ExternalEndpointBindingIdentity,
        };
        for terminal in [false, true] {
            let binding = InstalledExternalExecutionBinding::direct_test_fixture(30);
            let identity = ExternalEndpointBindingIdentity {
                binding_id: binding.id().into(),
                binding_digest: binding.digest().into(),
            };
            let requirement = ExecutionEndpointRequirement::External {
                binding_id: binding.id().into(),
                stdout_max_bytes: 1024,
                stderr_max_bytes: 1024,
            };
            let backend = Arc::new(FaultBackend {
                capabilities: if terminal {
                    BTreeSet::from([
                        LifecycleCapability::SupervisorActivation,
                        LifecycleCapability::ExactTerminalObservation,
                    ])
                } else {
                    BTreeSet::new()
                },
                ..FaultBackend::new()
            });
            let registry =
                ExternalPlacementBackendRegistry::from_backends(vec![backend.clone()]).unwrap();
            assert_eq!(
                preflight_external_direct_dependencies(
                    &[binding],
                    &registry,
                    &requirement,
                    &identity,
                    30,
                    |binding| Ok(credential(binding)),
                    |_| Ok(()),
                )
                .is_ok(),
                terminal
            );
            assert_eq!(backend.qualification_calls.load(Ordering::SeqCst), 1);
            assert_eq!(backend.allocate_calls.load(Ordering::SeqCst), 0);
            assert_eq!(backend.allocation_observations.load(Ordering::SeqCst), 0);
            assert_eq!(backend.activation_calls.load(Ordering::SeqCst), 0);
            assert_eq!(backend.activation_observations.load(Ordering::SeqCst), 0);
            assert_eq!(backend.terminate_calls.load(Ordering::SeqCst), 0);
            assert_eq!(backend.termination_observations.load(Ordering::SeqCst), 0);
        }
    }

    #[test]
    fn direct_prebirth_runtime_proof_refusal_precedes_credential_and_backend() {
        use ryeos_engine::contracts::{
            ExecutionEndpointRequirement, ExternalEndpointBindingIdentity,
        };
        let binding = InstalledExternalExecutionBinding::direct_test_fixture(30);
        let identity = ExternalEndpointBindingIdentity {
            binding_id: binding.id().into(),
            binding_digest: binding.digest().into(),
        };
        let requirement = ExecutionEndpointRequirement::External {
            binding_id: binding.id().into(),
            stdout_max_bytes: 1024,
            stderr_max_bytes: 1024,
        };
        let backend = Arc::new(FaultBackend::new());
        let registry =
            ExternalPlacementBackendRegistry::from_backends(vec![backend.clone()]).unwrap();
        let credential_read = AtomicBool::new(false);
        assert!(
            preflight_external_direct_dependencies(
                &[binding],
                &registry,
                &requirement,
                &identity,
                30,
                |_| {
                    credential_read.store(true, Ordering::SeqCst);
                    bail!("credential must not be read")
                },
                |_| bail!("independent runtime proof is absent"),
            )
            .is_err()
        );
        assert!(!credential_read.load(Ordering::SeqCst));
        assert_eq!(backend.qualification_calls.load(Ordering::SeqCst), 0);
        assert_eq!(backend.allocate_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn direct_prebirth_preflight_refuses_identity_mode_and_limits_before_credentials() {
        use ryeos_engine::contracts::{
            ExecutionEndpointRequirement, ExternalEndpointBindingIdentity,
        };
        for refusal in [
            "local",
            "missing",
            "id",
            "digest",
            "session",
            "zero_timeout",
            "timeout",
            "stdout",
            "stderr",
        ] {
            let binding = if refusal == "session" {
                InstalledExternalExecutionBinding::test_fixture()
            } else {
                InstalledExternalExecutionBinding::direct_test_fixture(30)
            };
            let mut identity = ExternalEndpointBindingIdentity {
                binding_id: binding.id().into(),
                binding_digest: binding.digest().into(),
            };
            let mut requirement = ExecutionEndpointRequirement::External {
                binding_id: binding.id().into(),
                stdout_max_bytes: 1024,
                stderr_max_bytes: 1024,
            };
            let mut timeout = 30;
            match refusal {
                "local" => requirement = ExecutionEndpointRequirement::Local {},
                "id" => identity.binding_id = "another".into(),
                "digest" => identity.binding_digest = "0".repeat(64),
                "zero_timeout" => timeout = 0,
                "timeout" => timeout = 31,
                "stdout" => {
                    if let ExecutionEndpointRequirement::External {
                        stdout_max_bytes, ..
                    } = &mut requirement
                    {
                        *stdout_max_bytes = 0;
                    }
                }
                "stderr" => {
                    if let ExecutionEndpointRequirement::External {
                        stderr_max_bytes, ..
                    } = &mut requirement
                    {
                        *stderr_max_bytes = u64::MAX;
                    }
                }
                _ => {}
            }
            let bindings = if refusal == "missing" {
                vec![]
            } else {
                vec![binding]
            };
            let credential_read = AtomicBool::new(false);
            assert!(
                preflight_external_direct_dependencies(
                    &bindings,
                    &ExternalPlacementBackendRegistry::default(),
                    &requirement,
                    &identity,
                    timeout,
                    |_| {
                        credential_read.store(true, Ordering::SeqCst);
                        bail!("unexpected credential read")
                    },
                    |_| Ok(()),
                )
                .is_err(),
                "accepted {refusal}"
            );
            assert!(
                !credential_read.load(Ordering::SeqCst),
                "read credential for {refusal}"
            );
        }
    }

    #[test]
    fn activation_runtime_matches_retained_program_for_both_workloads() {
        // Component identity comparison only. These fixtures do not fabricate
        // the authoritative born capsule required by bootstrap authentication.
        for program in [
            AdmittedExternalExecutionProgram::StructuredSession(program()),
            AdmittedExternalExecutionProgram::DirectCommand(
                crate::thread_lifecycle::external_direct_program_test_fixture(),
            ),
        ] {
            require_external_activation_runtime(&program, program.runtime_manifest_hash().unwrap())
                .unwrap();
            let changed = if program.runtime_manifest_hash().unwrap() == "f".repeat(64) {
                "0".repeat(64)
            } else {
                "f".repeat(64)
            };
            assert!(require_external_activation_runtime(&program, &changed).is_err());
        }
    }

    #[test]
    fn direct_offline_qualification_requires_terminal_evidence_without_provider_contact() {
        let binding = InstalledExternalExecutionBinding::test_fixture();
        let credential = credential(&binding);
        let program = AdmittedExternalExecutionProgram::DirectCommand(
            crate::thread_lifecycle::external_direct_program_test_fixture(),
        );
        let mut contract = binding.backend_contract();
        contract.workload = crate::node_config::sections::external_execution::ExternalWorkloadBinding::DirectCommand {};
        contract.max_export_bytes = 0;
        for capabilities in [
            BTreeSet::new(),
            BTreeSet::from([LifecycleCapability::ExactTerminalObservation]),
            BTreeSet::from([
                LifecycleCapability::SupervisorActivation,
                LifecycleCapability::ExactTerminalObservation,
            ]),
        ] {
            let backend = Arc::new(FaultBackend {
                capabilities: capabilities.clone(),
                ..FaultBackend::new()
            });
            let registry =
                ExternalPlacementBackendRegistry::from_backends(vec![backend.clone()]).unwrap();
            assert_eq!(
                registry.qualify(&contract, &credential, &program).is_ok(),
                capabilities.contains(&LifecycleCapability::SupervisorActivation)
                    && capabilities.contains(&LifecycleCapability::ExactTerminalObservation)
            );
            assert_eq!(
                registry
                    .qualify_for_cleanup(&contract, &credential, &program)
                    .is_ok(),
                capabilities.contains(&LifecycleCapability::ExactTerminalObservation)
            );
            assert_eq!(backend.qualification_calls.load(Ordering::SeqCst), 2);
            assert_eq!(backend.allocate_calls.load(Ordering::SeqCst), 0);
            assert_eq!(backend.allocation_observations.load(Ordering::SeqCst), 0);
            assert_eq!(backend.activation_calls.load(Ordering::SeqCst), 0);
            assert_eq!(backend.activation_observations.load(Ordering::SeqCst), 0);
            assert_eq!(backend.terminate_calls.load(Ordering::SeqCst), 0);
            assert_eq!(backend.termination_observations.load(Ordering::SeqCst), 0);
        }
    }

    #[test]
    fn direct_offline_qualification_refuses_wrong_mode_export_and_budget_before_adapter() {
        let binding = InstalledExternalExecutionBinding::test_fixture();
        let credential = credential(&binding);
        let program = AdmittedExternalExecutionProgram::DirectCommand(
            crate::thread_lifecycle::external_direct_program_test_fixture(),
        );
        let backend = Arc::new(FaultBackend::new());
        let registry =
            ExternalPlacementBackendRegistry::from_backends(vec![backend.clone()]).unwrap();
        for refusal in ["session", "export", "budget"] {
            let mut contract = binding.backend_contract();
            if refusal != "session" {
                contract.workload = crate::node_config::sections::external_execution::ExternalWorkloadBinding::DirectCommand {};
                contract.max_export_bytes = 0;
            }
            match refusal {
                "export" => contract.max_export_bytes = 1,
                "budget" => contract.timeout_seconds = 1,
                _ => {}
            }
            assert!(registry.qualify(&contract, &credential, &program).is_err());
        }
        assert_eq!(backend.qualification_calls.load(Ordering::SeqCst), 0);
        assert_eq!(backend.allocate_calls.load(Ordering::SeqCst), 0);
        assert_eq!(backend.activation_calls.load(Ordering::SeqCst), 0);
        assert_eq!(backend.terminate_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn direct_binding_refuses_session_dependencies_before_backend_qualification() {
        let binding = InstalledExternalExecutionBinding::test_fixture();
        let credential = credential(&binding);
        let mut contract = binding.backend_contract();
        contract.workload = crate::node_config::sections::external_execution::ExternalWorkloadBinding::DirectCommand {};
        contract.max_export_bytes = 0;
        let connector = ExternalCandidateConnectorRegistry::default()
            .qualify(&contract)
            .err()
            .unwrap();
        let provider = ExternalProviderConfigurationRegistry::from_artifacts(Vec::new())
            .unwrap()
            .qualify(&contract)
            .err()
            .unwrap();
        let backend = ExternalPlacementBackendRegistry::from_backends(Vec::new())
            .unwrap()
            .qualify(
                &contract,
                &credential,
                &AdmittedExternalExecutionProgram::StructuredSession(program()),
            )
            .err()
            .unwrap();
        // Connector and provider qualification read the signed session
        // artifacts, which a direct-command binding has none of; the backend
        // refuses the structured-session program itself.
        for error in [connector, provider] {
            assert!(
                error
                    .to_string()
                    .contains("external direct command has no session artifact selection"),
                "{error:#}"
            );
        }
        assert!(
            backend
                .to_string()
                .contains("cannot authorize a structured session"),
            "{backend:#}"
        );
    }

    #[test]
    fn connector_registry_joins_exact_signed_artifact_and_live_path_binding() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("ryeos-external-candidate-connector");
        std::fs::write(&path, b"exact connector fixture").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let registry = ExternalCandidateConnectorRegistry::from_test_path(&path).unwrap();
        let artifact = registry.artifacts.values().next().unwrap();
        let mut contract = RetainedExternalExecutionBinding::test_fixture().backend_contract();
        let crate::node_config::sections::external_execution::ExternalWorkloadBinding::StructuredSession(session) = &mut contract.workload else {
            panic!("connector fixture requires a structured-session binding");
        };
        session.connector_artifact_hash = artifact.artifact_hash.clone();
        session.connector_artifact_bytes = artifact.artifact_bytes;
        assert!(
            ExternalCandidateConnectorRegistry::default()
                .qualify(&contract)
                .is_err()
        );
        let mut wrong_hash = contract.clone();
        let crate::node_config::sections::external_execution::ExternalWorkloadBinding::StructuredSession(session) = &mut wrong_hash.workload else {
            panic!("connector fixture requires a structured-session binding");
        };
        session.connector_artifact_hash = "0".repeat(64);
        assert!(registry.qualify(&wrong_hash).is_err());
        let mut wrong_size = contract.clone();
        let crate::node_config::sections::external_execution::ExternalWorkloadBinding::StructuredSession(session) = &mut wrong_size.workload else {
            panic!("connector fixture requires a structured-session binding");
        };
        session.connector_artifact_bytes += 1;
        assert!(registry.qualify(&wrong_size).is_err());
        let admitted = registry.qualify(&contract).unwrap();
        assert_eq!(admitted.executable_path().unwrap(), path);

        let retained = root.path().join("retained-old-connector");
        std::fs::rename(&path, retained).unwrap();
        std::fs::write(&path, b"replacement connector fixture").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(registry.qualify(&contract).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sealed_connector_peer_fixture() {
        use std::io::{Read as _, Write as _};
        let Some(endpoint) = std::env::var_os("RYEOS_TEST_SEALED_CONNECTOR_ENDPOINT") else {
            return;
        };
        let mut stream = lillux::LocalDuplexStream::connect(Path::new(&endpoint)).unwrap();
        let deadline =
            lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(10));
        let mut io = stream.with_deadline(deadline);
        io.write_all(b"ready").unwrap();
        let mut acknowledgement = [0; 1];
        io.read_exact(&mut acknowledgement).unwrap();
        assert_eq!(acknowledgement, [1]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn connector_peer_authenticates_sealed_bytes_and_rejects_wrong_digest() {
        use std::io::{Read as _, Write as _};
        let executable = std::env::current_exe().unwrap();
        let mut artifact = InstalledExternalCandidateConnector::open(&executable).unwrap();
        let bytes = std::fs::read(&executable).unwrap();
        let sealed = lillux::sealed_executable_memfd(c"ryeos-bundle-executable", &bytes).unwrap();
        let root = tempfile::tempdir().unwrap();
        let pinned = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
        pinned.tighten_owner_private_directory().unwrap();
        let listener = lillux::OwnerPrivateLocalDuplexListener::bind(&pinned, "peer").unwrap();
        let mut child = lillux::SubordinateProcess::spawn(lillux::SubordinateProcessRequest {
            cmd: sealed.path().to_string_lossy().into_owned(),
            argv0: None,
            args: vec![
                "--exact".into(),
                "external_placement::tests::sealed_connector_peer_fixture".into(),
            ],
            cwd: "/".into(),
            envs: vec![(
                "RYEOS_TEST_SEALED_CONNECTOR_ENDPOINT".into(),
                listener.endpoint().to_string_lossy().into_owned(),
            )],
            limits: None,
            inherited_fds: vec![sealed],
        })
        .unwrap();
        let deadline =
            lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(10));
        let mut stream = listener
            .accept_before(deadline)
            .unwrap()
            .expect("sealed peer connected");
        let mut ready = [0; 5];
        stream
            .with_deadline(deadline)
            .read_exact(&mut ready)
            .unwrap();
        assert_eq!(&ready, b"ready");
        let peer = stream.authenticated_peer().unwrap();
        assert!(
            peer.require_executable_name(artifact.executable.name())
                .is_err()
        );
        artifact.verify_peer(&peer).unwrap();
        artifact.artifact_hash = "0".repeat(64);
        assert!(artifact.verify_peer(&peer).is_err());
        stream.with_deadline(deadline).write_all(&[1]).unwrap();
        assert!(child.wait_exact_child().unwrap().success);
    }

    #[test]
    fn connector_registry_refuses_in_place_byte_mutation() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("ryeos-external-candidate-connector");
        std::fs::write(&path, b"exact connector fixture").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let registry = ExternalCandidateConnectorRegistry::from_test_path(&path).unwrap();
        let artifact = registry.artifacts.values().next().unwrap();
        let mut contract = RetainedExternalExecutionBinding::test_fixture().backend_contract();
        let crate::node_config::sections::external_execution::ExternalWorkloadBinding::StructuredSession(session) = &mut contract.workload else {
            panic!("connector fixture requires a structured-session binding");
        };
        session.connector_artifact_hash = artifact.artifact_hash.clone();
        session.connector_artifact_bytes = artifact.artifact_bytes;

        std::fs::write(&path, b"mutated connector bytes").unwrap();
        assert!(registry.qualify(&contract).is_err());
    }

    #[test]
    fn missing_connector_refuses_before_credential_or_backend_qualification() {
        let credential_read = AtomicBool::new(false);
        let bindings = [InstalledExternalExecutionBinding::test_fixture()];
        let error = preflight_external_candidate_dependencies(
            &bindings,
            &ExternalCandidateConnectorRegistry::default(),
            &ExternalProviderConfigurationRegistry::default(),
            &ExternalPlacementBackendRegistry::default(),
            &ryeos_engine::isolation::IsolationRuntime::disabled_for_authoring(),
            &program(),
            |_| -> Result<PlacementCredential> {
                credential_read.store(true, Ordering::SeqCst);
                bail!("credential access must remain unreachable")
            },
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("exact signed external candidate connector is not installed")
        );
        assert!(!credential_read.load(Ordering::SeqCst));
    }

    #[test]
    fn connector_process_group_requires_a_qualified_controller_session() {
        use ryeos_external_execution_contract::ExternalProviderConnectorProcessGroup::{
            Inherited, New,
        };

        assert!(require_connector_process_group_authority(Inherited, true, false).is_ok());
        assert!(require_connector_process_group_authority(New, false, true).is_ok());
        assert!(require_connector_process_group_authority(New, false, false).is_err());
        assert!(require_connector_process_group_authority(New, true, true).is_err());
    }

    #[test]
    fn expired_bootstrap_retry_requires_the_exact_registered_channel() {
        let dir = tempfile::tempdir().unwrap();
        let (_, reservation, _) = lifecycle_fixture(&dir.path().join("runtime.sqlite3"));
        let occurrence = ExternalAllocationOccurrence {
            schema: 1,
            binding_hash: reservation.binding_hash.clone(),
            request_digest: reservation.request_digest.clone(),
            occurrence_id: "external-one".into(),
            provider_observation_digest: "f".repeat(64),
        };
        let supervisor = lillux::crypto::SigningKey::from_bytes(&[63; 32]);
        let now = lillux::time::timestamp_millis();
        let binding = ryeos_state::external_execution::ExecutionChannelBinding {
            schema: ryeos_state::external_execution::EXECUTION_CHANNEL_BINDING_SCHEMA,
            execution_mode:
                ryeos_external_execution_contract::ExternalExecutionMode::StructuredSession {},
            placement_thread_id: reservation.placement_thread_id.clone(),
            allocation_request_digest: reservation.request_digest.clone(),
            occurrence_id: occurrence.occurrence_id.clone(),
            admitted_capsule_hash: reservation.admitted_capsule_hash.clone(),
            base_snapshot_hash: reservation.base_snapshot_hash.clone(),
            execution_binding_hash: reservation.binding_hash.clone(),
            supervisor_runtime_hash: "9".repeat(64),
            candidate_program_digest: "0".repeat(64),
            channel_nonce: "8".repeat(64),
            owner_public_key: reservation.channel_owner_public_key.clone(),
            supervisor_public_key: ryeos_state::external_execution::encode_channel_public_key(
                &supervisor.verifying_key(),
            )
            .unwrap(),
            issued_at_ms: now,
            execution_deadline_ms: now + 60_000,
            expires_at_ms: now + 120_000,
            candidate_export_max_bytes: 512,
            max_frames: 64,
            max_bytes: 1024,
        };
        assert!(
            existing_external_channel_matches(Some(&binding), &reservation, &occurrence).unwrap()
        );
        assert!(!existing_external_channel_matches(None, &reservation, &occurrence).unwrap());
        let mut substituted = binding;
        substituted.owner_public_key = ryeos_state::external_execution::encode_channel_public_key(
            &lillux::crypto::SigningKey::from_bytes(&[64; 32]).verifying_key(),
        )
        .unwrap();
        assert!(
            existing_external_channel_matches(Some(&substituted), &reservation, &occurrence)
                .is_err()
        );
    }

    #[test]
    fn production_channel_constructor_uses_the_channel_wire_identity() {
        let dir = tempfile::tempdir().unwrap();
        let (_, reservation, retained) = lifecycle_fixture(&dir.path().join("runtime.sqlite3"));
        let occurrence = ExternalAllocationOccurrence {
            schema: 1,
            binding_hash: reservation.binding_hash.clone(),
            request_digest: reservation.request_digest.clone(),
            occurrence_id: "external-channel-constructor".into(),
            provider_observation_digest: "f".repeat(64),
        };
        let supervisor = lillux::crypto::SigningKey::from_bytes(&[63; 32]);
        let program = AdmittedExternalExecutionProgram::StructuredSession(program());
        let channel = build_external_execution_channel_binding(
            &reservation,
            &occurrence,
            &program,
            &retained.backend_contract(),
            &ryeos_state::external_execution::encode_channel_public_key(
                &supervisor.verifying_key(),
            )
            .unwrap(),
            lillux::time::timestamp_millis(),
            "8".repeat(64),
        )
        .unwrap();
        assert_eq!(
            channel.schema,
            ryeos_state::external_execution::EXECUTION_CHANNEL_BINDING_SCHEMA
        );
        assert_eq!(channel.candidate_program_digest, program.digest().unwrap());
        channel.validate().unwrap();

        let mut changed_occurrence = occurrence;
        changed_occurrence.request_digest = "0".repeat(64);
        assert!(
            build_external_execution_channel_binding(
                &reservation,
                &changed_occurrence,
                &program,
                &retained.backend_contract(),
                &channel.supervisor_public_key,
                channel.issued_at_ms,
                "8".repeat(64),
            )
            .is_err()
        );
    }

    fn direct_helper_owner(
        placement: &str,
        program: ryeos_state::external_execution::admission::AdmittedExternalDirectProgram,
    ) -> ExternalAllocationOwner {
        ExternalAllocationOwner::DirectThread {
            chain_root_id: placement.to_owned(),
            launch_owner: crate::runtime_db::LaunchOwner {
                thread_id: placement.to_owned(),
                monotonic_launch_epoch: 1,
                unpredictable_nonce: "direct-helper-nonce".into(),
                daemon_generation_id: "direct-helper-daemon".into(),
            },
            program,
        }
    }

    #[test]
    fn direct_placement_helpers_preserve_exact_mode_digest_budget_and_no_export() {
        // Mechanical helper coverage, not signed endpoint or born-thread
        // admission: production independently rejoins the retained binding.
        let dir = tempfile::tempdir().unwrap();
        let (_, mut reservation, retained) = lifecycle_fixture(&dir.path().join("runtime.sqlite3"));
        let direct = crate::thread_lifecycle::external_direct_program_test_fixture();
        let session_owner = reservation.owner.clone();
        reservation.owner = direct_helper_owner(&reservation.placement_thread_id, direct.clone());
        reservation.binding_hash = direct.projection().endpoint_binding_digest.clone();
        reservation.timeout_seconds = u32::try_from(direct.projection().timeout_seconds).unwrap();
        let program = AdmittedExternalExecutionProgram::DirectCommand(direct);
        let mut contract = retained.backend_contract();
        contract.workload = crate::node_config::sections::external_execution::ExternalWorkloadBinding::DirectCommand {};
        contract.max_export_bytes = 0;
        contract.timeout_seconds = reservation.timeout_seconds;
        validate_external_placement_program(&program, &reservation, &contract).unwrap();
        let occurrence = ExternalAllocationOccurrence {
            schema: 1,
            binding_hash: reservation.binding_hash.clone(),
            request_digest: reservation.request_digest.clone(),
            occurrence_id: "direct-channel-helper".into(),
            provider_observation_digest: "f".repeat(64),
        };
        let supervisor = lillux::crypto::SigningKey::from_bytes(&[63; 32]);
        let channel = build_external_execution_channel_binding(
            &reservation,
            &occurrence,
            &program,
            &contract,
            &ryeos_state::external_execution::encode_channel_public_key(
                &supervisor.verifying_key(),
            )
            .unwrap(),
            lillux::time::timestamp_millis(),
            "8".repeat(64),
        )
        .unwrap();
        assert_eq!(channel.execution_mode, program.execution_mode());
        assert_eq!(channel.candidate_export_max_bytes, 0);
        assert_eq!(channel.candidate_program_digest, program.digest().unwrap());
        assert_eq!(
            channel.supervisor_runtime_hash,
            program.runtime_manifest_hash().unwrap()
        );
        channel.validate().unwrap();
        for field in [
            "binding",
            "reserved_timeout",
            "signed_timeout",
            "export",
            "session",
            "owner",
            "owner_program",
        ] {
            let mut changed_reservation = reservation.clone();
            let mut changed_contract = contract.clone();
            match field {
                "binding" => changed_reservation.binding_hash = "0".repeat(64),
                "reserved_timeout" => changed_reservation.timeout_seconds += 1,
                "signed_timeout" => changed_contract.timeout_seconds -= 1,
                "export" => changed_contract.max_export_bytes = 1,
                "session" => changed_contract.workload = retained.backend_contract().workload,
                "owner" => changed_reservation.owner = session_owner.clone(),
                "owner_program" => {
                    let ExternalAllocationOwner::DirectThread { program, .. } =
                        &mut changed_reservation.owner
                    else {
                        unreachable!()
                    };
                    let mut value = serde_json::to_value(&*program).unwrap();
                    value["guest_input_identity"] = serde_json::json!("0".repeat(64));
                    *program = serde_json::from_value(value).unwrap();
                    program.validate().unwrap();
                }
                _ => unreachable!(),
            }
            assert!(
                validate_external_placement_program(
                    &program,
                    &changed_reservation,
                    &changed_contract
                )
                .is_err(),
                "direct helper accepted changed {field}"
            );
        }
    }

    #[test]
    fn generic_placement_program_preserves_session_qualification_and_mode_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let (_, reservation, retained) = lifecycle_fixture(&dir.path().join("runtime.sqlite3"));
        let program = AdmittedExternalExecutionProgram::StructuredSession(program());
        let contract = retained.backend_contract();
        validate_external_placement_program(&program, &reservation, &contract).unwrap();
        let mut wrong_owner = reservation.clone();
        wrong_owner.owner = direct_helper_owner(
            &reservation.placement_thread_id,
            crate::thread_lifecycle::external_direct_program_test_fixture(),
        );
        assert!(validate_external_placement_program(&program, &wrong_owner, &contract).is_err());
        for field in ["runtime", "selection", "mode"] {
            let mut changed = contract.clone();
            match (&mut changed.workload, field) {
                (crate::node_config::sections::external_execution::ExternalWorkloadBinding::StructuredSession(session), "runtime") => {
                    session.runtime_manifest_hash = "0".repeat(64);
                }
                (crate::node_config::sections::external_execution::ExternalWorkloadBinding::StructuredSession(session), "selection") => {
                    session.runtime_selection_identity = "0".repeat(64);
                }
                (_, "mode") => {
                    changed.workload = crate::node_config::sections::external_execution::ExternalWorkloadBinding::DirectCommand {};
                    changed.max_export_bytes = 0;
                }
                _ => unreachable!(),
            }
            assert!(
                validate_external_placement_program(&program, &reservation, &changed).is_err(),
                "generic placement accepted changed session {field}"
            );
        }
    }

    #[test]
    fn controller_signed_import_joins_exact_activation_and_guest_runtime() {
        use ryeos_external_execution_contract::guest_import_authorization::{
            GUEST_OCCURRENCE_ASSIGNMENT_SCHEMA, GuestOccurrenceAssignment,
            GuestOccurrenceAssignmentDocument,
        };
        use ryeos_external_execution_contract::staging_package::{
            GUEST_IMPORT_TICKET_SCHEMA, GuestImportTicket,
        };

        let dir = tempfile::tempdir().unwrap();
        let (db, reservation, binding) = lifecycle_fixture(&dir.path().join("runtime.sqlite3"));
        let contract = binding.backend_contract();
        let occurrence = ExternalAllocationOccurrence {
            schema: 1,
            binding_hash: reservation.binding_hash.clone(),
            request_digest: reservation.request_digest.clone(),
            occurrence_id: "occ-signed-import".into(),
            provider_observation_digest: "f".repeat(64),
        };
        let authority =
            ExternalChannelAuthority::test_fixture(&reservation.channel_authority_generation);
        let guest_inputs = guest_input_authority(
            &dir.path().join("signed-import-inputs"),
            &reservation.base_snapshot_hash,
        );
        let package_parent = lillux::PinnedDirectory::open(dir.path()).unwrap().unwrap();
        let (intent, mut activation) = supervisor_activation(
            &contract,
            &reservation,
            &occurrence,
            &authority,
            &AdmittedExternalExecutionProgram::StructuredSession(program()),
            guest_inputs,
            &FaultBackend::new(),
            None,
            &package_parent,
            reservation.startup_deadline().unwrap(),
            crate::external_supervisor_native::test_supervisor_journal_envelope(),
        )
        .unwrap();
        let ticket = GuestImportTicket {
            schema: GUEST_IMPORT_TICKET_SCHEMA,
            binding_hash: reservation.binding_hash.clone(),
            allocation_request_digest: reservation.request_digest.clone(),
            occurrence_id: occurrence.occurrence_id.clone(),
            activation_request_digest: intent.activation_request_digest.clone(),
            guest_input_identity: intent.guest_input_identity.clone(),
            payload_sha256: intent.delivery.payload_sha256.clone(),
            manifest_sha256: intent.delivery.manifest_sha256.clone(),
            framed_bytes: intent.delivery.framed_bytes,
            regular_bytes: intent.delivery.regular_bytes,
            bootstrap_sha256: lillux::sha256_hex(
                &activation.bootstrap().canonical_bytes().unwrap(),
            ),
            supervisor_sha256: contract.supervisor_artifact_hash.clone(),
            launcher_sha256: contract.launcher_artifact_hash.clone(),
            maximum_regular_bytes: contract.max_guest_package_regular_bytes,
            maximum_framed_bytes: contract.max_guest_package_framed_bytes,
        };
        let signed = sign_guest_import_ticket(
            &contract,
            &reservation,
            &occurrence,
            &intent,
            activation.guest_inputs().projection(),
            ticket,
            &authority,
        )
        .unwrap();
        let assignment = GuestOccurrenceAssignment {
            placement_thread_id: &reservation.placement_thread_id,
            admitted_capsule_hash: &reservation.admitted_capsule_hash,
            base_snapshot_hash: &reservation.base_snapshot_hash,
            execution_binding_hash: &reservation.binding_hash,
            allocation_request_digest: &reservation.request_digest,
            occurrence_id: &occurrence.occurrence_id,
            activation_request_digest: &intent.activation_request_digest,
            supervisor_runtime_hash: &intent.supervisor_runtime_hash,
            guest_runtime_manifest_hash: &contract.guest_runtime_manifest_hash,
            attachment_deadline_ms: intent.attachment_deadline_ms,
        };
        let root = lillux::crypto::SigningKey::from_bytes(&[43; 32]);
        let signed_assignment =
            ryeos_external_execution::guest_import_authorization::sign_guest_occurrence_assignment(
                GuestOccurrenceAssignmentDocument {
                    schema: GUEST_OCCURRENCE_ASSIGNMENT_SCHEMA,
                    placement_thread_id: assignment.placement_thread_id.into(),
                    admitted_capsule_hash: assignment.admitted_capsule_hash.into(),
                    base_snapshot_hash: assignment.base_snapshot_hash.into(),
                    execution_binding_hash: assignment.execution_binding_hash.into(),
                    allocation_request_digest: assignment.allocation_request_digest.into(),
                    occurrence_id: assignment.occurrence_id.into(),
                    activation_request_digest: assignment.activation_request_digest.into(),
                    supervisor_runtime_hash: assignment.supervisor_runtime_hash.into(),
                    guest_runtime_manifest_hash: assignment.guest_runtime_manifest_hash.into(),
                    owner_public_key_hex: hex::encode(
                        authority.owner_signing_key().verifying_key().to_bytes(),
                    ),
                    attachment_deadline_ms: assignment.attachment_deadline_ms,
                },
                &root,
            )
            .unwrap();
        let import_bytes = ryeos_external_execution_contract::canonical_json(&signed).unwrap();
        let assignment_bytes =
            ryeos_external_execution_contract::canonical_json(&signed_assignment).unwrap();
        ryeos_external_execution::guest_import_authorization::verify_guest_import_documents(
            &import_bytes,
            &root.verifying_key(),
            assignment.guest_runtime_manifest_hash,
            &assignment_bytes,
        )
        .unwrap();
        // The same activation's lease record and the guest's verification of
        // the delivered import name the same three inputs in the same order.
        activation.import_authorization = Some(signed.clone());
        let delivered = ryeos_external_execution_contract::canonical_json(
            activation.signed_import_authorization().unwrap(),
        )
        .unwrap();
        let verified_delivery =
            ryeos_external_execution::guest_import_authorization::verify_guest_import_documents(
                &delivered,
                &root.verifying_key(),
                assignment.guest_runtime_manifest_hash,
                &assignment_bytes,
            )
            .unwrap();
        let record =
            external_lease_activation(&reservation, &occurrence, &intent, &activation).unwrap();
        let controller_inputs =
            ryeos_external_execution_contract::lease_ready_inputs::ordered_lease_ready_inputs(
                &record.activation_request_digest,
                &record.signed_import_sha256,
                &record.guest_package_sha256,
            );
        let guest_inputs = verified_delivery.ready_boot_inputs(&delivered);
        assert_eq!(
            guest_inputs.iter().map(String::as_str).collect::<Vec<_>>(),
            controller_inputs
        );
        assert_eq!(guest_inputs[0], intent.activation_request_digest);
        assert_eq!(guest_inputs[2], intent.delivery.payload_sha256);
        assert_eq!(
            guest_inputs[1],
            lillux::sha256_hex(&delivered),
            "the controller digests the very bytes the guest receives"
        );
        let mut reordered = guest_inputs.clone();
        reordered.swap(0, 1);
        assert_ne!(reordered, guest_inputs);
        assert_ne!(
            guest_inputs.iter().map(String::as_str).collect::<Vec<_>>(),
            [
                controller_inputs[1],
                controller_inputs[0],
                controller_inputs[2]
            ]
        );
        assert_ne!(
            signed.authorization.guest_runtime_manifest_hash,
            signed.authorization.supervisor_runtime_hash
        );
        assert!(
            ryeos_external_execution::guest_import_authorization::verify_guest_import_documents(
                &import_bytes,
                &root.verifying_key(),
                &"7".repeat(64),
                &assignment_bytes,
            )
            .is_err()
        );
        drop(db);
    }

    #[test]
    fn ambiguous_provider_mutations_reconcile_without_duplicate_contact() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime.sqlite3");
        let (db, reservation, binding) = lifecycle_fixture(&path);
        let backend = FaultBackend::new();
        let contract = binding.backend_contract();
        let credential = credential(&InstalledExternalExecutionBinding::test_fixture());
        let authority =
            ExternalChannelAuthority::test_fixture(&reservation.channel_authority_generation);
        assert!(matches!(
            db.claim_external_allocation_contact("T-one", &reservation.request_digest)
                .unwrap(),
            ExternalAllocationContactClaim::Contact(_)
        ));
        assert!(
            backend
                .allocate(
                    &contract,
                    &credential,
                    &reservation,
                    reservation.startup_deadline().unwrap()
                )
                .is_err()
        );
        assert_eq!(backend.allocate_calls.load(Ordering::SeqCst), 1);
        drop(db);

        let db = crate::runtime_db::test_native::TestRuntime::open(&path);
        assert!(matches!(
            db.claim_external_allocation_contact("T-one", &reservation.request_digest)
                .unwrap(),
            ExternalAllocationContactClaim::Reconcile(_)
        ));
        let ExternalAllocationResolution::Bound {
            occurrence_id,
            provider_observation_digest,
        } = backend
            .reconcile_allocation(
                &contract,
                &credential,
                &reservation,
                reservation.startup_deadline().unwrap(),
            )
            .unwrap()
            .value
        else {
            panic!("fixture reconciliation did not identify its occurrence");
        };
        let occurrence = ExternalAllocationOccurrence {
            schema: 1,
            binding_hash: reservation.binding_hash.clone(),
            request_digest: reservation.request_digest.clone(),
            occurrence_id,
            provider_observation_digest,
        };
        db.bind_external_allocation("T-one", &occurrence, test_observation_timing())
            .unwrap();
        assert_eq!(backend.allocate_calls.load(Ordering::SeqCst), 1);
        assert_eq!(backend.allocation_observations.load(Ordering::SeqCst), 1);

        let guest_inputs = guest_input_authority(
            &dir.path().join("guest-inputs"),
            &reservation.base_snapshot_hash,
        );
        let package_parent = lillux::PinnedDirectory::open(dir.path()).unwrap().unwrap();
        let (activation_intent, activation) = supervisor_activation(
            &contract,
            &reservation,
            &occurrence,
            &authority,
            &AdmittedExternalExecutionProgram::StructuredSession(program()),
            guest_inputs,
            &backend,
            None,
            &package_parent,
            reservation.startup_deadline().unwrap(),
            crate::external_supervisor_native::test_supervisor_journal_envelope(),
        )
        .unwrap();
        assert!(
            db.begin_external_supervisor_activation("T-one", &activation_intent)
                .unwrap()
        );
        assert!(
            backend
                .activate_supervisor(
                    &contract,
                    &credential,
                    &reservation,
                    &occurrence,
                    &activation_intent,
                    &activation,
                    reservation.startup_deadline().unwrap(),
                )
                .is_err()
        );
        assert_eq!(backend.activation_calls.load(Ordering::SeqCst), 1);
        drop(db);

        let db = crate::runtime_db::test_native::TestRuntime::open(&path);
        assert!(
            !db.begin_external_supervisor_activation("T-one", &activation_intent)
                .unwrap()
        );
        let ExternalSupervisorActivationResolution::Started {
            provider_observation_digest,
        } = backend
            .reconcile_supervisor_activation(
                &contract,
                &credential,
                &reservation,
                &occurrence,
                &activation_intent,
                reservation.startup_deadline().unwrap(),
            )
            .unwrap()
            .value
        else {
            panic!("fixture activation reconciliation did not prove supervisor start");
        };
        db.settle_external_supervisor_activation(
            "T-one",
            &ExternalSupervisorActivationObservation {
                schema: 1,
                binding_hash: reservation.binding_hash.clone(),
                request_digest: reservation.request_digest.clone(),
                occurrence_id: occurrence.occurrence_id.clone(),
                activation_request_digest: activation_intent.activation_request_digest.clone(),
                activation_state: "started".into(),
                provider_observation_digest,
            },
            test_observation_timing(),
        )
        .unwrap();
        assert_eq!(backend.activation_calls.load(Ordering::SeqCst), 1);
        assert_eq!(backend.activation_observations.load(Ordering::SeqCst), 1);

        let intent = ExternalTerminationIntent {
            schema: 1,
            binding_hash: reservation.binding_hash.clone(),
            request_digest: reservation.request_digest.clone(),
            occurrence_id: occurrence.occurrence_id.clone(),
            termination_request_digest: "1".repeat(64),
        };
        assert!(db.begin_external_termination("T-one", &intent).unwrap());
        assert!(
            backend
                .terminate(&contract, &credential, &reservation, &occurrence, &intent)
                .is_err()
        );
        assert_eq!(backend.terminate_calls.load(Ordering::SeqCst), 1);
        drop(db);

        let db = crate::runtime_db::test_native::TestRuntime::open(&path);
        assert!(!db.begin_external_termination("T-one", &intent).unwrap());
        let ExternalTerminationResolution::Terminal {
            provider_observation_digest,
        } = backend
            .reconcile_termination(&contract, &credential, &reservation, &occurrence, &intent)
            .unwrap()
        else {
            panic!("fixture cleanup reconciliation did not prove termination");
        };
        db.settle_external_terminal(
            "T-one",
            &ExternalTerminalObservation {
                schema: 1,
                binding_hash: reservation.binding_hash.clone(),
                request_digest: reservation.request_digest.clone(),
                occurrence_id: occurrence.occurrence_id,
                termination_request_digest: intent.termination_request_digest,
                terminal_state: "terminated".into(),
                provider_observation_digest,
            },
        )
        .unwrap();
        assert_eq!(backend.terminate_calls.load(Ordering::SeqCst), 1);
        assert_eq!(backend.termination_observations.load(Ordering::SeqCst), 1);
        assert_eq!(
            db.external_allocation("T-one").unwrap().unwrap().phase,
            ExternalAllocationPhase::Terminated
        );
    }

    #[test]
    fn composed_permit_and_reconciliation_preserve_one_mutation_each() {
        let (_store_owner, store, reservation, binding, controller_lifetime, _) =
            placement_store_fixture();
        let backend = Arc::new(FaultBackend::new());
        let gate = Arc::new(AtomicBool::new(false));
        let guest_root = tempfile::tempdir().unwrap();
        let decision = prepared_fixture(
            store.clone(),
            backend.clone(),
            &reservation,
            &binding,
            gate.clone(),
            controller_lifetime.clone(),
        )
        .claim()
        .unwrap();
        let ExternalPlacementContactDecision::Contact(permit) = decision else {
            panic!("fresh reservation did not return its unique contact permit");
        };
        assert!(gate.load(Ordering::Acquire));
        assert!(permit.contact(test_startup_deadline()).is_err());
        assert!(!gate.load(Ordering::Acquire));

        let decision = prepared_fixture(
            store.clone(),
            backend.clone(),
            &reservation,
            &binding,
            gate.clone(),
            controller_lifetime.clone(),
        )
        .claim()
        .unwrap();
        let ExternalPlacementContactDecision::Reconcile(recovery) = decision else {
            panic!("ambiguous create did not return reconciliation authority");
        };
        assert_eq!(
            recovery
                .reconcile_with_deadline(test_startup_deadline(), test_observation_timing())
                .unwrap()
                .value
                .phase,
            ExternalAllocationPhase::Bound
        );

        let decision = prepared_fixture_with_guest_inputs(
            store.clone(),
            backend.clone(),
            &reservation,
            &binding,
            gate.clone(),
            controller_lifetime.clone(),
            guest_root.path(),
        )
        .claim()
        .unwrap();
        let ExternalPlacementContactDecision::Reconcile(activation) = decision else {
            panic!("bound occurrence did not retain activation authority");
        };
        assert!(activation.activate_or_reconcile().is_err());
        assert_eq!(backend.activation_calls.load(Ordering::SeqCst), 1);

        let decision = prepared_fixture(
            store.clone(),
            backend.clone(),
            &reservation,
            &binding,
            gate.clone(),
            controller_lifetime.clone(),
        )
        .claim()
        .unwrap();
        let ExternalPlacementContactDecision::Reconcile(activation) = decision else {
            panic!("ambiguous activation did not retain reconciliation authority");
        };
        let activated = activation.activate_or_reconcile().unwrap();
        assert_eq!(
            activated
                .observation
                .as_ref()
                .map(|value| value.activation_state.as_str()),
            Some("started")
        );

        let decision = prepared_fixture(
            store.clone(),
            backend.clone(),
            &reservation,
            &binding,
            gate.clone(),
            controller_lifetime.clone(),
        )
        .claim()
        .unwrap();
        let ExternalPlacementContactDecision::Reconcile(cleanup) = decision else {
            panic!("activated occurrence did not retain cleanup authority");
        };
        assert!(cleanup.terminate_or_reconcile().is_err());
        assert_eq!(
            store.external_allocation("T-one").unwrap().unwrap().phase,
            ExternalAllocationPhase::Quarantined
        );

        let decision = prepared_fixture(
            store.clone(),
            backend.clone(),
            &reservation,
            &binding,
            gate,
            controller_lifetime,
        )
        .claim()
        .unwrap();
        let ExternalPlacementContactDecision::Reconcile(cleanup) = decision else {
            panic!("ambiguous cleanup did not retain reconciliation authority");
        };
        assert_eq!(
            cleanup.terminate_or_reconcile().unwrap().phase,
            ExternalAllocationPhase::Terminated
        );
        assert_eq!(backend.allocate_calls.load(Ordering::SeqCst), 1);
        assert_eq!(backend.allocation_observations.load(Ordering::SeqCst), 1);
        assert_eq!(backend.activation_calls.load(Ordering::SeqCst), 1);
        assert_eq!(backend.activation_observations.load(Ordering::SeqCst), 1);
        assert_eq!(backend.terminate_calls.load(Ordering::SeqCst), 1);
        assert_eq!(backend.termination_observations.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn unresolved_activation_can_terminate_its_exact_occurrence_without_retry() {
        let (_store_owner, store, reservation, binding, controller_lifetime, _) =
            placement_store_fixture();
        let backend = Arc::new(FaultBackend::with_reconciled_activation("pending"));
        let gate = Arc::new(AtomicBool::new(false));
        let guest_root = tempfile::tempdir().unwrap();

        let ExternalPlacementContactDecision::Contact(contact) = prepared_fixture(
            store.clone(),
            backend.clone(),
            &reservation,
            &binding,
            gate.clone(),
            controller_lifetime.clone(),
        )
        .claim()
        .unwrap() else {
            panic!("fresh reservation lost its contact permit");
        };
        assert!(contact.contact(test_startup_deadline()).is_err());
        let ExternalPlacementContactDecision::Reconcile(allocation) = prepared_fixture(
            store.clone(),
            backend.clone(),
            &reservation,
            &binding,
            gate.clone(),
            controller_lifetime.clone(),
        )
        .claim()
        .unwrap() else {
            panic!("ambiguous allocation lost reconciliation authority");
        };
        assert_eq!(
            allocation
                .reconcile_with_deadline(test_startup_deadline(), test_observation_timing())
                .unwrap()
                .value
                .phase,
            ExternalAllocationPhase::Bound,
        );

        let ExternalPlacementContactDecision::Reconcile(activation) =
            prepared_fixture_with_guest_inputs(
                store.clone(),
                backend.clone(),
                &reservation,
                &binding,
                gate.clone(),
                controller_lifetime.clone(),
                guest_root.path(),
            )
            .claim()
            .unwrap()
        else {
            panic!("bound occurrence lost activation authority");
        };
        assert!(activation.activate_or_reconcile().is_err());
        let ExternalPlacementContactDecision::Reconcile(reconcile) = prepared_fixture(
            store.clone(),
            backend.clone(),
            &reservation,
            &binding,
            gate.clone(),
            controller_lifetime.clone(),
        )
        .claim()
        .unwrap() else {
            panic!("ambiguous activation lost reconciliation authority");
        };
        assert!(
            reconcile
                .activate_or_reconcile()
                .unwrap()
                .observation
                .is_none()
        );
        assert_eq!(backend.activation_calls.load(Ordering::SeqCst), 1);
        assert_eq!(backend.activation_observations.load(Ordering::SeqCst), 1);

        let app_root = tempfile::tempdir().unwrap();
        let state = crate::state::test_support::build_with_store(
            app_root.path(),
            store.clone(),
            controller_lifetime.clone(),
        )
        .unwrap();
        request_external_candidate_cleanup(&state, &reservation.placement_thread_id).unwrap();
        assert_eq!(
            store
                .external_allocation(&reservation.placement_thread_id)
                .unwrap()
                .unwrap()
                .phase,
            ExternalAllocationPhase::Quarantined,
        );

        let ExternalPlacementContactDecision::Reconcile(cleanup) = prepared_fixture(
            store.clone(),
            backend.clone(),
            &reservation,
            &binding,
            gate.clone(),
            controller_lifetime.clone(),
        )
        .claim()
        .unwrap() else {
            panic!("unresolved activation lost exact cleanup authority");
        };
        assert!(cleanup.terminate_or_reconcile().is_err());
        let ExternalPlacementContactDecision::Reconcile(reconcile) = prepared_fixture(
            store.clone(),
            backend.clone(),
            &reservation,
            &binding,
            gate,
            controller_lifetime,
        )
        .claim()
        .unwrap() else {
            panic!("uncertain termination lost exact reconciliation authority");
        };
        assert_eq!(
            reconcile.terminate_or_reconcile().unwrap().phase,
            ExternalAllocationPhase::Terminated,
        );
        assert_eq!(backend.activation_calls.load(Ordering::SeqCst), 1);
        assert_eq!(backend.terminate_calls.load(Ordering::SeqCst), 1);
        assert_eq!(backend.termination_observations.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_staged_activation_without_an_upload_source_is_a_plain_refusal_before_any_claim() {
        let (_owner, store, reservation, binding, lifetime, _lock_path) = placement_store_fixture();
        let backend = Arc::new(FaultBackend {
            stages_runtime: true,
            ..FaultBackend::new()
        });
        let gate = Arc::new(AtomicBool::new(false));
        let guest_root = tempfile::tempdir().unwrap();
        let ExternalPlacementContactDecision::Contact(contact) = prepared_fixture(
            store.clone(),
            backend.clone(),
            &reservation,
            &binding,
            gate.clone(),
            lifetime.clone(),
        )
        .claim()
        .unwrap() else {
            panic!("fresh reservation lost its contact permit");
        };
        assert!(contact.contact(test_startup_deadline()).is_err());
        let ExternalPlacementContactDecision::Reconcile(allocation) = prepared_fixture(
            store.clone(),
            backend.clone(),
            &reservation,
            &binding,
            gate.clone(),
            lifetime.clone(),
        )
        .claim()
        .unwrap() else {
            panic!("ambiguous allocation lost reconciliation authority");
        };
        allocation
            .reconcile_with_deadline(test_startup_deadline(), test_observation_timing())
            .unwrap();
        let ExternalPlacementContactDecision::Reconcile(activation) =
            prepared_fixture_with_guest_inputs(
                store.clone(),
                backend.clone(),
                &reservation,
                &binding,
                gate,
                lifetime,
                guest_root.path(),
            )
            .claim()
            .unwrap()
        else {
            panic!("bound occurrence lost activation authority");
        };
        let refusal = format!("{:#}", activation.activate_or_reconcile().unwrap_err());
        assert!(
            refusal.contains("no upload source is installed"),
            "{refusal}"
        );
        assert_eq!(backend.prepare_calls.load(Ordering::SeqCst), 0);
        assert_eq!(backend.activation_calls.load(Ordering::SeqCst), 0);
        assert!(
            store
                .external_supervisor_activation(&reservation.placement_thread_id)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn replacement_controller_fences_uncertain_activation_and_reconciles_exact_occurrence() {
        let (predecessor_owner, predecessor, reservation, binding, predecessor_lifetime, lock_path) =
            placement_store_fixture();
        let backend = Arc::new(FaultBackend::with_reconciled_activation("pending"));
        let gate = Arc::new(AtomicBool::new(false));
        let guest_root = tempfile::tempdir().unwrap();

        let ExternalPlacementContactDecision::Contact(contact) = prepared_fixture(
            predecessor.clone(),
            backend.clone(),
            &reservation,
            &binding,
            gate.clone(),
            predecessor_lifetime.clone(),
        )
        .claim()
        .unwrap() else {
            panic!("fresh reservation lost its contact permit");
        };
        assert!(contact.contact(test_startup_deadline()).is_err());
        let ExternalPlacementContactDecision::Reconcile(allocation) = prepared_fixture(
            predecessor.clone(),
            backend.clone(),
            &reservation,
            &binding,
            gate.clone(),
            predecessor_lifetime.clone(),
        )
        .claim()
        .unwrap() else {
            panic!("ambiguous allocation lost reconciliation authority");
        };
        assert_eq!(
            allocation
                .reconcile_with_deadline(test_startup_deadline(), test_observation_timing())
                .unwrap()
                .value
                .phase,
            ExternalAllocationPhase::Bound,
        );
        let ExternalPlacementContactDecision::Reconcile(activation) =
            prepared_fixture_with_guest_inputs(
                predecessor.clone(),
                backend.clone(),
                &reservation,
                &binding,
                gate.clone(),
                predecessor_lifetime.clone(),
                guest_root.path(),
            )
            .claim()
            .unwrap()
        else {
            panic!("bound occurrence lost activation authority");
        };
        assert!(activation.activate_or_reconcile().is_err());
        assert_eq!(backend.activation_calls.load(Ordering::SeqCst), 1);
        drop(predecessor);
        drop(predecessor_lifetime);
        drop(predecessor_owner);

        let (_replacement_store_owner, replacement_store, replacement_lifetime) =
            reopen_placement_store(&lock_path);
        let app_root = tempfile::tempdir().unwrap();
        let replacement = crate::state::test_support::build_with_store(
            app_root.path(),
            replacement_store.clone(),
            replacement_lifetime.clone(),
        )
        .unwrap();
        assert_eq!(
            fence_external_candidates_after_controller_restart(&replacement).unwrap(),
            1
        );
        assert_eq!(
            replacement_store
                .external_allocation(&reservation.placement_thread_id)
                .unwrap()
                .unwrap()
                .phase,
            ExternalAllocationPhase::Quarantined,
        );
        let ExternalPlacementContactDecision::Reconcile(cleanup) = prepared_fixture(
            replacement_store.clone(),
            backend.clone(),
            &reservation,
            &binding,
            gate.clone(),
            replacement_lifetime.clone(),
        )
        .claim()
        .unwrap() else {
            panic!("replacement controller lost exact cleanup authority");
        };
        assert!(cleanup.terminate_or_reconcile().is_err());
        let ExternalPlacementContactDecision::Reconcile(reconcile) = prepared_fixture(
            replacement_store,
            backend.clone(),
            &reservation,
            &binding,
            gate,
            replacement_lifetime,
        )
        .claim()
        .unwrap() else {
            panic!("replacement controller lost uncertain termination reconciliation");
        };
        assert_eq!(
            reconcile.terminate_or_reconcile().unwrap().phase,
            ExternalAllocationPhase::Terminated,
        );
        assert_eq!(backend.allocate_calls.load(Ordering::SeqCst), 1);
        assert_eq!(backend.activation_calls.load(Ordering::SeqCst), 1);
        assert_eq!(backend.terminate_calls.load(Ordering::SeqCst), 1);
        assert_eq!(backend.termination_observations.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn expired_live_startup_cap_never_claims_or_contacts_a_wall_valid_allocation() {
        let (_store_owner, store, reservation, binding, lifetime, _) = placement_store_fixture();
        let backend = Arc::new(FaultBackend::new());
        let gate = Arc::new(AtomicBool::new(false));
        reservation.startup_deadline().unwrap();
        let error = advance_prepared_external_start(
            prepared_fixture(
                store.clone(),
                backend.clone(),
                &reservation,
                &binding,
                gate,
                lifetime,
            ),
            lillux::time::MonotonicDeadline::after(lillux::time::Duration::ZERO),
        )
        .unwrap_err();
        assert!(error.to_string().contains("live deadline"));
        assert_eq!(backend.allocate_calls.load(Ordering::SeqCst), 0);
        assert_eq!(backend.allocation_observations.load(Ordering::SeqCst), 0);
        assert_eq!(
            store.external_allocation("T-one").unwrap().unwrap().phase,
            ExternalAllocationPhase::Reserved
        );
    }

    #[test]
    fn late_startup_observation_timing_rejects_either_lateness_source() {
        // Exercise the post-contact timing predicate without sleeping through
        // fixture construction or depending on scheduler/load speed.
        assert!(require_timely_lifecycle_observation(true, test_startup_deadline()).is_err());
        assert!(
            require_timely_lifecycle_observation(
                false,
                lillux::time::MonotonicDeadline::after(lillux::time::Duration::ZERO),
            )
            .is_err()
        );
        require_timely_lifecycle_observation(false, test_startup_deadline()).unwrap();
    }

    #[test]
    fn late_startup_observations_are_retained_without_release_or_replacement_contact() {
        for late_activation in [false, true] {
            // Use the normal fixture admission budget. Lateness is injected
            // only after provider entry through its explicit observation seam;
            // setup speed must not decide whether contact happens at all.
            let (_store_owner, store, reservation, binding, lifetime, _) =
                placement_store_fixture();
            let backend = Arc::new(FaultBackend {
                late_allocation: !late_activation,
                late_activation,
                ..FaultBackend::new()
            });
            let gate = Arc::new(AtomicBool::new(false));
            let prepared = || {
                prepared_fixture(
                    store.clone(),
                    backend.clone(),
                    &reservation,
                    &binding,
                    gate.clone(),
                    lifetime.clone(),
                )
            };
            assert!(advance_prepared_external_start(prepared(), test_startup_deadline()).is_err());
            if late_activation {
                assert_eq!(
                    advance_prepared_external_start(prepared(), test_startup_deadline()).unwrap(),
                    ExternalCandidateStartProgress::OccurrenceBound
                );
                let guest = tempfile::tempdir().unwrap();
                let activation_error = advance_prepared_external_start(
                    prepared_fixture_with_guest_inputs(
                        store.clone(),
                        backend.clone(),
                        &reservation,
                        &binding,
                        gate.clone(),
                        lifetime.clone(),
                        guest.path(),
                    ),
                    test_startup_deadline(),
                )
                .unwrap_err();
                assert_eq!(
                    backend.activation_calls.load(Ordering::SeqCst),
                    1,
                    "late activation fixture did not reach provider contact: {activation_error:#}"
                );
                assert_eq!(
                    store
                        .external_supervisor_activation("T-one")
                        .unwrap()
                        .unwrap()
                        .observation
                        .unwrap()
                        .activation_state,
                    "started"
                );
            }
            let record = store.external_allocation("T-one").unwrap().unwrap();
            assert_eq!(record.phase, ExternalAllocationPhase::Quarantined);
            assert_eq!(
                record.reservation, reservation,
                "late contact renewed its reservation"
            );
            assert!(
                record.occurrence.is_some(),
                "late contact evidence must not disappear"
            );
            assert!(
                store
                    .optional_external_execution_channel("T-one")
                    .unwrap()
                    .is_none()
            );
            assert_eq!(
                advance_prepared_external_start(prepared(), test_startup_deadline()).unwrap(),
                ExternalCandidateStartProgress::CleanupRequired
            );
            assert_eq!(backend.allocate_calls.load(Ordering::SeqCst), 1);
            assert_eq!(
                backend.activation_calls.load(Ordering::SeqCst),
                usize::from(late_activation)
            );
            // Cleanup has independent bounded authority after late contact.
            for expect_error in [true, false] {
                let ExternalPlacementContactDecision::Reconcile(cleanup) =
                    prepared().claim().unwrap()
                else {
                    panic!("late startup regained allocation authority");
                };
                let result = cleanup.terminate_or_reconcile();
                if expect_error {
                    assert!(result.is_err());
                } else {
                    assert_eq!(result.unwrap().phase, ExternalAllocationPhase::Terminated);
                }
            }
            assert_eq!(backend.allocate_calls.load(Ordering::SeqCst), 1);
        }
    }

    #[test]
    fn bounded_start_driver_never_repeats_an_ambiguous_mutation() {
        let (_store_owner, store, reservation, binding, controller_lifetime, _) =
            placement_store_fixture();
        let backend = Arc::new(FaultBackend::new());
        let gate = Arc::new(AtomicBool::new(false));
        let guest_root = tempfile::tempdir().unwrap();

        let first = advance_prepared_external_start(
            prepared_fixture(
                store.clone(),
                backend.clone(),
                &reservation,
                &binding,
                gate.clone(),
                controller_lifetime.clone(),
            ),
            test_startup_deadline(),
        );
        assert!(first.is_err());
        assert_eq!(
            store.external_allocation("T-one").unwrap().unwrap().phase,
            ExternalAllocationPhase::ContactPending
        );
        assert_eq!(backend.allocate_calls.load(Ordering::SeqCst), 1);

        assert_eq!(
            advance_prepared_external_start(
                prepared_fixture(
                    store.clone(),
                    backend.clone(),
                    &reservation,
                    &binding,
                    gate.clone(),
                    controller_lifetime.clone(),
                ),
                test_startup_deadline()
            )
            .unwrap(),
            ExternalCandidateStartProgress::OccurrenceBound
        );
        assert_eq!(backend.allocate_calls.load(Ordering::SeqCst), 1);
        assert_eq!(backend.allocation_observations.load(Ordering::SeqCst), 1);

        let activation = advance_prepared_external_start(
            prepared_fixture_with_guest_inputs(
                store.clone(),
                backend.clone(),
                &reservation,
                &binding,
                gate.clone(),
                controller_lifetime.clone(),
                guest_root.path(),
            ),
            test_startup_deadline(),
        );
        assert!(activation.is_err());
        assert_eq!(backend.activation_calls.load(Ordering::SeqCst), 1);

        assert_eq!(
            advance_prepared_external_start(
                prepared_fixture(
                    store,
                    backend.clone(),
                    &reservation,
                    &binding,
                    gate,
                    controller_lifetime,
                ),
                test_startup_deadline()
            )
            .unwrap(),
            ExternalCandidateStartProgress::AttachmentPending
        );
        assert_eq!(backend.activation_calls.load(Ordering::SeqCst), 1);
        assert_eq!(backend.activation_observations.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn unresolved_or_negative_activation_never_opens_the_channel() {
        for (resolution, expected) in [
            ("pending", ExternalCandidateStartProgress::SupervisorPending),
            (
                "not_started",
                ExternalCandidateStartProgress::CleanupRequired,
            ),
        ] {
            let (_store_owner, store, reservation, binding, controller_lifetime, _) =
                placement_store_fixture();
            let backend = Arc::new(FaultBackend::with_reconciled_activation(resolution));
            let gate = Arc::new(AtomicBool::new(false));
            let guest_root = tempfile::tempdir().unwrap();
            assert!(
                advance_prepared_external_start(
                    prepared_fixture(
                        store.clone(),
                        backend.clone(),
                        &reservation,
                        &binding,
                        gate.clone(),
                        controller_lifetime.clone(),
                    ),
                    test_startup_deadline()
                )
                .is_err()
            );
            assert_eq!(
                advance_prepared_external_start(
                    prepared_fixture(
                        store.clone(),
                        backend.clone(),
                        &reservation,
                        &binding,
                        gate.clone(),
                        controller_lifetime.clone(),
                    ),
                    test_startup_deadline()
                )
                .unwrap(),
                ExternalCandidateStartProgress::OccurrenceBound
            );
            assert!(
                advance_prepared_external_start(
                    prepared_fixture_with_guest_inputs(
                        store.clone(),
                        backend.clone(),
                        &reservation,
                        &binding,
                        gate.clone(),
                        controller_lifetime.clone(),
                        guest_root.path(),
                    ),
                    test_startup_deadline()
                )
                .is_err()
            );
            assert_eq!(
                advance_prepared_external_start(
                    prepared_fixture(
                        store.clone(),
                        backend.clone(),
                        &reservation,
                        &binding,
                        gate,
                        controller_lifetime,
                    ),
                    test_startup_deadline()
                )
                .unwrap(),
                expected
            );
            assert!(
                store
                    .optional_external_execution_channel(&reservation.placement_thread_id)
                    .unwrap()
                    .is_none()
            );
            assert_eq!(backend.activation_calls.load(Ordering::SeqCst), 1);
            assert_eq!(backend.activation_observations.load(Ordering::SeqCst), 1);
        }
    }

    fn assert_start_driver_opens_only_after_signed_readiness(
        activation_resolution: &'static str,
        before_channel: ExternalCandidateStartProgress,
    ) {
        let (_store_owner, store, reservation, retained, controller_lifetime, _) =
            placement_store_fixture();
        let backend = Arc::new(FaultBackend::with_reconciled_activation(
            activation_resolution,
        ));
        let gate = Arc::new(AtomicBool::new(false));
        let guest_root = tempfile::tempdir().unwrap();

        assert!(
            advance_prepared_external_start(
                prepared_fixture(
                    store.clone(),
                    backend.clone(),
                    &reservation,
                    &retained,
                    gate.clone(),
                    controller_lifetime.clone(),
                ),
                test_startup_deadline()
            )
            .is_err()
        );
        assert_eq!(
            advance_prepared_external_start(
                prepared_fixture(
                    store.clone(),
                    backend.clone(),
                    &reservation,
                    &retained,
                    gate.clone(),
                    controller_lifetime.clone(),
                ),
                test_startup_deadline()
            )
            .unwrap(),
            ExternalCandidateStartProgress::OccurrenceBound
        );
        assert!(
            advance_prepared_external_start(
                prepared_fixture_with_guest_inputs(
                    store.clone(),
                    backend.clone(),
                    &reservation,
                    &retained,
                    gate.clone(),
                    controller_lifetime.clone(),
                    guest_root.path(),
                ),
                test_startup_deadline()
            )
            .is_err()
        );
        assert_eq!(
            advance_prepared_external_start(
                prepared_fixture(
                    store.clone(),
                    backend.clone(),
                    &reservation,
                    &retained,
                    gate.clone(),
                    controller_lifetime.clone(),
                ),
                test_startup_deadline()
            )
            .unwrap(),
            before_channel
        );

        let occurrence = store
            .external_allocation(&reservation.placement_thread_id)
            .unwrap()
            .unwrap()
            .occurrence
            .unwrap();
        let candidate_program = program();
        let supervisor = lillux::crypto::SigningKey::from_bytes(&[63; 32]);
        let now = lillux::time::timestamp_millis();
        let channel = ryeos_state::external_execution::ExecutionChannelBinding {
            schema: ryeos_state::external_execution::EXECUTION_CHANNEL_BINDING_SCHEMA,
            execution_mode:
                ryeos_external_execution_contract::ExternalExecutionMode::StructuredSession {},
            placement_thread_id: reservation.placement_thread_id.clone(),
            allocation_request_digest: reservation.request_digest.clone(),
            occurrence_id: occurrence.occurrence_id,
            admitted_capsule_hash: reservation.admitted_capsule_hash.clone(),
            base_snapshot_hash: reservation.base_snapshot_hash.clone(),
            execution_binding_hash: reservation.binding_hash.clone(),
            supervisor_runtime_hash: candidate_program.runtime_manifest_hash.clone(),
            candidate_program_digest: candidate_program.digest().unwrap(),
            channel_nonce: "8".repeat(64),
            owner_public_key: reservation.channel_owner_public_key.clone(),
            supervisor_public_key: ryeos_state::external_execution::encode_channel_public_key(
                &supervisor.verifying_key(),
            )
            .unwrap(),
            issued_at_ms: now,
            execution_deadline_ms: now + 60_000,
            expires_at_ms: now + 120_000,
            candidate_export_max_bytes: 512,
            max_frames: 64,
            max_bytes: 2048,
        };
        store.register_external_execution_channel(&channel).unwrap();
        assert_eq!(
            advance_prepared_external_start(
                prepared_fixture(
                    store.clone(),
                    backend.clone(),
                    &reservation,
                    &retained,
                    gate.clone(),
                    controller_lifetime.clone(),
                ),
                test_startup_deadline()
            )
            .unwrap(),
            ExternalCandidateStartProgress::ChannelAttached
        );

        let ready = ryeos_state::external_execution::SignedExecutionFrame::sign(
            ryeos_state::external_execution::ExecutionFrame {
                schema: 1,
                binding_digest: channel.digest().unwrap(),
                direction: ryeos_state::external_execution::ChannelDirection::SupervisorToOwner,
                sequence: 1,
                previous_frame_digest: None,
                acknowledged_peer_sequence: 0,
                payload: ryeos_state::external_execution::ExecutionChannelPayload::Ready {
                    supervisor_runtime_hash: channel.supervisor_runtime_hash.clone(),
                    base_snapshot_hash: channel.base_snapshot_hash.clone(),
                    boot: None,
                },
            },
            &channel,
            &supervisor,
        )
        .unwrap();
        let ready_wire = lillux::canonical_json(&serde_json::to_value(ready).unwrap()).unwrap();
        let authority =
            ExternalChannelAuthority::test_fixture(&reservation.channel_authority_generation);
        store
            .exchange_external_supervisor_frame(
                &reservation.placement_thread_id,
                ready_wire.as_bytes(),
                authority.owner_signing_key(),
                16,
                1024 * 1024,
            )
            .unwrap();

        assert_eq!(
            advance_prepared_external_start(
                prepared_fixture(
                    store,
                    backend.clone(),
                    &reservation,
                    &retained,
                    gate,
                    controller_lifetime,
                ),
                test_startup_deadline()
            )
            .unwrap(),
            ExternalCandidateStartProgress::Ready(channel)
        );
        assert_eq!(backend.allocate_calls.load(Ordering::SeqCst), 1);
        assert_eq!(backend.activation_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn start_driver_opens_execution_only_after_signed_readiness() {
        assert_start_driver_opens_only_after_signed_readiness(
            "started",
            ExternalCandidateStartProgress::AttachmentPending,
        );
    }

    #[test]
    fn signed_ready_can_resolve_provider_pending_activation_without_retry() {
        assert_start_driver_opens_only_after_signed_readiness(
            "pending",
            ExternalCandidateStartProgress::SupervisorPending,
        );
    }

    #[test]
    fn cleanup_classification_is_derived_only_from_durable_external_state() {
        let (store_owner, store, reservation, binding, controller_lifetime, _) =
            placement_store_fixture();
        assert_eq!(
            classify_external_start_cleanup(&store, "T-one"),
            ExternalCandidateStartCleanup::Proved
        );
        assert_eq!(
            store.external_allocation("T-one").unwrap().unwrap().phase,
            ExternalAllocationPhase::NoContact
        );
        // One test Store per thread: close the first node before the second.
        drop(store);
        drop(controller_lifetime);
        drop(store_owner);

        let root = tempfile::tempdir().unwrap().keep();
        let identity =
            ryeos_node_runtime::identity::NodeIdentity::create(&root.join("node-key.pem")).unwrap();
        let signer = Arc::new(crate::state_store::NodeIdentitySigner::from_identity(
            &identity,
        ));
        let mut trust = ryeos_state::refs::TrustStore::new();
        trust.insert(identity.fingerprint().to_owned(), *identity.verifying_key());
        let contacted_owner =
            crate::state_store::test_native::TestStateStore::open(&root, signer, Arc::new(trust))
                .unwrap();
        let lifetime = contacted_owner.controller_lifetime();
        let contacted = Arc::clone(contacted_owner.store());
        contacted
            .install_external_placement_test_fixture(&reservation, &binding)
            .unwrap();
        let decision = prepared_fixture(
            contacted.clone(),
            Arc::new(FaultBackend::new()),
            &reservation,
            &binding,
            Arc::new(AtomicBool::new(false)),
            lifetime,
        )
        .claim()
        .unwrap();
        let ExternalPlacementContactDecision::Contact(permit) = decision else {
            panic!("fixture did not retain the unique contact permit");
        };
        assert!(permit.contact(test_startup_deadline()).is_err());
        assert_eq!(
            classify_external_start_cleanup(&contacted, "T-one"),
            ExternalCandidateStartCleanup::Unproved
        );
        assert_eq!(
            contacted
                .external_allocation("T-one")
                .unwrap()
                .unwrap()
                .phase,
            ExternalAllocationPhase::ContactPending
        );
    }

    #[test]
    fn allocation_reconciliation_refuses_an_active_contact_lease_without_contact() {
        let (_store_owner, store, reservation, binding, controller_lifetime, _) =
            placement_store_fixture();
        let backend = Arc::new(FaultBackend::new());
        let gate = Arc::new(AtomicBool::new(false));
        let ExternalPlacementContactDecision::Contact(permit) = prepared_fixture(
            store.clone(),
            backend.clone(),
            &reservation,
            &binding,
            gate.clone(),
            controller_lifetime.clone(),
        )
        .claim()
        .unwrap() else {
            panic!("fresh reservation did not return a contact permit");
        };
        drop(permit);
        let ExternalPlacementContactDecision::Reconcile(reconciliation) = prepared_fixture(
            store.clone(),
            backend.clone(),
            &reservation,
            &binding,
            gate.clone(),
            controller_lifetime,
        )
        .claim()
        .unwrap() else {
            panic!("contacted reservation did not return reconciliation authority");
        };
        assert!(
            gate.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        );
        let successor = ExternalContactLease { gate: gate.clone() };
        assert!(reconciliation.reconcile().is_err());
        assert_eq!(backend.allocation_observations.load(Ordering::SeqCst), 0);
        assert!(gate.load(Ordering::Acquire));
        assert_eq!(
            store.external_allocation("T-one").unwrap().unwrap().phase,
            ExternalAllocationPhase::ContactPending
        );
        drop(successor);
        assert!(!gate.load(Ordering::Acquire));
    }

    #[test]
    fn allocation_reconciliation_releases_lease_after_each_authoritative_resolution() {
        for resolution in ["bound", "pending", "no_occurrence"] {
            let (_store_owner, store, reservation, binding, controller_lifetime, _) =
                placement_store_fixture();
            let backend = Arc::new(FaultBackend {
                reconciled_allocation: resolution,
                ..FaultBackend::new()
            });
            let gate = Arc::new(AtomicBool::new(false));
            let ExternalPlacementContactDecision::Contact(permit) = prepared_fixture(
                store.clone(),
                backend.clone(),
                &reservation,
                &binding,
                gate.clone(),
                controller_lifetime.clone(),
            )
            .claim()
            .unwrap() else {
                panic!("fresh reservation did not return a contact permit");
            };
            drop(permit);
            let ExternalPlacementContactDecision::Reconcile(reconciliation) = prepared_fixture(
                store.clone(),
                backend.clone(),
                &reservation,
                &binding,
                gate.clone(),
                controller_lifetime,
            )
            .claim()
            .unwrap() else {
                panic!("contacted reservation did not return reconciliation authority");
            };
            let record = reconciliation
                .reconcile_with_deadline(test_startup_deadline(), test_observation_timing())
                .unwrap()
                .value;
            assert!(!gate.load(Ordering::Acquire));
            assert_eq!(backend.allocation_observations.load(Ordering::SeqCst), 1);
            let retained = store.external_allocation("T-one").unwrap().unwrap();
            assert_eq!(record.phase, retained.phase);
            assert_eq!(
                record.phase,
                match resolution {
                    "bound" => ExternalAllocationPhase::Bound,
                    "pending" => ExternalAllocationPhase::ContactPending,
                    "no_occurrence" => ExternalAllocationPhase::ContactedNoOccurrence,
                    _ => unreachable!(),
                }
            );
        }
    }

    #[test]
    fn consumed_negative_contact_lease_cannot_clear_a_successor_gate() {
        let gate = Arc::new(AtomicBool::new(true));
        let mut original = Some(ExternalContactLease { gate: gate.clone() });
        // This is the negative-response transfer: the old lease is consumed,
        // not merely released through a borrowed reference and left armed.
        drop(original.take());
        assert!(
            gate.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        );
        let successor = ExternalContactLease { gate: gate.clone() };
        drop(original);
        assert!(gate.load(Ordering::Acquire));
        drop(successor);
        assert!(!gate.load(Ordering::Acquire));
    }

    #[test]
    fn active_contact_permit_blocks_negative_reconciliation_settlement() {
        let (_store_owner, store, reservation, binding, controller_lifetime, _) =
            placement_store_fixture();
        let backend = Arc::new(FaultBackend::new());
        let gate = Arc::new(AtomicBool::new(false));
        let decision = prepared_fixture(
            store.clone(),
            backend,
            &reservation,
            &binding,
            gate.clone(),
            controller_lifetime,
        )
        .claim()
        .unwrap();
        let ExternalPlacementContactDecision::Contact(permit) = decision else {
            panic!("fresh reservation did not return a contact permit");
        };
        assert!(
            apply_reconciliation_resolution(
                &store,
                &gate,
                &reservation,
                ExternalAllocationResolution::NoOccurrence {
                    provider_observation_digest: "f".repeat(64),
                },
                test_observation_timing()
            )
            .is_err()
        );
        assert_eq!(
            store.external_allocation("T-one").unwrap().unwrap().phase,
            ExternalAllocationPhase::ContactPending
        );
        drop(permit);
        assert!(!gate.load(Ordering::Acquire));
        assert_eq!(
            apply_reconciliation_resolution(
                &store,
                &gate,
                &reservation,
                ExternalAllocationResolution::NoOccurrence {
                    provider_observation_digest: "f".repeat(64),
                },
                test_observation_timing()
            )
            .unwrap()
            .phase,
            ExternalAllocationPhase::ContactedNoOccurrence
        );
    }

    #[test]
    fn active_supervisor_contact_blocks_termination_mutation() {
        let (_store_owner, store, reservation, binding, controller_lifetime, _) =
            placement_store_fixture();
        let backend = Arc::new(FaultBackend::new());
        let gate = Arc::new(AtomicBool::new(false));
        assert!(matches!(
            store
                .claim_external_allocation_contact("T-one", &reservation.request_digest)
                .unwrap(),
            ExternalAllocationContactClaim::Contact(_)
        ));
        store
            .bind_external_allocation(
                "T-one",
                &ExternalAllocationOccurrence {
                    schema: 1,
                    binding_hash: reservation.binding_hash.clone(),
                    request_digest: reservation.request_digest.clone(),
                    occurrence_id: "fixture-occurrence".into(),
                    provider_observation_digest: "f".repeat(64),
                },
                test_observation_timing(),
            )
            .unwrap();
        let decision = prepared_fixture(
            store,
            backend.clone(),
            &reservation,
            &binding,
            gate.clone(),
            controller_lifetime,
        )
        .claim()
        .unwrap();
        let ExternalPlacementContactDecision::Reconcile(cleanup) = decision else {
            panic!("bound occurrence did not retain cleanup authority");
        };
        gate.store(true, Ordering::Release);
        assert!(cleanup.terminate_or_reconcile().is_err());
        assert_eq!(backend.terminate_calls.load(Ordering::SeqCst), 0);
        assert!(gate.load(Ordering::Acquire));
        gate.store(false, Ordering::Release);
    }

    #[test]
    fn contact_permit_keeps_replacement_controller_excluded() {
        let (store_owner, store, reservation, binding, controller_lifetime, lock_path) =
            placement_store_fixture();
        let gate = Arc::new(AtomicBool::new(false));
        let decision = prepared_fixture(
            store,
            Arc::new(FaultBackend::new()),
            &reservation,
            &binding,
            gate,
            controller_lifetime.clone(),
        )
        .claim()
        .unwrap();
        let ExternalPlacementContactDecision::Contact(permit) = decision else {
            panic!("fresh reservation did not return a contact permit");
        };

        // Simulate the composition root dropping its primary guard after a
        // bounded runtime shutdown. The in-flight permit must now be the only
        // holder of that controller lifetime. A native Store also retains its
        // own lease on the operator lock for as long as it is open, and the
        // permit keeps the Store open, so the OS lock alone cannot show which
        // holder excludes the replacement. Track the controller lease itself.
        let controller = Arc::downgrade(&controller_lifetime);
        drop(controller_lifetime);
        assert!(
            controller.upgrade().is_some(),
            "the contact permit released the controller lifetime it must retain"
        );
        assert!(ryeos_node_runtime::state_lock::StateLock::acquire(&lock_path).is_err());

        drop(permit);
        assert!(
            controller.upgrade().is_none(),
            "a holder other than the contact permit retained the controller lifetime"
        );
        // The Store's own lease ends when its owner closes it.
        drop(store_owner);
        ryeos_node_runtime::state_lock::StateLock::acquire(&lock_path)
            .expect("replacement remained excluded after the contact permit stopped");
    }

    #[test]
    fn policy_change_after_supervisor_activation_does_not_block_recovery() {
        use crate::external_supervisor_native::{
            EXTERNAL_SUPERVISOR_JOURNAL_CLASS, SupervisorJournalFunding,
            test_supervisor_journal_envelope,
        };
        use ryeos_node_runtime::node_policy::NodePolicySnapshot;
        use ryeos_scheduler_contract::{NodeMaintenancePolicy, OfflineNativeQuota};

        let policy = |operation_millis: Option<u64>| {
            let envelope = test_supervisor_journal_envelope();
            let offline_native = operation_millis
                .map(|millis| {
                    (
                        EXTERNAL_SUPERVISOR_JOURNAL_CLASS.to_owned(),
                        OfflineNativeQuota {
                            max_native_storage_bytes: envelope.storage_bytes,
                            max_native_allocator_steps: envelope.allocator_steps,
                            max_native_connections: envelope.connections,
                            max_operation_millis: millis,
                            max_cleanup_millis: envelope.cleanup_millis,
                            max_cleanup_steps: envelope.cleanup_steps,
                        },
                    )
                })
                .into_iter()
                .collect();
            NodePolicySnapshot::from_test_records(vec![Arc::new(NodeMaintenancePolicy {
                schema: 1,
                schedules: Vec::new(),
                offline_reset: None,
                offline_native,
                remote_offline_operations: Vec::new(),
            })])
        };
        let contract = InstalledExternalExecutionBinding::test_fixture().backend_contract();
        let lifetime =
            crate::external_supervisor_native::external_supervisor_occurrence_lifetime_ms(
                &contract,
                contract.timeout_seconds,
            )
            .unwrap();
        let at_activation = policy(Some(lifetime));
        let shrunk = policy(Some(lifetime - 1));
        let revoked = policy(None);

        // The policy in force at first contact and first activation funds the
        // occurrence.
        for (phase, activation_exists) in [
            (ExternalAllocationPhase::Reserved, false),
            (ExternalAllocationPhase::Bound, false),
        ] {
            let funding = qualification_start_journal_funding(phase, activation_exists);
            assert_eq!(funding, SupervisorJournalFunding::New);
            funding
                .require(&at_activation, &contract, contract.timeout_seconds)
                .unwrap();
            assert!(
                funding
                    .require(&shrunk, &contract, contract.timeout_seconds)
                    .is_err()
            );
            assert!(
                funding
                    .require(&revoked, &contract, contract.timeout_seconds)
                    .is_err()
            );
        }

        // Once the activation is retained, a narrowed or revoked policy no
        // longer reaches its recovery: the sealed envelope stays its funding.
        let funding = qualification_start_journal_funding(ExternalAllocationPhase::Bound, true);
        assert_eq!(funding, SupervisorJournalFunding::Retained);
        for changed in [&shrunk, &revoked] {
            funding
                .require(changed, &contract, contract.timeout_seconds)
                .unwrap();
        }
    }
}
