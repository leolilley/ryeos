use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Re-export so callback/graph/runtime callers reference one method-call type
/// without each taking a direct `ryeos-engine` dependency.
pub use ryeos_engine::method_call::MethodCall;

/// One replayed event as the runtime consumes it. The daemon's persisted record
/// carries more columns (chain/thread sequence, hashes, storage class); only the
/// transcript-relevant fields are deserialized — the rest are ignored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayedEventRecord {
    pub event_type: String,
    pub payload: Value,
}

/// A page of replayed events. `next_cursor` is the `after_chain_seq` to pass on
/// the next call when the chain has more events than the page limit; `None` when
/// the page is the last.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayResponse {
    pub events: Vec<ReplayedEventRecord>,
    #[serde(default)]
    pub next_cursor: Option<i64>,
}

#[derive(Debug, thiserror::Error)]
pub enum CallbackError {
    #[error("{code}: {message}")]
    ActionFailed {
        code: String,
        message: String,
        retryable: bool,
    },
    #[error("transport error: {0}")]
    Transport(#[from] anyhow::Error),
}

impl CallbackError {
    pub fn retryable(&self) -> bool {
        match self {
            Self::ActionFailed { retryable, .. } => *retryable,
            // A transport failure after sending has an unknown outcome: the
            // daemon may have applied a non-idempotent action and only its reply
            // was lost. Until the RPC layer exposes a proven-before-delivery
            // failure, reissuing is unsafe.
            Self::Transport(_) => false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DispatchActionRequest {
    pub thread_id: String,
    pub project_path: String,
    pub action: ActionPayload,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hook_dispatch: Option<HookDispatchIdentity>,
}

/// Stable logical occurrence of one runtime-hook event.
///
/// The daemon namespaces this caller-supplied coordinate under authoritative
/// chain and execution identity before using it for idempotency. Keeping the
/// finite event vocabulary typed prevents an arbitrary string from silently
/// creating a second ledger namespace for the same lifecycle boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum HookDispatchOccurrence {
    GraphStarted {
        graph_run_id: String,
        definition_ref: String,
        definition_hash: String,
    },
    GraphStepCompleted {
        graph_run_id: String,
        definition_ref: String,
        definition_hash: String,
        step: u32,
        node: String,
    },
    GraphCompleted {
        graph_run_id: String,
        definition_ref: String,
        definition_hash: String,
        steps: u32,
    },
    DirectiveAfterStep {
        definition_ref: String,
        definition_hash: String,
        turn: u32,
    },
    DirectiveContinuation {
        definition_ref: String,
        definition_hash: String,
        turn: u32,
    },
}

impl HookDispatchOccurrence {
    pub const fn event(&self) -> &'static str {
        match self {
            Self::GraphStarted { .. } => "graph_started",
            Self::GraphStepCompleted { .. } => "graph_step_completed",
            Self::GraphCompleted { .. } => "graph_completed",
            Self::DirectiveAfterStep { .. } => "after_step",
            Self::DirectiveContinuation { .. } => "continuation",
        }
    }
}

/// Exact hook-dispatch identity attached by the shared evaluator after it has
/// selected a compiled hook and validated the event context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookDispatchIdentity {
    pub occurrence: HookDispatchOccurrence,
    pub hook_id: String,
    pub layer: crate::hooks_loader::HookLayer,
    pub context_hash: String,
}

/// A graph node's request to launch a detached follow CHILD and suspend the
/// calling parent until the child's whole continuation chain reaches terminal.
///
/// The daemon derives everything trust-bearing (acting principal, parent chain
/// root, provenance, the caps the child runs under) from validated server-side
/// state — never from this request. These fields only identify WHICH follow
/// this is (the idempotency `follow_key` is
/// `parent_thread_id/graph_run_id/follow_node/step_count`) and WHAT child to run.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpawnFollowChildRequest {
    /// The caller's own thread — the graph (parent) issuing the follow. Named
    /// `thread_id` to match the callback wire convention (the caller's thread),
    /// where "parent" is just its follow-semantics role.
    pub thread_id: String,
    /// Project path the parent runs in, for callback-token validation.
    pub project_path: String,
    pub graph_run_id: String,
    pub follow_node: String,
    pub step_count: i64,
    /// Required non-empty cohort. Single-child callers emit one element.
    pub children: Vec<FollowChildSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_window_width: Option<u32>,
    /// Optional graph frontier id, recorded on the waiter for diagnostics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frontier_id: Option<String>,
    /// Exact `continued` terminal payload the graph will emit on stdout after
    /// the daemon atomically records the follow handoff.
    pub completion: TerminalCompletion,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FollowChildSpec {
    pub item_ref: String,
    pub ref_bindings: BTreeMap<String, String>,
    #[serde(default)]
    pub parameters: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub facets: Option<Value>,
}

/// Terminal completion a runtime sends when it self-finalizes a thread.
///
/// `cost` is carried as raw JSON so the runtime callback wire does not couple
/// to a cross-crate cost type; the daemon maps it into its own cost record.
#[derive(Debug, Clone, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalCompletion {
    pub status: crate::ThreadTerminalStatus,
    pub outcome_code: Option<String>,
    pub result: Option<Value>,
    pub error: Option<Value>,
    pub cost: Option<Value>,
    /// The runtime's `RuntimeResult.outputs` — its structured return value,
    /// distinct from the terminal `result` (which some runtimes set to a sentinel
    /// while the real values ride here). Carried so a detached child's outputs are
    /// persisted for a follow parent to consume.
    pub outputs: Value,
    /// The runtime's `RuntimeResult.warnings` accumulated before finalize.
    pub warnings: Vec<String>,
}

