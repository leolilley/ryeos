//! One local process owner for an already released qualification occurrence.
//!
//! The Q start journal owns the private roots and held-process transition.
//! This module keeps their original operational authorities together until the
//! exclusive session pool has accepted the released Worker.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context as _, Result, ensure};
use ryeos_app::external_qualification_command::{
    QualificationWorkerCommandOutcome, SignedQualificationWorkerRequest,
};
use ryeos_app::external_qualification_local::{
    QualificationLocalOwner, with_qualification_local_owner,
};
use ryeos_app::persistent_session::{
    ExclusivePersistentSessionReservation, ExclusiveRetirementOutcome,
    PersistentSessionCleanupObserver, PersistentSessionObservationSink, StartedPersistentSession,
};
use ryeos_app::qualification_workspace_custody::{
    QualificationWorkspaceCustody, QualificationWorkspaceCustodyCoordinate,
};
use ryeos_app::temp_dir_guard::TempDirGuard;
use ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeVerificationCoordinate;

use super::external_qualification_start::ExternalQualificationReady;

fn remaining_qualification_native_observation_window(
    binding: &ryeos_state::external_execution::ExecutionChannelBinding,
) -> Result<lillux::time::Duration> {
    let remaining_ms = binding
        .expires_at_ms
        .checked_sub(lillux::time::timestamp_millis())
        .filter(|remaining| *remaining > 0)
        .context("Q native whole-scope settlement was not retained before channel expiry")?;
    Ok(lillux::time::Duration::from_millis(u64::try_from(
        remaining_ms,
    )?))
}

struct ReleasedQualificationWorker {
    running: ryeos_engine::dispatch::RunningExecution,
    process_identity: ryeos_app::process::ExecutionProcessIdentity,
    socket: lillux::InheritedDuplexChannel,
    lifelines: Vec<Box<dyn Send + Sync>>,
    connector_settlement: PersistentSessionCleanupObserver,
    reservation: ExclusivePersistentSessionReservation,
    workspace: Arc<TempDirGuard>,
    state_root: lillux::PinnedDirectory,
    custody: Arc<QualificationWorkspaceCustody>,
    boot_identity: String,
    observation_sink: PersistentSessionObservationSink,
    wire_max_bytes: u32,
}

enum LaunchOutcome {
    Released(ReleasedQualificationWorker),
    Failed {
        error: anyhow::Error,
        connector_settlement: PersistentSessionCleanupObserver,
        attached_process: Option<ryeos_app::process::ExecutionProcessIdentity>,
    },
}

fn fence_start_failure(
    owner: &QualificationLocalOwner<'_>,
    error: anyhow::Error,
    reason: &str,
) -> anyhow::Error {
    match owner.mark_cleanup_unproved(reason) {
        Ok(()) => error,
        Err(fence) => error.context(format!("Q local cleanup fence failed: {fence:#}")),
    }
}

/// Retains the accepted root and the original workspace/state-root owners
/// while the exclusive pool owns this exact attached process. Its caller must
/// retire the pool process and complete Q settlement before dropping it.
pub(crate) struct ExternalQualificationLocalWorker {
    ready: ExternalQualificationReady,
    workspace: Arc<TempDirGuard>,
    state_root: lillux::PinnedDirectory,
    _custody: Arc<QualificationWorkspaceCustody>,
    process_identity: ryeos_app::process::ExecutionProcessIdentity,
    boot_identity: String,
    wire_max_bytes: u32,
    retired: bool,
}

impl ExternalQualificationLocalWorker {
    pub(crate) fn ready(&self) -> &ExternalQualificationReady {
        &self.ready
    }

    pub(crate) fn process_identity(&self) -> &ryeos_app::process::ExecutionProcessIdentity {
        &self.process_identity
    }

    pub(crate) fn boot_identity(&self) -> &str {
        &self.boot_identity
    }

    pub(crate) fn wire_max_bytes(&self) -> u32 {
        self.wire_max_bytes
    }

