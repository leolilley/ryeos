use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context as _, Result};
use serde_json::{Value, json};

use ryeos_engine::canonical_ref::CanonicalRef;

use crate::dispatch_error::DispatchError;

use super::super::process_attachment::AttachedProcessGuard;
use super::{EnvelopeCallback, LaunchEnvelope, RuntimeResult};
use ryeos_runtime::envelope::RuntimeResultStatus;
use ryeos_runtime::process_outcome::RuntimeProcessOutcome;

pub(super) struct SpawnRuntimeParams<'a> {
    pub state: &'a ryeos_app::state::AppState,
    pub descriptor: &'a ryeos_engine::protocols::ProtocolDescriptor,
    /// Exact verified runtime item selected by the runtime registry.
    pub item_ref: &'a CanonicalRef,
    pub observation_declarations:
        &'a BTreeMap<String, ryeos_engine::runtime_registry::ChildObservationDecl>,
    pub acting_principal: &'a str,
    pub binary: &'a str,
    pub project_path: &'a Path,
    pub project_authority: ryeos_engine::isolation::IsolationProjectAuthority,
    pub immutable_project: Option<ryeos_state::PinnedProjectMaterialization>,
    pub filesystem_authority_ceiling: ryeos_engine::isolation::IsolationFilesystemAuthorityCeiling,
    pub network_authority_ceiling: ryeos_engine::isolation::IsolationNetworkAuthorityCeiling,
    pub project_state_scope: Option<&'a str>,
    pub live_access: Option<ryeos_engine::isolation::IsolationLiveAccessAuthority>,
    pub state_root: Option<&'a Path>,
    pub workspace_lifeline: Option<std::sync::Arc<ryeos_app::temp_dir_guard::TempDirGuard>>,
    /// Whether this launch owns the journal lifecycle for the supplied
    /// workspace. Callback children may borrow the path but never its owner
    /// transitions.
    pub owns_workspace: bool,
    pub envelope: &'a LaunchEnvelope,
    pub timeout_secs: u64,
    /// Durable execution-tree deadline minted under the shared accounting
    /// scope. Kept absolute until the final pre-spawn boundary so queueing and
    /// launch preparation cannot restart or extend the remaining window.
    pub aggregate_deadline_at_ms: Option<i64>,
    pub callback: &'a EnvelopeCallback,
    pub thread_id: &'a str,
    pub launch_owner: &'a str,
    pub vault_bindings: &'a [(String, String)],
    pub thread_auth_token: &'a str,
    pub roots: ryeos_app::env_contract::DaemonRootEnv,
    pub isolation: &'a ryeos_engine::isolation::IsolationRuntime,
    pub verified_command: &'a ryeos_engine::isolation::IsolationDescriptorBoundCommand,
    pub external_realizations: Option<super::super::external_content::BoundExternalRealizations>,
    pub source_closure: Option<super::super::source_closure::BoundSourceClosure>,
    pub cas_root: &'a Path,
    /// Daemon-allocated checkpoint dir for a replay-aware runtime.
    pub checkpoint_dir: Option<&'a Path>,
    /// Exact descriptor authority paired with `checkpoint_dir`.
    pub checkpoint_authority: Option<&'a lillux::PinnedDirectory>,
    /// Whether the replay-aware runtime should load that checkpoint.
    pub is_resume: bool,
    /// Clear the predecessor-to-successor auto-launch counter only after the
    /// successor process has crossed the durable attachment boundary. Its
    /// later same-thread restart budget is a distinct recovery coordinate.
    pub rearm_native_resume_budget_after_attach: bool,
}

pub(super) struct SpawnedRuntime {
    thread_id: String,
    runtime_ref: String,
    observation_declarations:
        BTreeMap<String, ryeos_engine::runtime_registry::ChildObservationDecl>,
    process: Option<lillux::RunningProcess>,
    attached_process: Option<AttachedProcessGuard>,
    workspace_lifeline: Option<std::sync::Arc<ryeos_app::temp_dir_guard::TempDirGuard>>,
    external_realizations: Option<super::super::external_content::BoundExternalRealizations>,
    source_closure: Option<super::super::source_closure::BoundSourceClosure>,
    immediate_result: Option<RuntimeResult>,
}

pub(super) struct SpawnedRuntimeWaitResult {
    pub result: Result<RuntimeProcessOutcome>,
    /// True only after `AttachedProcessGuard::settle_after_reap` proved the
    /// exact process group absent and compare-cleared its workspace binding.
    /// An immediate spawn failure has no such attached-wait proof.
    pub settled_attached_wait: bool,
}

