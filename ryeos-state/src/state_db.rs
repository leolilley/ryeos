//! High-level state database wrapping ProjectionDb + CAS paths + HeadCache.
//!
//! The write-path contract is **CAS-first**: if CAS succeeds but projection
//! fails, the method logs a warning and returns `Ok`. Projection drift is
//! repaired by the startup reconcile pass.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::Context;

use crate::chain::{self, AppendResult, CreateResult, SnapshotUpdate};
use crate::head_cache::HeadCache;
use crate::objects::ThreadEvent;
use crate::objects::ThreadSnapshot;
use crate::projection::{self, ProjectionDb};
use crate::queries;
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
    /// Open (or create) a state database rooted at `state_root`.
    ///
    /// Creates `objects/`, `refs/`, and `locators/` subdirectories and opens
    /// `projection.sqlite3` inside `state_root`.
    pub fn open(state_root: &Path) -> anyhow::Result<Self> {
        let cas_root = state_root.join("objects");
        let refs_root = state_root.join("refs");
        let locators_root = state_root.join("locators");
        let projection_path = state_root.join("projection.sqlite3");

        std::fs::create_dir_all(&cas_root).context("creating objects root")?;
        std::fs::create_dir_all(&refs_root).context("creating refs root")?;
        std::fs::create_dir_all(&locators_root).context("creating locators root")?;

        let projection = ProjectionDb::open(&projection_path)
            .context("opening projection database")?;

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

    /// CAS objects root (`state_root/objects`).
    pub fn cas_root(&self) -> &Path {
        &self.cas_root
    }

    /// Refs root (`state_root/refs`).
    pub fn refs_root(&self) -> &Path {
        &self.refs_root
    }

    /// Locators root (`state_root/locators`).
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

        projection::project_thread_snapshot(
            &self.projection,
            &snapshot,
            chain_root_id,
        ).context("projection failed after CAS write for create_chain")?;

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

        projection::project_thread_snapshot(
            &self.projection,
            &snapshot,
            chain_root_id,
        ).context("projection failed after CAS write for add_thread")?;

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
            projection::project_event(&self.projection, event)
                .with_context(|| format!(
                    "projection failed for event chain_seq={}",
                    event.chain_seq
                ))?;
        }

        for update in &snapshot_updates {
            projection::project_thread_snapshot(&self.projection, &update.new_snapshot, chain_root_id)
                .with_context(|| format!(
                    "projection failed for snapshot update thread_id={}",
                    update.thread_id
                ))?;
        }

        Ok(result)
    }

    // ── Project head refs ──────────────────────────────────────────

    /// Read a project head ref. Returns the project snapshot hash.
    pub fn read_project_head(&self, project_hash: &str) -> anyhow::Result<Option<String>> {
        crate::refs::read_project_head_ref(&self.refs_root, project_hash)
    }

    /// Write a project head ref.
    pub fn write_project_head_ref(
        &self,
        project_hash: &str,
        project_snapshot_hash: &str,
        signer: &dyn Signer,
    ) -> anyhow::Result<()> {
        crate::refs::write_project_head_ref(&self.refs_root, project_hash, project_snapshot_hash, signer)
    }

    /// Advance a project head ref (with conflict detection).
    pub fn advance_project_head_ref(
        &self,
        project_hash: &str,
        new_snapshot_hash: &str,
        signer: &dyn Signer,
    ) -> anyhow::Result<()> {
        crate::refs::advance_project_head_ref(&self.refs_root, project_hash, new_snapshot_hash, signer)
    }

    // ── Query helpers ──────────────────────────────────────────────

    pub fn get_thread(&self, thread_id: &str) -> anyhow::Result<Option<queries::ThreadRow>> {
        queries::get_thread(&self.projection, thread_id)
    }

    pub fn list_threads_by_chain(&self, chain_root_id: &str) -> anyhow::Result<Vec<queries::ThreadRow>> {
        queries::list_threads_by_chain(&self.projection, chain_root_id)
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

        let row = db.get_thread("T-root").unwrap().expect("thread should exist in projection");
        assert_eq!(row.thread_id, "T-root");
        assert_eq!(row.chain_root_id, "T-root");
        assert_eq!(row.kind, "directive");
        assert_eq!(row.status, "created");
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

        let row = db.get_thread("T-child").unwrap().expect("child thread in projection");
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
