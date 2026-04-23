use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

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

        // maintenance.gc requires async dispatch (run_maintenance_gc is async)
        let response = if request.method == "maintenance.gc" {
            dispatch_maintenance_gc(request, &state).await
        } else {
            dispatch(request, &state)
        };

        let encoded = rmp_serde::to_vec_named(&response).context("failed to encode response")?;
        write_frame(&mut stream, &encoded).await?;
    }
}

/// Async handler for `maintenance.gc` RPC method.
///
/// Routes through the daemon's maintenance GC flow:
/// lock → quiesce write barrier → GC → resume.
async fn dispatch_maintenance_gc(request: RpcRequest, state: &AppState) -> RpcResponse {
    use ryeos_state::gc::GcParams;

    let params: GcParams = match serde_json::from_value(request.params.clone()) {
        Ok(p) => p,
        Err(err) => {
            return RpcResponse::err(
                request.request_id,
                "invalid_params",
                format!("invalid maintenance.gc params: {}", err),
            );
        }
    };

    match crate::maintenance::run_maintenance_gc(state, &params).await {
        Ok(result) => match serde_json::to_value(result) {
            Ok(value) => RpcResponse::ok(request.request_id, value),
            Err(err) => RpcResponse::err(request.request_id, "encode_failed", err.to_string()),
        },
        Err(err) => RpcResponse::err(request.request_id, "gc_failed", err.to_string()),
    }
}

