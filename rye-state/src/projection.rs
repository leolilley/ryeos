//! SQLite projection of CAS state.
//!
//! The projection is a rebuildable view of durable CAS objects stored in SQLite.
//! It provides fast read access and is the authoritative source for thread
//! queries during normal operation.

use anyhow::Context;
use rusqlite::{Connection, OptionalExtension};

// ============= Schema =============

const SCHEMA_SQL: &str = r#"
PRAGMA journal_mode=WAL;
PRAGMA foreign_keys=ON;

-- Projection metadata: tracks indexed chain state hashes
CREATE TABLE IF NOT EXISTS projection_meta (
    chain_root_id TEXT PRIMARY KEY,
    indexed_chain_state_hash TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

-- Threads: the primary durable table
CREATE TABLE IF NOT EXISTS threads (
    thread_id TEXT PRIMARY KEY,
    chain_root_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN (
        'created',
        'running',
        'completed',
        'failed',
        'cancelled',
        'killed',
        'timed_out',
        'continued'
    )),
    item_ref TEXT NOT NULL,
    executor_ref TEXT NOT NULL,
    launch_mode TEXT NOT NULL CHECK (launch_mode IN ('inline', 'detached')),
    current_site_id TEXT NOT NULL,
    origin_site_id TEXT NOT NULL,
    upstream_thread_id TEXT,
    requested_by TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    started_at TEXT,
    finished_at TEXT
);

CREATE INDEX IF NOT EXISTS idx_threads_chain_root ON threads(chain_root_id);
CREATE INDEX IF NOT EXISTS idx_threads_status ON threads(status);
CREATE INDEX IF NOT EXISTS idx_threads_created_at ON threads(created_at);
CREATE INDEX IF NOT EXISTS idx_threads_updated_at ON threads(updated_at);
"#;

/// Metadata for a projection entry.
#[derive(Debug, Clone)]
pub struct ProjectionMeta {
    pub chain_root_id: String,
    pub indexed_chain_state_hash: String,
    pub updated_at: String,
}

/// Projection database connection wrapper.
pub struct ProjectionDb {
    conn: Connection,
}

impl ProjectionDb {
    /// Open or create a projection database.
    pub fn open(path: &std::path::Path) -> anyhow::Result<Self> {
        let conn = rusqlite::Connection::open(path)
            .context("failed to open projection database")?;

        // Apply schema
        conn.execute_batch(SCHEMA_SQL)
            .context("failed to initialize projection schema")?;

        Ok(Self { conn })
    }

    /// Get the underlying connection for queries.
    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// Get a mutable connection for transactions.
    pub fn connection_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    /// Get projection metadata for a chain.
    pub fn get_projection_meta(&self, chain_root_id: &str) -> anyhow::Result<Option<ProjectionMeta>> {
        let mut stmt = self
            .conn
            .prepare("SELECT chain_root_id, indexed_chain_state_hash, updated_at FROM projection_meta WHERE chain_root_id = ?")
            .context("failed to prepare query")?;

        let meta = stmt
            .query_row([chain_root_id], |row| {
                Ok(ProjectionMeta {
                    chain_root_id: row.get(0)?,
                    indexed_chain_state_hash: row.get(1)?,
                    updated_at: row.get(2)?,
                })
            })
            .optional()
            .context("failed to query projection_meta")?;

        Ok(meta)
    }

    /// Update projection metadata for a chain.
    pub fn update_projection_meta(
        &self,
        meta: &ProjectionMeta,
    ) -> anyhow::Result<()> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO projection_meta (chain_root_id, indexed_chain_state_hash, updated_at) VALUES (?, ?, ?)",
                rusqlite::params![&meta.chain_root_id, &meta.indexed_chain_state_hash, &meta.updated_at],
            )
            .context("failed to update projection_meta")?;

        Ok(())
    }
}

// ============= Write operations =============

