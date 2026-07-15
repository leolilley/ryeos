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
    self, BundleEventAppendRequest, BundleEventAppendResult, BundleEventRecord,
};
use crate::bundle_projection::BundleProjectionDb;
use crate::chain::{self, AddThreadWithEventsResult, AppendResult, CreateResult, SnapshotUpdate};
use crate::head_cache::HeadCache;
use crate::objects::bundle_event::validate_bundle_identifier;
use crate::objects::ThreadEvent;
use crate::objects::ThreadSnapshot;
use crate::operational::{
    AdmissionAttestationRecord, CasEntriesByStateSummary, CasEntryAttribution, CasEntryKind,
    CasEntryState, FinishSyncJobAttempt, NewAdmissionAttestationRecord, NewCasEntryAttribution,
    NewSyncJob, NewSyncJobAttempt, OperationalDb, SyncJobAttemptRecord, SyncJobRecord,
    SyncJobState, SyncJobUpdate,
};
use crate::projection::{
    self, project_committed_chain, ChainRetentionProjection, DueTerminalChain,
    DueTerminalChainCursor, ProjectionDb,
};
use crate::queries;
use crate::recovery::{
    HeadOperation, PendingChainHeadTransition, ProjectionRecoveryGeneration, RecoveryStore,
    TransitionPhase,
};
use crate::refs::{GenericHeadRef, SignedRef, TrustStore};
use crate::signer::Signer;

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
    let generation = match recovery.read_generation() {
        Ok(generation) => generation,
        Err(error) => {
            tracing::warn!(error = %error, "invalid projection recovery generation; rebuilding baseline");
            None
        }
    };

    if let Some(generation) = generation {
        if generation.projection_schema_epoch == expected_epoch {
            let projection_path = runtime_state_dir.join(&generation.projection_file);
            match ProjectionDb::open_selected_current_in_directory(
                recovery.runtime_directory(),
                std::ffi::OsStr::new(&generation.projection_file),
                false,
            ) {
                Ok(db)
                    if db.projection_instance_id()?.as_deref()
                        == Some(generation.projection_instance_id.as_str()) =>
                {
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
                    let deleted =
                        cleanup_superseded_projection_instances(recovery, &projection_path)?;
                    if deleted != 0 {
                        tracing::info!(deleted, "removed superseded projection instances");
                    }
                    return Ok(db);
                }
                Ok(_) => tracing::warn!(
                    expected_instance = %generation.projection_instance_id,
                    "projection instance does not match recovery generation; rebuilding baseline"
                ),
                Err(error) => tracing::warn!(
                    error = %error,
                    "selected projection is not current; rebuilding generation baseline"
                ),
            }
        }
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
    )
    .map(|(projection, _)| projection)
}

fn rebuild_and_install_projection(
    runtime_state_dir: &Path,
    cas_root: &Path,
    refs_root: &Path,
    recovery: &RecoveryStore,
    trust_store: &TrustStore,
    runtime_liveness: Option<&dyn RuntimeLivenessInspector>,
    observer: Option<&dyn ProjectionRecoveryObserver>,
    cas_mutation_guard: Option<&crate::recovery::CasMutationGuard>,
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

    let instance_id = recovery.new_projection_instance_id();
    let projection_file = format!("projection.{instance_id}.sqlite3");
    let instance_path = runtime_state_dir.join(&projection_file);
    let runtime_directory = recovery.runtime_directory();
    remove_projection_sidecars(runtime_directory, &projection_file)?;
    if let Some(colliding) =
        runtime_directory.open_regular(std::ffi::OsStr::new(&projection_file), false)?
    {
        runtime_directory
            .remove_if_same(std::ffi::OsStr::new(&projection_file), &colliding)
            .context("remove colliding projection instance")?;
    }

    let staged = ProjectionDb::create_in_directory(
        runtime_directory,
        std::ffi::OsStr::new(&projection_file),
    )
    .context("open staged projection instance")?;
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
    )
    .context("verify staged projection baseline before publication")?;
    staged.close_durable()?;
    remove_projection_sidecars(runtime_directory, &projection_file)?;
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
    recovery.write_generation(&generation)?;

    let projection = ProjectionDb::open_selected_current_in_directory(
        runtime_directory,
        std::ffi::OsStr::new(&generation.projection_file),
        false,
    )
    .context("open selected projection instance")?;
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
    let deleted = cleanup_superseded_projection_instances(recovery, &instance_path)?;
    if deleted != 0 {
        tracing::info!(deleted, "removed superseded projection instances");
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
}

const SUPERSEDED_PROJECTION_INSTANCES_TO_KEEP: usize = 2;
const SUPERSEDED_PROJECTION_DELETIONS_PER_PASS: usize = 4;
const SUPERSEDED_PROJECTION_INSPECTIONS_PER_PASS: usize = 16;
const SUPERSEDED_PROJECTION_DIRECTORY_ENTRIES_PER_PASS: usize = 64;

