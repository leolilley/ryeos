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

-- CAS entry attribution: why a CAS object/blob is present locally.
CREATE TABLE IF NOT EXISTS cas_entries (
    hash TEXT PRIMARY KEY,
    entry_kind TEXT NOT NULL CHECK (entry_kind IN ('object', 'blob')),
    bytes INTEGER NOT NULL CHECK (bytes >= 0),
    first_seen_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    source_principal TEXT,
    source_peer TEXT,
    job_id TEXT,
    state TEXT NOT NULL CHECK (state IN ('local', 'staged', 'accepted', 'mirrored', 'rejected'))
);

CREATE INDEX IF NOT EXISTS idx_cas_entries_state ON cas_entries(state);
CREATE INDEX IF NOT EXISTS idx_cas_entries_source_principal ON cas_entries(source_principal);
CREATE INDEX IF NOT EXISTS idx_cas_entries_source_peer ON cas_entries(source_peer);
CREATE INDEX IF NOT EXISTS idx_cas_entries_job_id ON cas_entries(job_id);
"#;

use crate::sqlite_schema;

/// Application ID stamp for projection.db.
/// RYPJ = 0x5259504a ("RY" + "PJ" for "projection").
const PROJECTION_APP_ID: i32 = 0x5259_504a;

/// Schema spec for projection.db — the single source of truth for
/// what tables/columns/indexes this database must contain.
fn projection_schema_spec() -> sqlite_schema::SchemaSpec {
    sqlite_schema::SchemaSpec {
        application_id: PROJECTION_APP_ID,
        tables: &[
            sqlite_schema::TableSpec {
                name: "projection_meta",
                columns: &[
                    sqlite_schema::ColumnSpec {
                        name: "chain_root_id",
                        col_type: "TEXT",
                        pk: true,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "indexed_chain_state_hash",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "updated_at",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                ],
            },
            sqlite_schema::TableSpec {
                name: "threads",
                columns: &[
                    sqlite_schema::ColumnSpec {
                        name: "thread_id",
                        col_type: "TEXT",
                        pk: true,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "chain_root_id",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "kind",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "status",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "item_ref",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "executor_ref",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "launch_mode",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "current_site_id",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "origin_site_id",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "upstream_thread_id",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "requested_by",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "created_at",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "updated_at",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "started_at",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "finished_at",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                ],
            },
            sqlite_schema::TableSpec {
                name: "events",
                columns: &[
                    sqlite_schema::ColumnSpec {
                        name: "event_id",
                        col_type: "INTEGER",
                        pk: true,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "chain_root_id",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "chain_seq",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "thread_id",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "thread_seq",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "event_type",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "durability",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "ts",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "prev_chain_event_hash",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "prev_thread_event_hash",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "payload",
                        col_type: "BLOB",
                        pk: false,
                        not_null: true,
                    },
                ],
            },
            sqlite_schema::TableSpec {
                name: "event_replay_index",
                columns: &[
                    sqlite_schema::ColumnSpec {
                        name: "thread_id",
                        col_type: "TEXT",
                        pk: true,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "last_indexed_chain_seq",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "updated_at",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                ],
            },
            sqlite_schema::TableSpec {
                name: "thread_edges",
                columns: &[
                    sqlite_schema::ColumnSpec {
                        name: "edge_id",
                        col_type: "INTEGER",
                        pk: true,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "chain_root_id",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "parent_thread_id",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "child_thread_id",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "spawn_seq",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "spawn_reason",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "created_at",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                ],
            },
            sqlite_schema::TableSpec {
                name: "thread_results",
                columns: &[
                    sqlite_schema::ColumnSpec {
                        name: "thread_id",
                        col_type: "TEXT",
                        pk: true,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "chain_root_id",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "status",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "result",
                        col_type: "BLOB",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "error",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "updated_at",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                ],
            },
            sqlite_schema::TableSpec {
                name: "thread_artifacts",
                columns: &[
                    sqlite_schema::ColumnSpec {
                        name: "artifact_id",
                        col_type: "INTEGER",
                        pk: true,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "chain_root_id",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "thread_id",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "kind",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "metadata",
                        col_type: "BLOB",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "created_at",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                ],
            },
            sqlite_schema::TableSpec {
                name: "thread_facets",
                columns: &[
                    sqlite_schema::ColumnSpec {
                        name: "facet_id",
                        col_type: "INTEGER",
                        pk: true,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "thread_id",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "key",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "value",
                        col_type: "BLOB",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "updated_at",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                ],
            },
            sqlite_schema::TableSpec {
                name: "cas_entries",
                columns: &[
                    sqlite_schema::ColumnSpec {
                        name: "hash",
                        col_type: "TEXT",
                        pk: true,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "entry_kind",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "bytes",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "first_seen_at",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "updated_at",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "source_principal",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "source_peer",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "job_id",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "state",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                ],
            },
        ],
        indexes: &[
            sqlite_schema::IndexSpec {
                name: "idx_threads_chain_root",
                table: "threads",
                columns: &["chain_root_id"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_threads_status",
                table: "threads",
                columns: &["status"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_threads_created_at",
                table: "threads",
                columns: &["created_at"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_threads_updated_at",
                table: "threads",
                columns: &["updated_at"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_events_chain_root",
                table: "events",
                columns: &["chain_root_id"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_events_thread_id",
                table: "events",
                columns: &["thread_id"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_events_ts",
                table: "events",
                columns: &["ts"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_edges_parent",
                table: "thread_edges",
                columns: &["parent_thread_id"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_edges_child",
                table: "thread_edges",
                columns: &["child_thread_id"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_results_chain_root",
                table: "thread_results",
                columns: &["chain_root_id"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_artifacts_thread",
                table: "thread_artifacts",
                columns: &["thread_id"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_artifacts_chain_root",
                table: "thread_artifacts",
                columns: &["chain_root_id"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_facets_thread",
                table: "thread_facets",
                columns: &["thread_id"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_cas_entries_state",
                table: "cas_entries",
                columns: &["state"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_cas_entries_source_principal",
                table: "cas_entries",
                columns: &["source_principal"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_cas_entries_source_peer",
                table: "cas_entries",
                columns: &["source_peer"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_cas_entries_job_id",
                table: "cas_entries",
                columns: &["job_id"],
                unique: false,
            },
        ],
    }
}
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CasEntryKind {
    Object,
    Blob,
}

impl CasEntryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Object => "object",
            Self::Blob => "blob",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CasEntryState {
    Local,
    Staged,
    Accepted,
    Mirrored,
    Rejected,
}

impl CasEntryState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Staged => "staged",
            Self::Accepted => "accepted",
            Self::Mirrored => "mirrored",
            Self::Rejected => "rejected",
        }
    }

    fn from_str(value: &str) -> anyhow::Result<Self> {
        match value {
            "local" => Ok(Self::Local),
            "staged" => Ok(Self::Staged),
            "accepted" => Ok(Self::Accepted),
            "mirrored" => Ok(Self::Mirrored),
            "rejected" => Ok(Self::Rejected),
            other => anyhow::bail!("unknown CAS entry state: {other}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CasEntryAttribution {
    pub hash: String,
    pub entry_kind: CasEntryKind,
    pub bytes: u64,
    pub first_seen_at: String,
    pub updated_at: String,
    pub source_principal: Option<String>,
    pub source_peer: Option<String>,
    pub job_id: Option<String>,
    pub state: CasEntryState,
}

#[derive(Debug, Clone)]
pub struct NewCasEntryAttribution {
    pub hash: String,
    pub entry_kind: CasEntryKind,
    pub bytes: u64,
    pub source_principal: Option<String>,
    pub source_peer: Option<String>,
    pub job_id: Option<String>,
    pub state: CasEntryState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CasEntriesByStateSummary {
    pub state: CasEntryState,
    pub count: u64,
    pub total_bytes: u64,
}

impl ProjectionDb {
    /// Open or create a projection database.
    ///
    /// If the file exists, verifies it matches the schema spec exactly
    /// (tables, columns, indexes, application_id). If the file is empty
    /// or missing, initialises it from the DDL and stamps the
    /// application_id.
    pub fn open(path: &std::path::Path) -> anyhow::Result<Self> {
        let conn =
            rusqlite::Connection::open(path).context("failed to open projection database")?;

        let spec = projection_schema_spec();

        if sqlite_schema::is_empty_or_owned(&conn, spec.application_id)? {
            sqlite_schema::init_owned(&conn, &spec, SCHEMA_SQL, path)?;
        } else {
            sqlite_schema::assert_owned(&conn, &spec, path)?;
        }

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
    pub fn get_projection_meta(
        &self,
        chain_root_id: &str,
    ) -> anyhow::Result<Option<ProjectionMeta>> {
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
    pub fn update_projection_meta(&self, meta: &ProjectionMeta) -> anyhow::Result<()> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO projection_meta (chain_root_id, indexed_chain_state_hash, updated_at) VALUES (?, ?, ?)",
                rusqlite::params![&meta.chain_root_id, &meta.indexed_chain_state_hash, &meta.updated_at],
            )
            .context("failed to update projection_meta")?;

        Ok(())
    }

    pub fn record_cas_entry(&self, entry: &NewCasEntryAttribution) -> anyhow::Result<()> {
        if !lillux::valid_hash(&entry.hash) {
            anyhow::bail!("invalid CAS entry hash: {}", entry.hash);
        }
        let bytes = i64::try_from(entry.bytes).context("CAS entry byte count exceeds i64")?;
        let now = lillux::time::iso8601_now();
        self.conn
            .execute(
                "INSERT INTO cas_entries (
                    hash, entry_kind, bytes, first_seen_at, updated_at,
                    source_principal, source_peer, job_id, state
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
                ON CONFLICT(hash) DO UPDATE SET
                    entry_kind = excluded.entry_kind,
                    bytes = excluded.bytes,
                    updated_at = excluded.updated_at,
                    source_principal = COALESCE(excluded.source_principal, cas_entries.source_principal),
                    source_peer = COALESCE(excluded.source_peer, cas_entries.source_peer),
                    job_id = COALESCE(excluded.job_id, cas_entries.job_id),
                    state = excluded.state",
                rusqlite::params![
                    &entry.hash,
                    entry.entry_kind.as_str(),
                    bytes,
                    &now,
                    &now,
                    &entry.source_principal,
                    &entry.source_peer,
                    &entry.job_id,
                    entry.state.as_str(),
                ],
            )
            .context("failed to record CAS entry attribution")?;
        Ok(())
    }

    pub fn set_cas_entry_state(&self, hash: &str, state: CasEntryState) -> anyhow::Result<()> {
        if !lillux::valid_hash(hash) {
            anyhow::bail!("invalid CAS entry hash: {hash}");
        }
        let changed = self
            .conn
            .execute(
                "UPDATE cas_entries SET state = ?, updated_at = ? WHERE hash = ?",
                rusqlite::params![state.as_str(), lillux::time::iso8601_now(), hash],
            )
            .context("failed to update CAS entry attribution state")?;
        if changed == 0 {
            anyhow::bail!("CAS entry attribution not found for hash {hash}");
        }
        Ok(())
    }

    pub fn get_cas_entry(&self, hash: &str) -> anyhow::Result<Option<CasEntryAttribution>> {
        if !lillux::valid_hash(hash) {
            anyhow::bail!("invalid CAS entry hash: {hash}");
        }
        self.conn
            .query_row(
                "SELECT hash, entry_kind, bytes, first_seen_at, updated_at,
                    source_principal, source_peer, job_id, state
                 FROM cas_entries WHERE hash = ?",
                [hash],
                cas_entry_from_row,
            )
            .optional()
            .context("failed to get CAS entry attribution")
    }

    pub fn list_cas_entries_by_state(
        &self,
        state: CasEntryState,
    ) -> anyhow::Result<Vec<CasEntryAttribution>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT hash, entry_kind, bytes, first_seen_at, updated_at,
                    source_principal, source_peer, job_id, state
                 FROM cas_entries WHERE state = ? ORDER BY first_seen_at, hash",
            )
            .context("failed to prepare CAS entry attribution query")?;
        let rows = stmt
            .query_map([state.as_str()], cas_entry_from_row)
            .context("failed to query CAS entry attribution by state")?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to collect CAS entry attribution rows")
    }

    pub fn cas_entries_by_state_summary(&self) -> anyhow::Result<Vec<CasEntriesByStateSummary>> {
        let mut stmt = self
            .conn
            .prepare("SELECT state, COUNT(*) AS count, COALESCE(SUM(bytes), 0) AS total_bytes FROM cas_entries GROUP BY state ORDER BY state")
            .context("failed to prepare CAS entry attribution summary")?;
        let rows = stmt
            .query_map([], |row| {
                let state: String = row.get("state")?;
                let count: i64 = row.get("count")?;
                let total_bytes: i64 = row.get("total_bytes")?;
                Ok(CasEntriesByStateSummary {
                    state: CasEntryState::from_str(&state)
                        .map_err(|_| rusqlite::Error::InvalidQuery)?,
                    count: u64::try_from(count).map_err(|_| rusqlite::Error::InvalidQuery)?,
                    total_bytes: u64::try_from(total_bytes)
                        .map_err(|_| rusqlite::Error::InvalidQuery)?,
                })
            })
            .context("failed to query CAS entry attribution summary")?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to collect CAS entry attribution summary")
    }
}

fn cas_entry_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CasEntryAttribution> {
    let entry_kind: String = row.get("entry_kind")?;
    let state: String = row.get("state")?;
    let bytes: i64 = row.get("bytes")?;
    Ok(CasEntryAttribution {
        hash: row.get("hash")?,
        entry_kind: match entry_kind.as_str() {
            "object" => CasEntryKind::Object,
            "blob" => CasEntryKind::Blob,
            _ => return Err(rusqlite::Error::InvalidQuery),
        },
        bytes: u64::try_from(bytes).map_err(|_| rusqlite::Error::InvalidQuery)?,
        first_seen_at: row.get("first_seen_at")?,
        updated_at: row.get("updated_at")?,
        source_principal: row.get("source_principal")?,
        source_peer: row.get("source_peer")?,
        job_id: row.get("job_id")?,
        state: CasEntryState::from_str(&state).map_err(|_| rusqlite::Error::InvalidQuery)?,
    })
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

    db.connection()
        .execute(
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

    // Project the snapshot's `result` / `error` fields into the
    // `thread_results` table so callers (e.g. graph runtime callback
    // dispatch through `dispatch_subprocess` → `build_execute_result`)
    // can read the leaf value back. Without this insert, the
    // `thread_results` table stays empty even on terminal status, and
    // every `get_thread_result` returns None — which surfaces as
    // `response.result == null` at the callback boundary.
    //
    // Idempotent under INSERT OR REPLACE: the snapshot is the source
    // of truth, so re-projection (rebuild, re-apply) overwrites with
    // the same row.
    if snapshot.result.is_some() || snapshot.error.is_some() {
        let result_blob = snapshot
            .result
            .as_ref()
            .map(|v| serde_json::to_vec(v).unwrap_or_default());
        let error_text = snapshot.error.as_ref().map(|v| match v {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        });
        db.connection()
            .execute(
                "INSERT OR REPLACE INTO thread_results (
                thread_id, chain_root_id, status, result, error, updated_at
            ) VALUES (?, ?, ?, ?, ?, ?)",
                rusqlite::params![
                    &snapshot.thread_id,
                    chain_root_id,
                    snapshot.status.to_string(),
                    result_blob,
                    error_text,
                    &snapshot.updated_at,
                ],
            )
            .context("failed to project thread result")?;
    }

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
pub fn project_event(db: &ProjectionDb, event: &crate::ThreadEvent) -> anyhow::Result<()> {
    event.validate()?;

    let payload =
        serde_json::to_vec(&event.payload).context("failed to serialize event payload")?;

    db.connection()
        .execute(
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
            let metadata_blob = metadata
                .map(|m| serde_json::to_vec(&m).context("failed to serialize metadata"))
                .transpose()?;

            db.connection()
                .execute(
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
    db.connection()
        .execute(
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
    use ryeos_tracing::test as trace_test;
    use std::collections::BTreeMap;

    #[test]
    fn open_creates_projection_db() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("projection.db");
        let db = ProjectionDb::open(&path).unwrap();

        // Verify tables were created
        let mut stmt = db
            .conn
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
        assert_eq!(
            retrieved.indexed_chain_state_hash,
            meta.indexed_chain_state_hash
        );
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
    fn record_cas_entry_preserves_first_seen_and_updates_state() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("projection.db");
        let db = ProjectionDb::open(&path).unwrap();
        let hash = "ab".repeat(32);

        db.record_cas_entry(&NewCasEntryAttribution {
            hash: hash.clone(),
            entry_kind: CasEntryKind::Object,
            bytes: 128,
            source_principal: Some("fp:source".to_string()),
            source_peer: Some("peer-a".to_string()),
            job_id: Some("job-a".to_string()),
            state: CasEntryState::Staged,
        })
        .unwrap();

        let first = db.get_cas_entry(&hash).unwrap().unwrap();
        assert_eq!(first.hash, hash);
        assert_eq!(first.entry_kind, CasEntryKind::Object);
        assert_eq!(first.bytes, 128);
        assert_eq!(first.state, CasEntryState::Staged);
        assert_eq!(first.source_principal.as_deref(), Some("fp:source"));

        db.record_cas_entry(&NewCasEntryAttribution {
            hash: hash.clone(),
            entry_kind: CasEntryKind::Object,
            bytes: 256,
            source_principal: None,
            source_peer: None,
            job_id: None,
            state: CasEntryState::Accepted,
        })
        .unwrap();

        let updated = db.get_cas_entry(&hash).unwrap().unwrap();
        assert_eq!(updated.first_seen_at, first.first_seen_at);
        assert_eq!(updated.bytes, 256);
        assert_eq!(updated.state, CasEntryState::Accepted);
        assert_eq!(updated.source_principal.as_deref(), Some("fp:source"));
        assert_eq!(updated.source_peer.as_deref(), Some("peer-a"));
        assert_eq!(updated.job_id.as_deref(), Some("job-a"));
    }

    #[test]
    fn cas_entry_state_queries_are_deterministic() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("projection.db");
        let db = ProjectionDb::open(&path).unwrap();
        let staged_hash = "cd".repeat(32);
        let mirrored_hash = "ef".repeat(32);

        db.record_cas_entry(&NewCasEntryAttribution {
            hash: staged_hash.clone(),
            entry_kind: CasEntryKind::Blob,
            bytes: 11,
            source_principal: None,
            source_peer: Some("peer-b".to_string()),
            job_id: Some("job-b".to_string()),
            state: CasEntryState::Staged,
        })
        .unwrap();
        db.record_cas_entry(&NewCasEntryAttribution {
            hash: mirrored_hash.clone(),
            entry_kind: CasEntryKind::Object,
            bytes: 22,
            source_principal: None,
            source_peer: None,
            job_id: None,
            state: CasEntryState::Mirrored,
        })
        .unwrap();
        db.set_cas_entry_state(&staged_hash, CasEntryState::Accepted)
            .unwrap();

        let accepted = db
            .list_cas_entries_by_state(CasEntryState::Accepted)
            .unwrap();
        assert_eq!(accepted.len(), 1);
        assert_eq!(accepted[0].hash, staged_hash);
        assert_eq!(accepted[0].entry_kind, CasEntryKind::Blob);

        let summary = db.cas_entries_by_state_summary().unwrap();
        assert_eq!(summary.len(), 2);
        assert_eq!(summary[0].state, CasEntryState::Accepted);
        assert_eq!(summary[0].count, 1);
        assert_eq!(summary[0].total_bytes, 11);
        assert_eq!(summary[1].state, CasEntryState::Mirrored);
        assert_eq!(summary[1].count, 1);
        assert_eq!(summary[1].total_bytes, 22);
    }

    #[test]
    fn record_cas_entry_rejects_invalid_hash() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("projection.db");
        let db = ProjectionDb::open(&path).unwrap();

        let err = db
            .record_cas_entry(&NewCasEntryAttribution {
                hash: "not-a-hash".to_string(),
                entry_kind: CasEntryKind::Object,
                bytes: 1,
                source_principal: None,
                source_peer: None,
                job_id: None,
                state: CasEntryState::Local,
            })
            .unwrap_err();
        assert!(err.to_string().contains("invalid CAS entry hash"));
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
        assert!(
            span.is_some(),
            "expected state:project_event span, got: {:?}",
            spans.iter().map(|s| &s.name).collect::<Vec<_>>()
        );

        let span = span.unwrap();
        let field_val = |name: &str| -> Option<&str> {
            span.fields
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(field_val("thread_id"), Some("T-trace"));
        assert_eq!(field_val("event_type"), Some("test_event"));
    }
}