struct RequiredNullable<T>(Option<T>);

impl<'de, T> Deserialize<'de> for RequiredNullable<T>
where
    T: serde::de::DeserializeOwned,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Deserialize through `Value` first. Unlike `Option<T>`, `Value`
        // rejects an absent struct field while still representing an explicit
        // JSON null, preserving the required-but-nullable wire distinction.
        let value = Value::deserialize(deserializer)?;
        serde_json::from_value(value)
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TerminalCompletionWire {
    status: crate::ThreadTerminalStatus,
    outcome_code: RequiredNullable<String>,
    result: RequiredNullable<Value>,
    error: RequiredNullable<Value>,
    cost: RequiredNullable<Value>,
    outputs: Value,
    warnings: Vec<String>,
}

impl<'de> Deserialize<'de> for TerminalCompletion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = TerminalCompletionWire::deserialize(deserializer)?;
        Ok(Self {
            status: wire.status,
            outcome_code: wire.outcome_code.0,
            result: wire.result.0,
            error: wire.error.0,
            cost: wire.cost.0,
            outputs: wire.outputs,
            warnings: wire.warnings,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionPayload {
    /// Stable runtime-occurrence identity. Required for detached actions so a
    /// daemon crash/retry cannot mint the same logical child twice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    pub item_id: String,
    pub ref_bindings: BTreeMap<String, String>,
    #[serde(default)]
    pub params: Value,
    pub thread: String,
    /// Optional method call mirroring the `/execute` `call` block, so a graph
    /// node action can select a non-default method (e.g. knowledge `query`).
    /// Absent for actions that take the kind's default method, and for kinds
    /// that declare no methods.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call: Option<MethodCall>,
    /// Cohort/fleet facets to stamp on the spawned child at spawn — a
    /// `{key: value}` map, only meaningful for a `thread: "detached"` dispatch
    /// (the daemon appends a `thread_facet_set` event per entry before launch).
    /// Absent for inline dispatches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub facets: Option<Value>,
    /// Bounded-fanout launch window for a `thread: "detached"` dispatch: the
    /// daemon mints the child immediately but keeps at most `width` window
    /// members launched-and-live at once (a member is the child CHAIN — the
    /// slot survives `thread_continued` and frees on a hard terminal). The
    /// daemon namespaces `key` under the parent thread id, so a caller can
    /// only pace its own children. Absent for inline dispatches and
    /// unbounded spawns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_window: Option<LaunchWindow>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum HookActionPrimary {
    Execute,
}

/// Authored hook actions use the shared execute-action grammar, with two
/// deliberate conveniences at this pre-callback boundary: `primary: execute`
/// is accepted as the hook source's routing declaration and `thread` defaults
/// to `inline`. Once parsed, the callback receives the exact [`ActionPayload`]
/// used by every other dispatch path.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthoredHookAction {
    #[serde(default)]
    primary: Option<HookActionPrimary>,
    item_id: String,
    ref_bindings: BTreeMap<String, String>,
    #[serde(default)]
    params: Value,
    #[serde(default = "inline_thread")]
    thread: String,
    #[serde(default)]
    call: Option<MethodCall>,
    #[serde(default)]
    facets: Option<Value>,
    #[serde(default)]
    launch_window: Option<LaunchWindow>,
}

fn inline_thread() -> String {
    "inline".to_string()
}

/// Parse the one canonical hook-action shape shared by every runtime.
///
/// Missing/empty `item_id`, missing `ref_bindings`, empty `thread`, unknown
/// fields, an unsupported `primary`, and malformed typed blocks all fail before
/// any callback occurs.
pub fn parse_hook_action(action: Value) -> Result<ActionPayload, String> {
    let authored: AuthoredHookAction =
        serde_json::from_value(action).map_err(|error| format!("invalid hook action: {error}"))?;
    let AuthoredHookAction {
        primary: _primary,
        item_id,
        ref_bindings,
        params,
        thread,
        call,
        facets,
        launch_window,
    } = authored;
    if item_id.trim().is_empty() {
        return Err("invalid hook action: `item_id` must be a non-empty string".to_string());
    }
    if thread.trim().is_empty() {
        return Err("invalid hook action: `thread` must be a non-empty string".to_string());
    }
    Ok(ActionPayload {
        operation_id: None,
        item_id,
        ref_bindings,
        params,
        thread,
        call,
        facets,
        launch_window,
    })
}

/// See [`ActionPayload::launch_window`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchWindow {
    pub key: String,
    pub width: u32,
}