pub(crate) fn dispatch(request: RpcRequest, state: &AppState) -> RpcResponse {
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
        "artifacts.publish" => rpc_result(
            request.request_id,
            handle_publish_artifact(&request.params, state),
        ),
        "threads.get_facets" => rpc_result(
            request.request_id,
            handle_get_facets(&request.params, state),
        ),
        "identity.public_key" => rpc_result(
            request.request_id,
            handle_public_key(state),
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
        "runtime.finalize_thread" => handle_finalize(params, state),
        "runtime.mark_running" => handle_mark_running(params, state),
        "runtime.request_continuation" => handle_request_continuation(params, state),
        "runtime.publish_artifact" => handle_publish_artifact(params, state),
        "runtime.get_facets" => handle_get_facets(params, state),
        "runtime.get_thread" => handle_get(params, state),
        "runtime.submit_command" => handle_submit_command(params, state),
        "runtime.claim_commands" => handle_claim_commands(params, state),
        "runtime.complete_command" => handle_complete_command(params, state),
        "runtime.get_thread_events" => handle_replay_events(params, state),
        "runtime.attach_process" => handle_attach_process(params, state),
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
            let facets = state.state_store.get_facets(&params.thread_id)?;
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
    let facets_map: std::collections::HashMap<&str, &str> =
        facets.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    Ok(serde_json::to_value(facets_map).context("failed to encode facets")?)
}

fn handle_public_key(state: &AppState) -> Result<serde_json::Value> {
    let identity_path = state
        .config
        .state_dir
        .join("identity")
        .join("public-identity.json");
    let doc = crate::identity::NodeIdentity::load_public_identity(&identity_path)?;
    serde_json::to_value(doc).context("failed to encode public identity")
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::execution::callback_token::CallbackCapabilityStore;
    use crate::identity::NodeIdentity;
    use crate::kind_profiles::KindProfileRegistry;
    use crate::services::command_service::CommandService;
    use crate::services::event_store::EventStoreService;
    use crate::services::thread_lifecycle::{ThreadCreateParams, ThreadLifecycleService};
    use crate::state::AppState;
    use crate::state_store::StateStore;
    use crate::uds::protocol::RpcError;
    use crate::write_barrier::WriteBarrier;
    use std::sync::Arc;
    use std::time::Instant;
    use tempfile::TempDir;

    /// Build a minimal AppState for UDS dispatch tests.
    fn setup_app_state() -> (TempDir, AppState) {
        let tmpdir = TempDir::new().unwrap();
        let state_root = tmpdir.path().join(".state");
        let runtime_db_path = tmpdir.path().join("runtime.sqlite3");
        let key_path = tmpdir.path().join("identity").join("node-key.pem");
        let config = Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            db_path: runtime_db_path.clone(),
            uds_path: tmpdir.path().join("test.sock"),
            state_dir: tmpdir.path().to_path_buf(),
            signing_key_path: key_path.clone(),
            system_data_dir: tmpdir.path().join("system"),
            bundle_roots: Vec::new(),
            require_auth: false,
            authorized_keys_dir: tmpdir.path().join("auth"),
        };

        let identity = NodeIdentity::create(&key_path).unwrap();
        identity.write_public_identity(
            &tmpdir.path().join("identity").join("public-identity.json"),
        ).unwrap();

        let signer = Arc::new(
            crate::state_store::NodeIdentitySigner::from_identity(&identity),
        );
        let write_barrier = WriteBarrier::new();
        let state_store = Arc::new(
            StateStore::new(state_root, runtime_db_path, signer, write_barrier).unwrap(),
        );
        let kind_profiles = Arc::new(KindProfileRegistry::load_defaults());
        let events = Arc::new(EventStoreService::new(state_store.clone()));
        let threads = Arc::new(ThreadLifecycleService::new(
            state_store.clone(),
            kind_profiles.clone(),
            events.clone(),
        ));
        let commands = Arc::new(CommandService::new(
            state_store.clone(),
            kind_profiles,
            events.clone(),
        ));

        let engine = ryeos_engine::engine::Engine::new(
            ryeos_engine::kind_registry::KindRegistry::empty(),
            ryeos_engine::executor_registry::ExecutorRegistry::new(),
            ryeos_engine::metadata::MetadataParserRegistry::new(),
            None,
            Vec::new(),
        );

        let state = AppState {
            config: Arc::new(config),
            state_store,
            engine: Arc::new(engine),
            identity: Arc::new(identity),
            threads,
            events,
            commands,
            callback_tokens: Arc::new(CallbackCapabilityStore::new()),
            write_barrier: Arc::new(WriteBarrier::new()),
            started_at: Instant::now(),
            started_at_iso: lillux::time::iso8601_now(),
        };

        (tmpdir, state)
    }

    fn make_create_params(thread_id: &str, chain_root_id: &str) -> ThreadCreateParams {
        ThreadCreateParams {
            thread_id: thread_id.to_string(),
            chain_root_id: chain_root_id.to_string(),
            kind: "directive_run".to_string(),
            item_ref: "test/directive".to_string(),
            executor_ref: "test/executor".to_string(),
            launch_mode: "inline".to_string(),
            current_site_id: "site:test".to_string(),
            origin_site_id: "site:test".to_string(),
            upstream_thread_id: None,
            requested_by: Some("user:test".to_string()),
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

    #[test]
    fn system_health_returns_ok() {
        let (_tmp, state) = setup_app_state();
        let resp = dispatch(rpc("system.health", json!({})), &state);
        assert!(resp.error.is_none());
        assert_eq!(rpc_ok(&resp)["status"], "ok");
    }

    #[test]
    fn system_status_returns_status_object() {
        let (_tmp, state) = setup_app_state();
        let resp = dispatch(rpc("system.status", json!({})), &state);
        assert!(resp.error.is_none());
        let result = rpc_ok(&resp);
        assert!(result.get("version").is_some());
        assert!(result.get("uptime_seconds").is_some());
        assert!(result.get("bind").is_some());
    }

    #[test]
    fn unknown_method_returns_error() {
        let (_tmp, state) = setup_app_state();
        let resp = dispatch(rpc("nonexistent.method", json!({})), &state);
        let err = rpc_err(&resp);
        assert_eq!(err.code, "unknown_method");
    }

    // ── identity methods ────────────────────────────────────────────

    #[test]
    fn identity_public_key_returns_key() {
        let (_tmp, state) = setup_app_state();
        let resp = dispatch(rpc("identity.public_key", json!({})), &state);
        assert!(resp.error.is_none(), "expected ok, got error: {:?}", resp.error);
        let result = rpc_ok(&resp);
        assert_eq!(result["kind"], "identity/v1");
        assert!(result.get("principal_id").is_some());
        assert!(result.get("signing_key").is_some());
    }

    // ── thread lifecycle methods ────────────────────────────────────

    #[test]
    fn threads_create_and_get_roundtrip() {
        let (_tmp, state) = setup_app_state();
        let params = make_create_params("T-1", "T-1");

        let create_resp = dispatch(
            rpc("threads.create", serde_json::to_value(&params).unwrap()),
            &state,
        );
        assert!(create_resp.error.is_none(), "create failed: {:?}", create_resp.error);

        let get_resp = dispatch(rpc("threads.get", json!({ "thread_id": "T-1" })), &state);
        assert!(get_resp.error.is_none(), "get failed: {:?}", get_resp.error);
        let result = rpc_ok(&get_resp);
        assert_eq!(result["thread"]["thread_id"], "T-1");
        assert_eq!(result["thread"]["kind"], "directive_run");
    }

    #[test]
    fn threads_get_missing_returns_null() {
        let (_tmp, state) = setup_app_state();
        let resp = dispatch(rpc("threads.get", json!({ "thread_id": "NONEXISTENT" })), &state);
        assert!(resp.error.is_none());
        assert_eq!(*rpc_ok(&resp), serde_json::Value::Null);
    }

    #[test]
    fn threads_list_returns_threads() {
        let (_tmp, state) = setup_app_state();

        state.threads.create_thread(&make_create_params("T-list-1", "T-list-1")).unwrap();
        state.threads.create_thread(&make_create_params("T-list-2", "T-list-2")).unwrap();

        let resp = dispatch(rpc("threads.list", json!({ "limit": 10 })), &state);
        assert!(resp.error.is_none());
        let result = rpc_ok(&resp);
        let threads = result["threads"].as_array().unwrap();
        assert!(threads.len() >= 2);
    }

    #[test]
    fn threads_chain_requires_existing_thread() {
        let (_tmp, state) = setup_app_state();
        let resp = dispatch(rpc("threads.chain", json!({ "thread_id": "NONEXISTENT" })), &state);
        assert!(resp.error.is_none());
        assert_eq!(*rpc_ok(&resp), serde_json::Value::Null);
    }

    #[test]
    fn threads_children_empty_for_root() {
        let (_tmp, state) = setup_app_state();
        state.threads.create_thread(&make_create_params("T-root", "T-root")).unwrap();

        let resp = dispatch(
            rpc("threads.children", json!({ "thread_id": "T-root" })),
            &state,
        );
        assert!(resp.error.is_none());
        let result = rpc_ok(&resp);
        assert_eq!(result["children"], json!([]));
    }

    // ── thread finalize and events ──────────────────────────────────

    #[test]
    fn events_replay_after_thread_lifecycle() {
        let (_tmp, state) = setup_app_state();
        state.threads.create_thread(&make_create_params("T-events-1", "T-events-1")).unwrap();

        let finalize_resp = dispatch(
            rpc("threads.finalize", json!({
                "thread_id": "T-events-1",
                "status": "completed",
                "outcome_code": "test",
            })),
            &state,
        );
        assert!(finalize_resp.error.is_none(), "finalize failed: {:?}", finalize_resp.error);

        let replay_resp = dispatch(
            rpc("events.replay", json!({ "thread_id": "T-events-1", "limit": 10 })),
            &state,
        );
        assert!(replay_resp.error.is_none(), "replay failed: {:?}", replay_resp.error);
        let result = rpc_ok(&replay_resp);
        let events = result["events"].as_array().unwrap();
        assert!(events.len() >= 2, "expected >= 2 events, got {}", events.len());
        let types: Vec<&str> = events.iter().map(|e| e["event_type"].as_str().unwrap()).collect();
        assert!(types.contains(&"thread_created"));
        assert!(types.contains(&"thread_completed"));
    }

    // ── command methods ────────────────────────────────────────────

    #[test]
    fn commands_submit_and_claim() {
        let (_tmp, state) = setup_app_state();
        state.threads.create_thread(&make_create_params("T-cmd-1", "T-cmd-1")).unwrap();

        // Mark running first — cancel is only allowed on running threads
        let _ = dispatch(
            rpc("threads.mark_running", json!({ "thread_id": "T-cmd-1" })),
            &state,
        );

        let submit_resp = dispatch(
            rpc("commands.submit", json!({
                "thread_id": "T-cmd-1",
                "command_type": "cancel",
            })),
            &state,
        );
        assert!(submit_resp.error.is_none(), "submit failed: {:?}", submit_resp.error);
        let submitted = rpc_ok(&submit_resp);
        assert_eq!(submitted["command_type"], "cancel");

        let claim_resp = dispatch(
            rpc("commands.claim", json!({ "thread_id": "T-cmd-1" })),
            &state,
        );
        assert!(claim_resp.error.is_none(), "claim failed: {:?}", claim_resp.error);
        let claimed = rpc_ok(&claim_resp);
        let commands = claimed["commands"].as_array().unwrap();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0]["command_type"], "cancel");
    }

    // ── error handling ──────────────────────────────────────────────

    #[test]
    fn threads_create_missing_fields_returns_error() {
        let (_tmp, state) = setup_app_state();
        let resp = dispatch(rpc("threads.create", json!({ "thread_id": "T-Bad" })), &state);
        assert!(resp.error.is_some());
        assert_eq!(rpc_err(&resp).code, "request_failed");
    }

    #[test]
    fn events_replay_missing_thread_returns_error() {
        let (_tmp, state) = setup_app_state();
        let resp = dispatch(
            rpc("events.replay", json!({ "thread_id": "NONEXISTENT" })),
            &state,
        );
        assert!(resp.error.is_some());
    }

    #[test]
    fn threads_get_facets_missing_thread_id_returns_error() {
        let (_tmp, state) = setup_app_state();
        let resp = dispatch(rpc("threads.get_facets", json!({})), &state);
        assert!(resp.error.is_some());
        assert_eq!(rpc_err(&resp).code, "request_failed");
    }
}

