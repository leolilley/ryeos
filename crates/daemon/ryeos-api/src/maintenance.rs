//! Maintenance mode GC executor.
//!
//! Runs GC with the write barrier quiesced, ensuring no concurrent CAS
//! writes during garbage collection. Used by the daemon for online GC.
//!
//! Flow:
//! 1. Acquire the GC run lock
//! 2. Acquire the cross-process CAS mutation guard on a blocking worker
//! 3. Quiesce the in-process write barrier
//! 4. Run GC on that same guarded worker and resume writes
//!
//! If quiesce times out, GC is aborted without running.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};

use ryeos_state::gc::{self, GcParams, GcResult};

use ryeos_app::state::AppState;
use ryeos_app::state_store::NodeIdentitySigner;

/// Default quiesce timeout: 30 seconds.
const DEFAULT_QUIESCE_TIMEOUT: Duration = Duration::from_secs(30);

enum GcWorkerCommand {
    Run(WriteBarrierResume),
    Abort,
}

/// Cancellation-safe ownership of the fully quiesced state. If the async GC
/// coordinator is dropped at any await, ordinary writes are immediately
/// re-enabled while dropping the command sender releases the CAS worker.
struct WriteBarrierResume {
    barrier: Arc<ryeos_app::write_barrier::WriteBarrier>,
}

struct GcRunInput<'a> {
    state_authority: &'a ryeos_state::PinnedStateAuthority,
    cas_guard: &'a ryeos_state::CasMutationGuard,
    signer: &'a NodeIdentitySigner,
    params: &'a GcParams,
    state_store: &'a ryeos_app::state_store::StateStore,
    scheduler_db: &'a ryeos_scheduler::SchedulerDb,
    fire_retention: Option<gc::retention::FireRetentionPolicy>,
    reaped_seats: usize,
    terminal_chain_candidates: usize,
    retired_terminal_chains: usize,
    deleted_thread_projection_rows: usize,
    deleted_thread_runtime_rows: usize,
    deleted_thread_runtime_files: usize,
    pending_retirements_recovered: usize,
}

