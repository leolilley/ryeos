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

-- Events: durable thread events
CREATE TABLE IF NOT EXISTS events (
    event_id INTEGER PRIMARY KEY AUTOINCREMENT,
    chain_root_id TEXT NOT NULL,
    chain_seq INTEGER NOT NULL,
    thread_id TEXT NOT NULL,
    thread_seq INTEGER NOT NULL,
    event_type TEXT NOT NULL,
    durability TEXT NOT NULL CHECK (durability IN ('durable')),
    ts TEXT NOT NULL,
    prev_chain_event_hash TEXT,
    prev_thread_event_hash TEXT,
    payload BLOB NOT NULL,
    UNIQUE(chain_root_id, chain_seq)
);

CREATE INDEX IF NOT EXISTS idx_events_chain_root ON events(chain_root_id);
CREATE INDEX IF NOT EXISTS idx_events_thread_id ON events(thread_id);
CREATE INDEX IF NOT EXISTS idx_events_ts ON events(ts);

-- Event replay index: track indexed position per thread
CREATE TABLE IF NOT EXISTS event_replay_index (
    thread_id TEXT PRIMARY KEY,
    last_indexed_chain_seq INTEGER NOT NULL,
    updated_at TEXT NOT NULL
);

-- Thread edges: parent -> child relationships
CREATE TABLE IF NOT EXISTS thread_edges (
    edge_id INTEGER PRIMARY KEY AUTOINCREMENT,
    chain_root_id TEXT NOT NULL,
    parent_thread_id TEXT NOT NULL,
    child_thread_id TEXT NOT NULL,
    spawn_seq INTEGER,
    spawn_reason TEXT,
    created_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_edges_parent ON thread_edges(parent_thread_id);
CREATE INDEX IF NOT EXISTS idx_edges_child ON thread_edges(child_thread_id);

-- Thread results: final output and status
CREATE TABLE IF NOT EXISTS thread_results (
    thread_id TEXT PRIMARY KEY,
    chain_root_id TEXT NOT NULL,
    status TEXT NOT NULL,
    result BLOB,
    error TEXT,
    updated_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_results_chain_root ON thread_results(chain_root_id);

-- Thread artifacts: published outputs
CREATE TABLE IF NOT EXISTS thread_artifacts (
    artifact_id INTEGER PRIMARY KEY AUTOINCREMENT,
    chain_root_id TEXT NOT NULL,
    thread_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    metadata BLOB,
    created_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_artifacts_thread ON thread_artifacts(thread_id);
CREATE INDEX IF NOT EXISTS idx_artifacts_chain_root ON thread_artifacts(chain_root_id);

-- Thread facets: extensible attributes
CREATE TABLE IF NOT EXISTS thread_facets (
    facet_id INTEGER PRIMARY KEY AUTOINCREMENT,
    thread_id TEXT NOT NULL,
    key TEXT NOT NULL,
    value BLOB NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE(thread_id, key)
);

CREATE INDEX IF NOT EXISTS idx_facets_thread ON thread_facets(thread_id);
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
/// Upserts a thread record based on the snapshot. If the snapshot has
/// an `upstream_thread_id`, derives and inserts a thread edge from the
/// upstream to this thread.
pub fn project_thread_snapshot(
    db: &ProjectionDb,
    snapshot: &crate::ThreadSnapshot,
    chain_root_id: &str,
) -> anyhow::Result<()> {
    snapshot.validate()?;
    tracing::trace!(
        thread_id = %snapshot.thread_id,
        chain_root_id = %chain_root_id,
        status = %snapshot.status,
        upstream = ?snapshot.upstream_thread_id,
        "project thread snapshot"
    );

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

    // Derive edge from upstream_thread_id (CAS-truth derived projection)
    if let Some(ref upstream_id) = snapshot.upstream_thread_id {
        // Avoid duplicate edges — only insert if not already present
        let exists: bool = db.connection().query_row(
            "SELECT COUNT(*) > 0 FROM thread_edges WHERE parent_thread_id = ? AND child_thread_id = ? AND chain_root_id = ?",
            rusqlite::params![upstream_id, &snapshot.thread_id, chain_root_id],
            |row| row.get(0),
        ).unwrap_or(false);

        if !exists {
            db.connection().execute(
                "INSERT INTO thread_edges (
                    chain_root_id, parent_thread_id, child_thread_id, spawn_seq, spawn_reason, created_at
                ) VALUES (?, ?, ?, NULL, 'spawned', ?)",
                rusqlite::params![
                    chain_root_id,
                    upstream_id,
                    &snapshot.thread_id,
                    &snapshot.created_at,
                ],
            )
            .context("failed to project derived thread edge")?;
        }
    }

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
    tracing::trace!(
        chain_root_id = %chain_state.chain_root_id,
        chain_state_hash = %chain_state_hash,
        thread_count = chain_state.threads.len(),
        "project chain state"
    );

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

/// Project a thread event into the events table.
///
/// Called when durable events are appended to the chain. For
/// `artifact_published` events, also derives an artifact row from
/// the event payload.
#[tracing::instrument(
    level = "debug",
    name = "state:project_event",
    skip(db, event),
    fields(
        thread_id = %event.thread_id,
        event_type = %event.event_type,
    )
)]
pub fn project_event(
    db: &ProjectionDb,
    event: &crate::ThreadEvent,
) -> anyhow::Result<()> {
    event.validate()?;

    let payload = serde_json::to_vec(&event.payload)
        .context("failed to serialize event payload")?;

    db.connection().execute(
        "INSERT OR IGNORE INTO events (
            chain_root_id, chain_seq, thread_id, thread_seq,
            event_type, durability, ts, prev_chain_event_hash,
            prev_thread_event_hash, payload
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        rusqlite::params![
            &event.chain_root_id,
            event.chain_seq,
            &event.thread_id,
            event.thread_seq,
            &event.event_type,
            event.durability.to_string(),
            &event.ts,
            &event.prev_chain_event_hash,
            &event.prev_thread_event_hash,
            &payload,
        ],
    )
    .context("failed to project event")?;

    // Derive artifact row from artifact_published events (CAS-truth derived)
    if event.event_type == "artifact_published" {
        if let Some(artifact_type) = event.payload.get("artifact_type").and_then(|v| v.as_str()) {
            let metadata = event.payload.get("metadata").cloned();
            let metadata_blob = metadata.map(|m| {
                serde_json::to_vec(&m).context("failed to serialize metadata")
            }).transpose()?;

            db.connection().execute(
                "INSERT OR IGNORE INTO thread_artifacts (
                    chain_root_id, thread_id, kind, metadata, created_at
                ) VALUES (?, ?, ?, ?, ?)",
                rusqlite::params![
                    &event.chain_root_id,
                    &event.thread_id,
                    artifact_type,
                    metadata_blob,
                    &event.ts,
                ],
            )
            .context("failed to project derived artifact")?;
        }
    }

    Ok(())
}

