//! Graph observer hooks, wired to the shared `ryeos-runtime` hook machinery.
//!
//! Authored `config.hooks` are typed [`HookDefinition`]s — the same grammar
//! directives use, one hook vocabulary across runtimes. The walker fires them
//! at graph lifecycle events (`graph_started`, `graph_step_completed`,
//! `graph_completed`); each matching hook's action is dispatched through the
//! SAME callback path a node action uses, so a hook child is a real dispatch:
//! effective_caps enforced at the callback boundary, cost accrued, visible in
//! the braid. Graph hooks are observers — the control result a hook may return
//! is ignored; routing stays the walker's job.

use serde_json::Value;

use ryeos_runtime::callback::{ActionPayload, CallbackError, DispatchActionRequest, MethodCall};
use ryeos_runtime::callback_client::CallbackClient;
use ryeos_runtime::hooks_eval::{run_hooks, HookDispatcher};
use ryeos_runtime::HookDefinition;

/// Fire every hook matching `event` against `context`. Returns `Err` with a
/// diagnostic when a hook's condition, action interpolation, or dispatch fails
/// (including a hook child that ran but reported failure, normalized to
/// `hook_child_failed`) — the caller records it as a warning rather than failing
/// the graph. The hook control result is discarded: graph hooks observe.
pub async fn run_graph_hooks(
    callback: &CallbackClient,
    thread_id: &str,
    project_path: &str,
    hooks: &[HookDefinition],
    event: &str,
    context: &Value,
) -> Result<(), String> {
    if hooks.is_empty() {
        return Ok(());
    }
    let dispatcher = build_dispatcher(callback.clone(), thread_id.to_string());
    run_hooks(event, context, hooks, project_path, &dispatcher)
        .await
        .map(|_control| ())
}

/// Build a [`HookDispatcher`] that routes a hook action through the runtime
/// callback exactly like a node action, then normalizes the leaf envelope so a
/// failed hook child surfaces as a `hook_child_failed` error instead of a silent
/// success.
fn build_dispatcher(callback: CallbackClient, thread_id: String) -> HookDispatcher {
    Box::new(move |action, project_path| {
        let cb = callback.clone();
        let tid = thread_id.clone();
        Box::pin(async move {
            // Build the dispatch payload leniently, exactly as a node action does
            // (`thread` defaults to "inline", `call` is optional) — a hook action
            // rides the identical callback path, not a stricter contract.
            let payload = hook_action_payload(&action)?;
            let response = cb
                .dispatch_action(DispatchActionRequest {
                    thread_id: tid,
                    project_path,
                    action: payload,
                })
                .await
                .map_err(|e| CallbackError::Transport(anyhow::anyhow!("{e}")))?;
            // Hooks run on the leaf result only — the parent-thread snapshot has
            // no bearing on hook control flow.
            normalize_hook_dispatch_result(response.result)
        })
    })
}