fn cleanup_superseded_projection_instances(
    recovery: &RecoveryStore,
    selected_path: &Path,
) -> anyhow::Result<usize> {
    let selected_name = selected_path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("selected projection path has no filename"))?;
    let generation_file = recovery
        .read_generation()?
        .map(|generation| generation.projection_file);
    let runtime_directory = recovery.runtime_directory().try_clone()?;
    let mut candidates = Vec::new();
    for entry_name in
        runtime_directory.entry_names_bounded(SUPERSEDED_PROJECTION_DIRECTORY_ENTRIES_PER_PASS)?
    {
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
        let candidate_file = runtime_directory
            .open_regular(&entry_name, false)?
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
        if candidates.len() >= SUPERSEDED_PROJECTION_INSPECTIONS_PER_PASS {
            break;
        }
    }
    candidates.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| right.1.cmp(&left.1)));

    let mut deleted = 0usize;
    for (_, name, entry_name, instance_file) in candidates
        .into_iter()
        .skip(SUPERSEDED_PROJECTION_INSTANCES_TO_KEEP)
    {
        if deleted == SUPERSEDED_PROJECTION_DELETIONS_PER_PASS {
            break;
        }
        let Some((lease_name, instance_lease)) =
            try_acquire_projection_cleanup_lease(&runtime_directory, &name)?
        else {
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

fn remove_projection_sidecars(
    directory: &lillux::PinnedDirectory,
    projection_name: &str,
) -> anyhow::Result<()> {
    let sidecars = [
        std::ffi::OsString::from(format!("{projection_name}-wal")),
        std::ffi::OsString::from(format!("{projection_name}-shm")),
        std::ffi::OsString::from(format!("{projection_name}-journal")),
    ];
    let mut existing = Vec::new();
    for name in sidecars {
        if let Some(file) = directory.open_regular(&name, false)? {
            let sidecar = directory.path().join(&name);
            existing.push((sidecar, file));
        }
    }
    // Preflight the complete set before unlinking any member, so one invalid
    // sidecar cannot leave a partially cleaned generation.
    for (sidecar, file) in &existing {
        let name = sidecar
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("projection sidecar has no filename"))?;
        let current = directory
            .open_regular(name, false)?
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
                    if acknowledge {
                        if !acknowledge_if_projection_current(
                            projection,
                            recovery,
                            &chain_lock,
                            &current,
                            Some(head),
                        )? {
                            anyhow::bail!(
                                "pending Set did not converge while replaying {}",
                                current.chain_root_id
                            );
                        }
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
                    if acknowledge {
                        if !acknowledge_if_projection_current(
                            projection,
                            recovery,
                            &chain_lock,
                            &replacement,
                            Some(&head),
                        )? {
                            anyhow::bail!(
                                "advanced-head repair Set did not converge while replaying {}",
                                replacement.chain_root_id
                            );
                        }
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
    if let Some(head) = head {
        if head.target_hash == target {
            return Ok(PendingSetHead::Published(head));
        }
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
    _cas_directory: lillux::PinnedDirectory,
    _refs_directory: lillux::PinnedDirectory,
    _locators_directory: lillux::PinnedDirectory,
    _recovery_directory: lillux::PinnedDirectory,
    cas_root: PathBuf,
    refs_root: PathBuf,
    locators_root: PathBuf,
    projection: ProjectionDb,
    operational: OperationalAccess,
    recovery: RecoveryStore,
    trust_store: Arc<TrustStore>,
    head_cache: Mutex<HeadCache>,
    projection_repair_sink: Arc<dyn ProjectionRepairSink>,
}

/// Cloneable, descriptor-bound authority for maintenance work that must run
/// after releasing the [`StateDb`] mutex.
///
/// Paths are retained only for diagnostics. Every authoritative operation is
/// rooted in these already-open directory descriptors and the exact trust
/// snapshot owned by the originating state handle.
pub struct PinnedStateAuthority {
    runtime_directory: lillux::PinnedDirectory,
    cas_directory: lillux::PinnedDirectory,
    refs_directory: lillux::PinnedDirectory,
    recovery: RecoveryStore,
    trust_store: Arc<TrustStore>,
}

impl PinnedStateAuthority {
    pub(crate) fn open_existing(
        runtime_state_dir: &Path,
        trust_store: Arc<TrustStore>,
        create_recovery: bool,
    ) -> anyhow::Result<Self> {
        let runtime_directory = lillux::PinnedDirectory::open(runtime_state_dir)?
            .ok_or_else(|| anyhow::anyhow!("runtime-state directory is absent"))?;
        let cas_directory = runtime_directory
            .open_child_directory(std::ffi::OsStr::new("objects"))?
            .ok_or_else(|| anyhow::anyhow!("CAS root is absent"))?;
        let refs_directory = runtime_directory
            .open_child_directory(std::ffi::OsStr::new("refs"))?
            .ok_or_else(|| anyhow::anyhow!("refs root is absent"))?;
        let recovery_parent = if create_recovery {
            runtime_directory.open_or_create_child(std::ffi::OsStr::new("recovery"), 0o700)?
        } else {
            runtime_directory
                .open_child_directory(std::ffi::OsStr::new("recovery"))?
                .ok_or_else(|| anyhow::anyhow!("recovery directory is absent"))?
        };
        let recovery_directory = if create_recovery {
            recovery_parent
                .open_or_create_child(std::ffi::OsStr::new("thread-projection"), 0o700)?
        } else {
            recovery_parent
                .open_child_directory(std::ffi::OsStr::new("thread-projection"))?
                .ok_or_else(|| anyhow::anyhow!("thread-projection recovery directory is absent"))?
        };
        let recovery = RecoveryStore::from_pinned_directories(
            runtime_directory.try_clone()?,
            recovery_directory,
        )?;
        Ok(Self {
            runtime_directory,
            cas_directory,
            refs_directory,
            recovery,
            trust_store,
        })
    }

    pub fn runtime_directory(&self) -> &lillux::PinnedDirectory {
        &self.runtime_directory
    }

    pub fn cas_store(&self) -> anyhow::Result<lillux::CasStore> {
        Ok(lillux::CasStore::from_pinned_root(
            self.cas_directory.try_clone()?,
        ))
    }

    pub fn cas_directory(&self) -> &lillux::PinnedDirectory {
        &self.cas_directory
    }

    pub fn refs_directory(&self) -> &lillux::PinnedDirectory {
        &self.refs_directory
    }

    pub fn recovery(&self) -> &RecoveryStore {
        &self.recovery
    }

    pub fn trust_store(&self) -> &TrustStore {
        self.trust_store.as_ref()
    }

    pub fn ensure_guard(&self, guard: &crate::recovery::CasMutationGuard) -> anyhow::Result<()> {
        guard.ensure_protects_pinned_runtime(&self.runtime_directory)
    }
}

enum OperationalAccess {
    Available(OperationalDb),
    ProjectionMaintenance,
}

impl StateDb {
    /// Open (or create) a state database rooted at `runtime_state_dir`.
    ///
    /// Creates `objects/`, `refs/`, and `locators/` subdirectories and opens
    /// the generation-selected projection instance inside
    /// `runtime_state_dir`.
    pub fn open(runtime_state_dir: &Path, trust_store: Arc<TrustStore>) -> anyhow::Result<Self> {
        Self::open_with_options(
            runtime_state_dir,
            Arc::new(PendingProjectionRepairs::default()),
            trust_store,
            None,
            None,
        )
    }

    /// Open a state database with a caller-provided projection repair sink.
    pub fn open_with_projection_repair_sink(
        runtime_state_dir: &Path,
        projection_repair_sink: Arc<dyn ProjectionRepairSink>,
        trust_store: Arc<TrustStore>,
    ) -> anyhow::Result<Self> {
        Self::open_with_options(
            runtime_state_dir,
            projection_repair_sink,
            trust_store,
            None,
            None,
        )
    }

    pub fn open_with_recovery_observer(
        runtime_state_dir: &Path,
        projection_repair_sink: Arc<dyn ProjectionRepairSink>,
        trust_store: Arc<TrustStore>,
        observer: Arc<dyn ProjectionRecoveryObserver>,
    ) -> anyhow::Result<Self> {
        Self::open_with_options(
            runtime_state_dir,
            projection_repair_sink,
            trust_store,
            Some(observer),
            None,
        )
    }

    pub fn open_with_projection_repair_sink_and_runtime_liveness(
        runtime_state_dir: &Path,
        projection_repair_sink: Arc<dyn ProjectionRepairSink>,
        trust_store: Arc<TrustStore>,
        runtime_liveness: &dyn RuntimeLivenessInspector,
    ) -> anyhow::Result<Self> {
        Self::open_with_options(
            runtime_state_dir,
            projection_repair_sink,
            trust_store,
            None,
            Some(runtime_liveness),
        )
    }

    pub fn open_with_recovery_observer_and_runtime_liveness(
        runtime_state_dir: &Path,
        projection_repair_sink: Arc<dyn ProjectionRepairSink>,
        trust_store: Arc<TrustStore>,
        observer: Arc<dyn ProjectionRecoveryObserver>,
        runtime_liveness: &dyn RuntimeLivenessInspector,
    ) -> anyhow::Result<Self> {
        Self::open_with_options(
            runtime_state_dir,
            projection_repair_sink,
            trust_store,
            Some(observer),
            Some(runtime_liveness),
        )
    }

    /// Open exactly the selected generation for fail-only verification. This
    /// path creates no directories, databases, leases, schemas, sidecars, or
    /// replacement generation and does not replay/acknowledge journal records.
    pub fn open_for_projection_verification(
        runtime_state_dir: &Path,
        trust_store: Arc<TrustStore>,
    ) -> anyhow::Result<Self> {
        let runtime_state_directory = lillux::PinnedDirectory::open(runtime_state_dir)?
            .ok_or_else(|| anyhow::anyhow!("runtime state directory is absent"))?;
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
        let generation = recovery
            .read_generation()?
            .ok_or_else(|| anyhow::anyhow!("projection recovery generation is absent"))?;
        if generation.projection_schema_epoch != ProjectionDb::current_schema_epoch() {
            anyhow::bail!(
                "projection generation schema epoch mismatch: selected={}, current={}",
                generation.projection_schema_epoch,
                ProjectionDb::current_schema_epoch()
            );
        }
        let projection = ProjectionDb::open_selected_current_in_directory(
            &runtime_state_directory,
            std::ffi::OsStr::new(&generation.projection_file),
            true,
        )?;
        if projection.projection_instance_id()?.as_deref()
            != Some(generation.projection_instance_id.as_str())
        {
            anyhow::bail!("selected projection instance identity does not match generation");
        }

        Ok(Self {
            _runtime_state_directory: runtime_state_directory,
            _cas_directory: cas_directory,
            _refs_directory: refs_directory,
            _locators_directory: locators_directory,
            _recovery_directory: recovery_directory,
            cas_root,
            refs_root,
            locators_root,
            projection,
            operational: OperationalAccess::ProjectionMaintenance,
            recovery,
            trust_store,
            head_cache: Mutex::new(HeadCache::new()),
            projection_repair_sink: Arc::new(PendingProjectionRepairs::default()),
        })
    }

    /// Open established authority/recovery roots for an explicit offline
    /// rebuild without selecting, replaying, validating, or replacing the
    /// current projection as a side effect of opening the service mode.
    pub fn open_for_projection_rebuild(
        runtime_state_dir: &Path,
        trust_store: Arc<TrustStore>,
    ) -> anyhow::Result<Self> {
        let runtime_state_directory = lillux::PinnedDirectory::open(runtime_state_dir)?
            .ok_or_else(|| anyhow::anyhow!("runtime state directory is absent"))?;
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

        Ok(Self {
            _runtime_state_directory: runtime_state_directory,
            _cas_directory: cas_directory,
            _refs_directory: refs_directory,
            _locators_directory: locators_directory,
            _recovery_directory: recovery_directory,
            cas_root,
            refs_root,
            locators_root,
            projection: ProjectionDb::open_transient()?,
            operational: OperationalAccess::ProjectionMaintenance,
            recovery,
            trust_store,
            head_cache: Mutex::new(HeadCache::new()),
            projection_repair_sink: Arc::new(PendingProjectionRepairs::default()),
        })
    }

    fn open_with_options(
        runtime_state_dir: &Path,
        projection_repair_sink: Arc<dyn ProjectionRepairSink>,
        trust_store: Arc<TrustStore>,
        observer: Option<Arc<dyn ProjectionRecoveryObserver>>,
        runtime_liveness: Option<&dyn RuntimeLivenessInspector>,
    ) -> anyhow::Result<Self> {
        let runtime_state_directory = lillux::PinnedDirectory::open_or_create(runtime_state_dir)
            .context("establish no-follow runtime state root")?;
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

        // Operational rows are source-of-truth state and live in a fixed store
        // that is independent of replaceable projection generations.
        let operational = OperationalDb::open_at_pinned_runtime_state_dir(&runtime_state_directory)
            .context("open stable operational state")?;

        let projection = open_recovered_projection(
            runtime_state_dir,
            &cas_root,
            &refs_root,
            &recovery,
            trust_store.as_ref(),
            runtime_liveness,
            observer.as_deref(),
        )?;
        Ok(Self {
            _runtime_state_directory: runtime_state_directory,
            _cas_directory: cas_directory,
            _refs_directory: refs_directory,
            _locators_directory: locators_directory,
            _recovery_directory: recovery_directory,
            cas_root,
            refs_root,
            locators_root,
            projection,
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
                    return Ok(())
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

    /// Exact generation-selected projection instance.
    pub fn selected_projection_path(&self) -> &Path {
        self.projection.path()
    }

    /// Strictly verify the generation pointer, signed authoritative heads,
    /// complete CAS closures, and projection cursors without rebuilding or
    /// otherwise mutating state. Callers must hold CAS/head mutation exclusion
    /// across this full scan.
    pub fn verify_projection_generation(
        &self,
    ) -> anyhow::Result<crate::rebuild::ProjectionVerificationReport> {
        let generation = self
            .recovery
            .read_generation()?
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
        )
    }

    /// Build and publish a fresh projection generation, then swap
    /// this live StateDb handle to the selected instance. Callers must keep the
    /// application offline/not-ready for the entire operation.
    pub fn rebuild_projection_generation(
        &mut self,
        runtime_liveness: Option<&dyn RuntimeLivenessInspector>,
    ) -> anyhow::Result<crate::rebuild::RebuildReport> {
        self.rebuild_projection_generation_inner(None, runtime_liveness, None)
    }

    pub fn rebuild_projection_generation_with_observer(
        &mut self,
        observer: Option<&dyn ProjectionRecoveryObserver>,
        runtime_liveness: Option<&dyn RuntimeLivenessInspector>,
    ) -> anyhow::Result<crate::rebuild::RebuildReport> {
        self.rebuild_projection_generation_inner(observer, runtime_liveness, None)
    }

    /// Rebuild while consuming an exclusive CAS mutation guard already held by
    /// the application. This preserves the global guard-before-store-lock
    /// hierarchy for the offline rebuild service.
    pub fn rebuild_projection_generation_admitted(
        &mut self,
        runtime_liveness: Option<&dyn RuntimeLivenessInspector>,
        cas_mutation_guard: &crate::recovery::CasMutationGuard,
    ) -> anyhow::Result<crate::rebuild::RebuildReport> {
        self.rebuild_projection_generation_inner(None, runtime_liveness, Some(cas_mutation_guard))
    }

    fn rebuild_projection_generation_inner(
        &mut self,
        observer: Option<&dyn ProjectionRecoveryObserver>,
        runtime_liveness: Option<&dyn RuntimeLivenessInspector>,
        cas_mutation_guard: Option<&crate::recovery::CasMutationGuard>,
    ) -> anyhow::Result<crate::rebuild::RebuildReport> {
        let runtime_state_dir = self
            .cas_root
            .parent()
            .ok_or_else(|| anyhow::anyhow!("CAS root has no runtime-state parent"))?
            .to_path_buf();
        let (new_projection, report) = rebuild_and_install_projection(
            &runtime_state_dir,
            &self.cas_root,
            &self.refs_root,
            &self.recovery,
            self.trust_store.as_ref(),
            runtime_liveness,
            observer,
            cas_mutation_guard,
        )?;
        let old_projection = std::mem::replace(&mut self.projection, new_projection);
        drop(old_projection);
        let deleted =
            cleanup_superseded_projection_instances(&self.recovery, self.projection.path())?;
        if deleted != 0 {
            tracing::info!(deleted, "removed superseded projection instances");
        }
        Ok(report)
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
        if let Some(pending) = pending {
            if !acknowledge_if_projection_current(
                &self.projection,
                &self.recovery,
                &chain_lock,
                &pending,
                Some(&head),
            )? {
                anyhow::bail!("repaired projection did not converge for {chain_root_id}");
            }
        }
        Ok(report)
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

    /// Refs root (`runtime_state_dir/refs`).
    pub fn refs_root(&self) -> &Path {
        &self.refs_root
    }

    /// Retain this store's exact runtime/CAS/refs/trust authority for a
    /// long-running worker without retaining the StateDb mutex.
    pub fn pinned_authority(&self) -> anyhow::Result<PinnedStateAuthority> {
        Ok(PinnedStateAuthority {
            runtime_directory: self._runtime_state_directory.try_clone()?,
            cas_directory: self._cas_directory.try_clone()?,
            refs_directory: self._refs_directory.try_clone()?,
            recovery: self.recovery.clone(),
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
    pub fn create_chain(
        &self,
        chain_root_id: &str,
        snapshot: ThreadSnapshot,
        signer: &dyn Signer,
    ) -> anyhow::Result<CommittedWrite<CreateResult>> {
        self.create_chain_inner(chain_root_id, snapshot, signer, None, None)
    }

    pub fn create_chain_with_runtime_liveness(
        &self,
        chain_root_id: &str,
        snapshot: ThreadSnapshot,
        signer: &dyn Signer,
        runtime_liveness: &dyn RuntimeLivenessInspector,
    ) -> anyhow::Result<CommittedWrite<CreateResult>> {
        self.create_chain_inner(
            chain_root_id,
            snapshot,
            signer,
            Some(runtime_liveness),
            None,
        )
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
            Some(cas_mutation_guard),
        )
    }

    fn create_chain_inner(
        &self,
        chain_root_id: &str,
        snapshot: ThreadSnapshot,
        signer: &dyn Signer,
        runtime_liveness: Option<&dyn RuntimeLivenessInspector>,
        cas_mutation_guard: Option<&crate::recovery::CasMutationGuard>,
    ) -> anyhow::Result<CommittedWrite<CreateResult>> {
        let _owned_cas_mutation_guard = match cas_mutation_guard {
            Some(guard) => {
                guard.ensure_protects_cas_root(&self.cas_root)?;
                None
            }
            None => Some(crate::recovery::CasMutationGuard::shared_from_cas_root(
                &self.cas_root,
            )?),
        };
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

    /// Add a new thread to an existing chain.
    ///
    /// Delegates to [`chain::add_thread_to_chain`] then projects the snapshot.
    /// Same CAS-first contract as [`Self::create_chain`].
    pub fn add_thread(
        &self,
        chain_root_id: &str,
        snapshot: ThreadSnapshot,
        signer: &dyn Signer,
    ) -> anyhow::Result<CommittedWrite<CreateResult>> {
        self.add_thread_inner(chain_root_id, snapshot, signer, None, None)
    }

    pub fn add_thread_with_runtime_liveness(
        &self,
        chain_root_id: &str,
        snapshot: ThreadSnapshot,
        signer: &dyn Signer,
        runtime_liveness: &dyn RuntimeLivenessInspector,
    ) -> anyhow::Result<CommittedWrite<CreateResult>> {
        self.add_thread_inner(
            chain_root_id,
            snapshot,
            signer,
            Some(runtime_liveness),
            None,
        )
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
            Some(cas_mutation_guard),
        )
    }

    fn add_thread_inner(
        &self,
        chain_root_id: &str,
        snapshot: ThreadSnapshot,
        signer: &dyn Signer,
        runtime_liveness: Option<&dyn RuntimeLivenessInspector>,
        cas_mutation_guard: Option<&crate::recovery::CasMutationGuard>,
    ) -> anyhow::Result<CommittedWrite<CreateResult>> {
        let _owned_cas_mutation_guard = match cas_mutation_guard {
            Some(guard) => {
                guard.ensure_protects_cas_root(&self.cas_root)?;
                None
            }
            None => Some(crate::recovery::CasMutationGuard::shared_from_cas_root(
                &self.cas_root,
            )?),
        };
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
    pub fn append_events(
        &self,
        chain_root_id: &str,
        thread_id: &str,
        events: Vec<ThreadEvent>,
        snapshot_updates: Vec<SnapshotUpdate>,
        signer: &dyn Signer,
    ) -> anyhow::Result<CommittedWrite<AppendResult>> {
        self.append_events_inner(
            chain_root_id,
            thread_id,
            events,
            snapshot_updates,
            signer,
            None,
            None,
        )
    }

    pub fn append_events_with_runtime_liveness(
        &self,
        chain_root_id: &str,
        thread_id: &str,
        events: Vec<ThreadEvent>,
        snapshot_updates: Vec<SnapshotUpdate>,
        signer: &dyn Signer,
        runtime_liveness: &dyn RuntimeLivenessInspector,
    ) -> anyhow::Result<CommittedWrite<AppendResult>> {
        self.append_events_inner(
            chain_root_id,
            thread_id,
            events,
            snapshot_updates,
            signer,
            Some(runtime_liveness),
            None,
        )
    }

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
        self.append_events_inner(
            chain_root_id,
            thread_id,
            events,
            snapshot_updates,
            signer,
            Some(runtime_liveness),
            Some(cas_mutation_guard),
        )
    }

    fn append_events_inner(
        &self,
        chain_root_id: &str,
        thread_id: &str,
        events: Vec<ThreadEvent>,
        snapshot_updates: Vec<SnapshotUpdate>,
        signer: &dyn Signer,
        runtime_liveness: Option<&dyn RuntimeLivenessInspector>,
        cas_mutation_guard: Option<&crate::recovery::CasMutationGuard>,
    ) -> anyhow::Result<CommittedWrite<AppendResult>> {
        if events.iter().any(|event| !event.durability.is_cas_stored()) {
            anyhow::bail!("StateDb::append_events cannot persist ephemeral events");
        }

        let _owned_cas_mutation_guard = match cas_mutation_guard {
            Some(guard) => {
                guard.ensure_protects_cas_root(&self.cas_root)?;
                None
            }
            None => Some(crate::recovery::CasMutationGuard::shared_from_cas_root(
                &self.cas_root,
            )?),
        };
        let chain_lock = crate::chain::ChainLock::acquire_in_refs_directory(
            &self._refs_directory,
            &self._cas_directory,
            &self.recovery,
            chain_root_id,
        )?;
        let invalidates_retirement = self.pending_remove_invalidated_by_mutation_locked(
            chain_root_id,
            ProspectiveChainMutation::UpdateSnapshots(&snapshot_updates),
            &chain_lock,
        )?;
        let cancelled_remove = self.converge_pending_before_set_locked(
            chain_root_id,
            invalidates_retirement,
            runtime_liveness,
            &chain_lock,
        )?;
        let mut cache = self.head_cache.lock().expect("head_cache lock");
        let result = match chain::append_events_with_trust_under_lock(
            chain::AppendEventsInput {
                cas_root: &self.cas_root,
                refs_root: &self.refs_root,
                chain_root_id,
                thread_id,
                events,
                snapshot_updates: snapshot_updates.clone(),
                signer,
            },
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
                self.note_pending_transition_error(chain_root_id, "append_events", &error);
                return Err(error);
            }
        };
        if let Err(error) = self.mark_pending_set_applying_locked(
            chain_root_id,
            &result.chain_state_hash,
            &chain_lock,
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
        Ok(self.committed_write_locked(
            result,
            chain_root_id,
            &committed_hash,
            "append_events",
            projected,
            &chain_lock,
        ))
    }

    /// Add a new thread and its initial durable events in one chain-head update.
    ///
    /// This is used for relation-bearing child creation where a thread must not
    /// become durable without the events that define what kind of child it is.
    pub fn add_thread_with_events(
        &self,
        chain_root_id: &str,
        snapshot: ThreadSnapshot,
        events: Vec<ThreadEvent>,
        signer: &dyn Signer,
    ) -> anyhow::Result<CommittedWrite<AddThreadWithEventsResult>> {
        self.add_thread_with_events_inner(chain_root_id, snapshot, events, signer, None, None)
    }

    pub fn add_thread_with_events_and_runtime_liveness(
        &self,
        chain_root_id: &str,
        snapshot: ThreadSnapshot,
        events: Vec<ThreadEvent>,
        signer: &dyn Signer,
        runtime_liveness: &dyn RuntimeLivenessInspector,
    ) -> anyhow::Result<CommittedWrite<AddThreadWithEventsResult>> {
        self.add_thread_with_events_inner(
            chain_root_id,
            snapshot,
            events,
            signer,
            Some(runtime_liveness),
            None,
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
            events,
            signer,
            Some(runtime_liveness),
            Some(cas_mutation_guard),
        )
    }

    fn add_thread_with_events_inner(
        &self,
        chain_root_id: &str,
        snapshot: ThreadSnapshot,
        events: Vec<ThreadEvent>,
        signer: &dyn Signer,
        runtime_liveness: Option<&dyn RuntimeLivenessInspector>,
        cas_mutation_guard: Option<&crate::recovery::CasMutationGuard>,
    ) -> anyhow::Result<CommittedWrite<AddThreadWithEventsResult>> {
        if events.iter().any(|event| !event.durability.is_cas_stored()) {
            anyhow::bail!("StateDb::add_thread_with_events cannot persist ephemeral events");
        }

        let _owned_cas_mutation_guard = match cas_mutation_guard {
            Some(guard) => {
                guard.ensure_protects_cas_root(&self.cas_root)?;
                None
            }
            None => Some(crate::recovery::CasMutationGuard::shared_from_cas_root(
                &self.cas_root,
            )?),
        };
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
    ) -> anyhow::Result<()> {
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
    ) -> anyhow::Result<()> {
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
    ) -> anyhow::Result<()> {
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
    ) -> anyhow::Result<()> {
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
    ) -> anyhow::Result<()> {
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
    ) -> anyhow::Result<()> {
        self.publish_external_chain_head(chain_root_id, target_hash, signer, cas_mutation_guard)
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
    ) -> anyhow::Result<()> {
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
        // The exact trusted current target is the import anchor. An equal
        // target is idempotent; a different target must prove that current is
        // an ancestor, so import can advance but never fork or roll back a
        // locally published chain.
        crate::rebuild::verify_repair_closure_anchored_with_cas(
            &cas,
            chain_root_id,
            target_hash,
            true,
            current_hash,
        )?;
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
            if let Err(error) = crate::rebuild::repair_one_chain_verified_with_cas_and_observer(
                &self.projection,
                &cas,
                chain_root_id,
                target_hash,
                None,
            ) {
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
        if pending.phase == TransitionPhase::Prepared {
            if let Err(error) = self.recovery.advance_phase(
                &chain_lock,
                chain_root_id,
                &pending.transition_id,
                TransitionPhase::Prepared,
                TransitionPhase::HeadPublished,
            ) {
                self.note_pending_transition_error(
                    chain_root_id,
                    "publish_external_chain_head",
                    &error,
                );
                return Err(error);
            }
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
        let projected = crate::rebuild::repair_one_chain_verified_with_cas_and_observer(
            &self.projection,
            &cas,
            chain_root_id,
            target_hash,
            None,
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
        bundle_events::append_bundle_event_admitted_pinned(
            &cas,
            &self._refs_directory,
            request,
            signer,
            self.trust_store.as_ref(),
        )
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

    pub fn open_bundle_projection(
        &self,
        projection_name: &str,
    ) -> anyhow::Result<BundleProjectionDb> {
        validate_bundle_identifier("projection_name", projection_name)?;
        let projection_directory = self
            ._runtime_state_directory
            .open_or_create_child(std::ffi::OsStr::new("bundle-projections"), 0o777)
            .context("creating bundle projection root")?;
        let filename = format!("{projection_name}.sqlite3");
        BundleProjectionDb::open_in_directory(projection_directory, std::ffi::OsStr::new(&filename))
    }

    // ── Query helpers ──────────────────────────────────────────────

    pub fn get_thread(&self, thread_id: &str) -> anyhow::Result<Option<queries::ThreadRow>> {
        queries::get_thread(&self.projection, thread_id)
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

    pub fn update_sync_job(&self, job_id: &str, update: &SyncJobUpdate) -> anyhow::Result<()> {
        self.operational()?.update_sync_job(job_id, update)
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

    fn operational(&self) -> anyhow::Result<&OperationalDb> {
        match &self.operational {
            OperationalAccess::Available(db) => Ok(db),
            OperationalAccess::ProjectionMaintenance => {
                anyhow::bail!("operational state is unavailable in projection maintenance mode")
            }
        }
    }
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

    fn test_trust_store() -> Arc<TrustStore> {
        let signer = TestSigner::default();
        let mut trust_store = TrustStore::new();
        trust_store.insert(signer.fingerprint().to_string(), signer.verifying_key());
        Arc::new(trust_store)
    }

    fn open_temp() -> (tempfile::TempDir, StateDb) {
        let dir = tempfile::tempdir().unwrap();
        let db = StateDb::open(dir.path(), test_trust_store()).unwrap();
        (dir, db)
    }

    fn open_temp_trusted(signer: &TestSigner) -> (tempfile::TempDir, StateDb) {
        let dir = tempfile::tempdir().unwrap();
        let mut trust_store = TrustStore::new();
        trust_store.insert(signer.fingerprint().to_string(), signer.verifying_key());
        let db = StateDb::open(dir.path(), Arc::new(trust_store)).unwrap();
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
                node_policy: CapturedNodeHistoryPolicyProvenance::MissingConfig,
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

    struct AlwaysCancelledRecovery;

    impl ProjectionRecoveryObserver for AlwaysCancelledRecovery {
        fn update(&self, _progress: ProjectionRecoveryProgress) {}

        fn is_cancelled(&self) -> bool {
            true
        }
    }

    #[test]
    fn open_creates_directories() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("state");
        let db = StateDb::open(&root, test_trust_store()).unwrap();

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
            let db = StateDb::open(&root, test_trust_store()).unwrap();
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

        let reopened = StateDb::open(&root, test_trust_store()).unwrap();

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

        let reopened = StateDb::open(&root, test_trust_store()).unwrap();

        assert_eq!(reopened.selected_projection_path(), selected_path);
        assert_eq!(
            count_orphans(),
            orphan_count - (SUPERSEDED_PROJECTION_DELETIONS_PER_PASS * 2),
            "repeated normal opens must make bounded progress"
        );
    }

    #[test]
    fn valid_current_generation_open_does_not_enumerate_signed_heads() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("state");
        let selected_path = {
            let db = StateDb::open(&root, test_trust_store()).unwrap();
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

        let reopened = StateDb::open(&root, test_trust_store()).unwrap();
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
                let db = StateDb::open(&root, test_trust_store()).unwrap();
                create_test_root_chain(&db);
                db.projection()
                    .connection()
                    .execute("DELETE FROM threads WHERE thread_id = 'T-root'", [])
                    .unwrap();
                let recovery = RecoveryStore::from_runtime_state_dir(&root).unwrap();
                (
                    db.selected_projection_path().to_path_buf(),
                    recovery.read_generation().unwrap().unwrap(),
                )
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

            let reopened = StateDb::open(&root, test_trust_store()).unwrap();
            assert_ne!(
                reopened.selected_projection_path(),
                old_path,
                "an absent or wrong-epoch pointer must select a new instance"
            );
            assert!(
                reopened.get_thread("T-root").unwrap().is_some(),
                "the new baseline must restore rows from signed CAS truth"
            );
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
            let db = StateDb::open(&root, test_trust_store()).unwrap();
            create_test_root_chain(&db);
            db.projection()
                .connection()
                .execute("DELETE FROM threads WHERE thread_id = 'T-root'", [])
                .unwrap();
            db.selected_projection_path().to_path_buf()
        };

        let connection = rusqlite::Connection::open(&old_path).unwrap();
        connection
            .execute(
                "UPDATE projection_recovery_identity
                 SET projection_instance_id = 'different-instance'
                 WHERE singleton = 1",
                [],
            )
            .unwrap();
        drop(connection);

        let reopened = StateDb::open(&root, test_trust_store()).unwrap();
        assert_ne!(reopened.selected_projection_path(), old_path);
        assert!(reopened.get_thread("T-root").unwrap().is_some());
        let generation = RecoveryStore::from_runtime_state_dir(&root)
            .unwrap()
            .read_generation()
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
    fn interrupted_rebuild_never_replaces_the_selected_generation() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("state");
        let (old_path, old_generation) = {
            let db = StateDb::open(&root, test_trust_store()).unwrap();
            create_test_root_chain(&db);
            let recovery = RecoveryStore::from_runtime_state_dir(&root).unwrap();
            (
                db.selected_projection_path().to_path_buf(),
                recovery.read_generation().unwrap().unwrap(),
            )
        };
        let recovery = RecoveryStore::from_runtime_state_dir(&root).unwrap();
        let trust_store = test_trust_store();

        let error = rebuild_and_install_projection(
            &root,
            &root.join("objects"),
            &root.join("refs"),
            &recovery,
            trust_store.as_ref(),
            None,
            Some(&AlwaysCancelledRecovery),
            None,
        )
        .err()
        .expect("cancelled rebuild must fail before publication");

        assert!(format!("{error:#}").contains("projection recovery cancelled"));
        assert_eq!(recovery.read_generation().unwrap().unwrap(), old_generation);
        let selected = ProjectionDb::open_selected_current(&old_path).unwrap();
        assert_eq!(
            selected.projection_instance_id().unwrap().as_deref(),
            Some(old_generation.projection_instance_id.as_str())
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
        let staged = ProjectionDb::open_transient().unwrap();

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
    fn generation_pointer_published_before_ack_recovers_selected_instance() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("state");
        let db = StateDb::open(&root, test_trust_store()).unwrap();
        let target_hash = create_test_root_chain(&db);
        let transition = {
            let chain_lock = crate::chain::ChainLock::acquire(db.refs_root(), "T-root").unwrap();
            db.recovery
                .prepare_set(&chain_lock, "T-root", None, &target_hash)
                .unwrap()
        };
        let recovery = db.recovery.clone();
        let trust_store = db.head_trust_store_arc();
        let cas_root = db.cas_root().to_path_buf();
        let refs_root = db.refs_root().to_path_buf();
        drop(db);

        let guard = crate::recovery::CasMutationGuard::acquire_exclusive(&root).unwrap();
        let instance_id = recovery.new_projection_instance_id();
        let projection_file = format!("projection.{instance_id}.sqlite3");
        let instance_path = root.join(&projection_file);
        let staged = ProjectionDb::open(&instance_path).unwrap();
        staged.set_projection_instance_id(&instance_id).unwrap();
        let (_, baseline) = crate::rebuild::rebuild_projection_with_verified_baseline(
            &staged,
            &cas_root,
            &refs_root,
            trust_store.as_ref(),
            None,
        )
        .unwrap();
        replay_pending_into(
            &staged,
            &cas_root,
            &refs_root,
            &recovery,
            trust_store.as_ref(),
            None,
            false,
            None,
        )
        .unwrap();
        crate::rebuild::verify_preverified_projection(
            &staged,
            &cas_root,
            &refs_root,
            trust_store.as_ref(),
            &baseline,
            &guard,
            None,
        )
        .unwrap();
        staged.close_durable().unwrap();

        let generation = ProjectionRecoveryGeneration::new(
            ProjectionDb::current_schema_epoch(),
            instance_id,
            projection_file,
        )
        .unwrap();
        recovery.write_generation(&generation).unwrap();
        drop(guard);
        assert_eq!(
            recovery
                .read_pending("T-root")
                .unwrap()
                .unwrap()
                .transition_id,
            transition.transition_id,
            "publishing generation.json must not acknowledge staged replay"
        );

        let reopened = StateDb::open(&root, trust_store).unwrap();
        assert_eq!(reopened.selected_projection_path(), instance_path);
        assert!(recovery.read_pending("T-root").unwrap().is_none());
        assert_eq!(
            reopened
                .projection()
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
            assert!(error.to_string().contains("different refs root"));
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
            .create_chain("T-root", test_root_snapshot("system/test"), &signer)
            .unwrap();
        let lock = crate::chain::ChainLock::acquire(db.refs_root(), "T-root").unwrap();
        db.recovery
            .prepare_remove(&lock, "T-root", created.value.chain_state_hash.as_str())
            .unwrap();
        let error = db.delete_chain_projection("T-root", &lock).unwrap_err();
        assert!(error.to_string().contains("before durable head removal"));
        assert!(db
            .projection()
            .get_projection_meta("T-root")
            .unwrap()
            .is_some());
    }

    #[test]
    fn create_chain_stores_in_projection() {
        let (_dir, db) = open_temp();
        let signer = TestSigner::default();

        let snapshot = test_root_snapshot("system/test");

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
                test_root_snapshot("system/test"),
                &TestSigner::default(),
            )
            .unwrap_err();

        assert!(error.to_string().contains("CAS root was replaced"));
        assert!(!original.join("objects").exists());
    }

    #[test]
    fn projection_uses_exact_snapshots_committed_after_clock_rollback_clamping() {
        let (_dir, db) = open_temp();
        let signer = TestSigner::default();
        let floor = "2099-01-01T00:00:10Z";
        let rolled_back = "2099-01-01T00:00:05Z";

        let mut root = test_root_snapshot("system/test");
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
        let snapshot = test_root_snapshot("system/test");
        db.create_chain("T-root", snapshot.clone(), &signer)
            .unwrap();

        let loaded = db
            .read_authoritative_thread_snapshot("T-root", "T-root")
            .unwrap()
            .expect("root snapshot should exist through authoritative head");
        assert_eq!(loaded.to_value(), snapshot.to_value());
        assert!(db
            .read_authoritative_thread_snapshot("T-root", "T-missing")
            .unwrap()
            .is_none());
        assert!(db
            .read_authoritative_thread_snapshot("T-missing", "T-missing")
            .unwrap()
            .is_none());
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
        let snapshot = test_root_snapshot("system/test");
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
        let snapshot = test_root_snapshot("system/test");
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
            .connection()
            .query_row("SELECT count(*) FROM events", [], |row| row.get(0))
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
            .connection()
            .query_row("SELECT count(*) FROM events", [], |row| row.get(0))
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
        let snapshot = test_root_snapshot("system/test");
        let initial = db.create_chain("T-root", snapshot, &signer).unwrap();
        db.projection()
            .connection()
            .execute_batch(
                "CREATE TRIGGER reject_test_event_projection
                 BEFORE INSERT ON events
                 BEGIN
                     SELECT RAISE(FAIL, 'injected event projection failure');
                 END;",
            )
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
            .connection()
            .execute_batch("DROP TRIGGER reject_test_event_projection")
            .unwrap();
        drop(db);
        let reopened = StateDb::open(dir.path(), test_trust_store()).unwrap();
        let projected = reopened
            .projection()
            .get_projection_meta("T-root")
            .unwrap()
            .expect("startup pending replay must restore the projection cursor");
        assert_eq!(projected.indexed_chain_state_hash, head.target_hash);
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
            StateDb::open_with_projection_repair_sink(dir.path(), sink.clone(), test_trust_store())
                .unwrap();
        let signer = TestSigner::default();
        let snapshot = test_root_snapshot("system/test");
        db.create_chain("T-root", snapshot, &signer).unwrap();
        db.projection()
            .connection()
            .execute_batch("DROP TABLE events")
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

    #[test]
    fn open_rebuilds_projection_after_schema_epoch_reset() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("state");
        let signer = TestSigner::default();

        let projection_path = {
            let db = StateDb::open(&root, test_trust_store()).unwrap();
            let snapshot = test_root_snapshot("system/test");
            db.create_chain("T-root", snapshot, &signer).unwrap();
            db.projection().path().to_path_buf()
        };

        let conn = rusqlite::Connection::open(&projection_path).unwrap();
        conn.execute_batch("PRAGMA user_version = 0;").unwrap();
        drop(conn);

        let db = StateDb::open(&root, test_trust_store()).unwrap();
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
            let db = StateDb::open(&root, test_trust_store()).unwrap();
            let snapshot = test_root_snapshot("directive:apps/tv-tracker/ai_chat");
            db.create_chain("T-root", snapshot, &signer).unwrap();

            let created =
                crate::objects::thread_event::NewEvent::new("T-root", "T-root", "thread_created")
                    .payload(serde_json::json!({
                        "item_ref": "directive:apps/tv-tracker/ai_chat",
                        "executor_ref": "runtime:directive-runtime",
                        "launch_mode": "inline",
                        "usage_subject": {
                            "namespace": "tv-tracker",
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
                        "spend_usd": 0.15,
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

        let conn = rusqlite::Connection::open(&projection_path).unwrap();
        conn.execute_batch("PRAGMA user_version = 0;").unwrap();
        drop(conn);

        let db = StateDb::open(&root, test_trust_store()).unwrap();
        let usage_rows: i64 = db
            .projection()
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM thread_usage_latest WHERE thread_id = 'T-root' \
                 AND input_tokens = 100 AND output_tokens = 50",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(usage_rows, 1);

        let subject_rows: i64 = db
            .projection()
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM thread_usage_subjects WHERE chain_root_id = 'T-root' \
                 AND namespace = 'tv-tracker' AND subject = 'csm01' \
                 AND asserted_by = 'fp:backend'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(subject_rows, 1);
    }

    #[test]
    fn create_chain_cas_objects_exist() {
        let (_dir, db) = open_temp();
        let signer = TestSigner::default();

        let snapshot = test_root_snapshot("system/test");

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

        let snapshot = test_root_snapshot("system/test");

        db.create_chain("T-root", snapshot, &signer).unwrap();

        let ref_path = db.refs_root().join("generic/chains/T-root/head");
        assert!(ref_path.exists());
    }

    #[test]
    fn add_thread_projects_snapshot() {
        let (_dir, db) = open_temp();
        let signer = TestSigner::default();

        let root_snapshot = test_root_snapshot("system/test");

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

        let root_snapshot = test_root_snapshot("system/test");

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

        let snapshot = test_root_snapshot("system/test");

        db.create_chain("T-root", snapshot, &signer).unwrap();

        let event = crate::objects::thread_event::NewEvent::new("T-root", "T-root", "test_event")
            .payload(serde_json::json!({"key": "value"}))
            .build();

        let result = db
            .append_events("T-root", "T-root", vec![event], vec![], &signer)
            .unwrap();

        assert_eq!(result.value.event_count, 1);

        let conn = db.projection().connection();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE thread_id = 'T-root'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn append_events_rejects_ephemeral_events() {
        let (_dir, db) = open_temp();
        let signer = TestSigner::default();

        let snapshot = test_root_snapshot("system/test");

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
                vec![crate::objects::thread_event::NewEvent::new(
                    "T-root",
                    "T-root",
                    "advanced_after_remove",
                )
                .build()],
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
        let signer = TestSigner::default();
        let (_source_dir, source) = open_temp_trusted(&signer);
        let (_target_dir, target) = open_temp_trusted(&signer);
        source
            .create_chain("T-root", test_root_snapshot("system/test"), &signer)
            .unwrap();

        let first = crate::sync::export_chain(
            source.cas_root(),
            source.refs_root(),
            "T-root",
            source.head_trust_store(),
        )
        .unwrap();
        let staged = crate::sync::stage_chain_import(target.cas_root(), &first).unwrap();
        let guard = crate::CasMutationGuard::shared_from_cas_root(target.cas_root()).unwrap();
        crate::sync::finalize_import(&target, staged, &signer, &guard).unwrap();

        let event =
            crate::objects::thread_event::NewEvent::new("T-root", "T-root", "import_advance_test")
                .build();
        let advanced = source
            .append_events("T-root", "T-root", vec![event], vec![], &signer)
            .unwrap()
            .value
            .chain_state_hash;
        let second = crate::sync::export_chain(
            source.cas_root(),
            source.refs_root(),
            "T-root",
            source.head_trust_store(),
        )
        .unwrap();
        let staged = crate::sync::stage_chain_import(target.cas_root(), &second).unwrap();
        crate::sync::finalize_import(&target, staged, &signer, &guard).unwrap();

        let imported = target
            .read_generic_head_ref("chains", "T-root")
            .unwrap()
            .unwrap();
        assert_eq!(imported.target_hash, advanced);
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
