//! Typed managed-runtime process outcomes that are deliberately not thread
//! terminals.
//!
//! `RuntimeResult` remains a closed terminal contract. A runtime that cannot
//! safely settle because a callback may have crossed the daemon boundary emits
//! this separate control envelope on stdout and exits successfully. The
//! executor validates it, keeps the thread nonterminal, consumes the existing
//! native-resume budget, and re-launches the same thread. No stderr text or
//! process exit code is interpreted as recovery authority.

use serde::{Deserialize, Serialize};

use crate::envelope::RuntimeResult;

pub const RUNTIME_PROCESS_CONTROL_SCHEMA: &str = "ryeos.runtime.process-control.v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeRecoveryReason {
    RetainedProgressOutcomeUnknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "process_outcome",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum RuntimeProcessControl {
    RecoveryRequired {
        schema: String,
        thread_id: String,
        reason: RuntimeRecoveryReason,
    },
}

#[derive(Debug)]
pub enum RuntimeProcessOutcome {
    Terminal(RuntimeResult),
    RecoveryRequired {
        thread_id: String,
        reason: RuntimeRecoveryReason,
    },
}

/// Typed runtime-internal error used only to reach the process-control stdout
/// boundary. It carries no diagnostic string from the daemon or a child.
#[derive(Debug, thiserror::Error)]
#[error("managed runtime requires same-thread recovery after an unknown retained-progress outcome")]
pub struct RuntimeRecoveryRequired {
    thread_id: String,
    reason: RuntimeRecoveryReason,
}

impl RuntimeRecoveryRequired {
    pub fn retained_progress_outcome_unknown(thread_id: impl Into<String>) -> Self {
        Self {
            thread_id: thread_id.into(),
            reason: RuntimeRecoveryReason::RetainedProgressOutcomeUnknown,
        }
    }

    pub fn control(&self) -> RuntimeProcessControl {
        RuntimeProcessControl::RecoveryRequired {
            schema: RUNTIME_PROCESS_CONTROL_SCHEMA.to_string(),
            thread_id: self.thread_id.clone(),
            reason: self.reason,
        }
    }
}

pub fn recovery_required_in_chain(error: &anyhow::Error) -> Option<&RuntimeRecoveryRequired> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<RuntimeRecoveryRequired>())
}

pub fn decode_runtime_process_stdout(stdout: &str) -> anyhow::Result<RuntimeProcessOutcome> {
    let value: serde_json::Value = serde_json::from_str(stdout)
        .map_err(|error| anyhow::anyhow!("failed to parse runtime stdout as JSON: {error}"))?;
    if value.get("process_outcome").is_some() {
        let control: RuntimeProcessControl = serde_json::from_value(value).map_err(|error| {
            anyhow::anyhow!("invalid runtime process-control envelope: {error}")
        })?;
        return match control {
            RuntimeProcessControl::RecoveryRequired {
                schema,
                thread_id,
                reason,
            } => {
                if schema != RUNTIME_PROCESS_CONTROL_SCHEMA {
                    anyhow::bail!("unsupported runtime process-control schema `{schema}`");
                }
                crate::validate_runtime_thread_id(&thread_id).map_err(|error| {
                    anyhow::anyhow!("invalid recovery thread identity: {error}")
                })?;
                Ok(RuntimeProcessOutcome::RecoveryRequired { thread_id, reason })
            }
        };
    }
    let terminal: RuntimeResult = serde_json::from_value(value)
        .map_err(|error| anyhow::anyhow!("invalid terminal runtime result: {error}"))?;
    Ok(RuntimeProcessOutcome::Terminal(terminal))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::{RuntimeResult, RuntimeResultStatus};
    use serde_json::json;

    #[test]
    fn recovery_control_is_distinct_from_the_terminal_domain() {
        let error = RuntimeRecoveryRequired::retained_progress_outcome_unknown("T-runtime");
        let encoded = serde_json::to_string(&error.control()).unwrap();
        match decode_runtime_process_stdout(&encoded).unwrap() {
            RuntimeProcessOutcome::RecoveryRequired { thread_id, reason } => {
                assert_eq!(thread_id, "T-runtime");
                assert_eq!(
                    reason,
                    RuntimeRecoveryReason::RetainedProgressOutcomeUnknown
                );
            }
            RuntimeProcessOutcome::Terminal(_) => panic!("control decoded as terminal"),
        }
    }

    #[test]
    fn terminal_result_remains_the_existing_exact_wire_shape() {
        let terminal = RuntimeResult {
            success: true,
            status: RuntimeResultStatus::Completed,
            thread_id: "T-runtime".to_string(),
            result: Some(json!({"ok":true})),
            outputs: json!({}),
            cost: None,
            warnings: vec![],
        };
        let encoded = serde_json::to_string(&terminal).unwrap();
        assert!(matches!(
            decode_runtime_process_stdout(&encoded).unwrap(),
            RuntimeProcessOutcome::Terminal(_)
        ));
    }

    #[test]
    fn unknown_or_malformed_control_never_falls_through_as_terminal() {
        for value in [
            json!({"process_outcome":"other"}),
            json!({
                "process_outcome":"recovery_required",
                "schema":"wrong",
                "thread_id":"T-runtime",
                "reason":"retained_progress_outcome_unknown"
            }),
        ] {
            assert!(decode_runtime_process_stdout(&value.to_string()).is_err());
        }
    }
}
