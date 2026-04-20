use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

use crate::services::budget_service::{
    BudgetGetParams, BudgetReleaseParams, BudgetReportParams, BudgetReserveParams,
};
use crate::services::command_service::{
    CommandClaimParams, CommandCompleteParams, CommandSubmitParams,
};
use crate::services::event_store::{EventAppendBatchParams, EventAppendParams, EventReplayParams};
use crate::services::thread_lifecycle::{
    ArtifactPublishParams, ThreadAttachProcessParams, ThreadChainParams, ThreadChildrenParams,
    ThreadContinuationParams, ThreadCreateParams, ThreadFinalizeParams, ThreadGetParams,
    ThreadListParams, ThreadMarkRunningParams,
};
use crate::state::AppState;
use crate::uds::protocol::{RpcRequest, RpcResponse};

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
        let response = dispatch(request, &state);
        let encoded = rmp_serde::to_vec_named(&response).context("failed to encode response")?;
        write_frame(&mut stream, &encoded).await?;
    }
}

fn dispatch(request: RpcRequest, state: &AppState) -> RpcResponse {
    match request.method.as_str() {
        "system.health" => RpcResponse::ok(request.request_id, json!({ "status": "ok" })),
        "system.status" => match serde_json::to_value(state.status()) {
            Ok(status) => RpcResponse::ok(request.request_id, status),
            Err(err) => RpcResponse::err(request.request_id, "encode_failed", err.to_string()),
        },
        "threads.create" => rpc_result(request.request_id, handle_create(&request.params, state)),
        "threads.mark_running" => rpc_result(
            request.request_id,
            handle_mark_running(&request.params, state),
        ),
        "threads.attach_process" => rpc_result(
            request.request_id,
            handle_attach_process(&request.params, state),
        ),
        "threads.finalize" => {
            rpc_result(request.request_id, handle_finalize(&request.params, state))
        }
        "threads.get" => rpc_result(request.request_id, handle_get(&request.params, state)),
        "threads.list" => rpc_result(request.request_id, handle_list(&request.params, state)),
        "threads.children" => {
            rpc_result(request.request_id, handle_children(&request.params, state))
        }
        "threads.chain" => rpc_result(request.request_id, handle_chain(&request.params, state)),
        "threads.request_continuation" => rpc_result(
            request.request_id,
            handle_request_continuation(&request.params, state),
        ),
        "events.append" => rpc_result(
            request.request_id,
            handle_append_event(&request.params, state),
        ),
        "events.append_batch" => rpc_result(
            request.request_id,
            handle_append_event_batch(&request.params, state),
        ),
        "events.replay" => rpc_result(
            request.request_id,
            handle_replay_events(&request.params, state),
        ),
        "commands.submit" => rpc_result(
            request.request_id,
            handle_submit_command(&request.params, state),
        ),
        "commands.claim" => rpc_result(
            request.request_id,
            handle_claim_commands(&request.params, state),
        ),
        "commands.complete" => rpc_result(
            request.request_id,
            handle_complete_command(&request.params, state),
        ),
        "budgets.reserve" => rpc_result(
            request.request_id,
            handle_reserve_budget(&request.params, state),
        ),
        "budgets.report" => rpc_result(
            request.request_id,
            handle_report_budget(&request.params, state),
        ),
        "budgets.release" => rpc_result(
            request.request_id,
            handle_release_budget(&request.params, state),
        ),
        "budgets.get" => rpc_result(
            request.request_id,
            handle_get_budget(&request.params, state),
        ),
        "artifacts.publish" => rpc_result(
            request.request_id,
            handle_publish_artifact(&request.params, state),
        ),
        "threads.set_facets" => rpc_result(
            request.request_id,
            handle_set_facets(&request.params, state),
        ),
        "threads.get_facets" => rpc_result(
            request.request_id,
            handle_get_facets(&request.params, state),
        ),
        other if other.starts_with("runtime.") => {
            rpc_result(request.request_id, dispatch_runtime_method(other, &request.params, state))
        }
        other => RpcResponse::err(
            request.request_id,
            "unknown_method",
            format!("unknown rpc method: {other}"),
        ),
    }
}