    pub(crate) fn original_workspace(&self) -> &Arc<TempDirGuard> {
        &self.workspace
    }

    pub(crate) fn state_root(&self) -> &lillux::PinnedDirectory {
        &self.state_root
    }

    /// Send the finite signed setup and turn through this exact attached
    /// exclusive Worker. The app owns the durable contact and observation journal;
    /// this owner only lends the already attached pool process for one send.
    /// The returned outcome is communication evidence, not qualification.
    pub(crate) fn execute_one_signed_request(
        &self,
        state: &ryeos_app::state::AppState,
        coordinate: &ConsumerRuntimeVerificationCoordinate,
        selected: &SignedQualificationWorkerRequest,
    ) -> Result<QualificationWorkerCommandOutcome> {
        self.ready.ensure_relay_running()?;
        ensure!(
            self.wire_max_bytes == selected.wire_max_bytes,
            "Q selected Request changed the attached signed wire maximum"
        );
        let placement = self.ready.placement().to_owned();
        let binding_digest = self.ready.binding().digest()?;
        let pool = Arc::clone(&state.persistent_sessions);
        ryeos_app::external_qualification_command::execute_signed_qualification_worker_request(
            state,
            &placement,
            coordinate,
            &binding_digest,
            self.ready.root(),
            selected,
            &self.process_identity,
            |body, deadline, on_delta| {
                pool.execute_exclusive_attributed_with_deadline(
                    &placement,
                    None,
                    body,
                    || deadline.has_elapsed(),
                    |delta| on_delta(delta),
                    Some(deadline),
                )
            },
        )
    }

    /// Wait only until the retained Q channel deadline or original callback
    /// expiry for the already settled send's asynchronous turn observation.
    /// This performs no new Worker or provider contact and returns no semantic
    /// success claim.
    pub(crate) fn wait_for_signed_turn_completion(
        &self,
        state: &ryeos_app::state::AppState,
        selected: &SignedQualificationWorkerRequest,
    ) -> Result<QualificationWorkerCommandOutcome> {
        let now = lillux::time::timestamp_millis();
        let remaining_ms = self
            .ready
            .binding()
            .execution_deadline_ms
            .checked_sub(now)
            .filter(|remaining| *remaining > 0)
            .context("Q Worker turn wait passed its retained channel deadline")?;
        let deadline =
            self.ready
                .root()
                .cap_execution_deadline(lillux::time::MonotonicDeadline::after(
                    lillux::time::Duration::from_millis(u64::try_from(remaining_ms)?),
                ));
        loop {
            ensure!(
                !deadline.has_elapsed(),
                "Q Worker turn observation exceeded channel or callback expiry"
            );
            self.ready.ensure_relay_running()?;
            ryeos_app::external_qualification_local::ensure_qualification_worker_credential_current(
                state, self.ready.placement(), self.ready.root(),
            )?;
            let outcome = ryeos_app::external_qualification_command::read_qualification_worker_command_outcome(
                state,
                self.ready.placement(),
                &selected.digest,
            )?
            .context("Q Worker send lost its durable owner settlement")?;
            ensure!(
                !deadline.has_elapsed(),
                "Q Worker turn observation exceeded channel or callback expiry"
            );
            if matches!(
                outcome,
                QualificationWorkerCommandOutcome::TurnCompleted { .. }
            ) {
                return Ok(outcome);
            }
            lillux::time::sleep(
                deadline
                    .remaining()
                    .min(lillux::time::Duration::from_millis(20)),
            );
        }
    }

