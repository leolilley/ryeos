use serde_json::{json, Value};

use ryeos_runtime::callback_client::CallbackClient;
use ryeos_runtime::envelope::RuntimeCost;

use crate::context::ExecutionContext;

/// Outcome of dispatching a single graph action leaf, classified from
/// the daemon execute envelope BEFORE the bare result is unwrapped.
///
/// The daemon wraps tool output in an audit envelope — `{outcome_code,
/// result, error, artifacts}` for subprocess leaves, `{success, status,
/// result, outputs, warnings}` for native-runtime leaves. A *failed*
/// leaf carries `result: null` with the diagnostic in `error`/`status`,
/// so unconditionally peeling to the bare `result` would turn a failure
/// into a silent `null` success that then poisons graph state via
/// suppressed interpolation errors. Classification happens once, here,
/// so a failing tool surfaces as a node error with an actionable
/// diagnostic instead.
#[derive(Debug)]
pub enum ActionOutcome {
    /// Leaf succeeded; carries the unwrapped result plus optional
    /// accounting metadata parsed from the envelope.
    Success(ActionSuccess),
    /// Leaf ran but reported failure (non-zero exit, runtime
    /// `success:false`, timeout). Carries a human-readable diagnostic and
    /// any cost the child reported before failing.
    Failure(ActionFailure),
}

/// A failed leaf dispatch: the diagnostic plus any cost the child spent
/// before failing. A failed LLM directive can burn tokens and still
/// return `success:false` with a non-null `cost`, so accounting must not
/// drop it.
#[derive(Debug)]
pub struct ActionFailure {
    /// Human-readable diagnostic including exit/status and a stderr
    /// excerpt where available.
    pub diagnostic: String,
    /// Cost reported by a native child before it failed. `None` for
    /// subprocess failures and transport failures (no child cost exists).
    pub cost: Option<RuntimeCost>,
}

/// A successful leaf dispatch: the graph-visible result plus optional
/// cost reported by a native child runtime.
#[derive(Debug)]
pub struct ActionSuccess {
    /// Bare, envelope-unwrapped result for `${result.*}` interpolation.
    ///
    /// For a native directive return carrying declared `outputs`, this is
    /// `{result: <inner>, outputs: <outputs>}` so a graph can reach the
    /// directive's structured outputs as `${result.outputs.X}`. The inner
    /// `result` of a directive return is the synthetic sentinel
    /// `"directive_return"` — not meaningful graph data — so the outputs
    /// are the payload. For every other leaf (subprocess, bare value,
    /// native return with no outputs) this is the bare inner result and
    /// the shape is unchanged.
    pub result: Value,
    /// Token/spend cost reported by a native child runtime (directive or
    /// sub-graph) in the envelope's `cost` field. `None` for subprocess
    /// leaves, cache hits, and bare values — cost is never invented.
    pub cost: Option<RuntimeCost>,
}

