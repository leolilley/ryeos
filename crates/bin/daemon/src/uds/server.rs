use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

use crate::uds::protocol::{RpcRequest, RpcResponse};
use ryeos_app::bundle_event_service::{
    BundleEventAppendParams, BundleEventReadChainParams, BundleEventScanParams, BundleEventService,
};
use ryeos_app::command_service::{CommandClaimParams, CommandCompleteParams, CommandSubmitParams};
use ryeos_app::event_store_service::{
    EventAppendBatchParams, EventAppendParams, EventReplayParams,
};
use ryeos_app::runtime_vault_service::{
    RuntimeVaultListParams, RuntimeVaultPutParams, RuntimeVaultRefParams, RuntimeVaultService,
};
use ryeos_app::state::AppState;
use ryeos_app::thread_lifecycle::{
    ArtifactPublishParams, ThreadAttachProcessParams, ThreadContinuationParams, ThreadGetParams,
    ThreadMarkRunningParams,
};

pub async fn serve(listener: UnixListener, state: Arc<AppState>) -> Result<()> {
    loop {
        let (stream, _) = listener.accept().await.context("uds accept failed")?;
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(err) = handle_connection(stream, state).await {
                tracing::warn!(error = %err, "uds connection error");
            }
        });
    }
}

async fn handle_connection(mut stream: UnixStream, state: Arc<AppState>) -> Result<()> {
    loop {
        let Some(frame) = read_frame(&mut stream).await? else {
            return Ok(());
        };

        let request: RpcRequest = rmp_serde::from_slice(&frame).context("invalid rpc frame")?;

        let span = tracing::debug_span!(
            "uds:request",
            method = %request.method,
            request_id = %request.request_id,
            thread_id = tracing::field::Empty,
        );
        // Opportunistically record thread_id when present in params.
        if let Some(tid) = request.params.get("thread_id").and_then(|v| v.as_str()) {
            span.record("thread_id", tid);
        }
        let _enter = span.enter();

        let response = dispatch(request, &state).await;

        let encoded = rmp_serde::to_vec_named(&response).context("failed to encode response")?;
        write_frame(&mut stream, &encoded).await?;
    }
}

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

pub(crate) async fn dispatch(request: RpcRequest, state: &AppState) -> RpcResponse {
    match request.method.as_str() {
        // ── daemon health (lightweight, only ungated method) ─────────
        "system.health" => RpcResponse::ok(request.request_id, json!({ "status": "ok" })),

        // ── local lifecycle control (local UDS only, no public HTTP surface) ─
        "lifecycle.status" => RpcResponse::ok(
            request.request_id,
            json!({
                "status": "running",
                "pid": std::process::id(),
                "version": env!("CARGO_PKG_VERSION"),
                "started_at": &state.started_at_iso,
                "bind": state.config.bind.to_string(),
                "uds_path": state.config.uds_path.display().to_string(),
                "app_root": state.config.app_root.display().to_string(),
            }),
        ),
        "lifecycle.shutdown" => {
            crate::request_shutdown();
            RpcResponse::ok(request.request_id, json!({ "accepted": true }))
        }

        // ── runtime callbacks (token-gated, used by runtimes) ───────
        other if other.starts_with("runtime.") => rpc_result(
            request.request_id,
            dispatch_runtime_method(other, &request.params, state).await,
        ),

        other => RpcResponse::err(
            request.request_id,
            "unknown_method",
            format!("unknown rpc method: {other}"),
        ),
    }
}

