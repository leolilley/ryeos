use super::*;
use async_trait::async_trait;
use ryeos_runtime::callback::{CallbackError, DispatchActionRequest};
use ryeos_runtime::envelope::RuntimeResultStatus;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

struct MockClient {
    results: Mutex<Vec<Value>>,
    /// Commands handed back on the FIRST `claim_commands`, then drained empty.
    pending_commands: Mutex<Vec<Value>>,
    /// Recorded `(command_id, status)` for every `complete_command`.
    completed: Mutex<Vec<(i64, String)>>,
    /// Status carried by the terminal `finalize_thread`, if any.
    finalized_status: Mutex<Option<ryeos_runtime::ThreadTerminalStatus>>,
}

impl MockClient {
    fn new(results: Vec<Value>) -> Self {
        Self {
            results: Mutex::new(results),
            pending_commands: Mutex::new(Vec::new()),
            completed: Mutex::new(Vec::new()),
            finalized_status: Mutex::new(None),
        }
    }

    fn with_pending_commands(results: Vec<Value>, commands: Vec<Value>) -> Self {
        let mock = Self::new(results);
        *mock.pending_commands.lock().unwrap() = commands;
        mock
    }
}

#[async_trait]
impl ryeos_runtime::callback::RuntimeCallbackAPI for MockClient {
    async fn dispatch_action(
        &self,
        _request: DispatchActionRequest,
    ) -> Result<Value, CallbackError> {
        let mut results = self.results.lock().unwrap();
        // Strict typed contract: CallbackClient::dispatch_action
        // requires `{thread, result}` shape; preserve any caller-
        // supplied leaf by wrapping it under `result`.
        if results.is_empty() {
            Ok(json!({"thread": {}, "result": {}}))
        } else {
            let result = results.remove(0);
            if result.get("__retryable_dispatch_error").is_some() {
                Err(CallbackError::ActionFailed {
                    code: "service_unavailable".to_string(),
                    message: "simulated transient dispatch failure".to_string(),
                    retryable: true,
                })
            } else {
                Ok(json!({"thread": {}, "result": result}))
            }
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
        completion: ryeos_runtime::TerminalCompletion,
    ) -> Result<Value, CallbackError> {
        *self.finalized_status.lock().unwrap() = Some(completion.status);
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
    async fn bundle_events_read_chain(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
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
        let commands = std::mem::take(&mut *self.pending_commands.lock().unwrap());
        Ok(json!({ "commands": commands }))
    }
    async fn complete_command(
        &self,
        _: &str,
        command_id: i64,
        status: &str,
        _: Value,
    ) -> Result<Value, CallbackError> {
        self.completed
            .lock()
            .unwrap()
            .push((command_id, status.to_string()));
        Ok(json!({}))
    }
    async fn publish_artifact(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
        Ok(json!({}))
    }
    async fn get_facets(&self, _: &str) -> Result<Value, CallbackError> {
        Ok(json!({}))
    }
    async fn spawn_follow_child(
        &self,
        _request: ryeos_runtime::callback::SpawnFollowChildRequest,
    ) -> Result<Value, CallbackError> {
        // Simulate the daemon accepting the follow handoff (it would settle
        // this thread `continued` server-side).
        Ok(json!({ "phase": "waiting" }))
    }
}

fn make_callback(results: Vec<Value>) -> CallbackClient {
    let inner: Arc<dyn ryeos_runtime::callback::RuntimeCallbackAPI> =
        Arc::new(MockClient::new(results));
    CallbackClient::from_inner(inner, "thread-test", "/tmp/test-project", "tat-test")
}

fn make_graph(yaml: &str) -> GraphDefinition {
    GraphDefinition::from_yaml(yaml, Some("test.yaml")).unwrap()
}

fn make_walker(graph: GraphDefinition, results: Vec<Value>) -> Walker {
    Walker::new(
        graph,
        "/tmp/test-project".to_string(),
        "thread-test".to_string(),
        make_callback(results),
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn strict_resume_params(
    graph: &GraphDefinition,
    current_node: &str,
    step_count: u32,
    state: Value,
    graph_run_id: &str,
    pending_follow: Option<Value>,
    follow_result: Option<Value>,
    retry_attempt: u32,
) -> Value {
    let mut checkpoint = json!({
        "schema_version": GRAPH_CHECKPOINT_SCHEMA_VERSION,
        "definition_ref": graph.definition_ref.clone(),
        "definition_hash": graph.definition_hash.clone(),
        "expression_language": EXPRESSION_LANGUAGE,
        "current_node": current_node,
        "step_count": step_count,
        "state": state,
        "graph_run_id": graph_run_id,
        "accounting": {"total": null, "nodes": [], "hooks": []},
        "suppressed_errors": [],
        "retry_attempt": retry_attempt,
        "written_at": "2026-07-14T00:00:00Z",
    });
    if let Some(pending) = pending_follow {
        checkpoint[follow_keys::PENDING_FOLLOW] = pending;
    }
    if let Some(result) = follow_result {
        checkpoint[follow_keys::FOLLOW_RESULT] = result;
    }
    let resume = crate::resume::from_checkpoint_value(&checkpoint, graph).unwrap();
    json!({"resume_state": serde_json::to_value(resume).unwrap()})
}

/// Build the injected resume DTO without first passing it through the strict
/// parser. Only corruption/preflight tests use this seam; normal resume tests
/// must use `strict_resume_params` so their fixtures prove the accepted
/// contract.
#[allow(clippy::too_many_arguments)]
fn unchecked_resume_params(
    graph: &GraphDefinition,
    current_node: &str,
    step_count: u32,
    state: Value,
    graph_run_id: &str,
    pending_follow: Option<Value>,
    follow_result: Option<Value>,
    retry_attempt: u32,
) -> Value {
    let mut resume = json!({
        "definition_ref": graph.definition_ref.clone(),
        "definition_hash": graph.definition_hash.clone(),
        "expression_language": EXPRESSION_LANGUAGE,
        "current_node": current_node,
        "step_count": step_count,
        "state": state,
        "graph_run_id": graph_run_id,
        "accounting": {"total": null, "nodes": [], "hooks": []},
        "suppressed_errors": [],
        "retry_attempt": retry_attempt,
    });
    if let Some(pending) = pending_follow {
        resume[follow_keys::PENDING_FOLLOW] = pending;
    }
    if let Some(result) = follow_result {
        resume[follow_keys::FOLLOW_RESULT] = result;
    }
    json!({"resume_state": resume})
}

#[tokio::test]
async fn simple_action_to_return() {
    let yaml = r#"
version: "1.0.0"
category: test
config:
  start: step1
  nodes:
    step1:
      action: {item_id: "tool:test/echo", params: {msg: hello}}
      assign: {echo_result: "${result}"}
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;
    let graph = make_graph(yaml);
    let w = make_walker(graph, vec![json!({"msg": "hello"})]);
    let result = w.execute(json!({}), None).await;
    assert!(result.success);
    assert_eq!(result.status, GraphRunStatus::Completed);
    assert_eq!(result.steps, 2);
}

#[tokio::test]
async fn assignment_keys_are_simultaneous_and_branch_reads_candidate_state() {
    let graph = make_graph(
        r#"
version: "1.0.0"
category: test
config:
  start: update
  state: {count: 1}
  nodes:
    update:
      action: {item_id: "tool:test/echo"}
      assign:
        previous: "${state.count}"
        count: "${state.count + 1}"
        observed: "${result.value}"
      next:
        type: conditional
        branches:
          - when: 'state.previous == 1 && state.count == 2 && result.value == 7'
            to: done
          - to: wrong
    done:
      node_type: return
    wrong:
      action: {item_id: "tool:test/wrong"}
      assign: {wrong_path: true}
"#,
    );
    let result = make_walker(graph, vec![json!({"value": 7})])
        .execute(json!({}), None)
        .await;

    assert!(result.success);
    assert_eq!(result.state["previous"], json!(1));
    assert_eq!(result.state["count"], json!(2));
    assert_eq!(result.state["observed"], json!(7));
    assert!(result.state.get("wrong_path").is_none());
}

#[tokio::test]
async fn branch_expression_failure_redirects_with_pre_node_state() {
    let graph = make_graph(
        r#"
version: "1.0.0"
category: test
config:
  start: update
  state: {count: 1}
  nodes:
    update:
      action: {item_id: "tool:test/echo"}
      assign: {count: "${state.count + 1}"}
      on_error: recover
      next:
        type: conditional
        branches:
          - when: '1 / inputs.zero > 0'
            to: wrong
          - to: wrong
    recover:
      node_type: return
      output: "${state.count}"
    wrong:
      action: {item_id: "tool:test/wrong"}
      assign: {wrong_path: true}
"#,
    );
    let result = make_walker(graph, vec![json!({"ok": true})])
        .execute(json!({"inputs": {"zero": 0}}), None)
        .await;

    assert!(result.success);
    assert_eq!(result.state["count"], json!(1));
    assert_eq!(result.result, Some(json!(1)));
    assert!(result.state.get("wrong_path").is_none());
}

#[tokio::test]
async fn expression_failure_under_continue_terminates_without_normal_edge() {
    let graph = make_graph(
        r#"
version: "1.0.0"
category: test
config:
  start: update
  on_error: continue
  state: {count: 1}
  nodes:
    update:
      action: {item_id: "tool:test/echo"}
      assign: {count: "${state.count + 1}"}
      next:
        type: conditional
        branches:
          - when: '1 / inputs.zero > 0'
            to: should_not_run
          - to: should_not_run
    should_not_run:
      action: {item_id: "tool:test/wrong"}
      assign: {wrong_path: true}
"#,
    );
    let result = make_walker(
        graph,
        vec![json!({"ok": true}), json!({"unexpected": true})],
    )
    .execute(json!({"inputs": {"zero": 0}}), None)
    .await;

    assert!(result.success);
    assert_eq!(result.status, GraphRunStatus::CompletedWithErrors);
    assert_eq!(result.state["count"], json!(1));
    assert!(result.state.get("wrong_path").is_none());
    assert_eq!(result.errors_suppressed, Some(1));
}

#[tokio::test]
async fn gate_condition_error_does_not_fall_through_to_default() {
    let graph = make_graph(
        r#"
version: "1.0.0"
category: test
config:
  start: choose
  on_error: continue
  nodes:
    choose:
      node_type: gate
      next:
        type: conditional
        branches:
          - when: '1 / inputs.zero > 0'
            to: should_not_run
          - to: should_not_run
    should_not_run:
      action: {item_id: "tool:test/wrong"}
      assign: {wrong_path: true}
"#,
    );
    let result = make_walker(graph, vec![json!({"unexpected": true})])
        .execute(json!({"inputs": {"zero": 0}}), None)
        .await;

    assert!(result.success);
    assert_eq!(result.status, GraphRunStatus::CompletedWithErrors);
    assert!(result.state.get("wrong_path").is_none());
    assert_eq!(result.errors_suppressed, Some(1));
}

#[tokio::test]
async fn return_output_preserves_nested_native_values() {
    let graph = make_graph(
        r#"
version: "1.0.0"
category: test
config:
  start: done
  state: {count: 2, items: [1, 2], enabled: true}
  nodes:
    done:
      node_type: return
      output:
        count: "${state.count}"
        items: "${state.items}"
        enabled: "${state.enabled}"
        summary: "count=${state.count}"
"#,
    );
    let result = make_walker(graph, vec![]).execute(json!({}), None).await;

    assert_eq!(
        result.result,
        Some(json!({
            "count": 2,
            "items": [1, 2],
            "enabled": true,
            "summary": "count=2",
        }))
    );
}

/// A cancel queued before the first node runs is drained between nodes: the
/// walker acks it `completed`, settles the run/thread `cancelled`, and never
/// executes the node.
#[tokio::test]
async fn cooperative_cancel_settles_cancelled_and_acks_command() {
    let yaml = r#"
version: "1.0.0"
category: test
config:
  start: step1
  nodes:
    step1:
      action: {item_id: "tool:test/echo", params: {msg: hello}}
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;
    let graph = make_graph(yaml);
    let mock = Arc::new(MockClient::with_pending_commands(
        vec![json!({"msg": "hello"})],
        vec![json!({"command_id": 7, "command_type": "cancel"})],
    ));
    let client =
        CallbackClient::from_inner(mock.clone(), "thread-test", "/tmp/test-project", "tat-test");
    let w = Walker::new(
        graph,
        "/tmp/test-project".to_string(),
        "thread-test".to_string(),
        client,
        None,
    );
    let result = w.execute(json!({}), None).await;

    assert!(!result.success);
    assert_eq!(result.status, GraphRunStatus::Cancelled);
    // Terminated before running step1.
    assert_eq!(result.steps, 0);
    // The cancel was acknowledged completed…
    assert_eq!(
        *mock.completed.lock().unwrap(),
        vec![(7, "completed".to_string())]
    );
    // …and the thread finalized cancelled, not failed.
    assert_eq!(
        *mock.finalized_status.lock().unwrap(),
        Some(ryeos_runtime::ThreadTerminalStatus::Cancelled)
    );
}

/// When cancel and kill queue in the same drained batch, kill (the harder
/// stop) wins the terminal status, and BOTH commands are still acked so
/// neither hangs in `claimed`.
#[tokio::test]
async fn cooperative_kill_outranks_cancel_in_one_batch() {
    let yaml = r#"
version: "1.0.0"
category: test
config:
  start: step1
  nodes:
    step1:
      action: {item_id: "tool:test/echo", params: {msg: hello}}
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;
    let graph = make_graph(yaml);
    let mock = Arc::new(MockClient::with_pending_commands(
        vec![json!({"msg": "hello"})],
        vec![
            json!({"command_id": 1, "command_type": "cancel"}),
            json!({"command_id": 2, "command_type": "kill"}),
        ],
    ));
    let client =
        CallbackClient::from_inner(mock.clone(), "thread-test", "/tmp/test-project", "tat-test");
    let w = Walker::new(
        graph,
        "/tmp/test-project".to_string(),
        "thread-test".to_string(),
        client,
        None,
    );
    let result = w.execute(json!({}), None).await;

    assert_eq!(result.status, GraphRunStatus::Killed);
    assert_eq!(
        *mock.finalized_status.lock().unwrap(),
        Some(ryeos_runtime::ThreadTerminalStatus::Killed)
    );
    // Both commands acked completed, regardless of which won the terminal.
    let completed = mock.completed.lock().unwrap().clone();
    assert!(completed.contains(&(1, "completed".to_string())));
    assert!(completed.contains(&(2, "completed".to_string())));
}

/// A signal-driven cancel flag (SIGTERM) already set finalizes the run
/// cancelled at the first node boundary, without executing a node — the same
/// cooperative terminal a claimed cancel command produces, but with no
/// command to settle.
#[tokio::test]
async fn signal_cancel_flag_settles_cancelled_between_nodes() {
    let yaml = r#"
version: "1.0.0"
category: test
config:
  start: step1
  nodes:
    step1:
      action: {item_id: "tool:test/echo", params: {msg: hello}}
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;
    let graph = make_graph(yaml);
    let mock = Arc::new(MockClient::new(vec![json!({"msg": "hello"})]));
    let client =
        CallbackClient::from_inner(mock.clone(), "thread-test", "/tmp/test-project", "tat-test");
    // Flag pre-set, as if SIGTERM already arrived before the first node.
    let flag = Arc::new(AtomicBool::new(true));
    let w = Walker::new(
        graph,
        "/tmp/test-project".to_string(),
        "thread-test".to_string(),
        client,
        None,
    )
    .with_cancel_flag(flag);
    let result = w.execute(json!({}), None).await;

    assert_eq!(result.status, GraphRunStatus::Cancelled);
    assert!(!result.success);
    assert_eq!(result.steps, 0);
    assert_eq!(
        *mock.finalized_status.lock().unwrap(),
        Some(ryeos_runtime::ThreadTerminalStatus::Cancelled)
    );
}

#[tokio::test]
async fn signal_cancel_flag_wakes_top_level_retry_backoff() {
    let yaml = r#"
version: "1.0.0"
category: test
config:
  start: done
  nodes:
    done:
      node_type: return
"#;
    let flag = Arc::new(AtomicBool::new(false));
    let walker = make_walker(make_graph(yaml), Vec::new()).with_cancel_flag(flag.clone());
    let setter = flag.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        setter.store(true, Ordering::Relaxed);
    });

    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        walker.sleep_retry_backoff(MAX_RETRY_BACKOFF_MS),
    )
    .await
    .expect("SIGTERM flag must wake a top-level retry backoff promptly");
}

#[tokio::test]
async fn follow_node_suspends_graph_as_continued() {
    let yaml = r#"
version: "1.0.0"
category: test
config:
  start: fetch
  nodes:
    fetch:
      follow: true
      action: {item_id: "directive:child", params: {}}
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;
    let graph = make_graph(yaml);
    let w = make_walker(graph, vec![]);
    let result = w.execute(json!({}), None).await;
    // A follow node hands off to a detached child and suspends: the daemon
    // settled this thread `continued`, so the walker reports continued (not
    // completed), suspended at the follow node (step 0) with no result yet.
    assert_eq!(result.status, GraphRunStatus::Continued);
    assert!(!result.success);
    assert_eq!(result.steps, 0);
    assert!(result.result.is_none());
}

/// Helper: assert no graph-state value contains an unresolved
/// `${...}` template (the P0 state-corruption symptom).
fn assert_no_raw_template(state: &Value) {
    let s = serde_json::to_string(state).unwrap();
    assert!(
        !s.contains("${"),
        "graph state must not carry unresolved templates, got: {s}"
    );
}

// ── Acceptance: a failing tool inside a graph produces ONE actionable
//    error (node + exit + stderr) and never poisons state. ──────────

#[tokio::test]
async fn failing_tool_on_error_fail_surfaces_diagnostic() {
    let yaml = r#"
version: "1.0.0"
category: test
config:
  start: step1
  on_error: fail
  nodes:
    step1:
      action: {item_id: "tool:test/fail"}
      assign: {captured: "${result.value}"}
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;
    let graph = make_graph(yaml);
    // Failed subprocess envelope: result null, error carries stderr.
    let w = make_walker(
        graph,
        vec![json!({
            "outcome_code": "exit:1",
            "result": null,
            "error": {"exit_code": 1, "stderr": "Traceback: boom"}
        })],
    );
    let result = w.execute(json!({}), None).await;

    assert!(!result.success, "failed tool must fail the graph");
    assert_eq!(result.status, GraphRunStatus::Error);
    let err = result.error.unwrap_or_default();
    assert!(err.contains("step1"), "error should name the node: {err}");
    assert!(err.contains("boom"), "error should carry stderr: {err}");
    // The poisoned-state symptom must be absent: no `${result...}`.
    assert_no_raw_template(&result.state);
}

#[tokio::test]
async fn failing_tool_on_error_continue_records_structured_error() {
    let yaml = r#"
version: "1.0.0"
category: test
config:
  start: step1
  on_error: continue
  nodes:
    step1:
      action: {item_id: "tool:test/fail"}
      assign: {captured: "${result.value}"}
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;
    let graph = make_graph(yaml);
    let w = make_walker(
        graph,
        vec![json!({
            "outcome_code": "exit:1",
            "result": null,
            "error": {"exit_code": 1, "stderr": "boom"}
        })],
    );
    let result = w.execute(json!({}), None).await;

    assert!(result.success);
    assert_eq!(result.status, GraphRunStatus::CompletedWithErrors);
    assert_eq!(result.errors_suppressed, Some(1));
    let errors = result.errors.unwrap();
    assert_eq!(errors[0].node, "step1");
    assert!(errors[0].error.contains("boom"), "got: {}", errors[0].error);
    // Assignment never ran against a `null`, so no raw template leaked.
    assert_no_raw_template(&result.state);
}

#[tokio::test]
async fn bare_user_status_error_is_not_a_graph_failure() {
    // A tool that legitimately returns domain data shaped like
    // `{status: "error", message: ...}` with a CLEAN process exit is
    // NOT a graph failure — only the execution envelope decides.
    let yaml = r#"
version: "1.0.0"
category: test
config:
  start: step1
  on_error: fail
  nodes:
    step1:
      action: {item_id: "tool:test/lookup"}
      assign: {outcome: "${result.status}"}
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;
    let graph = make_graph(yaml);
    let w = make_walker(
        graph,
        vec![json!({"status": "error", "message": "not found"})],
    );
    let result = w.execute(json!({}), None).await;

    assert!(result.success, "bare domain data must not fail the graph");
    assert_eq!(result.status, GraphRunStatus::Completed);
    assert_eq!(
        result.state.get("outcome").and_then(|v| v.as_str()),
        Some("error")
    );
}

#[tokio::test]
async fn assign_expression_failure_obeys_on_error() {
    // Tool succeeds, but `assign` references a missing field — the
    // expression failure is a node error (obeys on_error: fail),
    // NOT a suppressed error that merges the raw `${...}` into state.
    let yaml = r#"
version: "1.0.0"
category: test
config:
  start: step1
  on_error: fail
  nodes:
    step1:
      action: {item_id: "tool:test/echo"}
      assign: {captured: "${result.missing.deep}"}
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;
    let graph = make_graph(yaml);
    let w = make_walker(graph, vec![json!({"present": 1})]);
    let result = w.execute(json!({}), None).await;

    assert!(!result.success);
    assert_eq!(result.status, GraphRunStatus::Error);
    assert!(result.error.unwrap_or_default().contains("assign"));
    assert_no_raw_template(&result.state);
}

#[tokio::test]
async fn return_output_expression_failure_fails_run() {
    // A return node whose `output` template can't resolve must FAIL
    // the run rather than emit a raw `${...}` template as the result.
    let yaml = r#"
version: "1.0.0"
category: test
config:
  start: done
  nodes:
    done:
      node_type: return
      output: "${state.never_set}"
"#;
    let graph = make_graph(yaml);
    let w = make_walker(graph, vec![]);
    let result = w.execute(json!({}), None).await;

    assert!(!result.success);
    assert_eq!(result.status, GraphRunStatus::Error);
    assert!(result.error.unwrap_or_default().contains("output"));
    assert!(result.result.is_none(), "no raw template as result");
}

#[tokio::test]
async fn return_output_resolves_inputs() {
    // `${inputs.*}` must resolve in a return node's output (inputs are
    // threaded into the terminal expression context).
    let yaml = r#"
version: "1.0.0"
category: test
config:
  start: done
  nodes:
    done:
      node_type: return
      output: "${inputs.game_id}"
"#;
    let graph = make_graph(yaml);
    let w = make_walker(graph, vec![]);
    let result = w
        .execute(json!({"inputs": {"game_id": "g-42"}}), None)
        .await;

    assert!(result.success, "got: {:?}", result.error);
    assert_eq!(
        result.result.and_then(|v| v.as_str().map(String::from)),
        Some("g-42".to_string())
    );
}

#[tokio::test]
async fn return_output_accepts_map_template() {
    // A map `output:` renders each leaf and yields a structured
    // result (not just a string).
    let yaml = r#"
version: "1.0.0"
category: test
config:
  start: done
  nodes:
    done:
      node_type: return
      output:
        game_id: "${inputs.game_id}"
        nested:
          level: "${state.level}"
"#;
    let graph = make_graph(yaml);
    let w = make_walker(graph, vec![]);
    let result = w
        .execute(
            json!({"inputs": {"game_id": "g-7"}, "inject_state": {"level": "hard"}}),
            None,
        )
        .await;

    assert!(result.success, "got: {:?}", result.error);
    assert_eq!(
        result.result,
        Some(json!({"game_id": "g-7", "nested": {"level": "hard"}}))
    );
}

#[tokio::test]
async fn return_output_accepts_list_template() {
    // A list `output:` renders each element.
    let yaml = r#"
version: "1.0.0"
category: test
config:
  start: done
  nodes:
    done:
      node_type: return
      output:
        - "${inputs.a}"
        - "${state.b}"
"#;
    let graph = make_graph(yaml);
    let w = make_walker(graph, vec![]);
    let result = w
        .execute(
            json!({"inputs": {"a": "first"}, "inject_state": {"b": "second"}}),
            None,
        )
        .await;

    assert!(result.success, "got: {:?}", result.error);
    assert_eq!(result.result, Some(json!(["first", "second"])));
}

#[tokio::test]
async fn graph_exposes_directive_outputs_and_cost() {
    // P0/Phase A+C end-to-end: a directive node's declared `outputs`
    // reach graph state via `${result.outputs.X}`, and the directive's
    // reported cost lands in the aggregate + per-node accounting.
    let yaml = r#"
version: "1.0.0"
category: test
config:
  start: reason
  nodes:
    reason:
      node_type: action
      action:
        item_id: "directive:test/reason"
      assign:
        recommendations: "${result.outputs.recommendations}"
      next:
        type: unconditional
        to: done
    done:
      node_type: return
      output: "${state.recommendations}"
"#;
    let graph = make_graph(yaml);
    // Native directive envelope: payload in `outputs`, cost reported.
    let envelope = json!({
        "success": true,
        "status": "completed",
        "result": "directive_return",
        "outputs": {"recommendations": ["a", "b"]},
        "cost": {"input_tokens": 100, "output_tokens": 20, "total_usd": 0.001},
        "warnings": []
    });
    let w = make_walker(graph, vec![envelope]);
    let result = w.execute(json!({}), None).await;

    assert!(result.success, "got: {:?}", result.error);
    // A1: structured outputs flowed through assign into the result.
    assert_eq!(result.result, Some(json!(["a", "b"])));
    // C: aggregate + per-node cost recorded.
    let cost = result.cost.expect("graph cost should be populated");
    assert_eq!(cost.input_tokens, 100);
    assert_eq!(cost.output_tokens, 20);
    assert_eq!(result.node_costs.len(), 1);
    assert_eq!(result.node_costs[0].node, "reason");
    assert_eq!(result.node_costs[0].item_id, "directive:test/reason");
    assert_eq!(result.node_costs[0].cost.output_tokens, 20);
}

#[tokio::test]
async fn graph_aggregates_cost_across_two_directive_nodes() {
    let yaml = r#"
version: "1.0.0"
category: test
config:
  start: first
  nodes:
    first:
      node_type: action
      action:
        item_id: "directive:test/a"
      next:
        type: unconditional
        to: second
    second:
      node_type: action
      action:
        item_id: "directive:test/b"
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;
    let graph = make_graph(yaml);
    let env = |i: u64, o: u64, usd: f64| {
        json!({
            "success": true,
            "status": "completed",
            "result": "directive_return",
            "outputs": {"ok": true},
            "cost": {"input_tokens": i, "output_tokens": o, "total_usd": usd},
            "warnings": []
        })
    };
    let w = make_walker(graph, vec![env(10, 5, 0.001), env(30, 7, 0.002)]);
    let result = w.execute(json!({}), None).await;

    assert!(result.success, "got: {:?}", result.error);
    let cost = result.cost.expect("aggregate cost");
    assert_eq!(cost.input_tokens, 40);
    assert_eq!(cost.output_tokens, 12);
    assert!((cost.total_usd - 0.003).abs() < 1e-9);
    assert_eq!(result.node_costs.len(), 2);
}

#[tokio::test]
async fn graph_without_child_cost_reports_no_cost() {
    // A subprocess tool leaf carries no envelope `cost` — the graph
    // must finalize `cost: None` rather than an all-zeros record.
    let yaml = r#"
version: "1.0.0"
category: test
config:
  start: act
  nodes:
    act:
      node_type: action
      action:
        item_id: "tool:test/echo"
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;
    let graph = make_graph(yaml);
    // Subprocess envelope: clean exit, no cost field.
    let envelope = json!({
        "outcome_code": "exit:0",
        "result": {"ok": true},
        "error": null,
        "artifacts": []
    });
    let w = make_walker(graph, vec![envelope]);
    let result = w.execute(json!({}), None).await;

    assert!(result.success, "got: {:?}", result.error);
    assert!(result.cost.is_none(), "no child cost → no graph cost");
    assert!(result.node_costs.is_empty());
}

// ── Phase C: failure-path / foreach / reset / cache cost accounting ──

fn native_envelope(success: bool, outputs: Value, cost: Option<(u64, u64, f64)>) -> Value {
    let mut env = json!({
        "success": success,
        "status": if success { "completed" } else { "failed" },
        "result": if success { json!("directive_return") } else { json!({"error": "boom"}) },
        "outputs": outputs,
        "warnings": [],
        "cost": null,
    });
    if let Some((i, o, usd)) = cost {
        env["cost"] = json!({"input_tokens": i, "output_tokens": o, "total_usd": usd});
    }
    env
}

#[tokio::test]
async fn graph_failed_directive_child_reports_partial_cost() {
    // A directive that burns tokens then fails (success:false + cost)
    // must still surface its cost in the graph result and node_costs.
    let yaml = r#"
version: "1.0.0"
category: test
config:
  start: reason
  nodes:
    reason:
      node_type: action
      action: {item_id: "directive:test/reason"}
      next: {type: unconditional, to: done}
    done:
      node_type: return
"#;
    let env = native_envelope(false, Value::Null, Some((80, 0, 0.0008)));
    let w = make_walker(make_graph(yaml), vec![env]);
    let result = w.execute(json!({}), None).await;

    assert!(!result.success);
    let cost = result.cost.expect("failed child cost should be reported");
    assert_eq!(cost.input_tokens, 80);
    assert_eq!(result.node_costs.len(), 1);
    assert_eq!(result.node_costs[0].node, "reason");
}

#[tokio::test]
async fn graph_cost_recorded_when_assign_fails_after_success() {
    // Child succeeds (with cost), but `assign` evaluation fails →
    // cost must still be accounted, not lost to the error path.
    let yaml = r#"
version: "1.0.0"
category: test
config:
  start: reason
  nodes:
    reason:
      node_type: action
      action: {item_id: "directive:test/reason"}
      assign: {x: "${result.outputs.missing}"}
      next: {type: unconditional, to: done}
    done:
      node_type: return
"#;
    let env = native_envelope(
        true,
        json!({"recommendations": ["a"]}),
        Some((50, 10, 0.0005)),
    );
    let w = make_walker(make_graph(yaml), vec![env]);
    let result = w.execute(json!({}), None).await;

    assert!(!result.success, "assign failure should fail the run");
    let cost = result
        .cost
        .expect("cost from successful child must survive assign failure");
    assert_eq!(cost.input_tokens, 50);
}

#[tokio::test]
async fn graph_on_error_continue_records_cost_of_failed_child() {
    let yaml = r#"
version: "1.0.0"
category: test
config:
  start: reason
  on_error: continue
  nodes:
    reason:
      node_type: action
      action: {item_id: "directive:test/reason"}
      next: {type: unconditional, to: done}
    done:
      node_type: return
"#;
    let env = native_envelope(false, Value::Null, Some((30, 0, 0.0003)));
    let w = make_walker(make_graph(yaml), vec![env]);
    let result = w.execute(json!({}), None).await;

    assert!(result.success, "continue policy keeps the run successful");
    assert_eq!(result.status, GraphRunStatus::CompletedWithErrors);
    assert_eq!(
        result
            .cost
            .expect("cost recorded under continue")
            .input_tokens,
        30
    );
}

#[tokio::test]
async fn terminal_completion_and_runtime_carry_cost() {
    // The cost aggregate must reach TerminalCompletion.cost (the
    // callback wire), not just the in-process GraphResult.
    let yaml = r#"
version: "1.0.0"
category: test
config:
  start: reason
  nodes:
    reason:
      node_type: action
      action: {item_id: "directive:test/reason"}
      next: {type: unconditional, to: done}
    done:
      node_type: return
"#;
    let env = native_envelope(true, json!({"ok": true}), Some((100, 20, 0.001)));
    let (w, recorder) = make_recording_walker(make_graph(yaml), vec![env], None);
    let result = w.execute(json!({}), None).await;

    assert!(result.success, "got: {:?}", result.error);
    assert_eq!(result.cost.as_ref().unwrap().input_tokens, 100);
    let costs = recorder.finalize_costs.lock().unwrap();
    let last = costs.last().expect("a finalize_thread call").clone();
    let cost = last.expect("TerminalCompletion.cost should be populated");
    assert_eq!(cost["input_tokens"], 100);
}

#[tokio::test]
async fn foreach_aggregates_cost_including_failed_iteration_under_continue() {
    let yaml = r#"
version: "1.0.0"
category: test
config:
  start: iterate
  on_error: continue
  nodes:
    iterate:
      node_type: foreach
      over: "${state.items}"
      as: elem
      action: {item_id: "directive:test/step"}
      next: {type: unconditional, to: done}
    done:
      node_type: return
"#;
    // iter 1 succeeds (cost 10), iter 2 fails (cost 5) → aggregate 15.
    let results = vec![
        native_envelope(true, json!({"ok": true}), Some((10, 0, 0.001))),
        native_envelope(false, Value::Null, Some((5, 0, 0.0005))),
    ];
    let w = make_walker(make_graph(yaml), results);
    let result = w
        .execute(json!({"inject_state": {"items": ["a", "b"]}}), None)
        .await;

    assert!(result.success);
    let cost = result.cost.expect("foreach aggregate cost");
    assert_eq!(cost.input_tokens, 15);
    assert_eq!(
        result.node_costs.len(),
        1,
        "foreach aggregates to one record"
    );
    assert_eq!(result.node_costs[0].item_id, "directive:test/step");
}

#[tokio::test]
async fn parallel_foreach_aggregates_cost_across_iterations() {
    // Parallel path: cost aggregation must not depend on iteration
    // ordering. Sum is 15 regardless of which task drew which envelope.
    let yaml = r#"
version: "1.0.0"
category: test
config:
  start: iterate
  on_error: continue
  nodes:
    iterate:
      node_type: foreach
      over: "${state.items}"
      as: elem
      parallel: true
      action: {item_id: "directive:test/step"}
      next: {type: unconditional, to: done}
    done:
      node_type: return
"#;
    let results = vec![
        native_envelope(true, json!({"ok": true}), Some((10, 0, 0.001))),
        native_envelope(false, Value::Null, Some((5, 0, 0.0005))),
    ];
    let w = make_walker(make_graph(yaml), results);
    let result = w
        .execute(json!({"inject_state": {"items": ["a", "b"]}}), None)
        .await;

    assert!(result.success);
    assert_eq!(result.cost.expect("parallel aggregate").input_tokens, 15);
}

#[tokio::test]
async fn foreach_reports_already_spent_cost_under_fail_policy() {
    let yaml = r#"
version: "1.0.0"
category: test
config:
  start: iterate
  nodes:
    iterate:
      node_type: foreach
      over: "${state.items}"
      as: elem
      action: {item_id: "directive:test/step"}
      next: {type: unconditional, to: done}
    done:
      node_type: return
"#;
    let results = vec![
        native_envelope(true, json!({"ok": true}), Some((10, 0, 0.001))),
        native_envelope(false, Value::Null, Some((5, 0, 0.0005))),
    ];
    let w = make_walker(make_graph(yaml), results);
    let result = w
        .execute(json!({"inject_state": {"items": ["a", "b"]}}), None)
        .await;

    assert!(!result.success, "default fail policy aborts the foreach");
    let cost = result.cost.expect("already-spent foreach cost on failure");
    assert_eq!(cost.input_tokens, 15);
}

fn invalid_native_cost_envelope() -> Value {
    json!({
        "success": true,
        "status": "completed",
        "result": {"ok": true},
        "outputs": null,
        "warnings": [],
        "cost": {
            "input_tokens": i64::MAX as u64 + 1,
            "output_tokens": 0,
            "total_usd": 0.0
        }
    })
}

#[tokio::test]
async fn on_error_cannot_route_around_invalid_native_cost() {
    let graph = make_graph(
        r#"
version: "1.0.0"
category: test
config:
  start: act
  on_error: continue
  nodes:
    act:
      action: {item_id: "directive:test/step"}
      next: {type: unconditional, to: done}
    done: {node_type: return}
"#,
    );
    let (walker, recorder) =
        make_recording_walker(graph, vec![invalid_native_cost_envelope()], None);

    let result = walker.execute(json!({}), None).await;

    assert_eq!(result.status, GraphRunStatus::Error);
    assert!(result
        .error
        .as_deref()
        .is_some_and(|error| error.contains("invalid cost")));
    let events = recorder.recorded_events();
    let (_, _, payload, _) = events
        .iter()
        .find(|(_, event_type, _, _)| event_type == RuntimeEventType::ToolCallResult.as_str())
        .expect("invalid native cost emits a terminal tool result");
    assert_eq!(
        payload["status"],
        GraphToolCallStatus::IntegrityFailed.as_str()
    );
}

#[tokio::test]
async fn foreach_modes_cannot_continue_past_invalid_native_cost() {
    for parallel in [false, true] {
        let graph = make_graph(&format!(
            r#"
version: "1.0.0"
category: test
config:
  start: iterate
  on_error: continue
  nodes:
    iterate:
      node_type: foreach
      over: "${{state.items}}"
      as: elem
      parallel: {parallel}
      action: {{item_id: "directive:test/step"}}
      next: {{type: unconditional, to: done}}
    done: {{node_type: return}}
"#
        ));
        let (walker, recorder) =
            make_recording_walker(graph, vec![invalid_native_cost_envelope()], None);

        let result = walker
            .execute(json!({"inject_state": {"items": ["one"]}}), None)
            .await;

        assert_eq!(result.status, GraphRunStatus::Error, "parallel={parallel}");
        assert!(result
            .error
            .as_deref()
            .is_some_and(|error| error.contains("integrity failed")));
        let events = recorder.recorded_events();
        let (_, _, payload, _) = events
            .iter()
            .find(|(_, event_type, _, _)| {
                event_type == RuntimeEventType::GraphForeachIteration.as_str()
            })
            .expect("invalid native cost emits a foreach iteration result");
        assert_eq!(
            payload["status"],
            GraphToolCallStatus::IntegrityFailed.as_str(),
            "parallel={parallel}"
        );
    }
}

#[tokio::test]
async fn cost_accounting_resets_between_executes() {
    // A Walker reused across execute() calls must not accumulate stale
    // cost — each run reports only its own.
    let yaml = r#"
version: "1.0.0"
category: test
config:
  start: reason
  nodes:
    reason:
      node_type: action
      action: {item_id: "directive:test/reason"}
      next: {type: unconditional, to: done}
    done:
      node_type: return
"#;
    let results = vec![
        native_envelope(true, json!({"ok": true}), Some((100, 20, 0.001))),
        native_envelope(true, json!({"ok": true}), Some((100, 20, 0.001))),
    ];
    let w = make_walker(make_graph(yaml), results);
    let r1 = w.execute(json!({}), None).await;
    let r2 = w.execute(json!({}), None).await;

    assert_eq!(r1.cost.unwrap().input_tokens, 100);
    assert_eq!(
        r2.cost.unwrap().input_tokens,
        100,
        "second run must not include first run's cost"
    );
}

#[tokio::test]
async fn node_cache_does_not_cross_execution_authority_boundaries() {
    // A fresh graph execution must dispatch and bill independently. Cache
    // replay authority is intentionally scoped to one Walker::execute call.
    let yaml = r#"
version: "1.0.0"
category: cache_rebill
config:
  start: reason
  nodes:
    reason:
      node_type: action
      cache_result: true
      action: {item_id: "directive:test/reason"}
      assign: {got: "${result.outputs.recommendations}"}
      next: {type: unconditional, to: done}
    done:
      node_type: return
      output: "${state.got}"
"#;
    let env = native_envelope(
        true,
        json!({"recommendations": ["a", "b"]}),
        Some((100, 20, 0.001)),
    );
    let w1 = make_walker(make_graph(yaml), vec![env]);
    let r1 = w1.execute(json!({}), None).await;
    assert!(r1.success, "got: {:?}", r1.error);
    assert_eq!(r1.cost.expect("first run bills").input_tokens, 100);

    let second = native_envelope(
        true,
        json!({"recommendations": ["c", "d"]}),
        Some((100, 20, 0.001)),
    );
    let w2 = make_walker(make_graph(yaml), vec![second]);
    let r2 = w2.execute(json!({}), None).await;
    assert!(
        r2.success,
        "second dispatch should run; got: {:?}",
        r2.error
    );
    assert_eq!(r2.result, Some(json!(["c", "d"])));
    assert_eq!(r2.cost.expect("second run bills").input_tokens, 100);
}

#[tokio::test]
async fn config_state_seeds_initial_state() {
    // Authored `config.state` seeds graph state, so a foreach can run
    // off it with no caller `inject_state`.
    let yaml = r#"
version: "1.0.0"
category: test
config:
  start: iterate
  state:
    items: ["a", "b"]
  nodes:
    iterate:
      node_type: foreach
      over: "${state.items}"
      as: "elem"
      action: {item_id: "tool:test/echo", params: {value: "${elem}"}}
      collect: "results"
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;
    let graph = make_graph(yaml);
    let w = make_walker(graph, vec![json!({"v": "a"}), json!({"v": "b"})]);
    let result = w.execute(json!({}), None).await;

    assert!(result.success, "got: {:?}", result.error);
    let collected = result
        .state
        .get("results")
        .and_then(|v| v.as_array())
        .unwrap();
    assert_eq!(collected.len(), 2);
}

#[tokio::test]
async fn inject_state_overrides_config_state() {
    // Caller `inject_state` takes precedence over authored defaults.
    let yaml = r#"
version: "1.0.0"
category: test
config:
  start: done
  state:
    mode: "default"
  nodes:
    done:
      node_type: return
      output: "${state.mode}"
"#;
    let graph = make_graph(yaml);
    let w = make_walker(graph, vec![]);
    let result = w
        .execute(json!({"inject_state": {"mode": "override"}}), None)
        .await;

    assert!(result.success, "got: {:?}", result.error);
    assert_eq!(result.result, Some(json!("override")));
}

#[tokio::test]
async fn gate_node_conditional_routing() {
    let yaml = r#"
version: "1.0.0"
category: test
config:
  start: check
  state:
    mode: fast
  nodes:
    check:
      node_type: gate
      next:
        type: conditional
        branches:
          - when: 'state.mode == "fast"'
            to: fast_path
          - to: slow_path
    fast_path:
      node_type: return
      output: fast
    slow_path:
      node_type: return
      output: slow
"#;
    let graph = make_graph(yaml);
    let w = make_walker(graph, vec![]);
    let result = w.execute(json!({}), None).await;
    assert!(result.success);
    assert_eq!(result.result, Some(json!("fast")));
}

#[tokio::test]
async fn max_steps_exceeded() {
    let yaml = r#"
version: "1.0.0"
category: test
config:
  start: loop
  max_steps: 3
  nodes:
    loop:
      action: {item_id: "tool:test/noop"}
      next:
        type: unconditional
        to: loop
"#;
    let graph = make_graph(yaml);
    let w = make_walker(graph, vec![json!({}), json!({}), json!({})]);
    let result = w.execute(json!({}), None).await;
    assert!(!result.success);
    assert_eq!(result.status, GraphRunStatus::MaxStepsExceeded);
}

#[tokio::test]
async fn segment_steps_cuts_machine_continuation() {
    // With segment_steps=1 the first step advances and the per-thread budget
    // is hit before a terminal node — the walker cuts a machine continuation
    // (request_continuation succeeds) and settles `continued` rather than
    // running on toward max_steps. The successor would resume from the
    // checkpoint the last commit_step wrote.
    let yaml = r#"
version: "1.0.0"
category: test
config:
  start: loop
  max_steps: 100
  segment_steps: 1
  nodes:
    loop:
      action: {item_id: "tool:test/noop"}
      next:
        type: unconditional
        to: loop
"#;
    let graph = make_graph(yaml);
    let w = make_walker(graph, vec![json!({})]);
    let result = w.execute(json!({}), None).await;
    assert_eq!(result.status, GraphRunStatus::Continued, "got: {result:?}");
    assert!(!result.success);
    assert_eq!(result.steps, 1, "one step ran before the segment cut");
}

#[test]
fn validation_rejects_missing_start() {
    let yaml = r#"
version: "1.0.0"
category: test
config:
  start: nonexistent
  nodes:
    step1:
      action: {item_id: "tool:test/echo"}
"#;
    let graph = make_graph(yaml);
    let w = make_walker(graph, vec![]);
    let result = w.validate();
    assert!(!result.success);
}

#[tokio::test]
async fn foreach_sequential_collects_results() {
    let yaml = r#"
version: "1.0.0"
category: test
config:
  start: iterate
  nodes:
    iterate:
      node_type: foreach
      over: "${state.items}"
      as: "elem"
      action: {item_id: "tool:test/echo", params: {value: "${elem}"}}
      collect: "results"
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;
    let graph = make_graph(yaml);
    let w = make_walker(
        graph,
        vec![
            json!({"value": "a"}),
            json!({"value": "b"}),
            json!({"value": "c"}),
        ],
    );
    let result = w
        .execute(json!({"inject_state": {"items": ["a", "b", "c"]}}), None)
        .await;
    assert!(result.success);
    let results = result
        .state
        .get("results")
        .and_then(|v| v.as_array())
        .unwrap();
    assert_eq!(results.len(), 3);
}

#[tokio::test]
async fn foreach_lexical_variable_never_deletes_same_named_state() {
    for parallel in [false, true] {
        let yaml = format!(
            r#"
version: "1.0.0"
category: test
config:
  start: iterate
  nodes:
    iterate:
      node_type: foreach
      over: "${{state.items}}"
      as: item
      parallel: {parallel}
      action: {{item_id: "tool:test/echo", params: {{value: "${{item}}"}}}}
      collect: results
      next: {{type: unconditional, to: done}}
    done:
      node_type: return
"#
        );
        for items in [json!([]), json!([1])] {
            let graph = make_graph(&yaml);
            let dispatches = if items.as_array().unwrap().is_empty() {
                Vec::new()
            } else {
                vec![json!({"value": 1})]
            };
            let w = make_walker(graph, dispatches);
            let result = w
                .execute(
                    json!({"inject_state": {"items": items, "item": "persistent"}}),
                    None,
                )
                .await;

            assert!(result.success, "parallel={parallel}: {result:?}");
            assert_eq!(result.state["item"], json!("persistent"));
        }
    }
}

#[tokio::test]
async fn foreach_graph_env_preflight_precedes_over_and_lifecycle() {
    let missing = "RYEOS_TEST_FOREACH_GRAPH_ENV_DEFINITELY_MISSING";
    let yaml = format!(
        r#"
version: "1.0.0"
category: test
config:
  start: iterate
  env_requires: [{missing}]
  nodes:
    iterate:
      node_type: foreach
      over: "${{state.not_present}}"
      as: elem
      action: {{item_id: "tool:test/echo", params: {{value: "${{elem}}"}}}}
      collect: results
    done:
      node_type: return
"#
    );
    let (walker, recorder) = make_recording_walker(make_graph(&yaml), Vec::new(), None);

    let result = walker
        .execute(json!({}), Some("gr-foreach-env-preflight".to_string()))
        .await;

    assert_eq!(result.status, GraphRunStatus::Error);
    let error = result.error.unwrap_or_default();
    assert!(
        error.contains("env preflight failed") && error.contains(missing),
        "graph env preflight must win over `over` evaluation: {error}"
    );
    assert_eq!(recorder.dispatch_count(), 0);
    let emitted_foreach_started = recorder
        .recorded_events()
        .iter()
        .any(|(_, event, _, _)| event == RuntimeEventType::GraphForeachStarted.as_str());
    assert!(!emitted_foreach_started);
}

fn foreach_graph_yaml(parallel: bool, on_error: &str) -> String {
    format!(
        r#"
version: "1.0.0"
category: test
config:
  start: iterate
  on_error: {on_error}
  nodes:
    iterate:
      node_type: foreach
      over: "${{state.items}}"
      as: "elem"
      parallel: {parallel}
      action: {{item_id: "tool:test/echo", params: {{value: "${{elem}}"}}}}
      collect: "results"
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#
    )
}

#[tokio::test]
async fn foreach_item_failure_on_error_fail_fails_run() {
    // A failed subprocess inside a foreach with on_error: fail must
    // fail the whole run — not complete_with_errors.
    for parallel in [false, true] {
        let graph = make_graph(&foreach_graph_yaml(parallel, "fail"));
        let w = make_walker(
            graph,
            vec![
                json!({"value": "a"}),
                json!({
                    "outcome_code": "exit:1", "result": null,
                    "error": {"exit_code": 1, "stderr": "boom"}
                }),
            ],
        );
        let result = w
            .execute(json!({"inject_state": {"items": ["a", "b"]}}), None)
            .await;
        assert!(!result.success, "parallel={parallel}: should fail");
        assert_eq!(result.status, GraphRunStatus::Error, "parallel={parallel}");
        let err = result.error.unwrap_or_default();
        assert!(err.contains("boom"), "parallel={parallel}: got {err}");
        assert_no_raw_template(&result.state);
    }
}

#[tokio::test]
async fn foreach_item_failure_on_error_continue_records_errors() {
    for parallel in [false, true] {
        let graph = make_graph(&foreach_graph_yaml(parallel, "continue"));
        let w = make_walker(
            graph,
            vec![
                json!({"value": "a"}),
                json!({
                    "outcome_code": "exit:1", "result": null,
                    "error": {"exit_code": 1, "stderr": "boom"}
                }),
            ],
        );
        let result = w
            .execute(json!({"inject_state": {"items": ["a", "b"]}}), None)
            .await;
        assert!(result.success, "parallel={parallel}");
        assert_eq!(
            result.status,
            GraphRunStatus::CompletedWithErrors,
            "parallel={parallel}"
        );
        assert_eq!(result.errors_suppressed, Some(1), "parallel={parallel}");
        let errors = result.errors.unwrap();
        assert!(errors[0].error.contains("boom"), "parallel={parallel}");
        // collect aligns: [a-result, null]
        let collected = result
            .state
            .get("results")
            .and_then(|v| v.as_array())
            .unwrap();
        assert_eq!(collected.len(), 2, "parallel={parallel}");
        assert_eq!(collected[1], Value::Null, "parallel={parallel}");
        assert_no_raw_template(&result.state);
    }
}

#[tokio::test]
async fn foreach_parallel_expression_failure_not_dispatched() {
    // Parallel foreach whose action template can't resolve must NOT
    // dispatch a raw `${...}` — the item errors and yields a null.
    let yaml = r#"
version: "1.0.0"
category: test
config:
  start: iterate
  on_error: continue
  nodes:
    iterate:
      node_type: foreach
      over: "${state.items}"
      as: "elem"
      parallel: true
      action: {item_id: "tool:test/echo", params: {value: "${elem.missing.deep}"}}
      collect: "results"
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;
    let graph = make_graph(yaml);
    // No mock results queued: if a raw template were dispatched the
    // mock would still answer, but the item must be a recorded error.
    let w = make_walker(graph, vec![]);
    let result = w
        .execute(json!({"inject_state": {"items": ["a"]}}), None)
        .await;
    assert!(result.success);
    assert_eq!(result.errors_suppressed, Some(1));
    assert!(result.errors.unwrap()[0]
        .error
        .contains("expression evaluation"));
    assert_no_raw_template(&result.state);
}

#[tokio::test]
async fn foreach_sequential_assign_persists_to_state() {
    // Foreach `assign` must reach the committed final state.
    let yaml = r#"
version: "1.0.0"
category: test
config:
  start: iterate
  nodes:
    iterate:
      node_type: foreach
      over: "${state.items}"
      as: "elem"
      action: {item_id: "tool:test/echo", params: {value: "${elem}"}}
      assign: {last_value: "${result.value}"}
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;
    let graph = make_graph(yaml);
    let w = make_walker(graph, vec![json!({"value": "a"}), json!({"value": "b"})]);
    let result = w
        .execute(json!({"inject_state": {"items": ["a", "b"]}}), None)
        .await;
    assert!(result.success, "got: {:?}", result.error);
    assert_eq!(
        result.state.get("last_value").and_then(|v| v.as_str()),
        Some("b"),
        "foreach assign must persist (last item wins)"
    );
}

#[tokio::test]
async fn foreach_sequential_iteration_reads_prior_successful_delta() {
    let graph = make_graph(
        r#"
version: "1.0.0"
category: test
config:
  start: iterate
  state: {count: 0, items: [1, 2]}
  nodes:
    iterate:
      node_type: foreach
      over: "${state.items}"
      as: "elem"
      action: {item_id: "tool:test/echo", params: {value: "${elem}"}}
      assign: {count: "${state.count + elem}"}
      collect: results
      next:
        type: conditional
        branches:
          - when: 'state.count == 3 && length(state.results) == 2'
            to: done
          - to: wrong
    done:
      node_type: return
    wrong:
      action: {item_id: "tool:test/wrong"}
      assign: {wrong_path: true}
"#,
    );
    let result = make_walker(graph, vec![json!({"value": 1}), json!({"value": 2})])
        .execute(json!({}), None)
        .await;

    assert!(result.success);
    assert_eq!(result.state["count"], json!(3));
    assert_eq!(result.state["results"], json!([{"value": 1}, {"value": 2}]));
    assert!(result.state.get("elem").is_none());
    assert!(result.state.get("wrong_path").is_none());
}

#[tokio::test]
async fn foreach_continue_failure_contributes_no_delta_to_later_item() {
    let graph = make_graph(
        r#"
version: "1.0.0"
category: test
config:
  start: iterate
  on_error: continue
  state: {count: 0, items: [1, 2, 3]}
  nodes:
    iterate:
      node_type: foreach
      over: "${state.items}"
      as: "elem"
      action: {item_id: "tool:test/echo", params: {value: "${elem}"}}
      assign: {count: "${state.count + elem}"}
      collect: results
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#,
    );
    let result = make_walker(
        graph,
        vec![
            json!({"value": 1}),
            json!({
                "outcome_code": "exit:1",
                "result": null,
                "error": {"exit_code": 1, "stderr": "boom"},
            }),
            json!({"value": 3}),
        ],
    )
    .execute(json!({}), None)
    .await;

    assert!(result.success);
    assert_eq!(result.status, GraphRunStatus::CompletedWithErrors);
    assert_eq!(result.state["count"], json!(4));
    assert_eq!(
        result.state["results"],
        json!([{"value": 1}, null, {"value": 3}])
    );
    assert_eq!(result.errors_suppressed, Some(1));
}

#[tokio::test]
async fn foreach_assign_failure_under_continue_adds_null_without_delta() {
    // Action succeeds but `assign` references a missing field. Sequential
    // continue records a null slot and contributes no delta for the item.
    // Parallel assignment is rejected at graph load and has separate
    // validation coverage.
    let graph = make_graph(
        r#"
version: "1.0.0"
category: test
config:
  start: iterate
  on_error: continue
  nodes:
    iterate:
      node_type: foreach
      over: "${state.items}"
      as: "elem"
      action: {item_id: "tool:test/echo", params: {value: "${elem}"}}
      assign: {captured: "${result.missing.deep}"}
      collect: "results"
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#,
    );
    let result = make_walker(graph, vec![json!({"value": "a"})])
        .execute(json!({"inject_state": {"items": ["a"]}}), None)
        .await;

    assert!(result.success);
    assert_eq!(result.status, GraphRunStatus::CompletedWithErrors);
    assert_eq!(result.errors_suppressed, Some(1));
    assert_eq!(result.state["results"], json!([null]));
    assert!(result.state.get("captured").is_none());
}

#[tokio::test]
async fn foreach_item_failure_redirects_to_handler() {
    // A node-level `on_error: <handler>` redirects the whole foreach
    // to the handler node on item failure (no suppressed errors).
    let yaml = r#"
version: "1.0.0"
category: test
config:
  start: iterate
  on_error: fail
  nodes:
    iterate:
      node_type: foreach
      over: "${state.items}"
      as: "elem"
      action: {item_id: "tool:test/echo", params: {value: "${elem}"}}
      on_error: handler
      next:
        type: unconditional
        to: done
    handler:
      node_type: return
    done:
      node_type: return
"#;
    let graph = make_graph(yaml);
    let w = make_walker(
        graph,
        vec![json!({
            "outcome_code": "exit:1", "result": null,
            "error": {"exit_code": 1, "stderr": "boom"}
        })],
    );
    let result = w
        .execute(json!({"inject_state": {"items": ["a"]}}), None)
        .await;
    assert!(result.success, "redirect handler should complete the run");
    assert_eq!(result.status, GraphRunStatus::Completed);
    assert_eq!(result.errors_suppressed, None);
}

#[tokio::test]
async fn foreach_redirect_rolls_back_prior_delta_and_collect() {
    let graph = make_graph(
        r#"
version: "1.0.0"
category: test
config:
  start: iterate
  state: {count: 0, items: [1, 2]}
  nodes:
    iterate:
      node_type: foreach
      over: "${state.items}"
      as: elem
      action: {item_id: "tool:test/echo"}
      assign: {count: "${state.count + 1}"}
      collect: results
      on_error: recover
      next:
        type: unconditional
        to: wrong
    recover:
      node_type: return
    wrong:
      action: {item_id: "tool:test/wrong"}
      assign: {wrong_path: true}
"#,
    );
    let result = make_walker(
        graph,
        vec![
            json!({"value": 1}),
            json!({
                "outcome_code": "exit:1",
                "result": null,
                "error": {"exit_code": 1, "stderr": "boom"},
            }),
        ],
    )
    .execute(json!({}), None)
    .await;

    assert!(result.success);
    assert_eq!(result.state["count"], json!(0));
    assert!(result.state.get("results").is_none());
    assert!(result.state.get("wrong_path").is_none());
}

#[tokio::test]
async fn foreach_final_branch_error_rolls_back_candidate() {
    let graph = make_graph(
        r#"
version: "1.0.0"
category: test
config:
  start: iterate
  state: {count: 0, items: [1, 2]}
  nodes:
    iterate:
      node_type: foreach
      over: "${state.items}"
      as: elem
      action: {item_id: "tool:test/echo"}
      assign: {count: "${state.count + elem}"}
      collect: results
      on_error: recover
      next:
        type: conditional
        branches:
          - when: '1 / inputs.zero > 0'
            to: wrong
          - to: wrong
    recover:
      node_type: return
    wrong:
      action: {item_id: "tool:test/wrong"}
      assign: {wrong_path: true}
"#,
    );
    let result = make_walker(graph, vec![json!({"value": 1}), json!({"value": 2})])
        .execute(json!({"inputs": {"zero": 0}}), None)
        .await;

    assert!(result.success);
    assert_eq!(result.state["count"], json!(0));
    assert!(result.state.get("results").is_none());
    assert!(result.state.get("wrong_path").is_none());
}

#[tokio::test]
async fn on_error_continue_mode() {
    let yaml = r#"
version: "1.0.0"
category: test
config:
  start: step1
  on_error: continue
  nodes:
    step1:
      action: {item_id: "tool:test/fail"}
      next:
        type: unconditional
        to: step2
    step2:
      node_type: return
"#;
    let graph = make_graph(yaml);
    let w = make_walker(
        graph,
        vec![
            json!({"outcome_code": "exit:1", "result": null, "error": {"exit_code": 1, "stderr": "forced failure"}}),
        ],
    );
    let result = w.execute(json!({}), None).await;
    assert!(result.success);
    assert_eq!(result.status, GraphRunStatus::CompletedWithErrors);
    assert_eq!(result.errors_suppressed, Some(1));
}

#[test]
fn cache_result_replays_within_one_execution_cache() {
    let cache = NodeCache::new("cache-test-unique-sequential");
    let action = json!({"item_id": "tool:test/echo", "ref_bindings": {}});
    let key = compute_cache_key(
        "definition-hash",
        "cache-test-unique-sequential",
        "step1",
        &action,
    )
    .unwrap();

    assert!(cache.lookup(&key).is_none());

    let val = json!({"msg": "cached"});
    cache.store(&key, &val);
    let cached = cache.lookup(&key).unwrap();
    assert_eq!(cached, val);
}

// ── warning accumulator ─────────────────────────────────────────
//
// `record_callback_warning` MUST push exactly one labelled string per
// failed callback append, and `take_warnings()` MUST drain the
// buffer atomically. Together they ensure every callback failure at
// an event-emit site is surfaced (via the daemon's
// `RuntimeResult.warnings` field) rather than dropped. These tests
// pin that wire-level drift.

#[test]
fn record_callback_warning_pushes_when_result_is_err() {
    let yaml = r#"
version: "1.0.0"
category: test
config:
  start: done
  nodes:
    done:
      node_type: return
"#;
    let graph = make_graph(yaml);
    let w = make_walker(graph, vec![]);
    assert!(w.take_warnings().is_empty());

    w.record_callback_warning(
        "graph_step_started",
        Err(anyhow::anyhow!("event-store rejected unknown_event_type")),
    );

    let drained = w.take_warnings();
    assert_eq!(drained.len(), 1);
    assert!(
        drained[0].contains("graph_step_started") && drained[0].contains("event-store rejected"),
        "warning must carry both the event label and the underlying error; got: {:?}",
        drained
    );
    // Drained: a second take must return empty.
    assert!(w.take_warnings().is_empty());
}

#[test]
fn record_callback_warning_no_op_when_result_is_ok() {
    let yaml = r#"
version: "1.0.0"
category: test
config:
  start: done
  nodes:
    done:
      node_type: return
"#;
    let graph = make_graph(yaml);
    let w = make_walker(graph, vec![]);

    w.record_callback_warning("tool_call_start", Ok(()));
    w.record_callback_warning("tool_call_result", Ok(()));

    assert!(
        w.take_warnings().is_empty(),
        "Ok results must NOT produce warnings"
    );
}

#[test]
fn record_callback_warning_accumulates_multiple_errors() {
    let yaml = r#"
version: "1.0.0"
category: test
config:
  start: done
  nodes:
    done:
      node_type: return
"#;
    let graph = make_graph(yaml);
    let w = make_walker(graph, vec![]);

    w.record_callback_warning("graph_started", Err(anyhow::anyhow!("a")));
    w.record_callback_warning("graph_step_started", Err(anyhow::anyhow!("b")));
    w.record_callback_warning("graph_completed", Err(anyhow::anyhow!("c")));

    let drained = w.take_warnings();
    assert_eq!(drained.len(), 3);
    assert!(drained[0].contains("graph_started"));
    assert!(drained[1].contains("graph_step_started"));
    assert!(drained[2].contains("graph_completed"));
}

// ── F3 tests: commit_step behavior ──────────────────────────────

#[test]
fn step_outcome_action_ok_captures_fields() {
    let outcome = StepOutcome::ActionOk(Box::new(ActionOkOutcome {
        item_id: "tool:test/echo".to_string(),
        result: json!({"msg": "hello"}),
        assign: None,
        next: Some("done".to_string()),
        child_thread_id: None,
        cache_hit: false,
        cache_write_key: None,
        elapsed_ms: 42,
        cost: None,
    }));
    match outcome {
        StepOutcome::ActionOk(outcome) => {
            assert_eq!(outcome.item_id, "tool:test/echo");
            assert_eq!(outcome.next.as_deref(), Some("done"));
            assert_eq!(outcome.elapsed_ms, 42);
        }
        _ => panic!("expected ActionOk"),
    }
}

#[test]
fn step_outcome_leaf_soft_error_captures_error() {
    let outcome = StepOutcome::LeafSoftError(LeafSoftErrorOutcome {
        item_id: "tool:test/fail".to_string(),
        error: "boom".to_string(),
        next_on_error: NextOnError::PolicyFail,
        elapsed_ms: 10,
        cost: None,
        observation: None,
    });
    match outcome {
        StepOutcome::LeafSoftError(outcome) => {
            assert_eq!(outcome.error, "boom");
            assert!(matches!(outcome.next_on_error, NextOnError::PolicyFail));
        }
        _ => panic!("expected LeafSoftError"),
    }
}

#[test]
fn step_outcome_dispatch_hard_error_captures_error() {
    let outcome = StepOutcome::DispatchHardError(DispatchHardErrorOutcome {
        item_id: None,
        error: "permission denied".to_string(),
        next_on_error: NextOnError::Redirect("error_handler".to_string()),
        elapsed_ms: 1,
        cost: None,
    });
    match outcome {
        StepOutcome::DispatchHardError(outcome) => {
            assert!(outcome.item_id.is_none());
            assert_eq!(outcome.error, "permission denied");
            assert!(matches!(outcome.next_on_error, NextOnError::Redirect(_)));
        }
        _ => panic!("expected DispatchHardError"),
    }
}

#[test]
fn step_outcome_gate_taken_captures_target() {
    let outcome = StepOutcome::GateTaken(GateTakenOutcome {
        target: Some("fast_path".to_string()),
    });
    match outcome {
        StepOutcome::GateTaken(outcome) => {
            assert_eq!(outcome.target.as_deref(), Some("fast_path"));
        }
        _ => panic!("expected GateTaken"),
    }
}

#[test]
fn step_outcome_foreach_done_captures_count() {
    let outcome = StepOutcome::ForeachDone(Box::new(ForeachDoneOutcome {
        results: vec![json!(1), json!(2)],
        statuses: vec![GraphToolCallStatus::Ok, GraphToolCallStatus::Ok],
        total_items: 2,
        collect_key: Some("items".to_string()),
        assign_delta: json!({}),
        errors: Vec::new(),
        next: Some("done".to_string()),
        item_id: "tool:test/echo".to_string(),
        cost: None,
        observations: Vec::new(),
    }));
    match outcome {
        StepOutcome::ForeachDone(outcome) => {
            assert_eq!(outcome.next.as_deref(), Some("done"));
            assert_eq!(outcome.collect_key.as_deref(), Some("items"));
        }
        _ => panic!("expected ForeachDone"),
    }
}

#[test]
fn step_outcome_terminal_captures_status() {
    let outcome = StepOutcome::Terminal(TerminalOutcome {
        status: GraphRunStatus::MaxStepsExceeded,
        error: Some("hit limit".to_string()),
        origin: TerminalOrigin::RunControl,
        output: None,
    });
    match outcome {
        StepOutcome::Terminal(outcome) => {
            assert_eq!(outcome.status, GraphRunStatus::MaxStepsExceeded);
            assert_eq!(outcome.error.as_deref(), Some("hit limit"));
        }
        _ => panic!("expected Terminal"),
    }
}

// ── F3 commit_step tests: event ordering + checkpoint writes ─────

/// A mock callback client that records every `append_event` call
/// so tests can assert the exact event sequence produced by
/// `commit_step`.
struct RecordingMockClient {
    dispatch_results: Mutex<Vec<Value>>,
    events: Mutex<Vec<(String, String, Value, String)>>,
    /// (thread_id, status) pairs from finalize_thread calls.
    finalizations: Mutex<Vec<(String, ryeos_runtime::ThreadTerminalStatus)>>,
    /// `TerminalCompletion.cost` (raw JSON) from finalize_thread calls.
    finalize_costs: Mutex<Vec<Option<Value>>>,
    /// Collected artifacts from publish_artifact calls.
    artifacts: Mutex<Vec<Value>>,
    /// Recorded `spawn_follow_child` requests (for follow idempotency tests).
    follow_requests: Mutex<Vec<ryeos_runtime::callback::SpawnFollowChildRequest>>,
    /// When true, `spawn_follow_child` returns an error (failed-handoff test).
    follow_should_fail: bool,
    /// Count of `dispatch_action` calls (to prove a follow resume re-dispatches
    /// nothing).
    dispatch_count: Mutex<usize>,
}

impl RecordingMockClient {
    fn new(dispatch_results: Vec<Value>) -> Self {
        Self {
            dispatch_results: Mutex::new(dispatch_results),
            events: Mutex::new(Vec::new()),
            finalizations: Mutex::new(Vec::new()),
            finalize_costs: Mutex::new(Vec::new()),
            artifacts: Mutex::new(Vec::new()),
            follow_requests: Mutex::new(Vec::new()),
            follow_should_fail: false,
            dispatch_count: Mutex::new(0),
        }
    }