impl SpawnedRuntime {
    pub(super) fn wait(mut self) -> SpawnedRuntimeWaitResult {
        if let Some(result) = self.immediate_result.take() {
            return SpawnedRuntimeWaitResult {
                result: Ok(RuntimeProcessOutcome::Terminal(result)),
                settled_attached_wait: false,
            };
        }
        let Some(process) = self.process.take() else {
            return SpawnedRuntimeWaitResult {
                result: Err(anyhow::anyhow!(
                    "spawned runtime has no process or immediate result"
                )),
                settled_attached_wait: false,
            };
        };
        let result = process.wait();
        emit_captured_child_observation_records(
            &self.thread_id,
            &self.runtime_ref,
            &self.observation_declarations,
            &result.stderr,
            result.stderr_truncated,
        );
        // Decode/retry handling runs only after the exact reaped attachment and
        // its workspace membership settle together. Drop must not erase that
        // evidence when wait/cleanup or descendant settlement is unproved.
        let settlement = match self.attached_process.as_mut() {
            Some(attached_process) => attached_process.settle_after_reap(),
            None => Err(anyhow::anyhow!(
                "waited runtime lost its attached process owner"
            )),
        };
        if let Err(error) = settlement {
            return SpawnedRuntimeWaitResult {
                result: Err(error),
                settled_attached_wait: false,
            };
        }
        drop(self.attached_process.take());
        drop(self.workspace_lifeline.take());
        drop(self.external_realizations.take());
        drop(self.source_closure.take());
        let outcome = if !result.success {
            Ok(RuntimeProcessOutcome::Terminal(runtime_failure_result(
                &result.stderr,
                result.timed_out,
                result.output_limit_exceeded.map(|limit| limit.as_str()),
            )))
        } else {
            decode_runtime_stdout(&result.stdout)
        };
        SpawnedRuntimeWaitResult {
            result: outcome,
            settled_attached_wait: true,
        }
    }
}

