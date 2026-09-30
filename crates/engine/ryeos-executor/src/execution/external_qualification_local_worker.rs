//! One local process owner for an already released qualification occurrence.
//!
//! The Q start journal owns the workspace, scope, and held-process transition.
//! This module keeps their original operational authorities together until the
//! exclusive session pool has accepted the released Worker.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context as _, Result, ensure};
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

/// Before held spawn consumes the one-use scope, every fallible preparation
/// must explicitly retire its live kernel allocation. The ScopeBound journal
/// keeps recovery even when that physical retirement cannot be proved.
fn with_unlaunched_scope(
    scope: lillux::ProcessScope,
    run: impl FnOnce(&mut Option<lillux::ProcessScope>) -> Result<LaunchOutcome>,
) -> Result<LaunchOutcome> {
    let timeout = scope.recovery().control_timeout();
    let mut pending = Some(scope);
    let result = run(&mut pending);
    let Some(scope) = pending else {
        return result;
    };
    let retirement = scope.retire_unlaunched(timeout);
    let detail = match retirement {
        Ok(()) => "unlaunched Q process scope retired".to_owned(),
        Err(error) => format!("unlaunched Q process scope retirement unproved: {error}"),
    };
    match result {
        Ok(LaunchOutcome::Failed {
            error,
            connector_settlement,
            attached_process,
        }) => Ok(LaunchOutcome::Failed {
            error: error.context(detail),
            connector_settlement,
            attached_process,
        }),
        Err(error) => Err(error.context(detail)),
        Ok(LaunchOutcome::Released(_)) => {
            anyhow::bail!("Q Worker release retained an unconsumed process scope: {detail}")
        }
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

    pub(crate) fn original_workspace(&self) -> &Arc<TempDirGuard> {
        &self.workspace
    }

    pub(crate) fn state_root(&self) -> &lillux::PinnedDirectory {
        &self.state_root
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
                outcome == ExclusiveRetirementOutcome::Reaped,
                "Q exclusive Worker retirement has no whole-process proof: {outcome:?}"
            );
            // Reaped removes the one-shot pool entry. A later read-side error
            // must retry the exact death and terminal checks, not ask the
            // already retired pool to repeat its transition.
            self.retired = true;
            ryeos_app::process::assert_reaped_process_group_absent(&self.process_identity)?;
            ryeos_app::external_qualification_local::ensure_qualification_worker_terminal(
                state,
                self.ready.placement(),
                &self.process_identity,
            )?;
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
}

/// Attach the exact held Worker before release, then publish it to the
/// existing exclusive pool. The caller supplies the Q observation journal
/// sink; command CAS and semantic completion are separate later operations.
pub(crate) fn start_external_qualification_local_worker(
    state: &ryeos_app::state::AppState,
    coordinate: &ConsumerRuntimeVerificationCoordinate,
    ready: ExternalQualificationReady,
    observation_sink: PersistentSessionObservationSink,
) -> Result<ExternalQualificationLocalWorker> {
    let placement = ready.placement().to_owned();
    let binding_digest = ready.binding().digest()?;
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
            let launch = (|| -> Result<LaunchOutcome> {
                let state_root = owner.create_state_root(&reserved)?;
                let runtime_cache = state.config.runtime_root().cache();
                let (project, workspace) =
                    owner.create_original_workspace_root(&reserved, &state_root)?;
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
                // signed backend Create or the held Worker is allowed to fork.
                let materialization = {
                    let authority = state.state_store.pinned_state_authority()?;
                    let guard = authority.acquire_shared_guard()?;
                    let cache =
                        super::cache::MaterializationCache::new(runtime_cache.join("snapshots"));
                    let (path, lease, pinned) = super::checkout_project_snapshot(
                        &authority,
                        &guard,
                        &reserved.base_snapshot_hash,
                        super::ProjectMaterialization::EnforcedCowProject(&project),
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
                let workspace_ready = owner.create_original_workspace(
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
            )?;
                let (mut plan, session, mut mounts, mut lifelines, workspace_path, state_root_path) =
                    materials.into_spawn_parts();
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
                let (allocation, scope) = owner.allocate_original_scope(&workspace_ready)?;
                with_unlaunched_scope(scope, |pending_scope| {
                    let scope_bound = owner.start_record()?;
                    let workspace_view = owner.borrow_workspace_view(&scope_bound, &workspace)?;
                    let (prepared_environment, writable_runtime_view_mounts) =
                        super::persistent_session::prepare_retained_session_process_environment(
                            &content.process_environment,
                            &workspace_path,
                            Some(&workspace_view),
                            session.protocol.workspace_authority,
                            state.isolation.is_enforced(),
                        )?;

                    let boot_identity = lillux::sha256_hex(scope_bound.launch_owner.as_bytes());
                    let mut runtime_environment = BTreeMap::from([(
                        "RYEOS_STRUCTURED_SESSION_PROFILE_HASH".to_owned(),
                        ready.root().worker_profile().profile_hash.clone(),
                    )]);
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
                        ryeos_app::external_connector::prepare_external_candidate_connector(
                            state,
                            &placement,
                            &state_root_path,
                        )?;
                    let (
                        connector_mounts,
                        inherited_fds,
                        connector_lifelines,
                        connector_settlement,
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
                    let spawned = plan.spawn_persistent_session_held(
                        state,
                        &workspace_path,
                        Some(&workspace_view),
                        mounts,
                        writable_runtime_view_mounts,
                        inherited_fds,
                        vec![target_channel],
                        &session.lifecycle,
                        session.protocol.workspace_authority,
                        session.protocol.network_authority,
                        Some(&state_root_path),
                        &placement,
                        None,
                        &scope_bound.launch_owner,
                        "qualification_local_worker",
                        &placement,
                        Some(&allocation),
                        Some(
                            pending_scope
                                .take()
                                .context("Q held spawn lost its one-use process scope")?,
                        ),
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
                    if let Err(error) = owner.attach_worker_process(&held.process_identity) {
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
                    }))
                })
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
            let _ =
                ryeos_app::external_qualification_local::mark_qualification_local_cleanup_unproved(
                    state,
                    &placement,
                    "Q held Worker start failed after connector preparation",
                );
            return Err(error.context(format!(
                "Q held Worker start failed; death: {death:?}; connector: {relay:?}"
            )));
        }
        Err(error) => {
            if reserved_start {
                let _ = ryeos_app::external_qualification_local::mark_qualification_local_cleanup_unproved(
                    state,
                    &placement,
                    "Q local Worker start failed before connector preparation",
                );
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
        return Err(error);
    }
    Ok(ExternalQualificationLocalWorker {
        ready,
        workspace,
        state_root,
        _custody: custody,
        process_identity,
        boot_identity,
        retired: false,
    })
}
