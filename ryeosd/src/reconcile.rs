use anyhow::Result;
use serde_json::json;

use crate::process::pgid_alive;
use crate::services::thread_lifecycle::ThreadFinalizeParams;
use crate::state::AppState;

/// Reconcile daemon state after restart.
///
/// Three phases:
/// 1. Catch up projection from CAS (repair any projection drift)
/// 2. Rebuild thread locators if needed
/// 3. Find threads left in non-terminal status whose processes are dead,
///    and mark them failed.
#[tracing::instrument(name = "state:reconcile", skip(state))]
pub async fn reconcile(state: &AppState) -> Result<()> {
    // Phase 1: Catch up projection from CAS
    let cas_root = state.state_store.cas_root()?;
    let refs_root = state.state_store.refs_root()?;

    state.state_store.with_projection(|projection| {
        let catch_up = ryeos_state::rebuild::catch_up_projection(
            projection,
            &cas_root,
            &refs_root,
        )?;

        if catch_up.chains_updated > 0 {
            tracing::info!(
                chains_checked = catch_up.chains_checked,
                chains_updated = catch_up.chains_updated,
                threads_restored = catch_up.threads_restored,
                events_projected = catch_up.events_projected,
                "projection caught up from CAS"
            );
        }

        Ok::<_, anyhow::Error>(())
    })?;

    // Phase 2: Orphan thread cleanup
    let running_threads = state.state_store.list_threads_by_status(&["created", "running"])?;

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