/// Project a thread edge (parent-child relationship).
///
/// Called when a child thread is spawned.
pub fn project_thread_edge(
    db: &ProjectionDb,
    chain_root_id: &str,
    parent_thread_id: &str,
    child_thread_id: &str,
    spawn_seq: Option<i64>,
    spawn_reason: Option<&str>,
) -> anyhow::Result<()> {
    tracing::trace!(
        chain_root_id = %chain_root_id,
        parent_thread_id = %parent_thread_id,
        child_thread_id = %child_thread_id,
        spawn_reason = spawn_reason.unwrap_or(""),
        "project thread edge"
    );
    db.connection().execute(
        "INSERT INTO thread_edges (
            chain_root_id, parent_thread_id, child_thread_id, spawn_seq, spawn_reason, created_at
        ) VALUES (?, ?, ?, ?, ?, ?)",
        rusqlite::params![
            chain_root_id,
            parent_thread_id,
            child_thread_id,
            spawn_seq,
            spawn_reason,
            lillux::time::iso8601_now(),
        ],
    )
    .context("failed to project edge")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::thread_snapshot::ThreadSnapshotBuilder;
    use crate::objects::{ChainState, ChainThreadEntry, ThreadStatus};
    use std::collections::BTreeMap;
    use ryeos_tracing::test as trace_test;

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

    // ── Trace-capture tests ──────────────────────────────────────

    #[test]
    fn project_event_emits_span() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("projection.db");
        let db = ProjectionDb::open(&path).unwrap();

        use crate::objects::thread_event::NewEvent;
        let event = NewEvent::new("T-trace", "T-trace", "test_event")
            .payload(serde_json::json!({"key": "value"}))
            .build();

        let (_, spans) = trace_test::capture_traces(|| {
            let _ = project_event(&db, &event);
        });

        let span = trace_test::find_span(&spans, "state:project_event");
        assert!(span.is_some(), "expected state:project_event span, got: {:?}", spans.iter().map(|s| &s.name).collect::<Vec<_>>());

        let span = span.unwrap();
        let field_val = |name: &str| -> Option<&str> {
            span.fields.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
        };
        assert_eq!(field_val("thread_id"), Some("T-trace"));
        assert_eq!(field_val("event_type"), Some("test_event"));
    }
}
