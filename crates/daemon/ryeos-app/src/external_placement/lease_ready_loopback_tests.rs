//! A leased guest's admission end to end, in one process.
//!
//! Workload: a placed session holding a remote lease, a controller over a
//! real runtime store, and a guest. The controller side is the real node:
//! the lease reservation and its record, activation, the attached channel,
//! the authentication and exchange functions every carrier calls, the readiness
//! gate, the journal and the owner's Release authoring. The guest side is the
//! real serialized supervisor in lease mode over its own guest journal, with a
//! launcher that only counts releases. The lifecycle adapter is scripted: it
//! reads the readiness request the controller composed and the two frames it
//! held, and answers admitted or terminate. Frames cross between the two sides
//! as the exact signed bytes a carrier would move; no socket is involved.

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

use ryeos_external_execution::supervisor::{
    CandidateExecutionOutput, ExternalCandidateLauncherClient, LeaseReadyPlan,
    SerializedExternalCandidateSupervisor,
};
use ryeos_node_runtime::node_policy::sections::execution::ExternalLeaseBudget;
use ryeos_state::external_execution::guest_journal::{
    AuthenticatedLauncherReady, DurableNativeCandidateCapture, LauncherOccurrenceEvidence,
    PreparedGuestJournal,
};
use ryeos_state::external_execution::{
    AuthenticatedExecutionFrame, ChannelDirection, EXECUTION_CHANNEL_BINDING_SCHEMA,
    ExecutionChannelBinding, ExecutionChannelPayload, SignedExecutionFrame,
};

use crate::runtime_db::external_execution::{
    ExternalLeaseActivation, ExternalLeaseCharge, ExternalLeaseRecord, ExternalLeaseRecordState,
};

const PLACEMENT: &str = "T-one";
const OCCURRENCE: &str = "lease-one";
const AUTHORITY_GENERATION_DIGIT: &str = "3";

/// What the scripted adapter answers to a readiness request.
#[derive(Debug, Clone)]
enum Verdict {
    Admit,
    Terminate(&'static str),
}

/// One readiness judgement the adapter was asked for.
struct Judged {
    request: Vec<u8>,
    ready_frame: Vec<u8>,
    report_frame: Vec<u8>,
}

/// The scripted lifecycle adapter behind the installed backend.
struct AdapterScript {
    verdict: Mutex<Verdict>,
    judged: Mutex<Vec<Judged>>,
}

impl AdapterScript {
    fn new(verdict: Verdict) -> Arc<Self> {
        Arc::new(Self {
            verdict: Mutex::new(verdict),
            judged: Mutex::new(Vec::new()),
        })
    }

    fn judgements(&self) -> usize {
        self.judged.lock().unwrap().len()
    }
}

/// The fixture backend plus a readiness verdict taken from the script.
#[derive(Debug)]
struct ScriptedBackend {
    inner: super::tests::FixtureBackend,
    script: Arc<AdapterScript>,
}

impl std::fmt::Debug for AdapterScript {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("AdapterScript")
    }
}

impl ExternalPlacementBackend for ScriptedBackend {
    fn lifecycle_capabilities(&self) -> BTreeSet<LifecycleCapability> {
        self.inner.lifecycle_capabilities()
    }

    fn backend_id(&self) -> &str {
        self.inner.backend_id()
    }

    fn artifact_hash(&self) -> &str {
        self.inner.artifact_hash()
    }

    fn artifact_bytes(&self) -> u64 {
        self.inner.artifact_bytes()
    }

    fn supervisor_artifact(&self) -> (&str, u64) {
        self.inner.supervisor_artifact()
    }

    fn launcher_artifact(&self) -> (&str, u64) {
        self.inner.launcher_artifact()
    }

    fn settings_schema_digest(&self) -> &str {
        self.inner.settings_schema_digest()
    }

    fn qualify_offline(
        &self,
        contract: &ExternalPlacementBackendContract,
        credential: &PlacementCredential,
    ) -> Result<()> {
        self.inner.qualify_offline(contract, credential)
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
        self.inner.prepare_guest_package(
            contract,
            reservation,
            occurrence,
            bootstrap,
            guest_inputs,
            activation_request_digest,
            parent,
            deadline,
        )
    }

