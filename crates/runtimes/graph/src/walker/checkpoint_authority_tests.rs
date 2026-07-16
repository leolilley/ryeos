use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ryeos_runtime::callback::{CallbackError, DispatchActionRequest, SpawnFollowChildRequest};
use ryeos_runtime::callback_client::CallbackClient;
use ryeos_runtime::checkpoint::CheckpointWriter;
use ryeos_runtime::envelope::RuntimeResultStatus;
use ryeos_runtime::events::RuntimeEventType;
use ryeos_runtime::ThreadTerminalStatus;
use serde_json::{json, Value};

use super::*;

const INJECTED_CALLBACK_CRASH: &str = "injected crash after durable callback boundary";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CallbackCrashBoundary {
    Event(RuntimeEventType),
    Receipt,
}

/// Minimal callback fake for checkpoint-authority tests. A configured crash is
/// raised only after the callback effect has been recorded, which models a
/// process disappearing after the remote event/receipt store accepted it.
struct AuthorityClient {
    dispatch_results: Mutex<Vec<Value>>,
    dispatched_items: Mutex<Vec<String>>,
    follow_handoffs: AtomicUsize,
    continuation_handoffs: AtomicUsize,
    event_types: Mutex<Vec<String>>,
    artifacts: Mutex<Vec<Value>>,
    crash_boundary: Mutex<Option<CallbackCrashBoundary>>,
}

impl AuthorityClient {
    fn new(dispatch_results: Vec<Value>, crash_boundary: Option<CallbackCrashBoundary>) -> Self {
        Self {
            dispatch_results: Mutex::new(dispatch_results),
            dispatched_items: Mutex::new(Vec::new()),
            follow_handoffs: AtomicUsize::new(0),
            continuation_handoffs: AtomicUsize::new(0),
            event_types: Mutex::new(Vec::new()),
            artifacts: Mutex::new(Vec::new()),
            crash_boundary: Mutex::new(crash_boundary),
        }
    }

    fn crash_if_armed(&self, reached: CallbackCrashBoundary) {
        let should_crash = {
            let mut boundary = self.crash_boundary.lock().unwrap();
            if boundary.as_ref() == Some(&reached) {
                *boundary = None;
                true
            } else {
                false
            }
        };
        assert!(!should_crash, "{INJECTED_CALLBACK_CRASH}: {reached:?}");
    }

    fn assert_no_successor_or_handoff(&self, expected_body_dispatches: usize) {
        let dispatched = self.dispatched_items.lock().unwrap();
        assert_eq!(dispatched.len(), expected_body_dispatches, "{dispatched:?}");
        assert!(
            dispatched.iter().all(|item| item != "tool:test/successor"),
            "the successor must not run past an injected persistence crash: {dispatched:?}"
        );
        assert_eq!(self.follow_handoffs.load(Ordering::SeqCst), 0);
        assert_eq!(self.continuation_handoffs.load(Ordering::SeqCst), 0);
    }
}

#[async_trait]
impl ryeos_runtime::callback::RuntimeCallbackAPI for AuthorityClient {
    async fn dispatch_action(
        &self,
        request: DispatchActionRequest,
    ) -> Result<Value, CallbackError> {
        self.dispatched_items
            .lock()
            .unwrap()
            .push(request.action.item_id);
        let result = {
            let mut results = self.dispatch_results.lock().unwrap();
            if results.is_empty() {
                Value::Null
            } else {
                results.remove(0)
            }
        };
        Ok(json!({"thread": {}, "result": result}))
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
        self.continuation_handoffs.fetch_add(1, Ordering::SeqCst);
        Ok(json!({}))
    }

    async fn spawn_follow_child(&self, _: SpawnFollowChildRequest) -> Result<Value, CallbackError> {
        self.follow_handoffs.fetch_add(1, Ordering::SeqCst);
        Ok(json!({"phase": "waiting"}))
    }