/// Wire keys of [`ActionPayload`], for code that handles an action as a raw
/// `Value` map (the graph walker compiles and renders selected action fields
/// before dispatch). One source of truth: adding a field to `ActionPayload`
/// means adding its key here and deciding whether `CompiledActionTemplate`
/// accepts templates inside it — a literal that drifts from the struct is how
/// `facets` once shipped unresolved.
pub mod action_keys {
    pub const ITEM_ID: &str = "item_id";
    pub const REF_BINDINGS: &str = "ref_bindings";
    pub const PARAMS: &str = "params";
    pub const THREAD: &str = "thread";
    pub const CALL: &str = "call";
    pub const FACETS: &str = "facets";
    pub const LAUNCH_WINDOW: &str = "launch_window";

    /// Keys whose values may carry `${…}` templates and are rendered by
    /// `CompiledActionTemplate`. `THREAD` stays literal (a dispatch mode,
    /// never a template); the callback-owned ref bindings and `CALL` block may
    /// contain templates, so those complete values are included.
    pub const INTERPOLATED: &[&str] = &[ITEM_ID, REF_BINDINGS, PARAMS, CALL, FACETS];
}

/// Runtime-owned control keys carried in dispatch/launch params — parent budget,
/// parent thread, tree depth, and the continuation seed. Defined ONCE here (the
/// crate both the graph dispatcher and the executor launch depend on) so the
/// injector, the input-stripper, and the daemon seed path reference the same
/// names rather than duplicating string literals that can silently drift.
pub const PARAM_PARENT_LIMITS: &str = "parent_limits";
pub const PARAM_PARENT_THREAD_ID: &str = "parent_thread_id";
pub const PARAM_DEPTH: &str = "depth";
pub const PARAM_CONTINUATION: &str = "continuation";

/// Control keys stripped from directive prompt inputs (all runtime-owned).
pub const RESERVED_CONTROL_KEYS: &[&str] = &[
    PARAM_PARENT_LIMITS,
    PARAM_PARENT_THREAD_ID,
    PARAM_DEPTH,
    PARAM_CONTINUATION,
];

#[async_trait]
pub trait RuntimeCallbackAPI: Send + Sync {
    async fn dispatch_action(&self, request: DispatchActionRequest)
        -> Result<Value, CallbackError>;

    async fn attach_process(&self, thread_id: &str, pid: u32) -> Result<Value, CallbackError>;