    fn await_lease_ready(
        &self,
        request: &[u8],
        ready_frame: &[u8],
        report_frame: &[u8],
        _deadline: lillux::time::MonotonicDeadline,
    ) -> Result<Vec<u8>> {
        self.script.judged.lock().unwrap().push(Judged {
            request: request.to_vec(),
            ready_frame: ready_frame.to_vec(),
            report_frame: report_frame.to_vec(),
        });
        let parsed: serde_json::Value = serde_json::from_slice(request)?;
        let lease_id = parsed["expectation"]["lease_id"]
            .as_str()
            .context("the readiness request names no lease")?;
        let schema = parsed["schema"].clone();
        let verdict = match &*self.script.verdict.lock().unwrap() {
            Verdict::Admit => serde_json::json!({
                "verdict": "admitted",
                "schema": schema,
                "lease_id": lease_id,
                "admission_request_sha256": parsed["admission_request_sha256"],
                "boot_id": "scripted-boot",
                "ready_frame_digest": lillux::sha256_hex(ready_frame),
                "report_frame_digest": lillux::sha256_hex(report_frame),
                "met_by": {},
                "channel": {"chain": 1},
            }),
            Verdict::Terminate(reason) => serde_json::json!({
                "verdict": "terminate",
                "schema": schema,
                "lease_id": lease_id,
                "reason": reason,
            }),
        };
        Ok(serde_json::to_vec(&verdict)?)
    }
}

/// A launcher that counts how many times the candidate was released.
struct CountingLauncher {
    releases: Arc<AtomicUsize>,
}

impl ExternalCandidateLauncherClient for CountingLauncher {
    fn release(&mut self, _frame: &AuthenticatedExecutionFrame) -> Result<()> {
        self.releases.fetch_add(1, AtomicOrdering::SeqCst);
        Ok(())
    }

    fn poll_applied_launch(&mut self) -> Result<Option<lillux::LinuxSandboxAppliedLaunchReceipt>> {
        Ok(None)
    }

    fn apply_protocol_chunk(&mut self, _frame: &AuthenticatedExecutionFrame) -> Result<usize> {
        bail!("the loopback launcher admits no protocol input")
    }

    fn cancel(&mut self, _frame: &AuthenticatedExecutionFrame) -> Result<()> {
        bail!("the loopback launcher admits no cancellation")
    }

    fn complete_qualification(
        &mut self,
        _frame: &AuthenticatedExecutionFrame,
    ) -> Result<ryeos_external_execution::backends::linux::NativeQualificationCompletion> {
        bail!("the loopback launcher admits no qualification completion")
    }

    fn capture(
        &mut self,
        _frame: &AuthenticatedExecutionFrame,
    ) -> Result<DurableNativeCandidateCapture> {
        bail!("the loopback launcher captures nothing")
    }

    fn acknowledge_finish(&mut self, _frame_digest: &str) -> Result<()> {
        Ok(())
    }

    fn poll_execution_output(&mut self) -> Result<CandidateExecutionOutput> {
        Ok(CandidateExecutionOutput::Idle)
    }
}

/// The guest: the real supervisor in lease mode over its own guest journal,
/// on the test's thread because the test native host admits one thread at a
/// time.
struct Guest {
    runtime: Option<SerializedExternalCandidateSupervisor<CountingLauncher>>,
    releases: Arc<AtomicUsize>,
    binding: ExecutionChannelBinding,
    ready: Vec<u8>,
    report: Vec<u8>,
    // Field order is drop order: the journal retires in `Drop`, then the
    // directories under it.
    _journal_root: tempfile::TempDir,
}

impl Guest {
    /// Start a guest on `binding` whose Ready names `verified_inputs`; its
    /// two first frames are authored now and ready to send.
    fn start(
        binding: ExecutionChannelBinding,
        supervisor_key: &lillux::crypto::SigningKey,
        verified_inputs: Vec<String>,
        authority: &ryeos_state::PinnedStateAuthority,
    ) -> Self {
        let journal_root = tempfile::tempdir().unwrap();
        // The controller's store holds this thread's native host, which admits
        // no second State, so the guest's journal opens on the same host under
        // the controller store's state authority.
        let live = crate::state_store::test_native::with_store_host(|host| {
            Self::open_journal(host, authority, journal_root.path(), &binding)
        });
        let releases = Arc::new(AtomicUsize::new(0));
        let runtime = SerializedExternalCandidateSupervisor::new(
            live,
            CountingLauncher {
                releases: Arc::clone(&releases),
            },
        )
        .with_lease_ready(LeaseReadyPlan::observe(verified_inputs).unwrap());
        let ready = runtime
            .publish_ready(supervisor_key)
            .unwrap()
            .canonical()
            .as_bytes()
            .to_vec();
        let report = runtime
            .author_capability_report(supervisor_key)
            .unwrap()
            .expect("a leased guest authors a capability report")
            .canonical()
            .as_bytes()
            .to_vec();
        Self {
            runtime: Some(runtime),
            releases,
            binding,
            ready,
            report,
            _journal_root: journal_root,
        }
    }

