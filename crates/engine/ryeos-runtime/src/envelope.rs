//! Launch envelope types — re-exported from `ryeos_engine::launch_envelope_types`.
//!
//! Single source of truth lives in the engine crate; the daemon mints
//! the envelope and runtimes deserialise the same struct.
//!
//! `EnvelopeTarget` is gone — the runtime gets the root path / digest /
//! kind / id from `LaunchEnvelope.resolution.root` directly.

pub use ryeos_engine::launch_envelope_types::{
    AggregateExecutionLimits, COST_BASIS_ROLLUP, DeadlineLimits, EnvelopeAccountingScope,
    EnvelopeCallback, EnvelopePolicy, EnvelopeRequest, EnvelopeRoots, HardLimits, ItemDescriptor,
    LaunchEnvelope, LaunchEnvelopeBuilder, RuntimeCost, RuntimeCostError, RuntimeResult,
    RuntimeResultStatus, UsageStatus, UsdNanos,
};

/// Canonical managed-runtime result for the provider-neutral dedicated-session
/// controller. Both the subprocess and post-freeze crash recovery use this
/// constructor so a resumed disposition cannot invent a second terminal
/// payload shape.
pub fn dedicated_session_terminal_result(
    thread_id: String,
    session: serde_json::Value,
) -> RuntimeResult {
    let persisted_reason = session
        .get("terminal_reason")
        .and_then(serde_json::Value::as_str);
    // The callback-owned controller returns the just-settled reason while the
    // durable session projection uses `terminal_reason`. Accept the callback
    // spelling only for a state that proves termination has begun; arbitrary
    // live projections cannot declare themselves successful.
    let callback_reason = matches!(
        session.get("state").and_then(serde_json::Value::as_str),
        Some("terminal" | "freezing")
    )
    .then(|| session.get("reason").and_then(serde_json::Value::as_str))
    .flatten();
    let mut status = match persisted_reason.or(callback_reason) {
        Some("completed") => RuntimeResultStatus::Completed,
        Some("cancelled") => RuntimeResultStatus::Cancelled,
        Some("credential_revoked") => RuntimeResultStatus::Failed,
        _ => RuntimeResultStatus::Failed,
    };
    // A turn the workload reported as failed settled the session normally but
    // is not a success: the projection names it `worker_failure`, and the
    // child settles failed with its partial result still attached.
    if status == RuntimeResultStatus::Completed
        && session
            .get("outcome_kind")
            .and_then(serde_json::Value::as_str)
            == Some("worker_failure")
    {
        status = RuntimeResultStatus::Failed;
    }
    let cost = worker_result_cost(&session);
    RuntimeResult {
        success: status.is_success(),
        status,
        thread_id,
        result: Some(serde_json::json!({"session": session})),
        outputs: serde_json::json!({}),
        cost,
        warnings: Vec::new(),
    }
}