    async fn mark_running(&self, thread_id: &str) -> Result<Value, CallbackError>;

    async fn finalize_thread(
        &self,
        thread_id: &str,
        completion: TerminalCompletion,
    ) -> Result<Value, CallbackError>;

    async fn get_thread(&self, thread_id: &str) -> Result<Value, CallbackError>;

    /// Machine continuation handoff: the running source was cut off by a limit
    /// mid-task and asks the daemon to spawn + launch a chain-fold successor.
    /// Autonomous by construction — carries no reason/gate/mode, only an
    /// optional free-form string for logs.
    async fn request_continuation(
        &self,
        thread_id: &str,
        log_reason: Option<&str>,
        completion: TerminalCompletion,
    ) -> Result<Value, CallbackError>;

    /// Daemon-managed follow handoff: suspend the calling parent and launch a
    /// detached CHILD whose entire continuation chain the parent awaits.
    /// Get-or-create by `follow_key`: idempotent for an already-recorded waiter
    /// (a duplicate call returns the recorded IDs). Recovery of a crash gap —
    /// e.g. the waiter is durable but the detached launch never ran — is handled
    /// by the later reconcile sweep, not this call.
    ///
    /// Daemon-only: minting thread rows, seeding launch identity, and launching
    /// detached processes are things a mock / in-process client cannot do, so the
    /// default refuses. The real UDS client overrides it; graph test mocks that
    /// exercise follow override it to simulate the daemon.
    async fn spawn_follow_child(
        &self,
        request: SpawnFollowChildRequest,
    ) -> Result<Value, CallbackError> {
        let _ = request;
        Err(CallbackError::ActionFailed {
            code: "unsupported".to_string(),
            message: "spawn_follow_child is only supported by the daemon UDS client".to_string(),
            retryable: false,
        })
    }

    async fn append_event(
        &self,
        thread_id: &str,
        event_type: &str,
        payload: Value,
        storage_class: &str,
    ) -> Result<Value, CallbackError>;

    async fn append_events(
        &self,
        thread_id: &str,
        events: Vec<Value>,
    ) -> Result<Value, CallbackError>;

    /// Replay events for a thread or a whole chain. `params` carries
    /// `{ thread_id? , chain_root_id? , after_chain_seq? , limit? }` — a
    /// chain-scoped read (chain_root_id, no thread_id) folds every turn; a
    /// thread-scoped read filters to one thread. The daemon authorizes the
    /// target against the caller's chain.
    async fn replay_events(&self, params: Value) -> Result<Value, CallbackError>;

    async fn bundle_events_append(
        &self,
        thread_id: &str,
        request: Value,
    ) -> Result<Value, CallbackError>;

    async fn bundle_events_read_chain(
        &self,
        thread_id: &str,
        request: Value,
    ) -> Result<Value, CallbackError>;

    async fn bundle_events_scan(
        &self,
        thread_id: &str,
        request: Value,
    ) -> Result<Value, CallbackError>;

    async fn bundle_events_materialize_attachment(
        &self,
        _thread_id: &str,
        _request: Value,
    ) -> Result<Value, CallbackError> {
        Err(CallbackError::ActionFailed {
            code: "unsupported".to_string(),
            message:
                "bundle event attachment materialization is not supported by this callback client"
                    .to_string(),
            retryable: false,
        })
    }

    async fn vault_put(&self, thread_id: &str, request: Value) -> Result<Value, CallbackError>;

    async fn vault_get(&self, thread_id: &str, request: Value) -> Result<Value, CallbackError>;

    async fn vault_delete(&self, thread_id: &str, request: Value) -> Result<Value, CallbackError>;

    async fn vault_list(&self, thread_id: &str, request: Value) -> Result<Value, CallbackError>;

    async fn author_item(&self, _thread_id: &str, _request: Value) -> Result<Value, CallbackError> {
        Err(CallbackError::Transport(anyhow::anyhow!(
            "runtime.author_item callback is not implemented by this client"
        )))
    }

    async fn project_snapshot(
        &self,
        _thread_id: &str,
        _request: Value,
    ) -> Result<Value, CallbackError> {
        Err(CallbackError::Transport(anyhow::anyhow!(
            "runtime.project_snapshot callback is not implemented by this client"
        )))
    }

