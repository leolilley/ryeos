//! High-level state database wrapping ProjectionDb + CAS paths + HeadCache.
//!
//! The write-path contract is **CAS-first**: if CAS succeeds but projection
//! fails, the method logs a warning and returns `Ok`. Projection drift is
//! repaired by the startup reconcile pass.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

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
use crate::projection::{
    self, AdmissionAttestationRecord, CasEntriesByStateSummary, CasEntryAttribution, CasEntryState,
    NewAdmissionAttestationRecord, NewCasEntryAttribution, NewSyncJob, NewSyncJobAttempt,
    ProjectionDb, ProjectionOpenResult, SyncJobAttemptRecord, SyncJobRecord, SyncJobState,
    SyncJobUpdate,
};
use crate::queries;
use crate::refs::{GenericHeadRef, SignedRef};
use crate::signer::Signer;

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
}

impl StateDb {
    /// Open (or create) a state database rooted at `runtime_state_dir`.
    ///
    /// Creates `objects/`, `refs/`, and `locators/` subdirectories and opens
    /// `projection.sqlite3` inside `runtime_state_dir`.
    pub fn open(runtime_state_dir: &Path) -> anyhow::Result<Self> {
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
        }

        Ok(Self {
            cas_root,
            refs_root,
            locators_root,
            projection,
            head_cache: Mutex::new(HeadCache::new()),
        })
    }

    /// Access the underlying projection database for queries.
    pub fn projection(&self) -> &ProjectionDb {
        &self.projection
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
    ) -> anyhow::Result<CreateResult> {
        let result = {
            let mut cache = self.head_cache.lock().expect("head_cache lock");
            chain::create_chain(
                &self.cas_root,
                &self.refs_root,
                chain_root_id,
                snapshot.clone(),
                signer,
                &mut cache,
            )?
        };

        projection::project_thread_snapshot(&self.projection, &snapshot, chain_root_id)
            .context("projection failed after CAS write for create_chain")?;

        Ok(result)
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
    ) -> anyhow::Result<CreateResult> {
        let result = {
            let mut cache = self.head_cache.lock().expect("head_cache lock");
            chain::add_thread_to_chain(
                &self.cas_root,
                &self.refs_root,
                chain_root_id,
                snapshot.clone(),
                signer,
                &mut cache,
            )?
        };

        projection::project_thread_snapshot(&self.projection, &snapshot, chain_root_id)
            .context("projection failed after CAS write for add_thread")?;

        Ok(result)
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
    ) -> anyhow::Result<AppendResult> {
        if events.iter().any(|event| !event.durability.is_cas_stored()) {
            anyhow::bail!("StateDb::append_events cannot persist ephemeral events");
        }

        let result = {
            let mut cache = self.head_cache.lock().expect("head_cache lock");
            chain::append_events(
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
            )?
        };

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

        Ok(result)
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
    ) -> anyhow::Result<AddThreadWithEventsResult> {
        if events.iter().any(|event| !event.durability.is_cas_stored()) {
            anyhow::bail!("StateDb::add_thread_with_events cannot persist ephemeral events");
        }

        let result = {
            let mut cache = self.head_cache.lock().expect("head_cache lock");
            chain::add_thread_to_chain_with_events(
                &self.cas_root,
                &self.refs_root,
                chain_root_id,
                snapshot.clone(),
                events,
                signer,
                &mut cache,
            )?
        };

        projection::project_thread_snapshot_with_events(
            &self.projection,
            &snapshot,
            chain_root_id,
            &result.events,
        )
        .context("projection failed after CAS write for add_thread_with_events")?;

        Ok(result)
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
        assert!(!result.chain_state_hash.is_empty());
        assert!(!result.snapshot_hash.is_empty());

        let row = db
            .get_thread("T-root")
            .unwrap()
            .expect("thread should exist in projection");
        assert_eq!(row.thread_id, "T-root");
        assert_eq!(row.chain_root_id, "T-root");
        assert_eq!(row.kind, "directive");
        assert_eq!(row.status, "created");
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

        let snapshot_path =
            lillux::shard_path(db.cas_root(), "objects", &result.snapshot_hash, ".json");
        assert!(snapshot_path.exists());

        let chain_state_path =
            lillux::shard_path(db.cas_root(), "objects", &result.chain_state_hash, ".json");
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

        assert_eq!(result.event_count, 1);

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
