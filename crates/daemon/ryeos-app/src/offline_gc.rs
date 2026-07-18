//! Explicit offline garbage collection that retires all thread history.
//!
//! Normal maintenance GC deliberately preserves authoritative chain heads.
//! This module owns the separate operator-authorized path used when an entire
//! local thread-history epoch is disposable. It coordinates every store while
//! the daemon state lock, runtime namespace lock, and exclusive CAS mutation
//! guard are held.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::config::{Config, ConfigSources};
use crate::runtime_db::{RuntimeDb, RuntimeThreadHistoryDiscardReport};
use crate::state_lock::{default_lock_path, StateLock};

#[derive(Debug, Clone, Default)]
pub struct OfflineThreadHistoryGcOptions {
    pub app_root: Option<PathBuf>,
    pub dry_run: bool,
    pub sweep_cas: bool,
    /// Explicit immutable project-schema cutover for principal and deployed
    /// project HEADs. This is accepted only with the all-thread-history discard
    /// because live history may reference either namespace.
    pub discard_project_heads: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OfflineThreadHistoryGcPhase {
    CapturingAuthority,
    InspectingHistory,
    PublishingIntent,
    RetiringChainHeads,
    RetiringProjectHeads,
    RebuildingProjection,
    ClearingRuntime,
    ClearingScheduler,
    Finalizing,
    SweepingCas,
    Complete,
}

impl OfflineThreadHistoryGcPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CapturingAuthority => "capturing_authority",
            Self::InspectingHistory => "inspecting_history",
            Self::PublishingIntent => "publishing_intent",
            Self::RetiringChainHeads => "retiring_chain_heads",
            Self::RetiringProjectHeads => "retiring_project_heads",
            Self::RebuildingProjection => "rebuilding_projection",
            Self::ClearingRuntime => "clearing_runtime",
            Self::ClearingScheduler => "clearing_scheduler",
            Self::Finalizing => "finalizing",
            Self::SweepingCas => "sweeping_cas",
            Self::Complete => "complete",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OfflineThreadHistoryGcProgress {
    pub phase: OfflineThreadHistoryGcPhase,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<usize>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ProjectionDiscardReport {
    pub chains_rebuilt: usize,
    pub threads_restored: usize,
    pub events_projected: usize,
    pub superseded_instances_deleted: usize,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct OfflineThreadHistoryGcReport {
    pub app_root: PathBuf,
    pub dry_run: bool,
    pub chain_heads: usize,
    pub project_heads: usize,
    pub chain_ref_artifacts: usize,
    pub pending_transitions: usize,
    pub runtime_rows: RuntimeThreadHistoryDiscardReport,
    pub thread_runtime_artifacts: usize,
    pub scheduler_journal_artifacts: usize,
    pub scheduler_rows: ryeos_scheduler::db::SchedulerFireHistoryDiscardReport,
    pub projection: ProjectionDiscardReport,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cas_sweep: Option<ryeos_state::gc::GcResult>,
}

impl OfflineThreadHistoryGcReport {
    pub fn total_discarded_rows(&self) -> usize {
        self.runtime_rows.total_rows() + self.scheduler_rows.total_rows()
    }
}

/// Inspect or execute the explicit all-thread-history GC transaction.
pub fn run_offline_thread_history_gc(
    options: &OfflineThreadHistoryGcOptions,
) -> Result<OfflineThreadHistoryGcReport> {
    run_offline_thread_history_gc_inner(options, None)
}

pub fn run_offline_thread_history_gc_with_progress(
    options: &OfflineThreadHistoryGcOptions,
    observer: &mut dyn FnMut(&OfflineThreadHistoryGcProgress),
) -> Result<OfflineThreadHistoryGcReport> {
    run_offline_thread_history_gc_inner(options, Some(observer))
}

fn run_offline_thread_history_gc_inner(
    options: &OfflineThreadHistoryGcOptions,
    mut observer: Option<&mut dyn FnMut(&OfflineThreadHistoryGcProgress)>,
) -> Result<OfflineThreadHistoryGcReport> {
    publish_progress(
        &mut observer,
        OfflineThreadHistoryGcPhase::CapturingAuthority,
        None,
    );
    if options.dry_run && options.sweep_cas {
        anyhow::bail!(
            "--sweep-cas cannot be forecast before thread roots are retired; run the history dry-run first, then use --sweep-cas only with the confirmed discard"
        );
    }
    let config = Config::load(&ConfigSources {
        app_root: options.app_root.clone(),
        ..ConfigSources::default()
    })
    .context("load local node configuration for offline GC")?;
    let runtime_state_dir = config.runtime_state_dir();

    // This is the outer ownership proof. A running daemon and every
    // cooperating standalone state service hold the same lock.
    let _state_lock = StateLock::acquire(&default_lock_path(&config.app_root))
        .context("offline thread-history GC requires the daemon to be stopped")?;
    let runtime_directory =
        lillux::PinnedDirectory::open(&runtime_state_dir)?.ok_or_else(|| {
            anyhow::anyhow!("runtime state is absent: {}", runtime_state_dir.display())
        })?;
    let runtime_directory_lock = runtime_directory
        .lock_exclusive()
        .context("lock offline runtime-state namespace")?;

    // Signed chain heads belong to the node-identity trust domain, matching
    // ordinary daemon startup. Item/publisher trust is a separate domain and
    // must never be substituted for head authority here.
    let identity = crate::identity::NodeIdentity::load(&config.node_signing_key_path)
        .context("load node identity for signed-head verification")?;
    let mut head_trust = ryeos_state::refs::TrustStore::new();
    head_trust.insert(
        identity.fingerprint().to_string(),
        *identity.verifying_key(),
    );
    let head_trust = Arc::new(head_trust);
    let mut state_db = ryeos_state::StateDb::open_for_projection_rebuild(
        &runtime_state_dir,
        Arc::clone(&head_trust),
    )
    .context("open pinned state authority for offline thread-history GC")?;
    let state_authority = state_db.pinned_authority()?;
    if !state_authority
        .runtime_directory()
        .is_same_directory(&runtime_directory)?
    {
        anyhow::bail!("runtime-state path changed while offline GC captured authority");
    }

    let _gc_lock = if options.sweep_cas {
        Some(
            ryeos_state::gc::GcLock::acquire(&runtime_state_dir, "offline-thread-history")
                .context("acquire offline CAS GC lock")?,
        )
    } else {
        None
    };
    let cas_guard = state_authority
        .acquire_exclusive_guard(!options.dry_run)
        .context("acquire exclusive CAS/head mutation guard for offline GC")?;

    let mut runtime_db = open_runtime_db(
        &config,
        &runtime_directory,
        &runtime_directory_lock,
        options.dry_run,
    )?;
    let scheduler_db_path = runtime_state_dir.join("scheduler.sqlite3");
    let scheduler_db = open_scheduler_db(&scheduler_db_path, options.dry_run)?;

    // Validate every participating namespace before publishing destructive
    // intent. The second pass performs the same descriptor-rooted checks while
    // mutating; no pathname-only deletion is used.
    publish_progress(
        &mut observer,
        OfflineThreadHistoryGcPhase::InspectingHistory,
        None,
    );
    let authoritative_preview = state_db
        .discard_authoritative_thread_history_admitted(&cas_guard, true)
        .context("inspect authoritative thread history")?;
    let project_heads_preview = if options.discard_project_heads {
        state_db
            .discard_all_project_heads_admitted(&cas_guard, true)
            .context("inspect principal and deployed project HEADs")?
    } else {
        0
    };
    let runtime_preview = if runtime_db.requires_explicit_history_reset() {
        // Strict no-backcompat means an incompatible layout is never decoded
        // merely to estimate destructive counts.
        Default::default()
    } else {
        runtime_db
            .discard_all_thread_history(true)
            .context("inspect daemon runtime thread rows")?
    };
    let thread_runtime_preview = crate::state_store::discard_all_thread_runtime_files(
        &config.app_root,
        &runtime_directory,
        true,
    )
    .context("inspect per-thread runtime files")?;
    let scheduler_journal_preview = discard_scheduler_fire_journals(&runtime_directory, true)
        .context("inspect scheduler fire journals")?;
    let scheduler_preview = match scheduler_db.as_ref() {
        Some(db) => db
            .discard_fire_history(true)
            .context("inspect scheduler fire projection")?,
        None => Default::default(),
    };

    let operational_roots = if options.sweep_cas {
        Some(inspect_operational_gc_roots(
            &runtime_directory,
            &runtime_directory_lock,
            options.dry_run,
        )?)
    } else {
        None
    };

    if options.dry_run {
        publish_progress(&mut observer, OfflineThreadHistoryGcPhase::Complete, None);
        return Ok(OfflineThreadHistoryGcReport {
            app_root: config.app_root,
            dry_run: true,
            chain_heads: authoritative_preview.chain_heads,
            project_heads: project_heads_preview,
            chain_ref_artifacts: authoritative_preview.chain_ref_artifacts,
            pending_transitions: authoritative_preview.pending_transitions,
            runtime_rows: runtime_preview,
            thread_runtime_artifacts: thread_runtime_preview,
            scheduler_journal_artifacts: scheduler_journal_preview,
            scheduler_rows: scheduler_preview,
            projection: ProjectionDiscardReport {
                superseded_instances_deleted: authoritative_preview.superseded_projection_instances,
                ..ProjectionDiscardReport::default()
            },
            cas_sweep: None,
        });
    }

    publish_progress(
        &mut observer,
        OfflineThreadHistoryGcPhase::PublishingIntent,
        None,
    );
    state_db
        .begin_thread_history_discard_admitted(&cas_guard)
        .context("publish offline thread-history discard intent")?;
    runtime_db
        .apply_explicit_history_reset(&config.db_path)
        .context("apply runtime schema cutover after durable discard intent")?;

    let authoritative = {
        let mut authoritative_progress = |progress| match progress {
            ryeos_state::AuthoritativeThreadHistoryDiscardProgress::RetiringChainHeads {
                completed,
                total,
            } => publish_progress(
                &mut observer,
                OfflineThreadHistoryGcPhase::RetiringChainHeads,
                Some((completed, total)),
            ),
            ryeos_state::AuthoritativeThreadHistoryDiscardProgress::RebuildingProjection => {
                publish_progress(
                    &mut observer,
                    OfflineThreadHistoryGcPhase::RebuildingProjection,
                    None,
                )
            }
        };
        state_db
            .discard_authoritative_thread_history_admitted_with_progress(
                &cas_guard,
                false,
                &mut authoritative_progress,
            )
            .context("discard authoritative thread history")?
    };

    let project_heads = if options.discard_project_heads {
        publish_progress(
            &mut observer,
            OfflineThreadHistoryGcPhase::RetiringProjectHeads,
            None,
        );
        state_db
            .discard_all_project_heads_admitted(&cas_guard, false)
            .context("discard principal and deployed project HEADs for schema cutover")?
    } else {
        0
    };

    publish_progress(
        &mut observer,
        OfflineThreadHistoryGcPhase::ClearingRuntime,
        None,
    );
    let runtime_rows = runtime_db
        .discard_all_thread_history(false)
        .context("discard daemon runtime thread rows")?;
    let thread_runtime_artifacts = crate::state_store::discard_all_thread_runtime_files(
        &config.app_root,
        &runtime_directory,
        false,
    )
    .context("discard per-thread runtime files")?;

    publish_progress(
        &mut observer,
        OfflineThreadHistoryGcPhase::ClearingScheduler,
        None,
    );
    let scheduler_journal_artifacts = discard_scheduler_fire_journals(&runtime_directory, false)
        .context("discard scheduler fire journals")?;
    let scheduler_rows = match scheduler_db.as_ref() {
        Some(db) => db
            .discard_fire_history(false)
            .context("discard scheduler fire projection")?,
        None => Default::default(),
    };

    publish_progress(&mut observer, OfflineThreadHistoryGcPhase::Finalizing, None);
    state_db
        .finish_thread_history_discard_admitted(&cas_guard)
        .context("acknowledge completed offline thread-history discard")?;

    // Physical CAS reclamation is separable from retiring the roots. A sweep
    // failure therefore cannot strand an otherwise complete reset marker and
    // block startup; ordinary maintenance can retry reclamation later.
    let cas_sweep = match operational_roots {
        Some(roots) => {
            publish_progress(
                &mut observer,
                OfflineThreadHistoryGcPhase::SweepingCas,
                None,
            );
            Some(
                ryeos_state::gc::run_gc_with_pinned_authority(
                &state_authority,
                &cas_guard,
                None,
                &ryeos_state::gc::GcParams::default(),
                &roots,
            )
            .context(
                "thread history was retired successfully, but the optional CAS sweep failed; startup is unblocked and maintenance GC can retry the sweep",
                )?,
            )
        }
        None => None,
    };

    let rebuilt = authoritative.rebuilt_projection.unwrap_or_default();
    publish_progress(&mut observer, OfflineThreadHistoryGcPhase::Complete, None);
    Ok(OfflineThreadHistoryGcReport {
        app_root: config.app_root,
        dry_run: false,
        chain_heads: authoritative.chain_heads,
        project_heads,
        chain_ref_artifacts: authoritative.chain_ref_artifacts,
        pending_transitions: authoritative.pending_transitions,
        runtime_rows,
        thread_runtime_artifacts,
        scheduler_journal_artifacts,
        scheduler_rows,
        projection: ProjectionDiscardReport {
            chains_rebuilt: rebuilt.chains_rebuilt,
            threads_restored: rebuilt.threads_restored,
            events_projected: rebuilt.events_projected,
            superseded_instances_deleted: authoritative.superseded_projection_instances,
        },
        cas_sweep,
    })
}

fn publish_progress(
    observer: &mut Option<&mut dyn FnMut(&OfflineThreadHistoryGcProgress)>,
    phase: OfflineThreadHistoryGcPhase,
    counts: Option<(usize, usize)>,
) {
    if let Some(observer) = observer.as_deref_mut() {
        let (completed, total) = counts
            .map(|(completed, total)| (Some(completed), Some(total)))
            .unwrap_or((None, None));
        observer(&OfflineThreadHistoryGcProgress {
            phase,
            completed,
            total,
        });
    }
}

fn open_runtime_db(
    config: &Config,
    runtime_directory: &lillux::PinnedDirectory,
    runtime_directory_lock: &lillux::PinnedDirectoryLock,
    dry_run: bool,
) -> Result<RuntimeDb> {
    let parent_path = config.db_path.parent().unwrap_or_else(|| Path::new("."));
    let parent = if dry_run {
        lillux::PinnedDirectory::open(parent_path)?.ok_or_else(|| {
            anyhow::anyhow!(
                "runtime database parent is absent: {}",
                parent_path.display()
            )
        })?
    } else {
        lillux::PinnedDirectory::open_or_create(parent_path)?
    };
    if runtime_directory.is_same_directory(&parent)? {
        if dry_run {
            RuntimeDb::open_existing_current_with_namespace_authority(
                &config.db_path,
                parent,
                runtime_directory_lock.clone(),
            )
        } else {
            RuntimeDb::open_for_explicit_history_reset_with_namespace_authority(
                &config.db_path,
                parent,
                runtime_directory_lock.clone(),
            )
        }
    } else if dry_run {
        RuntimeDb::open_existing_current(&config.db_path)
    } else {
        RuntimeDb::open_for_explicit_history_reset(&config.db_path)
    }
    .with_context(|| format!("open runtime database {}", config.db_path.display()))
}

fn open_scheduler_db(path: &Path, dry_run: bool) -> Result<Option<ryeos_scheduler::SchedulerDb>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
                anyhow::bail!(
                    "scheduler database must be a regular non-symlink file: {}",
                    path.display()
                );
            }
            if dry_run {
                ryeos_scheduler::SchedulerDb::open_existing_current(path).map(Some)
            } else {
                ryeos_scheduler::SchedulerDb::open(path).map(Some)
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && dry_run => Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            ryeos_scheduler::SchedulerDb::open(path).map(Some)
        }
        Err(error) => Err(error.into()),
    }
}

