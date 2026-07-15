use anyhow::Context;

use super::{project_event, ProjectionDb, ProjectionMeta};

/// Project a thread snapshot into the projection database.
///
/// Upserts a thread record based on the snapshot. If the snapshot has
/// an `upstream_thread_id`, derives and inserts a thread edge from the
/// upstream to this thread.
pub fn project_thread_snapshot(
    db: &ProjectionDb,
    snapshot: &crate::ThreadSnapshot,
    chain_root_id: &str,
) -> anyhow::Result<()> {
    snapshot.validate()?;
    tracing::trace!(
        thread_id = %snapshot.thread_id,
        chain_root_id = %chain_root_id,
        status = %snapshot.status,
        upstream = ?snapshot.upstream_thread_id,
        "project thread snapshot"
    );
    let project_root = snapshot
        .project_root
        .as_ref()
        .map(|path| path.to_string_lossy().into_owned());

    db.connection()
        .execute(
            "INSERT OR REPLACE INTO threads (
            thread_id, chain_root_id, kind, status,
            item_ref, executor_ref, launch_mode,
            current_site_id, origin_site_id, upstream_thread_id, requested_by, project_root,
            base_project_snapshot_hash, result_project_snapshot_hash,
            created_at, updated_at, started_at, finished_at
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                &snapshot.thread_id,
                chain_root_id,
                &snapshot.kind_name,
                snapshot.status.to_string(),
                &snapshot.item_ref,
                &snapshot.executor_ref,
                &snapshot.launch_mode,
                &snapshot.current_site_id,
                &snapshot.origin_site_id,
                &snapshot.upstream_thread_id,
                &snapshot.requested_by,
                project_root,
                &snapshot.base_project_snapshot_hash,
                &snapshot.result_project_snapshot_hash,
                &snapshot.created_at,
                &snapshot.updated_at,
                &snapshot.started_at,
                &snapshot.finished_at,
            ],
        )
        .context("failed to project thread snapshot")?;

    // Project the snapshot's `result` / `error` fields into the
    // `thread_results` table so callers (e.g. graph runtime callback
    // dispatch through `dispatch_subprocess` → `build_execute_result`)
    // can read the leaf value back. Without this insert, the
    // `thread_results` table stays empty even on terminal status, and
    // every `get_thread_result` returns None — which surfaces as
    // `response.result == null` at the callback boundary.
    //
    // Idempotent under INSERT OR REPLACE: the snapshot is the source
    // of truth, so re-projection (rebuild, re-apply) overwrites with
    // the same row.
    if snapshot.result.is_some() || snapshot.error.is_some() || snapshot.outcome_code.is_some() {
        let result_blob = snapshot
            .result
            .as_ref()
            .map(|v| serde_json::to_vec(v).unwrap_or_default());
        // Store the error as JSON text so a structured error round-trips back
        // into the same shape on read (a string error stays a JSON string).
        let error_text = snapshot
            .error
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_default());
        db.connection()
            .execute(
                "INSERT OR REPLACE INTO thread_results (
                thread_id, chain_root_id, status, result, outcome_code, error, updated_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?)",
                rusqlite::params![
                    &snapshot.thread_id,
                    chain_root_id,
                    snapshot.status.to_string(),
                    result_blob,
                    &snapshot.outcome_code,
                    error_text,
                    &snapshot.updated_at,
                ],
            )
            .context("failed to project thread result")?;
    }

    // Derive edge from upstream_thread_id (CAS-truth derived projection)
    if let Some(ref upstream_id) = snapshot.upstream_thread_id {
        // Avoid duplicate edges — only insert if not already present
        let exists: bool = db.connection().query_row(
            "SELECT COUNT(*) > 0 FROM thread_edges WHERE parent_thread_id = ? AND child_thread_id = ? AND chain_root_id = ?",
            rusqlite::params![upstream_id, &snapshot.thread_id, chain_root_id],
            |row| row.get(0),
        ).unwrap_or(false);

        if !exists {
            db.connection().execute(
                "INSERT INTO thread_edges (
                    chain_root_id, parent_thread_id, child_thread_id, spawn_seq, spawn_reason, created_at
                ) VALUES (?, ?, ?, NULL, 'spawned', ?)",
                rusqlite::params![
                    chain_root_id,
                    upstream_id,
                    &snapshot.thread_id,
                    &snapshot.created_at,
                ],
            )
            .context("failed to project derived thread edge")?;
        }
    }

    Ok(())
}

/// Project a newly-created child thread and its initial durable events as one
/// read-model update.
///
/// This is used by relation-bearing child creation, where exposing the child
/// without the event that defines its relation would be misleading even though
/// the CAS/head transition is already atomic.
pub fn project_thread_snapshot_with_events(
    db: &ProjectionDb,
    snapshot: &crate::ThreadSnapshot,
    chain_root_id: &str,
    events: &[crate::ThreadEvent],
) -> anyhow::Result<()> {
    db.immediate_transaction("thread snapshot with events projection", || {
        project_thread_snapshot_with_events_in_transaction(db, snapshot, chain_root_id, events)
    })
}

pub(crate) fn project_thread_snapshot_with_events_in_transaction(
    db: &ProjectionDb,
    snapshot: &crate::ThreadSnapshot,
    chain_root_id: &str,
    events: &[crate::ThreadEvent],
) -> anyhow::Result<()> {
    project_thread_snapshot(db, snapshot, chain_root_id)?;
    for event in events {
        if event.durability.is_projection_indexed() {
            project_event(db, event).with_context(|| {
                format!("projection failed for event chain_seq={}", event.chain_seq)
            })?;
        }
    }
    Ok(())
}

/// Project all thread snapshots from a chain state into the projection database.
///
/// Also updates the projection metadata to track the indexed chain state hash.
pub fn project_chain_state(
    db: &ProjectionDb,
    chain_state: &crate::ChainState,
    chain_state_hash: &str,
) -> anyhow::Result<()> {
    chain_state.validate()?;
    tracing::trace!(
        chain_root_id = %chain_state.chain_root_id,
        chain_state_hash = %chain_state_hash,
        thread_count = chain_state.threads.len(),
        "project chain state"
    );

    // Update projection metadata
    let meta = ProjectionMeta {
        chain_root_id: chain_state.chain_root_id.clone(),
        indexed_chain_state_hash: chain_state_hash.to_string(),
        updated_at: chain_state.updated_at.clone(),
    };

    db.update_projection_meta(&meta)
        .context("failed to update projection metadata")?;

    Ok(())
}