    fn recorded_events(&self) -> Vec<(String, String, Value, String)> {
        self.events.lock().unwrap().clone()
    }

    fn dispatch_count(&self) -> usize {
        *self.dispatch_count.lock().unwrap()
    }

    fn recorded_follow_requests(&self) -> Vec<ryeos_runtime::callback::SpawnFollowChildRequest> {
        self.follow_requests.lock().unwrap().clone()
    }

    fn recorded_finalizations(&self) -> Vec<(String, ryeos_runtime::ThreadTerminalStatus)> {
        self.finalizations.lock().unwrap().clone()
    }
}

#[async_trait]
impl ryeos_runtime::callback::RuntimeCallbackAPI for RecordingMockClient {
    async fn dispatch_action(
        &self,
        _request: DispatchActionRequest,
    ) -> Result<Value, CallbackError> {
        *self.dispatch_count.lock().unwrap() += 1;
        let mut results = self.dispatch_results.lock().unwrap();
        if results.is_empty() {
            Ok(json!({"thread": {}, "result": {}}))
        } else {
            let result = results.remove(0);
            if result.get("__retryable_dispatch_error").is_some() {
                Err(CallbackError::ActionFailed {
                    code: "service_unavailable".to_string(),
                    message: "simulated transient dispatch failure".to_string(),
                    retryable: true,
                })
            } else {
                Ok(json!({"thread": {}, "result": result}))
            }
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
        thread_id: &str,
        completion: ryeos_runtime::TerminalCompletion,
    ) -> Result<Value, CallbackError> {
        self.finalize_costs
            .lock()
            .unwrap()
            .push(completion.cost.clone());
        self.finalizations
            .lock()
            .unwrap()
            .push((thread_id.to_string(), completion.status));
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
        thread_id: &str,
        event_type: &str,
        payload: Value,
        storage_class: &str,
    ) -> Result<Value, CallbackError> {
        self.events.lock().unwrap().push((
            thread_id.to_string(),
            event_type.to_string(),
            payload,
            storage_class.to_string(),
        ));
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
    async fn bundle_events_read_chain(&self, _: &str, _: Value) -> Result<Value, CallbackError> {
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
    async fn publish_artifact(&self, _: &str, artifact: Value) -> Result<Value, CallbackError> {
        self.artifacts.lock().unwrap().push(artifact);
        Ok(json!({}))
    }
    async fn get_facets(&self, _: &str) -> Result<Value, CallbackError> {
        Ok(json!({}))
    }
    async fn spawn_follow_child(
        &self,
        request: ryeos_runtime::callback::SpawnFollowChildRequest,
    ) -> Result<Value, CallbackError> {
        self.follow_requests.lock().unwrap().push(request);
        if self.follow_should_fail {
            Err(CallbackError::ActionFailed {
                code: "test".to_string(),
                message: "simulated daemon follow failure".to_string(),
                retryable: false,
            })
        } else {
            Ok(json!({ "phase": "waiting" }))
        }
    }
}

fn make_recording_callback(results: Vec<Value>) -> (CallbackClient, Arc<RecordingMockClient>) {
    let inner: Arc<RecordingMockClient> = Arc::new(RecordingMockClient::new(results));
    let client = CallbackClient::from_inner(
        inner.clone(),
        "thread-test",
        "/tmp/test-project",
        "tat-test",
    );
    (client, inner)
}

fn make_recording_walker(
    graph: GraphDefinition,
    results: Vec<Value>,
    checkpoint_dir: Option<&std::path::Path>,
) -> (Walker, Arc<RecordingMockClient>) {
    let (client, recorder) = make_recording_callback(results);
    let checkpoint = checkpoint_dir.map(|d| CheckpointWriter::new(d.to_path_buf()));
    let w = Walker::new(
        graph,
        "/tmp/test-project".to_string(),
        "thread-test".to_string(),
        client,
        checkpoint,
    );
    (w, recorder)
}

// ── §A per-step retry ────────────────────────────────────────────

const RETRY_YAML: &str = r#"
version: "1.0.0"
category: test
config:
  start: flaky
  nodes:
    flaky:
      action: {item_id: "tool:test/flaky"}
      retry: {attempts: 3, backoff_ms: 1}
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;

fn subprocess_failure() -> Value {
    json!({
        "outcome_code": "exit:1",
        "result": null,
        "error": {"exit_code": 1, "stderr": "boom"},
        "artifacts": [],
    })
}

fn subprocess_success() -> Value {
    json!({"outcome_code": null, "result": {"ok": true}, "error": null, "artifacts": []})
}

fn retryable_dispatch_failure() -> Value {
    json!({"__retryable_dispatch_error": true})
}

#[tokio::test]
async fn retry_redispatches_until_success() {
    // First dispatch fails, the retry re-dispatches and succeeds. The
    // failed attempt consumed a walker step, so `done` is reached at step 2.
    let graph = make_graph(RETRY_YAML);
    let w = make_walker(
        graph,
        vec![retryable_dispatch_failure(), subprocess_success()],
    );
    let result = w.execute(json!({}), None).await;
    assert!(result.success, "retry should recover: {result:?}");
    assert_eq!(result.status, GraphRunStatus::Completed);
    assert_eq!(
        result.steps, 3,
        "one failed attempt + successful re-dispatch + return = 3 completed steps"
    );
    // A recovered retry leaves no suppressed error behind.
    assert!(result.errors.is_none(), "recovered retry records no error");
}

#[tokio::test]
async fn retry_exhausts_then_routes_on_error() {
    // attempts:2 → two dispatches, both fail, then `on_error` redirects to
    // the recover return node. The retry is bounded — it does not loop
    // forever on a persistent failure.
    let yaml = r#"
version: "1.0.0"
category: test
config:
  start: flaky
  nodes:
    flaky:
      action: {item_id: "tool:test/flaky"}
      retry: {attempts: 2, backoff_ms: 1}
      on_error: recover
    recover:
      node_type: return
"#;
    let graph = make_graph(yaml);
    let w = make_walker(
        graph,
        vec![retryable_dispatch_failure(), retryable_dispatch_failure()],
    );
    let result = w.execute(json!({}), None).await;
    assert_eq!(result.status, GraphRunStatus::Completed);
    assert_eq!(
        result.steps, 3,
        "attempt 1 (retry) + attempt 2 (exhausted → redirect) + return = 3 steps"
    );
}

#[tokio::test]
async fn retry_emits_braid_visible_retry_event() {
    // A re-attempt emits exactly one graph_node_retry milestone carrying the
    // attempt number, the total, and the backoff — indexed (braid-visible).
    let graph = make_graph(RETRY_YAML);
    let (w, rec) = make_recording_walker(
        graph,
        vec![retryable_dispatch_failure(), subprocess_success()],
        None,
    );
    let result = w.execute(json!({}), Some("gr-retry".to_string())).await;
    assert!(result.success, "retry should recover: {result:?}");

    let events = rec.recorded_events();
    let retries: Vec<_> = events
        .iter()
        .filter(|(_, ty, _, _)| ty == "graph_node_retry")
        .collect();
    assert_eq!(
        retries.len(),
        1,
        "one failed attempt → exactly one retry event; events={events:#?}"
    );
    let (_, _, payload, storage_class) = retries[0];
    assert_eq!(payload["attempt"], 1);
    assert_eq!(payload["attempts"], 3);
    assert_eq!(payload["delay_ms"], 1);
    assert_eq!(payload["node"], "flaky");
    assert_eq!(
        storage_class, "indexed",
        "graph_node_retry is an indexed milestone"
    );
}

#[tokio::test]
async fn retry_fires_typed_graph_step_completed_hook() {
    let graph = make_graph(
        r#"
version: "1.0.0"
category: test
config:
  start: flaky
  hooks:
    - id: observe_retry
      event: graph_step_completed
      condition: 'status == "retry"'
      action: {item_id: "tool:test/observe_retry"}
  nodes:
    flaky:
      action: {item_id: "tool:test/flaky"}
      retry: {attempts: 2, backoff_ms: 1}
      next: {type: unconditional, to: done}
    done:
      node_type: return
"#,
    );
    let (walker, recorder) = make_recording_walker(
        graph,
        vec![
            retryable_dispatch_failure(),
            subprocess_success(),
            subprocess_success(),
        ],
        None,
    );

    let result = walker
        .execute(json!({}), Some("gr-retry-hook".to_string()))
        .await;

    assert!(result.success, "retry should recover: {result:?}");
    assert_eq!(
        recorder.dispatch_count(),
        3,
        "failed node attempt + retry observer hook + successful node attempt"
    );
}

#[tokio::test]
async fn retry_resumes_with_persisted_attempt_count() {
    // The attempt counter rides the checkpoint: a walker resumed with
    // `retry_attempt: 1` on a node whose only remaining attempt fails routes
    // straight to on_error — it does NOT restart the count and retry again.
    let yaml = r#"
version: "1.0.0"
category: test
config:
  start: flaky
  nodes:
    flaky:
      action: {item_id: "tool:test/flaky"}
      retry: {attempts: 2, backoff_ms: 1}
      on_error: recover
    recover:
      node_type: return
"#;
    let graph = make_graph(yaml);
    let params = strict_resume_params(&graph, "flaky", 5, json!({}), "gr-resumed", None, None, 1);
    let (w, rec) = make_recording_walker(graph, vec![retryable_dispatch_failure()], None);
    // Resume after attempt 1 already failed in the prior segment.
    let result = w.execute(params, None).await;
    assert_eq!(
        result.status,
        GraphRunStatus::Completed,
        "recover is terminal"
    );
    let events = rec.recorded_events();
    let retries = events
        .iter()
        .filter(|(_, ty, _, _)| ty == "graph_node_retry")
        .count();
    assert_eq!(
        retries, 0,
        "the persisted count was exhausted on the single remaining attempt — no new retry; \
         events={events:#?}"
    );
}

#[tokio::test]
async fn malformed_injected_resume_fails_without_cold_start_dispatch() {
    let graph = make_graph(
        r#"
version: "1.0.0"
category: test
config:
  start: act
  nodes:
    act:
      action: {item_id: "tool:test/must-not-run"}
      next: {type: unconditional, to: done}
    done:
      node_type: return
"#,
    );
    let mut params = strict_resume_params(
        &graph,
        "act",
        3,
        json!({"preserved": true}),
        "gr-strict-resume",
        None,
        None,
        0,
    );
    params["resume_state"]
        .as_object_mut()
        .unwrap()
        .remove("definition_hash");
    let (walker, recorder) = make_recording_walker(graph, vec![subprocess_success()], None);

    let result = walker.execute(params, Some("gr-outer".to_string())).await;

    assert_eq!(result.status, GraphRunStatus::Error, "{result:?}");
    assert!(
        result
            .error
            .as_deref()
            .unwrap_or_default()
            .contains(crate::resume::RESTART_REQUIRED),
        "{result:?}"
    );
    assert_eq!(
        recorder.dispatch_count(),
        0,
        "a present malformed resume must not execute the graph's cold-start node"
    );
}

#[tokio::test]
async fn deterministic_leaf_failure_does_not_consume_retry_budget() {
    let graph = make_graph(RETRY_YAML);
    let (w, recorder) = make_recording_walker(graph, vec![subprocess_failure()], None);
    let result = w
        .execute(json!({}), Some("gr-nonretryable".to_string()))
        .await;
    assert_eq!(result.status, GraphRunStatus::Error);
    assert_eq!(recorder.dispatch_count(), 1);
    assert!(recorder
        .recorded_events()
        .iter()
        .all(|(_, event_type, _, _)| event_type != "graph_node_retry"));
}

// ── §B2 graph hooks ──────────────────────────────────────────────

const HOOK_YAML: &str = r#"
version: "1.0.0"
category: test
config:
  start: done
  hooks:
    - id: notify
      event: graph_completed
      action: {item_id: "tool:test/notify", params: {}}
  nodes:
    done:
      node_type: return
"#;

#[tokio::test]
async fn graph_completed_hook_dispatches_through_callback() {
    // An authored graph_completed hook fires at the terminal, dispatching
    // its action through the same callback a node action uses.
    let graph = make_graph(HOOK_YAML);
    let (w, rec) = make_recording_walker(graph, vec![], None);
    let result = w.execute(json!({}), Some("gr-hook".to_string())).await;
    assert!(result.success, "graph completes: {result:?}");
    assert_eq!(
        rec.dispatch_count(),
        1,
        "the graph_completed hook must dispatch exactly once"
    );
    assert!(
        w.take_warnings().is_empty(),
        "a successful hook records no warning"
    );
}

#[tokio::test]
async fn graph_completed_hook_cost_is_accounted_and_attributed() {
    let graph = make_graph(HOOK_YAML);
    let hook_result = json!({
        "success": true,
        "status": "completed",
        "result": {"notified": true},
        "outputs": null,
        "warnings": [],
        "cost": {
            "input_tokens": 4,
            "output_tokens": 6,
            "total_usd": 0.25
        }
    });
    let (w, _rec) = make_recording_walker(graph, vec![hook_result], None);
    let result = w.execute(json!({}), Some("gr-hook-cost".to_string())).await;

    assert!(result.success, "graph completes: {result:?}");
    let cost = result.cost.expect("hook cost contributes to graph rollup");
    assert_eq!(cost.input_tokens, 4);
    assert_eq!(cost.output_tokens, 6);
    assert_eq!(result.hook_costs.len(), 1);
    assert_eq!(result.hook_costs[0].event, RuntimeEventType::GraphCompleted);
    assert_eq!(result.hook_costs[0].step, Some(1));
    assert!(result.node_costs.is_empty());
}

#[tokio::test]
async fn failing_hook_warns_but_does_not_fail_graph() {
    // A hook child that fails is a recorded warning, never a graph failure —
    // graph hooks are observers.
    let graph = make_graph(HOOK_YAML);
    let fail = json!({
        "outcome_code": "exit:1",
        "result": null,
        "error": {"exit_code": 1, "stderr": "hook boom"},
        "artifacts": [],
    });
    let (w, _rec) = make_recording_walker(graph, vec![fail], None);
    let result = w.execute(json!({}), Some("gr-hookfail".to_string())).await;
    assert!(
        result.success,
        "a failing observer hook must not fail the graph: {result:?}"
    );
    let warnings = w.take_warnings();
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("graph hook") && w.contains("graph_completed")),
        "expected a recorded hook warning, got: {warnings:?}"
    );
}

#[tokio::test]
async fn failed_hook_cost_is_retained_while_failure_remains_a_warning() {
    let graph = make_graph(HOOK_YAML);
    let fail = json!({
        "success": false,
        "status": "failed",
        "result": {"error": "hook boom"},
        "outputs": null,
        "warnings": [],
        "cost": {
            "input_tokens": 7,
            "output_tokens": 2,
            "total_usd": 0.5
        }
    });
    let (w, _rec) = make_recording_walker(graph, vec![fail], None);
    let result = w
        .execute(json!({}), Some("gr-hook-failed-cost".to_string()))
        .await;

    assert!(
        result.success,
        "observer failure does not steer graph: {result:?}"
    );
    assert_eq!(result.cost.unwrap().input_tokens, 7);
    assert_eq!(result.hook_costs.len(), 1);
    assert!(w
        .take_warnings()
        .iter()
        .any(|warning| warning.contains("graph hook") && warning.contains("hook_child_failed")));
}

#[tokio::test]
async fn malformed_hook_cost_fails_terminal_accounting() {
    let graph = make_graph(HOOK_YAML);
    let malformed = json!({
        "success": true,
        "status": "completed",
        "result": null,
        "outputs": null,
        "warnings": [],
        "cost": {
            "input_tokens": i64::MAX as u64 + 1,
            "output_tokens": 0,
            "total_usd": 0.0
        }
    });
    let (walker, _recorder) = make_recording_walker(graph, vec![malformed], None);

    let result = walker
        .execute(json!({}), Some("gr-hook-invalid-cost".to_string()))
        .await;

    assert_eq!(result.status, GraphRunStatus::Error);
    assert!(!result.success);
    assert!(result
        .error
        .as_deref()
        .is_some_and(|error| error.contains("accounting is incomplete")));
}

#[tokio::test]
async fn hook_cost_overflow_fails_terminal_accounting_loudly() {
    let graph = make_graph(
        r#"
version: "1.0.0"
category: test
config:
  start: done
  hooks:
    - {id: first, event: graph_completed, action: {item_id: "tool:test/one"}}
    - {id: overflow, event: graph_completed, action: {item_id: "tool:test/two"}}
  nodes:
    done: {node_type: return}
"#,
    );
    let cost_envelope = |input_tokens| {
        json!({
            "success": true,
            "status": "completed",
            "result": null,
            "outputs": null,
            "warnings": [],
            "cost": {
                "input_tokens": input_tokens,
                "output_tokens": 0,
                "total_usd": 0.0
            }
        })
    };
    let (walker, _recorder) = make_recording_walker(
        graph,
        vec![cost_envelope(i64::MAX as u64), cost_envelope(1)],
        None,
    );

    let result = walker
        .execute(json!({}), Some("gr-hook-overflow".to_string()))
        .await;

    assert_eq!(result.status, GraphRunStatus::Error);
    assert!(!result.success);
    assert!(result
        .error
        .as_deref()
        .is_some_and(|error| error.contains("accounting is incomplete")));
    assert_eq!(result.cost.unwrap().input_tokens, i64::MAX as u64);
}

#[tokio::test]
async fn graph_started_hook_accounting_failure_prevents_first_node_dispatch() {
    let graph = make_graph(
        r#"
version: "1.0.0"
category: test
config:
  start: act
  hooks:
    - {id: first, event: graph_started, action: {item_id: "tool:test/one"}}
    - {id: overflow, event: graph_started, action: {item_id: "tool:test/two"}}
  nodes:
    act:
      action: {item_id: "tool:test/must-not-run"}
      next: {type: unconditional, to: done}
    done: {node_type: return}
"#,
    );
    let cost_envelope = |input_tokens| {
        json!({
            "success": true,
            "status": "completed",
            "result": null,
            "outputs": null,
            "warnings": [],
            "cost": {
                "input_tokens": input_tokens,
                "output_tokens": 0,
                "total_usd": 0.0
            }
        })
    };
    let (walker, recorder) = make_recording_walker(
        graph,
        vec![cost_envelope(i64::MAX as u64), cost_envelope(1)],
        None,
    );

    let result = walker
        .execute(json!({}), Some("gr-start-hook-overflow".to_string()))
        .await;

    assert_eq!(result.status, GraphRunStatus::Error);
    assert_eq!(
        recorder.dispatch_count(),
        2,
        "only the two graph_started hooks may dispatch"
    );
}

#[tokio::test]
async fn resume_restores_graph_started_hook_cost_without_refiring_hook() {
    let graph = make_graph(
        r#"
version: "1.0.0"
category: test
config:
  start: done
  hooks:
    - {id: once, event: graph_started, action: {item_id: "tool:test/once"}}
  nodes:
    done: {node_type: return}
"#,
    );
    let mut params = strict_resume_params(
        &graph,
        "done",
        1,
        json!({}),
        "gr-resumed-hook",
        None,
        None,
        0,
    );
    params["resume_state"]["accounting"] = json!({
        "total": {
            "input_tokens": 2,
            "output_tokens": 3,
            "total_usd": 0.1,
            "basis": "rollup"
        },
        "nodes": [],
        "hooks": [{
            "event": "graph_started",
            "step": null,
            "cost": {
                "input_tokens": 2,
                "output_tokens": 3,
                "total_usd": 0.1
            }
        }]
    });
    let (walker, recorder) = make_recording_walker(graph, vec![], None);

    let result = walker
        .execute(params, Some("ignored-resume-id".to_string()))
        .await;

    assert!(result.success, "resumed graph completes: {result:?}");
    assert_eq!(
        recorder.dispatch_count(),
        0,
        "graph_started must be cold-start only"
    );
    assert_eq!(result.cost.unwrap().input_tokens, 2);
    assert_eq!(result.hook_costs.len(), 1);
    assert_eq!(result.hook_costs[0].event, RuntimeEventType::GraphStarted);
}

const FOLLOW_YAML: &str = r#"
version: "1.0.0"
category: test
config:
  start: fetch
  nodes:
    fetch:
      follow: true
      action: {item_id: "directive:child", params: {}}
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;

#[tokio::test]
async fn follow_suspend_emits_events_and_no_receipt() {
    let tmp = tempfile::tempdir().unwrap();
    let (w, rec) = make_recording_walker(make_graph(FOLLOW_YAML), vec![], Some(tmp.path()));
    // Status `continued` implies write_follow_checkpoint succeeded (a failed
    // checkpoint would route to a terminal error instead).
    let result = w.execute(json!({}), Some("gr-follow".to_string())).await;
    assert_eq!(result.status, GraphRunStatus::Continued);
    assert!(!result.success);
    assert_eq!(result.steps, 0);
    assert!(result.result.is_none());

    let types: Vec<String> = rec
        .recorded_events()
        .into_iter()
        .map(|(_, et, _, _)| et)
        .collect();
    assert!(types.iter().any(|t| t == "graph_step_started"));
    assert!(types.iter().any(|t| t == "graph_follow_suspended"));
    // The suspend must NOT emit the normal action lifecycle — the child result
    // does not exist yet; those are emitted on resume.
    for absent in [
        "tool_call_start",
        "tool_call_result",
        "graph_step_completed",
        "graph_completed",
    ] {
        assert!(
            !types.iter().any(|t| t == absent),
            "unexpected {absent} at suspend; events: {types:?}"
        );
    }
    // Suspended, not finalized (the daemon settles `continued`).
    assert!(rec.recorded_finalizations().is_empty());
    // The handoff carried exactly the run identity that forms the follow_key.
    let reqs = rec.recorded_follow_requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].graph_run_id, "gr-follow");
    assert_eq!(reqs[0].follow_node, "fetch");
    assert_eq!(reqs[0].step_count, 0);
    assert_eq!(reqs[0].children.len(), 1);
    assert_eq!(reqs[0].children[0].item_ref, "directive:child");
}