    /// The pool's `Reaped` result includes successful invocation of both the
    /// exact Q process-row settlement and connector relay retirement. Keep the
    /// original Ready, workspace, and state-root owners even if this fails.
    pub(crate) fn retire_for_native_completion(
        &mut self,
        state: &ryeos_app::state::AppState,
    ) -> Result<()> {
        if self.retired {
            ryeos_app::process::assert_reaped_process_group_absent(&self.process_identity)?;
            return ryeos_app::external_qualification_local::ensure_qualification_worker_terminal(
                state,
                self.ready.placement(),
                &self.process_identity,
            );
        }
        let result = (|| {
            let outcome = state
                .persistent_sessions
                .retire_exclusive(self.ready.placement())?;
            ensure!(
                matches!(
                    outcome,
                    ExclusiveRetirementOutcome::Reaped | ExclusiveRetirementOutcome::Absent
                ),
                "Q exclusive Worker retirement has no whole-process proof: {outcome:?}"
            );
            // A failed request may already have retired and removed this
            // exact pool entry. `Absent` carries no death proof by itself;
            // the attached process identity and terminal journal must still
            // independently verify its whole-group reap below.
            ryeos_app::process::assert_reaped_process_group_absent(&self.process_identity)?;
            ryeos_app::external_qualification_local::ensure_qualification_worker_terminal(
                state,
                self.ready.placement(),
                &self.process_identity,
            )?;
            ryeos_app::external_qualification_local::release_qualification_worker_credential_custody(
                state, self.ready.placement(),
            )?;
            self.retired = true;
            Ok(())
        })();
        if result.is_err() {
            let _ =
                ryeos_app::external_qualification_local::mark_qualification_local_cleanup_unproved(
                    state,
                    self.ready.placement(),
                    "Q exclusive Worker retirement remained unproved",
                );
        }
        result
    }

    /// Freeze the exact one-send transcript after the pool has reaped this
    /// original Worker. The returned CAS root is structural evidence only;
    /// it cannot stand in for native, provider or semantic settlement.
    pub(crate) fn retain_terminal_transcript(
        &self,
        state: &ryeos_app::state::AppState,
        coordinate: &ConsumerRuntimeVerificationCoordinate,
    ) -> Result<String> {
        ensure!(
            self.retired,
            "Q Worker transcript requires this owner's terminal retirement"
        );
        ryeos_app::external_qualification_transcript::retain_terminal_qualification_worker_transcript(
            state,
            self.ready.placement(),
            coordinate,
            &self.ready.binding().digest()?,
            self.ready.root(),
            &self.process_identity,
        )
    }

    /// Consume the signed setup and one turn, settling the attached local process
    /// even when the send or wait fails. Transcript publication must wait for
    /// original native settlement and bounded fixture receiver custody.
    /// The outer Q guest and cleanup lifeline remain with this owner for the
    /// separate native and provider settlement steps.
    pub(crate) fn run_signed_turn_and_retire(
        &mut self,
        state: &ryeos_app::state::AppState,
        coordinate: &ConsumerRuntimeVerificationCoordinate,
        selected: &SignedQualificationWorkerRequest,
    ) -> Result<()> {
        let turn = (|| -> Result<()> {
            self.execute_one_signed_request(state, coordinate, selected)?;
            self.wait_for_signed_turn_completion(state, selected)?;
            Ok(())
        })();
        // A failed send no longer needs guest traffic. Stop all relay
        // contact before retiring this failed local owner or handing cleanup
        // to the Ready lifeline. Normal retirement still needs the pump for
        // the later native completion exchange.
        let turn = match turn {
            Err(error) => match self.ready.stop_relay() {
                Ok(()) => Err(error),
                Err(relay_error) => {
                    Err(error.context(format!("Q relay shutdown also failed: {relay_error:#}")))
                }
            },
            Ok(()) => Ok(()),
        };
        let retirement = self.retire_for_native_completion(state);
        match (turn, retirement) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(turn_error), Ok(())) => Err(turn_error),
            (Ok(()), Err(retirement_error)) => Err(retirement_error),
            (Err(turn_error), Err(retirement_error)) => Err(turn_error.context(format!(
                "Q Worker retirement also remained unproved: {retirement_error:#}"
            ))),
        }
    }
}

