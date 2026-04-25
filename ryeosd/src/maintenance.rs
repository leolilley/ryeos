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

use crate::state::AppState;
use crate::state_store::NodeIdentitySigner;

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
    )
)]
pub async fn run_maintenance_gc(
    state: &AppState,
    params: &GcParams,
) -> Result<GcResult> {
    let state_root = state.config.state_dir.join(".state");
    let cas_root = state_root.join("objects");
    let refs_root = state_root.join("refs");

    // Step 1: Acquire GC lock
    let _gc_lock = gc::GcLock::acquire(
        &state_root,
        state.identity.fingerprint(),
    )
    .context("failed to acquire GC lock (another GC may be running)")?;

    tracing::info!(
        dry_run = params.dry_run,
        compact = params.compact,
        "maintenance GC: lock acquired"
    );

    // Step 2: Quiesce write barrier
    state
        .write_barrier
        .quiesce(DEFAULT_QUIESCE_TIMEOUT)
        .await
        .context("write barrier quiesce timed out (GC aborted)")?;

    // Step 3: Run GC in a blocking task (CPU/disk I/O heavy)
    // Extract all data from AppState before spawning, since the closure
    // must be 'static + Send.
    let signer = NodeIdentitySigner::from_identity(&state.identity);
    let params_clone = params.clone();
    let state_root_for_log = state_root.clone();

    let gc_result = tokio::task::spawn_blocking(move || {
        run_gc_and_log(&cas_root, &refs_root, &signer, &params_clone, &state_root_for_log)
    })
    .await;

    // ALWAYS resume writes, even if GC failed or panicked
    state.write_barrier.resume();

    // NOW propagate errors
    let gc_result = gc_result.context("GC blocking task panicked")??;

    Ok(gc_result)
}

/// Run GC and append event log. Called inside `spawn_blocking`.
fn run_gc_and_log(
    cas_root: &std::path::Path,
    refs_root: &std::path::Path,
    signer: &NodeIdentitySigner,
    params: &GcParams,
    state_root: &std::path::Path,
) -> Result<GcResult> {
    let result = gc::run_gc(
        cas_root,
        refs_root,
        Some(signer),
        params,
    )
    .context("GC pipeline failed")?;

    // Log the event (best-effort)
    if let Err(err) = gc::event_log::append_event(
        state_root,
        &gc::event_log::GcEvent {
            timestamp: lillux::time::iso8601_now(),
            dry_run: params.dry_run,
            compact: params.compact,
            roots_walked: result.roots_walked,
            reachable_objects: result.reachable_objects,
            reachable_blobs: result.reachable_blobs,
            deleted_objects: result.deleted_objects,
            deleted_blobs: result.deleted_blobs,
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
