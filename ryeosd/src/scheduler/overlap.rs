//! Overlap policy evaluation.
//!
//! Three policies: allow, skip, cancel_previous.
//! Default: skip.

use super::types::ScheduleSpecRecord;

/// All three overlap policies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlapPolicy {
    Allow,
    Skip,
    CancelPrevious,
}

pub fn resolve_overlap_policy(spec: &ScheduleSpecRecord) -> OverlapPolicy {
    match spec.overlap_policy.as_str() {
        "allow" => OverlapPolicy::Allow,
        "cancel_previous" => OverlapPolicy::CancelPrevious,
        "skip" | "" => OverlapPolicy::Skip,
        _ => OverlapPolicy::Skip,
    }
}

/// Check overlap for a schedule. Returns `true` if the fire should proceed.
pub async fn check_overlap(spec: &ScheduleSpecRecord, state: &crate::state::AppState) -> bool {
    let policy = resolve_overlap_policy(spec);

    let last_fire = match state.scheduler_db.get_inflight_for_schedule(&spec.schedule_id) {
        Ok(Some(f)) => f,
        Ok(None) => return true, // no in-flight fire
        Err(e) => {
            tracing::error!(schedule_id = %spec.schedule_id, error = %e, "overlap check failed — proceeding");
            return true;
        }
    };

    let previous_thread_id = match &last_fire.thread_id {
        Some(id) => id.clone(),
        None => return true, // no thread to check
    };

    match policy {
        OverlapPolicy::Allow => true,

        OverlapPolicy::Skip => {
            match state.threads.get_thread(&previous_thread_id) {
                Ok(Some(thread)) if thread_is_terminal(&thread.status) => true,
                Ok(Some(_)) => {
                    tracing::info!(
                        schedule_id = %spec.schedule_id,
                        previous_thread = %previous_thread_id,
                        overlap = "skip",
                        "previous fire still running — skipping"
                    );
                    false
                }
                Ok(None) => true,
                Err(_) => true,
            }
        }

        OverlapPolicy::CancelPrevious => {
            match state.threads.get_thread(&previous_thread_id) {
                Ok(Some(thread)) if thread_is_terminal(&thread.status) => true,
                Ok(Some(_)) => {
                    tracing::info!(
                        schedule_id = %spec.schedule_id,
                        previous_thread = %previous_thread_id,
                        overlap = "cancel_previous",
                        "cancelling previous fire"
                    );
                    let _ = state.commands.submit(&crate::services::command_service::CommandSubmitParams {
                        thread_id: previous_thread_id.clone(),
                        command_type: "cancel".to_string(),
                        requested_by: None,
                        params: None,
                    });
                    true
                }
                Ok(None) => true,
                Err(_) => true,
            }
        }
    }
}

pub fn thread_is_terminal(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "cancelled")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_spec(overlap_policy: &str) -> ScheduleSpecRecord {
        ScheduleSpecRecord {
            schedule_id: "test".to_string(),
            item_ref: "directive:test".to_string(),
            params: "{}".to_string(),
            schedule_type: "cron".to_string(),
            expression: "* * * * * *".to_string(),
            timezone: "UTC".to_string(),
            misfire_policy: "skip".to_string(),
            overlap_policy: overlap_policy.to_string(),
            enabled: true,
            project_root: None,
            signer_fingerprint: "fp:test".to_string(),
            spec_hash: "abc".to_string(),
            registered_at: 0,
            last_modified: 0,
            last_modified_by: Some("fp:test".to_string()),
            requester_fingerprint: "fp:test".to_string(),
            approved_scopes: "[]".to_string(),
        }
    }

    #[test]
    fn resolve_allow() {
        assert_eq!(resolve_overlap_policy(&make_spec("allow")), OverlapPolicy::Allow);
    }

    #[test]
    fn resolve_skip() {
        assert_eq!(resolve_overlap_policy(&make_spec("skip")), OverlapPolicy::Skip);
    }

    #[test]
    fn resolve_cancel_previous() {
        assert_eq!(resolve_overlap_policy(&make_spec("cancel_previous")), OverlapPolicy::CancelPrevious);
    }

    #[test]
    fn resolve_empty_defaults_to_skip() {
        assert_eq!(resolve_overlap_policy(&make_spec("")), OverlapPolicy::Skip);
    }

    #[test]
    fn resolve_unknown_defaults_to_skip() {
        assert_eq!(resolve_overlap_policy(&make_spec("invalid")), OverlapPolicy::Skip);
    }

    #[test]
    fn thread_is_terminal_completed() {
        assert!(thread_is_terminal("completed"));
    }

    #[test]
    fn thread_is_terminal_failed() {
        assert!(thread_is_terminal("failed"));
    }

    #[test]
    fn thread_is_terminal_cancelled() {
        assert!(thread_is_terminal("cancelled"));
    }

    #[test]
    fn thread_is_not_terminal_running() {
        assert!(!thread_is_terminal("running"));
    }

    #[test]
    fn thread_is_not_terminal_pending() {
        assert!(!thread_is_terminal("pending"));
    }
}