    async fn claim_commands(&self, thread_id: &str) -> Result<Value, CallbackError>;

    /// Report a claimed command as `completed` or `rejected`. `command_id` is the
    /// numeric id from the claimed `CommandRecord`; `status` must be
    /// `"completed"` or `"rejected"`.
    async fn complete_command(
        &self,
        thread_id: &str,
        command_id: i64,
        status: &str,
        result: Value,
    ) -> Result<Value, CallbackError>;

    async fn publish_artifact(
        &self,
        thread_id: &str,
        artifact: Value,
    ) -> Result<Value, CallbackError>;

    async fn get_facets(&self, thread_id: &str) -> Result<Value, CallbackError>;

    /// Drain-and-persist operator inputs staged for this RUNNING thread,
    /// returning `{ inputs: [LiveInput...] }` in FIFO order. The daemon
    /// appends each as a durable `cognition_in` through the running-guarded path
    /// before returning, so a non-empty result is already in the braid.
    ///
    /// Default: no live input (mocks and runtimes without a live data channel).
    /// Only the real UDS client overrides this.
    async fn poll_input(&self, _thread_id: &str) -> Result<Value, CallbackError> {
        Ok(serde_json::json!({ "inputs": [] }))
    }
}

pub fn client_from_env() -> Box<dyn RuntimeCallbackAPI> {
    let socket_path = crate::daemon_rpc::resolve_daemon_socket_path(None);
    let token = std::env::var("RYEOSD_CALLBACK_TOKEN")
        .expect("RYEOSD_CALLBACK_TOKEN must be set by daemon");
    let tat = std::env::var("RYEOSD_THREAD_AUTH_TOKEN")
        .expect("RYEOSD_THREAD_AUTH_TOKEN must be set by daemon");
    if socket_path.exists() {
        Box::new(crate::callback_uds::UdsRuntimeClient::new(
            socket_path,
            token,
            tat,
        ))
    } else {
        panic!("UDS socket not found at {}", socket_path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn action_payload_omits_call_when_none() {
        let payload = ActionPayload {
            operation_id: None,
            item_id: "tool:t/echo".to_string(),
            ref_bindings: BTreeMap::new(),
            params: json!({}),
            thread: "inline".to_string(),
            call: None,
            facets: None,
            launch_window: None,
        };
        let v = serde_json::to_value(&payload).unwrap();
        assert!(
            v.get("call").is_none(),
            "call must be skipped when None, got: {v}"
        );
    }

    #[test]
    fn action_payload_round_trips_call() {
        let wire = json!({
            "item_id": "knowledge:arc/resources",
            "ref_bindings": {},
            "params": {},
            "thread": "inline",
            "call": { "method": "query", "args": { "query": "hint", "limit": 5 } },
        });
        let payload: ActionPayload = serde_json::from_value(wire).unwrap();
        let call = payload.call.expect("call present");
        assert_eq!(call.method(), Some("query"));
        assert_eq!(call.args().unwrap()["limit"], 5);
    }

    #[test]
    fn action_payload_defaults_call_to_none() {
        // A wire payload with no `call` (the common case) deserializes fine.
        let wire = json!({
            "item_id": "tool:t/echo",
            "ref_bindings": {},
            "thread": "inline"
        });
        let payload: ActionPayload = serde_json::from_value(wire).unwrap();
        assert!(payload.call.is_none());
    }

    #[test]
    fn hook_action_uses_shared_execute_shape_and_defaults_inline() {
        let payload = parse_hook_action(json!({
            "primary": "execute",
            "item_id": "tool:t/echo",
            "ref_bindings": {},
            "params": {"message": "hi"}
        }))
        .unwrap();
        assert_eq!(payload.item_id, "tool:t/echo");
        assert_eq!(payload.thread, "inline");
        assert_eq!(payload.params["message"], "hi");
    }

    #[test]
    fn hook_action_rejects_drift_and_malformed_required_fields() {
        for invalid in [
            json!({"item_id": "tool:t/echo"}),
            json!({"item_id": "", "ref_bindings": {}}),
            json!({"params": {}, "ref_bindings": {}}),
            json!({"item_id": "tool:t/echo", "ref_bindings": {}, "thread": ""}),
            json!({"primary": "dispatch", "item_id": "tool:t/echo", "ref_bindings": {}}),
            json!({"item_id": "tool:t/echo", "ref_bindings": {}, "legacy": true}),
        ] {
            assert!(parse_hook_action(invalid).is_err());
        }
    }

    #[test]
    fn hook_dispatch_occurrence_uses_closed_tagged_wire() {
        let occurrence = HookDispatchOccurrence::GraphStepCompleted {
            graph_run_id: "graph-run-1".to_string(),
            definition_ref: "graph:test/workflow".to_string(),
            definition_hash: "definition-hash".to_string(),
            step: 9,
            node: "work".to_string(),
        };
        let wire = serde_json::to_value(&occurrence).unwrap();
        assert_eq!(wire["kind"], "graph_step_completed");
        assert_eq!(wire["graph_run_id"], "graph-run-1");
        assert_eq!(wire["step"], 9);
        assert_eq!(occurrence.event(), "graph_step_completed");

        assert!(serde_json::from_value::<HookDispatchOccurrence>(json!({
            "kind": "graph_step_finished",
            "graph_run_id": "graph-run-1",
            "step": 9,
        }))
        .is_err());
        assert!(serde_json::from_value::<HookDispatchOccurrence>(json!({
            "kind": "directive_after_step",
            "turn": 2,
            "legacy_event": "after_step",
        }))
        .is_err());
    }

    #[test]
    fn dispatch_action_hook_identity_round_trips_exactly() {
        let request = DispatchActionRequest {
            thread_id: "T-hook".to_string(),
            project_path: "/project".to_string(),
            action: ActionPayload {
                operation_id: None,
                item_id: "tool:test/hook".to_string(),
                ref_bindings: BTreeMap::new(),
                params: json!({"audit": true}),
                thread: "inline".to_string(),
                call: None,
                facets: None,
                launch_window: None,
            },
            hook_dispatch: Some(HookDispatchIdentity {
                occurrence: HookDispatchOccurrence::DirectiveContinuation {
                    definition_ref: "directive:test/runner".to_string(),
                    definition_hash: "definition-hash".to_string(),
                    turn: 3,
                },
                hook_id: "continuation-audit".to_string(),
                layer: crate::hooks_loader::HookLayer::Operator,
                context_hash: "context-digest".to_string(),
            }),
        };

        let wire = serde_json::to_value(&request).unwrap();
        let round_trip: DispatchActionRequest = serde_json::from_value(wire).unwrap();
        assert_eq!(round_trip.hook_dispatch, request.hook_dispatch);
    }

    #[test]
    fn terminal_completion_serializes_outputs_and_warnings() {
        // The UDS client serializes the WHOLE completion (anti-drift), so the wire
        // must carry outputs + warnings — a hand-listed param set previously
        // dropped them, losing a follow child's structured return.
        let completion = TerminalCompletion {
            status: crate::ThreadTerminalStatus::Completed,
            outcome_code: Some("success".to_string()),
            result: Some(json!("directive_return")),
            error: None,
            cost: None,
            outputs: json!({ "recommendations": ["a"] }),
            warnings: vec!["w1".to_string()],
        };
        let v = serde_json::to_value(&completion).unwrap();
        assert_eq!(v["outputs"]["recommendations"], json!(["a"]));
        assert_eq!(v["warnings"], json!(["w1"]));
    }

    #[test]
    fn terminal_completion_requires_every_exact_wire_key() {
        let complete = json!({
            "status": "completed",
            "outcome_code": null,
            "result": null,
            "error": null,
            "cost": null,
            "outputs": null,
            "warnings": [],
        });
        assert!(serde_json::from_value::<TerminalCompletion>(complete.clone()).is_ok());

        for key in [
            "status",
            "outcome_code",
            "result",
            "error",
            "cost",
            "outputs",
            "warnings",
        ] {
            let mut incomplete = complete.clone();
            incomplete.as_object_mut().unwrap().remove(key);
            assert!(
                serde_json::from_value::<TerminalCompletion>(incomplete).is_err(),
                "omitting `{key}` must violate the terminal completion wire contract"
            );
        }
    }
}
