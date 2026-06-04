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
    hash TEXT NOT NULL,
    entry_kind TEXT NOT NULL CHECK (entry_kind IN ('object', 'blob')),
    bytes INTEGER NOT NULL CHECK (bytes >= 0),
    first_seen_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    source_principal TEXT,
    source_peer TEXT,
    job_id TEXT,
    state TEXT NOT NULL CHECK (state IN ('local', 'staged', 'accepted', 'mirrored', 'rejected')),
    PRIMARY KEY(entry_kind, hash)
);

CREATE INDEX IF NOT EXISTS idx_cas_entries_state ON cas_entries(state);
CREATE INDEX IF NOT EXISTS idx_cas_entries_source_principal ON cas_entries(source_principal);
CREATE INDEX IF NOT EXISTS idx_cas_entries_source_peer ON cas_entries(source_peer);
CREATE INDEX IF NOT EXISTS idx_cas_entries_job_id ON cas_entries(job_id);

-- Durable distributed-substrate jobs. These are operational records, not CAS facts.
CREATE TABLE IF NOT EXISTS sync_jobs (
    job_id TEXT PRIMARY KEY,
    operation_type TEXT NOT NULL,
    peer TEXT,
    state TEXT NOT NULL CHECK (state IN ('planned', 'running', 'completed', 'failed', 'retryable', 'cancelled')),
    phase TEXT NOT NULL,
    roots_json BLOB NOT NULL,
    heads_json BLOB NOT NULL,
    uploaded_hashes_json BLOB NOT NULL,
    fetched_hashes_json BLOB NOT NULL,
    attempt_count INTEGER NOT NULL CHECK (attempt_count >= 0),
    max_attempts INTEGER NOT NULL CHECK (max_attempts >= 0),
    last_error TEXT,
    result_json BLOB,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    finished_at TEXT
);

CREATE INDEX IF NOT EXISTS idx_sync_jobs_state ON sync_jobs(state);
CREATE INDEX IF NOT EXISTS idx_sync_jobs_operation_type ON sync_jobs(operation_type);
CREATE INDEX IF NOT EXISTS idx_sync_jobs_peer ON sync_jobs(peer);

CREATE TABLE IF NOT EXISTS sync_job_attempts (
    attempt_id TEXT PRIMARY KEY,
    job_id TEXT NOT NULL,
    attempt_number INTEGER NOT NULL CHECK (attempt_number > 0),
    worker_id TEXT,
    state TEXT NOT NULL CHECK (state IN ('running', 'completed', 'failed', 'cancelled')),
    phase TEXT NOT NULL,
    started_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    finished_at TEXT,
    error TEXT,
    result_json BLOB,
    UNIQUE(job_id, attempt_number)
);

CREATE INDEX IF NOT EXISTS idx_sync_job_attempts_job_id ON sync_job_attempts(job_id);
CREATE INDEX IF NOT EXISTS idx_sync_job_attempts_state ON sync_job_attempts(state);
CREATE INDEX IF NOT EXISTS idx_sync_job_attempts_worker_id ON sync_job_attempts(worker_id);

-- Admission attestation lookup index. Attestations remain immutable CAS objects;
-- this projection makes subject/policy/issuer lookup efficient.
CREATE TABLE IF NOT EXISTS admission_attestations (
    attestation_hash TEXT PRIMARY KEY,
    subject_hash TEXT NOT NULL,
    policy TEXT NOT NULL,
    claim TEXT NOT NULL,
    issuer TEXT NOT NULL,
    issued_at TEXT NOT NULL,
    expires_at TEXT,
    head_ref_path TEXT,
    indexed_at TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('accepted', 'rejected'))
);

