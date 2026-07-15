//! Maintenance mode GC executor.
//!
//! Runs GC with the write barrier quiesced, ensuring no concurrent CAS
//! writes during garbage collection. Used by the daemon for online GC.
//!
//! Flow:
//! 1. Acquire GC lock (flock)
//! 2. Quiesce write barrier (block new writes, drain active writers)
//! 3. Run GC pipeline (compact + sweep) via spawn_blocking
//! 4. Resume writes
//!
//! If quiesce times out, GC is aborted without running.

use std::time::Duration;

use anyhow::{Context, Result};

use ryeos_state::gc::{self, GcParams, GcResult};

use ryeos_app::state::AppState;
use ryeos_app::state_store::NodeIdentitySigner;

/// Default quiesce timeout: 30 seconds.
const DEFAULT_QUIESCE_TIMEOUT: Duration = Duration::from_secs(30);

/// Run maintenance mode GC with write barrier quiesced.
///
/// This is the daemon-side coordinator for online GC. It:
/// 1. Acquires the GC flock lock (prevents concurrent GC runs)
/// 2. Quiesces the write barrier (pauses CAS writes)
/// 3. Runs the GC pipeline on a blocking thread (doesn't stall tokio)
/// 4. Resumes writes
///
/// Returns the GC result on success. If quiesce times out, returns an error
/// without running GC. The write barrier is always resumed on error paths.
#[tracing::instrument(
    name = "maintenance:gc",
    skip(state, params),
    fields(
        dry_run = params.dry_run,
        compact = params.compact,
        deep = params.deep,
    )
)]
pub async fn run_maintenance_gc(state: &AppState, params: &GcParams) -> Result<GcResult> {
    let runtime_state_dir = state.config.runtime_state_dir();
    let cas_root = runtime_state_dir.join("objects");
    let refs_root = runtime_state_dir.join("refs");

    // Step 1: Acquire GC lock
    let _gc_lock = gc::GcLock::acquire(&runtime_state_dir, state.identity.fingerprint())
        .context("failed to acquire GC lock (another GC may be running)")?;

    tracing::info!(
        dry_run = params.dry_run,
        compact = params.compact,
        deep = params.deep,
        "maintenance GC: lock acquired"
    );

    // Seat settlement is a normal braid write, so reap before quiescing the
    // write barrier. Claiming marks (rather than deletes) the lease: failed
    // settlement remains retryable and racing heartbeats cannot resurrect it.
    let mut reaped_seats = 0usize;
    let mut retired_service_chains = 0usize;
    let mut deleted_service_chain_rows = 0usize;
    if params.deep || params.prune_runtime_history {
        let cutoff = gc::retention::iso8601_days_ago(7);
        let retirement = state
            .state_store
            .retire_service_chains_before(&cutoff, params.dry_run)?;
        retired_service_chains = retirement.retired_chains;
        deleted_service_chain_rows = retirement.deleted_rows;
    }
    if params.deep && !params.dry_run {
        const SEAT_LEASE_GRACE_MS: i64 = 10 * 60 * 1_000;
        let cutoff = lillux::time::timestamp_millis() as i64 - SEAT_LEASE_GRACE_MS;
        for thread_id in state.state_store.expired_seat_leases(cutoff)? {
            if !state
                .state_store
                .claim_expired_seat_lease(&thread_id, cutoff)?
            {
                continue;
            }
            let Some(detail) = state.state_store.get_thread(&thread_id)? else {
                state.state_store.remove_seat_lease(&thread_id)?;
                continue;
            };
            if detail.kind != "seat_session" || detail.status != "running" {
                state.state_store.remove_seat_lease(&thread_id)?;
                continue;
            }
            state
                .threads
                .finalize_thread(&ryeos_app::thread_lifecycle::ThreadFinalizeParams {
                    thread_id,
                    status: "completed".to_string(),
                    outcome_code: Some("seat_lease_expired".to_string()),
                    result: None,
                    error: None,
                    metadata: None,
                    artifacts: Vec::new(),
                    final_cost: None,
                    summary_json: None,
                })?;
            state.state_store.remove_seat_lease(&detail.thread_id)?;
            reaped_seats += 1;
        }
    }

    // Step 2: Quiesce write barrier
    state
        .write_barrier
        .quiesce(DEFAULT_QUIESCE_TIMEOUT)
        .await
        .context("write barrier quiesce timed out (GC aborted)")?;

    if !params.dry_run {
        let active_sync_jobs = state
            .state_store
            .with_state_db(|db| db.count_active_sync_jobs())
            .context("failed to inspect active sync jobs before GC");
        match active_sync_jobs {
            Ok(0) => {}
            Ok(active_sync_jobs) => {
                state.write_barrier.resume();
                anyhow::bail!(
                    "GC refused: {active_sync_jobs} active sync job(s) may pin staged CAS roots"
                );
            }
            Err(err) => {
                state.write_barrier.resume();
                return Err(err);
            }
        }
    }

    // Step 3: Run GC in a blocking task (CPU/disk I/O heavy)
    // Extract all data from AppState before spawning, since the closure
    // must be 'static + Send.
    let signer = NodeIdentitySigner::from_identity(&state.identity);
    let params_clone = params.clone();
    let runtime_state_dir_for_log = runtime_state_dir.clone();
    let state_store = state.state_store.clone();

    let gc_result = tokio::task::spawn_blocking(move || {
        run_gc_and_log(
            &cas_root,
            &refs_root,
            &signer,
            &params_clone,
            &runtime_state_dir_for_log,
            &state_store,
            reaped_seats,
            retired_service_chains,
            deleted_service_chain_rows,
        )
    })
    .await;

    // ALWAYS resume writes, even if GC failed or panicked
    state.write_barrier.resume();

    // NOW propagate errors
    let mut gc_result = gc_result.context("GC blocking task panicked")??;
    gc_result.reaped_seats = reaped_seats;
    gc_result.retired_service_chains = retired_service_chains;
    gc_result.deleted_service_chain_rows = deleted_service_chain_rows;

    Ok(gc_result)
}

