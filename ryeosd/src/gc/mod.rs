//! Two-phase garbage collection pipeline.
//!
//! Phase 1 — Prune: delete stale cache and excess execution records.
//! Phase 2 — Sweep: mark reachable objects from all refs, delete unreachable.

pub mod epochs;
pub mod event_log;
pub mod lock;
pub mod mark_cache;
pub mod prune;
pub mod sweep;

pub mod compact;

use std::path::Path;
use std::time::Instant;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::cas::CasStore;
use crate::refs::RefStore;

// ── Configuration ───────────────────────────────────────────────────

/// Retention policy for GC compaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetentionPolicy {
    /// Max manual push snapshots to keep per project.
    pub manual_pushes: usize,
    /// Max daily checkpoint snapshots to keep.
    pub daily_checkpoints: usize,
    /// Max weekly checkpoint snapshots to keep.
    pub weekly_checkpoints: usize,
    /// Max successful execution records per (project, graph).
    pub max_success_executions: usize,
    /// Max failed execution records per (project, graph).
    pub max_failure_executions: usize,
    /// Max age (hours) for materialized cache snapshots.
    pub cache_max_age_hours: u64,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            manual_pushes: 10,
            daily_checkpoints: 7,
            weekly_checkpoints: 4,
            max_success_executions: 50,
            max_failure_executions: 20,
            cache_max_age_hours: 72,
        }
    }
}

/// Parameters for a GC run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GCParams {
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub aggressive: bool,
    #[serde(default)]
    pub policy: Option<RetentionPolicy>,
    #[serde(default)]
    pub compact: bool,
}

impl Default for GCParams {
    fn default() -> Self {
        Self {
            dry_run: false,
            aggressive: false,
            policy: None,
            compact: false,
        }
    }
}

// ── Results ─────────────────────────────────────────────────────────

/// Result of a full GC run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GCResult {
    pub prune: prune::PruneResult,
    pub compaction: Option<compact::CompactionResult>,
    pub sweep: sweep::SweepResult,
    pub total_freed_bytes: u64,
    pub duration_ms: u64,
}

// ── Orchestrator ────────────────────────────────────────────────────

/// Run the full two-phase GC pipeline.
pub fn run_gc(
    cas: &CasStore,
    refs: &RefStore,
    cas_root: &Path,
    state_dir: &Path,
    node_id: &str,
    params: &GCParams,
) -> Result<GCResult> {
    let start = Instant::now();

    let policy = params.policy.clone().unwrap_or_else(|| {
        if params.aggressive {
            RetentionPolicy {
                manual_pushes: 3,
                daily_checkpoints: 3,
                weekly_checkpoints: 2,
                max_success_executions: 10,
                max_failure_executions: 5,
                cache_max_age_hours: 24,
            }
        } else {
            RetentionPolicy::default()
        }
    });

    // Acquire GC lock (RAII — released on drop)
    let _lock = lock::GcLock::acquire(cas_root, node_id)?;

    // Phase 1: Prune
    _lock.update_phase(node_id, "prune")?;
    let prune_result = prune::run_prune(cas_root, state_dir, &policy, params.dry_run)?;

    // Phase 2: Compact (opt-in)
    let compaction_result = if params.compact {
        _lock.update_phase(node_id, "compact")?;
        Some(compact::run_compact(
            cas,
            refs,
            cas_root,
            &policy,
            params.dry_run,
        )?)
    } else {
        None
    };

    // Phase 3: Sweep
    _lock.update_phase(node_id, "sweep")?;
    let sweep_result = sweep::run_sweep(cas, refs, cas_root, params.dry_run)?;

    let total_freed = prune_result.freed_bytes + sweep_result.freed_bytes;
    let duration_ms = start.elapsed().as_millis() as u64;

    let result = GCResult {
        prune: prune_result,
        compaction: compaction_result,
        sweep: sweep_result,
        total_freed_bytes: total_freed,
        duration_ms,
    };

    tracing::info!(
        freed_bytes = result.total_freed_bytes,
        duration_ms = result.duration_ms,
        dry_run = params.dry_run,
        "GC complete"
    );

    let event = event_log::GCEvent::from_result(&result, params, node_id);
    if let Err(e) = event_log::append_event(cas_root, &event) {
        tracing::warn!(error = %e, "failed to write GC event log");
    }

    Ok(result)
}