impl ActionSuccess {
    /// A success with no accounting metadata — subprocess leaves, bare
    /// tool output, and cache hits. A cache hit replays a stored result
    /// and must NOT re-bill cost, so the walker rebuilds the outcome with
    /// this constructor.
    pub fn bare(result: Value) -> Self {
        Self { result, cost: None }
    }
}

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
) -> anyhow::Result<ActionOutcome> {
    let mut action = action.clone();

    if let Some(ctx) = exec_ctx {
        let item_id = action.get("item_id").and_then(|v| v.as_str()).unwrap_or("");
        let thread = action
            .get("thread")
            .and_then(|v| v.as_str())
            .unwrap_or("inline");
        // Inject parent context for child-spawning executes only
        if item_id.starts_with("directive:") || item_id.starts_with("graph:") || thread != "inline"
        {
            inject_parent_context(&mut action, ctx);
        }
    }

    let item_id = action.get("item_id").and_then(|v| v.as_str()).unwrap_or("");
    tracing::Span::current().record("tool_name", item_id);
    let params = action.get("params").cloned().unwrap_or(json!({}));
    let thread = action
        .get("thread")
        .and_then(|v| v.as_str())
        .unwrap_or("inline");

    // Optional method selector. The node's `call: { method, args }` block
    // (already `${…}`-interpolated by the walker) maps onto the daemon's
    // method dispatch. Absent (or explicit `null`, for parity with how
    // `/execute` deserializes `Option<MethodCall>`) → the leaf takes the
    // kind's default method. A malformed `call` is a node authoring error,
    // surfaced loudly.
    let call = match action.get("call") {
        None => None,
        Some(v) if v.is_null() => None,
        Some(call_val) => Some(
            serde_json::from_value::<ryeos_runtime::callback::MethodCall>(call_val.clone())
                .map_err(|e| anyhow::anyhow!("invalid `call` block for `{item_id}`: {e}"))?,
        ),
    };

    let request = ryeos_runtime::callback::DispatchActionRequest {
        thread_id: thread_id.to_string(),
        project_path: project_path.to_string(),
        action: ryeos_runtime::callback::ActionPayload {
            item_id: item_id.to_string(),
            params,
            thread: thread.to_string(),
            call,
        },
    };

    let response = client
        .dispatch_action(request)
        .await
        .map_err(|e| anyhow::anyhow!("dispatch failed: {e}"))?;

    // The typed callback contract puts the leaf-dispatcher value in
    // `response.result`; the wrapping `thread` snapshot is for audit
    // only and never feeds into graph-walker control flow. Classify the
    // envelope BEFORE unwrapping so a failed leaf becomes a structured
    // failure rather than a silent `null`. Only success peels to the
    // bare leaf value and chases continuations — `continuation_id`
    // (when present) lives at the leaf result's top level.
    match classify_envelope(response.result) {
        ActionOutcome::Failure(failure) => Ok(ActionOutcome::Failure(failure)),
        ActionOutcome::Success(mut success) => {
            // Continuation chasing operates on the leaf result only;
            // cost parsed from the immediate envelope rides along
            // unchanged. (Per the cost-accounting contract, continuation/
            // async child cost is not chased — only the immediate native
            // child envelope's cost is trusted.)
            //
            // CONTRACT: a native return carrying meaningful `outputs` is
            // wrapped to `{result, outputs}`, which would hide a top-level
            // `continuation_id` from `follow_continuation`. This is safe
            // only because a directive return with declared outputs is
            // terminal (it never also requests continuation). Subprocess
            // and bare leaves are not wrapped, so their `continuation_id`
            // stays visible.
            let followed =
                follow_continuation(client, &success.result, thread_id, project_path, 0).await?;
            success.result = followed;
            Ok(ActionOutcome::Success(success))
        }
    }
}

/// Classify a daemon execute envelope into success (bare unwrapped
/// result) or failure (diagnostic), peeling the audit wrapper only on
/// success.
///
/// The subprocess terminator (`ryeosd::dispatch::dispatch_subprocess`)
/// wraps tool stdout in `ExecuteResponseResult { outcome_code, result,
/// error, artifacts }`. The native-runtime terminator wraps with
/// `{ success, status, result, outputs, warnings }`. Both are daemon-
/// internal accounting; on success the graph user wants `${result.msg}`
/// to access the tool's actual JSON output, not `${result.result.msg}`.
///
/// Detection of the subprocess envelope keys ONLY off `outcome_code`
/// (always set by the terminator). `error`/`artifacts` are not used as
/// discriminators — a bare tool returning `{"result": ..., "error":
/// null}` must not be mistaken for an envelope. A bare tool that prints
/// `{"result": ...}` with no envelope markers is left alone.
///
/// `continuation_id` lives at the leaf's top level under the typed
/// callback contract, so classification MUST happen before continuation
/// chasing reads it.
fn classify_envelope(value: Value) -> ActionOutcome {
    let Some(obj) = value.as_object() else {
        return ActionOutcome::Success(ActionSuccess::bare(value));
    };
    if !obj.contains_key("result") {
        // No `result` key — not an envelope; bare leaf data.
        return ActionOutcome::Success(ActionSuccess::bare(value));
    }

    // Native-runtime envelope: `{success, status, result, outputs|warnings}`.
    let is_native = obj.contains_key("success")
        && obj.contains_key("status")
        && (obj.contains_key("outputs") || obj.contains_key("warnings"));
    if is_native {
        let ok = obj.get("success").and_then(Value::as_bool).unwrap_or(false);
        return if ok {
            ActionOutcome::Success(ActionSuccess {
                result: native_success_value(obj),
                cost: parse_native_cost(obj),
            })
        } else {
            // A failed native child (e.g. a directive that burned tokens
            // then errored) still reports `cost` — preserve it.
            ActionOutcome::Failure(ActionFailure {
                diagnostic: describe_native_failure(obj),
                cost: parse_native_cost(obj),
            })
        };
    }

    // Subprocess envelope: discriminated by `outcome_code`.
    if obj.contains_key("outcome_code") {
        return if subprocess_succeeded(obj) {
            ActionOutcome::Success(ActionSuccess::bare(inner_result(obj)))
        } else {
            // Subprocess leaves carry no stable cost field.
            ActionOutcome::Failure(ActionFailure {
                diagnostic: describe_subprocess_failure(obj),
                cost: None,
            })
        };
    }

    // Has `result` but no envelope markers — bare tool data.
    ActionOutcome::Success(ActionSuccess::bare(value))
}