    async fn append_event(
        &self,
        _: &str,
        event_type: &str,
        _: Value,
        _: &str,
    ) -> Result<Value, CallbackError> {
        self.event_types
            .lock()
            .unwrap()
            .push(event_type.to_string());
        let target = {
            let boundary = self.crash_boundary.lock().unwrap();
            boundary.as_ref().and_then(|boundary| match boundary {
                CallbackCrashBoundary::Event(target) if target.as_str() == event_type => {
                    Some(*target)
                }
                _ => None,
            })
        };
        if let Some(target) = target {
            self.crash_if_armed(CallbackCrashBoundary::Event(target));
        }
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
        Ok(json!({"commands": []}))
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
        let is_receipt = artifact["artifact_type"] == "graph_node_receipt";
        self.artifacts.lock().unwrap().push(artifact);
        if is_receipt {
            self.crash_if_armed(CallbackCrashBoundary::Receipt);
        }
        Ok(json!({}))
    }

    async fn get_facets(&self, _: &str) -> Result<Value, CallbackError> {
        Ok(json!({}))
    }
}

fn graph(yaml: &str) -> GraphDefinition {
    GraphDefinition::from_yaml(yaml, Some("checkpoint-authority.yaml")).unwrap()
}

#[derive(Debug, Clone, Copy)]
enum AuthorityScenario {
    ClassicAction,
    SequentialForeach,
    ParallelForeach,
    FollowResume,
    FollowFanoutResume,
}

struct Scenario {
    definition: GraphDefinition,
    prior: Value,
    dispatch_results: Vec<Value>,
    expected_body_dispatches: usize,
    has_receipt: bool,
    /// Every indexed node-level event emitted before this scenario's
    /// checkpoint. Run-level `graph_started` and ephemeral foreach iteration
    /// progress are outside the transition fence exercised here.
    event_boundaries: Vec<RuntimeEventType>,
}

fn schema_3_checkpoint(
    definition: &GraphDefinition,
    graph_run_id: &str,
    current_node: &str,
    step_count: u32,
    state: Value,
    pending_follow: Option<Value>,
    follow_result: Option<Value>,
) -> Value {
    let mut checkpoint = json!({
        "schema_version": GRAPH_CHECKPOINT_SCHEMA_VERSION,
        "definition_ref": definition.definition_ref.clone(),
        "definition_hash": definition.definition_hash.clone(),
        "expression_language": EXPRESSION_LANGUAGE,
        "graph_run_id": graph_run_id,
        "current_node": current_node,
        "step_count": step_count,
        "state": state,
        "accounting": {"total": null, "nodes": [], "hooks": []},
        "suppressed_errors": [],
        "retry_attempt": 0,
        "written_at": "2026-07-14T00:00:00Z",
    });
    if let Some(pending_follow) = pending_follow {
        checkpoint[follow_keys::PENDING_FOLLOW] = pending_follow;
    }
    if let Some(follow_result) = follow_result {
        checkpoint[follow_keys::FOLLOW_RESULT] = follow_result;
    }
    crate::resume::from_checkpoint_value(&checkpoint, definition).unwrap();
    checkpoint
}

