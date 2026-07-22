use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{json, Value};

use ryeos_runtime::callback_client::CallbackClient;
use ryeos_runtime::envelope::{RuntimeCost, RuntimeResultStatus};

use crate::context::ExecutionContext;
use crate::model::{FanoutItemStatus, GraphResult, GraphRunStatus};

const NATIVE_FAILURE_DIAGNOSTIC_CHARS: usize = 4_096;

/// Outcome of dispatching a single graph action leaf, classified from
/// the daemon execute envelope BEFORE the bare result is unwrapped.
///
/// The daemon wraps tool output in an audit envelope — `{outcome_code,
/// result, error, artifacts}` for subprocess leaves, `{success, status,
/// result, outputs, warnings}` for native-runtime leaves. A *failed*
/// leaf carries `result: null` with the diagnostic in `error`/`status`,
/// so unconditionally peeling to the bare `result` would turn a failure
/// into a silent `null` success that then poisons graph state via
/// suppressed expression errors. Classification happens once, here,
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
    /// Whether the same authored action is eligible for another attempt.
    /// Executed leaf failures default false; callback dispatch classification is
    /// carried by [`ActionDispatchError`] before an envelope exists.
    pub retryable: bool,
    /// The native child thread that returned this failure, when dispatch already
    /// created one. Preserved so failure paths publish the same lineage edge as
    /// successful child dispatches.
    pub child_thread_id: Option<String>,
    /// The child returned an authoritative envelope whose structure, status,
    /// or cost provenance is invalid. Such failures cannot be authored around
    /// with `on_error`, because doing so could settle after losing accounting.
    pub integrity: bool,
}

#[derive(Debug, thiserror::Error)]
#[error("{diagnostic}")]
pub struct ActionDispatchError {
    pub diagnostic: String,
    pub retryable: bool,
}

impl From<anyhow::Error> for ActionDispatchError {
    fn from(error: anyhow::Error) -> Self {
        Self {
            diagnostic: format!("{error:#}"),
            retryable: false,
        }
    }
}

/// A successful leaf dispatch: the graph-visible result plus optional
/// cost reported by a native child runtime.
#[derive(Debug)]
pub struct ActionSuccess {
    /// Bare, envelope-unwrapped result for `${result.*}` evaluation.
    ///
    /// For a native directive return carrying declared `outputs`, this is
    /// `{result: <inner>, outputs: <outputs>}` so a graph can reach the
    /// directive's structured outputs as `${result.outputs.X}`. The inner
    /// `result` of a directive return is the synthetic sentinel
    /// `"directive_return"` — not meaningful graph data — so the outputs
    /// are the payload. A `graph:*` child keeps its complete typed
    /// [`GraphResult`] in the durable runtime envelope while exposing that
    /// DTO's authored `result` here. For every other leaf (subprocess, bare
    /// value, native return with no outputs) this is the bare inner result and
    /// the shape is unchanged.
    pub result: Value,
    /// Token/spend cost reported by a native child runtime (directive or
    /// sub-graph) in the envelope's `cost` field. `None` for subprocess
    /// leaves, cache hits, and bare values — cost is never invented.
    pub cost: Option<RuntimeCost>,
    /// The spawned child thread's id, when this dispatch launched a native
    /// child thread (a directive or sub-graph). `None` for subprocess/tool
    /// leaves, bare values, and cache hits — nothing new was spawned. The
    /// walker emits a `child_thread_spawned` event from this so the dispatch
    /// edge lands in the parent's portable braid.
    pub child_thread_id: Option<String>,
}

impl ActionSuccess {
    /// A success with no accounting metadata — subprocess leaves, bare
    /// tool output, and cache hits. A cache hit replays a stored result
    /// and must NOT re-bill cost, so the walker rebuilds the outcome with
    /// this constructor.
    pub fn bare(result: Value) -> Self {
        Self {
            result,
            cost: None,
            child_thread_id: None,
        }
    }
}

