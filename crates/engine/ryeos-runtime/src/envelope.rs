//! Launch envelope types — re-exported from `ryeos_engine::launch_envelope_types`.
//!
//! Single source of truth lives in the engine crate; the daemon mints
//! the envelope and runtimes deserialise the same struct.
//!
//! `EnvelopeTarget` is gone — the runtime gets the root path / digest /
//! kind / id from `LaunchEnvelope.resolution.root` directly.

pub use ryeos_engine::launch_envelope_types::{
    EnvelopeCallback, EnvelopePolicy, EnvelopeRequest, EnvelopeRoots, HardLimits, ItemDescriptor,
    LaunchEnvelope, LaunchEnvelopeBuilder, RuntimeCost, RuntimeCostError, RuntimeResult,
    RuntimeResultStatus, COST_BASIS_ROLLUP,
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
#[serde(transparent)]
struct RequiredNullableString(Option<String>);

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

impl HookDispatchOutput {
    pub fn bare(value: serde_json::Value) -> Self {
        Self {
            value,
            cost: None,
            failure: None,
        }
    }
}

fn hook_child_failure(message: impl Into<String>) -> String {
    format!("hook_child_failed: {}", message.into())
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
                "cost": {"input_tokens": 1, "total_usd": 0.01},
            }),
        ] {
            assert!(!envelope_succeeded(&malformed));
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
