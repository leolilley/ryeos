use serde_json::{json, Value};

use super::RuntimeResult;
use ryeos_app::state_store::ThreadTerminalAuthority;
use ryeos_app::thread_lifecycle::ThreadFinalizeParams;
use ryeos_runtime::envelope::{RuntimeCost, RuntimeResultStatus};
use ryeos_state::objects::ThreadStatus;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthoritativeRuntimeEnvelope {
    success: bool,
    status: RuntimeResultStatus,
    result: Value,
    outputs: Value,
    warnings: Vec<String>,
    cost: Option<ryeos_runtime::envelope::RuntimeCost>,
}

pub(super) struct FallbackFinalization {
    pub params: ThreadFinalizeParams,
    pub managed_envelope: Value,
    /// Canonical response payload matching the terminal authority persisted by
    /// this fallback. This may differ from subprocess stdout when daemon intent
    /// (for example an operator kill) determines the authoritative status.
    pub runtime_result: RuntimeResult,
}

pub(super) fn is_thread_terminal_status(status: &str) -> bool {
    ThreadStatus::from_str_lossy(status).is_some_and(|status| status.is_terminal())
}

pub(super) fn runtime_terminal_status(status: RuntimeResultStatus) -> ThreadStatus {
    match status {
        RuntimeResultStatus::Completed => ThreadStatus::Completed,
        RuntimeResultStatus::Failed => ThreadStatus::Failed,
        RuntimeResultStatus::Cancelled => ThreadStatus::Cancelled,
        RuntimeResultStatus::Continued => ThreadStatus::Continued,
        RuntimeResultStatus::Killed => ThreadStatus::Killed,
        RuntimeResultStatus::TimedOut => ThreadStatus::TimedOut,
    }
}

fn runtime_result_status(status: ThreadStatus) -> RuntimeResultStatus {
    match status {
        ThreadStatus::Completed => RuntimeResultStatus::Completed,
        ThreadStatus::Failed => RuntimeResultStatus::Failed,
        ThreadStatus::Cancelled => RuntimeResultStatus::Cancelled,
        ThreadStatus::Continued => RuntimeResultStatus::Continued,
        ThreadStatus::Killed => RuntimeResultStatus::Killed,
        ThreadStatus::TimedOut => RuntimeResultStatus::TimedOut,
        ThreadStatus::Created | ThreadStatus::Running => {
            unreachable!("terminal authority requires a terminal status")
        }
    }
}

/// Verify subprocess stdout against the CAS terminal snapshot written by an
/// earlier runtime callback. The callback is the only terminal authority; a
/// contradictory process result is rejected loudly instead of being returned
/// as a second, incompatible outcome.
pub(super) fn reconcile_callback_finalization(
    authority: &ThreadTerminalAuthority,
    runtime_result: &RuntimeResult,
) -> anyhow::Result<()> {
    let stdout_status = runtime_terminal_status(runtime_result.status);
    if authority.status != stdout_status {
        anyhow::bail!(
            "runtime stdout status `{stdout_status}` contradicts callback-authoritative terminal status `{}`",
            authority.status
        );
    }

    let raw_envelope = authority.managed_envelope.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "callback-authoritative terminal snapshot is missing its managed runtime envelope"
        )
    })?;
    let envelope: AuthoritativeRuntimeEnvelope = serde_json::from_value(raw_envelope.clone())
        .map_err(|error| {
            anyhow::anyhow!("callback-authoritative managed runtime envelope is malformed: {error}")
        })?;
    if envelope.success != envelope.status.is_success()
        || runtime_terminal_status(envelope.status) != authority.status
    {
        anyhow::bail!(
            "callback-authoritative managed runtime envelope contradicts its terminal status"
        );
    }
    if envelope.success != runtime_result.success
        || envelope.status != runtime_result.status
        || envelope.result != runtime_result.result.clone().unwrap_or(Value::Null)
        || envelope.outputs != runtime_result.outputs
        || envelope.warnings != runtime_result.warnings
    {
        anyhow::bail!(
            "runtime stdout payload contradicts callback-authoritative managed runtime envelope"
        );
    }

    // The callback and stdout now carry the same kind-defined result layer. The
    // signed managed envelope is therefore authoritative for every externally
    // visible payload field, while the snapshot's typed cost remains an
    // independent settlement cross-check.
    match (
        authority.final_cost.as_ref(),
        envelope.cost.as_ref(),
        runtime_result.cost.as_ref(),
    ) {
        (None, None, None) => {}
        (Some(authority_cost), Some(envelope_cost), Some(runtime_cost))
            if authority_cost.turns == 0
                && authority_cost.input_tokens == runtime_cost.input_tokens
                && authority_cost.output_tokens == runtime_cost.output_tokens
                && authority_cost.spend == runtime_cost.total_usd
                && authority_cost.provider.is_none()
                && authority_cost.basis == runtime_cost.basis
                && authority_cost.metadata.is_none()
                && envelope_cost.input_tokens == runtime_cost.input_tokens
                && envelope_cost.output_tokens == runtime_cost.output_tokens
                && envelope_cost.total_usd == runtime_cost.total_usd
                && envelope_cost.basis == runtime_cost.basis => {}
        _ => anyhow::bail!("runtime stdout cost contradicts callback-authoritative terminal cost"),
    }

    Ok(())
}