#[tracing::instrument(
    name = "tool:execute",
    skip(client, action, _exec_ctx),
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
    _exec_ctx: Option<&ExecutionContext>,
) -> Result<ActionOutcome, ActionDispatchError> {
    let action = action.clone();

    let item_id = action.get("item_id").and_then(|v| v.as_str()).unwrap_or("");
    tracing::Span::current().record("tool_name", item_id);
    let params = action.get("params").cloned().unwrap_or(json!({}));
    let ref_bindings = action
        .get("ref_bindings")
        .ok_or_else(|| anyhow::anyhow!("action `{item_id}` is missing required `ref_bindings`"))
        .and_then(|value| {
            serde_json::from_value::<BTreeMap<String, String>>(value.clone())
                .map_err(|e| anyhow::anyhow!("invalid `ref_bindings` for `{item_id}`: {e}"))
        })?;
    let thread = action
        .get("thread")
        .and_then(|v| v.as_str())
        .unwrap_or("inline");

    // Optional method selector. The node's `call: { method, args }` block
    // (already rendered by the walker) maps onto the daemon's
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

    // Cohort/fleet facets ride the action Value (the walker sets them from the
    // node's rendered `facets:`); the daemon stamps them on a detached child.
    let facets = action.get("facets").cloned();

    // Bounded-fanout window (the foreach runners set it for a `detach` node
    // with `max_concurrency`); malformed is an authoring/plumbing error,
    // surfaced loudly.
    let launch_window = match action.get("launch_window") {
        None => None,
        Some(v) if v.is_null() => None,
        Some(v) => Some(
            serde_json::from_value::<ryeos_runtime::callback::LaunchWindow>(v.clone())
                .map_err(|e| anyhow::anyhow!("invalid `launch_window` for `{item_id}`: {e}"))?,
        ),
    };

    let request = ryeos_runtime::callback::DispatchActionRequest {
        thread_id: thread_id.to_string(),
        project_path: project_path.to_string(),
        action: ryeos_runtime::callback::ActionPayload {
            operation_id: action
                .get("operation_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            item_id: item_id.to_string(),
            ref_bindings,
            params,
            thread: thread.to_string(),
            call,
            facets,
            launch_window,
        },
        hook_dispatch: None,
    };

    let response = client
        .dispatch_action(request)
        .await
        .map_err(|error| ActionDispatchError {
            diagnostic: format!("dispatch failed: {error}"),
            retryable: error.retryable(),
        })?;

    // The typed callback contract puts the leaf-dispatcher value in
    // `response.result`; the wrapping `thread` snapshot is for audit
    // only and never feeds into graph-walker control flow. Classify the
    // envelope BEFORE unwrapping so a failed leaf becomes a structured
    // failure rather than a silent `null`. Only success peels to the bare
    // leaf value; a leaf that still requests inline continuation
    // (`continuation_id` at its top level) is then rejected loudly.
    // The `thread` snapshot is the spawned child (a native directive/sub-graph
    // child); capture its id BEFORE classifying `result` so the walker can emit a
    // `child_thread_spawned` event. Empty/absent for subprocess/tool/bare leaves,
    // which spawn no child thread.
    let callback_child_thread_id = response
        .thread
        .get("thread_id")
        .or_else(|| response.thread.get("id"));
    let (child_thread_id, child_thread_id_error) = match callback_child_thread_id {
        None => (None, None),
        Some(Value::String(thread_id)) => {
            match ryeos_runtime::validate_runtime_thread_id(thread_id) {
                Ok(()) => (Some(thread_id.clone()), None),
                Err(error) => (None, Some(error)),
            }
        }
        Some(_) => (None, Some("runtime thread_id is not a string".to_string())),
    };

    match classify_envelope_for_item(response.result, item_id) {
        ActionOutcome::Failure(mut failure) => {
            if let Some(error) = child_thread_id_error {
                failure
                    .diagnostic
                    .push_str(&format!("; invalid dispatched child identity: {error}"));
                failure.integrity = true;
                failure.retryable = false;
                failure.child_thread_id = None;
                return Ok(ActionOutcome::Failure(failure));
            }
            if let Some(authoritative_child_thread_id) = child_thread_id {
                if failure
                    .child_thread_id
                    .as_deref()
                    .is_some_and(|reported| reported != authoritative_child_thread_id)
                {
                    failure.diagnostic.push_str(&format!(
                        "; RuntimeFailure diagnostic thread does not match dispatched child `{authoritative_child_thread_id}`"
                    ));
                    failure.integrity = true;
                    failure.retryable = false;
                }
                failure.child_thread_id = Some(authoritative_child_thread_id);
            }
            if let Some(child_thread_id) = failure.child_thread_id.as_deref() {
                failure.diagnostic.push_str(&format!(
                    "; child_thread_id={child_thread_id}; full child diagnostic: \
                     `ryeos thread tail {child_thread_id}`"
                ));
            }
            Ok(ActionOutcome::Failure(failure))
        }
        ActionOutcome::Success(mut success) => {
            if let Some(error) = child_thread_id_error {
                return Ok(ActionOutcome::Failure(ActionFailure {
                    diagnostic: format!("invalid dispatched child identity: {error}"),
                    cost: success.cost,
                    retryable: false,
                    child_thread_id: None,
                    integrity: true,
                }));
            }
            success.child_thread_id = child_thread_id;
            // Inline continuation-chasing is retired. A dispatched child that
            // requests continuation must be launched from a `follow: true` node
            // (daemon-managed suspend/resume) — never chased synchronously here, which
            // blocked the graph on the whole child chain. A leaf still returning a
            // top-level `continuation_id` on a non-follow node is a loud authoring
            // error, not a silent block. (A native return with meaningful `outputs` is
            // wrapped to `{result, outputs}` and is terminal by contract, so its
            // `continuation_id` — which it never sets — is not the concern here;
            // subprocess and bare leaves keep `continuation_id` at the top level.)
            if success
                .result
                .get("continuation_id")
                .and_then(|v| v.as_str())
                .is_some()
            {
                return Ok(ActionOutcome::Failure(ActionFailure {
                    diagnostic: format!(
                        "action `{item_id}` returned a continuation_id on a non-follow node; \
                         inline continuation is retired — mark the node `follow: true` to run a \
                         continuing child under daemon-managed follow"
                    ),
                    cost: success.cost,
                    retryable: false,
                    child_thread_id: success.child_thread_id,
                    integrity: false,
                }));
            }
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
/// callback contract, so classification MUST happen before the inline-
/// continuation guard reads it.
#[cfg(test)]
fn classify_envelope(value: Value) -> ActionOutcome {
    classify_envelope_with_projection(value, NativeResultProjection::KindDefined)
}

/// Classify a live dispatch result using the exact kind selected by the
/// authored canonical item reference.
///
/// Graph runtimes retain their complete [`GraphResult`] in the durable native
/// envelope. At the graph-expression boundary, however, a graph action exposes
/// the child graph's authored return value. Kind-directed projection keeps that
/// contract explicit and prevents arbitrary non-graph payloads that happen to
/// resemble a `GraphResult` from being unwrapped.
fn classify_envelope_for_item(value: Value, item_ref: &str) -> ActionOutcome {
    let projection = match native_result_projection(item_ref) {
        Ok(projection) => projection,
        Err(diagnostic) => return malformed_native_runtime_failure(diagnostic, None),
    };
    classify_envelope_with_projection(value, projection)
}

fn classify_envelope_with_projection(
    value: Value,
    projection: NativeResultProjection,
) -> ActionOutcome {
    let Some(obj) = value.as_object() else {
        if projection == NativeResultProjection::GraphReturn {
            return malformed_native_runtime_failure(
                "graph action returned a non-native result instead of the required runtime envelope"
                    .to_string(),
                None,
            );
        }
        return ActionOutcome::Success(ActionSuccess::bare(value));
    };

    // Any native marker is authoritative. A partial native envelope must fail
    // closed instead of falling through as successful arbitrary tool data.
    if obj.contains_key("success") || obj.contains_key("status") {
        return classify_native_runtime_envelope(value, projection);
    }

    if projection == NativeResultProjection::GraphReturn {
        return malformed_native_runtime_failure(
            "graph action returned a non-native result instead of the required runtime envelope"
                .to_string(),
            None,
        );
    }

    // Subprocess envelope: discriminated by `outcome_code`.
    if obj.contains_key("outcome_code") {
        return classify_subprocess_envelope(value);
    }

    // Has `result` but no envelope markers — bare tool data.
    ActionOutcome::Success(ActionSuccess::bare(value))
}

/// Strictly classified result received through the daemon-managed follow
/// contract. The terminal status stays typed until a serde/wire boundary.
#[derive(Debug)]
pub(crate) struct ClassifiedFollowEnvelope {
    pub(crate) outcome: ActionOutcome,
    status: RuntimeResultStatus,
}

impl ClassifiedFollowEnvelope {
    pub(crate) const fn fanout_status(&self) -> FanoutItemStatus {
        if self.status.is_success() {
            FanoutItemStatus::Completed
        } else {
            FanoutItemStatus::Failed
        }
    }
}

/// Strictly classified cohort result received through the daemon-managed
/// follow contract. Item order is the checkpointed iteration order.
pub(crate) struct ClassifiedFollowFanoutEnvelope {
    pub(crate) items: Vec<ClassifiedFollowEnvelope>,
    pub(crate) statuses: Vec<FanoutItemStatus>,
}

/// Exact daemon-managed native result consumed by live graph dispatch.
///
/// A native marker is authoritative: once `success` or `status` is present,
/// structural drift is a failed child contract rather than arbitrary tool
/// output. Every canonical field is required, including nullable `cost`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeResultEnvelope {
    success: bool,
    status: RuntimeResultStatus,
    result: Value,
    outputs: Value,
    warnings: Vec<String>,
    cost: Value,
}

#[derive(Debug)]
struct RequiredNullableString(Option<String>);

impl<'de> Deserialize<'de> for RequiredNullableString {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        serde_json::from_value(value)
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

/// Exact daemon execute result consumed for subprocess leaves. The marker is
/// authoritative and every nullable field must still be present on the wire.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SubprocessResultEnvelope {
    outcome_code: RequiredNullableString,
    result: Value,
    error: Value,
    artifacts: Vec<Value>,
}

/// Exact wire shape written by `managed_runtime_envelope` and spliced into a
/// follow resume checkpoint. Every field is required, including nullable
/// `cost`; unknown fields and legacy status spellings are rejected by serde.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FollowResultEnvelope {
    success: bool,
    child_thread_id: String,
    status: RuntimeResultStatus,
    result: Value,
    outputs: Value,
    warnings: Vec<String>,
    cost: Value,
}

/// Exact cohort wrapper written by the daemon's follow join. Every field is
/// required and unknown fields are rejected before graph execution resumes.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FollowFanoutResumeEnvelope {
    fanout: bool,
    items: Vec<Value>,
    statuses: Vec<FanoutItemStatus>,
    failed: usize,
    expected: usize,
}

/// Parse and classify a canonical daemon-managed follow result.
///
/// Follow results are not arbitrary tool values: accepting a bare value or a
/// partial native envelope would let corrupt checkpoint data silently become a
/// successful child result. `continued` is an intermediate link in the child
/// continuation chain and is never a terminal follow result. For every other
/// closed status, `success` must agree exactly with the status outcome.
#[cfg(test)]
fn classify_follow_envelope(value: Value) -> Result<ClassifiedFollowEnvelope, String> {
    classify_follow_envelope_with_projection(value, NativeResultProjection::KindDefined)
}

/// Classify a single followed child using the canonical item ref captured in
/// the checkpointed action. A graph child exposes its authored return to the
/// resumed parent; every other kind retains its kind-defined native result.
pub(crate) fn classify_follow_envelope_for_item(
    value: Value,
    item_ref: &str,
) -> Result<ClassifiedFollowEnvelope, String> {
    let projection = native_result_projection(item_ref)?;
    classify_follow_envelope_with_projection(value, projection)
}

fn classify_follow_envelope_with_projection(
    value: Value,
    projection: NativeResultProjection,
) -> Result<ClassifiedFollowEnvelope, String> {
    let envelope: FollowResultEnvelope = serde_json::from_value(value)
        .map_err(|error| format!("malformed follow result envelope: {error}"))?;
    let FollowResultEnvelope {
        success,
        child_thread_id: envelope_child_thread_id,
        status,
        result,
        outputs,
        warnings: _warnings,
        cost,
    } = envelope;

    ryeos_runtime::validate_runtime_thread_id(&envelope_child_thread_id)
        .map_err(|error| format!("malformed follow result envelope: {error}"))?;

    if status == RuntimeResultStatus::Continued {
        return Err(
            "malformed follow result envelope: status `continued` is an intermediate child-chain handoff, not a terminal follow result"
                .to_string(),
        );
    }
    if success != status.is_success() {
        return Err(format!(
            "malformed follow result envelope: success={success} contradicts terminal status `{}`",
            status.as_str()
        ));
    }
    let cost = if cost.is_null() {
        None
    } else {
        let cost: RuntimeCost = serde_json::from_value(cost)
            .map_err(|error| format!("malformed follow result envelope cost: {error}"))?;
        cost.validate()
            .map_err(|error| format!("malformed follow result envelope cost: {error}"))?;
        Some(cost)
    };

    let outcome = if status.is_success() {
        let result = native_success_value(result, outputs, projection)
            .map_err(|error| format!("malformed follow result envelope: {error}"))?;
        ActionOutcome::Success(ActionSuccess {
            result,
            cost,
            child_thread_id: Some(envelope_child_thread_id),
        })
    } else {
        let structured_failure = parse_runtime_failure(&result);
        let mut runtime_failure_contract_error = runtime_failure_contract_error(&result);
        if let Some(failure) = structured_failure.as_ref() {
            if failure.diagnostic_locator.thread_id != envelope_child_thread_id {
                let mismatch = format!(
                    "RuntimeFailure diagnostic thread `{}` does not match follow child `{}`",
                    failure.diagnostic_locator.thread_id, envelope_child_thread_id
                );
                runtime_failure_contract_error = Some(match runtime_failure_contract_error {
                    Some(error) => format!("{error}; {mismatch}"),
                    None => mismatch,
                });
            }
        }
        let child_thread_id = Some(envelope_child_thread_id);
        let mut diagnostic = format!("child runtime failed (status: {})", status.as_str());
        if let Some(detail) = native_result_failure_detail(&result) {
            diagnostic.push_str(&format!(
                "; {}",
                excerpt(&detail, NATIVE_FAILURE_DIAGNOSTIC_CHARS)
            ));
        }
        if let Some(contract_error) = runtime_failure_contract_error.as_deref() {
            diagnostic.push_str(&format!(
                "; runtime failure contract invalid: {contract_error}"
            ));
        }
        if let Some(child_thread_id) = child_thread_id.as_deref() {
            diagnostic.push_str(&format!(
                "; child_thread_id={child_thread_id}; full child diagnostic: \
                 `ryeos thread tail {child_thread_id}`"
            ));
        }
        ActionOutcome::Failure(ActionFailure {
            diagnostic,
            cost,
            retryable: structured_failure
                .as_ref()
                .is_some_and(|failure| failure.retryable)
                && runtime_failure_contract_error.is_none(),
            child_thread_id,
            integrity: runtime_failure_contract_error.is_some(),
        })
    };

    Ok(ClassifiedFollowEnvelope { outcome, status })
}

/// Parse and classify the exact daemon-managed cohort result for a checkpointed
/// follow fanout. Structural drift is checkpoint corruption, not an authored
/// child failure, so callers must reject an error before applying `on_error`.
pub(crate) fn classify_follow_fanout_envelope(
    value: Value,
    item_refs: &[String],
) -> Result<ClassifiedFollowFanoutEnvelope, String> {
    let envelope: FollowFanoutResumeEnvelope = serde_json::from_value(value)
        .map_err(|error| format!("malformed follow fanout wrapper: {error}"))?;
    let FollowFanoutResumeEnvelope {
        fanout,
        items,
        statuses,
        failed,
        expected,
    } = envelope;

    if !fanout {
        return Err("malformed follow fanout wrapper: `fanout` must be true".to_string());
    }
    if item_refs.is_empty() {
        return Err(
            "malformed follow fanout wrapper: checkpointed cohort must contain at least one item"
                .to_string(),
        );
    }
    if expected != items.len() || items.len() != item_refs.len() || statuses.len() != items.len() {
        return Err(
            "malformed follow fanout wrapper: inconsistent expected/items/statuses/snapshot cardinality"
                .to_string(),
        );
    }
    let declared_failed = statuses
        .iter()
        .filter(|status| **status == FanoutItemStatus::Failed)
        .count();
    if failed != declared_failed {
        return Err(format!(
            "malformed follow fanout wrapper: failed={failed}, but typed statuses contain {declared_failed} failed items"
        ));
    }

    let mut aggregate_cost = RuntimeCost {
        input_tokens: 0,
        output_tokens: 0,
        total_usd: 0.0,
        basis: Some(ryeos_runtime::envelope::COST_BASIS_ROLLUP.to_string()),
    };
    let mut classified = Vec::with_capacity(items.len());
    for (index, ((item, declared_status), item_ref)) in items
        .into_iter()
        .zip(statuses.iter())
        .zip(item_refs.iter())
        .enumerate()
    {
        let item = classify_follow_envelope_for_item(item, item_ref)
            .map_err(|error| format!("follow fanout item {index}: {error}"))?;
        if item.fanout_status() != *declared_status {
            return Err(format!(
                "malformed follow fanout wrapper: item {index} status contradicts its terminal envelope outcome"
            ));
        }
        let cost = match &item.outcome {
            ActionOutcome::Success(success) => success.cost.as_ref(),
            ActionOutcome::Failure(failure) => failure.cost.as_ref(),
        };
        if let Some(cost) = cost {
            aggregate_cost.checked_accumulate(cost).map_err(|error| {
                format!("malformed follow fanout wrapper: aggregate cost is invalid: {error}")
            })?;
        }
        classified.push(item);
    }

    Ok(ClassifiedFollowFanoutEnvelope {
        items: classified,
        statuses,
    })
}

fn classify_native_runtime_envelope(
    value: Value,
    projection: NativeResultProjection,
) -> ActionOutcome {
    let envelope: NativeResultEnvelope = match serde_json::from_value(value) {
        Ok(envelope) => envelope,
        Err(error) => {
            return malformed_native_runtime_failure(
                format!("malformed native runtime envelope: {error}"),
                None,
            );
        }
    };
    let NativeResultEnvelope {
        success,
        status,
        result,
        outputs,
        warnings: _warnings,
        cost,
    } = envelope;
    let cost = match parse_native_cost(cost) {
        Ok(cost) => cost,
        Err(diagnostic) => {
            return ActionOutcome::Failure(ActionFailure {
                diagnostic,
                cost: None,
                retryable: false,
                child_thread_id: None,
                integrity: true,
            })
        }
    };
    if success != status.is_success() {
        return malformed_native_runtime_failure(
            format!(
                "native runtime envelope success={success} contradicts terminal status `{}`",
                status.as_str()
            ),
            cost,
        );
    }
    if status.is_success() {
        match native_success_value(result, outputs, projection) {
            Ok(result) => ActionOutcome::Success(ActionSuccess {
                result,
                cost,
                child_thread_id: None,
            }),
            Err(diagnostic) => malformed_native_runtime_failure(diagnostic, cost),
        }
    } else {
        // A failed native child (e.g. a directive that burned tokens then
        // errored) still reports `cost` — preserve it.
        let structured_failure = parse_runtime_failure(&result);
        let runtime_failure_contract_error = runtime_failure_contract_error(&result);
        let mut diagnostic = describe_native_failure(&result, status);
        if let Some(contract_error) = runtime_failure_contract_error.as_deref() {
            diagnostic.push_str(&format!(
                "; runtime failure contract invalid: {contract_error}"
            ));
        }
        ActionOutcome::Failure(ActionFailure {
            diagnostic,
            cost,
            retryable: structured_failure
                .as_ref()
                .is_some_and(|failure| failure.retryable)
                && runtime_failure_contract_error.is_none(),
            child_thread_id: structured_failure.map(|failure| failure.diagnostic_locator.thread_id),
            integrity: runtime_failure_contract_error.is_some(),
        })
    }
}

fn malformed_native_runtime_failure(
    diagnostic: String,
    cost: Option<RuntimeCost>,
) -> ActionOutcome {
    ActionOutcome::Failure(ActionFailure {
        diagnostic,
        cost,
        retryable: false,
        child_thread_id: None,
        integrity: true,
    })
}

fn classify_subprocess_envelope(value: Value) -> ActionOutcome {
    let envelope: SubprocessResultEnvelope = match serde_json::from_value(value) {
        Ok(envelope) => envelope,
        Err(error) => {
            return ActionOutcome::Failure(ActionFailure {
                diagnostic: format!("malformed subprocess result envelope: {error}"),
                cost: None,
                retryable: false,
                child_thread_id: None,
                integrity: true,
            });
        }
    };
    let SubprocessResultEnvelope {
        outcome_code: RequiredNullableString(outcome_code),
        result,
        error,
        artifacts: _artifacts,
    } = envelope;
    if error.is_null() {
        ActionOutcome::Success(ActionSuccess::bare(result))
    } else {
        ActionOutcome::Failure(ActionFailure {
            diagnostic: describe_subprocess_failure(outcome_code.as_deref(), &error),
            cost: None,
            retryable: false,
            child_thread_id: None,
            integrity: false,
        })
    }
}

/// Graph-visible success value for a native-runtime envelope.
///
/// A graph runtime's durable result is the complete typed [`GraphResult`], but
/// an authored graph action observes only that DTO's `result` field. Other
/// native kinds retain their kind-defined result projection: directives with
/// declared outputs expose `{result, outputs}`, while a native return with no
/// outputs exposes its bare result.
fn native_success_value(
    result: Value,
    outputs: Value,
    projection: NativeResultProjection,
) -> Result<Value, String> {
    match projection {
        NativeResultProjection::GraphReturn => project_graph_return(result, outputs),
        NativeResultProjection::KindDefined => {
            if has_meaningful_outputs(&outputs) {
                Ok(json!({ "result": result, "outputs": outputs }))
            } else {
                Ok(result)
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NativeResultProjection {
    KindDefined,
    GraphReturn,
}

fn native_result_projection(item_ref: &str) -> Result<NativeResultProjection, String> {
    let canonical = ryeos_engine::canonical_ref::CanonicalRef::parse(item_ref)
        .map_err(|error| format!("invalid dispatched item reference `{item_ref}`: {error}"))?;
    Ok(if canonical.kind == "graph" {
        NativeResultProjection::GraphReturn
    } else {
        NativeResultProjection::KindDefined
    })
}

fn project_graph_return(result: Value, outputs: Value) -> Result<Value, String> {
    if !outputs.is_null() {
        return Err("graph runtime envelope must carry null `outputs`".to_string());
    }

    let graph_result: GraphResult = serde_json::from_value(result)
        .map_err(|error| format!("graph runtime returned malformed GraphResult: {error}"))?;
    let definition_ref = ryeos_engine::canonical_ref::CanonicalRef::parse(
        &graph_result.definition_ref,
    )
    .map_err(|error| {
        format!(
            "graph runtime returned GraphResult with invalid definition_ref `{}`: {error}",
            graph_result.definition_ref
        )
    })?;
    if definition_ref.kind != "graph" {
        return Err(format!(
            "graph runtime returned GraphResult with non-graph definition_ref `{}`",
            graph_result.definition_ref
        ));
    }
    let successful_status = matches!(
        graph_result.status,
        GraphRunStatus::Valid | GraphRunStatus::Completed | GraphRunStatus::CompletedWithErrors
    );
    if !graph_result.success || !successful_status {
        return Err(format!(
            "graph runtime returned success envelope with contradictory GraphResult success={} status=`{}`",
            graph_result.success,
            graph_result.status.as_str()
        ));
    }

    Ok(graph_result.result.unwrap_or(Value::Null))
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

/// Parse the required nullable `cost` field of a native envelope into a typed
/// `RuntimeCost`. A null `cost` yields `None` — cost is never invented for a
/// child that did not report it. A present-but-malformed
/// `cost` (contract drift between the child runtime and the cost schema)
/// fails the leaf closed so a nominal success can never silently undercount.
fn parse_native_cost(raw: Value) -> Result<Option<RuntimeCost>, String> {
    if raw.is_null() {
        return Ok(None);
    }
    let cost: RuntimeCost = serde_json::from_value(raw)
        .map_err(|error| format!("native runtime envelope has malformed cost: {error}"))?;
    cost.validate()
        .map_err(|error| format!("native runtime envelope has invalid cost: {error}"))?;
    Ok(Some(cost))
}

fn describe_subprocess_failure(outcome_code: Option<&str>, error: &Value) -> String {
    let code = outcome_code.unwrap_or("unknown");
    let exit_code = error.get("exit_code").and_then(Value::as_i64);
    let stderr = error
        .get("stderr")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let stdout = error
        .get("stdout")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let mut msg = format!("tool failed (outcome_code: {code}");
    if let Some(ec) = exit_code {
        msg.push_str(&format!(", exit_code: {ec}"));
    }
    msg.push(')');
    // Keep the TAIL of the process streams: a traceback's actual error is
    // its last lines, and the daemon already tail-caps `error.stderr`, so a
    // head excerpt here would cut exactly the part an autopsy needs. (The
    // full daemon-capped tail stays durable on the tool child's own
    // `thread_failed` event.)
    if let Some(se) = stderr {
        msg.push_str(&format!("; stderr: {}", tail_excerpt(se, 800)));
    } else if let Some(so) = stdout {
        msg.push_str(&format!("; stdout: {}", tail_excerpt(so, 800)));
    }
    msg
}

fn describe_native_failure(result: &Value, status: RuntimeResultStatus) -> String {
    let mut msg = format!("child runtime failed (status: {})", status.as_str());
    if let Some(detail) = native_result_failure_detail(result) {
        msg.push_str(&format!(
            "; {}",
            excerpt(&detail, NATIVE_FAILURE_DIAGNOSTIC_CHARS)
        ));
    }
    msg
}

/// Extract a diagnostic from the required result payload of a canonical
/// native envelope. The canonical contract has no top-level `error` field;
/// failures carry their detail in `result`.
fn native_result_failure_detail(result: &Value) -> Option<String> {
    let non_empty = |s: &str| -> Option<String> {
        let trimmed = s.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    };
    if let Some(failure) = parse_runtime_failure(result) {
        return non_empty(&failure.summary);
    }
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

fn parse_runtime_failure(result: &Value) -> Option<ryeos_runtime::RuntimeFailure> {
    let failure: ryeos_runtime::RuntimeFailure = serde_json::from_value(result.clone()).ok()?;
    failure.validate().ok().map(|()| failure)
}

fn runtime_failure_contract_error(result: &Value) -> Option<String> {
    if result.get("kind").and_then(Value::as_str) != Some(ryeos_runtime::RUNTIME_FAILURE_KIND) {
        return None;
    }
    match serde_json::from_value::<ryeos_runtime::RuntimeFailure>(result.clone()) {
        Ok(failure) => failure.validate().err(),
        Err(error) => Some(format!("malformed versioned RuntimeFailure: {error}")),
    }
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

/// Tail-preserving excerpt for process streams, where the cause lands at
/// the END (tracebacks, final `logger.error` lines).
fn tail_excerpt(s: &str, max: usize) -> String {
    let total = s.chars().count();
    if total <= max {
        s.to_string()
    } else {
        let tail: String = s.chars().skip(total - max).collect();
        format!("[truncated …]{tail}")
    }
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

    fn make_mock_client_with_child(results: Vec<Value>, child_thread_id: &str) -> CallbackClient {
        let inner: Arc<dyn ryeos_runtime::callback::RuntimeCallbackAPI> =
            Arc::new(MockClient::with_child(results, child_thread_id));
        CallbackClient::from_inner(inner, "T-test", "/project", "tat-test")
    }

    struct MockClient {
        results: Mutex<Vec<Value>>,
        child_thread_id: Option<String>,
    }

    impl MockClient {
        fn new(results: Vec<Value>) -> Self {
            Self {
                results: Mutex::new(results),
                child_thread_id: None,
            }
        }

        fn with_child(results: Vec<Value>, child_thread_id: &str) -> Self {
            Self {
                results: Mutex::new(results),
                child_thread_id: Some(child_thread_id.to_string()),
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
                Ok(json!({
                    "thread": self
                        .child_thread_id
                        .as_ref()
                        .map(|id| json!({"thread_id": id}))
                        .unwrap_or_else(|| json!({})),
                    "result": results.remove(0),
                }))
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

    #[tokio::test]
    async fn forwards_call_block_to_callback() {
        let last = Arc::new(Mutex::new(None));
        let inner: Arc<dyn ryeos_runtime::callback::RuntimeCallbackAPI> =
            Arc::new(CapturingClient { last: last.clone() });
        let client = CallbackClient::from_inner(inner, "T-test", "/project", "tat-test");

        let action = json!({
            "item_id": "knowledge:arc/resources",
            "ref_bindings": {},
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
    async fn forwards_launch_window_to_callback() {
        let last = Arc::new(Mutex::new(None));
        let inner: Arc<dyn ryeos_runtime::callback::RuntimeCallbackAPI> =
            Arc::new(CapturingClient { last: last.clone() });
        let client = CallbackClient::from_inner(inner, "T-test", "/project", "tat-test");

        let action = json!({
            "item_id": "graph:t/leaf",
            "ref_bindings": {},
            "params": {},
            "thread": "detached",
            "launch_window": { "key": "gr-1:fan", "width": 12 },
        });
        dispatch_action(&client, &action, "T-test", "/project", None)
            .await
            .expect("dispatch ok");

        let forwarded = last.lock().unwrap().take().expect("action captured");
        let window = forwarded.launch_window.expect("launch_window forwarded");
        assert_eq!(window.key, "gr-1:fan");
        assert_eq!(window.width, 12);
    }

    #[tokio::test]
    async fn malformed_launch_window_fails_loudly() {
        let client = make_mock_client(vec![]);
        let action = json!({
            "item_id": "graph:t/leaf",
            "ref_bindings": {},
            "thread": "detached",
            "launch_window": { "width": "twelve" },
        });
        let err = dispatch_action(&client, &action, "T-test", "/project", None)
            .await
            .expect_err("malformed launch_window must fail");
        assert!(err.to_string().contains("launch_window"), "got: {err}");
    }

    #[tokio::test]
    async fn omits_call_block_when_absent() {
        let last = Arc::new(Mutex::new(None));
        let inner: Arc<dyn ryeos_runtime::callback::RuntimeCallbackAPI> =
            Arc::new(CapturingClient { last: last.clone() });
        let client = CallbackClient::from_inner(inner, "T-test", "/project", "tat-test");

        let action = json!({ "item_id": "tool:t/echo", "ref_bindings": {}, "params": {} });
        dispatch_action(&client, &action, "T-test", "/project", None)
            .await
            .expect("dispatch ok");

        let forwarded = last.lock().unwrap().take().expect("action captured");
        assert!(forwarded.call.is_none(), "no call block → None");
    }

    #[tokio::test]
    async fn null_call_block_treated_as_absent() {
        let last = Arc::new(Mutex::new(None));
        let inner: Arc<dyn ryeos_runtime::callback::RuntimeCallbackAPI> =
            Arc::new(CapturingClient { last: last.clone() });
        let client = CallbackClient::from_inner(inner, "T-test", "/project", "tat-test");

        // Parity with `/execute`'s `Option<MethodCall>`: explicit null == absent.
        let action =
            json!({ "item_id": "tool:t/echo", "ref_bindings": {}, "params": {}, "call": null });
        dispatch_action(&client, &action, "T-test", "/project", None)
            .await
            .expect("dispatch ok");

        let forwarded = last.lock().unwrap().take().expect("action captured");
        assert!(forwarded.call.is_none(), "call: null → None");
    }

    #[tokio::test]
    async fn graph_dispatch_does_not_inject_parent_context_for_budgeted_children() {
        let cases = ["directive:team/child", "graph:team/subgraph"];
        for item_id in cases {
            let last = Arc::new(Mutex::new(None));
            let inner: Arc<dyn ryeos_runtime::callback::RuntimeCallbackAPI> =
                Arc::new(CapturingClient { last: last.clone() });
            let client = CallbackClient::from_inner(inner, "T-test", "/project", "tat-test");

            let action = json!({
                "item_id": item_id,
                "ref_bindings": {},
                "params": {"user_input": "kept"},
            });
            dispatch_action(&client, &action, "T-test", "/project", None)
                .await
                .expect("dispatch ok");

            let forwarded = last.lock().unwrap().take().expect("action captured");
            assert_eq!(forwarded.params, json!({"user_input": "kept"}));
        }
    }

    #[tokio::test]
    async fn malformed_call_block_fails_loudly() {
        let client = make_mock_client(vec![]);
        let action = json!({
            "item_id": "knowledge:arc/resources",
            "ref_bindings": {},
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

    #[tokio::test]
    async fn inline_continuation_is_a_loud_error() {
        // Inline continuation-chasing is retired: a non-follow action whose leaf
        // returns a continuation_id must FAIL loudly (directing the author to
        // `follow: true`), never silently block chasing the chain.
        let client = make_mock_client(vec![json!({"continuation_id": "cont-1"})]);
        let action = json!({"item_id": "tool:test/deep", "ref_bindings": {}});
        let outcome = dispatch_action(&client, &action, "t-1", "/tmp/test", None)
            .await
            .unwrap();
        let failure = expect_action_failure(outcome);
        assert!(
            failure.diagnostic.contains("continuation_id") && failure.diagnostic.contains("follow"),
            "expected a loud inline-continuation error mentioning follow, got: {}",
            failure.diagnostic
        );
    }

    #[tokio::test]
    async fn failed_native_dispatch_preserves_spawned_child_thread_id() {
        let client = make_mock_client_with_child(
            vec![json!({
                "success": false,
                "status": RuntimeResultStatus::Failed,
                "result": {"error": "child failed"},
                "outputs": null,
                "warnings": [],
                "cost": null,
            })],
            "T-child-failed",
        );
        let action = json!({"item_id": "directive:test/child", "ref_bindings": {}});
        let outcome = dispatch_action(&client, &action, "T-parent", "/tmp/test", None)
            .await
            .expect("dispatch response");
        let failure = expect_action_failure(outcome);
        assert_eq!(failure.child_thread_id.as_deref(), Some("T-child-failed"));
        assert!(failure.diagnostic.contains("child failed"));
        assert!(failure
            .diagnostic
            .contains("ryeos thread tail T-child-failed"));
    }

    #[tokio::test]
    async fn direct_dispatch_rejects_unsafe_callback_child_thread_id() {
        let client = make_mock_client_with_child(
            vec![json!({
                "success": true,
                "status": RuntimeResultStatus::Completed,
                "result": {"ok": true},
                "outputs": null,
                "warnings": [],
                "cost": null,
            })],
            "T-child;tail",
        );
        let action = json!({"item_id": "directive:test/child", "ref_bindings": {}});
        let outcome = dispatch_action(&client, &action, "T-parent", "/tmp/test", None)
            .await
            .expect("dispatch response");
        let failure = expect_action_failure(outcome);
        assert!(failure.integrity);
        assert!(failure.child_thread_id.is_none());
        assert!(failure
            .diagnostic
            .contains("invalid dispatched child identity"));
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

    fn canonical_follow_envelope(
        success: bool,
        status: RuntimeResultStatus,
        result: Value,
    ) -> Value {
        json!({
            "success": success,
            "child_thread_id": "T-follow-child",
            "status": status,
            "result": result,
            "outputs": null,
            "warnings": [],
            "cost": null,
        })
    }

    fn canonical_native_envelope(
        success: bool,
        status: RuntimeResultStatus,
        result: Value,
    ) -> Value {
        json!({
            "success": success,
            "status": status,
            "result": result,
            "outputs": null,
            "warnings": [],
            "cost": null,
        })
    }

    fn completed_graph_result(result: Value) -> Value {
        json!({
            "success": true,
            "graph_id": "test/child",
            "definition_ref": "graph:test/child",
            "definition_hash": "sha256:test-child",
            "graph_run_id": "gr-child",
            "status": GraphRunStatus::Completed,
            "steps": 1,
            "state": {"private_child_state": true},
            "result": result,
            "node_costs": [],
            "hook_costs": [],
        })
    }

    #[test]
    fn graph_item_projects_authored_return_without_sniffing_non_graph_payloads() {
        let authored = json!({"child_ran": "sentinel"});
        let graph_result = completed_graph_result(authored.clone());
        let envelope =
            canonical_native_envelope(true, RuntimeResultStatus::Completed, graph_result.clone());

        assert_eq!(
            expect_success(classify_envelope_for_item(
                envelope.clone(),
                "graph:test/child",
            )),
            authored
        );
        assert_eq!(
            expect_success(classify_envelope_for_item(envelope, "directive:test/child",)),
            graph_result,
            "a non-graph payload is never projected based on its content"
        );
    }

    #[test]
    fn single_graph_follow_projects_authored_return() {
        let authored = json!({"child_ran": "sentinel"});
        let classified = classify_follow_envelope_for_item(
            canonical_follow_envelope(
                true,
                RuntimeResultStatus::Completed,
                completed_graph_result(authored.clone()),
            ),
            "graph:test/child",
        )
        .expect("canonical graph follow envelope");

        assert_eq!(expect_success(classified.outcome), authored);
    }

    #[test]
    fn failed_follow_preserves_child_thread_diagnostic_reference() {
        let envelope = canonical_follow_envelope(
            false,
            RuntimeResultStatus::Failed,
            json!({
                "kind": "runtime_failure",
                "version": 1,
                "code": "provider_accounting_invalid",
                "summary": "precise child failure",
                "diagnostic_locator": {
                    "thread_id": "T-follow-child",
                    "turn": 2,
                    "event_type": "thread_failed"
                },
                "retryable": false
            }),
        );

        let classified = classify_follow_envelope_for_item(envelope, "directive:test/child")
            .expect("canonical failed follow envelope");
        let failure = expect_action_failure(classified.outcome);
        assert_eq!(failure.child_thread_id.as_deref(), Some("T-follow-child"));
        assert!(failure.diagnostic.contains("precise child failure"));
        assert!(failure
            .diagnostic
            .contains("ryeos thread tail T-follow-child"));
    }

    #[test]
    fn canonical_follow_envelope_supplies_child_id_for_unstructured_failure_payload() {
        let envelope = canonical_follow_envelope(
            false,
            RuntimeResultStatus::Failed,
            json!("unstructured child failure"),
        );

        let classified = classify_follow_envelope_for_item(envelope, "graph:test/child")
            .expect("canonical failed follow envelope");
        let failure = expect_action_failure(classified.outcome);
        assert_eq!(failure.child_thread_id.as_deref(), Some("T-follow-child"));
        assert!(failure
            .diagnostic
            .contains("ryeos thread tail T-follow-child"));
    }

    #[test]
    fn unsupported_runtime_failure_version_is_an_integrity_failure() {
        let envelope = canonical_follow_envelope(
            false,
            RuntimeResultStatus::Failed,
            json!({
                "kind": "runtime_failure",
                "version": 99,
                "code": "provider_protocol_error",
                "summary": "future contract",
                "diagnostic_locator": {
                    "thread_id": "T-future",
                    "event_type": "thread_failed"
                },
                "retryable": true
            }),
        );
        let classified = classify_follow_envelope_for_item(envelope, "directive:test/child")
            .expect("structurally canonical outer follow envelope");
        let failure = expect_action_failure(classified.outcome);
        assert!(failure.integrity);
        assert!(!failure.retryable);
        assert!(failure
            .diagnostic
            .contains("unsupported runtime failure version 99"));
    }

    #[test]
    fn unrelated_versioned_failure_payload_is_not_a_runtime_failure_contract() {
        let envelope = canonical_follow_envelope(
            false,
            RuntimeResultStatus::Failed,
            json!({
                "version": 1,
                "code": "runtime_native_failure",
                "summary": "typed-looking but undiscriminated",
                "diagnostic_locator": {
                    "thread_id": "T-follow-child",
                    "event_type": "thread_failed"
                },
                "retryable": true,
                "error": "runtime-native failure"
            }),
        );
        let classified = classify_follow_envelope_for_item(envelope, "directive:test/child")
            .expect("canonical follow envelope");
        let failure = expect_action_failure(classified.outcome);
        assert!(!failure.integrity);
        assert!(failure.diagnostic.contains("runtime-native failure"));
    }

    #[test]
    fn graph_follow_fanout_projects_each_item_by_its_checkpointed_kind() {
        let graph_authored = json!({"child_ran": "graph"});
        let graph_result = completed_graph_result(graph_authored.clone());
        let graph_shaped_directive_result =
            completed_graph_result(json!({"child_ran": "directive"}));
        let wrapper = json!({
            "fanout": true,
            "expected": 2,
            "failed": 0,
            "statuses": [FanoutItemStatus::Completed, FanoutItemStatus::Completed],
            "items": [
                canonical_follow_envelope(
                    true,
                    RuntimeResultStatus::Completed,
                    graph_result,
                ),
                canonical_follow_envelope(
                    true,
                    RuntimeResultStatus::Completed,
                    graph_shaped_directive_result.clone(),
                ),
            ],
        });
        let item_refs = vec![
            "graph:test/child".to_string(),
            "directive:test/child".to_string(),
        ];
        let classified = classify_follow_fanout_envelope(wrapper, &item_refs)
            .expect("canonical mixed-kind fanout envelope");
        let mut items = classified.items.into_iter();

        assert_eq!(
            expect_success(items.next().expect("graph item").outcome),
            graph_authored
        );
        assert_eq!(
            expect_success(items.next().expect("directive item").outcome),
            graph_shaped_directive_result
        );
    }

    #[test]
    fn graph_projection_rejects_malformed_graph_result_as_integrity_failure() {
        let native_envelope = canonical_native_envelope(
            true,
            RuntimeResultStatus::Completed,
            json!({"child_ran": "missing graph result contract"}),
        );
        let failure = expect_action_failure(classify_envelope_for_item(
            native_envelope,
            "graph:test/child",
        ));
        assert!(failure.integrity);
        assert!(failure.diagnostic.contains("malformed GraphResult"));

        let follow_envelope = canonical_follow_envelope(
            true,
            RuntimeResultStatus::Completed,
            json!({"child_ran": "missing graph result contract"}),
        );
        let error =
            classify_follow_envelope_for_item(follow_envelope, "graph:test/child").unwrap_err();
        assert!(error.contains("malformed GraphResult"), "{error}");
    }

    #[test]
    fn graph_item_requires_native_envelope_and_graph_definition_ref() {
        for bare in [json!("bare"), json!({"child_ran": true})] {
            let failure =
                expect_action_failure(classify_envelope_for_item(bare, "graph:test/child"));
            assert!(failure.integrity);
            assert!(failure.diagnostic.contains("required runtime envelope"));
        }

        let mut wrong_kind = completed_graph_result(json!({"child_ran": true}));
        wrong_kind["definition_ref"] = json!("directive:test/child");
        let failure = expect_action_failure(classify_envelope_for_item(
            canonical_native_envelope(true, RuntimeResultStatus::Completed, wrong_kind),
            "graph:test/child",
        ));
        assert!(failure.integrity);
        assert!(failure.diagnostic.contains("non-graph definition_ref"));
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
            "warnings": [],
            "cost": null,
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
            "warnings": [],
            "cost": null,
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
            "warnings": [],
            "cost": null,
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
    fn classify_native_runtime_rejects_malformed_or_invalid_cost() {
        for cost in [
            json!({"input_tokens": 1, "total_usd": 0.01}),
            json!({"input_tokens": 1, "output_tokens": 2, "total_usd": -0.01}),
            json!({
                "input_tokens": 1,
                "output_tokens": 2,
                "total_usd": 0.01,
                "basis": "estimated",
            }),
        ] {
            let envelope = json!({
                "success": true,
                "status": "completed",
                "result": "directive_return",
                "outputs": {"x": 1},
                "cost": cost,
                "warnings": [],
            });
            let failure = expect_action_failure(classify_envelope(envelope));
            assert!(failure.diagnostic.contains("cost"));
            assert!(failure.cost.is_none());
        }
    }

    #[test]
    fn classify_native_markers_never_fall_through_as_bare_success() {
        for malformed in [
            json!({"success": false, "status": "failed", "result": null}),
            json!({"success": true}),
            json!({"status": "completed", "result": null}),
            json!({
                "success": true,
                "status": "completed",
                "result": null,
                "outputs": null,
                "warnings": [],
            }),
        ] {
            let failure = expect_action_failure(classify_envelope(malformed));
            assert!(failure
                .diagnostic
                .contains("malformed native runtime envelope"));
        }
    }

    #[test]
    fn classify_native_runtime_success_without_cost_is_none() {
        let envelope = json!({
            "success": true,
            "status": "completed",
            "result": {"state": {"x": 1}},
            "outputs": null,
            "warnings": [],
            "cost": null,
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
            "warnings": [],
            "cost": null,
        });
        assert_eq!(classify_success(envelope), json!({"foo": 1}));
    }

    #[test]
    fn classify_native_runtime_failure_preserves_cost() {
        // A failed LLM directive can burn tokens and still return
        // `success:false` with non-null `cost` — accounting must keep it.
        let envelope = json!({
            "success": false,
            "status": "failed",
            "result": {"error": "model refused"},
            "outputs": null,
            "cost": {"input_tokens": 80, "output_tokens": 0, "total_usd": 0.0008},
            "warnings": []
        });
        let failure = expect_action_failure(classify_envelope(envelope));
        assert!(failure.diagnostic.contains("model refused"));
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
    fn classify_subprocess_failure_keeps_the_stderr_tail() {
        // A long traceback's cause is its LAST lines — the diagnostic must
        // keep the tail, not the head, or every autopsy loses the exception.
        let noise = "frame line\n".repeat(200);
        let envelope = json!({
            "outcome_code": "exit:1",
            "result": null,
            "error": {"exit_code": 1, "stderr": format!("{noise}ValueError: the actual cause")},
            "artifacts": []
        });
        let diagnostic = classify_failure(envelope);
        assert!(
            diagnostic.contains("ValueError: the actual cause"),
            "tail must survive: {diagnostic}"
        );
        assert!(diagnostic.contains("[truncated"), "{diagnostic}");
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
    fn classify_subprocess_marker_rejects_partial_or_unknown_shapes() {
        for malformed in [
            json!({"outcome_code": "exit:0"}),
            json!({
                "outcome_code": null,
                "result": null,
                "error": null,
                "artifacts": [],
                "legacy": true,
            }),
        ] {
            let diagnostic = classify_failure(malformed);
            assert!(
                diagnostic.contains("malformed subprocess result envelope"),
                "{diagnostic}"
            );
        }
    }

    #[test]
    fn classify_native_runtime_failure_surfaces_status() {
        let envelope = json!({
            "success": false,
            "status": "failed",
            "result": null,
            "outputs": null,
            "warnings": [],
            "cost": null,
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
            "status": "failed",
            "result": {"error": "child graph failed: boom", "status": "error"},
            "outputs": null,
            "warnings": [],
            "cost": null,
        });
        assert!(classify_failure(envelope).contains("boom"));
    }

    #[test]
    fn classify_native_runtime_failure_rejects_unsupported_failure_version() {
        let envelope = json!({
            "success": false,
            "status": "failed",
            "result": {
                "kind": "runtime_failure",
                "version": 99,
                "code": "provider_protocol_error",
                "summary": "future contract",
                "diagnostic_locator": {
                    "thread_id": "T-native-child",
                    "event_type": "thread_failed"
                },
                "retryable": true
            },
            "outputs": null,
            "warnings": [],
            "cost": null,
        });
        let failure = expect_action_failure(classify_envelope(envelope));
        assert!(failure.integrity);
        assert!(!failure.retryable);
        assert!(failure
            .diagnostic
            .contains("unsupported runtime failure version 99"));
    }

    #[test]
    fn classify_native_runtime_failure_bounds_unstructured_inline_diagnostic_at_new_limit() {
        let marker = "precise-original-failure";
        let envelope = json!({
            "success": false,
            "status": "failed",
            "result": {"error": format!("{}{}", "context ".repeat(700), marker)},
            "outputs": null,
            "warnings": [],
            "cost": null,
        });

        let diagnostic = classify_failure(envelope);
        assert!(!diagnostic.contains(marker));
        assert!(diagnostic.contains("[truncated]"));
    }

    #[test]
    fn classify_native_runtime_rejects_unknown_or_contradictory_status() {
        let unknown = json!({
            "success": false,
            "status": "error",
            "result": {"error": "unknown status"},
            "outputs": null,
            "warnings": [],
            "cost": null,
        });
        assert!(classify_failure(unknown).contains("unknown variant"));

        let contradictory = json!({
            "success": true,
            "status": "failed",
            "result": null,
            "outputs": null,
            "warnings": [],
            "cost": null,
        });
        assert!(classify_failure(contradictory).contains("contradicts terminal status"));
    }

    #[test]
    fn malformed_native_cost_is_an_integrity_failure() {
        let failure = expect_action_failure(classify_envelope(json!({
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
        })));

        assert!(failure.integrity);
        assert!(failure.diagnostic.contains("invalid cost"));
    }

    #[test]
    fn classify_follow_envelope_preserves_typed_terminal_outcome() {
        let classified = classify_follow_envelope(canonical_follow_envelope(
            true,
            RuntimeResultStatus::Completed,
            json!({"answer": 42}),
        ))
        .expect("canonical follow envelope");
        assert_eq!(classified.fanout_status(), FanoutItemStatus::Completed);
        assert_eq!(expect_success(classified.outcome), json!({"answer": 42}));
    }

    #[test]
    fn classify_follow_envelope_rejects_bare_or_partial_values() {
        for malformed in [
            json!({"answer": 42}),
            json!({
                "success": true,
                "status": RuntimeResultStatus::Completed,
                "result": 42,
            }),
        ] {
            let error = classify_follow_envelope(malformed).unwrap_err();
            assert!(
                error.contains("malformed follow result envelope"),
                "{error}"
            );
        }
    }

    #[test]
    fn classify_follow_envelope_rejects_unknown_status_and_fields() {
        let unknown_status = json!({
            "success": false,
            "status": "error",
            "result": null,
            "outputs": null,
            "warnings": [],
            "cost": null,
        });
        assert!(classify_follow_envelope(unknown_status)
            .unwrap_err()
            .contains("unknown variant"));

        let mut unknown_field =
            canonical_follow_envelope(true, RuntimeResultStatus::Completed, json!(42));
        unknown_field["legacy_outcome"] = json!("success");
        assert!(classify_follow_envelope(unknown_field)
            .unwrap_err()
            .contains("unknown field"));
    }

    #[test]
    fn classify_follow_envelope_rejects_status_outcome_contradictions() {
        for malformed in [
            canonical_follow_envelope(true, RuntimeResultStatus::Failed, Value::Null),
            canonical_follow_envelope(false, RuntimeResultStatus::Completed, Value::Null),
        ] {
            let error = classify_follow_envelope(malformed).unwrap_err();
            assert!(error.contains("contradicts terminal status"), "{error}");
        }
    }

    #[test]
    fn classify_follow_envelope_requires_nonempty_child_thread_id() {
        let mut missing =
            canonical_follow_envelope(true, RuntimeResultStatus::Completed, json!(42));
        missing
            .as_object_mut()
            .expect("test envelope object")
            .remove("child_thread_id");
        let error = classify_follow_envelope(missing).unwrap_err();
        assert!(error.contains("missing field `child_thread_id`"), "{error}");

        let mut empty = canonical_follow_envelope(true, RuntimeResultStatus::Completed, json!(42));
        empty["child_thread_id"] = json!("   ");
        let error = classify_follow_envelope(empty).unwrap_err();
        assert!(error.contains("runtime thread_id is invalid"), "{error}");
    }

    #[test]
    fn classify_follow_envelope_rejects_intermediate_continued_status() {
        let error = classify_follow_envelope(canonical_follow_envelope(
            false,
            RuntimeResultStatus::Continued,
            Value::Null,
        ))
        .unwrap_err();
        assert!(
            error.contains("intermediate child-chain handoff"),
            "{error}"
        );
    }

    #[test]
    fn classify_follow_envelope_rejects_invalid_cost() {
        let mut envelope =
            canonical_follow_envelope(true, RuntimeResultStatus::Completed, json!({"answer": 42}));
        envelope["cost"] = json!({
            "input_tokens": 1,
            "output_tokens": 2,
            "total_usd": -0.01,
        });

        let error = classify_follow_envelope(envelope).unwrap_err();
        assert!(error.contains("must be non-negative"), "{error}");
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