#[tokio::test]
async fn follow_reentry_preserves_graph_run_id() {
    // First pass under the original run id records the handoff.
    let (w1, rec1) = make_recording_walker(make_graph(FOLLOW_YAML), vec![], None);
    let r1 = w1.execute(json!({}), Some("gr-original".to_string())).await;
    assert_eq!(r1.status, GraphRunStatus::Continued);
    assert_eq!(
        rec1.recorded_follow_requests()[0].graph_run_id,
        "gr-original"
    );

    // Resume with a DIFFERENT outer run id, but resume_state carrying the
    // original (as main.rs injects it). The re-entry MUST re-drive with the
    // ORIGINAL run id so the follow_key is unchanged — otherwise it would spawn
    // a second, distinct follow child.
    let graph = make_graph(FOLLOW_YAML);
    let resume = strict_resume_params(&graph, "fetch", 0, json!({}), "gr-original", None, None, 0);
    let (w2, rec2) = make_recording_walker(graph, vec![], None);
    let r2 = w2
        .execute(resume, Some("gr-different-outer".to_string()))
        .await;
    assert_eq!(r2.status, GraphRunStatus::Continued);
    let req = &rec2.recorded_follow_requests()[0];
    assert_eq!(
        req.graph_run_id, "gr-original",
        "re-entry must reuse the original run id, not the outer one"
    );
    assert_eq!(req.follow_node, "fetch");
    assert_eq!(req.step_count, 0);
}