/// Build the protocol subprocess request and start the child, returning as
/// soon as stdin has been handed to the successfully spawned process. Waiting
/// and result decoding are deliberately separate so accepted launch surfaces
/// can acknowledge the durable spawn-task handoff without waiting for runtime
/// completion.
pub(super) fn spawn_runtime(params: SpawnRuntimeParams<'_>) -> Result<SpawnedRuntime> {
    let SpawnRuntimeParams {
        state,
        descriptor,
        item_ref,
        observation_declarations,
        acting_principal,
        binary,
        project_path,
        project_authority,
        immutable_project,
        filesystem_authority_ceiling,
        network_authority_ceiling,
        project_state_scope,
        live_access,
        state_root,
        workspace_lifeline,
        owns_workspace,
        envelope,
        timeout_secs,
        aggregate_deadline_at_ms,
        callback,
        thread_id,
        launch_owner,
        vault_bindings,
        thread_auth_token,
        roots,
        isolation,
        verified_command,
        external_realizations,
        source_closure,
        cas_root,
        checkpoint_dir,
        checkpoint_authority,
        is_resume,
        rearm_native_resume_budget_after_attach,
    } = params;
    // Only this bounded protocol/environment construction precedes all
    // isolation setup and process contact. An arbitrary error returned later
    // must not be retroactively classified as a preparation refusal.
    let (spec, request, isolation_daemon_socket_path) = (|| -> Result<_> {
        let secret_map: BTreeMap<String, String> = vault_bindings.iter().cloned().collect();
        let callback_socket_requested = descriptor.env_injections.iter().any(|injection| {
            injection.source
                == ryeos_engine::protocol_vocabulary::EnvInjectionSource::CallbackSocketPath
        });
        let callback_ipc_requested = descriptor.callback_channel
            != ryeos_engine::protocol_vocabulary::CallbackChannel::None
            || callback_socket_requested;
        let isolation_daemon_socket_path =
            callback_ipc_requested.then_some(callback.socket_path.as_path());

        let callback_socket_path = callback
            .socket_path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("runtime callback socket path is not valid UTF-8"))?
            .to_owned();
        let project_path_string = project_path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("runtime project path is not valid UTF-8"))?
            .to_owned();
        let callback_bindings = ryeos_engine::protocols::CallbackBindings {
            socket_path: callback_socket_path,
            token: callback.token.clone(),
        };
        let timeout = effective_runtime_timeout(timeout_secs, aggregate_deadline_at_ms)?;
        let build_request = ryeos_engine::protocols::BuildRequest {
            item_ref,
            binary_path: Path::new(binary),
            args: &["--project-path".to_string(), project_path_string],
            cwd: project_path,
            project_path,
            project_state_scope,
            thread_id,
            callback: Some(&callback_bindings),
            launch_envelope: Some(envelope),
            timeout,
            acting_principal,
            cas_root,
            thread_auth_token: Some(thread_auth_token),
        };
        let mut spec = ryeos_engine::protocols::build_subprocess_spec(descriptor, &build_request)
            .map_err(|error| anyhow::anyhow!("builder failed: {error}"))?;

        let protocol_bindings = spec.env.iter().map(|(key, value)| {
            let source = descriptor
                .env_injections
                .iter()
                .find(|injection| injection.name == *key)
                .map(|injection| injection.source)
                .ok_or_else(|| {
                    anyhow::anyhow!("protocol builder emitted undeclared env `{key}`")
                })?;
            Ok(ryeos_app::env_contract::EnvBinding::new(
                key.clone(),
                value.clone(),
                ryeos_app::env_contract::EnvSourceDetail::ProtocolInjection { source },
            ))
        });
        let mut protocol_bindings: Vec<_> = protocol_bindings.collect::<Result<Vec<_>>>()?;
        if let Some(checkpoint_dir) = checkpoint_dir {
            let checkpoint_dir = checkpoint_dir
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("runtime checkpoint path is not valid UTF-8"))?;
            protocol_bindings.push(ryeos_app::env_contract::EnvBinding::new(
                "RYEOS_CHECKPOINT_DIR",
                checkpoint_dir,
                ryeos_app::env_contract::EnvSourceDetail::DaemonResume,
            ));
            if is_resume {
                protocol_bindings.push(ryeos_app::env_contract::EnvBinding::new(
                    "RYEOS_RESUME",
                    "1",
                    ryeos_app::env_contract::EnvSourceDetail::DaemonResume,
                ));
            }
        }
        // The sealed realization identity travels with the spawn: a runtime (or
        // any tool it hosts) references the admitted set from here rather than
        // re-observing content the contract forbids it to re-verify live.
        if let Some(bound) = &external_realizations {
            protocol_bindings.push(ryeos_app::env_contract::EnvBinding::new(
                "RYEOS_EXTERNAL_REALIZATIONS",
                bound.sealed_set_env(),
                ryeos_app::env_contract::EnvSourceDetail::PerSpawnDaemon,
            ));
        }
        if let Some(bound) = &source_closure {
            protocol_bindings.push(ryeos_app::env_contract::EnvBinding::new(
                "RYEOS_ADMITTED_SOURCE",
                bound.sealed_identity_env(),
                ryeos_app::env_contract::EnvSourceDetail::PerSpawnDaemon,
            ));
        }

        let declared_secret_bindings = secret_map
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()));
        spec.env = ryeos_app::env_contract::EnvContractBuilder::new()
            .with_base_allowlist(std::env::vars_os().map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.to_string_lossy().into_owned(),
                )
            }))?
            .with_daemon_roots(roots)?
            .with_bindings(
                ryeos_app::env_contract::EnvSourceKind::DeclaredSecret,
                declared_secret_bindings,
            )?
            .with_typed_bindings(protocol_bindings)?
            .build();

        let request = super::super::lillux_bridge::to_lillux_request(&spec)?;
        Ok((spec, request, isolation_daemon_socket_path))
    })()
    .map_err(|error| settle_unattached_runtime_failure(state, thread_id, launch_owner, error))?;
    let isolation_item_ref = item_ref.to_string();
    let mut admitted_mounts = external_realizations
        .as_ref()
        .map(|bound| bound.mounts().to_vec())
        .unwrap_or_default();
    if let Some(source) = &source_closure {
        admitted_mounts.extend_from_slice(source.mounts());
    }
    let workspace_view = if project_authority
        == ryeos_engine::isolation::IsolationProjectAuthority::RuntimeWorkspace
    {
        match super::super::runner::borrow_bound_workspace_view(
            state,
            workspace_lifeline.as_ref(),
            thread_id,
        ) {
            Ok(view) => view,
            Err(error) => {
                drop(request);
                return Err(settle_unattached_runtime_failure(
                    state,
                    thread_id,
                    launch_owner,
                    error,
                ));
            }
        }
    } else {
        // Private immutable/sparse process inputs are not the subject's shared
        // workspace. Disabled RuntimeWorkspace still checks borrower admission
        // above; its original owner explicitly returns no template descriptor.
        None
    };
    let applied = match isolation.apply_awaiting_attachment_with_provenance(
        request,
        ryeos_engine::isolation::IsolationLaunchContext {
            project_path: &spec.project_path,
            project_authority,
            immutable_project: immutable_project.as_ref(),
            workspace_view: workspace_view.as_ref(),
            filesystem_authority_ceiling,
            network_authority_ceiling,
            live_access: live_access.as_ref(),
            state_root,
            checkpoint_dir,
            checkpoint_authority,
            daemon_socket_path: isolation_daemon_socket_path,
            bundle_roots: &envelope.roots.bundle_roots,
            node_trusted_keys_dir: Some(&envelope.roots.node_trusted_keys_dir),
            verified_code: &[],
            verified_command: Some(verified_command),
            external_read_only_mounts: &admitted_mounts,
            writable_runtime_view_mounts: &[],
            target_channels: &[],
            item_ref: &isolation_item_ref,
            thread_id,
        },
    ) {
        Ok(applied) => applied,
        Err(error) => {
            // This exact seam only compiles a held-launch request, including
            // its generation checks; it never consumes the request via spawn.
            // The failed apply has already dropped that request. Release the
            // borrowed descriptor before settling membership. This is not a
            // classification of arbitrary EngineError values as no-contact.
            drop(workspace_view);
            return Err(settle_unattached_runtime_failure(
                state,
                thread_id,
                launch_owner,
                anyhow::Error::new(error).context("isolation apply failed"),
            ));
        }
    };
    drop(workspace_view);
    let request = applied.request;
    if let Err(error) = state
        .state_store
        .seed_isolation_provenance(thread_id, applied.provenance)
        .context("persist managed-runtime isolation provenance")
    {
        // No .spawn() has consumed this successfully prepared request. Release
        // its descriptor lifelines before settling the borrow, not via Drop
        // after the launch claim has disappeared.
        drop(request);
        return Err(settle_unattached_runtime_failure(
            state,
            thread_id,
            launch_owner,
            error,
        ));
    }
    let spawned = match request.spawn() {
        Ok(spawned) => spawned,
        Err(result) => {
            emit_captured_child_observation_records(
                thread_id,
                &item_ref.to_string(),
                observation_declarations,
                &result.stderr,
                result.stderr_truncated,
            );
            if result.aborted_before_attachment.is_some() {
                let failure = runtime_failure_result(
                    &result.stderr,
                    result.timed_out,
                    result.output_limit_exceeded.map(|limit| limit.as_str()),
                );
                let error = anyhow::anyhow!(
                    "managed runtime launch was aborted before attachment: {}",
                    failure.result.unwrap_or(Value::Null)
                );
                return Err(settle_unattached_runtime_failure(
                    state,
                    thread_id,
                    launch_owner,
                    error,
                ));
            }
            // No checked abort proof: preserve the failed outcome but never
            // turn it into permission to erase borrowed workspace membership.
            return Ok(SpawnedRuntime {
                thread_id: thread_id.to_string(),
                runtime_ref: item_ref.to_string(),
                observation_declarations: observation_declarations.clone(),
                process: None,
                attached_process: None,
                workspace_lifeline,
                external_realizations,
                source_closure,
                immediate_result: Some(runtime_failure_result(
                    &result.stderr,
                    result.timed_out,
                    result.output_limit_exceeded.map(|limit| limit.as_str()),
                )),
            });
        }
    };
    #[cfg(target_os = "linux")]
    let process_identity_result =
        ryeos_app::process::capture_execution_process_identity_from_pidfd(
            spawned.pid() as i64,
            Some(spawned.pgid()),
            spawned.pidfd(),
        )
        .context("capture held managed-runtime identity from Lillux pidfd");
    #[cfg(not(target_os = "linux"))]
    let process_identity_result = ryeos_app::process::capture_execution_process_identity(
        spawned.pid() as i64,
        Some(spawned.pgid()),
    )
    .context("capture held managed-runtime identity");
    let process_identity = match process_identity_result {
        Ok(identity) => identity,
        Err(error) => {
            return Err(match spawned.abort_and_reap() {
                Err(cleanup) => error.context(format!("pending-process cleanup failed: {cleanup}")),
                Ok(_) => settle_unattached_runtime_failure(state, thread_id, launch_owner, error),
            });
        }
    };
    // The runtime cannot self-attach before release. An existing identity at
    // this boundary is therefore an invariant violation, not an adoption race.
    let attach_params = ryeos_app::thread_lifecycle::ThreadAttachProcessParams {
        thread_id: thread_id.to_string(),
        pid: spawned.pid() as i64,
        pgid: spawned.pgid(),
        process_identity: Some(process_identity.clone()),
        metadata: None,
        // Spawn metadata was seeded before launch. An empty self-attach
        // preserves it while establishing the immutable process identity.
        launch_metadata: ryeos_app::launch_metadata::RuntimeLaunchMetadata::default(),
    };
    let attach_result = if rearm_native_resume_budget_after_attach {
        state
            .threads
            .attach_new_process_owned_rearming_resume_budget(&attach_params, launch_owner)
    } else {
        state
            .threads
            .attach_new_process_owned(&attach_params, launch_owner)
    };
    if let Err(error) = attach_result {
        let error = error.context("attach held managed runtime process identity");
        return match spawned.abort_and_reap() {
            Err(cleanup) => {
                Err(error.context(format!("pending-process cleanup failed: {cleanup}")))
            }
            Ok(_) => Err(settle_unattached_runtime_failure(
                state,
                thread_id,
                launch_owner,
                error,
            )),
        };
    }
    let mut attached_process =
        match AttachedProcessGuard::new(state, thread_id, launch_owner, process_identity.clone()) {
            Ok(guard) => guard,
            Err(error) => {
                let cleanup = spawned
                    .abort_and_reap()
                    .map_err(anyhow::Error::from)
                    .and_then(|_| {
                        super::super::runner::clear_finished_process(
                            state,
                            thread_id,
                            &process_identity,
                            launch_owner,
                        )
                    });
                return Err(match cleanup {
                    Ok(()) => {
                        settle_unattached_runtime_failure(state, thread_id, launch_owner, error)
                    }
                    Err(cleanup) => error.context(format!(
                        "pending attachment owner cleanup failed; retaining authority: {cleanup:#}"
                    )),
                });
            }
        };
    if let Err(error) = super::super::runner::activate_workspace_after_process_attachment(
        state,
        workspace_lifeline.as_ref(),
        owns_workspace,
        thread_id,
        launch_owner,
        &process_identity,
    ) {
        let cleanup = spawned
            .abort_and_reap()
            .map_err(anyhow::Error::from)
            .and_then(|_| attached_process.settle_after_reap())
            .err();
        let error = error.context("activate managed-runtime workspace after attachment");
        return Err(match cleanup {
            Some(cleanup) => error.context(format!("pending-process cleanup failed: {cleanup}")),
            None => settle_unattached_runtime_failure(state, thread_id, launch_owner, error),
        });
    }
    if let Err(error) =
        state
            .threads
            .authorize_process_release_owned(thread_id, &process_identity, launch_owner)
    {
        let cleanup = spawned
            .abort_and_reap()
            .map_err(anyhow::Error::from)
            .and_then(|_| attached_process.settle_after_reap())
            .err();
        if let Some(cleanup) = cleanup {
            return Err(error.context(format!(
                "pending-process cleanup failed; retaining authority: {cleanup}"
            )));
        }
        let stop_settlement =
            super::super::process_attachment::finalize_requested_stop_if_present(state, thread_id);
        let error = match stop_settlement {
            Ok(true) => {
                anyhow::anyhow!("managed runtime stopped before attachment release: {error}")
            }
            Ok(false) => error.context("authorize managed runtime release after durable attachment"),
            Err(stop_error) => error.context(format!(
                "authorize managed runtime release after durable attachment; stop settlement also failed: {stop_error:#}"
            )),
        };
        return Err(settle_unattached_runtime_failure(
            state,
            thread_id,
            launch_owner,
            error,
        ));
    }
    let spawned = match spawned.release_after_attachment() {
        Ok(spawned) => spawned,
        Err(error) => {
            if !error.cleanup_is_settled() {
                // A dead numeric process/group is not proof that the selected
                // scope and release-wrapper resources were settled. Preserve
                // the exact attachment and membership for existing recovery.
                return Err(anyhow::Error::new(error)
                    .context("managed runtime release cleanup is unproved; retaining authority"));
            }
            let settlement = attached_process.settle_after_reap();
            return Err(match settlement {
                Ok(()) => settle_unattached_runtime_failure(
                    state,
                    thread_id,
                    launch_owner,
                    anyhow::Error::new(error)
                        .context("release managed runtime after durable process attachment"),
                ),
                Err(settlement) => anyhow::Error::new(error).context(format!(
                    "release failed; exact attachment retained: {settlement:#}"
                )),
            });
        }
    };
    Ok(SpawnedRuntime {
        thread_id: thread_id.to_string(),
        runtime_ref: item_ref.to_string(),
        observation_declarations: observation_declarations.clone(),
        process: Some(spawned),
        attached_process: Some(attached_process),
        workspace_lifeline,
        external_realizations,
        source_closure,
        immediate_result: None,
    })
}