/// Drive one already authenticated Q Ready occurrence through its signed
/// session setup and single Worker turn and independent transport, native, local and provider
/// settlement. Every transition rechecks its own durable owner. This returns
/// only after exact provider terminal proof; it makes no verifier verdict or
/// product qualification claim. A failure drops the original cleanup lifeline,
/// which retains cancellation unless a normal provider intent already owns
/// reconciliation after a lost response.
pub(crate) fn run_external_qualification_to_normal_terminal(
    state: &ryeos_app::state::AppState,
    coordinate: &ConsumerRuntimeVerificationCoordinate,
    ready: ExternalQualificationReady,
) -> Result<()> {
    use ryeos_app::external_placement::ExternalCandidateCleanupProgress;

    let (mut worker, selected) =
        start_external_qualification_local_worker_for_signed_request(state, coordinate, ready)?;
    let placement = worker.ready().placement().to_owned();
    let binding_digest = worker.ready().binding().digest()?;
    worker.run_signed_turn_and_retire(state, coordinate, &selected)?;

    // The signed frame is durably queued for the existing channel transport;
    // its return is not evidence that the guest processed the command.
    ryeos_app::external_placement::request_external_qualification_native_completion(
        state, &placement,
    )?;
    let native = loop {
        if let Some(native) =
            ryeos_app::external_placement::apply_external_qualification_native_settlement(
                state, &placement,
            )?
        {
            break native;
        }
        worker.ready().ensure_relay_running()?;
        // Completion must be authored before its execution deadline. The
        // signed native death observation may arrive later, during the
        // binding's post-execution observation window.
        lillux::time::sleep(
            remaining_qualification_native_observation_window(worker.ready().binding())?
                .min(lillux::time::Duration::from_millis(50)),
        );
    };
    ensure!(
        native.placement_thread_id == placement && native.binding_digest == binding_digest,
        "Q native settlement changed its exact channel"
    );

    // Publish once only after the original native frame is Applied and its
    // optional capture has verified controller custody. This reuses historical
    // evidence ownership; it cannot repeat the turn or mint contact permission.
    worker.retain_terminal_transcript(state, coordinate)?;

    // Join the last bounded file contact before physical local cleanup or
    // provider termination can acquire the same placement contact gate.
    worker.ready.stop_relay()?;

    // Private root removal is local cleanup, independent of the expired
    // guest protocol deadline. It retains original descriptors and has its
    // own exact directory removal authority.
    let local = ryeos_app::external_qualification_local::cleanup_qualification_local_worker(
        state, &placement,
    )?;
    ensure!(
        local.phase
            == ryeos_app::runtime_db::external_execution::QualificationLocalStartPhase::Closed,
        "Q original local private roots remain unclosed"
    );

    // The first provider call commits its intent before contact. A Pending
    // response remains recoverable under that exact intent after this owner
    // drops; the process-local polling window is bounded independently of
    // the guest protocol deadline because this is historical cleanup.
    let provider_deadline =
        lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(30));
    loop {
        match ryeos_app::external_placement::advance_external_qualification_normal_settlement(
            state, &placement,
        )? {
            ExternalCandidateCleanupProgress::Proved => break,
            ExternalCandidateCleanupProgress::Pending => {
                ensure!(
                    !provider_deadline.has_elapsed(),
                    "Q provider terminal remains pending under its durable normal intent"
                );
                lillux::time::sleep(
                    provider_deadline
                        .remaining()
                        .min(lillux::time::Duration::from_millis(50)),
                );
            }
        }
    }
    worker.ready.disarm_after_normal_terminal()
}