/// Parse an interpolated hook action into the dispatch payload the callback
/// expects, mirroring the graph node dispatcher: `item_id`/`params` are read
/// directly, `thread` defaults to `"inline"`, and a malformed `call` block fails
/// loudly rather than silently dropping the method selector.
fn hook_action_payload(action: &Value) -> Result<ActionPayload, CallbackError> {
    let item_id = action
        .get("item_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let params = action.get("params").cloned().unwrap_or(Value::Null);
    let thread = action
        .get("thread")
        .and_then(|v| v.as_str())
        .unwrap_or("inline")
        .to_string();
    let call = match action.get("call") {
        None | Some(Value::Null) => None,
        Some(call_val) => Some(
            serde_json::from_value::<MethodCall>(call_val.clone()).map_err(|e| {
                CallbackError::Transport(anyhow::anyhow!("invalid hook `call` block: {e}"))
            })?,
        ),
    };
    Ok(ActionPayload {
        item_id,
        params,
        thread,
        call,
        // Hooks dispatch inline (observers); no detached-child facets or
        // launch window.
        facets: None,
        launch_window: None,
    })
}

/// Normalize a hook child's dispatch envelope: a native-runtime or managed
/// envelope reporting failure becomes a `hook_child_failed` error (so a failing
/// hook is loud, not a silent success); a successful envelope peels to its inner
/// result; a bare tool value passes through untouched.
fn normalize_hook_dispatch_result(result: Value) -> Result<Value, CallbackError> {
    let Some(obj) = result.as_object() else {
        return Ok(result);
    };

    let is_native_runtime_envelope = obj.contains_key("success")
        && obj.contains_key("status")
        && obj.contains_key("result")
        && (obj.contains_key("outputs")
            || obj.contains_key("warnings")
            || obj.contains_key("cost"));
    if is_native_runtime_envelope {
        let success = obj.get("success").and_then(Value::as_bool).unwrap_or(false);
        let status = obj
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        if !success || status != "completed" {
            let message = obj
                .get("error")
                .or_else(|| obj.get("result"))
                .map(Value::to_string)
                .unwrap_or_else(|| format!("hook child returned status `{status}`"));
            return Err(CallbackError::ActionFailed {
                code: "hook_child_failed".to_string(),
                message,
                retryable: false,
            });
        }
        return Ok(obj.get("result").cloned().unwrap_or(Value::Null));
    }

    let is_managed_envelope =
        obj.contains_key("outcome_code") && obj.contains_key("result") && obj.contains_key("error");
    if is_managed_envelope {
        if let Some(error) = obj.get("error").filter(|value| !value.is_null()) {
            return Err(CallbackError::ActionFailed {
                code: "hook_child_failed".to_string(),
                message: error.to_string(),
                retryable: false,
            });
        }
        return Ok(obj.get("result").cloned().unwrap_or(Value::Null));
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalize_passes_bare_tool_value() {
        let bare = json!({"msg": "hi"});
        assert_eq!(normalize_hook_dispatch_result(bare.clone()).unwrap(), bare);
        assert_eq!(
            normalize_hook_dispatch_result(json!("scalar")).unwrap(),
            json!("scalar")
        );
    }

    #[test]
    fn normalize_unwraps_successful_native_envelope() {
        let env = json!({
            "success": true,
            "status": "completed",
            "result": {"ok": 1},
            "outputs": null,
            "warnings": [],
        });
        assert_eq!(
            normalize_hook_dispatch_result(env).unwrap(),
            json!({"ok": 1})
        );
    }

    #[test]
    fn normalize_rejects_failed_native_envelope_as_hook_child_failed() {
        let env = json!({
            "success": false,
            "status": "error",
            "result": {"error": "boom"},
            "outputs": null,
            "warnings": [],
        });
        let err = normalize_hook_dispatch_result(env).unwrap_err();
        assert!(matches!(
            err,
            CallbackError::ActionFailed { ref code, .. } if code == "hook_child_failed"
        ));
    }

    #[test]
    fn normalize_rejects_failed_managed_envelope() {
        let env = json!({
            "outcome_code": "exit:1",
            "result": null,
            "error": {"exit_code": 1},
        });
        let err = normalize_hook_dispatch_result(env).unwrap_err();
        assert!(matches!(
            err,
            CallbackError::ActionFailed { ref code, .. } if code == "hook_child_failed"
        ));
    }

    #[test]
    fn normalize_unwraps_successful_managed_envelope() {
        let env = json!({
            "outcome_code": "exit:0",
            "result": {"v": 2},
            "error": null,
        });
        assert_eq!(
            normalize_hook_dispatch_result(env).unwrap(),
            json!({"v": 2})
        );
    }

    #[tokio::test]
    async fn run_graph_hooks_is_a_noop_without_hooks() {
        let inner: std::sync::Arc<dyn ryeos_runtime::callback::RuntimeCallbackAPI> =
            std::sync::Arc::new(NoopClient);
        let client = CallbackClient::from_inner(inner, "T-test", "/tmp", "tat");
        let ctx = json!({"event": "graph_started"});
        // Empty hook list → Ok, and the dispatcher (which would panic here) is
        // never built.
        assert!(
            run_graph_hooks(&client, "T-test", "/tmp", &[], "graph_started", &ctx)
                .await
                .is_ok()
        );
    }

    struct NoopClient;

    #[async_trait::async_trait]
    impl ryeos_runtime::callback::RuntimeCallbackAPI for NoopClient {
        async fn dispatch_action(&self, _: DispatchActionRequest) -> Result<Value, CallbackError> {
            panic!("dispatch must not be called for an empty hook list");
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
        async fn request_continuation(
            &self,
            _: &str,
            _: Option<&str>,
        ) -> Result<Value, CallbackError> {
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
        async fn replay_events(&self, _: Value) -> Result<Value, CallbackError> {
            Ok(json!({"events": []}))
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
            _: i64,
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
}