    /// The guest's live journal for `binding`, opened on `host`.
    fn open_journal(
        host: &lillux_sqlite::test_host::TestNativeHost,
        authority: &ryeos_state::PinnedStateAuthority,
        journal_root: &std::path::Path,
        binding: &ExecutionChannelBinding,
    ) -> ryeos_state::external_execution::guest_journal::LiveGuestJournal {
        let journal_directory = lillux::PinnedDirectory::open(journal_root)
            .unwrap()
            .unwrap();
        journal_directory.tighten_owner_private_directory().unwrap();
        let prepared = PreparedGuestJournal::create(
            journal_directory,
            authority,
            &"f".repeat(64),
            binding.clone(),
            host.factory(),
            host.destination(),
            &ryeos_state::external_execution::native_journal::testing::census(host),
        )
        .unwrap();
        let occurrence = LauncherOccurrenceEvidence::from_held_launcher(
            lillux::ExactProcessIdentity {
                boot_id: "loopback-guest-boot".into(),
                target_pid: 200,
                target_start_time_ticks: 300,
                group_leader_pid: 200,
                group_leader_start_time_ticks: 300,
            },
            &"1".repeat(64),
            &"2".repeat(64),
        )
        .unwrap();
        prepared
            .bind_launcher(occurrence)
            .unwrap()
            .mark_launcher_ready(
                AuthenticatedLauncherReady::from_handshake_transcript(&"3".repeat(64)).unwrap(),
            )
            .unwrap()
    }

    /// Apply one owner frame as the guest's control loop does; returns the
    /// number of releases so far.
    fn deliver(&mut self, wire: &[u8]) -> usize {
        let verified = SignedExecutionFrame::decode_and_verify(
            wire,
            &self.binding,
            lillux::time::timestamp_millis(),
        )
        .unwrap();
        let runtime = self.runtime.as_mut().unwrap();
        match verified.frame().payload {
            ExecutionChannelPayload::Acknowledge { .. } => {
                runtime.record_owner_acknowledgement(wire).unwrap();
            }
            ExecutionChannelPayload::Release => {
                runtime.dispatch_release(wire).unwrap();
            }
            _ => {}
        }
        self.releases.load(AtomicOrdering::SeqCst)
    }
}

impl Drop for Guest {
    fn drop(&mut self) {
        crate::state_store::test_native::with_store_host(|host| {
            if let Some(runtime) = self.runtime.take() {
                let (journal, _launcher) = runtime.into_parts();
                ryeos_state::external_execution::native_journal::testing::retire(
                    host,
                    journal.close(),
                );
            }
        });
    }
}

fn now_ms() -> i64 {
    i64::try_from(lillux::time::timestamp_millis()).unwrap()
}

fn budget() -> ExternalLeaseBudget {
    ExternalLeaseBudget {
        max_concurrent_leases: 2,
        max_lease_lifetime_seconds: 4 * 60 * 60,
        daily_lease_seconds: 8 * 60 * 60,
        turn_end_margin_seconds: 30,
    }
}

fn open_store(
    root: &std::path::Path,
    identity: &ryeos_node_runtime::identity::NodeIdentity,
) -> (
    crate::state_store::test_native::TestStateStore,
    Arc<crate::state_store::StateStore>,
    Arc<StateLockLease>,
) {
    let signer = Arc::new(crate::state_store::NodeIdentitySigner::from_identity(
        identity,
    ));
    let mut trust = ryeos_state::refs::TrustStore::new();
    trust.insert(identity.fingerprint().to_owned(), *identity.verifying_key());
    let owner =
        crate::state_store::test_native::TestStateStore::open(root, signer, Arc::new(trust))
            .unwrap();
    let lifetime = owner.controller_lifetime();
    let store = Arc::clone(owner.store());
    (owner, store, lifetime)
}