CREATE INDEX IF NOT EXISTS idx_admission_attestations_subject ON admission_attestations(subject_hash);
CREATE INDEX IF NOT EXISTS idx_admission_attestations_policy ON admission_attestations(policy);
CREATE INDEX IF NOT EXISTS idx_admission_attestations_issuer ON admission_attestations(issuer);
CREATE INDEX IF NOT EXISTS idx_admission_attestations_subject_policy_claim_issuer ON admission_attestations(subject_hash, policy, claim, issuer);
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
                        pk: true,
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
            sqlite_schema::TableSpec {
                name: "sync_jobs",
                columns: &[
                    sqlite_schema::ColumnSpec {
                        name: "job_id",
                        col_type: "TEXT",
                        pk: true,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "operation_type",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "peer",
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
                    sqlite_schema::ColumnSpec {
                        name: "phase",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "roots_json",
                        col_type: "BLOB",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "heads_json",
                        col_type: "BLOB",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "uploaded_hashes_json",
                        col_type: "BLOB",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "fetched_hashes_json",
                        col_type: "BLOB",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "attempt_count",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "max_attempts",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "last_error",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "result_json",
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
                    sqlite_schema::ColumnSpec {
                        name: "updated_at",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
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
                name: "sync_job_attempts",
                columns: &[
                    sqlite_schema::ColumnSpec {
                        name: "attempt_id",
                        col_type: "TEXT",
                        pk: true,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "job_id",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "attempt_number",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "worker_id",
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
                    sqlite_schema::ColumnSpec {
                        name: "phase",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "started_at",
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
                        name: "finished_at",
                        col_type: "TEXT",
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
                        name: "result_json",
                        col_type: "BLOB",
                        pk: false,
                        not_null: false,
                    },
                ],
            },
            sqlite_schema::TableSpec {
                name: "admission_attestations",
                columns: &[
                    sqlite_schema::ColumnSpec {
                        name: "attestation_hash",
                        col_type: "TEXT",
                        pk: true,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "subject_hash",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "policy",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "claim",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "issuer",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "issued_at",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "expires_at",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "head_ref_path",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "indexed_at",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
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
            sqlite_schema::IndexSpec {
                name: "idx_sync_jobs_state",
                table: "sync_jobs",
                columns: &["state"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_sync_jobs_operation_type",
                table: "sync_jobs",
                columns: &["operation_type"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_sync_jobs_peer",
                table: "sync_jobs",
                columns: &["peer"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_sync_job_attempts_job_id",
                table: "sync_job_attempts",
                columns: &["job_id"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_sync_job_attempts_state",
                table: "sync_job_attempts",
                columns: &["state"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_sync_job_attempts_worker_id",
                table: "sync_job_attempts",
                columns: &["worker_id"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_admission_attestations_subject",
                table: "admission_attestations",
                columns: &["subject_hash"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_admission_attestations_policy",
                table: "admission_attestations",
                columns: &["policy"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_admission_attestations_issuer",
                table: "admission_attestations",
                columns: &["issuer"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_admission_attestations_subject_policy_claim_issuer",
                table: "admission_attestations",
                columns: &["subject_hash", "policy", "claim", "issuer"],
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionAttestationState {
    Accepted,
    Rejected,
}

impl AdmissionAttestationState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Rejected => "rejected",
        }
    }

    fn from_str(value: &str) -> anyhow::Result<Self> {
        match value {
            "accepted" => Ok(Self::Accepted),
            "rejected" => Ok(Self::Rejected),
            other => anyhow::bail!("unknown admission attestation state: {other}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmissionAttestationRecord {
    pub attestation_hash: String,
    pub subject_hash: String,
    pub policy: String,
    pub claim: String,
    pub issuer: String,
    pub issued_at: String,
    pub expires_at: Option<String>,
    pub head_ref_path: Option<String>,
    pub indexed_at: String,
    pub state: AdmissionAttestationState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAdmissionAttestationRecord {
    pub attestation_hash: String,
    pub subject_hash: String,
    pub policy: String,
    pub claim: String,
    pub issuer: String,
    pub issued_at: String,
    pub expires_at: Option<String>,
    pub head_ref_path: Option<String>,
    pub state: AdmissionAttestationState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncJobState {
    Planned,
    Running,
    Completed,
    Failed,
    Retryable,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncJobAttemptState {
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl SyncJobAttemptState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    fn from_str(value: &str) -> anyhow::Result<Self> {
        match value {
            "running" => Ok(Self::Running),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            other => anyhow::bail!("unknown sync job attempt state: {other}"),
        }
    }

    fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

impl SyncJobState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Retryable => "retryable",
            Self::Cancelled => "cancelled",
        }
    }

    fn from_str(value: &str) -> anyhow::Result<Self> {
        match value {
            "planned" => Ok(Self::Planned),
            "running" => Ok(Self::Running),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "retryable" => Ok(Self::Retryable),
            "cancelled" => Ok(Self::Cancelled),
            other => anyhow::bail!("unknown sync job state: {other}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SyncJobRecord {
    pub job_id: String,
    pub operation_type: String,
    pub peer: Option<String>,
    pub state: SyncJobState,
    pub phase: String,
    pub roots: Vec<String>,
    pub heads: Vec<String>,
    pub uploaded_hashes: Vec<String>,
    pub fetched_hashes: Vec<String>,
    pub attempt_count: u64,
    pub max_attempts: u64,
    pub last_error: Option<String>,
    pub result: Option<serde_json::Value>,
    pub created_at: String,
    pub updated_at: String,
    pub finished_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSyncJob {
    pub job_id: String,
    pub operation_type: String,
    pub peer: Option<String>,
    pub roots: Vec<String>,
    pub heads: Vec<String>,
    pub max_attempts: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SyncJobUpdate {
    pub state: SyncJobState,
    pub phase: String,
    pub roots: Option<Vec<String>>,
    pub heads: Option<Vec<String>>,
    pub uploaded_hashes: Vec<String>,
    pub fetched_hashes: Vec<String>,
    pub last_error: Option<String>,
    pub result: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SyncJobAttemptRecord {
    pub attempt_id: String,
    pub job_id: String,
    pub attempt_number: u64,
    pub worker_id: Option<String>,
    pub state: SyncJobAttemptState,
    pub phase: String,
    pub started_at: String,
    pub updated_at: String,
    pub finished_at: Option<String>,
    pub error: Option<String>,
    pub result: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSyncJobAttempt {
    pub attempt_id: String,
    pub job_id: String,
    pub worker_id: Option<String>,
    pub phase: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FinishSyncJobAttempt {
    pub state: SyncJobAttemptState,
    pub phase: String,
    pub error: Option<String>,
    pub result: Option<serde_json::Value>,
}

impl ProjectionDb {
    fn immediate_transaction<T>(
        &self,
        label: &'static str,
        f: impl FnOnce() -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        self.conn
            .execute_batch("BEGIN IMMEDIATE")
            .with_context(|| format!("failed to begin {label} transaction"))?;
        match f() {
            Ok(value) => {
                self.conn
                    .execute_batch("COMMIT")
                    .with_context(|| format!("failed to commit {label} transaction"))?;
                Ok(value)
            }
            Err(err) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(err)
            }
        }
    }

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
        validate_canonical_hash("CAS entry hash", &entry.hash)?;
        let bytes = i64::try_from(entry.bytes).context("CAS entry byte count exceeds i64")?;
        let now = lillux::time::iso8601_now();
        self.conn
            .execute(
                "INSERT INTO cas_entries (
                    hash, entry_kind, bytes, first_seen_at, updated_at,
                    source_principal, source_peer, job_id, state
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
                ON CONFLICT(entry_kind, hash) DO UPDATE SET
                    bytes = CASE
                        WHEN cas_entries.state IN ('local', 'accepted', 'mirrored')
                            AND excluded.state IN ('staged', 'rejected')
                            THEN cas_entries.bytes
                        WHEN cas_entries.state = 'rejected'
                            AND excluded.state = 'staged'
                            THEN cas_entries.bytes
                        ELSE excluded.bytes
                    END,
                    updated_at = excluded.updated_at,
                    source_principal = COALESCE(excluded.source_principal, cas_entries.source_principal),
                    source_peer = COALESCE(excluded.source_peer, cas_entries.source_peer),
                    job_id = COALESCE(excluded.job_id, cas_entries.job_id),
                    state = CASE
                        WHEN cas_entries.state IN ('local', 'accepted', 'mirrored')
                            AND excluded.state IN ('staged', 'rejected')
                            THEN cas_entries.state
                        WHEN cas_entries.state = 'rejected'
                            AND excluded.state = 'staged'
                            THEN cas_entries.state
                        ELSE excluded.state
                    END",
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

    pub fn set_cas_entry_state(
        &self,
        entry_kind: CasEntryKind,
        hash: &str,
        state: CasEntryState,
    ) -> anyhow::Result<()> {
        validate_canonical_hash("CAS entry hash", hash)?;
        let current = self.get_cas_entry(entry_kind, hash)?.ok_or_else(|| {
            anyhow::anyhow!(
                "CAS entry attribution not found for {} hash {hash}",
                entry_kind.as_str()
            )
        })?;
        if !cas_entry_transition_allowed(current.state, state) {
            anyhow::bail!(
                "illegal CAS entry state transition for {} hash {}: {} -> {}",
                entry_kind.as_str(),
                hash,
                current.state.as_str(),
                state.as_str()
            );
        }
        let changed = self
            .conn
            .execute(
                "UPDATE cas_entries SET state = ?, updated_at = ? WHERE entry_kind = ? AND hash = ?",
                rusqlite::params![
                    state.as_str(),
                    lillux::time::iso8601_now(),
                    entry_kind.as_str(),
                    hash,
                ],
            )
            .context("failed to update CAS entry attribution state")?;
        if changed == 0 {
            anyhow::bail!(
                "CAS entry attribution not found for {} hash {hash}",
                entry_kind.as_str()
            );
        }
        Ok(())
    }

    pub fn get_cas_entry(
        &self,
        entry_kind: CasEntryKind,
        hash: &str,
    ) -> anyhow::Result<Option<CasEntryAttribution>> {
        validate_canonical_hash("CAS entry hash", hash)?;
        self.conn
            .query_row(
                "SELECT hash, entry_kind, bytes, first_seen_at, updated_at,
                    source_principal, source_peer, job_id, state
                 FROM cas_entries WHERE entry_kind = ? AND hash = ?",
                rusqlite::params![entry_kind.as_str(), hash],
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

    pub fn record_admission_attestation(
        &self,
        record: &NewAdmissionAttestationRecord,
    ) -> anyhow::Result<()> {
        validate_canonical_hash("admission attestation hash", &record.attestation_hash)?;
        validate_canonical_hash("admission subject hash", &record.subject_hash)?;
        validate_non_empty_label("admission policy", &record.policy)?;
        validate_non_empty_label("admission claim", &record.claim)?;
        validate_non_empty_label("admission issuer", &record.issuer)?;
        validate_non_empty_label("admission issued_at", &record.issued_at)?;
        if let Some(head_ref_path) = record.head_ref_path.as_deref() {
            if head_ref_path.is_empty()
                || head_ref_path.len() > 512
                || head_ref_path.starts_with('/')
                || head_ref_path.contains("..")
            {
                anyhow::bail!("invalid admission head_ref_path: {head_ref_path}");
            }
        }
        let now = lillux::time::iso8601_now();
        self.conn
            .execute(
                "INSERT INTO admission_attestations (
                    attestation_hash, subject_hash, policy, claim, issuer, issued_at,
                    expires_at, head_ref_path, indexed_at, state
                 ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(attestation_hash) DO UPDATE SET
                    subject_hash = excluded.subject_hash,
                    policy = excluded.policy,
                    claim = excluded.claim,
                    issuer = excluded.issuer,
                    issued_at = excluded.issued_at,
                    expires_at = excluded.expires_at,
                    head_ref_path = excluded.head_ref_path,
                    indexed_at = excluded.indexed_at,
                    state = excluded.state",
                rusqlite::params![
                    &record.attestation_hash,
                    &record.subject_hash,
                    &record.policy,
                    &record.claim,
                    &record.issuer,
                    &record.issued_at,
                    &record.expires_at,
                    &record.head_ref_path,
                    &now,
                    record.state.as_str(),
                ],
            )
            .context("failed to record admission attestation index")?;
        Ok(())
    }

    pub fn list_admission_attestations_for_subject(
        &self,
        subject_hash: &str,
        policy: Option<&str>,
    ) -> anyhow::Result<Vec<AdmissionAttestationRecord>> {
        validate_canonical_hash("admission subject hash", subject_hash)?;
        if let Some(policy) = policy {
            validate_non_empty_label("admission policy", policy)?;
            let mut stmt = self
                .conn
                .prepare(
                    "SELECT attestation_hash, subject_hash, policy, claim, issuer, issued_at,
                        expires_at, head_ref_path, indexed_at, state
                     FROM admission_attestations
                     WHERE subject_hash = ? AND policy = ?
                     ORDER BY indexed_at DESC, attestation_hash DESC",
                )
                .context("failed to prepare admission attestation subject/policy query")?;
            let rows = stmt
                .query_map(
                    rusqlite::params![subject_hash, policy],
                    admission_attestation_from_row,
                )
                .context("failed to query admission attestations by subject/policy")?;
            return rows
                .collect::<rusqlite::Result<Vec<_>>>()
                .context("failed to collect admission attestations by subject/policy");
        }

        let mut stmt = self
            .conn
            .prepare(
                "SELECT attestation_hash, subject_hash, policy, claim, issuer, issued_at,
                    expires_at, head_ref_path, indexed_at, state
                 FROM admission_attestations
                 WHERE subject_hash = ?
                 ORDER BY indexed_at DESC, attestation_hash DESC",
            )
            .context("failed to prepare admission attestation subject query")?;
        let rows = stmt
            .query_map([subject_hash], admission_attestation_from_row)
            .context("failed to query admission attestations by subject")?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to collect admission attestations by subject")
    }

    pub fn create_sync_job(&self, job: &NewSyncJob) -> anyhow::Result<SyncJobRecord> {
        validate_sync_job_id(&job.job_id)?;
        validate_non_empty_label("operation_type", &job.operation_type)?;
        for hash in job.roots.iter().chain(job.heads.iter()) {
            validate_canonical_hash("sync job root/head hash", hash)?;
        }
        let max_attempts = i64::try_from(job.max_attempts).context("max_attempts exceeds i64")?;
        let now = lillux::time::iso8601_now();
        let roots_json = serde_json::to_vec(&job.roots).context("failed to serialize job roots")?;
        let heads_json = serde_json::to_vec(&job.heads).context("failed to serialize job heads")?;
        let empty_hashes = serde_json::to_vec(&Vec::<String>::new())?;
        self.conn
            .execute(
                "INSERT INTO sync_jobs (
                    job_id, operation_type, peer, state, phase, roots_json, heads_json,
                    uploaded_hashes_json, fetched_hashes_json, attempt_count, max_attempts,
                    last_error, result_json, created_at, updated_at, finished_at
                 ) VALUES (?, ?, ?, 'planned', 'planned', ?, ?, ?, ?, 0, ?, NULL, NULL, ?, ?, NULL)",
                rusqlite::params![
                    &job.job_id,
                    &job.operation_type,
                    &job.peer,
                    roots_json,
                    heads_json,
                    empty_hashes,
                    empty_hashes,
                    max_attempts,
                    &now,
                    &now,
                ],
            )
            .context("failed to create sync job")?;
        self.get_sync_job(&job.job_id)?
            .ok_or_else(|| anyhow::anyhow!("created sync job {} not found", job.job_id))
    }

    pub fn update_sync_job(&self, job_id: &str, update: &SyncJobUpdate) -> anyhow::Result<()> {
        validate_sync_job_id(job_id)?;
        validate_non_empty_label("phase", &update.phase)?;
        let current_state = self
            .conn
            .query_row(
                "SELECT state FROM sync_jobs WHERE job_id = ?",
                [job_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .context("failed to load sync job state")?
            .ok_or_else(|| anyhow::anyhow!("sync job not found: {job_id}"))?;
        let current_state = SyncJobState::from_str(&current_state)?;
        validate_sync_job_transition(current_state, update.state)?;
        if matches!(
            update.state,
            SyncJobState::Completed | SyncJobState::Failed | SyncJobState::Cancelled
        ) {
            let running_attempts: i64 = self
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM sync_job_attempts WHERE job_id = ? AND state = 'running'",
                    [job_id],
                    |row| row.get(0),
                )
                .context("failed to count running sync job attempts")?;
            if running_attempts > 0 {
                anyhow::bail!(
                    "cannot mark sync job {job_id} terminal while {running_attempts} attempt(s) are still running"
                );
            }
        }
        for hash in update
            .roots
            .iter()
            .flatten()
            .chain(update.heads.iter().flatten())
            .chain(update.uploaded_hashes.iter())
            .chain(update.fetched_hashes.iter())
        {
            validate_canonical_hash("sync job transfer hash", hash)?;
        }
        let roots_json = update
            .roots
            .as_ref()
            .map(serde_json::to_vec)
            .transpose()
            .context("failed to serialize job roots")?;
        let heads_json = update
            .heads
            .as_ref()
            .map(serde_json::to_vec)
            .transpose()
            .context("failed to serialize job heads")?;
        let uploaded_json = serde_json::to_vec(&update.uploaded_hashes)
            .context("failed to serialize uploaded hashes")?;
        let fetched_json = serde_json::to_vec(&update.fetched_hashes)
            .context("failed to serialize fetched hashes")?;
        let result_json = update
            .result
            .as_ref()
            .map(serde_json::to_vec)
            .transpose()
            .context("failed to serialize sync job result")?;
        let now = lillux::time::iso8601_now();
        let finished_at = match update.state {
            SyncJobState::Completed | SyncJobState::Failed | SyncJobState::Cancelled => {
                Some(now.clone())
            }
            SyncJobState::Planned | SyncJobState::Running | SyncJobState::Retryable => None,
        };
        let changed = self
            .conn
            .execute(
                "UPDATE sync_jobs SET
                state = ?, phase = ?,
                roots_json = COALESCE(?, roots_json), heads_json = COALESCE(?, heads_json),
                uploaded_hashes_json = ?, fetched_hashes_json = ?,
                last_error = ?, result_json = ?,
                updated_at = ?, finished_at = ?
             WHERE job_id = ?",
                rusqlite::params![
                    update.state.as_str(),
                    &update.phase,
                    roots_json,
                    heads_json,
                    uploaded_json,
                    fetched_json,
                    &update.last_error,
                    result_json,
                    &now,
                    &finished_at,
                    job_id,
                ],
            )
            .context("failed to update sync job")?;
        debug_assert_eq!(changed, 1);
        Ok(())
    }

    pub fn create_sync_job_attempt(
        &self,
        attempt: &NewSyncJobAttempt,
    ) -> anyhow::Result<SyncJobAttemptRecord> {
        self.immediate_transaction("create sync job attempt", || {
            self.create_sync_job_attempt_inner(attempt)
        })
    }

    fn create_sync_job_attempt_inner(
        &self,
        attempt: &NewSyncJobAttempt,
    ) -> anyhow::Result<SyncJobAttemptRecord> {
        validate_sync_job_id(&attempt.job_id)?;
        validate_sync_job_id(&attempt.attempt_id)?;
        validate_non_empty_label("phase", &attempt.phase)?;
        if let Some(worker_id) = attempt.worker_id.as_deref() {
            validate_non_empty_label("worker_id", worker_id)?;
        }

        let (job_state, attempt_count, max_attempts) = self
            .conn
            .query_row(
                "SELECT state, attempt_count, max_attempts FROM sync_jobs WHERE job_id = ?",
                [&attempt.job_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()
            .context("failed to load sync job state for attempt")?
            .ok_or_else(|| anyhow::anyhow!("sync job not found: {}", attempt.job_id))?;
        let job_state = SyncJobState::from_str(&job_state)?;
        if !matches!(
            job_state,
            SyncJobState::Planned | SyncJobState::Running | SyncJobState::Retryable
        ) {
            anyhow::bail!(
                "cannot create attempt for terminal sync job {} in state {}",
                attempt.job_id,
                job_state.as_str()
            );
        }
        if attempt_count >= max_attempts {
            anyhow::bail!(
                "sync job {} has exhausted attempts ({attempt_count}/{max_attempts})",
                attempt.job_id
            );
        }
        let running_attempts: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sync_job_attempts WHERE job_id = ? AND state = 'running'",
                [&attempt.job_id],
                |row| row.get(0),
            )
            .context("failed to count running sync job attempts")?;
        if running_attempts > 0 {
            anyhow::bail!("sync job {} already has a running attempt", attempt.job_id);
        }

        let attempt_number: i64 = self
            .conn
            .query_row(
                "SELECT COALESCE(MAX(attempt_number), 0) + 1 FROM sync_job_attempts WHERE job_id = ?",
                [&attempt.job_id],
                |row| row.get(0),
            )
            .context("failed to compute next sync job attempt number")?;
        let now = lillux::time::iso8601_now();
        self.conn
            .execute(
                "INSERT INTO sync_job_attempts (
                    attempt_id, job_id, attempt_number, worker_id, state, phase,
                    started_at, updated_at, finished_at, error, result_json
                ) VALUES (?, ?, ?, ?, 'running', ?, ?, ?, NULL, NULL, NULL)",
                rusqlite::params![
                    &attempt.attempt_id,
                    &attempt.job_id,
                    attempt_number,
                    &attempt.worker_id,
                    &attempt.phase,
                    &now,
                    &now,
                ],
            )
            .context("failed to create sync job attempt")?;
        self.conn
            .execute(
                "UPDATE sync_jobs SET state = 'running', phase = ?, attempt_count = attempt_count + 1, updated_at = ?, finished_at = NULL WHERE job_id = ?",
                rusqlite::params![&attempt.phase, &now, &attempt.job_id],
            )
            .context("failed to activate sync job attempt")?;

        self.get_sync_job_attempt(&attempt.attempt_id)?
            .ok_or_else(|| {
                anyhow::anyhow!("created sync job attempt {} not found", attempt.attempt_id)
            })
    }

    pub fn finish_sync_job_attempt(
        &self,
        attempt_id: &str,
        finish: &FinishSyncJobAttempt,
    ) -> anyhow::Result<()> {
        self.immediate_transaction("finish sync job attempt", || {
            self.finish_sync_job_attempt_inner(attempt_id, finish)
        })
    }

    fn finish_sync_job_attempt_inner(
        &self,
        attempt_id: &str,
        finish: &FinishSyncJobAttempt,
    ) -> anyhow::Result<()> {
        validate_sync_job_id(attempt_id)?;
        validate_non_empty_label("phase", &finish.phase)?;
        if !finish.state.is_terminal() {
            anyhow::bail!(
                "finish_sync_job_attempt requires terminal state, got {}",
                finish.state.as_str()
            );
        }
        let current_state = self
            .conn
            .query_row(
                "SELECT state FROM sync_job_attempts WHERE attempt_id = ?",
                [attempt_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .context("failed to load sync job attempt state")?
            .ok_or_else(|| anyhow::anyhow!("sync job attempt not found: {attempt_id}"))?;
        let current_state = SyncJobAttemptState::from_str(&current_state)?;
        if current_state.is_terminal() {
            anyhow::bail!(
                "sync job attempt {attempt_id} is already terminal in state {}",
                current_state.as_str()
            );
        }
        let result_json = finish
            .result
            .as_ref()
            .map(serde_json::to_vec)
            .transpose()
            .context("failed to serialize sync job attempt result")?;
        let now = lillux::time::iso8601_now();
        let changed = self
            .conn
            .execute(
                "UPDATE sync_job_attempts SET
                    state = ?, phase = ?, updated_at = ?, finished_at = ?, error = ?, result_json = ?
                 WHERE attempt_id = ?",
                rusqlite::params![
                    finish.state.as_str(),
                    &finish.phase,
                    &now,
                    &now,
                    &finish.error,
                    result_json,
                    attempt_id,
                ],
            )
            .context("failed to finish sync job attempt")?;
        debug_assert_eq!(changed, 1);
        Ok(())
    }

    pub fn finish_sync_job_attempt_and_update_job(
        &self,
        attempt_id: &str,
        finish: &FinishSyncJobAttempt,
        job_id: &str,
        update: &SyncJobUpdate,
    ) -> anyhow::Result<()> {
        self.immediate_transaction("finish sync job attempt and update job", || {
            let attempt_job_id: String = self
                .conn
                .query_row(
                    "SELECT job_id FROM sync_job_attempts WHERE attempt_id = ?",
                    [attempt_id],
                    |row| row.get(0),
                )
                .optional()
                .context("failed to load sync job attempt owner")?
                .ok_or_else(|| anyhow::anyhow!("sync job attempt not found: {attempt_id}"))?;
            if attempt_job_id != job_id {
                anyhow::bail!(
                    "sync job attempt {attempt_id} belongs to job {attempt_job_id}, not {job_id}"
                );
            }
            self.finish_sync_job_attempt_inner(attempt_id, finish)?;
            self.update_sync_job(job_id, update)?;
            Ok(())
        })
    }

    pub fn get_sync_job_attempt(
        &self,
        attempt_id: &str,
    ) -> anyhow::Result<Option<SyncJobAttemptRecord>> {
        validate_sync_job_id(attempt_id)?;
        self.conn
            .query_row(
                "SELECT attempt_id, job_id, attempt_number, worker_id, state, phase,
                    started_at, updated_at, finished_at, error, result_json
                 FROM sync_job_attempts WHERE attempt_id = ?",
                [attempt_id],
                sync_job_attempt_from_row,
            )
            .optional()
            .context("failed to get sync job attempt")
    }

    pub fn list_sync_job_attempts(
        &self,
        job_id: &str,
    ) -> anyhow::Result<Vec<SyncJobAttemptRecord>> {
        validate_sync_job_id(job_id)?;
        let mut stmt = self
            .conn
            .prepare(
                "SELECT attempt_id, job_id, attempt_number, worker_id, state, phase,
                    started_at, updated_at, finished_at, error, result_json
                 FROM sync_job_attempts WHERE job_id = ? ORDER BY attempt_number ASC",
            )
            .context("failed to prepare sync job attempt list query")?;
        let rows = stmt
            .query_map([job_id], sync_job_attempt_from_row)
            .context("failed to query sync job attempts")?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to collect sync job attempts")
    }

    pub fn get_sync_job(&self, job_id: &str) -> anyhow::Result<Option<SyncJobRecord>> {
        validate_sync_job_id(job_id)?;
        self.conn
            .query_row(
                "SELECT job_id, operation_type, peer, state, phase, roots_json, heads_json,
                    uploaded_hashes_json, fetched_hashes_json, attempt_count, max_attempts,
                    last_error, result_json, created_at, updated_at, finished_at
                 FROM sync_jobs WHERE job_id = ?",
                [job_id],
                sync_job_from_row,
            )
            .optional()
            .context("failed to get sync job")
    }

    pub fn list_sync_jobs_by_state(
        &self,
        state: Option<SyncJobState>,
        limit: usize,
    ) -> anyhow::Result<Vec<SyncJobRecord>> {
        let limit = limit.clamp(1, 500);
        let sql = if state.is_some() {
            "SELECT job_id, operation_type, peer, state, phase, roots_json, heads_json,
                uploaded_hashes_json, fetched_hashes_json, attempt_count, max_attempts,
                last_error, result_json, created_at, updated_at, finished_at
             FROM sync_jobs WHERE state = ? ORDER BY created_at DESC, job_id DESC LIMIT ?"
        } else {
            "SELECT job_id, operation_type, peer, state, phase, roots_json, heads_json,
                uploaded_hashes_json, fetched_hashes_json, attempt_count, max_attempts,
                last_error, result_json, created_at, updated_at, finished_at
             FROM sync_jobs ORDER BY created_at DESC, job_id DESC LIMIT ?"
        };
        let mut stmt = self
            .conn
            .prepare(sql)
            .context("failed to prepare sync job list query")?;
        let rows = if let Some(state) = state {
            stmt.query_map(
                rusqlite::params![state.as_str(), limit as i64],
                sync_job_from_row,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?
        } else {
            stmt.query_map(rusqlite::params![limit as i64], sync_job_from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        Ok(rows)
    }

    pub fn count_active_sync_jobs(&self) -> anyhow::Result<u64> {
        let count: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sync_jobs WHERE state IN ('planned', 'running', 'retryable')",
                [],
                |row| row.get(0),
            )
            .context("failed to count active sync jobs")?;
        u64::try_from(count).context("active sync job count was negative")
    }
}

fn validate_sync_job_id(job_id: &str) -> anyhow::Result<()> {
    validate_non_empty_label("job_id", job_id)?;
    if job_id.len() > 128
        || !job_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
    {
        anyhow::bail!("invalid sync job id: {job_id}");
    }
    Ok(())
}

fn validate_canonical_hash(label: &str, hash: &str) -> anyhow::Result<()> {
    if !lillux::valid_hash(hash) || hash.bytes().any(|b| b.is_ascii_uppercase()) {
        anyhow::bail!("invalid {label}: {hash}");
    }
    Ok(())
}

fn cas_entry_transition_allowed(current: CasEntryState, next: CasEntryState) -> bool {
    !matches!(
        (current, next),
        (
            CasEntryState::Local | CasEntryState::Accepted | CasEntryState::Mirrored,
            CasEntryState::Staged | CasEntryState::Rejected
        ) | (CasEntryState::Rejected, CasEntryState::Staged)
    )
}

fn validate_sync_job_transition(from: SyncJobState, to: SyncJobState) -> anyhow::Result<()> {
    use SyncJobState::*;
    let allowed = match from {
        Planned => matches!(to, Planned | Running | Failed | Cancelled),
        Running => matches!(to, Running | Completed | Failed | Retryable | Cancelled),
        Retryable => matches!(to, Retryable | Running | Failed | Cancelled),
        Completed | Failed | Cancelled => false,
    };
    if !allowed {
        anyhow::bail!(
            "invalid sync job state transition: {} -> {}",
            from.as_str(),
            to.as_str()
        );
    }
    Ok(())
}

fn validate_non_empty_label(label: &str, value: &str) -> anyhow::Result<()> {
    if value.is_empty() || value.len() > 256 || value.contains('/') || value.contains("..") {
        anyhow::bail!("invalid {label}: {value}");
    }
    Ok(())
}

fn admission_attestation_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<AdmissionAttestationRecord> {
    let state: String = row.get("state")?;
    Ok(AdmissionAttestationRecord {
        attestation_hash: row.get("attestation_hash")?,
        subject_hash: row.get("subject_hash")?,
        policy: row.get("policy")?,
        claim: row.get("claim")?,
        issuer: row.get("issuer")?,
        issued_at: row.get("issued_at")?,
        expires_at: row.get("expires_at")?,
        head_ref_path: row.get("head_ref_path")?,
        indexed_at: row.get("indexed_at")?,
        state: AdmissionAttestationState::from_str(&state)
            .map_err(|_| rusqlite::Error::InvalidQuery)?,
    })
}

fn sync_job_attempt_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SyncJobAttemptRecord> {
    let state: String = row.get("state")?;
    let attempt_number: i64 = row.get("attempt_number")?;
    let result_json: Option<Vec<u8>> = row.get("result_json")?;
    Ok(SyncJobAttemptRecord {
        attempt_id: row.get("attempt_id")?,
        job_id: row.get("job_id")?,
        attempt_number: u64::try_from(attempt_number).map_err(|_| rusqlite::Error::InvalidQuery)?,
        worker_id: row.get("worker_id")?,
        state: SyncJobAttemptState::from_str(&state).map_err(|_| rusqlite::Error::InvalidQuery)?,
        phase: row.get("phase")?,
        started_at: row.get("started_at")?,
        updated_at: row.get("updated_at")?,
        finished_at: row.get("finished_at")?,
        error: row.get("error")?,
        result: result_json
            .map(|bytes| serde_json::from_slice(&bytes).map_err(|_| rusqlite::Error::InvalidQuery))
            .transpose()?,
    })
}

fn sync_job_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SyncJobRecord> {
    let state: String = row.get("state")?;
    let roots_json: Vec<u8> = row.get("roots_json")?;
    let heads_json: Vec<u8> = row.get("heads_json")?;
    let uploaded_json: Vec<u8> = row.get("uploaded_hashes_json")?;
    let fetched_json: Vec<u8> = row.get("fetched_hashes_json")?;
    let result_json: Option<Vec<u8>> = row.get("result_json")?;
    let attempt_count: i64 = row.get("attempt_count")?;
    let max_attempts: i64 = row.get("max_attempts")?;
    Ok(SyncJobRecord {
        job_id: row.get("job_id")?,
        operation_type: row.get("operation_type")?,
        peer: row.get("peer")?,
        state: SyncJobState::from_str(&state).map_err(|_| rusqlite::Error::InvalidQuery)?,
        phase: row.get("phase")?,
        roots: serde_json::from_slice(&roots_json).map_err(|_| rusqlite::Error::InvalidQuery)?,
        heads: serde_json::from_slice(&heads_json).map_err(|_| rusqlite::Error::InvalidQuery)?,
        uploaded_hashes: serde_json::from_slice(&uploaded_json)
            .map_err(|_| rusqlite::Error::InvalidQuery)?,
        fetched_hashes: serde_json::from_slice(&fetched_json)
            .map_err(|_| rusqlite::Error::InvalidQuery)?,
        attempt_count: u64::try_from(attempt_count).map_err(|_| rusqlite::Error::InvalidQuery)?,
        max_attempts: u64::try_from(max_attempts).map_err(|_| rusqlite::Error::InvalidQuery)?,
        last_error: row.get("last_error")?,
        result: result_json
            .map(|bytes| serde_json::from_slice(&bytes).map_err(|_| rusqlite::Error::InvalidQuery))
            .transpose()?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
        finished_at: row.get("finished_at")?,
    })
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

        let first = db
            .get_cas_entry(CasEntryKind::Object, &hash)
            .unwrap()
            .unwrap();
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

        let updated = db
            .get_cas_entry(CasEntryKind::Object, &hash)
            .unwrap()
            .unwrap();
        assert_eq!(updated.first_seen_at, first.first_seen_at);
        assert_eq!(updated.bytes, 256);
        assert_eq!(updated.state, CasEntryState::Accepted);
        assert_eq!(updated.source_principal.as_deref(), Some("fp:source"));
        assert_eq!(updated.source_peer.as_deref(), Some("peer-a"));
        assert_eq!(updated.job_id.as_deref(), Some("job-a"));

        db.record_cas_entry(&NewCasEntryAttribution {
            hash: hash.clone(),
            entry_kind: CasEntryKind::Object,
            bytes: 512,
            source_principal: Some("fp:untrusted".to_string()),
            source_peer: Some("peer-untrusted".to_string()),
            job_id: Some("job-untrusted".to_string()),
            state: CasEntryState::Staged,
        })
        .unwrap();

        let still_accepted = db
            .get_cas_entry(CasEntryKind::Object, &hash)
            .unwrap()
            .unwrap();
        assert_eq!(still_accepted.bytes, 256);
        assert_eq!(still_accepted.state, CasEntryState::Accepted);

        db.record_cas_entry(&NewCasEntryAttribution {
            hash: hash.clone(),
            entry_kind: CasEntryKind::Object,
            bytes: 1024,
            source_principal: Some("fp:rejected".to_string()),
            source_peer: Some("peer-rejected".to_string()),
            job_id: Some("job-rejected".to_string()),
            state: CasEntryState::Rejected,
        })
        .unwrap();

        let not_downgraded = db
            .get_cas_entry(CasEntryKind::Object, &hash)
            .unwrap()
            .unwrap();
        assert_eq!(not_downgraded.bytes, 256);
        assert_eq!(not_downgraded.state, CasEntryState::Accepted);

        let rejected_hash = "ac".repeat(32);
        db.record_cas_entry(&NewCasEntryAttribution {
            hash: rejected_hash.clone(),
            entry_kind: CasEntryKind::Object,
            bytes: 10,
            source_principal: None,
            source_peer: None,
            job_id: None,
            state: CasEntryState::Staged,
        })
        .unwrap();
        db.record_cas_entry(&NewCasEntryAttribution {
            hash: rejected_hash.clone(),
            entry_kind: CasEntryKind::Object,
            bytes: 20,
            source_principal: None,
            source_peer: None,
            job_id: None,
            state: CasEntryState::Rejected,
        })
        .unwrap();
        db.record_cas_entry(&NewCasEntryAttribution {
            hash: rejected_hash.clone(),
            entry_kind: CasEntryKind::Object,
            bytes: 30,
            source_principal: None,
            source_peer: None,
            job_id: None,
            state: CasEntryState::Staged,
        })
        .unwrap();
        let stays_rejected = db
            .get_cas_entry(CasEntryKind::Object, &rejected_hash)
            .unwrap()
            .unwrap();
        assert_eq!(stays_rejected.bytes, 20);
        assert_eq!(stays_rejected.state, CasEntryState::Rejected);
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
        db.set_cas_entry_state(CasEntryKind::Blob, &staged_hash, CasEntryState::Accepted)
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
    fn cas_entry_lookup_is_keyed_by_kind_and_hash() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("projection.db");
        let db = ProjectionDb::open(&path).unwrap();
        let hash = "99".repeat(32);

        db.record_cas_entry(&NewCasEntryAttribution {
            hash: hash.clone(),
            entry_kind: CasEntryKind::Object,
            bytes: 10,
            source_principal: None,
            source_peer: None,
            job_id: None,
            state: CasEntryState::Local,
        })
        .unwrap();
        db.record_cas_entry(&NewCasEntryAttribution {
            hash: hash.clone(),
            entry_kind: CasEntryKind::Blob,
            bytes: 20,
            source_principal: None,
            source_peer: None,
            job_id: None,
            state: CasEntryState::Staged,
        })
        .unwrap();

        db.set_cas_entry_state(CasEntryKind::Blob, &hash, CasEntryState::Accepted)
            .unwrap();

        let object = db
            .get_cas_entry(CasEntryKind::Object, &hash)
            .unwrap()
            .unwrap();
        let blob = db
            .get_cas_entry(CasEntryKind::Blob, &hash)
            .unwrap()
            .unwrap();
        assert_eq!(object.bytes, 10);
        assert_eq!(object.state, CasEntryState::Local);
        assert_eq!(blob.bytes, 20);
        assert_eq!(blob.state, CasEntryState::Accepted);
    }

    #[test]
    fn admission_attestation_index_is_queryable_by_subject_and_policy() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("projection.db");
        let db = ProjectionDb::open(&path).unwrap();
        let subject = "11".repeat(32);
        let attestation = "22".repeat(32);

        db.record_admission_attestation(&NewAdmissionAttestationRecord {
            attestation_hash: attestation.clone(),
            subject_hash: subject.clone(),
            policy: "local-node-v1".to_string(),
            claim: "accepted".to_string(),
            issuer: "fp:issuer".to_string(),
            issued_at: "2026-05-30T00:00:00Z".to_string(),
            expires_at: None,
            head_ref_path: Some(format!("admissions/local-node-v1/{subject}/head")),
            state: AdmissionAttestationState::Accepted,
        })
        .unwrap();

        let by_subject = db
            .list_admission_attestations_for_subject(&subject, None)
            .unwrap();
        assert_eq!(by_subject.len(), 1);
        assert_eq!(by_subject[0].attestation_hash, attestation);
        assert_eq!(by_subject[0].policy, "local-node-v1");
        assert_eq!(by_subject[0].state, AdmissionAttestationState::Accepted);

        let by_policy = db
            .list_admission_attestations_for_subject(&subject, Some("local-node-v1"))
            .unwrap();
        assert_eq!(by_policy.len(), 1);

        db.record_admission_attestation(&NewAdmissionAttestationRecord {
            attestation_hash: "33".repeat(32),
            subject_hash: subject.clone(),
            policy: "local-node-v1".to_string(),
            claim: "accepted".to_string(),
            issuer: "fp:other-issuer".to_string(),
            issued_at: "2026-05-30T00:01:00Z".to_string(),
            expires_at: None,
            head_ref_path: Some(format!("admissions/local-node-v1/{subject}/head")),
            state: AdmissionAttestationState::Accepted,
        })
        .unwrap();
        let multi_issuer = db
            .list_admission_attestations_for_subject(&subject, Some("local-node-v1"))
            .unwrap();
        assert_eq!(multi_issuer.len(), 2);

        let other_policy = db
            .list_admission_attestations_for_subject(&subject, Some("other-policy"))
            .unwrap();
        assert!(other_policy.is_empty());
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

        let err = db
            .record_cas_entry(&NewCasEntryAttribution {
                hash: "AB".repeat(32),
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
    fn sync_job_lifecycle_is_persisted() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("projection.db");
        let db = ProjectionDb::open(&path).unwrap();
        let root_hash = "11".repeat(32);
        let head_hash = "22".repeat(32);
        let uploaded_hash = "33".repeat(32);
        let fetched_hash = "44".repeat(32);

        let created = db
            .create_sync_job(&NewSyncJob {
                job_id: "job:alpha".to_string(),
                operation_type: "mirror_pull".to_string(),
                peer: Some("node-a".to_string()),
                roots: vec![root_hash.clone()],
                heads: vec![head_hash.clone()],
                max_attempts: 3,
            })
            .unwrap();

        assert_eq!(created.job_id, "job:alpha");
        assert_eq!(created.operation_type, "mirror_pull");
        assert_eq!(created.peer.as_deref(), Some("node-a"));
        assert_eq!(created.state, SyncJobState::Planned);
        assert_eq!(created.phase, "planned");
        assert_eq!(created.roots, vec![root_hash]);
        assert_eq!(created.heads, vec![head_hash]);
        assert_eq!(created.attempt_count, 0);
        assert_eq!(created.max_attempts, 3);
        assert!(created.finished_at.is_none());

        db.update_sync_job(
            "job:alpha",
            &SyncJobUpdate {
                state: SyncJobState::Running,
                phase: "fetching_closure".to_string(),
                roots: None,
                heads: None,
                uploaded_hashes: vec![uploaded_hash.clone()],
                fetched_hashes: vec![fetched_hash.clone()],
                last_error: None,
                result: None,
            },
        )
        .unwrap();

        let running = db.get_sync_job("job:alpha").unwrap().unwrap();
        assert_eq!(running.state, SyncJobState::Running);
        assert_eq!(running.phase, "fetching_closure");
        assert_eq!(running.uploaded_hashes, vec![uploaded_hash]);
        assert_eq!(running.fetched_hashes, vec![fetched_hash]);
        assert_eq!(running.attempt_count, 0);
        assert!(running.finished_at.is_none());

        db.update_sync_job(
            "job:alpha",
            &SyncJobUpdate {
                state: SyncJobState::Completed,
                phase: "done".to_string(),
                roots: None,
                heads: None,
                uploaded_hashes: running.uploaded_hashes,
                fetched_hashes: running.fetched_hashes,
                last_error: None,
                result: Some(serde_json::json!({"accepted": true})),
            },
        )
        .unwrap();

        let completed = db.get_sync_job("job:alpha").unwrap().unwrap();
        assert_eq!(completed.state, SyncJobState::Completed);
        assert_eq!(completed.phase, "done");
        assert_eq!(completed.attempt_count, 0);
        assert_eq!(
            completed.result,
            Some(serde_json::json!({"accepted": true}))
        );
        assert!(completed.finished_at.is_some());

        let completed_jobs = db
            .list_sync_jobs_by_state(Some(SyncJobState::Completed), 10)
            .unwrap();
        assert_eq!(completed_jobs.len(), 1);
        assert_eq!(completed_jobs[0].job_id, "job:alpha");
        assert_eq!(db.count_active_sync_jobs().unwrap(), 0);
    }

    #[test]
    fn sync_job_attempt_lifecycle_is_persisted() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("projection.db");
        let db = ProjectionDb::open(&path).unwrap();

        db.create_sync_job(&NewSyncJob {
            job_id: "job:attempts".to_string(),
            operation_type: "remote_execute".to_string(),
            peer: Some("node-a".to_string()),
            roots: vec![],
            heads: vec![],
            max_attempts: 2,
        })
        .unwrap();

        let first = db
            .create_sync_job_attempt(&NewSyncJobAttempt {
                attempt_id: "attempt:one".to_string(),
                job_id: "job:attempts".to_string(),
                worker_id: Some("worker-a".to_string()),
                phase: "pushing".to_string(),
            })
            .unwrap();
        assert_eq!(first.attempt_number, 1);
        assert_eq!(first.state, SyncJobAttemptState::Running);
        assert_eq!(first.phase, "pushing");
        assert_eq!(first.worker_id.as_deref(), Some("worker-a"));
        assert!(first.finished_at.is_none());
        assert_eq!(
            db.get_sync_job("job:attempts")
                .unwrap()
                .unwrap()
                .attempt_count,
            1
        );

        let err = db
            .create_sync_job_attempt(&NewSyncJobAttempt {
                attempt_id: "attempt:concurrent".to_string(),
                job_id: "job:attempts".to_string(),
                worker_id: None,
                phase: "pushing".to_string(),
            })
            .unwrap_err();
        assert!(err.to_string().contains("already has a running attempt"));

        db.finish_sync_job_attempt(
            "attempt:one",
            &FinishSyncJobAttempt {
                state: SyncJobAttemptState::Failed,
                phase: "push_failed".to_string(),
                error: Some("network".to_string()),
                result: Some(serde_json::json!({"retryable": true})),
            },
        )
        .unwrap();
        let finished = db.get_sync_job_attempt("attempt:one").unwrap().unwrap();
        assert_eq!(finished.state, SyncJobAttemptState::Failed);
        assert_eq!(finished.phase, "push_failed");
        assert_eq!(finished.error.as_deref(), Some("network"));
        assert_eq!(
            finished.result,
            Some(serde_json::json!({"retryable": true}))
        );
        assert!(finished.finished_at.is_some());

        let err = db
            .finish_sync_job_attempt(
                "attempt:one",
                &FinishSyncJobAttempt {
                    state: SyncJobAttemptState::Completed,
                    phase: "done".to_string(),
                    error: None,
                    result: None,
                },
            )
            .unwrap_err();
        assert!(err.to_string().contains("already terminal"));

        let second = db
            .create_sync_job_attempt(&NewSyncJobAttempt {
                attempt_id: "attempt:two".to_string(),
                job_id: "job:attempts".to_string(),
                worker_id: Some("worker-b".to_string()),
                phase: "retrying".to_string(),
            })
            .unwrap();
        assert_eq!(second.attempt_number, 2);
        let err = db
            .update_sync_job(
                "job:attempts",
                &SyncJobUpdate {
                    state: SyncJobState::Completed,
                    phase: "done".to_string(),
                    roots: None,
                    heads: None,
                    uploaded_hashes: vec![],
                    fetched_hashes: vec![],
                    last_error: None,
                    result: None,
                },
            )
            .unwrap_err();
        assert!(err.to_string().contains("attempt(s) are still running"));
        assert_eq!(
            db.list_sync_job_attempts("job:attempts")
                .unwrap()
                .into_iter()
                .map(|attempt| attempt.attempt_id)
                .collect::<Vec<_>>(),
            vec!["attempt:one".to_string(), "attempt:two".to_string()]
        );
    }

    #[test]
    fn sync_job_attempt_completion_must_match_parent_job() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("projection.db");
        let db = ProjectionDb::open(&path).unwrap();

        for job_id in ["job:attempt-owner-a", "job:attempt-owner-b"] {
            db.create_sync_job(&NewSyncJob {
                job_id: job_id.to_string(),
                operation_type: "remote_execute".to_string(),
                peer: None,
                roots: vec![],
                heads: vec![],
                max_attempts: 1,
            })
            .unwrap();
        }
        db.create_sync_job_attempt(&NewSyncJobAttempt {
            attempt_id: "attempt:owner".to_string(),
            job_id: "job:attempt-owner-a".to_string(),
            worker_id: None,
            phase: "running".to_string(),
        })
        .unwrap();

        let err = db
            .finish_sync_job_attempt_and_update_job(
                "attempt:owner",
                &FinishSyncJobAttempt {
                    state: SyncJobAttemptState::Completed,
                    phase: "done".to_string(),
                    error: None,
                    result: None,
                },
                "job:attempt-owner-b",
                &SyncJobUpdate {
                    state: SyncJobState::Completed,
                    phase: "done".to_string(),
                    roots: None,
                    heads: None,
                    uploaded_hashes: vec![],
                    fetched_hashes: vec![],
                    last_error: None,
                    result: None,
                },
            )
            .unwrap_err();
        assert!(err.to_string().contains("belongs to job"));

        let attempt = db.get_sync_job_attempt("attempt:owner").unwrap().unwrap();
        assert_eq!(attempt.state, SyncJobAttemptState::Running);
        let wrong_job = db.get_sync_job("job:attempt-owner-b").unwrap().unwrap();
        assert_eq!(wrong_job.state, SyncJobState::Planned);
    }

    #[test]
    fn sync_job_attempts_respect_parent_limits_and_state() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("projection.db");
        let db = ProjectionDb::open(&path).unwrap();

        db.create_sync_job(&NewSyncJob {
            job_id: "job:limited".to_string(),
            operation_type: "remote_execute".to_string(),
            peer: None,
            roots: vec![],
            heads: vec![],
            max_attempts: 1,
        })
        .unwrap();
        db.create_sync_job_attempt(&NewSyncJobAttempt {
            attempt_id: "attempt:limited:one".to_string(),
            job_id: "job:limited".to_string(),
            worker_id: None,
            phase: "running".to_string(),
        })
        .unwrap();
        db.finish_sync_job_attempt(
            "attempt:limited:one",
            &FinishSyncJobAttempt {
                state: SyncJobAttemptState::Failed,
                phase: "failed".to_string(),
                error: None,
                result: None,
            },
        )
        .unwrap();
        let err = db
            .create_sync_job_attempt(&NewSyncJobAttempt {
                attempt_id: "attempt:limited:two".to_string(),
                job_id: "job:limited".to_string(),
                worker_id: None,
                phase: "retrying".to_string(),
            })
            .unwrap_err();
        assert!(err.to_string().contains("has exhausted attempts"));

        db.create_sync_job(&NewSyncJob {
            job_id: "job:terminal".to_string(),
            operation_type: "remote_execute".to_string(),
            peer: None,
            roots: vec![],
            heads: vec![],
            max_attempts: 1,
        })
        .unwrap();
        db.update_sync_job(
            "job:terminal",
            &SyncJobUpdate {
                state: SyncJobState::Running,
                phase: "running".to_string(),
                roots: None,
                heads: None,
                uploaded_hashes: vec![],
                fetched_hashes: vec![],
                last_error: None,
                result: None,
            },
        )
        .unwrap();
        db.update_sync_job(
            "job:terminal",
            &SyncJobUpdate {
                state: SyncJobState::Completed,
                phase: "done".to_string(),
                roots: None,
                heads: None,
                uploaded_hashes: vec![],
                fetched_hashes: vec![],
                last_error: None,
                result: None,
            },
        )
        .unwrap();
        let err = db
            .create_sync_job_attempt(&NewSyncJobAttempt {
                attempt_id: "attempt:terminal".to_string(),
                job_id: "job:terminal".to_string(),
                worker_id: None,
                phase: "too_late".to_string(),
            })
            .unwrap_err();
        assert!(err.to_string().contains("cannot create attempt"));
    }

    #[test]
    fn active_sync_job_count_only_includes_non_terminal_jobs() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("projection.db");
        let db = ProjectionDb::open(&path).unwrap();

        db.create_sync_job(&NewSyncJob {
            job_id: "job-running".to_string(),
            operation_type: "mirror_pull".to_string(),
            peer: None,
            roots: vec![],
            heads: vec![],
            max_attempts: 3,
        })
        .unwrap();
        db.create_sync_job(&NewSyncJob {
            job_id: "job-completed".to_string(),
            operation_type: "mirror_pull".to_string(),
            peer: None,
            roots: vec![],
            heads: vec![],
            max_attempts: 3,
        })
        .unwrap();
        db.update_sync_job(
            "job-completed",
            &SyncJobUpdate {
                state: SyncJobState::Running,
                phase: "running".to_string(),
                roots: None,
                heads: None,
                uploaded_hashes: vec![],
                fetched_hashes: vec![],
                last_error: None,
                result: None,
            },
        )
        .unwrap();
        db.update_sync_job(
            "job-completed",
            &SyncJobUpdate {
                state: SyncJobState::Completed,
                phase: "done".to_string(),
                roots: None,
                heads: None,
                uploaded_hashes: vec![],
                fetched_hashes: vec![],
                last_error: None,
                result: None,
            },
        )
        .unwrap();

        assert_eq!(db.count_active_sync_jobs().unwrap(), 1);
    }

    #[test]
    fn sync_job_rejects_illegal_and_terminal_transitions() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("projection.db");
        let db = ProjectionDb::open(&path).unwrap();

        db.create_sync_job(&NewSyncJob {
            job_id: "job-transition".to_string(),
            operation_type: "mirror_pull".to_string(),
            peer: None,
            roots: vec![],
            heads: vec![],
            max_attempts: 3,
        })
        .unwrap();

        let err = db
            .update_sync_job(
                "job-transition",
                &SyncJobUpdate {
                    state: SyncJobState::Completed,
                    phase: "done".to_string(),
                    roots: None,
                    heads: None,
                    uploaded_hashes: vec![],
                    fetched_hashes: vec![],
                    last_error: None,
                    result: None,
                },
            )
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("invalid sync job state transition"));

        db.update_sync_job(
            "job-transition",
            &SyncJobUpdate {
                state: SyncJobState::Running,
                phase: "running".to_string(),
                roots: None,
                heads: None,
                uploaded_hashes: vec![],
                fetched_hashes: vec![],
                last_error: None,
                result: None,
            },
        )
        .unwrap();
        db.update_sync_job(
            "job-transition",
            &SyncJobUpdate {
                state: SyncJobState::Failed,
                phase: "failed".to_string(),
                roots: None,
                heads: None,
                uploaded_hashes: vec![],
                fetched_hashes: vec![],
                last_error: Some("boom".to_string()),
                result: None,
            },
        )
        .unwrap();

        let err = db
            .update_sync_job(
                "job-transition",
                &SyncJobUpdate {
                    state: SyncJobState::Running,
                    phase: "reactivated".to_string(),
                    roots: None,
                    heads: None,
                    uploaded_hashes: vec![],
                    fetched_hashes: vec![],
                    last_error: None,
                    result: None,
                },
            )
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("invalid sync job state transition"));
    }

    #[test]
    fn sync_job_rejects_invalid_hashes() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("projection.db");
        let db = ProjectionDb::open(&path).unwrap();

        let err = db
            .create_sync_job(&NewSyncJob {
                job_id: "job-invalid".to_string(),
                operation_type: "mirror_pull".to_string(),
                peer: None,
                roots: vec!["not-a-hash".to_string()],
                heads: vec![],
                max_attempts: 1,
            })
            .unwrap_err();

        assert!(err.to_string().contains("invalid sync job root/head hash"));

        let err = db
            .create_sync_job(&NewSyncJob {
                job_id: "job-uppercase".to_string(),
                operation_type: "mirror_pull".to_string(),
                peer: None,
                roots: vec!["AA".repeat(32)],
                heads: vec![],
                max_attempts: 1,
            })
            .unwrap_err();

        assert!(err.to_string().contains("invalid sync job root/head hash"));
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