impl Drop for WriteBarrierResume {
    fn drop(&mut self) {
        self.barrier.resume();
    }
}

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
    params.validate()?;
    // Validate authored units before acquiring locks or performing any partial
    // cleanup. The storage/query boundary uses signed milliseconds.
    let seat_lease_grace_ms = params
        .seat_lease_grace_seconds
        .map(|seconds| {
            seconds
                .checked_mul(1_000)
                .and_then(|millis| i64::try_from(millis).ok())
                .context("seat_lease_grace_seconds is too large")
        })
        .transpose()?;
    let runtime_state_dir = state.config.runtime_state_dir();
    let fire_retention = gc::retention::FireRetentionPolicy::from_bounds(
        params.schedule_fire_max_age_days,
        params.schedule_fire_max_count,
    );

    // Step 1: Acquire GC lock
    let _gc_lock = if params.dry_run {
        gc::GcLock::acquire_existing(&runtime_state_dir)
    } else {
        gc::GcLock::acquire(&runtime_state_dir, state.identity.fingerprint())
    }
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
    let mut terminal_chain_candidates = 0usize;
    let mut retired_terminal_chains = 0usize;
    let mut deleted_thread_projection_rows = 0usize;
    let mut deleted_thread_runtime_rows = 0usize;
    let mut deleted_thread_runtime_files = 0usize;
    let mut pending_retirements_recovered = 0usize;
    // Every scheduler mutation and dispatch takes this gate. Keep its write
    // side across terminal-chain pin inspection and both fire-history stores,
    // so maintenance observes and publishes one closed scheduler state.
    let _scheduler_retention_guard =
        if params.deep || params.prune_runtime_history || fire_retention.is_some() {
            Some(state.scheduler_runtime_gate.clone().write_owned().await)
        } else {
            None
        };
    if _scheduler_retention_guard.is_some() && !state.scheduler_db.fire_projection_is_current()? {
        anyhow::bail!(
            "maintenance GC requires a current scheduler fire projection before pin inspection"
        );
    }
    if params.deep || params.prune_runtime_history {
        let retirement = state.state_store.retire_due_terminal_chains(
            &lillux::time::iso8601_now(),
            params.dry_run,
            |thread_ids| {
                let mut pins = 0u64;
                for thread_id in thread_ids {
                    if state.scheduler_db.find_fire_by_thread(thread_id)?.is_some() {
                        pins = pins
                            .checked_add(1)
                            .ok_or_else(|| anyhow::anyhow!("scheduler pin count overflow"))?;
                    }
                }
                Ok(pins)
            },
        )?;
        terminal_chain_candidates = retirement.candidate_chains;
        retired_terminal_chains = retirement.retired_chains;
        deleted_thread_projection_rows = retirement.deleted_projection_rows;
        deleted_thread_runtime_rows = retirement.deleted_runtime_rows;
        deleted_thread_runtime_files = retirement.deleted_runtime_files;
        pending_retirements_recovered = retirement.pending_retirements_recovered;
    }
    if !params.dry_run {
        if let Some(grace_ms) = seat_lease_grace_ms {
            let now_ms = lillux::time::timestamp_millis();
            let cutoff = now_ms.saturating_sub(grace_ms);
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
                // The claimed lease row is the structural authority that this
                // thread participates in lease expiry. Maintenance must not
                // rediscover that relationship from an authored kind string.
                if detail.status != "running" {
                    state.state_store.remove_seat_lease(&thread_id)?;
                    continue;
                }
                state.threads.finalize_thread(
                    &ryeos_app::thread_lifecycle::ThreadFinalizeParams {
                        thread_id,
                        status: "completed".to_string(),
                        outcome_code: Some("seat_lease_expired".to_string()),
                        result: None,
                        error: None,
                        metadata: None,
                        artifacts: Vec::new(),
                        final_cost: None,
                        summary_json: None,
                    },
                )?;
                state.state_store.remove_seat_lease(&detail.thread_id)?;
                reaped_seats += 1;
            }
        }
    }

    // The global mutation order is cross-process CAS guard, then in-process
    // write permit/barrier. A dedicated blocking worker can hold the !Send file
    // guard while the async coordinator waits for active permit holders to
    // drain, without ever carrying that guard across an await.
    let signer = NodeIdentitySigner::from_identity(&state.identity);
    let params_clone = params.clone();
    let state_store = state.state_store.clone();
    let scheduler_db = state.scheduler_db.clone();
    let state_authority = state
        .state_store
        .with_state_db(|db| db.pinned_authority())
        .context("capture pinned state authority for GC")?;
    let (guard_ready_tx, guard_ready_rx) =
        tokio::sync::oneshot::channel::<std::result::Result<(), String>>();
    let (command_tx, command_rx) = std::sync::mpsc::sync_channel(1);
    let gc_worker = tokio::task::spawn_blocking(move || -> Result<Option<GcResult>> {
        let cas_guard = match state_authority.acquire_exclusive_guard(!params_clone.dry_run) {
            Ok(guard) => guard,
            Err(error) => {
                let _ = guard_ready_tx.send(Err(error.to_string()));
                return Err(error).context("acquire exclusive CAS mutation guard for GC");
            }
        };
        if let Err(error) = state_authority
            .ensure_guard(&cas_guard)
            .context("GC guard does not protect the live pinned state authority")
        {
            let _ = guard_ready_tx.send(Err(error.to_string()));
            return Err(error);
        }
        let _ = guard_ready_tx.send(Ok(()));
        match command_rx.recv() {
            Ok(GcWorkerCommand::Run(_write_barrier_resume)) => run_gc_and_log(GcRunInput {
                state_authority: &state_authority,
                cas_guard: &cas_guard,
                signer: &signer,
                params: &params_clone,
                state_store: &state_store,
                scheduler_db: &scheduler_db,
                fire_retention,
                reaped_seats,
                terminal_chain_candidates,
                retired_terminal_chains,
                deleted_thread_projection_rows,
                deleted_thread_runtime_rows,
                deleted_thread_runtime_files,
                pending_retirements_recovered,
            })
            .map(Some),
            Ok(GcWorkerCommand::Abort) | Err(_) => Ok(None),
        }
    });
    guard_ready_rx
        .await
        .context("GC guard worker stopped before reporting acquisition")?
        .map_err(anyhow::Error::msg)?;

    if let Err(error) = state
        .write_barrier
        .quiesce(DEFAULT_QUIESCE_TIMEOUT)
        .await
        .context("write barrier quiesce timed out (GC aborted)")
    {
        let _ = command_tx.send(GcWorkerCommand::Abort);
        let _ = gc_worker.await;
        return Err(error);
    }
    let write_barrier_resume = WriteBarrierResume {
        barrier: state.write_barrier.clone(),
    };

    if !params.dry_run {
        let active_sync_jobs = state
            .state_store
            .with_state_db(|db| db.count_active_sync_jobs())
            .context("failed to inspect active sync jobs before GC");
        match active_sync_jobs {
            Ok(0) => {}
            Ok(active_sync_jobs) => {
                let _ = command_tx.send(GcWorkerCommand::Abort);
                let _ = gc_worker.await;
                anyhow::bail!(
                    "GC refused: {active_sync_jobs} active sync job(s) may pin staged CAS roots"
                );
            }
            Err(err) => {
                let _ = command_tx.send(GcWorkerCommand::Abort);
                let _ = gc_worker.await;
                return Err(err);
            }
        }
    }

    command_tx
        .send(GcWorkerCommand::Run(write_barrier_resume))
        .context("GC guard worker stopped before run command")?;
    drop(command_tx);
    let mut gc_result = gc_worker
        .await
        .context("GC blocking task panicked")??
        .ok_or_else(|| anyhow::anyhow!("GC guard worker aborted before running"))?;
    gc_result.reaped_seats = reaped_seats;
    gc_result.terminal_chain_candidates = terminal_chain_candidates;
    gc_result.retired_terminal_chains = retired_terminal_chains;
    gc_result.deleted_thread_projection_rows = deleted_thread_projection_rows;
    gc_result.deleted_thread_runtime_rows = deleted_thread_runtime_rows;
    gc_result.deleted_thread_runtime_files = deleted_thread_runtime_files;
    gc_result.pending_retirements_recovered = pending_retirements_recovered;

    Ok(gc_result)
}