/// An app over `store` whose node holds the protected channel authority in
/// `vault`, a lease budget, and the one installed scripted backend.
fn compose_app(
    root: &std::path::Path,
    store: Arc<crate::state_store::StateStore>,
    lifetime: Arc<StateLockLease>,
    vault: Arc<crate::vault::SealedEnvelopeVault>,
    script: &Arc<AdapterScript>,
) -> AppState {
    let mut state = crate::state::test_support::build_with_store(root, store, lifetime).unwrap();
    state.vault = vault;
    state.external_placement_backends = Arc::new(
        ExternalPlacementBackendRegistry::from_backends(vec![Arc::new(ScriptedBackend {
            inner: super::tests::FixtureBackend {
                artifact: "d".repeat(64),
            },
            script: Arc::clone(script),
        })])
        .unwrap(),
    );
    let policy =
        ryeos_node_runtime::node_policy::sections::execution::NodeExecutionAdmissionPolicy {
            schema: 4,
            max_live_fanout: 1,
            max_private_materialization_copy_bytes: 1024 * 1024,
            host_env_passthrough: vec![],
            workload_client: None,
            resource_authority: None,
            producer_resource_ceiling: None,
            external_lease_budget: Some(budget()),
        };
    state.node_policy = Arc::new(
        ryeos_node_runtime::node_policy::NodePolicySnapshot::from_test_records(vec![Arc::new(
            policy,
        )]),
    );
    state
}

/// A leased session whose guest has been activated and attached, with a
/// guest ready to dial.
struct Loopback {
    // Field order is drop order: the guest, the app, the store, then the
    // directories under them.
    guest: Guest,
    state: AppState,
    store_owner: crate::state_store::test_native::TestStateStore,
    root: tempfile::TempDir,
    vault: Arc<crate::vault::SealedEnvelopeVault>,
    script: Arc<AdapterScript>,
    binding: ExecutionChannelBinding,
    owner_key: [u8; 32],
    verified_inputs: Vec<String>,
}

