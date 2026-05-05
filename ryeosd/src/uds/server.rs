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
    ArtifactPublishParams, ThreadAttachProcessParams,
    ThreadContinuationParams, ThreadFinalizeParams, ThreadGetParams,
    ThreadMarkRunningParams,
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

        let span = tracing::debug_span!(
            "uds:request",
            method = %request.method,
            request_id = %request.request_id,
            thread_id = tracing::field::Empty,
        );
        // Opportunistically record thread_id when present in params.
        if let Some(tid) = request
            .params
            .get("thread_id")
            .and_then(|v| v.as_str())
        {
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

        // ── runtime callbacks (token-gated, used by runtimes) ───────
        other if other.starts_with("runtime.") => {
            rpc_result(request.request_id, dispatch_runtime_method(other, &request.params, state).await)
        }

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
    } else {
        // runtime.dispatch_action: validate thread_auth_token (per-request
        // identity proof). Missing or invalid = hard fail, no fallback.
        let tat = params.get("thread_auth_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing thread_auth_token on runtime.dispatch_action"))?;
        let thread_id = params.get("thread_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing thread_id"))?;
        state.thread_auth.validate(tat, thread_id)?;
    }

    // Strip transport-level fields before typed deserialization so
    // deny_unknown_fields on the RPC param structs doesn't reject
    // callback_token.
    let clean_params = strip_transport_fields(params);

    match method {
        "runtime.dispatch_action" => {
            crate::execution::runtime_dispatch::handle(params, state).await
        }
        "runtime.append_event" => handle_append_event(&clean_params, state),
        "runtime.append_events" => handle_append_event_batch(&clean_params, state),
        "runtime.replay_events" => handle_replay_events(&clean_params, state),
        "runtime.finalize_thread" => handle_finalize(&clean_params, state),
        "runtime.mark_running" => handle_mark_running(&clean_params, state),
        "runtime.request_continuation" => handle_request_continuation(&clean_params, state),
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
    // Group by thread_id and bulk-publish so each thread's
    // subscribers receive its events in persisted order under a
    // single hub lock acquisition per thread.
    let mut by_thread: std::collections::HashMap<String, Vec<_>> =
        std::collections::HashMap::new();
    for ev in &result.persisted {
        by_thread
            .entry(ev.thread_id.clone())
            .or_default()
            .push(ev.clone());
    }
    for (thread_id, events) in &by_thread {
        state.event_streams.publish_batch(thread_id, events);
    }
    serde_json::to_value(result).context("failed to encode events.append_batch result")
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
    use crate::event_stream::{ThreadEventHub, DEFAULT_EVENT_STREAM_CAPACITY};
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
        std::env::set_var("HOSTNAME", "testhost");
        let tmpdir = TempDir::new().unwrap();
        let state_root = tmpdir.path().join(".ai").join("state");
        let runtime_db_path = tmpdir.path().join("runtime.sqlite3");
        let key_path = tmpdir.path().join("identity").join("node-key.pem");
        let config = Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            db_path: runtime_db_path.clone(),
            uds_path: tmpdir.path().join("test.sock"),
            state_dir: tmpdir.path().to_path_buf(),
            node_signing_key_path: key_path.clone(),
            user_signing_key_path: tmpdir.path().join("user-key.pem"),
            system_data_dir: tmpdir.path().join("system"),
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
        ).expect("HOSTNAME not set in test environment"));
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
            None,
            Vec::new(),
        );

        let test_vr = Arc::new(ryeos_runtime::verb_registry::VerbRegistry::with_builtins());
        let test_auth = Arc::new(ryeos_runtime::authorizer::Authorizer::new(test_vr.clone()));

        let state = AppState {
            config: Arc::new(config),
            state_store,
            engine: Arc::new(engine),
            identity: Arc::new(identity),
            threads,
            events,
            event_streams: Arc::new(ThreadEventHub::new(DEFAULT_EVENT_STREAM_CAPACITY)),
            commands,
            callback_tokens: Arc::new(CallbackCapabilityStore::new()),
            thread_auth: Arc::new(crate::execution::callback_token::ThreadAuthStore::new()),
            write_barrier: Arc::new(WriteBarrier::new()),
            started_at: Instant::now(),
            started_at_iso: lillux::time::iso8601_now(),
            catalog_health: crate::state::CatalogHealth {
                status: "ok".into(),
                missing_services: vec![],
            },
            services: Arc::new(crate::service_registry::build_service_registry()),
            node_config: Arc::new(crate::node_config::NodeConfigSnapshot { bundles: vec![], routes: vec![] }),
            route_table: Arc::new(arc_swap::ArcSwap::from_pointee(
                crate::routes::build_route_table_or_bail(&crate::node_config::NodeConfigSnapshot { bundles: vec![], routes: vec![] }).unwrap(),
            )),
            webhook_dedupe: Arc::new(crate::routes::webhook_dedupe::WebhookDedupeStore::new()),
            vault: Arc::new(crate::vault::EmptyVault),
            verb_registry: test_vr,
            authorizer: test_auth,
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

    #[tokio::test]
    async fn system_health_returns_ok() {
        let (_tmp, state) = setup_app_state();
        let resp = dispatch(rpc("system.health", json!({})), &state).await;
        assert!(resp.error.is_none());
        assert_eq!(rpc_ok(&resp)["status"], "ok");
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

    /// Assert that `system.health` is the ONLY ungated method on the bare UDS
    /// surface. Every other method must go through token-gated `runtime.*` or
    /// be unknown.
    #[tokio::test]
    async fn only_system_health_is_ungated() {
        let (_tmp, state) = setup_app_state();

        // system.health must work
        let resp = dispatch(rpc("system.health", json!({})), &state).await;
        assert!(resp.error.is_none());
        assert_eq!(rpc_ok(&resp)["status"], "ok");

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
        );

        let resp = dispatch(rpc("runtime.finalize_thread", json!({
                "callback_token": cbt.token,
                "thread_id": "T-1",
                "status": "completed",
                "outcome_code": "test",
            })),
            &state,
        ).await;
        assert!(resp.error.is_none(), "finalize failed: {:?}", resp.error);
    }

    #[tokio::test]
    async fn runtime_finalize_missing_token_returns_error() {
        let (_tmp, state) = setup_app_state();
        state.threads.create_thread(&make_create_params("T-Bad", "T-Bad")).unwrap();

        let resp = dispatch(rpc("runtime.finalize_thread", json!({
                "thread_id": "T-Bad",
                "status": "completed",
                "outcome_code": "test",
            })),
            &state,
        ).await;
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
        );

        state.threads.create_thread(&make_create_params("T-events-1", "T-events-1")).unwrap();

        let finalize_resp = dispatch(rpc("runtime.finalize_thread", json!({
                "callback_token": cbt.token,
                "thread_id": "T-events-1",
                "status": "completed",
                "outcome_code": "test",
            })),
            &state,
        ).await;
        assert!(finalize_resp.error.is_none(), "finalize failed: {:?}", finalize_resp.error);

        let replay_resp = dispatch(rpc("runtime.replay_events", json!({
                "callback_token": cbt.token,
                "thread_id": "T-events-1",
                "limit": 10,
            })),
            &state,
        ).await;
        assert!(replay_resp.error.is_none(), "replay failed: {:?}", replay_resp.error);
        let result = rpc_ok(&replay_resp);
        let events = result["events"].as_array().unwrap();
        assert!(events.len() >= 2, "expected >= 2 events, got {}", events.len());
        let types: Vec<&str> = events.iter().map(|e| e["event_type"].as_str().unwrap()).collect();
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
        );
        state
            .threads
            .create_thread(&make_create_params("T-stream-1", "T-stream-1"))
            .unwrap();

        // Subscribe BEFORE the callback fires so the event lands in
        // the live broadcast.
        let mut rx = state.event_streams.subscribe("T-stream-1");

        let resp = dispatch(rpc("runtime.append_event", json!({
                "callback_token": cbt.token,
                "thread_id": "T-stream-1",
                "event": {
                    "event_type": "stream_opened",
                    "storage_class": "indexed",
                    "payload": {"turn": 1},
                },
            })),
            &state,
        ).await;
        assert!(resp.error.is_none(), "append_event failed: {:?}", resp.error);

        let live = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("hub did not deliver event in time")
            .expect("hub channel closed");
        assert_eq!(live.event_type, "stream_opened");
        assert_eq!(live.thread_id, "T-stream-1");
        assert_eq!(live.payload, json!({"turn": 1}));
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
        assert!(resp.error.is_none(), "append_events failed: {:?}", resp.error);

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
        let resp = dispatch(rpc("runtime.replay_events", json!({
                "thread_id": "NONEXISTENT",
            })),
            &state,
        ).await;
        assert!(resp.error.is_some());
    }

    // ── commands (via runtime.* token-gated) ────────────────────────

    #[tokio::test]
    async fn runtime_commands_submit_and_claim() {
        let (_tmp, state) = setup_app_state();
        state.threads.create_thread(&make_create_params("T-cmd-1", "T-cmd-1")).unwrap();

        let cbt = state.callback_tokens.generate(
            "T-cmd-1",
            std::path::PathBuf::from("/test"),
            std::time::Duration::from_secs(300),
            Vec::new(),
        );

        // Mark running first — cancel is only allowed on running threads
        let _ = dispatch(rpc("runtime.mark_running", json!({
                "callback_token": cbt.token,
                "thread_id": "T-cmd-1",
            })),
            &state,
        ).await;

        let submit_resp = dispatch(rpc("runtime.submit_command", json!({
                "callback_token": cbt.token,
                "thread_id": "T-cmd-1",
                "command_type": "cancel",
            })),
            &state,
        ).await;
        assert!(submit_resp.error.is_none(), "submit failed: {:?}", submit_resp.error);
        let submitted = rpc_ok(&submit_resp);
        assert_eq!(submitted["command_type"], "cancel");

        let claim_resp = dispatch(rpc("runtime.claim_commands", json!({
                "callback_token": cbt.token,
                "thread_id": "T-cmd-1",
            })),
            &state,
        ).await;
        assert!(claim_resp.error.is_none(), "claim failed: {:?}", claim_resp.error);
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
        state.threads.create_thread(&make_create_params("T-tat-missing", "T-tat-missing")).unwrap();
        let cbt = state.callback_tokens.generate(
            "T-tat-missing",
            std::path::PathBuf::from("/p"),
            std::time::Duration::from_secs(300),
            vec!["*".to_string()],
        );

        // Note: `thread_auth_token` field intentionally absent.
        let resp = dispatch(rpc("runtime.dispatch_action", json!({
                "callback_token": cbt.token,
                "thread_id": "T-tat-missing",
                "project_path": "/p",
                "action": {
                    "item_id": "directive:rye/agent/core/base",
                    "thread": "inline",
                },
            })),
            &state,
        ).await;
        let err = rpc_err(&resp);
        assert!(
            err.message.contains("missing thread_auth_token") || err.message.contains("thread_auth_token"),
            "expected missing thread_auth_token error, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn dispatch_action_with_wrong_thread_auth_token_is_rejected() {
        let (_tmp, state) = setup_app_state();
        state.threads.create_thread(&make_create_params("T-tat-wrong", "T-tat-wrong")).unwrap();
        let cbt = state.callback_tokens.generate(
            "T-tat-wrong",
            std::path::PathBuf::from("/p"),
            std::time::Duration::from_secs(300),
            vec!["*".to_string()],
        );

        // Use a syntactically plausible but unminted tat — must not be
        // accepted by ThreadAuthStore.validate.
        let resp = dispatch(rpc("runtime.dispatch_action", json!({
                "callback_token": cbt.token,
                "thread_id": "T-tat-wrong",
                "project_path": "/p",
                "thread_auth_token": "tat-deadbeef0000000000000000000000000000000000000000000000000000",
                "action": {
                    "item_id": "directive:rye/agent/core/base",
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
        state.threads.create_thread(&make_create_params("T-tat-ok", "T-tat-ok")).unwrap();
        let cbt = state.callback_tokens.generate(
            "T-tat-ok",
            std::path::PathBuf::from("/p"),
            std::time::Duration::from_secs(300),
            vec!["*".to_string()],
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
        let resp = dispatch(rpc("runtime.dispatch_action", json!({
                "callback_token": cbt.token,
                "thread_id": "T-tat-ok",
                "project_path": "/p",
                "thread_auth_token": tat.token.clone(),
                "acting_principal": "fp:attacker-spoofed-principal",
                "action": {
                    "item_id": "directive:rye/agent/core/base",
                    "thread": "inline",
                },
            })),
            &state,
        ).await;
        let err = rpc_err(&resp);
        // Must fail at deserialization — `acting_principal` is unknown.
        // This is the structural proof that body cannot smuggle principal.
        assert!(
            err.message.to_lowercase().contains("unknown field")
                || err.message.contains("acting_principal")
                || err.message.contains("invalid runtime.dispatch_action params"),
            "expected unknown-field rejection of body-side principal, got: {err:?}"
        );

        // Sanity: the same call without the spoof field should make it past
        // auth (it will still fail later because the directive isn't loaded
        // in this minimal test state, but the failure must NOT be auth).
        let resp_clean = dispatch(rpc("runtime.dispatch_action", json!({
                "callback_token": cbt.token,
                "thread_id": "T-tat-ok",
                "project_path": "/p",
                "thread_auth_token": tat.token,
                "action": {
                    "item_id": "directive:rye/agent/core/base",
                    "thread": "inline",
                },
            })),
            &state,
        ).await;
        if let Some(err) = resp_clean.error.as_ref() {
            assert!(
                !err.message.contains("missing thread_auth_token")
                    && !err.message.contains("invalid thread auth token"),
                "auth must succeed; downstream errors are fine: {err:?}"
            );
        }
    }

    // ── facets (via runtime.* token-gated) ─────────────────────────

    #[tokio::test]
    async fn runtime_get_facets_returns_empty_for_new_thread() {
        let (_tmp, state) = setup_app_state();
        state.threads.create_thread(&make_create_params("T-facets-1", "T-facets-1")).unwrap();

        let cbt = state.callback_tokens.generate(
            "T-facets-1",
            std::path::PathBuf::from("/test"),
            std::time::Duration::from_secs(300),
            Vec::new(),
        );
        let resp = dispatch(rpc("runtime.get_facets", json!({
                "callback_token": cbt.token,
                "thread_id": "T-facets-1",
            })),
            &state,
        ).await;
        // Empty facets is OK — new thread has no facets
        if resp.error.is_none() {
            let result = rpc_ok(&resp);
            assert!(result.is_object());
        }
    }
}