impl AuthorityScenario {
    fn build(self) -> Scenario {
        match self {
            Self::ClassicAction => {
                let definition = graph(
                    r#"
version: "1.0.0"
category: test
config:
  start: act
  nodes:
    act:
      action: {item_id: "tool:test/action", ref_bindings: {}}
      assign: {candidate: "${result.value}"}
      next: {type: unconditional, to: after}
    after:
      action: {item_id: "tool:test/successor", ref_bindings: {}}
"#,
                );
                let prior = schema_3_checkpoint(
                    &definition,
                    "gr-classic-authority",
                    "act",
                    0,
                    json!({"committed": true}),
                    None,
                    None,
                );
                Scenario {
                    definition,
                    prior,
                    dispatch_results: vec![json!({"value": 7})],
                    expected_body_dispatches: 1,
                    has_receipt: true,
                    event_boundaries: vec![
                        RuntimeEventType::GraphStepStarted,
                        RuntimeEventType::ToolCallStart,
                        RuntimeEventType::ToolCallResult,
                        RuntimeEventType::GraphStepCompleted,
                    ],
                }
            }
            Self::SequentialForeach | Self::ParallelForeach => {
                let parallel = matches!(self, Self::ParallelForeach);
                let definition = graph(&format!(
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
      action: {{item_id: "tool:test/foreach", ref_bindings: {{}}, params: {{item: "${{item}}"}}}}
      collect: candidate_results
      next: {{type: unconditional, to: after}}
    after:
      action: {{item_id: "tool:test/successor", ref_bindings: {{}}}}
"#,
                ));
                let run_id = if parallel {
                    "gr-parallel-foreach-authority"
                } else {
                    "gr-sequential-foreach-authority"
                };
                let prior = schema_3_checkpoint(
                    &definition,
                    run_id,
                    "iterate",
                    0,
                    json!({"committed": true, "items": [1, 2]}),
                    None,
                    None,
                );
                Scenario {
                    definition,
                    prior,
                    dispatch_results: vec![json!({"value": 1}), json!({"value": 2})],
                    expected_body_dispatches: 2,
                    // Foreach has progress and completion events but does not
                    // publish a node receipt in the current production fence.
                    has_receipt: false,
                    event_boundaries: vec![
                        RuntimeEventType::GraphForeachStarted,
                        RuntimeEventType::GraphStepStarted,
                        RuntimeEventType::GraphStepCompleted,
                    ],
                }
            }
            Self::FollowResume => {
                let definition = graph(
                    r#"
version: "1.0.0"
category: test
config:
  start: await_child
  nodes:
    await_child:
      follow: true
      action: {item_id: "directive:test/child", ref_bindings: {}}
      assign: {candidate_child: "${result}"}
      next: {type: unconditional, to: after}
    after:
      action: {item_id: "tool:test/successor", ref_bindings: {}}
"#,
                );
                let run_id = "gr-follow-resume-authority";
                let prior = schema_3_checkpoint(
                    &definition,
                    run_id,
                    "await_child",
                    4,
                    json!({"committed": true}),
                    Some(json!({
                        "follow_node": "await_child",
                        "step_count": 4,
                        "graph_run_id": run_id,
                    })),
                    Some(json!({
                        "success": true,
                        "status": ThreadTerminalStatus::Completed.as_str(),
                        "result": {"answer": 42},
                        "outputs": null,
                        "warnings": [],
                        "cost": null,
                    })),
                );
                Scenario {
                    definition,
                    prior,
                    dispatch_results: Vec::new(),
                    expected_body_dispatches: 0,
                    has_receipt: true,
                    event_boundaries: vec![
                        RuntimeEventType::GraphStepStarted,
                        RuntimeEventType::ToolCallStart,
                        RuntimeEventType::ToolCallResult,
                        RuntimeEventType::GraphStepCompleted,
                    ],
                }
            }
            Self::FollowFanoutResume => {
                let definition = graph(
                    r#"
version: "1.0.0"
category: test
config:
  start: await_cohort
  nodes:
    await_cohort:
      follow: true
      over: "${state.jobs}"
      as: job
      parallel: true
      action: {item_id: "directive:test/child", ref_bindings: {}, params: {job: "${job}"}}
      collect: candidate_children
      next: {type: unconditional, to: after}
    after:
      action: {item_id: "tool:test/successor", ref_bindings: {}}
"#,
                );
                let run_id = "gr-fanout-resume-authority";
                let prior = schema_3_checkpoint(
                    &definition,
                    run_id,
                    "await_cohort",
                    6,
                    json!({"committed": true, "jobs": ["mutated"]}),
                    Some(json!({
                        "follow_node": "await_cohort",
                        "step_count": 6,
                        "graph_run_id": run_id,
                        "iteration_snapshot": ["a", "b"],
                    })),
                    Some(json!({
                        "fanout": true,
                        "expected": 2,
                        "failed": 0,
                        "statuses": [FanoutItemStatus::Completed, FanoutItemStatus::Completed],
                        "items": [
                            {
                                "success": true,
                                "status": RuntimeResultStatus::Completed,
                                "result": {"answer": 1},
                                "outputs": null,
                                "warnings": [],
                                "cost": null,
                            },
                            {
                                "success": true,
                                "status": RuntimeResultStatus::Completed,
                                "result": {"answer": 2},
                                "outputs": null,
                                "warnings": [],
                                "cost": null,
                            },
                        ],
                    })),
                );
                Scenario {
                    definition,
                    prior,
                    dispatch_results: Vec::new(),
                    expected_body_dispatches: 0,
                    has_receipt: true,
                    event_boundaries: vec![
                        RuntimeEventType::GraphStepStarted,
                        RuntimeEventType::GraphStepCompleted,
                        RuntimeEventType::GraphBranchTaken,
                    ],
                }
            }
        }
    }