/// Select the signed Request before Q held start so the pushed-batch sink can
/// capture its exact digest. The start independently rechecks the accepted
/// Worker and protocol under its Q occurrence gate. The later send reselects
/// the same Request under that gate before consuming the one-use contact row.
pub(crate) fn start_external_qualification_local_worker_for_signed_request(
    state: &ryeos_app::state::AppState,
    coordinate: &ConsumerRuntimeVerificationCoordinate,
    ready: ExternalQualificationReady,
) -> Result<(
    ExternalQualificationLocalWorker,
    SignedQualificationWorkerRequest,
)> {
    let placement = ready.placement().to_owned();
    let binding_digest = ready.binding().digest()?;
    let selected =
        ryeos_app::external_qualification_command::select_signed_qualification_worker_request(
            state,
            &placement,
            coordinate,
            &binding_digest,
            ready.root(),
        )?;
    let sink_state = (*state).clone();
    let sink_placement = placement.clone();
    let sink_digest = selected.digest.clone();
    let selected_wire_max_bytes = selected.wire_max_bytes;
    let worker = start_external_qualification_local_worker(
        state,
        coordinate,
        ready,
        move |wire_max_bytes| {
            ensure!(
                wire_max_bytes == selected_wire_max_bytes,
                "Q held Worker changed its selected signed wire maximum"
            );
            Ok(Arc::new(move |batch| {
                ryeos_app::external_qualification_command::ingest_qualification_worker_observation_batch(
                    &sink_state,
                    &sink_placement,
                    &sink_digest,
                    wire_max_bytes,
                    batch,
                )
            }))
        },
    )?;
    Ok((worker, selected))
}

