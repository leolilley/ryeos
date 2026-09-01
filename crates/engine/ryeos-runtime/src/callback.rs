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

pub const RUNTIME_ACTION_OUTCOME_UNKNOWN_CODE: &str = "runtime_action_outcome_unknown";
pub const RUNTIME_ACTION_RESULT_UNAVAILABLE_CODE: &str = "runtime_action_result_unavailable";

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

    /// The request may have crossed the daemon's action boundary, but the
    /// caller did not receive authoritative settlement. Kind runtimes must
    /// leave the owning thread unfinished and re-drive the same operation ID;
    /// converting this into an authored retry or ordinary failure could
    /// duplicate an effect or discard a retained result.
    pub fn runtime_action_outcome_unknown(&self) -> bool {
        match self {
            Self::Transport(_) => true,
            Self::ActionFailed { code, .. } => {
                code == RUNTIME_ACTION_OUTCOME_UNKNOWN_CODE
                    || code == RUNTIME_ACTION_RESULT_UNAVAILABLE_CODE
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DispatchActionRequest {
    pub thread_id: String,
    pub action: ActionPayload,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hook_dispatch: Option<HookDispatchIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect_dispatch: Option<EffectDispatchRequest>,
}

/// Opaque authorization selected by a kind runtime for one dispatch.
///
/// The callback capability contains the complete admitted authorization. The
/// runtime can select an ID but cannot assert a class, policy digest, source
/// identity, cache key, or callee coordinate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectDispatchRequest {
    pub authorization_id: String,
}

/// Runtime-to-daemon request for one source-scoped project observation. The
/// runtime owns only the graph occurrence and the bounded source request; the
/// daemon derives chain and admitted source identity from callback authority.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectObservationPublishParams {
    pub thread_id: String,
    pub graph_run_id: String,
    pub node: String,
    pub step: u32,
    pub observation: crate::ProjectObservationRequest,
}

/// Canonical digest of the behavior-bearing action admitted for dispatch.
///
/// `operation_id` names one runtime-asserted occurrence. It is deliberately
/// excluded here so recorded-effect identity remains reusable across two
/// behaviorally identical admitted occurrences. The daemon binds the opaque
/// occurrence to this independently derived digest in its runtime-action
/// intent before child contact.
pub fn dispatch_action_digest(action: &ActionPayload) -> anyhow::Result<String> {
    let mut behavior = action.clone();
    behavior.operation_id = None;
    let value = serde_json::to_value(behavior)?;
    let canonical = lillux::cas::canonical_json(&value)?;
    Ok(lillux::sha256_hex(canonical.as_bytes()))
}

/// Whether a runtime action occurrence uses the one canonical wire spelling:
/// exactly 32 bytes rendered as 64 lowercase hexadecimal characters.
///
/// Keep this with [`ActionPayload`] rather than borrowing the CAS path helper:
/// CAS lookup accepts case-insensitive hexadecimal input, while action
/// occurrence identity is compared and persisted as an exact protocol value.
pub fn valid_action_operation_id(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

/// One scalar coordinate in a runtime-owned hook occurrence.
///
/// Occurrence coordinates are deliberately bounded to strings and counters:
/// they identify a lifecycle boundary but cannot smuggle an unbounded nested
/// document across the callback authority boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum HookDispatchCoordinate {
    Text(String),
    Counter(u32),
}

/// Stable logical occurrence of one runtime-hook event.
///
/// The signed kind contract and launch-captured hook authorization own the
/// `(owner_kind, event)` vocabulary. The generic callback substrate binds that
/// pair to the admitted effective definition and opaque scalar coordinates;
/// adding a hook-capable kind or event therefore requires no executor wire
/// change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookDispatchOccurrence {
    pub owner_kind: String,
    pub event: String,
    pub definition_ref: String,
    pub root_raw_content_digest: String,
    pub effective_definition_digest: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub coordinates: BTreeMap<String, HookDispatchCoordinate>,
}