#[tokio::test]
async fn follow_failed_handoff_terminates_error() {
    let inner: Arc<RecordingMockClient> = Arc::new(RecordingMockClient {
        follow_should_fail: true,
        ..RecordingMockClient::new(vec![])
    });
    let client = CallbackClient::from_inner(
        inner.clone(),
        "thread-test",
        "/tmp/test-project",
        "tat-test",
    );
    let w = Walker::new(
        make_graph(FOLLOW_YAML),
        "/tmp/test-project".to_string(),
        "thread-test".to_string(),
        client,
        None,
    );
    let result = w.execute(json!({}), Some("gr-fail".to_string())).await;

    // A failed handoff settles a terminal error — NEVER `continued` with no
    // child behind it.
    assert_ne!(result.status, GraphRunStatus::Continued);
    assert!(!result.success);
    assert!(
        result
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("follow handoff failed"),
        "expected follow-handoff error, got: {:?}",
        result.error
    );
    // The thread is finalized, not left dangling `continued`.
    assert!(!inner.recorded_finalizations().is_empty());
}

#[tokio::test]
async fn follow_resume_consumes_child_result_and_completes() {
    // Resume INTO the follow node with a spliced child envelope: the node must
    // consume it (classify like a live dispatch) and run the NORMAL outcome —
    // receipt + step_completed + completion — instead of re-suspending.
    let graph = make_graph(FOLLOW_YAML);
    let resume = strict_resume_params(
        &graph,
        "fetch",
        0,
        json!({}),
        "gr-resume",
        Some(json!({
            "follow_node": "fetch",
            "step_count": 0,
            "graph_run_id": "gr-resume",
        })),
        Some(follow_terminal_envelope(
            RuntimeResultStatus::Completed,
            json!({"msg": "child done"}),
        )),
        0,
    );
    let (w, rec) = make_recording_walker(graph, vec![], None);
    let result = w.execute(resume, Some("gr-resume".to_string())).await;

    // Ran to completion, NOT continued — the child result was consumed.
    assert!(result.success);
    assert_eq!(result.status, GraphRunStatus::Completed);

    let types: Vec<String> = rec
        .recorded_events()
        .into_iter()
        .map(|(_, et, _, _)| et)
        .collect();
    // The normal lifecycle deferred from suspend now lands on resume.
    assert!(types.iter().any(|t| t == "graph_step_completed"));
    assert!(types.iter().any(|t| t == "graph_completed"));
    // It did NOT re-suspend, issued no new follow handoff, and — critically —
    // never re-dispatched: the child already ran; the parent only consumed it.
    assert!(!types.iter().any(|t| t == "graph_follow_suspended"));
    assert!(rec.recorded_follow_requests().is_empty());
    assert_eq!(rec.dispatch_count(), 0);
}

