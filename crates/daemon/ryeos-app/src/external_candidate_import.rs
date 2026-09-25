//! Process-local wake ownership for durable external candidate imports.
//!
//! The signed runtime transcript and receiver CAS remain authoritative. This
//! pool only prevents duplicate reconstruction work in one daemon generation;
//! losing it on restart is safe because a retried exact seal reconstructs from
//! the durable claimed prefix.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result, bail, ensure};

use crate::state_store::StateStore;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ImportKey {
    placement: String,
    seal_sequence: u64,
    seal_digest: String,
}

#[derive(Default)]
pub struct ExternalCandidateImportPool {
    /// Exact live owners. `true` records a wake that arrived while the owner
    /// was running, so the owner must re-read durable state before retiring.
    active: Mutex<BTreeMap<ImportKey, bool>>,
    changed: lillux::task::HostCondition,
    shutdown: AtomicBool,
    #[cfg(feature = "test-support")]
    last_failure: Mutex<Option<String>>,
}

impl ExternalCandidateImportPool {
    /// Discover exact eligible seals from the durable transcript and wake one
    /// owner for each. Supplying a placement is the ingress fast path; `None`
    /// is daemon-start/periodic recovery across all retained channels.
    pub fn wake_recoverable(
        self: &Arc<Self>,
        state_store: Arc<StateStore>,
        placement: Option<&str>,
    ) -> Result<usize> {
        let targets = state_store.recoverable_external_candidate_imports(placement)?;
        let mut started = 0_usize;
        for target in targets {
            started = started
                .checked_add(usize::from(self.wake(
                    Arc::clone(&state_store),
                    &target.placement,
                    target.seal_sequence,
                    &target.seal_digest,
                )?))
                .context("external candidate import wake count overflow")?;
        }
        Ok(started)
    }

    /// Wake one bounded reconstruction owner. Exact duplicate wakes coalesce;
    /// a different seal for the same placement is refused while ownership is
    /// live rather than executing two candidate imports concurrently.
    pub fn wake(
        self: &Arc<Self>,
        state_store: Arc<StateStore>,
        placement: &str,
        seal_sequence: u64,
        seal_digest: &str,
    ) -> Result<bool> {
        ensure!(
            !self.shutdown.load(Ordering::Acquire),
            "external candidate import pool is shutting down"
        );
        ensure!(
            seal_sequence > 0,
            "external candidate import seal sequence is zero"
        );
        ensure!(
            lillux::valid_hash(seal_digest)
                && !seal_digest.bytes().any(|byte| byte.is_ascii_uppercase()),
            "external candidate import seal digest is not canonical"
        );
        ryeos_runtime::validate_runtime_thread_id(placement)
            .map_err(anyhow::Error::msg)
            .context("external candidate import placement is not canonical")?;
        let key = ImportKey {
            placement: placement.to_owned(),
            seal_sequence,
            seal_digest: seal_digest.to_owned(),
        };
        let reconcile = {
            let state_store = Arc::clone(&state_store);
            Arc::new(move |key: &ImportKey| {
                let result = state_store.reconcile_external_candidate_import(
                    &key.placement,
                    key.seal_sequence,
                    &key.seal_digest,
                );
                crate::dedicated_session_service::notify_projection_change(&key.placement);
                result
            }) as Arc<dyn Fn(&ImportKey) -> Result<()> + Send + Sync>
        };
        self.wake_with(key, reconcile)
    }