impl HookDispatchOccurrence {
    pub fn new(
        owner_kind: impl Into<String>,
        event: impl Into<String>,
        definition_ref: impl Into<String>,
        root_raw_content_digest: impl Into<String>,
        effective_definition_digest: impl Into<String>,
    ) -> Self {
        Self {
            owner_kind: owner_kind.into(),
            event: event.into(),
            definition_ref: definition_ref.into(),
            root_raw_content_digest: root_raw_content_digest.into(),
            effective_definition_digest: effective_definition_digest.into(),
            coordinates: BTreeMap::new(),
        }
    }

    pub fn with_text_coordinate(
        mut self,
        key: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        self.coordinates
            .insert(key.into(), HookDispatchCoordinate::Text(value.into()));
        self
    }

    pub fn with_counter_coordinate(mut self, key: impl Into<String>, value: u32) -> Self {
        self.coordinates
            .insert(key.into(), HookDispatchCoordinate::Counter(value));
        self
    }

    pub fn text_coordinate(&self, key: &str) -> Option<&str> {
        match self.coordinates.get(key) {
            Some(HookDispatchCoordinate::Text(value)) => Some(value),
            _ => None,
        }
    }

    pub fn counter_coordinate(&self, key: &str) -> Option<u32> {
        match self.coordinates.get(key) {
            Some(HookDispatchCoordinate::Counter(value)) => Some(*value),
            _ => None,
        }
    }

