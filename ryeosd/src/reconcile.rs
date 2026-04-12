use anyhow::Result;
use serde_json::json;

use crate::process::pgid_alive;
use crate::services::thread_lifecycle::ThreadFinalizeParams;
use crate::state::AppState;

/// Reconcile daemon state after restart.
///
/// Finds threads left in non-terminal status whose processes are dead,
/// and marks them failed. This handles the case where the daemon or
/// a worker process crashed mid-execution.
pub async fn reconcile(state: &AppState) -> Result<()> {
    let running_threads = state.db.list_threads_by_status(&["created", "running"])?;

    if running_threads.is_empty() {
        tracing::debug!("no orphaned threads");
        return Ok(());
    }

    tracing::info!(count = running_threads.len(), "checking non-terminal threads");

    let mut reconciled = 0;
    for thread in &running_threads {
        let pgid = thread.runtime.pgid;

        let process_dead = match pgid {
            Some(pgid) => !pgid_alive(pgid),
            None => thread.status == "running",
        };

        if process_dead {
            tracing::info!(
                thread_id = %thread.thread_id,
                kind = %thread.kind,
                status = %thread.status,
                pgid = ?pgid,
                "dead process, marking failed"
            );

            if let Err(err) = state.threads.finalize_thread(&ThreadFinalizeParams {
                thread_id: thread.thread_id.clone(),
                status: "failed".to_string(),
                outcome_code: Some("daemon_reconciled".to_string()),
                result: None,
                error: Some(json!({
                    "message": "thread in non-terminal state after daemon restart; process is dead",
                    "reconciled_from_status": &thread.status,
                    "pgid": pgid,
                })),
                metadata: None,
                artifacts: Vec::new(),
                final_cost: None,
                summary_json: None,
            }) {
                tracing::warn!(
                    thread_id = %thread.thread_id,
                    error = %err,
                    "failed to finalize thread"
                );
            } else {
                reconciled += 1;
            }
        }
    }

    tracing::info!(count = reconciled, "reconciled orphaned threads");
    Ok(())
}