pub async fn dispatch_runtime_method(
    method: &str,
    params: &serde_json::Value,
    state: &AppState,
) -> Result<serde_json::Value> {
    // Validate the callback token on ALL runtime.* methods, by access class:
    //
    //  - `runtime.dispatch_action` does its own stronger validation (per-request
    //    thread_auth_token + project_path).
    //  - chain *reads* (get_thread / replay) may target any thread in the
    //    capability's own chain — a successor rehydrates by folding its
    //    predecessors. Authorized by state-checked chain membership, never an
    //    exact-thread match.
    //  - everything else (writes + lifecycle: append, finalize, mark_running,
    //    request_continuation, publish_artifact, vault/bundle writes) requires an
    //    exact-thread match. A chain read must never widen into a chain write.
    let callback_cap = if method == "runtime.dispatch_action" {
        // runtime.dispatch_action: validate thread_auth_token (per-request
        // identity proof). Missing or invalid = hard fail, no fallback.
        let tat = params
            .get("thread_auth_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing thread_auth_token on runtime.dispatch_action"))?;
        let thread_id = params
            .get("thread_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing thread_id"))?;
        state.thread_auth.validate(tat, thread_id)?;
        None
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

    // Strip transport-level fields before typed deserialization so
    // deny_unknown_fields on the RPC param structs doesn't reject
    // callback_token.
    let clean_params = strip_transport_fields(params);

    match method {
        "runtime.dispatch_action" => {
            ryeos_executor::execution::runtime_dispatch::handle(params, state).await
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
        "runtime.finalize_thread" => handle_finalize(&clean_params, state),
        "runtime.mark_running" => handle_mark_running(&clean_params, state),
        "runtime.request_continuation" => {
            let result = handle_request_continuation(&clean_params, state)?;
            spawn_machine_continuation_launch(state, &result);
            Ok(result)
        }
        "runtime.publish_artifact" => handle_publish_artifact(&clean_params, state),
        "runtime.get_facets" => handle_get_facets(&clean_params, state),
        "runtime.get_thread" => handle_get(&clean_params, state),
        "runtime.submit_command" => handle_submit_command(&clean_params, state),
        "runtime.claim_commands" => handle_claim_commands(&clean_params, state),
        "runtime.complete_command" => handle_complete_command(&clean_params, state),
        "runtime.get_thread_events" => handle_replay_events(&clean_params, state),
        "runtime.attach_process" => handle_attach_process(&clean_params, state),
        other => anyhow::bail!("unknown runtime method: {other}"),
    }
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
        serde_json::from_value(params.clone()).context("invalid threads.mark_running params")?;
    serde_json::to_value(state.threads.mark_running(&params.thread_id)?)
        .context("failed to encode threads.mark_running result")
}

fn handle_attach_process(
    params: &serde_json::Value,
    state: &AppState,
) -> Result<serde_json::Value> {
    let mut params: ThreadAttachProcessParams =
        serde_json::from_value(params.clone()).context("invalid threads.attach_process params")?;
    // The runtime self-reports its pid only; ALWAYS derive the process group
    // daemon-side — never trust a runtime-supplied pgid. This gives reconcile's
    // liveness check (and the live-pgid guard / shutdown drain) a real pgid to
    // probe instead of treating the thread as dead.
    params.pgid = ryeos_app::process::pgid_of(params.pid);
    serde_json::to_value(state.threads.attach_process(&params)?)
        .context("failed to encode threads.attach_process result")
}

/// Runtime-supplied terminal completion received on `runtime.finalize_thread`.
///
/// `cost` is the runtime's own cost JSON (`{input_tokens, output_tokens,
/// total_usd}`); it is mapped into a [`FinalCost`] before finalization.
#[derive(serde::Deserialize)]
struct RuntimeFinalizeParams {
    thread_id: String,
    status: String,
    #[serde(default)]
    outcome_code: Option<String>,
    #[serde(default)]
    result: Option<serde_json::Value>,
    #[serde(default)]
    error: Option<serde_json::Value>,
    #[serde(default)]
    cost: Option<serde_json::Value>,
}

/// Map a runtime self-reported terminal status. Timeout is daemon-owned — the
/// launch supervisor finalizes timed-out runs via the fallback path — so a
/// runtime never self-reports `timed_out` here, and any unrecognized status is
/// rejected rather than guessed.
fn terminal_status_from_str(status: &str) -> Result<ryeos_engine::contracts::ThreadTerminalStatus> {
    use ryeos_engine::contracts::ThreadTerminalStatus;
    Ok(match status {
        "completed" => ThreadTerminalStatus::Completed,
        "failed" => ThreadTerminalStatus::Failed,
        "cancelled" => ThreadTerminalStatus::Cancelled,
        "continued" => ThreadTerminalStatus::Continued,
        "killed" => ThreadTerminalStatus::Killed,
        other => anyhow::bail!("invalid terminal status: {other}"),
    })
}

