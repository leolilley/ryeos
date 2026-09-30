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
    ExternalObservationTiming, ExternalSupervisorActivationIntent,
    ExternalSupervisorActivationObservation, ExternalSupervisorActivationRecord,
    ExternalTerminalObservation, ExternalTerminationIntent,
};
use crate::runtime_db::{WorkspaceRecord, WorkspaceState};
use crate::state::AppState;
use crate::state_lock::StateLockLease;
use crate::vault::external_channel::{ExternalChannelAuthority, ExternalChannelAuthorityAccess};
use crate::vault::placement::PlacementCredential;

/// Trusted controller adapter metadata and offline contract verification. This
/// method must not perform provider I/O; all provider mutation is fenced by the
/// later contact permit.
pub(crate) struct ExternalLifecycleObservation<T> {
    pub value: T,
    pub deadline_exceeded: bool,
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
    #[cfg(any(test, feature = "test-support"))]
    fn artifact_bytes(&self) -> u64;
    fn supervisor_artifact(&self) -> (&str, u64);
    fn launcher_artifact(&self) -> (&str, u64);
    #[cfg(any(test, feature = "test-support"))]
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
        ryeos_external_execution::restoration_verifier_delivery::SealedRestorationVerifierUpload,
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

    /// Prepare and locally re-import the exact secret-bearing guest package
    /// before the durable activation/contact claim. No provider contact is
    /// permitted here. The returned commitment is inserted with that claim.
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

