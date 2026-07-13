use serde_json::{json, Value};

use super::RuntimeResult;
use ryeos_app::thread_lifecycle::ThreadFinalizeParams;

pub(super) struct FallbackFinalization {
    pub params: ThreadFinalizeParams,
    pub managed_envelope: Value,
}

pub(super) fn is_runtime_terminal_status(status: &str) -> bool {
    matches!(
        status,
        "completed" | "failed" | "cancelled" | "killed" | "timed_out" | "continued"
    )
}

pub(super) fn normalize_runtime_terminal_status(status: &str) -> &'static str {
    match status {
        "completed" => "completed",
        "cancelled" => "cancelled",
        "killed" => "killed",
        "timed_out" => "timed_out",
        "continued" => "continued",
        // RuntimeResult historically used "errored" internally. The thread
        // lifecycle vocabulary uses "failed" for that terminal state.
        "failed" | "errored" => "failed",
        _ => "failed",
    }
}

/// Materialize the canonical DB finalization and managed follow envelope when
/// a runtime exits without finalizing itself through its callback.
pub(super) fn fallback_finalization(
    thread_id: &str,
    runtime_result: &RuntimeResult,
    terminal_status: &str,
) -> FallbackFinalization {
    let terminal_error = if terminal_status == "completed" {
        None
    } else {
        Some(json!({
            "reason": "runtime_exited_without_callback_finalization",
            "runtime_status": runtime_result.status.clone(),
            "result": runtime_result.result.clone(),
        }))
    };
    let final_cost = runtime_result.cost.as_ref().map(|cost| {
        ryeos_engine::contracts::FinalCost {
            turns: 0,
            input_tokens: cost.input_tokens as i64,
            output_tokens: cost.output_tokens as i64,
            spend: cost.total_usd,
            provider: None,
            basis: cost.basis.clone(),
            metadata: None,
        }
    });
    let raw_cost = runtime_result
        .cost
        .as_ref()
        .and_then(|cost| serde_json::to_value(cost).ok());
    let managed_envelope = ryeos_app::thread_lifecycle::managed_runtime_envelope(
        terminal_status,
        runtime_result.result.as_ref(),
        terminal_error.as_ref(),
        raw_cost.as_ref(),
        &runtime_result.outputs,
        &runtime_result.warnings,
    );
    let outcome_code = if terminal_status == "completed" {
        "success"
    } else {
        terminal_status
    };

    FallbackFinalization {
        params: ThreadFinalizeParams {
            thread_id: thread_id.to_string(),
            status: terminal_status.to_string(),
            outcome_code: Some(outcome_code.to_string()),
            result: runtime_result.result.clone(),
            error: terminal_error,
            metadata: None,
            artifacts: Vec::new(),
            final_cost,
            summary_json: None,
        },
        managed_envelope,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime_result(status: &str) -> RuntimeResult {
        RuntimeResult {
            success: status == "completed",
            status: status.to_string(),
            thread_id: "runtime-thread".to_string(),
            result: Some(json!("payload")),
            outputs: json!({"answer": 42}),
            cost: None,
            warnings: vec!["warning".to_string()],
        }
    }

    #[test]
    fn runtime_terminal_status_normalization_matches_thread_vocabulary() {
        assert_eq!(normalize_runtime_terminal_status("completed"), "completed");
        assert_eq!(normalize_runtime_terminal_status("timed_out"), "timed_out");
        assert_eq!(normalize_runtime_terminal_status("cancelled"), "cancelled");
        assert_eq!(normalize_runtime_terminal_status("errored"), "failed");
        assert_eq!(normalize_runtime_terminal_status("unexpected"), "failed");
    }

    #[test]
    fn runtime_terminal_status_detection_rejects_running_states() {
        assert!(is_runtime_terminal_status("completed"));
        assert!(is_runtime_terminal_status("failed"));
        assert!(is_runtime_terminal_status("timed_out"));
        assert!(!is_runtime_terminal_status("created"));
        assert!(!is_runtime_terminal_status("running"));
    }

    #[test]
    fn completed_fallback_has_success_outcome_and_no_error() {
        let fallback = fallback_finalization("thread-1", &runtime_result("completed"), "completed");

        assert_eq!(fallback.params.status, "completed");
        assert_eq!(fallback.params.outcome_code.as_deref(), Some("success"));
        assert_eq!(fallback.params.error, None);
        assert_eq!(fallback.managed_envelope["success"], true);
        assert_eq!(fallback.managed_envelope["outputs"]["answer"], 42);
    }

    #[test]
    fn failed_fallback_records_abnormal_exit_context() {
        let fallback = fallback_finalization("thread-2", &runtime_result("errored"), "failed");

        assert_eq!(fallback.params.status, "failed");
        assert_eq!(fallback.params.outcome_code.as_deref(), Some("failed"));
        assert_eq!(
            fallback.params.error.as_ref().unwrap()["reason"],
            "runtime_exited_without_callback_finalization"
        );
        assert_eq!(fallback.params.error.as_ref().unwrap()["runtime_status"], "errored");
        assert_eq!(fallback.managed_envelope["success"], false);
        assert_eq!(fallback.managed_envelope["result"], "payload");
    }
}