/// Run GC and append event log. Called inside `spawn_blocking`.
fn run_gc_and_log(
    cas_root: &std::path::Path,
    refs_root: &std::path::Path,
    signer: &NodeIdentitySigner,
    params: &GcParams,
    runtime_state_dir: &std::path::Path,
    state_store: &ryeos_app::state_store::StateStore,
    reaped_seats: usize,
    retired_service_chains: usize,
    deleted_service_chain_rows: usize,
) -> Result<GcResult> {
    let mut runtime_cleanup = GcResult::default();
    gc::purge_runtime_state(runtime_state_dir, params, &mut runtime_cleanup)
        .context("runtime-state purge failed")?;

    let mut result =
        gc::run_gc(cas_root, refs_root, Some(signer), params).context("GC pipeline failed")?;
    result.deleted_runtime_files = runtime_cleanup.deleted_runtime_files;
    result.deleted_fire_records = runtime_cleanup.deleted_fire_records;
    result.freed_bytes += runtime_cleanup.freed_bytes;
    result.reaped_seats = reaped_seats;
    result.retired_service_chains = retired_service_chains;
    result.deleted_service_chain_rows = deleted_service_chain_rows;

    // Retention: retire old terminal sync-job rows. Runs under the deep profile
    // only (see GcParams docs), never on a dry run. The write barrier is
    // quiesced here, so no concurrent sync-job writes race the delete.
    if !params.dry_run && (params.deep || params.prune_runtime_history) {
        let cutoff = gc::retention::iso8601_days_ago(gc::retention::SYNC_JOB_RETENTION_DAYS);
        match state_store.with_state_db(|db| db.delete_terminal_sync_jobs_before(&cutoff)) {
            Ok((jobs, attempts)) => {
                result.deleted_sync_jobs = jobs;
                result.deleted_sync_job_attempts = attempts;
                if jobs > 0 {
                    tracing::info!(
                        deleted_sync_jobs = jobs,
                        deleted_sync_job_attempts = attempts,
                        cutoff = %cutoff,
                        "retention: retired terminal sync jobs"
                    );
                }
            }
            Err(err) => {
                tracing::warn!(error = %err, "sync-job retention sweep failed");
            }
        }
    }

    // Log the event (best-effort)
    if let Err(err) = gc::event_log::append_event(
        runtime_state_dir,
        &gc::event_log::GcEvent {
            timestamp: lillux::time::iso8601_now(),
            dry_run: params.dry_run,
            compact: params.compact,
            roots_walked: result.roots_walked,
            reachable_objects: result.reachable_objects,
            reachable_blobs: result.reachable_blobs,
            deleted_objects: result.deleted_objects,
            deleted_blobs: result.deleted_blobs,
            deleted_runtime_files: result.deleted_runtime_files,
            deleted_fire_records: result.deleted_fire_records,
            deleted_sync_jobs: result.deleted_sync_jobs,
            deleted_sync_job_attempts: result.deleted_sync_job_attempts,
            reaped_seats: result.reaped_seats,
            retired_service_chains: result.retired_service_chains,
            deleted_service_chain_rows: result.deleted_service_chain_rows,
            freed_bytes: result.freed_bytes,
            snapshots_compacted: result
                .compaction
                .as_ref()
                .map(|c| c.snapshots_removed)
                .unwrap_or(0),
            duration_ms: result.duration_ms,
        },
    ) {
        tracing::warn!(error = %err, "failed to append GC event log");
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiesce_timeout_is_reasonable() {
        // Verify the default timeout is reasonable for production use
        assert!(DEFAULT_QUIESCE_TIMEOUT >= Duration::from_secs(10));
        assert!(DEFAULT_QUIESCE_TIMEOUT <= Duration::from_secs(120));
    }
}
