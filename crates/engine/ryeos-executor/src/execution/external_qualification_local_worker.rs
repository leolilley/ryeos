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
    selected_request: SignedQualificationWorkerRequest,
}

enum LaunchOutcome {
    Released(ReleasedQualificationWorker),
    Failed {
        error: anyhow::Error,
        connector_settlement: PersistentSessionCleanupObserver,
        attached_process: Option<ryeos_app::process::ExecutionProcessIdentity>,
    },
}

/// The retained containment chosen for one enforced Worker launch.
struct WorkerContainment {
    scope: lillux::ProcessScope,
    allocation: Option<lillux::ProcessScopeAllocation>,
    /// Present only for a cgroup scope, which needs explicit retirement.
    cgroup_recovery: Option<lillux::ProcessScopeRecovery>,
}

/// Select and prepare the node's qualified mechanism. A cgroup scope is
/// planned and allocated through the runtime's admitted provider; the
/// PID-namespace init is bound by the held launch itself. The Worker row
/// persists the bound recovery at attachment either way.
fn allocate_worker_containment(
    state: &ryeos_app::state::AppState,
    placement: &str,
) -> Result<WorkerContainment> {
    use ryeos_engine::isolation::DescendantContainmentGuarantee;
    let guarantee = state
        .isolation
        .inspection()
        .process_scope_readiness
        .descendant_containment
        .enforced_guarantee()
        .context("enforced Q Worker has no qualified descendant containment")?;
    match guarantee {
        DescendantContainmentGuarantee::PidNamespaceInit => Ok(WorkerContainment {
            scope: lillux::ProcessScope::pid_namespace_init(),
            allocation: None,
            cgroup_recovery: None,
        }),
        DescendantContainmentGuarantee::CgroupScope => {
            let name =
                ryeos_app::external_qualification_local::qualification_worker_scope_name(placement);
            let allocation = state.isolation.plan_process_scope(&name)?;
            let scope = state.isolation.allocate_process_scope(&allocation)?;
            let recovery = scope.recovery().clone();
            Ok(WorkerContainment {
                scope,
                allocation: Some(allocation),
                cgroup_recovery: Some(recovery),
            })
        }
        DescendantContainmentGuarantee::TrustedProcessGroup => {
            anyhow::bail!("the trusted process-group lane is not a Q enforced containment")
        }
    }
}

/// Prove a cgroup scope empty and remove it after a failed held spawn.
fn settle_unlaunched_scope(
    state: &ryeos_app::state::AppState,
    recovery: &lillux::ProcessScopeRecovery,
) -> Result<()> {
    let timeout = state.isolation.process_scope_control_timeout()?;
    recovery
        .terminate_and_wait(timeout)
        .map_err(anyhow::Error::msg)?;
    recovery
        .retire_after_settlement()
        .map_err(anyhow::Error::msg)
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
    wire_max_bytes: u32,
    retired: bool,
}

