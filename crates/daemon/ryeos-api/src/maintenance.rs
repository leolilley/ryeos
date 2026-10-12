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
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use ryeos_state::gc::{self, GcParams, GcResult};

use ryeos_app::state::AppState;
use ryeos_app::state_store::NodeIdentitySigner;
use ryeos_app::thread_lifecycle::presentation_transfer::UiTransferSettlementWriteOutcome;

/// Default quiesce timeout: 30 seconds.
const DEFAULT_QUIESCE_TIMEOUT: Duration = Duration::from_secs(30);
// Signed event reads happen under the existing GC flock, before the write
// barrier is quiesced. Bound settlement turns per run, not per seat; lease
// claim collection is separately bounded to one 16-row slice. Each turn
// reads at most one 512 KiB event (with a 1 MiB page request).
const MAX_CLAIMED_SETTLEMENT_TURNS_PER_GC: usize = 1024;
const MAX_CLAIMED_SETTLEMENT_TIME_PER_GC: Duration = Duration::from_millis(250);

fn claimed_discovery_root(roots: &[String], start: usize, turn: usize) -> Option<&str> {
    (!roots.is_empty()).then(|| roots[(start.wrapping_add(turn)) % roots.len()].as_str())
}

fn claimed_settlement_has_work(discovery_roots: usize, pending_readers: bool) -> bool {
    discovery_roots != 0 || pending_readers
}

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
    external_content_limits:
        ryeos_app::node_policy::sections::external_content::ExternalContentImportLimits,
    state_store: &'a ryeos_app::state_store::StateStore,
    accounting: Option<&'a ryeos_app::accounting_db::AccountingDb>,
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
    // Avoid claiming leases when this run already knows the scheduler
    // projection cannot satisfy its later retention admission. The writer
    // rechecks under its own read gate; the GC pass rechecks under write.
    if (params.deep || params.prune_runtime_history || fire_retention.is_some())
        && !state.scheduler_db.fire_projection_is_current()?
    {
        anyhow::bail!(
            "maintenance GC requires a current scheduler fire projection before pin inspection"
        );
    }
    if !params.dry_run
        && let Some(grace_ms) = seat_lease_grace_ms
    {
        let now_ms = lillux::time::timestamp_millis();
        let cutoff = now_ms.saturating_sub(grace_ms);
        let mut claimed_seat_roots = Vec::new();
        for thread_id in state.threads.next_expired_ui_seat_lease_slice(cutoff, 16)? {
            if !state
                .state_store
                .claim_expired_seat_lease(&thread_id, cutoff)?
            {
                continue;
            }
            // The projection can be absent or stale; signed-root discovery
            // must still run for every surviving exact claim.
            claimed_seat_roots.push(thread_id.clone());
            let Some(detail) = state.state_store.get_thread(&thread_id)? else {
                // Projection absence is not signed terminality. Retain a
                // claimed fence unless the exact seat root proves terminal.
                if state
                    .state_store
                    .remove_terminal_seat_lease_if_proved(&thread_id)?
                {
                    state
                        .threads
                        .discard_claimed_ui_seat_transfer_scan(&thread_id)?;
                    claimed_seat_roots.pop();
                    reaped_seats += 1;
                }
                continue;
            };
            // The claimed lease row is the structural authority that this
            // thread participates in lease expiry. Maintenance must not
            // rediscover that relationship from an authored kind string.
            if detail.status != "running" {
                if state
                    .state_store
                    .remove_terminal_seat_lease_if_proved(&thread_id)?
                {
                    state
                        .threads
                        .discard_claimed_ui_seat_transfer_scan(&thread_id)?;
                    claimed_seat_roots.pop();
                    reaped_seats += 1;
                }
                continue;
            }
            // The common finalizer deliberately refuses signed seats until
            // exact lifecycle transfer clearance exists. Settlement may
            // advance retained testimony, but even an empty local scan does
            // not prove absence of inbound source-only Prepared elsewhere.
            // Keep the claimed lease and Running seat for retry/clearance.
        }
        // Give both positive discovery and exact anchored settlement readers
        // repeated fair turns. Advancing only the decoder would leave an old
        // transfer identity undiscovered for one daily GC per history event.
        let discovery_start = state
            .threads
            .next_claimed_ui_seat_discovery_start(claimed_seat_roots.len());
        // Claim/projection I/O must not consume the discovery budget before
        // the first signed-history turn. The claim slice is bounded above.
        let settlement_deadline = Instant::now() + MAX_CLAIMED_SETTLEMENT_TIME_PER_GC;
        for turn in 0..MAX_CLAIMED_SETTLEMENT_TURNS_PER_GC {
            if Instant::now() >= settlement_deadline
                || !claimed_settlement_has_work(
                    claimed_seat_roots.len(),
                    state.threads.has_pending_claimed_ui_transfer_settlement()?,
                )
            {
                break;
            }
            if let Some(seat_root_id) =
                claimed_discovery_root(&claimed_seat_roots, discovery_start, turn)
            {
                if let Err(error) = settle_claimed_seat_transfers(state, seat_root_id).await {
                    tracing::warn!(%seat_root_id, %error, "claimed UI seat transfer discovery deferred");
                }
            }
            match state
                .threads
                .advance_claimed_ui_transfer_settlement_once()
                .await
            {
                Ok(Some(UiTransferSettlementWriteOutcome::Deferred(_)))
                | Ok(Some(UiTransferSettlementWriteOutcome::Conflict)) => {
                    tracing::debug!("claimed seat transfer settlement retained for retry");
                }
                Ok(Some(_)) | Ok(None) => {}
                Err(error) => {
                    tracing::warn!(%error, "claimed seat transfer settlement retained for retry");
                }
            }
        }
        for seat_root_id in &claimed_seat_roots {
            match state
                .threads
                .finalize_claimed_ui_seat_if_cleared(seat_root_id)
                .await
            {
                Ok(true) => reaped_seats += 1,
                Ok(false) => {}
                Err(error) => tracing::debug!(%seat_root_id, %error,
                    "claimed signed seat exhaustive finalization deferred"),
            }
        }
    }

    // Every scheduler mutation and dispatch takes this gate. Settlement above
    // acquires its read side itself, so it must run before this write side.
    // Keep the write side across terminal-chain pin inspection and both
    // fire-history stores for a closed scheduler state during GC.
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
        let mut retirement = state.state_store.retire_due_terminal_chains(
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
        let signed = state
            .threads
            .advance_terminal_ui_seat_retirement(&lillux::time::iso8601_now(), params.dry_run)?;
        retirement.candidate_chains += signed.candidate_chains;
        retirement.retired_chains += signed.retired_chains;
        retirement.deleted_projection_rows += signed.deleted_projection_rows;
        retirement.deleted_runtime_rows += signed.deleted_runtime_rows;
        retirement.deleted_runtime_files += signed.deleted_runtime_files;
        retirement.pending_retirements_recovered += signed.pending_retirements_recovered;
        terminal_chain_candidates = retirement.candidate_chains;
        retired_terminal_chains = retirement.retired_chains;
        deleted_thread_projection_rows = retirement.deleted_projection_rows;
        deleted_thread_runtime_rows = retirement.deleted_runtime_rows;
        deleted_thread_runtime_files = retirement.deleted_runtime_files;
        pending_retirements_recovered = retirement.pending_retirements_recovered;
    }

    // The global mutation order is cross-process CAS guard, then in-process
    // write permit/barrier. A dedicated blocking worker can hold the !Send file
    // guard while the async coordinator waits for active permit holders to
    // drain, without ever carrying that guard across an await.
    let signer = NodeIdentitySigner::from_identity(&state.identity);
    let params_clone = params.clone();
    let external_content_limits = state
        .node_policy
        .require::<ryeos_app::node_policy::sections::external_content::ExternalContentImportPolicyRecord>()?
        .limits
        .clone();
    let state_store = state.state_store.clone();
    let accounting = state.accounting.clone();
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
        let command = command_rx.recv();
        match command {
            Ok(GcWorkerCommand::Run(_write_barrier_resume)) => run_gc_and_log(GcRunInput {
                state_authority: &state_authority,
                cas_guard: &cas_guard,
                signer: &signer,
                params: &params_clone,
                external_content_limits,
                state_store: &state_store,
                accounting: accounting.as_deref(),
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
    let worker_result = gc_worker.await.context("GC blocking task panicked")?;
    let mut gc_result = match worker_result {
        Ok(Some(result)) => result,
        Ok(None) => anyhow::bail!("GC guard worker aborted before running"),
        Err(error) => {
            tracing::error!(
                error = %format!("{error:#}"),
                "maintenance GC pipeline failed"
            );
            return Err(error);
        }
    };
    gc_result.reaped_seats = reaped_seats;
    gc_result.terminal_chain_candidates = terminal_chain_candidates;
    gc_result.retired_terminal_chains = retired_terminal_chains;
    gc_result.deleted_thread_projection_rows = deleted_thread_projection_rows;
    gc_result.deleted_thread_runtime_rows = deleted_thread_runtime_rows;
    gc_result.deleted_thread_runtime_files = deleted_thread_runtime_files;
    gc_result.pending_retirements_recovered = pending_retirements_recovered;

    Ok(gc_result)
}

/// Advance only known transfer testimony on this already-claimed exact seat.
/// Discovery is only a bounded-page locator. Each known identity is proved
/// again by the exact two-seat writer; a partial scan is never absence proof.
/// This pass does not clear the signed-seat finalization veto.
async fn settle_claimed_seat_transfers(state: &AppState, seat_root_id: &str) -> Result<()> {
    state.threads.queue_next_claimed_ui_seat_transfer(seat_root_id)
}

/// Run GC and append event log. Called inside `spawn_blocking`.
fn run_gc_and_log(input: GcRunInput<'_>) -> Result<GcResult> {
    let GcRunInput {
        state_authority,
        cas_guard,
        signer,
        params,
        external_content_limits,
        state_store,
        accounting,
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
    // Effect-record retention runs before root gathering so a pruned row's
    // object is sweepable in this same pass. The cap is a row count: results
    // are bounded per record, and the CAS bytes follow the rows.
    const MAX_EFFECT_RECORDS: usize = 10_000;
    const MAX_PROVIDER_CALL_RECORDS: usize = 10_000;
    let provider_namespace = ryeos_state::ReplayIndexNamespace::new(
        ryeos_provider_contract::PROVIDER_CALL_REPLAY_NAMESPACE,
    )?;
    let effect_namespace =
        ryeos_state::ReplayIndexNamespace::new(ryeos_effect_contract::EFFECT_REPLAY_NAMESPACE)?;
    let pruned_provider_call_records = state_store
        .with_state_db(|db| db.prune_replay_records(&provider_namespace, MAX_PROVIDER_CALL_RECORDS))
        .context("prune provider call record retention")?;
    if pruned_provider_call_records > 0 {
        tracing::info!(
            pruned_provider_call_records,
            "provider-call-record retention pruned rows"
        );
    }
    let pruned_effect_records = state_store
        .with_state_db(|db| db.prune_replay_records(&effect_namespace, MAX_EFFECT_RECORDS))
        .context("prune stale effect records before CAS sweep")?;
    if pruned_effect_records > 0 {
        tracing::info!(pruned_effect_records, "effect-record retention pruned rows");
    }
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
    operational_object_roots.extend(
        scheduler_db
            .operational_snapshot_roots()
            .context("failed to collect scheduler snapshot roots")?,
    );
    let mirrored_entries = state_store
        .with_state_db(|db| db.list_cas_entries_by_state(ryeos_state::CasEntryState::Mirrored))
        .context("read durable mirrored CAS roots before sweep")?;
    let mut operational_blob_roots = std::collections::BTreeSet::new();
    operational_blob_roots.extend(
        state_store
            .external_execution_blob_roots()
            .context("read retained external candidate evidence roots before sweep")?,
    );
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
    // Indexed effect records are replay authority: they stay reachable
    // exactly as long as their index rows exist, and retention is row
    // deletion followed by an ordinary sweep.
    operational_object_roots.extend(
        state_store
            .with_state_db(|db| db.list_replay_record_hashes())
            .context("read durable replay record roots before sweep")?,
    );
    if let Some(accounting) = accounting {
        operational_object_roots.extend(
            accounting
                .provider_evidence_object_roots()
                .context("read durable provider evidence roots before sweep")?,
        );
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

    // Materialized project generations are executor-owned cache. Deep GC
    // reclaims them through their exact construction/lease protocol while the
    // daemon write barrier and cross-process CAS guard are held; the generic
    // state-layer cache purge deliberately does not guess at this liveness.
    let materialization_cleanup = if params.deep {
        ryeos_executor::execution::cache::MaterializationCache::from_runtime_state(
            runtime_directory.path(),
        )
        .prune_inactive_generations(params.dry_run)
        .context("lease-aware materialized snapshot cache purge failed")?
    } else {
        ryeos_executor::execution::cache::MaterializationPruneReport::default()
    };

    let run_gc = || {
        gc::run_gc_with_pinned_authority(
            state_authority,
            cas_guard,
            Some(signer),
            params,
            &operational_roots,
        )
        .context("GC pipeline failed")
    };
    let mut result = if params.compact && !params.dry_run {
        state_store.with_project_head_compaction_authority(run_gc)?
    } else {
        run_gc()?
    };
    let limits = external_content_limits;
    let store = if params.dry_run {
        ryeos_state::LargeObjectStore::open_under(state_authority.runtime_directory())?
    } else {
        Some(state_authority.large_object_store()?)
    };
    if let Some(store) = store {
        let capacity = store.filesystem_capacity()?;
        let stored_bytes = store.total_stored_bytes()?;
        let reserve_shortfall = limits
            .minimum_free_bytes
            .saturating_sub(capacity.available_bytes);
        let reserve_budget = stored_bytes.saturating_sub(reserve_shortfall);
        let effective_budget = limits.store_budget_bytes.min(reserve_budget);
        let sweep = store.sweep_to_budget_with_mode(
            effective_budget,
            &result.reachable_large_object_hashes,
            params.dry_run,
        )?;
        let staging = store.sweep_abandoned_staging_with_mode(params.dry_run)?;
        result.inspected_large_objects = sweep.inspected_objects;
        result.deleted_large_objects = sweep.evicted.len();
        result.deleted_large_object_staging_files = staging.files;
        result.preserved_rooted_large_objects = sweep.retained_roots;
        result.preserved_leased_large_objects = sweep.retained_leased;
        result.freed_large_object_bytes = sweep
            .evicted
            .iter()
            .fold(staging.bytes, |total, (_, bytes)| {
                total.saturating_add(*bytes)
            });
        result.freed_bytes = result
            .freed_bytes
            .saturating_add(result.freed_large_object_bytes);
    }
    result.deleted_runtime_files = runtime_cleanup
        .deleted_runtime_files
        .saturating_add(materialization_cleanup.reclaimable_files);
    result.deleted_fire_records = runtime_cleanup.deleted_fire_records;
    result.deleted_fire_projection_rows = deleted_fire_projection_rows;
    result.freed_bytes = result
        .freed_bytes
        .saturating_add(runtime_cleanup.freed_bytes)
        .saturating_add(materialization_cleanup.reclaimable_bytes);
    result.inspected_materialized_snapshots = materialization_cleanup.inspected_generations;
    result.deleted_materialized_snapshots = materialization_cleanup.reclaimable_generations;
    result.preserved_active_materialized_snapshots = materialization_cleanup.active_generations;
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
    if !params.dry_run
        && let Some(retention_days) = params.sync_job_retention_days
    {
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

    // Dry-run is mutation-free across authoritative and operational stores;
    // even the ordinary best-effort audit append is therefore suppressed.
    if !params.dry_run
        && let Err(err) = gc::event_log::append_event_in_directory(
            runtime_directory,
            &gc::event_log::GcEvent {
                timestamp: lillux::time::iso8601_now(),
                dry_run: params.dry_run,
                compact: params.compact,
                roots_walked: result.roots_walked,
                reachable_objects: result.reachable_objects,
                reachable_blobs: result.reachable_blobs,
                reachable_large_objects: result.reachable_large_objects,
                deleted_objects: result.deleted_objects,
                deleted_blobs: result.deleted_blobs,
                inspected_large_objects: result.inspected_large_objects,
                deleted_large_objects: result.deleted_large_objects,
                deleted_large_object_staging_files: result.deleted_large_object_staging_files,
                preserved_rooted_large_objects: result.preserved_rooted_large_objects,
                preserved_leased_large_objects: result.preserved_leased_large_objects,
                freed_large_object_bytes: result.freed_large_object_bytes,
                deleted_cas_staging_files: result.deleted_cas_staging_files,
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
                inspected_materialized_snapshots: result.inspected_materialized_snapshots,
                deleted_materialized_snapshots: result.deleted_materialized_snapshots,
                preserved_active_materialized_snapshots: result
                    .preserved_active_materialized_snapshots,
                freed_bytes: result.freed_bytes,
                snapshots_compacted: result
                    .compaction
                    .as_ref()
                    .map(|c| c.snapshots_removed)
                    .unwrap_or(0),
                duration_ms: result.duration_ms,
            },
        )
    {
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

    #[test]
    fn claimed_settlement_turns_keep_old_readers_when_current_slice_is_empty() {
        assert!(claimed_settlement_has_work(0, true));
        assert!(claimed_settlement_has_work(1, false));
        assert!(!claimed_settlement_has_work(0, false));
        let roots = vec!["T-one".to_string(), "T-two".to_string()];
        assert_eq!(claimed_discovery_root(&roots, 0, 0), Some("T-one"));
        assert_eq!(claimed_discovery_root(&roots, 0, 1), Some("T-two"));
        assert_eq!(claimed_discovery_root(&roots, 1, 0), Some("T-two"));
        assert_eq!(claimed_discovery_root(&roots, 1, 1), Some("T-one"));
        assert_eq!(claimed_discovery_root(&[], 0, 0), None);
    }
}
