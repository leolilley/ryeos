use crate::sqlite_schema;

// ============= Schema =============

pub(super) const SCHEMA_SQL: &str = r#"
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
    event_hash TEXT NOT NULL,
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

CREATE UNIQUE INDEX IF NOT EXISTS idx_events_event_hash ON events(event_hash);
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
    outcome_code TEXT,
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

-- Reverse lookup for cohort/fleet queries: "the threads where key=value"
-- (e.g. fleet=<run id>). Without this a fleet filter scans every facet row.
CREATE INDEX IF NOT EXISTS idx_facets_key_value ON thread_facets(key, value);

-- Latest cumulative usage per thread. Raw thread_usage events are cumulative,
-- so summary queries must read this latest-per-thread projection instead of
-- summing the events table directly.
CREATE TABLE IF NOT EXISTS thread_usage_latest (
    thread_id TEXT PRIMARY KEY,
    chain_root_id TEXT NOT NULL,
    chain_seq INTEGER NOT NULL,
    thread_seq INTEGER NOT NULL,

    completed_turns INTEGER NOT NULL,
    input_tokens INTEGER NOT NULL,
    output_tokens INTEGER NOT NULL,
    spend_usd REAL NOT NULL,
    spawns_used INTEGER NOT NULL,

    started_at TEXT NOT NULL,
    settled_at TEXT NOT NULL,
    last_settled_turn_seq INTEGER NOT NULL,
    elapsed_ms INTEGER NOT NULL,

    provider_id TEXT,
    model TEXT,
    profile TEXT
);

CREATE INDEX IF NOT EXISTS idx_thread_usage_latest_chain
    ON thread_usage_latest(chain_root_id);
CREATE INDEX IF NOT EXISTS idx_thread_usage_latest_settled_at
    ON thread_usage_latest(settled_at);
CREATE INDEX IF NOT EXISTS idx_thread_usage_latest_model
    ON thread_usage_latest(provider_id, model);

-- App-level usage attribution asserted by an authorized RyeOS principal at
-- root launch time. Keyed by chain root so child/continuation usage can join
-- back to the root app subject.
CREATE TABLE IF NOT EXISTS thread_usage_subjects (
    chain_root_id TEXT PRIMARY KEY,
    namespace TEXT NOT NULL,
    subject TEXT NOT NULL,
    asserted_by TEXT,
    created_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_thread_usage_subjects_subject
    ON thread_usage_subjects(namespace, subject);

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


/// Application ID stamp for projection.db.
/// RYPJ = 0x5259504a ("RY" + "PJ" for "projection").
pub(super) const PROJECTION_APP_ID: i32 = 0x5259_504a;

/// Manual projection DERIVATION version. Bump this ONLY when the derivation
/// logic changes — when `project_event` / `project_thread_snapshot` produce
/// DIFFERENT rows for the SAME CAS input (e.g. changing how an already-emitted
/// event type projects), so previously-derived rows are stale and must be
/// rebuilt.
///
/// You do NOT bump this for a schema change (a new table/column/index) — those
/// are detected automatically by the spec fingerprint in
/// [`projection_schema_epoch`]. Adding derivation for a brand-new event type
/// also needs no bump (there are no past events of that type to re-derive).
pub(super) const PROJECTION_DERIVATION_VERSION: u64 = 1;

/// The projection schema epoch, stored in SQLite's `PRAGMA user_version`.
///
/// DERIVED, not hand-maintained: a fingerprint of the schema spec folded with
/// the manual derivation version. Any schema change (add/drop/rename a
/// table/column/index) changes the fingerprint and auto-triggers the
/// reset-and-rebuild-from-CAS on the next open — a schema change can never be
/// silently forgotten (the old failure mode was: the spec expects an index the
/// DB lacks, `assert_owned` fails, the daemon won't start). `assert_owned` stays
/// the hard backstop on a freshly-built DB.
pub(super) fn projection_schema_epoch() -> i32 {
    let fingerprint = schema_spec_fingerprint(&projection_schema_spec());
    let combined = fingerprint ^ PROJECTION_DERIVATION_VERSION.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    // Fold the 64-bit value into the i32 `user_version` slot.
    (combined ^ (combined >> 32)) as i32
}

/// Deterministic, order-independent fingerprint of a schema spec — its tables
/// (with columns) and indexes. Canonicalized (sorted) so cosmetic reordering in
/// the spec does not churn the epoch. FNV-1a keeps it dependency-free and stable
/// across builds (a std hasher is not).
pub(super) fn schema_spec_fingerprint(spec: &sqlite_schema::SchemaSpec) -> u64 {
    let mut parts: Vec<String> = vec![format!("app={}", spec.application_id)];
    let mut tables: Vec<String> = spec
        .tables
        .iter()
        .map(|t| {
            let mut cols: Vec<String> = t
                .columns
                .iter()
                .map(|c| format!("{}:{}:{}:{}", c.name, c.col_type, c.pk, c.not_null))
                .collect();
            cols.sort();
            format!("T:{}[{}]", t.name, cols.join(","))
        })
        .collect();
    tables.sort();
    let mut indexes: Vec<String> = spec
        .indexes
        .iter()
        .map(|i| {
            format!(
                "I:{}:{}:{}:{}",
                i.name,
                i.table,
                i.columns.join(","),
                i.unique
            )
        })
        .collect();
    indexes.sort();
    parts.extend(tables);
    parts.extend(indexes);
    fnv1a_64(parts.join(";").as_bytes())
}

fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Schema spec for projection.db — the single source of truth for
/// what tables/columns/indexes this database must contain.
pub(super) fn projection_schema_spec() -> sqlite_schema::SchemaSpec {
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
                        name: "event_hash",
                        col_type: "TEXT",
                        pk: false,
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
                        name: "outcome_code",
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
                name: "thread_usage_latest",
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
                        name: "chain_seq",
                        col_type: "INTEGER",
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
                        name: "completed_turns",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "input_tokens",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "output_tokens",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "spend_usd",
                        col_type: "REAL",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "spawns_used",
                        col_type: "INTEGER",
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
                        name: "settled_at",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "last_settled_turn_seq",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "elapsed_ms",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "provider_id",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "model",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "profile",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                ],
            },
            sqlite_schema::TableSpec {
                name: "thread_usage_subjects",
                columns: &[
                    sqlite_schema::ColumnSpec {
                        name: "chain_root_id",
                        col_type: "TEXT",
                        pk: true,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "namespace",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "subject",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "asserted_by",
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
                name: "idx_events_event_hash",
                table: "events",
                columns: &["event_hash"],
                unique: true,
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
                name: "idx_facets_key_value",
                table: "thread_facets",
                columns: &["key", "value"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_thread_usage_latest_chain",
                table: "thread_usage_latest",
                columns: &["chain_root_id"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_thread_usage_latest_settled_at",
                table: "thread_usage_latest",
                columns: &["settled_at"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_thread_usage_latest_model",
                table: "thread_usage_latest",
                columns: &["provider_id", "model"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_thread_usage_subjects_subject",
                table: "thread_usage_subjects",
                columns: &["namespace", "subject"],
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
