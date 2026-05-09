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

fn thread_is_terminal(status: &str) -> bool {
    matches!(status, "completed" | "failed")
}
