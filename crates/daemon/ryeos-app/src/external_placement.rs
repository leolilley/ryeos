//! Sole application-owned admission boundary for external candidate placement.
//!
//! Project/candidate inputs never select an endpoint, credential, account or
//! backend request. This owner rejoins the exact durable session and capsule to
//! one node-signed binding, an installed adapter artifact and a protected vault
//! generation before reserving capacity. Only the winner of the durable contact
//! claim receives a non-cloneable credential-bearing permit.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context, Result, bail, ensure};

use crate::node_config::sections::external_execution::{
    ExternalPlacementBackendContract, InstalledExternalExecutionBinding,
    RetainedExternalExecutionBinding,
};
use crate::runtime_db::external_execution::{
    ExternalAllocationContactClaim, ExternalAllocationPhase, ExternalAllocationRecord,
    ExternalAllocationReservation,
};
use crate::runtime_db::{WorkspaceRecord, WorkspaceState};
use crate::state::AppState;
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
}

#[derive(Debug, Default)]
pub struct ExternalPlacementBackendRegistry {
    backends: BTreeMap<(String, String), Arc<dyn ExternalPlacementBackend>>,
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
        Ok(Self { backends: by_id })
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
                existing.phase != ExternalAllocationPhase::NoContact,
                "settled no-contact placement has no recovery authority"
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
            binding,
            contract,
            credential,
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
    binding: RetainedExternalExecutionBinding,
    contract: ExternalPlacementBackendContract,
    credential: PlacementCredential,
    record: ExternalAllocationRecord,
}

impl PreparedExternalPlacement {
    /// Consume preparation and durably decide whether this process owns the
    /// one allowed allocator call. A false claim is reconciliation authority,
    /// never permission to allocate again.
    pub(crate) fn claim(self) -> Result<ExternalPlacementContactDecision> {
        let claim = self.state_store.claim_external_allocation_contact(
            &self.record.reservation.placement_thread_id,
            &self.record.reservation.request_digest,
        )?;
        match claim {
            ExternalAllocationContactClaim::Contact(record) => Ok(
                ExternalPlacementContactDecision::Contact(ExternalPlacementContactPermit {
                    backend: self.backend,
                    binding: self.binding,
                    contract: self.contract,
                    credential: self.credential,
                    reservation: record.reservation,
                }),
            ),
            ExternalAllocationContactClaim::Reconcile(record) => Ok(
                ExternalPlacementContactDecision::Reconcile(ExternalPlacementReconciliation {
                    backend: self.backend,
                    binding: self.binding,
                    contract: self.contract,
                    credential: self.credential,
                    record,
                }),
            ),
            ExternalAllocationContactClaim::NoContact(record) => {
                Ok(ExternalPlacementContactDecision::NoContact(record))
            }
        }
    }
}

pub(crate) enum ExternalPlacementContactDecision {
    Contact(ExternalPlacementContactPermit),
    Reconcile(ExternalPlacementReconciliation),
    NoContact(ExternalAllocationRecord),
}

/// Process-local, non-cloneable and non-serializable proof of the single
/// durable contact claim. Provider adapters consume it; workers never see it.
pub(crate) struct ExternalPlacementContactPermit {
    backend: Arc<dyn ExternalPlacementBackend>,
    binding: RetainedExternalExecutionBinding,
    contract: ExternalPlacementBackendContract,
    credential: PlacementCredential,
    reservation: ExternalAllocationReservation,
}

/// Existing or uncertain contact must be observed through this exact retained
/// authority. It deliberately has no conversion into a new contact permit.
pub(crate) struct ExternalPlacementReconciliation {
    backend: Arc<dyn ExternalPlacementBackend>,
    binding: RetainedExternalExecutionBinding,
    contract: ExternalPlacementBackendContract,
    credential: PlacementCredential,
    record: ExternalAllocationRecord,
}

// Access remains inside this module until a lifecycle adapter consumes these
// exact types. Keeping the fields live now prevents a future adapter from
// reconstructing authority from public hashes or caller input.
impl ExternalPlacementContactPermit {
    pub(crate) fn backend(&self) -> &Arc<dyn ExternalPlacementBackend> {
        &self.backend
    }
    pub(crate) fn binding(&self) -> &RetainedExternalExecutionBinding {
        &self.binding
    }
    pub(crate) fn contract(&self) -> &ExternalPlacementBackendContract {
        &self.contract
    }
    pub(crate) fn credential(&self) -> &PlacementCredential {
        &self.credential
    }
    pub(crate) fn reservation(&self) -> &ExternalAllocationReservation {
        &self.reservation
    }
}

impl ExternalPlacementReconciliation {
    pub(crate) fn backend(&self) -> &Arc<dyn ExternalPlacementBackend> {
        &self.backend
    }
    pub(crate) fn binding(&self) -> &RetainedExternalExecutionBinding {
        &self.binding
    }
    pub(crate) fn contract(&self) -> &ExternalPlacementBackendContract {
        &self.contract
    }
    pub(crate) fn credential(&self) -> &PlacementCredential {
        &self.credential
    }
    pub(crate) fn record(&self) -> &ExternalAllocationRecord {
        &self.record
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct FixtureBackend {
        artifact: String,
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
}
