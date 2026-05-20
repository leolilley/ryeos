use serde_json::{json, Value};

use ryeos_runtime::callback_client::CallbackClient;

use crate::context::ExecutionContext;

#[tracing::instrument(
    name = "tool:execute",
    skip(client, action, exec_ctx),
    fields(
        thread_id = %thread_id,
        tool_name = tracing::field::Empty,
    )
)]
pub async fn dispatch_action(
    client: &CallbackClient,
    action: &Value,
    thread_id: &str,
    project_path: &str,
    exec_ctx: Option<&ExecutionContext>,
) -> anyhow::Result<Value> {
    let mut action = action.clone();

    if let Some(ctx) = exec_ctx {
        let item_id = action.get("item_id").and_then(|v| v.as_str()).unwrap_or("");
        let thread = action.get("thread").and_then(|v| v.as_str()).unwrap_or("inline");
        // Inject parent context for child-spawning executes only
        if item_id.starts_with("directive:") || item_id.starts_with("graph:") || thread != "inline" {
            inject_parent_context(&mut action, ctx);
        }
    }

    let item_id = action.get("item_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    tracing::Span::current().record("tool_name", item_id);
    let params = action.get("params").cloned().unwrap_or(json!({}));
    let thread = action.get("thread").and_then(|v| v.as_str()).unwrap_or("inline");

    let request = ryeos_runtime::callback::DispatchActionRequest {
        thread_id: thread_id.to_string(),
        project_path: project_path.to_string(),
        action: ryeos_runtime::callback::ActionPayload {
            item_id: item_id.to_string(),
            params,
            thread: thread.to_string(),
        },
    };

    let response = client.dispatch_action(request).await
        .map_err(|e| anyhow::anyhow!("dispatch failed: {e}"))?;

    // The typed callback contract puts the leaf-dispatcher value in
    // `response.result`; the wrapping `thread` snapshot is for audit
    // only and never feeds into graph-walker control flow. Pass JUST
    // the leaf value into continuation chasing — `continuation_id`
    // (when present) lives at the leaf result's top level under the
    // typed contract.
    let leaf = unwrap_execute_envelope(response.result);
    let followed = follow_continuation(client, &leaf, thread_id, project_path, 0).await?;
    Ok(followed)
}

/// Peel the daemon's audit envelope so graph `assign` templates see the
/// bare tool output, not daemon plumbing.
///
/// The subprocess terminator (`ryeosd::dispatch::dispatch_subprocess`)
/// wraps tool stdout in `ExecuteResponseResult { outcome_code, result,
/// error, artifacts }`. The native-runtime terminator wraps with
/// `{ success, status, result, outputs, warnings }`. Both are daemon-
/// internal accounting; the graph user wants `${result.msg}` to access
/// the tool's actual JSON output, not `${result.result.msg}`.
///
/// Detection is structural: an Object containing `result` plus EITHER
/// the subprocess audit fields (`outcome_code`/`error`/`artifacts`) OR
/// the native-runtime envelope fields (`success`/`status`/`outputs`).
/// A bare tool that happens to print `{"result": ...}` without those
/// sibling fields is left alone — we don't unwrap on `result` alone.
///
/// `continuation_id` lives at the leaf's top level under the typed
/// callback contract, so the unwrap MUST happen before continuation
/// chasing reads it.
fn unwrap_execute_envelope(value: Value) -> Value {
    let Some(obj) = value.as_object() else {
        return value;
    };
    let has_result = obj.contains_key("result");
    if !has_result {
        return value;
    }
    let is_subprocess_envelope = obj.contains_key("outcome_code")
        || obj.contains_key("error")
        || obj.contains_key("artifacts");
    let is_native_runtime_envelope = obj.contains_key("success")
        && obj.contains_key("status")
        && (obj.contains_key("outputs") || obj.contains_key("warnings"));
    if is_subprocess_envelope || is_native_runtime_envelope {
        return obj.get("result").cloned().unwrap_or(Value::Null);
    }
    value
}

fn inject_parent_context(action: &mut Value, ctx: &ExecutionContext) {
    let Some(map) = action.as_object_mut() else { return; };

    // Ensure params exists as an object
    if !map.contains_key("params") || !map["params"].is_object() {
        map.insert("params".into(), json!({}));
    }
    let params = map.get_mut("params")
        .and_then(Value::as_object_mut)
        .unwrap();

    if let Some(ref parent_id) = ctx.parent_thread_id {
        params.entry("parent_thread_id").or_insert(json!(parent_id));
    }
    if !ctx.limits.is_null() {
        params.entry("parent_limits").or_insert(ctx.limits.clone());
    }
    params.entry("depth").or_insert(json!(ctx.depth + 1));
}

#[allow(clippy::only_used_in_recursion)]
fn follow_continuation<'a>(
    client: &'a CallbackClient,
    result: &'a Value,
    thread_id: &'a str,
    project_path: &'a str,
    depth: u32,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<Value>> + Send + 'a>> {
    Box::pin(async move {
        if depth >= 20 {
            return Ok(result.clone());
        }

        // Typed callback contract: continuation IDs live at the leaf
        // result's top level. The legacy `.data.continuation_id`
        // sidechannel is gone — there is one source of truth here.
        let continuation_id = result
            .get("continuation_id")
            .and_then(|v| v.as_str());

        let Some(cont_id) = continuation_id else {
            return Ok(result.clone());
        };

        let thread_result = client.get_thread_by_id(cont_id).await
            .map_err(|e| anyhow::anyhow!("continuation thread lookup failed: {e}"))?;

        let thread_status = thread_result
            .get("thread")
            .and_then(|t| t.get("status"))
            .and_then(|s| s.as_str())
            .unwrap_or("unknown");

        if thread_status == "continued" {
            // No silent fallback to the legacy `continuation.successor_thread_id`
            // sidechannel: `runtime.get_thread` already returns a stable
            // `{ thread, result, artifacts, facets }` shape and continued
            // threads MUST advertise their successor under
            // `thread.successor_thread_id`. A missing field is a daemon
            // contract violation, not a soft case.
            let successor_id = thread_result
                .get("thread")
                .and_then(|t| t.get("successor_thread_id"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!(
                    "continued thread {cont_id} missing thread.successor_thread_id \
                     in get_thread response — daemon contract violation"
                ))?;

            // Recurse with a leaf-shaped value: continuation IDs live
            // at the leaf's top level under the typed callback contract.
            let inner = json!({"continuation_id": successor_id});
            return follow_continuation(client, &inner, thread_id, project_path, depth + 1).await;
        }

        // Terminal: return the leaf value directly. No fallback to
        // returning the whole `{thread, result, ...}` wrapper —
        // `runtime.get_thread` always carries `result` for non-continued
        // threads, and a missing field is a daemon contract violation.
        let terminal_result = thread_result
            .get("result")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!(
                "thread {cont_id} status={thread_status:?} missing top-level \
                 `result` field in get_thread response — daemon contract violation"
            ))?;

        Ok(terminal_result)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_runtime::callback::{CallbackError, DispatchActionRequest};
    use std::sync::{Arc, Mutex};

    fn make_mock_client(results: Vec<Value>) -> CallbackClient {
        let inner: Arc<dyn ryeos_runtime::callback::RuntimeCallbackAPI> = Arc::new(MockClient::new(results));
        CallbackClient::from_inner(inner, "T-test", "/project", "tat-test")
    }

    struct MockClient {
        results: Mutex<Vec<Value>>,
    }

    impl MockClient {
        fn new(results: Vec<Value>) -> Self {
            Self { results: Mutex::new(results) }
        }
    }

    #[async_trait::async_trait]
    impl ryeos_runtime::callback::RuntimeCallbackAPI for MockClient {
        async fn dispatch_action(&self, _request: DispatchActionRequest) -> Result<Value, CallbackError> {
            let mut results = self.results.lock().unwrap();
            // Strict typed contract: wrap leaf in `{thread, result}`.
            if results.is_empty() {
                Ok(json!({"thread": {}, "result": {}}))
            } else {
                Ok(json!({"thread": {}, "result": results.remove(0)}))
            }
        }
        async fn attach_process(&self, _: &str, _: u32) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn mark_running(&self, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn finalize_thread(&self, _: &str, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn get_thread(&self, id: &str) -> Result<Value, CallbackError> {
            Ok(json!({"thread": {"status": "continued", "successor_thread_id": "cont-next", "id": id}}))
        }
        async fn request_continuation(&self, _: &str, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn append_event(&self, _: &str, _: &str, _: Value, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn append_events(&self, _: &str, _: Vec<Value>) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn replay_events(&self, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn claim_commands(&self, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn complete_command(&self, _: &str, _: &str, _: Value) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn publish_artifact(&self, _: &str, _: Value) -> Result<Value, CallbackError> { Ok(json!({})) }
        async fn get_facets(&self, _: &str) -> Result<Value, CallbackError> { Ok(json!({})) }
    }

    #[test]
    fn inject_parent_context_into_params() {
        let mut action = json!({"item_id": "directive:test", "params": {}});
        let ctx = ExecutionContext {
            parent_thread_id: Some("T-parent".to_string()),
            limits: json!({"turns": 10}),
            depth: 2,
        };
        inject_parent_context(&mut action, &ctx);
        assert_eq!(action["params"]["parent_thread_id"], "T-parent");
        assert_eq!(action["params"]["depth"], 3);
        assert_eq!(action["params"]["parent_limits"]["turns"], 10);
    }

    #[tokio::test]
    async fn follow_continuation_respects_max_depth() {
        // Leaf-shaped continuation: typed contract puts continuation_id
        // at the leaf's top level, and follow_continuation recurses on
        // leaves. The mock get_thread always returns "continued" so the
        // chain runs to depth 20 and then returns the leaf as-is.
        let client = make_mock_client(vec![json!({"continuation_id": "cont-1"})]);
        let action = json!({"item_id": "tool:test/deep"});
        let result = dispatch_action(&client, &action, "t-1", "/tmp/test", None).await.unwrap();
        assert!(
            result.get("continuation_id").and_then(|v| v.as_str()).is_some(),
            "expected leaf continuation_id at top level after max-depth abort, got: {result}"
        );
    }

    // ── unwrap_execute_envelope ────────────────────────────────────────

    #[test]
    fn unwrap_subprocess_envelope_exposes_inner_result() {
        // dispatch_subprocess returns ExecuteResponseResult-shaped value:
        // `{outcome_code, result, error, artifacts}`. Graph templates
        // expect `${result.msg}` to access the bare tool output.
        let envelope = json!({
            "outcome_code": "completed",
            "result": {"msg": "hello"},
            "error": null,
            "artifacts": []
        });
        let unwrapped = unwrap_execute_envelope(envelope);
        assert_eq!(unwrapped, json!({"msg": "hello"}));
    }

    #[test]
    fn unwrap_native_runtime_envelope_exposes_inner_result() {
        // dispatch_managed_subprocess wraps RuntimeResult fields:
        // `{success, status, result, outputs, warnings}`. Same unwrap
        // applies for graph→graph or graph→directive dispatches.
        let envelope = json!({
            "success": true,
            "status": "completed",
            "result": {"state": {"x": 1}},
            "outputs": null,
            "warnings": []
        });
        let unwrapped = unwrap_execute_envelope(envelope);
        assert_eq!(unwrapped, json!({"state": {"x": 1}}));
    }

    #[test]
    fn unwrap_leaves_bare_tool_output_alone() {
        // A tool that prints `{"msg": "hello"}` directly (no envelope)
        // must NOT be unwrapped — there's no `result` key to peel.
        let bare = json!({"msg": "hello"});
        assert_eq!(unwrap_execute_envelope(bare.clone()), bare);
    }

    #[test]
    fn unwrap_leaves_continuation_id_alone() {
        // Leaf-shaped continuation values (from sub-thread dispatches)
        // carry `continuation_id` at the top level. They have no audit
        // fields, so no unwrap.
        let cont = json!({"continuation_id": "cont-abc"});
        assert_eq!(unwrap_execute_envelope(cont.clone()), cont);
    }

    #[test]
    fn unwrap_leaves_innocent_result_key_alone() {
        // A tool that legitimately prints `{"result": ...}` without any
        // audit/envelope sibling fields must NOT be unwrapped.
        let bare = json!({"result": "not an envelope"});
        assert_eq!(unwrap_execute_envelope(bare.clone()), bare);
    }

    #[test]
    fn unwrap_handles_non_object_values() {
        assert_eq!(unwrap_execute_envelope(json!(null)), json!(null));
        assert_eq!(unwrap_execute_envelope(json!("string")), json!("string"));
        assert_eq!(unwrap_execute_envelope(json!([1, 2, 3])), json!([1, 2, 3]));
    }

    #[test]
    fn unwrap_handles_subprocess_envelope_with_null_inner_result() {
        // Tool dispatched but no stdout produced — `result` is Null
        // inside the envelope. Unwrap returns Null, not the wrapper.
        let envelope = json!({
            "outcome_code": "completed",
            "result": null,
            "error": null,
            "artifacts": []
        });
        assert_eq!(unwrap_execute_envelope(envelope), json!(null));
    }
}