fn inner_result(obj: &serde_json::Map<String, Value>) -> Value {
    obj.get("result").cloned().unwrap_or(Value::Null)
}

/// Graph-visible success value for a native-runtime envelope.
///
/// When the child declared structured `outputs` (a directive return), wrap
/// as `{result: <inner>, outputs: <outputs>}` so the graph can read
/// `${result.outputs.X}`. When there are no meaningful outputs (a native
/// return, a sub-graph result, or a directive with no declared outputs —
/// which emits `outputs: {}`), return the bare inner result so existing
/// `${result.state}` / `${result.foo}` call sites keep working unchanged.
fn native_success_value(obj: &serde_json::Map<String, Value>) -> Value {
    let inner = inner_result(obj);
    match obj.get("outputs").filter(|v| has_meaningful_outputs(v)) {
        Some(outputs) => json!({ "result": inner, "outputs": outputs.clone() }),
        None => inner,
    }
}

/// Whether a native envelope's `outputs` carries declared data. A
/// directive with no declared outputs emits `outputs: {}`; treating that
/// (and `null`) as absent keeps the bare-result shape for the common case
/// so `${result.foo}` does not silently become `${result.result.foo}`.
fn has_meaningful_outputs(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Object(map) => !map.is_empty(),
        _ => true,
    }
}

/// Parse the optional `cost` field of a native envelope into a typed
/// `RuntimeCost`. A missing or null `cost` yields `None` — cost is never
/// invented for a child that did not report it. A present-but-malformed
/// `cost` (contract drift between the child runtime and the cost schema)
/// is logged loudly rather than silently dropped, so under-accounting is
/// visible to operators.
fn parse_native_cost(obj: &serde_json::Map<String, Value>) -> Option<RuntimeCost> {
    let raw = obj.get("cost").filter(|v| !v.is_null())?;
    match serde_json::from_value(raw.clone()) {
        Ok(cost) => Some(cost),
        Err(e) => {
            tracing::warn!(
                error = %e,
                cost = %raw,
                "native child reported a malformed `cost`; dropping it from graph \
                 accounting (cost-schema contract drift)"
            );
            None
        }
    }
}

/// A subprocess leaf failed iff the envelope carries a non-null `error`
/// payload — the terminator populates it (with exit_code/stderr/stdout,
/// or a timeout message) for any non-zero exit, timeout, or dispatch
/// failure. A clean completion leaves `error` null AND nulls
/// `outcome_code` (the daemon nulls `outcome_code` for a completed
/// thread), so `error` — not `outcome_code` — is the success signal.
fn subprocess_succeeded(obj: &serde_json::Map<String, Value>) -> bool {
    obj.get("error").map(Value::is_null).unwrap_or(true)
}