impl ExternalQualificationLocalWorker {
    pub(crate) fn ready(&self) -> &ExternalQualificationReady {
        &self.ready
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
                    on_delta,
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
        // The pool notices an ended Worker on its own one-second tick, so the
        // attachment check that follows it needs no finer cadence than that.
        const ATTACHMENT_CHECK_INTERVAL: lillux::time::Duration =
            lillux::time::Duration::from_secs(1);
        let mut attachment_checked = lillux::time::MonotonicTimer::start();
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
            // The turn is still pending, so a Worker that has ended can never
            // finish it: fail now rather than at the unrelated deadline.
            if attachment_checked.elapsed() >= ATTACHMENT_CHECK_INTERVAL {
                ryeos_app::external_qualification_local::ensure_qualification_worker_attached(
                    state,
                    self.ready.placement(),
                )?;
                attachment_checked = lillux::time::MonotonicTimer::start();
            }
            lillux::time::sleep(
                deadline
                    .remaining()
                    .min(lillux::time::Duration::from_millis(20)),
            );
        }
    }

    /// After the turn, end the Worker's session protocol from the owner side
    /// and wait for the connector to retire normally. The owner's input
    /// closure makes the guest close the candidate's stdin; a candidate that
    /// ends on EOF exits, the guest signs its stdout EOF and the relay
    /// delivers it to the live connector peer, which is the only way the
    /// connector closes `completed`. Retiring the Worker first kills that peer
    /// and records a disconnect, which native completion must refuse.
    pub(crate) fn close_protocol_input_and_await_connector_completion(
        &self,
        state: &ryeos_app::state::AppState,
    ) -> Result<()> {
        use ryeos_app::state_store::ExternalConnectorSettlement;
        ryeos_app::external_placement::request_external_protocol_input_close(
            state,
            self.ready.placement(),
        )?;
        let now = lillux::time::timestamp_millis();
        let remaining_ms = self
            .ready
            .binding()
            .execution_deadline_ms
            .checked_sub(now)
            .filter(|remaining| *remaining > 0)
            .context("Q connector retirement wait passed its retained channel deadline")?;
        let deadline =
            self.ready
                .root()
                .cap_execution_deadline(lillux::time::MonotonicDeadline::after(
                    lillux::time::Duration::from_millis(u64::try_from(remaining_ms)?),
                ));
        loop {
            ensure!(
                !deadline.has_elapsed(),
                "Q connector did not retire before the channel deadline after the owner closed its input"
            );
            self.ready.ensure_relay_running()?;
            match state
                .state_store
                .external_connector_settlement(self.ready.placement())?
            {
                ExternalConnectorSettlement::Closed { reason } => {
                    ensure!(
                        reason.as_deref() == Some("completed"),
                        "Q connector closed `{}` before the guest protocol EOF",
                        reason.as_deref().unwrap_or("unknown")
                    );
                    return Ok(());
                }
                ExternalConnectorSettlement::Absent => {
                    anyhow::bail!("Q connector vanished after the Worker turn")
                }
                ExternalConnectorSettlement::Open => lillux::time::sleep(
                    deadline
                        .remaining()
                        .min(lillux::time::Duration::from_millis(50)),
                ),
            }
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
            ryeos_app::external_qualification_local::ensure_qualification_worker_terminal(
                state,
                self.ready.placement(),
                &self.process_identity,
            )?;
            // Persist the SAME original post-death canary observation before
            // releasing credential custody. Its helper holds the same removal
            // gate through read+commit; no live callback is needed for cleanup.
            let canary = ryeos_app::external_qualification_local::retain_qualification_controller_canary_terminal(
                state, self.ready.placement(),
            );
            // A refused observation must not strand proven-safe cleanup.
            let credential = ryeos_app::external_qualification_local::release_qualification_worker_credential_custody(
                state, self.ready.placement(),
            );
            match (canary, credential) {
                (Ok(()), Ok(_)) => {}
                (Err(error), Ok(_)) => return Err(error),
                (Ok(()), Err(error)) => return Err(error),
                (Err(error), Err(cleanup)) => {
                    return Err(error.context(format!(
                        "Q canary refusal also retained credential cleanup: {cleanup:#}"
                    )));
                }
            }
            self.retired = true;
            Ok(())
        })();
        let Err(error) = result else {
            return Ok(());
        };
        let _ = ryeos_app::external_qualification_local::mark_qualification_local_cleanup_unproved(
            state,
            self.ready.placement(),
            "Q exclusive Worker retirement remained unproved",
        );
        // The failed retirement keeps both private roots in custody; name
        // them so the retained cleanup debt can be located.
        Err(error.context(format!(
            "Q retirement failed; workspace {:?} and state root {} remain retained",
            self.workspace.path(),
            self.state_root.path().display()
        )))
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
            self.close_protocol_input_and_await_connector_completion(state)?;
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
                    Err(super::external_qualification_start::with_secondary_failure(
                        error,
                        "Q relay shutdown also failed",
                        &relay_error,
                    ))
                }
            },
            Ok(()) => Ok(()),
        };
        let retirement = self.retire_for_native_completion(state);
        match (turn, retirement) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(turn_error), Ok(())) => Err(turn_error),
            (Ok(()), Err(retirement_error)) => Err(retirement_error),
            (Err(turn_error), Err(retirement_error)) => {
                Err(super::external_qualification_start::with_secondary_failure(
                    turn_error,
                    "Q Worker retirement also remained unproved",
                    &retirement_error,
                ))
            }
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

    let settled = (|| -> Result<()> {
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
        Ok(())
    })();

    // Join the last bounded file contact before physical local cleanup or
    // provider termination can acquire the same placement contact gate.
    let relay = worker.ready.stop_relay();

    // Private root removal is local cleanup, independent of the expired
    // guest protocol deadline and of the native outcome: the Worker is
    // terminal, its connector closed and its credential released, so the
    // roots are proven-safe to retire now. Leaving them open on a native
    // refusal would keep every periodic cleanup pass refusing the scratch
    // until the next daemon start. The removal retains original descriptors
    // and has its own exact directory removal authority.
    let local = ryeos_app::external_qualification_local::cleanup_qualification_local_worker(
        state, &placement,
    )
    .and_then(|local| {
        ensure!(
            local.phase
                == ryeos_app::runtime_db::external_execution::QualificationLocalStartPhase::Closed,
            "Q original local private roots remain unclosed"
        );
        Ok(())
    });
    match (settled.and(relay), local) {
        (Ok(()), Ok(())) => {}
        (Err(error), Ok(())) => return Err(error),
        (Ok(()), Err(local)) => return Err(local),
        (Err(error), Err(local)) => {
            return Err(super::external_qualification_start::with_secondary_failure(
                error,
                "Q local private roots also remain retained",
                &local,
            ));
        }
    }

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