impl Loopback {
    fn new(verdict: Verdict) -> Self {
        let root = tempfile::tempdir().unwrap();
        let script = AdapterScript::new(verdict);
        let identity =
            ryeos_node_runtime::identity::NodeIdentity::create(&root.path().join("node-key.pem"))
                .unwrap();
        let (store_owner, store, lifetime) = open_store(root.path(), &identity);

        // The protected channel authority the reservation names.
        let vault = Arc::new(crate::vault::SealedEnvelopeVault::new(
            root.path().join("vault.enc"),
            lillux::vault::VaultSecretKey::generate(),
        ));
        let generation = AUTHORITY_GENERATION_DIGIT.repeat(64);
        let access = ExternalChannelAuthorityAccess::new(&generation).unwrap();
        let authority = access
            .decode(vault.ensure_external_channel_authority(&access).unwrap())
            .unwrap();
        let owner_key = authority.owner_signing_key().to_bytes();

        let retained = RetainedExternalExecutionBinding::test_fixture();
        let now = now_ms();
        let reservation = ExternalAllocationReservation {
            schema: EXTERNAL_ALLOCATION_RESERVATION_SCHEMA,
            placement_thread_id: PLACEMENT.into(),
            admitted_capsule_hash: "a".repeat(64),
            owner: ExternalAllocationOwner::DedicatedSession(ExternalDedicatedSessionOwner {
                workspace_id: "W-one".into(),
                worker_instance_id: "worker-one".into(),
                worker_boot_epoch: 1,
            }),
            base_snapshot_hash: "b".repeat(64),
            binding_hash: retained.digest().into(),
            capacity_owner: retained.capacity_owner().into(),
            channel_authority_generation: generation,
            channel_owner_public_key: authority.owner_public_key(),
            channel_bootstrap_capability_hash: authority.bootstrap_capability_hash(),
            admitted_adapter_request_base64: None,
            request_digest: "e".repeat(64),
            max_active: 1,
            timeout_seconds: 60,
            startup_started_at_ms: now,
            startup_deadline_ms: now + 60_000,
            contact_deadline_ms: now + 60_000,
        };
        // The fixture installs the session's rows with a lease-free
        // allocation, which is settled and removed so the lease-holding one
        // can be reserved in its place.
        store
            .install_external_placement_test_fixture(&reservation, &retained)
            .unwrap();
        store
            .cancel_uncontacted_external_allocation(PLACEMENT)
            .unwrap();
        store
            .with_runtime_sql_for_test(|connection| {
                Ok(connection.execute(
                    "DELETE FROM external_execution_allocation WHERE placement_thread_id=?1",
                    [PLACEMENT],
                )?)
            })
            .unwrap();
        let mut leased = reservation;
        leased.admitted_adapter_request_base64 = Some(
            ExternalAllocationReservation::encode_admitted_adapter_request(b"{\"lease\":1}")
                .unwrap(),
        );
        leased.request_digest = "f".repeat(64);
        store
            .reserve_external_lease_allocation(
                &leased,
                &retained,
                &ExternalLeaseCharge {
                    lifetime_seconds: 60,
                    budget_weight: 1,
                    max_concurrent_leases: 2,
                    daily_lease_seconds: 8 * 60 * 60,
                },
            )
            .unwrap();
        let state = compose_app(root.path(), store, lifetime, Arc::clone(&vault), &script);

        // Contact, occurrence and supervisor activation, as the placement
        // owner records them.
        assert!(matches!(
            state
                .state_store
                .claim_external_allocation_contact(PLACEMENT, &leased.request_digest)
                .unwrap(),
            ExternalAllocationContactClaim::Contact(_)
        ));
        let occurrence = ExternalAllocationOccurrence {
            schema: 1,
            binding_hash: leased.binding_hash.clone(),
            request_digest: leased.request_digest.clone(),
            occurrence_id: OCCURRENCE.into(),
            provider_observation_digest: "f".repeat(64),
        };
        state
            .state_store
            .bind_external_allocation(
                PLACEMENT,
                &occurrence,
                ExternalObservationTiming::Startup {
                    deadline_exceeded: false,
                    live_deadline: lillux::time::MonotonicDeadline::after(
                        lillux::time::Duration::from_secs(60),
                    ),
                },
            )
            .unwrap();
        let intent = activation_intent(&leased, &occurrence);
        assert!(
            state
                .state_store
                .begin_external_supervisor_activation(PLACEMENT, &intent)
                .unwrap()
        );

        // The guest's key is registered at attachment; the channel binding is
        // the one attachment would build.
        let supervisor_key = lillux::crypto::generate_signing_key();
        let issued = lillux::time::timestamp_millis();
        let binding = ExecutionChannelBinding {
            schema: EXECUTION_CHANNEL_BINDING_SCHEMA,
            execution_mode:
                ryeos_external_execution_contract::ExternalExecutionMode::StructuredSession {},
            placement_thread_id: PLACEMENT.into(),
            allocation_request_digest: leased.request_digest.clone(),
            occurrence_id: OCCURRENCE.into(),
            admitted_capsule_hash: leased.admitted_capsule_hash.clone(),
            base_snapshot_hash: leased.base_snapshot_hash.clone(),
            execution_binding_hash: leased.binding_hash.clone(),
            supervisor_runtime_hash: intent.supervisor_runtime_hash.clone(),
            candidate_program_digest: "0".repeat(64),
            channel_nonce: "9".repeat(64),
            owner_public_key: authority.owner_public_key(),
            supervisor_public_key: STANDARD.encode(supervisor_key.verifying_key().as_bytes()),
            issued_at_ms: issued - 1_000,
            execution_deadline_ms: issued + 59_000,
            expires_at_ms: issued + 120_000,
            candidate_export_max_bytes: 512 * 1024,
            max_frames: 100,
            max_bytes: 1024 * 1024,
        };
        state
            .state_store
            .register_external_execution_channel(&binding)
            .unwrap();

        // The lease record turns activated with the digests the guest was
        // delivered, which the guest names as its verified inputs.
        let activation = ExternalLeaseActivation {
            lease_id: OCCURRENCE.into(),
            activation_request_digest: intent.activation_request_digest.clone(),
            signed_import_sha256: "3".repeat(64),
            guest_package_sha256: intent.delivery.payload_sha256.clone(),
            ready_deadline_ms: leased.startup_deadline_ms,
        };
        state
            .state_store
            .activate_external_lease_record(PLACEMENT, &activation)
            .unwrap();
        let verified_inputs = vec![
            activation.activation_request_digest,
            activation.signed_import_sha256,
            activation.guest_package_sha256,
        ];
        let guest = Guest::start(
            binding.clone(),
            &supervisor_key,
            verified_inputs.clone(),
            state.state_store.external_candidate_test_authority(),
        );
        Self {
            root,
            store_owner,
            state,
            vault,
            script,
            binding,
            owner_key,
            verified_inputs,
            guest,
        }
    }

    /// One frame through the same authentication and exchange functions every
    /// carrier calls.
    fn exchange(&self, wire: &[u8]) -> Result<ExternalChannelExchangeResult> {
        exchange_on(&self.state, wire)
    }

    fn lease(&self) -> ExternalLeaseRecord {
        lease_of(&self.state)
    }