/// Attach the exact held Worker before release, then publish it to the
/// existing exclusive pool. The caller supplies the Q observation journal
/// sink; command CAS and semantic completion are separate later operations.
pub(crate) fn start_external_qualification_local_worker(
    state: &ryeos_app::state::AppState,
    coordinate: &ConsumerRuntimeVerificationCoordinate,
    ready: ExternalQualificationReady,
    make_observation_sink: impl FnOnce(u32) -> Result<PersistentSessionObservationSink>,
) -> Result<ExternalQualificationLocalWorker> {
    let placement = ready.placement().to_owned();
    let binding_digest = ready.binding().digest()?;
    let contract = ready.root().worker_launch_contract();
    let _credential_operation =
        ryeos_app::hosted_operation::acquire_credential_profile_operation_until(
            &contract.credential_profile_id,
            ready
                .root()
                .cap_execution_deadline(lillux::time::MonotonicDeadline::after(
                    lillux::time::Duration::from_secs(contract.max_lifetime_seconds),
                )),
        )?;
    let mut reserved_start = false;
    let prepared = with_qualification_local_owner(
        state,
        &placement,
        coordinate,
        &binding_digest,
        ready.root(),
        |owner| {
            let reserved = owner.reserve_start()?;
            reserved_start = true;
            let (project, workspace, state_root) =
                owner.create_original_runtime_roots(&reserved)?;
            let credential_home = owner.credential_home()?;
            let credential_home_path = credential_home.path().to_path_buf();
            let launch = (|| -> Result<LaunchOutcome> {
                let runtime_cache = state.config.runtime_root().cache();
                let custody_coordinate =
                    QualificationWorkspaceCustodyCoordinate::from_start(&reserved)?;
                let custody = state
                    .qualification_workspace_custody
                    .borrow_exact(&custody_coordinate)?;
                ensure!(
                    project.parent() == Some(Path::new(&reserved.workspace_root))
                        && state_root.path() == Path::new(&reserved.state_root),
                    "Q local runtime roots differ from their reserved coordinates"
                );
                // CAS construction and its fork-sensitive guard end before the
                // held trusted Worker is allowed to fork.
                let budget = super::external_content::private_materialization_budget()?;
                let materialization = {
                    let authority = state.state_store.pinned_state_authority()?;
                    let guard = authority.acquire_shared_guard()?;
                    let cache =
                        super::cache::MaterializationCache::new(runtime_cache.join("snapshots"));
                    let (path, lease, pinned) = super::checkout_project_snapshot(
                        &authority,
                        &guard,
                        &reserved.base_snapshot_hash,
                        super::ProjectMaterialization::PrivateWritableWorkspace {
                            target_dir: &project,
                            budget: Some(&budget),
                        },
                        &cache,
                    )?;
                    ensure!(
                        path == project,
                        "Q base snapshot changed its original project path"
                    );
                    workspace.retain_lease(lease);
                    authority.ensure_guard(&guard)?;
                    pinned
                };
                let workspace_ready = owner.prepare_original_workspace(
                    &reserved,
                    &workspace,
                    &project,
                    &materialization,
                )?;
                drop(materialization);

                let materials = super::external_qualification_local_materials::prepare_external_qualification_local_materials(
                state,
                ready.root(),
                &project,
                Arc::clone(&workspace),
                &state_root,
                &credential_home,
                &budget,
            )?;
                let (mut plan, session, mut mounts, mut lifelines, workspace_path, state_root_path) =
                    materials.into_spawn_parts();
                let wire_max_bytes = session.wire.max_frame_bytes;
                let observation_sink = make_observation_sink(wire_max_bytes)?;
                ensure!(
                    workspace_path == project && state_root_path == state_root.path(),
                    "Q prepared material changed its original runtime roots"
                );
                let content = ready
                    .root()
                    .purpose()
                    .consumer_content
                    .as_ref()
                    .context("Q local Worker has no accepted process environment")?;
                let reservation = state.persistent_sessions.reserve_exclusive(
                    &placement,
                    &session.lifecycle,
                    &session.wire,
                )?;
                {
                    let workspace_directory =
                        owner.borrow_workspace_directory(&workspace_ready, &workspace)?;
                    let (prepared_environment, writable_runtime_view_mounts) =
                        super::persistent_session::prepare_retained_session_process_environment(
                            &content.process_environment,
                            &workspace_path,
                            Some(&workspace_directory),
                            session.protocol.workspace_authority,
                            false,
                        )?;

                    let boot_identity = lillux::sha256_hex(workspace_ready.launch_owner.as_bytes());
                    let contract = ready.root().worker_launch_contract();
                    let mut runtime_environment = BTreeMap::from([
                        (
                            "RYEOS_STRUCTURED_SESSION_PROFILE_HASH".to_owned(),
                            ready.root().worker_profile().profile_hash.clone(),
                        ),
                        (
                            contract.credential_home_env.clone(),
                            credential_home_path.to_string_lossy().into_owned(),
                        ),
                        (
                            contract.workspace_env.clone(),
                            workspace_path.to_string_lossy().into_owned(),
                        ),
                        (
                            "RYEOS_STRUCTURED_SESSION_ROUTE_SET".to_owned(),
                            contract.route_set.clone(),
                        ),
                        (
                            "RYEOS_STRUCTURED_SESSION_EFFECT_CLASSES".to_owned(),
                            contract.allowed_effect_classes.join(","),
                        ),
                    ]);
                    ensure!(
                        runtime_environment.len() == 5,
                        "Q launch context environment slots collide with fixed protocol authority"
                    );
                    let identity_env = session
                        .protocol
                        .readiness_identity_env
                        .as_ref()
                        .context("Q signed protocol has no readiness identity environment")?;
                    ensure!(
                        runtime_environment
                            .insert(identity_env.clone(), boot_identity.clone())
                            .is_none(),
                        "Q readiness identity collides with profile authority"
                    );
                    if !content.process_environment.is_empty() {
                        let encoded =
                            lillux::canonical_json(&serde_json::to_value(&prepared_environment)?)?;
                        ensure!(
                            runtime_environment
                                .insert(
                                    ryeos_state::objects::SESSION_PROCESS_ENVIRONMENT_ENV
                                        .to_owned(),
                                    encoded,
                                )
                                .is_none(),
                            "Q process environment collides with runtime authority"
                        );
                    }
                    let mut allowlist = session.protocol.runtime_env_allowlist.clone();
                    allowlist.push(identity_env.clone());
                    plan.bind_persistent_session_runtime_environment(
                        &runtime_environment,
                        &allowlist,
                    )?;

                    let (daemon_socket, worker_socket) = lillux::inherited_duplex_channel_pair()
                        .map_err(anyhow::Error::msg)
                        .context("create Q inherited structured-session channel")?;
                    let target_channel =
                        ryeos_engine::isolation::IsolationTargetChannelAuthority::new(
                            worker_socket,
                            0,
                            session.wire.channel_env.clone(),
                        )?;
                    // Connector preparation is one-shot and follows authenticated Q
                    // Ready. Its relay and mount descriptors join the process owner.
                    let connector =
                        ryeos_app::external_connector::prepare_qualification_candidate_connector(
                            state,
                            &placement,
                            &credential_home,
                        )?;
                    let (
                        connector_mounts,
                        inherited_fds,
                        connector_lifelines,
                        connector_settlement,
                        configuration_observation,
                    ) = connector.into_parts();
                    mounts.extend(connector_mounts);
                    lifelines.extend(connector_lifelines);
                    if let Err(error) = owner.authorize_spawn() {
                        return Ok(LaunchOutcome::Failed {
                            error: fence_start_failure(
                                owner,
                                error,
                                "Q local Worker lost live spawn authority",
                            ),
                            connector_settlement,
                            attached_process: None,
                        });
                    }
                    // Disabled isolation accepts no backend workspace view.
                    // Retain the original private directory through the held
                    // process lifetime; use the existing trusted launch path.
                    lifelines.push(Box::new(workspace_directory));
                    let spawned = plan.spawn_persistent_session_held(
                        state,
                        &workspace_path,
                        None,
                        mounts,
                        writable_runtime_view_mounts,
                        inherited_fds,
                        vec![target_channel],
                        &session.lifecycle,
                        session.protocol.workspace_authority,
                        session.protocol.network_authority,
                        Some(&credential_home_path),
                        &placement,
                        None,
                        &workspace_ready.launch_owner,
                        "qualification_local_worker",
                        &placement,
                        None,
                        None,
                    );
                    let held = match spawned {
                        Ok(held) => held,
                        Err(error) => {
                            return Ok(LaunchOutcome::Failed {
                                error: fence_start_failure(
                                    owner,
                                    error,
                                    "Q held Worker spawn failed",
                                ),
                                connector_settlement,
                                attached_process: None,
                            });
                        }
                    };
                    let process_identity = held.process_identity.clone();
                    if let Err(error) = owner
                        .attach_worker_process(&held.process_identity, &configuration_observation)
                    {
                        let reaped = held.abort_and_reap();
                        return Ok(LaunchOutcome::Failed {
                            error: fence_start_failure(
                                owner,
                                error.context(format!(
                                    "Q held Worker attachment failed; reap: {reaped:?}"
                                )),
                                "Q held Worker attachment failed",
                            ),
                            connector_settlement,
                            attached_process: None,
                        });
                    }
                    // An attached held child is cleanup custody, not permission to
                    // execute after its invoking callback expires or is revoked.
                    if let Err(error) = owner.authorize_worker_release() {
                        let reaped = held.abort_and_reap();
                        return Ok(LaunchOutcome::Failed {
                            error: fence_start_failure(
                                owner,
                                error.context(format!(
                                    "Q held Worker release authorization failed; reap: {reaped:?}"
                                )),
                                "Q held Worker release authorization failed",
                            ),
                            connector_settlement,
                            attached_process: Some(process_identity),
                        });
                    }
                    // A later termination cannot overtake a held child: the Q contact
                    // gate stays held through release, then pool readiness runs alone.
                    let running = match held.release_after_attachment() {
                        Ok(running) => running,
                        Err(error) => {
                            return Ok(LaunchOutcome::Failed {
                                error: fence_start_failure(
                                    owner,
                                    error.context("Q attached Worker release failed"),
                                    "Q attached Worker release failed",
                                ),
                                connector_settlement,
                                attached_process: Some(process_identity),
                            });
                        }
                    };
                    Ok(LaunchOutcome::Released(ReleasedQualificationWorker {
                        running,
                        process_identity,
                        socket: daemon_socket,
                        lifelines,
                        connector_settlement,
                        reservation,
                        workspace,
                        state_root,
                        custody,
                        boot_identity,
                        observation_sink,
                        wire_max_bytes,
                    }))
                }
            })();
            launch.map_err(|error| {
                fence_start_failure(
                    owner,
                    error,
                    "Q local Worker preparation failed after reservation",
                )
            })
        },
    );
    let prepared = match prepared {
        Ok(LaunchOutcome::Released(prepared)) => prepared,
        Ok(LaunchOutcome::Failed {
            error,
            connector_settlement,
            attached_process,
        }) => {
            let death = attached_process
                .as_ref()
                .map(|identity| ryeos_app::process::assert_reaped_process_group_absent(identity));
            if death.as_ref().is_some_and(Result::is_ok) {
                let _ =
                    ryeos_app::external_qualification_local::settle_qualification_worker_process(
                        state, &placement,
                    );
            }
            let relay = connector_settlement();
            let credential =
                ryeos_app::external_qualification_local::release_qualification_worker_credential_custody(
                    state, &placement,
                );
            let _ =
                ryeos_app::external_qualification_local::mark_qualification_local_cleanup_unproved(
                    state,
                    &placement,
                    "Q held Worker start failed after connector preparation",
                );
            return Err(error.context(format!(
                "Q held Worker start failed; death: {death:?}; connector: {relay:?}; credential: {credential:?}"
            )));
        }
        Err(error) => {
            if reserved_start {
                let _ = ryeos_app::external_qualification_local::mark_qualification_local_cleanup_unproved(
                    state,
                    &placement,
                    "Q local Worker start failed before connector preparation",
                );
                let credential = ryeos_app::external_qualification_local::release_qualification_worker_credential_custody(
                    state, &placement,
                );
                return Err(error.context(format!("Q startup credential cleanup: {credential:?}")));
            }
            return Err(error);
        }
    };
    // The contact gate is released before pool readiness starts its reader;
    // observations may themselves commit through that same occurrence gate.
    let ReleasedQualificationWorker {
        running,
        process_identity,
        socket,
        lifelines,
        connector_settlement,
        reservation,
        workspace,
        state_root,
        custody,
        boot_identity,
        observation_sink,
        wire_max_bytes,
    } = prepared;
    let settlement_state = state.clone();
    let settlement_placement = placement.clone();
    let process_settlement: PersistentSessionCleanupObserver = Arc::new(move || {
        ryeos_app::external_qualification_local::settle_qualification_worker_process(
            &settlement_state,
            &settlement_placement,
        )
    });
    let started = StartedPersistentSession {
        running,
        socket,
        lifelines,
        expected_boot_identity: Some(boot_identity.clone()),
        observation_sink: Some(observation_sink),
        cleanup_observer: Some(process_settlement),
        retirement_observer: Some(connector_settlement),
        resource_occupancy_limit: None,
        resource_cleanup_allowance_ms: None,
        resource_attribution_sink: None,
        resource_eligibility: None,
    };
    if let Err(error) = reservation.bind(started) {
        let _ = ryeos_app::external_qualification_local::mark_qualification_local_cleanup_unproved(
            state,
            &placement,
            "Q exclusive Worker readiness or pool binding failed",
        );
        let credential = ryeos_app::external_qualification_local::release_qualification_worker_credential_custody(
            state, &placement,
        );
        return Err(error.context(format!("Q readiness credential cleanup: {credential:?}")));
    }
    Ok(ExternalQualificationLocalWorker {
        ready,
        workspace,
        state_root,
        _custody: custody,
        process_identity,
        boot_identity,
        wire_max_bytes,
        retired: false,
    })
}
