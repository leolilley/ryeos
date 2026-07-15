//! High-level state database wrapping ProjectionDb + CAS paths + HeadCache.
//!
//! The write-path contract is **CAS-first**: if CAS succeeds but projection
//! fails, the method logs a warning and returns `Ok`. Projection drift is
//! repaired by the startup reconcile pass.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Context;

use crate::bundle_events::{
    self, BundleEventAppendRequest, BundleEventAppendResult, BundleEventChainPage,
    BundleEventRecord, BundleEventScanCursor, BundleEventScanPage,
};
use crate::bundle_projection::BundleProjectionDb;
use crate::chain::{self, AddThreadWithEventsResult, AppendResult, CreateResult, SnapshotUpdate};
use crate::head_cache::HeadCache;
use crate::objects::bundle_event::validate_bundle_identifier;
use crate::objects::ThreadEvent;
use crate::objects::ThreadSnapshot;
use crate::projection::{
    self, project_committed_chain, AdmissionAttestationRecord, CasEntriesByStateSummary,
    CasEntryAttribution, CasEntryState, NewAdmissionAttestationRecord, NewCasEntryAttribution,
    NewSyncJob, NewSyncJobAttempt, ProjectionDb, ProjectionOpenResult, SyncJobAttemptRecord,
    SyncJobRecord, SyncJobState, SyncJobUpdate,
};
use crate::queries;
use crate::refs::{GenericHeadRef, SignedRef};
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

#[derive(Debug, Default)]
struct PendingProjectionRepairs {
    requests: Mutex<Vec<ProjectionRepairRequest>>,
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
    cas_root: PathBuf,
    refs_root: PathBuf,
    locators_root: PathBuf,
    projection: ProjectionDb,
    head_cache: Mutex<HeadCache>,
    projection_repair_sink: Arc<dyn ProjectionRepairSink>,
}

impl StateDb {
    /// Open (or create) a state database rooted at `runtime_state_dir`.
    ///
    /// Creates `objects/`, `refs/`, and `locators/` subdirectories and opens
    /// `projection.sqlite3` inside `runtime_state_dir`.
    pub fn open(runtime_state_dir: &Path) -> anyhow::Result<Self> {
        Self::open_with_projection_repair_sink(
            runtime_state_dir,
            Arc::new(PendingProjectionRepairs::default()),
        )
    }

    /// Open a state database with a caller-provided projection repair sink.
    pub fn open_with_projection_repair_sink(
        runtime_state_dir: &Path,
        projection_repair_sink: Arc<dyn ProjectionRepairSink>,
    ) -> anyhow::Result<Self> {
        let cas_root = runtime_state_dir.join("objects");
        let refs_root = runtime_state_dir.join("refs");
        let locators_root = runtime_state_dir.join("locators");
        let projection_path = runtime_state_dir.join("projection.sqlite3");

        std::fs::create_dir_all(&cas_root).context("creating objects root")?;
        std::fs::create_dir_all(&refs_root).context("creating refs root")?;
        std::fs::create_dir_all(&locators_root).context("creating locators root")?;

        let ProjectionOpenResult {
            db: projection,
            reset,
        } = ProjectionDb::open_with_status(&projection_path)
            .context("opening projection database")?;
        if reset {
            let report = crate::rebuild::rebuild_projection(&projection, &cas_root, &refs_root)
                .context("rebuilding projection after schema epoch reset")?;
            tracing::info!(
                path = %projection_path.display(),
                chains_rebuilt = report.chains_rebuilt,
                threads_restored = report.threads_restored,
                events_projected = report.events_projected,
                "projection rebuilt after schema epoch reset"
            );
        } else {
            let report = crate::rebuild::catch_up_projection(&projection, &cas_root, &refs_root)
                .context("catching projection up to authoritative chain heads")?;
            if report.chains_updated > 0 {
                tracing::info!(
                    chains_checked = report.chains_checked,
                    chains_updated = report.chains_updated,
                    threads_restored = report.threads_restored,
                    events_projected = report.events_projected,
                    "projection caught up at startup"
                );
            }
        }

        Ok(Self {
            cas_root,
            refs_root,
            locators_root,
            projection,
            head_cache: Mutex::new(HeadCache::new()),
            projection_repair_sink,
        })
    }