/// Resolve a terminal snapshot that won the settlement race while the
/// subprocess was still running. Runtime callbacks carry a managed envelope
/// and must exactly match stdout. Operator cancellation, daemon reconciliation,
/// and other external settlements intentionally have no runtime envelope; in
/// that case the signed snapshot supersedes stdout and is returned canonically.
pub(super) fn reconcile_terminal_finalization(
    authority: &ThreadTerminalAuthority,
    runtime_result: &RuntimeResult,
) -> anyhow::Result<RuntimeResult> {
    if authority.managed_envelope.is_some() {
        reconcile_callback_finalization(authority, runtime_result)?;
        return Ok(runtime_result.clone());
    }

    let status = runtime_result_status(authority.status);
    let cost = authority.final_cost.as_ref().map(|cost| RuntimeCost {
        input_tokens: cost.input_tokens,
        output_tokens: cost.output_tokens,
        total_usd: cost.spend,
        basis: cost.basis.clone(),
    });
    if let Some(cost) = &cost {
        cost.validate().map_err(|error| {
            anyhow::anyhow!("external terminal authority has invalid cost: {error}")
        })?;
    }

    Ok(RuntimeResult {
        success: status.is_success(),
        status,
        thread_id: runtime_result.thread_id.clone(),
        result: authority.result.clone().or_else(|| authority.error.clone()),
        outputs: Value::Null,
        cost,
        warnings: Vec::new(),
    })
}

