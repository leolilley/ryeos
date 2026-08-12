//! Launch envelope types — re-exported from `ryeos_engine::launch_envelope_types`.
//!
//! Single source of truth lives in the engine crate; the daemon mints
//! the envelope and runtimes deserialise the same struct.
//!
//! `EnvelopeTarget` is gone — the runtime gets the root path / digest /
//! kind / id from `LaunchEnvelope.resolution.root` directly.

pub use ryeos_engine::launch_envelope_types::{
    COST_BASIS_ROLLUP, EnvelopeAccountingScope, EnvelopeCallback, EnvelopePolicy, EnvelopeRequest,
    EnvelopeRoots, HardLimits, ItemDescriptor, LaunchEnvelope, LaunchEnvelopeBuilder, RuntimeCost,
    RuntimeCostError, RuntimeResult, RuntimeResultStatus, UsdNanos,
};

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
                "subprocess failed (outcome_code={}): {error}",
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
    } = envelope;
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