const FOLLOW_ON_ERROR_YAML: &str = r#"
version: "1.0.0"
category: test
config:
  start: fetch
  nodes:
    fetch:
      follow: true
      action: {item_id: "directive:child", params: {}}
      on_error: recover
      next: {type: unconditional, to: done}
    recover:
      node_type: return
      output: "recovered"
    done:
      node_type: return
"#;

const FOLLOW_ENV_YAML: &str = r#"
version: "1.0.0"
category: test
config:
  start: fetch
  nodes:
    fetch:
      follow: true
      action: {item_id: "directive:child", params: {}}
      env_requires: ["RYEOS_FOLLOW_TEST_DEFINITELY_UNSET"]
      next: {type: unconditional, to: done}
    done:
      node_type: return
"#;

/// Build a resume_state params object for the `fetch` follow node with an
/// optional child envelope.
fn follow_resume_params(graph: &GraphDefinition, follow_result: Option<Value>) -> Value {
    strict_resume_params(
        graph,
        "fetch",
        0,
        json!({}),
        "gr-resume",
        Some(json!({
            "follow_node": "fetch",
            "step_count": 0,
            "graph_run_id": "gr-resume",
        })),
        follow_result,
        0,
    )
}

fn follow_terminal_envelope(status: RuntimeResultStatus, result: Value) -> Value {
    json!({
        "success": status.is_success(),
        "status": status,
        "result": result,
        "outputs": null,
        "warnings": [],
        "cost": null,
    })
}