    fn supervisor_frames_journalled(&self) -> Vec<String> {
        journalled_kinds(&self.state)
    }

    /// Drop the app and store and reopen them as a restarted controller over
    /// the same node key, vault and script.
    fn restart(
        self,
    ) -> (
        tempfile::TempDir,
        crate::state_store::test_native::TestStateStore,
        AppState,
    ) {
        let Self {
            root,
            store_owner,
            state,
            vault,
            script,
            guest,
            ..
        } = self;
        drop(guest);
        drop(state);
        drop(store_owner);
        let identity =
            ryeos_node_runtime::identity::NodeIdentity::load(&root.path().join("node-key.pem"))
                .unwrap();
        let (store_owner, store, lifetime) = open_store(root.path(), &identity);
        // The app's own identity keys are minted afresh for the new app.
        std::fs::remove_dir_all(root.path().join("identity")).unwrap();
        let state = compose_app(root.path(), store, lifetime, vault, &script);
        (root, store_owner, state)
    }
}

fn exchange_on(state: &AppState, wire: &[u8]) -> Result<ExternalChannelExchangeResult> {
    let authenticated = authenticate_external_channel_frame(state, PLACEMENT, OCCURRENCE, wire)?;
    exchange_external_channel_frame(state, &authenticated, wire)
}

fn lease_of(state: &AppState) -> ExternalLeaseRecord {
    state
        .state_store
        .external_lease_record(PLACEMENT)
        .unwrap()
        .expect("the placement holds a lease record")
}

/// The payload kinds of the supervisor frames in the node's channel journal,
/// in sequence order.
fn journalled_kinds(state: &AppState) -> Vec<String> {
    state
        .state_store
        .with_runtime_sql_for_test(|connection| {
            let mut statement = connection.prepare(
                "SELECT json_extract(frame_json,'$.frame.payload.kind')
                   FROM external_execution_frame
                  WHERE direction='supervisor_to_owner' ORDER BY sequence",
            )?;
            let kinds = statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(kinds)
        })
        .unwrap()
}

fn activation_intent(
    reservation: &ExternalAllocationReservation,
    occurrence: &ExternalAllocationOccurrence,
) -> ExternalSupervisorActivationIntent {
    let contract = RetainedExternalExecutionBinding::test_fixture().backend_contract();
    let attachment_deadline_ms =
        reservation.contact_deadline_ms + i64::from(contract.observation_timeout_seconds) * 1_000;
    let post_execution_timeout_seconds =
        contract.observation_timeout_seconds + contract.cleanup_timeout_seconds;
    let channel_max_bytes = contract.max_transfer_bytes.min(64 * 1024 * 1024);
    let guest_input_identity = "9".repeat(64);
    let activation_request_digest =
        crate::runtime_db::external_execution::external_supervisor_activation_request_digest(
            reservation,
            occurrence,
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
    ExternalSupervisorActivationIntent {
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
        guest_input_identity,
        activation_request_digest,
        attachment_deadline_ms,
        execution_timeout_seconds: reservation.timeout_seconds,
        post_execution_timeout_seconds,
        channel_max_bytes,
        delivery,
    }
}

/// The plain refusal an exchange returned; a success has no `Debug` to report.
fn refusal_of(result: Result<ExternalChannelExchangeResult>) -> String {
    match result {
        Ok(_) => panic!("the exchange unexpectedly succeeded"),
        Err(error) => format!("{error:#}"),
    }
}

fn owner_signing_key(bytes: &[u8; 32]) -> lillux::crypto::SigningKey {
    lillux::crypto::SigningKey::from_bytes(bytes)
}

fn payload_kind(binding: &ExecutionChannelBinding, wire: &[u8]) -> &'static str {
    let verified =
        SignedExecutionFrame::decode_and_verify(wire, binding, lillux::time::timestamp_millis())
            .unwrap();
    assert_eq!(
        verified.frame().direction,
        ChannelDirection::OwnerToSupervisor
    );
    match verified.frame().payload {
        ExecutionChannelPayload::Acknowledge { .. } => "acknowledge",
        ExecutionChannelPayload::Release => "release",
        _ => "other",
    }
}

