//! Graph observer hooks, wired to the shared `ryeos-runtime` hook machinery.
//!
//! Authored `config.hooks` use the shared source grammar and compile with the
//! graph definition into [`CompiledHook`]s. The walker fires them at graph
//! lifecycle events (`graph_started`, `graph_step_completed`,
//! `graph_completed`); each matching hook's action is dispatched through the
//! SAME callback path a node action uses, so a hook child is a real dispatch:
//! effective_caps enforced at the callback boundary, cost accrued, visible in
//! the braid. Graph hooks are observers — the control result a hook may return
//! is ignored; routing stays the walker's job.

use serde_json::Value;

use ryeos_runtime::callback::{parse_hook_action, CallbackError, DispatchActionRequest};
use ryeos_runtime::callback_client::CallbackClient;
use ryeos_runtime::envelope::{
    normalize_hook_dispatch_result, RuntimeCost, HOOK_INTEGRITY_FAILURE_CODE,
};
use ryeos_runtime::events::RuntimeEventType;
use ryeos_runtime::hooks_eval::{run_hooks, HookDispatcher, HookRunError};
use ryeos_runtime::CompiledHook;

/// Fire every hook matching `event` against `context`. Returns `Err` with a
/// diagnostic when a hook's condition, action evaluation, or dispatch fails
/// (including a hook child that ran but reported failure, normalized to
/// `hook_child_failed`) — the caller records it as a warning rather than failing
/// the graph. The hook control result is discarded: graph hooks observe.
pub async fn run_graph_hooks(
    callback: &CallbackClient,
    thread_id: &str,
    project_path: &str,
    hooks: &[CompiledHook],
    event: RuntimeEventType,
    context: &Value,
) -> Result<Option<RuntimeCost>, HookRunError> {
    if hooks.is_empty() {
        return Ok(None);
    }
    let dispatcher = build_dispatcher(callback.clone(), thread_id.to_string());
    run_hooks(event.as_str(), context, hooks, project_path, &dispatcher)
        .await
        .map(|result| result.cost)
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
            let payload = parse_hook_action(action).map_err(|message| CallbackError::ActionFailed {
                code: "invalid_hook_action".to_string(),
                message,
                retryable: false,
            })?;
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
            normalize_hook_dispatch_result(response.result).map_err(|message| {
                CallbackError::ActionFailed {
                    code: HOOK_INTEGRITY_FAILURE_CODE.to_string(),
                    message: message.to_string(),
                    retryable: false,
                }
            })
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalize_passes_bare_tool_value() {
        let bare = json!({"msg": "hi"});
        assert_eq!(normalize_hook_dispatch_result(bare.clone()).unwrap().value, bare);
        assert_eq!(
            normalize_hook_dispatch_result(json!("scalar")).unwrap().value,
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
            "cost": null,
        });
        assert_eq!(
            normalize_hook_dispatch_result(env).unwrap().value,
            json!({"ok": 1})
        );
    }

    #[test]
    fn normalize_preserves_valid_native_cost_for_walker_accounting() {
        let output = normalize_hook_dispatch_result(json!({
            "success": true,
            "status": "completed",
            "result": null,
            "outputs": null,
            "warnings": [],
            "cost": {
                "input_tokens": 3,
                "output_tokens": 5,
                "total_usd": 0.25
            }
        }))
        .unwrap();
        let cost = output.cost.unwrap();
        assert_eq!(cost.input_tokens, 3);
        assert_eq!(cost.output_tokens, 5);
        assert_eq!(cost.total_usd, 0.25);
    }

    #[test]
    fn normalize_rejects_failed_native_envelope_as_hook_child_failed() {
        let env = json!({
            "success": false,
            "status": "failed",
            "result": {"error": "boom"},
            "outputs": null,
            "warnings": [],
            "cost": {
                "input_tokens": 7,
                "output_tokens": 11,
                "total_usd": 0.5
            },
        });
        let output = normalize_hook_dispatch_result(env).unwrap();
        assert!(output.failure.unwrap().contains("hook_child_failed"));
        assert_eq!(output.cost.unwrap().input_tokens, 7);
    }

    #[test]
    fn normalize_rejects_legacy_or_contradictory_native_status() {
        for env in [
            json!({
                "success": false,
                "status": "error",
                "result": {"error": "boom"},
                "outputs": null,
                "warnings": [],
                "cost": null,
            }),
            json!({
                "success": true,
                "status": "failed",
                "result": null,
                "outputs": null,
                "warnings": [],
                "cost": null,
            }),
        ] {
            match normalize_hook_dispatch_result(env) {
                Ok(output) => assert!(output
                    .failure
                    .is_some_and(|failure| failure.contains("hook_child_failed"))),
                Err(error) => assert!(error.contains("hook_child_failed")),
            }
        }
    }

    #[test]
    fn normalize_rejects_failed_managed_envelope() {
        let env = json!({
            "outcome_code": "exit:1",
            "result": null,
            "error": {"exit_code": 1},
            "artifacts": [],
        });
        let output = normalize_hook_dispatch_result(env).unwrap();
        assert!(output.failure.unwrap().contains("hook_child_failed"));
    }

    #[test]
    fn normalize_unwraps_successful_managed_envelope() {
        let env = json!({
            "outcome_code": "exit:0",
            "result": {"v": 2},
            "error": null,
            "artifacts": [],
        });
        assert_eq!(
            normalize_hook_dispatch_result(env).unwrap().value,
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
            run_graph_hooks(
                &client,
                "T-test",
                "/tmp",
                &[],
                RuntimeEventType::GraphStarted,
                &ctx,
            )
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
            _: ryeos_runtime::TerminalCompletion,
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