/// The cost a worker's result reports, in the shared runtime cost shape.
///
/// The session projection carries `worker_result_cost`: the three JSON
/// pointers the admitted profile declares for the result-marked notification,
/// each applied to the projection's `worker_result` payload. Each pointer must
/// select a non-negative integer; micro-dollars are exact multiples of the
/// fixed-point nano-dollar unit. A projection with no declared pointers, a
/// pointer that is absent or selects anything else, or a value that does not
/// satisfy the shared cost contract reports no cost.
pub fn worker_result_cost(session: &serde_json::Value) -> Option<RuntimeCost> {
    let declared = session.get("worker_result_cost")?.as_object()?;
    let payload = session.get("worker_result")?;
    let select =
        |field: &str| -> Option<u64> { payload.pointer(declared.get(field)?.as_str()?)?.as_u64() };
    let input_tokens = select("input_tokens")?;
    let output_tokens = select("output_tokens")?;
    let nanos = select("cost_micro_usd")?
        .checked_mul(1_000)
        .and_then(|nanos| i64::try_from(nanos).ok())?;
    let cost = RuntimeCost {
        input_tokens,
        output_tokens,
        total_usd: UsdNanos::from_nanos(nanos).ok()?,
        basis: None,
        usage_status: Default::default(),
    };
    cost.validate().ok()?;
    Some(cost)
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ManagedNativeEnvelope {
    success: bool,
    status: RuntimeResultStatus,
    result: serde_json::Value,
    outputs: serde_json::Value,
    warnings: Vec<String>,
    cost: serde_json::Value,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ManagedRuntimeTerminalWire {
    success: bool,
    child_thread_id: String,
    status: RuntimeResultStatus,
    result: serde_json::Value,
    outputs: serde_json::Value,
    warnings: Vec<String>,
    cost: serde_json::Value,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ManagedFollowEnvelope {
    projection: String,
    success: bool,
    child_thread_id: String,
    status: RuntimeResultStatus,
    result: serde_json::Value,
    cost: serde_json::Value,
    /// Present only for a followed directive child.
    #[serde(default)]
    reads: Option<FollowReads>,
}

/// What a followed directive child's recorded tool calls show it read, as the
/// daemon projected it. Reaches a graph as `dispatch.reads`. The refs are what
/// the model asked for (`requested`), not proof the content reached or was
/// understood; only tools that declare `reads` are covered.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FollowReads {
    pub refs: Vec<ReadEntry>,
    /// More reads than the bound held; the list is cut, never silently.
    pub truncated: bool,
    /// What the list does not cover, in words a graph author reads.
    pub covered: String,
}

/// One read a followed child's tool call made: the tool item, the space that
/// item resolved in, the ref the model named, and the basis of that ref.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadEntry {
    pub tool: String,
    /// `bundle`, `node` or `project`. Never a trust claim: a project-space tool
    /// can declare `reads` without reading anything.
    pub space: String,
    #[serde(rename = "ref")]
    pub reference: String,
    /// Always `requested` today: the model's own argument. `resolved` is for a
    /// tool whose result names a canonical item ref; none does yet.
    pub basis: String,
}

pub const FOLLOW_READS_COVERED: &str = "declared-read-tools-only";
pub const FOLLOW_READS_BASIS_REQUESTED: &str = "requested";
pub const MAX_FOLLOW_READ_REFS: usize = 256;
pub const MAX_FOLLOW_READ_REF_BYTES: usize = 512;

impl FollowReads {
    pub fn validate(&self) -> Result<(), String> {
        if self.refs.len() > MAX_FOLLOW_READ_REFS {
            return Err(format!(
                "follow reads hold {} refs, at most {MAX_FOLLOW_READ_REFS}",
                self.refs.len()
            ));
        }
        if self.covered != FOLLOW_READS_COVERED {
            return Err("follow reads must state what they cover".to_string());
        }
        for entry in &self.refs {
            if entry.reference.is_empty() || entry.reference.len() > MAX_FOLLOW_READ_REF_BYTES {
                return Err("a follow read ref is empty or over the bound".to_string());
            }
            if !matches!(entry.space.as_str(), "bundle" | "node" | "project") {
                return Err(format!("follow read space `{}` is unknown", entry.space));
            }
            if entry.basis != FOLLOW_READS_BASIS_REQUESTED {
                return Err(format!("follow read basis `{}` is unknown", entry.basis));
            }
        }
        Ok(())
    }
}

/// Add the reads a followed directive child made to a compact follow envelope.
pub fn with_follow_reads(
    mut envelope: serde_json::Value,
    reads: &FollowReads,
) -> Result<serde_json::Value, String> {
    reads.validate()?;
    let object = envelope
        .as_object_mut()
        .ok_or_else(|| "follow terminal envelope is not an object".to_string())?;
    object.insert(
        "reads".to_string(),
        serde_json::to_value(reads).map_err(|error| error.to_string())?,
    );
    Ok(envelope)
}

pub const FOLLOW_ACTION_RESULT_PROJECTION: &str = "action_result";

/// Complete callback-authoritative runtime terminal shape retained on the
/// child thread. This is distinct from the compact follow-resume projection.
#[derive(Debug)]
pub struct ManagedRuntimeTerminalEnvelope {
    pub success: bool,
    pub child_thread_id: String,
    pub status: RuntimeResultStatus,
    pub result: serde_json::Value,
    pub outputs: serde_json::Value,
    pub warnings: Vec<String>,
    pub cost: Option<RuntimeCost>,
}

/// Strictly decoded compact terminal envelope stored only for parent follow
/// resume. The child thread retains the complete callback-authoritative
/// runtime terminal separately.
#[derive(Debug)]
pub struct FollowTerminalEnvelope {
    pub success: bool,
    pub child_thread_id: String,
    pub status: RuntimeResultStatus,
    pub result: serde_json::Value,
    pub cost: Option<RuntimeCost>,
}

struct RequiredNullableString(Option<String>);

impl<'de> serde::Deserialize<'de> for RequiredNullableString {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = <serde_json::Value as serde::Deserialize>::deserialize(deserializer)?;
        serde_json::from_value(value)
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ManagedSubprocessEnvelope {
    outcome_code: RequiredNullableString,
    result: serde_json::Value,
    error: serde_json::Value,
    artifacts: Vec<serde_json::Value>,
}

/// A hook child result after the daemon's exact envelope has been removed.
/// `cost` remains typed so graph accounting cannot silently discard or
/// reinterpret malformed billing data.
#[derive(Debug)]
pub struct HookDispatchOutput {
    pub value: serde_json::Value,
    pub cost: Option<RuntimeCost>,
    pub failure: Option<HookDispatchFailure>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookDispatchFailureKind {
    Child,
    Integrity,
}

#[derive(Debug)]
pub struct HookDispatchFailure {
    pub kind: HookDispatchFailureKind,
    pub message: String,
}

impl HookDispatchFailure {
    pub fn contains(&self, needle: &str) -> bool {
        self.message.contains(needle)
    }
}

impl std::fmt::Display for HookDispatchFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

pub const HOOK_INTEGRITY_FAILURE_CODE: &str = "hook_child_integrity_failed";
pub const HOOK_DISPATCH_INTEGRITY_FAILURE_SCHEMA: &str = "ryeos.hook.dispatch-integrity-failure.v1";
const HOOK_DISPATCH_INTEGRITY_FAILURE_FIELD: &str = "_ryeos_hook_dispatch_integrity_failure";
const MAX_HOOK_DISPATCH_INTEGRITY_MESSAGE_BYTES: usize = 8 * 1024;
pub const MAX_HOOK_OBSERVATION_ACTION_BYTES: usize = 64 * 1024;
pub const MAX_HOOK_OBSERVATION_BYTES: usize = 192 * 1024;
pub const MAX_HOOK_OBSERVATION_JSON_DEPTH: usize = 32;
pub const MAX_HOOK_OBSERVATION_JSON_VALUES: usize = 8_192;
pub const MAX_HOOK_OBSERVATION_KIND_BYTES: usize = 128;

impl HookDispatchOutput {
    pub fn bare(value: serde_json::Value) -> Self {
        Self {
            value,
            cost: None,
            failure: None,
        }
    }
}

/// Canonical ledger-completable carrier for a dispatcher error whose outcome
/// is known after reservation. The executor stores and replays this exact
/// callback result; runtimes reject it as integrity-typed, while a genuinely
/// crash-ambiguous pending reservation remains non-replayable.
pub fn hook_dispatch_integrity_failure(message: &str) -> serde_json::Value {
    let mut end = message.len().min(MAX_HOOK_DISPATCH_INTEGRITY_MESSAGE_BYTES);
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    serde_json::json!({
        HOOK_DISPATCH_INTEGRITY_FAILURE_FIELD: {
            "schema_version": HOOK_DISPATCH_INTEGRITY_FAILURE_SCHEMA,
            "message": &message[..end],
        }
    })
}

fn hook_child_failure(message: impl Into<String>) -> String {
    format!("hook_child_failed: {}", message.into())
}

/// Bound the fully rendered parameters of an observation-producing action
/// before it is dispatched. This is intentionally smaller than the graph
/// checkpoint limit: observation publication cannot use runtime state as an
/// unbounded transport.
pub fn validate_hook_observation_action(action: &serde_json::Value) -> Result<(), String> {
    let params = action
        .as_object()
        .and_then(|object| object.get("params"))
        .unwrap_or(&serde_json::Value::Null);
    validate_bounded_json(
        params,
        MAX_HOOK_OBSERVATION_ACTION_BYTES,
        MAX_HOOK_OBSERVATION_JSON_DEPTH,
        MAX_HOOK_OBSERVATION_JSON_VALUES,
        "observation action params",
    )
}

/// Validate and reconstruct the one leaf value an observation hook may
/// publish. Runtime envelopes must already have been peeled by
/// `normalize_hook_dispatch_result`; unknown top-level fields are rejected.
pub fn normalize_hook_observation(value: serde_json::Value) -> Result<serde_json::Value, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "observation result must be an object".to_string())?;
    if object.len() != 2 || !object.contains_key("kind") || !object.contains_key("payload") {
        return Err("observation result must contain exactly `kind` and `payload`".to_string());
    }
    let kind = object
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "observation result `kind` must be a string".to_string())?;
    validate_observation_kind(kind)?;
    let normalized = serde_json::json!({
        "kind": kind,
        "payload": object.get("payload").expect("checked payload").clone(),
    });
    validate_bounded_json(
        &normalized,
        MAX_HOOK_OBSERVATION_BYTES,
        MAX_HOOK_OBSERVATION_JSON_DEPTH,
        MAX_HOOK_OBSERVATION_JSON_VALUES,
        "observation result",
    )?;
    Ok(normalized)
}