#[tokio::test]
async fn follow_resume_success_accounts_cost() {
    // A native child envelope with cost: resume must land the receipt AND the
    // child cost in graph accounting, exactly like a live native dispatch.
    let graph = make_graph(FOLLOW_YAML);
    let mut envelope = follow_terminal_envelope(RuntimeResultStatus::Completed, json!("child_ok"));
    envelope["outputs"] = json!({"x": 1});
    envelope["cost"] = json!({"input_tokens": 120, "output_tokens": 45, "total_usd": 0.0012});
    let params = follow_resume_params(&graph, Some(envelope));
    let (w, rec) = make_recording_walker(graph, vec![], None);
    let result = w.execute(params, Some("gr-resume".to_string())).await;

    assert!(result.success);
    assert_eq!(result.status, GraphRunStatus::Completed);
    // No re-dispatch, no re-suspend.
    assert_eq!(rec.dispatch_count(), 0);
    assert!(rec.recorded_follow_requests().is_empty());
    // The child cost flows into the run total + per-node costs.
    let cost = result.cost.expect("follow child cost must be accounted");
    assert_eq!(cost.input_tokens, 120);
    assert_eq!(cost.output_tokens, 45);
    assert!(!result.node_costs.is_empty());
}

#[tokio::test]
async fn follow_resume_failure_routes_on_error() {
    // A native FAILURE envelope on resume must behave like a live leaf failure:
    // error receipt + graph_step_completed(error), on_error redirect taken,
    // failed-child cost preserved — and no dispatch/handoff.
    let graph = make_graph(FOLLOW_ON_ERROR_YAML);
    let mut envelope = follow_terminal_envelope(
        RuntimeResultStatus::Failed,
        json!({"error": "model refused"}),
    );
    envelope["cost"] = json!({"input_tokens": 80, "output_tokens": 0, "total_usd": 0.0008});
    let params = follow_resume_params(&graph, Some(envelope));
    let (w, rec) = make_recording_walker(graph, vec![], None);
    let result = w.execute(params, Some("gr-resume".to_string())).await;

    // on_error: recover redirects to the recover return node → the run
    // completes rather than hard-failing.
    assert_eq!(result.status, GraphRunStatus::Completed);
    assert_eq!(rec.dispatch_count(), 0);
    assert!(rec.recorded_follow_requests().is_empty());
    // The follow node's step recorded an ERROR completion, and the failed
    // child's cost was still accounted.
    let step_completed_error = rec
        .recorded_events()
        .into_iter()
        .any(|(_, et, payload, _)| {
            et == RuntimeEventType::GraphStepCompleted.as_str()
                && payload.get("status").and_then(|s| s.as_str())
                    == Some(GraphStepStatus::Error.as_str())
        });
    assert!(
        step_completed_error,
        "expected an error graph_step_completed"
    );
    assert_eq!(
        result
            .cost
            .expect("failed child cost preserved")
            .input_tokens,
        80
    );
}

#[tokio::test]
async fn follow_resume_rejects_noncanonical_terminal_envelopes() {
    let cases = [
        json!({"result": {"ok": true}}),
        json!({
            "success": false,
            "status": "error",
            "result": {"error": "unknown status"},
            "outputs": null,
            "warnings": [],
            "cost": null,
        }),
        json!({
            "success": true,
            "status": RuntimeResultStatus::Failed,
            "result": null,
            "outputs": null,
            "warnings": [],
            "cost": null,
        }),
    ];

    for malformed in cases {
        let graph = make_graph(FOLLOW_ON_ERROR_YAML);
        let params = unchecked_resume_params(
            &graph,
            "fetch",
            0,
            json!({}),
            "gr-resume",
            Some(json!({
                "follow_node": "fetch",
                "step_count": 0,
                "graph_run_id": "gr-resume",
            })),
            Some(malformed),
            0,
        );
        let (walker, recorder) = make_recording_walker(graph, vec![], None);
        let result = walker.execute(params, Some("gr-resume".to_string())).await;
        assert_eq!(result.status, GraphRunStatus::Error, "{result:?}");
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("malformed follow result envelope"),
            "{result:?}"
        );
        assert_eq!(recorder.dispatch_count(), 0);
        assert!(recorder.recorded_follow_requests().is_empty());
    }
}

#[tokio::test]
async fn follow_bare_marker_resuspends() {
    // A resume with a pending_follow marker but NO spliced result must NOT
    // consume anything — it re-drives the suspend idempotently with the
    // original run id / node / step.
    let graph = make_graph(FOLLOW_YAML);
    let params = follow_resume_params(&graph, None);
    let (w, rec) = make_recording_walker(graph, vec![], None);
    let result = w.execute(params, Some("gr-resume".to_string())).await;

    assert_eq!(result.status, GraphRunStatus::Continued);
    assert_eq!(rec.dispatch_count(), 0);
    let reqs = rec.recorded_follow_requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].graph_run_id, "gr-resume");
    assert_eq!(reqs[0].follow_node, "fetch");
    assert_eq!(reqs[0].step_count, 0);
    assert!(rec
        .recorded_events()
        .into_iter()
        .any(|(_, et, _, _)| et == "graph_follow_suspended"));
}

#[tokio::test]
async fn follow_resume_ignores_failing_env_preflight() {
    // A follow-resume node with a failing env_requires must still consume the
    // stored child result — the child already ran; a parent-side env gap must
    // not turn its result into a dispatch error.
    let graph = make_graph(FOLLOW_ENV_YAML);
    let envelope = follow_terminal_envelope(RuntimeResultStatus::Completed, json!({"ok": true}));
    let params = follow_resume_params(&graph, Some(envelope));
    let (w, rec) = make_recording_walker(graph, vec![], None);
    let result = w.execute(params, Some("gr-resume".to_string())).await;

    assert!(result.success);
    assert_eq!(result.status, GraphRunStatus::Completed);
    assert_eq!(rec.dispatch_count(), 0);
}

const TWO_FOLLOW_YAML: &str = r#"
version: "1.0.0"
category: test
config:
  start: fetch1
  nodes:
    fetch1:
      follow: true
      action: {item_id: "directive:child1", params: {}}
      next: {type: unconditional, to: fetch2}
    fetch2:
      follow: true
      action: {item_id: "directive:child2", params: {}}
      next: {type: unconditional, to: done}
    done:
      node_type: return
"#;