/// Run GC and append event log. Called inside `spawn_blocking`.
fn run_gc_and_log(input: GcRunInput<'_>) -> Result<GcResult> {
    let GcRunInput {
        state_authority,
        cas_guard,
        signer,
        params,
        state_store,
        scheduler_db,
        fire_retention,
        reaped_seats,
        terminal_chain_candidates,
        retired_terminal_chains,
        deleted_thread_projection_rows,
        deleted_thread_runtime_rows,
        deleted_thread_runtime_files,
        pending_retirements_recovered,
    } = input;
    let runtime_directory = state_authority.runtime_directory();
    // The worker already owns the exclusive cross-process guard and the daemon
    // write barrier is quiesced. Every unresolved Set/Remove closure remains a
    // temporary root until its compare-acknowledged journal record is removed.
    let mut operational_object_roots = state_store
        .with_state_db(|db| db.pending_transition_object_roots())
        .context("read pending chain-head transition roots before CAS sweep")?
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
    operational_object_roots.extend(
        state_store
            .active_resume_snapshot_roots()
            .context("failed to collect active runtime snapshot roots")?,
    );
    let mirrored_entries = state_store
        .with_state_db(|db| db.list_cas_entries_by_state(ryeos_state::CasEntryState::Mirrored))
        .context("read durable mirrored CAS roots before sweep")?;
    let mut operational_blob_roots = std::collections::BTreeSet::new();
    for entry in mirrored_entries {
        match entry.entry_kind {
            ryeos_state::CasEntryKind::Object => {
                operational_object_roots.insert(entry.hash);
            }
            ryeos_state::CasEntryKind::Blob => {
                operational_blob_roots.insert(entry.hash);
            }
        }
    }
    let operational_roots = gc::AdditionalCasRoots {
        object_hashes: operational_object_roots.into_iter().collect(),
        blob_hashes: operational_blob_roots.into_iter().collect(),
    };
    let mut runtime_cleanup = GcResult::default();
    let retention_started = fire_retention.is_some() && !params.dry_run;
    if fire_retention.is_some() {
        if params.dry_run {
            let pending_outbox = scheduler_db
                .pending_fire_outbox()
                .context("inspect scheduler fire outbox before retention dry-run")?;
            if pending_outbox != 0 {
                anyhow::bail!(
                    "scheduler fire retention dry-run requires an empty outbox; {pending_outbox} durable snapshot(s) still need publication"
                );
            }
        } else {
            scheduler_db
                .drain_fire_outbox_in_directory(runtime_directory)
                .context("drain scheduler fire outbox before retention")?;
        }
    }
    if retention_started {
        scheduler_db
            .begin_fire_retention()
            .context("invalidate scheduler fire projection before journal retention")?;
    }
    let retention_result = (|| -> Result<(Vec<gc::retention::FireRetentionTarget>, usize)> {
        let targets = gc::purge_runtime_state_in_directory(
            runtime_directory,
            params,
            fire_retention,
            &mut runtime_cleanup,
        )
        .context("runtime-state purge failed")?;
        let deleted_projection_rows = if retention_started {
            scheduler_db
                .finish_fire_retention(&targets)
                .context("publish exact scheduler fire retention projection")?
        } else if fire_retention.is_some() && params.dry_run {
            scheduler_db
                .verify_fire_retention_targets(&targets)
                .context("verify exact scheduler fire retention projection for dry-run")?;
            targets.len()
        } else {
            targets.len()
        };
        Ok((targets, deleted_projection_rows))
    })();
    let (_, deleted_fire_projection_rows) = match retention_result {
        Ok(result) => result,
        Err(error) if retention_started => {
            // Do not release the scheduler gate with an invalid projection. The
            // marker makes the crash path safe; this in-process repair gives a
            // failed maintenance call the same convergence before returning.
            let repair = (|| -> Result<()> {
                ryeos_scheduler::projection::rebuild_fires_from_runtime_directory(
                    runtime_directory,
                    scheduler_db,
                )?;
                let specs = scheduler_db.list_specs(false, None)?;
                scheduler_db.rebuild_cursors_for_specs(&specs, lillux::time::timestamp_millis())?;
                Ok(())
            })();
            match repair {
                Ok(()) => return Err(error),
                Err(repair_error) => {
                    anyhow::bail!(
                        "{error:#}; scheduler retention repair also failed: {repair_error:#}"
                    )
                }
            }
        }
        Err(error) => return Err(error),
    };

    let mut result = gc::run_gc_with_pinned_authority(
        state_authority,
        cas_guard,
        Some(signer),
        params,
        &operational_roots,
    )
    .context("GC pipeline failed")?;
    result.deleted_runtime_files = runtime_cleanup.deleted_runtime_files;
    result.deleted_fire_records = runtime_cleanup.deleted_fire_records;
    result.deleted_fire_projection_rows = deleted_fire_projection_rows;
    result.freed_bytes += runtime_cleanup.freed_bytes;
    result.reaped_seats = reaped_seats;
    result.terminal_chain_candidates = terminal_chain_candidates;
    result.retired_terminal_chains = retired_terminal_chains;
    result.deleted_thread_projection_rows = deleted_thread_projection_rows;
    result.deleted_thread_runtime_rows = deleted_thread_runtime_rows;
    result.deleted_thread_runtime_files = deleted_thread_runtime_files;
    result.pending_retirements_recovered = pending_retirements_recovered;

    // Retention: retire old terminal sync-job rows only when the invocation
    // explicitly authors a window. Rust supplies no fallback age. The write
    // barrier is quiesced here, so no concurrent sync-job writes race delete.
    if !params.dry_run {
        if let Some(retention_days) = params.sync_job_retention_days {
            let cutoff = gc::retention::iso8601_days_ago(retention_days);
            let (jobs, attempts) = state_store
                .with_state_db(|db| db.delete_terminal_sync_jobs_before(&cutoff))
                .context("explicit sync-job retention sweep failed")?;
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
    }

    // Dry-run is mutation-free across authoritative and operational stores;
    // even the ordinary best-effort audit append is therefore suppressed.
    if !params.dry_run {
        if let Err(err) = gc::event_log::append_event_in_directory(
            runtime_directory,
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
                deleted_fire_projection_rows: result.deleted_fire_projection_rows,
                deleted_sync_jobs: result.deleted_sync_jobs,
                deleted_sync_job_attempts: result.deleted_sync_job_attempts,
                reaped_seats: result.reaped_seats,
                terminal_chain_candidates: result.terminal_chain_candidates,
                retired_terminal_chains: result.retired_terminal_chains,
                deleted_thread_projection_rows: result.deleted_thread_projection_rows,
                deleted_thread_runtime_rows: result.deleted_thread_runtime_rows,
                deleted_thread_runtime_files: result.deleted_thread_runtime_files,
                pending_retirements_recovered: result.pending_retirements_recovered,
                retired_durable_cas_uploads: result.retired_durable_cas_uploads,
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
