//! Sole application-owned admission boundary for external candidate placement.
//!
//! Project/candidate inputs never select an endpoint, credential, account or
//! backend request. This owner rejoins the exact durable session and capsule to
//! one node-signed binding, an installed adapter artifact and a protected vault
//! generation before reserving capacity. Only the winner of the durable contact
//! claim receives a non-cloneable credential-bearing permit.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use anyhow::{Context, Result, bail, ensure};

#[cfg(test)]
use crate::node_config::sections::external_execution::RetainedExternalExecutionBinding;
use crate::node_config::sections::external_execution::{
    ExternalPlacementBackendContract, InstalledExternalExecutionBinding,
};
use crate::runtime_db::external_execution::{
    ExternalAllocationContactClaim, ExternalAllocationOccurrence, ExternalAllocationPhase,
    ExternalAllocationRecord, ExternalAllocationReservation, ExternalNoOccurrenceEvidence,
    ExternalTerminalObservation, ExternalTerminationIntent,
};
use crate::runtime_db::{WorkspaceRecord, WorkspaceState};
use crate::state::AppState;
use crate::state_lock::StateLockLease;
use crate::vault::placement::PlacementCredential;

/// Trusted controller adapter metadata and offline contract verification. This
/// method must not perform provider I/O; all provider mutation is fenced by the
/// later contact permit.
pub(crate) trait ExternalPlacementBackend: Send + Sync + std::fmt::Debug {
    fn backend_id(&self) -> &str;
    fn artifact_hash(&self) -> &str;
    fn qualify_offline(
        &self,
        contract: &ExternalPlacementBackendContract,
        credential: &PlacementCredential,
    ) -> Result<()>;