    fn redrive_dispatch_results(self) -> Vec<Value> {
        match self {
            Self::ClassicAction => vec![json!({"value": 7}), json!({"successor": true})],
            Self::SequentialForeach | Self::ParallelForeach => vec![
                json!({"value": 1}),
                json!({"value": 2}),
                json!({"successor": true}),
            ],
            Self::FollowResume | Self::FollowFanoutResume => {
                vec![json!({"successor": true})]
            }
        }
    }

    fn expected_dispatches_before_event(self, event: RuntimeEventType) -> usize {
        if matches!(self, Self::SequentialForeach | Self::ParallelForeach)
            && event == RuntimeEventType::GraphForeachStarted
        {
            0
        } else {
            self.build().expected_body_dispatches
        }
    }

    fn assert_persisted_candidate(self, state: &Value) {
        assert_eq!(state["committed"], json!(true));
        match self {
            Self::ClassicAction => assert_eq!(state["candidate"], json!(7)),
            Self::SequentialForeach | Self::ParallelForeach => {
                assert_eq!(state["candidate_results"].as_array().map(Vec::len), Some(2));
                assert!(state.get("item").is_none());
            }
            Self::FollowResume => assert_eq!(state["candidate_child"]["answer"], json!(42)),
            Self::FollowFanoutResume => {
                assert_eq!(
                    state["candidate_children"].as_array().map(Vec::len),
                    Some(2)
                );
                assert!(state.get("job").is_none());
            }
        }
    }
}

fn resume_params(definition: &GraphDefinition, checkpoint: &Value) -> Value {
    let resume = crate::resume::from_checkpoint_value(checkpoint, definition).unwrap();
    json!({"resume_state": serde_json::to_value(resume).unwrap()})
}

fn walker_with_checkpoint(
    scenario: &Scenario,
    checkpoint_dir: &std::path::Path,
    callback_crash: Option<CallbackCrashBoundary>,
) -> (Walker, Arc<AuthorityClient>, CheckpointWriter) {
    let checkpoint = CheckpointWriter::new(checkpoint_dir.to_path_buf());
    checkpoint.write(&scenario.prior).unwrap();
    let callback = Arc::new(AuthorityClient::new(
        scenario.dispatch_results.clone(),
        callback_crash,
    ));
    let client = CallbackClient::from_inner(
        callback.clone(),
        "thread-checkpoint-authority",
        "/tmp/test-project",
        "tat-checkpoint-authority",
    );
    let walker = Walker::new(
        scenario.definition.clone(),
        "/tmp/test-project".to_string(),
        "thread-checkpoint-authority".to_string(),
        client,
        Some(checkpoint.clone()),
    );
    (walker, callback, checkpoint)
}

async fn assert_safe_redrive(
    kind: AuthorityScenario,
    definition: &GraphDefinition,
    checkpoint: &Value,
) {
    // An old authoritative cursor deliberately re-runs the body: external
    // actions remain protected by their existing idempotency contract, not by
    // pretending an accepted dispatch was rolled back. A newly persisted
    // successor cursor, by contrast, must skip the body completely.
    let resume = crate::resume::from_checkpoint_value(checkpoint, definition).unwrap();
    let resumes_at_successor = resume.current_node == "after";
    let callback = Arc::new(AuthorityClient::new(kind.redrive_dispatch_results(), None));
    let client = CallbackClient::from_inner(
        callback.clone(),
        "thread-checkpoint-redrive",
        "/tmp/test-project",
        "tat-checkpoint-redrive",
    );
    let walker = Walker::new(
        definition.clone(),
        "/tmp/test-project".to_string(),
        "thread-checkpoint-redrive".to_string(),
        client,
        None,
    );
    let result = walker
        .execute(
            json!({"resume_state": serde_json::to_value(resume).unwrap()}),
            None,
        )
        .await;
    assert!(
        result.success,
        "authoritative checkpoint did not re-drive: {result:?}"
    );
    assert_eq!(result.status, GraphRunStatus::Completed);
    let dispatched = callback.dispatched_items.lock().unwrap();
    let expected = if resumes_at_successor {
        1
    } else {
        kind.build().expected_body_dispatches + 1
    };
    assert_eq!(dispatched.len(), expected, "{dispatched:?}");
    assert_eq!(
        dispatched.last().map(String::as_str),
        Some("tool:test/successor")
    );
    assert_eq!(callback.follow_handoffs.load(Ordering::SeqCst), 0);
    assert_eq!(callback.continuation_handoffs.load(Ordering::SeqCst), 0);
}

