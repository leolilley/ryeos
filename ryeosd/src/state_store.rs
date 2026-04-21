//! StateStore facade — bridges ryeosd Database API with rye-state CAS operations.
//! 
//! This module provides:
//! - StateStore: Unified interface delegating to rye-state CAS
//! - NodeIdentitySigner: Wrapper around NodeIdentity for rye-state Signer trait
//!
//! This module provides a unified interface that:
//! - Delegates durable state (threads, events, snapshots) to rye-state CAS
//! - Manages transient state (pid, pgid, commands) in runtime.sqlite3
//! - Maintains backward compatibility with existing Database API
//!
//! Architecture:
//! ```text
//! ryeosd::StateStore
//!   ├── rye_state::StateStore (CAS-backed durable)
//!   │   ├── .state/objects/ (CAS blobs)
//!   │   ├── .state/refs/ (signed refs)
//!   │   └── projection.sqlite3 (read-only view)
//!   ├── runtime.sqlite3 (transient state)
//!   │   ├── thread_runtime (pid, pgid)
//!   │   ├── thread_commands (lease/claim)
//!   │   └── Future transient tables
//!   └── Signer trait (from daemon identity)
//! ```

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use rusqlite::Connection;
use ed25519_dalek::Signer as Ed25519Signer;

use rye_state::{
    chain, objects,
    objects::{ThreadSnapshot, ThreadStatus, ThreadEvent, EventDurability},
    HeadCache,
};
use rye_state::signer::Signer;

use crate::db::{
    NewThreadRecord, ThreadBudgetRecord, PersistedEventRecord, NewEventRecord,
    ThreadDetail, ThreadListItem, RuntimeInfo, BudgetInfo,
};

/// Wrapper around NodeIdentity to implement rye-state Signer trait.
pub struct NodeIdentitySigner {
    fingerprint: String,
    signing_key: ed25519_dalek::SigningKey,
}

impl NodeIdentitySigner {
    /// Create a new signer from a NodeIdentity
    pub fn from_identity(identity: &crate::identity::NodeIdentity) -> Self {
        Self {
            fingerprint: identity.fingerprint().to_string(),
            signing_key: identity.signing_key().clone(),
        }
    }
}

impl Signer for NodeIdentitySigner {
    fn sign(&self, data: &[u8]) -> Vec<u8> {
        self.signing_key.sign(data).to_bytes().to_vec()
    }

    fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
}

/// Unified state store combining CAS-backed durable state and transient runtime state.
pub struct StateStore {
    /// Path to .state/ directory root
    state_root: PathBuf,

    /// Transient runtime state (pid, pgid, commands).
    runtime_db: Arc<Mutex<Connection>>,

    /// In-memory cache of verified chain heads (for fast reads).
    head_cache: Arc<Mutex<HeadCache>>,

    /// Signing key for signed refs (passed from daemon identity).
    signer: Arc<dyn Signer>,
}

impl std::fmt::Debug for StateStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StateStore")
            .field("state_root", &self.state_root)
            .field("runtime_db", &"<Connection>")
            .field("head_cache", &"<HeadCache>")
            .field("signer", &"<Signer>")
            .finish()
    }
}

impl StateStore {
    /// Create a new StateStore with CAS and runtime database.
    pub fn new(
        state_root: PathBuf,
        runtime_db_path: PathBuf,
        signer: Arc<dyn Signer>,
    ) -> Result<Self> {
        // Verify state_root exists
        std::fs::create_dir_all(&state_root)
            .context("failed to create state_root directory")?;

        // Initialize runtime database for transient state
        if let Some(parent) = runtime_db_path.parent() {
            std::fs::create_dir_all(parent)
                .context("failed to create runtime db directory")?;
        }
        let runtime_db = rusqlite::Connection::open(&runtime_db_path)
            .context("failed to open runtime.sqlite3")?;

        // Run migrations for runtime DB
        Self::init_runtime_schema(&runtime_db)?;

        let head_cache = Arc::new(Mutex::new(HeadCache::new()));

        Ok(Self {
            state_root,
            runtime_db: Arc::new(Mutex::new(runtime_db)),
            head_cache,
            signer,
        })
    }