fn describe_subprocess_failure(obj: &serde_json::Map<String, Value>) -> String {
    let code = obj
        .get("outcome_code")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let err = obj.get("error");
    let exit_code = err.and_then(|e| e.get("exit_code")).and_then(Value::as_i64);
    let stderr = err
        .and_then(|e| e.get("stderr"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let stdout = err
        .and_then(|e| e.get("stdout"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let mut msg = format!("tool failed (outcome_code: {code}");
    if let Some(ec) = exit_code {
        msg.push_str(&format!(", exit_code: {ec}"));
    }
    msg.push(')');
    if let Some(se) = stderr {
        msg.push_str(&format!("; stderr: {}", excerpt(se, 800)));
    } else if let Some(so) = stdout {
        msg.push_str(&format!("; stdout: {}", excerpt(so, 800)));
    }
    msg
}

fn describe_native_failure(obj: &serde_json::Map<String, Value>) -> String {
    let status = obj
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("failed");
    let mut msg = format!("child runtime failed (status: {status})");
    if let Some(detail) = native_failure_detail(obj) {
        msg.push_str(&format!("; {}", excerpt(&detail, 800)));
    }
    msg
}

/// Extract the most actionable failure detail from a native-runtime
/// envelope. Child graph/directive runtimes return a structured result
/// (e.g. `GraphResult { error, ... }`) under `result`, so a bare
/// `status` is rarely enough — prefer the inner `error`, then a string
/// result, then a compact JSON excerpt of the structured result.
fn native_failure_detail(obj: &serde_json::Map<String, Value>) -> Option<String> {
    let non_empty = |s: &str| -> Option<String> {
        let t = s.trim();
        (!t.is_empty()).then(|| t.to_string())
    };

    if let Some(s) = obj.get("error").and_then(Value::as_str).and_then(non_empty) {
        return Some(s);
    }
    let result = obj.get("result")?;
    if let Some(s) = result.as_str().and_then(non_empty) {
        return Some(s);
    }
    if let Some(res_obj) = result.as_object() {
        if let Some(s) = res_obj
            .get("error")
            .and_then(Value::as_str)
            .and_then(non_empty)
        {
            return Some(s);
        }
        // Last resort: a compact JSON excerpt so the diagnostic is not
        // reduced to just `status`.
        return Some(result.to_string());
    }
    None
}

/// Truncate a diagnostic excerpt at a char boundary so it never splits a
/// multi-byte UTF-8 sequence.
fn excerpt(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max).collect();
        format!("{truncated}… [truncated]")
    }
}

fn inject_parent_context(action: &mut Value, ctx: &ExecutionContext) {
    let Some(map) = action.as_object_mut() else {
        return;
    };

    // Ensure params exists as an object
    if !map.contains_key("params") || !map["params"].is_object() {
        map.insert("params".into(), json!({}));
    }
    let params = map
        .get_mut("params")
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

        // Continuation IDs live at the leaf result's top level — one
        // source of truth.
        let continuation_id = result.get("continuation_id").and_then(|v| v.as_str());

        let Some(cont_id) = continuation_id else {
            return Ok(result.clone());
        };

        let thread_result = client
            .get_thread_by_id(cont_id)
            .await
            .map_err(|e| anyhow::anyhow!("continuation thread lookup failed: {e}"))?;

        let thread_status = thread_result
            .get("thread")
            .and_then(|t| t.get("status"))
            .and_then(|s| s.as_str())
            .unwrap_or("unknown");

        if thread_status == "continued" {
            // `runtime.get_thread` returns a stable `{ thread, result,
            // artifacts, facets }` shape; a continued thread MUST
            // advertise its successor under `thread.successor_thread_id`.
            // A missing field is a daemon contract violation, not a soft
            // case.
            let successor_id = thread_result
                .get("thread")
                .and_then(|t| t.get("successor_thread_id"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "continued thread {cont_id} missing thread.successor_thread_id \
                     in get_thread response — daemon contract violation"
                    )
                })?;

            // Recurse with a leaf-shaped value: continuation IDs live
            // at the leaf's top level under the typed callback contract.
            let inner = json!({"continuation_id": successor_id});
            return follow_continuation(client, &inner, thread_id, project_path, depth + 1).await;
        }

        // Terminal: return the leaf value directly. `runtime.get_thread`
        // always carries `result` for non-continued threads; a missing
        // field is a daemon contract violation.
        let terminal_result = thread_result.get("result").cloned().ok_or_else(|| {
            anyhow::anyhow!(
                "thread {cont_id} status={thread_status:?} missing top-level \
                 `result` field in get_thread response — daemon contract violation"
            )
        })?;

        Ok(terminal_result)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_runtime::callback::{CallbackError, DispatchActionRequest};
    use std::sync::{Arc, Mutex};

    fn make_mock_client(results: Vec<Value>) -> CallbackClient {
        let inner: Arc<dyn ryeos_runtime::callback::RuntimeCallbackAPI> =
            Arc::new(MockClient::new(results));
        CallbackClient::from_inner(inner, "T-test", "/project", "tat-test")
    }

    struct MockClient {
        results: Mutex<Vec<Value>>,
    }

    impl MockClient {
        fn new(results: Vec<Value>) -> Self {
            Self {
                results: Mutex::new(results),
            }
        }
    }

    #[async_trait::async_trait]
    impl ryeos_runtime::callback::RuntimeCallbackAPI for MockClient {
        async fn dispatch_action(
            &self,
            _request: DispatchActionRequest,
        ) -> Result<Value, CallbackError> {
            let mut results = self.results.lock().unwrap();
            // Strict typed contract: wrap leaf in `{thread, result}`.
            if results.is_empty() {
                Ok(json!({"thread": {}, "result": {}}))
            } else {
                Ok(json!({"thread": {}, "result": results.remove(0)}))
            }
        }
        async fn attach_process(&self, _: &str, _: u32) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn mark_running(&self, _: &str) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn finalize_thread(
            &self,
            _: &str,
            _: ryeos_runtime::TerminalCompletion,
        ) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn get_thread(&self, id: &str) -> Result<Value, CallbackError> {
            Ok(
                json!({"thread": {"status": "continued", "successor_thread_id": "cont-next", "id": id}}),
            )
        }
        async fn request_continuation(&self, _: &str, _: &str) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn append_event(
            &self,
            _: &str,
            _: &str,
            _: Value,
            _: &str,
        ) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn append_events(&self, _: &str, _: Vec<Value>) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn replay_events(&self, _: &str) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn bundle_events_append(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn bundle_events_read_chain(
            &self,
            _: &str,
            _: Value,
        ) -> Result<Value, CallbackError> {
            Ok(json!({"events": []}))
        }
        async fn bundle_events_scan(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({"events": []}))
        }
        async fn vault_put(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn vault_get(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn vault_delete(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn vault_list(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({"keys": []}))
        }
        async fn claim_commands(&self, _: &str) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn complete_command(
            &self,
            _: &str,
            _: &str,
            _: Value,
        ) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn publish_artifact(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn get_facets(&self, _: &str) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
    }

    /// Mock that records the `action` of the last dispatch so a test can
    /// assert what the graph forwarded across the callback wire.
    struct CapturingClient {
        last: Arc<Mutex<Option<ryeos_runtime::callback::ActionPayload>>>,
    }

    #[async_trait::async_trait]
    impl ryeos_runtime::callback::RuntimeCallbackAPI for CapturingClient {
        async fn dispatch_action(
            &self,
            request: DispatchActionRequest,
        ) -> Result<Value, CallbackError> {
            *self.last.lock().unwrap() = Some(request.action);
            Ok(json!({"thread": {}, "result": {}}))
        }
        async fn attach_process(&self, _: &str, _: u32) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn mark_running(&self, _: &str) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn finalize_thread(
            &self,
            _: &str,
            _: ryeos_runtime::TerminalCompletion,
        ) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn get_thread(&self, _: &str) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn request_continuation(&self, _: &str, _: &str) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn append_event(
            &self,
            _: &str,
            _: &str,
            _: Value,
            _: &str,
        ) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn append_events(&self, _: &str, _: Vec<Value>) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn replay_events(&self, _: &str) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn bundle_events_append(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn bundle_events_read_chain(
            &self,
            _: &str,
            _: Value,
        ) -> Result<Value, CallbackError> {
            Ok(json!({"events": []}))
        }
        async fn bundle_events_scan(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({"events": []}))
        }
        async fn vault_put(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn vault_get(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn vault_delete(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn vault_list(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({"keys": []}))
        }
        async fn claim_commands(&self, _: &str) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn complete_command(
            &self,
            _: &str,
            _: &str,
            _: Value,
        ) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn publish_artifact(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
        async fn get_facets(&self, _: &str) -> Result<Value, CallbackError> {
            Ok(json!({}))
        }
    }

    #[tokio::test]
    async fn forwards_call_block_to_callback() {
        let last = Arc::new(Mutex::new(None));
        let inner: Arc<dyn ryeos_runtime::callback::RuntimeCallbackAPI> = Arc::new(CapturingClient {
            last: last.clone(),
        });
        let client = CallbackClient::from_inner(inner, "T-test", "/project", "tat-test");

        let action = json!({
            "item_id": "knowledge:arc/resources",
            "params": {},
            "call": { "method": "query", "args": { "query": "hint", "limit": 5 } },
        });
        dispatch_action(&client, &action, "T-test", "/project", None)
            .await
            .expect("dispatch ok");

        let forwarded = last.lock().unwrap().take().expect("action captured");
        let call = forwarded.call.expect("call forwarded");
        assert_eq!(call.method(), Some("query"));
        assert_eq!(call.args().unwrap()["limit"], 5);
    }

    #[tokio::test]
    async fn omits_call_block_when_absent() {
        let last = Arc::new(Mutex::new(None));
        let inner: Arc<dyn ryeos_runtime::callback::RuntimeCallbackAPI> = Arc::new(CapturingClient {
            last: last.clone(),
        });
        let client = CallbackClient::from_inner(inner, "T-test", "/project", "tat-test");

        let action = json!({ "item_id": "tool:t/echo", "params": {} });
        dispatch_action(&client, &action, "T-test", "/project", None)
            .await
            .expect("dispatch ok");

        let forwarded = last.lock().unwrap().take().expect("action captured");
        assert!(forwarded.call.is_none(), "no call block → None");
    }

    #[tokio::test]
    async fn null_call_block_treated_as_absent() {
        let last = Arc::new(Mutex::new(None));
        let inner: Arc<dyn ryeos_runtime::callback::RuntimeCallbackAPI> = Arc::new(CapturingClient {
            last: last.clone(),
        });
        let client = CallbackClient::from_inner(inner, "T-test", "/project", "tat-test");

        // Parity with `/execute`'s `Option<MethodCall>`: explicit null == absent.
        let action = json!({ "item_id": "tool:t/echo", "params": {}, "call": null });
        dispatch_action(&client, &action, "T-test", "/project", None)
            .await
            .expect("dispatch ok");

        let forwarded = last.lock().unwrap().take().expect("action captured");
        assert!(forwarded.call.is_none(), "call: null → None");
    }

    #[tokio::test]
    async fn malformed_call_block_fails_loudly() {
        let client = make_mock_client(vec![]);
        let action = json!({
            "item_id": "knowledge:arc/resources",
            "params": {},
            "call": { "op": "query" }, // unknown field — deny_unknown_fields
        });
        let err = dispatch_action(&client, &action, "T-test", "/project", None)
            .await
            .expect_err("malformed call must fail");
        assert!(
            err.to_string().contains("invalid `call` block"),
            "got: {err}"
        );
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
        let outcome = dispatch_action(&client, &action, "t-1", "/tmp/test", None)
            .await
            .unwrap();
        let result = expect_success(outcome);
        assert!(
            result
                .get("continuation_id")
                .and_then(|v| v.as_str())
                .is_some(),
            "expected leaf continuation_id at top level after max-depth abort, got: {result}"
        );
    }

    // ── classify_envelope ──────────────────────────────────────────────

    fn expect_success(outcome: ActionOutcome) -> Value {
        expect_action_success(outcome).result
    }

    fn expect_action_success(outcome: ActionOutcome) -> ActionSuccess {
        match outcome {
            ActionOutcome::Success(s) => s,
            ActionOutcome::Failure(f) => panic!("expected Success, got Failure: {}", f.diagnostic),
        }
    }

    fn expect_action_failure(outcome: ActionOutcome) -> ActionFailure {
        match outcome {
            ActionOutcome::Failure(f) => f,
            ActionOutcome::Success(s) => panic!("expected Failure, got Success: {:?}", s.result),
        }
    }

    fn classify_success(value: Value) -> Value {
        expect_success(classify_envelope(value))
    }

    fn classify_failure(value: Value) -> String {
        expect_action_failure(classify_envelope(value)).diagnostic
    }

    #[test]
    fn classify_subprocess_success_exposes_inner_result() {
        // A clean subprocess exit (`outcome_code: exit:0`) peels to the
        // bare tool output so `${result.msg}` works.
        let envelope = json!({
            "outcome_code": "exit:0",
            "result": {"msg": "hello"},
            "error": null,
            "artifacts": []
        });
        assert_eq!(classify_success(envelope), json!({"msg": "hello"}));
    }

    #[test]
    fn classify_native_runtime_success_exposes_inner_result() {
        // `{success, status, result, outputs, warnings}` — graph→graph or
        // graph→directive dispatch. Success peels to the inner result.
        let envelope = json!({
            "success": true,
            "status": "completed",
            "result": {"state": {"x": 1}},
            "outputs": null,
            "warnings": []
        });
        assert_eq!(classify_success(envelope), json!({"state": {"x": 1}}));
    }

    #[test]
    fn classify_native_runtime_success_with_outputs_exposes_outputs() {
        // A directive return: inner `result` is the synthetic sentinel and
        // the payload is in `outputs`. The graph-visible value must wrap
        // both so `${result.outputs.recommendations}` resolves to an array.
        let envelope = json!({
            "success": true,
            "status": "completed",
            "result": "directive_return",
            "outputs": {"recommendations": ["a", "b"], "abstractions": {"k": 1}},
            "warnings": []
        });
        assert_eq!(
            classify_success(envelope),
            json!({
                "result": "directive_return",
                "outputs": {"recommendations": ["a", "b"], "abstractions": {"k": 1}}
            })
        );
    }

    #[test]
    fn classify_native_runtime_success_without_outputs_preserves_inner_result() {
        // No `outputs` (null) → bare inner result, unchanged shape, so
        // existing `${result.state}` graph→graph call sites keep working.
        let envelope = json!({
            "success": true,
            "status": "completed",
            "result": {"state": {"x": 1}},
            "outputs": null,
            "warnings": []
        });
        assert_eq!(classify_success(envelope), json!({"state": {"x": 1}}));
    }

    #[test]
    fn classify_native_runtime_parses_cost() {
        // A native child reporting `cost` exposes it as typed RuntimeCost
        // for graph accounting; the result is still the bare inner value.
        let envelope = json!({
            "success": true,
            "status": "completed",
            "result": "directive_return",
            "outputs": {"x": 1},
            "cost": {"input_tokens": 120, "output_tokens": 45, "total_usd": 0.0012},
            "warnings": []
        });
        let success = expect_action_success(classify_envelope(envelope));
        let cost = success.cost.expect("cost should be parsed");
        assert_eq!(cost.input_tokens, 120);
        assert_eq!(cost.output_tokens, 45);
        assert!((cost.total_usd - 0.0012).abs() < f64::EPSILON);
    }

    #[test]
    fn classify_native_runtime_success_without_cost_is_none() {
        let envelope = json!({
            "success": true,
            "status": "completed",
            "result": {"state": {"x": 1}},
            "outputs": null,
            "warnings": []
        });
        assert!(expect_action_success(classify_envelope(envelope))
            .cost
            .is_none());
    }

    #[test]
    fn classify_native_runtime_success_with_empty_outputs_preserves_inner_result() {
        // A directive with NO declared outputs emits `outputs: {}`. That
        // must NOT wrap the result — `${result.foo}` has to keep working,
        // not silently become `${result.result.foo}`.
        let envelope = json!({
            "success": true,
            "status": "completed",
            "result": {"foo": 1},
            "outputs": {},
            "warnings": []
        });
        assert_eq!(classify_success(envelope), json!({"foo": 1}));
    }

    #[test]
    fn classify_native_runtime_failure_preserves_cost() {
        // A failed LLM directive can burn tokens and still return
        // `success:false` with non-null `cost` — accounting must keep it.
        let envelope = json!({
            "success": false,
            "status": "error",
            "result": {"error": "model refused"},
            "outputs": null,
            "cost": {"input_tokens": 80, "output_tokens": 0, "total_usd": 0.0008},
            "warnings": []
        });
        let failure = expect_action_failure(classify_envelope(envelope));
        assert!(failure.diagnostic.contains("error"));
        let cost = failure.cost.expect("failed child cost should be preserved");
        assert_eq!(cost.input_tokens, 80);
    }

    #[test]
    fn classify_subprocess_failure_has_no_cost() {
        let envelope = json!({
            "outcome_code": "exit:1",
            "result": null,
            "error": {"exit_code": 1, "stderr": "boom"},
            "artifacts": []
        });
        assert!(expect_action_failure(classify_envelope(envelope))
            .cost
            .is_none());
    }

    #[test]
    fn classify_subprocess_failure_surfaces_diagnostic_not_null() {
        // P0 regression guard: a non-zero subprocess exit must NOT
        // collapse to a `null` success — it must classify as a failure
        // carrying the exit code and stderr excerpt.
        let envelope = json!({
            "outcome_code": "exit:1",
            "result": null,
            "error": {"exit_code": 1, "stdout": "", "stderr": "Traceback: boom"},
            "artifacts": []
        });
        let diagnostic = classify_failure(envelope);
        assert!(diagnostic.contains("exit:1"), "got: {diagnostic}");
        assert!(diagnostic.contains("boom"), "got: {diagnostic}");
    }

    #[test]
    fn classify_subprocess_failure_with_error_payload_and_zero_code() {
        // A non-null `error` payload marks failure even if outcome_code
        // looks benign.
        let envelope = json!({
            "outcome_code": "exit:0",
            "result": null,
            "error": {"exit_code": 0, "stderr": "late failure"},
            "artifacts": []
        });
        assert!(classify_failure(envelope).contains("late failure"));
    }

    #[test]
    fn classify_native_runtime_failure_surfaces_status() {
        let envelope = json!({
            "success": false,
            "status": "failed",
            "result": null,
            "outputs": null,
            "warnings": []
        });
        assert!(classify_failure(envelope).contains("failed"));
    }

    #[test]
    fn classify_native_runtime_failure_surfaces_structured_child_error() {
        // graph→graph: the child returns a structured GraphResult under
        // `result`; the parent diagnostic must dig out `result.error`
        // rather than collapsing to just the status.
        let envelope = json!({
            "success": false,
            "status": "error",
            "result": {"error": "child graph failed: boom", "status": "error"},
            "outputs": null,
            "warnings": []
        });
        assert!(classify_failure(envelope).contains("boom"));
    }

    #[test]
    fn classify_leaves_bare_tool_output_alone() {
        // A tool that prints `{"msg": "hello"}` directly (no envelope)
        // is a success with no peeling — there's no `result` key.
        let bare = json!({"msg": "hello"});
        assert_eq!(classify_success(bare.clone()), bare);
    }

    #[test]
    fn classify_leaves_continuation_id_alone() {
        let cont = json!({"continuation_id": "cont-abc"});
        assert_eq!(classify_success(cont.clone()), cont);
    }

    #[test]
    fn classify_leaves_innocent_result_key_alone() {
        // A tool that legitimately prints `{"result": ...}` without any
        // envelope marker (no outcome_code, no success/status) is bare
        // data — not unwrapped.
        let bare = json!({"result": "not an envelope"});
        assert_eq!(classify_success(bare.clone()), bare);
    }

    #[test]
    fn classify_does_not_unwrap_on_error_key_alone() {
        // `error` alone is NOT an envelope discriminator — a bare tool
        // returning `{result, error: null}` must pass through untouched.
        let bare = json!({"result": {"v": 1}, "error": null});
        assert_eq!(classify_success(bare.clone()), bare);
    }

    #[test]
    fn classify_handles_non_object_values() {
        assert_eq!(classify_success(json!(null)), json!(null));
        assert_eq!(classify_success(json!("string")), json!("string"));
        assert_eq!(classify_success(json!([1, 2, 3])), json!([1, 2, 3]));
    }

    #[test]
    fn classify_subprocess_success_with_null_inner_result() {
        // Clean exit, no stdout — success carrying a `null` result (the
        // tool genuinely produced nothing), NOT a failure.
        let envelope = json!({
            "outcome_code": "exit:0",
            "result": null,
            "error": null,
            "artifacts": []
        });
        assert_eq!(classify_success(envelope), json!(null));
    }

    #[test]
    fn classify_completed_thread_envelope_is_success() {
        // The real graph→tool callback success shape: a completed thread
        // nulls `outcome_code` (only failures carry exit:<n>/timeout), and
        // `error` is null. This MUST classify as success — `error` is the
        // discriminator, not `outcome_code`. (Regression: requiring
        // `outcome_code == "exit:0"` here broke every graph→tool dispatch.)
        let envelope = json!({
            "outcome_code": null,
            "result": {"ok": true, "n": 7},
            "error": null,
            "artifacts": []
        });
        assert_eq!(classify_success(envelope), json!({"ok": true, "n": 7}));
    }
}
