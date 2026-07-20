use std::sync::Arc;

#[cfg(target_os = "linux")]
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

use anyhow::{anyhow, Context, Result};
use arc_swap::{ArcSwap, ArcSwapOption};
use serde_json::json;
#[cfg(test)]
use serde_json::Value;
use tokio::net::UnixListener;
use tokio::sync::Semaphore;

#[cfg(test)]
use crate::uds::protocol::{RpcRequest, RpcResponse};
use ryeos_app::bundle_event_service::{
    BundleEventAppendParams, BundleEventMaterializeAttachmentParams, BundleEventReadChainParams,
    BundleEventScanParams, BundleEventService,
};
use ryeos_app::command_service::{CommandClaimParams, CommandCompleteParams, CommandSubmitParams};
use ryeos_app::event_store_service::{
    EventAppendBatchParams, EventAppendParams, EventReplayParams,
};
use ryeos_app::live_input_queue::MAX_LIVE_INPUT_POLL_RESPONSE_BYTES;
use ryeos_app::runtime_item_author_service::{RuntimeAuthorItemParams, RuntimeItemAuthorService};
use ryeos_app::runtime_project_snapshot_service::{
    RuntimeProjectSnapshotRequest, RuntimeProjectSnapshotService,
};
use ryeos_app::runtime_vault_service::{
    RuntimeVaultListParams, RuntimeVaultPutParams, RuntimeVaultRefParams, RuntimeVaultService,
};
use ryeos_app::state::AppState;
use ryeos_app::thread_lifecycle::{
    ArtifactPublishParams, ThreadAttachProcessParams, ThreadContinuationParams, ThreadGetParams,
    ThreadMarkRunningParams,
};
use ryeos_runtime::callback_client::MAX_RUNTIME_REPLAY_PAGE_LIMIT;

mod routing;
mod transport;

#[cfg(test)]
pub(crate) use routing::dispatch;

struct DynamicServerInner {
    lifecycle: ArcSwap<ryeos_node::LifecycleResponse>,
    application: ArcSwapOption<AppState>,
}

/// Request-level UDS publication state. The listener can begin serving with
/// only lifecycle identity/progress, then publish AppState once callbacks are
/// safe. Each frame reloads this state, so a connection accepted during boot
/// observes later application and Ready publication.
#[derive(Clone)]
pub struct DynamicServerState {
    inner: Arc<DynamicServerInner>,
}

impl DynamicServerState {
    pub fn bootstrap(lifecycle: ryeos_node::LifecycleResponse) -> Result<Self> {
        lifecycle
            .validate()
            .map_err(|message| anyhow!("invalid initial lifecycle response: {message}"))?;
        Ok(Self {
            inner: Arc::new(DynamicServerInner {
                lifecycle: ArcSwap::from_pointee(lifecycle),
                application: ArcSwapOption::empty(),
            }),
        })
    }

    pub fn publish_application(&self, state: Arc<AppState>) {
        self.inner.application.store(Some(state));
    }

    pub fn unpublish_application(&self) {
        self.inner.application.store(None);
    }

    pub fn publish_lifecycle(&self, lifecycle: ryeos_node::LifecycleResponse) -> Result<()> {
        lifecycle
            .validate()
            .map_err(|message| anyhow!("invalid lifecycle response: {message}"))?;
        let current = self.lifecycle();
        if lifecycle.identity != current.identity {
            anyhow::bail!("lifecycle identity cannot change after listener publication");
        }
        if current.status != ryeos_node::LifecycleWireState::Starting {
            anyhow::bail!("terminal lifecycle state cannot be republished");
        }
        if lifecycle.startup.sequence <= current.startup.sequence {
            anyhow::bail!("lifecycle publication sequence must increase");
        }
        if lifecycle.ready && self.inner.application.load().is_none() {
            anyhow::bail!("cannot publish Ready before application state");
        }
        self.inner.lifecycle.store(Arc::new(lifecycle));
        Ok(())
    }

    pub fn lifecycle(&self) -> Arc<ryeos_node::LifecycleResponse> {
        self.inner.lifecycle.load_full()
    }

    pub fn application_is_published(&self) -> bool {
        self.inner.application.load().is_some()
    }

    fn application(&self) -> Option<Arc<AppState>> {
        self.inner.application.load_full()
    }
}

pub async fn serve(listener: UnixListener, state: Arc<AppState>) -> Result<()> {
    let dynamic = DynamicServerState::bootstrap(ready_lifecycle_response(&state))?;
    dynamic.publish_application(state);
    serve_dynamic(listener, dynamic).await
}

pub async fn serve_dynamic(listener: UnixListener, state: DynamicServerState) -> Result<()> {
    let mut connections = tokio::task::JoinSet::new();
    let connection_slots = Arc::new(Semaphore::new(MAX_UDS_CONNECTIONS));
    let frame_bytes = Arc::new(Semaphore::new(MAX_UDS_IN_FLIGHT_FRAME_BYTES));
    let mut shutdown_rx = crate::subscribe_shutdown();
    let shutdown = async move {
        // Subscribe before checking the flag so a request racing listener startup
        // is observed either through the durable process-local flag or the channel.
        if crate::shutdown_requested() {
            return;
        }
        match shutdown_rx.as_mut() {
            Some(receiver) => {
                let _ = receiver.recv().await;
            }
            None => std::future::pending::<()>().await,
        }
    };
    tokio::pin!(shutdown);

    let listener_result = loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => break Ok(()),
            permit = Arc::clone(&connection_slots).acquire_owned() => {
                let connection_permit = permit.context("UDS connection semaphore closed")?;
                let accepted = tokio::select! {
                    biased;
                    _ = &mut shutdown => break Ok(()),
                    accepted = listener.accept() => accepted,
                };
                let (stream, _) = match accepted {
                    Ok(accepted) => accepted,
                    Err(error) => break Err(error).context("uds accept failed"),
                };
                let state = state.clone();
                let frame_bytes = Arc::clone(&frame_bytes);
                connections.spawn(async move {
                    let _connection_permit = connection_permit;
                    transport::handle_connection(stream, state, frame_bytes).await
                });
            }
            joined = connections.join_next(), if !connections.is_empty() => {
                if let Some(joined) = joined {
                    report_connection_exit(joined);
                }
            }
        }
    };

    // No connection may outlive the supervised UDS listener. In particular,
    // shutdown must cancel frames already blocked in reads or runtime dispatch.
    connections.abort_all();
    while let Some(joined) = connections.join_next().await {
        report_connection_exit(joined);
    }

    listener_result
}

fn report_connection_exit(joined: std::result::Result<Result<()>, tokio::task::JoinError>) {
    match joined {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!(%error, "uds connection error"),
        Err(error) if error.is_cancelled() => {}
        Err(error) => tracing::error!(%error, "uds connection task failed"),
    }
}

fn ready_lifecycle_response(state: &AppState) -> ryeos_node::LifecycleResponse {
    let ready_at = lillux::time::iso8601_now();
    let build = ryeos_app::build_info::get();
    let identity = ryeos_node::LifecycleIdentity {
        pid: std::process::id(),
        bind: state.config.bind.to_string(),
        uds_path: state.config.uds_path.clone(),
        app_root: state.config.app_root.clone(),
        started_at: state.started_at_iso.clone(),
        version: build.version.to_string(),
        revision: Some(build.revision.to_string()),
        build_date: Some(build.build_date.to_string()),
    };
    let mut startup = ryeos_node::StartupSnapshot::bootstrapping(&state.started_at_iso);
    startup.phase_started_at = ready_at.clone();
    startup.elapsed_ms = state.started_at.elapsed().as_millis() as u64;
    let mut response = ryeos_node::LifecycleResponse::running(identity, ready_at, startup);
    response.thread_projection =
        serde_json::to_value(state.state_store.projection_health_snapshot()).ok();
    response
}

/// Kernel-authenticated identity of the process that opened this Unix stream.
/// The pidfd, not the reusable numeric PID, remains authoritative for the
/// connection lifetime.
pub(crate) struct AuthenticatedUnixPeer {
    pid: i64,
    #[cfg(target_os = "linux")]
    pidfd: OwnedFd,
}

impl AuthenticatedUnixPeer {
    fn pid(&self) -> i64 {
        self.pid
    }

    #[cfg(target_os = "linux")]
    fn pidfd(&self) -> BorrowedFd<'_> {
        self.pidfd.as_fd()
    }
}

const MAX_UDS_CONNECTIONS: usize = 32;
const MAX_UDS_IN_FLIGHT_FRAME_BYTES: usize = 32 * 1024 * 1024;

const TRANSPORT_FIELDS: &[&str] = &["callback_token", "thread_auth_token"];

fn strip_transport_fields(params: &serde_json::Value) -> serde_json::Value {
    match params {
        serde_json::Value::Object(map) => {
            let filtered: serde_json::Map<String, serde_json::Value> = map
                .iter()
                .filter(|(k, _)| !TRANSPORT_FIELDS.contains(&k.as_str()))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            serde_json::Value::Object(filtered)
        }
        other => other.clone(),
    }
}

pub(crate) async fn dispatch_runtime_method(
    method: &str,
    params: &serde_json::Value,
    state: &AppState,
    peer: Option<&AuthenticatedUnixPeer>,
) -> Result<serde_json::Value> {
    // Validate the callback token on ALL runtime.* methods, by access class:
    //
    //  - thread-auth methods (dispatch_action, spawn_follow_child) prove a
    //    per-request `thread_auth_token` against the caller's own thread here,
    //    then do their own stronger validation (callback token + project_path +
    //    server-side trust derivation) in the handler.
    //  - chain *reads* (get_thread / replay) may target any thread in the
    //    capability's own chain — a successor rehydrates by folding its
    //    predecessors. Authorized by state-checked chain membership, never an
    //    exact-thread match.
    //  - everything else (writes + lifecycle: append, finalize, mark_running,
    //    request_continuation, publish_artifact, vault/bundle writes) requires an
    //    exact-thread match. A chain read must never widen into a chain write.
    let mut validated_thread_auth: Option<ryeos_app::callback_token::ThreadAuthState> = None;
    let callback_cap = if is_thread_auth_method(method) {
        // Per-request identity proof against the caller's own thread. Missing or
        // invalid = hard fail, no fallback. The handler re-validates the callback
        // token and derives principal / provenance / caps from server-side state.
        let tat = params
            .get("thread_auth_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing thread_auth_token on {method}"))?;
        let thread_id = params
            .get("thread_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing thread_id on {method}"))?;
        state.thread_auth.validate(tat, thread_id)?;
        None
    } else if matches!(
        method,
        "runtime.poll_input" | "runtime.author_item" | "runtime.project_snapshot"
    ) {
        // runtime.poll_input drains staged operator inputs and persists them as
        // durable `cognition_in` for the running thread. Require BOTH proofs the
        // runtime holds: the per-request thread_auth_token (like dispatch_action)
        // AND the exact-thread callback token (write tier — it appends durable
        // events). runtime.author_item is also a durable signed project write,
        // so it uses the same two-proof boundary. Either proof alone is
        // insufficient.
        let tat = params
            .get("thread_auth_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing thread_auth_token on {method}"))?;
        let thread_id = params
            .get("thread_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing thread_id"))?;
        let thread_auth = state.thread_auth.validate(tat, thread_id)?;
        if matches!(method, "runtime.author_item" | "runtime.project_snapshot") {
            validated_thread_auth = Some(thread_auth);
        }
        let token = params
            .get("callback_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing callback_token"))?;
        Some(
            state
                .callback_tokens
                .validate_token_and_thread(token, thread_id)?,
        )
    } else if is_chain_read_method(method) {
        let token = params
            .get("callback_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing callback_token"))?;
        let cap = state.callback_tokens.validate_token_only(token)?;
        authorize_chain_read(&cap, params, state)?;
        Some(cap)
    } else {
        let token = params
            .get("callback_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing callback_token"))?;
        let thread_id = params
            .get("thread_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing thread_id"))?;
        Some(
            state
                .callback_tokens
                .validate_token_and_thread(token, thread_id)?,
        )
    };

    let callback_launch_owner = callback_cap
        .as_ref()
        .map(|cap| {
            cap.launch_owner
                .as_deref()
                .ok_or_else(|| anyhow!("execution callback capability has no launch owner"))
        })
        .transpose()?;
    if let (Some(cap), Some(owner)) = (callback_cap.as_ref(), callback_launch_owner) {
        state
            .state_store
            .assert_launch_owner(&cap.thread_id, owner)?;
    }

    enforce_runtime_callback_admission(method, params, state)?;

    // Strip transport-level fields before typed deserialization so
    // deny_unknown_fields on the RPC param structs doesn't reject
    // callback_token.
    let clean_params = strip_transport_fields(params);

    match method {
        "runtime.dispatch_action" => {
            ryeos_executor::execution::runtime_dispatch::handle(params, state).await
        }
        "runtime.spawn_follow_child" => {
            ryeos_executor::execution::spawn_follow_child::handle(params, state).await
        }
        "runtime.append_event" => handle_append_event(&clean_params, state),
        "runtime.append_events" => handle_append_event_batch(&clean_params, state),
        "runtime.replay_events" => handle_replay_events(&clean_params, state),
        "runtime.bundle_events_append" => {
            handle_bundle_events_append(&clean_params, state, callback_cap.as_ref())
        }
        "runtime.bundle_events_read_chain" => {
            handle_bundle_events_read_chain(&clean_params, state, callback_cap.as_ref())
        }
        "runtime.bundle_events_scan" => {
            handle_bundle_events_scan(&clean_params, state, callback_cap.as_ref())
        }
        "runtime.bundle_events_materialize_attachment" => {
            handle_bundle_events_materialize_attachment(&clean_params, state, callback_cap.as_ref())
        }
        "runtime.vault_put" => {
            handle_runtime_vault_put(&clean_params, state, callback_cap.as_ref())
        }
        "runtime.vault_get" => {
            handle_runtime_vault_get(&clean_params, state, callback_cap.as_ref())
        }
        "runtime.vault_delete" => {
            handle_runtime_vault_delete(&clean_params, state, callback_cap.as_ref())
        }
        "runtime.vault_list" => {
            handle_runtime_vault_list(&clean_params, state, callback_cap.as_ref())
        }
        "runtime.author_item" => handle_runtime_author_item(
            &clean_params,
            state,
            callback_cap.as_ref(),
            validated_thread_auth.as_ref(),
        ),
        "runtime.project_snapshot" => handle_runtime_project_snapshot(
            &clean_params,
            state,
            callback_cap.as_ref(),
            validated_thread_auth.as_ref(),
        ),
        "runtime.finalize_thread" => {
            let owner = callback_launch_owner
                .ok_or_else(|| anyhow!("runtime.finalize_thread requires a launch owner"))?;
            let result = handle_finalize(&clean_params, state, owner)?;
            // A self-finalizing follow child (the normal path) flips its waiter to
            // `ready` here — kick the parent resume live, keyed on the child's chain.
            if let Some(chain_root_id) = result.get("chain_root_id").and_then(|v| v.as_str()) {
                ryeos_executor::execution::launch::kick_follow_resume_if_ready(
                    state,
                    chain_root_id,
                );
                ryeos_executor::execution::launch::kick_launch_window_for_terminal(
                    state,
                    chain_root_id,
                );
            }
            Ok(result)
        }
        "runtime.mark_running" => handle_mark_running(&clean_params, state),
        "runtime.request_continuation" => {
            let (result, prepared) = handle_request_continuation(&clean_params, state).await?;
            spawn_machine_continuation_launch(state, &result, prepared);
            Ok(result)
        }
        "runtime.publish_artifact" => handle_publish_artifact(&clean_params, state),
        "runtime.get_facets" => handle_get_facets(&clean_params, state),
        "runtime.get_thread" => handle_get(&clean_params, state),
        "runtime.submit_command" => handle_submit_command(&clean_params, state).await,
        "runtime.claim_commands" => handle_claim_commands(&clean_params, state),
        "runtime.complete_command" => handle_complete_command(&clean_params, state),
        "runtime.get_thread_events" => handle_replay_events(&clean_params, state),
        "runtime.attach_process" => {
            let owner = callback_launch_owner
                .ok_or_else(|| anyhow!("runtime.attach_process requires a launch owner"))?;
            handle_attach_process(&clean_params, state, peer, owner).await
        }
        "runtime.poll_input" => handle_poll_input(&clean_params, state),
        other => anyhow::bail!("unknown runtime method: {other}"),
    }
}

/// Fence callback mutations once a durable stop (or daemon shutdown) has
/// closed authoring. Cooperative cancellation retains only the narrow surface
/// needed to settle already-issued commands and finalize.
fn enforce_runtime_callback_admission(
    method: &str,
    params: &serde_json::Value,
    state: &AppState,
) -> Result<()> {
    if is_unrestricted_runtime_read_method(method) || method == "runtime.attach_process" {
        return Ok(());
    }
    let thread_id = params
        .get("thread_id")
        .and_then(|value| value.as_str())
        .ok_or_else(|| anyhow!("missing thread_id on {method}"))?;

    if is_running_runtime_mutation(method) || is_sensitive_runtime_read_method(method) {
        state
            .state_store
            .ensure_running_runtime_mutation_allowed(thread_id)?;
        return Ok(());
    }
    state
        .state_store
        .ensure_runtime_callback_mutation_allowed(thread_id, is_stop_completion_method(method))
}

fn is_running_runtime_mutation(method: &str) -> bool {
    matches!(
        method,
        "runtime.append_event"
            | "runtime.append_events"
            | "runtime.dispatch_action"
            | "runtime.spawn_follow_child"
            | "runtime.request_continuation"
            | "runtime.author_item"
            | "runtime.vault_put"
            | "runtime.vault_delete"
            | "runtime.bundle_events_append"
            | "runtime.bundle_events_materialize_attachment"
            | "runtime.publish_artifact"
            | "runtime.submit_command"
            | "runtime.poll_input"
    )
}

fn is_stop_completion_method(method: &str) -> bool {
    matches!(
        method,
        "runtime.finalize_thread" | "runtime.claim_commands" | "runtime.complete_command"
    )
}

fn is_unrestricted_runtime_read_method(method: &str) -> bool {
    is_chain_read_method(method) || method == "runtime.get_facets"
}

/// Reads that expose bundle state or vault material are live authority, not
/// post-run inspection. Stop/shutdown/terminal therefore revoke them at the
/// same running-thread boundary as authoring.
fn is_sensitive_runtime_read_method(method: &str) -> bool {
    matches!(
        method,
        "runtime.bundle_events_read_chain"
            | "runtime.bundle_events_scan"
            | "runtime.vault_get"
            | "runtime.vault_list"
    )
}

/// Runtime methods that carry a per-request `thread_auth_token`. The prelude
/// proves the token against the caller's own `thread_id`; the handler performs
/// the stronger validation (callback token + project_path) and derives every
/// trust-bearing field (principal, provenance, caps) from server-side state.
fn is_thread_auth_method(method: &str) -> bool {
    matches!(
        method,
        "runtime.dispatch_action" | "runtime.spawn_follow_child"
    )
}

/// Runtime read methods a callback may invoke against any thread in its own
/// chain (to rehydrate predecessors), not just its exact thread. Reads only.
fn is_chain_read_method(method: &str) -> bool {
    matches!(
        method,
        "runtime.get_thread" | "runtime.replay_events" | "runtime.get_thread_events"
    )
}

