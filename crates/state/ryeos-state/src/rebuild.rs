//! Projection rebuild and named-chain repair from CAS.
//!
//! The projection (SQLite) is a rebuildable view of immutable CAS objects.
//! If the selected projection instance is absent or corrupt, everything can be
//! recovered by walking CAS from signed heads.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::projection::{project_event, project_thread_snapshot, ProjectionDb};
use crate::refs::{self, TrustStore};
use crate::state_db::{
    ProjectionRecoveryObserver, ProjectionRecoveryProgress, ProjectionRecoveryStage,
};
use crate::{ChainState, ThreadEvent, ThreadSnapshot};

#[cfg(not(test))]
const REBUILD_TX_CHAIN_BATCH: usize = 200;
#[cfg(test)]
const REBUILD_TX_CHAIN_BATCH: usize = 2;

/// Report from a full projection rebuild.
#[derive(Debug, Clone, Default)]
pub struct RebuildReport {
    pub chains_rebuilt: usize,
    pub threads_restored: usize,
    pub events_projected: usize,
}

/// Capability proving that one exact signed-head set passed complete
/// closure/history validation and was replayed successfully. The private
/// contents prevent callers from manufacturing a trust bypass for staged
/// publication verification.
pub(crate) struct VerifiedBaseline {
    heads: Vec<VerifiedBaselineHead>,
}

struct VerifiedBaselineHead {
    meta: crate::projection::ProjectionMeta,
}

/// Report from a strict, non-mutating verification of the selected projection.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ProjectionVerificationReport {
    pub signed_heads_verified: usize,
    pub projection_cursors_verified: usize,
    pub projection_tables_verified: usize,
}

/// Deterministic semantic fingerprint of one registered projection table.
/// Surrogate SQLite row IDs are excluded for head-derived tables because live
/// projection order across independent chains is not authoritative.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectionTableFingerprint {
    table: &'static str,
    columns: Vec<String>,
    rows: u64,
    digest: [u8; 32],
}

const HEAD_DERIVED_PROJECTION_TABLES: &[&str] = &[
    "projection_meta",
    "threads",
    "chain_retention",
    "events",
    "event_replay_index",
    "thread_edges",
    "thread_results",
    "thread_artifacts",
    "thread_facets",
    "thread_usage_latest",
    "thread_usage_subjects",
];

const SURROGATE_COLUMNS: &[&str] = &["event_id", "edge_id", "artifact_id", "facet_id"];

const CLEAR_HEAD_DERIVED_PROJECTION_SQL: &str = "DELETE FROM projection_meta;
     DELETE FROM chain_retention;
     DELETE FROM threads;
     DELETE FROM events;
     DELETE FROM event_replay_index;
     DELETE FROM thread_edges;
     DELETE FROM thread_results;
     DELETE FROM thread_artifacts;
     DELETE FROM thread_facets;
     DELETE FROM thread_usage_latest;
     DELETE FROM thread_usage_subjects;";

/// Report from one journal-selected chain repair.
#[derive(Debug, Clone, Default)]
pub struct ChainRepairReport {
    pub chains_checked: usize,
    pub chains_updated: usize,
    pub threads_restored: usize,
    pub events_projected: usize,
}

/// Time-based progress reporter for full rebuild loops. These run
/// synchronously on the daemon boot path and can grind for minutes on a big
/// store; the low-rate structured note complements the live lifecycle
/// observer without emitting one log record per chain.
struct RebuildProgress {
    started: std::time::Instant,
    next_note: std::time::Instant,
}

const REBUILD_PROGRESS_EVERY: std::time::Duration = std::time::Duration::from_secs(15);
const REBUILD_OBSERVER_PROGRESS_EVERY: std::time::Duration = std::time::Duration::from_millis(250);
const SQLITE_CANCELLATION_POLL_EVERY: std::time::Duration = std::time::Duration::from_millis(10);