fn validate_observation_kind(kind: &str) -> Result<(), String> {
    if kind.is_empty() || kind.len() > MAX_HOOK_OBSERVATION_KIND_BYTES {
        return Err(format!(
            "observation result `kind` must be 1..={MAX_HOOK_OBSERVATION_KIND_BYTES} bytes"
        ));
    }
    let mut segments = kind.split('.');
    let mut count = 0usize;
    for segment in &mut segments {
        count += 1;
        let mut bytes = segment.bytes();
        if !bytes.next().is_some_and(|byte| byte.is_ascii_lowercase())
            || !bytes.all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
            })
        {
            return Err(
                "observation result `kind` must use lowercase namespaced segments".to_string(),
            );
        }
    }
    if count < 2 {
        return Err("observation result `kind` must be namespaced".to_string());
    }
    Ok(())
}

fn validate_bounded_json(
    value: &serde_json::Value,
    max_bytes: usize,
    max_depth: usize,
    max_values: usize,
    label: &str,
) -> Result<(), String> {
    let canonical = lillux::canonical_json(value)
        .map_err(|error| format!("{label} cannot be represented as canonical JSON: {error}"))?;
    if canonical.len() > max_bytes {
        return Err(format!(
            "{label} is {} bytes; maximum is {max_bytes}",
            canonical.len()
        ));
    }
    let mut stack = vec![(value, 1usize)];
    let mut values = 0usize;
    while let Some((current, depth)) = stack.pop() {
        values = values.saturating_add(1);
        if values > max_values {
            return Err(format!("{label} exceeds {max_values} JSON values"));
        }
        if depth > max_depth {
            return Err(format!("{label} exceeds {max_depth} JSON levels"));
        }
        match current {
            serde_json::Value::Array(items) => {
                stack.extend(items.iter().map(|item| (item, depth + 1)));
            }
            serde_json::Value::Object(items) => {
                stack.extend(items.values().map(|item| (item, depth + 1)));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Strictly classify and peel a hook child's daemon envelope.
///
/// Native markers (`success`/`status`) and the subprocess marker
/// (`outcome_code`) are authoritative. Once present, the value must satisfy the
/// complete, exact DTO; partial envelopes and unknown fields never fall
/// through as successful bare tool data.
pub fn normalize_hook_dispatch_result(
    value: serde_json::Value,
) -> Result<HookDispatchOutput, String> {
    let Some(object) = value.as_object() else {
        return Ok(HookDispatchOutput::bare(value));
    };

    if object.contains_key(HOOK_DISPATCH_INTEGRITY_FAILURE_FIELD) {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct FailureCarrier {
            schema_version: String,
            message: String,
        }
        if object.len() != 1 {
            return Err(hook_child_failure(
                "malformed daemon hook dispatch integrity failure carrier",
            ));
        }
        let carrier: FailureCarrier = serde_json::from_value(
            object
                .get(HOOK_DISPATCH_INTEGRITY_FAILURE_FIELD)
                .cloned()
                .expect("field presence checked"),
        )
        .map_err(|error| {
            hook_child_failure(format!(
                "malformed daemon hook dispatch integrity failure carrier: {error}"
            ))
        })?;
        if carrier.schema_version != HOOK_DISPATCH_INTEGRITY_FAILURE_SCHEMA {
            return Err(hook_child_failure(format!(
                "unsupported daemon hook dispatch integrity failure schema `{}`",
                carrier.schema_version
            )));
        }
        return Err(hook_child_failure(format!(
            "daemon hook dispatch failed after reservation: {}",
            carrier.message
        )));
    }

    if object.contains_key("success") || object.contains_key("status") {
        let envelope: ManagedNativeEnvelope = serde_json::from_value(value).map_err(|error| {
            hook_child_failure(format!("malformed native runtime envelope: {error}"))
        })?;
        let ManagedNativeEnvelope {
            success,
            status,
            result,
            outputs: _outputs,
            warnings: _warnings,
            cost,
        } = envelope;
        let cost = if cost.is_null() {
            None
        } else {
            let cost: RuntimeCost = serde_json::from_value(cost).map_err(|error| {
                hook_child_failure(format!("malformed native runtime cost: {error}"))
            })?;
            cost.validate().map_err(|error| {
                hook_child_failure(format!("invalid native runtime cost: {error}"))
            })?;
            Some(cost)
        };
        let failure = if success != status.is_success() {
            Some(HookDispatchFailure {
                kind: HookDispatchFailureKind::Integrity,
                message: hook_child_failure(format!(
                    "native envelope success={success} contradicts status `{}`",
                    status.as_str()
                )),
            })
        } else {
            (!status.is_success()).then(|| HookDispatchFailure {
                kind: HookDispatchFailureKind::Child,
                message: hook_child_failure(format!(
                    "child runtime failed with status `{}`: {result}",
                    status.as_str()
                )),
            })
        };
        return Ok(HookDispatchOutput {
            value: result,
            cost,
            failure,
        });
    }

    if object.contains_key("outcome_code") {
        let envelope: ManagedSubprocessEnvelope =
            serde_json::from_value(value).map_err(|error| {
                hook_child_failure(format!("malformed subprocess envelope: {error}"))
            })?;
        let ManagedSubprocessEnvelope {
            outcome_code: RequiredNullableString(outcome_code),
            result,
            error,
            artifacts: _artifacts,
        } = envelope;
        let failure = (!error.is_null()).then(|| HookDispatchFailure {
            kind: HookDispatchFailureKind::Child,
            message: hook_child_failure(format!(
                "subprocess failed (outcome_code={}): {error:#}",
                outcome_code.as_deref().unwrap_or("unknown")
            )),
        });
        return Ok(HookDispatchOutput {
            value: result,
            cost: None,
            failure,
        });
    }

    Ok(HookDispatchOutput::bare(value))
}

/// Canonical success classification for daemon/runtime result envelopes. Graph
/// dispatch and follow joins must agree: native envelopes require both the
/// typed terminal status and `success` to indicate completion, while subprocess
/// envelopes use the terminator's `outcome_code` discriminator.
pub fn envelope_succeeded(value: &serde_json::Value) -> bool {
    let Some(obj) = value.as_object() else {
        return true;
    };
    if obj.contains_key("success") || obj.contains_key("status") {
        let Ok(envelope) = serde_json::from_value::<ManagedNativeEnvelope>(value.clone()) else {
            return false;
        };
        let ManagedNativeEnvelope {
            success,
            status,
            result: _result,
            outputs: _outputs,
            warnings: _warnings,
            cost,
        } = envelope;
        let valid_cost = if cost.is_null() {
            true
        } else {
            serde_json::from_value::<RuntimeCost>(cost).is_ok_and(|cost| cost.validate().is_ok())
        };
        return valid_cost && success && status == RuntimeResultStatus::Completed;
    }
    if obj.contains_key("outcome_code") {
        let Ok(envelope) = serde_json::from_value::<ManagedSubprocessEnvelope>(value.clone())
        else {
            return false;
        };
        let ManagedSubprocessEnvelope {
            outcome_code: RequiredNullableString(_outcome_code),
            result: _result,
            error,
            artifacts: _artifacts,
        } = envelope;
        return error.is_null();
    }
    true
}

/// Decode and validate the exact daemon-managed follow envelope.
///
/// This contract intentionally differs from a direct native envelope by
/// requiring the authoritative child thread identity.
pub fn decode_follow_terminal_envelope(
    value: &serde_json::Value,
) -> Result<FollowTerminalEnvelope, String> {
    let envelope: ManagedFollowEnvelope = serde_json::from_value(value.clone())
        .map_err(|error| format!("malformed follow terminal envelope: {error}"))?;
    let ManagedFollowEnvelope {
        projection,
        success,
        child_thread_id,
        status,
        result,
        cost,
        reads,
    } = envelope;
    if let Some(reads) = &reads {
        reads
            .validate()
            .map_err(|error| format!("malformed follow terminal envelope reads: {error}"))?;
    }
    if projection != FOLLOW_ACTION_RESULT_PROJECTION {
        return Err(format!(
            "malformed follow terminal envelope: projection `{projection}` is not `{FOLLOW_ACTION_RESULT_PROJECTION}`"
        ));
    }
    crate::validate_runtime_thread_id(&child_thread_id)
        .map_err(|error| format!("malformed follow terminal envelope: {error}"))?;
    if status == RuntimeResultStatus::Continued {
        return Err(
            "malformed follow terminal envelope: continued is not a terminal status".to_string(),
        );
    }
    if success != status.is_success() {
        return Err(format!(
            "malformed follow terminal envelope: success={success} contradicts status `{}`",
            status.as_str()
        ));
    }
    let cost = if cost.is_null() {
        None
    } else {
        let cost: RuntimeCost = serde_json::from_value(cost)
            .map_err(|error| format!("malformed follow terminal envelope cost: {error}"))?;
        cost.validate()
            .map_err(|error| format!("invalid follow terminal envelope cost: {error}"))?;
        Some(cost)
    };
    Ok(FollowTerminalEnvelope {
        success,
        child_thread_id,
        status,
        result,
        cost,
    })
}

/// Decode the complete runtime terminal envelope before any parent-action
/// projection is applied.
pub fn decode_managed_runtime_terminal_envelope(
    value: &serde_json::Value,
) -> Result<ManagedRuntimeTerminalEnvelope, String> {
    let envelope: ManagedRuntimeTerminalWire = serde_json::from_value(value.clone())
        .map_err(|error| format!("malformed managed runtime terminal envelope: {error}"))?;
    let ManagedRuntimeTerminalWire {
        success,
        child_thread_id,
        status,
        result,
        outputs,
        warnings,
        cost,
    } = envelope;
    crate::validate_runtime_thread_id(&child_thread_id)
        .map_err(|error| format!("malformed managed runtime terminal envelope: {error}"))?;
    if success != status.is_success() {
        return Err(format!(
            "malformed managed runtime terminal envelope: success={success} contradicts status `{}`",
            status.as_str()
        ));
    }
    let cost = if cost.is_null() {
        None
    } else {
        let cost: RuntimeCost = serde_json::from_value(cost)
            .map_err(|error| format!("malformed managed runtime terminal cost: {error}"))?;
        cost.validate()
            .map_err(|error| format!("invalid managed runtime terminal cost: {error}"))?;
        Some(cost)
    };
    Ok(ManagedRuntimeTerminalEnvelope {
        success,
        child_thread_id,
        status,
        result,
        outputs,
        warnings,
        cost,
    })
}

/// Encode the compact parent-visible terminal contract for one followed child.
/// The child thread retains its complete terminal result; the resume payload
/// carries only the value that an ordinary authored action would observe.
pub fn encode_follow_terminal_envelope(
    child_thread_id: &str,
    status: RuntimeResultStatus,
    result: serde_json::Value,
    cost: Option<&RuntimeCost>,
) -> Result<serde_json::Value, String> {
    crate::validate_runtime_thread_id(child_thread_id)
        .map_err(|error| format!("invalid follow terminal child identity: {error}"))?;
    if status == RuntimeResultStatus::Continued {
        return Err("continued is not a terminal follow status".to_string());
    }
    if let Some(cost) = cost {
        cost.validate()
            .map_err(|error| format!("invalid follow terminal envelope cost: {error}"))?;
    }
    Ok(serde_json::json!({
        "projection": FOLLOW_ACTION_RESULT_PROJECTION,
        "success": status.is_success(),
        "child_thread_id": child_thread_id,
        "status": status,
        "result": result,
        "cost": cost,
    }))
}

/// Item kind of a bounded worker execution, whose followed value is its
/// outcome record rather than a kind-defined action result.
pub const WORKER_EXECUTION_ITEM_KIND: &str = "worker_execution";

/// The outcome classification a settled thread status implies when its child
/// reported no classification of its own.
pub const fn status_outcome_kind(status: RuntimeResultStatus) -> &'static str {
    match status {
        RuntimeResultStatus::Completed => "completed",
        RuntimeResultStatus::Cancelled | RuntimeResultStatus::Killed => "cancelled",
        RuntimeResultStatus::TimedOut => "deadline",
        RuntimeResultStatus::Failed | RuntimeResultStatus::Continued => "failed",
    }
}

/// The terminal session projection inside a settled worker result, whether it
/// is the result itself (`{session}`) or wrapped in a failure payload
/// (`{reason, result: {session}}`).
pub fn worker_terminal_session(result: &serde_json::Value) -> Option<&serde_json::Value> {
    result
        .get("session")
        .or_else(|| result.pointer("/result/session"))
        .filter(|session| session.is_object())
}

/// The value a parent receives for a followed worker child, for every
/// terminal status: the child thread, its terminal status, the closed
/// `outcome_kind`, the worker's own result payload (`null` when the worker
/// produced none) and its cost in the shared cost shape. A settled value that
/// carries no terminal session projection falls back to the classification its
/// thread status implies.
pub fn worker_follow_value(
    child_thread_id: &str,
    status: RuntimeResultStatus,
    result: &serde_json::Value,
    cost: Option<&RuntimeCost>,
) -> serde_json::Value {
    let session = worker_terminal_session(result);
    let outcome_kind = session
        .and_then(|session| session.get("outcome_kind"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_else(|| status_outcome_kind(status));
    let worker_result = session
        .and_then(|session| session.get("worker_result"))
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    serde_json::json!({
        "child_thread_id": child_thread_id,
        "status": status,
        "outcome_kind": outcome_kind,
        "result": worker_result,
        "cost": cost,
    })
}

/// Apply the default native-kind action projection shared by direct dispatch
/// and followed-child settlement. Kinds with a stronger contract, such as
/// graph, project through their kind-owned decoder instead.
pub fn project_kind_defined_action_result(
    result: serde_json::Value,
    outputs: serde_json::Value,
) -> serde_json::Value {
    let has_outputs = match &outputs {
        serde_json::Value::Null => false,
        serde_json::Value::Object(map) => !map.is_empty(),
        _ => true,
    };
    if has_outputs {
        serde_json::json!({ "result": result, "outputs": outputs })
    } else {
        result
    }
}

/// Validate the exact daemon-managed follow envelope and return its closed
/// terminal status.
pub fn follow_envelope_terminal_status(
    value: &serde_json::Value,
) -> Result<RuntimeResultStatus, String> {
    Ok(decode_follow_terminal_envelope(value)?.status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn session_with(kind: &str, worker_result: serde_json::Value) -> serde_json::Value {
        json!({"outcome_kind": kind, "worker_result": worker_result, "state": "terminal"})
    }

    #[test]
    fn worker_follow_value_projects_a_completed_result() {
        let worker_result = json!({"turn_id":"t1","result":{"text":"hello"}});
        let cost = RuntimeCost {
            input_tokens: 3,
            output_tokens: 4,
            total_usd: UsdNanos::from_nanos(5_000).unwrap(),
            basis: None,
            usage_status: Default::default(),
        };
        let value = worker_follow_value(
            "T-child",
            RuntimeResultStatus::Completed,
            &json!({"session": session_with("completed", worker_result.clone())}),
            Some(&cost),
        );
        assert_eq!(
            value,
            json!({
                "child_thread_id":"T-child","status":"completed","outcome_kind":"completed",
                "result":worker_result,
                "cost":{"input_tokens":3,"output_tokens":4,"total_usd":"0.000005"},
            })
        );
    }

    #[test]
    fn worker_follow_value_projects_every_failure_kind_with_a_null_result() {
        for kind in [
            "deadline",
            "budget",
            "approval_required",
            "outcome_unknown",
            "worker_failure",
            "cancelled",
            "failed",
        ] {
            let session = session_with(kind, serde_json::Value::Null);
            // The session may be the result itself or wrapped in a failure
            // payload; both project identically.
            for result in [
                json!({"session": session}),
                json!({"reason":"runtime_exited_without_callback_finalization",
                       "runtime_status":"cancelled","result":{"session": session}}),
            ] {
                let value =
                    worker_follow_value("T-child", RuntimeResultStatus::Cancelled, &result, None);
                assert_eq!(value["outcome_kind"], kind);
                assert_eq!(value["status"], "cancelled");
                assert_eq!(value["result"], serde_json::Value::Null);
                assert_eq!(value["cost"], serde_json::Value::Null);
                assert_eq!(value["child_thread_id"], "T-child");
            }
        }
    }

    #[test]
    fn worker_follow_value_without_a_session_uses_the_status_classification() {
        for (status, kind) in [
            (RuntimeResultStatus::Failed, "failed"),
            (RuntimeResultStatus::Cancelled, "cancelled"),
            (RuntimeResultStatus::Killed, "cancelled"),
            (RuntimeResultStatus::TimedOut, "deadline"),
        ] {
            let value = worker_follow_value("T-child", status, &json!({"error":"boom"}), None);
            assert_eq!(value["outcome_kind"], kind);
            assert_eq!(value["result"], serde_json::Value::Null);
        }
    }

    fn declared_cost() -> serde_json::Value {
        json!({
            "input_tokens": "/result/usage/input_tokens",
            "output_tokens": "/result/usage/output_tokens",
            "cost_micro_usd": "/result/usage/cost_micro_usd",
        })
    }

    #[test]
    fn worker_result_usage_maps_to_the_shared_runtime_cost() {
        let session = json!({
            "state":"terminal", "terminal_reason":"completed",
            "worker_result_cost": declared_cost(),
            "worker_result":{"turn_id":"t1","result":{"text":"hi","usage":{
                "input_tokens":120,"output_tokens":34,"cost_micro_usd":2500}}},
        });
        let result = dedicated_session_terminal_result("T-done".to_owned(), session);
        let cost = result.cost.expect("usage maps to cost");
        assert_eq!(cost.input_tokens, 120);
        assert_eq!(cost.output_tokens, 34);
        assert_eq!(cost.total_usd.to_canonical_string(), "0.0025");
        assert_eq!(cost.basis, None);
    }

    #[test]
    fn worker_result_cost_follows_the_declared_pointers() {
        let session = json!({
            "worker_result_cost": {
                "input_tokens": "/u/in", "output_tokens": "/u/out",
                "cost_micro_usd": "/u/micro"},
            "worker_result": {"u": {"in": 7, "out": 8, "micro": 3}},
        });
        let cost = worker_result_cost(&session).expect("declared pointers map");
        assert_eq!((cost.input_tokens, cost.output_tokens), (7, 8));
        assert_eq!(cost.total_usd.to_canonical_string(), "0.000003");
    }

    #[test]
    fn worker_result_without_declared_or_usable_usage_reports_no_cost() {
        let usage = json!({"usage":{"input_tokens":1,"output_tokens":1,"cost_micro_usd":1}});
        for session in [
            json!({"terminal_reason":"completed"}),
            json!({"terminal_reason":"completed","worker_result":null}),
            // No declared pointers: usage at the conventional path is ignored.
            json!({"worker_result":{"result":usage.clone()}}),
            json!({"worker_result_cost":null,"worker_result":{"result":usage.clone()}}),
            json!({"worker_result_cost":declared_cost(),"worker_result":{"result":{"text":"x"}}}),
            json!({"worker_result_cost":declared_cost(),
                "worker_result":{"result":{"usage":{"input_tokens":1,"output_tokens":1}}}}),
            json!({"worker_result_cost":declared_cost(),"worker_result":{"result":{"usage":{
                "input_tokens":-1,"output_tokens":1,"cost_micro_usd":1}}}}),
            // An unknown cost (null with an `unknown` status) is no cost, not
            // a zero one, even though the token counts are present.
            json!({"worker_result_cost":declared_cost(),"worker_result":{"result":{"usage":{
                "input_tokens":1,"output_tokens":1,"cost_micro_usd":null,
                "cost_status":"unknown"}}}}),
            json!({"worker_result_cost":{"input_tokens":"/result/usage/input_tokens",
                "output_tokens":null,"cost_micro_usd":"/result/usage/cost_micro_usd"},
                "worker_result":{"result":usage.clone()}}),
        ] {
            assert!(worker_result_cost(&session).is_none(), "{session}");
        }
    }

    #[test]
    fn a_worker_failure_outcome_settles_failed_with_its_result_attached() {
        let failed = dedicated_session_terminal_result(
            "T-crashed".to_owned(),
            json!({"state":"terminal", "terminal_reason":"completed",
                   "outcome_kind":"worker_failure",
                   "worker_result":{"status":"crashed"}}),
        );
        assert_eq!(failed.status, RuntimeResultStatus::Failed);
        assert!(!failed.success);
        assert_eq!(
            failed.result.unwrap()["session"]["worker_result"]["status"],
            "crashed"
        );
        let good = dedicated_session_terminal_result(
            "T-good".to_owned(),
            json!({"state":"terminal", "terminal_reason":"completed",
                   "outcome_kind":"completed"}),
        );
        assert_eq!(good.status, RuntimeResultStatus::Completed);
        assert!(good.success);
    }

    #[test]
    fn dedicated_session_callback_settlement_requires_a_terminalizing_state() {
        let completed = dedicated_session_terminal_result(
            "T-completed".to_owned(),
            json!({"state":"freezing", "reason":"completed"}),
        );
        assert_eq!(completed.status, RuntimeResultStatus::Completed);
        assert!(completed.success);

        let cancelled = dedicated_session_terminal_result(
            "T-cancelled".to_owned(),
            json!({"state":"terminal", "reason":"cancelled"}),
        );
        assert_eq!(cancelled.status, RuntimeResultStatus::Cancelled);
        assert!(!cancelled.success);

        let forged_live = dedicated_session_terminal_result(
            "T-live".to_owned(),
            json!({"state":"idle", "reason":"completed"}),
        );
        assert_eq!(forged_live.status, RuntimeResultStatus::Failed);
        assert!(!forged_live.success);
    }

    #[test]
    fn hook_observation_accepts_only_the_bounded_namespaced_leaf_envelope() {
        let expected = json!({
            "kind": "build.step_completed",
            "payload": {"step": 4, "ok": true}
        });
        assert_eq!(
            normalize_hook_observation(expected.clone()).unwrap(),
            expected
        );

        for rejected in [
            json!(null),
            json!({"kind": "build.step_completed"}),
            json!({"kind": "build.step_completed", "payload": {}, "extra": true}),
            json!({"kind": "unscoped", "payload": {}}),
            json!({"kind": "Build.completed", "payload": {}}),
        ] {
            assert!(normalize_hook_observation(rejected).is_err());
        }
    }

    #[test]
    fn hook_observation_rejects_byte_depth_and_value_limit_overflow() {
        assert!(
            normalize_hook_observation(json!({
                "kind": "build.payload",
                "payload": "x".repeat(MAX_HOOK_OBSERVATION_BYTES)
            }))
            .unwrap_err()
            .contains("maximum")
        );

        let mut deep = json!(true);
        for _ in 0..MAX_HOOK_OBSERVATION_JSON_DEPTH {
            deep = json!([deep]);
        }
        assert!(
            normalize_hook_observation(json!({
                "kind": "build.deep",
                "payload": deep
            }))
            .unwrap_err()
            .contains("JSON levels")
        );

        assert!(
            normalize_hook_observation(json!({
                "kind": "build.wide",
                "payload": vec![0; MAX_HOOK_OBSERVATION_JSON_VALUES]
            }))
            .unwrap_err()
            .contains("JSON values")
        );
    }

    #[test]
    fn hook_observation_action_params_are_capped_before_dispatch() {
        validate_hook_observation_action(&json!({
            "item_id": "tool:test/evidence",
            "params": {"path": ["a", "b"]}
        }))
        .unwrap();
        let error = validate_hook_observation_action(&json!({
            "item_id": "tool:test/evidence",
            "params": {"path": "x".repeat(MAX_HOOK_OBSERVATION_ACTION_BYTES)}
        }))
        .unwrap_err();
        assert!(error.contains("observation action params"));
        assert!(error.contains("maximum"));
    }

    #[test]
    fn known_post_reservation_failure_is_a_bounded_replayable_integrity_carrier() {
        let message = format!("dispatch failed: {}é", "x".repeat(16 * 1024));
        let carrier = hook_dispatch_integrity_failure(&message);
        let error = normalize_hook_dispatch_result(carrier.clone()).unwrap_err();
        assert!(error.contains("daemon hook dispatch failed after reservation"));
        assert!(serde_json::to_vec(&carrier).unwrap().len() < 9 * 1024);

        let mut malformed = carrier;
        malformed[HOOK_DISPATCH_INTEGRITY_FAILURE_FIELD]["extra"] = json!(true);
        assert!(
            normalize_hook_dispatch_result(malformed)
                .unwrap_err()
                .contains("malformed daemon hook dispatch integrity failure carrier")
        );
    }

    #[test]
    fn native_success_requires_typed_consistent_status() {
        assert!(envelope_succeeded(&json!({
            "success": true,
            "status": RuntimeResultStatus::Completed,
            "result": null,
            "outputs": null,
            "warnings": [],
            "cost": null,
        })));
        for rejected in [
            json!({
                "success": true,
                "status": RuntimeResultStatus::Failed,
                "result": null,
                "outputs": null,
                "warnings": [],
                "cost": null,
            }),
            json!({
                "success": true,
                "status": "error",
                "result": null,
                "outputs": null,
                "warnings": [],
                "cost": null,
            }),
            json!({
                "success": false,
                "status": RuntimeResultStatus::Completed,
                "result": null,
                "outputs": null,
                "warnings": [],
                "cost": null,
            }),
        ] {
            assert!(!envelope_succeeded(&rejected));
        }
    }

    #[test]
    fn native_markers_never_fall_through_as_bare_success() {
        for malformed in [
            json!({"success": false, "status": "failed", "result": null}),
            json!({
                "success": true,
                "status": "completed",
                "result": null,
                "outputs": null,
                "warnings": [],
            }),
            json!({
                "success": true,
                "status": "completed",
                "result": null,
                "outputs": null,
                "warnings": [],
                "cost": {"input_tokens": 1, "total_usd": "0.01"},
            }),
        ] {
            assert!(!envelope_succeeded(&malformed));
        }
    }

    #[test]
    fn follow_envelope_requires_child_identity_and_consistent_closed_status() {
        let valid = json!({
            "projection": FOLLOW_ACTION_RESULT_PROJECTION,
            "success": true,
            "child_thread_id": "T-follow-child",
            "status": RuntimeResultStatus::Completed,
            "result": null,
            "cost": null,
        });
        let decoded = decode_follow_terminal_envelope(&valid).unwrap();
        assert!(decoded.success);
        assert_eq!(decoded.child_thread_id, "T-follow-child");
        assert_eq!(decoded.status, RuntimeResultStatus::Completed);
        assert_eq!(decoded.result, serde_json::Value::Null);
        assert!(decoded.cost.is_none());
        assert_eq!(
            follow_envelope_terminal_status(&valid).unwrap(),
            RuntimeResultStatus::Completed
        );

        for malformed in [
            json!({
                "success": true,
                "child_thread_id": "T-follow-child",
                "status": RuntimeResultStatus::Completed,
                "result": null,
                "cost": null,
            }),
            json!({
                "projection": FOLLOW_ACTION_RESULT_PROJECTION,
                "success": true,
                "status": RuntimeResultStatus::Completed,
                "result": null,
                "cost": null,
            }),
            json!({
                "projection": FOLLOW_ACTION_RESULT_PROJECTION,
                "success": true,
                "child_thread_id": "T-follow-child",
                "status": RuntimeResultStatus::Failed,
                "result": null,
                "cost": null,
            }),
            json!({
                "projection": FOLLOW_ACTION_RESULT_PROJECTION,
                "success": true,
                "child_thread_id": "T-follow-child",
                "status": RuntimeResultStatus::Completed,
                "result": null,
                "cost": null,
                "unexpected": true,
            }),
        ] {
            assert!(follow_envelope_terminal_status(&malformed).is_err());
        }
    }

    #[test]
    fn subprocess_success_requires_the_exact_managed_shape() {
        for outcome_code in [json!(null), json!("exit:0")] {
            assert!(envelope_succeeded(&json!({
                "outcome_code": outcome_code,
                "result": {"ok": true},
                "error": null,
                "artifacts": [],
            })));
        }
        assert!(!envelope_succeeded(&json!({
            "outcome_code": "exit:1",
            "result": null,
            "error": {"exit_code": 1},
            "artifacts": [],
        })));
        for malformed in [
            json!({"outcome_code": "exit:0"}),
            json!({
                "outcome_code": "exit:0",
                "result": null,
                "error": null,
                "artifacts": [],
                "legacy": true,
            }),
        ] {
            assert!(!envelope_succeeded(&malformed));
        }
    }
}