/// Called only from a bounded pre-contact region or after checked abort/reap.
/// An arbitrary engine error, JoinError, cancellation or Drop is not proof;
/// isolation refusal qualifies only at the compile-only request seam above.
/// The caller retains its workspace/source lifelines throughout settlement;
/// any launch request/held process must already have been consumed or dropped.
fn settle_unattached_runtime_failure(
    state: &ryeos_app::state::AppState,
    thread_id: &str,
    launch_owner: &str,
    error: anyhow::Error,
) -> anyhow::Error {
    let settlement = (|| -> Result<()> {
        // Shutdown owns recovery once attachment admission closes. An active
        // request must not erase the coordinator's retained obligations.
        if !state.state_store.process_attachment_admission_is_open() {
            return Ok(());
        }
        state
            .state_store
            .assert_launch_owner(thread_id, launch_owner)?;
        if !super::super::process_attachment::finalize_requested_stop_if_present(state, thread_id)?
        {
            let code = error
                .downcast_ref::<DispatchError>()
                .map(DispatchError::code)
                .unwrap_or("pre_runtime_failure");
            if let Err(finalize_error) = state.threads.finalize_thread_owned(
                &ryeos_app::thread_lifecycle::ThreadFinalizeParams {
                    thread_id: thread_id.to_owned(),
                    status: "failed".to_owned(),
                    outcome_code: Some(code.to_owned()),
                    result: None,
                    error: Some(json!({"code": code, "message": format!("{error:#}")})),
                    metadata: None,
                    artifacts: Vec::new(),
                    final_cost: None,
                    summary_json: None,
                },
                launch_owner,
            ) {
                if !state.threads.get_thread(thread_id)?.is_some_and(|thread| {
                    ryeos_app::state_store::is_terminal_status(&thread.status)
                }) {
                    return Err(finalize_error);
                }
            }
        }
        if let Some(binding) = state.state_store.thread_workspace_binding(thread_id)? {
            let owner: ryeos_app::runtime_db::LaunchOwner = serde_json::from_str(launch_owner)?;
            if binding.borrower_launch_owner != owner {
                anyhow::bail!(
                    "settled runtime cannot retire another workspace borrower's membership"
                );
            }
            if !state
                .state_store
                .settle_thread_workspace_owned(thread_id, &binding)?
            {
                anyhow::bail!(
                    "settled runtime retains process authority or unsettled workspace descendants"
                );
            }
        }
        Ok(())
    })();
    match settlement {
        Ok(()) => error,
        Err(cleanup) => error.context(format!(
            "managed runtime failure settlement retained authority: {cleanup:#}"
        )),
    }
}

