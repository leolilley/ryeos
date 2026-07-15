//! Scheduler-facing classification of completed thread result payloads.
//!
//! Thread status is a lifecycle signal: a runtime can exit cleanly and mark a
//! thread `completed` while the tool's structured result reports that the
//! application work failed. Scheduler fire history uses this classifier to keep
//! `status` lifecycle-oriented while making `outcome` reflect the tool result.

use serde_json::Value;

use ryeos_state::objects::ThreadStatus;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreadResultOutcome {
    Success,
    ResultFailed { reason: Option<String> },
}

pub fn classify_result_payload(value: &Value) -> ThreadResultOutcome {
    if has_success_false(value) {
        return ThreadResultOutcome::ResultFailed {
            reason: extract_failure_reason(value),
        };
    }

    if let Some(inner) = value.get("result") {
        if has_success_false(inner) {
            return ThreadResultOutcome::ResultFailed {
                reason: extract_failure_reason(inner),
            };
        }
    }

    ThreadResultOutcome::Success
}

pub fn completed_fire_outcome(result: Option<&ThreadResultOutcome>) -> &'static str {
    match result {
        Some(ThreadResultOutcome::ResultFailed { .. }) => "result_failed",
        _ => "success",
    }
}

/// Classify thread lifecycle status through the state crate's canonical typed
/// vocabulary. Unknown values fail closed as nonterminal.
pub fn thread_status_is_terminal(status: &str) -> bool {
    ThreadStatus::from_str_lossy(status).is_some_and(|status| status.is_terminal())
}

pub fn fire_status_for_thread_status(thread_status: &str) -> &'static str {
    match thread_status {
        "completed" => "completed",
        "cancelled" => "cancelled",
        _ => "failed",
    }
}

pub fn fire_outcome_for_terminal(
    thread_status: &str,
    result: Option<&ThreadResultOutcome>,
    fallback_outcome_code: Option<&str>,
) -> String {
    match thread_status {
        "completed" => completed_fire_outcome(result).to_string(),
        "cancelled" => "thread_cancelled".to_string(),
        "killed" => fallback_outcome_code.unwrap_or("thread_killed").to_string(),
        "timed_out" => fallback_outcome_code
            .unwrap_or("thread_timed_out")
            .to_string(),
        "continued" => fallback_outcome_code
            .unwrap_or("thread_continued")
            .to_string(),
        _ => fallback_outcome_code.unwrap_or("thread_failed").to_string(),
    }
}

fn has_success_false(value: &Value) -> bool {
    value.as_object().and_then(|obj| obj.get("success")) == Some(&Value::Bool(false))
}

fn extract_failure_reason(value: &Value) -> Option<String> {
    for key in ["error", "message"] {
        if let Some(reason) = value.get(key).and_then(|v| v.as_str()) {
            return Some(reason.to_string());
        }
    }

    let steps = value.get("steps")?.as_object()?;
    for step in steps.values() {
        let failed = step
            .get("success")
            .is_some_and(|v| v == &Value::Bool(false))
            || step
                .get("status")
                .and_then(|v| v.as_str())
                .is_some_and(|s| matches!(s, "error" | "failed"));
        if failed {
            for key in ["error", "message"] {
                if let Some(reason) = step.get(key).and_then(|v| v.as_str()) {
                    return Some(reason.to_string());
                }
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classifies_success_true_as_success() {
        assert_eq!(
            classify_result_payload(&json!({ "success": true })),
            ThreadResultOutcome::Success
        );
    }

    #[test]
    fn classifies_top_level_success_false_as_result_failed() {
        assert_eq!(
            classify_result_payload(&json!({ "success": false, "error": "boom" })),
            ThreadResultOutcome::ResultFailed {
                reason: Some("boom".to_string())
            }
        );
    }

    #[test]
    fn classifies_nested_success_false_as_result_failed() {
        assert_eq!(
            classify_result_payload(&json!({ "result": { "success": false, "error": "boom" } })),
            ThreadResultOutcome::ResultFailed {
                reason: Some("boom".to_string())
            }
        );
    }

    #[test]
    fn extracts_reason_from_failed_step() {
        assert_eq!(
            classify_result_payload(&json!({
                "result": {
                    "success": false,
                    "steps": {
                        "hydrate_profiles": {
                            "status": "error",
                            "error": "insert failed"
                        }
                    }
                }
            })),
            ThreadResultOutcome::ResultFailed {
                reason: Some("insert failed".to_string())
            }
        );
    }

    #[test]
    fn unrelated_payload_is_success() {
        assert_eq!(
            classify_result_payload(&json!("ok")),
            ThreadResultOutcome::Success
        );
        assert_eq!(
            classify_result_payload(&json!({ "status": "ok" })),
            ThreadResultOutcome::Success
        );
    }

    #[test]
    fn completed_fire_outcome_uses_result_failure() {
        assert_eq!(
            completed_fire_outcome(Some(&ThreadResultOutcome::ResultFailed { reason: None })),
            "result_failed"
        );
        assert_eq!(completed_fire_outcome(None), "success");
    }

    #[test]
    fn completed_terminal_outcome_prefers_result_failure_over_success_outcome_code() {
        assert_eq!(
            fire_outcome_for_terminal(
                "completed",
                Some(&ThreadResultOutcome::ResultFailed { reason: None }),
                Some("success"),
            ),
            "result_failed"
        );
    }

    #[test]
    fn canonical_terminal_thread_statuses_are_all_terminal() {
        for status in [
            "completed",
            "failed",
            "cancelled",
            "killed",
            "timed_out",
            "continued",
        ] {
            assert!(
                thread_status_is_terminal(status),
                "canonical terminal status was rejected: {status}"
            );
        }
    }

    #[test]
    fn active_and_unknown_thread_statuses_are_not_terminal() {
        for status in ["created", "running", "pending", "", "Completed"] {
            assert!(
                !thread_status_is_terminal(status),
                "nonterminal or unknown status was admitted: {status:?}"
            );
        }
    }

    #[test]
    fn every_canonical_terminal_status_has_a_stable_fire_classification() {
        let cases = [
            ("completed", "completed", "success"),
            ("failed", "failed", "thread_failed"),
            ("cancelled", "cancelled", "thread_cancelled"),
            ("killed", "failed", "thread_killed"),
            ("timed_out", "failed", "thread_timed_out"),
            ("continued", "failed", "thread_continued"),
        ];
        for (thread_status, fire_status, fire_outcome) in cases {
            assert!(thread_status_is_terminal(thread_status));
            assert_eq!(
                fire_status_for_thread_status(thread_status),
                fire_status,
                "wrong fire status for {thread_status}"
            );
            assert_eq!(
                fire_outcome_for_terminal(thread_status, None, None),
                fire_outcome,
                "wrong fire outcome for {thread_status}"
            );
        }
    }
}