#[tokio::test]
async fn two_sequential_follow_nodes_suspend_and_resume_in_order() {
    // fetch1 (follow) → fetch2 (follow) → done. Each follow node suspends; after
    // consuming its child result the graph advances to the NEXT follow node and
    // suspends again, then finally completes.

    // Pass 1: suspend at the first follow node.
    let (w1, rec1) = make_recording_walker(make_graph(TWO_FOLLOW_YAML), vec![], None);
    let r1 = w1.execute(json!({}), Some("gr-seq".to_string())).await;
    assert_eq!(r1.status, GraphRunStatus::Continued);
    assert_eq!(rec1.recorded_follow_requests()[0].follow_node, "fetch1");

    // Pass 2: resume fetch1 with its child result → advance to fetch2 → suspend
    // there (a DISTINCT follow handoff, at the next step).
    let graph2 = make_graph(TWO_FOLLOW_YAML);
    let resume1 = strict_resume_params(
        &graph2,
        "fetch1",
        0,
        json!({}),
        "gr-seq",
        Some(json!({
            "follow_node": "fetch1",
            "step_count": 0,
            "graph_run_id": "gr-seq",
        })),
        Some(follow_terminal_envelope(
            RuntimeResultStatus::Completed,
            json!("child1 done"),
        )),
        0,
    );
    let (w2, rec2) = make_recording_walker(graph2, vec![], None);
    let r2 = w2.execute(resume1, Some("gr-seq".to_string())).await;
    assert_eq!(
        r2.status,
        GraphRunStatus::Continued,
        "must suspend again at the second follow node"
    );
    let req2 = rec2.recorded_follow_requests();
    assert_eq!(
        req2.len(),
        1,
        "resuming fetch1 issues exactly one new handoff (fetch2)"
    );
    assert_eq!(
        req2[0].follow_node, "fetch2",
        "the second suspend is at fetch2"
    );
    let fetch2_step = req2[0].step_count;

    // Pass 3: resume fetch2 with its child result → the graph completes.
    let graph3 = make_graph(TWO_FOLLOW_YAML);
    let resume2 = strict_resume_params(
        &graph3,
        "fetch2",
        u32::try_from(fetch2_step).expect("follow step count fits checkpoint schema"),
        json!({}),
        "gr-seq",
        Some(json!({
            "follow_node": "fetch2",
            "step_count": fetch2_step,
            "graph_run_id": "gr-seq",
        })),
        Some(follow_terminal_envelope(
            RuntimeResultStatus::Completed,
            json!("child2 done"),
        )),
        0,
    );
    let (w3, _rec3) = make_recording_walker(graph3, vec![], None);
    let r3 = w3.execute(resume2, Some("gr-seq".to_string())).await;
    assert_eq!(
        r3.status,
        GraphRunStatus::Completed,
        "after both follow nodes resume, the graph completes"
    );
    assert!(r3.success);
}

const FOLLOW_FANOUT_YAML: &str = r#"
version: "1.0.0"
category: test
config:
  start: fan
  nodes:
    fan:
      follow: true
      over: "${state.jobs}"
      as: job
      parallel: true
      max_concurrency: 2
      collect: gathered
      facets: {lane: "${job.lane}"}
      action:
        item_id: "directive:${job.kind}"
        params: {value: "${job.value}", run: "${_run.graph_run_id}"}
      on_error: recover
      next:
        type: conditional
        branches:
          - when: 'length(state.gathered) >= 0'
            to: done
          - to: recover
    recover:
      node_type: return
    done:
      node_type: return
"#;

fn fanout_resume(graph: &GraphDefinition, items: Value, wrapper: Option<Value>) -> Value {
    strict_resume_params(
        graph,
        "fan",
        0,
        json!({"jobs": [{"kind":"mutated","value":99,"lane":"x"}]}),
        "gr-fan",
        Some(json!({
            "follow_node": "fan",
            "step_count": 0,
            "graph_run_id": "gr-fan",
            "iteration_snapshot": items,
        })),
        wrapper,
        0,
    )
}

fn unchecked_fanout_resume(graph: &GraphDefinition, items: Value, wrapper: Value) -> Value {
    unchecked_resume_params(
        graph,
        "fan",
        0,
        json!({"jobs": [{"kind":"mutated","value":99,"lane":"x"}]}),
        "gr-fan",
        Some(json!({
            "follow_node": "fan",
            "step_count": 0,
            "graph_run_id": "gr-fan",
            "iteration_snapshot": items,
        })),
        Some(wrapper),
        0,
    )
}

#[tokio::test]
async fn follow_fanout_spawns_one_ordered_rendered_cohort() {
    let tmp = tempfile::tempdir().unwrap();
    let (w, rec) = make_recording_walker(make_graph(FOLLOW_FANOUT_YAML), vec![], Some(tmp.path()));
    let jobs = json!([
        {"kind":"alpha","value":1,"lane":"red"},
        {"kind":"beta","value":2,"lane":"blue"}
    ]);
    let result = w
        .execute(
            json!({"inject_state":{"jobs": jobs}}),
            Some("gr-fan".into()),
        )
        .await;
    assert_eq!(result.status, GraphRunStatus::Continued);
    let requests = rec.recorded_follow_requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.graph_run_id, "gr-fan");
    assert_eq!(request.follow_node, "fan");
    assert_eq!(request.launch_window_width, Some(2));
    let children = &request.children;
    assert_eq!(children.len(), 2);
    assert_eq!(children[0].item_ref, "directive:alpha");
    assert_eq!(children[0].parameters, json!({"value":1,"run":"gr-fan"}));
    assert_eq!(children[0].facets, Some(json!({"lane":"red"})));
    assert_eq!(children[1].item_ref, "directive:beta");
    let checkpoint: Value =
        serde_json::from_str(&std::fs::read_to_string(tmp.path().join("latest.json")).unwrap())
            .unwrap();
    assert_eq!(checkpoint["pending_follow"]["iteration_snapshot"], jobs);
}

#[tokio::test]
async fn follow_fanout_binds_item_before_rendering_action() {
    let (w, rec) = make_recording_walker(make_graph(FOLLOW_FANOUT_YAML), vec![], None);
    let result = w
        .execute(
            json!({"inject_state":{"jobs":[{"kind":"bound","value":7,"lane":"z"}]}}),
            Some("gr-fan".into()),
        )
        .await;
    assert_eq!(
        result.status,
        GraphRunStatus::Continued,
        "per-item templates must not fail before binding: {result:?}"
    );
    assert_eq!(
        rec.recorded_follow_requests()[0].children[0].item_ref,
        "directive:bound"
    );
}

#[tokio::test]
async fn follow_fanout_rejects_aggregate_launch_payload_before_handoff() {
    let yaml = r#"
version: "1.0.0"
category: test
config:
  start: fan
  on_error: continue
  nodes:
    fan:
      follow: true
      over: "${state.jobs}"
      as: job
      parallel: true
      max_concurrency: 2
      collect: gathered
      action:
        item_id: "directive:test/child"
        params: {payload: "${state.shared}"}
      next: {type: unconditional, to: done}
    done:
      node_type: return
"#;
    let (walker, recorder) = make_recording_walker(make_graph(yaml), Vec::new(), None);
    let jobs = (0..20).map(|index| json!(index)).collect::<Vec<_>>();
    let shared = "x".repeat(256 * 1024);

    let result = walker
        .execute(
            json!({"inject_state": {"jobs": jobs, "shared": shared}}),
            Some("gr-fanout-launch-budget".to_string()),
        )
        .await;

    assert_eq!(result.status, GraphRunStatus::Error);
    let error = result.error.unwrap_or_default();
    assert!(error.contains("follow fanout launch cohort"));
    assert!(error.contains("rye-expr/1 bounds"));
    assert!(
        recorder.recorded_follow_requests().is_empty(),
        "an over-budget cohort must fail before the daemon handoff"
    );
}

#[tokio::test]
async fn follow_fanout_empty_completes_without_spawn() {
    let (w, rec) = make_recording_walker(make_graph(FOLLOW_FANOUT_YAML), vec![], None);
    let result = w
        .execute(
            json!({"inject_state":{"jobs":[], "job":"persistent"}}),
            Some("gr-fan".into()),
        )
        .await;
    assert!(result.success);
    assert_eq!(result.state["gathered"], json!([]));
    assert_eq!(result.state["job"], json!("persistent"));
    assert!(rec.recorded_follow_requests().is_empty());
}

#[tokio::test]
async fn follow_fanout_bare_marker_redrives_same_key_from_snapshot() {
    let snapshot = json!([{"kind":"original","value":1,"lane":"a"}]);
    let graph = make_graph(FOLLOW_FANOUT_YAML);
    let params = fanout_resume(&graph, snapshot, None);
    let (w, rec) = make_recording_walker(graph, vec![], None);
    let result = w.execute(params, Some("outer-other".into())).await;
    assert_eq!(result.status, GraphRunStatus::Continued);
    let request = &rec.recorded_follow_requests()[0];
    assert_eq!(request.graph_run_id, "gr-fan");
    assert_eq!(request.follow_node, "fan");
    assert_eq!(request.children[0].item_ref, "directive:original");
}

#[tokio::test]
async fn follow_fanout_error_redirect_rolls_back_collected_candidate() {
    let snapshot = json!([
        {"kind":"a","value":1,"lane":"a"},
        {"kind":"b","value":2,"lane":"b"}
    ]);
    let wrapper = json!({
        "fanout": true,
        "expected": 2,
        "failed": 1,
        "statuses": [FanoutItemStatus::Completed, FanoutItemStatus::Failed],
        "items":[
            {
                "success": true,
                "status": RuntimeResultStatus::Completed,
                "result": {"ok": 1},
                "outputs": null,
                "warnings": [],
                "cost": {"input_tokens":3,"output_tokens":1,"total_usd":0.1},
            },
            {
                "success": false,
                "status": RuntimeResultStatus::Failed,
                "result": {"error":"boom"},
                "outputs": null,
                "warnings": [],
                "cost": {"input_tokens":4,"output_tokens":0,"total_usd":0.2},
            },
        ]
    });
    let graph = make_graph(FOLLOW_FANOUT_YAML);
    let params = fanout_resume(&graph, snapshot, Some(wrapper));
    let (w, rec) = make_recording_walker(graph, vec![], None);
    let result = w.execute(params, Some("gr-fan".into())).await;
    assert_eq!(result.status, GraphRunStatus::Completed);
    assert!(result.state.get("gathered").is_none());
    assert_eq!(result.cost.unwrap().input_tokens, 7);
    assert!(rec.recorded_follow_requests().is_empty());
    assert_eq!(rec.dispatch_count(), 0);
}

#[tokio::test]
async fn follow_fanout_continue_commits_ordered_results() {
    let yaml = FOLLOW_FANOUT_YAML
        .replace("  nodes:\n", "  on_error: continue\n  nodes:\n")
        .replace("      on_error: recover\n", "");
    let snapshot = json!([
        {"kind":"a","value":1,"lane":"a"},
        {"kind":"b","value":2,"lane":"b"}
    ]);
    let wrapper = json!({
        "fanout": true,
        "expected": 2,
        "failed": 1,
        "statuses": [FanoutItemStatus::Completed, FanoutItemStatus::Failed],
        "items": [
            follow_terminal_envelope(
                RuntimeResultStatus::Completed,
                json!({"ok": 1}),
            ),
            follow_terminal_envelope(
                RuntimeResultStatus::Failed,
                json!({"error": "boom"}),
            ),
        ]
    });
    let graph = make_graph(&yaml);
    let params = fanout_resume(&graph, snapshot, Some(wrapper));
    let (w, _) = make_recording_walker(graph, vec![], None);
    let result = w.execute(params, Some("gr-fan".into())).await;

    assert_eq!(result.status, GraphRunStatus::CompletedWithErrors);
    assert_eq!(result.state["gathered"], json!([{"ok": 1}, null]));
    assert_eq!(result.errors_suppressed, Some(1));
}

#[tokio::test]
async fn follow_fanout_branch_error_rolls_back_collected_candidate() {
    let yaml = FOLLOW_FANOUT_YAML.replace("length(state.gathered) >= 0", "1 / inputs.zero > 0");
    let snapshot = json!([{"kind":"a","value":1,"lane":"a"}]);
    let wrapper = json!({
        "fanout": true,
        "expected": 1,
        "failed": 0,
        "statuses": [FanoutItemStatus::Completed],
        "items": [follow_terminal_envelope(
            RuntimeResultStatus::Completed,
            json!({"ok": 1}),
        )]
    });
    let graph = make_graph(&yaml);
    let mut params = fanout_resume(&graph, snapshot, Some(wrapper));
    let (w, _) = make_recording_walker(graph, vec![], None);
    params["inputs"] = json!({"zero": 0});
    let result = w.execute(params, Some("gr-fan".into())).await;

    assert_eq!(result.status, GraphRunStatus::Completed);
    assert!(result.state.get("gathered").is_none());
}

#[tokio::test]
async fn follow_fanout_malformed_wrapper_fails_loudly() {
    let snapshot = json!([{"kind":"a","value":1,"lane":"a"}]);
    let bad = json!({
        "fanout": true,
        "expected": 2,
        "failed": 0,
        "statuses": [FanoutItemStatus::Completed],
        "items": [{"result": 1}],
    });
    let graph = make_graph(FOLLOW_FANOUT_YAML);
    let params = unchecked_fanout_resume(&graph, snapshot, bad);
    let (w, _) = make_recording_walker(graph, vec![], None);
    let result = w.execute(params, Some("gr-fan".into())).await;
    assert!(result
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("cardinality"));
}

#[tokio::test]
async fn follow_fanout_rejects_status_contract_drift() {
    let failed_item =
        follow_terminal_envelope(RuntimeResultStatus::Failed, json!({"error": "boom"}));
    let cases = [
        (
            json!({
                "fanout": true,
                "expected": 1,
                "failed": 1,
                "statuses": ["error"],
                "items": [failed_item.clone()],
            }),
            "malformed follow fanout wrapper",
        ),
        (
            json!({
                "fanout": true,
                "expected": 1,
                "failed": 0,
                "statuses": [FanoutItemStatus::Completed],
                "items": [failed_item.clone()],
            }),
            "status contradicts its terminal envelope outcome",
        ),
        (
            json!({
                "fanout": true,
                "expected": 1,
                "failed": 0,
                "statuses": [FanoutItemStatus::Failed],
                "items": [failed_item],
            }),
            "typed statuses contain 1 failed items",
        ),
        (
            json!({
                "fanout": true,
                "expected": 1,
                "failed": 0,
                "statuses": [FanoutItemStatus::Completed],
                "items": [{"result": {"ok": true}}],
            }),
            "follow fanout item 0",
        ),
    ];
    for (wrapper, expected_error) in cases {
        let snapshot = json!([{"kind":"a","value":1,"lane":"a"}]);
        let graph = make_graph(FOLLOW_FANOUT_YAML);
        let params = unchecked_fanout_resume(&graph, snapshot, wrapper);
        let (walker, recorder) = make_recording_walker(graph, vec![], None);
        let result = walker.execute(params, Some("gr-fan".into())).await;
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains(expected_error),
            "{result:?}"
        );
        assert!(recorder.recorded_follow_requests().is_empty());
        assert_eq!(recorder.dispatch_count(), 0);
    }
}