    /// Initialize runtime database schema (transient state only).
    fn init_runtime_schema(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            r#"
            PRAGMA journal_mode=WAL;
            PRAGMA foreign_keys=ON;

            CREATE TABLE IF NOT EXISTS thread_runtime (
                thread_id TEXT PRIMARY KEY,
                pid INTEGER,
                pgid INTEGER,
                metadata BLOB
            );

            CREATE TABLE IF NOT EXISTS thread_commands (
                command_id INTEGER PRIMARY KEY AUTOINCREMENT,
                thread_id TEXT NOT NULL,
                command_type TEXT NOT NULL,
                status TEXT NOT NULL,
                requested_by TEXT,
                params BLOB,
                result BLOB,
                created_at TEXT NOT NULL,
                claimed_at TEXT,
                completed_at TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_thread_commands_status 
                ON thread_commands(thread_id, status);
            "#,
        )?;
        Ok(())
    }

    /// Create a new root thread in CAS and initialize runtime state.
    pub fn create_thread(
        &self,
        thread: &NewThreadRecord,
        _budget: Option<&ThreadBudgetRecord>,
    ) -> Result<Vec<PersistedEventRecord>> {
        let now = chrono::Utc::now().to_rfc3339();

        // Build ThreadSnapshot from NewThreadRecord
        let snapshot = ThreadSnapshot {
            schema: objects::SCHEMA_VERSION,
            kind: "thread_snapshot".to_string(),
            thread_id: thread.thread_id.clone(),
            chain_root_id: thread.chain_root_id.clone(),
            status: ThreadStatus::Created,
            kind_name: thread.kind.clone(),
            item_ref: thread.item_ref.clone(),
            executor_ref: thread.executor_ref.clone(),
            launch_mode: thread.launch_mode.clone(),
            current_site_id: thread.current_site_id.clone(),
            origin_site_id: thread.origin_site_id.clone(),
            upstream_thread_id: thread.upstream_thread_id.clone(),
            requested_by: thread.requested_by.clone(),
            created_at: now.clone(),
            updated_at: now.clone(),
            started_at: None,
            finished_at: None,
            result: None,
            error: None,
            budget: None,
            artifacts: vec![],
            facets: Default::default(),
            last_event_hash: None,
            last_chain_seq: 0,
            last_thread_seq: 0,
        };

        // Call rye-state to create the chain
        let cas_root = self.state_root.join("objects");
        let refs_root = self.state_root.join("refs");

        let mut head_cache = self.head_cache.lock()
            .map_err(|_| anyhow!("failed to lock head_cache"))?;

        let _create_result = chain::create_chain(
            &cas_root,
            &refs_root,
            &thread.thread_id,
            snapshot,
            self.signer.as_ref(),
            &mut *head_cache,
        )?;

        // Insert runtime record
        let runtime_db = self.runtime_db.lock()
            .map_err(|_| anyhow!("failed to lock runtime_db"))?;

        runtime_db.execute(
            "INSERT INTO thread_runtime (thread_id, pid, pgid, metadata) 
             VALUES (?1, NULL, NULL, NULL)",
            rusqlite::params![&thread.thread_id],
        )?;

        drop(runtime_db);

        // Return the persisted event (simplified for now)
        Ok(vec![
            PersistedEventRecord {
                event_id: 1,
                chain_root_id: thread.chain_root_id.clone(),
                chain_seq: 1,
                thread_id: thread.thread_id.clone(),
                thread_seq: 1,
                event_type: "thread_created".to_string(),
                storage_class: "indexed".to_string(),
                ts: now,
                payload: serde_json::json!({
                    "kind": &thread.kind,
                    "item_ref": &thread.item_ref,
                    "executor_ref": &thread.executor_ref,
                    "launch_mode": &thread.launch_mode,
                }),
            },
        ])
    }