#[cfg(test)]
thread_local! {
    static CHAIN_REF_ENUMERATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn take_chain_ref_enumerations_for_test() -> usize {
    CHAIN_REF_ENUMERATIONS.with(|count| count.replace(0))
}

impl RebuildProgress {
    fn new() -> Self {
        let now = std::time::Instant::now();
        Self {
            started: now,
            next_note: now + REBUILD_PROGRESS_EVERY,
        }
    }

    fn note(&mut self, stage: &str, chains: usize, threads: usize, events: usize) {
        if std::time::Instant::now() < self.next_note {
            return;
        }
        tracing::info!(
            stage,
            chains,
            threads,
            events,
            elapsed_s = self.started.elapsed().as_secs(),
            "projection {stage} in progress"
        );
        self.next_note = std::time::Instant::now() + REBUILD_PROGRESS_EVERY;
    }
}

/// Rate-limits observer updates without rate-limiting cancellation checks.
/// Rebuild loops call `ensure_not_cancelled` for every chain, while UI/status
/// consumers receive at most four intermediate snapshots per second.
struct ObserverProgressThrottle {
    next_update: std::time::Instant,
}

impl ObserverProgressThrottle {
    fn new() -> Self {
        Self {
            next_update: std::time::Instant::now() + REBUILD_OBSERVER_PROGRESS_EVERY,
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
        self.next_update = now + REBUILD_OBSERVER_PROGRESS_EVERY;
    }
}

/// Full rebuild: delete and recreate projection from CAS.
///
/// Walks every signed chain head and projects all thread snapshots
/// and durable events into the projection database.
#[tracing::instrument(name = "state:rebuild", skip(projection, cas_root, refs_root))]
pub fn rebuild_projection(
    projection: &ProjectionDb,
    cas_root: &Path,
    refs_root: &Path,
    trust_store: &TrustStore,
) -> Result<RebuildReport> {
    rebuild_projection_with_observer(projection, cas_root, refs_root, trust_store, None)
}

pub fn rebuild_projection_with_observer(
    projection: &ProjectionDb,
    cas_root: &Path,
    refs_root: &Path,
    trust_store: &TrustStore,
    observer: Option<&dyn ProjectionRecoveryObserver>,
) -> Result<RebuildReport> {
    let cas = lillux::CasStore::new(cas_root.to_path_buf());
    rebuild_projection_with_verified_baseline_inner(
        projection,
        &cas,
        RebuildHeads::Path(refs_root),
        trust_store,
        observer,
    )
    .map(|(report, _)| report)
}

#[cfg(test)]
pub(crate) fn rebuild_projection_with_verified_baseline(
    projection: &ProjectionDb,
    cas_root: &Path,
    refs_root: &Path,
    trust_store: &TrustStore,
    observer: Option<&dyn ProjectionRecoveryObserver>,
) -> Result<(RebuildReport, VerifiedBaseline)> {
    let cas = lillux::CasStore::new(cas_root.to_path_buf());
    rebuild_projection_with_verified_baseline_inner(
        projection,
        &cas,
        RebuildHeads::Path(refs_root),
        trust_store,
        observer,
    )
}

pub(crate) fn rebuild_projection_with_verified_baseline_pinned(
    projection: &ProjectionDb,
    cas: &lillux::CasStore,
    refs: &lillux::PinnedDirectory,
    trust_store: &TrustStore,
    observer: Option<&dyn ProjectionRecoveryObserver>,
) -> Result<(RebuildReport, VerifiedBaseline)> {
    rebuild_projection_with_verified_baseline_inner(
        projection,
        cas,
        RebuildHeads::Pinned(refs),
        trust_store,
        observer,
    )
}

#[derive(Clone, Copy)]
enum RebuildHeads<'a> {
    Path(&'a Path),
    Pinned(&'a lillux::PinnedDirectory),
}

fn rebuild_projection_with_verified_baseline_inner(
    projection: &ProjectionDb,
    cas: &lillux::CasStore,
    refs: RebuildHeads<'_>,
    trust_store: &TrustStore,
    observer: Option<&dyn ProjectionRecoveryObserver>,
) -> Result<(RebuildReport, VerifiedBaseline)> {
    let mut report = RebuildReport::default();
    let mut verified_heads = Vec::new();
    let mut progress = RebuildProgress::new();
    let mut observer_progress = ObserverProgressThrottle::new();

    // Clear existing projection tables (schema will be re-created)
    let conn = projection.connection();
    conn.execute_batch(CLEAR_HEAD_DERIVED_PROJECTION_SQL)
        .context("failed to clear projection tables")?;

    ensure_not_cancelled(observer)?;
    let heads = chain_heads_with_observer(refs, trust_store, observer)?;
    // Retention intentionally leaves empty per-chain directories behind. Only
    // authoritative head entries are work units and belong in progress totals.
    let chains_total = heads.len();
    notify_observer(
        observer,
        ProjectionRecoveryProgress {
            stage: ProjectionRecoveryStage::Rebuilding,
            chains_total: Some(chains_total),
            chains_done: 0,
            threads_restored: 0,
            events_projected: 0,
            pending_total: None,
            pending_done: 0,
        },
    )?;
    if heads.is_empty() {
        return Ok((
            report,
            VerifiedBaseline {
                heads: verified_heads,
            },
        ));
    }

    let mut batch = ProjectionBatch::new(projection, "rebuild");

    for head in heads {
        ensure_not_cancelled(observer)?;
        let chain_root_id = head.name;
        validate_chain_root_id_for_rebuild(&chain_root_id)?;
        let chain_state_hash = head.target_hash.as_str();
        verify_repair_closure_with_cas_and_observer(
            cas,
            &chain_root_id,
            chain_state_hash,
            true,
            observer,
        )?;

        batch.ensure_started()?;

        // Walk chain history (newest to oldest via prev_chain_state_hash)
        let chain_report = rebuild_chain_with_observer(
            projection,
            cas,
            &chain_root_id,
            chain_state_hash,
            observer,
        )?;

        // Update projection_meta to point to the head
        // Read the head chain_state to get updated_at
        let meta = head_projection_meta(cas, &chain_root_id, chain_state_hash)?;
        projection.update_projection_meta(&meta)?;
        verified_heads.push(VerifiedBaselineHead { meta });

        report.chains_rebuilt += 1;
        report.threads_restored += chain_report.threads;
        report.events_projected += chain_report.events;
        batch.note_chain()?;
        progress.note(
            "rebuild",
            report.chains_rebuilt,
            report.threads_restored,
            report.events_projected,
        );
        observer_progress.maybe_notify(
            observer,
            ProjectionRecoveryProgress {
                stage: ProjectionRecoveryStage::Rebuilding,
                chains_total: Some(chains_total),
                chains_done: report.chains_rebuilt,
                threads_restored: report.threads_restored,
                events_projected: report.events_projected,
                pending_total: None,
                pending_done: 0,
            },
        );
    }

    batch.commit_partial()?;

    notify_observer(
        observer,
        ProjectionRecoveryProgress {
            stage: ProjectionRecoveryStage::Rebuilding,
            chains_total: Some(chains_total),
            chains_done: report.chains_rebuilt,
            threads_restored: report.threads_restored,
            events_projected: report.events_projected,
            pending_total: None,
            pending_done: 0,
        },
    )?;

    Ok((
        report,
        VerifiedBaseline {
            heads: verified_heads,
        },
    ))
}

/// Verify every authoritative signed chain head, its complete CAS closure, and
/// the exact projection cursor set without changing the projection database.
/// Extra cursors are rejected because they represent rows for a head that is no
/// longer authoritative; missing or stale cursors are rejected per chain.
pub fn verify_projection_with_trust(
    projection: &ProjectionDb,
    cas_root: &Path,
    refs_root: &Path,
    trust_store: &TrustStore,
) -> Result<ProjectionVerificationReport> {
    verify_projection(projection, cas_root, refs_root, trust_store)
}

pub(crate) fn verify_projection(
    projection: &ProjectionDb,
    cas_root: &Path,
    refs_root: &Path,
    trust_store: &TrustStore,
) -> Result<ProjectionVerificationReport> {
    verify_projection_with_observer(projection, cas_root, refs_root, trust_store, None)
}

pub(crate) fn verify_projection_with_observer(
    projection: &ProjectionDb,
    cas_root: &Path,
    refs_root: &Path,
    trust_store: &TrustStore,
    observer: Option<&dyn ProjectionRecoveryObserver>,
) -> Result<ProjectionVerificationReport> {
    let cas = lillux::CasStore::new(cas_root.to_path_buf());
    verify_projection_with_authority(
        projection,
        &cas,
        RebuildHeads::Path(refs_root),
        trust_store,
        observer,
    )
}

pub(crate) fn verify_projection_with_trust_pinned(
    projection: &ProjectionDb,
    cas: &lillux::CasStore,
    refs: &lillux::PinnedDirectory,
    trust_store: &TrustStore,
) -> Result<ProjectionVerificationReport> {
    verify_projection_with_authority(
        projection,
        cas,
        RebuildHeads::Pinned(refs),
        trust_store,
        None,
    )
}

fn verify_projection_with_authority(
    projection: &ProjectionDb,
    cas: &lillux::CasStore,
    refs: RebuildHeads<'_>,
    trust_store: &TrustStore,
    observer: Option<&dyn ProjectionRecoveryObserver>,
) -> Result<ProjectionVerificationReport> {
    ensure_not_cancelled(observer)?;
    let mut cursors = BTreeMap::new();
    let mut statement = projection
        .connection()
        .prepare(
            "SELECT chain_root_id, indexed_chain_state_hash, updated_at
             FROM projection_meta
             ORDER BY chain_root_id",
        )
        .context("read projection cursors for verification")?;
    let rows = statement.query_map([], |row| {
        Ok(crate::projection::ProjectionMeta {
            chain_root_id: row.get(0)?,
            indexed_chain_state_hash: row.get(1)?,
            updated_at: row.get(2)?,
        })
    })?;
    for row in rows {
        ensure_not_cancelled(observer)?;
        let cursor = row.context("decode projection cursor for verification")?;
        cursors.insert(cursor.chain_root_id.clone(), cursor);
    }

    let mut report = ProjectionVerificationReport::default();
    for head in chain_heads_with_observer(refs, trust_store, observer)? {
        ensure_not_cancelled(observer)?;
        let chain_root_id = head.name;
        validate_chain_root_id_for_rebuild(&chain_root_id)?;
        verify_repair_closure_with_cas_and_observer(
            cas,
            &chain_root_id,
            &head.target_hash,
            true,
            observer,
        )?;
        report.signed_heads_verified += 1;

        let expected = head_projection_meta(cas, &chain_root_id, &head.target_hash)?;
        let actual = cursors.remove(&chain_root_id).ok_or_else(|| {
            anyhow::anyhow!(
                "projection cursor missing for signed head {chain_root_id}@{}",
                head.target_hash
            )
        })?;
        if actual.indexed_chain_state_hash != expected.indexed_chain_state_hash
            || actual.updated_at != expected.updated_at
        {
            anyhow::bail!(
                "projection cursor mismatch for signed head {chain_root_id}: expected hash={} updated_at={}, got hash={} updated_at={}",
                expected.indexed_chain_state_hash,
                expected.updated_at,
                actual.indexed_chain_state_hash,
                actual.updated_at,
            );
        }
        report.projection_cursors_verified += 1;
    }

    if let Some((chain_root_id, cursor)) = cursors.into_iter().next() {
        anyhow::bail!(
            "projection contains cursor without an authoritative signed head: {chain_root_id}@{}",
            cursor.indexed_chain_state_hash
        );
    }

    // A cursor is a commit marker, not proof that every materialized row is
    // present and correct. Rebuild the expected head-derived view in memory
    // under the caller's frozen CAS/head guard, then compare deterministic
    // semantic fingerprints for every authoritative projection table.
    let expected = ProjectionDb::open_transient()?;
    rebuild_projection_with_verified_baseline_inner(&expected, cas, refs, trust_store, observer)
        .context("rederive expected projection during verification")?;
    let expected_fingerprints = projection_content_fingerprints_with_observer(&expected, observer)?;
    verify_projection_matches_fingerprints_with_observer(
        projection,
        &expected_fingerprints,
        observer,
    )?;
    report.projection_tables_verified = expected_fingerprints.len();
    Ok(report)
}

/// Verify a staged projection against the exact head set that already passed
/// full closure/history validation in this rebuild. This entry point is
/// intentionally crate-private and requires the opaque capability returned by
/// [`rebuild_projection_with_verified_baseline`]. Manual verification keeps
/// using [`verify_projection_with_observer`] and never skips trust validation.
#[cfg(test)]
pub(crate) fn verify_preverified_projection(
    projection: &ProjectionDb,
    cas_root: &Path,
    refs_root: &Path,
    trust_store: &TrustStore,
    baseline: &VerifiedBaseline,
    cas_mutation_guard: &crate::recovery::CasMutationGuard,
    observer: Option<&dyn ProjectionRecoveryObserver>,
) -> Result<(
    ProjectionVerificationReport,
    Vec<ProjectionTableFingerprint>,
)> {
    if !cas_mutation_guard.is_exclusive() {
        anyhow::bail!("preverified projection verification requires exclusive mutation exclusion");
    }
    cas_mutation_guard.ensure_protects_cas_root(cas_root)?;
    let cas = lillux::CasStore::new(cas_root.to_path_buf());
    verify_preverified_projection_inner(
        projection,
        &cas,
        RebuildHeads::Path(refs_root),
        trust_store,
        baseline,
        observer,
    )
}

pub(crate) fn verify_preverified_projection_pinned(
    projection: &ProjectionDb,
    cas: &lillux::CasStore,
    refs: &lillux::PinnedDirectory,
    trust_store: &TrustStore,
    baseline: &VerifiedBaseline,
    cas_mutation_guard: &crate::recovery::CasMutationGuard,
    runtime: &lillux::PinnedDirectory,
    observer: Option<&dyn ProjectionRecoveryObserver>,
) -> Result<(
    ProjectionVerificationReport,
    Vec<ProjectionTableFingerprint>,
)> {
    if !cas_mutation_guard.is_exclusive() {
        anyhow::bail!("preverified projection verification requires exclusive mutation exclusion");
    }
    cas_mutation_guard.ensure_protects_pinned_runtime(runtime)?;
    verify_preverified_projection_inner(
        projection,
        cas,
        RebuildHeads::Pinned(refs),
        trust_store,
        baseline,
        observer,
    )
}

fn verify_preverified_projection_inner(
    projection: &ProjectionDb,
    cas: &lillux::CasStore,
    refs: RebuildHeads<'_>,
    trust_store: &TrustStore,
    baseline: &VerifiedBaseline,
    observer: Option<&dyn ProjectionRecoveryObserver>,
) -> Result<(
    ProjectionVerificationReport,
    Vec<ProjectionTableFingerprint>,
)> {
    ensure_not_cancelled(observer)?;

    // Re-read every signed head so the capability is useful only for the
    // exact frozen head set it proved. Legitimate mutation is excluded by the
    // caller's guard; this comparison also fails closed if that invariant is
    // ever violated.
    let mut observed_heads = Vec::new();
    for head in chain_heads_with_observer(refs, trust_store, observer)? {
        ensure_not_cancelled(observer)?;
        let chain_root_id = head.name;
        validate_chain_root_id_for_rebuild(&chain_root_id)?;
        observed_heads.push(head_projection_meta(
            cas,
            &chain_root_id,
            &head.target_hash,
        )?);
    }
    require_exact_baseline_heads(baseline, &observed_heads)?;

    let mut cursors = read_projection_cursors(projection, observer)?;
    for head in &baseline.heads {
        ensure_not_cancelled(observer)?;
        let expected = &head.meta;
        let actual = cursors.remove(&expected.chain_root_id).ok_or_else(|| {
            anyhow::anyhow!(
                "projection cursor missing for preverified head {}@{}",
                expected.chain_root_id,
                expected.indexed_chain_state_hash
            )
        })?;
        require_exact_projection_meta("projection cursor", expected, &actual)?;
    }
    if let Some((chain_root_id, cursor)) = cursors.into_iter().next() {
        anyhow::bail!(
            "projection contains cursor outside the preverified baseline: {chain_root_id}@{}",
            cursor.indexed_chain_state_hash
        );
    }

    // Independently rederive every expected row using the strict canonical
    // loaders. The earlier capability proves the expensive semantic closure;
    // immutable hash checks and exact cursor comparisons still run here.
    // This pass polls cancellation but deliberately publishes no progress, so
    // operator counters never reset during internal verification.
    let expected = ProjectionDb::open_transient()?;
    rederive_projection_from_verified_baseline(&expected, cas, baseline, observer)
        .context("rederive expected projection from preverified baseline")?;
    let expected_fingerprints = projection_content_fingerprints_with_observer(&expected, observer)?;
    verify_projection_matches_fingerprints_with_observer(
        projection,
        &expected_fingerprints,
        observer,
    )?;

    Ok((
        ProjectionVerificationReport {
            signed_heads_verified: observed_heads.len(),
            projection_cursors_verified: baseline.heads.len(),
            projection_tables_verified: expected_fingerprints.len(),
        },
        expected_fingerprints,
    ))
}

fn require_exact_baseline_heads(
    baseline: &VerifiedBaseline,
    observed: &[crate::projection::ProjectionMeta],
) -> Result<()> {
    if baseline.heads.len() != observed.len() {
        anyhow::bail!(
            "signed head set changed after baseline validation: expected {}, observed {}",
            baseline.heads.len(),
            observed.len()
        );
    }
    for (expected, actual) in baseline.heads.iter().map(|head| &head.meta).zip(observed) {
        require_exact_projection_meta("signed head", expected, actual)?;
    }
    Ok(())
}

fn require_exact_projection_meta(
    label: &str,
    expected: &crate::projection::ProjectionMeta,
    actual: &crate::projection::ProjectionMeta,
) -> Result<()> {
    if actual.chain_root_id != expected.chain_root_id
        || actual.indexed_chain_state_hash != expected.indexed_chain_state_hash
        || actual.updated_at != expected.updated_at
    {
        anyhow::bail!(
            "{label} mismatch: expected {}@{} updated_at={}, got {}@{} updated_at={}",
            expected.chain_root_id,
            expected.indexed_chain_state_hash,
            expected.updated_at,
            actual.chain_root_id,
            actual.indexed_chain_state_hash,
            actual.updated_at
        );
    }
    Ok(())
}

fn read_projection_cursors(
    projection: &ProjectionDb,
    observer: Option<&dyn ProjectionRecoveryObserver>,
) -> Result<BTreeMap<String, crate::projection::ProjectionMeta>> {
    ensure_not_cancelled(observer)?;
    let mut statement = projection
        .connection()
        .prepare(
            "SELECT chain_root_id, indexed_chain_state_hash, updated_at
             FROM projection_meta
             ORDER BY chain_root_id",
        )
        .context("read projection cursors for preverified verification")?;
    let rows = statement.query_map([], |row| {
        Ok(crate::projection::ProjectionMeta {
            chain_root_id: row.get(0)?,
            indexed_chain_state_hash: row.get(1)?,
            updated_at: row.get(2)?,
        })
    })?;
    let mut cursors = BTreeMap::new();
    for row in rows {
        ensure_not_cancelled(observer)?;
        let cursor = row.context("decode projection cursor for preverified verification")?;
        cursors.insert(cursor.chain_root_id.clone(), cursor);
    }
    Ok(cursors)
}

fn rederive_projection_from_verified_baseline(
    projection: &ProjectionDb,
    cas: &lillux::CasStore,
    baseline: &VerifiedBaseline,
    observer: Option<&dyn ProjectionRecoveryObserver>,
) -> Result<()> {
    ensure_not_cancelled(observer)?;
    projection
        .connection()
        .execute_batch(CLEAR_HEAD_DERIVED_PROJECTION_SQL)
        .context("clear expected preverified projection")?;

    let mut batch = ProjectionBatch::new(projection, "preverified rederive");
    for head in &baseline.heads {
        ensure_not_cancelled(observer)?;
        batch.ensure_started()?;
        rebuild_chain_with_observer(
            projection,
            cas,
            &head.meta.chain_root_id,
            &head.meta.indexed_chain_state_hash,
            observer,
        )?;
        let meta = head_projection_meta(
            cas,
            &head.meta.chain_root_id,
            &head.meta.indexed_chain_state_hash,
        )?;
        require_exact_projection_meta("preverified rederive head", &head.meta, &meta)?;
        projection.update_projection_meta(&meta)?;
        batch.note_chain()?;
    }
    batch.commit_partial()?;
    ensure_not_cancelled(observer)
}

pub(crate) fn projection_content_fingerprints_with_observer(
    projection: &ProjectionDb,
    observer: Option<&dyn ProjectionRecoveryObserver>,
) -> Result<Vec<ProjectionTableFingerprint>> {
    with_projection_sqlite_cancellation(projection, observer, || {
        ensure_not_cancelled(observer)?;
        verify_sqlite_integrity(projection)?;
        ensure_not_cancelled(observer)?;
        projection_table_fingerprints(projection, HEAD_DERIVED_PROJECTION_TABLES, observer)
    })
}

pub(crate) fn verify_projection_matches_fingerprints_with_observer(
    projection: &ProjectionDb,
    expected: &[ProjectionTableFingerprint],
    observer: Option<&dyn ProjectionRecoveryObserver>,
) -> Result<()> {
    with_projection_sqlite_cancellation(projection, observer, || {
        verify_projection_matches_registered_fingerprints(
            projection,
            HEAD_DERIVED_PROJECTION_TABLES,
            expected,
            observer,
        )
    })
}

/// Run a potentially long SQLite verification operation with a scoped
/// cancellation watcher. `InterruptHandle` is available without optional
/// rusqlite hook features and does not install connection-global callbacks:
/// the watcher and its borrowed observer are joined before this function
/// returns. Once cancellation wins, any SQLite interruption is reported as the
/// recovery cancellation rather than as an opaque database failure.
fn with_projection_sqlite_cancellation<T>(
    projection: &ProjectionDb,
    observer: Option<&dyn ProjectionRecoveryObserver>,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    ensure_not_cancelled(observer)?;
    let Some(observer) = observer else {
        return operation();
    };

    let interrupt = projection.connection().get_interrupt_handle();
    let interrupted = std::sync::atomic::AtomicBool::new(false);
    let result = std::thread::scope(|scope| {
        let (finished_tx, finished_rx) = std::sync::mpsc::channel();
        let interrupted = &interrupted;
        scope.spawn(move || loop {
            match finished_rx.recv_timeout(SQLITE_CANCELLATION_POLL_EVERY) {
                Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if observer.is_cancelled() {
                        interrupted.store(true, std::sync::atomic::Ordering::Release);
                        // Keep interrupting until the scoped operation exits:
                        // cancellation may become visible just before SQLite
                        // starts its next scan/sort statement.
                        interrupt.interrupt();
                    }
                }
            }
        });

        let result = operation();
        let _ = finished_tx.send(());
        result
    });

    if interrupted.load(std::sync::atomic::Ordering::Acquire) || observer.is_cancelled() {
        anyhow::bail!("projection recovery cancelled");
    }
    result
}

fn projection_table_fingerprints(
    projection: &ProjectionDb,
    tables: &[&'static str],
    observer: Option<&dyn ProjectionRecoveryObserver>,
) -> Result<Vec<ProjectionTableFingerprint>> {
    let mut fingerprints = Vec::with_capacity(tables.len());
    for table in tables.iter().copied() {
        ensure_not_cancelled(observer)?;
        fingerprints.push(projection_table_fingerprint(projection, table, observer)?);
    }
    Ok(fingerprints)
}

fn verify_projection_matches_registered_fingerprints(
    projection: &ProjectionDb,
    tables: &[&'static str],
    expected: &[ProjectionTableFingerprint],
    observer: Option<&dyn ProjectionRecoveryObserver>,
) -> Result<()> {
    ensure_not_cancelled(observer)?;
    verify_sqlite_integrity(projection)?;
    ensure_not_cancelled(observer)?;
    let actual = projection_table_fingerprints(projection, tables, observer)?;
    verify_fingerprint_sets(expected, &actual)
}

fn verify_fingerprint_sets(
    expected: &[ProjectionTableFingerprint],
    actual: &[ProjectionTableFingerprint],
) -> Result<()> {
    if actual.len() != expected.len() {
        anyhow::bail!(
            "projection table fingerprint set changed: expected {}, got {}",
            expected.len(),
            actual.len()
        );
    }
    for (expected, actual) in expected.iter().zip(actual.iter()) {
        if expected != actual {
            anyhow::bail!(
                "projection table mismatch for {}: expected rows={}, digest={}; got rows={}, digest={}",
                expected.table,
                expected.rows,
                hex_digest(&expected.digest),
                actual.rows,
                hex_digest(&actual.digest),
            );
        }
    }
    Ok(())
}

fn verify_sqlite_integrity(projection: &ProjectionDb) -> Result<()> {
    let result: String = projection
        .connection()
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .context("run projection SQLite integrity check")?;
    if result != "ok" {
        anyhow::bail!("projection SQLite integrity check failed: {result}");
    }
    Ok(())
}

fn projection_table_fingerprint(
    projection: &ProjectionDb,
    table: &'static str,
    observer: Option<&dyn ProjectionRecoveryObserver>,
) -> Result<ProjectionTableFingerprint> {
    if !HEAD_DERIVED_PROJECTION_TABLES.contains(&table) {
        anyhow::bail!("refusing to fingerprint unregistered projection table {table}");
    }
    let columns = projection_table_columns(projection, table, true)?;
    if columns.is_empty() {
        anyhow::bail!("projection table {table} has no semantic columns");
    }
    let quoted_columns = quoted_projection_columns(&columns);
    let sql = format!("SELECT {quoted_columns} FROM \"{table}\" ORDER BY {quoted_columns}");
    let mut statement = projection.connection().prepare(&sql)?;
    let mut rows = statement.query([])?;
    let mut hasher = Sha256::new();
    hasher.update(b"ryeos:projection-table:v1\0");
    hash_framed(&mut hasher, 1, table.as_bytes());
    for column in &columns {
        hash_framed(&mut hasher, 2, column.as_bytes());
    }
    let mut row_count = 0_u64;
    while let Some(row) = rows.next()? {
        ensure_not_cancelled(observer)?;
        row_count = row_count
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("projection row count overflow for {table}"))?;
        hasher.update([3]);
        for index in 0..columns.len() {
            use rusqlite::types::ValueRef;
            match row.get_ref(index)? {
                ValueRef::Null => hasher.update([0]),
                ValueRef::Integer(value) => hash_framed(&mut hasher, 1, &value.to_be_bytes()),
                ValueRef::Real(value) => {
                    hash_framed(&mut hasher, 2, &value.to_bits().to_be_bytes())
                }
                ValueRef::Text(value) => hash_framed(&mut hasher, 3, value),
                ValueRef::Blob(value) => hash_framed(&mut hasher, 4, value),
            }
        }
    }
    Ok(ProjectionTableFingerprint {
        table,
        columns,
        rows: row_count,
        digest: hasher.finalize().into(),
    })
}

fn projection_table_columns(
    projection: &ProjectionDb,
    table: &'static str,
    semantic_only: bool,
) -> Result<Vec<String>> {
    let pragma = format!("PRAGMA table_info(\"{table}\")");
    let mut schema = projection.connection().prepare(&pragma)?;
    let columns = schema
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if columns.is_empty() {
        anyhow::bail!("projection table {table} is absent");
    }
    if semantic_only {
        Ok(columns
            .into_iter()
            .filter(|column| !SURROGATE_COLUMNS.contains(&column.as_str()))
            .collect())
    } else {
        Ok(columns)
    }
}

fn quoted_projection_columns(columns: &[String]) -> String {
    columns
        .iter()
        .map(|column| format!("\"{}\"", column.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(", ")
}

fn hash_framed(hasher: &mut Sha256, tag: u8, value: &[u8]) {
    hasher.update([tag]);
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

fn hex_digest(digest: &[u8; 32]) -> String {
    use std::fmt::Write as _;
    let mut output = String::with_capacity(64);
    for byte in digest {
        let _ = write!(&mut output, "{byte:02x}");
    }
    output
}

fn ensure_not_cancelled(observer: Option<&dyn ProjectionRecoveryObserver>) -> Result<()> {
    if observer.is_some_and(ProjectionRecoveryObserver::is_cancelled) {
        anyhow::bail!("projection recovery cancelled");
    }
    Ok(())
}

fn notify_observer(
    observer: Option<&dyn ProjectionRecoveryObserver>,
    progress: ProjectionRecoveryProgress,
) -> Result<()> {
    ensure_not_cancelled(observer)?;
    if let Some(observer) = observer {
        observer.update(progress);
    }
    Ok(())
}

pub(crate) fn repair_one_chain_verified_with_cas_and_observer(
    projection: &ProjectionDb,
    cas: &lillux::CasStore,
    chain_root_id: &str,
    head_hash: &str,
    observer: Option<&dyn ProjectionRecoveryObserver>,
) -> Result<ChainRepairReport> {
    ensure_not_cancelled(observer)?;
    if !lillux::valid_hash(head_hash) || head_hash.bytes().any(|byte| byte.is_ascii_uppercase()) {
        anyhow::bail!("invalid chain head hash: {head_hash}");
    }
    verify_repair_closure_with_cas_and_observer(cas, chain_root_id, head_hash, true, observer)?;

    let current_meta = projection.get_projection_meta(chain_root_id)?;
    if current_meta
        .as_ref()
        .map(|meta| meta.indexed_chain_state_hash.as_str())
        == Some(head_hash)
    {
        return Ok(ChainRepairReport {
            chains_checked: 1,
            ..Default::default()
        });
    }

    let mut report = ChainRepairReport {
        chains_checked: 1,
        ..Default::default()
    };
    projection.immediate_transaction("repair one chain", || {
        let chain_report = if let Some(meta) = current_meta.as_ref() {
            rebuild_chain_delta_with_observer(
                projection,
                cas,
                chain_root_id,
                &meta.indexed_chain_state_hash,
                head_hash,
                observer,
            )?
        } else {
            rebuild_chain_with_observer(projection, cas, chain_root_id, head_hash, observer)?
        };
        let meta = head_projection_meta(cas, chain_root_id, head_hash)?;
        projection.update_projection_meta(&meta)?;
        report.chains_updated = 1;
        report.threads_restored = chain_report.threads;
        report.events_projected = chain_report.events;
        Ok(())
    })?;
    Ok(report)
}

fn verify_repair_closure_with_cas_and_observer(
    cas: &lillux::CasStore,
    chain_root_id: &str,
    head_hash: &str,
    verify_all_hashes: bool,
    observer: Option<&dyn ProjectionRecoveryObserver>,
) -> Result<()> {
    verified_repair_closure_inner(
        cas,
        chain_root_id,
        head_hash,
        verify_all_hashes,
        None,
        observer,
    )
    .map(|_| ())
}

/// Verify a complete target closure and require it to descend from the
/// currently signed head. External publication uses this after its head CAS
/// check and before preparing any durable Set transition.
pub(crate) fn verify_repair_closure_anchored_with_cas(
    cas: &lillux::CasStore,
    chain_root_id: &str,
    head_hash: &str,
    verify_all_hashes: bool,
    expected_ancestor: Option<&str>,
) -> Result<()> {
    verified_repair_closure_inner(
        cas,
        chain_root_id,
        head_hash,
        verify_all_hashes,
        expected_ancestor,
        None,
    )
    .map(|_| ())
}

/// Verify one exact chain head from pinned CAS authority and return the
/// complete closure that was checked. Import staging uses the returned hashes
/// as durable temporary GC roots, including descendants omitted from an
/// incremental payload because they were already present locally. The caller
/// must retain the authority's mutation guard for the complete verification.
pub(crate) fn verified_repair_closure_with_cas(
    cas: &lillux::CasStore,
    chain_root_id: &str,
    head_hash: &str,
    verify_all_hashes: bool,
) -> Result<crate::object_closure::ObjectClosureReport> {
    verified_repair_closure_inner(cas, chain_root_id, head_hash, verify_all_hashes, None, None)
}

fn verified_repair_closure_inner(
    cas: &lillux::CasStore,
    chain_root_id: &str,
    head_hash: &str,
    verify_all_hashes: bool,
    expected_ancestor: Option<&str>,
    observer: Option<&dyn ProjectionRecoveryObserver>,
) -> Result<crate::object_closure::ObjectClosureReport> {
    let mut cancellation_check = || ensure_not_cancelled(observer);
    cancellation_check()?;
    let closure = crate::object_closure::collect_object_closure_with_cas_and_check(
        cas,
        [head_hash.to_string()],
        &mut cancellation_check,
    )?;
    if !closure.is_complete() {
        anyhow::bail!(
            "incomplete CAS closure for chain {chain_root_id}@{head_hash}: missing_objects={}, missing_blobs={}, malformed_objects={}, unsupported_objects={}",
            closure.missing_objects.len(),
            closure.missing_blobs.len(),
            closure.malformed_objects.len(),
            closure.unsupported_objects.len(),
        );
    }
    if verify_all_hashes {
        for hash in &closure.object_hashes {
            cancellation_check()?;
            let value = cas
                .get_object(hash)?
                .ok_or_else(|| anyhow::anyhow!("verified CAS object {hash} disappeared"))?;
            match value.get("kind").and_then(Value::as_str) {
                Some("chain_state") => {
                    let state: crate::ChainState = serde_json::from_value(value)
                        .with_context(|| format!("decode chain_state {hash}"))?;
                    state.validate()?;
                    if state.chain_root_id != chain_root_id {
                        anyhow::bail!(
                            "chain_state {hash} root mismatch: expected {chain_root_id}, got {}",
                            state.chain_root_id
                        );
                    }
                }
                Some("thread_snapshot") => {
                    let snapshot: ThreadSnapshot = serde_json::from_value(value)
                        .with_context(|| format!("decode thread_snapshot {hash}"))?;
                    snapshot.validate()?;
                    if snapshot.chain_root_id != chain_root_id {
                        anyhow::bail!(
                            "thread_snapshot {hash} root mismatch: expected {chain_root_id}, got {}",
                            snapshot.chain_root_id
                        );
                    }
                }
                Some("thread_event") => {
                    let event: ThreadEvent = serde_json::from_value(value)
                        .with_context(|| format!("decode thread_event {hash}"))?;
                    event.validate()?;
                    if event.chain_root_id != chain_root_id {
                        anyhow::bail!(
                            "thread_event {hash} root mismatch: expected {chain_root_id}, got {}",
                            event.chain_root_id
                        );
                    }
                }
                _ => {}
            }
        }
        for hash in &closure.blob_hashes {
            cancellation_check()?;
            cas.get_blob(hash)?
                .ok_or_else(|| anyhow::anyhow!("verified CAS blob {hash} disappeared"))?;
        }
    }
    cancellation_check()?;
    let state: crate::ChainState = serde_json::from_value(
        cas.get_object(head_hash)?
            .ok_or_else(|| anyhow::anyhow!("chain head object {head_hash} disappeared"))?,
    )
    .with_context(|| format!("decode chain head object {head_hash}"))?;
    state.validate()?;
    if state.chain_root_id != chain_root_id {
        anyhow::bail!(
            "chain head object root mismatch: expected {chain_root_id}, got {}",
            state.chain_root_id
        );
    }
    crate::chain::validate_authoritative_history_with_cas_and_check(
        cas,
        chain_root_id,
        head_hash,
        expected_ancestor,
        &mut cancellation_check,
    )
    .with_context(|| {
        format!("invalid authoritative history for chain {chain_root_id}@{head_hash}")
    })?;
    Ok(closure)
}

fn chain_heads_with_observer(
    refs: RebuildHeads<'_>,
    trust_store: &TrustStore,
    observer: Option<&dyn ProjectionRecoveryObserver>,
) -> Result<Vec<refs::GenericHeadRef>> {
    #[cfg(test)]
    CHAIN_REF_ENUMERATIONS.with(|count| count.set(count.get().saturating_add(1)));

    ensure_not_cancelled(observer)?;
    let heads = match refs {
        RebuildHeads::Path(root) => {
            refs::list_verified_generic_head_refs(root, "chains", trust_store)
        }
        RebuildHeads::Pinned(root) => {
            refs::list_verified_chain_heads_in_directory(root, trust_store)
        }
    }
    .context("enumerate authoritative chain heads")?;
    ensure_not_cancelled(observer)?;
    Ok(heads)
}

fn validate_chain_root_id_for_rebuild(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 255
        || matches!(value, "." | "..")
        || value.contains(['/', '\\'])
        || value.bytes().any(|byte| byte.is_ascii_control())
    {
        anyhow::bail!("invalid chain root ID in authoritative refs: {value:?}");
    }
    Ok(())
}

fn head_projection_meta(
    cas: &lillux::CasStore,
    chain_root_id: &str,
    chain_state_hash: &str,
) -> Result<crate::projection::ProjectionMeta> {
    let state = load_chain_state(cas, chain_state_hash, chain_root_id).with_context(|| {
        format!("derive projection cursor for {chain_root_id}@{chain_state_hash}")
    })?;
    Ok(crate::projection::ProjectionMeta {
        chain_root_id: chain_root_id.to_string(),
        indexed_chain_state_hash: chain_state_hash.to_string(),
        updated_at: state.updated_at,
    })
}

struct ProjectionBatch<'a> {
    projection: &'a ProjectionDb,
    stage: &'static str,
    tx: Option<rusqlite::Transaction<'a>>,
    chains: usize,
}

impl<'a> ProjectionBatch<'a> {
    fn new(projection: &'a ProjectionDb, stage: &'static str) -> Self {
        Self {
            projection,
            stage,
            tx: None,
            chains: 0,
        }
    }

    fn ensure_started(&mut self) -> Result<()> {
        if self.tx.is_none() {
            // Projection rebuild code reachable under this batch must
            // not start its own transaction; rusqlite/SQLite will reject nested
            // BEGINs on this connection. Keep sync-job immediate transactions
            // out of this path.
            self.tx = Some(
                self.projection
                    .connection()
                    .unchecked_transaction()
                    .with_context(|| {
                        format!("failed to begin projection {} transaction", self.stage)
                    })?,
            );
        }
        Ok(())
    }

    fn note_chain(&mut self) -> Result<()> {
        self.chains += 1;
        if self.chains >= REBUILD_TX_CHAIN_BATCH {
            self.commit_partial()?;
        }
        Ok(())
    }

    fn commit_partial(&mut self) -> Result<()> {
        if let Some(tx) = self.tx.take() {
            tx.commit().with_context(|| {
                format!("failed to commit projection {} transaction", self.stage)
            })?;
            self.chains = 0;
        }
        Ok(())
    }
}

/// Internal report for a single chain rebuild.
struct ChainReport {
    threads: usize,
    events: usize,
}

/// Rebuild a single chain's projection by walking chain_state history
/// from the given head hash toward earlier links via prev_chain_state_hash.
///
/// Projects all thread snapshots and durable events found along the way.
#[cfg(test)]
fn rebuild_chain(
    projection: &ProjectionDb,
    cas_root: &Path,
    chain_root_id: &str,
    head_hash: &str,
) -> Result<ChainReport> {
    let cas = lillux::CasStore::new(cas_root.to_path_buf());
    rebuild_chain_with_observer(projection, &cas, chain_root_id, head_hash, None)
}

fn rebuild_chain_with_observer(
    projection: &ProjectionDb,
    cas: &lillux::CasStore,
    chain_root_id: &str,
    head_hash: &str,
    observer: Option<&dyn ProjectionRecoveryObserver>,
) -> Result<ChainReport> {
    ensure_not_cancelled(observer)?;
    // A full repair is replacement, never an overlay. This also makes a
    // failed delta-ancestry fallback remove stale derived rows before replay.
    projection.delete_chain_projection_in_transaction(chain_root_id)?;
    let mut report = ChainReport {
        threads: 0,
        events: 0,
    };

    // Load the complete ChainState history head → oldest. Replay must never
    // translate an unreadable or malformed authoritative link into an early
    // end-of-history: doing so could still publish the head cursor over an
    // incomplete materialized view.
    let mut states = Vec::new();
    let mut current_hash = head_hash.to_string();
    let mut visited: HashSet<String> = HashSet::new();

    loop {
        ensure_not_cancelled(observer)?;
        if !visited.insert(current_hash.clone()) {
            anyhow::bail!("cycle in ChainState history for {chain_root_id} at {current_hash}");
        }
        let state = load_chain_state(cas, &current_hash, chain_root_id)
            .with_context(|| format!("load ChainState history link {current_hash}"))?;
        let previous = state.prev_chain_state_hash.clone();
        states.push((current_hash, state));
        match previous {
            Some(previous) => current_hash = previous,
            None => break,
        }
    }

    // Reverse: oldest → newest so we project in chronological order
    states.reverse();

    // Walk chain states in order and project thread snapshots.
    for (state_hash, state) in &states {
        ensure_not_cancelled(observer)?;
        for (thread_id, entry) in &state.threads {
            ensure_not_cancelled(observer)?;
            let snapshot = load_thread_snapshot(cas, &entry.snapshot_hash).with_context(|| {
                format!(
                    "load snapshot {} for thread {thread_id} from ChainState {state_hash}",
                    entry.snapshot_hash
                )
            })?;
            validate_snapshot_entry(state, thread_id, entry, &snapshot).with_context(|| {
                format!(
                    "validate snapshot {} for thread {thread_id} from ChainState {state_hash}",
                    entry.snapshot_hash
                )
            })?;
            // Earlier snapshots are intentionally overwritten by later states.
            project_thread_snapshot(projection, &snapshot, chain_root_id)?;
        }
    }

    // For events, walk from each thread's last_event_hash toward earlier links
    // via prev_chain_event_hash to get all events in the chain.
    // But we need the chain-level events, so we walk from chain_state's last_event_hash.
    let (_, head_state) = states
        .last()
        .expect("a full ChainState traversal always contains its head");
    report.events += walk_and_project_events_with_observer(projection, cas, head_state, observer)?;

    // Count unique threads
    let conn = projection.connection();
    let count: usize = conn
        .query_row(
            "SELECT COUNT(DISTINCT thread_id) FROM threads WHERE chain_root_id = ?",
            rusqlite::params![chain_root_id],
            |row| row.get(0),
        )
        .with_context(|| format!("count rebuilt threads for chain {chain_root_id}"))?;
    report.threads = count;

    Ok(report)
}

/// Rebuild only the delta from `from_hash` to `to_hash` for a chain.
#[cfg(test)]
fn rebuild_chain_delta(
    projection: &ProjectionDb,
    cas_root: &Path,
    chain_root_id: &str,
    from_hash: &str,
    to_hash: &str,
) -> Result<ChainReport> {
    let cas = lillux::CasStore::new(cas_root.to_path_buf());
    rebuild_chain_delta_with_observer(projection, &cas, chain_root_id, from_hash, to_hash, None)
}

fn rebuild_chain_delta_with_observer(
    projection: &ProjectionDb,
    cas: &lillux::CasStore,
    chain_root_id: &str,
    from_hash: &str,
    to_hash: &str,
    observer: Option<&dyn ProjectionRecoveryObserver>,
) -> Result<ChainReport> {
    ensure_not_cancelled(observer)?;
    // Collect chain state hashes from `to_hash` toward earlier links until we reach `from_hash`
    let mut states = Vec::new();
    let mut current_hash = to_hash.to_string();
    let mut visited: HashSet<String> = HashSet::new();
    let mut found_from = false;

    loop {
        ensure_not_cancelled(observer)?;
        if current_hash == from_hash {
            found_from = true;
            break;
        }
        if !visited.insert(current_hash.clone()) {
            anyhow::bail!("cycle in ChainState delta for {chain_root_id} at {current_hash}");
        }
        let state = load_chain_state(cas, &current_hash, chain_root_id)
            .with_context(|| format!("load ChainState delta link {current_hash}"))?;
        let previous = state.prev_chain_state_hash.clone();
        states.push((current_hash, state));
        match previous {
            Some(previous) => current_hash = previous,
            None => break,
        }
    }

    if !found_from {
        // Couldn't find from_hash in chain — fall back to full rebuild
        return rebuild_chain_with_observer(projection, cas, chain_root_id, to_hash, observer);
    }

    // Reaching a cursor hash is not enough: its object supplies the exclusive
    // event boundary and must remain readable, canonical, and root-bound.
    let from_state = load_chain_state(cas, from_hash, chain_root_id)
        .with_context(|| format!("load indexed ChainState cursor {from_hash}"))?;

    // Reverse to get chronological order of new states
    states.reverse();

    let mut report = ChainReport {
        threads: 0,
        events: 0,
    };

    // Project thread snapshots from new chain states. Walk NEWEST state
    // first: with the per-thread dedup below, the first snapshot projected
    // per thread must be its latest — walking oldest-first here regresses a
    // row to the delta's earliest state (a thread that went
    // running → completed inside one delta reads back `running`, and the
    // startup reconciler then finalizes a finished thread as failed).
    let mut seen_threads: HashSet<String> = HashSet::new();
    for (state_hash, state) in states.iter().rev() {
        ensure_not_cancelled(observer)?;
        for (thread_id, entry) in &state.threads {
            ensure_not_cancelled(observer)?;
            let snapshot = load_thread_snapshot(cas, &entry.snapshot_hash).with_context(|| {
                format!(
                    "load snapshot {} for thread {thread_id} from delta ChainState {state_hash}",
                    entry.snapshot_hash
                )
            })?;
            validate_snapshot_entry(state, thread_id, entry, &snapshot).with_context(|| {
                format!(
                    "validate snapshot {} for thread {thread_id} from delta ChainState {state_hash}",
                    entry.snapshot_hash
                )
            })?;
            if seen_threads.contains(thread_id) {
                continue;
            }
            project_thread_snapshot(projection, &snapshot, chain_root_id)?;
            seen_threads.insert(thread_id.clone());
            report.threads += 1;
        }
    }

    // Project new events
    if let Some((_, head_state)) = states.last() {
        report.events = match walk_and_project_events_from_with_observer(
            projection,
            cas,
            head_state,
            Some(&from_state),
            observer,
        ) {
            Ok(Some(events)) => events,
            Ok(None) => {
                // The indexed event cursor is not an ancestor of
                // the authoritative head. Replace the chain rather
                // than layering a full replay over stale rows.
                return rebuild_chain_with_observer(
                    projection,
                    cas,
                    chain_root_id,
                    to_hash,
                    observer,
                );
            }
            Err(error) if error.is::<crate::projection::ProjectionEventConflict>() => {
                // A row at the authoritative event identity is not
                // the exact canonical event. Replace every row for
                // the chain before replaying from CAS truth.
                return rebuild_chain_with_observer(
                    projection,
                    cas,
                    chain_root_id,
                    to_hash,
                    observer,
                );
            }
            Err(error) => return Err(error),
        };
    }

    Ok(report)
}

/// Load an exact, canonical ChainState from CAS for projection replay.
fn load_chain_state(
    cas: &lillux::CasStore,
    hash: &str,
    expected_chain_root_id: &str,
) -> Result<ChainState> {
    crate::objects::thread_snapshot::validate_canonical_hash("ChainState hash", hash)?;
    let value = cas
        .get_object(hash)?
        .ok_or_else(|| anyhow::anyhow!("ChainState {hash} is absent"))?;
    let state: ChainState = serde_json::from_value(value)
        .with_context(|| format!("failed to parse ChainState {hash}"))?;
    state
        .validate()
        .with_context(|| format!("invalid ChainState {hash}"))?;
    crate::chain::validate_chain_positions(&state)
        .with_context(|| format!("invalid ChainState cursor positions {hash}"))?;
    let canonical = lillux::canonical_json(&state.to_value())
        .with_context(|| format!("failed to canonicalize ChainState {hash}"))?;
    let canonical_hash = lillux::sha256_hex(canonical.as_bytes());
    if canonical_hash != hash {
        anyhow::bail!(
            "ChainState {hash} is not canonically encoded: canonical hash is {canonical_hash}"
        );
    }
    if state.chain_root_id != expected_chain_root_id {
        anyhow::bail!(
            "ChainState {hash} root mismatch: expected {expected_chain_root_id}, got {}",
            state.chain_root_id
        );
    }
    Ok(state)
}

/// Load an exact, canonical thread snapshot from CAS.
fn load_thread_snapshot(cas: &lillux::CasStore, hash: &str) -> Result<ThreadSnapshot> {
    crate::objects::thread_snapshot::validate_canonical_hash("thread snapshot hash", hash)?;
    let value = cas
        .get_object(hash)?
        .ok_or_else(|| anyhow::anyhow!("thread_snapshot {hash} is absent"))?;
    let snapshot: ThreadSnapshot = serde_json::from_value(value)
        .with_context(|| format!("failed to parse thread_snapshot {hash}"))?;
    snapshot
        .validate()
        .with_context(|| format!("invalid thread_snapshot {hash}"))?;
    let canonical = lillux::canonical_json(&snapshot.to_value())
        .with_context(|| format!("failed to canonicalize thread_snapshot {hash}"))?;
    let canonical_hash = lillux::sha256_hex(canonical.as_bytes());
    if canonical_hash != hash {
        anyhow::bail!(
            "thread_snapshot {hash} is not canonically encoded: canonical hash is {canonical_hash}"
        );
    }
    Ok(snapshot)
}

fn validate_snapshot_entry(
    state: &ChainState,
    thread_id: &str,
    entry: &crate::objects::ChainThreadEntry,
    snapshot: &ThreadSnapshot,
) -> Result<()> {
    crate::chain::validate_stored_snapshot(state, thread_id, entry, snapshot)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReplayThreadCursor {
    next_hash: Option<String>,
    next_sequence: u64,
}

fn walk_and_project_events_with_observer(
    projection: &ProjectionDb,
    cas: &lillux::CasStore,
    head_state: &ChainState,
    observer: Option<&dyn ProjectionRecoveryObserver>,
) -> Result<usize> {
    walk_and_project_events_from_with_observer(projection, cas, head_state, None, observer)?
        .ok_or_else(|| anyhow::anyhow!("full event replay unexpectedly reported a missing cursor"))
}

/// Collect an event delta newest-to-oldest, excluding the exact event position
/// represented by `stop_state`, then project oldest-to-newest. Every requested
/// hash is byte- and canonical-hash checked; root, chain sequence, per-thread
/// sequence/link, timestamp ordering, and the excluded cursor state must all
/// agree before the first projection row is written. `None` means the stop
/// cursor is not an ancestor and leaves projection rows untouched.
#[cfg(test)]
fn walk_and_project_events_from(
    projection: &ProjectionDb,
    cas_root: &Path,
    head_state: &ChainState,
    stop_state: Option<&ChainState>,
) -> Result<Option<usize>> {
    let cas = lillux::CasStore::new(cas_root.to_path_buf());
    walk_and_project_events_from_with_observer(projection, &cas, head_state, stop_state, None)
}

fn walk_and_project_events_from_with_observer(
    projection: &ProjectionDb,
    cas: &lillux::CasStore,
    head_state: &ChainState,
    stop_state: Option<&ChainState>,
    observer: Option<&dyn ProjectionRecoveryObserver>,
) -> Result<Option<usize>> {
    ensure_not_cancelled(observer)?;
    head_state.validate()?;
    crate::chain::validate_chain_positions(head_state)?;
    if let Some(stop_state) = stop_state {
        stop_state.validate()?;
        crate::chain::validate_chain_positions(stop_state)?;
        if stop_state.chain_root_id != head_state.chain_root_id {
            anyhow::bail!(
                "event replay cursor belongs to chain {}, not {}",
                stop_state.chain_root_id,
                head_state.chain_root_id
            );
        }
    }

    let mut events = Vec::new();
    let mut current_hash = head_state.last_event_hash.clone();
    let mut expected_chain_sequence = head_state.last_chain_seq;
    let stop_hash = stop_state.and_then(|state| state.last_event_hash.as_deref());
    let stop_sequence = stop_state.map_or(0, |state| state.last_chain_seq);
    let mut thread_cursors = head_state
        .threads
        .iter()
        .map(|(thread_id, entry)| {
            (
                thread_id.clone(),
                ReplayThreadCursor {
                    next_hash: entry.last_event_hash.clone(),
                    next_sequence: entry.last_thread_seq,
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut visited: HashSet<String> = HashSet::new();
    let mut newer_timestamp = crate::objects::parse_canonical_timestamp(&head_state.updated_at)
        .context("invalid event replay head timestamp")?;

    loop {
        ensure_not_cancelled(observer)?;
        if current_hash.as_deref() == stop_hash {
            if expected_chain_sequence != stop_sequence {
                anyhow::bail!(
                    "event replay cursor sequence mismatch: expected {stop_sequence}, reached {expected_chain_sequence}"
                );
            }
            if let Some(cursor_hash) = stop_hash {
                let cursor_event = load_thread_event(cas, cursor_hash, &head_state.chain_root_id)
                    .context("load excluded event replay cursor")?;
                if cursor_event.chain_seq != stop_sequence {
                    anyhow::bail!(
                        "excluded event cursor {cursor_hash} has chain sequence {}, expected {stop_sequence}",
                        cursor_event.chain_seq
                    );
                }
                let cursor_timestamp = crate::objects::parse_canonical_timestamp(&cursor_event.ts)
                    .with_context(|| format!("invalid excluded event cursor {cursor_hash}"))?;
                if cursor_timestamp > newer_timestamp {
                    anyhow::bail!(
                        "excluded event cursor {cursor_hash} timestamp exceeds the replay delta"
                    );
                }
            }
            validate_event_replay_thread_cursor(&thread_cursors, stop_state)?;
            events.reverse();
            for event in &events {
                ensure_not_cancelled(observer)?;
                project_event(projection, event)?;
            }
            return Ok(Some(events.len()));
        }

        let Some(event_hash) = current_hash.clone() else {
            return Ok(None);
        };
        if expected_chain_sequence == 0 {
            anyhow::bail!("event history continues after chain sequence zero at {event_hash}");
        }
        if !visited.insert(event_hash.clone()) {
            anyhow::bail!("cycle in event history at {event_hash}");
        }

        let event = load_thread_event(cas, &event_hash, &head_state.chain_root_id)?;
        if event.chain_seq != expected_chain_sequence {
            anyhow::bail!(
                "event {event_hash} chain sequence mismatch: expected {expected_chain_sequence}, got {}",
                event.chain_seq
            );
        }
        let event_timestamp = crate::objects::parse_canonical_timestamp(&event.ts)
            .with_context(|| format!("invalid event timestamp for {event_hash}"))?;
        if event_timestamp > newer_timestamp {
            anyhow::bail!("event {event_hash} timestamp exceeds its newer chain position");
        }
        newer_timestamp = event_timestamp;

        let cursor = thread_cursors.get_mut(&event.thread_id).ok_or_else(|| {
            anyhow::anyhow!(
                "event {event_hash} names thread {} absent from the replay head",
                event.thread_id
            )
        })?;
        if cursor.next_hash.as_deref() != Some(event_hash.as_str()) {
            anyhow::bail!(
                "event {event_hash} is not the expected thread link for {}",
                event.thread_id
            );
        }
        if cursor.next_sequence != event.thread_seq {
            anyhow::bail!(
                "event {event_hash} thread sequence mismatch for {}: expected {}, got {}",
                event.thread_id,
                cursor.next_sequence,
                event.thread_seq
            );
        }
        cursor.next_hash = event.prev_thread_event_hash.clone();
        cursor.next_sequence = cursor
            .next_sequence
            .checked_sub(1)
            .ok_or_else(|| anyhow::anyhow!("event {event_hash} underflowed thread sequence"))?;
        current_hash = event.prev_chain_event_hash.clone();
        expected_chain_sequence = expected_chain_sequence
            .checked_sub(1)
            .ok_or_else(|| anyhow::anyhow!("event {event_hash} underflowed chain sequence"))?;
        if event.durability.is_projection_indexed() {
            events.push(event);
        }
    }
}

fn load_thread_event(
    cas: &lillux::CasStore,
    hash: &str,
    expected_chain_root_id: &str,
) -> Result<ThreadEvent> {
    crate::objects::thread_snapshot::validate_canonical_hash("thread event hash", hash)?;
    let value = cas
        .get_object(hash)?
        .ok_or_else(|| anyhow::anyhow!("thread event {hash} is absent"))?;
    let event: ThreadEvent = serde_json::from_value(value)
        .with_context(|| format!("failed to parse thread event {hash}"))?;
    event
        .validate()
        .with_context(|| format!("invalid thread event {hash}"))?;
    let canonical = lillux::canonical_json(&event.to_value())
        .with_context(|| format!("failed to canonicalize thread event {hash}"))?;
    let canonical_hash = lillux::sha256_hex(canonical.as_bytes());
    if canonical_hash != hash {
        anyhow::bail!(
            "thread event {hash} is not canonically encoded: canonical hash is {canonical_hash}"
        );
    }
    if event.chain_root_id != expected_chain_root_id {
        anyhow::bail!(
            "thread event {hash} root mismatch: expected {expected_chain_root_id}, got {}",
            event.chain_root_id
        );
    }
    Ok(event)
}

fn validate_event_replay_thread_cursor(
    cursors: &BTreeMap<String, ReplayThreadCursor>,
    stop_state: Option<&ChainState>,
) -> Result<()> {
    if let Some(stop_state) = stop_state {
        for thread_id in stop_state.threads.keys() {
            if !cursors.contains_key(thread_id) {
                anyhow::bail!(
                    "event replay stop state contains thread {thread_id} absent from the head"
                );
            }
        }
    }
    for (thread_id, cursor) in cursors {
        let (expected_hash, expected_sequence) = stop_state
            .and_then(|state| state.threads.get(thread_id))
            .map_or((None, 0), |entry| {
                (entry.last_event_hash.as_deref(), entry.last_thread_seq)
            });
        if cursor.next_hash.as_deref() != expected_hash || cursor.next_sequence != expected_sequence
        {
            anyhow::bail!(
                "event replay thread cursor mismatch for {thread_id}: expected hash={expected_hash:?} sequence={expected_sequence}, reached hash={:?} sequence={}",
                cursor.next_hash,
                cursor.next_sequence
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::refs::SignedRef;
    use crate::signer::{Signer as _, TestSigner};
    use ryeos_tracing::test as trace_test;

    #[derive(Default)]
    struct RecordingRecoveryObserver {
        updates: std::sync::Mutex<Vec<ProjectionRecoveryProgress>>,
    }

    impl ProjectionRecoveryObserver for RecordingRecoveryObserver {
        fn update(&self, progress: ProjectionRecoveryProgress) {
            self.updates.lock().unwrap().push(progress);
        }
    }

    #[derive(Default)]
    struct CancelAfterInitialCheck {
        checks: std::sync::atomic::AtomicUsize,
    }

    impl ProjectionRecoveryObserver for CancelAfterInitialCheck {
        fn update(&self, _progress: ProjectionRecoveryProgress) {}

        fn is_cancelled(&self) -> bool {
            self.checks
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                > 0
        }
    }

    fn test_trust_store() -> TrustStore {
        let signer = TestSigner::default();
        let mut trust_store = TrustStore::new();
        trust_store.insert(signer.fingerprint().to_string(), signer.verifying_key());
        trust_store
    }

    fn write_object(cas_root: &Path, value: &Value) -> String {
        let canonical = lillux::canonical_json(value).unwrap();
        let hash = lillux::sha256_hex(canonical.as_bytes());
        let path = lillux::shard_path(cas_root, "objects", &hash, ".json");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        lillux::atomic_write(&path, canonical.as_bytes()).unwrap();
        hash
    }

    fn write_raw_object(cas_root: &Path, bytes: &[u8]) -> String {
        let hash = lillux::sha256_hex(bytes);
        let path = lillux::shard_path(cas_root, "objects", &hash, ".json");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        lillux::atomic_write(&path, bytes).unwrap();
        hash
    }

    fn write_signed_head(refs_root: &Path, chain_root_id: &str, target_hash: &str) {
        let signer = TestSigner::default();
        let head_path = refs_root
            .join("generic/chains")
            .join(chain_root_id)
            .join("head");
        let signed_ref = SignedRef::new(
            format!("chains/{chain_root_id}/head"),
            target_hash.to_string(),
            "2026-04-22T00:00:00Z".to_string(),
            signer.fingerprint().to_string(),
        );
        refs::write_signed_ref(&head_path, signed_ref, &signer).unwrap();
    }

    fn write_created_chain(
        cas_root: &Path,
        refs_root: &Path,
        chain_root_id: &str,
        requested_by: Option<&str>,
    ) -> String {
        let mut snapshot = make_snapshot_json(chain_root_id, chain_root_id, "created");
        snapshot["requested_by"] =
            requested_by.map_or(Value::Null, |value| Value::String(value.into()));
        let snapshot_hash = write_object(cas_root, &snapshot);
        let state = make_chain_state(
            chain_root_id,
            None,
            vec![(chain_root_id, &snapshot_hash, None, 0, "created")],
            None,
            0,
        );
        let state_hash = write_object(cas_root, &state);
        write_signed_head(refs_root, chain_root_id, &state_hash);
        state_hash
    }

    fn make_chain_state(
        chain_root_id: &str,
        prev_hash: Option<&str>,
        threads: Vec<(&str, &str, Option<&str>, u64, &str)>,
        last_event_hash: Option<&str>,
        last_chain_seq: u64,
    ) -> Value {
        let mut threads_map = serde_json::Map::new();
        for (tid, snap_hash, evt_hash, thread_seq, status) in threads {
            let entry = serde_json::json!({
                "snapshot_hash": snap_hash,
                "last_event_hash": evt_hash,
                "last_thread_seq": thread_seq,
                "status": status,
            });
            threads_map.insert(tid.to_string(), entry);
        }

        serde_json::json!({
            "kind": "chain_state",
            "schema": 1,
            "chain_root_id": chain_root_id,
            "prev_chain_state_hash": prev_hash,
            "last_event_hash": last_event_hash,
            "last_chain_seq": last_chain_seq,
            "updated_at": "2026-04-22T00:00:00Z",
            "threads": threads_map,
        })
    }

    fn make_snapshot_json(thread_id: &str, chain_root_id: &str, status: &str) -> Value {
        let started = status != "created";
        let terminal = matches!(
            status,
            "completed" | "failed" | "cancelled" | "killed" | "timed_out" | "continued"
        );
        let mut snapshot = serde_json::json!({
            "kind": "thread_snapshot",
            "schema": 2,
            "thread_id": thread_id,
            "chain_root_id": chain_root_id,
            "status": status,
            "kind_name": "directive",
            "item_ref": "directive:test/item",
            "executor_ref": "test/executor",
            "launch_mode": "inline",
            "current_site_id": "site:test",
            "origin_site_id": "site:test",
            "base_project_snapshot_hash": null,
            "result_project_snapshot_hash": null,
            "created_at": "2026-04-22T00:00:00Z",
            "updated_at": "2026-04-22T00:00:00Z",
            "started_at": if started { serde_json::json!("2026-04-22T00:00:00Z") } else { serde_json::Value::Null },
            "finished_at": if terminal { serde_json::json!("2026-04-22T00:00:00Z") } else { serde_json::Value::Null },
            "result": null,
            "outcome_code": null,
            "error": null,
            "budget": null,
            "artifacts": [],
            "facets": {},
            "last_event_hash": null,
            "last_chain_seq": 0,
            "last_thread_seq": 0,
            "upstream_thread_id": null,
            "requested_by": null,
            "project_root": null,
            "captured_history_policy": null,
        });
        if thread_id == chain_root_id {
            snapshot.as_object_mut().unwrap().insert(
                "captured_history_policy".to_string(),
                serde_json::json!({
                    "retention": { "mode": "durable" },
                    "canonical_item_ref": "directive:test/item",
                    "item_content_hash": "11".repeat(32),
                    "item_signer_fingerprint": "22".repeat(32),
                    "item_trust_class": "trusted",
                    "kind_schema_content_hash": "33".repeat(32),
                    "resolved_from": {
                        "node_default": {
                            "node_policy": "missing_config"
                        }
                    },
                }),
            );
        }
        snapshot
    }

    fn make_event_json(
        chain_root_id: &str,
        thread_id: &str,
        chain_seq: u64,
        thread_seq: u64,
        prev_chain: Option<&str>,
        event_type: &str,
    ) -> Value {
        serde_json::json!({
            "kind": "thread_event",
            "schema": 1,
            "chain_root_id": chain_root_id,
            "chain_seq": chain_seq,
            "thread_id": thread_id,
            "thread_seq": thread_seq,
            "event_type": event_type,
            "durability": "durable",
            "ts": "2026-04-22T00:00:00Z",
            "prev_chain_event_hash": prev_chain,
            "prev_thread_event_hash": prev_chain,
            "payload": {}
        })
    }

    fn seed_projection_chain(
        projection: &ProjectionDb,
        cas_root: &Path,
        chain_root_id: &str,
        state_hash: &str,
    ) {
        rebuild_chain(projection, cas_root, chain_root_id, state_hash).unwrap();
        let cas = lillux::CasStore::new(cas_root.to_path_buf());
        let meta = head_projection_meta(&cas, chain_root_id, state_hash).unwrap();
        projection.update_projection_meta(&meta).unwrap();
    }

    fn replay_full_and_advance_cursor(
        projection: &ProjectionDb,
        cas_root: &Path,
        chain_root_id: &str,
        state_hash: &str,
    ) -> Result<()> {
        projection.immediate_transaction("test full replay", || {
            rebuild_chain(projection, cas_root, chain_root_id, state_hash)?;
            let cas = lillux::CasStore::new(cas_root.to_path_buf());
            let meta = head_projection_meta(&cas, chain_root_id, state_hash)?;
            projection.update_projection_meta(&meta)
        })
    }

    fn replay_delta_and_advance_cursor(
        projection: &ProjectionDb,
        cas_root: &Path,
        chain_root_id: &str,
        from_hash: &str,
        to_hash: &str,
    ) -> Result<()> {
        projection.immediate_transaction("test delta replay", || {
            rebuild_chain_delta(projection, cas_root, chain_root_id, from_hash, to_hash)?;
            let cas = lillux::CasStore::new(cas_root.to_path_buf());
            let meta = head_projection_meta(&cas, chain_root_id, to_hash)?;
            projection.update_projection_meta(&meta)
        })
    }

    #[test]
    fn rebuild_projection_single_chain() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let refs_root = tmp.path().join("refs");
        std::fs::create_dir_all(&cas_root).unwrap();
        std::fs::create_dir_all(&refs_root).unwrap();

        let created_snapshot = make_snapshot_json("T-root", "T-root", "created");
        let created_snapshot_hash = write_object(&cas_root, &created_snapshot);
        let genesis = make_chain_state(
            "T-root",
            None,
            vec![("T-root", &created_snapshot_hash, None, 0, "created")],
            None,
            0,
        );
        let genesis_hash = write_object(&cas_root, &genesis);
        let event = make_event_json("T-root", "T-root", 1, 1, None, "thread_created");
        let event_hash = write_object(&cas_root, &event);
        let mut running_snapshot = make_snapshot_json("T-root", "T-root", "running");
        running_snapshot["last_event_hash"] = Value::String(event_hash.clone());
        running_snapshot["last_chain_seq"] = Value::from(1);
        running_snapshot["last_thread_seq"] = Value::from(1);
        let running_snapshot_hash = write_object(&cas_root, &running_snapshot);
        let head = make_chain_state(
            "T-root",
            Some(&genesis_hash),
            vec![(
                "T-root",
                &running_snapshot_hash,
                Some(&event_hash),
                1,
                "running",
            )],
            Some(&event_hash),
            1,
        );
        let head_hash = write_object(&cas_root, &head);
        write_signed_head(&refs_root, "T-root", &head_hash);

        let proj_path = tmp.path().join("projection.db");
        let proj = ProjectionDb::open(&proj_path).unwrap();

        let report = rebuild_projection(&proj, &cas_root, &refs_root, &test_trust_store()).unwrap();
        assert_eq!(report.chains_rebuilt, 1);
        assert_eq!(report.threads_restored, 1);
        assert_eq!(report.events_projected, 1);

        // Verify projection has the thread
        let conn = proj.connection();
        let status: String = conn
            .query_row(
                "SELECT status FROM threads WHERE thread_id = 'T-root'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "running");

        // Verify projection has the event
        let event_count: usize = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE thread_id = 'T-root'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(event_count, 1);
    }

    #[test]
    fn rebuild_projection_multiple_chains() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let refs_root = tmp.path().join("refs");
        std::fs::create_dir_all(&cas_root).unwrap();
        std::fs::create_dir_all(&refs_root).unwrap();

        for chain_id in ["T-a", "T-b"] {
            let snap = make_snapshot_json(chain_id, chain_id, "created");
            let snap_hash = write_object(&cas_root, &snap);
            let cs = make_chain_state(
                chain_id,
                None,
                vec![(chain_id, &snap_hash, None, 0, "created")],
                None,
                0,
            );
            let cs_hash = write_object(&cas_root, &cs);
            write_signed_head(&refs_root, chain_id, &cs_hash);
        }

        let proj_path = tmp.path().join("projection.db");
        let proj = ProjectionDb::open(&proj_path).unwrap();

        let report = rebuild_projection(&proj, &cas_root, &refs_root, &test_trust_store()).unwrap();
        assert_eq!(report.chains_rebuilt, 2);
    }

    #[test]
    fn rebuild_projection_chain_history() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let refs_root = tmp.path().join("refs");
        std::fs::create_dir_all(&cas_root).unwrap();
        std::fs::create_dir_all(&refs_root).unwrap();

        let snap1 = make_snapshot_json("T-root", "T-root", "created");
        let snap1_hash = write_object(&cas_root, &snap1);
        let event = make_event_json("T-root", "T-root", 1, 1, None, "thread_started");
        let event_hash = write_object(&cas_root, &event);
        let mut snap2 = make_snapshot_json("T-root", "T-root", "running");
        snap2["last_event_hash"] = Value::String(event_hash.clone());
        snap2["last_chain_seq"] = Value::from(1);
        snap2["last_thread_seq"] = Value::from(1);
        let snap2_hash = write_object(&cas_root, &snap2);

        let cs1 = make_chain_state(
            "T-root",
            None,
            vec![("T-root", &snap1_hash, None, 0, "created")],
            None,
            0,
        );
        let cs1_hash = write_object(&cas_root, &cs1);

        let cs2 = make_chain_state(
            "T-root",
            Some(&cs1_hash),
            vec![("T-root", &snap2_hash, Some(&event_hash), 1, "running")],
            Some(&event_hash),
            1,
        );
        let cs2_hash = write_object(&cas_root, &cs2);
        write_signed_head(&refs_root, "T-root", &cs2_hash);

        let proj_path = tmp.path().join("projection.db");
        let proj = ProjectionDb::open(&proj_path).unwrap();

        let report = rebuild_projection(&proj, &cas_root, &refs_root, &test_trust_store()).unwrap();
        assert_eq!(report.chains_rebuilt, 1);
        // Thread count = 1 (same thread_id, latest snapshot wins)
        assert_eq!(report.threads_restored, 1);
        assert_eq!(report.events_projected, 1);

        // Verify latest status
        let conn = proj.connection();
        let status: String = conn
            .query_row(
                "SELECT status FROM threads WHERE thread_id = 'T-root'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "running");
    }

    #[test]
    fn full_replay_missing_intermediate_state_preserves_previous_cursor_and_rows() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        std::fs::create_dir_all(&cas_root).unwrap();
        let projection = ProjectionDb::open(&tmp.path().join("projection.db")).unwrap();

        let snapshot = make_snapshot_json("T-root", "T-root", "created");
        let snapshot_hash = write_object(&cas_root, &snapshot);
        let base = make_chain_state(
            "T-root",
            None,
            vec![("T-root", &snapshot_hash, None, 0, "created")],
            None,
            0,
        );
        let base_hash = write_object(&cas_root, &base);
        seed_projection_chain(&projection, &cas_root, "T-root", &base_hash);

        let missing_hash = "ab".repeat(32);
        let head = make_chain_state(
            "T-root",
            Some(&missing_hash),
            vec![("T-root", &snapshot_hash, None, 0, "created")],
            None,
            0,
        );
        let head_hash = write_object(&cas_root, &head);

        let error = replay_full_and_advance_cursor(&projection, &cas_root, "T-root", &head_hash)
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("failed to read ChainState"),
            "unexpected error: {error:#}"
        );
        let cursor = projection.get_projection_meta("T-root").unwrap().unwrap();
        assert_eq!(cursor.indexed_chain_state_hash, base_hash);
        let status: String = projection
            .connection()
            .query_row(
                "SELECT status FROM threads WHERE thread_id = 'T-root'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "created");
    }

    #[test]
    fn full_replay_malformed_intermediate_state_preserves_previous_cursor() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        std::fs::create_dir_all(&cas_root).unwrap();
        let projection = ProjectionDb::open(&tmp.path().join("projection.db")).unwrap();

        let snapshot = make_snapshot_json("T-root", "T-root", "created");
        let snapshot_hash = write_object(&cas_root, &snapshot);
        let base = make_chain_state(
            "T-root",
            None,
            vec![("T-root", &snapshot_hash, None, 0, "created")],
            None,
            0,
        );
        let base_hash = write_object(&cas_root, &base);
        seed_projection_chain(&projection, &cas_root, "T-root", &base_hash);

        let malformed_state_hash = write_raw_object(&cas_root, b"{");
        let head = make_chain_state(
            "T-root",
            Some(&malformed_state_hash),
            vec![("T-root", &snapshot_hash, None, 0, "created")],
            None,
            0,
        );
        let head_hash = write_object(&cas_root, &head);

        let error = replay_full_and_advance_cursor(&projection, &cas_root, "T-root", &head_hash)
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("failed to parse ChainState"),
            "unexpected error: {error:#}"
        );
        let cursor = projection.get_projection_meta("T-root").unwrap().unwrap();
        assert_eq!(cursor.indexed_chain_state_hash, base_hash);
    }

    #[test]
    fn full_replay_malformed_snapshot_preserves_previous_cursor_and_rows() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        std::fs::create_dir_all(&cas_root).unwrap();
        let projection = ProjectionDb::open(&tmp.path().join("projection.db")).unwrap();

        let snapshot = make_snapshot_json("T-root", "T-root", "created");
        let snapshot_hash = write_object(&cas_root, &snapshot);
        let base = make_chain_state(
            "T-root",
            None,
            vec![("T-root", &snapshot_hash, None, 0, "created")],
            None,
            0,
        );
        let base_hash = write_object(&cas_root, &base);
        seed_projection_chain(&projection, &cas_root, "T-root", &base_hash);

        let malformed_snapshot_hash = write_raw_object(&cas_root, b"{");
        let head = make_chain_state(
            "T-root",
            None,
            vec![("T-root", &malformed_snapshot_hash, None, 0, "created")],
            None,
            0,
        );
        let head_hash = write_object(&cas_root, &head);

        let error = replay_full_and_advance_cursor(&projection, &cas_root, "T-root", &head_hash)
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("failed to parse thread_snapshot"),
            "unexpected error: {error:#}"
        );
        let cursor = projection.get_projection_meta("T-root").unwrap().unwrap();
        assert_eq!(cursor.indexed_chain_state_hash, base_hash);
        let status: String = projection
            .connection()
            .query_row(
                "SELECT status FROM threads WHERE thread_id = 'T-root'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "created");
    }

    #[test]
    fn delta_replay_missing_indexed_state_preserves_cursor() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        std::fs::create_dir_all(&cas_root).unwrap();
        let projection = ProjectionDb::open(&tmp.path().join("projection.db")).unwrap();

        let created = make_snapshot_json("T-root", "T-root", "created");
        let created_hash = write_object(&cas_root, &created);
        let base = make_chain_state(
            "T-root",
            None,
            vec![("T-root", &created_hash, None, 0, "created")],
            None,
            0,
        );
        let base_hash = write_object(&cas_root, &base);
        seed_projection_chain(&projection, &cas_root, "T-root", &base_hash);

        let missing_cursor_hash = "cd".repeat(32);
        projection
            .update_projection_meta(&crate::projection::ProjectionMeta {
                chain_root_id: "T-root".to_string(),
                indexed_chain_state_hash: missing_cursor_hash.clone(),
                updated_at: "2026-04-22T00:00:00Z".to_string(),
            })
            .unwrap();
        let running = make_snapshot_json("T-root", "T-root", "running");
        let running_hash = write_object(&cas_root, &running);
        let head = make_chain_state(
            "T-root",
            Some(&missing_cursor_hash),
            vec![("T-root", &running_hash, None, 0, "running")],
            None,
            0,
        );
        let head_hash = write_object(&cas_root, &head);

        let error = replay_delta_and_advance_cursor(
            &projection,
            &cas_root,
            "T-root",
            &missing_cursor_hash,
            &head_hash,
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("load indexed ChainState cursor"),
            "unexpected error: {error:#}"
        );
        let cursor = projection.get_projection_meta("T-root").unwrap().unwrap();
        assert_eq!(cursor.indexed_chain_state_hash, missing_cursor_hash);
        let status: String = projection
            .connection()
            .query_row(
                "SELECT status FROM threads WHERE thread_id = 'T-root'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "created");
    }

    #[test]
    fn rebuild_projection_empty_no_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let refs_root = tmp.path().join("refs");
        std::fs::create_dir_all(&cas_root).unwrap();
        std::fs::create_dir_all(&refs_root).unwrap();

        let proj_path = tmp.path().join("projection.db");
        let proj = ProjectionDb::open(&proj_path).unwrap();

        let report = rebuild_projection(&proj, &cas_root, &refs_root, &test_trust_store()).unwrap();
        assert_eq!(report.chains_rebuilt, 0);
        assert_eq!(report.threads_restored, 0);
        assert_eq!(report.events_projected, 0);
    }

    #[test]
    fn preverified_baseline_accepts_exact_manifest_without_progress_reset() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let refs_root = tmp.path().join("refs");
        std::fs::create_dir_all(&cas_root).unwrap();
        std::fs::create_dir_all(&refs_root).unwrap();
        write_created_chain(&cas_root, &refs_root, "T-root", None);

        let projection = ProjectionDb::open(&tmp.path().join("projection.db")).unwrap();
        let trust_store = test_trust_store();
        let guard = crate::recovery::CasMutationGuard::acquire_exclusive(tmp.path()).unwrap();
        let observer = RecordingRecoveryObserver::default();
        let (report, baseline) = rebuild_projection_with_verified_baseline(
            &projection,
            &cas_root,
            &refs_root,
            &trust_store,
            Some(&observer),
        )
        .unwrap();
        assert_eq!(report.chains_rebuilt, 1);
        let updates_before_verification = observer.updates.lock().unwrap().len();

        let (verification, fingerprints) = verify_preverified_projection(
            &projection,
            &cas_root,
            &refs_root,
            &trust_store,
            &baseline,
            &guard,
            Some(&observer),
        )
        .unwrap();

        assert_eq!(verification.signed_heads_verified, 1);
        assert_eq!(verification.projection_cursors_verified, 1);
        assert_eq!(
            verification.projection_tables_verified,
            HEAD_DERIVED_PROJECTION_TABLES.len()
        );
        assert_eq!(fingerprints.len(), HEAD_DERIVED_PROJECTION_TABLES.len());
        assert_eq!(
            observer.updates.lock().unwrap().len(),
            updates_before_verification,
            "internal expected-row rederivation must not publish a second rebuild cycle"
        );
    }

    #[test]
    fn preverified_baseline_rejects_changed_missing_and_extra_trusted_heads() {
        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let refs_root = tmp.path().join("refs");
        std::fs::create_dir_all(&cas_root).unwrap();
        std::fs::create_dir_all(&refs_root).unwrap();
        let original_hash = write_created_chain(&cas_root, &refs_root, "T-root", None);

        let projection = ProjectionDb::open(&tmp.path().join("projection.db")).unwrap();
        let trust_store = test_trust_store();
        let guard = crate::recovery::CasMutationGuard::acquire_exclusive(tmp.path()).unwrap();
        let (_, baseline) = rebuild_projection_with_verified_baseline(
            &projection,
            &cas_root,
            &refs_root,
            &trust_store,
            None,
        )
        .unwrap();

        write_created_chain(&cas_root, &refs_root, "T-root", Some("changed-head"));
        let changed = verify_preverified_projection(
            &projection,
            &cas_root,
            &refs_root,
            &trust_store,
            &baseline,
            &guard,
            None,
        )
        .unwrap_err();
        assert!(format!("{changed:#}").contains("signed head mismatch"));

        write_signed_head(&refs_root, "T-root", &original_hash);
        std::fs::remove_file(refs_root.join("generic/chains/T-root/head")).unwrap();
        let missing = verify_preverified_projection(
            &projection,
            &cas_root,
            &refs_root,
            &trust_store,
            &baseline,
            &guard,
            None,
        )
        .unwrap_err();
        assert!(format!("{missing:#}").contains("signed head set changed"));

        write_signed_head(&refs_root, "T-root", &original_hash);
        write_created_chain(&cas_root, &refs_root, "T-extra", None);
        let extra = verify_preverified_projection(
            &projection,
            &cas_root,
            &refs_root,
            &trust_store,
            &baseline,
            &guard,
            None,
        )
        .unwrap_err();
        assert!(format!("{extra:#}").contains("signed head set changed"));
    }

    #[test]
    fn sqlite_verification_watcher_returns_canonical_cancellation() {
        let projection = ProjectionDb::open_transient().unwrap();
        let observer = CancelAfterInitialCheck::default();

        let error = with_projection_sqlite_cancellation(&projection, Some(&observer), || {
            while observer.checks.load(std::sync::atomic::Ordering::SeqCst) < 2 {
                std::thread::yield_now();
            }
            Ok(())
        })
        .unwrap_err();

        assert_eq!(error.to_string(), "projection recovery cancelled");
    }

    #[test]
    fn event_replay_is_chronological_and_delta_excludes_its_cursor() {
        use crate::objects::thread_event::NewEvent;
        use crate::objects::{ChainStateBuilder, ChainThreadEntry, ThreadStatus};

        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("cas");
        let projection = ProjectionDb::open(&tmp.path().join("projection.db")).unwrap();
        let older = NewEvent::new("T-root", "T-root", crate::event_types::THREAD_FACET_SET)
            .chain_seq(1)
            .thread_seq(1)
            .payload(serde_json::json!({"key": "phase", "value": "old"}))
            .build_with_ts("2026-04-22T00:00:01Z".to_string());
        let older_hash = write_object(&cas_root, &older.to_value());
        let newer = NewEvent::new("T-root", "T-root", crate::event_types::THREAD_FACET_SET)
            .chain_seq(2)
            .thread_seq(2)
            .prev_chain_event_hash(Some(older_hash.clone()))
            .prev_thread_event_hash(Some(older_hash.clone()))
            .payload(serde_json::json!({"key": "phase", "value": "new"}))
            .build_with_ts("2026-04-22T00:00:02Z".to_string());
        let newer_hash = write_object(&cas_root, &newer.to_value());
        let snapshot_hash = "0".repeat(64);
        let older_state = ChainStateBuilder::new("T-root")
            .last_event_hash(Some(older_hash.clone()))
            .last_chain_seq(1)
            .updated_at("2026-04-22T00:00:01Z".to_string())
            .thread(
                "T-root",
                ChainThreadEntry {
                    snapshot_hash: snapshot_hash.clone(),
                    last_event_hash: Some(older_hash.clone()),
                    last_thread_seq: 1,
                    status: ThreadStatus::Running,
                },
            )
            .build();
        let newer_state = ChainStateBuilder::new("T-root")
            .last_event_hash(Some(newer_hash.clone()))
            .last_chain_seq(2)
            .updated_at("2026-04-22T00:00:02Z".to_string())
            .thread(
                "T-root",
                ChainThreadEntry {
                    snapshot_hash,
                    last_event_hash: Some(newer_hash.clone()),
                    last_thread_seq: 2,
                    status: ThreadStatus::Running,
                },
            )
            .build();

        project_event(&projection, &older).unwrap();
        assert_eq!(
            walk_and_project_events_from(&projection, &cas_root, &newer_state, Some(&older_state),)
                .unwrap(),
            Some(1)
        );
        // Replaying the same delta cannot duplicate its base/derived rows.
        walk_and_project_events_from(&projection, &cas_root, &newer_state, Some(&older_state))
            .unwrap();

        let value: Vec<u8> = projection
            .connection()
            .query_row(
                "SELECT value FROM thread_facets WHERE thread_id='T-root' AND key='phase'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let events: i64 = projection
            .connection()
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(value, b"new");
        assert_eq!(events, 2);
    }

    // ── Trace-capture tests ──────────────────────────────────────

    #[test]
    fn rebuild_projection_emits_span() {
        let tempdir = tempfile::tempdir().unwrap();
        let cas_root = tempdir.path().join("cas");
        let refs_root = tempdir.path().join("refs");
        let proj_path = tempdir.path().join("projection.db");
        let proj = ProjectionDb::open(&proj_path).unwrap();

        // Empty rebuild — still should emit the span
        let (_, spans) = trace_test::capture_traces(|| {
            let _ = rebuild_projection(&proj, &cas_root, &refs_root, &test_trust_store());
        });

        let span = trace_test::find_span(&spans, "state:rebuild");
        assert!(
            span.is_some(),
            "expected state:rebuild span, got: {:?}",
            spans
                .iter()
                .map(|s: &ryeos_tracing::test::RecordedSpan| &s.name)
                .collect::<Vec<_>>()
        );
    }
}