/// Project a thread snapshot into the projection database.
///
/// Upserts a thread record based on the snapshot.
pub fn project_thread_snapshot(
    db: &ProjectionDb,
    snapshot: &crate::ThreadSnapshot,
    chain_root_id: &str,
) -> anyhow::Result<()> {
    snapshot.validate()?;

    db.connection().execute(
        "INSERT OR REPLACE INTO threads (
            thread_id, chain_root_id, kind, status,
            item_ref, executor_ref, launch_mode,
            current_site_id, origin_site_id, upstream_thread_id, requested_by,
            created_at, updated_at, started_at, finished_at
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        rusqlite::params![
            &snapshot.thread_id,
            chain_root_id,
            &snapshot.kind_name,
            snapshot.status.to_string(),
            &snapshot.item_ref,
            &snapshot.executor_ref,
            &snapshot.launch_mode,
            &snapshot.current_site_id,
            &snapshot.origin_site_id,
            &snapshot.upstream_thread_id,
            &snapshot.requested_by,
            &snapshot.created_at,
            &snapshot.updated_at,
            &snapshot.started_at,
            &snapshot.finished_at,
        ],
    )
    .context("failed to project thread snapshot")?;

    Ok(())
}

/// Project all thread snapshots from a chain state into the projection database.
///
/// Also updates the projection metadata to track the indexed chain state hash.
pub fn project_chain_state(
    db: &ProjectionDb,
    chain_state: &crate::ChainState,
    chain_state_hash: &str,
) -> anyhow::Result<()> {
    chain_state.validate()?;

    // Update projection metadata
    let meta = ProjectionMeta {
        chain_root_id: chain_state.chain_root_id.clone(),
        indexed_chain_state_hash: chain_state_hash.to_string(),
        updated_at: chain_state.updated_at.clone(),
    };

    db.update_projection_meta(&meta)
        .context("failed to update projection metadata")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::thread_snapshot::ThreadSnapshotBuilder;
    use crate::objects::{ChainState, ChainThreadEntry, ThreadStatus};
    use std::collections::BTreeMap;

    #[test]
    fn open_creates_projection_db() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("projection.db");
        let db = ProjectionDb::open(&path).unwrap();
        
        // Verify tables were created
        let mut stmt = db.conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table'")
            .unwrap();
        
        let tables: Vec<String> = stmt
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        
        assert!(tables.contains(&"projection_meta".to_string()));
        assert!(tables.contains(&"threads".to_string()));
    }

    #[test]
    fn update_and_get_projection_meta() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("projection.db");
        let db = ProjectionDb::open(&path).unwrap();

        let meta = ProjectionMeta {
            chain_root_id: "T-root".to_string(),
            indexed_chain_state_hash: "01".repeat(32),
            updated_at: "2026-04-21T12:00:00Z".to_string(),
        };

        db.update_projection_meta(&meta).unwrap();

        let retrieved = db.get_projection_meta("T-root").unwrap();
        assert!(retrieved.is_some());
        let retrieved = retrieved.unwrap();
        assert_eq!(retrieved.chain_root_id, "T-root");
        assert_eq!(retrieved.indexed_chain_state_hash, meta.indexed_chain_state_hash);
    }

    #[test]
    fn get_missing_projection_meta_returns_none() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("projection.db");
        let db = ProjectionDb::open(&path).unwrap();

        let result = db.get_projection_meta("T-missing").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn project_thread_snapshot_succeeds() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("projection.db");
        let db = ProjectionDb::open(&path).unwrap();

        let snapshot = ThreadSnapshotBuilder::new(
            "T-test",
            "T-root",
            "directive",
            "system/test",
            "directive-runtime",
        )
        .build();

        let result = project_thread_snapshot(&db, &snapshot, "T-root");
        assert!(result.is_ok());
    }

    #[test]
    fn project_chain_state_updates_metadata() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("projection.db");
        let db = ProjectionDb::open(&path).unwrap();

        let mut threads = BTreeMap::new();
        threads.insert(
            "T-root".to_string(),
            ChainThreadEntry {
                snapshot_hash: "01".repeat(32),
                last_event_hash: None,
                last_thread_seq: 0,
                status: ThreadStatus::Created,
            },
        );

        let chain_state = ChainState {
            schema: 1,
            kind: "chain_state".to_string(),
            chain_root_id: "T-root".to_string(),
            prev_chain_state_hash: None,
            last_event_hash: None,
            last_chain_seq: 0,
            updated_at: "2026-04-21T12:00:00Z".to_string(),
            threads,
        };

        let hash = "02".repeat(32);
        project_chain_state(&db, &chain_state, &hash).unwrap();

        let meta = db.get_projection_meta("T-root").unwrap();
        assert!(meta.is_some());
        assert_eq!(meta.unwrap().indexed_chain_state_hash, hash);
    }
}