async fn expect_injected_crash(walker: Walker, params: Value) {
    let join_error = tokio::spawn(async move { walker.execute(params, None).await })
        .await
        .expect_err("the configured persistence boundary must crash the task");
    assert!(
        join_error.is_panic(),
        "unexpected task failure: {join_error}"
    );
}

async fn exercise_event_boundaries(kind: AuthorityScenario) {
    let event_boundaries = kind.build().event_boundaries;
    for event in event_boundaries {
        let scenario = kind.build();
        let tmp = tempfile::tempdir().unwrap();
        let params = resume_params(&scenario.definition, &scenario.prior);
        let (walker, callback, checkpoint) = walker_with_checkpoint(
            &scenario,
            tmp.path(),
            Some(CallbackCrashBoundary::Event(event)),
        );
        expect_injected_crash(walker, params).await;
        assert!(
            callback
                .event_types
                .lock()
                .unwrap()
                .iter()
                .any(|recorded| recorded == event.as_str()),
            "faulted event was not recorded first: {event:?}"
        );
        let latest = checkpoint.load_latest().unwrap().unwrap();
        assert_eq!(latest, scenario.prior, "event boundary: {event:?}");
        crate::resume::from_checkpoint_value(&latest, &scenario.definition).unwrap();
        callback.assert_no_successor_or_handoff(kind.expected_dispatches_before_event(event));
        assert_safe_redrive(kind, &scenario.definition, &latest).await;
    }
}

async fn exercise_receipt_boundary(kind: AuthorityScenario) {
    let scenario = kind.build();
    assert!(scenario.has_receipt);
    let tmp = tempfile::tempdir().unwrap();
    let params = resume_params(&scenario.definition, &scenario.prior);
    let (walker, callback, checkpoint) =
        walker_with_checkpoint(&scenario, tmp.path(), Some(CallbackCrashBoundary::Receipt));
    expect_injected_crash(walker, params).await;
    assert!(
        callback
            .artifacts
            .lock()
            .unwrap()
            .iter()
            .any(|artifact| artifact["artifact_type"] == "graph_node_receipt"),
        "receipt crash must occur after the receipt was accepted"
    );
    let latest = checkpoint.load_latest().unwrap().unwrap();
    assert_eq!(latest, scenario.prior);
    crate::resume::from_checkpoint_value(&latest, &scenario.definition).unwrap();
    callback.assert_no_successor_or_handoff(scenario.expected_body_dispatches);
    assert_safe_redrive(kind, &scenario.definition, &latest).await;
}

async fn exercise_rejected_checkpoint_boundary(kind: AuthorityScenario) {
    let scenario = kind.build();
    let tmp = tempfile::tempdir().unwrap();
    let params = resume_params(&scenario.definition, &scenario.prior);
    let (walker, callback, checkpoint) = walker_with_checkpoint(&scenario, tmp.path(), None);
    walker.fail_checkpoint_writes_after(0);
    let result = walker.execute(params, None).await;
    assert_eq!(result.status, GraphRunStatus::Error);
    assert!(
        result
            .error
            .as_deref()
            .is_some_and(|error| error.contains("injected checkpoint persistence failure")),
        "expected an injected checkpoint rejection, got {result:?}"
    );
    let latest = checkpoint.load_latest().unwrap().unwrap();
    assert_eq!(latest, scenario.prior);
    crate::resume::from_checkpoint_value(&latest, &scenario.definition).unwrap();
    callback.assert_no_successor_or_handoff(scenario.expected_body_dispatches);
    assert_safe_redrive(kind, &scenario.definition, &latest).await;
}