fn inspect_operational_gc_roots(
    runtime_directory: &lillux::PinnedDirectory,
    runtime_directory_lock: &lillux::PinnedDirectoryLock,
    read_only: bool,
) -> Result<ryeos_state::gc::AdditionalCasRoots> {
    let operational = ryeos_state::OperationalDb::open_existing_current_with_namespace_authority(
        runtime_directory,
        runtime_directory_lock.clone(),
        read_only,
    )
    .context("open operational state before CAS sweep")?;
    let active_sync_jobs = operational
        .count_active_sync_jobs()
        .context("inspect active sync jobs before offline CAS sweep")?;
    if active_sync_jobs != 0 {
        anyhow::bail!(
            "offline CAS sweep refused: {active_sync_jobs} active sync job(s) may pin staged roots; rerun without --sweep-cas"
        );
    }
    let mirrored = operational
        .list_cas_entries_by_state(ryeos_state::CasEntryState::Mirrored)
        .context("collect durable mirrored CAS roots")?;
    let mut roots = ryeos_state::gc::AdditionalCasRoots::default();
    for entry in mirrored {
        match entry.entry_kind {
            ryeos_state::CasEntryKind::Object => roots.object_hashes.push(entry.hash),
            ryeos_state::CasEntryKind::Blob => roots.blob_hashes.push(entry.hash),
        }
    }
    Ok(roots)
}