/// Materialize the canonical DB finalization and managed follow envelope when
/// a runtime exits without finalizing itself through its callback.
pub(super) fn fallback_finalization(
    thread_id: &str,
    runtime_result: &RuntimeResult,
    terminal_status: ThreadStatus,
) -> FallbackFinalization {
    let subprocess_status = runtime_result.status;
    let subprocess_result = runtime_result.result.clone();
    let mut runtime_result = runtime_result.clone();
    runtime_result.status = runtime_result_status(terminal_status);
    runtime_result.success = runtime_result.status.is_success();
    let terminal_error = if terminal_status == ThreadStatus::Completed {
        None
    } else {
        Some(json!({
            "reason": "runtime_exited_without_callback_finalization",
            "runtime_status": subprocess_status,
            "result": subprocess_result.clone(),
        }))
    };
    // The managed envelope uses result-first, then terminal error. Return that
    // same canonical payload to the caller even when stdout supplied no result.
    runtime_result.result = subprocess_result.clone().or_else(|| terminal_error.clone());
    let final_cost = runtime_result
        .cost
        .as_ref()
        .map(|cost| ryeos_engine::contracts::FinalCost {
            turns: 0,
            input_tokens: cost.input_tokens,
            output_tokens: cost.output_tokens,
            spend: cost.total_usd,
            provider: None,
            basis: cost.basis.clone(),
            metadata: None,
        });
    let raw_cost = runtime_result.cost.as_ref().map(|cost| {
        serde_json::to_value(cost)
            .expect("validated runtime cost must serialize for fallback settlement")
    });
    let managed_envelope = ryeos_app::thread_lifecycle::managed_runtime_envelope(
        thread_id,
        terminal_status.as_str(),
        runtime_result.result.as_ref(),
        terminal_error.as_ref(),
        raw_cost.as_ref(),
        &runtime_result.outputs,
        &runtime_result.warnings,
    );
    let outcome_code = if terminal_status == ThreadStatus::Completed {
        "success"
    } else {
        terminal_status.as_str()
    };

    FallbackFinalization {
        params: ThreadFinalizeParams {
            thread_id: thread_id.to_string(),
            status: terminal_status.as_str().to_string(),
            outcome_code: Some(outcome_code.to_string()),
            result: subprocess_result,
            error: terminal_error,
            metadata: None,
            artifacts: Vec::new(),
            final_cost,
            summary_json: None,
        },
        managed_envelope,
        runtime_result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime_result(status: RuntimeResultStatus) -> RuntimeResult {
        RuntimeResult {
            success: status.is_success(),
            status,
            thread_id: "runtime-thread".to_string(),
            result: Some(json!("payload")),
            outputs: json!({"answer": 42}),
            cost: None,
            warnings: vec!["warning".to_string()],
        }
    }

    fn authority(status: ThreadStatus) -> ThreadTerminalAuthority {
        let result = runtime_result(RuntimeResultStatus::from(match status {
            ThreadStatus::Completed => ryeos_engine::contracts::ThreadTerminalStatus::Completed,
            ThreadStatus::Failed => ryeos_engine::contracts::ThreadTerminalStatus::Failed,
            ThreadStatus::Cancelled => ryeos_engine::contracts::ThreadTerminalStatus::Cancelled,
            ThreadStatus::Continued => ryeos_engine::contracts::ThreadTerminalStatus::Continued,
            ThreadStatus::Killed => ryeos_engine::contracts::ThreadTerminalStatus::Killed,
            ThreadStatus::TimedOut => {
                return ThreadTerminalAuthority {
                    status,
                    result: Some(json!("payload")),
                    error: None,
                    final_cost: None,
                    managed_envelope: Some(json!({
                        "success": false,
                        "child_thread_id": "T-authority",
                        "status": "timed_out",
                        "result": "payload",
                        "outputs": {"answer": 42},
                        "cost": null,
                        "warnings": ["warning"]
                    })),
                }
            }
            ThreadStatus::Created | ThreadStatus::Running => {
                panic!("authority helper requires terminal status")
            }
        }));
        ThreadTerminalAuthority {
            status,
            result: Some(json!("payload")),
            error: None,
            final_cost: None,
            managed_envelope: Some(json!({
                "success": result.success,
                "child_thread_id": "T-authority",
                "status": result.status,
                "result": result.result,
                "outputs": result.outputs,
                "cost": result.cost,
                "warnings": result.warnings,
            })),
        }
    }

    #[test]
    fn runtime_terminal_status_maps_every_closed_variant() {
        let cases = [
            (RuntimeResultStatus::Completed, ThreadStatus::Completed),
            (RuntimeResultStatus::Failed, ThreadStatus::Failed),
            (RuntimeResultStatus::Cancelled, ThreadStatus::Cancelled),
            (RuntimeResultStatus::Continued, ThreadStatus::Continued),
            (RuntimeResultStatus::Killed, ThreadStatus::Killed),
            (RuntimeResultStatus::TimedOut, ThreadStatus::TimedOut),
        ];

        for (runtime_status, thread_status) in cases {
            assert_eq!(runtime_terminal_status(runtime_status), thread_status);
        }
    }

    #[test]
    fn runtime_terminal_status_detection_rejects_running_states() {
        assert!(is_thread_terminal_status("completed"));
        assert!(is_thread_terminal_status("failed"));
        assert!(is_thread_terminal_status("timed_out"));
        assert!(!is_thread_terminal_status("created"));
        assert!(!is_thread_terminal_status("running"));
    }

    #[test]
    fn external_terminal_authority_supersedes_subprocess_stdout() {
        let authority = ThreadTerminalAuthority {
            status: ThreadStatus::Cancelled,
            result: None,
            error: Some(json!({"reason": "operator_cancelled"})),
            final_cost: None,
            managed_envelope: None,
        };
        let stdout = runtime_result(RuntimeResultStatus::Completed);

        let reconciled = reconcile_terminal_finalization(&authority, &stdout).unwrap();

        assert_eq!(reconciled.status, RuntimeResultStatus::Cancelled);
        assert!(!reconciled.success);
        assert_eq!(
            reconciled.result,
            Some(json!({"reason": "operator_cancelled"}))
        );
        assert_eq!(reconciled.outputs, Value::Null);
        assert!(reconciled.warnings.is_empty());
    }

    #[test]
    fn completed_fallback_has_success_outcome_and_no_error() {
        let fallback = fallback_finalization(
            "thread-1",
            &runtime_result(RuntimeResultStatus::Completed),
            ThreadStatus::Completed,
        );

        assert_eq!(fallback.params.status, "completed");
        assert_eq!(fallback.params.outcome_code.as_deref(), Some("success"));
        assert_eq!(fallback.params.error, None);
        assert_eq!(fallback.managed_envelope["success"], true);
        assert_eq!(fallback.managed_envelope["outputs"]["answer"], 42);
    }

    #[test]
    fn failed_fallback_records_abnormal_exit_context() {
        let fallback = fallback_finalization(
            "thread-2",
            &runtime_result(RuntimeResultStatus::Failed),
            ThreadStatus::Failed,
        );

        assert_eq!(fallback.params.status, "failed");
        assert_eq!(fallback.params.outcome_code.as_deref(), Some("failed"));
        assert_eq!(
            fallback.params.error.as_ref().unwrap()["reason"],
            "runtime_exited_without_callback_finalization"
        );
        assert_eq!(
            fallback.params.error.as_ref().unwrap()["runtime_status"],
            "failed"
        );
        assert_eq!(fallback.managed_envelope["success"], false);
        assert_eq!(fallback.managed_envelope["result"], "payload");
    }

    #[test]
    fn kill_intent_fallback_canonicalizes_persistence_and_response() {
        let mut subprocess_result = runtime_result(RuntimeResultStatus::Failed);
        subprocess_result.result = None;
        let fallback =
            fallback_finalization("thread-killed", &subprocess_result, ThreadStatus::Killed);

        assert_eq!(fallback.params.status, "killed");
        assert_eq!(fallback.params.outcome_code.as_deref(), Some("killed"));
        assert_eq!(fallback.runtime_result.status, RuntimeResultStatus::Killed);
        assert!(!fallback.runtime_result.success);
        assert_eq!(fallback.managed_envelope["status"], "killed");
        assert_eq!(fallback.managed_envelope["success"], false);
        assert_eq!(
            fallback.runtime_result.result.as_ref().unwrap(),
            &fallback.managed_envelope["result"]
        );
        assert_eq!(
            fallback.params.error.as_ref().unwrap()["runtime_status"],
            "failed"
        );
        assert_eq!(fallback.params.result, None);
    }

    #[test]
    fn runtime_cost_rejects_values_the_settlement_store_cannot_represent() {
        for (input_tokens, output_tokens) in [(i64::MAX as u64 + 1, 0), (0, i64::MAX as u64 + 1)] {
            let cost = ryeos_runtime::envelope::RuntimeCost {
                input_tokens,
                output_tokens,
                total_usd: 1.0,
                basis: None,
            };
            assert!(cost.validate().is_err());
        }
    }

    #[test]
    fn callback_authority_accepts_an_identical_runtime_result() {
        reconcile_callback_finalization(
            &authority(ThreadStatus::Completed),
            &runtime_result(RuntimeResultStatus::Completed),
        )
        .unwrap();

        let mut with_cost = runtime_result(RuntimeResultStatus::Completed);
        with_cost.cost = Some(ryeos_runtime::envelope::RuntimeCost {
            input_tokens: 3,
            output_tokens: 5,
            total_usd: 0.02,
            basis: None,
        });
        let mut cost_authority = authority(ThreadStatus::Completed);
        cost_authority.final_cost = Some(ryeos_engine::contracts::FinalCost {
            turns: 0,
            input_tokens: 3,
            output_tokens: 5,
            spend: 0.02,
            provider: None,
            basis: None,
            metadata: None,
        });
        cost_authority.managed_envelope.as_mut().unwrap()["cost"] =
            serde_json::to_value(with_cost.cost.as_ref().unwrap()).unwrap();
        reconcile_callback_finalization(&cost_authority, &with_cost).unwrap();
    }

    #[test]
    fn callback_authority_rejects_status_and_cost_contradictions() {
        let completed = authority(ThreadStatus::Completed);
        let failed = runtime_result(RuntimeResultStatus::Failed);
        assert!(reconcile_callback_finalization(&completed, &failed).is_err());

        let mut cost_result = runtime_result(RuntimeResultStatus::Completed);
        cost_result.cost = Some(ryeos_runtime::envelope::RuntimeCost {
            input_tokens: 1,
            output_tokens: 2,
            total_usd: 0.01,
            basis: None,
        });
        assert!(reconcile_callback_finalization(&completed, &cost_result).is_err());
    }

    #[test]
    fn callback_authority_rejects_result_output_and_warning_contradictions() {
        let completed = authority(ThreadStatus::Completed);
        let mut contradictory = runtime_result(RuntimeResultStatus::Completed);
        contradictory.result = Some(json!("different"));
        assert!(reconcile_callback_finalization(&completed, &contradictory).is_err());

        let mut contradictory = runtime_result(RuntimeResultStatus::Completed);
        contradictory.outputs = json!({"answer": 99});
        assert!(reconcile_callback_finalization(&completed, &contradictory).is_err());

        let mut contradictory = runtime_result(RuntimeResultStatus::Completed);
        contradictory.warnings.push("different".to_string());
        assert!(reconcile_callback_finalization(&completed, &contradictory).is_err());
    }
}