pub fn dispatch_runtime_method(
    method: &str,
    params: &serde_json::Value,
    state: &AppState,
) -> Result<serde_json::Value> {
    // Validate callback token on ALL runtime.* methods
    // dispatch_action does its own stronger validation (primary + project_path)
    if method != "runtime.dispatch_action" {
        let token = params.get("callback_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing callback_token"))?;
        let thread_id = params.get("thread_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing thread_id"))?;
        state.callback_tokens.validate_token_and_thread(token, thread_id)?;
    }

    match method {
        "runtime.dispatch_action" => {
            crate::execution::runtime_dispatch::handle(params, state)
        }
        "runtime.append_event" => handle_append_event(params, state),
        "runtime.append_events" => handle_append_event_batch(params, state),
        "runtime.replay_events" => handle_replay_events(params, state),
        "runtime.reserve_budget" => handle_reserve_budget(params, state),
        "runtime.report_budget" => handle_report_budget(params, state),
        "runtime.release_budget" => handle_release_budget(params, state),
        "runtime.get_budget" => handle_get_budget(params, state),
        "runtime.finalize_thread" => handle_finalize(params, state),
        "runtime.mark_running" => handle_mark_running(params, state),
        "runtime.request_continuation" => handle_request_continuation(params, state),
        "runtime.publish_artifact" => handle_publish_artifact(params, state),
        "runtime.set_facets" => handle_set_facets(params, state),
        other => anyhow::bail!("unknown runtime method: {other}"),
    }
}

fn handle_create(params: &serde_json::Value, state: &AppState) -> Result<serde_json::Value> {
    let params: ThreadCreateParams =
        serde_json::from_value(params.clone()).context("invalid threads.create params")?;
    serde_json::to_value(state.threads.create_thread(&params)?)
        .context("failed to encode threads.create result")
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
    let params: ThreadAttachProcessParams =
        serde_json::from_value(params.clone()).context("invalid threads.attach_process params")?;
    serde_json::to_value(state.threads.attach_process(&params)?)
        .context("failed to encode threads.attach_process result")
}

fn handle_finalize(params: &serde_json::Value, state: &AppState) -> Result<serde_json::Value> {
    let params: ThreadFinalizeParams =
        serde_json::from_value(params.clone()).context("invalid threads.finalize params")?;
    serde_json::to_value(state.threads.finalize_thread(&params)?)
        .context("failed to encode threads.finalize result")
}