fn discard_scheduler_fire_journals(
    runtime_directory: &lillux::PinnedDirectory,
    dry_run: bool,
) -> Result<usize> {
    let Some(schedules) =
        runtime_directory.open_child_directory(std::ffi::OsStr::new("schedules"))?
    else {
        return Ok(0);
    };
    let mut artifacts = 0usize;
    for schedule_name in schedules.entry_names()? {
        let schedule_id = schedule_name
            .to_str()
            .context("scheduler fire directory name is not UTF-8")?;
        ryeos_scheduler::crontab::validate_schedule_id(schedule_id)?;
        let schedule = schedules
            .open_child_directory(&schedule_name)?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "scheduler fire entry is not a directory: {}",
                    schedules.path().join(&schedule_name).display()
                )
            })?;
        for entry_name in schedule.entry_names()? {
            if !matches!(
                entry_name.to_str(),
                Some("fires.jsonl" | ".fires.jsonl.lock")
            ) {
                anyhow::bail!(
                    "unexpected scheduler fire-history entry: {}",
                    schedule.path().join(&entry_name).display()
                );
            }
            let file = schedule.open_regular(&entry_name, false)?.ok_or_else(|| {
                anyhow::anyhow!(
                    "scheduler fire-history entry is not a regular file: {}",
                    schedule.path().join(&entry_name).display()
                )
            })?;
            if !dry_run {
                schedule.remove_if_same(&entry_name, &file)?;
            }
            artifacts = artifacts
                .checked_add(1)
                .context("scheduler fire-history artifact count overflow")?;
        }
        if !dry_run && !schedules.remove_empty_child_if_same(&schedule_name, &schedule)? {
            anyhow::bail!(
                "scheduler fire directory changed during discard: {}",
                schedule.path().display()
            );
        }
        artifacts = artifacts
            .checked_add(1)
            .context("scheduler fire-history artifact count overflow")?;
    }
    Ok(artifacts)
}