fn effective_runtime_timeout(
    execution_timeout_secs: u64,
    aggregate_deadline_at_ms: Option<i64>,
) -> Result<std::time::Duration> {
    let execution = (execution_timeout_secs != 0)
        .then(|| std::time::Duration::from_secs(execution_timeout_secs));
    let aggregate = aggregate_deadline_at_ms
        .map(|deadline| {
            let remaining_ms = deadline
                .checked_sub(lillux::time::timestamp_millis())
                .filter(|remaining| *remaining > 0)
                .ok_or_else(|| DispatchError::LaunchPreparationFailed {
                    code: "budget_exhausted".to_owned(),
                    message: "aggregate execution duration elapsed before runtime spawn".to_owned(),
                    classification: "policy".to_owned(),
                    binding: None,
                    details: Box::new(BTreeMap::new()),
                })?;
            Ok::<_, anyhow::Error>(std::time::Duration::from_millis(u64::try_from(
                remaining_ms,
            )?))
        })
        .transpose()?;
    Ok(match (execution, aggregate) {
        (Some(execution), Some(aggregate)) => execution.min(aggregate),
        (Some(timeout), None) | (None, Some(timeout)) => timeout,
        (None, None) => std::time::Duration::ZERO,
    })
}

const MAX_CAPTURED_CHILD_OBSERVATION_LINES: usize = 1024;