fn handle_get(params: &serde_json::Value, state: &AppState) -> Result<serde_json::Value> {
    let params: ThreadGetParams =
        serde_json::from_value(params.clone()).context("invalid threads.get params")?;
    match state.threads.get_thread(&params.thread_id)? {
        Some(thread) => {
            let facets = state.db.get_facets(&params.thread_id)?;
            let facets_map: std::collections::HashMap<&str, &str> =
                facets.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
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

fn handle_list(params: &serde_json::Value, state: &AppState) -> Result<serde_json::Value> {
    let params: ThreadListParams =
        serde_json::from_value(params.clone()).context("invalid threads.list params")?;
    state.threads.list_threads(params.limit)
}

fn handle_children(params: &serde_json::Value, state: &AppState) -> Result<serde_json::Value> {
    let params: ThreadChildrenParams =
        serde_json::from_value(params.clone()).context("invalid threads.children params")?;
    serde_json::to_value(json!({
        "children": state.threads.list_children(&params.thread_id)?,
    }))
    .context("failed to encode threads.children result")
}

fn handle_chain(params: &serde_json::Value, state: &AppState) -> Result<serde_json::Value> {
    let params: ThreadChainParams =
        serde_json::from_value(params.clone()).context("invalid threads.chain params")?;
    match state.threads.get_chain(&params.thread_id)? {
        Some(chain) => serde_json::to_value(chain).context("failed to encode threads.chain result"),
        None => Ok(serde_json::Value::Null),
    }
}

fn handle_request_continuation(
    params: &serde_json::Value,
    state: &AppState,
) -> Result<serde_json::Value> {
    let params: ThreadContinuationParams = serde_json::from_value(params.clone())
        .context("invalid threads.request_continuation params")?;
    serde_json::to_value(state.threads.request_continuation(&params)?)
        .context("failed to encode threads.request_continuation result")
}

fn handle_append_event(params: &serde_json::Value, state: &AppState) -> Result<serde_json::Value> {
    let params: EventAppendParams =
        serde_json::from_value(params.clone()).context("invalid events.append params")?;
    serde_json::to_value(state.events.append(&params)?)
        .context("failed to encode events.append result")
}

fn handle_append_event_batch(
    params: &serde_json::Value,
    state: &AppState,
) -> Result<serde_json::Value> {
    let params: EventAppendBatchParams =
        serde_json::from_value(params.clone()).context("invalid events.append_batch params")?;
    serde_json::to_value(state.events.append_batch(&params)?)
        .context("failed to encode events.append_batch result")
}

fn handle_replay_events(params: &serde_json::Value, state: &AppState) -> Result<serde_json::Value> {
    let params: EventReplayParams =
        serde_json::from_value(params.clone()).context("invalid events.replay params")?;
    serde_json::to_value(state.events.replay(&params)?)
        .context("failed to encode events.replay result")
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

fn handle_reserve_budget(
    params: &serde_json::Value,
    state: &AppState,
) -> Result<serde_json::Value> {
    let params: BudgetReserveParams =
        serde_json::from_value(params.clone()).context("invalid budgets.reserve params")?;
    serde_json::to_value(state.budgets.reserve(&params)?)
        .context("failed to encode budgets.reserve result")
}

fn handle_report_budget(params: &serde_json::Value, state: &AppState) -> Result<serde_json::Value> {
    let params: BudgetReportParams =
        serde_json::from_value(params.clone()).context("invalid budgets.report params")?;
    serde_json::to_value(state.budgets.report(&params)?)
        .context("failed to encode budgets.report result")
}

fn handle_release_budget(
    params: &serde_json::Value,
    state: &AppState,
) -> Result<serde_json::Value> {
    let params: BudgetReleaseParams =
        serde_json::from_value(params.clone()).context("invalid budgets.release params")?;
    serde_json::to_value(state.budgets.release(&params)?)
        .context("failed to encode budgets.release result")
}

fn handle_get_budget(params: &serde_json::Value, state: &AppState) -> Result<serde_json::Value> {
    let params: BudgetGetParams =
        serde_json::from_value(params.clone()).context("invalid budgets.get params")?;
    match state.budgets.get(&params)? {
        Some(budget) => serde_json::to_value(budget).context("failed to encode budgets.get result"),
        None => Ok(serde_json::Value::Null),
    }
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

fn handle_set_facets(params: &serde_json::Value, state: &AppState) -> Result<serde_json::Value> {
    let thread_id = params
        .get("thread_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing thread_id"))?;
    let facets_value = params
        .get("facets")
        .ok_or_else(|| anyhow::anyhow!("missing facets"))?;
    let facets_map: std::collections::HashMap<String, String> =
        serde_json::from_value(facets_value.clone())
            .context("facets must be a map of string keys to string values")?;
    let facets: Vec<(String, String)> = facets_map.into_iter().collect();
    state.db.set_facets(thread_id, &facets)?;
    Ok(json!({ "ok": true }))
}

fn handle_get_facets(params: &serde_json::Value, state: &AppState) -> Result<serde_json::Value> {
    let thread_id = params
        .get("thread_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing thread_id"))?;
    let facets = state.db.get_facets(thread_id)?;
    let facets_map: std::collections::HashMap<&str, &str> =
        facets.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    Ok(serde_json::to_value(facets_map).context("failed to encode facets")?)
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
        return Err(anyhow!("frame too large: {} bytes (max {})", frame_len, MAX_FRAME_SIZE));
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