    fn committed_write<T>(
        &self,
        value: T,
        chain_root_id: &str,
        committed_head_hash: &str,
        operation: &'static str,
        projected: anyhow::Result<()>,
    ) -> CommittedWrite<T> {
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

    /// Access the underlying projection database for queries.
    pub fn projection(&self) -> &ProjectionDb {
        &self.projection
    }

    /// Catch the rebuildable thread projection up to all authoritative heads.
    pub fn catch_up_projection(&self) -> anyhow::Result<crate::rebuild::CatchUpReport> {
        crate::rebuild::catch_up_projection(&self.projection, &self.cas_root, &self.refs_root)
    }

    /// CAS objects root (`runtime_state_dir/objects`).
    pub fn cas_root(&self) -> &Path {
        &self.cas_root
    }

    /// Refs root (`runtime_state_dir/refs`).
    pub fn refs_root(&self) -> &Path {
        &self.refs_root
    }

    /// Locators root (`runtime_state_dir/locators`).
    pub fn locators_root(&self) -> &Path {
        &self.locators_root
    }

    /// Create a new execution chain with a root thread.
    ///
    /// Delegates to [`chain::create_chain`] then projects the thread snapshot
    /// into SQLite. If CAS succeeds but projection fails, logs a warning and
    /// returns `Ok` — projection drift is fixed by startup reconcile.
    pub fn create_chain(
        &self,
        chain_root_id: &str,
        snapshot: ThreadSnapshot,
        signer: &dyn Signer,
    ) -> anyhow::Result<CommittedWrite<CreateResult>> {
        let mut cache = self.head_cache.lock().expect("head_cache lock");
        let result = chain::create_chain(
            &self.cas_root,
            &self.refs_root,
            chain_root_id,
            snapshot.clone(),
            signer,
            &mut cache,
        )?;
        let projected = project_committed_chain(
            &self.projection,
            &cache,
            chain_root_id,
            &result.chain_state_hash,
            || {
                projection::project_thread_snapshot(&self.projection, &snapshot, chain_root_id)
                    .context("projecting created thread snapshot")
            },
        );
        let committed_hash = result.chain_state_hash.clone();
        Ok(self.committed_write(
            result,
            chain_root_id,
            &committed_hash,
            "create_chain",
            projected,
        ))
    }

    /// Create a root snapshot and its initial durable events in one head update
    /// and one projection transaction.
    pub fn create_chain_with_events(
        &self,
        chain_root_id: &str,
        snapshot: ThreadSnapshot,
        events: Vec<ThreadEvent>,
        signer: &dyn Signer,
    ) -> anyhow::Result<CommittedWrite<AddThreadWithEventsResult>> {
        if events.iter().any(|event| !event.durability.is_cas_stored()) {
            anyhow::bail!("StateDb::create_chain_with_events cannot persist ephemeral events");
        }
        let mut cache = self.head_cache.lock().expect("head_cache lock");
        let result = chain::create_chain_with_events(
            &self.cas_root,
            &self.refs_root,
            chain_root_id,
            snapshot.clone(),
            events,
            signer,
            &mut cache,
        )?;
        let projected = project_committed_chain(
            &self.projection,
            &cache,
            chain_root_id,
            &result.chain_state_hash,
            || {
                projection::project_thread_snapshot_with_events_in_transaction(
                    &self.projection,
                    &snapshot,
                    chain_root_id,
                    &result.events,
                )
                .context("projecting root thread and initial events")
            },
        );
        let committed_hash = result.chain_state_hash.clone();
        Ok(self.committed_write(
            result,
            chain_root_id,
            &committed_hash,
            "create_chain_with_events",
            projected,
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
        let mut cache = self.head_cache.lock().expect("head_cache lock");
        let result = chain::add_thread_to_chain(
            &self.cas_root,
            &self.refs_root,
            chain_root_id,
            snapshot.clone(),
            signer,
            &mut cache,
        )?;
        let projected = project_committed_chain(
            &self.projection,
            &cache,
            chain_root_id,
            &result.chain_state_hash,
            || {
                projection::project_thread_snapshot(&self.projection, &snapshot, chain_root_id)
                    .context("projecting added thread snapshot")
            },
        );
        let committed_hash = result.chain_state_hash.clone();
        Ok(self.committed_write(
            result,
            chain_root_id,
            &committed_hash,
            "add_thread",
            projected,
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
        if events.iter().any(|event| !event.durability.is_cas_stored()) {
            anyhow::bail!("StateDb::append_events cannot persist ephemeral events");
        }

        let mut cache = self.head_cache.lock().expect("head_cache lock");
        let result = chain::append_events(
            chain::AppendEventsInput {
                cas_root: &self.cas_root,
                refs_root: &self.refs_root,
                chain_root_id,
                thread_id,
                events,
                snapshot_updates: snapshot_updates.clone(),
                signer,
            },
            &mut cache,
        )?;

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

                for update in &snapshot_updates {
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
        Ok(self.committed_write(
            result,
            chain_root_id,
            &committed_hash,
            "append_events",
            projected,
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
        if events.iter().any(|event| !event.durability.is_cas_stored()) {
            anyhow::bail!("StateDb::add_thread_with_events cannot persist ephemeral events");
        }

        let mut cache = self.head_cache.lock().expect("head_cache lock");
        let result = chain::add_thread_to_chain_with_events(
            &self.cas_root,
            &self.refs_root,
            chain_root_id,
            snapshot.clone(),
            events,
            signer,
            &mut cache,
        )?;

        let projected = project_committed_chain(
            &self.projection,
            &cache,
            chain_root_id,
            &result.chain_state_hash,
            || {
                projection::project_thread_snapshot_with_events_in_transaction(
                    &self.projection,
                    &snapshot,
                    chain_root_id,
                    &result.events,
                )
                .context("projecting thread and initial events")
            },
        );

        let committed_hash = result.chain_state_hash.clone();
        Ok(self.committed_write(
            result,
            chain_root_id,
            &committed_hash,
            "add_thread_with_events",
            projected,
        ))
    }

    /// Add a thread and append events to an existing thread under one signed
    /// chain head, then project the entire cross-thread transition in one
    /// projection transaction.
    #[allow(clippy::too_many_arguments)]
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
        if new_thread_events
            .iter()
            .chain(existing_thread_events.iter())
            .any(|event| !event.durability.is_cas_stored())
        {
            anyhow::bail!(
                "StateDb::add_thread_with_events_and_append cannot persist ephemeral events"
            );
        }

        let mut cache = self.head_cache.lock().expect("head_cache lock");
        let result = chain::add_thread_to_chain_with_events_and_append(
            &self.cas_root,
            &self.refs_root,
            chain_root_id,
            snapshot.clone(),
            new_thread_events,
            existing_thread_id,
            existing_thread_events,
            snapshot_updates.clone(),
            signer,
            &mut cache,
        )?;
        let projected = project_committed_chain(
            &self.projection,
            &cache,
            chain_root_id,
            &result.chain_state_hash,
            || {
                projection::project_thread_snapshot_with_events_in_transaction(
                    &self.projection,
                    &snapshot,
                    chain_root_id,
                    &result.events,
                )
                .context("projecting atomic new-thread and existing-thread events")?;
                for update in &snapshot_updates {
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
        Ok(self.committed_write(
            result,
            chain_root_id,
            &committed_hash,
            "add_thread_with_events_and_append",
            projected,
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
        crate::refs::read_project_head_ref(&self.refs_root, principal_key, project_hash)
    }

    /// Write a principal-scoped project head ref.
    pub fn write_project_head_ref(
        &self,
        principal_key: &str,
        project_hash: &str,
        project_snapshot_hash: &str,
        signer: &dyn Signer,
    ) -> anyhow::Result<()> {
        crate::refs::write_project_head_ref(
            &self.refs_root,
            principal_key,
            project_hash,
            project_snapshot_hash,
            signer,
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
        crate::refs::advance_project_head_ref(
            &self.refs_root,
            principal_key,
            project_hash,
            new_snapshot_hash,
            expected_current_hash,
            signer,
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
        crate::refs::write_generic_head_ref(&self.refs_root, namespace, name, target_hash, signer)
    }

    /// Read a namespace-neutral signed head from `refs/generic`.
    pub fn read_generic_head_ref(
        &self,
        namespace: &str,
        name: &str,
    ) -> anyhow::Result<Option<SignedRef>> {
        crate::refs::read_generic_head_ref(&self.refs_root, namespace, name)
    }

    pub fn terminal_service_chain_candidates(&self, cutoff: &str) -> anyhow::Result<Vec<String>> {
        let mut stmt = self.projection.connection().prepare(
            "SELECT root.chain_root_id FROM threads root
             WHERE root.thread_id = root.chain_root_id
               AND root.kind = 'service_run'
               AND root.finished_at IS NOT NULL AND root.finished_at < ?1
               AND NOT EXISTS (
                   SELECT 1 FROM threads member
                   WHERE member.chain_root_id = root.chain_root_id
                     AND (
                       member.status NOT IN ('completed','failed','cancelled','killed','timed_out','continued')
                       OR member.updated_at >= ?1
                     )
               )
             ORDER BY root.finished_at",
        )?;
        let rows = stmt.query_map([cutoff], |row| row.get(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn terminal_service_chain_is_retirable(
        &self,
        chain_root_id: &str,
        cutoff: &str,
    ) -> anyhow::Result<bool> {
        Ok(self.projection.connection().query_row(
            "SELECT EXISTS(
                SELECT 1 FROM threads root
                WHERE root.thread_id=?1 AND root.chain_root_id=?1 AND root.kind='service_run'
                  AND root.finished_at IS NOT NULL AND root.finished_at < ?2
                  AND NOT EXISTS (
                    SELECT 1 FROM threads member WHERE member.chain_root_id=?1
                    AND (
                      member.status NOT IN ('completed','failed','cancelled','killed','timed_out','continued')
                      OR member.updated_at >= ?2
                    )
                  )
             )",
            [chain_root_id, cutoff],
            |row| row.get(0),
        )?)
    }

    pub fn remove_chain_head_ref(&self, chain_root_id: &str) -> anyhow::Result<bool> {
        let removed =
            crate::refs::remove_generic_head_ref(&self.refs_root, "chains", chain_root_id)?;
        self.head_cache
            .lock()
            .expect("head_cache lock")
            .invalidate(chain_root_id);
        Ok(removed)
    }

    pub fn delete_chain_projection(&self, chain_root_id: &str) -> anyhow::Result<usize> {
        let conn = self.projection.connection();
        conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| {
            let thread_ids = "SELECT thread_id FROM threads WHERE chain_root_id=?1";
            let mut deleted = 0usize;
            deleted += conn.execute(
                &format!("DELETE FROM event_replay_index WHERE thread_id IN ({thread_ids})"),
                [chain_root_id],
            )?;
            deleted +=
                conn.execute("DELETE FROM events WHERE chain_root_id=?1", [chain_root_id])?;
            deleted += conn.execute(
                "DELETE FROM thread_edges WHERE chain_root_id=?1",
                [chain_root_id],
            )?;
            deleted += conn.execute(
                "DELETE FROM thread_results WHERE chain_root_id=?1",
                [chain_root_id],
            )?;
            deleted += conn.execute(
                "DELETE FROM thread_artifacts WHERE chain_root_id=?1",
                [chain_root_id],
            )?;
            deleted += conn.execute(
                &format!("DELETE FROM thread_facets WHERE thread_id IN ({thread_ids})"),
                [chain_root_id],
            )?;
            deleted += conn.execute(
                "DELETE FROM thread_usage_latest WHERE chain_root_id=?1",
                [chain_root_id],
            )?;
            deleted += conn.execute(
                "DELETE FROM thread_usage_subjects WHERE chain_root_id=?1",
                [chain_root_id],
            )?;
            deleted += conn.execute(
                "DELETE FROM projection_meta WHERE chain_root_id=?1",
                [chain_root_id],
            )?;
            deleted += conn.execute(
                "DELETE FROM threads WHERE chain_root_id=?1",
                [chain_root_id],
            )?;
            Ok::<_, rusqlite::Error>(deleted)
        })();
        match result {
            Ok(deleted) => {
                conn.execute_batch("COMMIT")?;
                Ok(deleted)
            }
            Err(err) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(err.into())
            }
        }
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
        crate::refs::advance_generic_head_ref(
            &self.refs_root,
            namespace,
            name,
            new_target_hash,
            expected_current_hash,
            signer,
        )
    }

    /// List namespace-neutral signed heads beneath `refs/generic/<prefix>`.
    pub fn list_generic_head_refs(&self, prefix: &str) -> anyhow::Result<Vec<GenericHeadRef>> {
        crate::refs::list_generic_head_refs(&self.refs_root, prefix)
    }

    // ── Bundle event chains ────────────────────────────────

    pub fn append_bundle_event(
        &self,
        request: BundleEventAppendRequest,
        signer: &dyn Signer,
    ) -> anyhow::Result<BundleEventAppendResult> {
        bundle_events::append_bundle_event(&self.cas_root, &self.refs_root, request, signer)
    }

    pub fn read_bundle_event_chain(
        &self,
        bundle_id: &str,
        event_kind: &str,
        chain_id: &str,
    ) -> anyhow::Result<Vec<BundleEventRecord>> {
        bundle_events::read_bundle_event_chain(
            &self.cas_root,
            &self.refs_root,
            bundle_id,
            event_kind,
            chain_id,
        )
    }

    pub fn scan_bundle_events(
        &self,
        bundle_id: &str,
        event_kind: &str,
    ) -> anyhow::Result<Vec<BundleEventRecord>> {
        bundle_events::scan_bundle_events(&self.cas_root, &self.refs_root, bundle_id, event_kind)
    }

    pub fn read_bundle_event_chain_page(
        &self,
        bundle_id: &str,
        event_kind: &str,
        chain_id: &str,
        cursor: Option<&str>,
        limit: usize,
        max_serialized_bytes: usize,
    ) -> anyhow::Result<BundleEventChainPage> {
        bundle_events::read_bundle_event_chain_page(
            &self.cas_root,
            &self.refs_root,
            bundle_id,
            event_kind,
            chain_id,
            cursor,
            limit,
            max_serialized_bytes,
        )
    }

    pub fn scan_bundle_events_page(
        &self,
        bundle_id: &str,
        event_kind: &str,
        cursor: Option<&BundleEventScanCursor>,
        limit: usize,
        max_serialized_bytes: usize,
    ) -> anyhow::Result<BundleEventScanPage> {
        bundle_events::scan_bundle_events_page(
            &self.cas_root,
            &self.refs_root,
            bundle_id,
            event_kind,
            cursor,
            limit,
            max_serialized_bytes,
        )
    }

    pub fn open_bundle_projection(
        &self,
        projection_name: &str,
    ) -> anyhow::Result<BundleProjectionDb> {
        validate_bundle_identifier("projection_name", projection_name)?;
        let runtime_state_dir = self.cas_root.parent().ok_or_else(|| {
            anyhow::anyhow!("CAS root has no parent: {}", self.cas_root.display())
        })?;
        let projection_root = runtime_state_dir.join("bundle-projections");
        fs::create_dir_all(&projection_root).context("creating bundle projection root")?;
        BundleProjectionDb::open(&projection_root.join(format!("{projection_name}.sqlite3")))
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
        self.projection.record_cas_entry(entry)
    }

    pub fn set_cas_entry_state(
        &self,
        entry_kind: crate::projection::CasEntryKind,
        hash: &str,
        state: CasEntryState,
    ) -> anyhow::Result<()> {
        self.projection.set_cas_entry_state(entry_kind, hash, state)
    }

    pub fn get_cas_entry(
        &self,
        entry_kind: crate::projection::CasEntryKind,
        hash: &str,
    ) -> anyhow::Result<Option<CasEntryAttribution>> {
        self.projection.get_cas_entry(entry_kind, hash)
    }

    pub fn list_cas_entries_by_state(
        &self,
        state: CasEntryState,
    ) -> anyhow::Result<Vec<CasEntryAttribution>> {
        self.projection.list_cas_entries_by_state(state)
    }

    pub fn cas_entries_by_state_summary(&self) -> anyhow::Result<Vec<CasEntriesByStateSummary>> {
        self.projection.cas_entries_by_state_summary()
    }

    pub fn record_admission_attestation(
        &self,
        record: &NewAdmissionAttestationRecord,
    ) -> anyhow::Result<()> {
        self.projection.record_admission_attestation(record)
    }

    pub fn list_admission_attestations_for_subject(
        &self,
        subject_hash: &str,
        policy: Option<&str>,
    ) -> anyhow::Result<Vec<AdmissionAttestationRecord>> {
        self.projection
            .list_admission_attestations_for_subject(subject_hash, policy)
    }

    pub fn create_sync_job(&self, job: &NewSyncJob) -> anyhow::Result<SyncJobRecord> {
        self.projection.create_sync_job(job)
    }

    pub fn update_sync_job(&self, job_id: &str, update: &SyncJobUpdate) -> anyhow::Result<()> {
        self.projection.update_sync_job(job_id, update)
    }

    pub fn create_sync_job_attempt(
        &self,
        attempt: &NewSyncJobAttempt,
    ) -> anyhow::Result<SyncJobAttemptRecord> {
        self.projection.create_sync_job_attempt(attempt)
    }

    pub fn finish_sync_job_attempt(
        &self,
        attempt_id: &str,
        finish: &crate::projection::FinishSyncJobAttempt,
    ) -> anyhow::Result<()> {
        self.projection.finish_sync_job_attempt(attempt_id, finish)
    }

    pub fn finish_sync_job_attempt_and_update_job(
        &self,
        attempt_id: &str,
        finish: &crate::projection::FinishSyncJobAttempt,
        job_id: &str,
        update: &SyncJobUpdate,
    ) -> anyhow::Result<()> {
        self.projection
            .finish_sync_job_attempt_and_update_job(attempt_id, finish, job_id, update)
    }

    pub fn get_sync_job_attempt(
        &self,
        attempt_id: &str,
    ) -> anyhow::Result<Option<SyncJobAttemptRecord>> {
        self.projection.get_sync_job_attempt(attempt_id)
    }

    pub fn list_sync_job_attempts(
        &self,
        job_id: &str,
    ) -> anyhow::Result<Vec<SyncJobAttemptRecord>> {
        self.projection.list_sync_job_attempts(job_id)
    }

    pub fn get_sync_job(&self, job_id: &str) -> anyhow::Result<Option<SyncJobRecord>> {
        self.projection.get_sync_job(job_id)
    }

    pub fn list_sync_jobs_by_state(
        &self,
        state: Option<SyncJobState>,
        limit: usize,
    ) -> anyhow::Result<Vec<SyncJobRecord>> {
        self.projection.list_sync_jobs_by_state(state, limit)
    }

    pub fn count_active_sync_jobs(&self) -> anyhow::Result<u64> {
        self.projection.count_active_sync_jobs()
    }

    /// Retention: delete terminal sync jobs (and their attempts) finished before
    /// `cutoff_iso`. Returns `(deleted_jobs, deleted_attempts)`.
    pub fn delete_terminal_sync_jobs_before(
        &self,
        cutoff_iso: &str,
    ) -> anyhow::Result<(usize, usize)> {
        self.projection.delete_terminal_sync_jobs_before(cutoff_iso)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::thread_snapshot::ThreadSnapshotBuilder;
    use crate::signer::TestSigner;

    fn open_temp() -> (tempfile::TempDir, StateDb) {
        let dir = tempfile::tempdir().unwrap();
        let db = StateDb::open(dir.path()).unwrap();
        (dir, db)
    }

    #[test]
    fn open_creates_directories() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("state");
        let db = StateDb::open(&root).unwrap();

        assert!(db.cas_root().exists());
        assert!(db.refs_root().exists());
        assert!(db.locators_root().exists());
        drop(db);
    }

    #[test]
    fn create_chain_stores_in_projection() {
        let (_dir, db) = open_temp();
        let signer = TestSigner::default();

        let snapshot = ThreadSnapshotBuilder::new(
            "T-root",
            "T-root",
            "directive",
            "system/test",
            "directive-runtime",
        )
        .build();

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

    #[test]
    fn projection_commit_is_idempotent_at_committed_cursor() {
        let (_dir, db) = open_temp();
        let signer = TestSigner::default();
        let snapshot = ThreadSnapshotBuilder::new(
            "T-root",
            "T-root",
            "directive",
            "system/test",
            "directive-runtime",
        )
        .build();
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
        let snapshot = ThreadSnapshotBuilder::new(
            "T-root",
            "T-root",
            "directive",
            "system/test",
            "directive-runtime",
        )
        .build();
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
        let snapshot = ThreadSnapshotBuilder::new(
            "T-root",
            "T-root",
            "directive",
            "system/test",
            "directive-runtime",
        )
        .build();
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
        let head = crate::refs::read_signed_ref(&db.refs_root().join("generic/chains/T-root/head"))
            .unwrap();
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
        let reopened = StateDb::open(dir.path()).unwrap();
        let projected = reopened
            .projection()
            .get_projection_meta("T-root")
            .unwrap()
            .expect("startup catch-up must restore the projection cursor");
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
        let db = StateDb::open_with_projection_repair_sink(dir.path(), sink.clone()).unwrap();
        let signer = TestSigner::default();
        let snapshot = ThreadSnapshotBuilder::new(
            "T-root",
            "T-root",
            "directive",
            "system/test",
            "directive-runtime",
        )
        .build();
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

        {
            let db = StateDb::open(&root).unwrap();
            let snapshot = ThreadSnapshotBuilder::new(
                "T-root",
                "T-root",
                "directive",
                "system/test",
                "directive-runtime",
            )
            .build();
            db.create_chain("T-root", snapshot, &signer).unwrap();
        }

        let projection_path = root.join("projection.sqlite3");
        let conn = rusqlite::Connection::open(&projection_path).unwrap();
        conn.execute_batch("PRAGMA user_version = 0;").unwrap();
        drop(conn);

        let db = StateDb::open(&root).unwrap();
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

        {
            let db = StateDb::open(&root).unwrap();
            let snapshot = ThreadSnapshotBuilder::new(
                "T-root",
                "T-root",
                "directive",
                "directive:apps/tv-tracker/ai_chat",
                "directive-runtime",
            )
            .build();
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
        }

        let projection_path = root.join("projection.sqlite3");
        let conn = rusqlite::Connection::open(&projection_path).unwrap();
        conn.execute_batch("PRAGMA user_version = 0;").unwrap();
        drop(conn);

        let db = StateDb::open(&root).unwrap();
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

        let snapshot = ThreadSnapshotBuilder::new(
            "T-root",
            "T-root",
            "directive",
            "system/test",
            "directive-runtime",
        )
        .build();

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

        let snapshot = ThreadSnapshotBuilder::new(
            "T-root",
            "T-root",
            "directive",
            "system/test",
            "directive-runtime",
        )
        .build();

        db.create_chain("T-root", snapshot, &signer).unwrap();

        let ref_path = db.refs_root().join("generic/chains/T-root/head");
        assert!(ref_path.exists());
    }

    #[test]
    fn add_thread_projects_snapshot() {
        let (_dir, db) = open_temp();
        let signer = TestSigner::default();

        let root_snapshot = ThreadSnapshotBuilder::new(
            "T-root",
            "T-root",
            "directive",
            "system/test",
            "directive-runtime",
        )
        .build();

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

        let root_snapshot = ThreadSnapshotBuilder::new(
            "T-root",
            "T-root",
            "directive",
            "system/test",
            "directive-runtime",
        )
        .build();

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

        let snapshot = ThreadSnapshotBuilder::new(
            "T-root",
            "T-root",
            "directive",
            "system/test",
            "directive-runtime",
        )
        .build();

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

        let snapshot = ThreadSnapshotBuilder::new(
            "T-root",
            "T-root",
            "directive",
            "system/test",
            "directive-runtime",
        )
        .build();

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