    pub(crate) fn qualify(
        &self,
        contract: &ExternalPlacementBackendContract,
    ) -> Result<Arc<InstalledExternalCandidateConnector>> {
        ensure!(
            contract.workload.structured_session()?.connector_protocol
                == ryeos_state::external_execution::admission::CONNECTOR_PROTOCOL,
            "signed external connector protocol is unsupported"
        );
        let artifact = self
            .artifacts
            .get(&(
                contract
                    .workload
                    .structured_session()?
                    .connector_artifact_hash
                    .clone(),
                contract
                    .workload
                    .structured_session()?
                    .connector_artifact_bytes,
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

    pub(crate) fn qualify(
        &self,
        contract: &ExternalPlacementBackendContract,
    ) -> Result<Arc<InstalledExternalProviderConfiguration>> {
        let artifact = self
            .artifacts
            .get(
                &contract
                    .workload
                    .structured_session()?
                    .provider_declaration_id,
            )
            .cloned()
            .context("exact signed external provider declaration is not installed")?;
        ensure!(
            artifact.artifact_hash
                == contract
                    .workload
                    .structured_session()?
                    .configuration_adapter_artifact_hash
                && artifact.artifact_bytes
                    == contract
                        .workload
                        .structured_session()?
                        .configuration_adapter_artifact_bytes
                && artifact.declaration.configuration_destination
                    == contract
                        .workload
                        .structured_session()?
                        .provider_configuration_destination,
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

#[derive(Debug, Default)]
pub struct ExternalPlacementBackendRegistry {
    backends: BTreeMap<(String, String), Arc<dyn ExternalPlacementBackend>>,
    contact_gates: Mutex<BTreeMap<String, Weak<AtomicBool>>>,
}

impl ExternalPlacementBackendRegistry {
    pub(crate) fn seal_restoration_verifier_upload(
        &self,
        producer: &crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
        qualification: &crate::node_config::sections::runtime_snapshot_qualification::InstalledRuntimeSnapshotQualificationBinding,
    ) -> Result<
        ryeos_external_execution::restoration_verifier_delivery::SealedRestorationVerifierUpload,
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

    fn qualify_for_purpose(
        &self,
        contract: &ExternalPlacementBackendContract,
        credential: &PlacementCredential,
        program: &AdmittedExternalExecutionProgram,
        cleanup_only: bool,
    ) -> Result<Arc<dyn ExternalPlacementBackend>> {
        program.validate()?;
        let required = match program {
            AdmittedExternalExecutionProgram::StructuredSession(program) => {
                let session = contract.workload.structured_session()?;
                ensure!(
                    program.runtime_manifest_hash == session.runtime_manifest_hash
                        && program.selection_identity_digest == session.runtime_selection_identity,
                    "external qualification changed its exact session runtime"
                );
                program.requirement.required_lifecycle_capabilities.clone()
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
                BTreeSet::new()
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
    ) -> Result<Arc<dyn ExternalPlacementBackend>> {
        let backend = self
            .backends
            .get(&(
                contract.backend.clone(),
                contract.backend_artifact_hash.clone(),
            ))
            .cloned()
            .context("exact signed external placement backend generation is not installed")?;
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
            ensure!(
                contract.runtime_qualification.is_some()
                    == effective.contains(&LifecycleCapability::IndependentGuestRuntimeAdmission),
                "external runtime qualification relationship differs from installed adapter admission"
            );
        }
        let missing: BTreeSet<_> = required.difference(&effective).copied().collect();
        ensure!(
            missing.is_empty(),
            "external lifecycle adapter lacks required capabilities: {missing:?}"
        );
        Ok(backend)
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
            require_current_runtime_qualification_for_start(
                state,
                &binding.backend_contract(),
                binding.digest(),
            )
        },
    )
}

/// Recheck the current published witness under its actual product owner's
/// grant and run the exact installed adapter's offline probe before fresh
/// contact. The signed relationship alone is never a startup permit.
fn require_current_runtime_qualification_for_start(
    state: &AppState,
    contract: &ExternalPlacementBackendContract,
    binding_hash: &str,
) -> Result<()> {
    let _ = admit_current_runtime_qualification(state, contract, binding_hash)?;
    Ok(())
}

/// Acquired session-runtime compatibility does not substitute for the separate
/// guest-owner snapshot proof. Missing testimony is not an activation permit.
fn require_current_runtime_content_qualification(
    state: &AppState,
    contract: &ExternalPlacementBackendContract,
) -> Result<
    Option<ryeos_state::external_content::qualification_publication::PublishedContentQualification>,
> {
    let session = match &contract.workload {
        crate::node_config::sections::external_execution::ExternalWorkloadBinding::StructuredSession(session) => session,
        crate::node_config::sections::external_execution::ExternalWorkloadBinding::DirectCommand {} => {
            ensure!(contract.runtime_content_qualification.is_none(),
                "direct workload cannot inherit structured-session compatibility testimony");
            return Ok(None);
        }
    };
    let selected = contract
        .runtime_content_qualification
        .as_ref()
        .context("external structured session has no qualified runtime content selection")?;
    let operator = crate::operator_authority::admitted_operator_authority_for_principal(
        state,
        &selected.owner_principal,
    )?;
    let context = operator.handler_context();
    let qualified =
        crate::operator_external_content::content_qualification::load_current_qualified_content(
            state,
            &context,
            &selected.activation_ref,
            &selected.coordinate_id,
            &selected.attestation_hash,
            &session.runtime_manifest_hash,
        )?;
    qualified.evidence.result.validate_claims_for(
        &qualified.evidence.purpose.policy_source.policy,
        &selected.required_claims,
    )?;
    Ok(Some(qualified))
}

fn admit_current_runtime_qualification(
    state: &AppState,
    contract: &ExternalPlacementBackendContract,
    binding_hash: &str,
) -> Result<Option<ryeos_state::objects::RetainedExternalRuntimeQualification>> {
    require_current_runtime_content_qualification(state, contract)?;
    let Some(qualification) = &contract.runtime_qualification else {
        return Ok(None);
    };
    let witness = crate::operator_external_content::product_qualification::verify_current_external_runtime_qualification(
        state,
        qualification,
        &contract.guest_runtime_manifest_hash,
    )?;
    let retained = ryeos_state::objects::RetainedExternalRuntimeQualification {
        binding_hash: binding_hash.to_owned(),
        guest_runtime_manifest_hash: contract.guest_runtime_manifest_hash.clone(),
        owner_principal: qualification.owner_principal.clone(),
        proof: ryeos_state::external_content::products::composition::AdmittedProductQualification {
            attestation_hash: witness.attestation_hash,
            evidence: witness.evidence,
        },
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
            && content.evidence.purpose.owner_fingerprint == selected.owner_principal,
        "retained content testimony differs from signed session binding"
    );
    content.evidence.result.validate_claims_for(
        &content.evidence.purpose.policy_source.policy,
        &selected.required_claims,
    )?;
    verify_retained_runtime_content(state, content)?;
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
    program.verify_selections(capsule.retained_product_selections.as_ref())?;
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
        .require::<crate::node_policy::sections::object_closure::NodeObjectClosurePolicy>()?
        .closure_limits()?;
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
        .require::<crate::node_policy::sections::object_closure::NodeObjectClosurePolicy>()?
        .closure_limits()?;
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    crate::operator_external_content::product_qualification::verify_retained_qualification_guarded(
        state,
        &authority,
        &guard,
        limits,
        &retained.proof,
        &retained.owner_principal,
    )?;
    let product = crate::operator_external_content::product_receipt::load_product_source(
        state,
        &authority,
        &guard,
        limits,
        &retained.owner_principal,
        &retained.proof.evidence.product_witness_hash,
        &retained.proof.evidence.witness_source,
        crate::operator_external_content::product_receipt::ProductSourceVerification::Retained,
    )?;
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
                    && retained.proof.attestation_hash == selected.attestation_hash,
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
    backends.qualify_dependencies(&contract, &credential, BTreeSet::new(), true)?;
    Ok(retained)
}

/// Admission-only check used while sealing a session capsule. It performs no
/// provider I/O and grants no allocation/contact authority.
pub fn preflight_external_candidate_program(
    state: &AppState,
    program: &ryeos_state::external_execution::admission::AdmittedExternalCandidateProgram,
) -> Result<(
    Option<ryeos_state::objects::RetainedExternalRuntimeQualification>,
    ryeos_state::objects::RetainedExternalRuntimeContentQualification,
)> {
    let binding = select_binding(&state.node_config.external_execution, program)?;
    let contract = binding.backend_contract();
    let runtime_proof = admit_current_runtime_qualification(state, &contract, binding.digest())?;
    let content = require_current_runtime_content_qualification(state, &contract)?
        .context("external candidate has no authenticated runtime content")?;
    program.require_content_qualification(&content.evidence)?;
    let selected = contract
        .runtime_content_qualification
        .as_ref()
        .context("external candidate has no signed content selection")?;
    let retained_content = ryeos_state::objects::RetainedExternalRuntimeContentQualification {
        binding_hash: binding.digest().to_owned(),
        runtime_manifest_hash: program.runtime_manifest_hash.clone(),
        activation_ref: selected.activation_ref.clone(),
        coordinate_id: content.coordinate_id,
        attestation_hash: content.attestation_hash,
        evidence: content.evidence,
    };
    retained_content.validate()?;
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
    let now = i64::try_from(lillux::time::timestamp_millis())?;
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
        i64::try_from(lillux::time::timestamp_millis())?,
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
            program.verify_selections(capsule.retained_product_selections.as_ref())?;
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
    };
    let contract = retained.backend_contract();
    validate_external_placement_program(&program, reservation, &contract)?;
    Ok(program)
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
            require_current_runtime_qualification_for_start(
                self.state,
                &contract,
                binding.digest(),
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
            require_current_runtime_qualification_for_start(
                self.state,
                &contract,
                binding.digest(),
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
        let request_digest = ryeos_state::objects::canonical_value_digest(&serde_json::json!({
            "domain":"ryeos.external-direct-placement-request.v1", "placement_thread_id":placement,
            "admitted_capsule_hash":capsule_hash, "base_snapshot_hash":snapshot_hash,
            "binding_hash":binding.digest(), "owner":owner, "backend_contract":contract,
            "channel_authority_generation":authority_generation,
            "channel_owner_public_key":channel_authority.owner_public_key(),
            "channel_bootstrap_capability_hash":channel_authority.bootstrap_capability_hash(),
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
            let now = i64::try_from(lillux::time::timestamp_millis())?;
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
            backend,
            contract,
            credential,
            contact_gate,
            controller_lifetime,
            channel_authority,
            program: generic_program,
            guest_inputs: None,
            record,
        })
    }

    /// Prepare or exactly recover one placement. The placement id is an
    /// already-born dedicated session; no caller-authored allocation shape is
    /// accepted at this boundary.
    pub(crate) fn prepare(&self, placement: &str) -> Result<PreparedExternalPlacement> {
        self.prepare_for_purpose(placement, false)
    }

    fn prepare_for_cleanup(&self, placement: &str) -> Result<PreparedExternalPlacement> {
        self.prepare_for_purpose(placement, true)
    }

    fn prepare_for_purpose(
        &self,
        placement: &str,
        cleanup_only: bool,
    ) -> Result<PreparedExternalPlacement> {
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
        program.verify_selections(capsule.retained_product_selections.as_ref())?;
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

        let (binding, contract, credential_access) = if let Some(existing) = &existing {
            ensure!(
                !existing.phase.is_settled(),
                "settled external placement has no recovery authority"
            );
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
        if !cleanup_only {
            require_retained_session_runtime_qualification(self.state, &capsule, &binding)?;
        }
        if !cleanup_only
            && existing
                .as_ref()
                .is_none_or(|record| record.phase == ExternalAllocationPhase::Reserved)
        {
            require_current_runtime_qualification_for_start(
                self.state,
                &contract,
                binding.digest(),
            )?;
        }
        let credential = credential_access.decode(
            self.state
                .vault
                .placement_credential(&credential_access)
                .context("read protected external placement credential")?,
        )?;
        let admitted = AdmittedExternalExecutionProgram::StructuredSession(program.clone());
        let backend = if cleanup_only {
            ensure!(
                existing.is_some(),
                "external cleanup has no retained allocation"
            );
            self.state.external_placement_backends.qualify_for_cleanup(
                &contract,
                &credential,
                &admitted,
            )?
        } else {
            self.state
                .external_placement_backends
                .qualify(&contract, &credential, &admitted)?
        };
        if existing.is_none() {
            require_guest_package_artifact_floor(&contract, backend.as_ref())?;
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
            let now = i64::try_from(lillux::time::timestamp_millis())?;
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
        let record = self
            .state
            .state_store
            .reserve_external_allocation(&reservation, &binding)?;
        Ok(PreparedExternalPlacement {
            state_store: self.state.state_store.clone(),
            backend,
            contract,
            credential,
            contact_gate,
            controller_lifetime,
            channel_authority,
            program: AdmittedExternalExecutionProgram::StructuredSession(program.clone()),
            guest_inputs: None,
            record,
        })
    }
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
                    prepared = prepared.with_guest_inputs(inputs)?;
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
        .and_then(|prepared| prepared.with_guest_inputs(guest_inputs))
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

fn fence_external_candidate_automatically(state: &AppState, placement: &str) -> Result<bool> {
    if preserves_external_direct_settlement(state, placement)? {
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
        ExternalAllocationOwner::DedicatedSession(_) => owner.prepare_for_cleanup(placement)?,
        ExternalAllocationOwner::DirectThread { .. } => {
            owner.prepare_retained_direct_for_cleanup(placement)?
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
    let placements = state
        .state_store
        .recoverable_external_cleanup_placements()?;
    let mut report = ExternalCandidateCleanupRecovery {
        discovered: placements.len(),
        ..Default::default()
    };
    for placement in placements {
        let result = (|| {
            if preserves_external_direct_settlement(state, &placement)? {
                advance_external_direct_settlement(state, &placement)
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
        }
    }
}

impl Drop for ExternalCandidateCleanupLifeline {
    fn drop(&mut self) {
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
    match prepared.claim()? {
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

pub(crate) struct PreparedExternalPlacement {
    state_store: Arc<crate::state_store::StateStore>,
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
    record: ExternalAllocationRecord,
}

impl PreparedExternalPlacement {
    fn with_guest_inputs(
        mut self,
        guest_inputs: ryeos_external_execution::guest_inputs::ExternalGuestInputAuthority,
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
        Ok(self)
    }

    /// Consume preparation and durably decide whether this process owns the
    /// one allowed allocator call. A false claim is reconciliation authority,
    /// never permission to allocate again.
    pub(crate) fn claim(self) -> Result<ExternalPlacementContactDecision> {
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
        let claim = match self.state_store.claim_external_allocation_contact(
            &self.record.reservation.placement_thread_id,
            &self.record.reservation.request_digest,
        ) {
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
                        backend: self.backend,
                        contract: self.contract,
                        credential: self.credential,
                        contact_gate: self.contact_gate,
                        channel_authority: self.channel_authority,
                        program: self.program,
                        guest_inputs: self.guest_inputs,
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
    backend: Arc<dyn ExternalPlacementBackend>,
    contract: ExternalPlacementBackendContract,
    credential: PlacementCredential,
    contact_gate: Arc<AtomicBool>,
    channel_authority: ExternalChannelAuthority,
    program: AdmittedExternalExecutionProgram,
    guest_inputs: Option<ryeos_external_execution::guest_inputs::ExternalGuestInputAuthority>,
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
        let release = self.state_store.admit_external_ready_and_author_release(
            placement,
            self.channel_authority.owner_signing_key(),
            live_deadline,
        )?;
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
            let guest_inputs = self
                .guest_inputs
                .take()
                .context("first supervisor activation has no exact guest input authority")?;
            let package_parent = self.state_store.external_guest_package_parent()?;
            let (intent, mut activation) = supervisor_activation(
                &self.contract,
                &current.reservation,
                occurrence,
                &self.channel_authority,
                &self.program,
                guest_inputs,
                self.backend.as_ref(),
                &package_parent,
                deadline,
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
            let claim = self
                .state_store
                .begin_external_supervisor_activation(placement, &intent);
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

fn require_guest_package_artifact_floor_bytes(
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
mod guest_package_artifact_floor_tests {
    use super::require_guest_package_artifact_floor_bytes;

    #[test]
    fn refuses_impossible_signed_budget_before_allocation() {
        assert!(require_guest_package_artifact_floor_bytes(9, 4, 5).is_ok());
        assert!(require_guest_package_artifact_floor_bytes(8, 4, 5).is_err());
        assert!(require_guest_package_artifact_floor_bytes(u64::MAX, u64::MAX, 1).is_err());
    }
}

fn supervisor_activation(
    contract: &ExternalPlacementBackendContract,
    reservation: &ExternalAllocationReservation,
    occurrence: &ExternalAllocationOccurrence,
    authority: &ExternalChannelAuthority,
    program: &AdmittedExternalExecutionProgram,
    guest_inputs: ryeos_external_execution::guest_inputs::ExternalGuestInputAuthority,
    backend: &dyn ExternalPlacementBackend,
    package_parent: &lillux::PinnedDirectory,
    deadline: lillux::time::MonotonicDeadline,
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
        schema: 7,
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
            import_authorization,
            signed_assignment: None,
        },
    ))
}

#[allow(clippy::too_many_arguments)]
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

#[allow(clippy::too_many_arguments)]
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
    use crate::vault::{NodeVault as _, SealedEnvelopeVault};

    #[allow(clippy::type_complexity)]
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
    ) -> Result<()> {
        install_precontact_binding_with_settings(
            state,
            program,
            controller,
            tls_root_certificates_der_base64,
            serde_json::json!({"region":"composed-test", "plan":"bounded-fixture"}),
            b"fixture-secret",
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
        let now = i64::try_from(lillux::time::timestamp_millis())?;
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
            request_digest: "5".repeat(64),
            max_active: 1,
            timeout_seconds: 60,
            startup_started_at_ms: now,
            startup_deadline_ms: now
                .checked_add(60_000)
                .context("fixture startup deadline overflow")?,
            contact_deadline_ms: i64::try_from(lillux::time::timestamp_millis())?
                .checked_add(30_000)
                .context("ordinary external fixture contact deadline overflow")?,
        };
        let operator =
            crate::identity::NodeIdentity::load(&state.config.operator_signing_key_path)?
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
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn prepare_bound_external_supervisor_inner(
        state: &mut AppState,
        capsule: ryeos_state::objects::AdmittedPersistentSessionCapsule,
        base_snapshot_hash: String,
        guest_inputs: ryeos_external_execution_contract::ExternalGuestInputProjection,
        controller: ryeos_state::external_execution::transport::ExternalControllerTransportContract,
        tls_root_certificates_der_base64: Vec<String>,
        supervisor_artifact_hash: String,
        launcher_artifact_hash: String,
        workspace: Option<(&std::path::Path, &Arc<crate::temp_dir_guard::TempDirGuard>)>,
    ) -> Result<ryeos_state::external_execution::transport::ExternalSupervisorBootstrap> {
        capsule.validate()?;
        let program = capsule
            .external_candidate
            .clone()
            .context("composed external fixture capsule has no candidate program")?;
        program.verify_selections(capsule.retained_product_selections.as_ref())?;
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
        )?;
        let credential_access = retained.credential_access()?;
        vault.provision_placement_credential(
            &credential_access,
            &credential_access.test_value("fixture-secret"),
        )?;
        let contract = retained.backend_contract();
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
            "program":&program,
            "backend_contract":&contract,
        }))?;
        let now = i64::try_from(lillux::time::timestamp_millis())?;
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
            let operator =
                crate::identity::NodeIdentity::load(&state.config.operator_signing_key_path)?
                    .principal_id();
            state
                .state_store
                .install_external_placement_test_fixture_with_workspace_owner(
                    &reservation,
                    &retained,
                    project_root,
                    &operator,
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
            state
                .state_store
                .install_external_placement_test_fixture(&reservation, &retained)?;
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
            schema: 7,
            controller,
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
        let now = i64::try_from(lillux::time::timestamp_millis())?;
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
    struct FixtureBackend {
        artifact: String,
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
                schema: 6,
                protocol: ryeos_state::external_execution::admission::PROTOCOL.into(),
                required_lifecycle_capabilities: BTreeSet::new(),
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
        ryeos_state::external_execution::admission::AdmittedExternalCandidateProgram {
            requirement,
            qualification_use,
            runtime_manifest_kind: ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND.into(),
            runtime_manifest_hash: "b".repeat(64),
            runtime_witness_hash: "1".repeat(64),
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

    fn lifecycle_fixture(
        path: &std::path::Path,
    ) -> (
        crate::runtime_db::RuntimeDb,
        ExternalAllocationReservation,
        RetainedExternalExecutionBinding,
    ) {
        use crate::runtime_db::{
            DedicatedCandidateDisposition, NewCredentialProfile, NewDedicatedSession,
            WorkspaceBinding, WorkspaceState,
        };
        let db = crate::runtime_db::RuntimeDb::open(path).unwrap();
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
            db.claim_thread_launch("T-one", "claim-one", "daemon:external-placement-test")
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
        let now = i64::try_from(lillux::time::timestamp_millis()).unwrap();
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
            request_digest: "e".repeat(64),
            max_active: 1,
            timeout_seconds: 60,
            startup_started_at_ms: now,
            startup_deadline_ms: now + 60_000,
            contact_deadline_ms: i64::try_from(lillux::time::timestamp_millis()).unwrap() + 60_000,
        };
        db.reserve_external_allocation(&reservation, &binding)
            .unwrap();
        (db, reservation, binding)
    }

    fn placement_store_fixture() -> (
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
        Arc<crate::state_store::StateStore>,
        ExternalAllocationReservation,
        RetainedExternalExecutionBinding,
        Arc<StateLockLease>,
        std::path::PathBuf,
    ) {
        let root = tempfile::tempdir().unwrap().keep();
        let lock_path = crate::state_lock::default_lock_path(&root);
        let controller = crate::state_lock::StateLock::acquire(&lock_path).unwrap();
        let controller_lifetime = Arc::new(controller.retain());
        drop(controller);
        let state_dir = root.join(".ai/state");
        let identity = crate::identity::NodeIdentity::create(&root.join("node-key.pem")).unwrap();
        let signer = Arc::new(crate::state_store::NodeIdentitySigner::from_identity(
            &identity,
        ));
        let mut trust = ryeos_state::refs::TrustStore::new();
        trust.insert(identity.fingerprint().to_owned(), *identity.verifying_key());
        let store = Arc::new(
            crate::state_store::StateStore::new_with_head_trust(
                root,
                state_dir.clone(),
                state_dir.join("runtime.sqlite3"),
                signer,
                crate::write_barrier::WriteBarrier::new(),
                Arc::new(trust),
            )
            .unwrap(),
        );
        let binding = RetainedExternalExecutionBinding::test_fixture();
        let channel_authority = ExternalChannelAuthority::test_fixture(&"3".repeat(64));
        let now = i64::try_from(lillux::time::timestamp_millis()).unwrap();
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
            request_digest: "e".repeat(64),
            max_active: 1,
            timeout_seconds: 60,
            startup_started_at_ms: now,
            startup_deadline_ms: now + startup_ms,
            contact_deadline_ms: i64::try_from(lillux::time::timestamp_millis()).unwrap() + 60_000,
        };
        store
            .install_external_placement_test_fixture(&reservation, &binding)
            .unwrap();
        (store, reservation, binding, controller_lifetime, lock_path)
    }

    #[test]
    fn guest_assignment_is_signed_by_retained_node_for_only_the_bound_occurrence() {
        let (store, reservation, binding, _lifetime, lock_path) = placement_store_fixture();
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
            &package_parent,
            reservation.startup_deadline().unwrap(),
        )
        .unwrap();
        let signed = store
            .author_external_guest_assignment(&reservation, &occurrence, &intent, &contract)
            .unwrap();
        let root = lock_path.ancestors().nth(3).unwrap();
        let identity = crate::identity::NodeIdentity::load(&root.join("node-key.pem")).unwrap();
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
        let (store, reservation, _, _lifetime, _) = placement_store_fixture();
        let app_root = tempfile::tempdir().unwrap();
        let mut state = crate::state::test_support::build(app_root.path()).unwrap();
        state.state_store = store.clone();
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
        let (predecessor, reservation, _, predecessor_lifetime, lock_path) =
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

        let (reopened, _replacement_lifetime) = reopen_placement_store(&lock_path);
        let app_root = tempfile::tempdir().unwrap();
        let mut replacement = crate::state::test_support::build(app_root.path()).unwrap();
        replacement.state_store = reopened.clone();

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
    ) -> (Arc<crate::state_store::StateStore>, Arc<StateLockLease>) {
        let root = lock_path.ancestors().nth(3).unwrap().to_path_buf();
        assert_eq!(crate::state_lock::default_lock_path(&root), lock_path);
        let controller = crate::state_lock::StateLock::acquire(lock_path).unwrap();
        let replacement_lifetime = Arc::new(controller.retain());
        let identity = crate::identity::NodeIdentity::load(&root.join("node-key.pem")).unwrap();
        let signer = Arc::new(crate::state_store::NodeIdentitySigner::from_identity(
            &identity,
        ));
        let mut trust = ryeos_state::refs::TrustStore::new();
        trust.insert(identity.fingerprint().to_owned(), *identity.verifying_key());
        let state_dir = root.join(".ai/state");
        let reopened = Arc::new(
            crate::state_store::StateStore::new_with_head_trust(
                root,
                state_dir.clone(),
                state_dir.join("runtime.sqlite3"),
                signer,
                crate::write_barrier::WriteBarrier::new(),
                Arc::new(trust),
            )
            .unwrap(),
        );
        (reopened, replacement_lifetime)
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
        let (predecessor, reservation, binding, predecessor_lifetime, lock_path) =
            placement_store_fixture();
        drop(predecessor);
        drop(predecessor_lifetime);

        let (reopened, replacement_lifetime) = reopen_placement_store(&lock_path);
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
        let (predecessor, reservation, binding, predecessor_lifetime, lock_path) =
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

        let (reopened, replacement_lifetime) = reopen_placement_store(&lock_path);
        let app_root = tempfile::tempdir().unwrap();
        let mut replacement = crate::state::test_support::build(app_root.path()).unwrap();
        replacement.state_store = reopened.clone();
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
        .with_guest_inputs(guest_input_authority(
            guest_root,
            &reservation.base_snapshot_hash,
        ))
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
            },
        );
        let retained = ryeos_state::objects::RetainedExternalRuntimeQualification {
            binding_hash: binding.digest().to_owned(),
            guest_runtime_manifest_hash: contract.guest_runtime_manifest_hash.clone(),
            owner_principal,
            proof,
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
            let (store, reservation, binding, lifetime, _) = placement_store_fixture();
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
            let (store, reservation, binding, lifetime, _) = placement_store_fixture();
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
            let (store, reservation, binding, lifetime, _) = placement_store_fixture();
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
        for error in [connector, provider, backend] {
            assert!(
                error
                    .to_string()
                    .contains("cannot authorize a structured session")
            );
        }
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
        let now = i64::try_from(lillux::time::timestamp_millis()).unwrap();
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
            i64::try_from(lillux::time::timestamp_millis()).unwrap(),
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
            i64::try_from(lillux::time::timestamp_millis()).unwrap(),
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
        let (intent, activation) = supervisor_activation(
            &contract,
            &reservation,
            &occurrence,
            &authority,
            &AdmittedExternalExecutionProgram::StructuredSession(program()),
            guest_inputs,
            &FaultBackend::new(),
            &package_parent,
            reservation.startup_deadline().unwrap(),
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

        let db = crate::runtime_db::RuntimeDb::open(&path).unwrap();
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
            &package_parent,
            reservation.startup_deadline().unwrap(),
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

        let db = crate::runtime_db::RuntimeDb::open(&path).unwrap();
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

        let db = crate::runtime_db::RuntimeDb::open(&path).unwrap();
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
        let (store, reservation, binding, controller_lifetime, _) = placement_store_fixture();
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
        let (store, reservation, binding, controller_lifetime, _) = placement_store_fixture();
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
        let mut state = crate::state::test_support::build(app_root.path()).unwrap();
        state.state_store = store.clone();
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
    fn replacement_controller_fences_uncertain_activation_and_reconciles_exact_occurrence() {
        let (predecessor, reservation, binding, predecessor_lifetime, lock_path) =
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

        let (replacement_store, replacement_lifetime) = reopen_placement_store(&lock_path);
        let app_root = tempfile::tempdir().unwrap();
        let mut replacement = crate::state::test_support::build(app_root.path()).unwrap();
        replacement.state_store = replacement_store.clone();
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
        let (store, reservation, binding, lifetime, _) = placement_store_fixture();
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
            let (store, reservation, binding, lifetime, _) = placement_store_fixture();
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
        let (store, reservation, binding, controller_lifetime, _) = placement_store_fixture();
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
            let (store, reservation, binding, controller_lifetime, _) = placement_store_fixture();
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
        let (store, reservation, retained, controller_lifetime, _) = placement_store_fixture();
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
        let now = i64::try_from(lillux::time::timestamp_millis()).unwrap();
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
        let (store, reservation, binding, controller_lifetime, _) = placement_store_fixture();
        assert_eq!(
            classify_external_start_cleanup(&store, "T-one"),
            ExternalCandidateStartCleanup::Proved
        );
        assert_eq!(
            store.external_allocation("T-one").unwrap().unwrap().phase,
            ExternalAllocationPhase::NoContact
        );

        let root = tempfile::tempdir().unwrap().keep();
        let lock_path = crate::state_lock::default_lock_path(&root);
        let controller = crate::state_lock::StateLock::acquire(&lock_path).unwrap();
        let lifetime = Arc::new(controller.retain());
        drop(controller);
        let state_dir = root.join(".ai/state");
        let identity = crate::identity::NodeIdentity::create(&root.join("node-key.pem")).unwrap();
        let signer = Arc::new(crate::state_store::NodeIdentitySigner::from_identity(
            &identity,
        ));
        let mut trust = ryeos_state::refs::TrustStore::new();
        trust.insert(identity.fingerprint().to_owned(), *identity.verifying_key());
        let contacted = Arc::new(
            crate::state_store::StateStore::new_with_head_trust(
                root,
                state_dir.clone(),
                state_dir.join("runtime.sqlite3"),
                signer,
                crate::write_barrier::WriteBarrier::new(),
                Arc::new(trust),
            )
            .unwrap(),
        );
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

        drop(controller_lifetime);
    }

    #[test]
    fn allocation_reconciliation_refuses_an_active_contact_lease_without_contact() {
        let (store, reservation, binding, controller_lifetime, _) = placement_store_fixture();
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
            let (store, reservation, binding, controller_lifetime, _) = placement_store_fixture();
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
        let (store, reservation, binding, controller_lifetime, _) = placement_store_fixture();
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
        let (store, reservation, binding, controller_lifetime, _) = placement_store_fixture();
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
        let (store, reservation, binding, controller_lifetime, lock_path) =
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
        // bounded runtime shutdown. The in-flight permit is now the only
        // holder and must still exclude the replacement controller.
        drop(controller_lifetime);
        assert!(crate::state_lock::StateLock::acquire(&lock_path).is_err());

        drop(permit);
        crate::state_lock::StateLock::acquire(&lock_path)
            .expect("replacement remained excluded after the contact permit stopped");
    }
}