    /// Append events to a thread's execution chain.
    pub fn append_events(
        &self,
        chain_root_id: &str,
        thread_id: &str,
        events: &[NewEventRecord],
    ) -> Result<Vec<PersistedEventRecord>> {
        let now = chrono::Utc::now().to_rfc3339();

        // Convert NewEventRecord → ThreadEvent
        let thread_events: Vec<ThreadEvent> = events
            .iter()
            .enumerate()
            .map(|(idx, event)| ThreadEvent {
                schema: objects::SCHEMA_VERSION,
                kind: "thread_event".to_string(),
                chain_root_id: chain_root_id.to_string(),
                chain_seq: 0, // Will be set by rye-state
                thread_id: thread_id.to_string(),
                thread_seq: (idx + 1) as u64, // Sequential in thread
                event_type: event.event_type.clone(),
                durability: match event.storage_class.as_str() {
                    "indexed" => EventDurability::Durable,
                    "journal" => EventDurability::Journal,
                    "ephemeral" => EventDurability::Ephemeral,
                    _ => EventDurability::Durable,
                },
                ts: now.clone(),
                prev_chain_event_hash: None,
                prev_thread_event_hash: None,
                payload: event.payload.clone(),
            })
            .collect();

        // Delegate to rye-state
        let cas_root = self.state_root.join("objects");
        let refs_root = self.state_root.join("refs");
        let mut head_cache = self.head_cache.lock()
            .map_err(|_| anyhow!("failed to lock head_cache"))?;

        let result = chain::append_events(
            &cas_root,
            &refs_root,
            chain_root_id,
            thread_id,
            thread_events,
            vec![], // snapshot_updates (empty for now)
            self.signer.as_ref(),
            &mut *head_cache,
        )?;

        // Convert AppendResult to PersistedEventRecord
        let persisted = result.events
            .iter()
            .enumerate()
            .map(|(idx, event)| PersistedEventRecord {
                event_id: (result.first_chain_seq as i64) + (idx as i64),
                chain_root_id: event.chain_root_id.clone(),
                chain_seq: (result.first_chain_seq as i64) + (idx as i64),
                thread_id: event.thread_id.clone(),
                thread_seq: event.thread_seq as i64,
                event_type: event.event_type.clone(),
                storage_class: match event.durability {
                    objects::EventDurability::Durable => "indexed".to_string(),
                    objects::EventDurability::Journal => "journal".to_string(),
                    objects::EventDurability::Ephemeral => "ephemeral".to_string(),
                },
                ts: event.ts.clone(),
                payload: event.payload.clone(),
            })
            .collect();

        Ok(persisted)
    }

    /// Attach process information (pid, pgid) to a running thread.
    pub fn attach_thread_process(
        &self,
        thread_id: &str,
        pid: i64,
        pgid: i64,
    ) -> Result<()> {
        let runtime_db = self.runtime_db.lock()
            .map_err(|_| anyhow!("failed to lock runtime_db"))?;

        runtime_db.execute(
            "UPDATE thread_runtime SET pid = ?1, pgid = ?2 WHERE thread_id = ?3",
            rusqlite::params![pid, pgid, thread_id],
        )?;

        Ok(())
    }

    /// Read thread details from projection (durable state).
    pub fn read_thread(&self, thread_id: &str) -> Result<Option<ThreadDetail>> {
        // Read from rye-state projection
        let cas_root = self.state_root.join("objects");
        let refs_root = self.state_root.join("refs");
        
        let head_cache = self.head_cache.lock()
            .map_err(|_| anyhow!("failed to lock head_cache"))?;

        // First, we need to find the chain_root_id. For now, assume it's the thread_id (root thread case)
        // In full implementation, would query runtime.sqlite3 or cache to find chain membership
        let chain_root_id = thread_id;

        let snapshot = match chain::read_thread_snapshot(
            &cas_root,
            &refs_root,
            chain_root_id,
            thread_id,
            &*head_cache,
        ) {
            Ok(result) => result.snapshot,
            Err(_) => None,
        };

        let snapshot = match snapshot {
            Some(s) => s,
            None => return Ok(None),
        };

        drop(head_cache);

        // Read runtime info from runtime.sqlite3
        let runtime_db = self.runtime_db.lock()
            .map_err(|_| anyhow!("failed to lock runtime_db"))?;

        let (pid, pgid) = runtime_db
            .query_row(
                "SELECT pid, pgid FROM thread_runtime WHERE thread_id = ?1",
                rusqlite::params![thread_id],
                |row| Ok((row.get::<_, Option<i64>>(0)?, row.get::<_, Option<i64>>(1)?)),
            )
            .ok()
            .unwrap_or((None, None));

        drop(runtime_db);

        // Assemble ThreadDetail
        Ok(Some(ThreadDetail {
            thread_id: snapshot.thread_id,
            chain_root_id: snapshot.chain_root_id,
            kind: snapshot.kind_name,
            status: snapshot.status.to_string(),
            item_ref: snapshot.item_ref,
            executor_ref: snapshot.executor_ref,
            launch_mode: snapshot.launch_mode,
            current_site_id: snapshot.current_site_id,
            origin_site_id: snapshot.origin_site_id,
            upstream_thread_id: snapshot.upstream_thread_id,
            requested_by: snapshot.requested_by,
            created_at: snapshot.created_at,
            updated_at: snapshot.updated_at,
            started_at: snapshot.started_at,
            finished_at: snapshot.finished_at,
            runtime: RuntimeInfo { pid, pgid },
            budget: snapshot.budget.map(|b| BudgetInfo {
                budget_parent_id: None,
                reserved_spend: 0.0,
                actual_spend: 0.0,
                status: "unknown".to_string(),
                metadata: Some(b),
            }),
            allowed_actions: vec![], // Computed based on status
        }))
    }