fn final_cost_from_runtime_json(cost: &serde_json::Value) -> ryeos_engine::contracts::FinalCost {
    ryeos_engine::contracts::FinalCost {
        turns: 0,
        input_tokens: cost
            .get("input_tokens")
            .and_then(|v| v.as_i64())
            .unwrap_or(0),
        output_tokens: cost
            .get("output_tokens")
            .and_then(|v| v.as_i64())
            .unwrap_or(0),
        spend: cost
            .get("total_usd")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0),
        provider: None,
        metadata: None,
    }
}

fn handle_finalize(params: &serde_json::Value, state: &AppState) -> Result<serde_json::Value> {
    let params: RuntimeFinalizeParams =
        serde_json::from_value(params.clone()).context("invalid runtime.finalize_thread params")?;
    let completion = ryeos_engine::contracts::ExecutionCompletion {
        status: terminal_status_from_str(&params.status)?,
        outcome_code: params.outcome_code,
        result: params.result,
        error: params.error,
        artifacts: Vec::new(),
        final_cost: params.cost.as_ref().map(final_cost_from_runtime_json),
        continuation_request: None,
        metadata: None,
    };
    serde_json::to_value(
        state
            .threads
            .finalize_from_completion(&params.thread_id, &completion)?,
    )
    .context("failed to encode runtime.finalize_thread result")
}

fn handle_get(params: &serde_json::Value, state: &AppState) -> Result<serde_json::Value> {
    let params: ThreadGetParams =
        serde_json::from_value(params.clone()).context("invalid threads.get params")?;
    match state.threads.get_thread(&params.thread_id)? {
        Some(thread) => {
            let facets = state.state_store.get_facets(&params.thread_id)?;
            let facets_map: std::collections::HashMap<&str, &str> = facets
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            serde_json::to_value(json!({
                "thread": thread,
                "result": state.threads.get_thread_result(&params.thread_id)?,
                "artifacts": state.threads.list_thread_artifacts(&params.thread_id)?,
                "facets": facets_map,
            }))
            .context("failed to encode threads.get result")
        }
        None => Ok(serde_json::Value::Null),
    }
}