#[test]
fn an_admitted_guest_is_journalled_released_and_launched() {
    let mut loopback = Loopback::new(Verdict::Admit);
    assert_eq!(loopback.lease().state, ExternalLeaseRecordState::Activated);

    // Frame 1: Ready is held, un-journalled, and answered with an idle reply.
    let ready = loopback.guest.ready.clone();
    let held = loopback.exchange(&ready).unwrap();
    assert!(
        !held.incoming_new && held.outbound.is_empty() && held.acknowledgement_digest.is_none()
    );
    assert_eq!(
        loopback.script.judgements(),
        0,
        "Ready alone asks the adapter nothing"
    );
    assert!(loopback.supervisor_frames_journalled().is_empty());
    assert_eq!(
        loopback.lease().held_ready_frame.as_deref(),
        Some(ready.as_slice())
    );

    // Frame 2: the capability report triggers the adapter's verdict.
    let report = loopback.guest.report.clone();
    let admitted = loopback.exchange(&report).unwrap();
    assert!(
        !admitted.incoming_new,
        "admission journalled the report with Ready, so its exchange is a duplicate"
    );
    assert_eq!(loopback.script.judgements(), 1);
    {
        let judged = loopback.script.judged.lock().unwrap();
        assert_eq!(judged[0].ready_frame, ready);
        assert_eq!(judged[0].report_frame, report);
        let request: serde_json::Value = serde_json::from_slice(&judged[0].request).unwrap();
        assert_eq!(request["expectation"]["lease_id"], OCCURRENCE);
        assert_eq!(
            request["expectation"]["activation_request_digest"],
            loopback.verified_inputs[0].as_str()
        );
        assert_eq!(
            request["expectation"]["channel"]["occurrence_id"],
            OCCURRENCE
        );
    }

    // Ready is journalled now, the record is admitted and keeps the adapter's
    // resume state and the digests of exactly the frames judged.
    assert_eq!(
        loopback.supervisor_frames_journalled(),
        ["ready", "capability_report"]
    );
    let record = loopback.lease();
    assert_eq!(record.state, ExternalLeaseRecordState::Admitted);
    let admission = record
        .admission
        .expect("an admitted lease keeps its admission");
    assert_eq!(admission.ready_frame_digest, lillux::sha256_hex(&ready));
    assert_eq!(admission.report_frame_digest, lillux::sha256_hex(&report));
    assert_eq!(admission.resume_state_json, "{\"chain\":1}");

    // The owner releases the candidate the way its start loop does once Ready
    // is journalled; the guest's retry of its report collects the owner frames.
    let owner = owner_signing_key(&loopback.owner_key);
    let release = loopback
        .state
        .state_store
        .admit_external_ready_and_author_release(
            PLACEMENT,
            &owner,
            lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(60)),
        )
        .unwrap()
        .expect("a journalled Ready is released");
    let collected = loopback.exchange(&report).unwrap();
    assert!(!collected.incoming_new);
    let mut releases = 0;
    let mut saw_release = false;
    for frame in &collected.outbound {
        let kind = payload_kind(&loopback.binding, &frame.canonical_wire);
        saw_release |= kind == "release" && frame.frame_digest == release.digest();
        releases = loopback.guest.deliver(&frame.canonical_wire);
    }
    assert!(saw_release, "the owner's Release reached the guest");
    assert_eq!(releases, 1, "the guest launched the candidate on Release");
}

#[test]
fn a_terminate_verdict_refuses_the_lease_durably_and_closes_its_stream() {
    let loopback = Loopback::new(Verdict::Terminate("the guest reports an open route"));
    let ready = loopback.guest.ready.clone();
    loopback.exchange(&ready).unwrap();
    let report = loopback.guest.report.clone();
    let refusal = refusal_of(loopback.exchange(&report));
    assert!(
        refusal.contains("the lease was refused")
            && refusal.contains("the guest reports an open route"),
        "{refusal}"
    );
    assert_eq!(loopback.script.judgements(), 1);

    let record = loopback.lease();
    assert_eq!(record.state, ExternalLeaseRecordState::Refused);
    assert!(
        record
            .refusal_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("the guest reports an open route"))
    );
    assert!(record.admission.is_none());
    assert!(
        loopback.supervisor_frames_journalled().is_empty(),
        "a refused guest's frames never enter the journal"
    );
    let allocation = loopback
        .state
        .state_store
        .external_allocation(PLACEMENT)
        .unwrap()
        .unwrap();
    // The guest was contacted, so cleanup quarantines the allocation until
    // the provider's termination is observed; the refusal itself is already
    // durable.
    assert_eq!(allocation.phase, ExternalAllocationPhase::Quarantined);

    // The stream route turns this standing into close 1008 after an upgrade
    // and 410 before one; the guest's retries are refused for good and the
    // adapter is not asked again.
    match external_channel_stream_admission(&loopback.state, PLACEMENT, OCCURRENCE) {
        Err(ExternalChannelStreamRefusal::OccurrenceRefused(reason)) => {
            assert!(
                reason.contains("the guest reports an open route"),
                "{reason}"
            );
        }
        other => panic!(
            "a refused lease must close its stream: {:?}",
            other.map(|_| ())
        ),
    }
    let retry = refusal_of(loopback.exchange(&report));
    assert!(retry.contains("the lease was refused"), "{retry}");
    assert_eq!(loopback.script.judgements(), 1);
}