/// Assert the R3 fence order for an action-success step:
/// graph_step_started → tool_call_start → tool_call_result → graph_step_completed
/// followed (on advance) by checkpoint, and finally GraphCompleted on terminal.
#[tokio::test]
async fn commit_step_emits_events_in_fence_order() {
    let yaml = r#"
version: "1.0.0"
category: test
config:
  start: step1
  nodes:
    step1:
      action: {item_id: "tool:test/echo", params: {msg: hello}}
      assign: {echo_result: "${result}"}
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;
    let graph = make_graph(yaml);
    let tmp = tempfile::tempdir().unwrap();
    let (w, recorder) =
        make_recording_walker(graph, vec![json!({"msg": "hello"})], Some(tmp.path()));

    let result = w
        .execute(json!({}), Some("gr-fence-test".to_string()))
        .await;
    assert!(result.success);
    assert_eq!(result.definition_ref, "graph:test/test");
    assert_eq!(result.graph_run_id, "gr-fence-test");
    assert_eq!(
        result.definition_hash,
        lillux::cas::sha256_hex(lillux::signature::strip_signature_lines(yaml).as_bytes())
    );

    let events = recorder.recorded_events();
    let types: Vec<&str> = events.iter().map(|(_, et, _, _)| et.as_str()).collect();

    for (_, event_type, payload, _) in &events {
        match event_type.as_str() {
            "graph_started"
            | "graph_completed"
            | "graph_step_started"
            | "graph_step_completed"
            | "tool_call_start"
            | "tool_call_result"
            | "graph_branch_taken"
            | "graph_foreach_iteration" => {
                assert_eq!(
                    payload["definition_ref"].as_str(),
                    Some(result.definition_ref.as_str())
                );
                assert_eq!(
                    payload["definition_hash"].as_str(),
                    Some(result.definition_hash.as_str())
                );
            }
            _ => {}
        }
    }

    for (_, event_type, payload, _) in &events {
        match event_type.as_str() {
            "graph_step_started"
            | "graph_step_completed"
            | "tool_call_start"
            | "tool_call_result"
            | "graph_branch_taken"
            | "graph_foreach_iteration" => {
                let node = payload["node"]
                    .as_str()
                    .expect("node lifecycle event carries node");
                let expected_node_ref = format!("graph:test/test#node:{node}");
                assert_eq!(
                    payload["node_ref"].as_str(),
                    Some(expected_node_ref.as_str())
                );
            }
            _ => {}
        }
    }

    // graph_started is emitted before the loop starts
    let idx = types.iter().position(|&t| t == "graph_started").unwrap();

    // Step 1: action node — R3 fence order
    assert_eq!(
        types[idx + 1],
        "graph_step_started",
        "fence: graph_step_started first"
    );
    assert_eq!(
        types[idx + 2],
        "tool_call_start",
        "fence: tool_call_start second"
    );
    assert_eq!(
        types[idx + 3],
        "tool_call_result",
        "fence: tool_call_result third"
    );
    assert_eq!(
        types[idx + 4],
        "graph_step_completed",
        "fence: graph_step_completed fourth"
    );

    // The return is a node too: its successful terminal outcome completes its
    // own step before the graph-level terminal event.
    assert_eq!(
        types[idx + 5],
        "graph_step_started",
        "return node starts its terminal step"
    );
    assert_eq!(
        types[idx + 6],
        "graph_step_completed",
        "return node completes its terminal step"
    );
    assert_eq!(
        types[idx + 7],
        "graph_completed",
        "graph completes after the return step"
    );

    // GraphCompleted must appear exactly once
    let completed_count = types.iter().filter(|&&t| t == "graph_completed").count();
    assert_eq!(
        completed_count, 1,
        "GraphCompleted must be emitted exactly once, got {completed_count}"
    );

    let artifacts = recorder.artifacts.lock().unwrap();
    let receipt_artifact = artifacts
        .iter()
        .find(|a| a["artifact_type"] == "graph_node_receipt")
        .expect("action receipt artifact should be published");
    assert_eq!(
        receipt_artifact["uri"].as_str(),
        Some("graph://runs/gr-fence-test/node-receipts/0")
    );
    let receipt = &receipt_artifact["metadata"];
    assert_eq!(
        receipt["definition_ref"].as_str(),
        Some(result.definition_ref.as_str())
    );
    assert_eq!(
        receipt["definition_hash"].as_str(),
        Some(result.definition_hash.as_str())
    );
    assert_eq!(receipt["graph_run_id"].as_str(), Some("gr-fence-test"));
    assert_eq!(receipt["node"].as_str(), Some("step1"));
    assert_eq!(
        receipt["node_result_hash"].as_str(),
        Some(hash_json_value(&json!({"msg": "hello"})).unwrap().as_str())
    );
}

/// Every non-terminal `Advance` must write a checkpoint. For a
/// two-step graph (action → return), the final checkpoint should
/// point at the return node. We verify via the TempDir checkpoint file.
#[tokio::test]
async fn commit_step_writes_checkpoint_on_every_advance() {
    let yaml = r#"
version: "1.0.0"
category: test
config:
  start: step1
  max_steps: 10
  nodes:
    step1:
      action: {item_id: "tool:test/echo", params: {msg: hello}}
      next:
        type: unconditional
        to: step2
    step2:
      action: {item_id: "tool:test/echo", params: {msg: world}}
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;
    let graph = make_graph(yaml);
    let tmp = tempfile::tempdir().unwrap();
    let (w, _recorder) = make_recording_walker(
        graph,
        vec![json!({"msg": "hello"}), json!({"msg": "world"})],
        Some(tmp.path()),
    );

    let result = w.execute(json!({}), Some("gr-cp-test".to_string())).await;
    assert!(result.success);

    // After step1 completes, checkpoint points at "step2" (the next node).
    // After step2 completes, checkpoint points at "done" (the return node).
    // The return node itself is terminal — no checkpoint is written for it.
    let checkpoint_file = tmp.path().join("latest.json");
    assert!(
        checkpoint_file.exists(),
        "checkpoint file must exist after graph completes"
    );
    let contents = std::fs::read_to_string(&checkpoint_file).unwrap();
    let cp: Value = serde_json::from_str(&contents).unwrap();
    assert_eq!(
        cp["current_node"], "done",
        "checkpoint must point at the next cursor (done)"
    );
    assert_eq!(
        cp["step_count"], 2,
        "checkpoint step_count must be 2 (two action steps, return is terminal)"
    );
}

/// Gate node must produce: graph_step_started → graph_branch_taken → graph_step_completed → checkpoint.
#[tokio::test]
async fn gate_step_emits_lifecycle_and_checkpoint() {
    let yaml = r#"
version: "1.0.0"
category: test
config:
  start: check
  nodes:
    check:
      node_type: gate
      next:
        type: conditional
        branches:
          - when: 'state.mode == "fast"'
            to: fast_path
          - to: slow_path
    fast_path:
      node_type: return
    slow_path:
      node_type: return
"#;
    let graph = make_graph(yaml);
    let tmp = tempfile::tempdir().unwrap();
    let (w, recorder) = make_recording_walker(graph, vec![], Some(tmp.path()));

    let result = w
        .execute(
            json!({"inject_state": {"mode": "fast"}}),
            Some("gr-gate-test".to_string()),
        )
        .await;
    assert!(result.success);

    let events = recorder.recorded_events();
    let types: Vec<&str> = events.iter().map(|(_, et, _, _)| et.as_str()).collect();

    // Gate lifecycle: graph_step_started → graph_branch_taken → graph_step_completed
    let step_started_idx = types
        .iter()
        .position(|&t| t == "graph_step_started")
        .unwrap();
    assert_eq!(
        types[step_started_idx + 1],
        "graph_branch_taken",
        "gate must emit graph_branch_taken after graph_step_started"
    );
    assert_eq!(
        types[step_started_idx + 2],
        "graph_step_completed",
        "gate must emit graph_step_completed after graph_branch_taken"
    );

    // Verify the branch target is correct
    let branch_event = events
        .iter()
        .find(|(_, et, _, _)| et == "graph_branch_taken")
        .unwrap();
    assert_eq!(branch_event.2["target"], "fast_path");
    assert_eq!(
        branch_event.2["node_ref"].as_str(),
        Some("graph:test/test#node:check")
    );
    assert_eq!(
        branch_event.2["target_node_ref"].as_str(),
        Some("graph:test/test#node:fast_path")
    );

    // Checkpoint must exist pointing at the next node
    let checkpoint_file = tmp.path().join("latest.json");
    assert!(
        checkpoint_file.exists(),
        "checkpoint must exist after gate step"
    );
    let contents = std::fs::read_to_string(&checkpoint_file).unwrap();
    let cp: Value = serde_json::from_str(&contents).unwrap();
    assert_eq!(cp["current_node"], "fast_path");
    // S5: payload is versioned and carries an accounting snapshot so resume
    // restores accumulated cost rather than restarting it at zero. `total`
    // may be null (no cost-bearing node yet); `nodes` is always an array.
    assert_eq!(cp["schema_version"], GRAPH_CHECKPOINT_SCHEMA_VERSION);
    let accounting = cp
        .get("accounting")
        .expect("checkpoint must carry an accounting snapshot");
    assert!(
        accounting["nodes"].is_array(),
        "accounting.nodes must be an array: {accounting}"
    );
}

/// Foreach node must emit per-iteration events (graph_foreach_iteration)
/// and collect results into state.
#[tokio::test]
async fn foreach_step_emits_iteration_events() {
    let yaml = r#"
version: "1.0.0"
category: test
config:
  start: iterate
  nodes:
    iterate:
      node_type: foreach
      over: "${state.items}"
      as: "elem"
      action: {item_id: "tool:test/echo", params: {value: "${elem}"}}
      collect: "results"
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;
    let graph = make_graph(yaml);
    let (w, recorder) = make_recording_walker(
        graph,
        vec![
            json!({"value": "a"}),
            json!({"value": "b"}),
            json!({"value": "c"}),
        ],
        None,
    );

    let result = w
        .execute(
            json!({"inject_state": {"items": ["a", "b", "c"]}}),
            Some("gr-fe-test".to_string()),
        )
        .await;
    assert!(result.success);

    let events = recorder.recorded_events();
    let types: Vec<&str> = events.iter().map(|(_, et, _, _)| et.as_str()).collect();

    // Foreach must emit per-iteration events
    let iteration_count = types
        .iter()
        .filter(|&&t| t == "graph_foreach_iteration")
        .count();
    assert_eq!(iteration_count, 3,
        "foreach must emit exactly 3 graph_foreach_iteration events for 3 items, got {iteration_count}");

    // Both the foreach and the terminal return are nodes and emit the complete
    // step lifecycle.
    let step_started = types.iter().filter(|&&t| t == "graph_step_started").count();
    let step_completed = types
        .iter()
        .filter(|&&t| t == "graph_step_completed")
        .count();
    assert_eq!(step_started, 2, "foreach and return both emit step_started");
    assert_eq!(
        step_completed, 2,
        "foreach and return both emit step_completed"
    );
}

#[test]
fn node_result_hash_uses_canonical_json() {
    let mut left = serde_json::Map::new();
    left.insert("b".into(), json!(2));
    left.insert("a".into(), json!(1));

    let mut right = serde_json::Map::new();
    right.insert("a".into(), json!(1));
    right.insert("b".into(), json!(2));

    let left = Value::Object(left);
    let right = Value::Object(right);
    let expected = lillux::cas::sha256_hex(lillux::cas::canonical_json(&right).unwrap().as_bytes());

    assert_eq!(hash_json_value(&left).unwrap(), expected);
    assert_eq!(
        hash_json_value(&left).unwrap(),
        hash_json_value(&right).unwrap()
    );
}

#[tokio::test]
async fn action_leaf_errors_publish_error_receipts() {
    let yaml = r#"
version: "1.0.0"
category: test
config:
  start: step1
  on_error: fail
  nodes:
    step1:
      action: {item_id: "tool:test/fail"}
"#;
    let graph = make_graph(yaml);
    let (w, recorder) = make_recording_walker(
        graph,
        vec![
            json!({"outcome_code": "exit:1", "result": null, "error": {"exit_code": 1, "stderr": "forced"}}),
        ],
        None,
    );

    let result = w
        .execute(json!({}), Some("gr-error-receipt".to_string()))
        .await;
    assert!(!result.success);

    let artifacts = recorder.artifacts.lock().unwrap();
    let receipt_artifact = artifacts
        .iter()
        .find(|a| a["artifact_type"] == "graph_node_receipt" && a["metadata"]["node"] == "step1")
        .expect("error node receipt should be published");
    assert_eq!(
        receipt_artifact["uri"].as_str(),
        Some("graph://runs/gr-error-receipt/node-receipts/0")
    );
    let receipt = &receipt_artifact["metadata"];

    assert_eq!(
        receipt["definition_ref"].as_str(),
        Some(result.definition_ref.as_str())
    );
    assert_eq!(
        receipt["definition_hash"].as_str(),
        Some(result.definition_hash.as_str())
    );
    assert_eq!(receipt["graph_run_id"].as_str(), Some("gr-error-receipt"));
    assert_eq!(receipt["node_result_hash"], Value::Null);
    let receipt_error = receipt["error"].as_str().unwrap_or_default();
    assert!(
        receipt_error.contains("exit:1") && receipt_error.contains("forced"),
        "receipt error should carry the failure diagnostic, got: {receipt_error}"
    );
}

#[tokio::test]
async fn action_dispatch_hard_errors_publish_error_receipts() {
    let yaml = r#"
version: "1.0.0"
category: test
config:
  start: step1
  on_error: fail
  nodes:
    step1:
      env_requires: [RYEOS_TEST_MISSING_FOR_HARD_ERROR_RECEIPT]
      action: {item_id: "tool:test/env"}
"#;
    let graph = make_graph(yaml);
    let (w, recorder) = make_recording_walker(graph, vec![], None);

    let result = w
        .execute(json!({}), Some("gr-hard-error-receipt".to_string()))
        .await;
    assert!(!result.success);

    let artifacts = recorder.artifacts.lock().unwrap();
    let receipt_artifact = artifacts
        .iter()
        .find(|a| a["artifact_type"] == "graph_node_receipt" && a["metadata"]["node"] == "step1")
        .expect("hard-error node receipt should be published");
    assert_eq!(
        receipt_artifact["uri"].as_str(),
        Some("graph://runs/gr-hard-error-receipt/node-receipts/0")
    );
    let receipt = &receipt_artifact["metadata"];

    assert_eq!(
        receipt["definition_ref"].as_str(),
        Some(result.definition_ref.as_str())
    );
    assert_eq!(
        receipt["definition_hash"].as_str(),
        Some(result.definition_hash.as_str())
    );
    assert_eq!(
        receipt["graph_run_id"].as_str(),
        Some("gr-hard-error-receipt")
    );
    assert_eq!(receipt["node_result_hash"], Value::Null);
    assert!(receipt["error"]
        .as_str()
        .is_some_and(|err| err.contains("env preflight failed")));
}

#[tokio::test]
async fn action_error_redirects_write_checkpoint() {
    let yaml = r#"
version: "1.0.0"
category: test
config:
  start: step1
  nodes:
    step1:
      on_error: handler
      action: {item_id: "tool:test/fail"}
    handler:
      node_type: return
"#;
    let graph = make_graph(yaml);
    let tmp = tempfile::tempdir().unwrap();
    let (w, _recorder) = make_recording_walker(
        graph,
        vec![
            json!({"outcome_code": "exit:1", "result": null, "error": {"exit_code": 1, "stderr": "forced"}}),
        ],
        Some(tmp.path()),
    );

    let result = w
        .execute(json!({}), Some("gr-error-redirect".to_string()))
        .await;
    assert!(result.success);

    let checkpoint_file = tmp.path().join("latest.json");
    assert!(
        checkpoint_file.exists(),
        "redirect advance must write checkpoint"
    );
    let contents = std::fs::read_to_string(&checkpoint_file).unwrap();
    let cp: Value = serde_json::from_str(&contents).unwrap();
    assert_eq!(cp["current_node"], "handler");
    assert_eq!(cp["step_count"], 1);
    assert_eq!(cp["graph_run_id"], "gr-error-redirect");
}

/// Terminal outcomes must emit GraphCompleted exactly once.
/// Test both the success path (return node) and the error path (on_error: fail).
#[tokio::test]
async fn commit_step_terminates_emit_graph_completed_exactly_once() {
    // Success path
    let yaml_ok = r#"
version: "1.0.0"
category: test
config:
  start: step1
  nodes:
    step1:
      action: {item_id: "tool:test/echo", params: {msg: hi}}
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;
    let graph_ok = make_graph(yaml_ok);
    let (w_ok, recorder_ok) = make_recording_walker(graph_ok, vec![json!({"msg": "hi"})], None);

    let result_ok = w_ok.execute(json!({}), Some("gr-t1".to_string())).await;
    assert!(result_ok.success);
    let events_ok = recorder_ok.recorded_events();
    let types_ok: Vec<&str> = events_ok.iter().map(|(_, et, _, _)| et.as_str()).collect();
    let completed_ok = types_ok.iter().filter(|&&t| t == "graph_completed").count();
    assert_eq!(
        completed_ok, 1,
        "success path: exactly 1 GraphCompleted, got {completed_ok}"
    );

    // Error path: on_error: fail with a leaf that returns status=error
    let yaml_err = r#"
version: "1.0.0"
category: test
config:
  start: step1
  on_error: fail
  nodes:
    step1:
      action: {item_id: "tool:test/fail"}
      next:
        type: unconditional
        to: done
    done:
      node_type: return
"#;
    let graph_err = make_graph(yaml_err);
    let (w_err, recorder_err) = make_recording_walker(
        graph_err,
        vec![
            json!({"outcome_code": "exit:1", "result": null, "error": {"exit_code": 1, "stderr": "forced"}}),
        ],
        None,
    );

    let result_err = w_err.execute(json!({}), Some("gr-t2".to_string())).await;
    assert!(!result_err.success);
    let events_err = recorder_err.recorded_events();
    let types_err: Vec<&str> = events_err.iter().map(|(_, et, _, _)| et.as_str()).collect();
    let completed_err = types_err
        .iter()
        .filter(|&&t| t == "graph_completed")
        .count();
    assert_eq!(
        completed_err, 1,
        "error path: exactly 1 GraphCompleted, got {completed_err}"
    );

    // Verify the error path's GraphCompleted carries status=error
    let events_err_full = recorder_err.recorded_events();
    let gc = events_err_full
        .iter()
        .find(|(_, et, _, _)| et == "graph_completed")
        .unwrap();
    assert_eq!(gc.2["status"], "error");
}

#[test]
fn warning_buffer_bounds_oversized_diagnostics_and_resets_after_take() {
    let mut warnings = WarningBuffer::default();
    warnings.push("x".repeat(MAX_GRAPH_WARNING_SCALAR_BYTES + 1));
    assert_eq!(
        warnings.snapshot(),
        vec![GRAPH_WARNINGS_TRUNCATED.to_string()]
    );

    assert_eq!(warnings.take(), vec![GRAPH_WARNINGS_TRUNCATED.to_string()]);
    warnings.push("next run".to_string());
    assert_eq!(warnings.take(), vec!["next run".to_string()]);
}