    pub fn event(&self) -> &str {
        &self.event
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
    pub result_mode: crate::hooks_loader::HookResultMode,
    pub context_contract: ryeos_engine::hooks::HookContextContract,
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
    pub graph_run_id: String,
    pub follow_node: String,
    pub step_count: i64,
    /// Closed result shape selected by the graph operation. A cohort remains a
    /// cohort when filtering leaves exactly one child; cardinality never
    /// rewrites the checkpoint wire contract.
    pub result_shape: FollowResultShape,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FollowResultShape {
    Single,
    Cohort,
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

/// Versioned failure payload returned by native runtimes. The bounded summary
/// is safe to propagate through parent graphs; the locator points to the
/// child's durable, lossless terminal diagnostic.
pub const RUNTIME_FAILURE_KIND: &str = "runtime_failure";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeFailure {
    pub kind: String,
    pub version: u8,
    pub code: String,
    pub summary: String,
    pub diagnostic_locator: RuntimeFailureDiagnosticLocator,
    pub retryable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeFailureDiagnosticLocator {
    pub thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt_id: Option<String>,
    pub event_type: String,
}

impl RuntimeFailure {
    pub fn validate(&self) -> Result<(), String> {
        if self.kind != RUNTIME_FAILURE_KIND {
            return Err(format!("unsupported runtime failure kind `{}`", self.kind));
        }
        if self.version != 1 {
            return Err(format!(
                "unsupported runtime failure version {}",
                self.version
            ));
        }
        if self.code.is_empty()
            || self.code.len() > 64
            || !self
                .code
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte == b'_' || byte.is_ascii_digit())
        {
            return Err(
                "runtime failure code must be 1..=64 lowercase ASCII letters/digits/underscores"
                    .to_string(),
            );
        }
        if self.summary.is_empty()
            || self.summary.chars().count() > 4_096
            || self
                .summary
                .chars()
                .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
        {
            return Err(
                "runtime failure summary is empty, oversized, or contains control characters"
                    .to_string(),
            );
        }
        let locator = &self.diagnostic_locator;
        validate_runtime_thread_id(&locator.thread_id)?;
        if locator.event_type != "thread_failed" {
            return Err("runtime failure locator event_type must be thread_failed".to_string());
        }
        if locator.attempt_id.as_ref().is_some_and(|attempt_id| {
            attempt_id.is_empty()
                || attempt_id.len() > 256
                || attempt_id.chars().any(char::is_control)
        }) {
            return Err("runtime failure locator has an invalid attempt_id".to_string());
        }
        Ok(())
    }
}

pub fn validate_runtime_thread_id(thread_id: &str) -> Result<(), String> {
    if thread_id.len() < 3
        || thread_id.len() > 128
        || !thread_id.starts_with("T-")
        || !thread_id
            .bytes()
            .skip(2)
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err("runtime thread_id is invalid".to_string());
    }
    Ok(())
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
    /// Stable opaque identity asserted by the admitted runtime for one logical
    /// action occurrence. Ordinary inline and detached callbacks require it so
    /// daemon crash/retry cannot execute the occurrence through a second child.
    /// Admitted hook callbacks use their separate hook occurrence ledger.
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
    pub const OPERATION_ID: &str = "operation_id";
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

/// Request to bind one already-admitted exclusive subprocess dependency to
/// the calling runtime's durable root and workspace. Integration-specific
/// runtimes name the dependency and environment slots; the daemon derives all
/// paths and authority from retained launch/profile state.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DedicatedSessionStartRequest {
    pub thread_id: String,
    pub dependency_ref: String,
    pub credential_profile_id: String,
    pub required_credential_state: String,
    /// Exact signed route set selected by the root execution. The worker
    /// profile admits the route IDs; callers cannot widen this after launch.
    pub route_set: String,
    /// Sorted, unique effect-class ceiling admitted by the root launch.
    pub allowed_effect_classes: Vec<String>,
    pub credential_home_env: String,
    pub workspace_env: String,
    pub require_pinned_cow: bool,
    pub required_terminal_publication: String,
    /// Whether this execution may recover a retained upstream session after a
    /// worker restart. The daemon verifies this against the signed protocol
    /// profile before launching the worker.
    pub recover_upstream_session: bool,
}

/// One opaque command issued by the integration runtime that owns a dedicated
/// session root. The daemon supplies the durable at-most-once boundary; the
/// signed runtime and worker protocol own the payload meaning.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DedicatedSessionCommandRequest {
    pub thread_id: String,
    pub idempotency_key: String,
    pub command_kind: String,
    pub payload: Value,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DedicatedSessionTerminateRequest {
    pub thread_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DedicatedSessionWaitRequest {
    pub thread_id: String,
    pub observed_updated_at_ms: i64,
    pub timeout_ms: u64,
}

#[async_trait]
pub trait RuntimeCallbackAPI: Send + Sync {
    async fn dispatch_action(&self, request: DispatchActionRequest)
    -> Result<Value, CallbackError>;

    async fn attach_process(&self, thread_id: &str, pid: u32) -> Result<Value, CallbackError>;

    /// Feature-gated daemon crash-qualification seam.
    ///
    /// Production daemons do not serve this method. A runtime calls it only
    /// when an explicit test-only phase selection is present; the qualifying
    /// daemon reports that exact boundary to its parent and deliberately never
    /// answers. Keeping the vocabulary opaque here prevents the callback
    /// substrate from learning runtime- or kind-specific phases.
    #[doc(hidden)]
    async fn reach_test_phase_cut(
        &self,
        _thread_id: &str,
        _phase: &str,
    ) -> Result<Value, CallbackError> {
        Err(CallbackError::ActionFailed {
            code: "unsupported".to_string(),
            message: "runtime phase cuts require an explicit daemon test-support build".to_string(),
            retryable: false,
        })
    }

    async fn start_dedicated_session(
        &self,
        request: DedicatedSessionStartRequest,
    ) -> Result<Value, CallbackError> {
        let _ = request;
        Err(CallbackError::ActionFailed {
            code: "unsupported".to_string(),
            message: "dedicated sessions are only supported by the daemon UDS client".to_string(),
            retryable: false,
        })
    }

    async fn dedicated_session_status(&self, _thread_id: &str) -> Result<Value, CallbackError> {
        Err(CallbackError::ActionFailed {
            code: "unsupported".to_string(),
            message: "dedicated sessions are only supported by the daemon UDS client".to_string(),
            retryable: false,
        })
    }

    async fn wait_dedicated_session(
        &self,
        request: DedicatedSessionWaitRequest,
    ) -> Result<Value, CallbackError> {
        let _ = request;
        Err(CallbackError::ActionFailed {
            code: "unsupported".to_string(),
            message: "dedicated sessions are only supported by the daemon UDS client".to_string(),
            retryable: false,
        })
    }

    async fn dedicated_session_command(
        &self,
        request: DedicatedSessionCommandRequest,
    ) -> Result<Value, CallbackError> {
        let _ = request;
        Err(CallbackError::ActionFailed {
            code: "unsupported".to_string(),
            message: "dedicated sessions are only supported by the daemon UDS client".to_string(),
            retryable: false,
        })
    }

    async fn terminate_dedicated_session(
        &self,
        request: DedicatedSessionTerminateRequest,
    ) -> Result<Value, CallbackError> {
        let _ = request;
        Err(CallbackError::ActionFailed {
            code: "unsupported".to_string(),
            message: "dedicated sessions are only supported by the daemon UDS client".to_string(),
            retryable: false,
        })
    }

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

    /// Publish an opaque restore closure and its authoritative state-anchor
    /// milestone as one daemon-owned operation. Runtimes without this exact
    /// callback must fail rather than emitting a partial anchor.
    async fn publish_state_anchor(
        &self,
        _thread_id: &str,
        _request: Value,
    ) -> Result<Value, CallbackError> {
        Err(CallbackError::Transport(anyhow::anyhow!(
            "runtime.publish_state_anchor callback is not implemented by this client"
        )))
    }

    /// Publish an idempotent, daemon-authored project observation. This is a
    /// settlement-significant graph-commit boundary, not advisory telemetry.
    async fn publish_project_observation(
        &self,
        _params: ProjectObservationPublishParams,
    ) -> Result<Value, CallbackError> {
        Err(CallbackError::Transport(anyhow::anyhow!(
            "runtime.publish_project_observation callback is not implemented by this client"
        )))
    }

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

    /// Provider-attempt budget lifecycle (reserve → mark issued → settle,
    /// with unissued release and exact recovery reads). Daemon-ledger
    /// authority: the runtime asserts intent coordinates and digests only.
    ///
    /// Defaults refuse: a directive test must provide explicit accounting
    /// behavior — a missing method never silently degrades into settled mode.
    async fn provider_attempt_prepare(
        &self,
        _thread_id: &str,
        _params: Value,
    ) -> Result<Value, CallbackError> {
        Err(CallbackError::ActionFailed {
            code: "unsupported".to_string(),
            message: "provider_attempt_prepare is only supported by the daemon UDS client"
                .to_string(),
            retryable: false,
        })
    }

    async fn provider_attempt_mark_issued(
        &self,
        _thread_id: &str,
        _params: Value,
    ) -> Result<Value, CallbackError> {
        Err(CallbackError::ActionFailed {
            code: "unsupported".to_string(),
            message: "provider_attempt_mark_issued is only supported by the daemon UDS client"
                .to_string(),
            retryable: false,
        })
    }

    async fn provider_attempt_settle(
        &self,
        _thread_id: &str,
        _params: Value,
    ) -> Result<Value, CallbackError> {
        Err(CallbackError::ActionFailed {
            code: "unsupported".to_string(),
            message: "provider_attempt_settle is only supported by the daemon UDS client"
                .to_string(),
            retryable: false,
        })
    }

    async fn provider_attempt_release_unissued(
        &self,
        _thread_id: &str,
        _params: Value,
    ) -> Result<Value, CallbackError> {
        Err(CallbackError::ActionFailed {
            code: "unsupported".to_string(),
            message: "provider_attempt_release_unissued is only supported by the daemon UDS client"
                .to_string(),
            retryable: false,
        })
    }

    async fn provider_attempt_get(
        &self,
        _thread_id: &str,
        _params: Value,
    ) -> Result<Value, CallbackError> {
        Err(CallbackError::ActionFailed {
            code: "unsupported".to_string(),
            message: "provider_attempt_get is only supported by the daemon UDS client".to_string(),
            retryable: false,
        })
    }

    async fn provider_attempt_local_stream_start(
        &self,
        _thread_id: &str,
        _params: Value,
    ) -> Result<Value, CallbackError> {
        Err(CallbackError::ActionFailed {
            code: "unsupported".to_string(),
            message:
                "provider_attempt_local_stream_start is only supported by the daemon UDS client"
                    .to_string(),
            retryable: false,
        })
    }

    async fn provider_attempt_local_stream_next(
        &self,
        _thread_id: &str,
        _params: Value,
    ) -> Result<Value, CallbackError> {
        Err(CallbackError::ActionFailed {
            code: "unsupported".to_string(),
            message:
                "provider_attempt_local_stream_next is only supported by the daemon UDS client"
                    .to_string(),
            retryable: false,
        })
    }

    async fn provider_attempt_local_stream_control(
        &self,
        _thread_id: &str,
        _params: Value,
    ) -> Result<Value, CallbackError> {
        Err(CallbackError::ActionFailed {
            code: "unsupported".to_string(),
            message:
                "provider_attempt_local_stream_control is only supported by the daemon UDS client"
                    .to_string(),
            retryable: false,
        })
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
    fn runtime_failure_contract_rejects_unsupported_or_unbounded_values() {
        let valid = RuntimeFailure {
            kind: RUNTIME_FAILURE_KIND.to_string(),
            version: 1,
            code: "provider_protocol_error".to_string(),
            summary: "precise failure".to_string(),
            diagnostic_locator: RuntimeFailureDiagnosticLocator {
                thread_id: "T-child".to_string(),
                turn: Some(2),
                attempt_id: Some("T-child:2:1".to_string()),
                event_type: "thread_failed".to_string(),
            },
            retryable: false,
        };
        assert!(valid.validate().is_ok());

        let mut unsupported = valid.clone();
        unsupported.version = 2;
        assert!(unsupported.validate().unwrap_err().contains("unsupported"));

        let mut oversized = valid;
        oversized.summary = "x".repeat(4_097);
        assert!(oversized.validate().is_err());

        for unsafe_id in ["T-child;tail", "T-child name", "T-`child`"] {
            assert!(
                validate_runtime_thread_id(unsafe_id).is_err(),
                "{unsafe_id}"
            );
        }
    }

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
    fn hook_dispatch_occurrence_uses_generic_scalar_coordinate_wire() {
        let occurrence = HookDispatchOccurrence::new(
            "graph",
            "graph_step_completed",
            "graph:test/workflow",
            "a".repeat(64),
            "b".repeat(64),
        )
        .with_text_coordinate("graph_run_id", "graph-run-1")
        .with_counter_coordinate("step", 9)
        .with_text_coordinate("node", "work");
        let wire = serde_json::to_value(&occurrence).unwrap();
        assert_eq!(wire["owner_kind"], "graph");
        assert_eq!(wire["event"], "graph_step_completed");
        assert_eq!(wire["coordinates"]["graph_run_id"], "graph-run-1");
        assert_eq!(wire["coordinates"]["step"], 9);
        assert_eq!(occurrence.event(), "graph_step_completed");

        let future = serde_json::from_value::<HookDispatchOccurrence>(json!({
            "owner_kind": "future_runtime",
            "event": "phase_settled",
            "definition_ref": "future_runtime:test/item",
            "root_raw_content_digest": "a".repeat(64),
            "effective_definition_digest": "b".repeat(64),
            "coordinates": {"phase": 9}
        }))
        .unwrap();
        assert_eq!(future.event(), "phase_settled");
        assert!(
            serde_json::from_value::<HookDispatchOccurrence>(json!({
                "owner_kind": "graph",
                "event": "graph_step_completed",
                "definition_ref": "graph:test/workflow",
                "root_raw_content_digest": "a".repeat(64),
                "effective_definition_digest": "b".repeat(64),
                "coordinates": {"nested": {"not": "a scalar"}},
            }))
            .is_err()
        );
    }

    #[test]
    fn dispatch_action_hook_identity_round_trips_exactly() {
        let request = DispatchActionRequest {
            thread_id: "T-hook".to_string(),
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
                occurrence: HookDispatchOccurrence::new(
                    "directive",
                    "continuation",
                    "directive:test/runner",
                    "a".repeat(64),
                    "b".repeat(64),
                )
                .with_counter_coordinate("turn", 3),
                hook_id: "continuation-audit".to_string(),
                layer: crate::hooks_loader::HookLayer::Operator,
                result_mode: crate::hooks_loader::HookResultMode::Control,
                context_contract: ryeos_engine::hooks::HookContextContract {
                    schema: ryeos_engine::hooks::HOOK_CONTEXT_SCHEMA.to_string(),
                    allowed_roots: std::collections::BTreeSet::from(["event".to_string()]),
                },
                context_hash: "context-digest".to_string(),
            }),
            effect_dispatch: None,
        };

        let wire = serde_json::to_value(&request).unwrap();
        let round_trip: DispatchActionRequest = serde_json::from_value(wire).unwrap();
        assert_eq!(round_trip.hook_dispatch, request.hook_dispatch);
    }

    #[test]
    fn action_digest_excludes_occurrence_but_binds_behavior() {
        let action = ActionPayload {
            operation_id: Some("1".repeat(64)),
            item_id: "tool:test/mutate".to_string(),
            ref_bindings: BTreeMap::new(),
            params: json!({"value": 1}),
            thread: "inline".to_string(),
            call: None,
            facets: None,
            launch_window: None,
        };
        let original = dispatch_action_digest(&action).unwrap();

        let mut different_occurrence = action.clone();
        different_occurrence.operation_id = Some("2".repeat(64));
        assert_eq!(
            dispatch_action_digest(&different_occurrence).unwrap(),
            original
        );

        let mut different_behavior = action;
        different_behavior.params = json!({"value": 2});
        assert_ne!(
            dispatch_action_digest(&different_behavior).unwrap(),
            original
        );
    }

    #[test]
    fn action_operation_id_has_one_canonical_wire_spelling() {
        assert!(valid_action_operation_id(&"0".repeat(64)));
        assert!(valid_action_operation_id(&("abcdef".repeat(10) + "abcd")));
        assert!(!valid_action_operation_id(&"A".repeat(64)));
        assert!(!valid_action_operation_id(&"g".repeat(64)));
        assert!(!valid_action_operation_id(&"0".repeat(63)));
    }

    #[test]
    fn unavailable_runtime_action_result_requires_recovery() {
        let error = CallbackError::ActionFailed {
            code: RUNTIME_ACTION_RESULT_UNAVAILABLE_CODE.to_owned(),
            message: "the retained body was intentionally discarded".to_owned(),
            retryable: false,
        };
        assert!(error.runtime_action_outcome_unknown());
        assert!(!error.retryable());
    }

    #[test]
    fn callback_requests_reject_caller_supplied_project_path_authority() {
        let request = json!({
            "thread_id": "T-test",
            "project_path": "/host/project",
            "action": {
                "item_id": "tool:test/echo",
                "ref_bindings": {},
                "params": {},
                "thread": "inline"
            }
        });
        assert!(serde_json::from_value::<DispatchActionRequest>(request).is_err());

        let follow = json!({
            "thread_id": "T-test",
            "project_path": "/host/project",
            "graph_run_id": "gr-test",
            "follow_node": "child",
            "step_count": 0,
            "result_shape": "single",
            "children": [{
                "item_ref": "graph:test/child",
                "ref_bindings": {},
                "parameters": {}
            }],
            "completion": {
                "status": "continued",
                "outcome_code": "continued",
                "result": null,
                "error": null,
                "cost": null
            }
        });
        assert!(serde_json::from_value::<SpawnFollowChildRequest>(follow).is_err());
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