/// Authorize a chain read: the capability's own thread and the read target must
/// share a chain root. Reads across a chain (predecessors/siblings) are allowed;
/// reads into another chain are rejected. Membership is resolved from state —
/// the runtime cannot assert its own chain.
fn authorize_chain_read(
    cap: &ryeos_app::callback_token::CallbackCapability,
    params: &serde_json::Value,
    state: &AppState,
) -> Result<()> {
    let cap_chain = state
        .state_store
        .get_thread(&cap.thread_id)?
        .map(|d| d.chain_root_id)
        .ok_or_else(|| anyhow!("callback capability thread not found: {}", cap.thread_id))?;

    // Target chain: an explicit `chain_root_id` param wins; otherwise the
    // requested `thread_id`'s chain root.
    let target_chain = if let Some(cr) = params.get("chain_root_id").and_then(|v| v.as_str()) {
        cr.to_string()
    } else {
        let thread_id = params
            .get("thread_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("chain read requires thread_id or chain_root_id"))?;
        state
            .state_store
            .get_thread(thread_id)?
            .map(|d| d.chain_root_id)
            .ok_or_else(|| anyhow!("chain read target thread not found: {thread_id}"))?
    };

    if cap_chain != target_chain {
        anyhow::bail!("callback capability does not authorize reads outside its chain");
    }
    Ok(())
}

fn handle_mark_running(params: &serde_json::Value, state: &AppState) -> Result<serde_json::Value> {
    let params: ThreadMarkRunningParams =
        serde_json::from_value(params.clone()).context("invalid runtime.mark_running params")?;
    serde_json::to_value(state.threads.mark_running(&params.thread_id)?)
        .context("failed to encode runtime.mark_running result")
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeAttachProcessParams {
    thread_id: String,
    pid: i64,
}

async fn handle_attach_process(
    params: &serde_json::Value,
    state: &AppState,
    peer: Option<&AuthenticatedUnixPeer>,
    launch_owner: &str,
) -> Result<serde_json::Value> {
    let wire: RuntimeAttachProcessParams =
        serde_json::from_value(params.clone()).context("invalid runtime.attach_process params")?;
    let peer = verify_attaching_peer_pid(wire.pid, peer)?;
    // The runtime reports its own PID, which must match the accepted stream's
    // kernel credential above. Derive and pin both the target and group leader
    // daemon-side; never trust a runtime-supplied PGID or identity. The durable
    // boot/start-time tuple is required for every later signal.
    let process_identity = {
        #[cfg(target_os = "linux")]
        {
            ryeos_app::process::capture_execution_process_identity_from_pidfd(
                wire.pid,
                None,
                peer.pidfd(),
            )
            .context("capture runtime process identity from Unix peer pidfd")?
        }
        #[cfg(not(target_os = "linux"))]
        {
            anyhow::bail!("runtime.attach_process requires Linux SO_PEERPIDFD support");
        }
    };
    let params = ThreadAttachProcessParams {
        thread_id: wire.thread_id,
        pid: wire.pid,
        pgid: process_identity.pgid(),
        process_identity: Some(process_identity),
        metadata: None,
        launch_metadata: ryeos_app::launch_metadata::RuntimeLaunchMetadata::default(),
    };
    let attached = match state.threads.attach_process_owned(&params, launch_owner) {
        Ok(thread) => thread,
        Err(error) => {
            if let Some(identity) = params.process_identity.clone() {
                if let Err(join_error) = tokio::task::spawn_blocking(move || {
                    ryeos_app::process::kill_by_action(
                        &identity,
                        ryeos_app::process::ShutdownAction::Hard,
                    )
                })
                .await
                {
                    tracing::warn!(
                        thread_id = %params.thread_id,
                        error = %join_error,
                        "runtime.attach_process cleanup worker failed"
                    );
                }
            }
            return Err(error)
                .context("runtime.attach_process refused; spawned process-group kill attempted");
        }
    };
    if state
        .state_store
        .launch_window_is_cancelled(&attached.chain_root_id)?
    {
        let stop_state = state.clone();
        let stop_thread_id = attached.thread_id.clone();
        let report = tokio::task::spawn_blocking(move || {
            ryeos_app::cascade::signal_thread(
                &stop_state.state_store,
                &stop_thread_id,
                ryeos_app::cascade::CascadeMode::Graceful,
            )
        })
        .await
        .context("late attachment cancellation worker failed")?;
        tracing::info!(
            thread_id = %attached.thread_id,
            report = %report,
            "late follow-child process attachment observed cancellation tombstone"
        );
    }
    serde_json::to_value(attached).context("failed to encode runtime.attach_process result")
}

fn verify_attaching_peer_pid(
    reported_pid: i64,
    peer: Option<&AuthenticatedUnixPeer>,
) -> Result<&AuthenticatedUnixPeer> {
    let peer = peer.ok_or_else(|| {
        anyhow!("runtime.attach_process requires a kernel-authenticated Unix peer pidfd")
    })?;
    let peer_pid = peer.pid();
    if reported_pid != peer_pid {
        anyhow::bail!(
            "runtime.attach_process PID mismatch: reported {reported_pid}, Unix peer {peer_pid}"
        );
    }
    Ok(peer)
}

/// Runtime-supplied terminal completion received on `runtime.finalize_thread`.
///
/// `cost` is the runtime's own cost JSON (`{input_tokens, output_tokens,
/// total_usd}`); it is mapped into a [`FinalCost`] before finalization.
#[derive(Debug)]
struct RuntimeFinalizeParams {
    thread_id: String,
    status: ryeos_engine::contracts::ThreadTerminalStatus,
    outcome_code: Option<String>,
    result: Option<serde_json::Value>,
    error: Option<serde_json::Value>,
    cost: Option<RuntimeFinalizeCost>,
    /// The runtime's structured outputs + warnings. Preserved into the canonical
    /// managed envelope so a detached follow child's return data survives to the
    /// parent's resume.
    outputs: serde_json::Value,
    warnings: Vec<String>,
}

struct RequiredNullable<T>(Option<T>);

impl<'de, T> serde::Deserialize<'de> for RequiredNullable<T>
where
    T: serde::de::DeserializeOwned,
{
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = <serde_json::Value as serde::Deserialize>::deserialize(deserializer)?;
        serde_json::from_value(value)
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeFinalizeParamsWire {
    thread_id: String,
    status: ryeos_engine::contracts::ThreadTerminalStatus,
    outcome_code: RequiredNullable<String>,
    result: RequiredNullable<serde_json::Value>,
    error: RequiredNullable<serde_json::Value>,
    cost: RequiredNullable<RuntimeFinalizeCost>,
    outputs: serde_json::Value,
    warnings: Vec<String>,
}

impl<'de> serde::Deserialize<'de> for RuntimeFinalizeParams {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = RuntimeFinalizeParamsWire::deserialize(deserializer)?;
        Ok(Self {
            thread_id: wire.thread_id,
            status: wire.status,
            outcome_code: wire.outcome_code.0,
            result: wire.result.0,
            error: wire.error.0,
            cost: wire.cost.0,
            outputs: wire.outputs,
            warnings: wire.warnings,
        })
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeFinalizeCost {
    input_tokens: u64,
    output_tokens: u64,
    total_usd: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    basis: Option<String>,
}

fn final_cost_from_runtime(
    cost: &RuntimeFinalizeCost,
) -> std::result::Result<
    ryeos_engine::contracts::FinalCost,
    ryeos_runtime::envelope::RuntimeCostError,
> {
    let runtime_cost = ryeos_runtime::envelope::RuntimeCost {
        input_tokens: cost.input_tokens,
        output_tokens: cost.output_tokens,
        total_usd: cost.total_usd,
        basis: cost.basis.clone(),
    };
    runtime_cost.validate()?;
    Ok(ryeos_engine::contracts::FinalCost {
        turns: 0,
        input_tokens: runtime_cost.input_tokens,
        output_tokens: runtime_cost.output_tokens,
        spend: runtime_cost.total_usd,
        provider: None,
        basis: runtime_cost.basis,
        metadata: None,
    })
}

fn handle_finalize(
    params: &serde_json::Value,
    state: &AppState,
    launch_owner: &str,
) -> Result<serde_json::Value> {
    let params: RuntimeFinalizeParams =
        serde_json::from_value(params.clone()).context("invalid runtime.finalize_thread params")?;
    let final_cost = params
        .cost
        .as_ref()
        .map(final_cost_from_runtime)
        .transpose()
        .context("invalid runtime.finalize_thread cost")?;
    let raw_cost = params
        .cost
        .as_ref()
        .map(serde_json::to_value)
        .transpose()
        .context("failed to encode validated runtime.finalize_thread cost")?;
    // Build the canonical managed envelope from the RAW runtime fields (raw cost,
    // outputs, warnings) BEFORE `completion` moves result/error, so a followed
    // child's structured return survives to the parent's resume.
    let managed_envelope = ryeos_app::thread_lifecycle::managed_runtime_envelope(
        params.status.as_str(),
        params.result.as_ref(),
        params.error.as_ref(),
        raw_cost.as_ref(),
        &params.outputs,
        &params.warnings,
    );
    let completion = ryeos_engine::contracts::ExecutionCompletion {
        status: params.status,
        outcome_code: params.outcome_code,
        result: params.result,
        error: params.error,
        artifacts: Vec::new(),
        final_cost,
        continuation_request: None,
        metadata: None,
    };
    let finalized = state.threads.finalize_from_runtime_completion_owned(
        &params.thread_id,
        launch_owner,
        &completion,
        Some(managed_envelope),
    )?;
    // Terminal state is authoritative even while the wrapper remains alive.
    // Revoke both in-memory credential classes immediately; an already-admitted
    // concurrent request is still bounded by the StateStore lifecycle check.
    state
        .callback_tokens
        .invalidate_for_thread(&params.thread_id);
    state.thread_auth.invalidate_for_thread(&params.thread_id);
    serde_json::to_value(finalized).context("failed to encode runtime.finalize_thread result")
}

fn handle_get(params: &serde_json::Value, state: &AppState) -> Result<serde_json::Value> {
    let params: ThreadGetParams =
        serde_json::from_value(params.clone()).context("invalid runtime.get_thread params")?;
    match state.threads.get_thread(&params.thread_id)? {
        // This chain-rehydration surface is intentionally slim. Artifacts and
        // facets have their own bounded APIs; embedding their complete
        // collections here made a tiny predecessor lookup allocate an
        // arbitrarily large response.
        Some(thread) => serde_json::to_value(json!({
            "thread": thread,
            "result": state.threads.get_thread_result(&params.thread_id)?,
        }))
        .context("failed to encode runtime.get_thread result"),
        None => Ok(serde_json::Value::Null),
    }
}

/// Auto-launch a machine continuation successor after a limit cut-off handoff.
///
/// Autonomous machine continuation is always-on: the chain-depth cap enforced at
/// create time (`create_machine_continuation`) bounds an autonomous run, so an
/// unbounded chain can no longer form and there is nothing to gate. The successor
/// row, chain link, and authoritative audit are recorded by the continuation
/// lifecycle; this moves the exact prepared authority into the launch task.
///
/// Spawned daemon-side (NOT from the dying runtime — a lifecycle hazard) after
/// the source is settled `continued` and the state-store write lock has dropped.
/// The prepared launcher claims the launch lease, so a concurrent reconcile
/// cannot double-launch the same successor.
fn spawn_machine_continuation_launch(
    state: &AppState,
    result: &serde_json::Value,
    prepared: ryeos_executor::execution::launch::PreparedMachineSuccessorLaunch,
) {
    let Some(successor_id) = result
        .get("successor_thread_id")
        .and_then(|v| v.as_str())
        .map(str::to_string)
    else {
        tracing::warn!("machine continuation: result missing successor_thread_id; not launching");
        return;
    };
    let st = state.clone();
    tokio::spawn(async move {
        use ryeos_executor::execution::launch::SuccessorLaunchOutcome;
        match ryeos_executor::execution::launch::launch_prepared_machine_successor(
            st,
            &successor_id,
            prepared,
        )
        .await
        {
            Ok(SuccessorLaunchOutcome::Launched(_)) => {}
            Ok(SuccessorLaunchOutcome::Skipped(reason)) => {
                tracing::debug!(
                    successor_id = %successor_id,
                    reason,
                    "machine continuation: successor launch skipped"
                );
            }
            Err(err) => {
                tracing::error!(
                    successor_id = %successor_id,
                    error = %err,
                    "machine continuation: successor launch failed"
                );
            }
        }
    });
}

async fn handle_request_continuation(
    params: &serde_json::Value,
    state: &AppState,
) -> Result<(
    serde_json::Value,
    ryeos_executor::execution::launch::PreparedMachineSuccessorLaunch,
)> {
    let params: ThreadContinuationParams = serde_json::from_value(params.clone())
        .context("invalid runtime.request_continuation params")?;
    let resume_context = state
        .state_store
        .get_launch_metadata(&params.thread_id)?
        .and_then(|metadata| metadata.resume_context)
        .ok_or_else(|| {
            anyhow!(
                "source thread {} has no captured ResumeContext",
                params.thread_id
            )
        })?;
    let successor_thread_id = ryeos_app::thread_lifecycle::new_thread_id();
    let prepared = ryeos_executor::execution::launch::prepare_machine_successor_launch(
        state,
        &successor_thread_id,
        &resume_context,
        &params.thread_id,
    )
    .await?;
    let initial_events = prepared.initial_audit_events()?;
    let result = state.threads.request_continuation_with_events(
        &params,
        &successor_thread_id,
        &resume_context,
        prepared.launch_metadata(),
        initial_events,
    )?;
    let encoded = serde_json::to_value(result)
        .context("failed to encode runtime.request_continuation result")?;
    Ok((encoded, prepared.with_persisted_birth_audit()))
}

fn handle_append_event(params: &serde_json::Value, state: &AppState) -> Result<serde_json::Value> {
    let params: EventAppendParams =
        serde_json::from_value(params.clone()).context("invalid events.append params")?;
    let persisted = state.events.append(&params)?;
    // Publish AFTER persistence so live subscribers receive the same
    // chain_seq the event store recorded. Persistence-first is the
    // contract; SSE consumers replay from the event store on lag.
    state
        .event_streams
        .publish(&persisted.thread_id, persisted.clone());
    serde_json::to_value(persisted).context("failed to encode events.append result")
}

fn handle_append_event_batch(
    params: &serde_json::Value,
    state: &AppState,
) -> Result<serde_json::Value> {
    let params: EventAppendBatchParams =
        serde_json::from_value(params.clone()).context("invalid events.append_batch params")?;
    let result = state.events.append_batch(&params)?;
    // Publish the whole batch in persisted order under one hub-lock acquire:
    // each thread's lane sees its events in order, and the firehose sees the
    // batch contiguously without interleaving a concurrent publisher.
    state.event_streams.publish_ordered(&result.persisted);
    serde_json::to_value(result).context("failed to encode events.append_batch result")
}

fn handle_replay_events(params: &serde_json::Value, state: &AppState) -> Result<serde_json::Value> {
    let params: EventReplayParams =
        serde_json::from_value(params.clone()).context("invalid events.replay params")?;
    if params.limit == 0 || params.limit > MAX_RUNTIME_REPLAY_PAGE_LIMIT {
        anyhow::bail!(
            "runtime events.replay limit must be between 1 and {}",
            MAX_RUNTIME_REPLAY_PAGE_LIMIT
        );
    }
    serde_json::to_value(state.events.replay(&params)?)
        .context("failed to encode events.replay result")
}

/// `runtime.poll_input` — poll-and-persist staged operator inputs for a running
/// thread. The queue is daemon-side scratch; this is the ONLY place a queued
/// input becomes a durable braid event.
///
/// Contract: drain FIFO → append indexed `cognition_in` (content only) through
/// the running-guarded path → return the persisted inputs for the runtime to
/// fold. The guard is the terminal-safety anchor: if the thread is no longer
/// running, the append is a no-op and the drained items are discarded (never a
/// `cognition_in` after terminal). A transient append error restores the items
/// to the front so a later poll retries.
fn handle_poll_input(params: &serde_json::Value, state: &AppState) -> Result<serde_json::Value> {
    let thread_id = params
        .get("thread_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("missing thread_id on runtime.poll_input"))?;

    // Resolve the chain root BEFORE draining so a lookup failure can't strand
    // drained input — nothing has been drained yet. If the thread vanished there
    // is nowhere to persist.
    let Some(detail) = state.state_store.get_thread(thread_id)? else {
        return Ok(json!({ "inputs": [] }));
    };

    let pending = state.live_input.drain(thread_id);
    if pending.is_empty() {
        return Ok(json!({ "inputs": [] }));
    }
    let n = pending.len();

    // Encode the response BEFORE persisting: a serialize failure must restore the
    // drained items (releasing the in-flight reservation), never leave durable
    // `cognition_in` the runtime never receives.
    let inputs_value = match serde_json::to_value(&pending) {
        Ok(v) => v,
        Err(e) => {
            state.live_input.restore_front(thread_id, pending);
            return Err(anyhow::Error::new(e).context("failed to encode poll_input inputs"));
        }
    };
    let response = json!({ "inputs": inputs_value });
    let response_bytes = match serde_json::to_vec(&response) {
        Ok(bytes) => bytes.len(),
        Err(error) => {
            state.live_input.restore_front(thread_id, pending);
            return Err(anyhow::Error::new(error).context("failed to size poll_input response"));
        }
    };
    if response_bytes > MAX_LIVE_INPUT_POLL_RESPONSE_BYTES {
        state.live_input.restore_front(thread_id, pending);
        anyhow::bail!(
            "runtime.poll_input response is {response_bytes} bytes; maximum is {MAX_LIVE_INPUT_POLL_RESPONSE_BYTES}"
        );
    }

    // A `cognition_in` carries only `content` — the intent is a delivery concern,
    // not part of the braid. Indexed (durable) so resume folds it in order.
    let events: Vec<ryeos_app::state_store::NewEventRecord> = pending
        .iter()
        .map(|s| ryeos_app::state_store::NewEventRecord {
            event_type: "cognition_in".to_string(),
            storage_class: "indexed".to_string(),
            payload: json!({ "content": s.content }),
        })
        .collect();

    match state
        .threads
        .append_thread_events(&detail.chain_root_id, thread_id, &events)
    {
        // Persisted while running — release the reservation and hand the inputs
        // back for the loop to fold.
        Ok(Some(_persisted)) => {
            state.live_input.ack_drained(thread_id, n);
            Ok(response)
        }
        // Not running (terminal) — discard and release the reservation. The
        // queue close at finalize already cleared queued items; this drops
        // anything that raced in.
        Ok(None) => {
            state.live_input.ack_drained(thread_id, n);
            Ok(json!({ "inputs": [] }))
        }
        // Transient failure — restore for a later poll, then surface the error.
        Err(e) => {
            state.live_input.restore_front(thread_id, pending);
            Err(e)
        }
    }
}

fn handle_bundle_events_append(
    params: &serde_json::Value,
    state: &AppState,
    cap: Option<&ryeos_app::callback_token::CallbackCapability>,
) -> Result<serde_json::Value> {
    let cap = cap.ok_or_else(|| anyhow::anyhow!("missing callback capability"))?;
    let params: BundleEventAppendParams =
        serde_json::from_value(params.clone()).context("invalid bundle_events.append params")?;
    serde_json::to_value(BundleEventService::append(
        &state.state_store,
        &state.authorizer,
        cap,
        params,
    )?)
    .context("failed to encode bundle_events.append result")
}

fn handle_bundle_events_read_chain(
    params: &serde_json::Value,
    state: &AppState,
    cap: Option<&ryeos_app::callback_token::CallbackCapability>,
) -> Result<serde_json::Value> {
    let cap = cap.ok_or_else(|| anyhow::anyhow!("missing callback capability"))?;
    let params: BundleEventReadChainParams = serde_json::from_value(params.clone())
        .context("invalid bundle_events.read_chain params")?;
    serde_json::to_value(BundleEventService::read_chain(
        &state.state_store,
        &state.authorizer,
        cap,
        params,
    )?)
    .context("failed to encode bundle_events.read_chain result")
}

fn handle_bundle_events_scan(
    params: &serde_json::Value,
    state: &AppState,
    cap: Option<&ryeos_app::callback_token::CallbackCapability>,
) -> Result<serde_json::Value> {
    let cap = cap.ok_or_else(|| anyhow::anyhow!("missing callback capability"))?;
    let params: BundleEventScanParams =
        serde_json::from_value(params.clone()).context("invalid bundle_events.scan params")?;
    serde_json::to_value(BundleEventService::scan(
        &state.state_store,
        &state.authorizer,
        cap,
        params,
    )?)
    .context("failed to encode bundle_events.scan result")
}

fn handle_bundle_events_materialize_attachment(
    params: &serde_json::Value,
    state: &AppState,
    cap: Option<&ryeos_app::callback_token::CallbackCapability>,
) -> Result<serde_json::Value> {
    let cap = cap.ok_or_else(|| anyhow::anyhow!("missing callback capability"))?;
    let params: BundleEventMaterializeAttachmentParams = serde_json::from_value(params.clone())
        .context("invalid bundle_events.materialize_attachment params")?;
    serde_json::to_value(BundleEventService::materialize_attachment(
        &state.state_store,
        &state.authorizer,
        cap,
        params,
    )?)
    .context("failed to encode bundle_events.materialize_attachment result")
}

fn handle_runtime_vault_put(
    params: &serde_json::Value,
    state: &AppState,
    cap: Option<&ryeos_app::callback_token::CallbackCapability>,
) -> Result<serde_json::Value> {
    let cap = cap.ok_or_else(|| anyhow::anyhow!("missing callback capability"))?;
    let params: RuntimeVaultPutParams =
        serde_json::from_value(params.clone()).context("invalid vault.put params")?;
    serde_json::to_value(RuntimeVaultService::put(
        &state.vault,
        &state.authorizer,
        cap,
        params,
    )?)
    .context("failed to encode vault.put result")
}

fn handle_runtime_vault_get(
    params: &serde_json::Value,
    state: &AppState,
    cap: Option<&ryeos_app::callback_token::CallbackCapability>,
) -> Result<serde_json::Value> {
    let cap = cap.ok_or_else(|| anyhow::anyhow!("missing callback capability"))?;
    let params: RuntimeVaultRefParams =
        serde_json::from_value(params.clone()).context("invalid vault.get params")?;
    serde_json::to_value(RuntimeVaultService::get(
        &state.vault,
        &state.authorizer,
        cap,
        params,
    )?)
    .context("failed to encode vault.get result")
}

fn handle_runtime_vault_delete(
    params: &serde_json::Value,
    state: &AppState,
    cap: Option<&ryeos_app::callback_token::CallbackCapability>,
) -> Result<serde_json::Value> {
    let cap = cap.ok_or_else(|| anyhow::anyhow!("missing callback capability"))?;
    let params: RuntimeVaultRefParams =
        serde_json::from_value(params.clone()).context("invalid vault.delete params")?;
    serde_json::to_value(RuntimeVaultService::delete(
        &state.vault,
        &state.authorizer,
        cap,
        params,
    )?)
    .context("failed to encode vault.delete result")
}

fn handle_runtime_vault_list(
    params: &serde_json::Value,
    state: &AppState,
    cap: Option<&ryeos_app::callback_token::CallbackCapability>,
) -> Result<serde_json::Value> {
    let cap = cap.ok_or_else(|| anyhow::anyhow!("missing callback capability"))?;
    let params: RuntimeVaultListParams =
        serde_json::from_value(params.clone()).context("invalid vault.list params")?;
    serde_json::to_value(RuntimeVaultService::list(
        &state.vault,
        &state.authorizer,
        cap,
        params,
    )?)
    .context("failed to encode vault.list result")
}

fn handle_runtime_author_item(
    params: &serde_json::Value,
    state: &AppState,
    cap: Option<&ryeos_app::callback_token::CallbackCapability>,
    thread_auth: Option<&ryeos_app::callback_token::ThreadAuthState>,
) -> Result<serde_json::Value> {
    let cap = cap.ok_or_else(|| anyhow::anyhow!("missing callback capability"))?;
    let thread_auth = thread_auth.ok_or_else(|| anyhow::anyhow!("missing thread auth state"))?;
    let params: RuntimeAuthorItemParams =
        serde_json::from_value(params.clone()).context("invalid runtime.author_item params")?;
    serde_json::to_value(RuntimeItemAuthorService::author(
        &state.identity,
        &state.authorizer,
        cap,
        thread_auth,
        params,
    )?)
    .context("failed to encode runtime.author_item result")
}

fn handle_runtime_project_snapshot(
    params: &serde_json::Value,
    state: &AppState,
    cap: Option<&ryeos_app::callback_token::CallbackCapability>,
    thread_auth: Option<&ryeos_app::callback_token::ThreadAuthState>,
) -> Result<serde_json::Value> {
    let cap = cap.ok_or_else(|| anyhow::anyhow!("missing callback capability"))?;
    let thread_auth = thread_auth.ok_or_else(|| anyhow::anyhow!("missing thread auth state"))?;
    let params: RuntimeProjectSnapshotRequest = serde_json::from_value(params.clone())
        .context("invalid runtime.project_snapshot params")?;
    RuntimeProjectSnapshotService::execute(state, cap, thread_auth, params)
}

async fn handle_submit_command(
    params: &serde_json::Value,
    state: &AppState,
) -> Result<serde_json::Value> {
    let params: CommandSubmitParams =
        serde_json::from_value(params.clone()).context("invalid commands.submit params")?;
    let thread_id = params.thread_id.clone();
    let command_type = params.command_type.clone();
    let record = state.commands.submit(&params)?;

    // Same daemon-side enforcement as the API `commands.submit`: a cancel/kill
    // submitted over the runtime callback signals the target and cascades to its
    // live descendants, so both entry points behave identically. Logged, not
    // raised — the command is already enqueued.
    let stop_mode = match command_type.as_str() {
        "kill" => Some(ryeos_app::cascade::CascadeMode::Hard),
        "cancel" => Some(ryeos_app::cascade::CascadeMode::Graceful),
        _ => None,
    };
    if let Some(mode) = stop_mode {
        let stop_state = state.clone();
        let stop_thread_id = thread_id.clone();
        let stop_result = tokio::task::spawn_blocking(move || {
            ryeos_app::cascade::stop_thread_and_descendants(&stop_state, &stop_thread_id, mode)
        })
        .await;
        match stop_result {
            Ok(Ok((report, cancelled_roots))) => {
                for root in cancelled_roots {
                    ryeos_executor::execution::launch::kick_follow_resume_if_ready(state, &root);
                }
                tracing::info!(thread_id = %thread_id, command_type = %command_type,
                    report = %report, "cancel/kill signalled target and descendants");
            }
            Ok(Err(e)) => tracing::warn!(
                thread_id = %thread_id,
                command_type = %command_type,
                error = %e,
                "cancel/kill stop failed on runtime submit_command"
            ),
            Err(e) => tracing::warn!(
                thread_id = %thread_id,
                command_type = %command_type,
                error = %e,
                "cancel/kill stop worker failed on runtime submit_command"
            ),
        }
    }

    serde_json::to_value(record).context("failed to encode commands.submit result")
}

fn handle_claim_commands(
    params: &serde_json::Value,
    state: &AppState,
) -> Result<serde_json::Value> {
    let params: CommandClaimParams =
        serde_json::from_value(params.clone()).context("invalid commands.claim params")?;
    serde_json::to_value(state.commands.claim(&params)?)
        .context("failed to encode commands.claim result")
}

/// Runtime-facing params for `runtime.complete_command`. Unlike the service-side
/// `CommandCompleteParams`, it carries `thread_id` — validated against the
/// callback token at the exact-thread trust boundary before this handler runs —
/// which is then dropped when mapping to the service params.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeCompleteCommandParams {
    #[allow(dead_code)]
    thread_id: String,
    command_id: i64,
    status: String,
    #[serde(default)]
    result: Option<serde_json::Value>,
}

fn handle_complete_command(
    params: &serde_json::Value,
    state: &AppState,
) -> Result<serde_json::Value> {
    let rt: RuntimeCompleteCommandParams = serde_json::from_value(params.clone())
        .context("invalid runtime.complete_command params")?;
    // Trust boundary: the callback token was validated against `rt.thread_id`,
    // but `command_id` is a global autoincrement. Confirm the command belongs to
    // this thread before settling it — otherwise a runtime holding a valid token
    // for its OWN thread could settle, or inject a `result` into, another
    // thread's command, and that forged record would be delivered to the
    // victim's `commands.wait`. A command's thread binding is immutable, so this
    // read-then-settle is not a TOCTOU.
    match state.state_store.get_command(rt.command_id)? {
        Some(existing) if existing.thread_id == rt.thread_id => {}
        Some(_) => anyhow::bail!(
            "command {} does not belong to thread {}",
            rt.command_id,
            rt.thread_id
        ),
        None => anyhow::bail!("command {} not found", rt.command_id),
    }
    let complete = CommandCompleteParams {
        command_id: rt.command_id,
        status: rt.status,
        result: rt.result,
    };
    let record = state.commands.complete(&complete)?;
    // Wake any `commands.wait` blocked on this command's settlement. Publish
    // after the row is durably updated so a woken waiter reads a consistent
    // terminal row.
    ryeos_app::command_hub::global().publish(&record);
    serde_json::to_value(record).context("failed to encode runtime.complete_command result")
}

fn handle_publish_artifact(
    params: &serde_json::Value,
    state: &AppState,
) -> Result<serde_json::Value> {
    let params: ArtifactPublishParams =
        serde_json::from_value(params.clone()).context("invalid artifacts.publish params")?;
    let artifact = state.threads.publish_artifact(&params)?;
    serde_json::to_value(artifact).context("failed to encode artifacts.publish result")
}

fn handle_get_facets(params: &serde_json::Value, state: &AppState) -> Result<serde_json::Value> {
    let thread_id = params
        .get("thread_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing thread_id"))?;
    let facets = state.state_store.get_facets(thread_id)?;
    let facets_map: std::collections::HashMap<&str, &str> = facets
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    serde_json::to_value(facets_map).context("failed to encode facets")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::uds::protocol::RpcError;
    use ryeos_app::callback_token::CallbackCapabilityStore;
    use ryeos_app::command_service::CommandService;
    use ryeos_app::event_store_service::EventStoreService;
    use ryeos_app::event_stream::{ThreadEventHub, DEFAULT_EVENT_STREAM_CAPACITY};
    use ryeos_app::identity::NodeIdentity;
    use ryeos_app::kind_profiles::{KindProfileRegistry, ThreadKindProfile};
    use ryeos_app::state::AppState;
    use ryeos_app::state_store::{NewThreadRecord, StateStore};
    use ryeos_app::thread_lifecycle::{
        ThreadCreateParams, ThreadFinalizeParams, ThreadLifecycleService,
    };
    use ryeos_app::write_barrier::WriteBarrier;
    use std::sync::Arc;
    use std::time::Instant;
    use tempfile::TempDir;

    #[test]
    fn runtime_attach_requires_matching_unix_peer_pid() {
        fn peer(pid: i64) -> AuthenticatedUnixPeer {
            AuthenticatedUnixPeer {
                pid,
                #[cfg(target_os = "linux")]
                pidfd: std::fs::File::open("/dev/null").unwrap().into(),
            }
        }

        let matching = peer(42);
        verify_attaching_peer_pid(42, Some(&matching)).unwrap();

        let missing = verify_attaching_peer_pid(42, None)
            .err()
            .expect("missing authenticated Unix peer should be rejected");
        assert!(format!("{missing:#}").contains("kernel-authenticated Unix peer pidfd"));

        let other = peer(43);
        let mismatched = verify_attaching_peer_pid(42, Some(&other))
            .err()
            .expect("mismatched authenticated Unix peer should be rejected");
        let message = format!("{mismatched:#}");
        assert!(message.contains("reported 42"), "got: {message}");
        assert!(message.contains("Unix peer 43"), "got: {message}");
    }

    type TestProvenance = ryeos_app::execution_provenance::ExecutionProvenance;

    fn test_provenance(state: &AppState, path: &str) -> TestProvenance {
        let project_path = std::path::PathBuf::from(path);
        let authored_project_identity = format!("test:{path}");
        let authority_id = lillux::sha256_hex(
            format!("live-project\0{authored_project_identity}\0{path}").as_bytes(),
        );
        let authority = ryeos_state::objects::ExecutionProjectAuthority::LiveProject {
            authority_id: authority_id.clone(),
            authored_project_identity,
            canonical_root: project_path.clone(),
            live_access: ryeos_state::objects::LiveAccessAuthority {
                access: ryeos_state::objects::LiveProjectAccess::ReadWrite,
                denied_control_paths:
                    ryeos_state::project_sync::live_execution_denied_control_paths(),
                authorized_write_namespaces: vec!["project".to_string()],
                symlink_policy: ryeos_state::objects::LiveSymlinkPolicy::DescriptorRootedNoEscape,
            },
            environment: ryeos_state::objects::EnvironmentAuthority::ProjectOverlay {
                project_authority_id: authority_id,
                source_identity: format!("dotenv:{path}/.env"),
                include_operator_vault: true,
                name_authority: ryeos_state::objects::EnvironmentNameAuthority::DeclaredRequired,
            },
            capability_ceiling: Vec::new(),
            child_policy: ryeos_state::objects::ChildProjectAuthorityPolicy::Inherit,
        };
        ryeos_app::execution_provenance::ExecutionProvenance::root_live_fs(
            project_path,
            state.engine.clone(),
            authority,
        )
        .expect("synthetic UDS test live authority must be valid")
    }

    fn bind_test_callback_owner(
        state: &AppState,
        mut capability: ryeos_app::callback_token::CallbackCapability,
    ) -> ryeos_app::callback_token::CallbackCapability {
        let claim = match state
            .state_store
            .get_launch_claim(&capability.thread_id)
            .expect("read test launch claim")
        {
            Some(claim) => claim,
            None => {
                let claim_id = format!("test-callback-{}", capability.thread_id);
                assert!(matches!(
                    state
                        .state_store
                        .claim_thread_launch(
                            &capability.thread_id,
                            &claim_id,
                            "test-daemon-generation",
                        )
                        .expect("claim test callback launch"),
                    ryeos_app::runtime_db::LaunchClaimOutcome::Claimed
                ));
                state
                    .state_store
                    .get_launch_claim(&capability.thread_id)
                    .expect("read claimed test launch")
                    .expect("claimed test launch exists")
            }
        };
        assert!(state
            .callback_tokens
            .set_launch_owner(&capability.token, claim.claimed_by.clone()));
        capability.launch_owner = Some(claim.claimed_by);
        capability
    }

    fn generate_test_callback(
        state: &AppState,
        thread_id: &str,
        project_path: std::path::PathBuf,
        ttl: std::time::Duration,
        effective_caps: Vec<String>,
        provenance: TestProvenance,
        root_content_digest: String,
    ) -> ryeos_app::callback_token::CallbackCapability {
        bind_test_callback_owner(
            state,
            state.callback_tokens.generate(
                thread_id,
                project_path,
                ttl,
                effective_caps,
                provenance,
                root_content_digest,
            ),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn generate_test_callback_with_context(
        state: &AppState,
        thread_id: &str,
        project_path: std::path::PathBuf,
        ttl: std::time::Duration,
        effective_caps: Vec<String>,
        provenance: TestProvenance,
        effective_bundle_id: Option<String>,
        item_ref: Option<String>,
        root_content_digest: String,
        hard_limits: serde_json::Value,
        depth: u32,
    ) -> ryeos_app::callback_token::CallbackCapability {
        bind_test_callback_owner(
            state,
            state.callback_tokens.generate_with_context(
                thread_id,
                project_path,
                ttl,
                effective_caps,
                provenance,
                effective_bundle_id,
                item_ref,
                root_content_digest,
                hard_limits,
                depth,
            ),
        )
    }

    /// Build a minimal AppState for UDS dispatch tests.
    fn setup_app_state() -> (TempDir, AppState) {
        std::env::set_var("HOSTNAME", "testhost");
        let tmpdir = TempDir::new().unwrap();
        let runtime_state_dir = tmpdir.path().join(".ai").join("state");
        let runtime_db_path = tmpdir.path().join("runtime.sqlite3");
        let key_path = tmpdir.path().join("identity").join("node-key.pem");
        let config = Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            db_path: runtime_db_path.clone(),
            uds_path: tmpdir.path().join("test.sock"),
            app_root: tmpdir.path().to_path_buf(),
            node_signing_key_path: key_path.clone(),
            operator_signing_key_path: tmpdir.path().join("user-key.pem"),
            require_auth: false,
            authorized_keys_dir: tmpdir.path().join("auth"),
            tool_env_passthrough: Vec::new(),
        };

        let identity = NodeIdentity::create(&key_path).unwrap();
        identity
            .write_public_identity(&tmpdir.path().join("identity").join("public-identity.json"))
            .unwrap();

        let signer = Arc::new(ryeos_app::state_store::NodeIdentitySigner::from_identity(
            &identity,
        ));
        let mut head_trust = ryeos_state::refs::TrustStore::new();
        head_trust.insert(
            identity.fingerprint().to_string(),
            *identity.verifying_key(),
        );
        let write_barrier = WriteBarrier::new();
        let state_store = Arc::new(
            StateStore::new_with_head_trust(
                tmpdir.path().to_path_buf(),
                runtime_state_dir,
                runtime_db_path,
                signer,
                write_barrier,
                Arc::new(head_trust),
            )
            .unwrap(),
        );
        let engine = Arc::new(ryeos_engine::engine::Engine::new(
            ryeos_engine::kind_registry::KindRegistry::empty(),
            ryeos_engine::parsers::ParserDispatcher::new(
                ryeos_engine::parsers::ParserRegistry::empty(),
                std::sync::Arc::new(ryeos_engine::handlers::HandlerRegistry::empty()),
            ),
            Vec::new(),
        ));
        // Most rows in this minimal harness use the daemon-internal
        // `system_task` profile. Follow-reconciliation fixtures truthfully use
        // the standard bundle's `graph_run` profile without teaching the
        // production registry any hardcoded schema kinds.
        let kind_profiles = Arc::new(KindProfileRegistry::build(None).with_test_profile(
            "graph_run",
            ThreadKindProfile {
                root_executable: true,
                supports_interrupt: false,
                supports_continuation: true,
                supports_operator_followup: false,
            },
        ));
        let events = Arc::new(EventStoreService::new(state_store.clone()));
        let event_streams = Arc::new(ThreadEventHub::new(DEFAULT_EVENT_STREAM_CAPACITY));
        let threads = Arc::new(
            ThreadLifecycleService::new(
                state_store.clone(),
                engine.clone(),
                kind_profiles.clone(),
                events.clone(),
                event_streams.clone(),
            )
            .expect("HOSTNAME not set in test environment"),
        );
        let commands = Arc::new(CommandService::new(
            state_store.clone(),
            kind_profiles,
            events.clone(),
        ));

        let test_command_registry = Arc::new(
            ryeos_runtime::CommandRegistry::from_records(&[], &Default::default()).unwrap(),
        );
        let test_auth = Arc::new(ryeos_runtime::authorizer::Authorizer::new());

        let state = AppState {
            config: Arc::new(config),
            isolation: Arc::new(ryeos_engine::isolation::IsolationRuntime::default()),
            state_store,
            engine,
            engine_cache: ryeos_app::engine_cache::EngineCache::new(
                ryeos_app::engine_cache::EngineCacheConfig::default(),
            ),
            identity: Arc::new(identity),
            threads,
            live_input: Arc::new(ryeos_app::live_input_queue::LiveInputQueue::new()),
            events,
            event_streams,
            commands,
            callback_tokens: Arc::new(CallbackCapabilityStore::new()),
            thread_auth: Arc::new(ryeos_app::callback_token::ThreadAuthStore::new()),
            extensions: Arc::new(ryeos_app::extension_state::ExtensionState::new()),
            write_barrier: Arc::new(WriteBarrier::new()),
            started_at: Instant::now(),
            started_at_iso: lillux::time::iso8601_now(),
            catalog_health: ryeos_app::state::CatalogHealth {
                status: "ok".into(),
                missing_services: vec![],
            },
            services: Arc::new(ryeos_api::build_service_registry()),
            service_descriptors: ryeos_api::handlers::ALL,
            node_config: Arc::new(ryeos_app::node_config::NodeConfigSnapshot {
                bundles: vec![],
                routes: vec![],
                commands: vec![],
                hosted_node_policies: vec![],
                command_registration_policy: Default::default(),
            }),
            node_history_policy: Arc::new(
                ryeos_engine::history_policy::ResolvedNodeThreadHistoryPolicy::durable_without_config(),
            ),
            vault: Arc::new(ryeos_app::vault::SealedEnvelopeVault::new(
                tmpdir.path().join("vault-store.toml"),
                lillux::vault::VaultSecretKey::generate(),
            )),
            command_registry: test_command_registry,
            authorizer: test_auth,
            scheduler_db: Arc::new(crate::scheduler::db::SchedulerDb::new_in_memory().unwrap()),
            scheduler_runtime_gate: Arc::new(tokio::sync::RwLock::new(())),
            scheduler_reload_tx: None,
            ignore_matcher: Arc::new(ryeos_app::ignore::matcher_from_builtins()),
            vault_fingerprint: None,
        };

        (tmpdir, state)
    }

    fn make_create_params(thread_id: &str, chain_root_id: &str) -> ThreadCreateParams {
        let captured_history_policy = (thread_id == chain_root_id).then(|| {
            let hash = "a".repeat(64);
            ryeos_state::objects::CapturedThreadHistoryPolicy {
                retention: ryeos_state::objects::ThreadHistoryRetention::Durable,
                canonical_item_ref: "directive:test/directive".to_string(),
                item_content_hash: hash.clone(),
                item_signer_fingerprint: Some(hash.clone()),
                item_trust_class: ryeos_state::objects::CapturedItemTrustClass::Trusted,
                kind_schema_content_hash: hash,
                resolved_from: ryeos_state::objects::CapturedPolicyProvenance::NodeDefault {
                    node_policy:
                        ryeos_state::objects::CapturedNodeHistoryPolicyProvenance::MissingConfig,
                },
            }
        });
        ThreadCreateParams {
            thread_id: thread_id.to_string(),
            chain_root_id: chain_root_id.to_string(),
            kind: "system_task".to_string(),
            item_ref: "directive:test/directive".to_string(),
            executor_ref: "test/executor".to_string(),
            launch_mode: "wait".to_string(),
            current_site_id: "site:test".to_string(),
            origin_site_id: "site:test".to_string(),
            upstream_thread_id: None,
            requested_by: Some("user:test".to_string()),
            project_root: None,
            project_authority: ryeos_state::objects::ExecutionProjectAuthority::PROJECTLESS,
            base_project_snapshot_hash: None,
            usage_subject: None,
            usage_subject_asserted_by: None,
            captured_history_policy,
        }
    }

    /// Construct the authoritative root shape written for a detached follow
    /// child before its first launch. The sealed fixture is the source of truth
    /// for every identity field; mixing it with the generic system-task fixture
    /// would correctly make reconciliation refuse to relaunch the row.
    fn make_pending_follow_child_params(thread_id: &str) -> ThreadCreateParams {
        let sealed =
            ryeos_app::thread_lifecycle::SealedRootExecutionRequest::storage_test_fixture();
        ThreadCreateParams {
            thread_id: thread_id.to_string(),
            chain_root_id: thread_id.to_string(),
            kind: "graph_run".to_string(),
            item_ref: sealed.item_ref().to_string(),
            executor_ref: sealed.executor_ref().to_string(),
            launch_mode: "detached".to_string(),
            current_site_id: "site:test".to_string(),
            origin_site_id: "site:test".to_string(),
            upstream_thread_id: None,
            requested_by: Some("session:test".to_string()),
            project_root: None,
            project_authority: ryeos_state::objects::ExecutionProjectAuthority::PROJECTLESS,
            base_project_snapshot_hash: None,
            usage_subject: None,
            usage_subject_asserted_by: None,
            captured_history_policy: Some(sealed.captured_history_policy().clone()),
        }
    }

    fn ensure_test_root(state: &AppState, thread_id: &str) {
        if state.threads.get_thread(thread_id).unwrap().is_none() {
            state
                .threads
                .create_thread_for_test(&make_create_params(thread_id, thread_id))
                .unwrap();
        }
    }

    fn create_running_test_thread(state: &AppState, thread_id: &str) {
        state
            .threads
            .create_thread_for_test(&make_create_params(thread_id, thread_id))
            .unwrap();
        state.threads.mark_running(thread_id).unwrap();
    }

    fn rpc(method: &str, params: serde_json::Value) -> RpcRequest {
        RpcRequest {
            request_id: 1,
            method: method.to_string(),
            params,
        }
    }

    fn rpc_ok(resp: &RpcResponse) -> &serde_json::Value {
        resp.result.as_ref().expect("expected ok result")
    }

    fn rpc_err(resp: &RpcResponse) -> &RpcError {
        resp.error.as_ref().expect("expected error")
    }

    fn bootstrap_lifecycle(tmp: &TempDir) -> ryeos_node::LifecycleResponse {
        let started_at = "2026-07-14T00:00:00Z";
        ryeos_node::LifecycleResponse::starting(
            ryeos_node::LifecycleIdentity {
                pid: 42,
                bind: "127.0.0.1:7400".into(),
                uds_path: tmp.path().join("ryeosd.sock"),
                app_root: tmp.path().to_path_buf(),
                started_at: started_at.into(),
                version: "test".into(),
                revision: None,
                build_date: None,
            },
            ryeos_node::StartupSnapshot::bootstrapping(started_at),
        )
    }

    // ── system methods ──────────────────────────────────────────────

    #[tokio::test]
    async fn system_health_returns_ok() {
        let (_tmp, state) = setup_app_state();
        let resp = dispatch(rpc("system.health", json!({})), &state).await;
        assert!(resp.error.is_none());
        assert_eq!(rpc_ok(&resp)["status"], "ok");
    }

    #[tokio::test]
    async fn lifecycle_status_returns_readiness() {
        let (_tmp, state) = setup_app_state();
        let resp = dispatch(rpc("lifecycle.status", json!({})), &state).await;
        assert!(resp.error.is_none());
        assert_eq!(rpc_ok(&resp)["status"], "running");
        assert_eq!(rpc_ok(&resp)["schema"], 1);
        assert_eq!(rpc_ok(&resp)["ready"], true);
        assert_eq!(rpc_ok(&resp)["startup"]["phase"], "ready");
        assert!(rpc_ok(&resp)["ready_at"].is_string());
    }

    #[tokio::test]
    async fn dynamic_bootstrap_serves_status_and_classified_not_ready() {
        let tmp = TempDir::new().unwrap();
        let state = DynamicServerState::bootstrap(bootstrap_lifecycle(&tmp)).unwrap();

        let status =
            routing::dispatch_dynamic(rpc("lifecycle.status", json!({})), &state, None).await;
        assert_eq!(rpc_ok(&status)["status"], "starting");
        assert_eq!(rpc_ok(&status)["ready"], false);
        assert_eq!(rpc_ok(&status)["startup"]["sequence"], 0);

        let health = routing::dispatch_dynamic(rpc("system.health", json!({})), &state, None).await;
        assert_eq!(rpc_err(&health).code, "node_starting");

        let runtime =
            routing::dispatch_dynamic(rpc("runtime.get_thread", json!({})), &state, None).await;
        let error = rpc_err(&runtime);
        assert_eq!(error.code, "node_starting");
        assert!(error.retryable);
        assert_eq!(error.details["phase"], "bootstrapping");
        assert_eq!(error.details["sequence"], 0);

        // Application publication is a separate boundary from Ready. Once it
        // lands, token-gated recovery callbacks enter the runtime dispatcher
        // while ordinary external admission is still closed. A missing token
        // therefore fails at callback authentication, not at startup gating.
        let (_app_tmp, app) = setup_app_state();
        state.publish_application(Arc::new(app));
        let health = routing::dispatch_dynamic(rpc("system.health", json!({})), &state, None).await;
        assert_eq!(rpc_err(&health).code, "node_starting");
        let callback =
            routing::dispatch_dynamic(rpc("runtime.get_thread", json!({})), &state, None).await;
        let callback_error = rpc_err(&callback);
        assert_eq!(callback_error.code, "request_failed");
        assert!(callback_error.message.contains("missing callback_token"));
    }

    #[tokio::test]
    async fn dynamic_state_refuses_ready_before_application_publication() {
        let tmp = TempDir::new().unwrap();
        let starting = bootstrap_lifecycle(&tmp);
        let state = DynamicServerState::bootstrap(starting.clone()).unwrap();
        let ready = ryeos_node::LifecycleResponse::running(
            starting.identity,
            "2026-07-14T00:00:01Z",
            starting.startup,
        );
        let error = state.publish_lifecycle(ready).unwrap_err();
        assert!(error.to_string().contains("before application state"));
    }

    #[tokio::test]
    async fn unknown_method_returns_error() {
        let (_tmp, state) = setup_app_state();
        let resp = dispatch(rpc("nonexistent.method", json!({})), &state).await;
        let err = rpc_err(&resp);
        assert_eq!(err.code, "unknown_method");
    }

    // ── removed methods return unknown_method ──────────────────────

    #[tokio::test]
    async fn removed_methods_return_unknown() {
        let (_tmp, state) = setup_app_state();
        for method in [
            // V5.2 removed catalog methods
            "node.status",
            "identity.public_key",
            "threads.get",
            "threads.list",
            "threads.children",
            "threads.chain",
            // V5.2 cleanup: all runtime-internal bare methods removed
            "threads.create",
            "threads.mark_running",
            "threads.attach_process",
            "threads.finalize",
            "threads.request_continuation",
            "events.append",
            "events.append_batch",
            "events.replay",
            "commands.submit",
            "commands.claim",
            "commands.complete",
            "artifacts.publish",
            "threads.get_facets",
        ] {
            let resp = dispatch(rpc(method, json!({})), &state).await;
            assert_eq!(
                rpc_err(&resp).code,
                "unknown_method",
                "expected unknown_method for {method}"
            );
        }
    }

    /// Assert that only health and local lifecycle control are ungated on the
    /// bare UDS surface. Every other method must go through token-gated
    /// `runtime.*` or be unknown.
    #[tokio::test]
    async fn only_health_and_lifecycle_control_are_ungated() {
        let (_tmp, state) = setup_app_state();

        // system.health and lifecycle.status must work
        let resp = dispatch(rpc("system.health", json!({})), &state).await;
        assert!(resp.error.is_none());
        assert_eq!(rpc_ok(&resp)["status"], "ok");
        let resp = dispatch(rpc("lifecycle.status", json!({})), &state).await;
        assert!(resp.error.is_none());
        assert_eq!(rpc_ok(&resp)["status"], "running");

        // A sample of methods that MUST NOT be ungated
        for method in [
            "threads.create",
            "events.replay",
            "commands.submit",
            "artifacts.publish",
        ] {
            let resp = dispatch(rpc(method, json!({})), &state).await;
            assert_eq!(
                rpc_err(&resp).code,
                "unknown_method",
                "bare-namespace method `{method}` should not be ungated"
            );
        }
    }

    // ── thread lifecycle (runtime-internal, via runtime.*) ──────────

    #[test]
    fn runtime_finalize_requires_every_exact_wire_key() {
        let complete = json!({
            "thread_id": "T-exact",
            "status": "completed",
            "outcome_code": null,
            "result": null,
            "error": null,
            "cost": null,
            "outputs": null,
            "warnings": [],
        });
        assert!(serde_json::from_value::<RuntimeFinalizeParams>(complete.clone()).is_ok());

        for key in [
            "thread_id",
            "status",
            "outcome_code",
            "result",
            "error",
            "cost",
            "outputs",
            "warnings",
        ] {
            let mut incomplete = complete.clone();
            incomplete.as_object_mut().unwrap().remove(key);
            assert!(
                serde_json::from_value::<RuntimeFinalizeParams>(incomplete).is_err(),
                "omitting `{key}` must violate runtime.finalize_thread's exact wire"
            );
        }
    }

    #[tokio::test]
    async fn runtime_finalize_thread_works() {
        let (_tmp, state) = setup_app_state();
        let params = make_create_params("T-1", "T-1");

        // threads.create is internal — call service directly
        state.threads.create_thread_for_test(&params).unwrap();

        let cbt = generate_test_callback(
            &state,
            "T-1",
            std::path::PathBuf::from("/test"),
            std::time::Duration::from_secs(300),
            Vec::new(),
            test_provenance(&state, "/test"),
            "0".repeat(64),
        );

        let resp = dispatch(
            rpc(
                "runtime.finalize_thread",
                json!({
                    "callback_token": cbt.token,
                    "thread_id": "T-1",
                    "status": "completed",
                    "outcome_code": "success",
                    "result": "4",
                    "error": null,
                    "cost": {"input_tokens": 10, "output_tokens": 2, "total_usd": 0.01},
                    "outputs": null,
                    "warnings": [],
                }),
            ),
            &state,
        )
        .await;
        assert!(resp.error.is_none(), "finalize failed: {:?}", resp.error);

        // The completion's result and outcome_code must be persisted and
        // readable, not only returned live.
        let persisted = state
            .threads
            .get_thread_result("T-1")
            .unwrap()
            .expect("thread result row present after finalize");
        assert_eq!(persisted.outcome_code.as_deref(), Some("success"));
        assert_eq!(persisted.result, Some(json!("4")));
        let authority = state
            .threads
            .get_thread_terminal_authority("T-1")
            .unwrap()
            .expect("terminal authority present");
        let envelope = authority
            .managed_envelope
            .expect("managed runtime envelope persisted in signed snapshot");
        assert_eq!(envelope["result"], "4");
        assert_eq!(envelope["outputs"], Value::Null);
        assert_eq!(envelope["warnings"], json!([]));
    }

    #[test]
    fn runtime_finalize_uses_closed_terminal_status_enum() {
        for status in ["timed_out", "running", "done"] {
            let value = json!({
                "thread_id": "T-test",
                "status": status,
                "outputs": null,
                "warnings": [],
            });
            assert!(serde_json::from_value::<RuntimeFinalizeParams>(value).is_err());
        }
    }

    #[test]
    fn runtime_finalize_requires_outputs_and_warnings() {
        let complete = json!({
            "thread_id": "T-test",
            "status": "completed",
            "outputs": null,
            "warnings": [],
        });
        let mut missing_outputs = complete.clone();
        missing_outputs
            .as_object_mut()
            .expect("runtime finalize object")
            .remove("outputs");
        let mut missing_warnings = complete;
        missing_warnings
            .as_object_mut()
            .expect("runtime finalize object")
            .remove("warnings");

        assert!(serde_json::from_value::<RuntimeFinalizeParams>(missing_outputs).is_err());
        assert!(serde_json::from_value::<RuntimeFinalizeParams>(missing_warnings).is_err());
    }

    #[test]
    fn runtime_finalize_cost_shape_is_strict() {
        for cost in [
            json!({"input_tokens": 1, "total_usd": 0.01}),
            json!({"input_tokens": -1, "output_tokens": 2, "total_usd": 0.01}),
            json!({
                "input_tokens": 1,
                "output_tokens": 2,
                "total_usd": 0.01,
                "unexpected": true,
            }),
        ] {
            let value = json!({
                "thread_id": "T-test",
                "status": "completed",
                "cost": cost,
                "outputs": null,
                "warnings": [],
            });
            assert!(serde_json::from_value::<RuntimeFinalizeParams>(value).is_err());
        }
    }

    #[test]
    fn runtime_finalize_cost_rejects_invalid_values_and_storage_overflow() {
        for cost in [
            RuntimeFinalizeCost {
                input_tokens: 1,
                output_tokens: 2,
                total_usd: -0.01,
                basis: None,
            },
            RuntimeFinalizeCost {
                input_tokens: 1,
                output_tokens: 2,
                total_usd: f64::INFINITY,
                basis: None,
            },
            RuntimeFinalizeCost {
                input_tokens: 1,
                output_tokens: 2,
                total_usd: 0.01,
                basis: Some("estimate".to_string()),
            },
            RuntimeFinalizeCost {
                input_tokens: i64::MAX as u64 + 1,
                output_tokens: 2,
                total_usd: 0.01,
                basis: None,
            },
            RuntimeFinalizeCost {
                input_tokens: 1,
                output_tokens: i64::MAX as u64 + 1,
                total_usd: 0.01,
                basis: None,
            },
        ] {
            assert!(final_cost_from_runtime(&cost).is_err());
        }
    }

    // ── runtime.spawn_follow_child: auth + admission rejections ──────────
    // These reject before any mutation; the happy path / adoption / duplicate
    // cases need a managed runtime registered in the engine (D9) and live in a
    // dedicated fixture, not this empty-engine harness.

    const FOLLOW_KEY: &str = "P/gr-1/node-a/0";

    /// Create parent thread `P` (chain root `P`), make it native-resume (the
    /// follow gate requires a checkpoint-resumable parent), and mint a callback
    /// token (with `caps`) + a thread-auth token for it.
    fn setup_follow_parent(state: &AppState, caps: Vec<String>) -> (String, String) {
        state
            .threads
            .create_thread_for_test(&make_create_params("P", "P"))
            .unwrap();
        state.threads.mark_running("P").unwrap();
        state
            .state_store
            .seed_launch_metadata(
                "P",
                &ryeos_app::launch_metadata::RuntimeLaunchMetadata {
                    native_resume: Some(ryeos_engine::contracts::NativeResumeSpec {
                        checkpoint_interval_secs: 30,
                        max_auto_resume_attempts: 1,
                    }),
                    ..Default::default()
                },
            )
            .unwrap();
        let cbt = generate_test_callback(
            state,
            "P",
            std::path::PathBuf::from("/proj"),
            std::time::Duration::from_secs(300),
            caps,
            test_provenance(state, "/proj"),
            "0".repeat(64),
        );
        let tat = state.thread_auth.mint(
            "P",
            "user:test".to_string(),
            vec!["execute".to_string()],
            std::time::Duration::from_secs(300),
        );
        (cbt.token, tat.token)
    }

    fn follow_params(
        callback_token: &str,
        thread_auth_token: &str,
        child: &str,
    ) -> serde_json::Value {
        json!({
            "callback_token": callback_token,
            "thread_auth_token": thread_auth_token,
            "thread_id": "P",
            "project_path": "/proj",
            "graph_run_id": "gr-1",
            "follow_node": "node-a",
            "step_count": 0,
            "children": [{
                "item_ref": child,
                "ref_bindings": {},
                "parameters": {},
            }],
            "completion": {
                "status": "continued",
                "outcome_code": "continued",
                "result": null,
                "error": null,
                "cost": null,
                "outputs": null,
                "warnings": [],
            },
        })
    }

    fn no_waiter(state: &AppState) -> bool {
        state
            .state_store
            .get_follow_waiter_by_key(FOLLOW_KEY)
            .unwrap()
            .is_none()
    }

    fn no_waiter_key(state: &AppState, key: &str) -> bool {
        state
            .state_store
            .get_follow_waiter_by_key(key)
            .unwrap()
            .is_none()
    }

    fn new_successor_record(
        thread_id: &str,
        chain_root_id: &str,
        upstream: Option<&str>,
    ) -> ryeos_app::state_store::NewThreadRecord {
        let captured_history_policy = (thread_id == chain_root_id).then(|| {
            let hash = "a".repeat(64);
            ryeos_state::objects::CapturedThreadHistoryPolicy {
                retention: ryeos_state::objects::ThreadHistoryRetention::Durable,
                canonical_item_ref: "graph:test/graph".to_string(),
                item_content_hash: hash.clone(),
                item_signer_fingerprint: Some(hash.clone()),
                item_trust_class: ryeos_state::objects::CapturedItemTrustClass::Trusted,
                kind_schema_content_hash: hash,
                resolved_from: ryeos_state::objects::CapturedPolicyProvenance::NodeDefault {
                    node_policy:
                        ryeos_state::objects::CapturedNodeHistoryPolicyProvenance::MissingConfig,
                },
            }
        });
        ryeos_app::state_store::NewThreadRecord {
            thread_id: thread_id.to_string(),
            chain_root_id: chain_root_id.to_string(),
            kind: "graph".to_string(),
            item_ref: "graph:test/graph".to_string(),
            executor_ref: "test/executor".to_string(),
            launch_mode: "detached".to_string(),
            current_site_id: "site:test".to_string(),
            origin_site_id: "site:test".to_string(),
            upstream_thread_id: upstream.map(Into::into),
            requested_by: Some("user:test".to_string()),
            project_root: Some(std::path::PathBuf::from("/tmp/p")),
            project_authority: ryeos_state::objects::ExecutionProjectAuthority::pinned(
                "local:/tmp/p".to_string(),
                Some(std::path::PathBuf::from("/tmp/p")),
                "a".repeat(64),
                ryeos_state::objects::PinnedProjectRealization::Cow {
                    terminal_publication: ryeos_state::objects::PinnedTerminalPublication::Discard,
                },
                ryeos_state::objects::EnvironmentAuthority::None,
                Vec::new(),
            )
            .unwrap(),
            base_project_snapshot_hash: Some("a".repeat(64)),
            usage_subject: None,
            usage_subject_asserted_by: None,
            captured_history_policy,
        }
    }

    /// Build a running parent "P" with a captured ResumeContext, then its REAL
    /// graph-follow-resume successor "S" (marked + upstream-linked), advanced to
    /// `running` — the shape the `AlreadyClaimed` cleanup must accept.
    fn seed_marked_follow_successor(state: &AppState) {
        use ryeos_app::launch_metadata::{ResumeContext, RuntimeLaunchMetadata};
        use ryeos_engine::contracts::{
            EffectivePrincipal, ExecutionHints, Principal, ProjectContext,
        };
        let mut parent = make_create_params("P", "P");
        parent.project_root = Some(std::path::PathBuf::from("/tmp/p"));
        parent.base_project_snapshot_hash = Some("a".repeat(64));
        parent.project_authority = ryeos_state::objects::ExecutionProjectAuthority::pinned(
            "local:/tmp/p".to_string(),
            Some(std::path::PathBuf::from("/tmp/p")),
            "a".repeat(64),
            ryeos_state::objects::PinnedProjectRealization::Cow {
                terminal_publication: ryeos_state::objects::PinnedTerminalPublication::Discard,
            },
            ryeos_state::objects::EnvironmentAuthority::None,
            Vec::new(),
        )
        .unwrap();
        state.threads.create_thread_for_test(&parent).unwrap();
        state.threads.mark_running("P").unwrap();
        state
            .state_store
            .seed_launch_metadata(
                "P",
                &RuntimeLaunchMetadata::default()
                    .with_native_resume(ryeos_engine::contracts::NativeResumeSpec::default())
                    .with_resume_context(ResumeContext {
                        kind: "graph".into(),
                        item_ref: "graph:test/graph".into(),
                        ref_bindings: std::collections::BTreeMap::new(),
                        launch_mode: "detached".into(),
                        parameters: json!({}),
                        project_context: ProjectContext::LocalPath {
                            path: std::path::PathBuf::from("/tmp/p"),
                        },
                        project_authority: ryeos_state::objects::ExecutionProjectAuthority::pinned(
                            "local:/tmp/p".to_string(),
                            Some(std::path::PathBuf::from("/tmp/p")),
                            "a".repeat(64),
                            ryeos_state::objects::PinnedProjectRealization::Cow {
                                terminal_publication:
                                    ryeos_state::objects::PinnedTerminalPublication::Discard,
                            },
                            ryeos_state::objects::EnvironmentAuthority::None,
                            Vec::new(),
                        )
                        .unwrap(),
                        lifecycle_authority:
                            ryeos_state::objects::ExecutionLifecycleAuthority::DAEMON_RESTARTABLE,
                        stable_project_identity: Some(
                            ryeos_app::launch_metadata::StableProjectIdentity::from_path(
                                std::path::Path::new("/tmp/p"),
                                "site:test",
                            )
                            .unwrap(),
                        ),
                        local_overlay_root: Some(std::path::PathBuf::from("/tmp/p")),
                        original_snapshot_hash: Some("a".repeat(64)),
                        original_pushed_head_ref: None,
                        state_root: None,
                        current_site_id: "site:test".into(),
                        origin_site_id: "site:test".into(),
                        requested_by: EffectivePrincipal::Local(Principal {
                            fingerprint: "fp".into(),
                            scopes: vec![],
                        }),
                        execution_hints: ExecutionHints::default(),
                        effective_caps: vec![],
                        parent_delegation_caps: None,
                        executor_ref: None,
                        runtime_ref: None,
                    }),
            )
            .unwrap();
        state
            .state_store
            .create_follow_resume_successor(&new_successor_record("S", "P", Some("P")), "P", "P")
            .unwrap();
        state.threads.mark_running("S").unwrap();
    }

    /// Arm a `waiting` follow waiter (key `wk`) whose child chain is `child`.
    fn arm_waiting_follow(state: &AppState, wk: &str, child: &str) {
        arm_waiting_follow_succ(state, wk, child, "S");
    }

    /// Like [`arm_waiting_follow`] but with an explicit successor id. Use this when
    /// a single test arms MORE than one waiter — `follow_waiter.parent_successor_thread_id`
    /// is UNIQUE (a successor belongs to exactly one follow), so each must differ.
    fn arm_waiting_follow_succ(state: &AppState, wk: &str, child: &str, successor: &str) {
        // Follow mutations pin the authoritative parent chain. Model the real
        // suspended-parent shape instead of relying on runtime rows without a
        // signed state head.
        ensure_test_root(state, "P");
        state
            .state_store
            .reserve_follow(&ryeos_app::runtime_db::NewFollowWaiter {
                follow_key: wk.to_string(),
                parent_thread_id: "P".to_string(),
                parent_chain_root_id: "P".to_string(),
                follow_node: "n".to_string(),
                graph_run_id: "g".to_string(),
                step_count: 0,
                frontier_id: None,
                fanout: false,
                expected_children: 1,
                child_project_authority: None,
            })
            .unwrap();
        set_test_follow_child(state, wk, child);
        state
            .state_store
            .set_follow_parent_successor(wk, successor)
            .unwrap();
        state.state_store.mark_follow_waiting(wk).unwrap();
    }

    fn set_test_follow_child(state: &AppState, follow_key: &str, child: &str) {
        let sealed =
            ryeos_app::thread_lifecycle::SealedRootExecutionRequest::storage_test_fixture();
        let item_ref = sealed.item_ref();
        let hash = ryeos_app::runtime_db::follow_child_spec_hash(
            item_ref,
            &std::collections::BTreeMap::new(),
            &json!(null),
            None,
        )
        .unwrap();
        state
            .state_store
            .set_follow_child(follow_key, 0, item_ref, &hash, child, child, &sealed)
            .unwrap();
    }

    /// Persist the complete pre-launch authority reconciliation requires for a
    /// fresh follow child. This deliberately mirrors the production birth
    /// boundary: resume identity, sealed request, and parent context are one
    /// coherent record and agree with the waiter slot installed above.
    fn seed_pending_follow_child_metadata(state: &AppState, child: &str) {
        use ryeos_app::launch_metadata::{
            PersistedParentExecutionContext, ResumeContext, RuntimeLaunchMetadata,
        };
        use ryeos_engine::contracts::{EffectivePrincipal, Principal, ProjectContext};

        let sealed =
            ryeos_app::thread_lifecycle::SealedRootExecutionRequest::storage_test_fixture();
        let mut metadata = RuntimeLaunchMetadata::default()
            .with_launch_driver(ryeos_state::objects::ExecutionLaunchDriver::ManagedRuntime)
            .with_resume_context(ResumeContext {
                kind: "graph_run".to_string(),
                item_ref: sealed.item_ref().to_string(),
                ref_bindings: std::collections::BTreeMap::new(),
                launch_mode: "detached".to_string(),
                parameters: json!({}),
                project_context: ProjectContext::None,
                project_authority: ryeos_state::objects::ExecutionProjectAuthority::PROJECTLESS,
                lifecycle_authority:
                    ryeos_state::objects::ExecutionLifecycleAuthority::DAEMON_RESTARTABLE,
                stable_project_identity: None,
                local_overlay_root: None,
                original_snapshot_hash: None,
                original_pushed_head_ref: None,
                state_root: None,
                current_site_id: "site:test".to_string(),
                origin_site_id: "site:test".to_string(),
                requested_by: EffectivePrincipal::Local(Principal {
                    fingerprint: "session:test".to_string(),
                    scopes: Vec::new(),
                }),
                execution_hints: Default::default(),
                effective_caps: Vec::new(),
                parent_delegation_caps: Some(Vec::new()),
                executor_ref: Some(sealed.executor_ref().to_string()),
                runtime_ref: Some(sealed.runtime_ref().to_string()),
            })
            .with_admitted_artifact_identity(
                ryeos_state::objects::AdmittedLaunchArtifactIdentity::ManagedRuntime {
                    runtime_ref: sealed.runtime_ref().to_string(),
                    runtime_content_hash: "a".repeat(64),
                    runtime_signer_fingerprint: "fp:test-runtime".to_string(),
                    protocol_ref: "protocol:test/runtime".to_string(),
                    protocol_content_hash: "b".repeat(64),
                    protocol_signer_fingerprint: "fp:test-protocol".to_string(),
                    executor_ref: sealed.executor_ref().to_string(),
                    executor_content_hash: "c".repeat(64),
                    executor_bundle_manifest_hash: "d".repeat(64),
                    executor_bundle_signer_fingerprint: "fp:test-executor-bundle".to_string(),
                },
            )
            .with_admitted_prepared_launch(json!({
                "runtime_data": {},
                "required_secrets": [],
                "runtime_facts": {},
                "binding_records": {},
            }))
            .with_sealed_root_request(sealed);
        metadata.follow_parent_context = Some(PersistedParentExecutionContext {
            parent_thread_id: "P".to_string(),
            hard_limits: serde_json::Value::Null,
            depth: 0,
        });
        state
            .state_store
            .seed_launch_metadata(child, &metadata)
            .unwrap();
    }

    fn finalize_child(
        state: &AppState,
        child: &str,
        status: &str,
        result: Option<serde_json::Value>,
    ) {
        state
            .threads
            .finalize_thread(&ryeos_app::thread_lifecycle::ThreadFinalizeParams {
                thread_id: child.to_string(),
                status: status.to_string(),
                outcome_code: Some(status.to_string()),
                result,
                error: None,
                metadata: None,
                artifacts: vec![],
                final_cost: None,
                summary_json: None,
            })
            .unwrap();
    }

    #[tokio::test]
    async fn reconcile_follow_collects_ready_and_resuming_not_waiting() {
        let (_tmp, state) = setup_app_state();
        for child in ["CW", "CR", "CX"] {
            state
                .threads
                .create_thread_for_test(&make_create_params(child, child))
                .unwrap();
        }
        state.threads.mark_running("CW").unwrap();
        // A still-waiting waiter: its child chain has not been recorded terminal, so
        // the parent resume is not yet drivable — no intent. (Distinct successors:
        // parent_successor_thread_id is UNIQUE.)
        arm_waiting_follow_succ(&state, "wk-waiting", "CW", "S-w");
        // A waiter whose child chain reached terminal (flipped `waiting → ready`):
        // the parent resume IS drivable — one intent.
        arm_waiting_follow_succ(&state, "wk-ready", "CR", "S-r");
        state
            .state_store
            .mark_follow_child_terminal("CR", "CR", "completed", &json!({"success": true}))
            .unwrap();
        // A waiter whose resume was interrupted mid-flight (`resuming`) — re-driven,
        // so it too must be collected.
        arm_waiting_follow_succ(&state, "wk-resuming", "CX", "S-x");
        state
            .state_store
            .mark_follow_child_terminal("CX", "CX", "completed", &json!({"success": true}))
            .unwrap();
        state
            .state_store
            .mark_follow_resuming("wk-resuming")
            .unwrap();

        let actions = crate::reconcile::reconcile_follow(&state).unwrap();
        let resume_keys: Vec<&str> = actions
            .iter()
            .filter_map(|a| match a {
                crate::reconcile::FollowReconcileAction::Resume { follow_key } => {
                    Some(follow_key.as_str())
                }
                _ => None,
            })
            .collect();
        assert!(
            resume_keys.contains(&"wk-ready"),
            "a ready waiter must yield a parent-resume action, got {resume_keys:?}"
        );
        assert!(
            resume_keys.contains(&"wk-resuming"),
            "a resuming waiter must yield a parent-resume action, got {resume_keys:?}"
        );
        assert!(
            !resume_keys.contains(&"wk-waiting"),
            "a still-waiting waiter (no child row) must NOT yield a resume action, got {resume_keys:?}"
        );
    }

    #[tokio::test]
    async fn reconcile_follow_relaunches_pre_launch_child() {
        // Crash in the pre-launch window: the waiter is durably `waiting` but the
        // detached child launch never ran, so the child row is still `created`.
        // reconcile_follow must collect a relaunch (reconcile() proper skips it
        // rather than finalize-failing it).
        let (_tmp, state) = setup_app_state();
        state
            .threads
            .create_thread_for_test(&make_pending_follow_child_params("Cpre"))
            .unwrap();
        arm_waiting_follow(&state, "wk-pre", "Cpre");
        seed_pending_follow_child_metadata(&state, "Cpre");
        assert_eq!(
            state.threads.get_thread("Cpre").unwrap().unwrap().status,
            ryeos_state::objects::ThreadStatus::Created.as_str(),
            "child must be created (never launched) for this window"
        );

        let actions = crate::reconcile::reconcile_follow(&state).unwrap();
        let relaunch: Vec<&str> = actions
            .iter()
            .filter_map(|a| match a {
                crate::reconcile::FollowReconcileAction::RelaunchChild { child_thread_id } => {
                    Some(child_thread_id.as_str())
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            relaunch,
            vec!["Cpre"],
            "a waiting waiter with a created (never-launched) child must yield a relaunch, got {relaunch:?}"
        );
    }

    #[tokio::test]
    async fn reconcile_follow_does_not_relaunch_child_with_attached_pgid() {
        // A pgid attaches BEFORE the row flips created→running (launch in flight).
        // Such a child must NOT be relaunched — that would spawn a duplicate.
        let (_tmp, state) = setup_app_state();
        state
            .threads
            .create_thread_for_test(&make_create_params("Catt", "Catt"))
            .unwrap();
        state
            .threads
            .attach_process(&ryeos_app::thread_lifecycle::ThreadAttachProcessParams {
                thread_id: "Catt".to_string(),
                pid: 424242,
                pgid: 424242,
                process_identity: Some(ryeos_app::process::ExecutionProcessIdentity {
                    schema_version: ryeos_app::process::PROCESS_IDENTITY_SCHEMA_VERSION,
                    boot_id: "test-boot".to_string(),
                    target_pid: 424242,
                    target_start_time_ticks: 10,
                    group_leader_pid: 424242,
                    group_leader_start_time_ticks: 10,
                }),
                metadata: None,
                launch_metadata: Default::default(),
            })
            .unwrap();
        arm_waiting_follow(&state, "wk-att", "Catt");

        let actions = crate::reconcile::reconcile_follow(&state).unwrap();
        assert!(
            !actions.iter().any(|a| matches!(
                a,
                crate::reconcile::FollowReconcileAction::RelaunchChild { .. }
            )),
            "a child with an attached pgid (launch in flight) must NOT be relaunched, got {actions:?}"
        );
    }

    #[tokio::test]
    async fn reconcile_follow_does_not_relaunch_running_child() {
        // A running child is recovered by reconcile()'s native-resume path, not a
        // fresh follow relaunch.
        let (_tmp, state) = setup_app_state();
        state
            .threads
            .create_thread_for_test(&make_create_params("Crun", "Crun"))
            .unwrap();
        state.threads.mark_running("Crun").unwrap();
        arm_waiting_follow(&state, "wk-run", "Crun");

        let actions = crate::reconcile::reconcile_follow(&state).unwrap();
        assert!(
            !actions.iter().any(|a| matches!(
                a,
                crate::reconcile::FollowReconcileAction::RelaunchChild { .. }
            )),
            "a running child must NOT be follow-relaunched, got {actions:?}"
        );
    }

    #[tokio::test]
    async fn finalize_failed_and_kick_readies_follow_waiter() {
        // Regression: BOTH launch error paths (fresh follow-child launch AND
        // native-resume relaunch) finalize a failed follow child through
        // finalize_failed_and_kick_follow. Its finalize half must flip a waiting
        // follow waiter to `ready` so the kick has something to drive — otherwise a
        // relaunch failure leaves the parent suspended until the next restart.
        let (_tmp, state) = setup_app_state();
        state
            .threads
            .create_thread_for_test(&make_create_params("Cnr", "Cnr"))
            .unwrap();
        state.threads.mark_running("Cnr").unwrap();
        arm_waiting_follow(&state, "wk-nr", "Cnr");
        assert!(matches!(
            state
                .state_store
                .claim_thread_launch("Cnr", "claim-nr", "test-generation")
                .unwrap(),
            ryeos_app::runtime_db::LaunchClaimOutcome::Claimed
        ));
        let launch_owner = state
            .state_store
            .get_launch_claim("Cnr")
            .unwrap()
            .map(|claim| {
                lillux::canonical_json(&serde_json::to_value(claim.owner).unwrap()).unwrap()
            })
            .unwrap();

        ryeos_executor::execution::launch::finalize_failed_and_kick_follow(
            &state,
            "Cnr",
            "Cnr",
            &launch_owner,
            json!({ "error": "resume rebuild failed" }),
        )
        .unwrap();

        // The finalize half readied the waiter (synchronous; the kick is a detached
        // spawn that hasn't run yet). A hung waiter here == the bug Oracle flagged.
        assert_eq!(
            state
                .state_store
                .get_follow_waiter_by_key("wk-nr")
                .unwrap()
                .unwrap()
                .phase,
            ryeos_app::runtime_db::follow_phase::READY,
            "a failed follow-child (re)launch must ready the waiter for the parent resume"
        );
    }

    #[tokio::test]
    async fn continuation_successor_budget_failure_readies_follow_waiter() {
        // launch_successor_inner's budget-exhausted path must ready a followed
        // parent's waiter (via finalize_failed_and_kick_follow) — else a follow child
        // whose continuation successor can't relaunch strands the parent. Modeled
        // with a successor row awaited by a follow waiter; the finalize+kick code path
        // is identical whether or not it sits deeper in a chain.
        let (_tmp, state) = setup_app_state();
        state
            .threads
            .create_thread_for_test(&make_create_params("Ssucc", "Ssucc"))
            .unwrap();
        // Exhaust the per-successor auto-launch budget.
        for _ in 0..ryeos_app::thread_lifecycle::MAX_CONTINUATION_AUTO_ATTEMPTS {
            state.state_store.bump_resume_attempts("Ssucc").unwrap();
        }
        // A parent follow waiter awaits this successor's chain.
        arm_waiting_follow(&state, "wk-succ", "Ssucc");

        use ryeos_executor::execution::launch::SuccessorLaunchOutcome;
        let reason =
            match ryeos_executor::execution::launch::launch_successor(state.clone(), "Ssucc")
                .await
                .unwrap()
            {
                SuccessorLaunchOutcome::Skipped(r) => r,
                SuccessorLaunchOutcome::Launched(_) => {
                    panic!("a budget-exhausted successor must not launch")
                }
            };
        assert_eq!(reason, "budget_exhausted");
        assert_eq!(
            state
                .state_store
                .get_follow_waiter_by_key("wk-succ")
                .unwrap()
                .unwrap()
                .phase,
            ryeos_app::runtime_db::follow_phase::READY,
            "a budget-exhausted continuation successor in a followed chain must ready the waiter"
        );
    }

    #[tokio::test]
    async fn reconcile_follow_recovers_terminal_unrecorded_child() {
        // Crash window: the child chain reached a terminal that was persisted, but
        // the waiter was never flipped (record_follow_child_terminal never ran).
        // reconcile() skips terminal threads, so reconcile_follow must recover it →
        // ready → resume, or the parent hangs forever.
        let (_tmp, state) = setup_app_state();
        state
            .threads
            .create_thread_for_test(&make_create_params("Cterm", "Cterm"))
            .unwrap();
        state.threads.mark_running("Cterm").unwrap();
        // RAW state-store finalize bypasses record_follow_child_terminal, leaving the
        // waiter `waiting` — exactly the crash window.
        state
            .state_store
            .finalize_thread(
                "Cterm",
                &ryeos_app::state_store::FinalizeThreadRecord {
                    status: "completed".to_string(),
                    outcome_code: Some("success".to_string()),
                    result_json: Some(json!({ "answer": 42 })),
                    error_json: None,
                    artifacts: vec![],
                    final_cost: None,
                    managed_envelope: None,
                    result_project_snapshot_hash: None,
                },
            )
            .unwrap();
        arm_waiting_follow(&state, "wk-term", "Cterm");
        assert_eq!(
            state
                .state_store
                .get_follow_waiter_by_key("wk-term")
                .unwrap()
                .unwrap()
                .phase,
            ryeos_app::runtime_db::follow_phase::WAITING,
            "precondition: waiter is still waiting (terminal not recorded)"
        );

        let actions = crate::reconcile::reconcile_follow(&state).unwrap();
        assert!(
            actions.iter().any(|a| matches!(
                a,
                crate::reconcile::FollowReconcileAction::Resume { follow_key } if follow_key == "wk-term"
            )),
            "a terminal-but-unrecorded child must be recovered to a resume, got {actions:?}"
        );
        let waiter = state
            .state_store
            .get_follow_waiter_by_key("wk-term")
            .unwrap()
            .unwrap();
        assert_eq!(
            waiter.phase,
            ryeos_app::runtime_db::follow_phase::READY,
            "recovery must flip the waiter to ready"
        );
        // The synthesized envelope is a VISIBLE degraded FAILURE (so the parent
        // resumes into on_error, not a silent empty success), and carries the
        // persisted child status/result for diagnostics.
        let env = waiter.children[0]
            .terminal_envelope
            .as_ref()
            .expect("recovered waiter must carry a terminal envelope");
        assert_eq!(
            env["success"],
            json!(false),
            "degraded recovery is failure-shaped"
        );
        assert_eq!(env["status"], json!("failed"));
        assert_eq!(
            env["result"]["child_status"],
            json!("completed"),
            "envelope carries the persisted child status"
        );
        assert_eq!(
            env["result"]["child_result"],
            json!({ "answer": 42 }),
            "envelope carries the persisted child result"
        );
    }

    #[tokio::test]
    async fn cancelling_follow_child_resumes_parent_on_error() {
        // Cancellation contract for a suspended follow: cancelling the CHILD flips the
        // parent's waiter to ready with a VISIBLE failure envelope, so the parent
        // resumes into on_error (not a silent success). The parent itself is
        // `continued` and not cancellable; cancelling the resume successor instead
        // abandons the resume (handled in launch_follow_resume_successor).
        let (_tmp, state) = setup_app_state();
        state
            .threads
            .create_thread_for_test(&make_create_params("Ccancel", "Ccancel"))
            .unwrap();
        state.threads.mark_running("Ccancel").unwrap();
        arm_waiting_follow(&state, "wk-cancel", "Ccancel");
        // What threads/cancel does to the child: finalize it `cancelled`.
        finalize_child(&state, "Ccancel", "cancelled", None);

        let waiter = state
            .state_store
            .get_follow_waiter_by_key("wk-cancel")
            .unwrap()
            .unwrap();
        assert_eq!(
            waiter.phase,
            ryeos_app::runtime_db::follow_phase::READY,
            "cancelling the child must ready the parent's waiter"
        );
        let env = waiter.children[0]
            .terminal_envelope
            .as_ref()
            .expect("cancelled child must store a terminal envelope");
        assert_eq!(
            env["success"],
            json!(false),
            "a cancelled child resumes the parent into on_error, not a silent success"
        );
        assert_eq!(env["status"], json!("failed"));
        assert_eq!(
            env["result"]["child_status"],
            json!("cancelled"),
            "envelope carries the cancelled child status"
        );
    }

    #[tokio::test]
    async fn auxiliary_thread_terminal_in_followed_chain_does_not_ready_the_waiter() {
        // A followed child's launch pipeline runs AUXILIARY threads in the
        // child's own chain (e.g. a launch-time knowledge composition). The
        // first of those completes in milliseconds; recording it would resume
        // the parent with the auxiliary's envelope while the child is still
        // launching. Only the child itself — or a continuation successor of
        // it — settles the follow.
        let (_tmp, state) = setup_app_state();
        state
            .threads
            .create_thread_for_test(&make_create_params("Cfollow", "Cfollow"))
            .unwrap();
        state.threads.mark_running("Cfollow").unwrap();
        arm_waiting_follow(&state, "wk-aux", "Cfollow");

        // Auxiliary run riding the child's chain: own thread id, child's chain root.
        state
            .threads
            .create_thread_for_test(&make_create_params("Kaux", "Cfollow"))
            .unwrap();
        state.threads.mark_running("Kaux").unwrap();
        finalize_child(&state, "Kaux", "completed", Some(json!({ "positions": 1 })));

        let waiter = state
            .state_store
            .get_follow_waiter_by_key("wk-aux")
            .unwrap()
            .unwrap();
        assert_eq!(
            waiter.phase,
            ryeos_app::runtime_db::follow_phase::WAITING,
            "an auxiliary thread's terminal must not ready the waiter"
        );

        // The recovery path must agree: the chain's lineage tip (the child) is
        // still running, so there is nothing to recover despite a completed
        // auxiliary thread sitting in the chain.
        let actions = crate::reconcile::reconcile_follow(&state).unwrap();
        assert!(
            !actions.iter().any(|a| matches!(
                a,
                crate::reconcile::FollowReconcileAction::Resume { follow_key } if follow_key == "wk-aux"
            )),
            "recovery must not resume off an auxiliary terminal, got {actions:?}"
        );

        // The child's own terminal still settles the follow.
        finalize_child(&state, "Cfollow", "completed", Some(json!({ "ok": true })));
        let waiter = state
            .state_store
            .get_follow_waiter_by_key("wk-aux")
            .unwrap()
            .unwrap();
        assert_eq!(
            waiter.phase,
            ryeos_app::runtime_db::follow_phase::READY,
            "the child's own terminal readies the waiter"
        );
        assert_eq!(
            waiter.children[0].terminal_thread_id.as_deref(),
            Some("Cfollow"),
            "the recorded terminal is the child, not the auxiliary"
        );
    }

    #[tokio::test]
    async fn reconcile_follow_converges_reserved_with_child_and_successor() {
        // Partial spawn: child + successor recorded (so the parent is continued), but
        // crashed before mark_follow_waiting → stuck `reserved`. Converge to waiting;
        // the still-created child then yields a relaunch.
        let (_tmp, state) = setup_app_state();
        state
            .threads
            .create_thread_for_test(&make_pending_follow_child_params("Cres"))
            .unwrap();
        ensure_test_root(&state, "P");
        state
            .state_store
            .reserve_follow(&ryeos_app::runtime_db::NewFollowWaiter {
                follow_key: "wk-res".to_string(),
                parent_thread_id: "P".to_string(),
                parent_chain_root_id: "P".to_string(),
                follow_node: "n".to_string(),
                graph_run_id: "g".to_string(),
                step_count: 0,
                frontier_id: None,
                fanout: false,
                expected_children: 1,
                child_project_authority: None,
            })
            .unwrap();
        set_test_follow_child(&state, "wk-res", "Cres");
        seed_pending_follow_child_metadata(&state, "Cres");
        state
            .state_store
            .set_follow_parent_successor("wk-res", "S")
            .unwrap();
        assert_eq!(
            state
                .state_store
                .get_follow_waiter_by_key("wk-res")
                .unwrap()
                .unwrap()
                .phase,
            ryeos_app::runtime_db::follow_phase::RESERVED,
            "precondition: stuck reserved (mark_follow_waiting never ran)"
        );

        let actions = crate::reconcile::reconcile_follow(&state).unwrap();
        assert!(
            actions.iter().any(|a| matches!(
                a,
                crate::reconcile::FollowReconcileAction::RelaunchChild { child_thread_id } if child_thread_id == "Cres"
            )),
            "a reserved waiter with recorded child+successor + continued parent must converge and relaunch, got {actions:?}"
        );
        assert_eq!(
            state
                .state_store
                .get_follow_waiter_by_key("wk-res")
                .unwrap()
                .unwrap()
                .phase,
            ryeos_app::runtime_db::follow_phase::WAITING,
            "convergence must mark the waiter waiting"
        );
    }

    #[tokio::test]
    async fn reconcile_follow_leaves_reserved_when_parent_not_continued() {
        // Reserved, child recorded, but no successor and parent not continued → the
        // parent's own native resume re-drives spawn_follow_child; leave it.
        let (_tmp, state) = setup_app_state();
        ensure_test_root(&state, "Pnc");
        state
            .state_store
            .reserve_follow(&ryeos_app::runtime_db::NewFollowWaiter {
                follow_key: "wk-res2".to_string(),
                parent_thread_id: "Pnc".to_string(),
                parent_chain_root_id: "Pnc".to_string(),
                follow_node: "n".to_string(),
                graph_run_id: "g".to_string(),
                step_count: 0,
                frontier_id: None,
                fanout: false,
                expected_children: 1,
                child_project_authority: None,
            })
            .unwrap();
        set_test_follow_child(&state, "wk-res2", "Cnc");

        let actions = crate::reconcile::reconcile_follow(&state).unwrap();
        assert!(
            actions.is_empty(),
            "a reserved waiter whose parent has not continued must be left for the parent resume, got {actions:?}"
        );
        assert_eq!(
            state
                .state_store
                .get_follow_waiter_by_key("wk-res2")
                .unwrap()
                .unwrap()
                .phase,
            ryeos_app::runtime_db::follow_phase::RESERVED,
            "the waiter must remain reserved"
        );
    }

    #[tokio::test]
    async fn follow_resume_claim_held_by_advanced_marked_successor_clears_waiter() {
        // Blocker-1 recovery: a `resuming` waiter whose VALID follow-resume successor
        // was claimed + run by another launcher (e.g. a native-resume intent) must be
        // retired, not left `resuming` until a future restart.
        let (_tmp, state) = setup_app_state();
        // "S" is a real marked follow-resume successor of "P", advanced to running.
        seed_marked_follow_successor(&state);
        arm_waiting_follow(&state, "wk-held", "C");
        state
            .state_store
            .mark_follow_child_terminal("C", "C", "completed", &json!({"success": true}))
            .unwrap();
        state.state_store.mark_follow_resuming("wk-held").unwrap();
        // Someone else holds the launch claim on "S".
        assert!(matches!(
            state
                .state_store
                .claim_thread_launch("S", "other-claim", "other:test")
                .unwrap(),
            ryeos_app::runtime_db::LaunchClaimOutcome::Claimed
        ));

        use ryeos_executor::execution::launch::SuccessorLaunchOutcome;
        let reason = match ryeos_executor::execution::launch::launch_follow_resume_successor(
            state.clone(),
            "wk-held",
        )
        .await
        .unwrap()
        {
            SuccessorLaunchOutcome::Skipped(r) => r,
            SuccessorLaunchOutcome::Launched(_) => panic!("claim is held → must not launch"),
        };
        assert_eq!(reason, "already_claimed");
        assert!(
            no_waiter_key(&state, "wk-held"),
            "a VALID advanced follow successor means the resume is done — waiter cleared"
        );
    }

    #[tokio::test]
    async fn follow_resume_claim_held_by_unmarked_successor_keeps_waiter() {
        // Blocker-1 fail-closed: a `resuming` waiter pointing at a claimed row that is
        // NOT this parent's graph-follow-resume successor must NOT be cleared — the
        // AlreadyClaimed cleanup validates upstream + marker before retiring.
        let (_tmp, state) = setup_app_state();
        // A raw running "S" with no follow-resume marker (upstream None ≠ parent "P").
        state
            .threads
            .create_thread_for_test(&make_create_params("S", "S"))
            .unwrap();
        state.threads.mark_running("S").unwrap();
        arm_waiting_follow(&state, "wk-unmarked", "C");
        state
            .state_store
            .mark_follow_child_terminal("C", "C", "completed", &json!({"success": true}))
            .unwrap();
        state
            .state_store
            .mark_follow_resuming("wk-unmarked")
            .unwrap();
        assert!(matches!(
            state
                .state_store
                .claim_thread_launch("S", "other-claim", "other:test")
                .unwrap(),
            ryeos_app::runtime_db::LaunchClaimOutcome::Claimed
        ));

        use ryeos_executor::execution::launch::SuccessorLaunchOutcome;
        let reason = match ryeos_executor::execution::launch::launch_follow_resume_successor(
            state.clone(),
            "wk-unmarked",
        )
        .await
        .unwrap()
        {
            SuccessorLaunchOutcome::Skipped(r) => r,
            SuccessorLaunchOutcome::Launched(_) => panic!("claim is held → must not launch"),
        };
        assert_eq!(reason, "already_claimed");
        assert!(
            !no_waiter_key(&state, "wk-unmarked"),
            "claim held by an UNMARKED row must NOT clear the waiter (fail closed)"
        );
    }

    #[tokio::test]
    async fn follow_resume_refuses_successor_without_marker() {
        // Blocker-2 guard: a waiter pointing at a row that is NOT the parent's
        // graph-follow-resume successor must not be spliced or launched, and the
        // waiter must be left intact (suspected corruption is for inspection).
        let (_tmp, state) = setup_app_state();
        // "S" links upstream to the parent "P" but carries NO follow-resume edge.
        ensure_test_root(&state, "P");
        let mut params = make_create_params("S", "P");
        params.upstream_thread_id = Some("P".to_string());
        state
            .state_store
            .create_thread_for_test(&NewThreadRecord {
                thread_id: params.thread_id,
                chain_root_id: params.chain_root_id,
                kind: params.kind,
                item_ref: params.item_ref,
                executor_ref: params.executor_ref,
                launch_mode: params.launch_mode,
                current_site_id: params.current_site_id,
                origin_site_id: params.origin_site_id,
                upstream_thread_id: params.upstream_thread_id,
                requested_by: params.requested_by,
                project_root: params.project_root,
                project_authority: params.project_authority,
                base_project_snapshot_hash: params.base_project_snapshot_hash,
                usage_subject: params.usage_subject,
                usage_subject_asserted_by: params.usage_subject_asserted_by,
                captured_history_policy: params.captured_history_policy,
            })
            .unwrap();
        arm_waiting_follow(&state, "wk-nomarker", "C");
        state
            .state_store
            .mark_follow_child_terminal("C", "C", "completed", &json!({"success": true}))
            .unwrap();

        use ryeos_executor::execution::launch::SuccessorLaunchOutcome;
        let reason = match ryeos_executor::execution::launch::launch_follow_resume_successor(
            state.clone(),
            "wk-nomarker",
        )
        .await
        .unwrap()
        {
            SuccessorLaunchOutcome::Skipped(r) => r,
            SuccessorLaunchOutcome::Launched(_) => {
                panic!("a successor without the follow-resume marker must not launch")
            }
        };
        assert_eq!(reason, "not_follow_successor");
        assert!(
            !no_waiter_key(&state, "wk-nomarker"),
            "refusal on a suspected-bad successor must NOT clear the waiter"
        );
        // "S" was never launched — still `created`.
        assert_eq!(
            state.threads.get_thread("S").unwrap().unwrap().status,
            ryeos_state::objects::ThreadStatus::Created.as_str()
        );
    }

    #[tokio::test]
    async fn finalize_thread_without_envelope_degrades_follow_waiter() {
        let (_tmp, state) = setup_app_state();
        state
            .threads
            .create_thread_for_test(&make_create_params("C", "C"))
            .unwrap();
        state.threads.mark_running("C").unwrap();
        arm_waiting_follow(&state, "wk-degraded", "C");

        // The generic finalize path carries NO canonical envelope. A follow waiter
        // consuming it gets a visible in-band FAILURE, not a silent empty success —
        // so the parent resumes into its on_error path.
        finalize_child(&state, "C", "completed", Some(json!({ "answer": 42 })));

        let w = state
            .state_store
            .get_follow_waiter_by_key("wk-degraded")
            .unwrap()
            .unwrap();
        assert_eq!(w.phase, ryeos_app::runtime_db::follow_phase::READY);
        let env = w.children[0]
            .terminal_envelope
            .as_ref()
            .expect("degraded envelope stored");
        assert_eq!(env["success"], json!(false));
        assert_eq!(env["status"], json!("failed"));
        assert_eq!(
            env["result"]["code"],
            json!("degraded_follow_child_terminal_envelope")
        );
        assert_eq!(env["result"]["child_status"], json!("completed"));
    }

    #[tokio::test]
    async fn finalize_with_managed_envelope_preserves_outputs_on_follow_waiter() {
        let (_tmp, state) = setup_app_state();
        state
            .threads
            .create_thread_for_test(&make_create_params("C", "C"))
            .unwrap();
        state.threads.mark_running("C").unwrap();
        arm_waiting_follow(&state, "wk-mgd", "C");

        // The executor-fallback path carries the canonical envelope: outputs +
        // warnings survive to the follow waiter as a success.
        let envelope = json!({
            "success": true,
            "status": "completed",
            "result": "directive_return",
            "outputs": { "recommendations": ["x"] },
            "warnings": ["w1"],
            "cost": { "input_tokens": 5, "output_tokens": 1, "total_usd": 0.001 },
        });
        state
            .threads
            .finalize_thread_with_managed_envelope(
                &ryeos_app::thread_lifecycle::ThreadFinalizeParams {
                    thread_id: "C".to_string(),
                    status: "completed".to_string(),
                    outcome_code: Some("success".to_string()),
                    result: Some(json!("directive_return")),
                    error: None,
                    metadata: None,
                    artifacts: vec![],
                    final_cost: Some(ryeos_engine::contracts::FinalCost {
                        turns: 0,
                        input_tokens: 5,
                        output_tokens: 1,
                        spend: 0.001,
                        provider: None,
                        basis: None,
                        metadata: None,
                    }),
                    summary_json: None,
                },
                envelope,
            )
            .unwrap();

        let w = state
            .state_store
            .get_follow_waiter_by_key("wk-mgd")
            .unwrap()
            .unwrap();
        assert_eq!(w.phase, ryeos_app::runtime_db::follow_phase::READY);
        let env = w.children[0]
            .terminal_envelope
            .as_ref()
            .expect("canonical envelope stored");
        assert_eq!(env["success"], json!(true));
        assert_eq!(env["outputs"]["recommendations"], json!(["x"]));
        assert_eq!(env["warnings"], json!(["w1"]));
    }

    #[tokio::test]
    async fn finalize_continued_does_not_flip_follow_waiter() {
        let (_tmp, state) = setup_app_state();
        state
            .threads
            .create_thread_for_test(&make_create_params("C", "C"))
            .unwrap();
        state.threads.mark_running("C").unwrap();
        arm_waiting_follow(&state, "wk-cont", "C");

        // A `continued` finalize is an intermediate link in the child's own chain,
        // not the terminal tail — the waiter stays `waiting`.
        finalize_child(&state, "C", "continued", None);

        let w = state
            .state_store
            .get_follow_waiter_by_key("wk-cont")
            .unwrap()
            .unwrap();
        assert_eq!(w.phase, ryeos_app::runtime_db::follow_phase::WAITING);
        assert!(w.children[0].terminal_envelope.is_none());
    }

    #[tokio::test]
    async fn runtime_finalize_carries_outputs_and_warnings_to_follow_waiter() {
        let (_tmp, state) = setup_app_state();
        state
            .threads
            .create_thread_for_test(&make_create_params("C", "C"))
            .unwrap();
        state.threads.mark_running("C").unwrap();
        arm_waiting_follow(&state, "wk-out", "C");

        // The child SELF-finalizes via the runtime callback wire (not the executor
        // fallback), carrying a `directive_return`-style result plus its structured
        // outputs + warnings.
        let cbt = generate_test_callback(
            &state,
            "C",
            std::path::PathBuf::from("/proj"),
            std::time::Duration::from_secs(300),
            vec![],
            test_provenance(&state, "/proj"),
            "0".repeat(64),
        );
        let resp = dispatch(
            rpc(
                "runtime.finalize_thread",
                json!({
                    "callback_token": cbt.token,
                    "thread_id": "C",
                    "status": "completed",
                    "outcome_code": "success",
                    "result": "directive_return",
                    "error": null,
                    "outputs": { "recommendations": ["a", "b"] },
                    "warnings": ["w1"],
                    "cost": { "input_tokens": 10, "output_tokens": 2, "total_usd": 0.01 },
                }),
            ),
            &state,
        )
        .await;
        assert!(resp.error.is_none(), "finalize failed: {:?}", resp.error);

        // The stored follow envelope preserves outputs + warnings + raw cost — not
        // the fabricated empty forms.
        let w = state
            .state_store
            .get_follow_waiter_by_key("wk-out")
            .unwrap()
            .unwrap();
        assert_eq!(w.phase, ryeos_app::runtime_db::follow_phase::READY);
        let env = w.children[0]
            .terminal_envelope
            .as_ref()
            .expect("canonical envelope stored");
        assert_eq!(env["success"], json!(true));
        assert_eq!(env["result"], json!("directive_return"));
        assert_eq!(env["outputs"]["recommendations"], json!(["a", "b"]));
        assert_eq!(env["warnings"], json!(["w1"]));
        assert_eq!(env["cost"]["input_tokens"], json!(10));
    }

    #[tokio::test]
    async fn spawn_follow_child_rejects_missing_thread_auth_token() {
        let (_tmp, state) = setup_app_state();
        let (cbt, _tat) = setup_follow_parent(&state, vec!["ryeos.execute.tool.echo".into()]);
        let mut params = follow_params(&cbt, "unused", "tool:echo");
        params.as_object_mut().unwrap().remove("thread_auth_token");
        let resp = dispatch(rpc("runtime.spawn_follow_child", params), &state).await;
        assert!(resp.error.is_some());
        assert!(no_waiter(&state));
    }

    #[tokio::test]
    async fn spawn_follow_child_rejects_invalid_thread_auth_token() {
        let (_tmp, state) = setup_app_state();
        let (cbt, _tat) = setup_follow_parent(&state, vec!["ryeos.execute.tool.echo".into()]);
        let resp = dispatch(
            rpc(
                "runtime.spawn_follow_child",
                follow_params(&cbt, "tat-bogus", "tool:echo"),
            ),
            &state,
        )
        .await;
        assert!(resp.error.is_some());
        assert!(no_waiter(&state));
    }

    #[tokio::test]
    async fn spawn_follow_child_rejects_invalid_callback_token() {
        let (_tmp, state) = setup_app_state();
        let (_cbt, tat) = setup_follow_parent(&state, vec!["ryeos.execute.tool.echo".into()]);
        let resp = dispatch(
            rpc(
                "runtime.spawn_follow_child",
                follow_params("cbt-bogus", &tat, "tool:echo"),
            ),
            &state,
        )
        .await;
        assert!(resp.error.is_some());
        assert!(no_waiter(&state));
    }

    #[tokio::test]
    async fn spawn_follow_child_rejects_chain_root_mismatch() {
        let (_tmp, state) = setup_app_state();
        let (cbt, tat) = setup_follow_parent(&state, vec!["ryeos.execute.tool.echo".into()]);
        // Point the cap at a chain root other than the parent row's.
        assert!(state.callback_tokens.set_chain_root(&cbt, "OTHER-CHAIN"));
        let resp = dispatch(
            rpc(
                "runtime.spawn_follow_child",
                follow_params(&cbt, &tat, "tool:echo"),
            ),
            &state,
        )
        .await;
        assert!(resp.error.is_some());
        assert!(no_waiter(&state));
    }

    #[tokio::test]
    async fn spawn_follow_child_rejects_missing_execute_cap_without_mutation() {
        let (_tmp, state) = setup_app_state();
        // Parent holds an unrelated cap, not execute over `tool:echo`.
        let (cbt, tat) = setup_follow_parent(&state, vec!["ryeos.execute.tool.other".into()]);
        let resp = dispatch(
            rpc(
                "runtime.spawn_follow_child",
                follow_params(&cbt, &tat, "tool:echo"),
            ),
            &state,
        )
        .await;
        assert!(resp.error.is_some());
        assert!(no_waiter(&state));
    }

    #[tokio::test]
    async fn spawn_follow_child_rejects_non_native_resume_parent() {
        let (_tmp, state) = setup_app_state();
        // Parent has full authority but is NOT native-resume (no launch metadata
        // seeded) → refused: follow needs a checkpoint-resumable parent.
        state
            .threads
            .create_thread_for_test(&make_create_params("P", "P"))
            .unwrap();
        state.threads.mark_running("P").unwrap();
        let cbt = generate_test_callback(
            &state,
            "P",
            std::path::PathBuf::from("/proj"),
            std::time::Duration::from_secs(300),
            vec!["ryeos.execute.tool.*".into()],
            test_provenance(&state, "/proj"),
            "0".repeat(64),
        );
        let tat = state.thread_auth.mint(
            "P",
            "user:test".to_string(),
            vec!["execute".to_string()],
            std::time::Duration::from_secs(300),
        );
        let resp = dispatch(
            rpc(
                "runtime.spawn_follow_child",
                follow_params(&cbt.token, &tat.token, "tool:echo"),
            ),
            &state,
        )
        .await;
        assert!(resp.error.is_some());
        assert!(no_waiter(&state));
    }

    #[tokio::test]
    async fn spawn_follow_child_rejects_unmanaged_child_kind() {
        let (_tmp, state) = setup_app_state();
        // Parent HAS execute authority (wildcard), so admission passes the cap
        // gate; the empty test engine has no runtime for `tool`, so the managed-
        // runtime check rejects — and still no waiter is created.
        let (cbt, tat) = setup_follow_parent(&state, vec!["ryeos.execute.tool.*".into()]);
        let resp = dispatch(
            rpc(
                "runtime.spawn_follow_child",
                follow_params(&cbt, &tat, "tool:echo"),
            ),
            &state,
        )
        .await;
        assert!(resp.error.is_some());
        assert!(no_waiter(&state));
    }

    #[tokio::test]
    async fn finalize_publishes_terminal_event_to_live_subscriber() {
        let (_tmp, state) = setup_app_state();
        state
            .threads
            .create_thread_for_test(&make_create_params("T-pub", "T-pub"))
            .unwrap();

        // A subscriber attached before finalization must receive the
        // terminal event live, not only via event-store replay.
        let mut rx = state.event_streams.subscribe("T-pub");
        state
            .threads
            .finalize_thread(&ThreadFinalizeParams {
                thread_id: "T-pub".to_string(),
                status: "completed".to_string(),
                outcome_code: Some("success".to_string()),
                result: Some(json!("4")),
                error: None,
                metadata: None,
                artifacts: Vec::new(),
                final_cost: None,
                summary_json: None,
            })
            .unwrap();

        let event = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("terminal event delivered live before timeout")
            .expect("receiver did not lag/close");
        assert_eq!(event.event_type, "thread_completed");
        assert_eq!(event.thread_id, "T-pub");
        // Live subscribers must see the terminal result in the payload.
        assert_eq!(event.payload.get("result"), Some(&json!("4")));
        assert_eq!(event.payload.get("outcome_code"), Some(&json!("success")));
    }

    #[tokio::test]
    async fn cancel_publishes_thread_cancelled_to_live_subscriber() {
        // Cancellation finalizes through the same publish path; a subscriber
        // attached after prior events still receives `thread_cancelled`.
        let (_tmp, state) = setup_app_state();
        state
            .threads
            .create_thread_for_test(&make_create_params("T-cancel", "T-cancel"))
            .unwrap();
        state.threads.mark_running("T-cancel").unwrap();

        let mut rx = state.event_streams.subscribe("T-cancel");
        state
            .threads
            .finalize_thread(&ThreadFinalizeParams {
                thread_id: "T-cancel".to_string(),
                status: "cancelled".to_string(),
                outcome_code: Some("cancelled".to_string()),
                result: None,
                error: Some(serde_json::json!({ "reason": "cancelled_by_request" })),
                metadata: None,
                artifacts: Vec::new(),
                final_cost: None,
                summary_json: None,
            })
            .unwrap();

        let event = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("cancellation delivered live before timeout")
            .expect("receiver did not lag/close");
        assert_eq!(event.event_type, "thread_cancelled");
        assert_eq!(event.thread_id, "T-cancel");
    }

    #[tokio::test]
    async fn append_thread_events_publishes_to_live_subscriber() {
        // Seat braids append directly through the lifecycle service; a
        // `thread_events` subscriber attached before the append must receive
        // the seat event live, not only via replay.
        let (_tmp, state) = setup_app_state();
        state
            .threads
            .create_thread_for_test(&make_create_params("T-seat", "T-seat"))
            .unwrap();
        state.threads.mark_running("T-seat").unwrap();

        let mut rx = state.event_streams.subscribe("T-seat");
        let persisted = state
            .threads
            .append_thread_events(
                "T-seat",
                "T-seat",
                &[ryeos_app::state_store::NewEventRecord {
                    event_type: "seat.note".to_string(),
                    storage_class: "indexed".to_string(),
                    payload: serde_json::json!({ "text": "hello" }),
                }],
            )
            .unwrap()
            .expect("append accepted on a running thread");
        assert_eq!(persisted.len(), 1);

        let event = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("seat event delivered live before timeout")
            .expect("receiver did not lag/close");
        assert_eq!(event.event_type, "seat.note");
        assert_eq!(event.thread_id, "T-seat");
    }

    #[tokio::test]
    async fn runtime_finalize_missing_token_returns_error() {
        let (_tmp, state) = setup_app_state();
        state
            .threads
            .create_thread_for_test(&make_create_params("T-Bad", "T-Bad"))
            .unwrap();

        let resp = dispatch(
            rpc(
                "runtime.finalize_thread",
                json!({
                    "thread_id": "T-Bad",
                    "status": "completed",
                    "outcome_code": "test",
                }),
            ),
            &state,
        )
        .await;
        assert!(resp.error.is_some());
    }

    // ── events (via runtime.* token-gated) ──────────────────────────

    #[tokio::test]
    async fn runtime_finalize_revokes_callback_but_events_remain_replayable() {
        let (_tmp, state) = setup_app_state();
        create_running_test_thread(&state, "T-events-1");
        let cbt = generate_test_callback(
            &state,
            "T-events-1",
            std::path::PathBuf::from("/test"),
            std::time::Duration::from_secs(300),
            Vec::new(),
            test_provenance(&state, "/test"),
            "0".repeat(64),
        );
        let finalize_resp = dispatch(
            rpc(
                "runtime.finalize_thread",
                json!({
                    "callback_token": cbt.token,
                    "thread_id": "T-events-1",
                    "status": "completed",
                    "outcome_code": "test",
                    "result": null,
                    "error": null,
                    "cost": null,
                    "outputs": null,
                    "warnings": [],
                }),
            ),
            &state,
        )
        .await;
        assert!(
            finalize_resp.error.is_none(),
            "finalize failed: {:?}",
            finalize_resp.error
        );

        let replay_resp = dispatch(
            rpc(
                "runtime.replay_events",
                json!({
                    "callback_token": cbt.token,
                    "thread_id": "T-events-1",
                    "limit": 10,
                }),
            ),
            &state,
        )
        .await;
        assert!(
            rpc_err(&replay_resp)
                .message
                .contains("invalid callback capability"),
            "terminal finalization must revoke the runtime callback: {:?}",
            replay_resp.error,
        );

        let replay = state
            .events
            .replay(&EventReplayParams {
                chain_root_id: None,
                thread_id: Some("T-events-1".to_string()),
                after_chain_seq: None,
                limit: 10,
            })
            .unwrap();
        let events = replay.events;
        assert!(
            events.len() >= 2,
            "expected >= 2 events, got {}",
            events.len()
        );
        let types: Vec<&str> = events
            .iter()
            .map(|event| event.event_type.as_str())
            .collect();
        assert!(types.contains(&"thread_created"));
        assert!(types.contains(&"thread_completed"));
    }

    #[tokio::test]
    async fn append_event_bridges_to_event_stream_subscribers() {
        // Persistence-first contract: a successful runtime.append_event
        // RPC must (a) record the event in the event store AND (b)
        // publish the same PersistedEventRecord into the per-thread
        // hub so SSE subscribers tail in real time.
        let (_tmp, state) = setup_app_state();
        create_running_test_thread(&state, "T-stream-1");
        let cbt = generate_test_callback(
            &state,
            "T-stream-1",
            std::path::PathBuf::from("/test"),
            std::time::Duration::from_secs(300),
            Vec::new(),
            test_provenance(&state, "/test"),
            "0".repeat(64),
        );
        // Subscribe BEFORE the callback fires so the event lands in
        // the live broadcast.
        let mut rx = state.event_streams.subscribe("T-stream-1");

        let resp = dispatch(
            rpc(
                "runtime.append_event",
                json!({
                    "callback_token": cbt.token,
                    "thread_id": "T-stream-1",
                    "event": {
                        "event_type": "stream_opened",
                        "storage_class": "indexed",
                        "payload": {"turn": 1},
                    },
                }),
            ),
            &state,
        )
        .await;
        assert!(
            resp.error.is_none(),
            "append_event failed: {:?}",
            resp.error
        );

        let live = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("hub did not deliver event in time")
            .expect("hub channel closed");
        assert_eq!(live.event_type, "stream_opened");
        assert_eq!(live.thread_id, "T-stream-1");
        assert_eq!(live.payload, json!({"turn": 1}));
    }

    #[tokio::test]
    async fn ephemeral_append_event_bridges_without_replay_persistence() {
        let (_tmp, state) = setup_app_state();
        create_running_test_thread(&state, "T-ephemeral-1");
        let cbt = generate_test_callback(
            &state,
            "T-ephemeral-1",
            std::path::PathBuf::from("/test"),
            std::time::Duration::from_secs(300),
            Vec::new(),
            test_provenance(&state, "/test"),
            "0".repeat(64),
        );
        let mut rx = state.event_streams.subscribe("T-ephemeral-1");

        let resp = dispatch(
            rpc(
                "runtime.append_event",
                json!({
                    "callback_token": cbt.token,
                    "thread_id": "T-ephemeral-1",
                    "event": {
                        "event_type": "cognition_out",
                        "storage_class": "ephemeral",
                        "payload": {"turn": 1, "delta": "hello"},
                    },
                }),
            ),
            &state,
        )
        .await;
        assert!(
            resp.error.is_none(),
            "append_event failed: {:?}",
            resp.error
        );

        let live = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("hub did not deliver event in time")
            .expect("hub channel closed");
        assert_eq!(live.event_type, "cognition_out");
        assert_eq!(live.storage_class, "ephemeral");
        assert_eq!(live.chain_seq, 0);
        assert_eq!(live.payload, json!({"turn": 1, "delta": "hello"}));

        let replay = state
            .events
            .replay(&EventReplayParams {
                chain_root_id: None,
                thread_id: Some("T-ephemeral-1".to_string()),
                after_chain_seq: None,
                limit: 100,
            })
            .unwrap();
        assert!(!replay
            .events
            .iter()
            .any(|event| event.event_type == "cognition_out"));
    }

    #[tokio::test]
    async fn lifecycle_event_cannot_be_ephemeral() {
        let (_tmp, state) = setup_app_state();
        create_running_test_thread(&state, "T-ephemeral-bad");
        let cbt = generate_test_callback(
            &state,
            "T-ephemeral-bad",
            std::path::PathBuf::from("/test"),
            std::time::Duration::from_secs(300),
            Vec::new(),
            test_provenance(&state, "/test"),
            "0".repeat(64),
        );
        let resp = dispatch(
            rpc(
                "runtime.append_event",
                json!({
                    "callback_token": cbt.token,
                    "thread_id": "T-ephemeral-bad",
                    "event": {
                        "event_type": "thread_completed",
                        "storage_class": "ephemeral",
                        "payload": {},
                    },
                }),
            ),
            &state,
        )
        .await;

        let err = rpc_err(&resp);
        assert!(
            err.message.contains("cannot use ephemeral"),
            "got: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn append_events_batch_bridges_in_persisted_order() {
        // Bulk callback: each event lands in the broadcast in
        // persisted (chain_seq) order so SSE consumers reconstruct
        // the runtime's emission sequence verbatim.
        let (_tmp, state) = setup_app_state();
        create_running_test_thread(&state, "T-stream-2");
        let cbt = generate_test_callback(
            &state,
            "T-stream-2",
            std::path::PathBuf::from("/test"),
            std::time::Duration::from_secs(300),
            Vec::new(),
            test_provenance(&state, "/test"),
            "0".repeat(64),
        );
        let mut rx = state.event_streams.subscribe("T-stream-2");

        let resp = dispatch(rpc("runtime.append_events", json!({
                "callback_token": cbt.token,
                "thread_id": "T-stream-2",
                "events": [
                    {"event_type": "tool_call_start",  "payload": {"i": 1}, "storage_class": "indexed"},
                    {"event_type": "tool_call_result", "payload": {"i": 2}, "storage_class": "indexed"},
                    {"event_type": "stream_closed",    "payload": {"i": 3}, "storage_class": "indexed"},
                ],
            })),
            &state,
        ).await;
        assert!(
            resp.error.is_none(),
            "append_events failed: {:?}",
            resp.error
        );

        let mut last_seq = 0;
        for expected_type in ["tool_call_start", "tool_call_result", "stream_closed"] {
            let live = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
                .await
                .expect("hub did not deliver event in time")
                .expect("hub channel closed");
            assert_eq!(live.event_type, expected_type);
            assert!(live.chain_seq > last_seq, "chain_seq must be monotonic");
            last_seq = live.chain_seq;
        }
    }

    #[tokio::test]
    async fn runtime_events_replay_missing_token_returns_error() {
        let (_tmp, state) = setup_app_state();
        let resp = dispatch(
            rpc(
                "runtime.replay_events",
                json!({
                    "thread_id": "NONEXISTENT",
                }),
            ),
            &state,
        )
        .await;
        assert!(resp.error.is_some());
    }

    #[tokio::test]
    async fn runtime_bundle_events_use_callback_bundle_identity_and_caps() {
        let (_tmp, state) = setup_app_state();
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir(project.path().join("models")).unwrap();
        std::fs::write(
            project.path().join("models/checkpoint.bin"),
            b"learner-checkpoint",
        )
        .unwrap();
        create_running_test_thread(&state, "T-bundle-1");
        let cbt = generate_test_callback_with_context(
            &state,
            "T-bundle-1",
            project.path().to_path_buf(),
            std::time::Duration::from_secs(300),
            vec![
                "ryeos.append.bundle-events.example-bundle/example_event".to_string(),
                "ryeos.scan.bundle-events.example-bundle/example_event".to_string(),
            ],
            test_provenance(&state, project.path().to_str().unwrap()),
            Some("example-bundle".to_string()),
            Some("tool:example-bundle/send".to_string()),
            "0".repeat(64),
            serde_json::Value::Null,
            0,
        );

        let append = dispatch(
            rpc(
                "runtime.bundle_events_append",
                json!({
                    "callback_token": cbt.token,
                    "thread_id": "T-bundle-1",
                    "event_kind": "example_event",
                    "chain_id": "example_1",
                    "event_type": "example_planned",
                    "schema_version": 1,
                    "payload": {"example_id": "example_1"},
                    "idempotency_key": "record:example_1",
                    "attachments": [{
                        "name": "checkpoint",
                        "source_path": "models/checkpoint.bin",
                        "media_type": "application/octet-stream"
                    }]
                }),
            ),
            &state,
        )
        .await;
        assert!(append.error.is_none(), "append failed: {:?}", append.error);
        let event_hash = rpc_ok(&append)["event_hash"].as_str().unwrap();
        assert_eq!(rpc_ok(&append)["event"]["bundle_id"], "example-bundle");
        assert_eq!(
            rpc_ok(&append)["event"]["attribution"]["tool"],
            "tool:example-bundle/send"
        );
        assert_eq!(
            rpc_ok(&append)["event"]["attachments"][0]["name"],
            "checkpoint"
        );

        let materialize = dispatch(
            rpc(
                "runtime.bundle_events_materialize_attachment",
                json!({
                    "callback_token": cbt.token,
                    "thread_id": "T-bundle-1",
                    "event_kind": "example_event",
                    "event_hash": event_hash,
                    "attachment_name": "checkpoint",
                    "destination_path": "models/restored/checkpoint.bin"
                }),
            ),
            &state,
        )
        .await;
        assert!(
            materialize.error.is_none(),
            "materialize failed: {:?}",
            materialize.error
        );
        assert_eq!(
            std::fs::read(project.path().join("models/restored/checkpoint.bin")).unwrap(),
            b"learner-checkpoint"
        );

        let scan = dispatch(
            rpc(
                "runtime.bundle_events_scan",
                json!({
                    "callback_token": cbt.token,
                    "thread_id": "T-bundle-1",
                    "event_kind": "example_event",
                }),
            ),
            &state,
        )
        .await;
        assert!(scan.error.is_none(), "scan failed: {:?}", scan.error);
        assert_eq!(rpc_ok(&scan)["events"].as_array().unwrap().len(), 1);
        assert_eq!(rpc_ok(&scan)["events"][0]["event_hash"], event_hash);
    }

    #[tokio::test]
    async fn runtime_bundle_events_reject_bundle_id_input_and_missing_cap() {
        let (_tmp, state) = setup_app_state();
        create_running_test_thread(&state, "T-bundle-deny");
        create_running_test_thread(&state, "T-bundle-deny-2");
        let cbt = generate_test_callback_with_context(
            &state,
            "T-bundle-deny",
            std::path::PathBuf::from("/test"),
            std::time::Duration::from_secs(300),
            vec!["ryeos.append.bundle-events.example-bundle/example_event".to_string()],
            test_provenance(&state, "/test"),
            Some("example-bundle".to_string()),
            Some("tool:example-bundle/send".to_string()),
            "0".repeat(64),
            serde_json::Value::Null,
            0,
        );

        let caller_bundle_id = dispatch(
            rpc(
                "runtime.bundle_events_append",
                json!({
                    "callback_token": cbt.token,
                    "thread_id": "T-bundle-deny",
                    "bundle_id": "other-bundle",
                    "event_kind": "example_event",
                    "chain_id": "example_1",
                    "event_type": "example_planned",
                    "payload": {}
                }),
            ),
            &state,
        )
        .await;
        assert!(
            rpc_err(&caller_bundle_id)
                .message
                .contains("invalid bundle_events.append params"),
            "got: {}",
            rpc_err(&caller_bundle_id).message
        );

        let cbt = generate_test_callback_with_context(
            &state,
            "T-bundle-deny-2",
            std::path::PathBuf::from("/test"),
            std::time::Duration::from_secs(300),
            Vec::new(),
            test_provenance(&state, "/test"),
            Some("example-bundle".to_string()),
            Some("tool:example-bundle/send".to_string()),
            "0".repeat(64),
            serde_json::Value::Null,
            0,
        );
        let missing_cap = dispatch(
            rpc(
                "runtime.bundle_events_append",
                json!({
                    "callback_token": cbt.token,
                    "thread_id": "T-bundle-deny-2",
                    "event_kind": "example_event",
                    "chain_id": "example_1",
                    "event_type": "example_planned",
                    "payload": {}
                }),
            ),
            &state,
        )
        .await;
        assert!(
            rpc_err(&missing_cap)
                .message
                .contains("missing required capability"),
            "got: {}",
            rpc_err(&missing_cap).message
        );
    }

    #[tokio::test]
    async fn runtime_vault_put_get_list_delete_use_callback_bundle_identity_and_caps() {
        let (_tmp, state) = setup_app_state();
        create_running_test_thread(&state, "T-vault-1");
        let cbt = generate_test_callback_with_context(
            &state,
            "T-vault-1",
            std::path::PathBuf::from("/test"),
            std::time::Duration::from_secs(300),
            vec![
                "ryeos.put.vault.example-bundle/oauth".to_string(),
                "ryeos.get.vault.example-bundle/oauth".to_string(),
                "ryeos.list.vault.example-bundle/oauth".to_string(),
                "ryeos.delete.vault.example-bundle/oauth".to_string(),
            ],
            test_provenance(&state, "/test"),
            Some("example-bundle".to_string()),
            Some("tool:example-bundle/oauth/connect".to_string()),
            "0".repeat(64),
            serde_json::Value::Null,
            0,
        );

        let put = dispatch(
            rpc(
                "runtime.vault_put",
                json!({
                    "callback_token": cbt.token,
                    "thread_id": "T-vault-1",
                    "namespace": "oauth",
                    "key": "google_account_123",
                    "value": "refresh-token"
                }),
            ),
            &state,
        )
        .await;
        assert!(put.error.is_none(), "put failed: {:?}", put.error);
        let vault_ref = rpc_ok(&put)["ref"].as_str().unwrap().to_string();
        assert_eq!(
            vault_ref,
            format!(
                "{}{}",
                "vault://", "bundle/example-bundle/oauth/google_account_123"
            )
        );
        assert!(!rpc_ok(&put).as_object().unwrap().contains_key("value"));

        let get = dispatch(
            rpc(
                "runtime.vault_get",
                json!({
                    "callback_token": cbt.token,
                    "thread_id": "T-vault-1",
                    "ref": vault_ref
                }),
            ),
            &state,
        )
        .await;
        assert!(get.error.is_none(), "get failed: {:?}", get.error);
        assert_eq!(rpc_ok(&get)["value"], "refresh-token");

        let list = dispatch(
            rpc(
                "runtime.vault_list",
                json!({
                    "callback_token": cbt.token,
                    "thread_id": "T-vault-1",
                    "namespace": "oauth"
                }),
            ),
            &state,
        )
        .await;
        assert!(list.error.is_none(), "list failed: {:?}", list.error);
        assert_eq!(rpc_ok(&list)["keys"], json!(["google_account_123"]));

        let delete = dispatch(
            rpc(
                "runtime.vault_delete",
                json!({
                    "callback_token": cbt.token,
                    "thread_id": "T-vault-1",
                    "ref": rpc_ok(&get)["ref"].as_str().unwrap()
                }),
            ),
            &state,
        )
        .await;
        assert!(delete.error.is_none(), "delete failed: {:?}", delete.error);
        assert_eq!(rpc_ok(&delete)["deleted"], true);
    }

    #[tokio::test]
    async fn runtime_vault_rejects_bundle_id_input_missing_cap_and_other_bundle_ref() {
        let (_tmp, state) = setup_app_state();
        create_running_test_thread(&state, "T-vault-deny");
        create_running_test_thread(&state, "T-vault-deny-2");
        create_running_test_thread(&state, "T-vault-deny-3");
        let cbt = generate_test_callback_with_context(
            &state,
            "T-vault-deny",
            std::path::PathBuf::from("/test"),
            std::time::Duration::from_secs(300),
            vec!["ryeos.put.vault.example-bundle/oauth".to_string()],
            test_provenance(&state, "/test"),
            Some("example-bundle".to_string()),
            Some("tool:example-bundle/oauth/connect".to_string()),
            "0".repeat(64),
            serde_json::Value::Null,
            0,
        );

        let caller_bundle_id = dispatch(
            rpc(
                "runtime.vault_put",
                json!({
                    "callback_token": cbt.token,
                    "thread_id": "T-vault-deny",
                    "bundle_id": "other-bundle",
                    "namespace": "oauth",
                    "key": "google_account_123",
                    "value": "refresh-token"
                }),
            ),
            &state,
        )
        .await;
        assert!(
            rpc_err(&caller_bundle_id)
                .message
                .contains("invalid vault.put params"),
            "got: {}",
            rpc_err(&caller_bundle_id).message
        );

        let cbt = generate_test_callback_with_context(
            &state,
            "T-vault-deny-2",
            std::path::PathBuf::from("/test"),
            std::time::Duration::from_secs(300),
            Vec::new(),
            test_provenance(&state, "/test"),
            Some("example-bundle".to_string()),
            Some("tool:example-bundle/oauth/connect".to_string()),
            "0".repeat(64),
            serde_json::Value::Null,
            0,
        );
        let missing_cap = dispatch(
            rpc(
                "runtime.vault_put",
                json!({
                    "callback_token": cbt.token,
                    "thread_id": "T-vault-deny-2",
                    "namespace": "oauth",
                    "key": "google_account_123",
                    "value": "refresh-token"
                }),
            ),
            &state,
        )
        .await;
        assert!(
            rpc_err(&missing_cap)
                .message
                .contains("missing required capability"),
            "got: {}",
            rpc_err(&missing_cap).message
        );

        let cbt = generate_test_callback_with_context(
            &state,
            "T-vault-deny-3",
            std::path::PathBuf::from("/test"),
            std::time::Duration::from_secs(300),
            vec!["ryeos.get.vault.example-bundle/oauth".to_string()],
            test_provenance(&state, "/test"),
            Some("example-bundle".to_string()),
            Some("tool:example-bundle/oauth/connect".to_string()),
            "0".repeat(64),
            serde_json::Value::Null,
            0,
        );
        let other_bundle = dispatch(
            rpc(
                "runtime.vault_get",
                json!({
                    "callback_token": cbt.token,
                    "thread_id": "T-vault-deny-3",
                    "ref": format!("{}{}", "vault://", "bundle/other-bundle/oauth/google_account_123")
                }),
            ),
            &state,
        )
        .await;
        assert!(
            rpc_err(&other_bundle).message.contains("does not match"),
            "got: {}",
            rpc_err(&other_bundle).message
        );
    }

    // ── commands (via runtime.* token-gated) ────────────────────────

    #[tokio::test]
    async fn runtime_commands_submit_and_claim() {
        let (_tmp, state) = setup_app_state();
        state
            .threads
            .create_thread_for_test(&make_create_params("T-cmd-1", "T-cmd-1"))
            .unwrap();

        let cbt = generate_test_callback(
            &state,
            "T-cmd-1",
            std::path::PathBuf::from("/test"),
            std::time::Duration::from_secs(300),
            Vec::new(),
            test_provenance(&state, "/test"),
            "0".repeat(64),
        );

        // Mark running first — cancel is only allowed on running threads
        let _ = dispatch(
            rpc(
                "runtime.mark_running",
                json!({
                    "callback_token": cbt.token,
                    "thread_id": "T-cmd-1",
                }),
            ),
            &state,
        )
        .await;

        let submit_resp = dispatch(
            rpc(
                "runtime.submit_command",
                json!({
                    "callback_token": cbt.token,
                    "thread_id": "T-cmd-1",
                    "command_type": "cancel",
                }),
            ),
            &state,
        )
        .await;
        assert!(
            submit_resp.error.is_none(),
            "submit failed: {:?}",
            submit_resp.error
        );
        let submitted = rpc_ok(&submit_resp);
        assert_eq!(submitted["command_type"], "cancel");

        let claim_resp = dispatch(
            rpc(
                "runtime.claim_commands",
                json!({
                    "callback_token": cbt.token,
                    "thread_id": "T-cmd-1",
                }),
            ),
            &state,
        )
        .await;
        assert!(
            claim_resp.error.is_none(),
            "claim failed: {:?}",
            claim_resp.error
        );
        let claimed = rpc_ok(&claim_resp);
        let commands = claimed["commands"].as_array().unwrap();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0]["command_type"], "cancel");
    }

    // ── per-request MCP auth (wave 5.5 audit closure) ──────────────
    //
    // Three tests covering the trust boundary at runtime.dispatch_action:
    //   1. missing thread_auth_token → bail
    //   2. wrong / unknown thread_auth_token → bail
    //   3. correct thread_auth_token → server-side principal authoritative
    //      (the request body cannot smuggle a different acting principal,
    //      because the param struct is `deny_unknown_fields` and there is
    //      no principal field; the only path to a principal is through
    //      `state.thread_auth.validate(...)`)

    #[tokio::test]
    async fn dispatch_action_without_thread_auth_token_is_rejected() {
        let (_tmp, state) = setup_app_state();
        state
            .threads
            .create_thread_for_test(&make_create_params("T-tat-missing", "T-tat-missing"))
            .unwrap();
        let cbt = generate_test_callback(
            &state,
            "T-tat-missing",
            std::path::PathBuf::from("/p"),
            std::time::Duration::from_secs(300),
            vec!["*".to_string()],
            test_provenance(&state, "/p"),
            "0".repeat(64),
        );

        // Note: `thread_auth_token` field intentionally absent.
        let resp = dispatch(
            rpc(
                "runtime.dispatch_action",
                json!({
                    "callback_token": cbt.token,
                    "thread_id": "T-tat-missing",
                    "project_path": "/p",
                    "action": {
                        "item_id": "directive:ryeos/agent/core/base",
                        "ref_bindings": {},
                        "thread": "inline",
                    },
                }),
            ),
            &state,
        )
        .await;
        let err = rpc_err(&resp);
        assert!(
            err.message.contains("missing thread_auth_token")
                || err.message.contains("thread_auth_token"),
            "expected missing thread_auth_token error, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn dispatch_action_with_wrong_thread_auth_token_is_rejected() {
        let (_tmp, state) = setup_app_state();
        state
            .threads
            .create_thread_for_test(&make_create_params("T-tat-wrong", "T-tat-wrong"))
            .unwrap();
        let cbt = generate_test_callback(
            &state,
            "T-tat-wrong",
            std::path::PathBuf::from("/p"),
            std::time::Duration::from_secs(300),
            vec!["*".to_string()],
            test_provenance(&state, "/p"),
            "0".repeat(64),
        );

        // Use a syntactically plausible but unminted tat — must not be
        // accepted by ThreadAuthStore.validate.
        let resp = dispatch(rpc("runtime.dispatch_action", json!({
                "callback_token": cbt.token,
                "thread_id": "T-tat-wrong",
                "project_path": "/p",
                "thread_auth_token": "tat-deadbeef0000000000000000000000000000000000000000000000000000",
                "action": {
                    "item_id": "directive:ryeos/agent/core/base",
                    "ref_bindings": {},
                    "thread": "inline",
                },
            })),
            &state,
        ).await;
        let err = rpc_err(&resp);
        assert!(
            err.message.contains("invalid thread auth token")
                || err.message.contains("thread auth")
                || err.message.contains("thread_auth"),
            "expected invalid-thread-auth error, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn dispatch_action_with_correct_token_uses_server_side_principal() {
        let (_tmp, state) = setup_app_state();
        create_running_test_thread(&state, "T-tat-ok");
        let cbt = generate_test_callback(
            &state,
            "T-tat-ok",
            std::path::PathBuf::from("/p"),
            std::time::Duration::from_secs(300),
            vec!["*".to_string()],
            test_provenance(&state, "/p"),
            "0".repeat(64),
        );
        // Mint a tat for a SPECIFIC principal — this is the value the
        // daemon must use, not anything caller-controllable.
        let tat = state.thread_auth.mint(
            "T-tat-ok",
            "fp:server-authoritative-principal".to_string(),
            vec!["execute".to_string()],
            std::time::Duration::from_secs(300),
        );

        // The DispatchActionParams struct is `deny_unknown_fields` and
        // does NOT include a principal field, so the only acting principal
        // available downstream is the one from `thread_auth.validate(...)`.
        // Even attempting to pass an unknown field like `acting_principal`
        // must be rejected at parse time — proving spoofed body principals
        // never reach the inner handler.
        let resp = dispatch(
            rpc(
                "runtime.dispatch_action",
                json!({
                    "callback_token": cbt.token,
                    "thread_id": "T-tat-ok",
                    "project_path": "/p",
                    "thread_auth_token": tat.token.clone(),
                    "acting_principal": "fp:attacker-spoofed-principal",
                    "action": {
                        "item_id": "directive:ryeos/agent/core/base",
                        "ref_bindings": {},
                        "thread": "inline",
                    },
                }),
            ),
            &state,
        )
        .await;
        let err = rpc_err(&resp);
        // Must fail at deserialization — `acting_principal` is unknown.
        // This is the structural proof that body cannot smuggle principal.
        assert!(
            err.message.to_lowercase().contains("unknown field")
                || err.message.contains("acting_principal")
                || err
                    .message
                    .contains("invalid runtime.dispatch_action params"),
            "expected unknown-field rejection of body-side principal, got: {err:?}"
        );

        // Sanity: the same call without the spoof field should make it past
        // auth (it will still fail later because the directive isn't loaded
        // in this minimal test state, but the failure must NOT be auth).
        let resp_clean = dispatch(
            rpc(
                "runtime.dispatch_action",
                json!({
                    "callback_token": cbt.token,
                    "thread_id": "T-tat-ok",
                    "project_path": "/p",
                    "thread_auth_token": tat.token,
                    "action": {
                        "item_id": "directive:ryeos/agent/core/base",
                        "ref_bindings": {},
                        "thread": "inline",
                    },
                }),
            ),
            &state,
        )
        .await;
        if let Some(err) = resp_clean.error.as_ref() {
            assert!(
                !err.message.contains("missing thread_auth_token")
                    && !err.message.contains("invalid thread auth token"),
                "auth must succeed; downstream errors are fine: {err:?}"
            );
        }
    }

    #[tokio::test]
    async fn runtime_callback_with_empty_caps_is_denied_at_uds_boundary() {
        let (_tmp, state) = setup_app_state();
        create_running_test_thread(&state, "T-caps-empty");
        let cbt = generate_test_callback(
            &state,
            "T-caps-empty",
            std::path::PathBuf::from("/p"),
            std::time::Duration::from_secs(300),
            Vec::new(),
            test_provenance(&state, "/p"),
            "0".repeat(64),
        );
        let tat = state.thread_auth.mint(
            "T-caps-empty",
            "fp:server-authoritative-principal".to_string(),
            vec!["execute".to_string()],
            std::time::Duration::from_secs(300),
        );

        let resp = dispatch(
            rpc(
                "runtime.dispatch_action",
                json!({
                    "callback_token": cbt.token,
                    "thread_id": "T-caps-empty",
                    "project_path": "/p",
                    "thread_auth_token": tat.token,
                    "action": {
                        "item_id": "directive:ryeos/agent/core/base",
                        "ref_bindings": {},
                        "thread": "inline",
                    },
                }),
            ),
            &state,
        )
        .await;
        let err = rpc_err(&resp);
        assert!(
            err.code == "missing_cap"
                && err.message.contains(
                    "missing required capability: ryeos.execute.directive.ryeos/agent/core/base"
                ),
            "expected UDS boundary missing-cap denial from empty callback caps, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn runtime_callback_with_wildcard_caps_is_allowed_past_uds_boundary() {
        let (_tmp, state) = setup_app_state();
        create_running_test_thread(&state, "T-caps-wild");
        let cbt = generate_test_callback(
            &state,
            "T-caps-wild",
            std::path::PathBuf::from("/p"),
            std::time::Duration::from_secs(300),
            vec!["ryeos.*".to_string()],
            test_provenance(&state, "/p"),
            "0".repeat(64),
        );
        let tat = state.thread_auth.mint(
            "T-caps-wild",
            "fp:server-authoritative-principal".to_string(),
            vec!["execute".to_string()],
            std::time::Duration::from_secs(300),
        );

        let resp = dispatch(
            rpc(
                "runtime.dispatch_action",
                json!({
                    "callback_token": cbt.token,
                    "thread_id": "T-caps-wild",
                    "project_path": "/p",
                    "thread_auth_token": tat.token,
                    "action": {
                        "item_id": "directive:ryeos/agent/core/base",
                        "ref_bindings": {},
                        "thread": "inline",
                    },
                }),
            ),
            &state,
        )
        .await;

        if let Some(err) = resp.error.as_ref() {
            assert!(
                !err.message.contains("deny-all")
                    && !err.message.contains("required cap")
                    && !err.message.contains("effective_caps"),
                "wildcard caps must pass UDS cap enforcement; downstream errors are fine: {err:?}"
            );
        }
    }

    // ── facets (via runtime.* token-gated) ─────────────────────────

    #[tokio::test]
    async fn runtime_get_facets_returns_empty_for_new_thread() {
        let (_tmp, state) = setup_app_state();
        state
            .threads
            .create_thread_for_test(&make_create_params("T-facets-1", "T-facets-1"))
            .unwrap();

        let cbt = generate_test_callback(
            &state,
            "T-facets-1",
            std::path::PathBuf::from("/test"),
            std::time::Duration::from_secs(300),
            Vec::new(),
            test_provenance(&state, "/test"),
            "0".repeat(64),
        );
        let resp = dispatch(
            rpc(
                "runtime.get_facets",
                json!({
                    "callback_token": cbt.token,
                    "thread_id": "T-facets-1",
                }),
            ),
            &state,
        )
        .await;
        // Empty facets is OK — new thread has no facets
        if resp.error.is_none() {
            let result = rpc_ok(&resp);
            assert!(result.is_object());
        }
    }

    // ── chain-scoped callback authorization ─────────────────────────
    //
    // A successor's callback token may READ any thread in its own chain (to
    // rehydrate predecessors) but may only WRITE its exact thread, and may not
    // read into another chain.

    /// Predecessor `T-pred` + successor `T-succ` in chain `T-pred`, plus a token
    /// minted for the successor.
    fn chain_with_successor(state: &AppState) -> ryeos_app::callback_token::CallbackCapability {
        state
            .threads
            .create_thread_for_test(&make_create_params("T-pred", "T-pred"))
            .unwrap();
        state.threads.mark_running("T-pred").unwrap();
        // Successor shares the predecessor's chain root.
        state
            .threads
            .create_thread_for_test(&make_create_params("T-succ", "T-pred"))
            .unwrap();
        generate_test_callback(
            state,
            "T-succ",
            std::path::PathBuf::from("/test"),
            std::time::Duration::from_secs(300),
            Vec::new(),
            test_provenance(state, "/test"),
            "0".repeat(64),
        )
    }

    #[tokio::test]
    async fn successor_token_can_read_predecessor_in_chain() {
        let (_tmp, state) = setup_app_state();
        let succ = chain_with_successor(&state);

        // get_thread on the predecessor — a cross-thread read within the chain.
        let resp = dispatch(
            rpc(
                "runtime.get_thread",
                json!({"callback_token": succ.token, "thread_id": "T-pred"}),
            ),
            &state,
        )
        .await;
        assert!(
            resp.error.is_none(),
            "chain read of predecessor must pass: {resp:?}"
        );

        // replay by predecessor thread_id, and by chain_root_id — both reads.
        for params in [
            json!({"callback_token": succ.token, "thread_id": "T-pred"}),
            json!({"callback_token": succ.token, "chain_root_id": "T-pred"}),
        ] {
            let resp = dispatch(rpc("runtime.replay_events", params), &state).await;
            assert!(resp.error.is_none(), "chain replay must pass: {resp:?}");
        }
    }

    #[tokio::test]
    async fn successor_token_cannot_write_predecessor() {
        let (_tmp, state) = setup_app_state();
        let succ = chain_with_successor(&state);

        // Writes/lifecycle stay exact-thread: the successor token must not
        // append to, finalize, or mark-running the predecessor.
        for method in [
            "runtime.append_event",
            "runtime.finalize_thread",
            "runtime.mark_running",
            "runtime.request_continuation",
            "runtime.publish_artifact",
        ] {
            let resp = dispatch(
                rpc(
                    method,
                    json!({
                        "callback_token": succ.token,
                        "thread_id": "T-pred",
                        "status": "completed",
                        "event": {"event_type": "cognition_in", "payload": {}, "storage_class": "indexed"},
                    }),
                ),
                &state,
            )
            .await;
            assert!(
                resp.error.is_some(),
                "{method} against predecessor must be rejected for a successor token"
            );
        }
    }

    #[tokio::test]
    async fn token_cannot_read_another_chain() {
        let (_tmp, state) = setup_app_state();
        let succ = chain_with_successor(&state);

        // A thread in a DIFFERENT chain.
        state
            .threads
            .create_thread_for_test(&make_create_params("T-other", "T-other"))
            .unwrap();

        let resp = dispatch(
            rpc(
                "runtime.get_thread",
                json!({"callback_token": succ.token, "thread_id": "T-other"}),
            ),
            &state,
        )
        .await;
        assert!(
            resp.error.is_some(),
            "reading a thread outside the token's chain must be rejected"
        );
    }

    // ── threads/input handler response contract ─────────────────────

    #[tokio::test]
    async fn threads_input_refuses_follow_up_to_non_continuable_kind() {
        // A follow-up to a settled source whose KIND cannot chain-fold (here
        // `system_task`) is an EXPECTED, structured refusal at the API boundary
        // — delivery=refused, thread_id=null, and daemon-authored
        // execution.supports_continuation=false — NOT a 500 from the deeper
        // lifecycle guard. The client gates on this fact; the server enforces it
        // as defense-in-depth for a stale/third-party client.
        use ryeos_app::handler_context::HandlerContext;

        let (_tmp, state) = setup_app_state();
        let state = std::sync::Arc::new(state);

        // Completed `system_task` predecessor owned by `user:test`.
        state
            .threads
            .create_thread_for_test(&make_create_params("T-pred", "T-pred"))
            .unwrap();
        state.threads.mark_running("T-pred").unwrap();
        state
            .threads
            .finalize_thread(&ThreadFinalizeParams {
                thread_id: "T-pred".to_string(),
                status: "completed".to_string(),
                outcome_code: Some("success".to_string()),
                result: Some(json!({"a": 1})),
                error: None,
                metadata: None,
                artifacts: Vec::new(),
                final_cost: None,
                summary_json: None,
            })
            .unwrap();

        let ctx = HandlerContext::new("user:test".to_string(), Vec::new(), true);
        let req: ryeos_api::handlers::threads_input::Request = serde_json::from_value(json!({
            "input": "again",
            "target": {"kind": "thread", "thread_id": "T-pred"},
        }))
        .unwrap();
        let resp = ryeos_api::handlers::threads_input::handle(req, ctx, state.clone())
            .await
            .expect("a non-continuable kind is a refusal, not an error");

        assert_eq!(resp["delivery"], "refused");
        assert!(resp["thread_id"].is_null());
        assert_eq!(resp["execution"]["supports_continuation"], false);
        assert!(
            resp["notice"]
                .as_str()
                .unwrap_or_default()
                .contains("system_task"),
            "notice must name the non-continuable kind: {resp:?}"
        );
    }

    // ── client-facing thread projections carry daemon-authored execution facts ──
    //
    // These pin the WIRING of the continuation-authority surfacing: every
    // client-facing thread projection (`threads.get` / `list` / `children` /
    // `chain`) flattens the thread fields and nests an
    // `execution.supports_continuation` the client gates on, and the list rows
    // carry the chain-head edges (`upstream_thread_id` / `successor_thread_id`).
    //
    // These rows use the harness's internal non-continuable `system_task`
    // profile, so the value here is always `false`.
    // That is the right thing to assert at this layer: `supports_continuation`'s
    // true/false value is the kind profile's concern (covered in
    // `kind_profiles`), while the risk THIS change introduces is whether every
    // projection is decorated and shaped correctly. A directive→true /
    // graph→false contrast belongs to a full-daemon harness that loads real
    // signed kind schemas.

    /// Build a completed predecessor `T-pred` with a braided continuation
    /// successor `T-succ` (a real `thread_continued` handoff), so projections
    /// expose `successor_thread_id` / `upstream_thread_id` and the chain view
    /// returns both. Mirrors an operator follow-up onto a settled turn.
    fn pred_with_continuation_successor(state: &AppState) {
        use ryeos_app::state_store::NewThreadRecord;
        state
            .threads
            .create_thread_for_test(&make_create_params("T-pred", "T-pred"))
            .unwrap();
        state.threads.mark_running("T-pred").unwrap();
        state
            .threads
            .finalize_thread(&ThreadFinalizeParams {
                thread_id: "T-pred".to_string(),
                status: "completed".to_string(),
                outcome_code: Some("success".to_string()),
                result: None,
                error: None,
                metadata: None,
                artifacts: Vec::new(),
                final_cost: None,
                summary_json: None,
            })
            .unwrap();
        state
            .state_store
            .create_continuation_for_test(
                &NewThreadRecord {
                    thread_id: "T-succ".to_string(),
                    chain_root_id: "T-pred".to_string(),
                    kind: "system_task".to_string(),
                    item_ref: "directive:test/directive".to_string(),
                    executor_ref: "test/executor".to_string(),
                    launch_mode: "wait".to_string(),
                    current_site_id: "site:test".to_string(),
                    origin_site_id: "site:test".to_string(),
                    upstream_thread_id: Some("T-pred".to_string()),
                    requested_by: Some("user:test".to_string()),
                    project_root: None,
                    project_authority: ryeos_state::objects::ExecutionProjectAuthority::PROJECTLESS,
                    base_project_snapshot_hash: None,
                    usage_subject: None,
                    usage_subject_asserted_by: None,
                    captured_history_policy: None,
                },
                "T-pred",
                "T-pred",
                Some("follow-up"),
            )
            .unwrap();
    }

    #[tokio::test]
    async fn thread_get_view_carries_execution_facts() {
        let (_tmp, state) = setup_app_state();
        state
            .threads
            .create_thread_for_test(&make_create_params("T-1", "T-1"))
            .unwrap();

        let view = state
            .threads
            .get_thread_view("T-1")
            .unwrap()
            .expect("thread exists");
        let v = serde_json::to_value(&view).unwrap();

        // Flatten: the thread fields stay at the top level.
        assert_eq!(v["thread_id"], "T-1");
        assert_eq!(v["kind"], "system_task");
        // Daemon-authored execution facts, nested under `execution`.
        assert_eq!(v["execution"]["supports_continuation"], false);
    }

    #[tokio::test]
    async fn thread_list_carries_execution_and_head_fields() {
        let (_tmp, state) = setup_app_state();
        pred_with_continuation_successor(&state);

        let listing = state.threads.list_threads_filtered(100, None).unwrap();
        let rows = listing["threads"].as_array().expect("threads array");
        let row = |id: &str| {
            rows.iter()
                .find(|r| r["thread_id"] == id)
                .unwrap_or_else(|| panic!("row {id} missing from list: {listing:#?}"))
        };

        let pred = row("T-pred");
        let succ = row("T-succ");

        // Every row decorated.
        assert_eq!(pred["execution"]["supports_continuation"], false);
        assert_eq!(succ["execution"]["supports_continuation"], false);

        // Chain-head edges: the predecessor points at its successor; the
        // successor (the head — no successor of its own) points back upstream.
        assert_eq!(pred["successor_thread_id"], "T-succ");
        assert!(pred["upstream_thread_id"].is_null());
        assert_eq!(succ["upstream_thread_id"], "T-pred");
        assert!(succ["successor_thread_id"].is_null());
    }

    #[tokio::test]
    async fn thread_chain_decorates_every_row() {
        let (_tmp, state) = setup_app_state();
        pred_with_continuation_successor(&state);

        let chain = state
            .threads
            .get_chain("T-pred")
            .unwrap()
            .expect("chain exists");
        let v = serde_json::to_value(&chain).unwrap();
        let threads = v["threads"].as_array().expect("chain threads");
        assert!(
            threads.len() >= 2,
            "chain holds predecessor + successor: {v:#?}"
        );
        for t in threads {
            assert!(
                t["execution"]["supports_continuation"].is_boolean(),
                "every chain thread is decorated: {t:#?}"
            );
        }
    }

    #[tokio::test]
    async fn thread_children_decorate_every_row() {
        let (_tmp, state) = setup_app_state();
        state
            .threads
            .create_thread_for_test(&make_create_params("T-parent", "T-parent"))
            .unwrap();
        // A child is a thread whose `upstream_thread_id` points at the parent
        // (edges are derived from that link).
        let mut child = make_create_params("T-child", "T-parent");
        child.upstream_thread_id = Some("T-parent".to_string());
        state.threads.create_thread_for_test(&child).unwrap();

        let children = state.threads.list_children("T-parent").unwrap();
        let v = serde_json::to_value(&children).unwrap();
        let rows = v.as_array().expect("children array");
        assert!(
            !rows.is_empty(),
            "parent has a child via upstream edge: {v:#?}"
        );
        for c in rows {
            assert!(
                c["execution"]["supports_continuation"].is_boolean(),
                "every child is decorated: {c:#?}"
            );
            // Flatten: thread fields stay at the top level.
            assert!(
                c["thread_id"].is_string(),
                "flattened thread fields top-level"
            );
        }
    }
}