fn emit_captured_child_observation_records(
    expected_thread_id: &str,
    runtime_ref: &str,
    declarations: &BTreeMap<String, ryeos_engine::runtime_registry::ChildObservationDecl>,
    stderr: &str,
    stderr_truncated: bool,
) {
    let mut observed_lines = 0usize;
    let mut emitted = 0usize;
    let mut counts = BTreeMap::<String, u32>::new();
    let mut line_limit_exceeded = false;
    for line in stderr.lines() {
        let Some(encoded) =
            line.strip_prefix(ryeos_runtime::events::CAPTURED_CHILD_OBSERVATION_PREFIX)
        else {
            continue;
        };
        if observed_lines == MAX_CAPTURED_CHILD_OBSERVATION_LINES {
            line_limit_exceeded = true;
            break;
        }
        observed_lines = observed_lines.saturating_add(1);
        let Some(record) = ryeos_runtime::events::RuntimeChildObservationRecord::decode_declared(
            expected_thread_id,
            encoded,
            declarations,
        ) else {
            tracing::warn!(
                thread_id = expected_thread_id,
                runtime_ref,
                "discarding undeclared or invalid captured child observation"
            );
            continue;
        };
        let declaration = declarations
            .get(&record.event)
            .expect("decoded observations are declaration-bound");
        let count = counts.entry(record.event.clone()).or_default();
        if *count >= declaration.max_records {
            tracing::warn!(
                thread_id = expected_thread_id,
                runtime_ref,
                child_event = record.event,
                accepted_record_limit = declaration.max_records,
                "runtime exceeded its signed child-observation record limit"
            );
            continue;
        }
        *count = count.saturating_add(1);
        let normalized = match serde_json::to_string(&record) {
            Ok(normalized) => normalized,
            Err(error) => {
                tracing::warn!(
                    thread_id = expected_thread_id,
                    runtime_ref,
                    %error,
                    "failed to normalize a declared child observation"
                );
                continue;
            }
        };
        emitted = emitted.saturating_add(1);
        tracing::info!(
            event = "runtime_child_observation_record",
            child_event = record.event,
            child_schema_version = record.schema_version,
            child_clock_domain = record.clock_domain,
            invocation_id = record.invocation_id.as_deref(),
            thread_id = expected_thread_id,
            runtime_ref,
            child_observation_json = normalized,
            "captured signed-runtime-declared child observation"
        );
    }
    if line_limit_exceeded {
        tracing::warn!(
            thread_id = expected_thread_id,
            runtime_ref,
            accepted_observation_line_limit = MAX_CAPTURED_CHILD_OBSERVATION_LINES,
            "runtime emitted more child observation lines than the daemon accepts"
        );
    }
    if stderr_truncated {
        tracing::warn!(
            thread_id = expected_thread_id,
            runtime_ref,
            emitted_observation_records = emitted,
            "runtime stderr was truncated; child observations may be incomplete"
        );
    }
}
fn runtime_failure_result(
    stderr: &str,
    timed_out: bool,
    output_limit: Option<&str>,
) -> RuntimeResult {
    RuntimeResult {
        success: false,
        status: if timed_out {
            RuntimeResultStatus::TimedOut
        } else {
            RuntimeResultStatus::Failed
        },
        thread_id: String::new(),
        result: Some(match output_limit {
            Some(stream) => json!({
                "code": format!("output_limit:{stream}"),
                "message": stderr,
                "stream": stream,
            }),
            None => json!(stderr),
        }),
        outputs: Value::Null,
        cost: None,
        warnings: Vec::new(),
    }
}