    fn wake_with(
        self: &Arc<Self>,
        key: ImportKey,
        reconcile: Arc<dyn Fn(&ImportKey) -> Result<()> + Send + Sync>,
    ) -> Result<bool> {
        {
            let mut active = self
                .active
                .lock()
                .map_err(|_| anyhow::anyhow!("external candidate import pool poisoned"))?;
            ensure!(
                !self.shutdown.load(Ordering::Acquire),
                "external candidate import pool is shutting down"
            );
            if let Some(rerun) = active.get_mut(&key) {
                *rerun = true;
                return Ok(false);
            }
            if active.keys().any(|value| value.placement == key.placement) {
                bail!("external candidate import placement has a competing live seal");
            }
            active.insert(key.clone(), false);
        }

        let pool = Arc::clone(self);
        let thread_key = key.clone();
        let task_name = format!(
            "external-import-{}",
            &lillux::sha256_hex(key.placement.as_bytes())[..12]
        );
        let spawn = lillux::task::spawn_host_task(&task_name, move || {
            loop {
                let result = reconcile(&thread_key);
                match &result {
                    Ok(()) => tracing::info!(
                        placement = %thread_key.placement,
                        seal_sequence = thread_key.seal_sequence,
                        seal_digest = %thread_key.seal_digest,
                        "external candidate import retained"
                    ),
                    Err(error) => tracing::error!(
                        placement = %thread_key.placement,
                        seal_sequence = thread_key.seal_sequence,
                        seal_digest = %thread_key.seal_digest,
                        error = %error,
                        "external candidate import reconciliation failed"
                    ),
                }
                #[cfg(feature = "test-support")]
                {
                    *pool.last_failure.lock().expect("import diagnostic lock") =
                        result.as_ref().err().map(|error| format!("{error:#}"));
                }
                let Ok(mut active) = pool.active.lock() else {
                    return;
                };
                let rerun = active.get_mut(&thread_key).is_some_and(|rerun| {
                    let value = *rerun;
                    *rerun = false;
                    value
                });
                if rerun {
                    drop(active);
                    continue;
                }
                active.remove(&thread_key);
                pool.changed.notify_all();
                break;
            }
        });
        let task = match spawn {
            Ok(task) => task,
            Err(error) => {
                let mut active = self
                    .active
                    .lock()
                    .map_err(|_| anyhow::anyhow!("external candidate import pool poisoned"))?;
                active.remove(&key);
                self.changed.notify_all();
                return Err(error).context("spawn external candidate import owner");
            }
        };
        // This pool fences/waits on its active keys. The durable transcript
        // owns restart recovery; the host task handle is not execution evidence.
        task.detach();
        Ok(true)
    }

    /// Fence new wakes and wait for every already-owned reconstruction to
    /// release its CAS/state authority. A timeout is an unclean shutdown; the
    /// durable claimed transcript remains the next daemon's recovery input.
    pub fn shutdown_and_wait(&self, timeout: lillux::time::Duration) -> Result<usize> {
        let deadline = lillux::time::MonotonicDeadline::after(timeout);
        self.shutdown.store(true, Ordering::Release);
        let active = self
            .active
            .lock()
            .map_err(|_| anyhow::anyhow!("external candidate import pool poisoned"))?;
        let initial = active.len();
        let active = self
            .changed
            .wait_while_until(active, deadline, |active| !active.is_empty())
            .map_err(|_| anyhow::anyhow!("external candidate import pool poisoned"))?;
        ensure!(
            active.is_empty(),
            "external candidate import owners remain active at shutdown"
        );
        Ok(initial)
    }

    pub fn wait_for_idle(&self, timeout: lillux::time::Duration) -> Result<()> {
        let deadline = lillux::time::MonotonicDeadline::after(timeout);
        let active = self
            .active
            .lock()
            .map_err(|_| anyhow::anyhow!("external candidate import pool poisoned"))?;
        let active = self
            .changed
            .wait_while_until(active, deadline, |active| !active.is_empty())
            .map_err(|_| anyhow::anyhow!("external candidate import pool poisoned"))?;
        ensure!(
            active.is_empty(),
            "external candidate import owner did not become idle"
        );
        Ok(())
    }