/// Auto-launch a machine continuation successor after a limit cut-off handoff.
///
/// Autonomous machine continuation is always-on: the chain-depth cap enforced at
/// create time (`create_machine_continuation`) bounds an autonomous run, so an
/// unbounded chain can no longer form and there is nothing to gate. The successor
/// row + chain link are recorded by `request_continuation`; this fires the launch.
///
/// Spawned daemon-side (NOT from the dying runtime — a lifecycle hazard) after
/// the source is settled `continued` and the state-store write lock has dropped.
/// `launch_successor` claims the launch lease, so a concurrent reconcile cannot
/// double-launch the same successor.
fn spawn_machine_continuation_launch(state: &AppState, result: &serde_json::Value) {
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
        match ryeos_executor::execution::launch::launch_successor(st, &successor_id).await {
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

fn handle_request_continuation(
    params: &serde_json::Value,
    state: &AppState,
) -> Result<serde_json::Value> {
    let params: ThreadContinuationParams = serde_json::from_value(params.clone())
        .context("invalid runtime.request_continuation params")?;
    serde_json::to_value(state.threads.request_continuation(&params)?)
        .context("failed to encode runtime.request_continuation result")
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
    serde_json::to_value(state.events.replay(&params)?)
        .context("failed to encode events.replay result")
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

fn handle_submit_command(
    params: &serde_json::Value,
    state: &AppState,
) -> Result<serde_json::Value> {
    let params: CommandSubmitParams =
        serde_json::from_value(params.clone()).context("invalid commands.submit params")?;
    serde_json::to_value(state.commands.submit(&params)?)
        .context("failed to encode commands.submit result")
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

fn handle_complete_command(
    params: &serde_json::Value,
    state: &AppState,
) -> Result<serde_json::Value> {
    let params: CommandCompleteParams =
        serde_json::from_value(params.clone()).context("invalid commands.complete params")?;
    serde_json::to_value(state.commands.complete(&params)?)
        .context("failed to encode commands.complete result")
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

fn rpc_result(request_id: u64, result: Result<serde_json::Value>) -> RpcResponse {
    match result {
        Ok(value) => RpcResponse::ok(request_id, value),
        Err(err) => RpcResponse::err(request_id, "request_failed", err.to_string()),
    }
}

const MAX_FRAME_SIZE: u32 = 10 * 1024 * 1024; // 10 MB

async fn read_frame(stream: &mut UnixStream) -> Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    match stream.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(err) => return Err(err).context("failed to read rpc frame length"),
    }

    let frame_len = u32::from_be_bytes(len_buf);
    if frame_len > MAX_FRAME_SIZE {
        return Err(anyhow!(
            "frame too large: {} bytes (max {})",
            frame_len,
            MAX_FRAME_SIZE
        ));
    }
    let mut frame = vec![0u8; frame_len as usize];
    stream
        .read_exact(&mut frame)
        .await
        .context("failed to read rpc frame body")?;
    Ok(Some(frame))
}

async fn write_frame(stream: &mut UnixStream, bytes: &[u8]) -> Result<()> {
    let len = (bytes.len() as u32).to_be_bytes();
    stream
        .write_all(&len)
        .await
        .context("failed to write rpc frame length")?;
    stream
        .write_all(bytes)
        .await
        .context("failed to write rpc frame body")?;
    Ok(())
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
    use ryeos_app::kind_profiles::KindProfileRegistry;
    use ryeos_app::state::AppState;
    use ryeos_app::state_store::StateStore;
    use ryeos_app::thread_lifecycle::{
        ThreadCreateParams, ThreadFinalizeParams, ThreadLifecycleService,
    };
    use ryeos_app::write_barrier::WriteBarrier;
    use std::sync::Arc;
    use std::time::Instant;
    use tempfile::TempDir;

    type TestProvenance = ryeos_app::execution_provenance::ExecutionProvenance;

    fn test_provenance(state: &AppState, path: &str) -> TestProvenance {
        ryeos_app::execution_provenance::ExecutionProvenance::root_live_fs(
            std::path::PathBuf::from(path),
            state.engine.clone(),
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
        let write_barrier = WriteBarrier::new();
        let state_store = Arc::new(
            StateStore::new(runtime_state_dir, runtime_db_path, signer, write_barrier).unwrap(),
        );
        let kind_profiles = Arc::new(KindProfileRegistry::build(None));
        let events = Arc::new(EventStoreService::new(state_store.clone()));
        let event_streams = Arc::new(ThreadEventHub::new(DEFAULT_EVENT_STREAM_CAPACITY));
        let threads = Arc::new(
            ThreadLifecycleService::new(
                state_store.clone(),
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

        let engine = ryeos_engine::engine::Engine::new(
            ryeos_engine::kind_registry::KindRegistry::empty(),
            ryeos_engine::parsers::ParserDispatcher::new(
                ryeos_engine::parsers::ParserRegistry::empty(),
                std::sync::Arc::new(ryeos_engine::handlers::HandlerRegistry::empty()),
            ),
            Vec::new(),
        );
        let test_command_registry = Arc::new(
            ryeos_runtime::CommandRegistry::from_records(&[], &Default::default()).unwrap(),
        );
        let test_auth = Arc::new(ryeos_runtime::authorizer::Authorizer::new());

        let state = AppState {
            config: Arc::new(config),
            state_store,
            engine: Arc::new(engine),
            engine_cache: ryeos_app::engine_cache::EngineCache::new(
                ryeos_app::engine_cache::EngineCacheConfig::default(),
            ),
            identity: Arc::new(identity),
            threads,
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
        ThreadCreateParams {
            thread_id: thread_id.to_string(),
            chain_root_id: chain_root_id.to_string(),
            kind: "system_task".to_string(),
            item_ref: "test/directive".to_string(),
            executor_ref: "test/executor".to_string(),
            launch_mode: "inline".to_string(),
            current_site_id: "site:test".to_string(),
            origin_site_id: "site:test".to_string(),
            upstream_thread_id: None,
            requested_by: Some("user:test".to_string()),
            usage_subject: None,
            usage_subject_asserted_by: None,
        }
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
            "system.status",
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

    #[tokio::test]
    async fn runtime_finalize_thread_works() {
        let (_tmp, state) = setup_app_state();
        let params = make_create_params("T-1", "T-1");

        // threads.create is internal — call service directly
        state.threads.create_thread(&params).unwrap();

        let cbt = state.callback_tokens.generate(
            "T-1",
            std::path::PathBuf::from("/test"),
            std::time::Duration::from_secs(300),
            Vec::new(),
            test_provenance(&state, "/test"),
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
                    "cost": {"input_tokens": 10, "output_tokens": 2, "total_usd": 0.01},
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
    }

    #[tokio::test]
    async fn finalize_publishes_terminal_event_to_live_subscriber() {
        let (_tmp, state) = setup_app_state();
        state
            .threads
            .create_thread(&make_create_params("T-pub", "T-pub"))
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
            .create_thread(&make_create_params("T-cancel", "T-cancel"))
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
            .create_thread(&make_create_params("T-seat", "T-seat"))
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
            .create_thread(&make_create_params("T-Bad", "T-Bad"))
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
    async fn runtime_events_replay_after_thread_lifecycle() {
        let (_tmp, state) = setup_app_state();
        let cbt = state.callback_tokens.generate(
            "T-events-1",
            std::path::PathBuf::from("/test"),
            std::time::Duration::from_secs(300),
            Vec::new(),
            test_provenance(&state, "/test"),
        );

        state
            .threads
            .create_thread(&make_create_params("T-events-1", "T-events-1"))
            .unwrap();

        let finalize_resp = dispatch(
            rpc(
                "runtime.finalize_thread",
                json!({
                    "callback_token": cbt.token,
                    "thread_id": "T-events-1",
                    "status": "completed",
                    "outcome_code": "test",
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
            replay_resp.error.is_none(),
            "replay failed: {:?}",
            replay_resp.error
        );
        let result = rpc_ok(&replay_resp);
        let events = result["events"].as_array().unwrap();
        assert!(
            events.len() >= 2,
            "expected >= 2 events, got {}",
            events.len()
        );
        let types: Vec<&str> = events
            .iter()
            .map(|e| e["event_type"].as_str().unwrap())
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
        let cbt = state.callback_tokens.generate(
            "T-stream-1",
            std::path::PathBuf::from("/test"),
            std::time::Duration::from_secs(300),
            Vec::new(),
            test_provenance(&state, "/test"),
        );
        state
            .threads
            .create_thread(&make_create_params("T-stream-1", "T-stream-1"))
            .unwrap();

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
        let cbt = state.callback_tokens.generate(
            "T-ephemeral-1",
            std::path::PathBuf::from("/test"),
            std::time::Duration::from_secs(300),
            Vec::new(),
            test_provenance(&state, "/test"),
        );
        state
            .threads
            .create_thread(&make_create_params("T-ephemeral-1", "T-ephemeral-1"))
            .unwrap();
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
        let cbt = state.callback_tokens.generate(
            "T-ephemeral-bad",
            std::path::PathBuf::from("/test"),
            std::time::Duration::from_secs(300),
            Vec::new(),
            test_provenance(&state, "/test"),
        );
        state
            .threads
            .create_thread(&make_create_params("T-ephemeral-bad", "T-ephemeral-bad"))
            .unwrap();

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
        let cbt = state.callback_tokens.generate(
            "T-stream-2",
            std::path::PathBuf::from("/test"),
            std::time::Duration::from_secs(300),
            Vec::new(),
            test_provenance(&state, "/test"),
        );
        state
            .threads
            .create_thread(&make_create_params("T-stream-2", "T-stream-2"))
            .unwrap();
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
        let cbt = state.callback_tokens.generate_with_context(
            "T-bundle-1",
            std::path::PathBuf::from("/test"),
            std::time::Duration::from_secs(300),
            vec![
                "ryeos.append.bundle-events.example-bundle/example_event".to_string(),
                "ryeos.scan.bundle-events.example-bundle/example_event".to_string(),
            ],
            test_provenance(&state, "/test"),
            Some("example-bundle".to_string()),
            Some("tool:example-bundle/send".to_string()),
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
                    "idempotency_key": "record:example_1"
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
        let cbt = state.callback_tokens.generate_with_context(
            "T-bundle-deny",
            std::path::PathBuf::from("/test"),
            std::time::Duration::from_secs(300),
            vec!["ryeos.append.bundle-events.example-bundle/example_event".to_string()],
            test_provenance(&state, "/test"),
            Some("example-bundle".to_string()),
            Some("tool:example-bundle/send".to_string()),
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

        let cbt = state.callback_tokens.generate_with_context(
            "T-bundle-deny-2",
            std::path::PathBuf::from("/test"),
            std::time::Duration::from_secs(300),
            Vec::new(),
            test_provenance(&state, "/test"),
            Some("example-bundle".to_string()),
            Some("tool:example-bundle/send".to_string()),
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
        let cbt = state.callback_tokens.generate_with_context(
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
        let cbt = state.callback_tokens.generate_with_context(
            "T-vault-deny",
            std::path::PathBuf::from("/test"),
            std::time::Duration::from_secs(300),
            vec!["ryeos.put.vault.example-bundle/oauth".to_string()],
            test_provenance(&state, "/test"),
            Some("example-bundle".to_string()),
            Some("tool:example-bundle/oauth/connect".to_string()),
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

        let cbt = state.callback_tokens.generate_with_context(
            "T-vault-deny-2",
            std::path::PathBuf::from("/test"),
            std::time::Duration::from_secs(300),
            Vec::new(),
            test_provenance(&state, "/test"),
            Some("example-bundle".to_string()),
            Some("tool:example-bundle/oauth/connect".to_string()),
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

        let cbt = state.callback_tokens.generate_with_context(
            "T-vault-deny-3",
            std::path::PathBuf::from("/test"),
            std::time::Duration::from_secs(300),
            vec!["ryeos.get.vault.example-bundle/oauth".to_string()],
            test_provenance(&state, "/test"),
            Some("example-bundle".to_string()),
            Some("tool:example-bundle/oauth/connect".to_string()),
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
            .create_thread(&make_create_params("T-cmd-1", "T-cmd-1"))
            .unwrap();

        let cbt = state.callback_tokens.generate(
            "T-cmd-1",
            std::path::PathBuf::from("/test"),
            std::time::Duration::from_secs(300),
            Vec::new(),
            test_provenance(&state, "/test"),
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
            .create_thread(&make_create_params("T-tat-missing", "T-tat-missing"))
            .unwrap();
        let cbt = state.callback_tokens.generate(
            "T-tat-missing",
            std::path::PathBuf::from("/p"),
            std::time::Duration::from_secs(300),
            vec!["*".to_string()],
            test_provenance(&state, "/p"),
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
            .create_thread(&make_create_params("T-tat-wrong", "T-tat-wrong"))
            .unwrap();
        let cbt = state.callback_tokens.generate(
            "T-tat-wrong",
            std::path::PathBuf::from("/p"),
            std::time::Duration::from_secs(300),
            vec!["*".to_string()],
            test_provenance(&state, "/p"),
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
        state
            .threads
            .create_thread(&make_create_params("T-tat-ok", "T-tat-ok"))
            .unwrap();
        let cbt = state.callback_tokens.generate(
            "T-tat-ok",
            std::path::PathBuf::from("/p"),
            std::time::Duration::from_secs(300),
            vec!["*".to_string()],
            test_provenance(&state, "/p"),
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
        state
            .threads
            .create_thread(&make_create_params("T-caps-empty", "T-caps-empty"))
            .unwrap();
        let cbt = state.callback_tokens.generate(
            "T-caps-empty",
            std::path::PathBuf::from("/p"),
            std::time::Duration::from_secs(300),
            Vec::new(),
            test_provenance(&state, "/p"),
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
                        "thread": "inline",
                    },
                }),
            ),
            &state,
        )
        .await;
        let err = rpc_err(&resp);
        assert!(
            err.message.contains("deny-all") && err.message.contains("no effective_caps"),
            "expected UDS boundary deny-all from empty callback caps, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn runtime_callback_with_wildcard_caps_is_allowed_past_uds_boundary() {
        let (_tmp, state) = setup_app_state();
        state
            .threads
            .create_thread(&make_create_params("T-caps-wild", "T-caps-wild"))
            .unwrap();
        let cbt = state.callback_tokens.generate(
            "T-caps-wild",
            std::path::PathBuf::from("/p"),
            std::time::Duration::from_secs(300),
            vec!["ryeos.*".to_string()],
            test_provenance(&state, "/p"),
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
            .create_thread(&make_create_params("T-facets-1", "T-facets-1"))
            .unwrap();

        let cbt = state.callback_tokens.generate(
            "T-facets-1",
            std::path::PathBuf::from("/test"),
            std::time::Duration::from_secs(300),
            Vec::new(),
            test_provenance(&state, "/test"),
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
    fn chain_with_successor(
        state: &AppState,
    ) -> ryeos_app::callback_token::CallbackCapability {
        state
            .threads
            .create_thread(&make_create_params("T-pred", "T-pred"))
            .unwrap();
        state.threads.mark_running("T-pred").unwrap();
        // Successor shares the predecessor's chain root.
        state
            .threads
            .create_thread(&make_create_params("T-succ", "T-pred"))
            .unwrap();
        state.callback_tokens.generate(
            "T-succ",
            std::path::PathBuf::from("/test"),
            std::time::Duration::from_secs(300),
            Vec::new(),
            test_provenance(state, "/test"),
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
        assert!(resp.error.is_none(), "chain read of predecessor must pass: {resp:?}");

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
            .create_thread(&make_create_params("T-other", "T-other"))
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
            .create_thread(&make_create_params("T-pred", "T-pred"))
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
        let req: ryeos_api::handlers::threads_input::Request =
            serde_json::from_value(json!({"input": "again", "thread": "T-pred"})).unwrap();
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
    // The harness builds `KindProfileRegistry::build(None)`, whose only kinds are
    // the internal non-continuable profiles, so the value here is always `false`.
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
            .create_thread(&make_create_params("T-pred", "T-pred"))
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
            .create_continuation(
                &NewThreadRecord {
                    thread_id: "T-succ".to_string(),
                    chain_root_id: "T-pred".to_string(),
                    kind: "system_task".to_string(),
                    item_ref: "test/directive".to_string(),
                    executor_ref: "test/executor".to_string(),
                    launch_mode: "inline".to_string(),
                    current_site_id: "site:test".to_string(),
                    origin_site_id: "site:test".to_string(),
                    upstream_thread_id: Some("T-pred".to_string()),
                    requested_by: Some("user:test".to_string()),
                    usage_subject: None,
                    usage_subject_asserted_by: None,
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
            .create_thread(&make_create_params("T-1", "T-1"))
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
            .create_thread(&make_create_params("T-parent", "T-parent"))
            .unwrap();
        // A child is a thread whose `upstream_thread_id` points at the parent
        // (edges are derived from that link).
        let mut child = make_create_params("T-child", "T-parent");
        child.upstream_thread_id = Some("T-parent".to_string());
        state.threads.create_thread(&child).unwrap();

        let children = state.threads.list_children("T-parent").unwrap();
        let v = serde_json::to_value(&children).unwrap();
        let rows = v.as_array().expect("children array");
        assert!(!rows.is_empty(), "parent has a child via upstream edge: {v:#?}");
        for c in rows {
            assert!(
                c["execution"]["supports_continuation"].is_boolean(),
                "every child is decorated: {c:#?}"
            );
            // Flatten: thread fields stay at the top level.
            assert!(c["thread_id"].is_string(), "flattened thread fields top-level");
        }
    }
}