/// Resolve the signed turn from original controller custody before the sink
/// or held process exists. The selected request remains in the existing released
/// owner through pool publication; no callback can replace its sent digest.
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
                owner.prepare_original_workspace(
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
                // The SAME original occurrence gate is already held. Prepare
                // intent/file/binding once, then select its retained adapter
                // output without reacquiring that nonreentrant gate.
                owner.prepare_controller_canary(ready.original_guest_exclusion())?;
                // Creation/binding appended to the SAME original local row.
                // Refresh its exact record under this still-held owner gate;
                // the subsequent borrow keeps its complete equality check.
                let workspace_ready = owner.start_record()?;
                let selected_request = owner.select_signed_worker_request()?;
                ensure!(
                    selected_request.wire_max_bytes == wire_max_bytes,
                    "Q held Worker changed its selected signed wire maximum"
                );
                let sink_state = state.clone();
                let sink_placement = placement.clone();
                let sink_digest = selected_request.digest.clone();
                let observation_sink: PersistentSessionObservationSink = Arc::new(move |batch| {
                    ryeos_app::external_qualification_command::ingest_qualification_worker_observation_batch(
                        &sink_state, &sink_placement, &sink_digest, wire_max_bytes, batch,
                    )
                });
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
                    // Enforced isolation runs the Worker over the captured
                    // scratch root (the Q workspace's own named child of the
                    // execution root) with no backend view, retained by its
                    // PID-namespace init. Disabled isolation keeps the
                    // trusted lane over the private project directory.
                    let enforced = state.isolation.is_enforced();
                    let spawn_workspace = if enforced {
                        std::path::PathBuf::from(&workspace_ready.workspace_root)
                    } else {
                        workspace_path.clone()
                    };
                    let (prepared_environment, writable_runtime_view_mounts) =
                        super::persistent_session::prepare_retained_session_process_environment(
                            &content.process_environment,
                            // Beneath the workspace the Worker is told about,
                            // where its bridge looks on either lane.
                            &workspace_path,
                            (!enforced).then_some(&workspace_directory),
                            session.protocol.workspace_authority,
                            enforced,
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
                    // Enforced isolation retains the Worker's descendants by
                    // whichever mechanism the node qualified: its cgroup
                    // scope or the Worker's own PID-namespace init. The same
                    // guarantee the connector gate admitted against.
                    let containment = match enforced
                        .then(|| allocate_worker_containment(state, &placement))
                        .transpose()
                    {
                        Ok(containment) => containment,
                        // Failure after connector preparation settles through
                        // the same path as a failed spawn: fence the start
                        // and hand the connector settlement to the caller.
                        Err(error) => {
                            return Ok(LaunchOutcome::Failed {
                                error: fence_start_failure(
                                    owner,
                                    error,
                                    "Q Worker descendant containment could not be allocated",
                                ),
                                connector_settlement,
                                attached_process: None,
                            });
                        }
                    };
                    let scope_cleanup = containment
                        .as_ref()
                        .and_then(|containment| containment.cgroup_recovery.clone());
                    let (allocation, scope) = match containment {
                        Some(containment) => (containment.allocation, Some(containment.scope)),
                        None => (None, None),
                    };
                    let spawned = plan.spawn_persistent_session_held(
                        state,
                        &spawn_workspace,
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
                        allocation.as_ref(),
                        scope,
                        None,
                    );
                    let held = match spawned {
                        Ok(held) => held,
                        Err(error) => {
                            let error = match scope_cleanup.as_ref() {
                                Some(recovery) => match settle_unlaunched_scope(state, recovery) {
                                    Ok(()) => error,
                                    Err(cleanup) => {
                                        // The slot is left in place on purpose: its
                                        // deterministic name lets the Q local cleanup
                                        // sweep (`retire_qualification_worker_scope_slot`)
                                        // find and retire it, and the Q start is
                                        // fenced cleanup-unproved below.
                                        tracing::warn!(
                                            placement = %placement,
                                            error = %format!("{cleanup:#}"),
                                            "Q cgroup scope retained for the restart sweep after a failed held spawn"
                                        );
                                        error.context(format!(
                                            "Q cgroup scope cleanup remains unproved and is retained for the sweep: {cleanup:#}"
                                        ))
                                    }
                                },
                                None => error,
                            };
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
                        // No Worker row names this scope, so nothing else
                        // will retire it: prove it empty and remove it.
                        let scope_retired = match (&reaped, scope_cleanup.as_ref()) {
                            (Ok(_), Some(recovery)) => {
                                Some(settle_unlaunched_scope(state, recovery))
                            }
                            _ => None,
                        };
                        return Ok(LaunchOutcome::Failed {
                            error: fence_start_failure(
                                owner,
                                error.context(format!(
                                    "Q held Worker attachment failed; reap: {reaped:?}; scope: {scope_retired:?}"
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
                        selected_request,
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
                .map(ryeos_app::process::assert_reaped_process_group_absent);
            if death.as_ref().is_some_and(Result::is_ok) {
                let _ =
                    ryeos_app::external_qualification_local::settle_qualification_worker_process(
                        state, &placement,
                    );
            }
            let relay = connector_settlement();
            let canary = ryeos_app::external_qualification_local::retain_qualification_controller_canary_terminal(
                state, &placement,
            );
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
                "Q held Worker start failed; death: {death:?}; connector: {relay:?}; canary: {canary:?}; credential: {credential:?}"
            )));
        }
        Err(error) => {
            if reserved_start {
                let _ = ryeos_app::external_qualification_local::mark_qualification_local_cleanup_unproved(
                    state,
                    &placement,
                    "Q local Worker start failed before connector preparation",
                );
                let canary = ryeos_app::external_qualification_local::retain_qualification_controller_canary_terminal(
                    state, &placement,
                );
                let credential = ryeos_app::external_qualification_local::release_qualification_worker_credential_custody(
                    state, &placement,
                );
                return Err(error.context(format!(
                    "Q startup final canary attempt: {canary:?}; credential cleanup: {credential:?}"
                )));
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
        selected_request,
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
        relay_health: None,
        relay_owner: None,
        expected_boot_identity: Some(boot_identity.clone()),
        observation_sink: Some(observation_sink),
        cleanup_observer: None,
        // The Q worker row settlement writes durable rows; the connector
        // settlement retires the relay and mount reservations.
        retirement_observer: Some(process_settlement),
        retirement_os_only: Some(connector_settlement),
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
        let canary = ryeos_app::external_qualification_local::retain_qualification_controller_canary_terminal(
            state, &placement,
        );
        let credential = ryeos_app::external_qualification_local::release_qualification_worker_credential_custody(
            state, &placement,
        );
        return Err(error.context(format!(
            "Q readiness final canary attempt: {canary:?}; credential cleanup: {credential:?}"
        )));
    }
    Ok((
        ExternalQualificationLocalWorker {
            ready,
            workspace,
            state_root,
            _custody: custody,
            process_identity,
            wire_max_bytes,
            retired: false,
        },
        selected_request,
    ))
}