async fn exercise_persisted_checkpoint_boundary(kind: AuthorityScenario) {
    let scenario = kind.build();
    let prior_resume =
        crate::resume::from_checkpoint_value(&scenario.prior, &scenario.definition).unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let params = resume_params(&scenario.definition, &scenario.prior);
    let (walker, callback, checkpoint) = walker_with_checkpoint(&scenario, tmp.path(), None);
    walker.crash_after_checkpoint_writes(0);
    expect_injected_crash(walker, params).await;
    let latest = checkpoint.load_latest().unwrap().unwrap();
    assert_ne!(latest, scenario.prior);
    let latest_resume =
        crate::resume::from_checkpoint_value(&latest, &scenario.definition).unwrap();
    assert_eq!(latest_resume.current_node, "after");
    assert_eq!(latest_resume.step_count, prior_resume.step_count + 1);
    assert!(latest_resume.pending_follow.is_none());
    assert!(latest_resume.follow_result.is_none());
    kind.assert_persisted_candidate(&latest_resume.state);
    callback.assert_no_successor_or_handoff(scenario.expected_body_dispatches);
    assert_safe_redrive(kind, &scenario.definition, &latest).await;
}

async fn exercise_persistence_matrix(kind: AuthorityScenario) {
    exercise_event_boundaries(kind).await;
    if kind.build().has_receipt {
        exercise_receipt_boundary(kind).await;
    }
    exercise_rejected_checkpoint_boundary(kind).await;
    exercise_persisted_checkpoint_boundary(kind).await;
}

#[tokio::test]
async fn classic_action_persistence_boundary_matrix() {
    exercise_persistence_matrix(AuthorityScenario::ClassicAction).await;
}

#[tokio::test]
async fn sequential_foreach_persistence_boundary_matrix() {
    exercise_persistence_matrix(AuthorityScenario::SequentialForeach).await;
}

#[tokio::test]
async fn parallel_foreach_persistence_boundary_matrix() {
    exercise_persistence_matrix(AuthorityScenario::ParallelForeach).await;
}

#[tokio::test]
async fn follow_resume_persistence_boundary_matrix() {
    exercise_persistence_matrix(AuthorityScenario::FollowResume).await;
}

#[tokio::test]
async fn follow_fanout_resume_persistence_boundary_matrix() {
    exercise_persistence_matrix(AuthorityScenario::FollowFanoutResume).await;
}

#[tokio::test]
async fn identity_verified_checkpoint_resumes_numeric_increment() {
    let definition = graph(
        r#"
version: "1.0.0"
category: test
config:
  start: increment
  nodes:
    increment:
      action: {item_id: "tool:test/increment", ref_bindings: {}}
      assign: {count: "${state.count + 1}"}
      next: {type: unconditional, to: done}
    done:
      node_type: return
      output: "${state.count}"
"#,
    );
    let checkpoint = json!({
        "schema_version": GRAPH_CHECKPOINT_SCHEMA_VERSION,
        "definition_ref": definition.definition_ref.clone(),
        "definition_hash": definition.definition_hash.clone(),
        "expression_language": EXPRESSION_LANGUAGE,
        "graph_run_id": "gr-numeric-resume",
        "current_node": "increment",
        "step_count": 9,
        "state": {"count": 41},
        "accounting": {"total": null, "nodes": [], "hooks": []},
        "suppressed_errors": [],
        "retry_attempt": 0,
        "written_at": "2026-07-14T00:00:00Z",
    });
    let resume = crate::resume::from_checkpoint_value(&checkpoint, &definition).unwrap();
    let callback = Arc::new(AuthorityClient::new(vec![json!({})], None));
    let client = CallbackClient::from_inner(
        callback,
        "thread-numeric-resume",
        "/tmp/test-project",
        "tat-numeric-resume",
    );
    let walker = Walker::new(
        definition,
        "/tmp/test-project".to_string(),
        "thread-numeric-resume".to_string(),
        client,
        None,
    );

    let result = walker
        .execute(
            json!({"resume_state": serde_json::to_value(resume).unwrap()}),
            None,
        )
        .await;

    assert!(result.success, "numeric resume failed: {result:?}");
    assert_eq!(result.status, GraphRunStatus::Completed);
    assert_eq!(result.steps, 10);
    assert_eq!(result.state["count"], json!(42));
    assert_eq!(result.result, Some(json!(42)));
}