    #[cfg(feature = "test-support")]
    pub fn last_failure_for_test(&self) -> Option<String> {
        self.last_failure
            .lock()
            .expect("import diagnostic lock")
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use ryeos_state::external_execution::{
        ChannelDirection, ExecutionChannelBinding, ExecutionChannelPayload, ExecutionFrame,
        ExecutionFrameApplication, ExportContentKind, NativeNamespaceExit,
        NativeWriterExclusionMechanism, NativeWriterExclusionObservation, SignedExecutionFrame,
    };
    use ryeos_state::objects::{ProjectFile, ProjectSnapshot, ProjectSnapshotPolicy, ProjectTree};
    use std::sync::Condvar;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    fn key(sequence: u64, digest_byte: char) -> ImportKey {
        ImportKey {
            placement: "T-external-import".into(),
            seal_sequence: sequence,
            seal_digest: digest_byte.to_string().repeat(64),
        }
    }

    #[test]
    fn exact_wake_coalesces_without_losing_a_wake_during_active_work() {
        let pool = Arc::new(ExternalCandidateImportPool::default());
        let entered = Arc::new((Mutex::new(false), Condvar::new()));
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let calls = Arc::new(AtomicUsize::new(0));
        let reconcile = {
            let entered = Arc::clone(&entered);
            let release = Arc::clone(&release);
            let calls = Arc::clone(&calls);
            Arc::new(move |_: &ImportKey| {
                let call = calls.fetch_add(1, Ordering::SeqCst);
                if call == 0 {
                    let (lock, changed) = &*entered;
                    *lock.lock().unwrap() = true;
                    changed.notify_all();
                    let (lock, changed) = &*release;
                    let guard = lock.lock().unwrap();
                    drop(changed.wait_while(guard, |released| !*released).unwrap());
                }
                Ok(())
            }) as Arc<dyn Fn(&ImportKey) -> Result<()> + Send + Sync>
        };
        let import = key(3, 'a');
        assert!(
            pool.wake_with(import.clone(), Arc::clone(&reconcile))
                .unwrap()
        );
        {
            let (lock, changed) = &*entered;
            let guard = lock.lock().unwrap();
            drop(changed.wait_while(guard, |entered| !*entered).unwrap());
        }
        assert!(!pool.wake_with(import, reconcile).unwrap());
        {
            let (lock, changed) = &*release;
            *lock.lock().unwrap() = true;
            changed.notify_all();
        }
        pool.wait_for_idle(Duration::from_secs(2)).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn competing_seal_is_refused_and_shutdown_fences_new_work() {
        let pool = Arc::new(ExternalCandidateImportPool::default());
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let reconcile = {
            let release = Arc::clone(&release);
            Arc::new(move |_: &ImportKey| {
                let (lock, changed) = &*release;
                let guard = lock.lock().unwrap();
                drop(changed.wait_while(guard, |released| !*released).unwrap());
                Ok(())
            }) as Arc<dyn Fn(&ImportKey) -> Result<()> + Send + Sync>
        };
        assert!(pool.wake_with(key(3, 'a'), reconcile).unwrap());
        assert!(
            pool.wake_with(key(4, 'b'), Arc::new(|_| Ok(())))
                .unwrap_err()
                .to_string()
                .contains("competing live seal")
        );
        assert!(pool.shutdown_and_wait(Duration::from_millis(1)).is_err());
        {
            let (lock, changed) = &*release;
            *lock.lock().unwrap() = true;
            changed.notify_all();
        }
        pool.wait_for_idle(Duration::from_secs(2)).unwrap();
        assert!(
            pool.wake_with(key(3, 'a'), Arc::new(|_| Ok(())))
                .unwrap_err()
                .to_string()
                .contains("shutting down")
        );
    }

    #[test]
    fn failed_reconciliation_releases_process_local_ownership() {
        let pool = Arc::new(ExternalCandidateImportPool::default());
        let import = key(3, 'a');
        assert!(
            pool.wake_with(import.clone(), Arc::new(|_| bail!("fixture failure")))
                .unwrap()
        );
        pool.wait_for_idle(Duration::from_secs(2)).unwrap();
        assert!(pool.wake_with(import, Arc::new(|_| Ok(()))).unwrap());
        pool.wait_for_idle(Duration::from_secs(2)).unwrap();
    }

    struct CandidateFixture {
        root: std::path::PathBuf,
        store: Arc<StateStore>,
        pool: Arc<ExternalCandidateImportPool>,
        binding: ExecutionChannelBinding,
        owner: lillux::crypto::SigningKey,
        supervisor: lillux::crypto::SigningKey,
        next_supervisor_sequence: u64,
        previous_supervisor_digest: String,
        quiesce_sequence: u64,
        quiesce_digest: String,
        completion_request_digest: String,
    }

    struct CandidateContent {
        payloads: Vec<ExecutionChannelPayload>,
        snapshot_hash: String,
        blob_hash: String,
        evidence_hash: String,
    }

    impl CandidateFixture {
        fn new() -> Self {
            use crate::node_config::sections::external_execution::RetainedExternalExecutionBinding;
            use crate::runtime_db::external_execution::{
                ExternalAllocationOccurrence, ExternalAllocationOwner,
                ExternalAllocationReservation, ExternalDedicatedSessionOwner,
                ExternalSupervisorActivationIntent, external_supervisor_activation_request_digest,
            };
            use crate::vault::external_channel::ExternalChannelAuthority;

            let root = tempfile::tempdir().unwrap().keep();
            let store = Arc::new(open_store(&root));
            let authority = store.external_candidate_test_authority();
            let guard = authority.acquire_shared_guard().unwrap();
            let cas = authority.cas_store().unwrap();
            let policy = ProjectSnapshotPolicy::new(
                ryeos_state::project_sync::ProjectSyncScope::FullProject,
                vec![],
                vec![],
                Default::default(),
            )
            .unwrap();
            let policy_hash = cas.store_object(&policy.to_value()).unwrap();
            let tree = ProjectTree {
                files: Default::default(),
            };
            let base = ProjectSnapshot {
                project_tree_hash: cas.store_object(&tree.to_value()).unwrap(),
                effective_policy_hash: policy_hash,
                parent_hashes: vec![],
                created_at: "2026-09-21T00:00:00Z".into(),
                message: None,
                source: "external-import-composed-test".into(),
            };
            let base_snapshot_hash = cas.store_object(&base.to_value()).unwrap();
            drop(guard);

            let retained = RetainedExternalExecutionBinding::test_fixture();
            let channel_authority = ExternalChannelAuthority::test_fixture(&"3".repeat(64));
            let owner = lillux::crypto::SigningKey::from_bytes(&[19; 32]);
            let supervisor = lillux::crypto::generate_signing_key();
            let now = i64::try_from(lillux::time::timestamp_millis()).unwrap();
            let startup_deadline =
                lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(60));
            let reservation = ExternalAllocationReservation {
                schema:
                    crate::runtime_db::external_execution::EXTERNAL_ALLOCATION_RESERVATION_SCHEMA,
                placement_thread_id: "T-external-import-composed".into(),
                admitted_capsule_hash: "a".repeat(64),
                owner: ExternalAllocationOwner::DedicatedSession(ExternalDedicatedSessionOwner {
                    workspace_id: "W-external-import-composed".into(),
                    worker_instance_id: "worker-external-import-composed".into(),
                    worker_boot_epoch: 1,
                }),
                base_snapshot_hash: base_snapshot_hash.clone(),
                binding_hash: retained.digest().into(),
                capacity_owner: retained.capacity_owner().into(),
                channel_authority_generation: "3".repeat(64),
                channel_owner_public_key: channel_authority.owner_public_key(),
                channel_bootstrap_capability_hash: channel_authority.bootstrap_capability_hash(),
                request_digest: "e".repeat(64),
                max_active: 1,
                timeout_seconds: 60,
                contact_deadline_ms: now + 60_000,
                startup_started_at_ms: now,
                startup_deadline_ms: now + 60_000,
            };
            store
                .install_external_placement_test_fixture(&reservation, &retained)
                .unwrap();
            store
                .claim_external_allocation_contact(
                    &reservation.placement_thread_id,
                    &reservation.request_digest,
                )
                .unwrap();
            let occurrence = ExternalAllocationOccurrence {
                schema: 1,
                binding_hash: reservation.binding_hash.clone(),
                request_digest: reservation.request_digest.clone(),
                occurrence_id: "external-import-occurrence".into(),
                provider_observation_digest: "f".repeat(64),
            };
            store
                .bind_external_allocation(
                    &reservation.placement_thread_id,
                    &occurrence,
                    crate::runtime_db::external_execution::ExternalObservationTiming::Startup {
                        deadline_exceeded: false,
                        live_deadline: startup_deadline,
                    },
                )
                .unwrap();
            let contract = retained.backend_contract();
            let attachment_deadline_ms = reservation.contact_deadline_ms
                + i64::from(contract.observation_timeout_seconds) * 1_000;
            let post_execution_timeout_seconds =
                contract.observation_timeout_seconds + contract.cleanup_timeout_seconds;
            let channel_max_bytes = contract.max_transfer_bytes.min(64 * 1024 * 1024);
            let guest_input_identity = "9".repeat(64);
            let activation_request_digest = external_supervisor_activation_request_digest(
                &reservation,
                &occurrence,
                &contract,
                attachment_deadline_ms,
                post_execution_timeout_seconds,
                channel_max_bytes,
                &guest_input_identity,
            )
            .unwrap();
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
                    .structured_session()
                    .unwrap()
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
            assert!(
                store
                    .begin_external_supervisor_activation(
                        &reservation.placement_thread_id,
                        &activation,
                    )
                    .unwrap()
            );
            let binding = ExecutionChannelBinding {
                schema: ryeos_state::external_execution::EXECUTION_CHANNEL_BINDING_SCHEMA,
                execution_mode:
                    ryeos_external_execution_contract::ExternalExecutionMode::StructuredSession {},
                placement_thread_id: reservation.placement_thread_id.clone(),
                allocation_request_digest: reservation.request_digest.clone(),
                occurrence_id: occurrence.occurrence_id,
                admitted_capsule_hash: reservation.admitted_capsule_hash,
                base_snapshot_hash,
                execution_binding_hash: reservation.binding_hash,
                supervisor_runtime_hash: activation.supervisor_runtime_hash,
                candidate_program_digest: "0".repeat(64),
                channel_nonce: "9".repeat(64),
                owner_public_key: STANDARD.encode(owner.verifying_key().as_bytes()),
                supervisor_public_key: STANDARD.encode(supervisor.verifying_key().as_bytes()),
                issued_at_ms: now,
                execution_deadline_ms: now + 60_000,
                expires_at_ms: now + 180_000,
                candidate_export_max_bytes: 512 * 1024,
                max_frames: 100,
                max_bytes: 4 * 1024 * 1024,
            };
            store.register_external_execution_channel(&binding).unwrap();
            let pool = Arc::new(ExternalCandidateImportPool::default());
            let (ready_wire, ready_digest) = signed_wire(
                &binding,
                &supervisor,
                ChannelDirection::SupervisorToOwner,
                1,
                None,
                0,
                ExecutionChannelPayload::Ready {
                    supervisor_runtime_hash: binding.supervisor_runtime_hash.clone(),
                    base_snapshot_hash: binding.base_snapshot_hash.clone(),
                },
            );
            crate::external_placement::exchange_external_supervisor_frame_and_wake_imports(
                &store,
                &pool,
                &binding.placement_thread_id,
                &ready_wire,
                &owner,
            )
            .unwrap();
            let release = store
                .admit_external_ready_and_author_release(
                    &binding.placement_thread_id,
                    &owner,
                    startup_deadline,
                )
                .unwrap()
                .unwrap();
            let (release_ack_wire, release_ack_digest) = signed_wire(
                &binding,
                &supervisor,
                ChannelDirection::SupervisorToOwner,
                2,
                Some(ready_digest),
                release.frame().sequence,
                ExecutionChannelPayload::Acknowledge {
                    peer_frame_sequence: release.frame().sequence,
                    peer_frame_digest: release.digest().to_owned(),
                    application: ExecutionFrameApplication::Applied,
                },
            );
            crate::external_placement::exchange_external_supervisor_frame_and_wake_imports(
                &store,
                &pool,
                &binding.placement_thread_id,
                &release_ack_wire,
                &owner,
            )
            .unwrap();
            let completion_request_digest = "8".repeat(64);
            let quiesce = store
                .author_external_owner_frame(
                    &binding.placement_thread_id,
                    &owner,
                    ExecutionChannelPayload::Quiesce {
                        completion_request_digest: completion_request_digest.clone(),
                    },
                )
                .unwrap();
            Self {
                root,
                store,
                pool,
                binding,
                owner,
                supervisor,
                next_supervisor_sequence: 3,
                previous_supervisor_digest: release_ack_digest,
                quiesce_sequence: quiesce.frame().sequence,
                quiesce_digest: quiesce.digest().to_owned(),
                completion_request_digest,
            }
        }

        fn exchange(&mut self, payload: ExecutionChannelPayload) -> (u64, String) {
            let sequence = self.next_supervisor_sequence;
            let (wire, digest) = signed_wire(
                &self.binding,
                &self.supervisor,
                ChannelDirection::SupervisorToOwner,
                sequence,
                Some(self.previous_supervisor_digest.clone()),
                self.quiesce_sequence,
                payload,
            );
            crate::external_placement::exchange_external_supervisor_frame_and_wake_imports(
                &self.store,
                &self.pool,
                &self.binding.placement_thread_id,
                &wire,
                &self.owner,
            )
            .unwrap();
            self.next_supervisor_sequence += 1;
            self.previous_supervisor_digest = digest.clone();
            (sequence, digest)
        }

        fn exchange_without_wake(&mut self, payload: ExecutionChannelPayload) -> (u64, String) {
            let sequence = self.next_supervisor_sequence;
            let (wire, digest) = signed_wire(
                &self.binding,
                &self.supervisor,
                ChannelDirection::SupervisorToOwner,
                sequence,
                Some(self.previous_supervisor_digest.clone()),
                self.quiesce_sequence,
                payload,
            );
            self.store
                .exchange_external_supervisor_frame(
                    &self.binding.placement_thread_id,
                    &wire,
                    &self.owner,
                    16,
                    1024 * 1024,
                )
                .unwrap();
            self.next_supervisor_sequence += 1;
            self.previous_supervisor_digest = digest.clone();
            (sequence, digest)
        }

        fn acknowledge_quiesce(&mut self, wake: bool) -> (u64, String) {
            let payload = ExecutionChannelPayload::Acknowledge {
                peer_frame_sequence: self.quiesce_sequence,
                peer_frame_digest: self.quiesce_digest.clone(),
                application: ExecutionFrameApplication::Applied,
            };
            if wake {
                self.exchange(payload)
            } else {
                self.exchange_without_wake(payload)
            }
        }
    }

    fn open_store(root: &std::path::Path) -> StateStore {
        let identity_path = root.join("node-key.pem");
        let identity = if identity_path.exists() {
            crate::identity::NodeIdentity::load(&identity_path).unwrap()
        } else {
            crate::identity::NodeIdentity::create(&identity_path).unwrap()
        };
        let signer = Arc::new(crate::state_store::NodeIdentitySigner::from_identity(
            &identity,
        ));
        let mut trust = ryeos_state::refs::TrustStore::new();
        trust.insert(identity.fingerprint().to_owned(), *identity.verifying_key());
        let state_dir = root.join(".ai/state");
        StateStore::new_with_head_trust(
            root.to_owned(),
            state_dir.clone(),
            state_dir.join("runtime.sqlite3"),
            signer,
            crate::write_barrier::WriteBarrier::new(),
            Arc::new(trust),
        )
        .unwrap()
    }

    fn signed_wire(
        binding: &ExecutionChannelBinding,
        key: &lillux::crypto::SigningKey,
        direction: ChannelDirection,
        sequence: u64,
        previous: Option<String>,
        acknowledged_peer_sequence: u64,
        payload: ExecutionChannelPayload,
    ) -> (Vec<u8>, String) {
        let signed = SignedExecutionFrame::sign(
            ExecutionFrame {
                schema: 1,
                binding_digest: binding.digest().unwrap(),
                direction,
                sequence,
                previous_frame_digest: previous,
                acknowledged_peer_sequence,
                payload,
            },
            binding,
            key,
        )
        .unwrap();
        let wire = lillux::canonical_json(&serde_json::to_value(signed).unwrap())
            .unwrap()
            .into_bytes();
        let digest = lillux::sha256_hex(&wire);
        (wire, digest)
    }

    fn candidate_content(fixture: &CandidateFixture) -> CandidateContent {
        let authority = fixture.store.external_candidate_test_authority();
        let cas = authority.cas_store().unwrap();
        let base: ProjectSnapshot = ProjectSnapshot::from_value(
            &cas.get_object(&fixture.binding.base_snapshot_hash)
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        let blob = b"candidate-controller-content".to_vec();
        let blob_hash = lillux::sha256_hex(&blob);
        let file = ProjectFile {
            blob_hash: blob_hash.clone(),
            size: blob.len() as u64,
            normalized_mode: ProjectFile::REGULAR_MODE,
        };
        let file_bytes = lillux::canonical_json(&file.to_value())
            .unwrap()
            .into_bytes();
        let file_hash = lillux::sha256_hex(&file_bytes);
        let tree = ProjectTree {
            files: [("candidate.txt".into(), file_hash.clone())]
                .into_iter()
                .collect(),
        };
        let tree_bytes = lillux::canonical_json(&tree.to_value())
            .unwrap()
            .into_bytes();
        let tree_hash = lillux::sha256_hex(&tree_bytes);
        let snapshot = ProjectSnapshot {
            project_tree_hash: tree_hash.clone(),
            effective_policy_hash: base.effective_policy_hash,
            parent_hashes: vec![fixture.binding.base_snapshot_hash.clone()],
            created_at: "2026-09-21T00:00:01Z".into(),
            message: None,
            source: "external-import-composed-test".into(),
        };
        let snapshot_bytes = lillux::canonical_json(&snapshot.to_value())
            .unwrap()
            .into_bytes();
        let snapshot_hash = lillux::sha256_hex(&snapshot_bytes);
        let evidence = NativeWriterExclusionObservation {
            schema: 1,
            binding_digest: fixture.binding.digest().unwrap(),
            base_snapshot_hash: fixture.binding.base_snapshot_hash.clone(),
            completion_request_digest: fixture.completion_request_digest.clone(),
            mechanism: NativeWriterExclusionMechanism::NamespaceInitReaped,
            exit: NativeNamespaceExit::Code(0),
        };
        let evidence_bytes = lillux::canonical_json(&serde_json::to_value(evidence).unwrap())
            .unwrap()
            .into_bytes();
        let evidence_hash = lillux::sha256_hex(&evidence_bytes);
        let payloads = [
            (
                ExportContentKind::Object,
                snapshot_hash.clone(),
                snapshot_bytes,
            ),
            (ExportContentKind::Object, tree_hash, tree_bytes),
            (ExportContentKind::Object, file_hash, file_bytes),
            (ExportContentKind::Blob, blob_hash.clone(), blob),
            (
                ExportContentKind::Blob,
                evidence_hash.clone(),
                evidence_bytes,
            ),
        ]
        .into_iter()
        .map(
            |(content_kind, object_hash, bytes)| ExecutionChannelPayload::ExportObjectChunk {
                content_kind,
                object_hash,
                offset: 0,
                bytes_base64: STANDARD.encode(bytes),
                final_chunk: true,
            },
        )
        .collect();
        CandidateContent {
            payloads,
            snapshot_hash,
            blob_hash,
            evidence_hash,
        }
    }

    fn send_candidate(
        fixture: &mut CandidateFixture,
        content: &CandidateContent,
        wake: bool,
    ) -> (u64, String) {
        for payload in content.payloads.clone() {
            if wake {
                fixture.exchange(payload);
            } else {
                fixture.exchange_without_wake(payload);
            }
        }
        let seal = ExecutionChannelPayload::ExportSealed {
            candidate_snapshot_hash: content.snapshot_hash.clone(),
            candidate_output_capture_hash: None,
            completion_request_digest: fixture.completion_request_digest.clone(),
            writer_exclusion_evidence_hash: content.evidence_hash.clone(),
        };
        if wake {
            fixture.exchange(seal)
        } else {
            fixture.exchange_without_wake(seal)
        }
    }

    #[test]
    fn production_exchange_wakes_pool_roots_candidate_and_revalidates_replay() {
        let mut fixture = CandidateFixture::new();
        let content = candidate_content(&fixture);
        let (seal_sequence, seal_digest) = send_candidate(&mut fixture, &content, true);
        assert!(
            fixture
                .store
                .recoverable_external_candidate_imports(Some(&fixture.binding.placement_thread_id))
                .unwrap()
                .is_empty()
        );
        fixture.acknowledge_quiesce(true);
        fixture.pool.wait_for_idle(Duration::from_secs(2)).unwrap();
        assert!(
            fixture
                .store
                .active_resume_snapshot_roots()
                .unwrap()
                .contains(&content.snapshot_hash)
        );
        assert!(
            fixture
                .store
                .external_execution_blob_roots()
                .unwrap()
                .contains(&content.evidence_hash)
        );
        fixture
            .store
            .reconcile_external_candidate_import(
                &fixture.binding.placement_thread_id,
                seal_sequence,
                &seal_digest,
            )
            .unwrap();

        let authority = fixture.store.external_candidate_test_authority();
        let cas = authority.cas_store().unwrap();
        std::fs::remove_file(lillux::shard_path(
            cas.root(),
            "blobs",
            &content.blob_hash,
            "",
        ))
        .unwrap();
        assert!(
            fixture
                .store
                .reconcile_external_candidate_import(
                    &fixture.binding.placement_thread_id,
                    seal_sequence,
                    &seal_digest,
                )
                .unwrap_err()
                .to_string()
                .contains("blob")
        );
    }

    #[test]
    fn claimed_import_recovers_after_reopen_with_partial_cas_prefix() {
        let mut fixture = CandidateFixture::new();
        let content = candidate_content(&fixture);
        let (seal_sequence, seal_digest) = send_candidate(&mut fixture, &content, false);
        fixture.acknowledge_quiesce(false);
        fixture
            .store
            .claim_external_candidate_import_test_fixture(
                &fixture.binding.placement_thread_id,
                seal_sequence,
                &seal_digest,
            )
            .unwrap();
        let authority = fixture.store.external_candidate_test_authority();
        let cas = authority.cas_store().unwrap();
        let ExecutionChannelPayload::ExportObjectChunk {
            object_hash,
            bytes_base64,
            ..
        } = &content.payloads[0]
        else {
            panic!("candidate fixture did not begin with an object")
        };
        let value: serde_json::Value =
            serde_json::from_slice(&STANDARD.decode(bytes_base64).unwrap()).unwrap();
        assert_eq!(cas.store_object(&value).unwrap(), *object_hash);
        drop(cas);
        let root = fixture.root.clone();
        let placement = fixture.binding.placement_thread_id.clone();
        drop(fixture);

        let store = Arc::new(open_store(&root));
        let pool = Arc::new(ExternalCandidateImportPool::default());
        assert_eq!(
            pool.wake_recoverable(Arc::clone(&store), Some(&placement))
                .unwrap(),
            1
        );
        pool.wait_for_idle(Duration::from_secs(2)).unwrap();
        assert!(
            store
                .active_resume_snapshot_roots()
                .unwrap()
                .contains(&content.snapshot_hash)
        );
    }

    #[test]
    fn malformed_staged_member_never_becomes_retained_candidate() {
        let mut fixture = CandidateFixture::new();
        let mut content = candidate_content(&fixture);
        let ExecutionChannelPayload::ExportObjectChunk { bytes_base64, .. } =
            &mut content.payloads[3]
        else {
            panic!("candidate fixture did not retain its candidate blob")
        };
        *bytes_base64 = STANDARD.encode(b"changed candidate bytes");
        let (seal_sequence, seal_digest) = send_candidate(&mut fixture, &content, true);
        fixture.acknowledge_quiesce(true);
        fixture.pool.wait_for_idle(Duration::from_secs(2)).unwrap();
        assert!(
            !fixture
                .store
                .active_resume_snapshot_roots()
                .unwrap()
                .contains(&content.snapshot_hash)
        );
        assert_eq!(
            fixture
                .store
                .recoverable_external_candidate_imports(Some(&fixture.binding.placement_thread_id))
                .unwrap(),
            [
                crate::runtime_db::external_execution::ExternalCandidateImportTarget {
                    placement: fixture.binding.placement_thread_id.clone(),
                    seal_sequence,
                    seal_digest,
                }
            ]
        );
    }

    #[test]
    fn cancellation_excludes_pending_import_but_not_claimed_reconstruction() {
        let mut pending = CandidateFixture::new();
        let pending_content = candidate_content(&pending);
        send_candidate(&mut pending, &pending_content, false);
        pending.acknowledge_quiesce(false);
        pending
            .store
            .author_external_owner_revocation(&pending.binding.placement_thread_id, &pending.owner)
            .unwrap();
        assert!(
            pending
                .store
                .recoverable_external_candidate_imports(Some(&pending.binding.placement_thread_id,))
                .unwrap()
                .is_empty()
        );

        let mut claimed = CandidateFixture::new();
        let claimed_content = candidate_content(&claimed);
        let (seal_sequence, seal_digest) = send_candidate(&mut claimed, &claimed_content, false);
        claimed.acknowledge_quiesce(false);
        claimed
            .store
            .claim_external_candidate_import_test_fixture(
                &claimed.binding.placement_thread_id,
                seal_sequence,
                &seal_digest,
            )
            .unwrap();
        claimed
            .store
            .author_external_owner_revocation(&claimed.binding.placement_thread_id, &claimed.owner)
            .unwrap();
        assert_eq!(
            claimed
                .pool
                .wake_recoverable(
                    Arc::clone(&claimed.store),
                    Some(&claimed.binding.placement_thread_id),
                )
                .unwrap(),
            1
        );
        claimed.pool.wait_for_idle(Duration::from_secs(2)).unwrap();
        assert!(
            claimed
                .store
                .active_resume_snapshot_roots()
                .unwrap()
                .contains(&claimed_content.snapshot_hash)
        );
    }
}
