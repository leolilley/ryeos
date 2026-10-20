//! High-level state database wrapping CAS paths, the rebuildable projection,
//! stable operational state, and the head cache.
//!
//! The write-path contract is **journaled CAS-first**: once an authoritative
//! head is published, a projection failure leaves its exact transition pending
//! for the live repair worker and deterministic startup replay.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Context;

use crate::bundle_events::{
    self, BundleEventAppendRequest, BundleEventAppendResult, BundleEventChainPage,
    BundleEventCursor, BundleEventRecord, BundleEventScanPage,
};
use crate::chain::{self, AddThreadWithEventsResult, AppendResult, CreateResult, SnapshotUpdate};
use crate::head_cache::HeadCache;
use crate::objects::ThreadEvent;
use crate::objects::ThreadSnapshot;
use crate::operational::{
    AdmissionAttestationRecord, CasEntriesByStateSummary, CasEntryAttribution, CasEntryKind,
    CasEntryState, FinishSyncJobAttempt, NewAdmissionAttestationRecord, NewCasEntryAttribution,
    NewSyncJob, NewSyncJobAttempt, OperationalDb, SyncJobAttemptRecord, SyncJobRecord,
    SyncJobState, SyncJobUpdate,
};
use crate::projection::{
    self, ChainRetentionProjection, DueTerminalChain, DueTerminalChainCursor, ProjectionDb,
    project_committed_chain, project_initial_root_committed_chain,
};
use crate::queries;
use crate::recovery::{
    HeadOperation, PendingChainHeadTransition, ProjectionRecoveryGeneration, RecoveryStore,
    TransitionPhase,
};
use crate::refs::{GenericHeadRef, SignedRef, TrustStore};
use crate::signer::Signer;

pub(crate) mod live_native;
pub use live_native::require_exhausted_refusals;
pub use live_native::{AbandonedLiveOpenings, abandoned_live_openings};
pub use live_native::{
    LiveStateDatabaseOpenFailure, LiveStateStartupRecoveryFailure,
    ReleasedLiveStatePhysicalCustody, ReleasedLiveStateStartup,
    maximum_live_projection_physical_sources, quote_live_projection_physical,
    quote_live_projection_retired_sources, quote_live_projection_retry,
    quote_live_projection_validation, quote_live_state_destination_join,
    quote_live_state_finalization_root_receipts, quote_live_state_host_alias_retirement,
    quote_live_state_parent_root_membership, quote_live_state_physical_parent,
    quote_live_state_recovery_registration, quote_live_state_registered_root_validation,
    quote_live_state_root_registration_handoff, quote_maximum_state_destination_input_transfer,
    quote_released_live_state_physical_custody, quote_startup_closed_schema_sources,
    quote_startup_released_schema_source, quote_state_destination_input_transfer,
};
mod live_close;
pub use live_close::{
    ClosedLiveState, ClosedLiveStatePhysicalCustody, GenerationLeaseRetirement, LiveStateCensus,
    LiveStateCloseFailure, quote_closed_live_state_physical_custody,
    quote_live_projection_source_transfer, quote_live_schema_source_retirement,
    quote_normal_state_root_membership,
};
#[cfg(test)]
mod anchored_append_tests;
mod destination_join;
mod original_close;
#[cfg(test)]
mod rebuild_recovery_tests;
mod retirement_driver;
#[cfg(test)]
mod retirement_driver_tests;
pub(crate) use destination_join::OriginalStateDestinationJoin;
pub use destination_join::{
    maximum_state_destination_leaves, quote_generated_root_registration,
    quote_live_state_parent_transfer, quote_retired_generated_root_receipts,
    quote_state_destination_join, quote_state_leaf_admission, quote_state_leaf_continuations,
    quote_state_source_coalescing,
};
pub use original_close::{
    ClosedOriginalState, OriginalStateAcknowledgement, OriginalStateFinalAckAuthority,
    OriginalStateRetirementFailure, ReleasedOriginalStateRetirement, STATE_ACK_EFFECT_PREPAY,
    STATE_ACK_REPLAY_PREPAY,
};
pub use retirement_driver::{
    ChainRetirement, ChainRetirementReport, FencedRetirementReplay, RetirementEvidenceRef,
};

/// State of the rebuildable projection after an authoritative CAS write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectionStatus {
    Current,
    RepairRequired(ProjectionRepairRequest),
}

/// A request to repair a projection that fell behind an authoritative head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionRepairRequest {
    pub chain_root_id: String,
    pub committed_head_hash: String,
    pub operation: &'static str,
    pub error: String,
}

/// Receives repair requests after authoritative writes have committed.
///
/// Implementations must make `request_repair` non-blocking and infallible from
/// the caller's perspective. Durable queueing can be supplied by the daemon;
/// the state crate's default sink retains requests for the database lifetime.
pub trait ProjectionRepairSink: Send + Sync {
    fn request_repair(&self, request: ProjectionRepairRequest);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectionRecoveryStage {
    Opening,
    Rebuilding,
    ReplayingPending,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionRecoveryProgress {
    pub stage: ProjectionRecoveryStage,
    pub chains_total: Option<usize>,
    pub chains_done: usize,
    pub threads_restored: usize,
    pub events_projected: usize,
    pub pending_total: Option<usize>,
    pub pending_done: usize,
}

pub trait ProjectionRecoveryObserver: Send + Sync {
    fn update(&self, progress: ProjectionRecoveryProgress);

    fn is_cancelled(&self) -> bool {
        false
    }
}

struct CancellationOnlyRecoveryObserver<'a> {
    inner: &'a dyn ProjectionRecoveryObserver,
}

impl ProjectionRecoveryObserver for CancellationOnlyRecoveryObserver<'_> {
    fn update(&self, _progress: ProjectionRecoveryProgress) {}

    fn is_cancelled(&self) -> bool {
        self.inner.is_cancelled()
    }
}

/// Generic operational-state check used before discarding replaceable rows for
/// an unpublished, headless Set. Implementations must fail closed: unreadable
/// runtime state is an error, not evidence that recovery state is absent.
pub trait RuntimeLivenessInspector {
    fn chain_has_live_recovery_state(&self, chain_root_id: &str) -> anyhow::Result<bool>;
}

struct RuntimeStateNamespaceAuthority {
    directory: lillux::PinnedDirectory,
    lock: lillux::PinnedDirectoryLock,
}

#[derive(Debug, Default)]
struct PendingProjectionRepairs {
    requests: Mutex<Vec<ProjectionRepairRequest>>,
}

fn open_recovered_projection(
    runtime_state_dir: &Path,
    cas_root: &Path,
    refs_root: &Path,
    recovery: &RecoveryStore,
    trust_store: &TrustStore,
    runtime_liveness: Option<&dyn RuntimeLivenessInspector>,
    observer: Option<&dyn ProjectionRecoveryObserver>,
    original: &mut OriginalProjectionBuildContext,
) -> anyhow::Result<ProjectionDb> {
    notify_recovery_observer(
        observer,
        ProjectionRecoveryProgress {
            stage: ProjectionRecoveryStage::Opening,
            chains_total: None,
            chains_done: 0,
            threads_restored: 0,
            events_projected: 0,
            pending_total: None,
            pending_done: 0,
        },
    )?;
    let expected_epoch = ProjectionDb::current_schema_epoch();
    // A live context holds generation leases; its guarded reads run inside
    // the owning factory's native scope.
    let native_factory = Arc::clone(&original.factory);
    let generation = match recovery.read_generation_in(Some(&*native_factory)) {
        Ok(generation) => generation,
        Err(error) => {
            tracing::warn!(error = %error, "invalid projection recovery generation; rebuilding baseline");
            None
        }
    };

    if let Some(generation) = generation
        && generation.projection_schema_epoch == expected_epoch
    {
        let projection_path = runtime_state_dir.join(&generation.projection_file);
        // A refused native acquisition may still own partial effects. The
        // error retains it; recovery must settle that owner before replacement.
        let db = original.open_selected(
            recovery.runtime_directory(),
            std::ffi::OsStr::new(&generation.projection_file),
        )?;
        let matches = match db.projection_instance_id() {
            Ok(instance) => instance.as_deref() == Some(generation.projection_instance_id.as_str()),
            Err(error) => return Err(db.retain_operation_failure(error)),
        };
        if matches {
            let selected_work = (|| -> anyhow::Result<()> {
                replay_pending_into(
                    &db,
                    cas_root,
                    refs_root,
                    recovery,
                    trust_store,
                    runtime_liveness,
                    true,
                    observer,
                )?;
                let deleted = cleanup_superseded_projection_instances(
                    recovery,
                    Some(&*native_factory),
                    &projection_path,
                )?;
                if deleted != 0 {
                    tracing::info!(deleted, "removed superseded projection instances");
                }
                Ok(())
            })();
            if let Err(error) = selected_work {
                return Err(db.retain_operation_failure(error));
            }
            return Ok(db);
        }
        let closed = db
            .close_original(&original.factory)
            .map_err(anyhow::Error::new)?;
        // Cache the actual SQL terminal AND lease custody before validating
        // or choosing a replacement generation.
        let closed = Arc::new(closed);
        original.terminals.push(closed.native_proof());
        original.retired_selected.push(Arc::clone(&closed));
        closed.require_original(&original.factory)?;
        tracing::warn!(expected_instance = %generation.projection_instance_id,
            "projection instance does not match recovery generation; rebuilding baseline");
    }

    rebuild_and_install_projection(
        runtime_state_dir,
        cas_root,
        refs_root,
        recovery,
        trust_store,
        runtime_liveness,
        observer,
        None,
        original,
    )
    .map(|(projection, _)| projection)
}

// Recovery keeps each pinned/path authority and liveness observer explicit;
// combining them would make authority substitution easier to miss at callers.
#[allow(clippy::too_many_arguments)]
fn rebuild_and_install_projection(
    runtime_state_dir: &Path,
    cas_root: &Path,
    refs_root: &Path,
    recovery: &RecoveryStore,
    trust_store: &TrustStore,
    runtime_liveness: Option<&dyn RuntimeLivenessInspector>,
    observer: Option<&dyn ProjectionRecoveryObserver>,
    cas_mutation_guard: Option<&crate::recovery::CasMutationGuard>,
    original: &mut OriginalProjectionBuildContext,
) -> anyhow::Result<(ProjectionDb, crate::rebuild::RebuildReport)> {
    // A baseline observes every signed chain head and publishes a new selected
    // projection generation. Exclude every cross-process CAS/head publisher
    // for that complete observation-to-publication window.
    let owned_cas_mutation_guard;
    let active_cas_mutation_guard: &crate::recovery::CasMutationGuard = match cas_mutation_guard {
        Some(guard) => {
            if !guard.is_exclusive() {
                anyhow::bail!("projection rebuild requires an exclusive CAS mutation guard");
            }
            guard.ensure_protects_pinned_runtime(recovery.runtime_directory())?;
            guard
        }
        None => {
            owned_cas_mutation_guard =
                crate::recovery::CasMutationGuard::acquire_exclusive_in_pinned_runtime(
                    recovery.runtime_directory(),
                )?;
            &owned_cas_mutation_guard
        }
    };

    let native_factory = Arc::clone(&original.factory);
    let instance_id = recovery.new_projection_instance_id();
    let projection_file = format!("projection.{instance_id}.sqlite3");
    let instance_path = runtime_state_dir.join(&projection_file);
    let runtime_directory = recovery.runtime_directory();
    let staged = original.open(
        runtime_directory,
        std::ffi::OsStr::new(&projection_file),
        None,
    )?;
    let staged_work = (|| -> anyhow::Result<_> {
        staged.set_projection_instance_id(&instance_id)?;
        let refs_directory = runtime_directory
            .open_child_directory(std::ffi::OsStr::new("refs"))?
            .ok_or_else(|| anyhow::anyhow!("signed refs directory is absent during rebuild"))?;
        let cas_directory = runtime_directory
            .open_child_directory(std::ffi::OsStr::new("objects"))?
            .ok_or_else(|| anyhow::anyhow!("CAS objects directory is absent during rebuild"))?;
        let cas = lillux::CasStore::from_pinned_root(cas_directory.try_clone()?);
        let (report, verified_baseline) =
            crate::rebuild::rebuild_projection_with_verified_baseline_pinned(
                &staged,
                &cas,
                &refs_directory,
                trust_store,
                observer,
            )
            .context("build current projection baseline")?;

        // Fold transitions observed during the baseline into the staged database,
        // but never acknowledge them until this exact database is installed.
        let staging_observer = observer.map(|inner| CancellationOnlyRecoveryObserver { inner });
        replay_pending_into(
            &staged,
            cas_root,
            refs_root,
            recovery,
            trust_store,
            runtime_liveness,
            false,
            staging_observer
                .as_ref()
                .map(|observer| observer as &dyn ProjectionRecoveryObserver),
        )?;
        let (_, staged_fingerprints) = crate::rebuild::verify_preverified_projection_pinned(
            &staged,
            &cas,
            &refs_directory,
            trust_store,
            &verified_baseline,
            active_cas_mutation_guard,
            runtime_directory,
            observer,
            Some(&mut *original),
        )
        .context("verify staged projection baseline before publication")?;
        Ok((report, staged_fingerprints))
    })();
    let (report, staged_fingerprints) = match staged_work {
        Ok(result) => result,
        Err(error) => return Err(staged.retain_operation_failure(error)),
    };
    let staged_native = staged.close_original_generated(&original.factory)?;
    // Retain genuine terminal and durability owners BEFORE any late check.
    original.terminals.push(staged_native.closed());
    original.durable.push(staged_native.native_proof());
    let retained_projection_file = projection_file.clone();
    let publication = (|| -> anyhow::Result<_> {
        // Cancellation after the final checkpoint must leave this instance as an
        // unselected orphan. Never publish a new generation after shutdown won the
        // race while SQLite was checkpointing or fsyncing.
        ensure_recovery_not_cancelled(observer)?;

        let generation = ProjectionRecoveryGeneration::new(
            ProjectionDb::current_schema_epoch(),
            instance_id.clone(),
            projection_file,
        )?;
        // generation.json is the sole selected-instance pointer and is published
        // only after this generation instance has closed and been fsynced.
        staged_native.require_original(&original.factory)?;
        staged_native.require_coordinate(
            runtime_directory,
            std::ffi::OsStr::new(&generation.projection_file),
        )?;
        recovery.write_generation(&generation)?;

        let projection = original
            .open(
                runtime_directory,
                std::ffi::OsStr::new(&generation.projection_file),
                Some(&staged_native),
            )
            .context("open selected projection instance")?;
        let selected_work = (|| -> anyhow::Result<()> {
            if projection.projection_instance_id()?.as_deref() != Some(instance_id.as_str()) {
                anyhow::bail!("installed projection instance identity mismatch");
            }
            crate::rebuild::verify_projection_matches_fingerprints_with_observer(
                &projection,
                &staged_fingerprints,
                observer,
            )
            .context("verify selected projection bytes before journal acknowledgement")?;
            replay_pending_into(
                &projection,
                cas_root,
                refs_root,
                recovery,
                trust_store,
                runtime_liveness,
                true,
                observer,
            )?;
            let deleted = cleanup_superseded_projection_instances(
                recovery,
                Some(&*native_factory),
                &instance_path,
            )?;
            if deleted != 0 {
                tracing::info!(deleted, "removed superseded projection instances");
            }
            Ok(())
        })();
        if let Err(error) = selected_work {
            // generation.json already committed this exact selected instance.
            // Retain its real owner; do not roll back or publish a replacement.
            return Err(projection.retain_operation_failure(error));
        }
        tracing::info!(
            path = %instance_path.display(),
            projection_instance_id = %instance_id,
            chains_rebuilt = report.chains_rebuilt,
            threads_restored = report.threads_restored,
            events_projected = report.events_projected,
            "installed current projection baseline"
        );
        Ok((projection, report))
    })();
    match publication {
        Ok(result) => Ok(result),
        Err(error) => Err(anyhow::Error::new(GeneratedProjectionPublicationFailure {
            error,
            _staged: staged_native,
            _recovery: recovery.clone(),
            _instance_id: instance_id,
            _projection_file: retained_projection_file,
        })),
    }
}

const SUPERSEDED_PROJECTION_INSTANCES_TO_KEEP: usize = 2;
const SUPERSEDED_PROJECTION_DELETIONS_PER_PASS: usize = 4;
const SUPERSEDED_PROJECTION_INSPECTIONS_PER_PASS: usize = 16;
const SUPERSEDED_PROJECTION_DIRECTORY_ENTRIES_PER_PASS: usize = 64;

fn cleanup_superseded_projection_instances(
    recovery: &RecoveryStore,
    native_factory: Option<&lillux_sqlite::OriginalNativeFactory>,
    selected_path: &Path,
) -> anyhow::Result<usize> {
    cleanup_projection_instances(
        recovery,
        native_factory,
        selected_path,
        SUPERSEDED_PROJECTION_INSTANCES_TO_KEEP,
        Some(SUPERSEDED_PROJECTION_DIRECTORY_ENTRIES_PER_PASS),
        Some(SUPERSEDED_PROJECTION_INSPECTIONS_PER_PASS),
        Some(SUPERSEDED_PROJECTION_DELETIONS_PER_PASS),
        false,
    )
}

/// Explicit offline history discard has no rollback generation to retain: the
/// operator has authorized deletion of all prior thread projections. Unlike
/// ordinary bounded housekeeping, this pass must either remove every
/// unselected generation or fail while the durable discard marker remains.
fn cleanup_all_superseded_projection_instances(
    recovery: &RecoveryStore,
    native_factory: Option<&lillux_sqlite::OriginalNativeFactory>,
    selected_path: &Path,
) -> anyhow::Result<usize> {
    cleanup_projection_instances(
        recovery,
        native_factory,
        selected_path,
        0,
        None,
        None,
        None,
        true,
    )
}

fn ensure_existing_projection_cleanup_lease_available(
    directory: &lillux::PinnedDirectory,
    projection_name: &str,
) -> anyhow::Result<()> {
    let lease_name = std::ffi::OsString::from(format!("{projection_name}.lease"));
    let Some(lease) = directory.open_regular(&lease_name, true)? else {
        return Ok(());
    };
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        if unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::WouldBlock {
                anyhow::bail!(
                    "thread-history discard cannot remove projection still leased by another process: {}",
                    directory.path().join(projection_name).display()
                );
            }
            return Err(error).with_context(|| {
                format!(
                    "inspect projection cleanup lease {}",
                    directory.path().join(lease_name).display()
                )
            });
        }
    }
    #[cfg(not(unix))]
    {
        let _ = lease;
        anyhow::bail!("projection instance cleanup leasing is unavailable on this platform");
    }
    Ok(())
}

/// Validate every pre-existing projection store before the discard marker is
/// published. The new empty generation created later is not part of this
/// count, so the result is also the number of old projection stores retired.
fn inspect_projection_history_discard(recovery: &RecoveryStore) -> anyhow::Result<usize> {
    let runtime_directory = recovery.runtime_directory().try_clone()?;
    let mut instances = 0usize;
    for entry_name in runtime_directory.entry_names()? {
        let Some(name) = entry_name.to_str() else {
            continue;
        };
        let Some(instance_id) = name
            .strip_prefix("projection.")
            .and_then(|name| name.strip_suffix(".sqlite3"))
        else {
            continue;
        };
        if instance_id.is_empty()
            || !instance_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            continue;
        }
        runtime_directory
            .open_regular(&entry_name, false)?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "projection discard candidate is not a regular file: {}",
                    runtime_directory.path().join(&entry_name).display()
                )
            })?;
        inspect_projection_sidecars(&runtime_directory, name)?;
        ensure_existing_projection_cleanup_lease_available(&runtime_directory, name)?;
        instances = instances
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("projection discard count overflow"))?;
    }
    Ok(instances)
}

// Bounded housekeeping knobs plus the guarded-read owner stay explicit here.
#[allow(clippy::too_many_arguments)]
fn cleanup_projection_instances(
    recovery: &RecoveryStore,
    native_factory: Option<&lillux_sqlite::OriginalNativeFactory>,
    selected_path: &Path,
    instances_to_keep: usize,
    directory_entry_limit: Option<usize>,
    inspection_limit: Option<usize>,
    deletion_limit: Option<usize>,
    require_complete: bool,
) -> anyhow::Result<usize> {
    let selected_name = selected_path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("selected projection path has no filename"))?;
    let generation_file = recovery
        .read_generation_in(native_factory)?
        .map(|generation| generation.projection_file);
    let runtime_directory = recovery.runtime_directory().try_clone()?;
    let mut candidates = Vec::new();
    let entry_names = match directory_entry_limit {
        Some(limit) => runtime_directory.entry_names_bounded(limit)?,
        None => runtime_directory.entry_names()?,
    };
    for entry_name in entry_names {
        let name = match entry_name.clone().into_string() {
            Ok(name) => name,
            Err(_) => continue,
        };
        if std::ffi::OsStr::new(&name) == selected_name
            || generation_file.as_deref() == Some(name.as_str())
        {
            continue;
        }
        let Some(instance_id) = name
            .strip_prefix("projection.")
            .and_then(|name| name.strip_suffix(".sqlite3"))
        else {
            continue;
        };
        if instance_id.is_empty()
            || !instance_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            continue;
        }
        let path = runtime_directory.path().join(&entry_name);
        let candidate_file = open_projection_cleanup_metadata(&runtime_directory, &entry_name)?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "projection cleanup candidate is not a regular file: {}",
                    path.display()
                )
            })?;
        let modified = candidate_file
            .metadata()?
            .modified()
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        candidates.push((modified, name, entry_name, candidate_file));
        // Cap candidate discovery itself. Applying the cap only after this
        // loop would make every normal open stat and sort the full orphan set.
        if inspection_limit.is_some_and(|limit| candidates.len() >= limit) {
            break;
        }
    }
    candidates.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| right.1.cmp(&left.1)));

    let mut deleted = 0usize;
    for (_, name, entry_name, instance_file) in candidates.into_iter().skip(instances_to_keep) {
        if deletion_limit.is_some_and(|limit| deleted == limit) {
            break;
        }
        let Some((lease_name, instance_lease)) =
            try_acquire_projection_cleanup_lease(&runtime_directory, &name)?
        else {
            if require_complete {
                anyhow::bail!(
                    "thread-history discard cannot remove projection still leased by another process: {}",
                    runtime_directory.path().join(&entry_name).display()
                );
            }
            // An old process may still be reading this generation instance.
            // Its shared lease keeps both the database and sidecars intact.
            continue;
        };
        remove_projection_sidecars(&runtime_directory, &name)?;
        runtime_directory
            .remove_if_same(&entry_name, &instance_file)
            .with_context(|| format!("remove superseded projection {name}"))?;
        runtime_directory
            .remove_if_same(&lease_name, &instance_lease)
            .with_context(|| format!("remove superseded projection lease {name}.lease"))?;
        drop(instance_lease);
        deleted += 1;
    }
    Ok(deleted)
}

fn remove_pinned_directory_contents(
    directory: &lillux::PinnedDirectory,
    dry_run: bool,
) -> anyhow::Result<usize> {
    let mut removed = 0usize;
    for name in directory.entry_names()? {
        removed = removed
            .checked_add(remove_pinned_directory_entry(directory, &name, dry_run)?)
            .ok_or_else(|| anyhow::anyhow!("thread-history artifact count overflow"))?;
    }
    Ok(removed)
}

fn remove_pinned_directory_entry(
    directory: &lillux::PinnedDirectory,
    name: &std::ffi::OsStr,
    dry_run: bool,
) -> anyhow::Result<usize> {
    match directory.open_entry(name, false)? {
        Some(lillux::PinnedDirectoryEntry::Directory(child)) => {
            let removed = remove_pinned_directory_contents(&child, dry_run)?
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("thread-history artifact count overflow"))?;
            if !dry_run
                && !directory
                    .remove_empty_child_if_same(name, &child)
                    .map(|receipt| receipt.is_some())?
            {
                anyhow::bail!(
                    "thread-history directory changed during discard: {}",
                    child.path().display()
                );
            }
            Ok(removed)
        }
        Some(lillux::PinnedDirectoryEntry::Regular(file)) => {
            if !dry_run {
                directory.remove_if_same(name, &file)?;
            }
            Ok(1)
        }
        None => anyhow::bail!(
            "thread-history entry disappeared: {}",
            directory.path().join(name).display()
        ),
    }
}

fn remove_pinned_directory_contents_with_progress(
    directory: &lillux::PinnedDirectory,
    dry_run: bool,
    expected_heads: usize,
    progress: &mut dyn FnMut(usize, usize),
) -> anyhow::Result<usize> {
    if dry_run {
        return remove_pinned_directory_contents(directory, true);
    }
    let observed_heads = count_pinned_files_named(directory, std::ffi::OsStr::new("head"))?;
    if observed_heads != expected_heads {
        anyhow::bail!(
            "verified chain-head count changed before discard: expected {expected_heads}, observed {observed_heads}"
        );
    }
    progress(0, expected_heads);
    let mut retired_heads = 0usize;
    let removed = remove_pinned_directory_contents_tracking_heads(
        directory,
        &mut retired_heads,
        expected_heads,
        progress,
    )?;
    debug_assert_eq!(retired_heads, expected_heads);
    Ok(removed)
}

fn count_pinned_files_named(
    directory: &lillux::PinnedDirectory,
    target: &std::ffi::OsStr,
) -> anyhow::Result<usize> {
    let mut count = 0usize;
    for name in directory.entry_names()? {
        let entry = directory.open_entry(&name, false)?;
        match entry {
            Some(lillux::PinnedDirectoryEntry::Directory(child)) => {
                count = count
                    .checked_add(count_pinned_files_named(&child, target)?)
                    .ok_or_else(|| anyhow::anyhow!("thread-history head count overflow"))?;
            }
            Some(lillux::PinnedDirectoryEntry::Regular(_)) if name == target => {
                count = count
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("thread-history head count overflow"))?;
            }
            Some(lillux::PinnedDirectoryEntry::Regular(_)) => {}
            None => anyhow::bail!(
                "thread-history entry disappeared: {}",
                directory.path().join(&name).display()
            ),
        }
    }
    Ok(count)
}

fn remove_pinned_directory_contents_tracking_heads(
    directory: &lillux::PinnedDirectory,
    retired_heads: &mut usize,
    total_heads: usize,
    progress: &mut dyn FnMut(usize, usize),
) -> anyhow::Result<usize> {
    let mut removed = 0usize;
    for name in directory.entry_names()? {
        let entry = directory.open_entry(&name, false)?;
        match entry {
            Some(lillux::PinnedDirectoryEntry::Directory(child)) => {
                removed = removed
                    .checked_add(remove_pinned_directory_contents_tracking_heads(
                        &child,
                        retired_heads,
                        total_heads,
                        progress,
                    )?)
                    .and_then(|value| value.checked_add(1))
                    .ok_or_else(|| anyhow::anyhow!("thread-history artifact count overflow"))?;
                if !directory
                    .remove_empty_child_if_same(&name, &child)
                    .map(|receipt| receipt.is_some())?
                {
                    anyhow::bail!(
                        "thread-history directory changed during discard: {}",
                        child.path().display()
                    );
                }
            }
            Some(lillux::PinnedDirectoryEntry::Regular(file)) => {
                directory.remove_if_same(&name, &file)?;
                removed = removed
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("thread-history artifact count overflow"))?;
                if name == std::ffi::OsStr::new("head") {
                    *retired_heads = retired_heads
                        .checked_add(1)
                        .ok_or_else(|| anyhow::anyhow!("thread-history head count overflow"))?;
                    progress(*retired_heads, total_heads);
                }
            }
            None => anyhow::bail!(
                "thread-history entry disappeared: {}",
                directory.path().join(&name).display()
            ),
        }
    }
    Ok(removed)
}

fn try_acquire_projection_cleanup_lease(
    directory: &lillux::PinnedDirectory,
    projection_name: &str,
) -> anyhow::Result<Option<(std::ffi::OsString, fs::File)>> {
    let lease_name = std::ffi::OsString::from(format!("{projection_name}.lease"));
    let lease = directory.open_regular_create(&lease_name, true, false, 0o600)?;
    directory.sync()?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        if unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::WouldBlock {
                return Ok(None);
            }
            return Err(error).with_context(|| {
                format!(
                    "acquire exclusive projection cleanup lease {}",
                    directory.path().join(&lease_name).display()
                )
            });
        }
    }
    #[cfg(not(unix))]
    anyhow::bail!("projection instance cleanup leasing is unavailable on this platform");
    Ok(Some((lease_name, lease)))
}

// SQLite data descriptors remain with their native owner. Discovery and
// identity comparison need only a metadata pin, including during cleanup.
fn open_projection_cleanup_metadata(
    directory: &lillux::PinnedDirectory,
    name: &std::ffi::OsStr,
) -> anyhow::Result<Option<fs::File>> {
    let Some(file) = directory.open_mount_entry(name)? else {
        return Ok(None);
    };
    anyhow::ensure!(
        file.metadata()?.is_file(),
        "projection cleanup entry is not regular"
    );
    directory.ensure_open_regular_same_mount(&file)?;
    Ok(Some(file))
}

fn remove_projection_sidecars(
    directory: &lillux::PinnedDirectory,
    projection_name: &str,
) -> anyhow::Result<()> {
    let existing = inspect_projection_sidecars(directory, projection_name)?;
    // Preflight the complete set before unlinking any member, so one invalid
    // sidecar cannot leave a partially cleaned generation.
    for (sidecar, file) in &existing {
        let name = sidecar
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("projection sidecar has no filename"))?;
        let current = open_projection_cleanup_metadata(directory, name)?
            .ok_or_else(|| anyhow::anyhow!("projection sidecar disappeared"))?;
        ensure_same_projection_cleanup_file(file, &current, sidecar)?;
    }
    for (sidecar, file) in existing {
        let name = sidecar
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("projection sidecar has no filename"))?;
        directory
            .remove_if_same(name, &file)
            .with_context(|| format!("remove projection sidecar {}", sidecar.display()))?;
    }
    Ok(())
}

fn inspect_projection_sidecars(
    directory: &lillux::PinnedDirectory,
    projection_name: &str,
) -> anyhow::Result<Vec<(PathBuf, fs::File)>> {
    let sidecars = [
        std::ffi::OsString::from(format!("{projection_name}-wal")),
        std::ffi::OsString::from(format!("{projection_name}-shm")),
        std::ffi::OsString::from(format!("{projection_name}-journal")),
    ];
    let mut existing = Vec::new();
    for name in sidecars {
        if let Some(file) = open_projection_cleanup_metadata(directory, &name)? {
            let sidecar = directory.path().join(&name);
            existing.push((sidecar, file));
        }
    }
    Ok(existing)
}

fn ensure_same_projection_cleanup_file(
    expected: &fs::File,
    current: &fs::File,
    path: &Path,
) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let expected = expected.metadata()?;
        let current = current.metadata()?;
        if expected.dev() != current.dev() || expected.ino() != current.ino() {
            anyhow::bail!("projection cleanup file changed: {}", path.display());
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (expected, current, path);
        anyhow::bail!("projection cleanup file identity is unavailable on this platform")
    }
}

#[derive(Debug, Clone, Default)]
pub struct PendingReplayReport {
    pub records_checked: usize,
    pub sets_repaired: usize,
    pub stale_sets_cleared: usize,
    pub stale_removes_cleared: usize,
    pub removals_awaiting_runtime_cleanup: usize,
}

/// State-plane portion of an explicit offline all-thread-history discard.
/// Runtime-process rows, per-thread files, and scheduler fire history are
/// owned by higher layers and reported separately by the orchestrator.
#[derive(Debug, Clone, Default)]
pub struct AuthoritativeThreadHistoryDiscardReport {
    pub chain_heads: usize,
    pub chain_ref_artifacts: usize,
    pub pending_transitions: usize,
    pub retirement_fences: usize,
    pub evidence_obligations: usize,
    pub superseded_projection_instances: usize,
    pub rebuilt_projection: Option<crate::rebuild::RebuildReport>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthoritativeThreadHistoryDiscardProgress {
    RetiringChainHeads { completed: usize, total: usize },
    RebuildingProjection,
}

/// Exact signed-head state observed while replaying one pending Remove.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingRemoveHeadState {
    HeadAbsent,
    ExpectedHeadVisible,
    AdvancedHeadVisible { current_head_hash: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthoritativeTerminalChain {
    pub chain_root_id: String,
    pub head_hash: String,
    pub root_policy: crate::objects::CapturedThreadHistoryPolicy,
    pub terminal_at: String,
    pub retire_after: Option<i64>,
    pub thread_ids: Vec<String>,
}

enum ProspectiveChainMutation<'a> {
    AddSnapshot(&'a ThreadSnapshot),
    AddContinuation {
        snapshot: &'a ThreadSnapshot,
        source_thread_id: &'a str,
    },
    UpdateSnapshots(&'a [SnapshotUpdate]),
    ReplaceHead(&'a str),
}

struct CancelledPreparedRemove {
    expected_previous_hash: String,
}

impl AuthoritativeTerminalChain {
    /// Compare `now` to the deadline derived by the same helper used to build
    /// projection retention rows. An explicitly captured Durable policy is
    /// never due.
    pub fn is_due_at(&self, now: &str) -> anyhow::Result<bool> {
        let Some(retire_after) = self.retire_after else {
            return Ok(false);
        };
        let now = crate::objects::parse_canonical_timestamp(now)
            .with_context(|| format!("invalid canonical retention current time `{now}`"))?
            .timestamp();
        Ok(now >= retire_after)
    }
}

fn prospective_mutation_invalidates_retirement(
    cas: &lillux::CasStore,
    chain_root_id: &str,
    current_head_hash: &str,
    mutation: ProspectiveChainMutation<'_>,
) -> anyhow::Result<bool> {
    let mutation = match mutation {
        ProspectiveChainMutation::ReplaceHead(target_hash) => {
            return match load_terminal_chain_from_cas(cas, chain_root_id, target_hash)? {
                Some(chain) => Ok(!chain.is_due_at(&lillux::time::iso8601_now())?),
                None => Ok(true),
            };
        }
        mutation => mutation,
    };

    // A destructive retention decision may only be derived from a complete,
    // coherent authoritative history. Object-local validation cannot detect
    // a forked event chain, a sequence gap, or a snapshot/event mismatch.
    crate::chain::validate_authoritative_history_with_cas(
        cas,
        chain_root_id,
        current_head_hash,
        None,
    )?;

    let chain_state: crate::ChainState = load_hashed_cas_object(cas, current_head_hash)?;
    chain_state.validate()?;
    if chain_state.chain_root_id != chain_root_id {
        anyhow::bail!(
            "authoritative chain root mismatch: expected {chain_root_id}, got {}",
            chain_state.chain_root_id
        );
    }

    // Removal admission is rare and already serialized by the chain lock, so
    // load the whole signed closure and derive the exact post-mutation policy
    // state. This avoids guessing from event or execution kinds.
    let mut snapshots = BTreeMap::new();
    for (thread_id, entry) in &chain_state.threads {
        let snapshot: ThreadSnapshot = load_hashed_cas_object(cas, &entry.snapshot_hash)?;
        snapshot.validate()?;
        if snapshot.thread_id != *thread_id || snapshot.chain_root_id != chain_root_id {
            anyhow::bail!("authoritative snapshot identity mismatch for {thread_id}");
        }
        if snapshot.status != entry.status {
            anyhow::bail!("chain/snapshot status mismatch for {thread_id}");
        }
        snapshots.insert(thread_id.clone(), snapshot);
    }
    let timestamp_floor =
        crate::chain::prospective_mutation_timestamp_floor(&chain_state.updated_at)?;

    match mutation {
        ProspectiveChainMutation::AddSnapshot(snapshot) => {
            let mut snapshot = snapshot.clone();
            crate::chain::normalize_prospective_new_thread(
                &mut snapshot,
                chain_root_id,
                &timestamp_floor,
                None,
            )?;
            if snapshots
                .insert(snapshot.thread_id.clone(), snapshot.clone())
                .is_some()
            {
                anyhow::bail!("thread {} already exists in chain", snapshot.thread_id);
            }
        }
        ProspectiveChainMutation::AddContinuation {
            snapshot,
            source_thread_id,
        } => {
            if !snapshots.contains_key(source_thread_id) {
                anyhow::bail!(
                    "continuation source {source_thread_id} does not exist in chain {chain_root_id}"
                );
            }
            let mut snapshot = (*snapshot).clone();
            crate::chain::normalize_prospective_new_thread(
                &mut snapshot,
                chain_root_id,
                &timestamp_floor,
                Some(source_thread_id),
            )?;
            if snapshots
                .insert(snapshot.thread_id.clone(), snapshot.clone())
                .is_some()
            {
                anyhow::bail!("thread {} already exists in chain", snapshot.thread_id);
            }
        }
        ProspectiveChainMutation::UpdateSnapshots(updates) => {
            let mut updated = std::collections::BTreeSet::new();
            for update in updates {
                if update.thread_id != update.new_snapshot.thread_id {
                    anyhow::bail!(
                        "snapshot update ID mismatch: {} != {}",
                        update.thread_id,
                        update.new_snapshot.thread_id
                    );
                }
                if update.new_snapshot.chain_root_id != chain_root_id {
                    anyhow::bail!(
                        "updated snapshot {} belongs to chain {}, not {chain_root_id}",
                        update.thread_id,
                        update.new_snapshot.chain_root_id
                    );
                }
                if !updated.insert(update.thread_id.as_str()) {
                    anyhow::bail!("duplicate snapshot update for {}", update.thread_id);
                }
                let existing = snapshots.get_mut(&update.thread_id).ok_or_else(|| {
                    anyhow::anyhow!(
                        "snapshot update thread {} is absent from chain {chain_root_id}",
                        update.thread_id
                    )
                })?;
                let mut next = update.new_snapshot.clone();
                crate::chain::normalize_prospective_snapshot_transition(
                    existing,
                    &mut next,
                    &timestamp_floor,
                )?;
                *existing = next;
            }
        }
        ProspectiveChainMutation::ReplaceHead(_) => unreachable!(),
    }

    let mut root_policy = None;
    let mut terminal_members = Vec::with_capacity(snapshots.len());
    let mut thread_ids = Vec::with_capacity(snapshots.len());
    for (thread_id, snapshot) in snapshots {
        if thread_id == chain_root_id {
            root_policy = snapshot.captured_history_policy.clone();
        }
        terminal_members.push(crate::projection::TerminalMember {
            is_terminal: snapshot.status.is_terminal(),
            timestamp: snapshot
                .finished_at
                .as_ref()
                .unwrap_or(&snapshot.updated_at)
                .clone(),
        });
        thread_ids.push(thread_id);
    }
    let root_policy = root_policy.ok_or_else(|| {
        anyhow::anyhow!(
            "authoritative chain {chain_root_id} has no root member with captured history policy"
        )
    })?;
    let derived =
        crate::projection::derive_terminal_retention(terminal_members, &root_policy.retention)?;
    let Some(terminal_at) = derived.terminal_at else {
        return Ok(true);
    };
    let prospective = AuthoritativeTerminalChain {
        chain_root_id: chain_root_id.to_string(),
        head_hash: current_head_hash.to_string(),
        root_policy,
        terminal_at,
        retire_after: derived.retire_after,
        thread_ids,
    };
    Ok(!prospective.is_due_at(&lillux::time::iso8601_now())?)
}

/// Proof that a chain may accept a new runtime recovery pin. The chain lock is
/// retained by this value so callers can keep admission and pin creation in
/// one critical section while already holding the StateStore mutex.
pub struct AuthoritativeRuntimePinAdmission {
    pub chain_root_id: String,
    pub head_hash: String,
    _chain_locks: Vec<crate::chain::ChainLock>,
}

struct RecoveryObserverThrottle {
    next_update: std::time::Instant,
}

impl RecoveryObserverThrottle {
    fn new() -> Self {
        Self {
            next_update: std::time::Instant::now() + std::time::Duration::from_millis(250),
        }
    }

    fn maybe_notify(
        &mut self,
        observer: Option<&dyn ProjectionRecoveryObserver>,
        progress: ProjectionRecoveryProgress,
    ) {
        let Some(observer) = observer else {
            return;
        };
        let now = std::time::Instant::now();
        if now < self.next_update {
            return;
        }
        observer.update(progress);
        self.next_update = now + std::time::Duration::from_millis(250);
    }
}

// Replay exposes acknowledgment, liveness, trust, and recovery authority as
// independent inputs because each changes the safety of the operation.
#[allow(clippy::too_many_arguments)]
fn replay_pending_into(
    projection: &ProjectionDb,
    cas_root: &Path,
    refs_root: &Path,
    recovery: &RecoveryStore,
    trust_store: &TrustStore,
    runtime_liveness: Option<&dyn RuntimeLivenessInspector>,
    acknowledge: bool,
    observer: Option<&dyn ProjectionRecoveryObserver>,
) -> anyhow::Result<PendingReplayReport> {
    let expected_refs_root = recovery.runtime_directory().path().join("refs");
    if expected_refs_root != refs_root {
        anyhow::bail!(
            "projection replay refs root does not match pinned recovery authority: expected {}, got {}",
            expected_refs_root.display(),
            refs_root.display()
        );
    }
    let refs_directory = recovery
        .runtime_directory()
        .open_child_directory(std::ffi::OsStr::new("refs"))?
        .ok_or_else(|| anyhow::anyhow!("signed refs directory is absent during replay"))?;
    let expected_cas_root = recovery.runtime_directory().path().join("objects");
    if expected_cas_root != cas_root {
        anyhow::bail!(
            "projection replay CAS root does not match pinned recovery authority: expected {}, got {}",
            expected_cas_root.display(),
            cas_root.display()
        );
    }
    let cas_directory = recovery
        .runtime_directory()
        .open_child_directory(std::ffi::OsStr::new("objects"))?
        .ok_or_else(|| anyhow::anyhow!("CAS objects directory is absent during replay"))?;
    let cas = lillux::CasStore::from_pinned_root(cas_directory.try_clone()?);
    let mut report = PendingReplayReport::default();
    let mut observer_progress = RecoveryObserverThrottle::new();
    let mut records =
        recovery.pending_cursor_checked(None, || ensure_recovery_not_cancelled(observer))?;
    let pending_total = records.entry_count();
    notify_recovery_observer(
        observer,
        ProjectionRecoveryProgress {
            stage: ProjectionRecoveryStage::ReplayingPending,
            chains_total: None,
            chains_done: 0,
            threads_restored: 0,
            events_projected: 0,
            pending_total: Some(pending_total),
            pending_done: 0,
        },
    )?;
    while let Some(observed) =
        records.next_transition_checked(|| ensure_recovery_not_cancelled(observer))?
    {
        ensure_recovery_not_cancelled(observer)?;
        report.records_checked += 1;
        let chain_lock = if acknowledge {
            crate::chain::ChainLock::acquire_in_refs_directory(
                &refs_directory,
                &cas_directory,
                recovery,
                &observed.chain_root_id,
            )?
        } else {
            crate::chain::ChainLock::acquire_existing_in_refs_directory(
                &refs_directory,
                &cas_directory,
                recovery,
                &observed.chain_root_id,
            )?
        };
        let current = match recovery.read_pending(&observed.chain_root_id)? {
            Some(current) if current.transition_id == observed.transition_id => current,
            _ => {
                observer_progress.maybe_notify(
                    observer,
                    ProjectionRecoveryProgress {
                        stage: ProjectionRecoveryStage::ReplayingPending,
                        chains_total: None,
                        chains_done: 0,
                        threads_restored: 0,
                        events_projected: 0,
                        pending_total: Some(pending_total),
                        pending_done: report.records_checked,
                    },
                );
                continue;
            }
        };
        let head = chain_lock.read_verified_head(trust_store)?;
        match current.operation {
            HeadOperation::Set => match classify_pending_set_head(&current, head.as_ref())? {
                PendingSetHead::Published(head) => {
                    let current = advance_set_to_applying(recovery, &chain_lock, current)?;
                    crate::rebuild::repair_one_chain_verified_with_cas_and_observer(
                        projection,
                        &cas,
                        &current.chain_root_id,
                        &head.target_hash,
                        observer,
                    )?;
                    if acknowledge
                        && !acknowledge_if_projection_current(
                            projection,
                            recovery,
                            &chain_lock,
                            &current,
                            Some(head),
                        )?
                    {
                        anyhow::bail!(
                            "pending Set did not converge while replaying {}",
                            current.chain_root_id
                        );
                    }
                    report.sets_repaired += 1;
                }
                PendingSetHead::Unpublished(Some(previous)) => {
                    crate::rebuild::repair_one_chain_verified_with_cas_and_observer(
                        projection,
                        &cas,
                        &current.chain_root_id,
                        &previous.target_hash,
                        observer,
                    )?;
                    if acknowledge {
                        acknowledge_unpublished_set(
                            projection,
                            recovery,
                            &chain_lock,
                            &current,
                            Some(previous),
                        )?;
                    }
                    report.stale_sets_cleared += 1;
                }
                PendingSetHead::Unpublished(None) => {
                    let runtime_liveness = runtime_liveness.ok_or_else(|| {
                        anyhow::anyhow!(
                            "headless pending Set for {} requires runtime-liveness inspection",
                            current.chain_root_id
                        )
                    })?;
                    if runtime_liveness
                        .chain_has_live_recovery_state(&current.chain_root_id)
                        .with_context(|| {
                            format!(
                                "inspect runtime liveness for headless pending Set {}",
                                current.chain_root_id
                            )
                        })?
                    {
                        anyhow::bail!(
                            "headless pending Set for {} still has live recovery state",
                            current.chain_root_id
                        );
                    }
                    // The signed head is absent, so every chain-scoped row is
                    // replaceable. Deletion is idempotent and happens only
                    // after the generic runtime inspector proved no live pin.
                    projection.delete_chain_projection(&current.chain_root_id)?;
                    if acknowledge {
                        acknowledge_unpublished_set(
                            projection,
                            recovery,
                            &chain_lock,
                            &current,
                            None,
                        )?;
                    }
                    report.stale_sets_cleared += 1;
                }
            },
            HeadOperation::Remove => match head {
                None => {
                    let mut current = current;
                    if current.phase == TransitionPhase::Prepared {
                        current = recovery.advance_phase(
                            &chain_lock,
                            &current.chain_root_id,
                            &current.transition_id,
                            TransitionPhase::Prepared,
                            TransitionPhase::HeadPublished,
                        )?;
                    }
                    if current.phase == TransitionPhase::HeadPublished {
                        current = recovery.advance_phase(
                            &chain_lock,
                            &current.chain_root_id,
                            &current.transition_id,
                            TransitionPhase::HeadPublished,
                            TransitionPhase::ApplyingProjection,
                        )?;
                    }
                    projection.delete_chain_projection(&current.chain_root_id)?;
                    // Runtime DB/files are owned by the daemon StateStore. Keep
                    // Remove pending until it explicitly confirms that cleanup.
                    report.removals_awaiting_runtime_cleanup += 1;
                }
                Some(head)
                    if current.expected_previous_hash.as_deref()
                        == Some(head.target_hash.as_str()) =>
                {
                    if current.phase != TransitionPhase::Prepared {
                        anyhow::bail!(
                            "pending Remove for {} says head was published but expected head still exists",
                            current.chain_root_id
                        );
                    }
                    // Removal did not cross the authoritative boundary. Policy
                    // and runtime pins must be re-evaluated by StateStore.
                    report.removals_awaiting_runtime_cleanup += 1;
                }
                Some(head) => {
                    anyhow::ensure!(
                        recovery.retired_chain_fence(&current.chain_root_id)?
                            == crate::recovery::RetiredChainFence::Absent,
                        "fenced chain {} has a head its pending Remove does not name",
                        current.chain_root_id
                    );
                    // A trusted head different from the Remove's expected
                    // predecessor is newer authority. First verify its CAS
                    // closure, then atomically turn the stale Remove journal
                    // slot into a published Set for that exact head. A crash
                    // can now leave either the old Remove or a repairable Set,
                    // never an unjournaled advanced head and never a deletion
                    // path that can touch that head.
                    crate::rebuild::verify_repair_closure_anchored_with_cas(
                        &cas,
                        &current.chain_root_id,
                        &head.target_hash,
                        true,
                        None,
                    )?;
                    let replacement = recovery.replace_remove_with_published_set(
                        &chain_lock,
                        &current.chain_root_id,
                        &current.transition_id,
                        &head.target_hash,
                    )?;
                    let replacement = advance_set_to_applying(recovery, &chain_lock, replacement)?;
                    crate::rebuild::repair_one_chain_verified_with_cas_and_observer(
                        projection,
                        &cas,
                        &replacement.chain_root_id,
                        &head.target_hash,
                        observer,
                    )?;
                    if acknowledge
                        && !acknowledge_if_projection_current(
                            projection,
                            recovery,
                            &chain_lock,
                            &replacement,
                            Some(&head),
                        )?
                    {
                        anyhow::bail!(
                            "advanced-head repair Set did not converge while replaying {}",
                            replacement.chain_root_id
                        );
                    }
                    report.stale_removes_cleared += 1;
                }
            },
        }
        observer_progress.maybe_notify(
            observer,
            ProjectionRecoveryProgress {
                stage: ProjectionRecoveryStage::ReplayingPending,
                chains_total: None,
                chains_done: 0,
                threads_restored: 0,
                events_projected: 0,
                pending_total: Some(pending_total),
                pending_done: report.records_checked,
            },
        );
    }
    notify_recovery_observer(
        observer,
        ProjectionRecoveryProgress {
            stage: ProjectionRecoveryStage::ReplayingPending,
            chains_total: None,
            chains_done: 0,
            threads_restored: 0,
            events_projected: 0,
            pending_total: Some(pending_total),
            pending_done: report.records_checked,
        },
    )?;
    Ok(report)
}

fn ensure_recovery_not_cancelled(
    observer: Option<&dyn ProjectionRecoveryObserver>,
) -> anyhow::Result<()> {
    if observer.is_some_and(ProjectionRecoveryObserver::is_cancelled) {
        anyhow::bail!("projection recovery cancelled");
    }
    Ok(())
}

fn notify_recovery_observer(
    observer: Option<&dyn ProjectionRecoveryObserver>,
    progress: ProjectionRecoveryProgress,
) -> anyhow::Result<()> {
    ensure_recovery_not_cancelled(observer)?;
    if let Some(observer) = observer {
        observer.update(progress);
    }
    Ok(())
}

fn advance_set_to_applying(
    recovery: &RecoveryStore,
    chain_lock: &crate::chain::ChainLock,
    mut transition: PendingChainHeadTransition,
) -> anyhow::Result<PendingChainHeadTransition> {
    if transition.phase == TransitionPhase::Prepared {
        transition = recovery.advance_phase(
            chain_lock,
            &transition.chain_root_id,
            &transition.transition_id,
            TransitionPhase::Prepared,
            TransitionPhase::HeadPublished,
        )?;
    }
    if transition.phase == TransitionPhase::HeadPublished {
        transition = recovery.advance_phase(
            chain_lock,
            &transition.chain_root_id,
            &transition.transition_id,
            TransitionPhase::HeadPublished,
            TransitionPhase::ApplyingProjection,
        )?;
    }
    Ok(transition)
}

enum PendingSetHead<'a> {
    Published(&'a SignedRef),
    Unpublished(Option<&'a SignedRef>),
}

fn classify_pending_remove_head(
    transition: &PendingChainHeadTransition,
    head: Option<&SignedRef>,
) -> anyhow::Result<PendingRemoveHeadState> {
    if transition.operation != HeadOperation::Remove {
        anyhow::bail!("pending transition is not a Remove");
    }
    let expected = transition
        .expected_previous_hash
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("pending Remove is missing expected_previous_hash"))?;
    Ok(match head {
        None => PendingRemoveHeadState::HeadAbsent,
        Some(head) if head.target_hash == expected => PendingRemoveHeadState::ExpectedHeadVisible,
        Some(head) => PendingRemoveHeadState::AdvancedHeadVisible {
            current_head_hash: head.target_hash.clone(),
        },
    })
}

/// Resolve a pending Set from the durable phase and exact signed head. A
/// Prepared transition may still expose its expected previous head (or no
/// head for first publication). Once the target is visible—or the journal has
/// crossed HeadPublished—only the target is admissible.
fn classify_pending_set_head<'a>(
    transition: &PendingChainHeadTransition,
    head: Option<&'a SignedRef>,
) -> anyhow::Result<PendingSetHead<'a>> {
    if transition.operation != HeadOperation::Set {
        anyhow::bail!("pending transition is not a Set");
    }
    let target = transition
        .target_hash
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("pending Set is missing target_hash"))?;
    if let Some(head) = head
        && head.target_hash == target
    {
        return Ok(PendingSetHead::Published(head));
    }
    if transition.phase == TransitionPhase::Prepared {
        let still_previous = match (transition.expected_previous_hash.as_deref(), head) {
            (Some(expected), Some(head)) => head.target_hash == expected,
            (None, None) => true,
            _ => false,
        };
        if still_previous {
            return Ok(PendingSetHead::Unpublished(head));
        }
    }
    anyhow::bail!(
        "pending Set/head state conflict for {}: phase={:?}, expected_previous={:?}, target={}, visible={:?}",
        transition.chain_root_id,
        transition.phase,
        transition.expected_previous_hash,
        target,
        head.map(|head| head.target_hash.as_str()),
    )
}

fn acknowledge_unpublished_set(
    projection: &ProjectionDb,
    recovery: &RecoveryStore,
    chain_lock: &crate::chain::ChainLock,
    transition: &PendingChainHeadTransition,
    previous: Option<&SignedRef>,
) -> anyhow::Result<()> {
    if transition.phase != TransitionPhase::Prepared {
        anyhow::bail!("only a Prepared Set can be acknowledged as unpublished");
    }
    let indexed = projection.get_projection_meta(&transition.chain_root_id)?;
    let indexed_hash = indexed
        .as_ref()
        .map(|meta| meta.indexed_chain_state_hash.as_str());
    let expected_hash = previous.map(|head| head.target_hash.as_str());
    if indexed_hash != expected_hash {
        anyhow::bail!(
            "unpublished Set projection mismatch for {}: indexed={indexed_hash:?}, expected={expected_hash:?}",
            transition.chain_root_id,
        );
    }
    if !recovery.acknowledge(
        chain_lock,
        &transition.chain_root_id,
        &transition.transition_id,
    )? {
        anyhow::bail!(
            "pending Set changed while acknowledging unpublished transition for {}",
            transition.chain_root_id,
        );
    }
    Ok(())
}

fn load_hashed_cas_object<T: serde::de::DeserializeOwned>(
    cas: &lillux::CasStore,
    hash: &str,
) -> anyhow::Result<T> {
    if !lillux::valid_hash(hash) || hash.bytes().any(|byte| byte.is_ascii_uppercase()) {
        anyhow::bail!("invalid authoritative CAS hash: {hash}");
    }
    let value = cas
        .get_object(hash)
        .with_context(|| format!("read authoritative CAS object {hash}"))?
        .ok_or_else(|| anyhow::anyhow!("authoritative CAS object {hash} is absent"))?;
    serde_json::from_value(value).with_context(|| format!("decode authoritative CAS object {hash}"))
}

fn load_terminal_chain_from_cas(
    cas: &lillux::CasStore,
    chain_root_id: &str,
    head_hash: &str,
) -> anyhow::Result<Option<AuthoritativeTerminalChain>> {
    crate::chain::validate_authoritative_history_with_cas(cas, chain_root_id, head_hash, None)
        .with_context(|| {
            format!("validate authoritative history before retaining chain {chain_root_id}")
        })?;
    let chain_state: crate::ChainState = load_hashed_cas_object(cas, head_hash)?;
    chain_state.validate()?;
    if chain_state.chain_root_id != chain_root_id {
        anyhow::bail!(
            "authoritative chain root mismatch: expected {chain_root_id}, got {}",
            chain_state.chain_root_id
        );
    }

    let mut root_policy = None;
    let mut thread_ids = Vec::with_capacity(chain_state.threads.len());
    let mut terminal_members = Vec::with_capacity(chain_state.threads.len());
    for (thread_id, entry) in &chain_state.threads {
        let snapshot: ThreadSnapshot = load_hashed_cas_object(cas, &entry.snapshot_hash)?;
        snapshot.validate()?;
        if snapshot.thread_id != *thread_id || snapshot.chain_root_id != chain_root_id {
            anyhow::bail!("authoritative snapshot identity mismatch for {thread_id}");
        }
        if snapshot.status != entry.status {
            anyhow::bail!("chain/snapshot status mismatch for {thread_id}");
        }
        let member_terminal_at = snapshot
            .finished_at
            .as_ref()
            .unwrap_or(&snapshot.updated_at)
            .clone();
        terminal_members.push(crate::projection::TerminalMember {
            is_terminal: snapshot.status.is_terminal(),
            timestamp: member_terminal_at,
        });
        if thread_id == chain_root_id {
            root_policy = snapshot.captured_history_policy.clone();
        }
        thread_ids.push(thread_id.clone());
    }
    let root_policy = root_policy.ok_or_else(|| {
        anyhow::anyhow!(
            "authoritative chain {chain_root_id} has no root member with captured history policy"
        )
    })?;
    let derived =
        crate::projection::derive_terminal_retention(terminal_members, &root_policy.retention)?;
    let Some(terminal_at) = derived.terminal_at else {
        return Ok(None);
    };
    Ok(Some(AuthoritativeTerminalChain {
        chain_root_id: chain_root_id.to_string(),
        head_hash: head_hash.to_string(),
        root_policy,
        terminal_at,
        retire_after: derived.retire_after,
        thread_ids,
    }))
}

fn acknowledge_if_projection_current(
    projection: &ProjectionDb,
    recovery: &RecoveryStore,
    chain_lock: &crate::chain::ChainLock,
    transition: &PendingChainHeadTransition,
    head: Option<&SignedRef>,
) -> anyhow::Result<bool> {
    if transition.phase != TransitionPhase::ApplyingProjection {
        return Ok(false);
    }
    let head = match head {
        Some(head) => head,
        None => return Ok(false),
    };
    if transition.operation != HeadOperation::Set
        || transition.target_hash.as_deref() != Some(head.target_hash.as_str())
    {
        return Ok(false);
    }
    let current = projection.get_projection_meta(&transition.chain_root_id)?;
    if current
        .as_ref()
        .map(|meta| meta.indexed_chain_state_hash.as_str())
        != Some(head.target_hash.as_str())
    {
        return Ok(false);
    }
    recovery.acknowledge(
        chain_lock,
        &transition.chain_root_id,
        &transition.transition_id,
    )
}

fn ensure_state_chain_lock(
    refs_root: &Path,
    lock: &crate::chain::ChainLock,
    chain_root_id: &str,
) -> anyhow::Result<()> {
    lock.ensure_protects(refs_root, chain_root_id)
}

impl ProjectionRepairSink for PendingProjectionRepairs {
    fn request_repair(&self, request: ProjectionRepairRequest) {
        self.requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(request);
    }
}

/// A successfully committed authoritative write and its projection status.
#[derive(Debug, Clone)]
pub struct CommittedWrite<T> {
    pub value: T,
    pub projection: ProjectionStatus,
}

pub struct CreateChainWithEventSuccessorRequest<'a> {
    pub chain_root_id: &'a str,
    pub genesis_snapshot: ThreadSnapshot,
    pub event_successor_snapshot: ThreadSnapshot,
    pub events: Vec<ThreadEvent>,
    pub signer: &'a dyn Signer,
    pub runtime_liveness: &'a dyn RuntimeLivenessInspector,
    pub cas_mutation_guard: &'a crate::recovery::CasMutationGuard,
}

/// A member thread birth whose initial events are committed together with the
/// member's successor snapshot under one signed head.
pub struct AddThreadWithEventSuccessorRequest<'a> {
    pub chain_root_id: &'a str,
    pub genesis_snapshot: ThreadSnapshot,
    pub event_successor_snapshot: ThreadSnapshot,
    pub events: Vec<ThreadEvent>,
    pub signer: &'a dyn Signer,
    pub runtime_liveness: &'a dyn RuntimeLivenessInspector,
    pub cas_mutation_guard: &'a crate::recovery::CasMutationGuard,
}

struct CreateChainWithEventsRequest<'a> {
    chain_root_id: &'a str,
    snapshot: ThreadSnapshot,
    event_successor_snapshot: Option<ThreadSnapshot>,
    events: Vec<ThreadEvent>,
    signer: &'a dyn Signer,
    runtime_liveness: Option<&'a dyn RuntimeLivenessInspector>,
    cas_mutation_guard: &'a crate::recovery::CasMutationGuard,
}

#[derive(Debug, Clone)]
pub struct AuthoritativeThreadSnapshotWithLastEventReadback {
    /// Exact verified signed-head target from which both values were read.
    pub chain_head_hash: String,
    pub snapshot: ThreadSnapshot,
    /// Exact event selected by the current `ChainThreadEntry`. This can be
    /// newer than `snapshot.last_event_hash` when appends leave lifecycle
    /// state unchanged.
    pub last_event: Option<(String, ThreadEvent)>,
}

#[derive(Debug, Clone)]
pub struct AuthoritativeThreadSnapshotReadback {
    pub snapshot: ThreadSnapshot,
    pub last_event: Option<(String, ThreadEvent)>,
    pub is_chain_final_event: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthoritativeThreadAppendAnchorReadback {
    pub chain_last_event_hash: Option<String>,
    pub chain_last_seq: u64,
    pub thread_last_event_hash: Option<String>,
    pub thread_last_seq: u64,
}

#[derive(Debug, Clone)]
pub struct AuthoritativeThreadAppendHistoryReadback {
    pub anchor: AuthoritativeThreadAppendAnchorReadback,
    pub events: Vec<(String, ThreadEvent)>,
}

/// Opaque, state-authenticated fixed endpoint for a resumable thread-event
/// read. Its fields are deliberately not caller-constructible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthoritativeThreadEventReadAnchor {
    chain_root_id: String,
    thread_id: String,
    chain_head_hash: String,
    snapshot_hash: String,
    thread_last_event_hash: Option<String>,
    thread_last_seq: u64,
}

impl AuthoritativeThreadEventReadAnchor {
    pub fn chain_root_id(&self) -> &str {
        &self.chain_root_id
    }

    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    pub fn tip(&self) -> Option<(&str, u64)> {
        self.thread_last_event_hash
            .as_deref()
            .map(|hash| (hash, self.thread_last_seq))
    }
}

/// State-minted continuation for the next bounded slice of one exact anchor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthoritativeThreadEventReadProgress {
    chain_root_id: String,
    thread_id: String,
    chain_head_hash: String,
    snapshot_hash: String,
    next_event_hash: String,
    next_thread_seq: u64,
}

/// Events newest first, plus the continuation when older events remain.
#[derive(Debug, Clone)]
pub struct AuthoritativeThreadEventReadPage {
    pub events: Vec<(String, ThreadEvent)>,
    pub progress: Option<AuthoritativeThreadEventReadProgress>,
}

#[derive(Debug)]
pub enum ConditionalThreadEventAppendResult {
    Committed(CommittedWrite<AppendResult>),
    Conflict,
}

/// One anchored append: the exact thread frontier the caller read must still
/// be current under the chain lock that publishes the events.
pub struct AnchoredThreadAppend<'a> {
    pub chain_root_id: &'a str,
    pub thread_id: &'a str,
    pub events: Vec<ThreadEvent>,
    pub snapshot_updates: Vec<SnapshotUpdate>,
    pub signer: &'a dyn Signer,
    pub runtime_liveness: &'a dyn RuntimeLivenessInspector,
    pub expected: &'a AuthoritativeThreadEventReadAnchor,
}

/// Parameters of one append whose chain lock the caller already holds.
struct LockedAppend<'a> {
    chain_root_id: &'a str,
    thread_id: &'a str,
    events: Vec<ThreadEvent>,
    snapshot_updates: Vec<SnapshotUpdate>,
    signer: &'a dyn Signer,
    runtime_liveness: Option<&'a dyn RuntimeLivenessInspector>,
    expected: Option<&'a AuthoritativeThreadEventReadAnchor>,
}

/// A target append conditional on both its own and a counterpart chain's exact
/// frontier. The counterpart is never modified.
pub struct TwoAnchorThreadAppend<'a> {
    pub chain_root_id: &'a str,
    pub thread_id: &'a str,
    pub events: Vec<ThreadEvent>,
    pub signer: &'a dyn Signer,
    pub runtime_liveness: &'a dyn RuntimeLivenessInspector,
    pub expected: &'a AuthoritativeThreadEventReadAnchor,
    pub counterpart: &'a AuthoritativeThreadEventReadAnchor,
    /// Kind the target thread's signed snapshot must carry.
    pub required_target_kind: &'a str,
}

/// Single-target append under a mixed cut: the target's exact frontier plus
/// whatever live and retired dependencies the caller's read named. No second
/// live chain is required.
pub struct SingleAnchorThreadAppend<'a> {
    pub chain_root_id: &'a str,
    pub thread_id: &'a str,
    pub events: Vec<ThreadEvent>,
    pub signer: &'a dyn Signer,
    pub runtime_liveness: &'a dyn RuntimeLivenessInspector,
    pub expected: &'a AuthoritativeThreadEventReadAnchor,
    /// Kind the target thread's signed snapshot must carry.
    pub required_target_kind: &'a str,
}

/// Borrowed signed data captured while the target and every dependency chain
/// lock remain held. Not a lock handle, writer grant or reusable token.
pub struct SingleAnchorAdmissionView<'a> {
    pub target: &'a ThreadSnapshot,
    pub target_thread_ids: &'a [String],
}

struct MixedCutAdmission<'a> {
    target: &'a ThreadSnapshot,
    counterpart: Option<&'a ThreadSnapshot>,
    target_thread_ids: &'a [String],
    counterpart_thread_ids: Option<&'a [String]>,
}

/// A retired chain identity an append depends on: the fence store must still
/// record it as retired with this exact retained closure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetiredChainDependency {
    pub chain_root_id: String,
    pub manifest_hash: String,
}

/// Every additional chain a mixed-cut append excludes: live chains by exact
/// frontier and retired chains by fence. A chain appears in only one list.
pub struct MixedCutDependencies<'a> {
    pub current_anchors: &'a [AuthoritativeThreadEventReadAnchor],
    pub retired: &'a [RetiredChainDependency],
}

/// Borrowed signed data captured while both exact chain locks remain held.
/// This is not a lock handle, writer grant or reusable admission token.
pub struct TwoAnchorAdmissionView<'a> {
    pub target: &'a ThreadSnapshot,
    pub counterpart: &'a ThreadSnapshot,
    pub target_thread_ids: &'a [String],
    pub counterpart_thread_ids: &'a [String],
}

#[derive(Debug, Clone)]
pub struct AuthoritativeThreadEventChainReadback {
    pub snapshot: ThreadSnapshot,
    pub events: Vec<(String, ThreadEvent)>,
}

#[derive(Debug, Clone)]
pub struct AuthoritativeContinuationPredecessorReadback {
    pub updated_at: String,
    pub last_event_hash: Option<String>,
    pub last_chain_seq: u64,
    pub source_was_present: bool,
    pub source_last_event_hash: Option<String>,
    pub source_last_thread_seq: u64,
    pub successor_was_absent: bool,
}

#[derive(Debug, Clone)]
pub struct AuthoritativeThreadPairReadback {
    pub source: Option<AuthoritativeThreadSnapshotReadback>,
    pub successor: Option<AuthoritativeThreadEventChainReadback>,
    pub predecessor: Option<AuthoritativeContinuationPredecessorReadback>,
}

#[derive(Debug, Clone)]
pub struct AuthoritativeRootThreadReadback {
    pub snapshot: ThreadSnapshot,
    pub events: Vec<(String, ThreadEvent)>,
    pub is_chain_final_event: bool,
}

#[derive(Debug, Clone)]
pub struct AuthoritativeRootGenesisReadback {
    pub snapshot: Option<ThreadSnapshot>,
    pub updated_at: String,
    pub prev_chain_state_hash: Option<String>,
    pub last_event_hash: Option<String>,
    pub last_chain_seq: u64,
    pub only_root_thread: bool,
    pub root_last_event_hash: Option<String>,
    pub root_last_thread_seq: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct AuthoritativeRootBirthReadback {
    pub root: Option<AuthoritativeRootThreadReadback>,
    pub only_root_thread: bool,
    pub predecessor: Option<AuthoritativeRootGenesisReadback>,
}

impl<T> CommittedWrite<T> {
    fn new(value: T, projection: ProjectionStatus) -> Self {
        Self { value, projection }
    }
}

/// High-level state database.
///
/// Owns the on-disk roots (CAS, refs, locators), the SQLite projection,
/// and an in-memory head cache protected by a mutex.
pub struct StateDb {
    // Keep the exact no-follow authority roots alive for the lifetime of this
    // store. These descriptors also make ancestor replacement observable to
    // descriptor-relative helpers instead of silently rebinding the store.
    _runtime_state_directory: lillux::PinnedDirectory,
    _offline_namespace_lock: Option<lillux::PinnedDirectoryLock>,
    _cas_directory: lillux::PinnedDirectory,
    _refs_directory: lillux::PinnedDirectory,
    _locators_directory: lillux::PinnedDirectory,
    _recovery_directory: lillux::PinnedDirectory,
    cas_root: PathBuf,
    refs_root: PathBuf,
    locators_root: PathBuf,
    projection: ProjectionDb,
    original_projection_terminals: Vec<Arc<crate::projection::ClosedOriginalProjection>>,
    original_projection_context: Option<OriginalProjectionBuildContext>,
    operational: OperationalAccess,
    recovery: RecoveryStore,
    trust_store: Arc<TrustStore>,
    head_cache: Mutex<HeadCache>,
    projection_repair_sink: Arc<dyn ProjectionRepairSink>,
}

/// Full descriptor-bound State preparation for an offline projection rebuild.
///
/// Capturing this owner opens no SQLite connection. It retains the required
/// locators and recovery roots and the exact trust snapshot; GC's
/// optional-recovery authority is not a substitute.
/// The reset caller can authenticate its active operation and publish the
/// original quota claim before admitting a native projection connection.
pub struct PreparedOfflineProjection {
    runtime_state_directory: lillux::PinnedDirectory,
    cas_directory: lillux::PinnedDirectory,
    refs_directory: lillux::PinnedDirectory,
    locators_directory: lillux::PinnedDirectory,
    recovery_directory: lillux::PinnedDirectory,
    namespace_lock: Option<lillux::PinnedDirectoryLock>,
    cas_root: PathBuf,
    refs_root: PathBuf,
    locators_root: PathBuf,
    recovery: RecoveryStore,
    trust_store: Arc<TrustStore>,
}

impl PreparedOfflineProjection {
    pub fn capture(
        runtime_state_directory: lillux::PinnedDirectory,
        trust_store: Arc<TrustStore>,
        namespace: &lillux::PinnedDirectoryLock,
    ) -> anyhow::Result<Self> {
        namespace.ensure_protects(&runtime_state_directory)?;
        let prepared = Self::capture_inner(runtime_state_directory, trust_store)?;
        let prepared = Self {
            namespace_lock: Some(namespace.clone()),
            ..prepared
        };
        prepared.require_custody()?;
        Ok(prepared)
    }

    fn capture_inner(
        runtime_state_directory: lillux::PinnedDirectory,
        trust_store: Arc<TrustStore>,
    ) -> anyhow::Result<Self> {
        runtime_state_directory.ensure_path_binding()?;
        let runtime_state_dir = runtime_state_directory.path();
        let cas_root = runtime_state_dir.join("objects");
        let refs_root = runtime_state_dir.join("refs");
        let locators_root = runtime_state_dir.join("locators");
        let cas_directory = runtime_state_directory
            .open_child_directory(std::ffi::OsStr::new("objects"))?
            .ok_or_else(|| anyhow::anyhow!("CAS objects directory is absent"))?;
        let refs_directory = runtime_state_directory
            .open_child_directory(std::ffi::OsStr::new("refs"))?
            .ok_or_else(|| anyhow::anyhow!("signed refs directory is absent"))?;
        let locators_directory = runtime_state_directory
            .open_child_directory(std::ffi::OsStr::new("locators"))?
            .ok_or_else(|| anyhow::anyhow!("locators directory is absent"))?;
        let recovery_parent = runtime_state_directory
            .open_child_directory(std::ffi::OsStr::new("recovery"))?
            .ok_or_else(|| anyhow::anyhow!("projection recovery parent is absent"))?;
        let recovery_directory = recovery_parent
            .open_child_directory(std::ffi::OsStr::new("thread-projection"))?
            .ok_or_else(|| anyhow::anyhow!("projection recovery directory is absent"))?;
        let recovery = RecoveryStore::from_pinned_directories(
            runtime_state_directory.try_clone()?,
            recovery_directory.try_clone()?,
        )?;
        let prepared = Self {
            runtime_state_directory,
            cas_directory,
            refs_directory,
            locators_directory,
            recovery_directory,
            namespace_lock: None,
            cas_root,
            refs_root,
            locators_root,
            recovery,
            trust_store,
        };
        prepared.require_custody()?;
        Ok(prepared)
    }

    fn require_custody(&self) -> anyhow::Result<()> {
        if let Some(namespace) = &self.namespace_lock {
            namespace.ensure_protects(&self.runtime_state_directory)?;
        }
        self.runtime_state_directory.ensure_path_binding()?;
        self.cas_directory.ensure_path_binding()?;
        self.refs_directory.ensure_path_binding()?;
        self.locators_directory.ensure_path_binding()?;
        self.recovery_directory.ensure_path_binding()?;
        Ok(())
    }

    /// Clone only this preparation's actual roots/recovery/trust. This does
    /// not open SQL, issue a native grant, or manufacture mutation authority.
    pub fn authority(&self) -> anyhow::Result<PinnedStateAuthority> {
        self.require_custody()?;
        Ok(PinnedStateAuthority {
            offline_namespace_lock: self.namespace_lock.clone(),
            runtime_directory: self.runtime_state_directory.try_clone()?,
            cas_directory: self.cas_directory.try_clone()?,
            refs_directory: self.refs_directory.try_clone()?,
            recovery: Some(self.recovery.clone()),
            trust_store: Arc::clone(&self.trust_store),
        })
    }

    fn try_clone_preparation(&self) -> anyhow::Result<Self> {
        self.require_custody()?;
        Ok(Self {
            runtime_state_directory: self.runtime_state_directory.try_clone()?,
            cas_directory: self.cas_directory.try_clone()?,
            refs_directory: self.refs_directory.try_clone()?,
            locators_directory: self.locators_directory.try_clone()?,
            recovery_directory: self.recovery_directory.try_clone()?,
            namespace_lock: self.namespace_lock.clone(),
            cas_root: self.cas_root.clone(),
            refs_root: self.refs_root.clone(),
            locators_root: self.locators_root.clone(),
            recovery: self.recovery.clone(),
            trust_store: Arc::clone(&self.trust_store),
        })
    }

    /// Consume the actual preparation after the original issuer has durably
    /// reserved and initialized its factory. The factory supplies memory/SQL
    /// ownership; State mutation still requires the actual exclusive CAS guard.
    /// # Safety
    /// `lease_authority` must be this factory's genuinely admitted offline
    /// claim, retaining its original funding, clocks and namespace custody
    /// through native closure and physical settlement. A decoded grant is not
    /// sufficient; callers must preserve every failed acquisition owner.
    pub unsafe fn open_projection(
        self,
        factory: Arc<lillux_sqlite::OriginalNativeFactory>,
        lease_authority: Arc<dyn lillux_sqlite::OriginalNativeProjectionLeaseAuthority>,
        guard: &crate::recovery::CasMutationGuard,
    ) -> anyhow::Result<StateDb> {
        factory.require_original_process()?;
        lease_authority.require_original(&factory, false)?;
        self.require_custody()?;
        anyhow::ensure!(
            self.namespace_lock.is_some(),
            "original projection needs retained namespace exclusion"
        );
        anyhow::ensure!(
            guard.is_exclusive(),
            "original projection needs the actual exclusive CAS guard"
        );
        guard.ensure_protects_pinned_runtime(&self.runtime_state_directory)?;
        factory.require_runtime_binding(&self.runtime_state_directory)?;
        // Do every fallible descriptor clone BEFORE the first native open.
        // Both holders use the exact same roots, trust and exclusion generation.
        let state_owner = self.try_clone_preparation()?;
        let native_owner = Arc::new(OriginalPreparedProjectionAuthority {
            preparation: Mutex::new(Some(self)),
            factory: Arc::downgrade(&factory),
            lease_authority: Arc::downgrade(&lease_authority),
            outside_join: Mutex::new(std::sync::Weak::new()),
        });
        native_owner.require_binding(&factory)?;
        // SAFETY: this private host boundary retains the actual captured State
        // roots/trust/recovery/namespace owner and checks the issuer's same
        // runtime plus the caller's actual exclusive CAS guard. No DTO, enum,
        // fresh root, new trust or default factory supplies that proof.
        let inputs = unsafe {
            lillux_sqlite::OriginalNativeTransientInputs::issued(
                lillux_sqlite::OriginalNativeDomain::StateTransient,
                native_owner.clone(),
            )?
        };
        let projection = ProjectionDb::open_original_transient(Arc::clone(&factory), inputs)?;
        let issued_destinations =
            OriginalProjectionBuildContext::issued_destination_slots(&factory)?;
        Ok(state_owner.into_state(
            projection,
            Some(OriginalProjectionBuildContext {
                factory,
                authority: OriginalProjectionStateOwner::Offline(native_owner),
                terminals: Vec::new(),
                durable: Vec::new(),
                retired_selected: Vec::new(),
                schema_references: Mutex::new(Vec::new()),
                generation_leases: Mutex::new(Vec::new()),
                generation_lease_attempts: std::sync::atomic::AtomicU32::new(0),
                issued_destinations,
            }),
        ))
    }

    /// Open exactly the selected current projection generation for fail-only
    /// verification, through a bound `Existing` ticket of `factory` and the
    /// generation lease of `lease_authority`, with SQLite `query_only` set on
    /// the connection before any read. Nothing is created beyond the
    /// generation's own lease sidecar, rebuilt, replayed or acknowledged. A
    /// refused open keeps its native owners inside the returned error.
    ///
    /// # Safety
    /// `factory` and `lease_authority` come from one in-process host the
    /// caller funded under the namespace exclusion this preparation retains,
    /// and both revalidate that exclusion on every native boundary.
    pub unsafe fn open_selected_for_verification(
        self,
        factory: Arc<lillux_sqlite::OriginalNativeFactory>,
        lease_authority: Arc<dyn lillux_sqlite::OriginalNativeProjectionLeaseAuthority>,
    ) -> anyhow::Result<StateDb> {
        factory.require_original_process()?;
        lease_authority.require_original(&factory, false)?;
        self.require_custody()?;
        anyhow::ensure!(
            self.namespace_lock.is_some(),
            "projection verification needs retained namespace exclusion"
        );
        factory.require_runtime_binding(&self.runtime_state_directory)?;
        let generation = self
            .recovery
            .read_generation_in(Some(factory.as_ref()))?
            .ok_or_else(|| anyhow::anyhow!("projection recovery generation is absent"))?;
        if generation.projection_schema_epoch != ProjectionDb::current_schema_epoch() {
            anyhow::bail!(
                "projection generation schema epoch mismatch: selected={}, current={}",
                generation.projection_schema_epoch,
                ProjectionDb::current_schema_epoch()
            );
        }
        let state_owner = self.try_clone_preparation()?;
        let runtime = self.runtime_state_directory.try_clone()?;
        let native_owner = Arc::new(OriginalPreparedProjectionAuthority {
            preparation: Mutex::new(Some(self)),
            factory: Arc::downgrade(&factory),
            lease_authority: Arc::downgrade(&lease_authority),
            outside_join: Mutex::new(std::sync::Weak::new()),
        });
        native_owner.require_binding(&factory)?;
        let issued_destinations =
            OriginalProjectionBuildContext::issued_destination_slots(&factory)?;
        let context = OriginalProjectionBuildContext {
            factory,
            authority: OriginalProjectionStateOwner::Offline(native_owner),
            terminals: Vec::new(),
            durable: Vec::new(),
            retired_selected: Vec::new(),
            schema_references: Mutex::new(Vec::new()),
            generation_leases: Mutex::new(Vec::new()),
            generation_lease_attempts: std::sync::atomic::AtomicU32::new(0),
            issued_destinations,
        };
        let projection = match context
            .open_selected(&runtime, std::ffi::OsStr::new(&generation.projection_file))
        {
            Ok(projection) => projection,
            // The schema reference and any generation lease stay with the
            // context, which the refusal carries.
            Err(error) => {
                return Err(crate::operational::CarriedOwners::carry(
                    error.context("open selected projection for verification"),
                    (context, lease_authority),
                ));
            }
        };
        // Verification is fail-only: the connection refuses every write from
        // here on, at the SQLite level.
        let read_only = projection.with_sql(|connection| {
            connection
                .pragma_update(None, "query_only", "ON")
                .context("set verification projection query_only")?;
            let enforced: bool = connection.query_row("PRAGMA query_only", [], |row| row.get(0))?;
            anyhow::ensure!(enforced, "verification projection did not enter query_only");
            Ok(())
        });
        if let Err(error) = read_only {
            return Err(crate::operational::CarriedOwners::carry(
                projection.retain_operation_failure(error),
                (context, lease_authority),
            ));
        }
        let identity = projection.projection_instance_id();
        let matches = match identity {
            Ok(instance) => instance.as_deref() == Some(generation.projection_instance_id.as_str()),
            Err(error) => {
                return Err(crate::operational::CarriedOwners::carry(
                    projection.retain_operation_failure(error),
                    (context, lease_authority),
                ));
            }
        };
        if !matches {
            let error =
                anyhow::anyhow!("selected projection instance identity does not match generation");
            return Err(crate::operational::CarriedOwners::carry(
                projection.retain_operation_failure(error),
                (context, lease_authority),
            ));
        }
        // The context retains the lease authority by a weak edge; the caller's
        // host keeps the strong owner for the life of the store.
        drop(lease_authority);
        Ok(state_owner.into_state(projection, Some(context)))
    }

    // The projection must already be owned by the selected opening path.
    // This private transfer neither opens SQL nor admits native allocation.
    fn into_state(
        self,
        projection: ProjectionDb,
        original_projection_context: Option<OriginalProjectionBuildContext>,
    ) -> StateDb {
        StateDb {
            _runtime_state_directory: self.runtime_state_directory,
            _cas_directory: self.cas_directory,
            _refs_directory: self.refs_directory,
            _locators_directory: self.locators_directory,
            _recovery_directory: self.recovery_directory,
            _offline_namespace_lock: self.namespace_lock,
            cas_root: self.cas_root,
            refs_root: self.refs_root,
            locators_root: self.locators_root,
            projection,
            original_projection_terminals: Vec::new(),
            original_projection_context,
            operational: OperationalAccess::ProjectionMaintenance,
            recovery: self.recovery,
            trust_store: self.trust_store,
            head_cache: Mutex::new(HeadCache::new()),
            projection_repair_sink: Arc::new(PendingProjectionRepairs::default()),
        }
    }
}

// The actual State preparation and factory are retained for every generated
// connection. Closed proofs stay with State before further publication work.
#[derive(Clone)]
enum OriginalProjectionStateOwner {
    Offline(Arc<OriginalPreparedProjectionAuthority>),
    Live(Arc<live_native::LiveStateNativeOwner>),
}
impl OriginalProjectionStateOwner {
    fn require_binding(
        &self,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
    ) -> anyhow::Result<()> {
        match self {
            Self::Offline(owner) => owner.require_binding(factory),
            Self::Live(owner) => {
                lillux_sqlite::OriginalNativeDestinationAuthority::require_original(
                    owner.as_ref(),
                    factory,
                )
            }
        }
    }
    fn require_selected_runtime(&self, directory: &lillux::PinnedDirectory) -> anyhow::Result<()> {
        match self {
            Self::Offline(owner) => owner.require_selected_runtime(directory),
            Self::Live(owner) => {
                anyhow::ensure!(
                    directory.is_same_directory(owner.runtime())?,
                    "live generated projection has foreign runtime"
                );
                owner.namespace().ensure_protects(directory)
            }
        }
    }
}

impl lillux_sqlite::OriginalNativeDestinationAuthority for OriginalProjectionStateOwner {
    fn into_outside_custody(
        self: Arc<Self>,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        leaf: &lillux_sqlite::OriginalNativeOutsideLeafEvidence,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>,
    ) -> Result<
        lillux_sqlite::OriginalDestinationOutsideCustody,
        lillux_sqlite::OriginalDestinationCustodyFailure,
    > {
        destination_join::transfer_leaf(
            destination_join::StateDestinationLeaf::State(self),
            factory,
            aggregate,
            leaf,
            prepay,
        )
    }
    fn require_original(
        &self,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
    ) -> anyhow::Result<()> {
        self.require_binding(factory)
    }
}

// Retain the genuine closed schema ticket and State owner on every subsequent
// admission/lease refusal; this error does not claim recovery or settlement.
struct OriginalProjectionSchemaAdmissionFailure {
    error: anyhow::Error,
    _reference: Arc<crate::sqlite_schema::ClosedNativeSchemaSqlReference>,
    _owner: OriginalProjectionStateOwner,
}
impl std::fmt::Debug for OriginalProjectionSchemaAdmissionFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OriginalProjectionSchemaAdmissionFailure")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Display for OriginalProjectionSchemaAdmissionFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "projection schema admission retained: {}", self.error)
    }
}
impl std::error::Error for OriginalProjectionSchemaAdmissionFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}

pub(crate) struct OriginalProjectionBuildContext {
    factory: Arc<lillux_sqlite::OriginalNativeFactory>,
    authority: OriginalProjectionStateOwner,
    terminals: Vec<Arc<lillux_sqlite::ClosedOriginalNativeConnection>>,
    durable: Vec<Arc<lillux_sqlite::DurableOriginalNativeBound>>,
    retired_selected: Vec<Arc<crate::projection::ClosedOriginalProjection>>,
    schema_references: Mutex<Vec<Arc<crate::sqlite_schema::ClosedNativeSchemaSqlReference>>>,
    generation_leases: Mutex<Vec<Arc<crate::projection::OriginalGenerationLease>>>,
    generation_lease_attempts: std::sync::atomic::AtomicU32,
    // Concrete identities of every destination authority this context issued,
    // so a taken `dyn` input can be routed to its typed State leaf.
    issued_destinations: Mutex<Vec<StateIssuedDestination>>,
}
/// Test lease authority: verification and rebuild open only transient
/// projections, so every lease request refuses.
#[cfg(all(test, feature = "test-support"))]
pub(crate) struct TestProjectionLeaseAuthority;
#[cfg(all(test, feature = "test-support"))]
impl lillux_sqlite::OriginalNativeProjectionLeaseAuthority for TestProjectionLeaseAuthority {
    fn require_original(
        &self,
        _factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        _cleanup: bool,
    ) -> anyhow::Result<()> {
        anyhow::bail!("test State context admits no projection lease")
    }
    fn fork_deadline(
        &self,
        _factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        _cleanup: bool,
    ) -> anyhow::Result<lillux::time::MonotonicDeadline> {
        anyhow::bail!("test State context admits no projection lease")
    }
}
#[cfg(all(test, feature = "test-support"))]
impl OriginalProjectionBuildContext {
    /// Offline State context on a test host's factory. The caller retains
    /// `lease_authority` for the context's lifetime.
    pub(crate) fn for_test(
        host: &lillux_sqlite::test_host::TestNativeHost,
        preparation: PreparedOfflineProjection,
        lease_authority: &Arc<dyn lillux_sqlite::OriginalNativeProjectionLeaseAuthority>,
    ) -> anyhow::Result<Self> {
        let factory = Arc::clone(host.factory());
        factory.require_original_process()?;
        preparation.require_custody()?;
        anyhow::ensure!(
            preparation.namespace_lock.is_some(),
            "test State context needs retained namespace exclusion"
        );
        factory.require_runtime_binding(&preparation.runtime_state_directory)?;
        let native_owner = Arc::new(OriginalPreparedProjectionAuthority {
            preparation: Mutex::new(Some(preparation)),
            factory: Arc::downgrade(&factory),
            lease_authority: Arc::downgrade(lease_authority),
            outside_join: Mutex::new(std::sync::Weak::new()),
        });
        native_owner.require_binding(&factory)?;
        let issued_destinations = Self::issued_destination_slots(&factory)?;
        Ok(Self {
            factory,
            authority: OriginalProjectionStateOwner::Offline(native_owner),
            terminals: Vec::new(),
            durable: Vec::new(),
            retired_selected: Vec::new(),
            schema_references: Mutex::new(Vec::new()),
            generation_leases: Mutex::new(Vec::new()),
            generation_lease_attempts: std::sync::atomic::AtomicU32::new(0),
            issued_destinations,
        })
    }
    /// Closed terminals for the test host's census. Refuses, returning the
    /// context, while it holds any bound, selected, schema or lease custody.
    pub(crate) fn into_test_terminals(
        self,
    ) -> Result<Vec<lillux_sqlite::OriginalNativeConnectionTerminal>, Self> {
        let transient_only = self.durable.is_empty()
            && self.retired_selected.is_empty()
            && self
                .schema_references
                .try_lock()
                .is_ok_and(|references| references.is_empty())
            && self
                .generation_leases
                .try_lock()
                .is_ok_and(|leases| leases.is_empty());
        if !transient_only {
            return Err(self);
        }
        let Self {
            factory,
            authority,
            terminals,
            durable,
            retired_selected,
            schema_references,
            generation_leases,
            generation_lease_attempts,
            issued_destinations,
        } = self;
        drop((
            factory,
            authority,
            durable,
            retired_selected,
            schema_references,
            generation_leases,
            generation_lease_attempts,
            issued_destinations,
        ));
        Ok(terminals
            .into_iter()
            .map(lillux_sqlite::OriginalNativeConnectionTerminal::Closed)
            .collect())
    }
    pub(crate) fn test_terminal_count(&self) -> usize {
        self.terminals.len()
    }
}
/// Non-owning record of one destination authority issued by a State context.
enum StateIssuedDestination {
    Generated(std::sync::Weak<OriginalGeneratedProjectionAuthority>),
    Schema(std::sync::Weak<OriginalProjectionStateOwner>),
}
impl StateIssuedDestination {
    fn is_live(&self) -> bool {
        match self {
            Self::Generated(weak) => weak.strong_count() > 0,
            Self::Schema(weak) => weak.strong_count() > 0,
        }
    }
}
impl OriginalProjectionBuildContext {
    /// One slot per destination leaf the original ticket bound admits.
    fn issued_destination_slots(
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
    ) -> anyhow::Result<Mutex<Vec<StateIssuedDestination>>> {
        let maximum = destination_join::maximum_state_destination_leaves(
            factory.maximum_resource_tickets()?,
        )?;
        let mut slots = Vec::new();
        slots.try_reserve_exact(maximum)?;
        Ok(Mutex::new(slots))
    }
    /// Record an issued authority before any native effect. Dead entries are
    /// pruned first; a full list refuses, since each input owns one ticket.
    fn record_issued_destination(&self, issued: StateIssuedDestination) -> anyhow::Result<()> {
        let maximum = destination_join::maximum_state_destination_leaves(
            self.factory.maximum_resource_tickets()?,
        )?;
        let mut slots = self
            .issued_destinations
            .try_lock()
            .map_err(|_| anyhow::anyhow!("State destination issuance record busy or poisoned"))?;
        // Fund the prune scan (one step per record) and the push before
        // touching the record.
        let prune_steps = u64::try_from(slots.len())?
            .checked_add(1)
            .context("State destination issuance prune work overflow")?;
        self.factory.debit_issuance_steps(prune_steps)?;
        slots.retain(StateIssuedDestination::is_live);
        anyhow::ensure!(
            slots.len() < maximum && slots.len() < slots.capacity(),
            "State destination issuance exceeds its original ticket bound"
        );
        slots.push(issued);
        Ok(())
    }
    /// Concrete Generated authority issued here for this `dyn` allocation.
    fn issued_generated(
        &self,
        authority: &Arc<dyn lillux_sqlite::OriginalNativeDestinationAuthority>,
    ) -> anyhow::Result<Option<Arc<OriginalGeneratedProjectionAuthority>>> {
        let address = Arc::as_ptr(authority).cast::<()>();
        let slots = self
            .issued_destinations
            .try_lock()
            .map_err(|_| anyhow::anyhow!("State destination issuance record busy or poisoned"))?;
        Ok(slots.iter().find_map(|issued| match issued {
            StateIssuedDestination::Generated(weak)
                if std::ptr::eq(weak.as_ptr().cast::<()>(), address) =>
            {
                weak.upgrade()
            }
            _ => None,
        }))
    }
    /// Concrete schema-reference authority issued here for this allocation.
    fn issued_schema(
        &self,
        authority: &Arc<dyn lillux_sqlite::OriginalNativeDestinationAuthority>,
    ) -> anyhow::Result<Option<Arc<OriginalProjectionStateOwner>>> {
        let address = Arc::as_ptr(authority).cast::<()>();
        let slots = self
            .issued_destinations
            .try_lock()
            .map_err(|_| anyhow::anyhow!("State destination issuance record busy or poisoned"))?;
        Ok(slots.iter().find_map(|issued| match issued {
            StateIssuedDestination::Schema(weak)
                if std::ptr::eq(weak.as_ptr().cast::<()>(), address) =>
            {
                weak.upgrade()
            }
            _ => None,
        }))
    }
    /// Number of issuance records, bounding the identity scan of one take.
    pub(super) fn issued_destination_count(&self) -> anyhow::Result<usize> {
        Ok(self
            .issued_destinations
            .try_lock()
            .map_err(|_| anyhow::anyhow!("State destination issuance record busy or poisoned"))?
            .len())
    }
    /// Open the transient expected projection that verification rederives,
    /// on a fresh State ticket under this context's State owner. The issued
    /// authority is recorded before any native effect.
    pub(crate) fn open_expected_projection(&self) -> anyhow::Result<ProjectionDb> {
        self.authority.require_binding(&self.factory)?;
        let owner = Arc::new(self.authority.clone());
        self.record_issued_destination(StateIssuedDestination::Schema(Arc::downgrade(&owner)))?;
        let authority: Arc<dyn lillux_sqlite::OriginalNativeDestinationAuthority> = owner;
        // SAFETY: the actual prepared State owner retains all roots, recovery,
        // trust and namespace; this purpose admits only a transient in-memory
        // projection that never reaches State storage.
        let inputs = unsafe {
            lillux_sqlite::OriginalNativeTransientInputs::issued(
                lillux_sqlite::OriginalNativeDomain::StateTransient,
                authority,
            )?
        };
        ProjectionDb::open_original_transient(Arc::clone(&self.factory), inputs)
    }
    /// Close the expected projection and retain its genuine terminal here.
    /// A refused close keeps the actual connection in the returned error.
    pub(crate) fn retain_closed_expected_projection(
        &mut self,
        expected: ProjectionDb,
    ) -> anyhow::Result<()> {
        let closed = expected
            .close_original(&self.factory)
            .map_err(anyhow::Error::new)?;
        self.terminals.push(closed.native_proof());
        closed.require_original(&self.factory)
    }
    fn open(
        &self,
        directory: &lillux::PinnedDirectory,
        name: &std::ffi::OsStr,
        durable: Option<&Arc<crate::projection::DurableOriginalProjection>>,
    ) -> anyhow::Result<ProjectionDb> {
        self.open_bound(directory, name, durable, false)
    }
    fn open_selected(
        &self,
        directory: &lillux::PinnedDirectory,
        name: &std::ffi::OsStr,
    ) -> anyhow::Result<ProjectionDb> {
        self.open_bound(directory, name, None, true)
    }
    fn open_bound(
        &self,
        directory: &lillux::PinnedDirectory,
        name: &std::ffi::OsStr,
        durable: Option<&Arc<crate::projection::DurableOriginalProjection>>,
        selected: bool,
    ) -> anyhow::Result<ProjectionDb> {
        self.authority.require_binding(&self.factory)?;
        self.authority.require_selected_runtime(directory)?;
        let retained_directory = directory.try_clone()?;
        let root = Arc::new(directory.try_clone()?);
        let expected_identity = if selected {
            // Observe the selected inode through metadata only; never open a
            // second SQLite data descriptor beside the native Bound owner.
            let file = directory
                .open_mount_entry(name)?
                .context("selected projection absent")?;
            anyhow::ensure!(
                file.metadata()?.is_file(),
                "selected projection is not regular"
            );
            directory.ensure_open_regular_same_mount(&file)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                let metadata = file.metadata()?;
                Some((metadata.dev(), metadata.ino()))
            }
            #[cfg(not(unix))]
            anyhow::bail!("selected projection inode identity unavailable");
        } else {
            durable.map(|proof| proof.main_identity())
        };
        let schema_owner = Arc::new(self.authority.clone());
        self.record_issued_destination(StateIssuedDestination::Schema(Arc::downgrade(
            &schema_owner,
        )))?;
        let schema_authority: Arc<dyn lillux_sqlite::OriginalNativeDestinationAuthority> =
            schema_owner;
        // SAFETY: the actual prepared State owner retains all roots, recovery,
        // trust and namespace; this purpose admits only a transient reference.
        let schema_inputs = unsafe {
            lillux_sqlite::OriginalNativeTransientInputs::issued(
                lillux_sqlite::OriginalNativeDomain::NodeProjectionSchemaReference,
                schema_authority,
            )?
        };
        let reference = Arc::new(crate::sqlite_schema::prepare_native_schema_sql_reference(
            Arc::clone(&self.factory),
            schema_inputs,
            lillux_sqlite::OriginalNativeDomain::NodeProjectionSchemaReference,
            crate::projection::projection_schema_sql(),
        )?);
        // Preserve every actual schema ticket before any destination/lease
        // effect. Native close cannot discard a receipt needed by its census.
        match self.schema_references.try_lock() {
            Ok(mut references) => references.push(Arc::clone(&reference)),
            Err(_) => {
                return Err(anyhow::Error::new(
                    OriginalProjectionSchemaAdmissionFailure {
                        error: anyhow::anyhow!("actual projection schema receipt custody busy"),
                        _reference: reference,
                        _owner: self.authority.clone(),
                    },
                ));
            }
        }
        let opened = (|| -> anyhow::Result<ProjectionDb> {
            reference.terminal().require_original(&self.factory)?;
            let destination = Arc::new(OriginalGeneratedProjectionAuthority {
                preparation: self.authority.clone(),
                schema_reference: Arc::downgrade(&reference),
                root: Arc::clone(&root),
                name: name.to_os_string(),
                expected_identity,
            });
            destination.require_binding(&self.factory)?;
            self.record_issued_destination(StateIssuedDestination::Generated(Arc::downgrade(
                &destination,
            )))?;
            // SAFETY: this private State caller retains the actual original roots,
            // trust/recovery/namespace and same factory. Existing reopen also
            // retains the real durable source and checks its exact inode before
            // admission and after native open. The outer builder holds CAS guard.
            let creation_root = Arc::clone(&root);
            let inputs = unsafe {
                lillux_sqlite::OriginalNativeBoundInputs::issued(
                    root,
                    name,
                    if selected || durable.is_some() {
                        lillux_sqlite::OriginalNativeBoundMode::Existing
                    } else {
                        lillux_sqlite::OriginalNativeBoundMode::CreateExclusive
                    },
                    if selected {
                        lillux_sqlite::OriginalNativeDomain::NodeProjection
                    } else {
                        lillux_sqlite::OriginalNativeDomain::StateGeneratedProjection
                    },
                    destination,
                    None,
                )?
            };
            let lease = if let Some(proof) = durable {
                proof.require_original(&self.factory)?;
                proof.require_coordinate(directory, name)?;
                proof.clone_lease()?
            } else {
                // Bound every original generation preparation BEFORE creating a
                // sidecar or allocating its retained inputs. This is the actual
                // original resident budget's signed maximum, not a fresh allowance.
                // Refusals stay charged as attempts; no retry remints this slot.
                let maximum = self.factory.maximum_resource_tickets()?;
                self.generation_lease_attempts
                    .fetch_update(
                        std::sync::atomic::Ordering::AcqRel,
                        std::sync::atomic::Ordering::Acquire,
                        |current| current.checked_add(1).filter(|next| *next <= maximum),
                    )
                    .map_err(|_| {
                        anyhow::anyhow!("original generation preparation envelope exhausted")
                    })?;
                let mut lease_name = name.to_os_string();
                lease_name.push(".lease");
                // An existing sidecar is observed through O_PATH only. A data
                // open followed by close could release SQLite's POSIX locks if a
                // hostile/stale sidecar aliases a live main/WAL/SHM inode, before
                // native alias refusal gets a chance to run. Only genuine O_EXCL
                // creation may acquire a fresh data descriptor at this boundary.
                let lease = if selected {
                    match creation_root.open_mount_entry(&lease_name)? {
                        Some(metadata) => {
                            creation_root.ensure_open_regular_same_mount(&metadata)?;
                            metadata
                        }
                        None => creation_root.create_regular_exclusive(&lease_name, 0o600)?,
                    }
                } else {
                    creation_root.create_regular_exclusive(&lease_name, 0o600)?
                };
                if let Err(error) = directory.sync() {
                    return Err(anyhow::Error::new(OriginalProjectionLeaseSetupFailure {
                        error,
                        _lease: lease,
                        _directory: retained_directory,
                        _name: lease_name,
                        _factory: Arc::clone(&self.factory),
                    }));
                }
                // Neither producer acquires an unaccounted raw-file flock. The
                // creation descriptor gives way to the exact metadata coordinate;
                // the shared native mechanism owns the paid lock and its terminal.
                drop(lease);
                let authority: Arc<dyn lillux_sqlite::OriginalNativeProjectionLeaseAuthority> =
                    match &self.authority {
                        OriginalProjectionStateOwner::Offline(owner) => owner
                            .lease_authority
                            .upgrade()
                            .context("original offline lease authority retired")?,
                        OriginalProjectionStateOwner::Live(owner) => owner.clone(),
                    };
                authority.require_original(&self.factory, false)?;
                // SAFETY: each trusted construction boundary retains the actual
                // original issuer, funding, clocks and namespace through settlement.
                let native = unsafe {
                    crate::projection::OriginalGenerationLease::prepare(
                        Arc::clone(&self.factory),
                        directory.try_clone()?,
                        lease_name,
                        authority,
                    )
                }?;
                self.generation_leases
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(Arc::clone(&native));
                native.acquire()?;
                native
            };
            ProjectionDb::open_original_generated(
                Arc::clone(&self.factory),
                inputs,
                retained_directory,
                name.to_os_string(),
                lease,
                !selected && durable.is_none(),
                expected_identity,
                reference.definition(),
            )
        })();
        opened.map_err(|error| {
            anyhow::Error::new(OriginalProjectionSchemaAdmissionFailure {
                error,
                _reference: reference,
                _owner: self.authority.clone(),
            })
        })
    }
}
// A real exclusive file effect before SQLite admission, retained on every
// later setup refusal. No path-absence or Drop-based cleanup proof is minted.
struct OriginalProjectionLeaseSetupFailure {
    error: anyhow::Error,
    _lease: std::fs::File,
    _directory: lillux::PinnedDirectory,
    _name: std::ffi::OsString,
    _factory: Arc<lillux_sqlite::OriginalNativeFactory>,
}
impl std::fmt::Debug for OriginalProjectionLeaseSetupFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OriginalProjectionLeaseSetupFailure")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Display for OriginalProjectionLeaseSetupFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}
impl std::error::Error for OriginalProjectionLeaseSetupFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}

struct OriginalGeneratedProjectionAuthority {
    preparation: OriginalProjectionStateOwner,
    schema_reference: std::sync::Weak<crate::sqlite_schema::ClosedNativeSchemaSqlReference>,
    root: Arc<lillux::PinnedDirectory>,
    name: std::ffi::OsString,
    expected_identity: Option<(u64, u64)>,
}
impl OriginalGeneratedProjectionAuthority {
    fn require_binding(
        &self,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
    ) -> anyhow::Result<()> {
        self.preparation.require_binding(factory)?;
        self.schema_reference
            .upgrade()
            .context("original generated schema reference retired")?
            .terminal()
            .require_original(factory)?;
        self.root.ensure_path_binding()?;
        self.preparation.require_selected_runtime(&self.root)?;
        if let Some(expected) = self.expected_identity {
            let file = self
                .root
                .open_mount_entry(&self.name)?
                .context("original selected projection coordinate absent")?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                let metadata = file.metadata()?;
                anyhow::ensure!(
                    metadata.file_type().is_file() && (metadata.dev(), metadata.ino()) == expected,
                    "original selected projection coordinate changed"
                );
            }
            #[cfg(not(unix))]
            anyhow::bail!("original selected inode identity unavailable");
        }
        Ok(())
    }
}
impl lillux_sqlite::OriginalNativeDestinationAuthority for OriginalGeneratedProjectionAuthority {
    fn into_outside_custody(
        self: Arc<Self>,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        leaf: &lillux_sqlite::OriginalNativeOutsideLeafEvidence,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>,
    ) -> Result<
        lillux_sqlite::OriginalDestinationOutsideCustody,
        lillux_sqlite::OriginalDestinationCustodyFailure,
    > {
        destination_join::transfer_leaf(
            destination_join::StateDestinationLeaf::Generated(self),
            factory,
            aggregate,
            leaf,
            prepay,
        )
    }
    fn require_original(
        &self,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
    ) -> anyhow::Result<()> {
        self.require_binding(factory)
    }
}

// Retains an actual staged durable owner through any publication/reopen
// refusal. The recovery store still decides the real committed generation;
// this error neither rolls it back nor grants a replacement attempt.
struct GeneratedProjectionPublicationFailure {
    error: anyhow::Error,
    _staged: Arc<crate::projection::DurableOriginalProjection>,
    _recovery: RecoveryStore,
    _instance_id: String,
    _projection_file: String,
}
impl std::fmt::Debug for GeneratedProjectionPublicationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GeneratedProjectionPublicationFailure")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Display for GeneratedProjectionPublicationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}
impl std::error::Error for GeneratedProjectionPublicationFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}

// Native allocation authority remains distinct from CAS mutation authority.
// This Send+Sync retained destination contains no thread-affine CAS guard or
// fork-sensitive lease; each State mutation still validates its real guard.
struct OriginalPreparedProjectionAuthority {
    outside_join: Mutex<std::sync::Weak<destination_join::OriginalStateDestinationJoin>>,
    preparation: Mutex<Option<PreparedOfflineProjection>>,
    factory: std::sync::Weak<lillux_sqlite::OriginalNativeFactory>,
    // The original issuer/factory retains the real admission. After consuming
    // preparation, these destination receipts keep affinity without keeping a
    // paid resident budget alive through a strong admission backedge.
    lease_authority: std::sync::Weak<dyn lillux_sqlite::OriginalNativeProjectionLeaseAuthority>,
}
impl OriginalPreparedProjectionAuthority {
    fn require_binding(
        &self,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
    ) -> anyhow::Result<()> {
        let preparation = self.preparation.try_lock().map_err(|error| match error {
            std::sync::TryLockError::WouldBlock => {
                anyhow::Error::new(lillux_sqlite::NativeCheckContended)
                    .context("original State preparation owner busy")
            }
            std::sync::TryLockError::Poisoned(_) => {
                anyhow::anyhow!("original State preparation owner poisoned")
            }
        })?;
        let preparation = preparation
            .as_ref()
            .context("original State preparation transferred to finalizer")?;
        preparation.require_custody()?;
        anyhow::ensure!(
            preparation.namespace_lock.is_some(),
            "original State namespace exclusion absent"
        );
        let original = self
            .factory
            .upgrade()
            .context("original State factory owner absent")?;
        anyhow::ensure!(
            original.same_original(factory),
            "original State factory identity changed"
        );
        factory.require_runtime_binding(&preparation.runtime_state_directory)
    }
    fn require_selected_runtime(&self, directory: &lillux::PinnedDirectory) -> anyhow::Result<()> {
        let preparation = self.preparation.try_lock().map_err(|error| match error {
            std::sync::TryLockError::WouldBlock => {
                anyhow::Error::new(lillux_sqlite::NativeCheckContended)
                    .context("original State preparation owner busy")
            }
            std::sync::TryLockError::Poisoned(_) => {
                anyhow::anyhow!("original State preparation owner poisoned")
            }
        })?;
        let preparation = preparation
            .as_ref()
            .context("original State preparation transferred to finalizer")?;
        anyhow::ensure!(
            directory.is_same_directory(&preparation.runtime_state_directory)?,
            "original generated projection has foreign runtime"
        );
        Ok(())
    }
}
impl lillux_sqlite::OriginalNativeDestinationAuthority for OriginalPreparedProjectionAuthority {
    fn into_outside_custody(
        self: Arc<Self>,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        leaf: &lillux_sqlite::OriginalNativeOutsideLeafEvidence,
        prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>,
    ) -> Result<
        lillux_sqlite::OriginalDestinationOutsideCustody,
        lillux_sqlite::OriginalDestinationCustodyFailure,
    > {
        destination_join::transfer_leaf(
            destination_join::StateDestinationLeaf::Prepared(self),
            factory,
            aggregate,
            leaf,
            prepay,
        )
    }
    fn require_original(
        &self,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
    ) -> anyhow::Result<()> {
        self.require_binding(factory)
    }
}

/// Destination of the transient projection of a store opened by
/// [`StateDb::open_offline_native`]: the caller host's destination, gated on
/// a clone of the store's own captured State preparation. The projection
/// finishes in-process, so it has no outside custody receiver.
struct OfflineNativeStateDestination {
    preparation: PreparedOfflineProjection,
    host: Arc<dyn lillux_sqlite::OriginalNativeDestinationAuthority>,
}
impl lillux_sqlite::OriginalNativeDestinationAuthority for OfflineNativeStateDestination {
    fn require_original(
        &self,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
    ) -> anyhow::Result<()> {
        self.preparation.require_custody()?;
        anyhow::ensure!(
            self.preparation.namespace_lock.is_some(),
            "offline State namespace exclusion absent"
        );
        // Close and cleanup pass here after admission closes, so the factory's
        // admission-window runtime binding is checked once at open; the
        // host's destination enforces its own windows.
        self.host.require_original(factory)
    }
    fn into_outside_custody(
        self: Arc<Self>,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        _aggregate: &lillux_sqlite::OriginalNativeTerminal,
        _leaf: &lillux_sqlite::OriginalNativeOutsideLeafEvidence,
        _prepay: &dyn Fn(u64, u64) -> anyhow::Result<()>,
    ) -> Result<
        lillux_sqlite::OriginalDestinationOutsideCustody,
        lillux_sqlite::OriginalDestinationCustodyFailure,
    > {
        Err(lillux_sqlite::OriginalDestinationCustodyFailure::retain(
            factory.clone(),
            self,
            anyhow::anyhow!(
                "offline native State projection finishes in-process; no outside custody receiver"
            ),
        ))
    }
}

/// Cloneable, descriptor-bound authority for maintenance work that must run
/// after releasing the [`StateDb`] mutex.
///
/// Paths are retained only for diagnostics. Every authoritative operation is
/// rooted in these already-open directory descriptors and the exact trust
/// snapshot owned by the originating state handle.
pub struct PinnedStateAuthority {
    // Offline authority clones retain the SAME exclusion; no new flock or FD.
    offline_namespace_lock: Option<lillux::PinnedDirectoryLock>,
    runtime_directory: lillux::PinnedDirectory,
    cas_directory: lillux::PinnedDirectory,
    refs_directory: lillux::PinnedDirectory,
    recovery: Option<RecoveryStore>,
    trust_store: Arc<TrustStore>,
}

/// Physical sources retained by a drained Store for its consuming close path.
/// These are the exact descriptors captured by `PinnedStateAuthority`; the
/// finalizer registers them before it releases this owner. `recovery` contains
/// shared aliases to roots already owned by the State recovery frame and must
/// be joined to those exact registry memberships before it is consumed.
pub struct PinnedStateAuthorityFinalizationSources {
    pub offline_namespace_lock: Option<lillux::PinnedDirectoryLock>,
    pub runtime_directory: Option<lillux::PinnedDirectory>,
    pub cas_directory: Option<lillux::PinnedDirectory>,
    pub refs_directory: Option<lillux::PinnedDirectory>,
    pub recovery: Option<RecoveryStore>,
}

/// A valid continuation boundary which begins a different operator/remote
/// operation, rather than malformed or unavailable retained history.
#[derive(Debug)]
pub struct MachineContinuationBoundary;

impl std::fmt::Display for MachineContinuationBoundary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("machine continuation cannot cross an operator or remote boundary")
    }
}

impl std::error::Error for MachineContinuationBoundary {}

impl PinnedStateAuthority {
    /// Transfer the exact retained host descriptors after the Store has
    /// drained. This grants no new State permission and does not duplicate or
    /// reopen any path; callers must retain these owners through registry
    /// registration and the matching physical close receipts.
    pub fn into_finalization_sources(self) -> PinnedStateAuthorityFinalizationSources {
        let Self {
            offline_namespace_lock,
            runtime_directory,
            cas_directory,
            refs_directory,
            recovery,
            trust_store: _,
        } = self;
        PinnedStateAuthorityFinalizationSources {
            offline_namespace_lock,
            runtime_directory: Some(runtime_directory),
            cas_directory: Some(cas_directory),
            refs_directory: Some(refs_directory),
            recovery,
        }
    }

    /// Exact continuation lineage through one signed head, without a runtime
    /// database or projection lookup. The caller supplies an existing CAS guard.
    pub fn read_machine_continuation_lineage(
        &self,
        chain_root_id: &str,
        terminal_thread_id: &str,
        guard: &crate::recovery::CasMutationGuard,
    ) -> anyhow::Result<Option<(String, Vec<ThreadSnapshot>)>> {
        self.read_machine_continuation_segment(
            chain_root_id,
            chain_root_id,
            terminal_thread_id,
            guard,
        )
    }

    /// The exact machine segment starting at an already admitted caller. An
    /// earlier operator boundary in the same chain is outside this operation.
    pub fn read_machine_continuation_segment(
        &self,
        chain_root_id: &str,
        start_thread_id: &str,
        terminal_thread_id: &str,
        guard: &crate::recovery::CasMutationGuard,
    ) -> anyhow::Result<Option<(String, Vec<ThreadSnapshot>)>> {
        self.ensure_guard(guard)?;
        let chain_lock = crate::chain::ChainLock::acquire_in_refs_directory(
            &self.refs_directory,
            &self.cas_directory,
            self.require_recovery()?,
            chain_root_id,
        )?;
        chain::read_machine_continuation_lineage_with_trust(
            self.cas_directory.path(),
            self.refs_directory.path(),
            &chain_lock,
            chain_root_id,
            start_thread_id,
            terminal_thread_id,
            1024,
            65_536,
            64 * 1024 * 1024,
            self.trust_store.as_ref(),
            &mut HeadCache::new(),
        )
    }

    /// Read exact thread authority without a runtime database or global
    /// projection mutex. The request-local cache is only a verified-head memo.
    ///
    /// Events are visited newest-first under the same signed head as the
    /// returned snapshot. The callback must only accumulate local evidence:
    /// it must not mutate state or acquire another chain lock. Its partial
    /// observations are unusable unless this entire method succeeds.
    pub fn visit_thread_snapshot_events(
        &self,
        chain_root_id: &str,
        thread_id: &str,
        max_events: u64,
        max_bytes: u64,
        guard: &crate::recovery::CasMutationGuard,
        visit: impl FnMut(String, crate::objects::ThreadEvent),
    ) -> anyhow::Result<Option<(String, ThreadSnapshot)>> {
        self.ensure_guard(guard)?;
        let chain_lock = crate::chain::ChainLock::acquire_in_refs_directory(
            &self.refs_directory,
            &self.cas_directory,
            self.require_recovery()?,
            chain_root_id,
        )?;
        chain::visit_thread_snapshot_events_with_trust(
            self.cas_directory.path(),
            self.refs_directory.path(),
            &chain_lock,
            chain_root_id,
            thread_id,
            max_events,
            max_bytes,
            self.trust_store.as_ref(),
            &mut HeadCache::new(),
            visit,
        )
    }

    /// Read exact thread authority without a runtime database or projection lookup.
    pub fn read_thread_snapshot_with_event_presence(
        &self,
        chain_root_id: &str,
        thread_id: &str,
        event_type: &str,
        max_events: u64,
        max_bytes: u64,
        guard: &crate::recovery::CasMutationGuard,
    ) -> anyhow::Result<Option<(String, ThreadSnapshot, bool)>> {
        self.ensure_guard(guard)?;
        let chain_lock = crate::chain::ChainLock::acquire_in_refs_directory(
            &self.refs_directory,
            &self.cas_directory,
            self.require_recovery()?,
            chain_root_id,
        )?;
        chain::read_thread_snapshot_with_event_presence_with_trust(
            self.cas_directory.path(),
            self.refs_directory.path(),
            &chain_lock,
            chain_root_id,
            thread_id,
            event_type,
            max_events,
            max_bytes,
            self.trust_store.as_ref(),
            &mut HeadCache::new(),
        )
    }

    /// Duplicate the already-pinned authority without resolving any mutable
    /// pathname. Long-lived staged publications use this to retain the exact
    /// runtime/CAS/ref generation beyond one synchronous capture phase.
    pub fn try_clone(&self) -> anyhow::Result<Self> {
        Ok(Self {
            offline_namespace_lock: self.offline_namespace_lock.clone(),
            runtime_directory: self.runtime_directory.try_clone()?,
            cas_directory: self.cas_directory.try_clone()?,
            refs_directory: self.refs_directory.try_clone()?,
            recovery: self.recovery.clone(),
            trust_store: Arc::clone(&self.trust_store),
        })
    }

    pub(crate) fn from_pinned_runtime(
        runtime_directory: lillux::PinnedDirectory,
        trust_store: Arc<TrustStore>,
        create_recovery: bool,
    ) -> anyhow::Result<Self> {
        let cas_directory = runtime_directory
            .open_child_directory(std::ffi::OsStr::new("objects"))?
            .ok_or_else(|| anyhow::anyhow!("CAS root is absent"))?;
        let refs_directory = runtime_directory
            .open_child_directory(std::ffi::OsStr::new("refs"))?
            .ok_or_else(|| anyhow::anyhow!("refs root is absent"))?;
        let recovery = if create_recovery {
            let recovery_parent =
                runtime_directory.open_or_create_child(std::ffi::OsStr::new("recovery"), 0o700)?;
            let recovery_directory = recovery_parent
                .open_or_create_child(std::ffi::OsStr::new("thread-projection"), 0o700)?;
            Some(RecoveryStore::from_pinned_directories(
                runtime_directory.try_clone()?,
                recovery_directory,
            )?)
        } else {
            match runtime_directory.open_child_directory(std::ffi::OsStr::new("recovery"))? {
                Some(recovery_parent) => match recovery_parent
                    .open_child_directory(std::ffi::OsStr::new("thread-projection"))?
                {
                    Some(recovery_directory) => Some(RecoveryStore::from_pinned_directories(
                        runtime_directory.try_clone()?,
                        recovery_directory,
                    )?),
                    None => None,
                },
                None => None,
            }
        };
        Ok(Self {
            offline_namespace_lock: None,
            runtime_directory,
            cas_directory,
            refs_directory,
            recovery,
            trust_store,
        })
    }

    /// Reconstitute the exact descriptor-pinned state authority used by a
    /// dedicated external-candidate launcher. This boundary deliberately has
    /// an empty trust store and cannot validate signed heads or manufacture
    /// project admission. It may access only the already-present CAS, refs and
    /// recovery roots required to materialize and durably capture the exact
    /// admitted snapshot named by the launcher binding.
    pub fn from_external_candidate_runtime(
        runtime_directory: lillux::PinnedDirectory,
    ) -> anyhow::Result<Self> {
        Self::from_pinned_runtime(runtime_directory, Arc::new(TrustStore::new()), false)
    }

    pub fn runtime_directory(&self) -> &lillux::PinnedDirectory {
        &self.runtime_directory
    }

    pub fn cas_store(&self) -> anyhow::Result<lillux::CasStore> {
        Ok(lillux::CasStore::from_pinned_root(
            self.cas_directory.try_clone()?,
        ))
    }

    /// The large-content-tier large-object store, rooted beside the CAS under this
    /// exact pinned runtime generation.
    pub fn large_object_store(
        &self,
    ) -> anyhow::Result<crate::large_object_store::LargeObjectStore> {
        crate::large_object_store::LargeObjectStore::open_or_create_under(&self.runtime_directory)
    }

    pub fn cas_directory(&self) -> &lillux::PinnedDirectory {
        &self.cas_directory
    }

    pub fn refs_directory(&self) -> &lillux::PinnedDirectory {
        &self.refs_directory
    }

    pub fn recovery(&self) -> Option<&RecoveryStore> {
        self.recovery.as_ref()
    }

    pub fn require_recovery(&self) -> anyhow::Result<&RecoveryStore> {
        self.recovery
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("thread-projection recovery authority is absent"))
    }

    pub fn trust_store(&self) -> &TrustStore {
        self.trust_store.as_ref()
    }

    pub fn ensure_guard(&self, guard: &crate::recovery::CasMutationGuard) -> anyhow::Result<()> {
        if let Some(namespace) = &self.offline_namespace_lock {
            namespace.ensure_protects(&self.runtime_directory)?;
        }
        guard.ensure_protects_pinned_runtime(&self.runtime_directory)
    }

    /// Acquire the already-established shared CAS mutation guard relative to
    /// this exact pinned runtime authority. Ordinary publishers use this path
    /// so a renamed path cannot rebind their CAS/refs/recovery relationship.
    pub fn acquire_shared_guard(&self) -> anyhow::Result<crate::recovery::CasMutationGuard> {
        crate::recovery::CasMutationGuard::acquire_existing_shared_in_pinned_runtime(
            &self.runtime_directory,
        )
    }

    /// Acquire the SAME established pinned shared guard without renewing the
    /// caller's operation deadline. This bounds contention, not arbitrary I/O.
    pub fn acquire_shared_guard_until(
        &self,
        deadline: lillux::time::MonotonicDeadline,
    ) -> anyhow::Result<crate::recovery::CasMutationGuard> {
        crate::recovery::CasMutationGuard::acquire_existing_shared_in_pinned_runtime_until(
            &self.runtime_directory,
            deadline,
        )
    }

    /// Acquire the exclusive CAS mutation guard relative to this exact runtime
    /// authority. Mutable maintenance may establish the lock anchor; strict
    /// read-only maintenance requires the anchor to already exist.
    pub fn acquire_exclusive_guard(
        &self,
        create_anchor: bool,
    ) -> anyhow::Result<crate::recovery::CasMutationGuard> {
        if create_anchor {
            crate::recovery::CasMutationGuard::acquire_exclusive_in_pinned_runtime(
                &self.runtime_directory,
            )
        } else {
            crate::recovery::CasMutationGuard::acquire_existing_exclusive_in_pinned_runtime(
                &self.runtime_directory,
            )
        }
    }
}

enum OperationalAccess {
    Available(OperationalDb),
    ProjectionMaintenance,
}

#[cfg(test)]
impl StateDb {
    /// The live State parent of an open store, for frames built directly in
    /// tests.
    pub(crate) fn live_native_owner_for_test(&self) -> Arc<live_native::LiveStateNativeOwner> {
        match &self
            .original_projection_context
            .as_ref()
            .expect("live State context")
            .authority
        {
            OriginalProjectionStateOwner::Live(owner) => Arc::clone(owner),
            OriginalProjectionStateOwner::Offline(_) => panic!("a live State has a live parent"),
        }
    }
}
impl StateDb {
    /// Open established authority/recovery roots for an explicit stopped-node
    /// maintenance operation, with the current-schema transient projection
    /// on a ticket of the caller's native `factory`. Nothing is selected,
    /// replayed or persisted; the operation works on the authority roots.
    /// Close it with [`Self::close_offline_native`], which hands back the
    /// projection's native terminal for the caller's census.
    ///
    /// # Safety
    /// The caller must hold the exclusive stopped-node StateLock and the
    /// runtime namespace lock `namespace` for the whole life of the store.
    /// `factory` and `destination` must come from one native host the caller
    /// funded under that same exclusion, and `destination` must revalidate
    /// that exclusion on every native boundary.
    pub unsafe fn open_offline_native(
        runtime_state_directory: lillux::PinnedDirectory,
        namespace: &lillux::PinnedDirectoryLock,
        trust_store: Arc<TrustStore>,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        destination: Arc<dyn lillux_sqlite::OriginalNativeDestinationAuthority>,
    ) -> anyhow::Result<Self> {
        factory.require_original_process()?;
        factory.require_runtime_binding(&runtime_state_directory)?;
        let prepared =
            PreparedOfflineProjection::capture(runtime_state_directory, trust_store, namespace)?;
        let state_destination = Arc::new(OfflineNativeStateDestination {
            preparation: prepared.try_clone_preparation()?,
            host: destination,
        });
        lillux_sqlite::OriginalNativeDestinationAuthority::require_original(
            state_destination.as_ref(),
            factory,
        )?;
        // SAFETY: StateTransient requires the actual prepared State custody.
        // `state_destination` retains a clone of the same captured roots,
        // recovery, trust and namespace exclusion this store keeps, and
        // revalidates them and the caller's host destination on every native
        // boundary; the factory's runtime binding was checked above; the
        // caller's funded host issues the ticket under the exclusion this
        // function's contract requires.
        let inputs = unsafe {
            lillux_sqlite::OriginalNativeTransientInputs::issued(
                lillux_sqlite::OriginalNativeDomain::StateTransient,
                state_destination,
            )?
        };
        let projection = ProjectionDb::open_original_transient(Arc::clone(factory), inputs)?;
        Ok(prepared.into_state(projection, None))
    }

    /// Open established authority/recovery roots for a stopped-node or
    /// standalone projection rebuild on the caller's in-process host. Like
    /// [`Self::open_offline_native`], with a native projection build context
    /// on the same factory, so a rebuild opens its generated projection, its
    /// schema references and its generation lease through `factory` and
    /// `lease_authority`. Close it with [`Self::close_offline_native`].
    ///
    /// # Safety
    /// The caller must hold the exclusive stopped-node StateLock (or the
    /// standalone exclusion) and the runtime namespace lock `namespace` for
    /// the whole life of the store. `factory` and `lease_authority` come from
    /// one host the caller funded under that exclusion; the caller retains
    /// `lease_authority` until the host releases, and it revalidates the
    /// exclusion on every native boundary.
    pub unsafe fn open_offline_native_for_rebuild(
        runtime_state_directory: lillux::PinnedDirectory,
        namespace: &lillux::PinnedDirectoryLock,
        trust_store: Arc<TrustStore>,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        lease_authority: Arc<dyn lillux_sqlite::OriginalNativeProjectionLeaseAuthority>,
    ) -> anyhow::Result<Self> {
        factory.require_original_process()?;
        factory.require_runtime_binding(&runtime_state_directory)?;
        lease_authority.require_original(factory, false)?;
        let prepared =
            PreparedOfflineProjection::capture(runtime_state_directory, trust_store, namespace)?;
        let state_owner = prepared.try_clone_preparation()?;
        let native_owner = Arc::new(OriginalPreparedProjectionAuthority {
            preparation: Mutex::new(Some(prepared)),
            factory: Arc::downgrade(factory),
            lease_authority: Arc::downgrade(&lease_authority),
            outside_join: Mutex::new(std::sync::Weak::new()),
        });
        native_owner.require_binding(factory)?;
        // Reserve the issuance record before the first native owner exists.
        let issued_destinations =
            OriginalProjectionBuildContext::issued_destination_slots(factory)?;
        // SAFETY: the prepared State owner retains the captured roots, trust,
        // recovery and namespace exclusion and revalidates them on every
        // native boundary; the factory's runtime binding was checked above and
        // the caller's funded host issues the ticket under that exclusion.
        let inputs = unsafe {
            lillux_sqlite::OriginalNativeTransientInputs::issued(
                lillux_sqlite::OriginalNativeDomain::StateTransient,
                native_owner.clone(),
            )?
        };
        let projection = ProjectionDb::open_original_transient(Arc::clone(factory), inputs)?;
        Ok(state_owner.into_state(
            projection,
            Some(OriginalProjectionBuildContext {
                factory: Arc::clone(factory),
                authority: OriginalProjectionStateOwner::Offline(native_owner),
                terminals: Vec::new(),
                durable: Vec::new(),
                retired_selected: Vec::new(),
                schema_references: Mutex::new(Vec::new()),
                generation_leases: Mutex::new(Vec::new()),
                generation_lease_attempts: std::sync::atomic::AtomicU32::new(0),
                issued_destinations,
            }),
        ))
    }

    /// Close a store opened by [`Self::open_offline_native`] or
    /// [`Self::open_offline_native_for_rebuild`] into its native census:
    /// the current projection, every predecessor projection a rebuild
    /// retired, every connection the build context closed (staged generated
    /// projections and schema references), and every generation lease,
    /// closed after its consumers. `prior_refusal` is always `None`.
    ///
    /// A store that was not opened offline (no retained namespace exclusion,
    /// or Operational access), or whose build context belongs to another
    /// factory or to a live State, is refused whole and handed back. A
    /// refusal after the projection began closing carries every remaining
    /// native owner in the error.
    pub fn close_offline_native(
        self,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
    ) -> std::result::Result<LiveStateCensus, (Option<Self>, anyhow::Error)> {
        let checked = (|| -> anyhow::Result<()> {
            factory.require_original_process()?;
            anyhow::ensure!(
                matches!(self.operational, OperationalAccess::ProjectionMaintenance)
                    && self._offline_namespace_lock.is_some(),
                "offline native close refused: State was not opened offline under retained namespace exclusion"
            );
            if let Some(context) = &self.original_projection_context {
                anyhow::ensure!(
                    context.factory.same_original(factory)
                        && matches!(&context.authority, OriginalProjectionStateOwner::Offline(_)),
                    "offline native close refused: foreign or live projection build context"
                );
            }
            for retired in &self.original_projection_terminals {
                retired.require_original(factory)?;
            }
            Ok(())
        })();
        if let Err(error) = checked {
            return Err((Some(self), error));
        }
        let Self {
            _runtime_state_directory,
            _offline_namespace_lock,
            _cas_directory,
            _refs_directory,
            _locators_directory,
            _recovery_directory,
            cas_root: _,
            refs_root: _,
            locators_root: _,
            projection,
            original_projection_terminals,
            original_projection_context,
            operational: _,
            recovery,
            trust_store: _,
            head_cache: _,
            projection_repair_sink: _,
        } = self;
        let closed = match projection.close_original(factory) {
            Ok(closed) => closed,
            Err(failure) => {
                let error = anyhow::anyhow!("close offline native State projection: {failure}");
                return Err((
                    None,
                    crate::operational::CarriedOwners::carry(
                        crate::operational::CarriedOwners::carry(error, failure),
                        (
                            original_projection_context,
                            original_projection_terminals,
                            recovery,
                            _offline_namespace_lock,
                            _runtime_state_directory,
                        ),
                    ),
                ));
            }
        };
        let (outside, proof) = closed.into_outside_custody();
        let collected = (|| -> anyhow::Result<(Vec<lillux_sqlite::OriginalNativeConnectionTerminal>,
            Vec<Arc<lillux_sqlite::ClosedOriginalProjectionLease>>,
            Vec<Arc<crate::projection::OriginalGenerationLease>>)> {
            let mut owners = Vec::new();
            let mut closed_connections = vec![proof];
            let mut push = |connection: Arc<lillux_sqlite::ClosedOriginalNativeConnection>| {
                if !closed_connections.iter().any(|prior| Arc::ptr_eq(prior, &connection)) {
                    closed_connections.push(connection);
                }
            };
            for retired in &original_projection_terminals {
                push(retired.native_proof());
            }
            let mut terminals = Vec::new();
            let mut leases = Vec::new();
            if let Some(context) = &original_projection_context {
                for terminal in &context.terminals {
                    push(Arc::clone(terminal));
                }
                for retired in &context.retired_selected {
                    retired.require_original(factory)?;
                    push(retired.native_proof());
                }
                let references = context.schema_references.try_lock()
                    .map_err(|_| anyhow::anyhow!("offline schema receipt custody busy"))?.clone();
                for reference in &references {
                    reference.require_original_closed(factory, lillux_sqlite::OriginalNativeDomain::NodeProjectionSchemaReference)?;
                    terminals.push(reference.terminal().clone());
                }
                let generation_leases = context.generation_leases.try_lock()
                    .map_err(|_| anyhow::anyhow!("offline generation lease registry busy"))?.clone();
                owners.extend(generation_leases.iter().cloned());
                for lease in generation_leases {
                    // SAFETY: every generation consumer, the current, staged
                    // and retired projections, closed above or earlier; an
                    // offline store published no service consumer.
                    if let Some(terminal) = unsafe { lease.close_after_consumers(factory) }?
                        && !leases.iter().any(|prior| Arc::ptr_eq(prior, &terminal)) { leases.push(terminal); }
                }
            }
            let mut census: Vec<_> = closed_connections.into_iter()
                .map(lillux_sqlite::OriginalNativeConnectionTerminal::Closed).collect();
            census.append(&mut terminals);
            for terminal in &census { terminal.require_original(factory)?; }
            Ok((census, leases, owners))
        })();
        match collected {
            Ok((terminals, leases, owners)) => {
                // The census holds every native owner. The projection's file
                // custody, the durable proofs, the context and the roots end
                // here, the namespace exclusion last.
                drop(outside);
                drop(original_projection_context);
                drop(original_projection_terminals);
                drop((
                    recovery,
                    _cas_directory,
                    _refs_directory,
                    _locators_directory,
                    _recovery_directory,
                ));
                drop((_runtime_state_directory, _offline_namespace_lock));
                Ok(LiveStateCensus {
                    terminals,
                    leases,
                    lease_retirement: GenerationLeaseRetirement::new(owners),
                    prior_refusal: None,
                })
            }
            Err(error) => Err((
                None,
                crate::operational::CarriedOwners::carry(
                    error.context("offline native State census refused"),
                    (
                        outside,
                        original_projection_context,
                        original_projection_terminals,
                        recovery,
                        _offline_namespace_lock,
                        _runtime_state_directory,
                    ),
                ),
            )),
        }
    }

    /// Open exactly the selected current projection generation for fail-only
    /// verification on an in-process host (an offline or standalone host).
    /// Close it with [`Self::close_verification_native`].
    ///
    /// # Safety
    /// The caller holds the exclusive namespace lock `namespace` for the whole
    /// life of the store. `factory` and `lease_authority` come from one host
    /// the caller funded under that exclusion, retain it until the host
    /// releases, and revalidate it on every native boundary.
    pub unsafe fn open_offline_native_verification(
        runtime_state_directory: lillux::PinnedDirectory,
        namespace: &lillux::PinnedDirectoryLock,
        trust_store: Arc<TrustStore>,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        lease_authority: Arc<dyn lillux_sqlite::OriginalNativeProjectionLeaseAuthority>,
    ) -> anyhow::Result<Self> {
        factory.require_original_process()?;
        factory.require_runtime_binding(&runtime_state_directory)?;
        let prepared =
            PreparedOfflineProjection::capture(runtime_state_directory, trust_store, namespace)?;
        // SAFETY: forwarded from this function's contract.
        unsafe { prepared.open_selected_for_verification(Arc::clone(factory), lease_authority) }
    }

    /// Close a store opened by [`Self::open_offline_native_verification`]
    /// into its native census: the selected projection, its schema reference
    /// and its generation lease. A store that holds Operational access, that
    /// was not opened offline, or that built or retired a generation, is
    /// refused whole. A refusal after the projection began closing carries
    /// every remaining native owner in the error.
    pub fn close_verification_native(
        self,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
    ) -> std::result::Result<LiveStateCensus, (Option<Self>, anyhow::Error)> {
        let checked = (|| -> anyhow::Result<()> {
            factory.require_original_process()?;
            anyhow::ensure!(
                matches!(self.operational, OperationalAccess::ProjectionMaintenance)
                    && self._offline_namespace_lock.is_some(),
                "verification close refused: State was not opened offline under retained namespace exclusion"
            );
            anyhow::ensure!(
                self.original_projection_terminals.is_empty(),
                "verification close refused: State retired a projection"
            );
            let context = self
                .original_projection_context
                .as_ref()
                .context("verification close refused: State lacks its native projection context")?;
            anyhow::ensure!(
                context.factory.same_original(factory)
                    && matches!(&context.authority, OriginalProjectionStateOwner::Offline(_)),
                "verification close refused: foreign or live projection context"
            );
            anyhow::ensure!(
                context.durable.is_empty() && context.retired_selected.is_empty(),
                "verification close refused: State built or retired a projection generation"
            );
            Ok(())
        })();
        if let Err(error) = checked {
            return Err((Some(self), error));
        }
        let Self {
            _runtime_state_directory,
            _offline_namespace_lock,
            _cas_directory,
            _refs_directory,
            _locators_directory,
            _recovery_directory,
            cas_root: _,
            refs_root: _,
            locators_root: _,
            projection,
            original_projection_terminals: _,
            original_projection_context,
            operational: _,
            recovery,
            trust_store: _,
            head_cache: _,
            projection_repair_sink: _,
        } = self;
        let context = original_projection_context.expect("checked verification projection context");
        let closed = match projection.close_original(factory) {
            Ok(closed) => closed,
            Err(failure) => {
                let error = anyhow::anyhow!("close verification projection: {failure}");
                return Err((
                    None,
                    crate::operational::CarriedOwners::carry(
                        crate::operational::CarriedOwners::carry(error, failure),
                        (
                            context,
                            recovery,
                            _offline_namespace_lock,
                            _runtime_state_directory,
                        ),
                    ),
                ));
            }
        };
        let (outside, proof) = closed.into_outside_custody();
        let collected = (|| -> anyhow::Result<(Vec<lillux_sqlite::OriginalNativeConnectionTerminal>,
            Vec<Arc<lillux_sqlite::ClosedOriginalProjectionLease>>,
            Vec<Arc<crate::projection::OriginalGenerationLease>>)> {
            let mut terminals = vec![lillux_sqlite::OriginalNativeConnectionTerminal::Closed(proof)];
            for terminal in &context.terminals {
                terminals.push(lillux_sqlite::OriginalNativeConnectionTerminal::Closed(Arc::clone(terminal)));
            }
            let references = context.schema_references.try_lock()
                .map_err(|_| anyhow::anyhow!("verification schema receipt custody busy"))?.clone();
            for reference in &references {
                reference.require_original_closed(factory, lillux_sqlite::OriginalNativeDomain::NodeProjectionSchemaReference)?;
                terminals.push(reference.terminal().clone());
            }
            let generation_leases = context.generation_leases.try_lock()
                .map_err(|_| anyhow::anyhow!("verification generation lease registry busy"))?.clone();
            let mut leases = Vec::new();
            for lease in generation_leases.iter() {
                // SAFETY: the only generation consumer, the selected
                // projection, closed above; this store published nothing.
                if let Some(terminal) = unsafe { lease.close_after_consumers(factory) }?
                    && !leases.iter().any(|prior| Arc::ptr_eq(prior, &terminal)) { leases.push(terminal); }
            }
            Ok((terminals, leases, generation_leases))
        })();
        match collected {
            Ok((terminals, leases, owners)) => {
                // The census holds every native owner; the projection's file
                // custody, the context and the roots end here.
                drop(outside);
                drop(context);
                drop((
                    recovery,
                    _cas_directory,
                    _refs_directory,
                    _locators_directory,
                    _recovery_directory,
                ));
                drop((_offline_namespace_lock, _runtime_state_directory));
                Ok(LiveStateCensus {
                    terminals,
                    leases,
                    lease_retirement: GenerationLeaseRetirement::new(owners),
                    prior_refusal: None,
                })
            }
            Err(error) => Err((
                None,
                crate::operational::CarriedOwners::carry(
                    error.context("verification projection census refused"),
                    (
                        outside,
                        context,
                        recovery,
                        _offline_namespace_lock,
                        _runtime_state_directory,
                    ),
                ),
            )),
        }
    }

    /// Actual live State opening under the same paid factory and host exclusion.
    /// Native destination admission is issued only after retaining State roots.
    /// # Safety
    /// The host and lease authority must be the actual retained original node
    /// issuer for this factory, process, signed funding and service/cleanup
    /// clock. A caller-defined trait implementation is not proof of issuance.
    /// Namespace exclusion and these lifelines must outlive all native effects,
    /// including partial-start recovery and final lease/resource settlement.
    pub unsafe fn open_live_with_recovery_runtime_liveness_and_namespace_authority(
        runtime_state_dir: &Path,
        runtime_state_directory: lillux::PinnedDirectory,
        runtime_state_lock: lillux::PinnedDirectoryLock,
        projection_repair_sink: Arc<dyn ProjectionRepairSink>,
        trust_store: Arc<TrustStore>,
        observer: Option<Arc<dyn ProjectionRecoveryObserver>>,
        runtime_liveness: &dyn RuntimeLivenessInspector,
        factory: Arc<lillux_sqlite::OriginalNativeFactory>,
        host: Arc<dyn lillux_sqlite::OriginalNativeDestinationAuthority>,
        lease_authority: Arc<dyn lillux_sqlite::OriginalNativeProjectionLeaseAuthority>,
        deadline: lillux::time::MonotonicDeadline,
    ) -> anyhow::Result<Self> {
        Self::open_with_options(
            runtime_state_dir,
            projection_repair_sink,
            trust_store,
            observer,
            runtime_liveness,
            RuntimeStateNamespaceAuthority {
                directory: runtime_state_directory,
                lock: runtime_state_lock,
            },
            live_native::LiveStateNativeHost {
                factory,
                authority: host,
                lease_authority,
                deadline,
            },
        )
    }

    fn open_with_options(
        runtime_state_dir: &Path,
        projection_repair_sink: Arc<dyn ProjectionRepairSink>,
        trust_store: Arc<TrustStore>,
        observer: Option<Arc<dyn ProjectionRecoveryObserver>>,
        runtime_liveness: &dyn RuntimeLivenessInspector,
        namespace_authority: RuntimeStateNamespaceAuthority,
        native_host: live_native::LiveStateNativeHost,
    ) -> anyhow::Result<Self> {
        let RuntimeStateNamespaceAuthority {
            directory: runtime_state_directory,
            lock: runtime_state_lock,
        } = namespace_authority;
        if runtime_state_directory.path() != runtime_state_dir {
            anyhow::bail!(
                "runtime-state namespace authority path mismatch: selected={}, requested={}",
                runtime_state_directory.path().display(),
                runtime_state_dir.display()
            );
        }
        runtime_state_lock
            .ensure_protects(&runtime_state_directory)
            .context("verify runtime-state namespace authority")?;
        let cas_root = runtime_state_dir.join("objects");
        let refs_root = runtime_state_dir.join("refs");
        let locators_root = runtime_state_dir.join("locators");
        let cas_directory = runtime_state_directory
            .open_or_create_child(std::ffi::OsStr::new("objects"), 0o777)
            .context("creating objects root")?;
        let refs_directory = runtime_state_directory
            .open_or_create_child(std::ffi::OsStr::new("refs"), 0o777)
            .context("creating refs root")?;
        let locators_directory = runtime_state_directory
            .open_or_create_child(std::ffi::OsStr::new("locators"), 0o777)
            .context("creating locators root")?;
        let recovery_parent = runtime_state_directory
            .open_or_create_child(std::ffi::OsStr::new("recovery"), 0o700)
            .context("creating recovery parent")?;
        let recovery_directory = recovery_parent
            .open_or_create_child(std::ffi::OsStr::new("thread-projection"), 0o700)
            .context("creating thread-projection recovery root")?;
        let recovery = RecoveryStore::from_pinned_directories(
            runtime_state_directory.try_clone()?,
            recovery_directory.try_clone()?,
        )?;
        if recovery.thread_history_discard_in_progress()? {
            anyhow::bail!(
                "offline execution-history reset is incomplete; rerun `ryeos node reset execution-history --confirm` while the daemon is stopped"
            );
        }
        // The exclusive namespace lock proves no State writer is live.
        let removed = crate::recovery::sweep_interrupted_state_publications(
            &recovery_parent,
            &recovery_directory,
        )?;
        if removed != 0 {
            tracing::info!(removed, "removed interrupted State publications");
        }

        // Operational rows are source-of-truth state and live in a fixed store
        // that is independent of replaceable projection generations.
        // Reserve the issuance record before any native owner exists.
        let issued_destinations =
            OriginalProjectionBuildContext::issued_destination_slots(&native_host.factory)?;
        let native_owner = live_native::LiveStateNativeOwner::capture(
            native_host,
            &runtime_state_directory,
            &runtime_state_lock,
            &cas_directory,
            &refs_directory,
            &locators_directory,
            &recovery_directory,
            &recovery,
            Arc::clone(&trust_store),
        )?;
        let operational = OperationalDb::open_live_from_state_owner(Arc::clone(&native_owner))
            .context("open stable operational state")?;
        let projection_context = OriginalProjectionBuildContext {
            factory: Arc::clone(native_owner.factory()),
            authority: OriginalProjectionStateOwner::Live(Arc::clone(&native_owner)),
            terminals: Vec::new(),
            durable: Vec::new(),
            retired_selected: Vec::new(),
            schema_references: Mutex::new(Vec::new()),
            generation_leases: Mutex::new(Vec::new()),
            generation_lease_attempts: std::sync::atomic::AtomicU32::new(0),
            issued_destinations,
        };
        let mut custody = live_native::LiveStateOpenCustody {
            original: native_owner,
            operational: Some(operational),
            projection_context: Some(projection_context),
        };

        let context = custody
            .projection_context
            .as_mut()
            .expect("live State projection build context held by its custody");
        let projection = open_recovered_projection(
            runtime_state_dir,
            &cas_root,
            &refs_root,
            &recovery,
            trust_store.as_ref(),
            Some(runtime_liveness),
            observer.as_deref(),
            context,
        );
        let projection = match projection {
            Ok(projection) => projection,
            Err(error) => return Err(custody.fail(error)),
        };
        let operational = custody
            .operational
            .take()
            .expect("completed Operational owner");
        let original_projection_context = custody.projection_context.take();
        Ok(Self {
            _runtime_state_directory: runtime_state_directory,
            _offline_namespace_lock: None,
            _cas_directory: cas_directory,
            _refs_directory: refs_directory,
            _locators_directory: locators_directory,
            _recovery_directory: recovery_directory,
            cas_root,
            refs_root,
            locators_root,
            projection,
            original_projection_terminals: Vec::new(),
            original_projection_context,
            operational: OperationalAccess::Available(operational),
            recovery,
            trust_store,
            head_cache: Mutex::new(HeadCache::new()),
            projection_repair_sink,
        })
    }

    fn committed_write_locked<T>(
        &self,
        value: T,
        chain_root_id: &str,
        committed_head_hash: &str,
        operation: &'static str,
        projected: anyhow::Result<()>,
        chain_lock: &crate::chain::ChainLock,
    ) -> CommittedWrite<T> {
        let projected = projected
            .and_then(|()| self.acknowledge_projected_set_locked(chain_root_id, chain_lock));
        let projection = match projected {
            Ok(()) => ProjectionStatus::Current,
            Err(error) => {
                let request = ProjectionRepairRequest {
                    chain_root_id: chain_root_id.to_owned(),
                    committed_head_hash: committed_head_hash.to_owned(),
                    operation,
                    error: format!("{error:#}"),
                };
                self.projection_repair_sink.request_repair(request.clone());
                ProjectionStatus::RepairRequired(request)
            }
        };
        CommittedWrite::new(value, projection)
    }

    fn note_pending_transition_error(
        &self,
        chain_root_id: &str,
        operation: &'static str,
        error: &anyhow::Error,
    ) {
        let pending = match self.recovery.read_pending(chain_root_id) {
            Ok(Some(pending)) => pending,
            Ok(None) => return,
            Err(read_error) => {
                self.projection_repair_sink
                    .request_repair(ProjectionRepairRequest {
                        chain_root_id: chain_root_id.to_string(),
                        committed_head_hash: String::new(),
                        operation,
                        error: format!(
                            "{error:#}; pending transition could not be inspected: {read_error:#}"
                        ),
                    });
                return;
            }
        };
        self.projection_repair_sink
            .request_repair(ProjectionRepairRequest {
                chain_root_id: chain_root_id.to_string(),
                committed_head_hash: pending
                    .target_hash
                    .or(pending.expected_previous_hash)
                    .unwrap_or_default(),
                operation,
                error: format!("{error:#}"),
            });
    }

    fn acknowledge_projected_set_locked(
        &self,
        chain_root_id: &str,
        chain_lock: &crate::chain::ChainLock,
    ) -> anyhow::Result<()> {
        ensure_state_chain_lock(&self.refs_root, chain_lock, chain_root_id)?;
        let pending = match self.recovery.read_pending(chain_root_id)? {
            Some(pending) if pending.operation == HeadOperation::Set => pending,
            Some(_) => {
                anyhow::bail!("chain {chain_root_id} has pending Remove after a committed Set")
            }
            None => return Ok(()),
        };
        let head = chain_lock
            .read_verified_head(self.trust_store.as_ref())?
            .ok_or_else(|| {
                anyhow::anyhow!("chain {chain_root_id} head disappeared before Set ack")
            })?;
        if !acknowledge_if_projection_current(
            &self.projection,
            &self.recovery,
            chain_lock,
            &pending,
            Some(&head),
        )? {
            anyhow::bail!(
                "projection/head/pending transition do not agree for chain {chain_root_id}"
            );
        }
        Ok(())
    }

    fn mark_pending_set_applying_locked(
        &self,
        chain_root_id: &str,
        committed_head_hash: &str,
        chain_lock: &crate::chain::ChainLock,
    ) -> anyhow::Result<()> {
        ensure_state_chain_lock(&self.refs_root, chain_lock, chain_root_id)?;
        let mut pending = self
            .recovery
            .read_pending(chain_root_id)?
            .ok_or_else(|| anyhow::anyhow!("missing pending Set for {chain_root_id}"))?;
        if pending.operation != HeadOperation::Set
            || pending.target_hash.as_deref() != Some(committed_head_hash)
        {
            anyhow::bail!("pending transition does not match committed Set for {chain_root_id}");
        }
        if pending.phase == TransitionPhase::Prepared {
            pending = self.recovery.advance_phase(
                chain_lock,
                chain_root_id,
                &pending.transition_id,
                TransitionPhase::Prepared,
                TransitionPhase::HeadPublished,
            )?;
        }
        if pending.phase == TransitionPhase::HeadPublished {
            self.recovery.advance_phase(
                chain_lock,
                chain_root_id,
                &pending.transition_id,
                TransitionPhase::HeadPublished,
                TransitionPhase::ApplyingProjection,
            )?;
        }
        Ok(())
    }

    fn pending_remove_invalidated_by_mutation_locked(
        &self,
        chain_root_id: &str,
        mutation: ProspectiveChainMutation<'_>,
        chain_lock: &crate::chain::ChainLock,
    ) -> anyhow::Result<bool> {
        ensure_state_chain_lock(&self.refs_root, chain_lock, chain_root_id)?;
        let Some(pending) = self.recovery.read_pending(chain_root_id)? else {
            return Ok(false);
        };
        if pending.operation != HeadOperation::Remove {
            return Ok(false);
        }
        let Some(head) = chain_lock.read_verified_head(self.trust_store.as_ref())? else {
            return Ok(false);
        };
        if pending.expected_previous_hash.as_deref() != Some(head.target_hash.as_str()) {
            return Ok(false);
        }
        let cas = lillux::CasStore::from_pinned_root(self._cas_directory.try_clone()?);
        prospective_mutation_invalidates_retirement(
            &cas,
            chain_root_id,
            &head.target_hash,
            mutation,
        )
    }

    fn restore_cancelled_remove_after_failed_write_locked(
        &self,
        chain_root_id: &str,
        cancelled: &CancelledPreparedRemove,
        chain_lock: &crate::chain::ChainLock,
    ) -> anyhow::Result<()> {
        ensure_state_chain_lock(&self.refs_root, chain_lock, chain_root_id)?;
        let head = chain_lock
            .read_verified_head(self.trust_store.as_ref())?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "cannot restore cancelled Remove for {chain_root_id}: head is absent"
                )
            })?;
        if head.target_hash != cancelled.expected_previous_hash {
            anyhow::bail!(
                "cannot restore cancelled Remove for {chain_root_id}: head advanced to {}",
                head.target_hash
            );
        }

        if let Some(pending) = self.recovery.read_pending(chain_root_id)? {
            match pending.operation {
                HeadOperation::Remove
                    if pending.expected_previous_hash.as_deref()
                        == Some(cancelled.expected_previous_hash.as_str()) =>
                {
                    return Ok(());
                }
                HeadOperation::Set
                    if pending.phase == TransitionPhase::Prepared
                        && pending.expected_previous_hash.as_deref()
                            == Some(cancelled.expected_previous_hash.as_str()) =>
                {
                    if !self.recovery.acknowledge(
                        chain_lock,
                        chain_root_id,
                        &pending.transition_id,
                    )? {
                        anyhow::bail!(
                            "prepared Set changed while restoring Remove for {chain_root_id}"
                        );
                    }
                }
                _ => anyhow::bail!(
                    "cannot restore cancelled Remove for {chain_root_id}: transition slot changed"
                ),
            }
        }
        self.recovery.prepare_remove(
            chain_lock,
            chain_root_id,
            &cancelled.expected_previous_hash,
        )?;
        Ok(())
    }

    fn failed_write_after_cancelled_remove(
        &self,
        chain_root_id: &str,
        cancelled: Option<&CancelledPreparedRemove>,
        chain_lock: &crate::chain::ChainLock,
        write_error: anyhow::Error,
    ) -> anyhow::Error {
        let Some(cancelled) = cancelled else {
            return write_error;
        };
        match self.restore_cancelled_remove_after_failed_write_locked(
            chain_root_id,
            cancelled,
            chain_lock,
        ) {
            Ok(()) => write_error,
            Err(restore_error) => anyhow::anyhow!(
                "chain write failed after cancelling Prepared Remove: {write_error:#}; \
                 restoring the Remove also failed: {restore_error:#}"
            ),
        }
    }

    /// Converge the one durable transition slot before a new Set can be
    /// prepared. The caller holds the chain lock from this point through the
    /// new projection commit and acknowledgement.
    fn converge_pending_before_set_locked(
        &self,
        chain_root_id: &str,
        invalidates_retirement: bool,
        runtime_liveness: Option<&dyn RuntimeLivenessInspector>,
        chain_lock: &crate::chain::ChainLock,
    ) -> anyhow::Result<Option<CancelledPreparedRemove>> {
        ensure_state_chain_lock(&self.refs_root, chain_lock, chain_root_id)?;
        let cas = self.pinned_cas()?;
        let Some(pending) = self.recovery.read_pending(chain_root_id)? else {
            return Ok(None);
        };
        let head = chain_lock.read_verified_head(self.trust_store.as_ref())?;
        match pending.operation {
            HeadOperation::Set => match classify_pending_set_head(&pending, head.as_ref())? {
                PendingSetHead::Published(head) => {
                    crate::rebuild::repair_one_chain_verified_with_cas_and_observer(
                        &self.projection,
                        &cas,
                        chain_root_id,
                        &head.target_hash,
                        None,
                    )?;
                    let pending = advance_set_to_applying(&self.recovery, chain_lock, pending)?;
                    if !acknowledge_if_projection_current(
                        &self.projection,
                        &self.recovery,
                        chain_lock,
                        &pending,
                        Some(head),
                    )? {
                        anyhow::bail!(
                            "failed to converge pending Set before writing {chain_root_id}"
                        );
                    }
                }
                PendingSetHead::Unpublished(Some(previous)) => {
                    crate::rebuild::repair_one_chain_verified_with_cas_and_observer(
                        &self.projection,
                        &cas,
                        chain_root_id,
                        &previous.target_hash,
                        None,
                    )?;
                    acknowledge_unpublished_set(
                        &self.projection,
                        &self.recovery,
                        chain_lock,
                        &pending,
                        Some(previous),
                    )?;
                }
                PendingSetHead::Unpublished(None) => {
                    let runtime_liveness = runtime_liveness.ok_or_else(|| {
                        anyhow::anyhow!(
                            "headless pending Set for {chain_root_id} requires runtime-liveness inspection"
                        )
                    })?;
                    if runtime_liveness.chain_has_live_recovery_state(chain_root_id)? {
                        anyhow::bail!(
                            "headless pending Set for {chain_root_id} still has live recovery state"
                        );
                    }
                    self.projection.delete_chain_projection(chain_root_id)?;
                    acknowledge_unpublished_set(
                        &self.projection,
                        &self.recovery,
                        chain_lock,
                        &pending,
                        None,
                    )?;
                }
            },
            HeadOperation::Remove => {
                anyhow::ensure!(
                    self.recovery.retired_chain_fence(chain_root_id)?
                        == crate::recovery::RetiredChainFence::Absent,
                    "cannot publish chain {chain_root_id}: its pending Remove belongs to a fenced retirement"
                );
                if !invalidates_retirement {
                    anyhow::bail!(
                        "cannot publish chain {chain_root_id} while a still-eligible Remove is pending"
                    );
                }
                if pending.phase != TransitionPhase::Prepared {
                    anyhow::bail!(
                        "cannot write chain {chain_root_id} after its pending Remove crossed the head boundary"
                    );
                }
                let head = head.ok_or_else(|| {
                    anyhow::anyhow!(
                        "cannot cancel pending Remove for {chain_root_id}: head is absent"
                    )
                })?;
                if pending.expected_previous_hash.as_deref() != Some(head.target_hash.as_str()) {
                    anyhow::bail!(
                        "cannot cancel pending Remove for {chain_root_id}: authoritative head changed"
                    );
                }
                if !self
                    .recovery
                    .acknowledge(chain_lock, chain_root_id, &pending.transition_id)?
                {
                    anyhow::bail!(
                        "pending Remove changed while admitting write for {chain_root_id}"
                    );
                }
                let cancelled = CancelledPreparedRemove {
                    expected_previous_hash: head.target_hash,
                };
                return Ok(Some(cancelled));
            }
        }
        if self.recovery.read_pending(chain_root_id)?.is_some() {
            anyhow::bail!("chain {chain_root_id} transition did not converge");
        }
        Ok(None)
    }

    /// Access the underlying projection database for queries.
    pub fn projection(&self) -> &ProjectionDb {
        &self.projection
    }

    /// Owner of this State's projection leases, when it has native custody.
    /// Guarded evidence reads run inside its scope; without one they stay
    /// unscoped, which is sound only while this process lists no lease.
    fn guarded_read_factory(&self) -> Option<&lillux_sqlite::OriginalNativeFactory> {
        self.original_projection_context
            .as_ref()
            .map(|context| context.factory.as_ref())
    }

    /// Exact generation-selected projection instance.
    pub fn selected_projection_path(&self) -> &Path {
        self.projection.path()
    }

    /// Strictly verify the generation pointer, signed authoritative heads,
    /// complete CAS closures, and projection cursors without rebuilding or
    /// otherwise mutating state. Callers must hold CAS/head mutation exclusion
    /// across this full scan.
    pub fn verify_projection_generation(
        &mut self,
    ) -> anyhow::Result<crate::rebuild::ProjectionVerificationReport> {
        let generation = self
            .recovery
            .read_generation_in(self.guarded_read_factory())?
            .ok_or_else(|| anyhow::anyhow!("projection recovery generation is absent"))?;
        if generation.projection_schema_epoch != ProjectionDb::current_schema_epoch() {
            anyhow::bail!(
                "projection generation schema epoch mismatch: selected={}, current={}",
                generation.projection_schema_epoch,
                ProjectionDb::current_schema_epoch()
            );
        }
        let selected_file = self
            .projection
            .path()
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow::anyhow!("selected projection path has no UTF-8 filename"))?;
        if selected_file != generation.projection_file {
            anyhow::bail!(
                "projection generation selects {}, but the open instance is {}",
                generation.projection_file,
                selected_file
            );
        }
        let selected_instance_id = self
            .projection
            .projection_instance_id()?
            .ok_or_else(|| anyhow::anyhow!("selected projection has no instance identity"))?;
        if selected_instance_id != generation.projection_instance_id {
            anyhow::bail!(
                "projection instance identity mismatch: generation={}, selected={}",
                generation.projection_instance_id,
                selected_instance_id
            );
        }
        let pending = self.recovery.list_pending()?;
        if let Some(transition) = pending.first() {
            anyhow::bail!(
                "projection verification requires a converged journal; {} pending transition(s), first is {} ({:?}/{:?})",
                pending.len(),
                transition.chain_root_id,
                transition.operation,
                transition.phase,
            );
        }
        let cas = lillux::CasStore::from_pinned_root(self._cas_directory.try_clone()?);
        crate::rebuild::verify_projection_with_trust_pinned(
            &self.projection,
            &cas,
            &self._refs_directory,
            self.trust_store.as_ref(),
            self.original_projection_context.as_mut(),
        )
    }

    /// Build and publish a fresh projection generation, then swap
    /// this live StateDb handle to the selected instance. Callers must keep the
    /// application offline/not-ready for the entire operation.
    /// Rebuild while consuming an exclusive CAS mutation guard already held by
    /// the application. This preserves the global guard-before-store-lock
    /// hierarchy for the offline rebuild service.
    pub fn rebuild_projection_generation_admitted(
        &mut self,
        runtime_liveness: Option<&dyn RuntimeLivenessInspector>,
        cas_mutation_guard: &crate::recovery::CasMutationGuard,
    ) -> anyhow::Result<crate::rebuild::RebuildReport> {
        let runtime_state_dir = self
            .cas_root
            .parent()
            .ok_or_else(|| anyhow::anyhow!("CAS root has no runtime-state parent"))?
            .to_path_buf();
        let context = self.original_projection_context.as_mut().context(
            "a State without a projection build context cannot rebuild; open it with StateDb::open_offline_native_for_rebuild",
        )?;
        let (new_projection, report) = rebuild_and_install_projection(
            &runtime_state_dir,
            &self.cas_root,
            &self.refs_root,
            &self.recovery,
            self.trust_store.as_ref(),
            runtime_liveness,
            None,
            Some(cas_mutation_guard),
            context,
        )?;
        let old_projection = std::mem::replace(&mut self.projection, new_projection);
        let factory = old_projection.original_factory();
        let closed = old_projection
            .close_original(&factory)
            .map_err(anyhow::Error::new)?;
        let closed = Arc::new(closed);
        self.original_projection_terminals.push(Arc::clone(&closed));
        closed.require_original(&factory)?;
        let deleted = cleanup_superseded_projection_instances(
            &self.recovery,
            self.guarded_read_factory(),
            self.projection.path(),
        )?;
        if deleted != 0 {
            tracing::info!(deleted, "removed superseded projection instances");
        }
        Ok(report)
    }

    /// Durably mark an explicitly requested offline all-thread-history
    /// discard. The marker survives interruption and blocks ordinary startup
    /// until the same offline command reaches every participating store.
    pub fn begin_thread_history_discard_admitted(
        &self,
        cas_mutation_guard: &crate::recovery::CasMutationGuard,
        operation: &crate::recovery::ThreadHistoryDiscardOperationRef,
    ) -> anyhow::Result<()> {
        self.ensure_cas_mutation_guard(cas_mutation_guard)?;
        self.recovery
            .begin_thread_history_discard(cas_mutation_guard, operation)
    }

    /// Read the activation reference under the original admitted State owner.
    /// App verifies its signed operation before any destructive preview/join.
    pub fn thread_history_discard_operation_admitted(
        &self,
        cas_mutation_guard: &crate::recovery::CasMutationGuard,
    ) -> anyhow::Result<Option<crate::recovery::ThreadHistoryDiscardOperationRef>> {
        self.ensure_cas_mutation_guard(cas_mutation_guard)?;
        self.recovery
            .thread_history_discard_operation_in(self.guarded_read_factory())
    }

    /// Retire the complete authoritative thread-head namespace and publish an
    /// empty current projection. This is intentionally separate from normal
    /// GC, which never removes chain heads blindly. The caller must also hold
    /// the process-wide operator/state lock and clear the app-owned runtime and
    /// scheduler stores before acknowledging the durable discard marker.
    pub fn discard_authoritative_thread_history_admitted(
        &mut self,
        cas_mutation_guard: &crate::recovery::CasMutationGuard,
        dry_run: bool,
    ) -> anyhow::Result<AuthoritativeThreadHistoryDiscardReport> {
        self.discard_authoritative_thread_history_admitted_with_progress(
            cas_mutation_guard,
            dry_run,
            &mut |_| {},
        )
    }

    /// Progress is published after each verified chain head has been durably
    /// retired and again immediately before the empty projection generation
    /// is rebuilt.
    pub fn discard_authoritative_thread_history_admitted_with_progress(
        &mut self,
        cas_mutation_guard: &crate::recovery::CasMutationGuard,
        dry_run: bool,
        progress: &mut dyn FnMut(AuthoritativeThreadHistoryDiscardProgress),
    ) -> anyhow::Result<AuthoritativeThreadHistoryDiscardReport> {
        self.ensure_cas_mutation_guard(cas_mutation_guard)?;
        if !cas_mutation_guard.is_exclusive() {
            anyhow::bail!("thread-history discard requires the exclusive CAS guard");
        }
        if !dry_run
            && self
                .recovery
                .thread_history_discard_operation_in(self.guarded_read_factory())?
                .is_none()
        {
            anyhow::bail!("thread-history discard intent was not durably published");
        }

        // Verification is deliberately at the signed-head boundary, not at
        // the pointed-to snapshot shape: this command exists specifically to
        // retire otherwise unsupported historical closures.
        let heads = crate::refs::list_verified_chain_heads_in_directory(
            &self._refs_directory,
            self.trust_store.as_ref(),
        )?;
        let pending = self.recovery.list_pending()?;
        let projection_instances = inspect_projection_history_discard(&self.recovery)?;
        let chains_directory = match self
            ._refs_directory
            .open_child_directory(std::ffi::OsStr::new("generic"))?
        {
            Some(generic) => generic.open_child_directory(std::ffi::OsStr::new("chains"))?,
            None => None,
        };
        let mut retire_progress = |completed, total| {
            progress(
                AuthoritativeThreadHistoryDiscardProgress::RetiringChainHeads { completed, total },
            );
        };
        let chain_ref_artifacts = match chains_directory.as_ref() {
            Some(directory) => remove_pinned_directory_contents_with_progress(
                directory,
                dry_run,
                heads.len(),
                &mut retire_progress,
            )?,
            None => 0,
        };
        if !dry_run && chains_directory.is_none() {
            retire_progress(0, 0);
        }
        let pending_transitions = self
            .recovery
            .discard_all_pending_transitions(cas_mutation_guard, dry_run)?;
        debug_assert_eq!(pending_transitions, pending.len());
        // Every head and pending transition is gone, so no live holder can
        // still need a fence; marker absence then reads as an unknown root.
        let retirement_evidence = self
            .recovery
            .discard_retirement_evidence(cas_mutation_guard, dry_run)?;

        if dry_run {
            return Ok(AuthoritativeThreadHistoryDiscardReport {
                chain_heads: heads.len(),
                chain_ref_artifacts,
                pending_transitions,
                retirement_fences: retirement_evidence.retirement_fences,
                evidence_obligations: retirement_evidence.evidence_obligations,
                superseded_projection_instances: projection_instances,
                rebuilt_projection: None,
            });
        }

        self.head_cache.lock().expect("head_cache lock").clear();
        progress(AuthoritativeThreadHistoryDiscardProgress::RebuildingProjection);
        let rebuilt_projection =
            self.rebuild_projection_generation_admitted(None, cas_mutation_guard)?;
        cleanup_all_superseded_projection_instances(
            &self.recovery,
            self.guarded_read_factory(),
            self.projection.path(),
        )?;
        Ok(AuthoritativeThreadHistoryDiscardReport {
            chain_heads: heads.len(),
            chain_ref_artifacts,
            pending_transitions,
            retirement_fences: retirement_evidence.retirement_fences,
            evidence_obligations: retirement_evidence.evidence_obligations,
            superseded_projection_instances: projection_instances,
            rebuilt_projection: Some(rebuilt_projection),
        })
    }

    /// Replay only durable pending chain transitions. This never enumerates the
    /// chain-head directory.
    pub fn replay_pending_chain_transitions(&self) -> anyhow::Result<PendingReplayReport> {
        replay_pending_into(
            &self.projection,
            &self.cas_root,
            &self.refs_root,
            &self.recovery,
            self.trust_store.as_ref(),
            None,
            true,
            None,
        )
    }

    pub fn replay_pending_chain_transitions_with_runtime_liveness(
        &self,
        runtime_liveness: &dyn RuntimeLivenessInspector,
    ) -> anyhow::Result<PendingReplayReport> {
        replay_pending_into(
            &self.projection,
            &self.cas_root,
            &self.refs_root,
            &self.recovery,
            self.trust_store.as_ref(),
            Some(runtime_liveness),
            true,
            None,
        )
    }

    /// Replay pending transitions with startup progress/cancellation reporting.
    /// Normal live repair callers use the observer-free variant above.
    pub fn replay_pending_chain_transitions_with_observer(
        &self,
        observer: &dyn ProjectionRecoveryObserver,
    ) -> anyhow::Result<PendingReplayReport> {
        replay_pending_into(
            &self.projection,
            &self.cas_root,
            &self.refs_root,
            &self.recovery,
            self.trust_store.as_ref(),
            None,
            true,
            Some(observer),
        )
    }

    pub fn replay_pending_chain_transitions_with_observer_and_runtime_liveness(
        &self,
        observer: &dyn ProjectionRecoveryObserver,
        runtime_liveness: &dyn RuntimeLivenessInspector,
    ) -> anyhow::Result<PendingReplayReport> {
        replay_pending_into(
            &self.projection,
            &self.cas_root,
            &self.refs_root,
            &self.recovery,
            self.trust_store.as_ref(),
            Some(runtime_liveness),
            true,
            Some(observer),
        )
    }

    /// Repair one named chain from its current signed head and acknowledge a
    /// matching pending Set. Intended for the live repair worker and imports.
    pub fn repair_one_chain(
        &self,
        chain_root_id: &str,
    ) -> anyhow::Result<crate::rebuild::ChainRepairReport> {
        let cas = self.pinned_cas()?;
        let chain_lock = crate::chain::ChainLock::acquire_in_refs_directory(
            &self._refs_directory,
            &self._cas_directory,
            &self.recovery,
            chain_root_id,
        )?;
        let head = chain_lock
            .read_verified_head(self.trust_store.as_ref())?
            .ok_or_else(|| anyhow::anyhow!("chain {chain_root_id} has no signed head"))?;
        let pending = match self.recovery.read_pending(chain_root_id)? {
            Some(pending) if pending.operation == HeadOperation::Set => {
                if pending.target_hash.as_deref() != Some(head.target_hash.as_str()) {
                    anyhow::bail!(
                        "pending Set target does not match current head for {chain_root_id}"
                    );
                }
                Some(advance_set_to_applying(
                    &self.recovery,
                    &chain_lock,
                    pending,
                )?)
            }
            Some(_) => anyhow::bail!(
                "chain {chain_root_id} has pending Remove; ordinary repair cannot acknowledge it"
            ),
            None => None,
        };
        let report = crate::rebuild::repair_one_chain_verified_with_cas_and_observer(
            &self.projection,
            &cas,
            chain_root_id,
            &head.target_hash,
            None,
        )?;
        if let Some(pending) = pending
            && !acknowledge_if_projection_current(
                &self.projection,
                &self.recovery,
                &chain_lock,
                &pending,
                Some(&head),
            )?
        {
            anyhow::bail!("repaired projection did not converge for {chain_root_id}");
        }
        Ok(report)
    }

    /// What the fence store records for this chain identity.
    pub fn retired_chain_fence(
        &self,
        chain_root_id: &str,
    ) -> anyhow::Result<crate::recovery::RetiredChainFence> {
        self.recovery.retired_chain_fence(chain_root_id)
    }

    /// Every fenced chain whose closure is not yet released, with the manifest
    /// its fence binds.
    pub fn unreleased_retired_chains(&self) -> anyhow::Result<BTreeMap<String, String>> {
        self.recovery.unreleased_retired_chains()
    }

    pub fn pending_chain_transitions(&self) -> anyhow::Result<Vec<PendingChainHeadTransition>> {
        self.recovery.list_pending()
    }

    /// Read one transition without relisting the pending directory. Recovery
    /// loops use this for an O(P) compare/recheck pass instead of O(P^2).
    pub fn pending_chain_transition(
        &self,
        chain_root_id: &str,
    ) -> anyhow::Result<Option<PendingChainHeadTransition>> {
        self.recovery.read_pending(chain_root_id)
    }

    /// Open a single-pass, bounded-memory cursor over pending Removes.
    pub fn pending_remove_cursor(
        &self,
    ) -> anyhow::Result<crate::recovery::PendingTransitionCursor> {
        self.recovery.pending_cursor(Some(HeadOperation::Remove))
    }

    /// Pending Set targets are temporary CAS roots while a writer is between
    /// CAS creation and durable projection acknowledgement. GC must include
    /// these in its mark roots in addition to quiescing all publishers before
    /// their first CAS write.
    pub fn pending_transition_object_roots(&self) -> anyhow::Result<Vec<String>> {
        self.recovery.pending_transition_object_roots()
    }

    /// Admit creation of a new runtime recovery pin for an authoritative
    /// chain. The returned value retains the chain lock; callers must keep it
    /// alive until their runtime DB/file pin is durable.
    pub fn authorize_runtime_pin(
        &self,
        chain_root_id: &str,
    ) -> anyhow::Result<AuthoritativeRuntimePinAdmission> {
        let chain_lock = crate::chain::ChainLock::acquire_in_refs_directory(
            &self._refs_directory,
            &self._cas_directory,
            &self.recovery,
            chain_root_id,
        )?;
        if self
            .recovery
            .read_pending(chain_root_id)?
            .is_some_and(|pending| pending.operation == HeadOperation::Remove)
        {
            anyhow::bail!("chain {chain_root_id} is being removed and cannot accept runtime pins");
        }
        let head = chain_lock
            .read_verified_head(self.trust_store.as_ref())?
            .ok_or_else(|| {
                anyhow::anyhow!("chain {chain_root_id} has no authoritative signed head")
            })?;
        Ok(AuthoritativeRuntimePinAdmission {
            chain_root_id: chain_root_id.to_string(),
            head_hash: head.target_hash,
            _chain_locks: vec![chain_lock],
        })
    }

    /// Reserve a runtime pin for a not-yet-created child chain. The future
    /// chain lock closes the absent-head check; a pending Remove is a tombstone
    /// and always rejects reuse. The parent must remain authoritative.
    pub fn authorize_future_runtime_pin(
        &self,
        parent_chain_root_id: &str,
        future_chain_root_id: &str,
    ) -> anyhow::Result<AuthoritativeRuntimePinAdmission> {
        if parent_chain_root_id == future_chain_root_id {
            anyhow::bail!("future child chain must differ from its parent chain");
        }
        let mut lock_ids = [parent_chain_root_id, future_chain_root_id];
        lock_ids.sort_unstable();
        let mut chain_locks = Vec::with_capacity(2);
        for chain_root_id in lock_ids {
            chain_locks.push(crate::chain::ChainLock::acquire_in_refs_directory(
                &self._refs_directory,
                &self._cas_directory,
                &self.recovery,
                chain_root_id,
            )?);
        }
        if self.recovery.read_pending(future_chain_root_id)?.is_some() {
            anyhow::bail!("future chain {future_chain_root_id} already has a pending transition");
        }
        let future_lock = chain_locks
            .iter()
            .find(|lock| lock.chain_root_id() == future_chain_root_id)
            .ok_or_else(|| anyhow::anyhow!("future chain lock was not acquired"))?;
        if future_lock
            .read_verified_head(self.trust_store.as_ref())?
            .is_some()
        {
            anyhow::bail!("future chain {future_chain_root_id} already has an authoritative head");
        }
        if self
            .recovery
            .read_pending(parent_chain_root_id)?
            .is_some_and(|pending| pending.operation == HeadOperation::Remove)
        {
            anyhow::bail!("parent chain {parent_chain_root_id} is being removed");
        }
        let parent_lock = chain_locks
            .iter()
            .find(|lock| lock.chain_root_id() == parent_chain_root_id)
            .ok_or_else(|| anyhow::anyhow!("parent chain lock was not acquired"))?;
        let parent = parent_lock
            .read_verified_head(self.trust_store.as_ref())?
            .ok_or_else(|| {
                anyhow::anyhow!("parent chain {parent_chain_root_id} has no signed head")
            })?;
        Ok(AuthoritativeRuntimePinAdmission {
            chain_root_id: future_chain_root_id.to_string(),
            head_hash: parent.target_hash,
            _chain_locks: chain_locks,
        })
    }

    /// CAS objects root (`runtime_state_dir/objects`).
    pub fn cas_root(&self) -> &Path {
        &self.cas_root
    }

    pub(crate) fn pinned_cas(&self) -> anyhow::Result<lillux::CasStore> {
        Ok(lillux::CasStore::from_pinned_root(
            self._cas_directory.try_clone()?,
        ))
    }

    pub(crate) fn ensure_cas_mutation_guard(
        &self,
        guard: &crate::recovery::CasMutationGuard,
    ) -> anyhow::Result<()> {
        guard.ensure_protects_pinned_runtime(&self._runtime_state_directory)
    }

    /// Refs root (`runtime_state_dir/refs`).
    pub fn refs_root(&self) -> &Path {
        &self.refs_root
    }

    /// Explicit offline schema-cutover operation for every project-snapshot
    /// HEAD, including principal-scoped working heads and deployed-project
    /// heads. This is intentionally not part of ordinary GC: immutable project
    /// history is durable authority and may only be retired by an operator who
    /// has also retired every thread that could reference it.
    pub fn discard_all_project_heads_admitted(
        &self,
        guard: &crate::recovery::CasMutationGuard,
        dry_run: bool,
    ) -> anyhow::Result<usize> {
        self.ensure_cas_mutation_guard(guard)?;
        if !guard.is_exclusive() {
            anyhow::bail!("project HEAD discard requires an exclusive CAS mutation guard");
        }
        let principal_heads = crate::refs::list_verified_project_head_refs(
            &self.refs_root,
            self.trust_store.as_ref(),
        )?;
        let deployed_heads = crate::refs::list_verified_deployed_project_refs(
            &self.refs_root,
            self.trust_store.as_ref(),
        )?;
        let head_count = principal_heads
            .len()
            .checked_add(deployed_heads.len())
            .ok_or_else(|| anyhow::anyhow!("project HEAD count overflow"))?;
        if dry_run || head_count == 0 {
            return Ok(head_count);
        }
        if let Some(projects) = self
            ._refs_directory
            .open_child_directory(std::ffi::OsStr::new("projects"))?
        {
            projects.remove_contents_recursive()?;
        }
        if let Some(deployed) = self
            ._refs_directory
            .open_child_directory(std::ffi::OsStr::new("deployed"))?
            && let Some(projects) =
                deployed.open_child_directory(std::ffi::OsStr::new("projects"))?
        {
            projects.remove_contents_recursive()?;
        }
        Ok(head_count)
    }

    /// Retain this store's exact runtime/CAS/refs/trust authority for a
    /// long-running worker without retaining the StateDb mutex.
    pub fn pinned_authority(&self) -> anyhow::Result<PinnedStateAuthority> {
        Ok(PinnedStateAuthority {
            offline_namespace_lock: self._offline_namespace_lock.clone(),
            runtime_directory: self._runtime_state_directory.try_clone()?,
            cas_directory: self._cas_directory.try_clone()?,
            refs_directory: self._refs_directory.try_clone()?,
            recovery: Some(self.recovery.clone()),
            trust_store: Arc::clone(&self.trust_store),
        })
    }

    /// Verifying keys used for every authoritative chain-head read performed
    /// by this state handle.
    pub fn head_trust_store(&self) -> &TrustStore {
        self.trust_store.as_ref()
    }

    /// Clone the exact trust authority captured by this state handle for a
    /// worker that must outlive the StateDb lock. Authoritative traversal must
    /// never reconstruct trust from the local signer.
    pub fn head_trust_store_arc(&self) -> Arc<TrustStore> {
        Arc::clone(&self.trust_store)
    }

    /// Locators root (`runtime_state_dir/locators`).
    pub fn locators_root(&self) -> &Path {
        &self.locators_root
    }

    /// Create a new execution chain with a root thread.
    ///
    /// Delegates to [`chain::create_chain`] then projects the thread snapshot
    /// into SQLite. If head publication succeeds but projection fails, the
    /// returned committed write is pending and its durable transition remains
    /// available to the live repair worker and deterministic startup replay.
    #[cfg(test)]
    pub fn create_chain(
        &self,
        chain_root_id: &str,
        snapshot: ThreadSnapshot,
        signer: &dyn Signer,
    ) -> anyhow::Result<CommittedWrite<CreateResult>> {
        let guard = self.pinned_authority()?.acquire_shared_guard()?;
        self.create_chain_inner(chain_root_id, snapshot, signer, None, &guard)
    }

    pub fn create_chain_admitted(
        &self,
        chain_root_id: &str,
        snapshot: ThreadSnapshot,
        signer: &dyn Signer,
        runtime_liveness: &dyn RuntimeLivenessInspector,
        cas_mutation_guard: &crate::recovery::CasMutationGuard,
    ) -> anyhow::Result<CommittedWrite<CreateResult>> {
        self.create_chain_inner(
            chain_root_id,
            snapshot,
            signer,
            Some(runtime_liveness),
            cas_mutation_guard,
        )
    }

    fn create_chain_inner(
        &self,
        chain_root_id: &str,
        snapshot: ThreadSnapshot,
        signer: &dyn Signer,
        runtime_liveness: Option<&dyn RuntimeLivenessInspector>,
        cas_mutation_guard: &crate::recovery::CasMutationGuard,
    ) -> anyhow::Result<CommittedWrite<CreateResult>> {
        self.ensure_cas_mutation_guard(cas_mutation_guard)?;
        let chain_lock = crate::chain::ChainLock::acquire_in_refs_directory(
            &self._refs_directory,
            &self._cas_directory,
            &self.recovery,
            chain_root_id,
        )?;
        self.converge_pending_before_set_locked(
            chain_root_id,
            false,
            runtime_liveness,
            &chain_lock,
        )?;
        let mut cache = self.head_cache.lock().expect("head_cache lock");
        let result = match chain::create_chain_with_trust_under_lock(
            &self.cas_root,
            &self.refs_root,
            chain_root_id,
            snapshot.clone(),
            signer,
            self.trust_store.as_ref(),
            &mut cache,
            &chain_lock,
        ) {
            Ok(result) => result,
            Err(error) => {
                self.note_pending_transition_error(chain_root_id, "create_chain", &error);
                return Err(error);
            }
        };
        if let Err(error) = self.mark_pending_set_applying_locked(
            chain_root_id,
            &result.chain_state_hash,
            &chain_lock,
        ) {
            self.note_pending_transition_error(chain_root_id, "create_chain", &error);
            return Err(error);
        }
        let projected = project_committed_chain(
            &self.projection,
            &cache,
            chain_root_id,
            &result.chain_state_hash,
            || {
                projection::project_thread_snapshot(
                    &self.projection,
                    &result.snapshot,
                    chain_root_id,
                )
                .context("projecting exact committed thread snapshot")
            },
        );
        let committed_hash = result.chain_state_hash.clone();
        Ok(self.committed_write_locked(
            result,
            chain_root_id,
            &committed_hash,
            "create_chain",
            projected,
            &chain_lock,
        ))
    }

    /// Create a root snapshot and its initial durable events in one head update
    /// and one projection transaction.
    #[cfg(test)]
    pub fn create_chain_with_events(
        &self,
        chain_root_id: &str,
        snapshot: ThreadSnapshot,
        events: Vec<ThreadEvent>,
        signer: &dyn Signer,
    ) -> anyhow::Result<CommittedWrite<AddThreadWithEventsResult>> {
        let guard = self.pinned_authority()?.acquire_shared_guard()?;
        self.create_chain_with_events_inner(CreateChainWithEventsRequest {
            chain_root_id,
            snapshot,
            event_successor_snapshot: None,
            events,
            signer,
            runtime_liveness: None,
            cas_mutation_guard: &guard,
        })
    }

    pub fn create_chain_with_events_admitted(
        &self,
        chain_root_id: &str,
        snapshot: ThreadSnapshot,
        events: Vec<ThreadEvent>,
        signer: &dyn Signer,
        runtime_liveness: &dyn RuntimeLivenessInspector,
        cas_mutation_guard: &crate::recovery::CasMutationGuard,
    ) -> anyhow::Result<CommittedWrite<AddThreadWithEventsResult>> {
        self.create_chain_with_events_inner(CreateChainWithEventsRequest {
            chain_root_id,
            snapshot,
            event_successor_snapshot: None,
            events,
            signer,
            runtime_liveness: Some(runtime_liveness),
            cas_mutation_guard,
        })
    }

    pub fn create_chain_with_event_successor_admitted(
        &self,
        request: CreateChainWithEventSuccessorRequest<'_>,
    ) -> anyhow::Result<CommittedWrite<AddThreadWithEventsResult>> {
        self.create_chain_with_events_inner(CreateChainWithEventsRequest {
            chain_root_id: request.chain_root_id,
            snapshot: request.genesis_snapshot,
            event_successor_snapshot: Some(request.event_successor_snapshot),
            events: request.events,
            signer: request.signer,
            runtime_liveness: Some(request.runtime_liveness),
            cas_mutation_guard: request.cas_mutation_guard,
        })
    }

    fn create_chain_with_events_inner(
        &self,
        request: CreateChainWithEventsRequest<'_>,
    ) -> anyhow::Result<CommittedWrite<AddThreadWithEventsResult>> {
        let CreateChainWithEventsRequest {
            chain_root_id,
            snapshot,
            event_successor_snapshot,
            events,
            signer,
            runtime_liveness,
            cas_mutation_guard,
        } = request;
        if events.iter().any(|event| !event.durability.is_cas_stored()) {
            anyhow::bail!("StateDb::create_chain_with_events cannot persist ephemeral events");
        }
        self.ensure_cas_mutation_guard(cas_mutation_guard)?;
        let chain_lock = crate::chain::ChainLock::acquire_in_refs_directory(
            &self._refs_directory,
            &self._cas_directory,
            &self.recovery,
            chain_root_id,
        )?;
        self.converge_pending_before_set_locked(
            chain_root_id,
            false,
            runtime_liveness,
            &chain_lock,
        )?;
        let mut cache = self.head_cache.lock().expect("head_cache lock");
        let result = match chain::create_chain_with_events_and_trust_under_lock(
            &self.cas_root,
            &self.refs_root,
            chain_root_id,
            snapshot,
            event_successor_snapshot,
            events,
            signer,
            self.trust_store.as_ref(),
            &mut cache,
            &chain_lock,
        ) {
            Ok(result) => result,
            Err(error) => {
                self.note_pending_transition_error(
                    chain_root_id,
                    "create_chain_with_events",
                    &error,
                );
                return Err(error);
            }
        };
        if let Err(error) = self.mark_pending_set_applying_locked(
            chain_root_id,
            &result.chain_state_hash,
            &chain_lock,
        ) {
            self.note_pending_transition_error(chain_root_id, "create_chain_with_events", &error);
            return Err(error);
        }
        let projected = project_initial_root_committed_chain(
            &self.projection,
            &cache,
            chain_root_id,
            &result.chain_state_hash,
            result.snapshot.status,
            || {
                projection::project_thread_snapshot_with_events_in_transaction(
                    &self.projection,
                    &result.snapshot,
                    chain_root_id,
                    &result.events,
                )
                .context("projecting exact committed root thread and initial events")
            },
        );
        let committed_hash = result.chain_state_hash.clone();
        Ok(self.committed_write_locked(
            result,
            chain_root_id,
            &committed_hash,
            "create_chain_with_events",
            projected,
            &chain_lock,
        ))
    }

    /// Add a new thread to an existing chain.
    ///
    /// Delegates to [`chain::add_thread_to_chain`] then projects the snapshot.
    /// Same CAS-first contract as [`Self::create_chain`].
    #[cfg(test)]
    pub fn add_thread(
        &self,
        chain_root_id: &str,
        snapshot: ThreadSnapshot,
        signer: &dyn Signer,
    ) -> anyhow::Result<CommittedWrite<CreateResult>> {
        let guard = self.pinned_authority()?.acquire_shared_guard()?;
        self.add_thread_inner(chain_root_id, snapshot, signer, None, &guard)
    }

    pub fn add_thread_admitted(
        &self,
        chain_root_id: &str,
        snapshot: ThreadSnapshot,
        signer: &dyn Signer,
        runtime_liveness: &dyn RuntimeLivenessInspector,
        cas_mutation_guard: &crate::recovery::CasMutationGuard,
    ) -> anyhow::Result<CommittedWrite<CreateResult>> {
        self.add_thread_inner(
            chain_root_id,
            snapshot,
            signer,
            Some(runtime_liveness),
            cas_mutation_guard,
        )
    }

    fn add_thread_inner(
        &self,
        chain_root_id: &str,
        snapshot: ThreadSnapshot,
        signer: &dyn Signer,
        runtime_liveness: Option<&dyn RuntimeLivenessInspector>,
        cas_mutation_guard: &crate::recovery::CasMutationGuard,
    ) -> anyhow::Result<CommittedWrite<CreateResult>> {
        self.ensure_cas_mutation_guard(cas_mutation_guard)?;
        let chain_lock = crate::chain::ChainLock::acquire_in_refs_directory(
            &self._refs_directory,
            &self._cas_directory,
            &self.recovery,
            chain_root_id,
        )?;
        let invalidates_retirement = self.pending_remove_invalidated_by_mutation_locked(
            chain_root_id,
            ProspectiveChainMutation::AddSnapshot(&snapshot),
            &chain_lock,
        )?;
        let cancelled_remove = self.converge_pending_before_set_locked(
            chain_root_id,
            invalidates_retirement,
            runtime_liveness,
            &chain_lock,
        )?;
        let mut cache = self.head_cache.lock().expect("head_cache lock");
        let result = match chain::add_thread_to_chain_with_trust_under_lock(
            &self.cas_root,
            &self.refs_root,
            chain_root_id,
            snapshot.clone(),
            signer,
            self.trust_store.as_ref(),
            &mut cache,
            &chain_lock,
        ) {
            Ok(result) => result,
            Err(error) => {
                let error = self.failed_write_after_cancelled_remove(
                    chain_root_id,
                    cancelled_remove.as_ref(),
                    &chain_lock,
                    error,
                );
                self.note_pending_transition_error(chain_root_id, "add_thread", &error);
                return Err(error);
            }
        };
        if let Err(error) = self.mark_pending_set_applying_locked(
            chain_root_id,
            &result.chain_state_hash,
            &chain_lock,
        ) {
            self.note_pending_transition_error(chain_root_id, "add_thread", &error);
            return Err(error);
        }
        let projected = project_committed_chain(
            &self.projection,
            &cache,
            chain_root_id,
            &result.chain_state_hash,
            || {
                projection::project_thread_snapshot(
                    &self.projection,
                    &result.snapshot,
                    chain_root_id,
                )
                .context("projecting exact committed added-thread snapshot")
            },
        );
        let committed_hash = result.chain_state_hash.clone();
        Ok(self.committed_write_locked(
            result,
            chain_root_id,
            &committed_hash,
            "add_thread",
            projected,
            &chain_lock,
        ))
    }

    /// Append events to a thread in a chain.
    ///
    /// Delegates to [`chain::append_events`], then projects each event and
    /// each snapshot update into SQLite. Same CAS-first contract.
    #[cfg(test)]
    pub fn append_events(
        &self,
        chain_root_id: &str,
        thread_id: &str,
        events: Vec<ThreadEvent>,
        snapshot_updates: Vec<SnapshotUpdate>,
        signer: &dyn Signer,
    ) -> anyhow::Result<CommittedWrite<AppendResult>> {
        let guard = self.pinned_authority()?.acquire_shared_guard()?;
        match self.append_events_inner(
            chain_root_id,
            thread_id,
            events,
            snapshot_updates,
            signer,
            None,
            None,
            &guard,
        )? {
            ConditionalThreadEventAppendResult::Committed(committed) => Ok(committed),
            ConditionalThreadEventAppendResult::Conflict => {
                anyhow::bail!("unconditional append reported an anchor conflict")
            }
        }
    }

    // Admission authority, signer, liveness proof, and the held CAS guard stay
    // explicit because omitting any one changes the durability contract.
    #[allow(clippy::too_many_arguments)]
    pub fn append_events_admitted(
        &self,
        chain_root_id: &str,
        thread_id: &str,
        events: Vec<ThreadEvent>,
        snapshot_updates: Vec<SnapshotUpdate>,
        signer: &dyn Signer,
        runtime_liveness: &dyn RuntimeLivenessInspector,
        cas_mutation_guard: &crate::recovery::CasMutationGuard,
    ) -> anyhow::Result<CommittedWrite<AppendResult>> {
        match self.append_events_inner(
            chain_root_id,
            thread_id,
            events,
            snapshot_updates,
            signer,
            Some(runtime_liveness),
            None,
            cas_mutation_guard,
        )? {
            ConditionalThreadEventAppendResult::Committed(committed) => Ok(committed),
            ConditionalThreadEventAppendResult::Conflict => {
                anyhow::bail!("unconditional append reported an anchor conflict")
            }
        }
    }

    /// Append only while `request.expected` is still the thread's exact signed
    /// frontier, checked under the chain lock that publishes the events. A
    /// moved frontier or head returns `Conflict` and writes nothing.
    pub fn append_events_admitted_if_anchor(
        &self,
        request: AnchoredThreadAppend<'_>,
        cas_mutation_guard: &crate::recovery::CasMutationGuard,
    ) -> anyhow::Result<ConditionalThreadEventAppendResult> {
        Self::ensure_anchor_identity(&request)?;
        self.append_events_inner(
            request.chain_root_id,
            request.thread_id,
            request.events,
            request.snapshot_updates,
            request.signer,
            Some(request.runtime_liveness),
            Some(request.expected),
            cas_mutation_guard,
        )
    }

    /// The anchored append for a caller that already holds this chain's lock
    /// (for example inside a retention-evidence publication that holds both
    /// chains' locks). The lock must be this store's exact lock for the chain.
    pub fn append_events_admitted_if_anchor_under_lock(
        &self,
        request: AnchoredThreadAppend<'_>,
        chain_lock: &crate::chain::ChainLock,
        cas_mutation_guard: &crate::recovery::CasMutationGuard,
    ) -> anyhow::Result<ConditionalThreadEventAppendResult> {
        Self::ensure_anchor_identity(&request)?;
        self.ensure_cas_mutation_guard(cas_mutation_guard)?;
        anyhow::ensure!(
            request
                .events
                .iter()
                .all(|event| event.durability.is_cas_stored()),
            "StateDb::append_events cannot persist ephemeral events"
        );
        self.append_events_under_chain_lock(
            LockedAppend {
                chain_root_id: request.chain_root_id,
                thread_id: request.thread_id,
                events: request.events,
                snapshot_updates: request.snapshot_updates,
                signer: request.signer,
                runtime_liveness: Some(request.runtime_liveness),
                expected: Some(request.expected),
            },
            chain_lock,
        )
    }

    fn ensure_anchor_identity(request: &AnchoredThreadAppend<'_>) -> anyhow::Result<()> {
        anyhow::ensure!(
            request.expected.chain_root_id == request.chain_root_id
                && request.expected.thread_id == request.thread_id,
            "conditional append anchor belongs to another thread"
        );
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn append_events_inner(
        &self,
        chain_root_id: &str,
        thread_id: &str,
        events: Vec<ThreadEvent>,
        snapshot_updates: Vec<SnapshotUpdate>,
        signer: &dyn Signer,
        runtime_liveness: Option<&dyn RuntimeLivenessInspector>,
        expected: Option<&AuthoritativeThreadEventReadAnchor>,
        cas_mutation_guard: &crate::recovery::CasMutationGuard,
    ) -> anyhow::Result<ConditionalThreadEventAppendResult> {
        if events.iter().any(|event| !event.durability.is_cas_stored()) {
            anyhow::bail!("StateDb::append_events cannot persist ephemeral events");
        }

        self.ensure_cas_mutation_guard(cas_mutation_guard)?;
        let chain_lock = crate::chain::ChainLock::acquire_in_refs_directory(
            &self._refs_directory,
            &self._cas_directory,
            &self.recovery,
            chain_root_id,
        )?;
        self.append_events_under_chain_lock(
            LockedAppend {
                chain_root_id,
                thread_id,
                events,
                snapshot_updates,
                signer,
                runtime_liveness,
                expected,
            },
            &chain_lock,
        )
    }

    fn append_events_under_chain_lock(
        &self,
        request: LockedAppend<'_>,
        chain_lock: &crate::chain::ChainLock,
    ) -> anyhow::Result<ConditionalThreadEventAppendResult> {
        let LockedAppend {
            chain_root_id,
            thread_id,
            events,
            snapshot_updates,
            signer,
            runtime_liveness,
            expected,
        } = request;
        ensure_state_chain_lock(&self.refs_root, chain_lock, chain_root_id)?;
        // A stale anchor must refuse before any pending retirement is cancelled.
        if let Some(expected) = expected {
            let mut cache = self.head_cache.lock().expect("head_cache lock");
            if !chain::thread_append_anchor_matches_under_lock(
                &self.cas_root,
                &self.refs_root,
                self.trust_store.as_ref(),
                &mut cache,
                chain_lock,
                chain::ExpectedThreadAppendAnchor {
                    chain_root_id,
                    thread_id,
                    chain_head_hash: &expected.chain_head_hash,
                    thread_last_event_hash: expected.thread_last_event_hash.as_deref(),
                    thread_last_seq: expected.thread_last_seq,
                },
            )? {
                return Ok(ConditionalThreadEventAppendResult::Conflict);
            }
        }
        let invalidates_retirement = self.pending_remove_invalidated_by_mutation_locked(
            chain_root_id,
            ProspectiveChainMutation::UpdateSnapshots(&snapshot_updates),
            chain_lock,
        )?;
        let cancelled_remove = self.converge_pending_before_set_locked(
            chain_root_id,
            invalidates_retirement,
            runtime_liveness,
            chain_lock,
        )?;
        let mut cache = self.head_cache.lock().expect("head_cache lock");
        let input = chain::AppendEventsInput {
            cas_root: &self.cas_root,
            refs_root: &self.refs_root,
            chain_root_id,
            thread_id,
            events,
            snapshot_updates: snapshot_updates.clone(),
            signer,
        };
        let appended = match expected {
            Some(expected) => chain::append_events_with_trust_under_lock_if_anchor(
                input,
                self.trust_store.as_ref(),
                &mut cache,
                chain_lock,
                chain::ExpectedThreadAppendAnchor {
                    chain_root_id,
                    thread_id,
                    chain_head_hash: &expected.chain_head_hash,
                    thread_last_event_hash: expected.thread_last_event_hash.as_deref(),
                    thread_last_seq: expected.thread_last_seq,
                },
            ),
            None => chain::append_events_with_trust_under_lock(
                input,
                self.trust_store.as_ref(),
                &mut cache,
                chain_lock,
            )
            .map(chain::ConditionalAppendResult::Appended),
        };
        let result = match appended {
            Ok(chain::ConditionalAppendResult::Appended(result)) => result,
            Ok(chain::ConditionalAppendResult::Conflict) => {
                return Ok(ConditionalThreadEventAppendResult::Conflict);
            }
            Err(error) => {
                let error = self.failed_write_after_cancelled_remove(
                    chain_root_id,
                    cancelled_remove.as_ref(),
                    chain_lock,
                    error,
                );
                self.note_pending_transition_error(chain_root_id, "append_events", &error);
                return Err(error);
            }
        };
        if let Err(error) = self.mark_pending_set_applying_locked(
            chain_root_id,
            &result.chain_state_hash,
            chain_lock,
        ) {
            self.note_pending_transition_error(chain_root_id, "append_events", &error);
            return Err(error);
        }

        let projected = project_committed_chain(
            &self.projection,
            &cache,
            chain_root_id,
            &result.chain_state_hash,
            || {
                for event in &result.events {
                    if event.durability.is_projection_indexed() {
                        projection::project_event(&self.projection, event).with_context(|| {
                            format!("projection failed for event chain_seq={}", event.chain_seq)
                        })?;
                    }
                }

                for update in &result.snapshot_updates {
                    projection::project_thread_snapshot(
                        &self.projection,
                        &update.new_snapshot,
                        chain_root_id,
                    )
                    .with_context(|| {
                        format!(
                            "projection failed for snapshot update thread_id={}",
                            update.thread_id
                        )
                    })?;
                }

                Ok(())
            },
        );

        let committed_hash = result.chain_state_hash.clone();
        Ok(ConditionalThreadEventAppendResult::Committed(
            self.committed_write_locked(
                result,
                chain_root_id,
                &committed_hash,
                "append_events",
                projected,
                chain_lock,
            ),
        ))
    }

    /// Add a new thread and its initial durable events in one chain-head update.
    ///
    /// This is used for relation-bearing child creation where a thread must not
    /// become durable without the events that define what kind of child it is.
    #[cfg(test)]
    pub fn add_thread_with_events(
        &self,
        chain_root_id: &str,
        snapshot: ThreadSnapshot,
        events: Vec<ThreadEvent>,
        signer: &dyn Signer,
    ) -> anyhow::Result<CommittedWrite<AddThreadWithEventsResult>> {
        let guard = self.pinned_authority()?.acquire_shared_guard()?;
        self.add_thread_with_events_inner(
            chain_root_id,
            snapshot,
            None,
            events,
            signer,
            None,
            &guard,
        )
    }

    pub fn add_thread_with_events_admitted(
        &self,
        chain_root_id: &str,
        snapshot: ThreadSnapshot,
        events: Vec<ThreadEvent>,
        signer: &dyn Signer,
        runtime_liveness: &dyn RuntimeLivenessInspector,
        cas_mutation_guard: &crate::recovery::CasMutationGuard,
    ) -> anyhow::Result<CommittedWrite<AddThreadWithEventsResult>> {
        self.add_thread_with_events_inner(
            chain_root_id,
            snapshot,
            None,
            events,
            signer,
            Some(runtime_liveness),
            cas_mutation_guard,
        )
    }

    /// Add a member thread whose initial events carry it to a successor
    /// snapshot under one signed head: the member is created and transitions
    /// (for example created→running for an in-process handler child born in
    /// its parent's chain) in one authoritative commit.
    pub fn add_thread_with_event_successor_admitted(
        &self,
        request: AddThreadWithEventSuccessorRequest<'_>,
    ) -> anyhow::Result<CommittedWrite<AddThreadWithEventsResult>> {
        self.add_thread_with_events_inner(
            request.chain_root_id,
            request.genesis_snapshot,
            Some(request.event_successor_snapshot),
            request.events,
            request.signer,
            Some(request.runtime_liveness),
            request.cas_mutation_guard,
        )
    }

    fn add_thread_with_events_inner(
        &self,
        chain_root_id: &str,
        snapshot: ThreadSnapshot,
        event_successor_snapshot: Option<ThreadSnapshot>,
        events: Vec<ThreadEvent>,
        signer: &dyn Signer,
        runtime_liveness: Option<&dyn RuntimeLivenessInspector>,
        cas_mutation_guard: &crate::recovery::CasMutationGuard,
    ) -> anyhow::Result<CommittedWrite<AddThreadWithEventsResult>> {
        if events.iter().any(|event| !event.durability.is_cas_stored()) {
            anyhow::bail!("StateDb::add_thread_with_events cannot persist ephemeral events");
        }

        self.ensure_cas_mutation_guard(cas_mutation_guard)?;
        let chain_lock = crate::chain::ChainLock::acquire_in_refs_directory(
            &self._refs_directory,
            &self._cas_directory,
            &self.recovery,
            chain_root_id,
        )?;
        let invalidates_retirement = self.pending_remove_invalidated_by_mutation_locked(
            chain_root_id,
            ProspectiveChainMutation::AddSnapshot(&snapshot),
            &chain_lock,
        )?;
        let cancelled_remove = self.converge_pending_before_set_locked(
            chain_root_id,
            invalidates_retirement,
            runtime_liveness,
            &chain_lock,
        )?;
        let mut cache = self.head_cache.lock().expect("head_cache lock");
        let result = match chain::add_thread_to_chain_with_events_and_trust_under_lock(
            &self.cas_root,
            &self.refs_root,
            chain_root_id,
            snapshot.clone(),
            event_successor_snapshot,
            events,
            signer,
            self.trust_store.as_ref(),
            &mut cache,
            &chain_lock,
        ) {
            Ok(result) => result,
            Err(error) => {
                let error = self.failed_write_after_cancelled_remove(
                    chain_root_id,
                    cancelled_remove.as_ref(),
                    &chain_lock,
                    error,
                );
                self.note_pending_transition_error(chain_root_id, "add_thread_with_events", &error);
                return Err(error);
            }
        };
        if let Err(error) = self.mark_pending_set_applying_locked(
            chain_root_id,
            &result.chain_state_hash,
            &chain_lock,
        ) {
            self.note_pending_transition_error(chain_root_id, "add_thread_with_events", &error);
            return Err(error);
        }

        let projected = project_committed_chain(
            &self.projection,
            &cache,
            chain_root_id,
            &result.chain_state_hash,
            || {
                projection::project_thread_snapshot_with_events_in_transaction(
                    &self.projection,
                    &result.snapshot,
                    chain_root_id,
                    &result.events,
                )
                .context("projecting exact committed thread and initial events")
            },
        );

        let committed_hash = result.chain_state_hash.clone();
        Ok(self.committed_write_locked(
            result,
            chain_root_id,
            &committed_hash,
            "add_thread_with_events",
            projected,
            &chain_lock,
        ))
    }

    /// Add a thread and append events to an existing thread under one signed
    /// chain head, then project the entire cross-thread transition in one
    /// projection transaction.
    #[allow(clippy::too_many_arguments)]
    #[cfg(test)]
    pub fn add_thread_with_events_and_append(
        &self,
        chain_root_id: &str,
        snapshot: ThreadSnapshot,
        new_thread_events: Vec<ThreadEvent>,
        existing_thread_id: &str,
        existing_thread_events: Vec<ThreadEvent>,
        snapshot_updates: Vec<SnapshotUpdate>,
        signer: &dyn Signer,
    ) -> anyhow::Result<CommittedWrite<AddThreadWithEventsResult>> {
        let guard = self.pinned_authority()?.acquire_shared_guard()?;
        self.add_thread_with_events_and_append_inner(
            chain_root_id,
            snapshot,
            new_thread_events,
            existing_thread_id,
            existing_thread_events,
            snapshot_updates,
            signer,
            None,
            &guard,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add_thread_with_events_and_append_admitted(
        &self,
        chain_root_id: &str,
        snapshot: ThreadSnapshot,
        new_thread_events: Vec<ThreadEvent>,
        existing_thread_id: &str,
        existing_thread_events: Vec<ThreadEvent>,
        snapshot_updates: Vec<SnapshotUpdate>,
        signer: &dyn Signer,
        runtime_liveness: &dyn RuntimeLivenessInspector,
        cas_mutation_guard: &crate::recovery::CasMutationGuard,
    ) -> anyhow::Result<CommittedWrite<AddThreadWithEventsResult>> {
        self.add_thread_with_events_and_append_inner(
            chain_root_id,
            snapshot,
            new_thread_events,
            existing_thread_id,
            existing_thread_events,
            snapshot_updates,
            signer,
            Some(runtime_liveness),
            cas_mutation_guard,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn add_thread_with_events_and_append_inner(
        &self,
        chain_root_id: &str,
        snapshot: ThreadSnapshot,
        new_thread_events: Vec<ThreadEvent>,
        existing_thread_id: &str,
        existing_thread_events: Vec<ThreadEvent>,
        snapshot_updates: Vec<SnapshotUpdate>,
        signer: &dyn Signer,
        runtime_liveness: Option<&dyn RuntimeLivenessInspector>,
        cas_mutation_guard: &crate::recovery::CasMutationGuard,
    ) -> anyhow::Result<CommittedWrite<AddThreadWithEventsResult>> {
        if new_thread_events
            .iter()
            .chain(existing_thread_events.iter())
            .any(|event| !event.durability.is_cas_stored())
        {
            anyhow::bail!(
                "StateDb::add_thread_with_events_and_append cannot persist ephemeral events"
            );
        }

        self.ensure_cas_mutation_guard(cas_mutation_guard)?;
        let chain_lock = crate::chain::ChainLock::acquire_in_refs_directory(
            &self._refs_directory,
            &self._cas_directory,
            &self.recovery,
            chain_root_id,
        )?;
        let invalidates_retirement = self.pending_remove_invalidated_by_mutation_locked(
            chain_root_id,
            ProspectiveChainMutation::AddContinuation {
                snapshot: &snapshot,
                source_thread_id: existing_thread_id,
            },
            &chain_lock,
        )?;
        let cancelled_remove = self.converge_pending_before_set_locked(
            chain_root_id,
            invalidates_retirement,
            runtime_liveness,
            &chain_lock,
        )?;
        let mut cache = self.head_cache.lock().expect("head_cache lock");
        let result = match chain::add_thread_to_chain_with_events_and_append_with_trust_under_lock(
            &self.cas_root,
            &self.refs_root,
            chain_root_id,
            snapshot,
            new_thread_events,
            existing_thread_id,
            existing_thread_events,
            snapshot_updates,
            signer,
            self.trust_store.as_ref(),
            &mut cache,
            &chain_lock,
        ) {
            Ok(result) => result,
            Err(error) => {
                let error = self.failed_write_after_cancelled_remove(
                    chain_root_id,
                    cancelled_remove.as_ref(),
                    &chain_lock,
                    error,
                );
                self.note_pending_transition_error(
                    chain_root_id,
                    "add_thread_with_events_and_append",
                    &error,
                );
                return Err(error);
            }
        };
        if let Err(error) = self.mark_pending_set_applying_locked(
            chain_root_id,
            &result.chain_state_hash,
            &chain_lock,
        ) {
            self.note_pending_transition_error(
                chain_root_id,
                "add_thread_with_events_and_append",
                &error,
            );
            return Err(error);
        }
        let projected = project_committed_chain(
            &self.projection,
            &cache,
            chain_root_id,
            &result.chain_state_hash,
            || {
                projection::project_thread_snapshot_with_events_in_transaction(
                    &self.projection,
                    &result.snapshot,
                    chain_root_id,
                    &result.events,
                )
                .context("projecting atomic new-thread and existing-thread events")?;
                for update in &result.snapshot_updates {
                    projection::project_thread_snapshot(
                        &self.projection,
                        &update.new_snapshot,
                        chain_root_id,
                    )
                    .with_context(|| {
                        format!(
                            "projecting atomic snapshot update for thread {}",
                            update.thread_id
                        )
                    })?;
                }
                Ok(())
            },
        );
        let committed_hash = result.chain_state_hash.clone();
        Ok(self.committed_write_locked(
            result,
            chain_root_id,
            &committed_hash,
            "add_thread_with_events_and_append",
            projected,
            &chain_lock,
        ))
    }

    // ── Project head refs (principal-scoped) ──────────────────────

    /// Read a principal-scoped project head ref. Returns the project
    /// snapshot hash, or `None` if no HEAD exists.
    pub fn read_project_head(
        &self,
        principal_key: &str,
        project_hash: &str,
    ) -> anyhow::Result<Option<String>> {
        Ok(crate::refs::read_verified_project_head_ref_in_directory(
            &self._refs_directory,
            principal_key,
            project_hash,
            self.trust_store.as_ref(),
        )?
        .map(|head| head.target_hash))
    }

    /// Write a principal-scoped project head ref.
    pub fn write_project_head_ref(
        &self,
        principal_key: &str,
        project_hash: &str,
        project_snapshot_hash: &str,
        signer: &dyn Signer,
        cas_mutation_guard: &crate::recovery::CasMutationGuard,
    ) -> anyhow::Result<()> {
        self.ensure_cas_mutation_guard(cas_mutation_guard)?;
        crate::signer::ensure_signer_trusted(signer, self.trust_store.as_ref())?;
        let _project_lock = crate::refs::ProjectHeadLock::acquire_in_refs_directory(
            &self._refs_directory,
            principal_key,
            project_hash,
        )?;
        crate::refs::write_verified_project_head_ref_in_directory(
            &self._refs_directory,
            principal_key,
            project_hash,
            project_snapshot_hash,
            signer,
            self.trust_store.as_ref(),
            &_project_lock,
        )
    }

    /// Advance a principal-scoped project head ref (compare-and-swap).
    pub fn advance_project_head_ref(
        &self,
        principal_key: &str,
        project_hash: &str,
        new_snapshot_hash: &str,
        expected_current_hash: &str,
        signer: &dyn Signer,
        cas_mutation_guard: &crate::recovery::CasMutationGuard,
    ) -> anyhow::Result<()> {
        self.ensure_cas_mutation_guard(cas_mutation_guard)?;
        crate::signer::ensure_signer_trusted(signer, self.trust_store.as_ref())?;
        let _project_lock = crate::refs::ProjectHeadLock::acquire_in_refs_directory(
            &self._refs_directory,
            principal_key,
            project_hash,
        )?;
        crate::refs::advance_verified_project_head_ref_in_directory(
            &self._refs_directory,
            principal_key,
            project_hash,
            new_snapshot_hash,
            expected_current_hash,
            signer,
            self.trust_store.as_ref(),
            &_project_lock,
        )
    }

    /// Read and verify the node-level deployed-project ref.
    pub fn read_deployed_project_ref(
        &self,
        project_hash: &str,
    ) -> anyhow::Result<Option<SignedRef>> {
        crate::refs::read_verified_deployed_project_ref_in_directory(
            &self._refs_directory,
            project_hash,
            self.trust_store.as_ref(),
        )
    }

    /// Publish the first node-level deployed-project ref with a trusted signer.
    pub fn write_deployed_project_ref(
        &self,
        project_hash: &str,
        project_snapshot_hash: &str,
        signer: &dyn Signer,
        cas_mutation_guard: &crate::recovery::CasMutationGuard,
    ) -> anyhow::Result<()> {
        self.ensure_cas_mutation_guard(cas_mutation_guard)?;
        crate::signer::ensure_signer_trusted(signer, self.trust_store.as_ref())?;
        let _project_lock = crate::refs::DeployedProjectHeadLock::acquire_in_refs_directory(
            &self._refs_directory,
            project_hash,
        )?;
        crate::refs::write_verified_deployed_project_ref_in_directory(
            &self._refs_directory,
            project_hash,
            project_snapshot_hash,
            signer,
            self.trust_store.as_ref(),
            &_project_lock,
        )
    }

    /// Advance the node-level deployed-project ref after verifying its current
    /// signed envelope and exact path binding.
    pub fn advance_deployed_project_ref(
        &self,
        project_hash: &str,
        new_snapshot_hash: &str,
        expected_current_hash: &str,
        signer: &dyn Signer,
        cas_mutation_guard: &crate::recovery::CasMutationGuard,
    ) -> anyhow::Result<()> {
        self.ensure_cas_mutation_guard(cas_mutation_guard)?;
        crate::signer::ensure_signer_trusted(signer, self.trust_store.as_ref())?;
        let _project_lock = crate::refs::DeployedProjectHeadLock::acquire_in_refs_directory(
            &self._refs_directory,
            project_hash,
        )?;
        crate::refs::advance_verified_deployed_project_ref_in_directory(
            &self._refs_directory,
            project_hash,
            new_snapshot_hash,
            expected_current_hash,
            signer,
            self.trust_store.as_ref(),
            &_project_lock,
        )
    }

    /// Write a namespace-neutral signed head under `refs/generic`.
    pub fn write_generic_head_ref(
        &self,
        namespace: &str,
        name: &str,
        target_hash: &str,
        signer: &dyn Signer,
        cas_mutation_guard: &crate::recovery::CasMutationGuard,
    ) -> anyhow::Result<()> {
        self.ensure_cas_mutation_guard(cas_mutation_guard)?;
        if namespace == "chains" {
            anyhow::bail!("chain-head publication requires an admitted CAS mutation guard");
        }
        crate::signer::ensure_signer_trusted(signer, self.trust_store.as_ref())?;
        let _head_lock = crate::refs::GenericHeadLock::acquire_in_refs_directory(
            &self._refs_directory,
            namespace,
            name,
        )?;
        crate::refs::write_verified_generic_head_ref_in_directory(
            &self._refs_directory,
            namespace,
            name,
            target_hash,
            signer,
            self.trust_store.as_ref(),
            &_head_lock,
        )
    }

    /// Publish an already-staged authoritative chain closure while consuming
    /// the caller's guard-before-store-lock proof.
    pub fn write_chain_head_ref_admitted(
        &self,
        chain_root_id: &str,
        target_hash: &str,
        signer: &dyn Signer,
        cas_mutation_guard: &crate::recovery::CasMutationGuard,
        limits: crate::object_closure::ObjectClosureLimits,
    ) -> anyhow::Result<()> {
        self.publish_external_chain_head(
            chain_root_id,
            target_hash,
            signer,
            cas_mutation_guard,
            None,
            limits,
        )
    }

    /// Publish the one successor head authorized by a source-node writer
    /// transition. Unlike ordinary import this may establish a chain on a new
    /// node, but only after the signed grant, target placement attestation,
    /// continuation event, snapshots, and exact source anchor agree.
    pub fn write_transferred_chain_head_admitted(
        &self,
        transition: &crate::sync::AdmittedChainWriterTransition,
        signer: &dyn Signer,
        cas_mutation_guard: &crate::recovery::CasMutationGuard,
        limits: crate::object_closure::ObjectClosureLimits,
    ) -> anyhow::Result<()> {
        transition.validate()?;
        self.publish_external_chain_head(
            &transition.evidence.chain_root_id,
            &transition.target_chain_head_hash,
            signer,
            cas_mutation_guard,
            Some(transition),
            limits,
        )
    }

    /// Read and verify a namespace-neutral signed head from `refs/generic`.
    pub fn read_generic_head_ref(
        &self,
        namespace: &str,
        name: &str,
    ) -> anyhow::Result<Option<SignedRef>> {
        crate::refs::read_verified_generic_head_ref_in_directory(
            &self._refs_directory,
            namespace,
            name,
            self.trust_store.as_ref(),
        )
    }

    pub fn list_due_terminal_chains(
        &self,
        now: &str,
        limit: usize,
        after: Option<&DueTerminalChainCursor>,
    ) -> anyhow::Result<Vec<DueTerminalChain>> {
        self.projection.list_due_terminal_chains(now, limit, after)
    }

    pub fn chain_retention_projection(
        &self,
        chain_root_id: &str,
    ) -> anyhow::Result<Option<ChainRetentionProjection>> {
        self.projection.chain_retention_projection(chain_root_id)
    }

    /// Acquire this store's exact chain authority, including its retained refs,
    /// CAS, and recovery roots.
    pub fn acquire_chain_lock(
        &self,
        chain_root_id: &str,
    ) -> anyhow::Result<crate::chain::ChainLock> {
        crate::chain::ChainLock::acquire_in_refs_directory(
            &self._refs_directory,
            &self._cas_directory,
            &self.recovery,
            chain_root_id,
        )
    }

    /// Acquire an already-established exact chain authority without creating
    /// its directory or lock anchor.
    pub fn acquire_existing_chain_lock(
        &self,
        chain_root_id: &str,
    ) -> anyhow::Result<crate::chain::ChainLock> {
        crate::chain::ChainLock::acquire_existing_in_refs_directory(
            &self._refs_directory,
            &self._cas_directory,
            &self.recovery,
            chain_root_id,
        )
    }

    /// Verify, under the exact chain lock, that the current trusted head is
    /// signed by `required_signer` and is an ancestor of `candidate_head`.
    /// The returned hash is the precise local anchor observed by the caller.
    pub fn verify_current_chain_anchor_for_candidate(
        &self,
        chain_root_id: &str,
        candidate_head: &str,
        required_signer: &str,
        required_owner: &str,
        required_origin_site_id: &str,
        cas_mutation_guard: &crate::recovery::CasMutationGuard,
    ) -> anyhow::Result<String> {
        let authority = self.pinned_authority()?;
        authority.ensure_guard(cas_mutation_guard)?;
        let chain_lock = self.acquire_existing_chain_lock(chain_root_id)?;
        let current = chain_lock
            .read_verified_head(self.trust_store.as_ref())?
            .ok_or_else(|| anyhow::anyhow!("chain has no current trusted head"))?;
        if current.signer != required_signer {
            anyhow::bail!("current chain head is not signed by the required authority");
        }
        let root = {
            let mut head_cache = self.head_cache.lock().expect("head_cache lock");
            chain::read_thread_snapshot_with_trust(
                &self.cas_root,
                &self.refs_root,
                &chain_lock,
                chain_root_id,
                chain_root_id,
                self.trust_store.as_ref(),
                &mut head_cache,
            )?
            .ok_or_else(|| anyhow::anyhow!("chain has no authoritative root snapshot"))?
        };
        if root.chain_root_id != chain_root_id
            || root.thread_id != chain_root_id
            || root.requested_by.as_deref() != Some(required_owner)
            || root.origin_site_id != required_origin_site_id
        {
            anyhow::bail!("chain has no exact target-authored owner anchor");
        }
        crate::sync::verify_chain_closure_anchored_pinned(
            &authority.cas_store()?,
            chain_root_id,
            candidate_head,
            &current.target_hash,
        )?;
        Ok(current.target_hash)
    }

    /// Read the current authoritative snapshot through a trust-verified chain
    /// head and the verified in-memory head cache. This never consults the
    /// rebuildable projection. `None` means that the chain head or requested
    /// thread is absent; malformed, untrusted, or mismatched authority fails
    /// closed.
    pub fn read_authoritative_thread_snapshot(
        &self,
        chain_root_id: &str,
        thread_id: &str,
    ) -> anyhow::Result<Option<ThreadSnapshot>> {
        let chain_lock = crate::chain::ChainLock::acquire_in_refs_directory(
            &self._refs_directory,
            &self._cas_directory,
            &self.recovery,
            chain_root_id,
        )?;
        let mut head_cache = self.head_cache.lock().expect("head_cache lock");
        chain::read_thread_snapshot_with_trust(
            &self.cas_root,
            &self.refs_root,
            &chain_lock,
            chain_root_id,
            thread_id,
            self.trust_store.as_ref(),
            &mut head_cache,
        )
    }

    /// Read one current snapshot, its exact thread-entry tip, and their signed
    /// head under the same trust-verified chain authority without consulting
    /// the projection.
    pub fn read_authoritative_thread_snapshot_with_last_event(
        &self,
        chain_root_id: &str,
        thread_id: &str,
    ) -> anyhow::Result<Option<AuthoritativeThreadSnapshotWithLastEventReadback>> {
        let chain_lock = crate::chain::ChainLock::acquire_in_refs_directory(
            &self._refs_directory,
            &self._cas_directory,
            &self.recovery,
            chain_root_id,
        )?;
        let readback = {
            let mut head_cache = self.head_cache.lock().expect("head_cache lock");
            chain::read_thread_snapshot_with_entry_last_event_with_trust(
                &self.cas_root,
                &self.refs_root,
                &chain_lock,
                chain_root_id,
                thread_id,
                self.trust_store.as_ref(),
                &mut head_cache,
            )?
        };
        let Some((chain_head_hash, snapshot, last_event)) = readback else {
            return Ok(None);
        };
        Ok(Some(AuthoritativeThreadSnapshotWithLastEventReadback {
            chain_head_hash,
            snapshot,
            last_event,
        }))
    }

    /// Inspect an exact thread's authenticated history and snapshot under one
    /// signed head. Absence is proven only after the complete bounded walk.
    pub fn read_authoritative_thread_snapshot_with_event_presence(
        &self,
        chain_root_id: &str,
        thread_id: &str,
        event_type: &str,
        max_events: u64,
        max_bytes: u64,
    ) -> anyhow::Result<Option<(String, ThreadSnapshot, bool)>> {
        let authority = self.pinned_authority()?;
        let guard = authority.acquire_shared_guard()?;
        authority.read_thread_snapshot_with_event_presence(
            chain_root_id,
            thread_id,
            event_type,
            max_events,
            max_bytes,
            &guard,
        )
    }

    /// Read the current chain-global and thread-local append anchors through
    /// the trust-verified signed head without traversing event history.
    pub fn read_authoritative_thread_append_anchor(
        &self,
        chain_root_id: &str,
        thread_id: &str,
    ) -> anyhow::Result<Option<AuthoritativeThreadAppendAnchorReadback>> {
        let chain_lock = crate::chain::ChainLock::acquire_in_refs_directory(
            &self._refs_directory,
            &self._cas_directory,
            &self.recovery,
            chain_root_id,
        )?;
        let mut head_cache = self.head_cache.lock().expect("head_cache lock");
        chain::read_authoritative_thread_append_anchor_with_trust(
            &self.cas_root,
            &self.refs_root,
            &chain_lock,
            chain_root_id,
            thread_id,
            self.trust_store.as_ref(),
            &mut head_cache,
        )
        .map(|readback| {
            readback.map(|readback| AuthoritativeThreadAppendAnchorReadback {
                chain_last_event_hash: readback.chain_last_event_hash,
                chain_last_seq: readback.chain_last_seq,
                thread_last_event_hash: readback.thread_last_event_hash,
                thread_last_seq: readback.thread_last_seq,
            })
        })
    }

    /// Read one thread's complete event chain and its current append anchors
    /// from a single trust-verified signed head. Reserved for commit
    /// reconciliation after an append acknowledgement is ambiguous.
    pub fn read_authoritative_thread_append_history(
        &self,
        chain_root_id: &str,
        thread_id: &str,
    ) -> anyhow::Result<Option<AuthoritativeThreadAppendHistoryReadback>> {
        let chain_lock = crate::chain::ChainLock::acquire_in_refs_directory(
            &self._refs_directory,
            &self._cas_directory,
            &self.recovery,
            chain_root_id,
        )?;
        let mut head_cache = self.head_cache.lock().expect("head_cache lock");
        chain::read_authoritative_thread_event_chain_with_trust(
            &self.cas_root,
            &self.refs_root,
            &chain_lock,
            chain_root_id,
            thread_id,
            self.trust_store.as_ref(),
            &mut head_cache,
        )
        .map(|readback| {
            readback.map(|readback| AuthoritativeThreadAppendHistoryReadback {
                anchor: AuthoritativeThreadAppendAnchorReadback {
                    chain_last_event_hash: readback.anchor.chain_last_event_hash,
                    chain_last_seq: readback.anchor.chain_last_seq,
                    thread_last_event_hash: readback.anchor.thread_last_event_hash,
                    thread_last_seq: readback.anchor.thread_last_seq,
                },
                events: readback.events,
            })
        })
    }

    /// Capture one immutable thread-event frontier from the trust-verified
    /// signed head. The chain lock is released before this returns; later pages
    /// authenticate the fixed CAS predecessor path rather than a moving head.
    pub fn capture_thread_event_read_anchor(
        &self,
        chain_root_id: &str,
        thread_id: &str,
        guard: &crate::recovery::CasMutationGuard,
    ) -> anyhow::Result<Option<AuthoritativeThreadEventReadAnchor>> {
        self.ensure_cas_mutation_guard(guard)?;
        let chain_lock = self.acquire_chain_lock(chain_root_id)?;
        let readback = {
            let mut head_cache = self.head_cache.lock().expect("head_cache lock");
            chain::capture_authoritative_thread_event_read_anchor_with_trust(
                &self.cas_root,
                &self.refs_root,
                &chain_lock,
                chain_root_id,
                thread_id,
                self.trust_store.as_ref(),
                &mut head_cache,
            )?
        };
        drop(chain_lock);
        self.ensure_cas_mutation_guard(guard)?;
        Ok(readback.map(|readback| AuthoritativeThreadEventReadAnchor {
            chain_root_id: chain_root_id.to_owned(),
            thread_id: thread_id.to_owned(),
            chain_head_hash: readback.chain_head_hash,
            snapshot_hash: readback.snapshot_hash,
            thread_last_event_hash: readback.thread_last_event_hash,
            thread_last_seq: readback.thread_last_seq,
        }))
    }

    /// Authenticate at most one caller-bounded slice, newest first, from an
    /// immutable anchor. Event bytes are hash-bound to the signed head; running
    /// a registered payload contract over them is the caller's job.
    /// No chain lock is retained while walking event objects.
    /// Exhaustion of a bound returns opaque progress rather than treating a
    /// partial walk as absence.
    pub fn read_thread_event_page(
        &self,
        anchor: &AuthoritativeThreadEventReadAnchor,
        progress: Option<&AuthoritativeThreadEventReadProgress>,
        max_events: u64,
        max_bytes: u64,
        guard: &crate::recovery::CasMutationGuard,
    ) -> anyhow::Result<AuthoritativeThreadEventReadPage> {
        self.ensure_cas_mutation_guard(guard)?;
        anyhow::ensure!(
            max_events > 0 && max_bytes > 0,
            "authoritative thread event page bounds must be positive"
        );
        let (mut next_hash, mut next_seq) = match progress {
            Some(progress)
                if progress.chain_root_id == anchor.chain_root_id
                    && progress.thread_id == anchor.thread_id
                    && progress.chain_head_hash == anchor.chain_head_hash
                    && progress.snapshot_hash == anchor.snapshot_hash
                    && progress.next_thread_seq > 0 =>
            {
                (
                    Some(progress.next_event_hash.clone()),
                    progress.next_thread_seq,
                )
            }
            Some(_) => anyhow::bail!("thread event progress belongs to another fixed anchor"),
            None => (
                anchor.thread_last_event_hash.clone(),
                anchor.thread_last_seq,
            ),
        };
        if next_seq == 0 {
            anyhow::ensure!(
                next_hash.is_none(),
                "thread event anchor extends before sequence one"
            );
            return Ok(AuthoritativeThreadEventReadPage {
                events: Vec::new(),
                progress: None,
            });
        }
        let cas = self.pinned_cas()?;
        let mut events = Vec::new();
        let mut inspected_bytes = 0u64;
        while next_seq != 0 && (events.len() as u64) < max_events {
            let hash = next_hash
                .take()
                .ok_or_else(|| anyhow::anyhow!("thread event anchor ended before its sequence"))?;
            let remaining = max_bytes.saturating_sub(inspected_bytes);
            let limit = (crate::objects::MAX_THREAD_EVENT_SERIALIZED_BYTES as u64).min(remaining);
            if limit == 0 {
                next_hash = Some(hash);
                break;
            }
            let (file, size) = cas
                .open_object(&hash)?
                .ok_or_else(|| anyhow::anyhow!("thread event page CAS object is absent: {hash}"))?;
            if size > limit {
                anyhow::ensure!(
                    !events.is_empty(),
                    "thread event page object exceeds the byte work bound; \
                     increase the per-page byte budget"
                );
                next_hash = Some(hash);
                break;
            }
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut std::io::Read::take(file, limit + 1), &mut bytes)?;
            anyhow::ensure!(
                bytes.len() as u64 <= limit && lillux::sha256_hex(&bytes) == hash,
                "thread event page object changed or exceeded its byte bound"
            );
            let event: ThreadEvent =
                serde_json::from_slice(&bytes).context("decode bounded thread event page")?;
            let canonical = lillux::canonical_json(&event.to_value())
                .context("canonicalize bounded thread event page")?;
            anyhow::ensure!(
                canonical.as_bytes() == bytes.as_slice(),
                "thread event page object is not canonically encoded"
            );
            event.validate()?;
            anyhow::ensure!(
                event.chain_root_id == anchor.chain_root_id
                    && event.thread_id == anchor.thread_id
                    && event.thread_seq == next_seq,
                "thread event page differs from its fixed anchor"
            );
            inspected_bytes = inspected_bytes
                .checked_add(canonical.len() as u64)
                .ok_or_else(|| anyhow::anyhow!("thread event page byte count overflow"))?;
            next_hash.clone_from(&event.prev_thread_event_hash);
            events.push((hash, event));
            next_seq -= 1;
        }
        let progress = if next_seq == 0 {
            anyhow::ensure!(
                next_hash.is_none(),
                "thread event history extends before sequence one"
            );
            None
        } else {
            Some(AuthoritativeThreadEventReadProgress {
                chain_root_id: anchor.chain_root_id.clone(),
                thread_id: anchor.thread_id.clone(),
                chain_head_hash: anchor.chain_head_hash.clone(),
                snapshot_hash: anchor.snapshot_hash.clone(),
                next_event_hash: next_hash.ok_or_else(|| {
                    anyhow::anyhow!("thread event page ended before sequence {next_seq}")
                })?,
                next_thread_seq: next_seq,
            })
        };
        self.ensure_cas_mutation_guard(guard)?;
        Ok(AuthoritativeThreadEventReadPage { events, progress })
    }

    /// Run a short callback only while every supplied thread is still at its
    /// exact trust-verified signed head. `None` means a supplied cut is stale
    /// (changed head, snapshot or tip) or its chain has a pending transition.
    /// A chain that does not exist, and contradictory cuts for one thread,
    /// refuse with an error. Every chain lock stays held through the callback,
    /// which must not acquire locks, reenter state/CAS, perform I/O or write.
    pub fn with_thread_event_read_anchors<T>(
        &self,
        guard: &crate::recovery::CasMutationGuard,
        anchors: &[AuthoritativeThreadEventReadAnchor],
        action: impl FnOnce() -> anyhow::Result<T>,
    ) -> anyhow::Result<Option<T>> {
        self.ensure_cas_mutation_guard(guard)?;
        anyhow::ensure!(
            !anchors.is_empty(),
            "thread read action requires at least one exact anchor"
        );
        let live = Self::distinct_read_anchors(anchors)?;
        let locks = self.acquire_read_cut_locks(live.keys().copied())?;
        if !self.live_read_cuts_are_current(&locks, &live)? {
            return Ok(None);
        }
        self.ensure_cas_mutation_guard(guard)?;
        action().map(Some)
    }

    /// Read exclusion for a historical cut that mixes live chains with retired
    /// ones. Live anchors must still match their exact frontier. Each retired
    /// dependency must still be headless, without a pending transition, and
    /// fenced (not released) with the same retained closure; a fence naming a
    /// different closure refuses with an error, as does a chain named both
    /// live and retired, a conflicting duplicate retired cut, or an empty
    /// retired list. `None` means a cut is stale. Locks are taken in chain
    /// order and held through the callback, which must not acquire locks,
    /// reenter state/CAS, perform I/O or write.
    pub fn with_thread_event_read_mixed_cut<T>(
        &self,
        guard: &crate::recovery::CasMutationGuard,
        current_anchors: &[AuthoritativeThreadEventReadAnchor],
        retired: &[RetiredChainDependency],
        action: impl FnOnce() -> anyhow::Result<T>,
    ) -> anyhow::Result<Option<T>> {
        self.ensure_cas_mutation_guard(guard)?;
        anyhow::ensure!(
            !retired.is_empty(),
            "mixed read exclusion requires a nonempty retired cut"
        );
        let live = Self::distinct_read_anchors(current_anchors)?;
        let mut retired_cuts = BTreeMap::new();
        for dependency in retired {
            anyhow::ensure!(
                !dependency.chain_root_id.is_empty()
                    && !live.contains_key(dependency.chain_root_id.as_str()),
                "mixed read exclusion mixes live and retired cuts"
            );
            if let Some(prior) = retired_cuts.insert(dependency.chain_root_id.as_str(), dependency)
            {
                anyhow::ensure!(
                    prior == dependency,
                    "mixed read exclusion has conflicting retired cuts"
                );
            }
        }
        let locks =
            self.acquire_read_cut_locks(live.keys().copied().chain(retired_cuts.keys().copied()))?;
        for (root, dependency) in &retired_cuts {
            let lock = locks
                .get(root)
                .ok_or_else(|| anyhow::anyhow!("retired read cut lock was not acquired"))?;
            if self.recovery.read_pending(root)?.is_some()
                || lock
                    .read_verified_head(self.trust_store.as_ref())?
                    .is_some()
            {
                return Ok(None);
            }
            match self.recovery.retired_chain_fence(root)? {
                crate::recovery::RetiredChainFence::Retired { manifest_hash } => {
                    anyhow::ensure!(
                        manifest_hash == dependency.manifest_hash,
                        "retired read dependency differs from its recorded fence"
                    );
                }
                crate::recovery::RetiredChainFence::Absent
                | crate::recovery::RetiredChainFence::Released { .. } => return Ok(None),
            }
        }
        if !self.live_read_cuts_are_current(&locks, &live)? {
            return Ok(None);
        }
        self.ensure_cas_mutation_guard(guard)?;
        action().map(Some)
    }

    /// Load one immutable thread event by its exact CAS hash without trusting
    /// the rebuildable projection. The shared authority guard keeps a
    /// concurrent retention sweep from removing the object mid-read; the
    /// typed envelope and canonical content hash are verified. `None` means
    /// the object is not retained; a non-canonical hash refuses.
    pub fn retained_thread_event_by_hash(
        &self,
        event_hash: &str,
        guard: &crate::recovery::CasMutationGuard,
    ) -> anyhow::Result<Option<ThreadEvent>> {
        self.ensure_cas_mutation_guard(guard)?;
        anyhow::ensure!(
            lillux::valid_hash(event_hash)
                && !event_hash.bytes().any(|byte| byte.is_ascii_uppercase()),
            "retained thread event hash is not canonical"
        );
        let value = self.pinned_cas()?.get_object_bounded(
            event_hash,
            crate::objects::MAX_THREAD_EVENT_SERIALIZED_BYTES as u64,
        )?;
        self.ensure_cas_mutation_guard(guard)?;
        let Some(value) = value else {
            return Ok(None);
        };
        let event: ThreadEvent =
            serde_json::from_value(value).context("decode retained thread event")?;
        event.validate().context("validate retained thread event")?;
        let actual = crate::objects::thread_event::hash_event(&event)
            .context("hash retained thread event")?;
        anyhow::ensure!(
            actual == event_hash,
            "retained thread event content hash changed"
        );
        Ok(Some(event))
    }

    /// Group read anchors by chain, folding identical repeats and refusing
    /// empty scopes or two different cuts for one thread.
    fn distinct_read_anchors(
        anchors: &[AuthoritativeThreadEventReadAnchor],
    ) -> anyhow::Result<BTreeMap<&str, Vec<&AuthoritativeThreadEventReadAnchor>>> {
        let mut by_root = BTreeMap::<&str, Vec<&AuthoritativeThreadEventReadAnchor>>::new();
        for anchor in anchors {
            anyhow::ensure!(
                !anchor.chain_root_id.is_empty() && !anchor.thread_id.is_empty(),
                "thread read action has an empty chain or thread scope"
            );
            let scoped = by_root.entry(anchor.chain_root_id.as_str()).or_default();
            if let Some(prior) = scoped
                .iter()
                .find(|prior| prior.thread_id == anchor.thread_id)
            {
                anyhow::ensure!(
                    *prior == anchor,
                    "thread read action has contradictory exact anchors"
                );
            } else {
                scoped.push(anchor);
            }
        }
        Ok(by_root)
    }

    /// Existing-chain locks for every root, in root order. A root with no
    /// established chain authority refuses rather than being created.
    fn acquire_read_cut_locks<'a>(
        &self,
        roots: impl Iterator<Item = &'a str>,
    ) -> anyhow::Result<BTreeMap<&'a str, crate::chain::ChainLock>> {
        let ordered = roots.collect::<std::collections::BTreeSet<_>>();
        let mut locks = BTreeMap::new();
        for root in ordered {
            locks.insert(root, self.acquire_existing_chain_lock(root)?);
        }
        Ok(locks)
    }

    /// Whether every live anchor still equals the trust-verified signed
    /// frontier under its held chain lock. A pending transition is stale.
    fn live_read_cuts_are_current(
        &self,
        locks: &BTreeMap<&str, crate::chain::ChainLock>,
        live: &BTreeMap<&str, Vec<&AuthoritativeThreadEventReadAnchor>>,
    ) -> anyhow::Result<bool> {
        let mut cache = self.head_cache.lock().expect("head_cache lock");
        for (root, scoped) in live {
            let lock = locks
                .get(root)
                .ok_or_else(|| anyhow::anyhow!("live read cut lock was not acquired"))?;
            if self.recovery.read_pending(root)?.is_some() {
                return Ok(false);
            }
            for anchor in scoped {
                let current = chain::capture_authoritative_thread_event_read_anchor_with_trust(
                    &self.cas_root,
                    &self.refs_root,
                    lock,
                    root,
                    &anchor.thread_id,
                    self.trust_store.as_ref(),
                    &mut cache,
                )?;
                if current.is_none_or(|current| {
                    current.chain_head_hash != anchor.chain_head_hash
                        || current.snapshot_hash != anchor.snapshot_hash
                        || current.thread_last_event_hash != anchor.thread_last_event_hash
                        || current.thread_last_seq != anchor.thread_last_seq
                }) {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    /// Exact signed membership for nonterminal as well as terminal chains.
    /// The caller retains the chain lock through any structural owner check.
    pub fn authoritative_chain_thread_ids_under_lock(
        &self,
        chain_root_id: &str,
        chain_lock: &crate::chain::ChainLock,
    ) -> anyhow::Result<Option<Vec<String>>> {
        ensure_state_chain_lock(&self.refs_root, chain_lock, chain_root_id)?;
        let Some(head) = chain_lock.read_verified_head(self.trust_store.as_ref())? else {
            return Ok(None);
        };
        let cas = self.pinned_cas()?;
        crate::chain::validate_authoritative_history_with_cas(
            &cas,
            chain_root_id,
            &head.target_hash,
            None,
        )?;
        let chain_state: crate::ChainState = load_hashed_cas_object(&cas, &head.target_hash)?;
        chain_state.validate()?;
        anyhow::ensure!(
            chain_state.chain_root_id == chain_root_id,
            "signed chain root differs from {chain_root_id}"
        );
        let mut thread_ids = Vec::with_capacity(chain_state.threads.len());
        for (thread_id, entry) in &chain_state.threads {
            let snapshot: ThreadSnapshot = load_hashed_cas_object(&cas, &entry.snapshot_hash)?;
            snapshot.validate()?;
            anyhow::ensure!(
                snapshot.thread_id == *thread_id
                    && snapshot.chain_root_id == chain_root_id
                    && snapshot.status == entry.status,
                "signed chain member {thread_id} contradicts its snapshot"
            );
            thread_ids.push(thread_id.clone());
        }
        Ok(Some(thread_ids))
    }

    /// Append events to a running target thread only while two exact retained
    /// chain frontiers remain current. Both chain locks are acquired in root-ID
    /// order and held through target publication; the counterpart is never
    /// modified. This is a serialization primitive, not domain authorization.
    /// Missing heads and pending publications are refused as conflicts, never
    /// repaired here.
    pub fn append_running_events_admitted_if_two_anchors(
        &self,
        request: TwoAnchorThreadAppend<'_>,
        cas_mutation_guard: &crate::recovery::CasMutationGuard,
    ) -> anyhow::Result<ConditionalThreadEventAppendResult> {
        self.append_running_events_admitted_if_two_anchors_with_admission(
            request,
            cas_mutation_guard,
            |_| Ok(true),
        )
    }

    /// Domain admission under both ordered chain locks, after the exact
    /// frontiers and signed target state are checked, immediately before
    /// publication. `admit` may read caller-owned state but must not reenter
    /// StateDb or acquire either chain lock. `false` is a clean conflict; an
    /// error publishes nothing and leaves retry policy with the caller.
    pub fn append_running_events_admitted_if_two_anchors_with_admission(
        &self,
        request: TwoAnchorThreadAppend<'_>,
        cas_mutation_guard: &crate::recovery::CasMutationGuard,
        admit: impl FnOnce(TwoAnchorAdmissionView<'_>) -> anyhow::Result<bool>,
    ) -> anyhow::Result<ConditionalThreadEventAppendResult> {
        self.append_running_events_admitted_if_mixed_cut_with_admission(
            request,
            MixedCutDependencies {
                current_anchors: &[],
                retired: &[],
            },
            cas_mutation_guard,
            admit,
        )
    }

    /// Two-anchor append that also excludes every further live or retired
    /// chain the caller's read depended on. Live dependencies must still match
    /// their exact frontier; retired dependencies must still be headless,
    /// unpending and fenced with the same retained closure.
    pub fn append_running_events_admitted_if_mixed_cut_with_admission(
        &self,
        request: TwoAnchorThreadAppend<'_>,
        dependencies: MixedCutDependencies<'_>,
        cas_mutation_guard: &crate::recovery::CasMutationGuard,
        admit: impl FnOnce(TwoAnchorAdmissionView<'_>) -> anyhow::Result<bool>,
    ) -> anyhow::Result<ConditionalThreadEventAppendResult> {
        let TwoAnchorThreadAppend {
            chain_root_id,
            thread_id,
            events,
            signer,
            runtime_liveness,
            expected,
            counterpart,
            required_target_kind,
        } = request;
        self.append_running_events_under_mixed_cut(
            SingleAnchorThreadAppend {
                chain_root_id,
                thread_id,
                events,
                signer,
                runtime_liveness,
                expected,
                required_target_kind,
            },
            Some(counterpart),
            dependencies,
            cas_mutation_guard,
            |view| {
                let (Some(other), Some(other_ids)) =
                    (view.counterpart, view.counterpart_thread_ids)
                else {
                    anyhow::bail!("two-anchor admission lost its counterpart");
                };
                admit(TwoAnchorAdmissionView {
                    target: view.target,
                    counterpart: other,
                    target_thread_ids: view.target_thread_ids,
                    counterpart_thread_ids: other_ids,
                })
            },
        )
    }

    /// Append to a running target whose read cut is the target's exact
    /// frontier plus zero or more further live frontiers and retired chains.
    /// Every dependency lock is taken in root order and held through
    /// publication. Live dependencies must still match their exact frontier;
    /// a retired dependency whose chain regained a head or pending transition,
    /// or whose fence is absent or released, is a conflict; a fence naming a
    /// different closure, a chain named both live and retired, or conflicting
    /// duplicate cuts refuse with an error. `admit` runs under the locks just
    /// before publication, must not reenter StateDb, and `false` is a clean
    /// conflict.
    pub fn append_running_events_admitted_if_single_anchor_mixed_cut_with_admission(
        &self,
        request: SingleAnchorThreadAppend<'_>,
        dependencies: MixedCutDependencies<'_>,
        cas_mutation_guard: &crate::recovery::CasMutationGuard,
        admit: impl FnOnce(SingleAnchorAdmissionView<'_>) -> anyhow::Result<bool>,
    ) -> anyhow::Result<ConditionalThreadEventAppendResult> {
        self.append_running_events_under_mixed_cut(
            request,
            None,
            dependencies,
            cas_mutation_guard,
            |view| {
                admit(SingleAnchorAdmissionView {
                    target: view.target,
                    target_thread_ids: view.target_thread_ids,
                })
            },
        )
    }

    fn append_running_events_under_mixed_cut(
        &self,
        request: SingleAnchorThreadAppend<'_>,
        counterpart: Option<&AuthoritativeThreadEventReadAnchor>,
        dependencies: MixedCutDependencies<'_>,
        cas_mutation_guard: &crate::recovery::CasMutationGuard,
        admit: impl FnOnce(MixedCutAdmission<'_>) -> anyhow::Result<bool>,
    ) -> anyhow::Result<ConditionalThreadEventAppendResult> {
        let SingleAnchorThreadAppend {
            chain_root_id,
            thread_id,
            events,
            signer,
            runtime_liveness,
            expected,
            required_target_kind,
        } = request;
        let MixedCutDependencies {
            current_anchors,
            retired,
        } = dependencies;
        self.ensure_cas_mutation_guard(cas_mutation_guard)?;
        anyhow::ensure!(
            expected.chain_root_id == chain_root_id
                && expected.thread_id == thread_id
                && counterpart.is_none_or(|other| other.chain_root_id != chain_root_id)
                && !events.is_empty()
                && events.iter().all(|event| event.durability.is_cas_stored()),
            "mixed-cut append requires distinct exact chains and durable events"
        );
        let mut live = BTreeMap::new();
        for anchor in current_anchors.iter().chain([expected]).chain(counterpart) {
            anyhow::ensure!(
                !anchor.chain_root_id.is_empty(),
                "append dependency has empty chain identity"
            );
            if let Some(prior) = live.insert(anchor.chain_root_id.as_str(), anchor) {
                anyhow::ensure!(
                    prior == anchor,
                    "append dependencies have conflicting exact live cuts"
                );
            }
        }
        let mut retired_cuts = BTreeMap::new();
        for dependency in retired {
            anyhow::ensure!(
                !dependency.chain_root_id.is_empty()
                    && !live.contains_key(dependency.chain_root_id.as_str()),
                "append dependency mixes live and retired cuts"
            );
            if let Some(prior) = retired_cuts.insert(dependency.chain_root_id.as_str(), dependency)
            {
                anyhow::ensure!(
                    prior == dependency,
                    "append dependencies have conflicting retired cuts"
                );
            }
        }
        let roots = live
            .keys()
            .chain(retired_cuts.keys())
            .copied()
            .collect::<std::collections::BTreeSet<_>>();
        let mut locks = BTreeMap::new();
        for root in roots {
            locks.insert(root, self.acquire_existing_chain_lock(root)?);
        }
        let lock_for = |root: &str| {
            locks
                .get(root)
                .ok_or_else(|| anyhow::anyhow!("append dependency lock was not acquired"))
        };
        for (root, dependency) in &retired_cuts {
            let lock = lock_for(root)?;
            if self.recovery.read_pending(root)?.is_some()
                || lock
                    .read_verified_head(self.trust_store.as_ref())?
                    .is_some()
            {
                return Ok(ConditionalThreadEventAppendResult::Conflict);
            }
            match self.recovery.retired_chain_fence(root)? {
                crate::recovery::RetiredChainFence::Retired { manifest_hash } => {
                    anyhow::ensure!(
                        manifest_hash == dependency.manifest_hash,
                        "retired append dependency differs from its recorded fence"
                    );
                }
                crate::recovery::RetiredChainFence::Absent
                | crate::recovery::RetiredChainFence::Released { .. } => {
                    return Ok(ConditionalThreadEventAppendResult::Conflict);
                }
            }
        }
        let (target_snapshot, counterpart_snapshot) = {
            let mut cache = self.head_cache.lock().expect("head_cache lock");
            for anchor in live.values() {
                let lock = lock_for(&anchor.chain_root_id)?;
                let current = if self.recovery.read_pending(&anchor.chain_root_id)?.is_some() {
                    None
                } else {
                    chain::capture_authoritative_thread_event_read_anchor_with_trust(
                        &self.cas_root,
                        &self.refs_root,
                        lock,
                        &anchor.chain_root_id,
                        &anchor.thread_id,
                        self.trust_store.as_ref(),
                        &mut cache,
                    )?
                };
                if current.is_none_or(|current| {
                    current.chain_head_hash != anchor.chain_head_hash
                        || current.snapshot_hash != anchor.snapshot_hash
                        || current.thread_last_event_hash != anchor.thread_last_event_hash
                        || current.thread_last_seq != anchor.thread_last_seq
                }) {
                    return Ok(ConditionalThreadEventAppendResult::Conflict);
                }
            }
            let Some(target) = chain::read_thread_snapshot_with_trust(
                &self.cas_root,
                &self.refs_root,
                lock_for(chain_root_id)?,
                chain_root_id,
                thread_id,
                self.trust_store.as_ref(),
                &mut cache,
            )?
            else {
                return Ok(ConditionalThreadEventAppendResult::Conflict);
            };
            if target.chain_root_id != chain_root_id
                || target.thread_id != thread_id
                || target.status != crate::objects::ThreadStatus::Running
                || target.kind_name != required_target_kind
            {
                return Ok(ConditionalThreadEventAppendResult::Conflict);
            }
            let other = match counterpart {
                None => None,
                Some(counterpart) => {
                    let Some(other) = chain::read_thread_snapshot_with_trust(
                        &self.cas_root,
                        &self.refs_root,
                        lock_for(&counterpart.chain_root_id)?,
                        &counterpart.chain_root_id,
                        &counterpart.thread_id,
                        self.trust_store.as_ref(),
                        &mut cache,
                    )?
                    else {
                        return Ok(ConditionalThreadEventAppendResult::Conflict);
                    };
                    if other.chain_root_id != counterpart.chain_root_id
                        || other.thread_id != counterpart.thread_id
                    {
                        return Ok(ConditionalThreadEventAppendResult::Conflict);
                    }
                    Some(other)
                }
            };
            (target, other)
        };
        let target_lock = lock_for(chain_root_id)?;
        let target_thread_ids = self
            .authoritative_chain_thread_ids_under_lock(chain_root_id, target_lock)?
            .ok_or_else(|| anyhow::anyhow!("mixed-cut target lost signed membership"))?;
        let counterpart_thread_ids = match counterpart {
            None => None,
            Some(counterpart) => Some(
                self.authoritative_chain_thread_ids_under_lock(
                    &counterpart.chain_root_id,
                    lock_for(&counterpart.chain_root_id)?,
                )?
                .ok_or_else(|| anyhow::anyhow!("mixed-cut counterpart lost signed membership"))?,
            ),
        };
        if !admit(MixedCutAdmission {
            target: &target_snapshot,
            counterpart: counterpart_snapshot.as_ref(),
            target_thread_ids: &target_thread_ids,
            counterpart_thread_ids: counterpart_thread_ids.as_deref(),
        })? {
            return Ok(ConditionalThreadEventAppendResult::Conflict);
        }
        // Every dependency lock stays held until this append returns; the
        // helper reuses the held target lock and never reacquires it.
        self.append_events_under_chain_lock(
            LockedAppend {
                chain_root_id,
                thread_id,
                events,
                snapshot_updates: Vec::new(),
                signer,
                runtime_liveness: Some(runtime_liveness),
                expected: Some(expected),
            },
            target_lock,
        )
    }

    /// Read one root publication and its canonical predecessor genesis under
    /// one trust-verified chain lock. `None` proves the signed head is absent.
    pub fn read_authoritative_root_birth(
        &self,
        chain_root_id: &str,
    ) -> anyhow::Result<Option<AuthoritativeRootBirthReadback>> {
        let chain_lock = crate::chain::ChainLock::acquire_in_refs_directory(
            &self._refs_directory,
            &self._cas_directory,
            &self.recovery,
            chain_root_id,
        )?;
        let mut head_cache = self.head_cache.lock().expect("head_cache lock");
        let readback = chain::read_authoritative_root_birth_with_trust(
            &self.cas_root,
            &self.refs_root,
            &chain_lock,
            chain_root_id,
            self.trust_store.as_ref(),
            &mut head_cache,
        )?;
        Ok(readback.map(|readback| AuthoritativeRootBirthReadback {
            root: readback.root.map(|root| AuthoritativeRootThreadReadback {
                snapshot: root.snapshot,
                events: root.events,
                is_chain_final_event: root.is_chain_final_event,
            }),
            only_root_thread: readback.only_root_thread,
            predecessor: readback
                .predecessor
                .map(|predecessor| AuthoritativeRootGenesisReadback {
                    snapshot: predecessor.snapshot,
                    updated_at: predecessor.updated_at,
                    prev_chain_state_hash: predecessor.prev_chain_state_hash,
                    last_event_hash: predecessor.last_event_hash,
                    last_chain_seq: predecessor.last_chain_seq,
                    only_root_thread: predecessor.only_root_thread,
                    root_last_event_hash: predecessor.root_last_event_hash,
                    root_last_thread_seq: predecessor.root_last_thread_seq,
                }),
        }))
    }

    /// Reproduce root genesis snapshot normalization before the zero-event
    /// predecessor is written.
    pub fn normalize_expected_root_birth(
        mut expected: ThreadSnapshot,
    ) -> anyhow::Result<ThreadSnapshot> {
        expected.last_event_hash = None;
        expected.last_chain_seq = 0;
        expected.last_thread_seq = 0;
        let timestamp_floor = expected.created_at.clone();
        let chain_root_id = expected.chain_root_id.clone();
        chain::normalize_prospective_new_thread(
            &mut expected,
            &chain_root_id,
            &timestamp_floor,
            None,
        )?;
        Ok(expected)
    }

    /// Reproduce initial root event timestamp normalization against the
    /// canonical zero-event genesis.
    pub fn normalize_expected_root_events(
        mut expected: Vec<ThreadEvent>,
        genesis_updated_at: &str,
    ) -> anyhow::Result<Vec<ThreadEvent>> {
        chain::normalize_prospective_event_timestamps(&mut expected, genesis_updated_at)?;
        Ok(expected)
    }

    /// Reproduce the optional Created-to-Running snapshot transition committed
    /// in the same head as a recorded in-process root's initial event batch.
    pub fn normalize_expected_root_event_successor(
        birth: &ThreadSnapshot,
        mut expected: ThreadSnapshot,
        last_event_hash: String,
        event_count: u64,
        append_timestamp: &str,
    ) -> anyhow::Result<ThreadSnapshot> {
        expected.last_event_hash = Some(last_event_hash);
        expected.last_chain_seq = event_count;
        expected.last_thread_seq = event_count;
        chain::normalize_prospective_snapshot_transition(birth, &mut expected, append_timestamp)?;
        Ok(expected)
    }

    /// Read the source snapshot and entry-final event plus the successor
    /// snapshot and complete event history from one trust-verified chain head,
    /// together with the predecessor head's global and source-thread anchors.
    /// Holding one chain lock across the read makes the result suitable for
    /// proving an atomic cross-thread transition without consulting the
    /// rebuildable projection.
    pub fn read_authoritative_continuation(
        &self,
        chain_root_id: &str,
        source_thread_id: &str,
        successor_thread_id: &str,
    ) -> anyhow::Result<AuthoritativeThreadPairReadback> {
        let chain_lock = crate::chain::ChainLock::acquire_in_refs_directory(
            &self._refs_directory,
            &self._cas_directory,
            &self.recovery,
            chain_root_id,
        )?;
        let mut head_cache = self.head_cache.lock().expect("head_cache lock");
        let readback = chain::read_authoritative_continuation_pair_with_trust(
            &self.cas_root,
            &self.refs_root,
            &chain_lock,
            chain::ContinuationReadIdentity {
                chain_root_id,
                source_thread_id,
                successor_thread_id,
            },
            self.trust_store.as_ref(),
            &mut head_cache,
        )?;
        Ok(AuthoritativeThreadPairReadback {
            source: readback
                .source
                .map(|source| AuthoritativeThreadSnapshotReadback {
                    snapshot: source.snapshot,
                    last_event: source.last_event,
                    is_chain_final_event: source.is_chain_final_event,
                }),
            successor: readback
                .successor
                .map(|successor| AuthoritativeThreadEventChainReadback {
                    snapshot: successor.snapshot,
                    events: successor.events,
                }),
            predecessor: readback.predecessor.map(|predecessor| {
                AuthoritativeContinuationPredecessorReadback {
                    updated_at: predecessor.updated_at,
                    last_event_hash: predecessor.last_event_hash,
                    last_chain_seq: predecessor.last_chain_seq,
                    source_was_present: predecessor.source_was_present,
                    source_last_event_hash: predecessor.source_last_event_hash,
                    source_last_thread_seq: predecessor.source_last_thread_seq,
                    successor_was_absent: predecessor.successor_was_absent,
                }
            }),
        })
    }

    /// Reproduce the two-stage event timestamp normalization used by the
    /// atomic add-successor/append-source transition.
    pub fn normalize_expected_continuation_events(
        mut successor_events: Vec<ThreadEvent>,
        source_event: ThreadEvent,
        predecessor_updated_at: &str,
    ) -> anyhow::Result<(Vec<ThreadEvent>, ThreadEvent)> {
        let successor_floor = chain::normalize_prospective_event_timestamps(
            &mut successor_events,
            predecessor_updated_at,
        )?;
        let mut source_events = vec![source_event];
        chain::normalize_prospective_event_timestamps(&mut source_events, &successor_floor)?;
        let source_event = source_events
            .pop()
            .expect("continuation source event batch is non-empty");
        Ok((successor_events, source_event))
    }

    /// Reproduce the state-layer stamping and timestamp normalization applied
    /// to a newly committed continuation successor for exact readback
    /// comparison.
    pub fn normalize_expected_continuation_successor(
        mut expected: ThreadSnapshot,
        committed: &ThreadSnapshot,
        source_thread_id: &str,
        timestamp_floor: &str,
    ) -> anyhow::Result<ThreadSnapshot> {
        expected
            .last_event_hash
            .clone_from(&committed.last_event_hash);
        expected.last_chain_seq = committed.last_chain_seq;
        expected.last_thread_seq = committed.last_thread_seq;
        chain::normalize_prospective_new_thread(
            &mut expected,
            &committed.chain_root_id,
            timestamp_floor,
            Some(source_thread_id),
        )?;
        Ok(expected)
    }

    /// Reproduce the state-layer stamping and lifecycle timestamp
    /// normalization applied to an existing source snapshot in a continuation
    /// commit.
    pub fn normalize_expected_continuation_source(
        previous: &ThreadSnapshot,
        mut expected: ThreadSnapshot,
        committed: &ThreadSnapshot,
        append_timestamp: &str,
    ) -> anyhow::Result<ThreadSnapshot> {
        expected
            .last_event_hash
            .clone_from(&committed.last_event_hash);
        expected.last_chain_seq = committed.last_chain_seq;
        expected.last_thread_seq = committed.last_thread_seq;
        chain::normalize_prospective_snapshot_transition(
            previous,
            &mut expected,
            append_timestamp,
        )?;
        Ok(expected)
    }

    /// Trust-verify and load the current authoritative chain closure used for a
    /// destructive retention decision. `None` means the head is absent or at
    /// least one member is nonterminal; malformed/mismatched CAS fails closed.
    pub fn authoritative_terminal_chain(
        &self,
        chain_root_id: &str,
    ) -> anyhow::Result<Option<AuthoritativeTerminalChain>> {
        let chain_lock = self.acquire_chain_lock(chain_root_id)?;
        self.authoritative_terminal_chain_under_lock(chain_root_id, &chain_lock)
    }

    pub fn authoritative_terminal_chain_under_lock(
        &self,
        chain_root_id: &str,
        chain_lock: &crate::chain::ChainLock,
    ) -> anyhow::Result<Option<AuthoritativeTerminalChain>> {
        ensure_state_chain_lock(&self.refs_root, chain_lock, chain_root_id)?;
        let head = match chain_lock.read_verified_head(self.trust_store.as_ref())? {
            Some(head) => head,
            None => return Ok(None),
        };
        let cas = lillux::CasStore::from_pinned_root(self._cas_directory.try_clone()?);
        load_terminal_chain_from_cas(&cas, chain_root_id, &head.target_hash)
    }

    /// Recover authoritative cleanup IDs from the CAS hash retained by a
    /// pending Remove after its signed head has already been durably unlinked.
    pub fn pending_removed_terminal_chain(
        &self,
        chain_root_id: &str,
    ) -> anyhow::Result<Option<AuthoritativeTerminalChain>> {
        let chain_lock = self.acquire_chain_lock(chain_root_id)?;
        self.pending_removed_terminal_chain_under_lock(chain_root_id, &chain_lock)
    }

    pub fn pending_removed_terminal_chain_under_lock(
        &self,
        chain_root_id: &str,
        chain_lock: &crate::chain::ChainLock,
    ) -> anyhow::Result<Option<AuthoritativeTerminalChain>> {
        ensure_state_chain_lock(&self.refs_root, chain_lock, chain_root_id)?;
        let pending = match self.recovery.read_pending(chain_root_id)? {
            Some(pending) if pending.operation == HeadOperation::Remove => pending,
            Some(_) => anyhow::bail!("chain {chain_root_id} pending transition is not Remove"),
            None => return Ok(None),
        };
        if chain_lock
            .read_verified_head(self.trust_store.as_ref())?
            .is_some()
        {
            return Ok(None);
        }
        let expected_hash = pending.expected_previous_hash.as_deref().ok_or_else(|| {
            anyhow::anyhow!("pending Remove for {chain_root_id} has no expected head hash")
        })?;
        let cas = lillux::CasStore::from_pinned_root(self._cas_directory.try_clone()?);
        load_terminal_chain_from_cas(&cas, chain_root_id, expected_hash)
    }

    /// Classify one still-current pending Remove against the exact trusted
    /// signed head while the caller retains the chain lock. This is the
    /// three-way boundary used by app-owned runtime cleanup: absent means the
    /// unlink committed, expected means policy/pins must be re-evaluated, and
    /// advanced means the Remove is stale and must never touch the new head.
    pub fn pending_chain_removal_head_state(
        &self,
        chain_root_id: &str,
        transition_id: &str,
        chain_lock: &crate::chain::ChainLock,
    ) -> anyhow::Result<PendingRemoveHeadState> {
        ensure_state_chain_lock(&self.refs_root, chain_lock, chain_root_id)?;
        let pending = self
            .recovery
            .read_pending(chain_root_id)?
            .ok_or_else(|| anyhow::anyhow!("pending Remove disappeared"))?;
        if pending.transition_id != transition_id {
            anyhow::bail!("pending Remove transition ID changed before head classification");
        }
        if pending.operation != HeadOperation::Remove {
            anyhow::bail!("pending transition is no longer a Remove");
        }
        let head = chain_lock.read_verified_head(self.trust_store.as_ref())?;
        classify_pending_remove_head(&pending, head.as_ref())
    }

    /// Read-only proof for a dry-run that an observed stale Remove is followed
    /// by the exact trusted, structurally valid authoritative history it would
    /// repair. The journal, projection, and signed head are not mutated.
    pub fn verify_advanced_head_after_stale_chain_removal(
        &self,
        chain_root_id: &str,
        remove_transition_id: &str,
        observed_head_hash: &str,
        chain_lock: &crate::chain::ChainLock,
    ) -> anyhow::Result<()> {
        ensure_state_chain_lock(&self.refs_root, chain_lock, chain_root_id)?;
        let cas = self.pinned_cas()?;
        let pending = self
            .recovery
            .read_pending(chain_root_id)?
            .ok_or_else(|| anyhow::anyhow!("pending Remove disappeared"))?;
        if pending.transition_id != remove_transition_id {
            anyhow::bail!("pending Remove transition ID changed before dry-run verification");
        }
        if pending.operation != HeadOperation::Remove {
            anyhow::bail!("pending transition is no longer a Remove");
        }
        let head = chain_lock
            .read_verified_head(self.trust_store.as_ref())?
            .ok_or_else(|| {
                anyhow::anyhow!("advanced signed head disappeared before verification")
            })?;
        if head.target_hash != observed_head_hash {
            anyhow::bail!(
                "advanced signed head changed before dry-run verification: observed {observed_head_hash}, now {}",
                head.target_hash
            );
        }
        if !matches!(
            classify_pending_remove_head(&pending, Some(&head))?,
            PendingRemoveHeadState::AdvancedHeadVisible { .. }
        ) {
            anyhow::bail!("pending Remove is not stale against an advanced head");
        }
        crate::rebuild::verify_repair_closure_anchored_with_cas(
            &cas,
            chain_root_id,
            &head.target_hash,
            true,
            None,
        )?;
        Ok(())
    }

    /// Convert a stale Remove into a durable repair Set for the already-visible
    /// trusted head, then project and compare-acknowledge it under the same
    /// chain lock. If projection fails, the Set remains durable for the normal
    /// bounded replay path; the advanced head is never removed.
    pub fn repair_advanced_head_after_stale_chain_removal(
        &self,
        chain_root_id: &str,
        remove_transition_id: &str,
        chain_lock: &crate::chain::ChainLock,
    ) -> anyhow::Result<crate::rebuild::ChainRepairReport> {
        ensure_state_chain_lock(&self.refs_root, chain_lock, chain_root_id)?;
        let cas = self.pinned_cas()?;
        let pending = self
            .recovery
            .read_pending(chain_root_id)?
            .ok_or_else(|| anyhow::anyhow!("pending Remove disappeared"))?;
        if pending.transition_id != remove_transition_id {
            anyhow::bail!("pending Remove transition ID changed before repair");
        }
        if pending.operation != HeadOperation::Remove {
            anyhow::bail!("pending transition is no longer a Remove");
        }
        let head = chain_lock
            .read_verified_head(self.trust_store.as_ref())?
            .ok_or_else(|| anyhow::anyhow!("advanced signed head disappeared before repair"))?;
        if !matches!(
            classify_pending_remove_head(&pending, Some(&head))?,
            PendingRemoveHeadState::AdvancedHeadVisible { .. }
        ) {
            anyhow::bail!("pending Remove is not stale against an advanced head");
        }

        crate::rebuild::verify_repair_closure_anchored_with_cas(
            &cas,
            chain_root_id,
            &head.target_hash,
            true,
            None,
        )?;
        let replacement = self.recovery.replace_remove_with_published_set(
            chain_lock,
            chain_root_id,
            remove_transition_id,
            &head.target_hash,
        )?;
        let replacement = advance_set_to_applying(&self.recovery, chain_lock, replacement)?;
        let report = match crate::rebuild::repair_one_chain_verified_with_cas_and_observer(
            &self.projection,
            &cas,
            chain_root_id,
            &head.target_hash,
            None,
        ) {
            Ok(report) => report,
            Err(error) => {
                self.note_pending_transition_error(
                    chain_root_id,
                    "repair_stale_remove_advanced_head",
                    &error,
                );
                return Err(error);
            }
        };
        if !acknowledge_if_projection_current(
            &self.projection,
            &self.recovery,
            chain_lock,
            &replacement,
            Some(&head),
        )? {
            anyhow::bail!("advanced-head repair Set did not converge for {chain_root_id}");
        }
        Ok(report)
    }

    /// Durably prepare and remove a chain head. The caller must hold the daemon
    /// write permit, StateStore mutex, and chain lock in that order, and must
    /// have completed its authoritative terminality/runtime-pin recheck. The
    /// supplied lock is validated against `chain_root_id` before any mutation.
    pub fn remove_chain_head_ref(
        &self,
        chain_root_id: &str,
        chain_lock: &crate::chain::ChainLock,
    ) -> anyhow::Result<bool> {
        ensure_state_chain_lock(&self.refs_root, chain_lock, chain_root_id)?;
        let current = chain_lock.read_verified_head(self.trust_store.as_ref())?;
        let current = match current {
            Some(current) => current,
            None => return Ok(false),
        };

        if let Some(pending) = self.recovery.read_pending(chain_root_id)? {
            match pending.operation {
                HeadOperation::Set => {
                    if !acknowledge_if_projection_current(
                        &self.projection,
                        &self.recovery,
                        chain_lock,
                        &pending,
                        Some(&current),
                    )? {
                        anyhow::bail!(
                            "cannot remove chain {chain_root_id} while its Set is not projected"
                        );
                    }
                }
                HeadOperation::Remove => {}
            }
        }
        let pending =
            self.recovery
                .prepare_remove(chain_lock, chain_root_id, &current.target_hash)?;
        self.projection_repair_sink
            .request_repair(ProjectionRepairRequest {
                chain_root_id: chain_root_id.to_string(),
                committed_head_hash: current.target_hash.clone(),
                operation: "remove_chain_head",
                error: "durable Remove remains pending until runtime and projection cleanup"
                    .to_string(),
            });
        let removed = chain_lock.remove_head()?;
        self.head_cache
            .lock()
            .expect("head_cache lock")
            .invalidate(chain_root_id);
        if removed && pending.phase == TransitionPhase::Prepared {
            self.recovery.advance_phase(
                chain_lock,
                chain_root_id,
                &pending.transition_id,
                TransitionPhase::Prepared,
                TransitionPhase::HeadPublished,
            )?;
        }
        Ok(removed)
    }

    /// Acknowledge a Remove after StateStore has idempotently deleted runtime
    /// DB rows/files and this projection's chain rows. The same chain lock must
    /// cover the complete head/runtime/projection/acknowledgement transaction.
    pub fn acknowledge_chain_removal_cleanup(
        &self,
        chain_root_id: &str,
        chain_lock: &crate::chain::ChainLock,
    ) -> anyhow::Result<bool> {
        ensure_state_chain_lock(&self.refs_root, chain_lock, chain_root_id)?;
        if chain_lock
            .read_verified_head(self.trust_store.as_ref())?
            .is_some()
        {
            anyhow::bail!("cannot acknowledge removal while chain head still exists");
        }
        if self
            .projection
            .get_projection_meta(chain_root_id)?
            .is_some()
        {
            anyhow::bail!("cannot acknowledge removal while projection cursor still exists");
        }
        let pending = self
            .recovery
            .read_pending(chain_root_id)?
            .ok_or_else(|| anyhow::anyhow!("chain {chain_root_id} has no pending removal"))?;
        if pending.operation != HeadOperation::Remove {
            anyhow::bail!("chain {chain_root_id} pending transition is not Remove");
        }
        if pending.phase != TransitionPhase::ApplyingProjection {
            anyhow::bail!(
                "chain {chain_root_id} removal cleanup is not in applying_projection phase"
            );
        }
        self.recovery
            .acknowledge(chain_lock, chain_root_id, &pending.transition_id)
    }

    /// Cancel a prepared Remove which has not unlinked its expected head while
    /// holding the lock for that exact chain.
    pub fn cancel_pending_chain_removal(
        &self,
        chain_root_id: &str,
        chain_lock: &crate::chain::ChainLock,
    ) -> anyhow::Result<bool> {
        ensure_state_chain_lock(&self.refs_root, chain_lock, chain_root_id)?;
        let pending = match self.recovery.read_pending(chain_root_id)? {
            Some(pending) if pending.operation == HeadOperation::Remove => pending,
            Some(_) => anyhow::bail!("chain {chain_root_id} pending transition is not Remove"),
            None => return Ok(false),
        };
        anyhow::ensure!(
            self.recovery.retired_chain_fence(chain_root_id)?
                == crate::recovery::RetiredChainFence::Absent,
            "cannot cancel the removal of fenced chain {chain_root_id}; its retirement only moves forward"
        );
        let head = chain_lock
            .read_verified_head(self.trust_store.as_ref())?
            .ok_or_else(|| anyhow::anyhow!("cannot cancel removal after head unlink"))?;
        if pending.expected_previous_hash.as_deref() != Some(head.target_hash.as_str()) {
            anyhow::bail!("cannot cancel removal after chain head advanced");
        }
        self.recovery
            .acknowledge(chain_lock, chain_root_id, &pending.transition_id)
    }

    /// Apply the projection half of a prepared, head-published Remove while
    /// holding the lock that protects the exact chain.
    pub fn delete_chain_projection(
        &self,
        chain_root_id: &str,
        chain_lock: &crate::chain::ChainLock,
    ) -> anyhow::Result<usize> {
        ensure_state_chain_lock(&self.refs_root, chain_lock, chain_root_id)?;
        let mut pending = self
            .recovery
            .read_pending(chain_root_id)?
            .ok_or_else(|| anyhow::anyhow!("chain {chain_root_id} has no pending removal"))?;
        if pending.operation != HeadOperation::Remove {
            anyhow::bail!("chain {chain_root_id} pending transition is not Remove");
        }
        if chain_lock
            .read_verified_head(self.trust_store.as_ref())?
            .is_some()
        {
            anyhow::bail!("cannot apply Remove cleanup before durable head removal");
        }
        if pending.phase == TransitionPhase::Prepared {
            pending = self.recovery.advance_phase(
                chain_lock,
                chain_root_id,
                &pending.transition_id,
                TransitionPhase::Prepared,
                TransitionPhase::HeadPublished,
            )?;
        }
        if pending.phase == TransitionPhase::HeadPublished {
            pending = self.recovery.advance_phase(
                chain_lock,
                chain_root_id,
                &pending.transition_id,
                TransitionPhase::HeadPublished,
                TransitionPhase::ApplyingProjection,
            )?;
        }
        debug_assert_eq!(pending.phase, TransitionPhase::ApplyingProjection);
        self.projection.delete_chain_projection(chain_root_id)
    }

    /// Advance a namespace-neutral signed head with compare-and-swap semantics.
    pub fn advance_generic_head_ref(
        &self,
        namespace: &str,
        name: &str,
        new_target_hash: &str,
        expected_current_hash: Option<&str>,
        signer: &dyn Signer,
        cas_mutation_guard: &crate::recovery::CasMutationGuard,
    ) -> anyhow::Result<()> {
        self.ensure_cas_mutation_guard(cas_mutation_guard)?;
        if namespace == "chains" {
            anyhow::bail!("chain-head publication requires an admitted CAS mutation guard");
        }
        crate::signer::ensure_signer_trusted(signer, self.trust_store.as_ref())?;
        let _head_lock = crate::refs::GenericHeadLock::acquire_in_refs_directory(
            &self._refs_directory,
            namespace,
            name,
        )?;
        crate::refs::advance_verified_generic_head_ref_in_directory(
            &self._refs_directory,
            namespace,
            name,
            new_target_hash,
            expected_current_hash,
            signer,
            self.trust_store.as_ref(),
            &_head_lock,
        )
    }

    /// Journal and project an already-present CAS closure (for sync/import)
    /// while retaining the chain lock across convergence, signed-head
    /// publication, projection, and compare-acknowledgement.
    fn publish_external_chain_head(
        &self,
        chain_root_id: &str,
        target_hash: &str,
        signer: &dyn Signer,
        cas_mutation_guard: &crate::recovery::CasMutationGuard,
        writer_transition: Option<&crate::sync::AdmittedChainWriterTransition>,
        limits: crate::object_closure::ObjectClosureLimits,
    ) -> anyhow::Result<()> {
        crate::signer::ensure_signer_trusted(signer, self.trust_store.as_ref())?;
        cas_mutation_guard.ensure_protects_pinned_runtime(&self._runtime_state_directory)?;
        let cas = self.pinned_cas()?;
        let chain_lock = crate::chain::ChainLock::acquire_in_refs_directory(
            &self._refs_directory,
            &self._cas_directory,
            &self.recovery,
            chain_root_id,
        )?;
        let current = chain_lock.read_verified_head(self.trust_store.as_ref())?;
        let current_hash = current.as_ref().map(|head| head.target_hash.as_str());
        let expected_anchor = writer_transition
            .map(|transition| transition.evidence.source_chain_head_hash.as_str())
            .or(current_hash);
        let mut evidence_roots = Vec::new();
        if let Some(transition) = writer_transition {
            evidence_roots.push(transition.writer_grant_hash.clone());
            evidence_roots.push(transition.evidence.placement_attestation_hash.clone());
            evidence_roots.extend(
                transition
                    .evidence
                    .source_accounting_transfer_hash
                    .iter()
                    .cloned(),
            );
        }
        let (_, mut validation_budget) =
            crate::rebuild::verify_repair_closure_anchored_with_cas_and_limits_and_budget(
                &cas,
                chain_root_id,
                target_hash,
                expected_anchor,
                limits,
                &evidence_roots,
            )?;
        let required_anchor = if let Some(transition) = writer_transition {
            verify_chain_writer_transition_adoption(
                &cas,
                transition,
                signer.fingerprint(),
                limits,
                &mut validation_budget,
            )?;
            if chain_root_id != transition.evidence.chain_root_id
                || target_hash != transition.target_chain_head_hash
            {
                anyhow::bail!("chain writer transition changed its publication coordinate");
            }
            match current.as_ref() {
                None => {}
                Some(head) if head.target_hash == transition.evidence.source_chain_head_hash => {}
                Some(head)
                    if head.target_hash == transition.target_chain_head_hash
                        && head.signer == transition.evidence.target_node_signer_fingerprint => {}
                Some(head) => {
                    // A returning placement may have a locally signed mirror
                    // from an earlier visit. It may advance only if that exact
                    // local head is an ancestor of the newly granted source
                    // frontier; the scoped grant still authorizes only the
                    // immediate source->target successor.
                    crate::rebuild::verify_repair_closure_anchored_with_existing_budget(
                        &cas,
                        chain_root_id,
                        &transition.evidence.source_chain_head_hash,
                        Some(&head.target_hash),
                        limits,
                        &mut validation_budget,
                    )?;
                }
            }
            Some(transition.evidence.source_chain_head_hash.as_str())
        } else {
            current_hash
        };
        // The exact trusted current target is the import anchor. An equal
        // target is idempotent; a different target must prove that current is
        // an ancestor, so import can advance but never fork or roll back a
        // locally published chain.
        if required_anchor != expected_anchor {
            crate::rebuild::verify_repair_closure_anchored_with_existing_budget(
                &cas,
                chain_root_id,
                target_hash,
                required_anchor,
                limits,
                &mut validation_budget,
            )?;
        }
        let invalidates_retirement = self.pending_remove_invalidated_by_mutation_locked(
            chain_root_id,
            ProspectiveChainMutation::ReplaceHead(target_hash),
            &chain_lock,
        )?;
        let cancelled_remove = self.converge_pending_before_set_locked(
            chain_root_id,
            invalidates_retirement,
            None,
            &chain_lock,
        )?;
        if current_hash == Some(target_hash) {
            if let Err(error) =
                crate::rebuild::repair_one_chain_verified_with_cas_and_limits_and_budget(
                    &self.projection,
                    &cas,
                    chain_root_id,
                    target_hash,
                    None,
                    limits,
                    &mut validation_budget,
                )
            {
                return Err(self.failed_write_after_cancelled_remove(
                    chain_root_id,
                    cancelled_remove.as_ref(),
                    &chain_lock,
                    error,
                ));
            }
            return Ok(());
        }
        let pending =
            match self
                .recovery
                .prepare_set(&chain_lock, chain_root_id, current_hash, target_hash)
            {
                Ok(pending) => pending,
                Err(error) => {
                    let error = self.failed_write_after_cancelled_remove(
                        chain_root_id,
                        cancelled_remove.as_ref(),
                        &chain_lock,
                        error,
                    );
                    self.note_pending_transition_error(
                        chain_root_id,
                        "publish_external_chain_head",
                        &error,
                    );
                    return Err(error);
                }
            };
        let signed_ref = SignedRef::new(
            format!("chains/{chain_root_id}/head"),
            target_hash.to_string(),
            lillux::time::iso8601_now(),
            signer.fingerprint().to_string(),
        );
        if let Err(error) = chain_lock.write_signed_head(signed_ref, signer) {
            let error = self.failed_write_after_cancelled_remove(
                chain_root_id,
                cancelled_remove.as_ref(),
                &chain_lock,
                error,
            );
            self.note_pending_transition_error(
                chain_root_id,
                "publish_external_chain_head",
                &error,
            );
            return Err(error);
        }
        if pending.phase == TransitionPhase::Prepared
            && let Err(error) = self.recovery.advance_phase(
                &chain_lock,
                chain_root_id,
                &pending.transition_id,
                TransitionPhase::Prepared,
                TransitionPhase::HeadPublished,
            )
        {
            self.note_pending_transition_error(
                chain_root_id,
                "publish_external_chain_head",
                &error,
            );
            return Err(error);
        }
        self.head_cache
            .lock()
            .expect("head_cache lock")
            .invalidate(chain_root_id);
        if let Err(error) =
            self.mark_pending_set_applying_locked(chain_root_id, target_hash, &chain_lock)
        {
            self.note_pending_transition_error(
                chain_root_id,
                "publish_external_chain_head",
                &error,
            );
            return Err(error);
        }
        let projected = crate::rebuild::repair_one_chain_verified_with_cas_and_limits_and_budget(
            &self.projection,
            &cas,
            chain_root_id,
            target_hash,
            None,
            limits,
            &mut validation_budget,
        )
        .and_then(|_| self.acknowledge_projected_set_locked(chain_root_id, &chain_lock));
        match projected {
            Ok(()) => Ok(()),
            Err(error) => {
                self.projection_repair_sink
                    .request_repair(ProjectionRepairRequest {
                        chain_root_id: chain_root_id.to_string(),
                        committed_head_hash: target_hash.to_string(),
                        operation: "publish_external_chain_head",
                        error: format!("{error:#}"),
                    });
                Err(error)
            }
        }
    }

    /// List namespace-neutral signed heads beneath `refs/generic/<prefix>`.
    pub fn list_generic_head_refs(&self, prefix: &str) -> anyhow::Result<Vec<GenericHeadRef>> {
        crate::refs::list_verified_generic_head_refs_in_directory(
            &self._refs_directory,
            prefix,
            self.trust_store.as_ref(),
        )
    }

    /// Explicit clean-cut retirement of every external-content binding head.
    ///
    /// This is intentionally not a generic namespace-deletion surface. A
    /// manifest schema cut changes every binding coordinate, so the stopped
    /// node must retire predecessor heads before v2 content is imported and
    /// rebound. The operation is idempotent after a crash: each head is
    /// locked, re-read, and unlinked through its pinned directory authority.
    pub fn discard_external_content_binding_heads(
        &self,
        guard: &crate::recovery::CasMutationGuard,
        dry_run: bool,
    ) -> anyhow::Result<usize> {
        guard.ensure_protects_pinned_runtime(&self._runtime_state_directory)?;
        if !guard.is_exclusive() {
            anyhow::bail!("external-content binding reset requires exclusive state authority");
        }
        let namespace = crate::objects::EXTERNAL_CONTENT_BINDING_HEAD_NAMESPACE;
        let heads = self.list_generic_head_refs(namespace)?;
        if dry_run {
            return Ok(heads.len());
        }
        let mut removed = 0usize;
        for expected in heads {
            if expected.namespace != namespace {
                anyhow::bail!("external-content binding enumeration escaped its namespace");
            }
            let head_lock = crate::refs::GenericHeadLock::acquire_in_refs_directory(
                &self._refs_directory,
                namespace,
                &expected.name,
            )?;
            let current = crate::refs::read_verified_generic_head_ref_in_directory(
                &self._refs_directory,
                namespace,
                &expected.name,
                self.trust_store.as_ref(),
            )?;
            let Some(current) = current else {
                continue;
            };
            if current.target_hash != expected.target_hash {
                anyhow::bail!("external-content binding head changed during stopped-node reset");
            }
            if crate::refs::remove_generic_head_ref_in_directory(
                &self._refs_directory,
                namespace,
                &expected.name,
                &head_lock,
            )? {
                removed = removed.saturating_add(1);
            }
        }
        self.activate_external_content_binding_epoch_after_reset(guard)?;
        Ok(removed)
    }

    /// Read the clean-cut epoch for external-content binding heads. Absence is
    /// allowed only for a fresh namespace with no heads; callers decide that
    /// distinction explicitly.
    pub fn external_content_binding_schema_epoch(&self) -> anyhow::Result<Option<u32>> {
        let name = std::ffi::OsStr::new("external-content-bindings.epoch");
        let Some(file) = self._runtime_state_directory.open_regular(name, false)? else {
            return Ok(None);
        };
        use std::io::Read as _;
        let mut bytes = Vec::new();
        file.take(32).read_to_end(&mut bytes)?;
        let text =
            std::str::from_utf8(&bytes).context("external-content binding epoch is not UTF-8")?;
        let epoch = text
            .strip_suffix('\n')
            .unwrap_or(text)
            .parse::<u32>()
            .context("external-content binding epoch is malformed")?;
        if epoch == 0 {
            anyhow::bail!("external-content binding epoch cannot be zero");
        }
        Ok(Some(epoch))
    }

    /// Initialize a fresh binding namespace, or prove an existing namespace
    /// is current. Ordinary bind paths may never advance a predecessor epoch.
    pub fn ensure_current_external_content_binding_epoch(
        &self,
        guard: &crate::recovery::CasMutationGuard,
    ) -> anyhow::Result<()> {
        self.write_external_content_binding_epoch(guard, false)
    }

    fn activate_external_content_binding_epoch_after_reset(
        &self,
        guard: &crate::recovery::CasMutationGuard,
    ) -> anyhow::Result<()> {
        self.write_external_content_binding_epoch(guard, true)
    }

    fn write_external_content_binding_epoch(
        &self,
        guard: &crate::recovery::CasMutationGuard,
        explicit_reset: bool,
    ) -> anyhow::Result<()> {
        guard.ensure_protects_pinned_runtime(&self._runtime_state_directory)?;
        let current = crate::objects::EXTERNAL_CONTENT_BINDING_SCHEMA_EPOCH;
        let name = std::ffi::OsStr::new("external-content-bindings.epoch");
        let expected = self._runtime_state_directory.open_regular(name, false)?;
        if let Some(stored) = self.external_content_binding_schema_epoch()? {
            if stored == current {
                return Ok(());
            }
            if !explicit_reset {
                anyhow::bail!(
                    "external-content binding epoch is {stored}, expected {current}; explicit reset is required"
                );
            }
            if stored > current {
                anyhow::bail!(
                    "external-content binding epoch {stored} is newer than this binary's epoch {current}"
                );
            }
        }
        if !explicit_reset
            && !self
                .list_generic_head_refs(crate::objects::EXTERNAL_CONTENT_BINDING_HEAD_NAMESPACE)?
                .is_empty()
        {
            anyhow::bail!(
                "external-content binding epoch is absent while binding heads exist; explicit reset is required"
            );
        }
        if !guard.is_exclusive() {
            anyhow::bail!(
                "external-content binding epoch publication requires exclusive state authority"
            );
        }
        self._runtime_state_directory.atomic_write_if_same(
            name,
            expected.as_ref(),
            format!("{current}\n").as_bytes(),
            0o600,
            crate::STATE_TEMP_PURPOSE,
        )?;
        if self.external_content_binding_schema_epoch()? != Some(current) {
            anyhow::bail!("external-content binding epoch publication did not verify");
        }
        Ok(())
    }

    // ── Bundle event chains ────────────────────────────────

    pub fn append_bundle_event_admitted(
        &self,
        request: BundleEventAppendRequest,
        signer: &dyn Signer,
        cas_guard: &crate::recovery::CasMutationGuard,
    ) -> anyhow::Result<BundleEventAppendResult> {
        crate::signer::ensure_signer_trusted(signer, self.trust_store.as_ref())?;
        cas_guard.ensure_protects_pinned_runtime(&self._runtime_state_directory)?;
        let cas = self.pinned_cas()?;
        bundle_events::append_bundle_event_pinned(
            &cas,
            &self._refs_directory,
            request,
            signer,
            self.trust_store.as_ref(),
        )
    }

    pub fn read_bundle_event_by_hash(&self, event_hash: &str) -> anyhow::Result<BundleEventRecord> {
        let cas = self.pinned_cas()?;
        bundle_events::read_bundle_event_by_hash_with_cas(&cas, event_hash)
    }

    pub fn read_bundle_event_chain(
        &self,
        bundle_id: &str,
        event_kind: &str,
        chain_id: &str,
    ) -> anyhow::Result<Vec<BundleEventRecord>> {
        let cas = self.pinned_cas()?;
        bundle_events::read_bundle_event_chain_pinned(
            &cas,
            &self._refs_directory,
            bundle_id,
            event_kind,
            chain_id,
            self.trust_store.as_ref(),
        )
    }

    pub fn scan_bundle_events(
        &self,
        bundle_id: &str,
        event_kind: &str,
    ) -> anyhow::Result<Vec<BundleEventRecord>> {
        let cas = self.pinned_cas()?;
        bundle_events::scan_bundle_events_pinned(
            &cas,
            &self._refs_directory,
            bundle_id,
            event_kind,
            self.trust_store.as_ref(),
        )
    }

    // Paging bounds and signing/trust authority remain explicit at the public
    // state boundary rather than being hidden in an unchecked options bag.
    #[allow(clippy::too_many_arguments)]
    pub fn read_bundle_event_chain_page(
        &self,
        bundle_id: &str,
        event_kind: &str,
        chain_id: &str,
        cursor: Option<&BundleEventCursor>,
        limit: usize,
        max_serialized_bytes: usize,
        signer: &dyn Signer,
    ) -> anyhow::Result<BundleEventChainPage> {
        let cas = self.pinned_cas()?;
        bundle_events::read_bundle_event_chain_page(
            &cas,
            &self._refs_directory,
            self.trust_store.as_ref(),
            bundle_id,
            event_kind,
            chain_id,
            cursor,
            limit,
            max_serialized_bytes,
            signer,
        )
    }

    pub fn scan_bundle_events_page(
        &self,
        bundle_id: &str,
        event_kind: &str,
        cursor: Option<&BundleEventCursor>,
        limit: usize,
        max_serialized_bytes: usize,
        signer: &dyn Signer,
    ) -> anyhow::Result<BundleEventScanPage> {
        let cas = self.pinned_cas()?;
        bundle_events::scan_bundle_events_page(
            &cas,
            &self._refs_directory,
            self.trust_store.as_ref(),
            bundle_id,
            event_kind,
            cursor,
            limit,
            max_serialized_bytes,
            signer,
        )
    }

    // ── Query helpers ──────────────────────────────────────────────

    pub fn get_thread(&self, thread_id: &str) -> anyhow::Result<Option<queries::ThreadRow>> {
        queries::get_thread(&self.projection, thread_id)
    }

    pub fn latest_state_anchor_event(
        &self,
        thread_id: &str,
    ) -> anyhow::Result<Option<queries::EventRow>> {
        queries::latest_state_anchor_event(&self.projection, thread_id)
    }

    pub fn latest_chain_state_anchor_event(
        &self,
        chain_root_id: &str,
    ) -> anyhow::Result<Option<queries::EventRow>> {
        queries::latest_chain_state_anchor_event(&self.projection, chain_root_id)
    }

    pub fn list_threads_by_chain(
        &self,
        chain_root_id: &str,
    ) -> anyhow::Result<Vec<queries::ThreadRow>> {
        queries::list_threads_by_chain(&self.projection, chain_root_id)
    }

    pub fn record_cas_entry(&self, entry: &NewCasEntryAttribution) -> anyhow::Result<()> {
        self.operational()?.record_cas_entry(entry)
    }

    pub fn set_cas_entry_state(
        &self,
        entry_kind: CasEntryKind,
        hash: &str,
        state: CasEntryState,
    ) -> anyhow::Result<()> {
        self.operational()?
            .set_cas_entry_state(entry_kind, hash, state)
    }

    pub fn get_cas_entry(
        &self,
        entry_kind: CasEntryKind,
        hash: &str,
    ) -> anyhow::Result<Option<CasEntryAttribution>> {
        self.operational()?.get_cas_entry(entry_kind, hash)
    }

    pub fn list_cas_entries_by_state(
        &self,
        state: CasEntryState,
    ) -> anyhow::Result<Vec<CasEntryAttribution>> {
        self.operational()?.list_cas_entries_by_state(state)
    }

    pub fn cas_entries_by_state_summary(&self) -> anyhow::Result<Vec<CasEntriesByStateSummary>> {
        self.operational()?.cas_entries_by_state_summary()
    }

    pub fn lookup_replay_record(
        &self,
        namespace: &crate::ReplayIndexNamespace,
        cache_key: &str,
        verify: impl FnMut(&crate::ReplayIndexRecord) -> crate::ReplayRecordVerification,
    ) -> anyhow::Result<crate::ReplayLookupOutcome> {
        self.operational()?
            .lookup_replay_record(namespace, cache_key, verify)
    }

    pub fn lookup_replay_index(
        &self,
        namespace: &crate::ReplayIndexNamespace,
        cache_key: &str,
    ) -> anyhow::Result<crate::ReplayLookupOutcome> {
        self.operational()?
            .lookup_replay_index(namespace, cache_key)
    }

    pub fn touch_replay_record_if_same(
        &self,
        namespace: &crate::ReplayIndexNamespace,
        indexed: &crate::ReplayIndexRecord,
    ) -> anyhow::Result<bool> {
        self.operational()?
            .touch_replay_record_if_same(namespace, indexed)
    }

    pub fn publish_replay_record(
        &self,
        namespace: &crate::ReplayIndexNamespace,
        candidate: &crate::ReplayIndexRecord,
        verify: impl FnMut(&crate::ReplayIndexRecord) -> crate::ReplayRecordVerification,
    ) -> anyhow::Result<crate::ReplayPublishOutcome> {
        self.operational()?
            .publish_replay_record(namespace, candidate, verify)
    }

    pub fn list_replay_record_hashes(&self) -> anyhow::Result<Vec<String>> {
        self.operational()?.list_replay_record_hashes()
    }

    pub fn delete_replay_records(
        &self,
        namespace: &crate::ReplayIndexNamespace,
        cache_keys: &[String],
    ) -> anyhow::Result<usize> {
        self.operational()?
            .delete_replay_records(namespace, cache_keys)
    }

    pub fn prune_replay_records(
        &self,
        namespace: &crate::ReplayIndexNamespace,
        max_rows: usize,
    ) -> anyhow::Result<usize> {
        self.operational()?
            .prune_replay_records(namespace, max_rows)
    }

    pub fn record_admission_attestation(
        &self,
        record: &NewAdmissionAttestationRecord,
    ) -> anyhow::Result<()> {
        self.operational()?.record_admission_attestation(record)
    }

    pub fn list_admission_attestations_for_subject(
        &self,
        subject_hash: &str,
        policy: Option<&str>,
    ) -> anyhow::Result<Vec<AdmissionAttestationRecord>> {
        self.operational()?
            .list_admission_attestations_for_subject(subject_hash, policy)
    }

    pub fn create_sync_job(&self, job: &NewSyncJob) -> anyhow::Result<SyncJobRecord> {
        self.operational()?.create_sync_job(job)
    }

    pub fn create_sync_job_with_initial_progress(
        &self,
        job: &NewSyncJob,
        state: SyncJobState,
        phase: &str,
        result: Option<&serde_json::Value>,
    ) -> anyhow::Result<SyncJobRecord> {
        self.operational()?
            .create_sync_job_with_initial_progress(job, state, phase, result)
    }

    pub fn update_sync_job(&self, job_id: &str, update: &SyncJobUpdate) -> anyhow::Result<()> {
        self.operational()?.update_sync_job(job_id, update)
    }

    pub fn complete_exhausted_sync_job_from_authority(
        &self,
        job_id: &str,
        expected_operation: &serde_json::Value,
        update: &SyncJobUpdate,
    ) -> anyhow::Result<()> {
        self.operational()?
            .complete_exhausted_sync_job_from_authority(job_id, expected_operation, update)
    }

    pub fn reconcile_interrupted_sync_job_attempts(&self) -> anyhow::Result<usize> {
        self.operational()?
            .reconcile_interrupted_sync_job_attempts()
    }

    pub fn create_sync_job_attempt(
        &self,
        attempt: &NewSyncJobAttempt,
    ) -> anyhow::Result<SyncJobAttemptRecord> {
        self.operational()?.create_sync_job_attempt(attempt)
    }

    pub fn finish_sync_job_attempt(
        &self,
        attempt_id: &str,
        finish: &FinishSyncJobAttempt,
    ) -> anyhow::Result<()> {
        self.operational()?
            .finish_sync_job_attempt(attempt_id, finish)
    }

    pub fn finish_sync_job_attempt_and_update_job(
        &self,
        attempt_id: &str,
        finish: &FinishSyncJobAttempt,
        job_id: &str,
        update: &SyncJobUpdate,
    ) -> anyhow::Result<()> {
        self.operational()?
            .finish_sync_job_attempt_and_update_job(attempt_id, finish, job_id, update)
    }

    pub fn get_sync_job_attempt(
        &self,
        attempt_id: &str,
    ) -> anyhow::Result<Option<SyncJobAttemptRecord>> {
        self.operational()?.get_sync_job_attempt(attempt_id)
    }

    pub fn list_sync_job_attempts(
        &self,
        job_id: &str,
    ) -> anyhow::Result<Vec<SyncJobAttemptRecord>> {
        self.operational()?.list_sync_job_attempts(job_id)
    }

    pub fn get_sync_job(&self, job_id: &str) -> anyhow::Result<Option<SyncJobRecord>> {
        self.operational()?.get_sync_job(job_id)
    }

    pub fn list_sync_jobs_by_state(
        &self,
        state: Option<SyncJobState>,
        limit: usize,
    ) -> anyhow::Result<Vec<SyncJobRecord>> {
        self.operational()?.list_sync_jobs_by_state(state, limit)
    }

    pub fn list_sync_jobs_by_operation_type_before(
        &self,
        operation_type: &str,
        before: Option<(&str, &str)>,
        limit: usize,
    ) -> anyhow::Result<Vec<SyncJobRecord>> {
        self.operational()?
            .list_sync_jobs_by_operation_type_before(operation_type, before, limit)
    }

    pub fn list_active_sync_jobs_by_operation_type(
        &self,
        operation_type: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<SyncJobRecord>> {
        self.operational()?
            .list_active_sync_jobs_by_operation_type(operation_type, limit)
    }

    pub fn list_active_sync_jobs_by_operation_type_after(
        &self,
        operation_type: &str,
        after: Option<(&str, &str)>,
        limit: usize,
    ) -> anyhow::Result<Vec<SyncJobRecord>> {
        self.operational()?
            .list_active_sync_jobs_by_operation_type_after(operation_type, after, limit)
    }

    pub fn list_sync_jobs_by_operation_type_and_state(
        &self,
        operation_type: &str,
        state: SyncJobState,
        limit: usize,
    ) -> anyhow::Result<Vec<SyncJobRecord>> {
        self.operational()?
            .list_sync_jobs_by_operation_type_and_state(operation_type, state, limit)
    }

    pub fn list_sync_jobs_by_operation_type_and_state_after(
        &self,
        operation_type: &str,
        state: SyncJobState,
        after: Option<(&str, &str)>,
        limit: usize,
    ) -> anyhow::Result<Vec<SyncJobRecord>> {
        self.operational()?
            .list_sync_jobs_by_operation_type_and_state_after(operation_type, state, after, limit)
    }

    pub fn count_active_sync_jobs(&self) -> anyhow::Result<u64> {
        self.operational()?.count_active_sync_jobs()
    }

    /// Retention: delete terminal sync jobs (and their attempts) finished before
    /// `cutoff_iso`. Returns `(deleted_jobs, deleted_attempts)`.
    pub fn delete_terminal_sync_jobs_before(
        &self,
        cutoff_iso: &str,
    ) -> anyhow::Result<(usize, usize)> {
        self.operational()?
            .delete_terminal_sync_jobs_before(cutoff_iso)
    }

    pub fn operational_credential_profiles(
        &self,
    ) -> anyhow::Result<Vec<crate::OperationalCredentialProfileRecord>> {
        self.operational()?.credential_profiles()
    }

    pub fn merge_operational_credential_profile(
        &self,
        profile: &crate::OperationalCredentialProfileRecord,
    ) -> anyhow::Result<crate::OperationalCredentialProfileRecord> {
        self.operational()?.merge_credential_profile(profile)
    }

    /// The Operational store of a live test State.
    #[cfg(test)]
    pub(crate) fn test_operational(&self) -> &OperationalDb {
        self.operational()
            .expect("test State carries Operational access")
    }

    fn operational(&self) -> anyhow::Result<&OperationalDb> {
        match &self.operational {
            OperationalAccess::Available(db) => Ok(db),
            OperationalAccess::ProjectionMaintenance => {
                anyhow::bail!("operational state is unavailable in projection maintenance mode")
            }
        }
    }
}

fn verify_chain_writer_transition_adoption(
    cas: &lillux::CasStore,
    transition: &crate::sync::AdmittedChainWriterTransition,
    publishing_signer: &str,
    limits: crate::object_closure::ObjectClosureLimits,
    budget: &mut crate::object_closure::BoundedCasObjectReadBudget,
) -> anyhow::Result<()> {
    use crate::objects::{
        AdmittedLaunchCapsule, Attestation, ChainState, ChainWriterTransitionEvidence,
        PlacementRuntimeSeed, RemoteContinuationAuthority, StateManifest, ThreadEvent,
        ThreadSnapshot, ThreadStatus,
    };

    transition.validate()?;
    let evidence = &transition.evidence;
    if publishing_signer != evidence.target_node_signer_fingerprint {
        anyhow::bail!("transferred chain head is not signed by its granted target writer");
    }
    let mut get = |hash: &str, maximum: u64| budget.get_object(cas, hash, maximum);

    let writer_value = get(&transition.writer_grant_hash, limits.max_object_bytes)?
        .ok_or_else(|| anyhow::anyhow!("chain writer grant is absent"))?;
    let writer_attestation = Attestation::from_value(&writer_value)?;
    writer_attestation.verify_with_key(&transition.source_node_verifying_key)?;
    let observed_writer = ChainWriterTransitionEvidence::from_attestation(&writer_attestation)?;
    if &observed_writer != evidence {
        anyhow::bail!("chain writer grant differs from admitted transition operands");
    }

    let placement_value = get(
        &evidence.placement_attestation_hash,
        limits.max_object_bytes,
    )?
    .ok_or_else(|| anyhow::anyhow!("target placement attestation is absent"))?;
    let placement = Attestation::from_value(&placement_value)?;
    placement.verify_with_key(&transition.target_node_verifying_key)?;
    if placement.issuer_fingerprint()? != evidence.target_node_signer_fingerprint
        || placement.subject_hash != evidence.transition_subject_hash
    {
        anyhow::bail!("target placement attestation contradicts the writer grant");
    }

    let capsule_value = get(
        &evidence.transition_subject_hash,
        crate::objects::MAX_ADMITTED_LAUNCH_CAPSULE_BYTES as u64,
    )?
    .ok_or_else(|| anyhow::anyhow!("target launch capsule is absent"))?;
    AdmittedLaunchCapsule::from_current_value(capsule_value)?;

    let source_value = get(&evidence.source_chain_head_hash, limits.max_object_bytes)?
        .ok_or_else(|| anyhow::anyhow!("writer transition source chain head is absent"))?;
    let source: ChainState = serde_json::from_value(source_value)?;
    source.validate()?;
    if source.chain_root_id != evidence.chain_root_id {
        anyhow::bail!("writer transition source head belongs to another chain");
    }
    let source_entry_before = source
        .threads
        .get(&evidence.source_placement_thread_id)
        .ok_or_else(|| anyhow::anyhow!("writer transition source placement is absent"))?;
    if source_entry_before.last_event_hash.as_deref()
        != Some(evidence.source_last_event_hash.as_str())
    {
        anyhow::bail!("writer transition source event is not the source head frontier");
    }

    let target_value = get(&transition.target_chain_head_hash, limits.max_object_bytes)?
        .ok_or_else(|| anyhow::anyhow!("transferred target chain head is absent"))?;
    let target: ChainState = serde_json::from_value(target_value)?;
    target.validate()?;
    if target.chain_root_id != evidence.chain_root_id
        || target.prev_chain_state_hash.as_deref() != Some(evidence.source_chain_head_hash.as_str())
    {
        anyhow::bail!("transferred chain head is not the immediate granted successor");
    }
    let source_entry_after = target
        .threads
        .get(&evidence.source_placement_thread_id)
        .ok_or_else(|| anyhow::anyhow!("transferred source placement disappeared"))?;
    if source_entry_after.status != ThreadStatus::Continued {
        anyhow::bail!("transferred source placement was not terminalized as continued");
    }
    let continuation_hash = source_entry_after
        .last_event_hash
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("transferred source has no continuation event"))?;
    let continuation_value = get(continuation_hash, limits.max_object_bytes)?
        .ok_or_else(|| anyhow::anyhow!("transferred continuation event is absent"))?;
    let continuation: ThreadEvent = serde_json::from_value(continuation_value)?;
    continuation.validate()?;
    if continuation.event_type != "thread_continued"
        || continuation.thread_id != evidence.source_placement_thread_id
        || continuation.chain_root_id != evidence.chain_root_id
        || continuation.prev_thread_event_hash.as_deref()
            != Some(evidence.source_last_event_hash.as_str())
    {
        anyhow::bail!("transferred continuation event is not the granted source edge");
    }
    let remote: RemoteContinuationAuthority = serde_json::from_value(
        continuation
            .payload
            .get("remote_adoption")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("continuation has no remote-adoption authority"))?,
    )?;
    remote.validate()?;
    if remote.operation_id != evidence.operation_id
        || remote.source_chain_head_hash != evidence.source_chain_head_hash
        || remote.source_last_event_hash != evidence.source_last_event_hash
        || remote.target_placement_attestation_hash != evidence.placement_attestation_hash
        || remote.chain_writer_grant_hash != transition.writer_grant_hash
        || remote.target_launch_capsule_hash != evidence.transition_subject_hash
        || remote.source_accounting_transfer_hash != evidence.source_accounting_transfer_hash
        || remote.source_site_id != evidence.source_site_id
        || remote.target_site_id != evidence.target_site_id
        || remote.target_node_signer_fingerprint != evidence.target_node_signer_fingerprint
        || remote.successor_thread_id != evidence.successor_placement_thread_id
    {
        anyhow::bail!("remote continuation event differs from its writer grant");
    }
    if let Some(hash) = &remote.source_accounting_transfer_hash {
        let value = get(hash, limits.max_object_bytes)?
            .ok_or_else(|| anyhow::anyhow!("accounting allowance transfer is absent"))?;
        let transfer: crate::objects::AccountingAllowanceTransfer = serde_json::from_value(value)?;
        if transfer.content_hash()? != *hash
            || transfer.operation_id != evidence.operation_id
            || transfer.source_chain_root_id != evidence.chain_root_id
            || transfer.source_placement_thread_id != evidence.source_placement_thread_id
        {
            anyhow::bail!("accounting allowance transfer contradicts its writer grant");
        }
    }
    let runtime_seed_value = get(&remote.target_runtime_seed_hash, limits.max_object_bytes)?
        .ok_or_else(|| anyhow::anyhow!("remote continuation runtime seed is absent"))?;
    let runtime_seed = PlacementRuntimeSeed::from_current_value(runtime_seed_value)?;
    if runtime_seed.content_hash()? != remote.target_runtime_seed_hash
        || runtime_seed.operation_id != evidence.operation_id
        || runtime_seed.chain_root_id != evidence.chain_root_id
        || runtime_seed.source_placement_thread_id != evidence.source_placement_thread_id
        || runtime_seed.successor_placement_thread_id != evidence.successor_placement_thread_id
        || runtime_seed.target_site_id != evidence.target_site_id
        || runtime_seed.owner_principal != evidence.owner_principal
        || runtime_seed.target_launch_capsule_hash != evidence.transition_subject_hash
    {
        anyhow::bail!("remote continuation runtime seed contradicts its writer grant");
    }
    let checkpoint_value = get(&remote.checkpoint_manifest_hash, limits.max_object_bytes)?
        .ok_or_else(|| anyhow::anyhow!("remote continuation checkpoint is absent"))?;
    StateManifest::from_current_value(checkpoint_value)?;

    let successor_entry = target
        .threads
        .get(&evidence.successor_placement_thread_id)
        .ok_or_else(|| anyhow::anyhow!("transferred successor placement is absent"))?;
    let successor_value = get(&successor_entry.snapshot_hash, limits.max_object_bytes)?
        .ok_or_else(|| anyhow::anyhow!("transferred successor snapshot is absent"))?;
    let successor = ThreadSnapshot::from_current_value(successor_value)?;
    if successor.chain_root_id != evidence.chain_root_id
        || successor.thread_id != evidence.successor_placement_thread_id
        || successor.upstream_thread_id.as_deref()
            != Some(evidence.source_placement_thread_id.as_str())
        || successor.current_site_id != evidence.target_site_id
        || successor.origin_site_id != evidence.origin_site_id
        || successor.requested_by.as_deref() != Some(evidence.owner_principal.as_str())
        || successor.admitted_launch_capsule_hash.as_deref()
            != Some(evidence.transition_subject_hash.as_str())
    {
        anyhow::bail!("transferred successor snapshot contradicts its writer grant");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::thread_snapshot::{
        CapturedItemTrustClass, CapturedNodeHistoryPolicyProvenance, CapturedPolicyProvenance,
        CapturedThreadHistoryPolicy, ThreadHistoryRetention, ThreadSnapshot, ThreadSnapshotBuilder,
    };
    use crate::operational::{OPERATIONAL_DB_FILENAME, OPERATIONAL_INITIALIZED_FILENAME};
    use crate::signer::TestSigner;
    use crate::test_support::{TestOfflineState, TestState};

    fn test_trust_store() -> Arc<TrustStore> {
        let signer = TestSigner::default();
        let mut trust_store = TrustStore::new();
        trust_store.insert(signer.fingerprint().to_string(), signer.verifying_key());
        Arc::new(trust_store)
    }

    /// Run `work` on the closed projection file at `path` on one bound
    /// ticket of this thread's shared test host; the connection closes into
    /// the census before the result is returned.
    fn with_closed_projection<T>(
        path: &Path,
        work: impl for<'c> FnOnce(&'c rusqlite::Connection) -> anyhow::Result<T>,
    ) -> T {
        let root = Arc::new(
            lillux::PinnedDirectory::open(path.parent().expect("projection parent"))
                .unwrap()
                .expect("projection parent present"),
        );
        let name = path.file_name().expect("projection file name");
        let host = crate::test_support::shared_test_host();
        let result = host.with_existing(
            root,
            name,
            lillux_sqlite::OriginalNativeDomain::NodeProjection,
            work,
        );
        crate::test_support::release_shared_test_host(host);
        result
    }

    fn offline_preparation_roots(root: &Path) {
        for name in ["objects", "refs", "locators", "recovery/thread-projection"] {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }
    }

    #[test]
    fn offline_preparation_retains_trust_and_namespace_without_opening_sql() {
        let dir = tempfile::tempdir().unwrap();
        offline_preparation_roots(dir.path());
        let root = lillux::PinnedDirectory::open(dir.path()).unwrap().unwrap();
        let namespace = root.lock_exclusive().unwrap();
        let trust = test_trust_store();
        let prepared =
            PreparedOfflineProjection::capture(root, Arc::clone(&trust), &namespace).unwrap();
        drop(namespace);
        let authority = prepared.authority().unwrap();
        let cloned_authority = authority.try_clone().unwrap();
        drop(prepared);
        drop(authority);
        let authority = cloned_authority;
        assert!(Arc::ptr_eq(&authority.trust_store, &trust));
        assert!(authority.recovery.is_some());
        let contender = lillux::PinnedDirectory::open(dir.path()).unwrap().unwrap();
        assert!(contender.try_lock_exclusive().unwrap().is_none());
        let mut entries = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        entries.sort();
        assert_eq!(
            entries,
            ["locators", "objects", "recovery", "refs"].map(std::ffi::OsString::from)
        );
    }

    #[test]
    fn offline_preparation_refuses_missing_required_roots_and_foreign_lock() {
        let dir = tempfile::tempdir().unwrap();
        offline_preparation_roots(dir.path());
        let root = lillux::PinnedDirectory::open(dir.path()).unwrap().unwrap();
        let namespace = root.lock_exclusive().unwrap();
        std::fs::remove_dir(dir.path().join("locators")).unwrap();
        assert!(
            PreparedOfflineProjection::capture(
                root.try_clone().unwrap(),
                test_trust_store(),
                &namespace
            )
            .is_err()
        );
        std::fs::create_dir(dir.path().join("locators")).unwrap();
        std::fs::remove_dir(dir.path().join("recovery/thread-projection")).unwrap();
        assert!(
            PreparedOfflineProjection::capture(
                root.try_clone().unwrap(),
                test_trust_store(),
                &namespace
            )
            .is_err()
        );
        std::fs::create_dir(dir.path().join("recovery/thread-projection")).unwrap();
        let other = tempfile::tempdir().unwrap();
        let other_root = lillux::PinnedDirectory::open(other.path())
            .unwrap()
            .unwrap();
        let other_lock = other_root.lock_exclusive().unwrap();
        assert!(PreparedOfflineProjection::capture(root, test_trust_store(), &other_lock).is_err());
    }

    #[test]
    fn offline_preparation_rechecks_root_binding() {
        let dir = tempfile::tempdir().unwrap();
        offline_preparation_roots(dir.path());
        let root = lillux::PinnedDirectory::open(dir.path()).unwrap().unwrap();
        let namespace = root.lock_exclusive().unwrap();
        let prepared =
            PreparedOfflineProjection::capture(root, test_trust_store(), &namespace).unwrap();
        prepared.authority().unwrap();
        std::fs::rename(dir.path().join("locators"), dir.path().join("old-locators")).unwrap();
        std::fs::create_dir(dir.path().join("locators")).unwrap();
        assert!(
            prepared.authority().is_err(),
            "a replacement cannot rebind captured authority"
        );
    }

    fn open_temp() -> (tempfile::TempDir, TestState) {
        let dir = tempfile::tempdir().unwrap();
        let db = TestState::open(dir.path(), test_trust_store()).unwrap();
        (dir, db)
    }

    fn open_temp_trusted(signer: &TestSigner) -> (tempfile::TempDir, TestState) {
        let dir = tempfile::tempdir().unwrap();
        let mut trust_store = TrustStore::new();
        trust_store.insert(signer.fingerprint().to_string(), signer.verifying_key());
        let db = TestState::open(dir.path(), Arc::new(trust_store)).unwrap();
        (dir, db)
    }

    #[cfg(unix)]
    #[test]
    fn sidecar_cleanup_rejects_symlink_without_removing_any_regular_sidecar() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("projection.sqlite3");
        let wal = PathBuf::from(format!("{}-wal", path.to_string_lossy()));
        let shm = PathBuf::from(format!("{}-shm", path.to_string_lossy()));
        let outside = dir.path().join("outside-shm");
        std::fs::write(&wal, b"wal").unwrap();
        std::fs::write(&outside, b"outside").unwrap();
        symlink(&outside, &shm).unwrap();

        let directory = lillux::PinnedDirectory::open(dir.path())
            .unwrap()
            .expect("temporary directory exists");
        assert!(remove_projection_sidecars(&directory, "projection.sqlite3").is_err());
        assert_eq!(std::fs::read(&wal).unwrap(), b"wal");
        assert_eq!(std::fs::read(&outside).unwrap(), b"outside");
    }

    fn test_root_snapshot(item_ref: &str) -> ThreadSnapshot {
        ThreadSnapshotBuilder::new(
            "T-root",
            "T-root",
            "directive",
            item_ref,
            "directive-runtime",
        )
        .captured_history_policy(Some(CapturedThreadHistoryPolicy {
            retention: ThreadHistoryRetention::Durable,
            canonical_item_ref: item_ref.to_string(),
            item_content_hash: "11".repeat(32),
            item_signer_fingerprint: Some("22".repeat(32)),
            item_trust_class: CapturedItemTrustClass::Trusted,
            kind_schema_content_hash: "33".repeat(32),
            resolved_from: CapturedPolicyProvenance::NodeDefault {
                node_policy: CapturedNodeHistoryPolicyProvenance::test_policy(),
            },
        }))
        .build()
    }

    fn create_test_root_chain(db: &StateDb) -> String {
        db.create_chain(
            "T-root",
            test_root_snapshot("directive:system/test"),
            &TestSigner::default(),
        )
        .unwrap()
        .value
        .chain_state_hash
    }

    #[test]
    fn open_creates_directories() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("state");
        let db = TestState::open(&root, test_trust_store()).unwrap();

        assert!(db.cas_root().exists());
        assert!(db.refs_root().exists());
        assert!(db.locators_root().exists());
        assert!(root.join(OPERATIONAL_DB_FILENAME).is_file());
        assert!(root.join(OPERATIONAL_INITIALIZED_FILENAME).is_file());
        drop(db);
    }

    #[test]
    fn normal_current_generation_open_runs_bounded_orphan_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("state");
        let selected_path = {
            let db = TestState::open(&root, test_trust_store()).unwrap();
            db.selected_projection_path().to_path_buf()
        };
        let orphan_count = SUPERSEDED_PROJECTION_INSPECTIONS_PER_PASS + 3;
        for index in 0..orphan_count {
            std::fs::write(
                root.join(format!("projection.orphan-{index:02}.sqlite3")),
                b"abandoned staged projection",
            )
            .unwrap();
        }

        let reopened = TestState::open(&root, test_trust_store()).unwrap();

        assert_eq!(reopened.selected_projection_path(), selected_path);
        let count_orphans = || {
            std::fs::read_dir(&root)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| {
                    entry.file_name().to_str().is_some_and(|name| {
                        name.starts_with("projection.orphan-") && name.ends_with(".sqlite3")
                    })
                })
                .count()
        };
        assert_eq!(
            count_orphans(),
            orphan_count - SUPERSEDED_PROJECTION_DELETIONS_PER_PASS,
            "one normal open must perform only one bounded deletion pass"
        );
        drop(reopened);

        let reopened = TestState::open(&root, test_trust_store()).unwrap();

        assert_eq!(reopened.selected_projection_path(), selected_path);
        assert_eq!(
            count_orphans(),
            orphan_count - (SUPERSEDED_PROJECTION_DELETIONS_PER_PASS * 2),
            "repeated normal opens must make bounded progress"
        );
    }

    #[test]
    fn explicit_thread_history_discard_retires_heads_and_every_old_projection() {
        let signer = TestSigner::default();
        let (dir, db) = open_temp_trusted(&signer);
        db.create_chain(
            "T-root",
            test_root_snapshot("directive:system/test"),
            &signer,
        )
        .unwrap();
        let old_selected = db.selected_projection_path().to_path_buf();
        drop(db);
        // Retirement evidence the reset must remove with the heads.
        {
            let store = RecoveryStore::from_runtime_state_dir(dir.path()).unwrap();
            let guard = crate::recovery::CasMutationGuard::acquire_exclusive(dir.path()).unwrap();
            let refs = dir.path().join("refs");
            let owner = crate::chain::ChainLock::acquire(&refs, "T-owner").unwrap();
            let holder = crate::chain::ChainLock::acquire(&refs, "T-holder").unwrap();
            store
                .publish_retired_chain_fence(&guard, &owner, "T-owner", &"ab".repeat(32))
                .unwrap();
            store
                .publish_evidence_obligation_then(
                    &guard,
                    &owner,
                    &holder,
                    "T-owner",
                    "T-holder",
                    || Ok(()),
                )
                .unwrap();
        }
        let operation = crate::recovery::ThreadHistoryDiscardOperationRef {
            operation_id: "a".repeat(32),
            sha256: "b".repeat(64),
        };
        {
            let directory = lillux::PinnedDirectory::open(dir.path()).unwrap().unwrap();
            let namespace = directory.lock_exclusive().unwrap();
            let mut db =
                TestOfflineState::open_for_rebuild(dir.path(), &namespace, test_trust_store())
                    .unwrap();
            let authority = db.pinned_authority().unwrap();
            let guard = authority.acquire_exclusive_guard(true).unwrap();
            let preview = db
                .discard_authoritative_thread_history_admitted(&guard, true)
                .unwrap();
            assert_eq!(preview.chain_heads, 1);
            assert_eq!(preview.retirement_fences, 1);
            assert_eq!(preview.evidence_obligations, 1);
            assert!(preview.chain_ref_artifacts >= 2);
            assert!(old_selected.is_file());
            assert!(
                db.discard_authoritative_thread_history_admitted(&guard, false)
                    .unwrap_err()
                    .to_string()
                    .contains("intent was not durably published")
            );
            db.begin_thread_history_discard_admitted(&guard, &operation)
                .unwrap();
            drop(guard);
            db.close().unwrap();
        }

        // Ordinary startup takes the namespace lock itself, so it runs with
        // no offline store open, as after a process died mid-reset.
        let startup_error = TestState::open(dir.path(), test_trust_store())
            .err()
            .expect("ordinary startup must refuse an incomplete discard");
        assert!(
            startup_error
                .to_string()
                .contains("offline execution-history reset is incomplete"),
            "unexpected startup error: {startup_error:#}"
        );

        {
            let directory = lillux::PinnedDirectory::open(dir.path()).unwrap().unwrap();
            let namespace = directory.lock_exclusive().unwrap();
            let mut db =
                TestOfflineState::open_for_rebuild(dir.path(), &namespace, test_trust_store())
                    .unwrap();
            let authority = db.pinned_authority().unwrap();
            let guard = authority.acquire_exclusive_guard(true).unwrap();
            let report = db
                .discard_authoritative_thread_history_admitted(&guard, false)
                .unwrap();
            let rebuilt = report.rebuilt_projection.as_ref().unwrap();
            assert_eq!(report.retirement_fences, 1);
            assert_eq!(report.evidence_obligations, 1);
            let recovery = RecoveryStore::from_runtime_state_dir(dir.path()).unwrap();
            assert_eq!(
                recovery.retired_chain_fence("T-owner").unwrap(),
                crate::RetiredChainFence::Absent
            );
            assert_eq!(rebuilt.chains_rebuilt, 0);
            assert_eq!(rebuilt.threads_restored, 0);
            assert!(report.superseded_projection_instances >= 1);
            assert!(!old_selected.exists());
            assert!(db.get_thread("T-root").unwrap().is_none());

            // The acknowledgement follows the State's close, as the reset
            // acknowledges through a closed State: an open State still holds
            // its projection lease, which refuses the unscoped marker read.
            db.close().unwrap();
            RecoveryStore::from_runtime_state_dir(dir.path())
                .unwrap()
                .finish_thread_history_discard(&guard, &operation)
                .unwrap();
            drop(guard);
        }
        let reopened = TestState::open(dir.path(), test_trust_store()).unwrap();
        assert!(reopened.get_thread("T-root").unwrap().is_none());
    }

    #[test]
    fn explicit_project_cutover_retires_principal_and_deployed_heads() {
        let signer = TestSigner::default();
        let (_dir, db) = open_temp_trusted(&signer);
        let authority = db.pinned_authority().unwrap();
        let guard = authority.acquire_exclusive_guard(true).unwrap();
        let principal_id = format!("fp:{}", signer.fingerprint());
        let principal_key = crate::refs::principal_storage_key(&principal_id).unwrap();
        let principal_project_hash = "a".repeat(64);
        let deployed_project_hash = "b".repeat(64);
        let principal_snapshot_hash = "c".repeat(64);
        let deployed_snapshot_hash = "d".repeat(64);

        db.write_project_head_ref(
            principal_key,
            &principal_project_hash,
            &principal_snapshot_hash,
            &signer,
            &guard,
        )
        .unwrap();
        db.write_deployed_project_ref(
            &deployed_project_hash,
            &deployed_snapshot_hash,
            &signer,
            &guard,
        )
        .unwrap();

        assert_eq!(
            db.discard_all_project_heads_admitted(&guard, true).unwrap(),
            2
        );
        assert_eq!(
            db.discard_all_project_heads_admitted(&guard, false)
                .unwrap(),
            2
        );
        assert!(
            db.read_project_head(principal_key, &principal_project_hash)
                .unwrap()
                .is_none()
        );
        assert!(
            db.read_deployed_project_ref(&deployed_project_hash)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn explicit_external_content_cutover_retires_only_binding_heads() {
        let signer = TestSigner::default();
        let (_dir, db) = open_temp_trusted(&signer);
        let authority = db.pinned_authority().unwrap();
        let guard = authority.acquire_exclusive_guard(true).unwrap();
        let namespace = crate::objects::EXTERNAL_CONTENT_BINDING_HEAD_NAMESPACE;
        let first = "a".repeat(64);
        let second = "b".repeat(64);
        db.write_generic_head_ref(namespace, &first, &"c".repeat(64), &signer, &guard)
            .unwrap();
        db.write_generic_head_ref(namespace, &second, &"d".repeat(64), &signer, &guard)
            .unwrap();
        db.write_generic_head_ref("unrelated", "retained", &"e".repeat(64), &signer, &guard)
            .unwrap();

        assert_eq!(
            db.discard_external_content_binding_heads(&guard, true)
                .unwrap(),
            2
        );
        assert_eq!(
            db.discard_external_content_binding_heads(&guard, false)
                .unwrap(),
            2
        );
        assert_eq!(
            db.external_content_binding_schema_epoch().unwrap(),
            Some(crate::objects::EXTERNAL_CONTENT_BINDING_SCHEMA_EPOCH)
        );
        assert!(db.list_generic_head_refs(namespace).unwrap().is_empty());
        assert!(
            db.read_generic_head_ref("unrelated", "retained")
                .unwrap()
                .is_some()
        );
        assert_eq!(
            db.discard_external_content_binding_heads(&guard, false)
                .unwrap(),
            0
        );

        drop(guard);
        let guard = authority.acquire_shared_guard().unwrap();
        db.ensure_current_external_content_binding_epoch(&guard)
            .expect("ordinary online binding may validate an already-current epoch");
    }

    #[test]
    fn fresh_external_content_epoch_requires_exclusive_authority_and_no_heads() {
        let signer = TestSigner::default();
        let (_dir, db) = open_temp_trusted(&signer);
        let authority = db.pinned_authority().unwrap();

        let shared = authority.acquire_shared_guard().unwrap();
        let error = db
            .ensure_current_external_content_binding_epoch(&shared)
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("requires exclusive state authority")
        );
        drop(shared);

        let exclusive = authority.acquire_exclusive_guard(true).unwrap();
        db.ensure_current_external_content_binding_epoch(&exclusive)
            .unwrap();
        assert_eq!(
            db.external_content_binding_schema_epoch().unwrap(),
            Some(crate::objects::EXTERNAL_CONTENT_BINDING_SCHEMA_EPOCH)
        );
    }

    #[test]
    fn missing_external_content_epoch_with_heads_requires_explicit_reset() {
        let signer = TestSigner::default();
        let (_dir, db) = open_temp_trusted(&signer);
        let authority = db.pinned_authority().unwrap();
        let guard = authority.acquire_exclusive_guard(true).unwrap();
        db.write_generic_head_ref(
            crate::objects::EXTERNAL_CONTENT_BINDING_HEAD_NAMESPACE,
            &"a".repeat(64),
            &"b".repeat(64),
            &signer,
            &guard,
        )
        .unwrap();

        let error = db
            .ensure_current_external_content_binding_epoch(&guard)
            .unwrap_err();
        assert!(error.to_string().contains("while binding heads exist"));
    }

    #[test]
    fn valid_current_generation_open_does_not_enumerate_signed_heads() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("state");
        let selected_path = {
            let db = TestState::open(&root, test_trust_store()).unwrap();
            db.selected_projection_path().to_path_buf()
        };
        assert!(
            crate::rebuild::take_chain_ref_enumerations_for_test() > 0,
            "the first baseline must exercise and reset the enumeration counter"
        );

        // Any baseline/head walk would try to decode this authoritative-looking
        // entry and fail. A valid current generation must enumerate only the
        // pending journal, so the poisoned head is deliberately untouched.
        let poison = root.join("refs/generic/chains/T-poison/head");
        std::fs::create_dir_all(poison.parent().unwrap()).unwrap();
        std::fs::write(&poison, b"not a signed ref").unwrap();

        let reopened = TestState::open(&root, test_trust_store()).unwrap();
        assert_eq!(reopened.selected_projection_path(), selected_path);
        assert!(poison.is_file());
        assert_eq!(
            crate::rebuild::take_chain_ref_enumerations_for_test(),
            0,
            "a valid current-generation open must perform zero chain-ref enumerations"
        );
    }

    #[test]
    fn absent_or_mismatched_generation_requires_a_fresh_baseline() {
        for mismatched_epoch in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("state");
            let (old_path, old_generation) = {
                let db = TestState::open(&root, test_trust_store()).unwrap();
                create_test_root_chain(&db);
                db.projection()
                    .with_sql(|connection| {
                        Ok(connection
                            .execute("DELETE FROM threads WHERE thread_id = 'T-root'", [])?)
                    })
                    .unwrap();
                let old_path = db.selected_projection_path().to_path_buf();
                // The generation file is read once no live SQLite alias of
                // its projection remains.
                db.close().unwrap();
                let recovery = RecoveryStore::from_runtime_state_dir(&root).unwrap();
                (old_path, recovery.read_generation().unwrap().unwrap())
            };

            let recovery = RecoveryStore::from_runtime_state_dir(&root).unwrap();
            if mismatched_epoch {
                let mut mismatched = old_generation;
                mismatched.projection_schema_epoch =
                    ProjectionDb::current_schema_epoch().wrapping_add(1);
                recovery.write_generation(&mismatched).unwrap();
            } else {
                std::fs::remove_file(recovery.generation_path()).unwrap();
            }

            let reopened = TestState::open(&root, test_trust_store()).unwrap();
            assert_ne!(
                reopened.selected_projection_path(),
                old_path,
                "an absent or wrong-epoch pointer must select a new instance"
            );
            assert!(
                reopened.get_thread("T-root").unwrap().is_some(),
                "the new baseline must restore rows from signed CAS truth"
            );
            reopened.close().unwrap();
            assert_eq!(
                recovery
                    .read_generation()
                    .unwrap()
                    .unwrap()
                    .projection_schema_epoch,
                ProjectionDb::current_schema_epoch()
            );
        }
    }

    #[test]
    fn pointer_instance_identity_mismatch_requires_a_fresh_baseline() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("state");
        let old_path = {
            let db = TestState::open(&root, test_trust_store()).unwrap();
            create_test_root_chain(&db);
            db.projection()
                .with_sql(|connection| {
                    Ok(connection.execute("DELETE FROM threads WHERE thread_id = 'T-root'", [])?)
                })
                .unwrap();
            db.selected_projection_path().to_path_buf()
        };

        with_closed_projection(&old_path, |connection| {
            Ok(connection.execute(
                "UPDATE projection_recovery_identity
                 SET projection_instance_id = 'different-instance'
                 WHERE singleton = 1",
                [],
            )?)
        });

        let reopened = TestState::open(&root, test_trust_store()).unwrap();
        assert_ne!(reopened.selected_projection_path(), old_path);
        assert!(reopened.get_thread("T-root").unwrap().is_some());
        // The reopened State holds its selected projection's lease, so the
        // generation is read inside its own factory's scope.
        let generation = RecoveryStore::from_runtime_state_dir(&root)
            .unwrap()
            .read_generation_scoped(reopened.host().factory())
            .unwrap()
            .unwrap();
        assert_eq!(
            reopened
                .projection()
                .projection_instance_id()
                .unwrap()
                .as_deref(),
            Some(generation.projection_instance_id.as_str())
        );
    }

    #[test]
    fn temporary_projection_replay_never_acknowledges_pending_work() {
        let (_dir, db) = open_temp();
        let target_hash = create_test_root_chain(&db);
        let transition = {
            let chain_lock = crate::chain::ChainLock::acquire(db.refs_root(), "T-root").unwrap();
            db.recovery
                .prepare_set(&chain_lock, "T-root", None, &target_hash)
                .unwrap()
        };
        let staged = crate::test_support::TestProjection::open();

        let report = replay_pending_into(
            &staged,
            db.cas_root(),
            db.refs_root(),
            &db.recovery,
            db.head_trust_store(),
            None,
            false,
            None,
        )
        .unwrap();

        assert_eq!(report.sets_repaired, 1);
        let pending = db
            .recovery
            .read_pending("T-root")
            .unwrap()
            .expect("temporary replay must leave the journal record intact");
        assert_eq!(pending.transition_id, transition.transition_id);
        assert_eq!(pending.phase, TransitionPhase::ApplyingProjection);
        assert_eq!(
            staged
                .get_projection_meta("T-root")
                .unwrap()
                .unwrap()
                .indexed_chain_state_hash,
            target_hash
        );
    }

    #[test]
    fn removal_mutations_reject_a_lock_for_another_chain() {
        let (_dir, db) = open_temp();
        let wrong_lock = crate::chain::ChainLock::acquire(db.refs_root(), "T-other").unwrap();

        for error in [
            db.remove_chain_head_ref("T-root", &wrong_lock).unwrap_err(),
            db.acknowledge_chain_removal_cleanup("T-root", &wrong_lock)
                .unwrap_err(),
            db.cancel_pending_chain_removal("T-root", &wrong_lock)
                .unwrap_err(),
            db.delete_chain_projection("T-root", &wrong_lock)
                .unwrap_err(),
        ] {
            assert!(error.to_string().contains("chain lock protects T-other"));
        }
    }

    #[test]
    fn removal_mutations_reject_same_chain_lock_from_another_refs_root() {
        let (_dir, db) = open_temp();
        drop(crate::chain::ChainLock::acquire(db.refs_root(), "T-root").unwrap());
        let other = tempfile::tempdir().unwrap();
        let wrong_root_lock =
            crate::chain::ChainLock::acquire(&other.path().join("refs"), "T-root").unwrap();

        for error in [
            db.remove_chain_head_ref("T-root", &wrong_root_lock)
                .unwrap_err(),
            db.acknowledge_chain_removal_cleanup("T-root", &wrong_root_lock)
                .unwrap_err(),
            db.cancel_pending_chain_removal("T-root", &wrong_root_lock)
                .unwrap_err(),
            db.delete_chain_projection("T-root", &wrong_root_lock)
                .unwrap_err(),
        ] {
            assert!(format!("{error:#}").contains("chain lock directory for T-root was replaced"));
        }
    }

    #[test]
    fn projection_delete_requires_a_published_pending_remove() {
        let (_dir, db) = open_temp();
        let lock = crate::chain::ChainLock::acquire(db.refs_root(), "T-root").unwrap();
        let error = db.delete_chain_projection("T-root", &lock).unwrap_err();
        assert!(error.to_string().contains("no pending removal"));
        drop(lock);

        let signer = TestSigner::default();
        let created = db
            .create_chain(
                "T-root",
                test_root_snapshot("directive:system/test"),
                &signer,
            )
            .unwrap();
        let lock = crate::chain::ChainLock::acquire(db.refs_root(), "T-root").unwrap();
        db.recovery
            .prepare_remove(&lock, "T-root", created.value.chain_state_hash.as_str())
            .unwrap();
        let error = db.delete_chain_projection("T-root", &lock).unwrap_err();
        assert!(error.to_string().contains("before durable head removal"));
        assert!(
            db.projection()
                .get_projection_meta("T-root")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn create_chain_stores_in_projection() {
        let (_dir, db) = open_temp();
        let signer = TestSigner::default();

        let snapshot = test_root_snapshot("directive:system/test");

        let result = db.create_chain("T-root", snapshot, &signer).unwrap();
        assert_eq!(result.projection, ProjectionStatus::Current);
        assert!(!result.value.chain_state_hash.is_empty());
        assert!(!result.value.snapshot_hash.is_empty());

        let row = db
            .get_thread("T-root")
            .unwrap()
            .expect("thread should exist in projection");
        assert_eq!(row.thread_id, "T-root");
        assert_eq!(row.chain_root_id, "T-root");
        assert_eq!(row.kind, "directive");
        assert_eq!(row.status, "created");

        let meta = db
            .projection()
            .get_projection_meta("T-root")
            .unwrap()
            .expect("successful projection must advance its cursor");
        assert_eq!(meta.indexed_chain_state_hash, result.value.chain_state_hash);
    }

    #[cfg(unix)]
    #[test]
    fn chain_mutation_fails_closed_after_cas_root_replacement() {
        let (dir, db) = open_temp();
        let original = db.cas_root().to_path_buf();
        let retained = dir.path().join("retained-objects");
        std::fs::rename(&original, &retained).unwrap();
        std::fs::create_dir(&original).unwrap();

        let error = db
            .create_chain(
                "T-root",
                test_root_snapshot("directive:system/test"),
                &TestSigner::default(),
            )
            .unwrap_err();

        assert!(format!("{error:#}").contains("CAS root was replaced"));
        assert!(!original.join("objects").exists());
        // Restore the bound root so the live State closes into its census.
        std::fs::remove_dir(&original).unwrap();
        std::fs::rename(&retained, &original).unwrap();
        db.close().unwrap();
    }

    #[test]
    fn projection_uses_exact_snapshots_committed_after_clock_rollback_clamping() {
        let (_dir, db) = open_temp();
        let signer = TestSigner::default();
        let floor = "2099-01-01T00:00:10Z";
        let rolled_back = "2099-01-01T00:00:05Z";

        let mut root = test_root_snapshot("directive:system/test");
        root.created_at = floor.into();
        root.updated_at = floor.into();
        let created = db.create_chain("T-root", root, &signer).unwrap();
        assert_eq!(created.value.snapshot.updated_at, floor);
        assert_eq!(db.get_thread("T-root").unwrap().unwrap().updated_at, floor);

        let child = ThreadSnapshotBuilder::new(
            "T-child",
            "T-root",
            "directive",
            "system/child",
            "directive-runtime",
        )
        .created_at(rolled_back.into())
        .updated_at(rolled_back.into())
        .build();
        let added = db.add_thread("T-root", child, &signer).unwrap();
        assert_eq!(added.value.snapshot.updated_at, floor);
        assert_eq!(db.get_thread("T-child").unwrap().unwrap().updated_at, floor);

        let mut running = created.value.snapshot.clone();
        running.status = crate::objects::ThreadStatus::Running;
        running.updated_at = rolled_back.into();
        running.started_at = Some(rolled_back.into());
        let event =
            crate::objects::thread_event::NewEvent::new("T-root", "T-root", "thread_started")
                .build_with_ts(rolled_back.into());
        let appended = db
            .append_events(
                "T-root",
                "T-root",
                vec![event],
                vec![SnapshotUpdate {
                    thread_id: "T-root".into(),
                    new_snapshot: running,
                }],
                &signer,
            )
            .unwrap();
        let committed_update = &appended.value.snapshot_updates[0].new_snapshot;
        assert_eq!(committed_update.updated_at, floor);
        assert_eq!(committed_update.started_at.as_deref(), Some(floor));
        let root_row = db.get_thread("T-root").unwrap().unwrap();
        assert_eq!(root_row.updated_at, committed_update.updated_at);
        assert_eq!(root_row.started_at, committed_update.started_at);

        let branch = ThreadSnapshotBuilder::new(
            "T-branch",
            "T-root",
            "directive",
            "system/branch",
            "directive-runtime",
        )
        .created_at(rolled_back.into())
        .updated_at(rolled_back.into())
        .build();
        let branch_event =
            crate::objects::thread_event::NewEvent::new("T-root", "T-branch", "thread_created")
                .build_with_ts(rolled_back.into());
        let added_with_events = db
            .add_thread_with_events("T-root", branch, vec![branch_event], &signer)
            .unwrap();
        assert_eq!(added_with_events.value.snapshot.updated_at, floor);
        assert_eq!(
            db.get_thread("T-branch").unwrap().unwrap().updated_at,
            floor
        );
    }

    #[test]
    fn authoritative_snapshot_read_uses_verified_head_and_cas() {
        let signer = TestSigner::default();
        let (_dir, db) = open_temp_trusted(&signer);
        let snapshot = test_root_snapshot("directive:system/test");
        db.create_chain("T-root", snapshot.clone(), &signer)
            .unwrap();

        let loaded = db
            .read_authoritative_thread_snapshot("T-root", "T-root")
            .unwrap()
            .expect("root snapshot should exist through authoritative head");
        assert_eq!(loaded.to_value(), snapshot.to_value());
        assert!(
            db.read_authoritative_thread_snapshot("T-root", "T-missing")
                .unwrap()
                .is_none()
        );
        assert!(
            db.read_authoritative_thread_snapshot("T-missing", "T-missing")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn authoritative_snapshot_last_event_uses_the_thread_entry_tip() {
        let signer = TestSigner::default();
        let (_dir, db) = open_temp_trusted(&signer);
        let snapshot = test_root_snapshot("directive:system/test");
        assert!(snapshot.last_event_hash.is_none());
        db.create_chain("T-root", snapshot, &signer).unwrap();

        let event = crate::objects::thread_event::NewEvent::new(
            "T-root",
            "T-root",
            "observation_without_snapshot_transition",
        )
        .payload(serde_json::json!({"observed": true}))
        .build();
        let appended = db
            .append_events("T-root", "T-root", vec![event], vec![], &signer)
            .unwrap();
        let committed_event = appended.value.events.last().unwrap();

        let readback = db
            .read_authoritative_thread_snapshot_with_last_event("T-root", "T-root")
            .unwrap()
            .expect("authoritative root");
        assert!(readback.snapshot.last_event_hash.is_none());
        assert_eq!(readback.chain_head_hash, appended.value.chain_state_hash);
        let (event_hash, event) = readback.last_event.expect("thread-entry tip");
        assert_eq!(
            event_hash,
            crate::objects::canonical_value_digest(&committed_event.to_value()).unwrap()
        );
        assert_eq!(event.event_type, "observation_without_snapshot_transition");
        assert_eq!(event.thread_seq, 1);
    }

    fn lineage_accounting_event(thread_id: &str) -> crate::ThreadEvent {
        let attempt_id = format!("A-lineage-{thread_id}");
        let payload = ryeos_accounting::ProviderAttemptBudgetTransitionV1 {
            version: ryeos_accounting::PROVIDER_ATTEMPT_BUDGET_TRANSITION_VERSION,
            transition_id: ryeos_accounting::transition_id(&attempt_id, 1),
            transition_sequence: 1,
            attempt_id,
            budget_authority_site_id: "S-site".into(),
            ledger_epoch: 1,
            execution_budget_id: "B-lineage".into(),
            root_chain_id: "T-root".into(),
            audit_chain_root_id: "T-root".into(),
            directive_budget_id: None,
            thread_id: thread_id.into(),
            turn: 1,
            attempt_number: 1,
            transition: ryeos_accounting::AttemptBudgetState::Reserved,
            observation: false,
            config_hash: "a".repeat(64),
            provider_id: "provider".into(),
            model: "model".into(),
            profile: None,
            reserved_usd_nanos: 0,
            budget_charge_usd_nanos: None,
            provider_actual_usd_nanos: None,
            released_usd_nanos: None,
            charge_basis: None,
            occurred_at_ms: 1000,
            reason: None,
        };
        payload.validate().unwrap();
        crate::objects::thread_event::NewEvent::new(
            "T-root",
            thread_id,
            crate::event_types::PROVIDER_ATTEMPT_BUDGET_TRANSITION_V1,
        )
        .payload(serde_json::to_value(payload).unwrap())
        .build()
    }

    #[test]
    fn authoritative_continuation_presence_survives_deferred_accounting() {
        for continued in [false, true] {
            let signer = TestSigner::default();
            let (_dir, db) = open_temp_trusted(&signer);
            let snapshot = test_root_snapshot("tool:system/verifier");
            db.create_chain("T-root", snapshot.clone(), &signer)
                .unwrap();
            let mut events = vec![
                crate::objects::thread_event::NewEvent::new(
                    "T-root",
                    "T-root",
                    crate::event_types::THREAD_COMPLETED,
                )
                .build(),
            ];
            if continued {
                events.push(
                    crate::objects::thread_event::NewEvent::new(
                        "T-root",
                        "T-root",
                        crate::event_types::THREAD_CONTINUED,
                    )
                    .payload(serde_json::json!({"successor_thread_id":"T-successor"}))
                    .build(),
                );
            }
            events.push(lineage_accounting_event("T-root"));
            let appended = db
                .append_events("T-root", "T-root", events, vec![], &signer)
                .unwrap();
            let (head, actual, present) = db
                .read_authoritative_thread_snapshot_with_event_presence(
                    "T-root",
                    "T-root",
                    crate::event_types::THREAD_CONTINUED,
                    10,
                    100_000,
                )
                .unwrap()
                .unwrap();
            assert_eq!(head, appended.value.chain_state_hash);
            assert_eq!(actual.to_value(), snapshot.to_value());
            assert_eq!(present, continued);
            let authority = db.pinned_authority().unwrap();
            let guard = authority.acquire_shared_guard().unwrap();
            let mut observed = Vec::new();
            let visited = authority
                .visit_thread_snapshot_events(
                    "T-root",
                    "T-root",
                    10,
                    100_000,
                    &guard,
                    |hash, event| observed.push((hash, event)),
                )
                .unwrap()
                .unwrap();
            assert_eq!(visited.0, head);
            assert_eq!(visited.1.to_value(), actual.to_value());
            assert!(!observed.is_empty());
            assert!(observed.windows(2).all(|pair| {
                pair[0].1.thread_seq == pair[1].1.thread_seq + 1
                    && pair[0].1.prev_thread_event_hash.as_ref() == Some(&pair[1].0)
            }));
            assert_eq!(observed.last().unwrap().1.thread_seq, 1);
            assert_eq!(
                observed
                    .iter()
                    .any(|(_, event)| event.event_type == crate::event_types::THREAD_CONTINUED),
                continued,
            );
            assert!(authority.visit_thread_snapshot_events(
                "T-root", "T-root", 1, 100_000, &guard, |_, _| {},
            ).is_err());
            assert!(
                authority
                    .visit_thread_snapshot_events("T-root", "T-root", 10, 1, &guard, |_, _| {},)
                    .is_err()
            );
            assert!(
                authority
                    .visit_thread_snapshot_events(
                        "T-root",
                        "T-absent",
                        10,
                        100_000,
                        &guard,
                        |_, _| panic!("absent thread cannot yield evidence"),
                    )
                    .unwrap()
                    .is_none()
            );
            assert!(
                db.read_authoritative_thread_snapshot_with_event_presence(
                    "T-root",
                    "T-root",
                    crate::event_types::THREAD_CONTINUED,
                    1,
                    100_000,
                )
                .is_err()
            );
            assert!(
                db.read_authoritative_thread_snapshot_with_event_presence(
                    "T-root",
                    "T-root",
                    crate::event_types::THREAD_CONTINUED,
                    10,
                    1,
                )
                .is_err()
            );
            assert!(
                db.read_authoritative_thread_snapshot_with_event_presence(
                    "T-root",
                    "T-absent",
                    crate::event_types::THREAD_CONTINUED,
                    10,
                    100_000,
                )
                .unwrap()
                .is_none()
            );
        }
    }

    #[test]
    fn machine_lineage_requires_exact_edges_and_birth_even_after_accounting() {
        for (operator, wrong_birth, remote) in [
            (false, false, false),
            (true, false, false),
            (false, true, false),
            (false, false, true),
        ] {
            let signer = TestSigner::default();
            let (_dir, db) = open_temp_trusted(&signer);
            db.create_chain("T-root", test_root_snapshot("graph:example/build"), &signer)
                .unwrap();
            let mut successor = ThreadSnapshotBuilder::new(
                "T-next",
                "T-root",
                "graph",
                "graph:example/build",
                "graph-runtime",
            )
            .build();
            successor.upstream_thread_id = Some("T-root".into());
            let birth = crate::objects::thread_event::NewEvent::new(
                "T-root",
                "T-next",
                crate::event_types::THREAD_CREATED,
            )
            .payload(serde_json::json!({
                "continuation_from": if wrong_birth { "T-other" } else { "T-root" }
            }))
            .build();
            let mut edge = serde_json::json!({
                "successor_thread_id": "T-next",
                "reason": if remote { "remote_adoption" } else { "graph_follow_resume" }
            });
            if operator {
                edge["successor_request_fingerprint"] = "a".repeat(64).into();
            }
            let continuation = crate::objects::thread_event::NewEvent::new(
                "T-root",
                "T-root",
                crate::event_types::THREAD_CONTINUED,
            )
            .payload(edge)
            .build();
            db.add_thread_with_events_and_append(
                "T-root",
                successor,
                vec![birth],
                "T-root",
                vec![continuation],
                vec![],
                &signer,
            )
            .unwrap();
            // Accounting after the continuation must not hide the edge; nor
            // should accounting on the final placement invalidate its lineage.
            for id in ["T-root", "T-next"] {
                db.append_events(
                    "T-root",
                    id,
                    vec![lineage_accounting_event(id)],
                    vec![],
                    &signer,
                )
                .unwrap();
            }
            let authority = db.pinned_authority().unwrap();
            let guard = authority.acquire_shared_guard().unwrap();
            let result = authority.read_machine_continuation_lineage("T-root", "T-next", &guard);
            if operator || wrong_birth || remote {
                assert!(result.is_err());
            } else {
                let (_, snapshots) = result.unwrap().unwrap();
                assert_eq!(
                    snapshots
                        .iter()
                        .map(|s| s.thread_id.as_str())
                        .collect::<Vec<_>>(),
                    vec!["T-root", "T-next"]
                );
                assert!(
                    authority
                        .read_machine_continuation_lineage("T-root", "T-root", &guard)
                        .is_err()
                );
            }
        }
    }

    #[test]
    fn machine_segment_starts_after_prior_operator_but_refuses_later_operator() {
        let signer = TestSigner::default();
        let (_dir, db) = open_temp_trusted(&signer);
        db.create_chain("T-root", test_root_snapshot("graph:example/build"), &signer)
            .unwrap();
        for (source, next, operator) in [
            ("T-root", "T-caller", true),
            ("T-caller", "T-terminal", false),
        ] {
            let mut snapshot = ThreadSnapshotBuilder::new(
                next,
                "T-root",
                "graph",
                "graph:example/build",
                "graph-runtime",
            )
            .build();
            snapshot.upstream_thread_id = Some(source.into());
            let mut edge =
                serde_json::json!({"successor_thread_id":next,"reason":"graph_follow_resume"});
            if operator {
                edge["successor_request_fingerprint"] = "a".repeat(64).into();
            }
            db.add_thread_with_events_and_append(
                "T-root",
                snapshot,
                vec![
                    crate::objects::thread_event::NewEvent::new(
                        "T-root",
                        next,
                        crate::event_types::THREAD_CREATED,
                    )
                    .payload(serde_json::json!({"continuation_from":source}))
                    .build(),
                ],
                source,
                vec![
                    crate::objects::thread_event::NewEvent::new(
                        "T-root",
                        source,
                        crate::event_types::THREAD_CONTINUED,
                    )
                    .payload(edge)
                    .build(),
                ],
                vec![],
                &signer,
            )
            .unwrap();
        }
        let authority = db.pinned_authority().unwrap();
        let guard = authority.acquire_shared_guard().unwrap();
        let (_, segment) = authority
            .read_machine_continuation_segment("T-root", "T-caller", "T-terminal", &guard)
            .unwrap()
            .unwrap();
        assert_eq!(segment.len(), 2);
        assert_eq!(segment[0].thread_id, "T-caller");
        let error = authority
            .read_machine_continuation_segment("T-root", "T-root", "T-terminal", &guard)
            .unwrap_err();
        assert!(
            error
                .downcast_ref::<MachineContinuationBoundary>()
                .is_some()
        );
    }

    #[test]
    fn machine_lineage_does_not_infer_continuation_from_same_chain_upstream() {
        let signer = TestSigner::default();
        let (_dir, db) = open_temp_trusted(&signer);
        db.create_chain("T-root", test_root_snapshot("graph:example/build"), &signer)
            .unwrap();
        let mut branch = ThreadSnapshotBuilder::new(
            "T-branch",
            "T-root",
            "graph",
            "graph:example/build",
            "graph-runtime",
        )
        .build();
        branch.upstream_thread_id = Some("T-root".into());
        db.add_thread_with_events(
            "T-root",
            branch,
            vec![
                crate::objects::thread_event::NewEvent::new(
                    "T-root",
                    "T-branch",
                    crate::event_types::THREAD_CREATED,
                )
                .payload(serde_json::json!({"continuation_from":"T-root"}))
                .build(),
            ],
            &signer,
        )
        .unwrap();
        let authority = db.pinned_authority().unwrap();
        let guard = authority.acquire_shared_guard().unwrap();
        assert!(
            authority
                .read_machine_continuation_lineage("T-root", "T-branch", &guard)
                .is_err()
        );
        assert!(
            authority
                .read_machine_continuation_lineage("T-unrelated", "T-branch", &guard)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn state_db_always_retains_head_trust() {
        let (_dir, db) = open_temp();
        assert!(!db.head_trust_store().is_empty());
    }

    #[test]
    fn projection_commit_is_idempotent_at_committed_cursor() {
        let (_dir, db) = open_temp();
        let signer = TestSigner::default();
        let snapshot = test_root_snapshot("directive:system/test");
        let committed = db.create_chain("T-root", snapshot, &signer).unwrap();
        let cache = db.head_cache.lock().unwrap();

        project_committed_chain(
            db.projection(),
            &cache,
            "T-root",
            &committed.value.chain_state_hash,
            || anyhow::bail!("idempotent projection must not replay rows"),
        )
        .unwrap();
    }

    #[test]
    fn projection_cursor_conflict_rolls_back_without_projecting_rows() {
        let (_dir, db) = open_temp();
        let signer = TestSigner::default();
        let snapshot = test_root_snapshot("directive:system/test");
        db.create_chain("T-root", snapshot, &signer).unwrap();
        let conflicting_hash = "f".repeat(64);
        db.projection()
            .update_projection_meta(&crate::projection::ProjectionMeta {
                chain_root_id: "T-root".to_string(),
                indexed_chain_state_hash: conflicting_hash.clone(),
                updated_at: "2026-01-01T00:00:00Z".to_string(),
            })
            .unwrap();
        let before: i64 = db
            .projection()
            .with_sql(|connection| {
                Ok(connection.query_row("SELECT count(*) FROM events", [], |row| row.get(0))?)
            })
            .unwrap();
        let event =
            crate::objects::thread_event::NewEvent::new("T-root", "T-root", "cursor_conflict_test")
                .build();

        let committed = db
            .append_events("T-root", "T-root", vec![event], vec![], &signer)
            .unwrap();

        assert!(matches!(
            committed.projection,
            ProjectionStatus::RepairRequired(_)
        ));
        let after: i64 = db
            .projection()
            .with_sql(|connection| {
                Ok(connection.query_row("SELECT count(*) FROM events", [], |row| row.get(0))?)
            })
            .unwrap();
        assert_eq!(after, before);
        assert_eq!(
            db.projection()
                .get_projection_meta("T-root")
                .unwrap()
                .unwrap()
                .indexed_chain_state_hash,
            conflicting_hash
        );
    }

    #[test]
    fn committed_write_reports_stale_projection_without_losing_cas_result() {
        let (dir, db) = open_temp();
        let signer = TestSigner::default();
        let snapshot = test_root_snapshot("directive:system/test");
        let initial = db.create_chain("T-root", snapshot, &signer).unwrap();
        db.projection()
            .with_sql(|connection| {
                Ok(connection.execute_batch(
                    "CREATE TRIGGER reject_test_event_projection
                 BEFORE INSERT ON events
                 BEGIN
                     SELECT RAISE(FAIL, 'injected event projection failure');
                 END;",
                )?)
            })
            .unwrap();

        let event = crate::objects::thread_event::NewEvent::new(
            "T-root",
            "T-root",
            "projection_failure_test",
        )
        .build();
        let committed = db
            .append_events("T-root", "T-root", vec![event], vec![], &signer)
            .unwrap();
        assert!(matches!(
            committed.projection,
            ProjectionStatus::RepairRequired(ProjectionRepairRequest {
                operation: "append_events",
                ..
            })
        ));
        let head = db
            .read_generic_head_ref("chains", "T-root")
            .unwrap()
            .expect("committed chain has a verified head");
        assert_eq!(head.target_hash, committed.value.chain_state_hash);
        assert_eq!(
            db.projection()
                .get_projection_meta("T-root")
                .unwrap()
                .unwrap()
                .indexed_chain_state_hash,
            initial.value.chain_state_hash,
            "failed row projection must roll back the cursor advance"
        );

        db.projection()
            .with_sql(|connection| {
                Ok(connection.execute_batch("DROP TRIGGER reject_test_event_projection")?)
            })
            .unwrap();
        drop(db);
        let reopened = TestState::open(dir.path(), test_trust_store()).unwrap();
        let projected = reopened
            .projection()
            .get_projection_meta("T-root")
            .unwrap()
            .expect("startup pending replay must restore the projection cursor");
        assert_eq!(projected.indexed_chain_state_hash, head.target_hash);
    }

    #[test]
    fn locked_open_sweeps_interrupted_state_publications_only() {
        let dir = tempfile::tempdir().unwrap();
        let projection = dir.path().join("recovery/thread-projection");
        std::fs::create_dir_all(projection.join("pending")).unwrap();
        let own = format!(".secure.tmp.{}.4242.1", crate::STATE_TEMP_PURPOSE.as_str());
        let owned = [
            dir.path().join("recovery").join(&own),
            projection.join("pending").join(&own),
        ];
        let foreign = projection.join(".secure.tmp.reset.4242.2");
        for path in owned.iter().chain([&foreign]) {
            std::fs::write(path, b"interrupted").unwrap();
        }
        // The sweep precedes every SQLite open; the live open pins and
        // locks the namespace itself.
        let opened = TestState::open(dir.path(), test_trust_store());
        for path in &owned {
            assert!(!path.exists(), "{}", path.display());
        }
        assert!(foreign.is_file());

        let retired_format = projection.join(".secure.tmp.4242.3");
        std::fs::write(&retired_format, b"unattributed").unwrap();
        drop(opened);
        let error = TestState::open(dir.path(), test_trust_store())
            .err()
            .expect("an unattributed secure temporary refuses State startup");
        assert!(
            error.to_string().contains("without an exact owner token"),
            "{error:#}"
        );
        assert!(retired_format.is_file());
        assert!(foreign.is_file());
    }

    fn discard_operation_for_startup() -> crate::recovery::ThreadHistoryDiscardOperationRef {
        crate::recovery::ThreadHistoryDiscardOperationRef {
            operation_id: "a".repeat(32),
            sha256: "b".repeat(64),
        }
    }

    /// The marker gate runs before every SQLite open, so its verdict is
    /// observable whether or not this host admits the later database opens.
    fn locked_open_error(root_path: &Path) -> Option<String> {
        TestState::open(root_path, test_trust_store())
            .err()
            .map(|error| format!("{error:#}"))
    }

    #[test]
    fn startup_accepts_an_acknowledged_discard_marker_and_refuses_active_or_retired_ones() {
        let dir = tempfile::tempdir().unwrap();
        let operation = discard_operation_for_startup();
        let store = crate::recovery::RecoveryStore::from_runtime_state_dir(dir.path()).unwrap();
        crate::recovery::CasMutationGuard::ensure_anchor(dir.path()).unwrap();
        let guard = crate::recovery::CasMutationGuard::acquire_exclusive(dir.path()).unwrap();
        store
            .begin_thread_history_discard(&guard, &operation)
            .unwrap();
        drop(guard);
        let active = locked_open_error(dir.path())
            .expect("ordinary startup must refuse an Active discard marker");
        assert!(
            active.contains("offline execution-history reset is incomplete"),
            "{active}"
        );

        let guard = crate::recovery::CasMutationGuard::acquire_exclusive(dir.path()).unwrap();
        store
            .finish_thread_history_discard(&guard, &operation)
            .unwrap();
        drop(guard);
        let marker = store.root().join("thread-history-discard.json");
        assert!(marker.is_file(), "the Acknowledged record persists");
        assert!(!store.thread_history_discard_in_progress().unwrap());
        assert_eq!(store.thread_history_discard_operation().unwrap(), None);
        assert_eq!(
            store.last_acknowledged_thread_history_discard().unwrap(),
            Some(operation.clone())
        );
        if let Some(error) = locked_open_error(dir.path()) {
            assert!(
                !error.contains("reset is incomplete") && !error.contains("thread-history discard"),
                "an Acknowledged marker must not block startup: {error}"
            );
        }

        let retired = format!(
            "{{\"kind\":\"thread_history_discard\",\"operation\":{{\"operation_id\":\"{}\",\"sha256\":\"{}\"}},\"schema\":2}}",
            operation.operation_id, operation.sha256
        );
        std::fs::write(&marker, retired.as_bytes()).unwrap();
        let refused = locked_open_error(dir.path())
            .expect("ordinary startup must refuse a retired-schema discard marker");
        assert!(
            refused.contains(crate::recovery::EXECUTION_HISTORY_SCRATCH_REPAIR),
            "{refused}"
        );
        assert_eq!(std::fs::read(&marker).unwrap(), retired.into_bytes());
    }

    const ACK_ABORT_CHILD: &str = "RYEOS_STATE_ACK_ABORT_CHILD";

    /// Child half of `crash_during_acknowledgement_leaves_only_state_debris_for_the_startup_sweep`.
    /// It does nothing unless that test spawned this process.
    #[test]
    fn acknowledgement_abort_child() {
        let Ok(root) = std::env::var(ACK_ABORT_CHILD) else {
            return;
        };
        lillux::disable_process_core_dumps().unwrap();
        let root = Path::new(&root);
        let store = crate::recovery::RecoveryStore::from_runtime_state_dir(root).unwrap();
        let guard = crate::recovery::CasMutationGuard::acquire_exclusive(root).unwrap();
        lillux::test_hooks::set_atomic_create_hook(
            lillux::test_hooks::AtomicCreateHookSelector {
                purpose: crate::STATE_TEMP_PURPOSE,
                target: "thread-history-discard.json".into(),
                occurrence: 1,
            },
            lillux::test_hooks::AtomicCreateHookAction::Abort,
        );
        let result = store.finish_thread_history_discard(&guard, &discard_operation_for_startup());
        panic!("armed abort hook did not fire during acknowledgement: {result:?}");
    }

    #[test]
    fn crash_during_acknowledgement_leaves_only_state_debris_for_the_startup_sweep() {
        let dir = tempfile::tempdir().unwrap();
        let operation = discard_operation_for_startup();
        let store = crate::recovery::RecoveryStore::from_runtime_state_dir(dir.path()).unwrap();
        crate::recovery::CasMutationGuard::ensure_anchor(dir.path()).unwrap();
        let guard = crate::recovery::CasMutationGuard::acquire_exclusive(dir.path()).unwrap();
        store
            .begin_thread_history_discard(&guard, &operation)
            .unwrap();
        drop(guard);
        let marker = store.root().join("thread-history-discard.json");
        let active = std::fs::read(&marker).unwrap();

        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "state_db::tests::acknowledgement_abort_child",
                "--test-threads=1",
                "--nocapture",
            ])
            .env(ACK_ABORT_CHILD, dir.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        let status = child.wait_with_output().unwrap().status;
        assert!(!status.success() && status.code().is_none(), "{status:?}");

        // The rename never happened: the marker is the exact Active record,
        // and the only other entry is the child's State-owned temporary.
        assert_eq!(std::fs::read(&marker).unwrap(), active);
        assert_eq!(
            store.thread_history_discard_operation().unwrap(),
            Some(operation.clone())
        );
        let mut names: Vec<std::ffi::OsString> = std::fs::read_dir(store.root())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name != "thread-history-discard.json")
            .collect();
        assert_eq!(names.len(), 1, "{names:?}");
        let debris = names.pop().unwrap();
        match lillux::parse_secure_temp_name(&debris) {
            lillux::SecureTempClass::Temp {
                purpose,
                pid: owner,
                seq: _,
            } => {
                assert_eq!(purpose, crate::STATE_TEMP_PURPOSE.as_str());
                assert_eq!(owner, pid);
            }
            other => panic!("unexpected debris {other:?}"),
        }
        let debris = store.root().join(debris);

        // The resumed acknowledgement publishes through a fresh temporary.
        let guard = crate::recovery::CasMutationGuard::acquire_exclusive(dir.path()).unwrap();
        store
            .finish_thread_history_discard(&guard, &operation)
            .unwrap();
        drop(guard);
        assert!(debris.is_file());
        assert_eq!(
            store.last_acknowledged_thread_history_discard().unwrap(),
            Some(operation.clone())
        );
        drop(store);

        if let Some(error) = locked_open_error(dir.path()) {
            assert!(
                !error.contains("reset is incomplete"),
                "an Acknowledged marker must not block startup: {error}"
            );
        }
        assert!(!debris.exists(), "{}", debris.display());
        assert!(marker.is_file());
    }

    const ACK_RENAMED_ABORT_CHILD: &str = "RYEOS_STATE_ACK_RENAMED_ABORT_CHILD";

    /// Child half of `crash_after_acknowledgement_rename_keeps_the_acknowledged_record`.
    /// It acknowledges completely, then dies before its caller can observe
    /// the result. It does nothing unless that test spawned this process.
    #[test]
    fn acknowledgement_renamed_abort_child() {
        let Ok(root) = std::env::var(ACK_RENAMED_ABORT_CHILD) else {
            return;
        };
        lillux::disable_process_core_dumps().unwrap();
        let root = Path::new(&root);
        let store = crate::recovery::RecoveryStore::from_runtime_state_dir(root).unwrap();
        let guard = crate::recovery::CasMutationGuard::acquire_exclusive(root).unwrap();
        store
            .finish_thread_history_discard(&guard, &discard_operation_for_startup())
            .unwrap();
        std::process::abort();
    }

    #[test]
    fn crash_after_acknowledgement_rename_keeps_the_acknowledged_record() {
        let dir = tempfile::tempdir().unwrap();
        let operation = discard_operation_for_startup();
        let store = crate::recovery::RecoveryStore::from_runtime_state_dir(dir.path()).unwrap();
        crate::recovery::CasMutationGuard::ensure_anchor(dir.path()).unwrap();
        let guard = crate::recovery::CasMutationGuard::acquire_exclusive(dir.path()).unwrap();
        store
            .begin_thread_history_discard(&guard, &operation)
            .unwrap();
        drop(guard);
        let marker = store.root().join("thread-history-discard.json");

        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "state_db::tests::acknowledgement_renamed_abort_child",
                "--test-threads=1",
                "--nocapture",
            ])
            .env(ACK_RENAMED_ABORT_CHILD, dir.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let status = child.wait_with_output().unwrap().status;
        assert!(!status.success() && status.code().is_none(), "{status:?}");

        // The rename landed before the crash: the marker is the Acknowledged
        // record of this operation, no longer Active, and no temporary of the
        // write survives beside it.
        let acknowledged = std::fs::read(&marker).unwrap();
        assert_eq!(store.thread_history_discard_operation().unwrap(), None);
        assert!(!store.thread_history_discard_in_progress().unwrap());
        assert_eq!(
            store.last_acknowledged_thread_history_discard().unwrap(),
            Some(operation.clone())
        );
        let names: Vec<std::ffi::OsString> = std::fs::read_dir(store.root())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(
            names,
            vec![std::ffi::OsString::from("thread-history-discard.json")]
        );

        // A retry of the same acknowledgement only syncs; the bytes stay.
        let guard = crate::recovery::CasMutationGuard::acquire_exclusive(dir.path()).unwrap();
        store
            .finish_thread_history_discard(&guard, &operation)
            .unwrap();
        drop(guard);
        assert_eq!(std::fs::read(&marker).unwrap(), acknowledged);
        drop(store);

        if let Some(error) = locked_open_error(dir.path()) {
            assert!(
                !error.contains("reset is incomplete") && !error.contains("thread-history discard"),
                "an Acknowledged marker must not block startup: {error}"
            );
        }
        assert!(marker.is_file());
    }

    #[derive(Default)]
    struct RecordingRepairSink {
        requests: Mutex<Vec<ProjectionRepairRequest>>,
    }

    impl ProjectionRepairSink for RecordingRepairSink {
        fn request_repair(&self, request: ProjectionRepairRequest) {
            self.requests.lock().unwrap().push(request);
        }
    }

    #[test]
    fn projection_failure_signals_repair_even_when_status_is_discarded() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(RecordingRepairSink::default());
        let db =
            TestState::open_with_repair_sink(dir.path(), test_trust_store(), sink.clone()).unwrap();
        let signer = TestSigner::default();
        let snapshot = test_root_snapshot("directive:system/test");
        db.create_chain("T-root", snapshot, &signer).unwrap();
        db.projection()
            .with_sql(|connection| Ok(connection.execute_batch("DROP TABLE events")?))
            .unwrap();
        let event = crate::objects::thread_event::NewEvent::new(
            "T-root",
            "T-root",
            "projection_failure_test",
        )
        .build();

        let committed_head_hash = db
            .append_events("T-root", "T-root", vec![event], vec![], &signer)
            .unwrap()
            .value
            .chain_state_hash;

        let requests = sink.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].chain_root_id, "T-root");
        assert_eq!(requests[0].committed_head_hash, committed_head_hash);
        assert_eq!(requests[0].operation, "append_events");
        assert!(!requests[0].error.is_empty());
    }

    /// Mark the closed State's selected generation as built under another
    /// projection schema epoch, as a schema upgrade leaves it; the next open
    /// rebuilds a fresh baseline instead of opening that generation.
    fn stamp_predecessor_projection_epoch(root: &Path) {
        let recovery = RecoveryStore::from_runtime_state_dir(root).unwrap();
        let mut generation = recovery.read_generation().unwrap().unwrap();
        generation.projection_schema_epoch = ProjectionDb::current_schema_epoch().wrapping_add(1);
        recovery.write_generation(&generation).unwrap();
    }

    #[test]
    fn open_rebuilds_projection_after_schema_epoch_reset() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("state");
        let signer = TestSigner::default();

        let projection_path = {
            let db = TestState::open(&root, test_trust_store()).unwrap();
            let snapshot = test_root_snapshot("directive:system/test");
            db.create_chain("T-root", snapshot, &signer).unwrap();
            db.projection().path().to_path_buf()
        };

        // A selected projection whose own stamp disagrees with its current
        // generation is refused, never silently replaced: its native opening
        // owns partial effects that must settle first.
        with_closed_projection(&projection_path, |connection| {
            Ok(connection.execute_batch("PRAGMA user_version = 0;")?)
        });
        let refusal = {
            let host = crate::test_support::shared_test_host();
            let refusal =
                crate::test_support::live_state_open_refusal(&host, &root, test_trust_store());
            crate::test_support::release_shared_test_host(host);
            refusal
        };
        assert!(
            refusal.contains("selected projection schema epoch mismatch"),
            "{refusal}"
        );

        stamp_predecessor_projection_epoch(&root);
        let db = TestState::open(&root, test_trust_store()).unwrap();
        assert_ne!(db.selected_projection_path(), projection_path);
        let row = db
            .get_thread("T-root")
            .unwrap()
            .expect("thread should be rebuilt from CAS/refs");
        assert_eq!(row.thread_id, "T-root");
        assert_eq!(row.chain_root_id, "T-root");
        assert_eq!(row.kind, "directive");
    }

    #[test]
    fn open_rebuilds_usage_projection_after_schema_epoch_reset() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("state");
        let signer = TestSigner::default();

        let projection_path = {
            let db = TestState::open(&root, test_trust_store()).unwrap();
            let snapshot = test_root_snapshot("directive:apps/example-app/ai_chat");
            db.create_chain("T-root", snapshot, &signer).unwrap();

            let created =
                crate::objects::thread_event::NewEvent::new("T-root", "T-root", "thread_created")
                    .payload(serde_json::json!({
                        "item_ref": "directive:apps/example-app/ai_chat",
                        "executor_ref": "runtime:directive-runtime",
                        "launch_mode": "wait",
                        "usage_subject": {
                            "namespace": "example-app",
                            "subject": "csm01"
                        },
                        "usage_subject_asserted_by": "fp:backend"
                    }))
                    .build();
            let usage =
                crate::objects::thread_event::NewEvent::new("T-root", "T-root", "thread_usage")
                    .payload(serde_json::json!({
                        "completed_turns": 3,
                        "input_tokens": 100,
                        "output_tokens": 50,
                        "spend_usd": "0.15",
                        "spawns_used": 1,
                        "started_at": "2026-06-01T00:00:00Z",
                        "settled_at": "2026-06-01T00:01:00Z",
                        "last_settled_turn_seq": 3,
                        "elapsed_ms": 1000,
                        "provider_id": "anthropic",
                        "model": "claude-test",
                        "profile": "general"
                    }))
                    .build();
            db.append_events("T-root", "T-root", vec![created, usage], vec![], &signer)
                .unwrap();
            db.projection().path().to_path_buf()
        };

        stamp_predecessor_projection_epoch(&root);
        let db = TestState::open(&root, test_trust_store()).unwrap();
        assert_ne!(db.selected_projection_path(), projection_path);
        let usage_rows: i64 = db
            .projection()
            .with_sql(|connection| {
                Ok(connection.query_row(
                    "SELECT COUNT(*) FROM thread_usage_latest WHERE thread_id = 'T-root' \
                 AND input_tokens = 100 AND output_tokens = 50",
                    [],
                    |row| row.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(usage_rows, 1);

        let subject_rows: i64 = db
            .projection()
            .with_sql(|connection| {
                Ok(connection.query_row(
                    "SELECT COUNT(*) FROM thread_usage_subjects WHERE chain_root_id = 'T-root' \
                 AND namespace = 'example-app' AND subject = 'csm01' \
                 AND asserted_by = 'fp:backend'",
                    [],
                    |row| row.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(subject_rows, 1);
    }

    #[test]
    fn create_chain_cas_objects_exist() {
        let (_dir, db) = open_temp();
        let signer = TestSigner::default();

        let snapshot = test_root_snapshot("directive:system/test");

        let result = db.create_chain("T-root", snapshot, &signer).unwrap();

        let snapshot_path = lillux::shard_path(
            db.cas_root(),
            "objects",
            &result.value.snapshot_hash,
            ".json",
        );
        assert!(snapshot_path.exists());

        let chain_state_path = lillux::shard_path(
            db.cas_root(),
            "objects",
            &result.value.chain_state_hash,
            ".json",
        );
        assert!(chain_state_path.exists());
    }

    #[test]
    fn create_chain_writes_signed_ref() {
        let (_dir, db) = open_temp();
        let signer = TestSigner::default();

        let snapshot = test_root_snapshot("directive:system/test");

        db.create_chain("T-root", snapshot, &signer).unwrap();

        let ref_path = db.refs_root().join("generic/chains/T-root/head");
        assert!(ref_path.exists());
    }

    #[test]
    fn add_thread_projects_snapshot() {
        let (_dir, db) = open_temp();
        let signer = TestSigner::default();

        let root_snapshot = test_root_snapshot("directive:system/test");

        db.create_chain("T-root", root_snapshot, &signer).unwrap();

        let child_snapshot = ThreadSnapshotBuilder::new(
            "T-child",
            "T-root",
            "tool",
            "test/child",
            "native:tool-runtime",
        )
        .build();

        db.add_thread("T-root", child_snapshot, &signer).unwrap();

        let row = db
            .get_thread("T-child")
            .unwrap()
            .expect("child thread in projection");
        assert_eq!(row.thread_id, "T-child");
        assert_eq!(row.chain_root_id, "T-root");
        assert_eq!(row.kind, "tool");
    }

    #[test]
    fn list_threads_by_chain_returns_all() {
        let (_dir, db) = open_temp();
        let signer = TestSigner::default();

        let root_snapshot = test_root_snapshot("directive:system/test");

        db.create_chain("T-root", root_snapshot, &signer).unwrap();

        let child = ThreadSnapshotBuilder::new(
            "T-child",
            "T-root",
            "tool",
            "test/child",
            "native:tool-runtime",
        )
        .build();

        db.add_thread("T-root", child, &signer).unwrap();

        let threads = db.list_threads_by_chain("T-root").unwrap();
        assert_eq!(threads.len(), 2);

        let ids: Vec<&str> = threads.iter().map(|t| t.thread_id.as_str()).collect();
        assert!(ids.contains(&"T-root"));
        assert!(ids.contains(&"T-child"));
    }

    #[test]
    fn append_events_projects_events() {
        let (_dir, db) = open_temp();
        let signer = TestSigner::default();

        let snapshot = test_root_snapshot("directive:system/test");

        db.create_chain("T-root", snapshot, &signer).unwrap();

        let event = crate::objects::thread_event::NewEvent::new("T-root", "T-root", "test_event")
            .payload(serde_json::json!({"key": "value"}))
            .build();

        let result = db
            .append_events("T-root", "T-root", vec![event], vec![], &signer)
            .unwrap();

        assert_eq!(result.value.event_count, 1);

        let count: i64 = db
            .projection()
            .with_sql(|connection| {
                Ok(connection.query_row(
                    "SELECT COUNT(*) FROM events WHERE thread_id = 'T-root'",
                    [],
                    |row| row.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn append_events_rejects_ephemeral_events() {
        let (_dir, db) = open_temp();
        let signer = TestSigner::default();

        let snapshot = test_root_snapshot("directive:system/test");

        db.create_chain("T-root", snapshot, &signer).unwrap();

        let event = crate::objects::thread_event::NewEvent::new("T-root", "T-root", "test_event")
            .durability(crate::objects::EventDurability::Ephemeral)
            .payload(serde_json::json!({"key": "value"}))
            .build();

        let err = db
            .append_events("T-root", "T-root", vec![event], vec![], &signer)
            .unwrap_err();

        assert!(
            err.to_string().contains("cannot persist ephemeral events"),
            "got: {err}"
        );
    }

    #[test]
    fn prepared_set_accepts_only_target_or_exact_previous_head() {
        let previous_hash = "1".repeat(64);
        let target_hash = "2".repeat(64);
        let unrelated_hash = "3".repeat(64);
        let transition = PendingChainHeadTransition {
            schema: 1,
            transition_id: "transition-1".into(),
            chain_root_id: "T-root".into(),
            operation: HeadOperation::Set,
            phase: TransitionPhase::Prepared,
            expected_previous_hash: Some(previous_hash.clone()),
            target_hash: Some(target_hash.clone()),
            prepared_at: "2026-07-14T00:00:00Z".into(),
        };
        let previous = SignedRef::new(
            "chains/T-root/head".into(),
            previous_hash,
            "2026-07-14T00:00:00Z".into(),
            "signer".into(),
        );
        let target = SignedRef::new(
            "chains/T-root/head".into(),
            target_hash,
            "2026-07-14T00:00:01Z".into(),
            "signer".into(),
        );
        let unrelated = SignedRef::new(
            "chains/T-root/head".into(),
            unrelated_hash,
            "2026-07-14T00:00:02Z".into(),
            "signer".into(),
        );

        assert!(matches!(
            classify_pending_set_head(&transition, Some(&previous)).unwrap(),
            PendingSetHead::Unpublished(Some(_))
        ));
        assert!(matches!(
            classify_pending_set_head(&transition, Some(&target)).unwrap(),
            PendingSetHead::Published(_)
        ));
        assert!(classify_pending_set_head(&transition, Some(&unrelated)).is_err());

        let mut published = transition;
        published.phase = TransitionPhase::HeadPublished;
        assert!(classify_pending_set_head(&published, Some(&previous)).is_err());
    }

    #[test]
    fn pending_remove_head_classification_is_exactly_three_way() {
        let expected_hash = "1".repeat(64);
        let advanced_hash = "2".repeat(64);
        let transition = PendingChainHeadTransition {
            schema: 1,
            transition_id: "transition-1".into(),
            chain_root_id: "T-root".into(),
            operation: HeadOperation::Remove,
            phase: TransitionPhase::Prepared,
            expected_previous_hash: Some(expected_hash.clone()),
            target_hash: None,
            prepared_at: "2026-07-14T00:00:00Z".into(),
        };
        let expected = SignedRef::new(
            "chains/T-root/head".into(),
            expected_hash,
            "2026-07-14T00:00:00Z".into(),
            "signer".into(),
        );
        let advanced = SignedRef::new(
            "chains/T-root/head".into(),
            advanced_hash.clone(),
            "2026-07-14T00:00:01Z".into(),
            "signer".into(),
        );

        assert_eq!(
            classify_pending_remove_head(&transition, None).unwrap(),
            PendingRemoveHeadState::HeadAbsent
        );
        assert_eq!(
            classify_pending_remove_head(&transition, Some(&expected)).unwrap(),
            PendingRemoveHeadState::ExpectedHeadVisible
        );
        assert_eq!(
            classify_pending_remove_head(&transition, Some(&advanced)).unwrap(),
            PendingRemoveHeadState::AdvancedHeadVisible {
                current_head_hash: advanced_hash,
            }
        );
    }

    #[test]
    fn stale_remove_repair_preserves_advanced_head_and_clears_repair_set() {
        let (_dir, db) = open_temp();
        let signer = TestSigner::default();
        let previous_hash = create_test_root_chain(&db);
        let advanced_hash = db
            .append_events(
                "T-root",
                "T-root",
                vec![
                    crate::objects::thread_event::NewEvent::new(
                        "T-root",
                        "T-root",
                        "advanced_after_remove",
                    )
                    .build(),
                ],
                vec![],
                &signer,
            )
            .unwrap()
            .value
            .chain_state_hash;
        let chain_lock = crate::chain::ChainLock::acquire(db.refs_root(), "T-root").unwrap();
        let remove = db
            .recovery
            .prepare_remove(&chain_lock, "T-root", &previous_hash)
            .unwrap();

        assert_eq!(
            db.pending_chain_removal_head_state("T-root", &remove.transition_id, &chain_lock,)
                .unwrap(),
            PendingRemoveHeadState::AdvancedHeadVisible {
                current_head_hash: advanced_hash.clone(),
            }
        );
        db.repair_advanced_head_after_stale_chain_removal(
            "T-root",
            &remove.transition_id,
            &chain_lock,
        )
        .unwrap();

        assert!(db.recovery.read_pending("T-root").unwrap().is_none());
        assert_eq!(
            db.read_generic_head_ref("chains", "T-root")
                .unwrap()
                .unwrap()
                .target_hash,
            advanced_hash
        );
        assert_eq!(
            db.projection()
                .get_projection_meta("T-root")
                .unwrap()
                .unwrap()
                .indexed_chain_state_hash,
            advanced_hash
        );
    }

    #[test]
    fn admitted_import_advances_only_to_a_descendant_head() {
        // A node holds one State at a time; source and target alternate,
        // each closed before the other opens, and exchange the chain only
        // through their on-disk stores.
        let signer = TestSigner::default();
        let source_dir = tempfile::tempdir().unwrap();
        let target_dir = tempfile::tempdir().unwrap();
        let trust = || {
            let mut trust_store = TrustStore::new();
            trust_store.insert(signer.fingerprint().to_string(), signer.verifying_key());
            Arc::new(trust_store)
        };
        let export = || {
            let source = TestState::open(source_dir.path(), trust()).unwrap();
            let exported = crate::sync::export_chain(
                source.cas_root().parent().unwrap(),
                "T-root",
                source.head_trust_store(),
            )
            .unwrap();
            source.close().unwrap();
            exported
        };
        let import = |exported: &_| {
            let target = TestState::open(target_dir.path(), trust()).unwrap();
            let staged =
                crate::sync::stage_chain_import(target.cas_root().parent().unwrap(), exported)
                    .unwrap();
            let guard = crate::CasMutationGuard::shared_from_cas_root(target.cas_root()).unwrap();
            crate::sync::finalize_import(&target, staged, &signer, &guard).unwrap();
            drop(guard);
            let imported = target
                .read_generic_head_ref("chains", "T-root")
                .unwrap()
                .unwrap();
            target.close().unwrap();
            imported.target_hash
        };

        let source = TestState::open(source_dir.path(), trust()).unwrap();
        source
            .create_chain(
                "T-root",
                test_root_snapshot("directive:system/test"),
                &signer,
            )
            .unwrap();
        source.close().unwrap();
        let first = export();
        import(&first);

        let source = TestState::open(source_dir.path(), trust()).unwrap();
        let event =
            crate::objects::thread_event::NewEvent::new("T-root", "T-root", "import_advance_test")
                .build();
        let advanced = source
            .append_events("T-root", "T-root", vec![event], vec![], &signer)
            .unwrap()
            .value
            .chain_state_hash;
        source.close().unwrap();
        let second = export();
        assert_eq!(import(&second), advanced);
    }

    #[test]
    fn get_thread_returns_none_for_missing() {
        let (_dir, db) = open_temp();
        let row = db.get_thread("nonexistent").unwrap();
        assert!(row.is_none());
    }

    #[test]
    fn list_threads_by_chain_empty() {
        let (_dir, db) = open_temp();
        let threads = db.list_threads_by_chain("no-such-chain").unwrap();
        assert!(threads.is_empty());
    }
}

#[cfg(all(test, feature = "test-support"))]
mod offline_native_tests {
    use super::*;
    use lillux_sqlite::OriginalNativeDestinationAuthority;
    use lillux_sqlite::test_host::TestNativeHost;

    fn runtime(dir: &Path) -> lillux::PinnedDirectory {
        for name in ["objects", "refs", "locators", "recovery/thread-projection"] {
            std::fs::create_dir_all(dir.join(name)).unwrap();
        }
        lillux::PinnedDirectory::open(dir).unwrap().unwrap()
    }

    #[test]
    fn close_offline_native_refuses_a_store_without_offline_exclusion() {
        let host = TestNativeHost::acquire();
        let dir = tempfile::tempdir().unwrap();
        let root = runtime(dir.path());
        let namespace = root.lock_exclusive().unwrap();
        // SAFETY: the test host funds the factory; the test is the sole user
        // of this temporary runtime directory and holds its namespace lock.
        let mut state = unsafe {
            StateDb::open_offline_native(
                root.try_clone().unwrap(),
                &namespace,
                Arc::new(TrustStore::new()),
                host.factory(),
                host.destination(),
            )
        }
        .unwrap();
        let lock = state._offline_namespace_lock.take();
        let (returned, error) = match state.close_offline_native(host.factory()) {
            Ok(_) => panic!("a store without retained namespace exclusion must be refused"),
            Err(refused) => refused,
        };
        assert!(
            format!("{error:#}").contains("not opened offline"),
            "{error:#}"
        );
        let mut state = returned.expect("the refused store is handed back whole");
        state._offline_namespace_lock = lock;
        let census = match state.close_offline_native(host.factory()) {
            Ok(census) => census,
            Err((_, error)) => panic!("offline store close refused: {error:#}"),
        };
        let LiveStateCensus {
            terminals,
            leases,
            lease_retirement,
            prior_refusal,
        } = census;
        assert!(leases.is_empty() && prior_refusal.is_none());
        assert_eq!(lease_retirement.unretired().unwrap(), 0);
        for terminal in terminals {
            host.retain_terminal(terminal);
        }
        let (issued, recorded) = host.ticket_census();
        assert_eq!(
            (issued, recorded),
            (1, 1),
            "the projection's terminal reaches the census"
        );
    }

    #[test]
    fn offline_state_destination_revalidates_the_captured_state_roots() {
        let host = TestNativeHost::acquire();
        let dir = tempfile::tempdir().unwrap();
        let root = runtime(dir.path());
        let namespace = root.lock_exclusive().unwrap();
        let preparation = PreparedOfflineProjection::capture(
            root.try_clone().unwrap(),
            Arc::new(TrustStore::new()),
            &namespace,
        )
        .unwrap();
        let destination = OfflineNativeStateDestination {
            preparation,
            host: host.destination(),
        };
        destination
            .require_original(host.factory())
            .expect("captured State roots admitted");
        std::fs::rename(dir.path().join("refs"), dir.path().join("refs.moved")).unwrap();
        destination
            .require_original(host.factory())
            .expect_err("a replaced State root is refused");
        std::fs::rename(dir.path().join("refs.moved"), dir.path().join("refs")).unwrap();
        destination
            .require_original(host.factory())
            .expect("restored State roots admitted again");
    }
}