#[test]
fn a_restart_refuses_the_lease_of_an_earlier_incarnation() {
    let loopback = Loopback::new(Verdict::Admit);
    let ready = loopback.guest.ready.clone();
    let report = loopback.guest.report.clone();
    loopback.exchange(&ready).unwrap();
    assert_eq!(loopback.lease().state, ExternalLeaseRecordState::Activated);
    assert_eq!(
        loopback.lease().held_ready_frame.as_deref(),
        Some(ready.as_slice())
    );

    let (_root, _store_owner, state) = loopback.restart();
    terminate_predecessor_external_leases(&state).unwrap();
    let record = lease_of(&state);
    assert_eq!(record.state, ExternalLeaseRecordState::Refused);
    assert_eq!(
        record.refusal_reason.as_deref(),
        Some("controller restarted")
    );

    // The earlier incarnation's guest redials the new controller: its stream
    // is refused and neither frame is judged or journalled.
    match external_channel_stream_admission(&state, PLACEMENT, OCCURRENCE) {
        Err(ExternalChannelStreamRefusal::OccurrenceRefused(reason)) => {
            assert_eq!(reason, "controller restarted");
        }
        other => panic!(
            "an earlier incarnation's lease must close its stream: {:?}",
            other.map(|_| ())
        ),
    }
    for wire in [&ready, &report] {
        let refusal = refusal_of(exchange_on(&state, wire));
        assert!(refusal.contains("the lease was refused"), "{refusal}");
    }
    assert!(journalled_kinds(&state).is_empty());
}

#[test]
fn a_release_authored_before_the_reports_exchange_strands_nothing() {
    let mut loopback = Loopback::new(Verdict::Admit);
    let ready = loopback.guest.ready.clone();
    let report = loopback.guest.report.clone();
    loopback.exchange(&ready).unwrap();

    // The gate admits the lease on the report, and the owner's start loop
    // authors Release before the report's own exchange has run.
    let allocation = loopback
        .state
        .state_store
        .external_allocation(PLACEMENT)
        .unwrap()
        .unwrap();
    let verified = SignedExecutionFrame::decode_and_verify(
        &report,
        &loopback.binding,
        lillux::time::timestamp_millis(),
    )
    .unwrap();
    let gate = super::lease_ready_gate::admit_frame(
        &loopback.state,
        &allocation,
        &loopback.binding,
        verified.frame(),
        &report,
    )
    .unwrap();
    assert_eq!(gate, super::lease_ready_gate::LeaseReadyGate::Proceed);
    assert_eq!(loopback.script.judgements(), 1);
    assert_eq!(
        loopback.supervisor_frames_journalled(),
        ["ready", "capability_report"]
    );

    let owner = owner_signing_key(&loopback.owner_key);
    let release = loopback
        .state
        .state_store
        .admit_external_ready_and_author_release(
            PLACEMENT,
            &owner,
            lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(60)),
        )
        .unwrap()
        .expect("a journalled Ready is released");

    // The report's exchange still succeeds, as a duplicate, and carries the
    // Release to the guest; the adapter is not asked again.
    let exchanged = loopback.exchange(&report).unwrap();
    assert!(!exchanged.incoming_new);
    assert_eq!(loopback.script.judgements(), 1);
    let mut saw_release = false;
    let mut releases = 0;
    for frame in &exchanged.outbound {
        let kind = payload_kind(&loopback.binding, &frame.canonical_wire);
        saw_release |= kind == "release" && frame.frame_digest == release.digest();
        releases = loopback.guest.deliver(&frame.canonical_wire);
    }
    assert!(saw_release, "the owner's Release reached the guest");
    assert_eq!(releases, 1, "the guest launched the candidate on Release");
    assert_eq!(
        loopback.supervisor_frames_journalled(),
        ["ready", "capability_report"]
    );
}