    /// The only allocator mutation. It is reachable solely by consuming the
    /// process-local contact permit after the durable contact CAS.
    fn allocate(
        &self,
        _contract: &ExternalPlacementBackendContract,
        _credential: &PlacementCredential,
        _reservation: &ExternalAllocationReservation,
    ) -> Result<ExternalAllocationResolution> {
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
    ) -> Result<ExternalAllocationResolution> {
        bail!("external placement backend does not implement allocation reconciliation")
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

#[derive(Debug, Default)]
pub struct ExternalPlacementBackendRegistry {
    backends: BTreeMap<(String, String), Arc<dyn ExternalPlacementBackend>>,
    contact_gates: Mutex<BTreeMap<String, Weak<AtomicBool>>>,
}

impl ExternalPlacementBackendRegistry {
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

/// Admission-only check used while sealing a session capsule. It performs no
/// provider I/O and grants no allocation/contact authority.
pub fn preflight_external_candidate_program(
    state: &AppState,
    program: &ryeos_state::external_execution::admission::AdmittedExternalCandidateProgram,
) -> Result<()> {
    let binding = select_binding(&state.node_config.external_execution, program)?;
    let contract = binding.backend_contract();
    let access = binding.credential_access()?;
    let credential = access.decode(
        state
            .vault
            .placement_credential(&access)
            .context("read protected external placement credential")?,
    )?;
    state
        .external_placement_backends
        .qualify(&contract, &credential)?;
    Ok(())
}

pub(crate) struct ExternalPlacementOwner<'a> {
    state: &'a AppState,
}

impl<'a> ExternalPlacementOwner<'a> {
    pub(crate) fn new(state: &'a AppState) -> Self {
        Self { state }
    }

    /// Prepare or exactly recover one placement. The placement id is an
    /// already-born dedicated session; no caller-authored allocation shape is
    /// accepted at this boundary.
    pub(crate) fn prepare(&self, placement: &str) -> Result<PreparedExternalPlacement> {
        let controller_lifetime = self
            .state
            .extensions
            .get::<StateLockLease>()
            .context("external placement controller has no retained state-lock lifetime")?;
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
        ensure!(
            thread.thread_id == placement
                && thread.chain_root_id == session.chain_root_id
                && thread.admitted_launch_capsule_hash.as_deref()
                    == Some(session.admitted_capsule_hash.as_str()),
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
        ensure!(
            workspace.workspace_id == session.workspace_id
                && workspace.thread_id.as_deref() == Some(placement)
                && workspace.launch_owner.as_deref() == Some("dedicated_worker_session")
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
        let credential = credential_access.decode(
            self.state
                .vault
                .placement_credential(&credential_access)
                .context("read protected external placement credential")?,
        )?;
        let backend = self
            .state
            .external_placement_backends
            .qualify(&contract, &credential)?;
        let contact_gate = self
            .state
            .external_placement_backends
            .contact_gate(placement)?;
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
            "program":program,
            "backend_contract":contract,
        }))?;
        let reservation = if let Some(existing) = existing {
            let expected = ExternalAllocationReservation {
                schema: 1,
                placement_thread_id: placement.to_owned(),
                admitted_capsule_hash: session.admitted_capsule_hash.clone(),
                workspace_id: session.workspace_id.clone(),
                worker_instance_id: worker_instance_id.to_owned(),
                worker_boot_epoch,
                base_snapshot_hash: workspace.base_snapshot.clone(),
                binding_hash: binding.digest().to_owned(),
                capacity_owner: binding.capacity_owner().to_owned(),
                request_digest,
                max_active: contract.max_active,
                timeout_seconds: contract.timeout_seconds,
                contact_deadline_ms: existing.reservation.contact_deadline_ms,
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
                schema: 1,
                placement_thread_id: placement.to_owned(),
                admitted_capsule_hash: session.admitted_capsule_hash.clone(),
                workspace_id: session.workspace_id.clone(),
                worker_instance_id: worker_instance_id.to_owned(),
                worker_boot_epoch,
                base_snapshot_hash: workspace.base_snapshot.clone(),
                binding_hash: binding.digest().to_owned(),
                capacity_owner: binding.capacity_owner().to_owned(),
                request_digest,
                max_active: contract.max_active,
                timeout_seconds: contract.timeout_seconds,
                contact_deadline_ms,
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
            record,
        })
    }
}

fn require_fresh_contact_owner(
    session: &crate::runtime_db::DedicatedSessionRecord,
    workspace: &WorkspaceRecord,
) -> Result<()> {
    ensure!(
        session.state == "admitted"
            && session.send_boundary == "none"
            && workspace.state == WorkspaceState::Ready,
        "new external contact requires an unreleased admitted session and ready workspace"
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
    record: ExternalAllocationRecord,
}

impl PreparedExternalPlacement {
    /// Consume preparation and durably decide whether this process owns the
    /// one allowed allocator call. A false claim is reconciliation authority,
    /// never permission to allocate again.
    pub(crate) fn claim(self) -> Result<ExternalPlacementContactDecision> {
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

impl ExternalContactLease {
    fn release(&self) {
        self.gate.store(false, Ordering::Release);
    }
}

// Access remains inside this module until a lifecycle adapter consumes these
// exact types. Keeping the fields live now prevents a future adapter from
// reconstructing authority from public hashes or caller input.
impl ExternalPlacementContactPermit {
    pub(crate) fn contact(self) -> Result<ExternalAllocationRecord> {
        let resolution =
            self.backend
                .allocate(&self.contract, &self.credential, &self.reservation)?;
        // The consuming call has returned, so this process can no longer issue
        // the delayed original request. Release before accepting an exact
        // negative response as settlement evidence.
        self.contact_lease.release();
        apply_reconciliation_resolution(
            &self.state_store,
            &self.contact_lease.gate,
            &self.reservation,
            resolution,
        )
    }
}

impl ExternalPlacementReconciliation {
    pub(crate) fn reconcile(self) -> Result<ExternalAllocationRecord> {
        let resolution = self.backend.reconcile_allocation(
            &self.contract,
            &self.credential,
            &self.record.reservation,
        )?;
        apply_reconciliation_resolution(
            &self.state_store,
            &self.contact_gate,
            &self.record.reservation,
            resolution,
        )
    }

    /// Start the exact termination request once, or reconcile it after
    /// restart. The durable intent is committed before the adapter mutation.
    pub(crate) fn terminate_or_reconcile(self) -> Result<ExternalAllocationRecord> {
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

fn apply_allocation_resolution(
    state_store: &crate::state_store::StateStore,
    reservation: &ExternalAllocationReservation,
    resolution: ExternalAllocationResolution,
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
        ExternalAllocationResolution::Pending => {}
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
        other => return apply_allocation_resolution(state_store, reservation, other),
    }
    state_store
        .external_allocation(&reservation.placement_thread_id)?
        .context("external allocation disappeared after lifecycle observation")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Debug)]
    struct FixtureBackend {
        artifact: String,
    }

    #[derive(Debug)]
    struct FaultBackend {
        artifact: String,
        allocate_calls: AtomicUsize,
        allocation_observations: AtomicUsize,
        terminate_calls: AtomicUsize,
        termination_observations: AtomicUsize,
    }

    impl FaultBackend {
        fn new() -> Self {
            Self {
                artifact: "d".repeat(64),
                allocate_calls: AtomicUsize::new(0),
                allocation_observations: AtomicUsize::new(0),
                terminate_calls: AtomicUsize::new(0),
                termination_observations: AtomicUsize::new(0),
            }
        }
    }

    impl ExternalPlacementBackend for FaultBackend {
        fn backend_id(&self) -> &str {
            "fixture"
        }

        fn artifact_hash(&self) -> &str {
            &self.artifact
        }

        fn qualify_offline(
            &self,
            _contract: &ExternalPlacementBackendContract,
            _credential: &PlacementCredential,
        ) -> Result<()> {
            Ok(())
        }

        fn allocate(
            &self,
            _contract: &ExternalPlacementBackendContract,
            _credential: &PlacementCredential,
            _reservation: &ExternalAllocationReservation,
        ) -> Result<ExternalAllocationResolution> {
            self.allocate_calls.fetch_add(1, Ordering::SeqCst);
            bail!("fixture lost the create response after provider mutation")
        }

        fn reconcile_allocation(
            &self,
            _contract: &ExternalPlacementBackendContract,
            _credential: &PlacementCredential,
            _reservation: &ExternalAllocationReservation,
        ) -> Result<ExternalAllocationResolution> {
            self.allocation_observations.fetch_add(1, Ordering::SeqCst);
            Ok(ExternalAllocationResolution::Bound {
                occurrence_id: "fixture-occurrence".into(),
                provider_observation_digest: "f".repeat(64),
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
            Ok(ExternalTerminationResolution::Terminal {
                provider_observation_digest: "2".repeat(64),
            })
        }
    }

    impl ExternalPlacementBackend for FixtureBackend {
        fn backend_id(&self) -> &str {
            "fixture"
        }

        fn artifact_hash(&self) -> &str {
            &self.artifact
        }

        fn qualify_offline(
            &self,
            contract: &ExternalPlacementBackendContract,
            credential: &PlacementCredential,
        ) -> Result<()> {
            ensure!(
                contract.region == "fixture-region"
                    && contract.plan == "fixture-plan"
                    && contract.network_policy
                        == "supervisor_pinned_owner_only_candidate_denied_v1"
                    && contract.storage_policy == "ephemeral_private_candidate_v1"
                    && contract.cleanup_proof == "provider_terminal_occurrence_v1"
                    && credential.generation() == "a".repeat(64)
                    && credential.secret() == "fixture-secret",
                "fixture backend received widened or wrong authority"
            );
            Ok(())
        }
    }

    fn program() -> ryeos_state::external_execution::admission::AdmittedExternalCandidateProgram {
        ryeos_state::external_execution::admission::AdmittedExternalCandidateProgram {
            requirement: ryeos_state::external_execution::admission::ExternalCandidateRequirement {
                schema: 1,
                protocol: ryeos_state::external_execution::admission::PROTOCOL.into(),
                runtime_product_declaration_id: "runtime".into(),
            },
            runtime_manifest_hash: "b".repeat(64),
            runtime_witness_hash: "1".repeat(64),
            qualification_attestation_hash: "2".repeat(64),
            selection_identity_digest: "c".repeat(64),
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
        db.reserve_workspace("W-one", &"b".repeat(64), "/fixture")
            .unwrap();
        db.transition_workspace(
            "W-one",
            &[WorkspaceState::Reserved],
            WorkspaceState::Constructing,
            None,
        )
        .unwrap();
        db.claim_workspace_construction("W-one", "T-one", "dedicated_worker_session")
            .unwrap();
        db.bind_workspace(WorkspaceBinding {
            workspace_id: "W-one",
            thread_id: "T-one",
            launch_owner: Some("dedicated_worker_session"),
            backend_id: Some("fixture"),
            backend_version: Some("fixture"),
            pinned_root_identities: Some("fixture"),
            mount_identity: Some("fixture"),
            workspace_output_partition_identity: None,
            base_output_capture_hash: None,
        })
        .unwrap();
        let binding = RetainedExternalExecutionBinding::test_fixture();
        let reservation = ExternalAllocationReservation {
            schema: 1,
            placement_thread_id: "T-one".into(),
            admitted_capsule_hash: "a".repeat(64),
            workspace_id: "W-one".into(),
            worker_instance_id: "worker-one".into(),
            worker_boot_epoch: 1,
            base_snapshot_hash: "b".repeat(64),
            binding_hash: binding.digest().into(),
            capacity_owner: binding.capacity_owner().into(),
            request_digest: "e".repeat(64),
            max_active: 1,
            timeout_seconds: 60,
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
        let reservation = ExternalAllocationReservation {
            schema: 1,
            placement_thread_id: "T-one".into(),
            admitted_capsule_hash: "a".repeat(64),
            workspace_id: "W-one".into(),
            worker_instance_id: "worker-one".into(),
            worker_boot_epoch: 1,
            base_snapshot_hash: "b".repeat(64),
            binding_hash: binding.digest().into(),
            capacity_owner: binding.capacity_owner().into(),
            request_digest: "e".repeat(64),
            max_active: 1,
            timeout_seconds: 60,
            contact_deadline_ms: i64::try_from(lillux::time::timestamp_millis()).unwrap() + 60_000,
        };
        store
            .install_external_placement_test_fixture(&reservation, &binding)
            .unwrap();
        (store, reservation, binding, controller_lifetime, lock_path)
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
            record: store
                .external_allocation(&reservation.placement_thread_id)
                .unwrap()
                .unwrap(),
        }
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
            .qualify(&selected.backend_contract(), &credential)
            .unwrap();

        let wrong_artifact =
            ExternalPlacementBackendRegistry::from_backends(vec![Arc::new(FixtureBackend {
                artifact: "e".repeat(64),
            })])
            .unwrap();
        assert!(
            wrong_artifact
                .qualify(&selected.backend_contract(), &credential)
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
            .qualify(&binding.backend_contract(), &credential)
            .unwrap();
        let mut newer = binding.backend_contract();
        newer.backend_artifact_hash = "e".repeat(64);
        rotated.qualify(&newer, &credential).unwrap();
    }

    #[test]
    fn ambiguous_provider_mutations_reconcile_without_duplicate_contact() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime.sqlite3");
        let (db, reservation, binding) = lifecycle_fixture(&path);
        let backend = FaultBackend::new();
        let contract = binding.backend_contract();
        let credential = credential(&InstalledExternalExecutionBinding::test_fixture());

        assert!(matches!(
            db.claim_external_allocation_contact("T-one", &reservation.request_digest)
                .unwrap(),
            ExternalAllocationContactClaim::Contact(_)
        ));
        assert!(
            backend
                .allocate(&contract, &credential, &reservation)
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
            .reconcile_allocation(&contract, &credential, &reservation)
            .unwrap()
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
        db.bind_external_allocation("T-one", &occurrence).unwrap();
        assert_eq!(backend.allocate_calls.load(Ordering::SeqCst), 1);
        assert_eq!(backend.allocation_observations.load(Ordering::SeqCst), 1);

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
        assert!(permit.contact().is_err());
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
            recovery.reconcile().unwrap().phase,
            ExternalAllocationPhase::Bound
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
            panic!("bound occurrence did not retain cleanup authority");
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
        assert_eq!(backend.terminate_calls.load(Ordering::SeqCst), 1);
        assert_eq!(backend.termination_observations.load(Ordering::SeqCst), 1);
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
            )
            .unwrap()
            .phase,
            ExternalAllocationPhase::ContactedNoOccurrence
        );
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