fn decode_runtime_stdout(stdout: &str) -> Result<RuntimeProcessOutcome> {
    ryeos_runtime::process_outcome::decode_runtime_process_stdout(stdout).map_err(|error| {
        anyhow::anyhow!(
            "failed to parse runtime stdout: {}\nstdout: {}",
            error,
            stdout_prefix(stdout, 500)
        )
    })
}

fn stdout_prefix(stdout: &str, max_bytes: usize) -> &str {
    let mut end = stdout.len().min(max_bytes);
    while !stdout.is_char_boundary(end) {
        end -= 1;
    }
    &stdout[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "linux")]
    use crate::augmentations::compose_context_positions::tests::bound_runtime_children_fixture;

    #[cfg(target_os = "linux")]
    #[test]
    fn managed_precontact_failure_settles_only_exact_bound_child() {
        let (_temp, state) = bound_runtime_children_fixture();
        let store = &state.state_store;
        let child = "T-runtime-child";
        let owner = store.get_launch_claim(child).unwrap().unwrap();
        let sibling = store.thread_workspace_binding("T-runtime-sibling").unwrap();
        let parent = store.thread_workspace_binding("T-runtime-parent").unwrap();
        // Exercise the real managed terminalization/settlement helper with the
        // actual env-validator error. This is not a complete spawn qualification.
        let error = match ryeos_app::env_contract::EnvContractBuilder::new().with_typed_bindings([
            ryeos_app::env_contract::EnvBinding::new(
                "PATH",
                "/ambient",
                ryeos_app::env_contract::EnvSourceDetail::RuntimeDescriptor,
            ),
        ]) {
            Ok(_) => panic!("descriptor PATH must remain forbidden"),
            Err(error) => anyhow::Error::new(error),
        };
        let result = settle_unattached_runtime_failure(&state, child, &owner.claimed_by, error);
        assert!(!format!("{result:#}").contains("settlement retained authority"));
        assert_eq!(store.get_thread(child).unwrap().unwrap().status, "failed");
        assert!(store.thread_workspace_binding(child).unwrap().is_none());
        assert_eq!(
            store.thread_workspace_binding("T-runtime-sibling").unwrap(),
            sibling
        );
        assert_eq!(
            store.thread_workspace_binding("T-runtime-parent").unwrap(),
            parent
        );
        assert_eq!(
            store.get_launch_claim(child).unwrap().unwrap().claimed_by,
            owner.claimed_by
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn managed_wrong_owner_and_unknown_contact_preserve_membership() {
        let (_temp, state) = bound_runtime_children_fixture();
        let store = &state.state_store;
        let child = "T-runtime-child";
        let binding = store.thread_workspace_binding(child).unwrap();
        let wrong = store
            .get_launch_claim("T-runtime-sibling")
            .unwrap()
            .unwrap();
        let error = settle_unattached_runtime_failure(
            &state,
            child,
            &wrong.claimed_by,
            anyhow::anyhow!("preparation failed"),
        );
        assert!(format!("{error:#}").contains("settlement retained authority"));
        assert_eq!(store.thread_workspace_binding(child).unwrap(), binding);
        assert!(!ryeos_app::state_store::is_terminal_status(
            &store.get_thread(child).unwrap().unwrap().status
        ));

        // The actual unproved-spawn-result wait path does not invoke the
        // settled helper or grant permission to erase an unattached borrower.
        let runtime = SpawnedRuntime {
            thread_id: child.to_owned(),
            runtime_ref: "runtime:test/unproved".to_owned(),
            observation_declarations: BTreeMap::new(),
            process: None,
            attached_process: None,
            workspace_lifeline: None,
            external_realizations: None,
            source_closure: None,
            immediate_result: Some(runtime_failure_result("no PID (not proof)", false, None)),
        };
        assert!(!runtime.wait().settled_attached_wait);
        assert_eq!(store.thread_workspace_binding(child).unwrap(), binding);
        let claim = store.get_launch_claim(child).unwrap().unwrap();
        store
            .release_active_thread_launch_claim(child, &claim.claim_id, &claim.claimed_by)
            .unwrap();
        assert!(
            store
                .settle_thread_workspace_owned(child, binding.as_ref().unwrap())
                .is_err()
        );
        // The quarantine query addresses the original workspace owner.
        let parent = store.get_launch_claim("T-runtime-parent").unwrap().unwrap();
        store
            .release_active_thread_launch_claim(
                "T-runtime-parent",
                &parent.claim_id,
                &parent.claimed_by,
            )
            .unwrap();
        assert!(
            store
                .has_retained_workspace_quarantine("T-runtime-parent")
                .unwrap()
        );
        assert_eq!(store.thread_workspace_binding(child).unwrap(), binding);
    }

    #[test]
    fn immediate_spawn_result_is_not_an_attached_wait_proof() {
        let runtime = SpawnedRuntime {
            thread_id: "T-immediate".to_string(),
            runtime_ref: "runtime:test/immediate".to_string(),
            observation_declarations: BTreeMap::new(),
            process: None,
            attached_process: None,
            workspace_lifeline: None,
            external_realizations: None,
            source_closure: None,
            immediate_result: Some(runtime_failure_result("spawn failed", false, None)),
        };

        let waited = runtime.wait();
        assert!(!waited.settled_attached_wait);
        assert!(matches!(
            waited.result,
            Ok(RuntimeProcessOutcome::Terminal(_))
        ));
    }

    #[test]
    fn subprocess_failure_preserves_stderr_and_failed_status() {
        let result = runtime_failure_result("permission denied", false, None);

        assert!(!result.success);
        assert_eq!(result.status, RuntimeResultStatus::Failed);
        assert_eq!(result.result, Some(json!("permission denied")));
        assert_eq!(result.outputs, Value::Null);
    }

    #[test]
    fn subprocess_timeout_uses_timed_out_status() {
        assert_eq!(
            runtime_failure_result("deadline", true, None).status,
            RuntimeResultStatus::TimedOut
        );
    }

    #[test]
    fn subprocess_output_limit_preserves_the_explicit_reason() {
        let result = runtime_failure_result("retention exceeded", false, Some("stdout"));

        assert_eq!(result.status, RuntimeResultStatus::Failed);
        assert_eq!(
            result.result.as_ref().unwrap()["code"],
            "output_limit:stdout"
        );
        assert_eq!(result.result.as_ref().unwrap()["stream"], "stdout");
    }

    #[test]
    fn stdout_decode_error_keeps_runtime_context() {
        let error = decode_runtime_stdout("not-json").unwrap_err().to_string();

        assert!(error.contains("failed to parse runtime stdout"));
        assert!(error.contains("stdout: not-json"));
    }

    #[test]
    fn stdout_decode_rejects_success_status_contradiction() {
        let stdout = serde_json::json!({
            "success": false,
            "status": "completed",
            "thread_id": "T-test",
            "outputs": null,
            "warnings": [],
        })
        .to_string();

        let error = decode_runtime_stdout(&stdout).unwrap_err().to_string();
        assert!(error.contains("failed to parse runtime stdout"));
        assert!(error.contains("contradicts `status` `completed`"));
    }

    #[test]
    fn stdout_decode_error_truncates_on_utf8_boundary() {
        let stdout = format!("{}é", "x".repeat(499));

        let error = decode_runtime_stdout(&stdout).unwrap_err().to_string();
        assert!(error.contains("failed to parse runtime stdout"));
        assert!(error.contains(&"x".repeat(499)));
    }

    #[test]
    fn captured_child_observation_is_declaration_and_thread_bound() {
        let declarations = BTreeMap::from([(
            "runtime_stage".to_string(),
            ryeos_engine::runtime_registry::ChildObservationDecl {
                schema_version: 3,
                clock_domain: "runtime_process_monotonic".to_string(),
                max_records: 1,
                max_record_bytes: 4096,
            },
        )]);
        let encoded = json!({
            "event": "runtime_stage",
            "schema_version": 3,
            "clock_domain": "runtime_process_monotonic",
            "invocation_id": "invocation-1",
            "thread_id": "T-expected",
            "runtime_owned_metric": 42
        })
        .to_string();

        let decoded = ryeos_runtime::events::RuntimeChildObservationRecord::decode_declared(
            "T-expected",
            &encoded,
            &declarations,
        )
        .expect("declared observation");
        assert_eq!(decoded.payload["runtime_owned_metric"], json!(42));
        assert!(
            ryeos_runtime::events::RuntimeChildObservationRecord::decode_declared(
                "T-other",
                &encoded,
                &declarations,
            )
            .is_none()
        );

        let mut wrong_version: Value = serde_json::from_str(&encoded).unwrap();
        wrong_version["schema_version"] = json!(2);
        assert!(
            ryeos_runtime::events::RuntimeChildObservationRecord::decode_declared(
                "T-expected",
                &wrong_version.to_string(),
                &declarations,
            )
            .is_none()
        );
    }
}