    /// List all threads in a chain.
    pub fn list_threads(&self, chain_root_id: &str) -> Result<Vec<ThreadListItem>> {
        // Query rye-state projection
        let cas_root = self.state_root.join("objects");
        let refs_root = self.state_root.join("refs");
        let head_cache = self.head_cache.lock()
            .map_err(|_| anyhow!("failed to lock head_cache"))?;

        let chain_state = match chain::read_chain_head(
            &cas_root,
            &refs_root,
            chain_root_id,
            &*head_cache,
        ) {
            Ok(cs) => cs,
            Err(_) => return Ok(vec![]),
        };

        // Convert ChainState threads to ThreadListItem
        let mut items = Vec::new();
        for (thread_id, entry) in chain_state.threads {
            items.push(ThreadListItem {
                thread_id,
                chain_root_id: chain_state.chain_root_id.clone(),
                kind: "unknown".to_string(), // Would need to read from snapshot
                status: entry.status.to_string(),
                item_ref: "unknown".to_string(),
                launch_mode: "inline".to_string(),
                current_site_id: "unknown".to_string(),
                origin_site_id: "unknown".to_string(),
                created_at: chain_state.updated_at.clone(),
                updated_at: chain_state.updated_at.clone(),
            });
        }

        Ok(items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Simple test signer
    struct TestSignerImpl;
    
    impl Signer for TestSignerImpl {
        fn sign(&self, _data: &[u8]) -> Vec<u8> {
            vec![0u8; 64]
        }
        fn fingerprint(&self) -> &str {
            "test-fingerprint"
        }
    }

    #[test]
    fn state_store_init_requires_valid_paths() {
        let tmpdir = tempfile::tempdir().unwrap();
        let state_root = tmpdir.path().join("state");
        let runtime_db_path = tmpdir.path().join("runtime.sqlite3");
        let signer = std::sync::Arc::new(TestSignerImpl);

        let result = StateStore::new(state_root, runtime_db_path, signer);
        assert!(result.is_ok());
    }

    #[test]
    fn create_thread_stores_in_cas() {
        let tmpdir = tempfile::tempdir().unwrap();
        let state_root = tmpdir.path().join("state");
        let runtime_db_path = tmpdir.path().join("runtime.sqlite3");
        let signer = std::sync::Arc::new(TestSignerImpl);

        let store = StateStore::new(state_root.clone(), runtime_db_path, signer).unwrap();

        let thread = NewThreadRecord {
            thread_id: "T-test-1".to_string(),
            chain_root_id: "T-test-1".to_string(),
            kind: "directive".to_string(),
            status: "created".to_string(),
            item_ref: "test/item".to_string(),
            executor_ref: "test/executor".to_string(),
            launch_mode: "inline".to_string(),
            current_site_id: "local".to_string(),
            origin_site_id: "local".to_string(),
            upstream_thread_id: None,
            requested_by: None,
            summary_json: None,
        };

        let result = store.create_thread(&thread, None);
        assert!(result.is_ok());

        let events = result.unwrap();
        assert!(!events.is_empty());
        assert_eq!(events[0].event_type, "thread_created");

        // Verify CAS objects exist
        let cas_root = state_root.join("objects");
        let objects = fs::read_dir(cas_root).unwrap();
        let count = objects.count();
        assert!(count > 0, "CAS should contain objects after thread creation");
    }

    #[test]
    fn append_events_converts_properly() {
        let tmpdir = tempfile::tempdir().unwrap();
        let state_root = tmpdir.path().join("state");
        let runtime_db_path = tmpdir.path().join("runtime.sqlite3");
        let signer = std::sync::Arc::new(TestSignerImpl);

        let store = StateStore::new(state_root, runtime_db_path, signer).unwrap();

        // Create thread first
        let thread = NewThreadRecord {
            thread_id: "T-test-1".to_string(),
            chain_root_id: "T-test-1".to_string(),
            kind: "directive".to_string(),
            status: "created".to_string(),
            item_ref: "test/item".to_string(),
            executor_ref: "test/executor".to_string(),
            launch_mode: "inline".to_string(),
            current_site_id: "local".to_string(),
            origin_site_id: "local".to_string(),
            upstream_thread_id: None,
            requested_by: None,
            summary_json: None,
        };

        store.create_thread(&thread, None).unwrap();

        // Append event
        let event = NewEventRecord {
            event_type: "test_event".to_string(),
            storage_class: "indexed".to_string(),
            payload: serde_json::json!({"test": "data"}),
        };

        let result = store.append_events("T-test-1", "T-test-1", &[event]);
        assert!(result.is_ok());

        let persisted = result.unwrap();
        assert!(!persisted.is_empty());
        assert_eq!(persisted[0].event_type, "test_event");
        assert_eq!(persisted[0].chain_seq, 1); // First actual event after snapshot
    }

    #[test]
    fn read_thread_returns_thread_detail() {
        let tmpdir = tempfile::tempdir().unwrap();
        let state_root = tmpdir.path().join("state");
        let runtime_db_path = tmpdir.path().join("runtime.sqlite3");
        let signer = std::sync::Arc::new(TestSignerImpl);

        let store = StateStore::new(state_root, runtime_db_path, signer).unwrap();

        let thread = NewThreadRecord {
            thread_id: "T-read-1".to_string(),
            chain_root_id: "T-read-1".to_string(),
            kind: "directive".to_string(),
            status: "created".to_string(),
            item_ref: "test/item".to_string(),
            executor_ref: "test/executor".to_string(),
            launch_mode: "inline".to_string(),
            current_site_id: "local".to_string(),
            origin_site_id: "local".to_string(),
            upstream_thread_id: None,
            requested_by: Some("user-1".to_string()),
            summary_json: None,
        };

        store.create_thread(&thread, None).unwrap();

        let result = store.read_thread("T-read-1");
        assert!(result.is_ok());

        let detail = result.unwrap();
        assert!(detail.is_some());
        let detail = detail.unwrap();
        assert_eq!(detail.thread_id, "T-read-1");
        assert_eq!(detail.kind, "directive");
        assert_eq!(detail.requested_by, Some("user-1".to_string()));
    }

    #[test]
    fn attach_thread_process_updates_runtime() {
        let tmpdir = tempfile::tempdir().unwrap();
        let state_root = tmpdir.path().join("state");
        let runtime_db_path = tmpdir.path().join("runtime.sqlite3");
        let signer = std::sync::Arc::new(TestSignerImpl);

        let store = StateStore::new(state_root, runtime_db_path, signer).unwrap();

        let thread = NewThreadRecord {
            thread_id: "T-proc-1".to_string(),
            chain_root_id: "T-proc-1".to_string(),
            kind: "directive".to_string(),
            status: "created".to_string(),
            item_ref: "test/item".to_string(),
            executor_ref: "test/executor".to_string(),
            launch_mode: "inline".to_string(),
            current_site_id: "local".to_string(),
            origin_site_id: "local".to_string(),
            upstream_thread_id: None,
            requested_by: None,
            summary_json: None,
        };

        store.create_thread(&thread, None).unwrap();

        let result = store.attach_thread_process("T-proc-1", 1234, 5678);
        assert!(result.is_ok());

        let detail = store.read_thread("T-proc-1").unwrap().unwrap();
        assert_eq!(detail.runtime.pid, Some(1234));
        assert_eq!(detail.runtime.pgid, Some(5678));
    }
}
