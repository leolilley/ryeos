use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DispatchActionRequest {
    pub thread_id: String,
    pub project_path: String,
    pub action: ActionPayload,
}

/// Terminal completion a runtime sends when it self-finalizes a thread.
///
/// `cost` is carried as raw JSON so the runtime callback wire does not couple
/// to a cross-crate cost type; the daemon maps it into its own cost record.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalCompletion {
    pub status: String,
    #[serde(default)]
    pub outcome_code: Option<String>,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<Value>,
    #[serde(default)]
    pub cost: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionPayload {
    pub item_id: String,
    #[serde(default)]
    pub params: Value,
    pub thread: String,
    /// Optional method call mirroring the `/execute` `call` block, so a graph
    /// node action can select a non-default method (e.g. knowledge `query`).
    /// Absent for actions that take the kind's default method, and for kinds
    /// that declare no methods.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call: Option<MethodCall>,
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
    ) -> Result<Value, CallbackError>;

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

    async fn vault_put(&self, thread_id: &str, request: Value) -> Result<Value, CallbackError>;

    async fn vault_get(&self, thread_id: &str, request: Value) -> Result<Value, CallbackError>;

    async fn vault_delete(&self, thread_id: &str, request: Value) -> Result<Value, CallbackError>;

    async fn vault_list(&self, thread_id: &str, request: Value) -> Result<Value, CallbackError>;

    async fn author_item(&self, _thread_id: &str, _request: Value) -> Result<Value, CallbackError> {
        Err(CallbackError::Transport(anyhow::anyhow!(
            "runtime.author_item callback is not implemented by this client"
        )))
    }

    async fn claim_commands(&self, thread_id: &str) -> Result<Value, CallbackError>;

    async fn complete_command(
        &self,
        thread_id: &str,
        command_id: &str,
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
            item_id: "tool:t/echo".to_string(),
            params: json!({}),
            thread: "inline".to_string(),
            call: None,
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
        let wire = json!({ "item_id": "tool:t/echo", "thread": "inline" });
        let payload: ActionPayload = serde_json::from_value(wire).unwrap();
        assert!(payload.call.is_none());
    }
}
