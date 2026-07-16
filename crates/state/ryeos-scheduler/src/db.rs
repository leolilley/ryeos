//! Scheduler SQLite database — `scheduler.sqlite3`.
//!
//! Separate from the selected thread-projection instance (which has strict
//! schema validation and its own recovery-generation pointer).
//! Own `application_id`, own schema, own rebuild path.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};

use super::planning;
use super::types::{
    validate_schedule_spec_record, FireRecord, ScheduleCursorRecord, ScheduleSpecRecord,
};

// ── Schema ──────────────────────────────────────────────────────────

const SCHEMA_SQL: &str = r#"
PRAGMA journal_mode=WAL;
PRAGMA foreign_keys=ON;

CREATE TABLE IF NOT EXISTS schedule_specs (
    schedule_id          TEXT PRIMARY KEY,
    item_ref             TEXT NOT NULL,
    ref_bindings         TEXT NOT NULL,
    params               TEXT NOT NULL,
    schedule_type        TEXT NOT NULL,
    expression           TEXT NOT NULL,
    timezone             TEXT NOT NULL,
    misfire_policy       TEXT NOT NULL,
    overlap_policy       TEXT NOT NULL,
    enabled              INTEGER NOT NULL,
    project_root         TEXT,
    signer_fingerprint   TEXT NOT NULL,
    spec_hash            TEXT NOT NULL,
    registered_at        INTEGER NOT NULL,
    requester_fingerprint TEXT NOT NULL,
    capabilities          TEXT NOT NULL,
    lateness_grace_secs   INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS schedule_fires (
    fire_id            TEXT PRIMARY KEY,
    schedule_id        TEXT NOT NULL,
    scheduled_at       INTEGER NOT NULL,
    fired_at           INTEGER,
    completed_at       INTEGER,
    thread_id          TEXT,
    status             TEXT NOT NULL,
    trigger_reason     TEXT NOT NULL,
    outcome            TEXT,
    signer_fingerprint TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS schedule_cursors (
    schedule_id        TEXT PRIMARY KEY,
    spec_hash          TEXT NOT NULL,
    last_scheduled_at  INTEGER,
    next_fire_at       INTEGER,
    last_evaluated_at  INTEGER,
    updated_at         INTEGER NOT NULL
);

-- This is a validity marker for the rebuildable fire projection, not a
-- history cursor. A rebuild clears it before touching schedule_fires and sets
-- it only after every retained JSONL journal has been replayed successfully.
CREATE TABLE IF NOT EXISTS scheduler_projection_state (
    projection_name    TEXT PRIMARY KEY,
    rebuild_complete   INTEGER NOT NULL CHECK (rebuild_complete IN (0, 1))
);

INSERT OR IGNORE INTO scheduler_projection_state
    (projection_name, rebuild_complete) VALUES ('fires', 0);

-- Transactional bridge from the durable SQLite scheduler state to the
-- append-only fire journal. Every ordinary schedule_fires mutation inserts a
-- complete post-mutation snapshot here in the same transaction. A gated
-- drainer syncs that snapshot to fires.jsonl before acknowledging this row.
CREATE TABLE IF NOT EXISTS schedule_fire_outbox (
    sequence          INTEGER PRIMARY KEY,
    fire_id           TEXT NOT NULL,
    schedule_id       TEXT NOT NULL,
    snapshot_json     TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_fires_schedule_id
    ON schedule_fires(schedule_id);
CREATE INDEX IF NOT EXISTS idx_fires_status
    ON schedule_fires(status);
CREATE INDEX IF NOT EXISTS idx_fires_schedule_scheduled
    ON schedule_fires(schedule_id, scheduled_at DESC);
"#;

/// RYSC = 0x5259_5343 ("RY" + "SC" for scheduler)
const SCHEDULER_APP_ID: i32 = 0x5259_5343;

use ryeos_state::sqlite_schema;

fn scheduler_schema_spec() -> sqlite_schema::SchemaSpec {
    sqlite_schema::SchemaSpec {
        application_id: SCHEDULER_APP_ID,
        tables: &[
            sqlite_schema::TableSpec {
                name: "schedule_specs",
                columns: &[
                    sqlite_schema::ColumnSpec {
                        name: "schedule_id",
                        col_type: "TEXT",
                        pk: true,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "item_ref",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "ref_bindings",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "params",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "schedule_type",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "expression",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "timezone",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "misfire_policy",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "overlap_policy",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "enabled",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "project_root",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "signer_fingerprint",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "spec_hash",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "registered_at",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "requester_fingerprint",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "capabilities",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "lateness_grace_secs",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                ],
            },
            sqlite_schema::TableSpec {
                name: "schedule_fires",
                columns: &[
                    sqlite_schema::ColumnSpec {
                        name: "fire_id",
                        col_type: "TEXT",
                        pk: true,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "schedule_id",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "scheduled_at",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "fired_at",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "completed_at",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "thread_id",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "status",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "trigger_reason",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "outcome",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "signer_fingerprint",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                ],
            },
            sqlite_schema::TableSpec {
                name: "schedule_cursors",
                columns: &[
                    sqlite_schema::ColumnSpec {
                        name: "schedule_id",
                        col_type: "TEXT",
                        pk: true,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "spec_hash",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "last_scheduled_at",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "next_fire_at",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "last_evaluated_at",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "updated_at",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                ],
            },
            sqlite_schema::TableSpec {
                name: "scheduler_projection_state",
                columns: &[
                    sqlite_schema::ColumnSpec {
                        name: "projection_name",
                        col_type: "TEXT",
                        pk: true,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "rebuild_complete",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                ],
            },
            sqlite_schema::TableSpec {
                name: "schedule_fire_outbox",
                columns: &[
                    sqlite_schema::ColumnSpec {
                        name: "sequence",
                        col_type: "INTEGER",
                        pk: true,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "fire_id",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "schedule_id",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "snapshot_json",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                ],
            },
        ],
        indexes: &[
            sqlite_schema::IndexSpec {
                name: "idx_fires_schedule_id",
                table: "schedule_fires",
                columns: &["schedule_id"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_fires_status",
                table: "schedule_fires",
                columns: &["status"],
                unique: false,
            },
            sqlite_schema::IndexSpec {
                name: "idx_fires_schedule_scheduled",
                table: "schedule_fires",
                columns: &["schedule_id", "scheduled_at"],
                unique: false,
            },
        ],
    }
}

fn prepare_owned_schema(
    conn: &Connection,
    spec: &sqlite_schema::SchemaSpec,
    ddl: &str,
    path: &Path,
) -> Result<()> {
    // scheduler.sqlite3 is completely rebuildable. It has one current schema
    // and no row migration: `open` replaces a recognized stale owned file,
    // then this helper initializes/asserts only the current DDL.
    sqlite_schema::prepare_owned(conn, spec, ddl, path, |_| Ok(()))?;
    sqlite_schema::assert_complete_schema_sql(conn, ddl, path)
}

fn scheduler_sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut sidecar = path.as_os_str().to_os_string();
    sidecar.push(suffix);
    PathBuf::from(sidecar)
}

fn validate_scheduler_sidecar_types(path: &Path) -> Result<()> {
    for sidecar in [
        scheduler_sidecar_path(path, "-wal"),
        scheduler_sidecar_path(path, "-shm"),
    ] {
        match std::fs::symlink_metadata(&sidecar) {
            Ok(metadata)
                if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {}
            Ok(_) => anyhow::bail!(
                "scheduler database sidecar must be a regular non-symlink file: {}",
                sidecar.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn assert_recognized_stale_scheduler_projection(conn: &Connection, path: &Path) -> Result<()> {
    let mut table_statement = conn.prepare(
        "SELECT name FROM sqlite_master
         WHERE type='table' AND name NOT LIKE 'sqlite_%'",
    )?;
    let tables = table_statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<std::collections::HashSet<_>>>()?;
    let known_tables: std::collections::HashSet<&str> = [
        "schedule_specs",
        "schedule_fires",
        "schedule_cursors",
        "scheduler_projection_state",
        "schedule_fire_outbox",
    ]
    .into_iter()
    .collect();
    if !tables.contains("schedule_specs")
        || !tables.contains("schedule_fires")
        || tables
            .iter()
            .any(|table| !known_tables.contains(table.as_str()))
    {
        anyhow::bail!(
            "refusing to replace unrecognized scheduler database structure at {}",
            path.display()
        );
    }
    let mut index_statement = conn.prepare(
        "SELECT name FROM sqlite_master
         WHERE type='index' AND name NOT LIKE 'sqlite_%'",
    )?;
    let indexes = index_statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let known_indexes = [
        "idx_fires_schedule_id",
        "idx_fires_status",
        "idx_fires_schedule_scheduled",
    ];
    if indexes
        .iter()
        .any(|index| !known_indexes.contains(&index.as_str()))
    {
        anyhow::bail!(
            "refusing to replace scheduler database with unknown indexes at {}",
            path.display()
        );
    }
    Ok(())
}

fn reset_owned_scheduler_projection(conn: Connection, path: &Path) -> Result<()> {
    let checkpoint: (i64, i64, i64) = conn
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .context("checkpoint stale scheduler projection before replacement")?;
    if checkpoint.0 != 0 || checkpoint.1 != checkpoint.2 {
        anyhow::bail!(
            "scheduler WAL checkpoint incomplete before replacement: busy={}, log={}, checkpointed={}",
            checkpoint.0,
            checkpoint.1,
            checkpoint.2
        );
    }
    conn.close()
        .map_err(|(_, error)| error)
        .context("close stale scheduler projection before replacement")?;
    for target in [
        scheduler_sidecar_path(path, "-shm"),
        scheduler_sidecar_path(path, "-wal"),
        path.to_path_buf(),
    ] {
        lillux::remove_file_durable(&target).with_context(|| {
            format!(
                "remove stale scheduler projection file {}",
                target.display()
            )
        })?;
    }
    Ok(())
}

// ── Database wrapper ────────────────────────────────────────────────

pub struct SchedulerDb {
    inner: std::sync::Mutex<Connection>,
    outbox_drain: std::sync::Mutex<()>,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct SchedulerFireHistoryDiscardReport {
    pub fires: usize,
    pub outbox_snapshots: usize,
    pub cursors: usize,
}

impl SchedulerFireHistoryDiscardReport {
    pub fn total_rows(&self) -> usize {
        self.fires + self.outbox_snapshots + self.cursors
    }
}

impl SchedulerDb {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("failed to create scheduler db dir {}", parent.display())
            })?;
            let metadata = std::fs::symlink_metadata(parent)?;
            if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
                anyhow::bail!(
                    "scheduler db parent must be a real directory: {}",
                    parent.display()
                );
            }
        }
        let spec = scheduler_schema_spec();
        let existed = match std::fs::symlink_metadata(path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
                    anyhow::bail!(
                        "scheduler db must be a regular non-symlink file: {}",
                        path.display()
                    );
                }
                true
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(error.into()),
        };
        validate_scheduler_sidecar_types(path)?;
        if !existed {
            for sidecar in [
                scheduler_sidecar_path(path, "-wal"),
                scheduler_sidecar_path(path, "-shm"),
            ] {
                if std::fs::symlink_metadata(&sidecar).is_ok() {
                    anyhow::bail!(
                        "orphan scheduler sidecar exists without owned database: {}",
                        sidecar.display()
                    );
                }
            }
        }
        let mut conn = Connection::open(path)
            .with_context(|| format!("failed to open scheduler db {}", path.display()))?;

        if existed {
            let integrity: String = conn
                .query_row("PRAGMA integrity_check", [], |row| row.get(0))
                .context("run scheduler database integrity check")?;
            if integrity != "ok" {
                anyhow::bail!(
                    "scheduler database integrity check failed for {}: {integrity}",
                    path.display()
                );
            }
            let app_id: i32 = conn.query_row("PRAGMA application_id", [], |row| row.get(0))?;
            if app_id == spec.application_id {
                let exact = sqlite_schema::assert_owned(&conn, &spec, path).and_then(|_| {
                    sqlite_schema::assert_complete_schema_sql(&conn, SCHEMA_SQL, path)
                });
                if exact.is_err() {
                    assert_recognized_stale_scheduler_projection(&conn, path)?;
                    tracing::warn!(
                        path = %path.display(),
                        "replacing stale owned scheduler projection from signed specs and fire journals"
                    );
                    reset_owned_scheduler_projection(conn, path)?;
                    conn = Connection::open(path)
                        .with_context(|| format!("reopen reset scheduler db {}", path.display()))?;
                }
            } else if app_id != 0 {
                anyhow::bail!(
                    "scheduler database application_id is {app_id}, expected {}; foreign database at {}",
                    spec.application_id,
                    path.display()
                );
            }
        }
        prepare_owned_schema(&conn, &spec, SCHEMA_SQL, path)?;
        Ok(Self {
            inner: std::sync::Mutex::new(conn),
            outbox_drain: std::sync::Mutex::new(()),
        })
    }

    /// Clear only execution-derived scheduler history. Signed schedule YAML
    /// remains authoritative, and `schedule_specs` is deliberately preserved.
    /// The projection marker is published current and empty in the same
    /// transaction so a crash cannot expose a partially cleared fire view.
    pub fn discard_fire_history(&self, dry_run: bool) -> Result<SchedulerFireHistoryDiscardReport> {
        let mut conn = self.lock()?;
        fn count(conn: &Connection, table: &str) -> Result<usize> {
            let rows: i64 =
                conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })?;
            usize::try_from(rows).context("scheduler fire-history row count is invalid")
        }
        if dry_run {
            return Ok(SchedulerFireHistoryDiscardReport {
                fires: count(&conn, "schedule_fires")?,
                outbox_snapshots: count(&conn, "schedule_fire_outbox")?,
                cursors: count(&conn, "schedule_cursors")?,
            });
        }

        let tx = conn.transaction()?;
        let report = SchedulerFireHistoryDiscardReport {
            outbox_snapshots: tx.execute("DELETE FROM schedule_fire_outbox", [])?,
            fires: tx.execute("DELETE FROM schedule_fires", [])?,
            cursors: tx.execute("DELETE FROM schedule_cursors", [])?,
        };
        tx.execute("DELETE FROM scheduler_projection_state", [])?;
        tx.execute(
            "INSERT INTO scheduler_projection_state
                (projection_name, rebuild_complete) VALUES ('fires', 1)",
            [],
        )?;
        tx.commit()?;
        Ok(report)
    }

    /// Open an already-existing scheduler store without creating or migrating
    /// it. Offline projection rebuild uses this fail-closed view when deciding
    /// whether a pending terminal Remove is pinned by a durable scheduler fire;
    /// an absent, foreign, stale, or corrupt store is never equivalent to an
    /// empty store.
    pub fn open_existing_current(path: &Path) -> Result<Self> {
        let metadata = std::fs::symlink_metadata(path)
            .with_context(|| format!("scheduler db must already exist at {}", path.display()))?;
        if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
            anyhow::bail!(
                "scheduler db must be a regular non-symlink file: {}",
                path.display()
            );
        }
        validate_scheduler_sidecar_types(path)?;
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("open existing scheduler db {}", path.display()))?;
        let spec = scheduler_schema_spec();
        sqlite_schema::assert_owned(&conn, &spec, path)
            .with_context(|| format!("verify current scheduler db {}", path.display()))?;
        sqlite_schema::assert_complete_schema_sql(&conn, SCHEMA_SQL, path)
            .with_context(|| format!("verify exact scheduler db {}", path.display()))?;
        let integrity: String = conn
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .context("run scheduler database integrity check")?;
        if integrity != "ok" {
            anyhow::bail!(
                "scheduler database integrity check failed for {}: {integrity}",
                path.display()
            );
        }
        if !fire_projection_is_current_conn(&conn)? {
            anyhow::bail!(
                "scheduler fire projection is incomplete at {}; complete scheduler recovery before offline use",
                path.display()
            );
        }
        Ok(Self {
            inner: std::sync::Mutex::new(conn),
            outbox_drain: std::sync::Mutex::new(()),
        })
    }

    /// Open an in-memory database (for standalone mode and tests where
    /// the scheduler is not actively running).
    pub fn new_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().context("failed to open in-memory scheduler db")?;
        let spec = scheduler_schema_spec();
        prepare_owned_schema(&conn, &spec, SCHEMA_SQL, Path::new(":memory:"))?;
        conn.execute(
            "UPDATE scheduler_projection_state
             SET rebuild_complete = 1
             WHERE projection_name = 'fires' AND rebuild_complete = 0",
            [],
        )?;
        Ok(Self {
            inner: std::sync::Mutex::new(conn),
            outbox_drain: std::sync::Mutex::new(()),
        })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.inner
            .lock()
            .map_err(|e| anyhow!("scheduler db lock poisoned: {}", e))
    }

    // ── schedule_specs ──────────────────────────────────────────

    pub fn upsert_spec(&self, rec: &ScheduleSpecRecord) -> Result<()> {
        validate_schedule_spec_record(rec)?;
        let conn = self.lock()?;
        upsert_spec_conn(&conn, rec)
            .with_context(|| format!("upsert_spec failed for {}", rec.schedule_id))?;
        Ok(())
    }

    /// Atomically replace the complete schedule-spec projection.
    ///
    /// Callers must fully verify and validate every signed source before this
    /// method is entered. One malformed source therefore leaves the previous
    /// complete projection untouched; a successful transaction publishes the
    /// whole new set and removes stale cursors/specs together.
    pub fn replace_specs(&self, records: &[ScheduleSpecRecord]) -> Result<usize> {
        let mut ids = std::collections::HashSet::with_capacity(records.len());
        for record in records {
            validate_schedule_spec_record(record)?;
            if !ids.insert(record.schedule_id.as_str()) {
                anyhow::bail!(
                    "duplicate schedule_id in replacement set: {}",
                    record.schedule_id
                );
            }
        }

        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        tx.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS desired_schedule_specs (
                 schedule_id TEXT PRIMARY KEY NOT NULL
             ) WITHOUT ROWID;
             DELETE FROM desired_schedule_specs;",
        )?;
        for record in records {
            tx.execute(
                "INSERT INTO desired_schedule_specs (schedule_id) VALUES (?1)",
                params![record.schedule_id],
            )?;
            upsert_spec_conn(&tx, record)
                .with_context(|| format!("replace spec {}", record.schedule_id))?;
        }
        tx.execute(
            "DELETE FROM schedule_cursors
             WHERE NOT EXISTS (
                 SELECT 1 FROM desired_schedule_specs desired
                 WHERE desired.schedule_id = schedule_cursors.schedule_id
             )",
            [],
        )?;
        let removed = tx.execute(
            "DELETE FROM schedule_specs
             WHERE NOT EXISTS (
                 SELECT 1 FROM desired_schedule_specs desired
                 WHERE desired.schedule_id = schedule_specs.schedule_id
             )",
            [],
        )?;
        tx.execute("DELETE FROM desired_schedule_specs", [])?;
        tx.commit()?;
        Ok(removed)
    }

    pub fn delete_spec(&self, schedule_id: &str) -> Result<bool> {
        let conn = self.lock()?;
        conn.execute(
            "DELETE FROM schedule_cursors WHERE schedule_id = ?1",
            params![schedule_id],
        )?;
        let n = conn.execute(
            "DELETE FROM schedule_specs WHERE schedule_id = ?1",
            params![schedule_id],
        )?;
        Ok(n > 0)
    }

    pub fn get_spec(&self, schedule_id: &str) -> Result<Option<ScheduleSpecRecord>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare_cached(
            "SELECT schedule_id, item_ref, ref_bindings, params, schedule_type, expression,
                    timezone, misfire_policy, overlap_policy, enabled,
                    project_root, signer_fingerprint, spec_hash, registered_at,
                    requester_fingerprint, capabilities, lateness_grace_secs
             FROM schedule_specs WHERE schedule_id = ?1",
        )?;
        stmt.query_row(params![schedule_id], row_to_spec)
            .optional()
            .map_err(Into::into)
    }

    pub fn load_enabled_specs(&self) -> Result<Vec<ScheduleSpecRecord>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare_cached(
            "SELECT schedule_id, item_ref, ref_bindings, params, schedule_type, expression,
                    timezone, misfire_policy, overlap_policy, enabled,
                    project_root, signer_fingerprint, spec_hash, registered_at,
                    requester_fingerprint, capabilities, lateness_grace_secs
             FROM schedule_specs WHERE enabled = 1",
        )?;
        let rows = stmt.query_map([], row_to_spec)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn list_specs(
        &self,
        enabled_only: bool,
        schedule_type: Option<&str>,
    ) -> Result<Vec<ScheduleSpecRecord>> {
        let sel = "SELECT schedule_id, item_ref, ref_bindings, params, schedule_type, expression,
                          timezone, misfire_policy, overlap_policy, enabled,
                          project_root, signer_fingerprint, spec_hash, registered_at,
                          requester_fingerprint, capabilities, lateness_grace_secs";
        let sql = match (enabled_only, schedule_type) {
            (true, Some(_)) => {
                format!("{sel} FROM schedule_specs WHERE enabled = 1 AND schedule_type = ?")
            }
            (true, None) => format!("{sel} FROM schedule_specs WHERE enabled = 1"),
            (false, Some(_)) => format!("{sel} FROM schedule_specs WHERE schedule_type = ?"),
            (false, None) => format!("{sel} FROM schedule_specs"),
        };
        let conn = self.lock()?;
        let mut stmt = conn.prepare_cached(&sql)?;
        let rows: Vec<ScheduleSpecRecord> = if let Some(st) = schedule_type {
            stmt.query_map(params![st], row_to_spec)?
                .collect::<Result<Vec<_>, _>>()?
        } else {
            stmt.query_map([], row_to_spec)?
                .collect::<Result<Vec<_>, _>>()?
        };
        Ok(rows)
    }

    /// List specs with optional requester filtering.
    ///
    /// When `filter_requester` is `Some(fp)`, only schedules with
    /// `requester_fingerprint = fp` are returned. `None` returns all
    /// schedules (internal callers that intentionally request an unfiltered view).
    pub fn list_specs_filtered(
        &self,
        enabled_only: bool,
        schedule_type: Option<&str>,
        filter_requester: Option<&str>,
    ) -> Result<Vec<ScheduleSpecRecord>> {
        let sel = "SELECT schedule_id, item_ref, ref_bindings, params, schedule_type, expression,
                          timezone, misfire_policy, overlap_policy, enabled,
                          project_root, signer_fingerprint, spec_hash, registered_at,
                          requester_fingerprint, capabilities, lateness_grace_secs";

        // Build WHERE clause dynamically based on filters.
        let mut conditions: Vec<String> = Vec::new();
        let mut param_values: Vec<String> = Vec::new();

        if enabled_only {
            conditions.push("enabled = 1".to_string());
        }
        if let Some(st) = schedule_type {
            conditions.push("schedule_type = ?".to_string());
            param_values.push(st.to_string());
        }
        if let Some(fp) = filter_requester {
            conditions.push("requester_fingerprint = ?".to_string());
            param_values.push(fp.to_string());
        }

        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", conditions.join(" AND "))
        };
        let sql = format!("{sel} FROM schedule_specs{where_clause}");

        let conn = self.lock()?;
        let mut stmt = conn.prepare_cached(&sql)?;

        let params: Vec<&dyn rusqlite::ToSql> = param_values
            .iter()
            .map(|v| v as &dyn rusqlite::ToSql)
            .collect();
        let rows: Vec<ScheduleSpecRecord> = stmt
            .query_map(params.as_slice(), row_to_spec)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // ── schedule_cursors ────────────────────────────────────────

    pub fn get_cursor(&self, schedule_id: &str) -> Result<Option<ScheduleCursorRecord>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare_cached(
            "SELECT schedule_id, spec_hash, last_scheduled_at, next_fire_at,
                    last_evaluated_at, updated_at
             FROM schedule_cursors WHERE schedule_id = ?1",
        )?;
        stmt.query_row(params![schedule_id], row_to_cursor)
            .optional()
            .map_err(Into::into)
    }

    pub fn get_cursors_batch(
        &self,
        schedule_ids: &[String],
    ) -> Result<std::collections::HashMap<String, ScheduleCursorRecord>> {
        if schedule_ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let placeholders: Vec<String> = schedule_ids
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 1))
            .collect();
        let sql = format!(
            "SELECT schedule_id, spec_hash, last_scheduled_at, next_fire_at,
                    last_evaluated_at, updated_at
             FROM schedule_cursors WHERE schedule_id IN ({})",
            placeholders.join(","),
        );
        let conn = self.lock()?;
        let mut stmt = conn.prepare(&sql)?;
        let params: Vec<&dyn rusqlite::ToSql> = schedule_ids
            .iter()
            .map(|id| id as &dyn rusqlite::ToSql)
            .collect();
        let rows = stmt.query_map(params.as_slice(), row_to_cursor)?;
        let mut map = std::collections::HashMap::new();
        for row in rows {
            let rec = row?;
            map.insert(rec.schedule_id.clone(), rec);
        }
        Ok(map)
    }

    pub fn upsert_cursor(&self, rec: &ScheduleCursorRecord) -> Result<()> {
        self.lock()?.execute(
            "INSERT INTO schedule_cursors
                (schedule_id, spec_hash, last_scheduled_at, next_fire_at,
                 last_evaluated_at, updated_at)
             VALUES (?1,?2,?3,?4,?5,?6)
             ON CONFLICT(schedule_id) DO UPDATE SET
                spec_hash=excluded.spec_hash,
                last_scheduled_at=excluded.last_scheduled_at,
                next_fire_at=excluded.next_fire_at,
                last_evaluated_at=excluded.last_evaluated_at,
                updated_at=excluded.updated_at",
            params![
                rec.schedule_id,
                rec.spec_hash,
                rec.last_scheduled_at,
                rec.next_fire_at,
                rec.last_evaluated_at,
                rec.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn delete_cursor(&self, schedule_id: &str) -> Result<bool> {
        let n = self.lock()?.execute(
            "DELETE FROM schedule_cursors WHERE schedule_id = ?1",
            params![schedule_id],
        )?;
        Ok(n > 0)
    }

    /// Whether the persisted fire projection completed its last full replay.
    /// Normal daemon startup trusts the indexed fire/cursor state only when
    /// this marker is set. A missing, duplicate, or non-boolean marker fails
    /// closed and routes startup through the explicit full replay path.
    pub fn fire_projection_is_current(&self) -> Result<bool> {
        let conn = self.lock()?;
        fire_projection_is_current_conn(&conn)
    }

    /// Invalidate the fire projection before a destructive full replay.
    /// Committing the invalid marker and clear together ensures a crash can
    /// never leave a partial replay looking current on the next startup.
    pub fn begin_fire_projection_rebuild(&self) -> Result<()> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        let pending_outbox: i64 =
            tx.query_row("SELECT COUNT(*) FROM schedule_fire_outbox", [], |row| {
                row.get(0)
            })?;
        if pending_outbox != 0 {
            anyhow::bail!(
                "cannot rebuild scheduler fire projection with {pending_outbox} undrained journal snapshot(s)"
            );
        }
        // Normalize invalid marker contents as part of entering rebuild. The
        // schema itself has already been ownership-verified by open().
        tx.execute("DELETE FROM scheduler_projection_state", [])?;
        tx.execute(
            "INSERT INTO scheduler_projection_state
                (projection_name, rebuild_complete) VALUES ('fires', 0)",
            [],
        )?;
        tx.execute("DELETE FROM schedule_fires", [])?;
        tx.execute("DELETE FROM schedule_cursors", [])?;
        tx.commit()?;
        Ok(())
    }

    /// Publish a successfully replayed fire projection. Cursor repair remains
    /// incremental: missing/stale schedule cursors are reconstructed from the
    /// indexed latest fire and do not require another JSONL walk.
    pub fn finish_fire_projection_rebuild(&self) -> Result<()> {
        let changed = self.lock()?.execute(
            "UPDATE scheduler_projection_state
             SET rebuild_complete = 1
             WHERE projection_name = 'fires' AND rebuild_complete = 0",
            [],
        )?;
        if changed != 1 {
            anyhow::bail!(
                "scheduler fire projection rebuild marker is missing or already published"
            );
        }
        Ok(())
    }

    /// Repair only absent, spec-stale, or fire-stale cursors from indexed
    /// projection rows. Work is proportional to the number of live schedules,
    /// never to historical fire count.
    pub fn reconcile_cursors_for_specs(
        &self,
        specs: &[ScheduleSpecRecord],
        now: i64,
    ) -> Result<usize> {
        let schedule_ids: Vec<String> = specs.iter().map(|s| s.schedule_id.clone()).collect();
        let cursors = self.get_cursors_batch(&schedule_ids)?;
        let last_fires = self.get_last_fires_batch(&schedule_ids)?;
        let mut repaired = 0usize;

        for spec in specs {
            let last_fire = last_fires.get(&spec.schedule_id);
            let projected_last = last_fire.map(|fire| fire.scheduled_at);
            let current = cursors.get(&spec.schedule_id).is_some_and(|cursor| {
                cursor.spec_hash == spec.spec_hash && cursor.last_scheduled_at == projected_last
            });
            if current {
                continue;
            }
            let plan = planning::plan_schedule(spec, last_fire, now);
            self.upsert_cursor(&ScheduleCursorRecord {
                schedule_id: spec.schedule_id.clone(),
                spec_hash: spec.spec_hash.clone(),
                last_scheduled_at: plan.last_scheduled_at,
                next_fire_at: plan.next_fire_at,
                last_evaluated_at: Some(now),
                updated_at: now,
            })?;
            repaired += 1;
        }
        Ok(repaired)
    }

    pub fn rebuild_cursors_for_specs(&self, specs: &[ScheduleSpecRecord], now: i64) -> Result<()> {
        let schedule_ids: Vec<String> = specs.iter().map(|s| s.schedule_id.clone()).collect();
        let last_fires = self.get_last_fires_batch(&schedule_ids)?;
        for spec in specs {
            let last_fire = last_fires.get(&spec.schedule_id);
            let plan = planning::plan_schedule(spec, last_fire, now);
            self.upsert_cursor(&ScheduleCursorRecord {
                schedule_id: spec.schedule_id.clone(),
                spec_hash: spec.spec_hash.clone(),
                last_scheduled_at: plan.last_scheduled_at,
                next_fire_at: plan.next_fire_at,
                last_evaluated_at: Some(now),
                updated_at: now,
            })?;
        }
        Ok(())
    }

    // ── schedule_fires ──────────────────────────────────────────

    /// Mutate a fire and enqueue its complete post-mutation snapshot for the
    /// append-only JSONL authority in one SQLite transaction.
    pub(crate) fn upsert_fire(&self, rec: &FireRecord) -> Result<()> {
        validate_fire_record(rec)?;
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        require_current_fire_projection_conn(&tx)?;
        if let Some(previous) = get_fire_conn(&tx, &rec.fire_id)? {
            rec.validate_transition_from(&previous)?;
        }
        upsert_fire_conn(&tx, rec)
            .with_context(|| format!("upsert_fire failed for {}", rec.fire_id))?;
        enqueue_current_fire_snapshot_conn(&tx, &rec.fire_id)?;
        refresh_cursor_for_schedule_conn(&tx, &rec.schedule_id, lillux::time::timestamp_millis())?;
        tx.commit()?;
        Ok(())
    }

    /// Apply one already-durable JSONL snapshot during a full projection
    /// replay. Rebuild must not enqueue the authority back into its own outbox.
    pub(crate) fn project_fire_from_journal(&self, rec: &FireRecord) -> Result<()> {
        validate_fire_record(rec)?;
        let conn = self.lock()?;
        upsert_fire_conn(&conn, rec)
            .with_context(|| format!("project fire from journal failed for {}", rec.fire_id))
    }

    /// Atomic claim: INSERT if absent. Returns true if the insert succeeded
    /// (fire was claimed), false if it already existed.
    pub(crate) fn claim_fire(&self, rec: &FireRecord) -> Result<bool> {
        validate_fire_record(rec)?;
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        require_current_fire_projection_conn(&tx)?;
        let changed = tx
            .execute(
                "INSERT OR IGNORE INTO schedule_fires
                (fire_id, schedule_id, scheduled_at, fired_at, completed_at, thread_id,
                 status, trigger_reason, outcome, signer_fingerprint)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                params![
                    rec.fire_id,
                    rec.schedule_id,
                    rec.scheduled_at,
                    rec.fired_at,
                    rec.completed_at,
                    rec.thread_id,
                    rec.status,
                    rec.trigger_reason,
                    rec.outcome,
                    rec.signer_fingerprint,
                ],
            )
            .with_context(|| format!("claim_fire failed for {}", rec.fire_id))?;
        if changed > 0 {
            enqueue_current_fire_snapshot_conn(&tx, &rec.fire_id)?;
            refresh_cursor_for_schedule_conn(
                &tx,
                &rec.schedule_id,
                lillux::time::timestamp_millis(),
            )?;
        }
        tx.commit()?;
        Ok(changed > 0)
    }

    /// Reclaim a fire that was persisted but never got a running thread.
    /// The caller has already proved the deterministic thread is absent. Fire
    /// dispatch identity is immutable, so reclaim is an eligibility check; it
    /// never rewrites fired_at, thread_id, trigger reason, or the journal.
    pub(crate) fn reclaim_fire(&self, fire_id: &str) -> Result<bool> {
        let conn = self.lock()?;
        require_current_fire_projection_conn(&conn)?;
        let Some(record) = get_fire_conn(&conn, fire_id)? else {
            return Ok(false);
        };
        record.validate()?;
        Ok(record.status == "dispatched")
    }

    /// Number of committed fire snapshots not yet synced into JSONL.
    pub fn pending_fire_outbox(&self) -> Result<usize> {
        let count: i64 =
            self.lock()?
                .query_row("SELECT COUNT(*) FROM schedule_fire_outbox", [], |row| {
                    row.get(0)
                })?;
        usize::try_from(count).context("negative scheduler fire outbox count")
    }

    /// Sync every committed outbox snapshot to its schedule journal, then
    /// acknowledge it. The caller must own the scheduler runtime gate (the
    /// ordinary read side, or the exclusive side during startup/maintenance).
    ///
    /// A process crash after `sync_data` and before the acknowledgement replays
    /// the same complete snapshot. The drainer is serialized so two read-side
    /// scheduler operations cannot append an older snapshot after a newer one;
    /// JSONL replay is therefore deterministic last-wins even with duplicates.
    pub fn drain_fire_outbox(&self, runtime_state_dir: &Path) -> Result<usize> {
        let runtime_directory = lillux::PinnedDirectory::open(runtime_state_dir)?
            .ok_or_else(|| anyhow!("runtime-state directory is absent"))?;
        self.drain_fire_outbox_in_directory(&runtime_directory)
    }

    /// Sync every committed outbox snapshot beneath one exact runtime root.
    pub fn drain_fire_outbox_in_directory(
        &self,
        runtime_directory: &lillux::PinnedDirectory,
    ) -> Result<usize> {
        let _drain = self
            .outbox_drain
            .lock()
            .map_err(|error| anyhow!("scheduler outbox drain lock poisoned: {error}"))?;
        let schedules_directory =
            runtime_directory.open_or_create_child(std::ffi::OsStr::new("schedules"), 0o700)?;
        let mut schedule_directories = std::collections::BTreeMap::new();
        let mut drained = 0usize;

        loop {
            let next: Option<(i64, String, String, String)> = {
                let conn = self.lock()?;
                conn.query_row(
                    "SELECT sequence, fire_id, schedule_id, snapshot_json
                     FROM schedule_fire_outbox
                     ORDER BY sequence ASC
                     LIMIT 1",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .optional()?
            };
            let Some((sequence, fire_id, schedule_id, snapshot_json)) = next else {
                break;
            };

            super::crontab::validate_schedule_id(&schedule_id).with_context(|| {
                format!("invalid schedule id in fire outbox sequence {sequence}")
            })?;
            let snapshot: FireRecord = serde_json::from_str(&snapshot_json)
                .with_context(|| format!("decode scheduler fire outbox sequence {sequence}"))?;
            validate_fire_record(&snapshot)
                .with_context(|| format!("validate scheduler fire outbox sequence {sequence}"))?;
            if snapshot.fire_id != fire_id || snapshot.schedule_id != schedule_id {
                anyhow::bail!(
                    "scheduler fire outbox identity mismatch at sequence {sequence}: row=({fire_id}, {schedule_id}), snapshot=({}, {})",
                    snapshot.fire_id,
                    snapshot.schedule_id,
                );
            }
            if !schedule_directories.contains_key(&schedule_id) {
                let directory = schedules_directory
                    .open_or_create_child(std::ffi::OsStr::new(&schedule_id), 0o700)?;
                schedule_directories.insert(schedule_id.clone(), directory);
            }
            let schedule_directory = schedule_directories
                .get(&schedule_id)
                .expect("schedule directory inserted above");
            super::projection::append_fire_jsonl_entry_in_directory(
                schedule_directory,
                &schedule_id,
                &snapshot,
            )
            .with_context(|| format!("sync scheduler fire outbox sequence {sequence}"))?;

            let changed = self.lock()?.execute(
                "DELETE FROM schedule_fire_outbox
                 WHERE sequence = ?1 AND fire_id = ?2 AND schedule_id = ?3
                   AND snapshot_json = ?4",
                params![sequence, fire_id, schedule_id, snapshot_json],
            )?;
            if changed != 1 {
                anyhow::bail!("scheduler fire outbox acknowledgement lost sequence {sequence}");
            }
            drained = drained
                .checked_add(1)
                .context("scheduler fire outbox drain count overflow")?;
        }

        Ok(drained)
    }

    pub fn get_fire(&self, fire_id: &str) -> Result<Option<FireRecord>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare_cached(
            "SELECT fire_id, schedule_id, scheduled_at, fired_at, completed_at, thread_id,
                    status, trigger_reason, outcome, signer_fingerprint
             FROM schedule_fires WHERE fire_id = ?1",
        )?;
        stmt.query_row(params![fire_id], row_to_fire)
            .optional()
            .map_err(Into::into)
    }

    /// Batch-get last fire for multiple schedules. Returns a map from
    /// schedule_id → FireRecord. Used by scheduler_list to avoid N+1 queries.
    pub fn get_last_fires_batch(
        &self,
        schedule_ids: &[String],
    ) -> Result<std::collections::HashMap<String, FireRecord>> {
        if schedule_ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let placeholders: Vec<String> = schedule_ids
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 1))
            .collect();
        let sql = format!(
            "SELECT fire_id, schedule_id, scheduled_at, fired_at, completed_at, thread_id,
                    status, trigger_reason, outcome, signer_fingerprint
             FROM schedule_fires
             WHERE schedule_id IN ({})
               AND fire_id = (
                   SELECT candidate.fire_id
                   FROM schedule_fires AS candidate
                   WHERE candidate.schedule_id = schedule_fires.schedule_id
                   ORDER BY candidate.scheduled_at DESC, candidate.fire_id DESC
                   LIMIT 1
               )",
            placeholders.join(","),
        );
        let conn = self.lock()?;
        let mut stmt = conn.prepare(&sql)?;
        let params: Vec<&dyn rusqlite::ToSql> = schedule_ids
            .iter()
            .map(|id| id as &dyn rusqlite::ToSql)
            .collect();
        let rows = stmt.query_map(params.as_slice(), row_to_fire)?;
        let mut map = std::collections::HashMap::new();
        for row in rows {
            let rec = row?;
            map.insert(rec.schedule_id.clone(), rec);
        }
        Ok(map)
    }

    pub fn get_last_fire(&self, schedule_id: &str) -> Result<Option<FireRecord>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare_cached(
            "SELECT fire_id, schedule_id, scheduled_at, fired_at, completed_at, thread_id,
                    status, trigger_reason, outcome, signer_fingerprint
             FROM schedule_fires
             WHERE schedule_id = ?1
             ORDER BY scheduled_at DESC, fire_id DESC LIMIT 1",
        )?;
        stmt.query_row(params![schedule_id], row_to_fire)
            .optional()
            .map_err(Into::into)
    }

    pub fn get_inflight_fires(&self) -> Result<Vec<FireRecord>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare_cached(
            "SELECT fire_id, schedule_id, scheduled_at, fired_at, completed_at, thread_id,
                    status, trigger_reason, outcome, signer_fingerprint
             FROM schedule_fires WHERE status = 'dispatched'",
        )?;
        let rows = stmt.query_map([], row_to_fire)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn get_inflight_for_schedule(&self, schedule_id: &str) -> Result<Option<FireRecord>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare_cached(
            "SELECT fire_id, schedule_id, scheduled_at, fired_at, completed_at, thread_id,
                    status, trigger_reason, outcome, signer_fingerprint
             FROM schedule_fires
             WHERE schedule_id = ?1 AND status = 'dispatched'
             ORDER BY scheduled_at DESC, fire_id DESC LIMIT 1",
        )?;
        stmt.query_row(params![schedule_id], row_to_fire)
            .optional()
            .map_err(Into::into)
    }

    pub fn find_fire_by_thread(&self, thread_id: &str) -> Result<Option<FireRecord>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare_cached(
            "SELECT fire_id, schedule_id, scheduled_at, fired_at, completed_at, thread_id,
                    status, trigger_reason, outcome, signer_fingerprint
             FROM schedule_fires
             WHERE thread_id = ?1 AND status = 'dispatched'",
        )?;
        stmt.query_row(params![thread_id], row_to_fire)
            .optional()
            .map_err(Into::into)
    }

    /// Find dispatched fires older than `threshold_secs` that may need repair.
    /// Used by the periodic repair sweep to finalize stale fires.
    pub fn find_stale_dispatched_fires(&self, threshold_secs: i64) -> Result<Vec<FireRecord>> {
        let cutoff = lillux::time::timestamp_millis() - (threshold_secs * 1000);
        let conn = self.lock()?;
        let mut stmt = conn.prepare_cached(
            "SELECT fire_id, schedule_id, scheduled_at, fired_at, completed_at, thread_id,
                    status, trigger_reason, outcome, signer_fingerprint
             FROM schedule_fires
             WHERE status = 'dispatched' AND fired_at IS NOT NULL AND fired_at < ?1",
        )?;
        let rows = stmt.query_map(params![cutoff], row_to_fire)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Count fires for a schedule (optionally filtered by status).
    pub fn count_fires(&self, schedule_id: &str, status_filter: Option<&str>) -> Result<usize> {
        let conn = self.lock()?;
        let total: usize = match status_filter {
            Some(s) => conn.query_row(
                "SELECT COUNT(*) FROM schedule_fires WHERE schedule_id = ?1 AND status = ?2",
                params![schedule_id, s],
                |row| row.get::<_, i64>(0),
            )? as usize,
            None => conn.query_row(
                "SELECT COUNT(*) FROM schedule_fires WHERE schedule_id = ?1",
                params![schedule_id],
                |row| row.get::<_, i64>(0),
            )? as usize,
        };
        Ok(total)
    }

    /// Commit an invalid projection marker before a non-dry fire-journal
    /// retention rewrite. If the process stops before the final transaction,
    /// startup sees this marker and rebuilds SQLite from exactly the retained
    /// journals.
    pub fn begin_fire_retention(&self) -> Result<()> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        if !fire_projection_is_current_conn(&tx)? {
            anyhow::bail!("cannot begin retention on an incomplete scheduler fire projection");
        }
        let pending_outbox: i64 =
            tx.query_row("SELECT COUNT(*) FROM schedule_fire_outbox", [], |row| {
                row.get(0)
            })?;
        if pending_outbox != 0 {
            anyhow::bail!(
                "cannot retain scheduler fire journals with {pending_outbox} undrained snapshot(s)"
            );
        }
        let changed = tx.execute(
            "UPDATE scheduler_projection_state
             SET rebuild_complete = 0
             WHERE projection_name = 'fires' AND rebuild_complete = 1",
            [],
        )?;
        if changed != 1 {
            anyhow::bail!("scheduler fire projection marker changed before retention");
        }
        tx.commit()?;
        Ok(())
    }

    /// Read-only proof that the exact portable-journal retention selection can
    /// be applied to the current SQLite projection. Dry-run uses this same
    /// target validation as the destructive transaction without invalidating
    /// the projection marker or deleting rows.
    pub fn verify_fire_retention_targets(
        &self,
        targets: &[ryeos_state::gc::retention::FireRetentionTarget],
    ) -> Result<()> {
        let conn = self.lock()?;
        if !fire_projection_is_current_conn(&conn)? {
            anyhow::bail!(
                "cannot verify retention against an incomplete scheduler fire projection"
            );
        }
        let pending_outbox: i64 =
            conn.query_row("SELECT COUNT(*) FROM schedule_fire_outbox", [], |row| {
                row.get(0)
            })?;
        if pending_outbox != 0 {
            anyhow::bail!(
                "cannot verify scheduler fire retention with {pending_outbox} undrained snapshot(s)"
            );
        }
        validate_fire_retention_targets_conn(&conn, targets)?;
        Ok(())
    }

    /// Delete exactly the identities chosen by the journal retention planner,
    /// refresh affected cursors, and republish the current marker in one final
    /// transaction. No SQL age/count selection is performed here.
    pub fn finish_fire_retention(
        &self,
        targets: &[ryeos_state::gc::retention::FireRetentionTarget],
    ) -> Result<usize> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        if fire_projection_is_current_conn(&tx)? {
            anyhow::bail!("scheduler fire retention marker was not invalidated");
        }

        let affected_schedules = validate_fire_retention_targets_conn(&tx, targets)?;
        for target in targets {
            let changed = tx.execute(
                "DELETE FROM schedule_fires WHERE fire_id = ?1 AND schedule_id = ?2",
                params![target.fire_id, target.schedule_id],
            )?;
            if changed != 1 {
                anyhow::bail!(
                    "scheduler fire changed during retention: {}",
                    target.fire_id
                );
            }
        }

        let now = lillux::time::timestamp_millis();
        for schedule_id in affected_schedules {
            refresh_cursor_for_schedule_conn(&tx, &schedule_id, now)
                .with_context(|| format!("refresh cursor after retaining {schedule_id}"))?;
        }
        let changed = tx.execute(
            "UPDATE scheduler_projection_state
             SET rebuild_complete = 1
             WHERE projection_name = 'fires' AND rebuild_complete = 0",
            [],
        )?;
        if changed != 1 {
            anyhow::bail!("scheduler fire retention marker disappeared before publication");
        }
        tx.commit()?;
        Ok(targets.len())
    }

    /// Finish an explicit schedule-history purge after its directory has been
    /// removed under the scheduler write gate. This is not retention selection:
    /// the operator requested the entire named history be removed. The invalid
    /// marker established by [`Self::begin_fire_retention`] still makes a crash
    /// between filesystem removal and this transaction rebuild-safe.
    pub fn finish_schedule_fire_purge(&self, schedule_id: &str) -> Result<usize> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        if fire_projection_is_current_conn(&tx)? {
            anyhow::bail!("scheduler fire purge marker was not invalidated");
        }
        let deleted = tx.execute(
            "DELETE FROM schedule_fires WHERE schedule_id = ?1",
            params![schedule_id],
        )?;
        refresh_cursor_for_schedule_conn(&tx, schedule_id, lillux::time::timestamp_millis())?;
        let changed = tx.execute(
            "UPDATE scheduler_projection_state
             SET rebuild_complete = 1
             WHERE projection_name = 'fires' AND rebuild_complete = 0",
            [],
        )?;
        if changed != 1 {
            anyhow::bail!("scheduler fire purge marker disappeared before publication");
        }
        tx.commit()?;
        Ok(deleted)
    }

    pub fn list_fires(
        &self,
        schedule_id: &str,
        status_filter: Option<&str>,
        limit: usize,
    ) -> Result<(Vec<FireRecord>, usize)> {
        let conn = self.lock()?;

        let total: usize = match status_filter {
            Some(s) => conn.query_row(
                "SELECT COUNT(*) FROM schedule_fires WHERE schedule_id = ?1 AND status = ?2",
                params![schedule_id, s],
                |row| row.get::<_, i64>(0),
            )? as usize,
            None => conn.query_row(
                "SELECT COUNT(*) FROM schedule_fires WHERE schedule_id = ?1",
                params![schedule_id],
                |row| row.get::<_, i64>(0),
            )? as usize,
        };

        let sql = match status_filter {
            Some(_) => {
                "SELECT fire_id, schedule_id, scheduled_at, fired_at, completed_at, thread_id,
                               status, trigger_reason, outcome, signer_fingerprint
                        FROM schedule_fires
                        WHERE schedule_id = ?1 AND status = ?2
                        ORDER BY scheduled_at DESC, fire_id DESC LIMIT ?3"
            }
            None => {
                "SELECT fire_id, schedule_id, scheduled_at, fired_at, completed_at, thread_id,
                            status, trigger_reason, outcome, signer_fingerprint
                     FROM schedule_fires
                     WHERE schedule_id = ?1
                     ORDER BY scheduled_at DESC, fire_id DESC LIMIT ?2"
            }
        };
        let mut stmt = conn.prepare_cached(sql)?;
        let fires: Vec<FireRecord> = if let Some(sf) = status_filter {
            stmt.query_map(params![schedule_id, sf, limit as i64], |row| {
                row_to_fire(row)
            })?
            .collect::<Result<Vec<_>, _>>()?
        } else {
            stmt.query_map(params![schedule_id, limit as i64], row_to_fire)?
                .collect::<Result<Vec<_>, _>>()?
        };
        Ok((fires, total))
    }
}

// ── Row mappers ─────────────────────────────────────────────────────

fn upsert_spec_conn(conn: &Connection, rec: &ScheduleSpecRecord) -> Result<()> {
    let capabilities_json = serde_json::to_string(&rec.capabilities)?;
    let ref_bindings_json = serde_json::to_string(&rec.ref_bindings)?;
    conn.execute(
        "INSERT INTO schedule_specs
            (schedule_id, item_ref, ref_bindings, params, schedule_type, expression,
             timezone, misfire_policy, overlap_policy, enabled,
             project_root, signer_fingerprint, spec_hash, registered_at,
             requester_fingerprint, capabilities, lateness_grace_secs)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)
         ON CONFLICT(schedule_id) DO UPDATE SET
            item_ref=excluded.item_ref, ref_bindings=excluded.ref_bindings,
            params=excluded.params,
            schedule_type=excluded.schedule_type, expression=excluded.expression,
            timezone=excluded.timezone, misfire_policy=excluded.misfire_policy,
            overlap_policy=excluded.overlap_policy, enabled=excluded.enabled,
            project_root=excluded.project_root, signer_fingerprint=excluded.signer_fingerprint,
            spec_hash=excluded.spec_hash, registered_at=excluded.registered_at,
            requester_fingerprint=excluded.requester_fingerprint,
            capabilities=excluded.capabilities,
            lateness_grace_secs=excluded.lateness_grace_secs",
        params![
            rec.schedule_id,
            rec.item_ref,
            ref_bindings_json,
            rec.params,
            rec.schedule_type,
            rec.expression,
            rec.timezone,
            rec.misfire_policy,
            rec.overlap_policy,
            rec.enabled as i32,
            rec.project_root,
            rec.signer_fingerprint,
            rec.spec_hash,
            rec.registered_at,
            rec.requester_fingerprint,
            capabilities_json,
            rec.lateness_grace_secs,
        ],
    )?;
    Ok(())
}

fn upsert_fire_conn(conn: &Connection, rec: &FireRecord) -> Result<()> {
    conn.execute(
        "INSERT INTO schedule_fires
            (fire_id, schedule_id, scheduled_at, fired_at, completed_at, thread_id,
             status, trigger_reason, outcome, signer_fingerprint)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
         ON CONFLICT(fire_id) DO UPDATE SET
            schedule_id=excluded.schedule_id,
            scheduled_at=excluded.scheduled_at,
            fired_at=excluded.fired_at,
            completed_at=excluded.completed_at,
            thread_id=excluded.thread_id,
            status=excluded.status,
            trigger_reason=excluded.trigger_reason,
            outcome=excluded.outcome,
            signer_fingerprint=excluded.signer_fingerprint",
        params![
            rec.fire_id,
            rec.schedule_id,
            rec.scheduled_at,
            rec.fired_at,
            rec.completed_at,
            rec.thread_id,
            rec.status,
            rec.trigger_reason,
            rec.outcome,
            rec.signer_fingerprint,
        ],
    )?;
    Ok(())
}

fn validate_fire_record(record: &FireRecord) -> Result<()> {
    record.validate()
}

fn enqueue_current_fire_snapshot_conn(conn: &Connection, fire_id: &str) -> Result<FireRecord> {
    let fire = get_fire_conn(conn, fire_id)?
        .ok_or_else(|| anyhow!("fire row missing after mutation: {fire_id}"))?;
    validate_fire_record(&fire)?;
    let snapshot_json = serde_json::to_string(&fire)
        .with_context(|| format!("encode post-mutation fire snapshot {fire_id}"))?;
    conn.execute(
        "INSERT INTO schedule_fire_outbox
            (fire_id, schedule_id, snapshot_json)
         VALUES (?1, ?2, ?3)",
        params![fire.fire_id, fire.schedule_id, snapshot_json],
    )?;
    Ok(fire)
}

fn validate_fire_retention_targets_conn(
    conn: &Connection,
    targets: &[ryeos_state::gc::retention::FireRetentionTarget],
) -> Result<std::collections::BTreeSet<String>> {
    let mut affected_schedules = std::collections::BTreeSet::new();
    let mut seen = std::collections::BTreeSet::new();
    for target in targets {
        if !seen.insert((target.schedule_id.as_str(), target.fire_id.as_str())) {
            anyhow::bail!(
                "duplicate scheduler fire retention target {} for {}",
                target.fire_id,
                target.schedule_id,
            );
        }
        let fire = get_fire_conn(conn, &target.fire_id)?.ok_or_else(|| {
            anyhow!(
                "retained journal selected missing scheduler fire {}",
                target.fire_id
            )
        })?;
        fire.validate().with_context(|| {
            format!(
                "validate scheduler fire retention target {}",
                target.fire_id
            )
        })?;
        if fire.schedule_id != target.schedule_id {
            anyhow::bail!(
                "scheduler fire retention identity mismatch for {}: journal={}, projection={}",
                target.fire_id,
                target.schedule_id,
                fire.schedule_id,
            );
        }
        if !ryeos_state::gc::retention::is_terminal_fire_status(&fire.status) {
            anyhow::bail!(
                "scheduler fire retention target {} is no longer terminal ({})",
                fire.fire_id,
                fire.status,
            );
        }
        let watermark: Option<String> = conn
            .query_row(
                "SELECT fire_id FROM schedule_fires
                 WHERE schedule_id = ?1
                 ORDER BY scheduled_at DESC, fire_id DESC
                 LIMIT 1",
                params![target.schedule_id],
                |row| row.get(0),
            )
            .optional()?;
        if watermark.as_deref() == Some(target.fire_id.as_str()) {
            anyhow::bail!(
                "scheduler fire retention refused cursor watermark {}",
                target.fire_id
            );
        }
        affected_schedules.insert(target.schedule_id.clone());
    }
    Ok(affected_schedules)
}

fn fire_projection_is_current_conn(conn: &Connection) -> Result<bool> {
    let row_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM scheduler_projection_state",
        [],
        |row| row.get(0),
    )?;
    let value: Option<i64> = conn
        .query_row(
            "SELECT rebuild_complete
             FROM scheduler_projection_state
             WHERE projection_name = 'fires'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    Ok(row_count == 1 && value == Some(1))
}

fn require_current_fire_projection_conn(conn: &Connection) -> Result<()> {
    if !fire_projection_is_current_conn(conn)? {
        anyhow::bail!("scheduler fire projection is incomplete; mutation refused");
    }
    Ok(())
}

fn row_to_spec(row: &rusqlite::Row<'_>) -> Result<ScheduleSpecRecord, rusqlite::Error> {
    let ref_bindings_json: String = row.get("ref_bindings")?;
    let ref_bindings = serde_json::from_str(&ref_bindings_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let capabilities_json: String = row.get("capabilities")?;
    let capabilities: Vec<String> = serde_json::from_str(&capabilities_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;
    Ok(ScheduleSpecRecord {
        schedule_id: row.get("schedule_id")?,
        item_ref: row.get("item_ref")?,
        ref_bindings,
        params: row.get("params")?,
        schedule_type: row.get("schedule_type")?,
        expression: row.get("expression")?,
        timezone: row.get("timezone")?,
        misfire_policy: row.get("misfire_policy")?,
        overlap_policy: row.get("overlap_policy")?,
        enabled: row.get::<_, i32>("enabled")? != 0,
        project_root: row.get("project_root")?,
        signer_fingerprint: row.get("signer_fingerprint")?,
        spec_hash: row.get("spec_hash")?,
        registered_at: row.get("registered_at")?,
        requester_fingerprint: row.get("requester_fingerprint")?,
        capabilities,
        lateness_grace_secs: row.get("lateness_grace_secs")?,
    })
}

fn row_to_fire(row: &rusqlite::Row<'_>) -> Result<FireRecord, rusqlite::Error> {
    Ok(FireRecord {
        fire_id: row.get("fire_id")?,
        schedule_id: row.get("schedule_id")?,
        scheduled_at: row.get("scheduled_at")?,
        fired_at: row.get("fired_at")?,
        completed_at: row.get("completed_at")?,
        thread_id: row.get("thread_id")?,
        status: row.get("status")?,
        trigger_reason: row.get("trigger_reason")?,
        outcome: row.get("outcome")?,
        signer_fingerprint: row.get("signer_fingerprint")?,
    })
}

fn row_to_cursor(row: &rusqlite::Row<'_>) -> Result<ScheduleCursorRecord, rusqlite::Error> {
    Ok(ScheduleCursorRecord {
        schedule_id: row.get("schedule_id")?,
        spec_hash: row.get("spec_hash")?,
        last_scheduled_at: row.get("last_scheduled_at")?,
        next_fire_at: row.get("next_fire_at")?,
        last_evaluated_at: row.get("last_evaluated_at")?,
        updated_at: row.get("updated_at")?,
    })
}

fn get_fire_conn(conn: &Connection, fire_id: &str) -> Result<Option<FireRecord>> {
    let mut stmt = conn.prepare_cached(
        "SELECT fire_id, schedule_id, scheduled_at, fired_at, completed_at, thread_id,
                status, trigger_reason, outcome, signer_fingerprint
         FROM schedule_fires WHERE fire_id = ?1",
    )?;
    stmt.query_row(params![fire_id], row_to_fire)
        .optional()
        .map_err(Into::into)
}

fn get_spec_conn(conn: &Connection, schedule_id: &str) -> Result<Option<ScheduleSpecRecord>> {
    let mut stmt = conn.prepare_cached(
        "SELECT schedule_id, item_ref, ref_bindings, params, schedule_type, expression,
                timezone, misfire_policy, overlap_policy, enabled,
                project_root, signer_fingerprint, spec_hash, registered_at,
                requester_fingerprint, capabilities, lateness_grace_secs
         FROM schedule_specs WHERE schedule_id = ?1",
    )?;
    stmt.query_row(params![schedule_id], row_to_spec)
        .optional()
        .map_err(Into::into)
}

fn get_last_fire_conn(conn: &Connection, schedule_id: &str) -> Result<Option<FireRecord>> {
    let mut stmt = conn.prepare_cached(
        "SELECT fire_id, schedule_id, scheduled_at, fired_at, completed_at, thread_id,
                status, trigger_reason, outcome, signer_fingerprint
         FROM schedule_fires
         WHERE schedule_id = ?1
         ORDER BY scheduled_at DESC, fire_id DESC LIMIT 1",
    )?;
    stmt.query_row(params![schedule_id], row_to_fire)
        .optional()
        .map_err(Into::into)
}

fn refresh_cursor_for_schedule_conn(conn: &Connection, schedule_id: &str, now: i64) -> Result<()> {
    let Some(spec) = get_spec_conn(conn, schedule_id)? else {
        conn.execute(
            "DELETE FROM schedule_cursors WHERE schedule_id = ?1",
            params![schedule_id],
        )?;
        return Ok(());
    };
    let last_fire = get_last_fire_conn(conn, schedule_id)?;
    let plan = planning::plan_schedule(&spec, last_fire.as_ref(), now);
    conn.execute(
        "INSERT INTO schedule_cursors
            (schedule_id, spec_hash, last_scheduled_at, next_fire_at,
             last_evaluated_at, updated_at)
         VALUES (?1,?2,?3,?4,?5,?6)
         ON CONFLICT(schedule_id) DO UPDATE SET
            spec_hash=excluded.spec_hash,
            last_scheduled_at=excluded.last_scheduled_at,
            next_fire_at=excluded.next_fire_at,
            last_evaluated_at=excluded.last_evaluated_at,
            updated_at=excluded.updated_at",
        params![
            spec.schedule_id,
            spec.spec_hash,
            plan.last_scheduled_at,
            plan.next_fire_at,
            now,
            now,
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_db() -> SchedulerDb {
        SchedulerDb::new_in_memory().expect("open current in-memory scheduler db")
    }

    fn fresh_test_db() -> SchedulerDb {
        SchedulerDb::open(&PathBuf::from(":memory:")).expect("open fresh scheduler db")
    }

    #[test]
    fn existing_current_open_never_creates_a_missing_scheduler_store() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("scheduler.sqlite3");
        assert!(SchedulerDb::open_existing_current(&path).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn existing_current_open_accepts_an_exact_owned_store() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("scheduler.sqlite3");
        let db = SchedulerDb::open(&path).unwrap();
        db.begin_fire_projection_rebuild().unwrap();
        db.finish_fire_projection_rebuild().unwrap();
        drop(db);
        SchedulerDb::open_existing_current(&path).unwrap();
    }

    #[test]
    fn existing_current_open_rejects_an_incomplete_fire_projection() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("scheduler.sqlite3");
        drop(SchedulerDb::open(&path).unwrap());
        assert!(SchedulerDb::open_existing_current(&path).is_err());
    }

    #[test]
    fn fresh_fire_projection_is_invalid_until_rebuild_publication() {
        let db = fresh_test_db();
        assert!(!db.fire_projection_is_current().unwrap());

        db.begin_fire_projection_rebuild().unwrap();
        assert!(!db.fire_projection_is_current().unwrap());

        db.finish_fire_projection_rebuild().unwrap();
        assert!(db.fire_projection_is_current().unwrap());
    }

    #[test]
    fn rebuild_normalizes_invalid_projection_marker_contents() {
        let db = test_db();
        db.lock()
            .unwrap()
            .execute(
                "INSERT INTO scheduler_projection_state
                    (projection_name, rebuild_complete) VALUES ('unexpected', 1)",
                [],
            )
            .unwrap();
        assert!(!db.fire_projection_is_current().unwrap());

        db.begin_fire_projection_rebuild().unwrap();
        db.finish_fire_projection_rebuild().unwrap();
        assert!(db.fire_projection_is_current().unwrap());
    }

    #[test]
    fn all_fire_history_discard_preserves_signed_schedule_projection() {
        let db = test_db();
        db.upsert_spec(&make_spec("sched")).unwrap();
        db.upsert_fire(&make_fire("sched", 1_000, "completed"))
            .unwrap();

        let preview = db.discard_fire_history(true).unwrap();
        assert_eq!(preview.fires, 1);
        assert_eq!(preview.outbox_snapshots, 1);
        assert_eq!(preview.cursors, 1);
        assert_eq!(db.discard_fire_history(true).unwrap().total_rows(), 3);

        let removed = db.discard_fire_history(false).unwrap();
        assert_eq!(removed.total_rows(), 3);
        assert!(db.get_spec("sched").unwrap().is_some());
        assert!(db.get_fire("sched@1000").unwrap().is_none());
        assert!(db.get_cursor("sched").unwrap().is_none());
        assert_eq!(db.pending_fire_outbox().unwrap(), 0);
        assert!(db.fire_projection_is_current().unwrap());
    }

    #[test]
    fn cursor_reconcile_repairs_only_projection_mismatches() {
        let db = test_db();
        let spec = make_spec("sched");
        db.upsert_spec(&spec).unwrap();
        db.upsert_fire(&make_fire("sched", 1_000, "completed"))
            .unwrap();

        assert_eq!(
            db.reconcile_cursors_for_specs(std::slice::from_ref(&spec), 2_000)
                .unwrap(),
            0
        );

        let mut stale = db.get_cursor("sched").unwrap().unwrap();
        stale.last_scheduled_at = None;
        db.upsert_cursor(&stale).unwrap();
        assert_eq!(db.reconcile_cursors_for_specs(&[spec], 2_000).unwrap(), 1);
        assert_eq!(
            db.get_cursor("sched").unwrap().unwrap().last_scheduled_at,
            Some(1_000)
        );
    }

    #[test]
    fn terminal_fire_retention_preserves_inflight_and_repairs_cursor_atomically() {
        let db = test_db();
        db.begin_fire_projection_rebuild().unwrap();
        db.finish_fire_projection_rebuild().unwrap();
        db.upsert_spec(&make_spec("sched")).unwrap();
        for scheduled_at in [1_000, 2_000, 3_000] {
            db.project_fire_from_journal(&make_fire("sched", scheduled_at, "completed"))
                .unwrap();
        }
        db.project_fire_from_journal(&make_fire("sched", 500, "dispatched"))
            .unwrap();

        db.begin_fire_retention().unwrap();
        let deleted = db
            .finish_fire_retention(&[ryeos_state::gc::retention::FireRetentionTarget {
                schedule_id: "sched".to_string(),
                fire_id: "sched@1000".to_string(),
            }])
            .unwrap();

        assert_eq!(deleted, 1);
        assert!(db.get_fire("sched@1000").unwrap().is_none());
        assert!(db.get_fire("sched@500").unwrap().is_some());
        assert_eq!(
            db.get_cursor("sched").unwrap().unwrap().last_scheduled_at,
            Some(3_000)
        );
        assert!(db.fire_projection_is_current().unwrap());
    }

    #[test]
    fn fire_retention_dry_verification_is_exact_and_mutation_free() {
        let db = test_db();
        db.begin_fire_projection_rebuild().unwrap();
        db.finish_fire_projection_rebuild().unwrap();
        for scheduled_at in [1_000, 2_000] {
            db.project_fire_from_journal(&make_fire("sched", scheduled_at, "completed"))
                .unwrap();
        }

        let old = ryeos_state::gc::retention::FireRetentionTarget {
            schedule_id: "sched".to_string(),
            fire_id: "sched@1000".to_string(),
        };
        db.verify_fire_retention_targets(std::slice::from_ref(&old))
            .unwrap();
        assert!(db.get_fire("sched@1000").unwrap().is_some());
        assert!(db.fire_projection_is_current().unwrap());

        assert!(db
            .verify_fire_retention_targets(&[ryeos_state::gc::retention::FireRetentionTarget {
                schedule_id: "sched".to_string(),
                fire_id: "sched@2000".to_string(),
            }])
            .is_err());
        assert!(db
            .verify_fire_retention_targets(&[ryeos_state::gc::retention::FireRetentionTarget {
                schedule_id: "sched".to_string(),
                fire_id: "sched@missing".to_string(),
            }])
            .is_err());
        assert!(db
            .verify_fire_retention_targets(&[old.clone(), old])
            .is_err());
    }

    #[test]
    fn fire_retention_refuses_an_undrained_outbox() {
        let db = test_db();
        db.begin_fire_projection_rebuild().unwrap();
        db.finish_fire_projection_rebuild().unwrap();
        db.upsert_spec(&make_spec("sched")).unwrap();
        db.upsert_fire(&make_fire("sched", 1_000, "completed"))
            .unwrap();

        assert_eq!(db.pending_fire_outbox().unwrap(), 1);
        assert!(db.begin_fire_retention().is_err());
        assert!(db.get_fire("sched@1000").unwrap().is_some());
        assert!(db.fire_projection_is_current().unwrap());
    }

    #[test]
    fn fire_outbox_drains_complete_snapshots_in_commit_order() {
        let db = test_db();
        let state = tempfile::tempdir().unwrap();
        let fire = make_fire("sched", 1_000, "dispatched");
        db.upsert_fire(&fire).unwrap();
        let completed = FireRecord {
            status: "completed".to_string(),
            completed_at: Some(2_000),
            outcome: Some("success".to_string()),
            ..fire
        };
        db.upsert_fire(&completed).unwrap();

        assert_eq!(db.pending_fire_outbox().unwrap(), 2);
        assert_eq!(db.drain_fire_outbox(state.path()).unwrap(), 2);
        assert_eq!(db.pending_fire_outbox().unwrap(), 0);
        let journal = std::fs::read_to_string(
            state
                .path()
                .join("schedules")
                .join("sched")
                .join("fires.jsonl"),
        )
        .unwrap();
        let snapshots = journal
            .lines()
            .map(|line| serde_json::from_str::<FireRecord>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(snapshots.len(), 2);
        assert_eq!(snapshots[0].status, "dispatched");
        assert_eq!(snapshots[1].status, "completed");
    }

    fn make_spec(id: &str) -> ScheduleSpecRecord {
        ScheduleSpecRecord {
            schedule_id: id.to_string(),
            item_ref: "directive:test".to_string(),
            ref_bindings: std::collections::BTreeMap::new(),
            params: r#"{"key":"value"}"#.to_string(),
            schedule_type: "interval".to_string(),
            expression: "60".to_string(),
            timezone: "UTC".to_string(),
            misfire_policy: "skip".to_string(),
            overlap_policy: "skip".to_string(),
            lateness_grace_secs: 60,
            enabled: true,
            project_root: None,
            signer_fingerprint: "11".repeat(32),
            spec_hash: "22".repeat(32),
            registered_at: 1000,
            requester_fingerprint: "fp:test".to_string(),
            capabilities: vec!["ryeos.execute.*".to_string()],
        }
    }

    fn make_fire(schedule_id: &str, scheduled_at: i64, status: &str) -> FireRecord {
        let fire_id = format!("{}@{}", schedule_id, scheduled_at);
        let terminal = status != "dispatched";
        FireRecord {
            thread_id: (status != "skipped").then(|| crate::types::thread_id_from_fire(&fire_id)),
            fire_id,
            schedule_id: schedule_id.to_string(),
            scheduled_at,
            fired_at: Some(scheduled_at),
            completed_at: terminal.then_some(scheduled_at + 1),
            status: status.to_string(),
            trigger_reason: "normal".to_string(),
            outcome: terminal.then(|| match status {
                "completed" => "success".to_string(),
                "cancelled" => "thread_cancelled".to_string(),
                "failed" => "thread_failed".to_string(),
                _ => "normal".to_string(),
            }),
            signer_fingerprint: "11".repeat(32),
        }
    }

    #[test]
    fn open_replaces_recognized_stale_owned_projection_without_reading_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scheduler.sqlite3");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                r#"
                CREATE TABLE schedule_specs (
                    schedule_id          TEXT PRIMARY KEY,
                    item_ref             TEXT NOT NULL,
                    params               TEXT NOT NULL DEFAULT '{}',
                    schedule_type        TEXT NOT NULL,
                    expression           TEXT NOT NULL,
                    timezone             TEXT NOT NULL DEFAULT 'UTC',
                    misfire_policy       TEXT NOT NULL DEFAULT 'skip',
                    overlap_policy       TEXT NOT NULL DEFAULT 'skip',
                    enabled              INTEGER NOT NULL DEFAULT 1,
                    project_root         TEXT,
                    signer_fingerprint   TEXT NOT NULL,
                    spec_hash            TEXT NOT NULL,
                    registered_at        INTEGER NOT NULL,
                    requester_fingerprint TEXT NOT NULL DEFAULT '',
                    capabilities          TEXT NOT NULL DEFAULT '[]'
                );
                CREATE TABLE schedule_fires (
                    fire_id            TEXT PRIMARY KEY,
                    schedule_id        TEXT NOT NULL,
                    scheduled_at       INTEGER NOT NULL,
                    fired_at           INTEGER,
                    thread_id          TEXT,
                    status             TEXT NOT NULL,
                    trigger_reason     TEXT NOT NULL DEFAULT 'normal',
                    outcome            TEXT,
                    signer_fingerprint TEXT NOT NULL
                );
                CREATE INDEX idx_fires_schedule_id ON schedule_fires(schedule_id);
                CREATE INDEX idx_fires_status ON schedule_fires(status);
                CREATE INDEX idx_fires_schedule_scheduled ON schedule_fires(schedule_id, scheduled_at DESC);
                "#,
            )
            .unwrap();
            conn.execute_batch(&format!("PRAGMA application_id = {};", SCHEDULER_APP_ID))
                .unwrap();
            conn.execute(
                "INSERT INTO schedule_specs
                 (schedule_id, item_ref, params, schedule_type, expression, timezone,
                  misfire_policy, overlap_policy, enabled, project_root, signer_fingerprint,
                  spec_hash, registered_at, requester_fingerprint, capabilities)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
                params![
                    "old-sched",
                    "directive:test",
                    "{}",
                    "interval",
                    "60",
                    "UTC",
                    "skip",
                    "skip",
                    1,
                    Option::<String>::None,
                    "fp:test",
                    "hash",
                    1000,
                    "fp:test",
                    r#"["ryeos.execute.*"]"#,
                ],
            )
            .unwrap();
        }

        let db = SchedulerDb::open(&path).unwrap();
        assert!(db.get_spec("old-sched").unwrap().is_none());
        assert!(!db.fire_projection_is_current().unwrap());
    }

    // ── Spec CRUD ──────────────────────────────────────────────

    #[test]
    fn upsert_and_get_spec() {
        let db = test_db();
        let spec = make_spec("test-sched");
        db.upsert_spec(&spec).unwrap();

        let got = db.get_spec("test-sched").unwrap().unwrap();
        assert_eq!(got.schedule_id, "test-sched");
        assert_eq!(got.item_ref, "directive:test");
        assert_eq!(got.expression, "60");
        assert!(got.enabled);
    }

    #[test]
    fn get_spec_nonexistent() {
        let db = test_db();
        assert!(db.get_spec("nope").unwrap().is_none());
    }

    #[test]
    fn upsert_spec_updates_existing() {
        let db = test_db();
        let mut spec = make_spec("sched");
        spec.expression = "30".to_string();
        db.upsert_spec(&spec).unwrap();

        let mut spec2 = make_spec("sched");
        spec2.expression = "120".to_string();
        db.upsert_spec(&spec2).unwrap();

        let got = db.get_spec("sched").unwrap().unwrap();
        assert_eq!(got.expression, "120");
    }

    #[test]
    fn delete_spec() {
        let db = test_db();
        db.upsert_spec(&make_spec("sched")).unwrap();
        assert!(db.delete_spec("sched").unwrap());
        assert!(db.get_spec("sched").unwrap().is_none());
    }

    #[test]
    fn delete_spec_nonexistent() {
        let db = test_db();
        assert!(!db.delete_spec("nope").unwrap());
    }

    #[test]
    fn load_enabled_specs() {
        let db = test_db();
        db.upsert_spec(&make_spec("a")).unwrap();
        db.upsert_spec(&make_spec("b")).unwrap();

        let mut spec_c = make_spec("c");
        spec_c.enabled = false;
        db.upsert_spec(&spec_c).unwrap();

        let specs = db.load_enabled_specs().unwrap();
        assert_eq!(specs.len(), 2);
        let ids: Vec<&str> = specs.iter().map(|s| s.schedule_id.as_str()).collect();
        assert!(ids.contains(&"a"));
        assert!(ids.contains(&"b"));
        assert!(!ids.contains(&"c"));
    }

    #[test]
    fn list_specs_with_type_filter() {
        let db = test_db();
        let mut cron_spec = make_spec("cron-sched");
        cron_spec.schedule_type = "cron".to_string();
        cron_spec.expression = "* * * * * *".to_string();
        db.upsert_spec(&cron_spec).unwrap();
        db.upsert_spec(&make_spec("interval-sched")).unwrap();

        let specs = db.list_specs(false, Some("cron")).unwrap();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].schedule_type, "cron");
    }

    #[test]
    fn list_specs_enabled_only() {
        let db = test_db();
        db.upsert_spec(&make_spec("enabled")).unwrap();
        let mut disabled = make_spec("disabled");
        disabled.enabled = false;
        db.upsert_spec(&disabled).unwrap();

        let specs = db.list_specs(true, None).unwrap();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].schedule_id, "enabled");
    }

    // ── Fire CRUD ──────────────────────────────────────────────

    #[test]
    fn upsert_and_get_fire() {
        let db = test_db();
        let fire = make_fire("sched", 1000, "dispatched");
        db.upsert_fire(&fire).unwrap();

        let got = db.get_fire("sched@1000").unwrap().unwrap();
        assert_eq!(got.fire_id, "sched@1000");
        assert_eq!(got.status, "dispatched");
    }

    #[test]
    fn upsert_fire_rejects_empty_signer_fingerprint() {
        let db = test_db();
        let mut fire = make_fire("sched", 1000, "dispatched");
        fire.signer_fingerprint = "  ".to_string();

        let error = db.upsert_fire(&fire).unwrap_err();
        assert!(error
            .to_string()
            .contains("signer_fingerprint must not be empty"));
        assert!(db.get_fire("sched@1000").unwrap().is_none());
    }

    #[test]
    fn get_fire_nonexistent() {
        let db = test_db();
        assert!(db.get_fire("nope").unwrap().is_none());
    }

    #[test]
    fn upsert_fire_updates_status() {
        let db = test_db();
        let fire = make_fire("sched", 1000, "dispatched");
        db.upsert_fire(&fire).unwrap();

        let updated = FireRecord {
            status: "completed".to_string(),
            outcome: Some("success".to_string()),
            ..fire
        };
        db.upsert_fire(&updated).unwrap();

        let got = db.get_fire("sched@1000").unwrap().unwrap();
        assert_eq!(got.status, "completed");
        assert_eq!(got.outcome.unwrap(), "success");
    }

    #[test]
    fn get_last_fire() {
        let db = test_db();
        db.upsert_fire(&make_fire("sched", 1000, "completed"))
            .unwrap();
        db.upsert_fire(&make_fire("sched", 2000, "dispatched"))
            .unwrap();

        let last = db.get_last_fire("sched").unwrap().unwrap();
        assert_eq!(last.scheduled_at, 2000);
    }

    #[test]
    fn get_last_fire_empty() {
        let db = test_db();
        assert!(db.get_last_fire("sched").unwrap().is_none());
    }

    #[test]
    fn rebuild_cursors_for_specs_uses_latest_scheduled_boundary() {
        let db = test_db();
        let spec = make_spec("sched");
        db.upsert_spec(&spec).unwrap();
        db.upsert_fire(&make_fire("sched", 61_000, "completed"))
            .unwrap();
        db.upsert_fire(&make_fire("sched", 121_000, "skipped"))
            .unwrap();

        db.rebuild_cursors_for_specs(&[spec], 122_000).unwrap();
        let cursor = db.get_cursor("sched").unwrap().unwrap();

        assert_eq!(cursor.spec_hash, "abc123");
        assert_eq!(cursor.last_scheduled_at, Some(121_000));
        assert_eq!(cursor.next_fire_at, Some(181_000));
        assert_eq!(cursor.last_evaluated_at, Some(122_000));
        assert_eq!(cursor.updated_at, 122_000);
    }

    #[test]
    fn get_inflight_fires() {
        let db = test_db();
        db.upsert_fire(&make_fire("sched", 1000, "completed"))
            .unwrap();
        db.upsert_fire(&make_fire("sched", 2000, "dispatched"))
            .unwrap();
        db.upsert_fire(&make_fire("other", 3000, "dispatched"))
            .unwrap();

        let inflight = db.get_inflight_fires().unwrap();
        assert_eq!(inflight.len(), 2);
    }

    #[test]
    fn get_inflight_for_schedule() {
        let db = test_db();
        db.upsert_fire(&make_fire("sched", 1000, "completed"))
            .unwrap();
        db.upsert_fire(&make_fire("sched", 2000, "dispatched"))
            .unwrap();
        db.upsert_fire(&make_fire("other", 3000, "dispatched"))
            .unwrap();

        let inflight = db.get_inflight_for_schedule("sched").unwrap().unwrap();
        assert_eq!(inflight.scheduled_at, 2000);
    }

    #[test]
    fn get_inflight_for_schedule_none() {
        let db = test_db();
        assert!(db.get_inflight_for_schedule("sched").unwrap().is_none());
    }

    #[test]
    fn find_fire_by_thread() {
        let db = test_db();
        let fire = make_fire("sched", 1000, "dispatched");
        db.upsert_fire(&fire).unwrap();

        let found = db
            .find_fire_by_thread(fire.thread_id.as_ref().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(found.fire_id, "sched@1000");
    }

    #[test]
    fn find_fire_by_thread_completed_not_found() {
        let db = test_db();
        let fire = make_fire("sched", 1000, "completed");
        db.upsert_fire(&fire).unwrap();

        let found = db
            .find_fire_by_thread(fire.thread_id.as_ref().unwrap())
            .unwrap();
        assert!(found.is_none());
    }

    #[test]
    fn explicit_schedule_fire_purge_is_marker_journaled() {
        let db = test_db();
        db.begin_fire_projection_rebuild().unwrap();
        db.finish_fire_projection_rebuild().unwrap();
        db.project_fire_from_journal(&make_fire("sched", 1000, "completed"))
            .unwrap();
        db.project_fire_from_journal(&make_fire("sched", 2000, "dispatched"))
            .unwrap();
        db.project_fire_from_journal(&make_fire("other", 3000, "dispatched"))
            .unwrap();

        db.begin_fire_retention().unwrap();
        assert!(!db.fire_projection_is_current().unwrap());
        let removed = db.finish_schedule_fire_purge("sched").unwrap();
        assert_eq!(removed, 2);
        assert!(db.get_fire("sched@1000").unwrap().is_none());
        assert!(db.get_fire("other@3000").unwrap().is_some());
        assert!(db.fire_projection_is_current().unwrap());
    }

    #[test]
    fn list_fires_with_status_filter() {
        let db = test_db();
        db.upsert_fire(&make_fire("sched", 1000, "completed"))
            .unwrap();
        db.upsert_fire(&make_fire("sched", 2000, "dispatched"))
            .unwrap();
        db.upsert_fire(&make_fire("sched", 3000, "completed"))
            .unwrap();

        let (fires, total) = db.list_fires("sched", Some("completed"), 10).unwrap();
        assert_eq!(total, 2);
        assert_eq!(fires.len(), 2);
    }

    #[test]
    fn list_fires_with_limit() {
        let db = test_db();
        db.upsert_fire(&make_fire("sched", 1000, "completed"))
            .unwrap();
        db.upsert_fire(&make_fire("sched", 2000, "completed"))
            .unwrap();
        db.upsert_fire(&make_fire("sched", 3000, "completed"))
            .unwrap();

        let (fires, total) = db.list_fires("sched", None, 2).unwrap();
        assert_eq!(total, 3);
        assert_eq!(fires.len(), 2);
        assert_eq!(fires[0].scheduled_at, 3000);
        assert_eq!(fires[1].scheduled_at, 2000);
    }

    #[test]
    fn list_fires_empty() {
        let db = test_db();
        let (fires, total) = db.list_fires("sched", None, 10).unwrap();
        assert_eq!(total, 0);
        assert!(fires.is_empty());
    }

    // ── claim_fire / reclaim_fire ──────────────────────────────────

    #[test]
    fn claim_fire_inserts_new() {
        let db = test_db();
        let fire = make_fire("sched", 1000, "dispatched");
        let claimed = db.claim_fire(&fire).unwrap();
        assert!(claimed, "first claim should succeed");
    }

    #[test]
    fn claim_fire_rejects_duplicate() {
        let db = test_db();
        let fire = make_fire("sched", 1000, "dispatched");
        db.claim_fire(&fire).unwrap();
        let claimed = db.claim_fire(&fire).unwrap();
        assert!(!claimed, "second claim of same fire_id should return false");
    }

    #[test]
    fn reclaim_fire_succeeds_for_dispatched_no_thread() {
        let db = test_db();
        // Insert a fire with dispatched status and no thread_id
        let mut fire = make_fire("sched", 1000, "dispatched");
        fire.thread_id = None;
        db.upsert_fire(&fire).unwrap();

        let reclaimed = db.reclaim_fire("sched@1000").unwrap();
        assert!(
            reclaimed,
            "dispatched fire with no thread should be reclaimable"
        );
    }

    #[test]
    fn reclaim_fire_succeeds_for_dispatched_with_thread() {
        let db = test_db();
        let fire = make_fire("sched", 1000, "dispatched");
        db.upsert_fire(&fire).unwrap();

        // Thread existence is checked by the caller (reconciler), not by reclaim_fire.
        // reclaim_fire clears the old execution id so redispatch mints the current
        // canonical thread id for this fire.
        let reclaimed = db.reclaim_fire("sched@1000").unwrap();
        assert!(
            reclaimed,
            "dispatched fire should be reclaimable even with thread_id set"
        );
        let reclaimed_fire = db.get_fire("sched@1000").unwrap().unwrap();
        assert_eq!(reclaimed_fire.thread_id, None);
    }

    #[test]
    fn reclaim_fire_rejects_completed() {
        let db = test_db();
        let fire = make_fire("sched", 1000, "completed");
        db.upsert_fire(&fire).unwrap();

        let reclaimed = db.reclaim_fire("sched@1000").unwrap();
        assert!(!reclaimed, "completed fire should not be reclaimable");
    }

    #[test]
    fn reclaim_fire_rejects_nonexistent() {
        let db = test_db();
        let reclaimed = db.reclaim_fire("nope@9999").unwrap();
        assert!(!reclaimed, "nonexistent fire should not be reclaimable");
    }

    #[test]
    fn find_stale_dispatched_fires() {
        let db = test_db();

        // Create a dispatched fire with fired_at in the past
        let mut old = make_fire("sched", 1000, "dispatched");
        old.fired_at = Some(lillux::time::timestamp_millis() - 60_000); // 60s ago
        db.upsert_fire(&old).unwrap();

        // Create a recent dispatched fire (should not be stale)
        let mut recent = make_fire("sched", 2000, "dispatched");
        recent.fired_at = Some(lillux::time::timestamp_millis());
        db.upsert_fire(&recent).unwrap();

        // Create a completed fire (should not show up even if old)
        let mut done = make_fire("sched", 3000, "completed");
        done.fired_at = Some(lillux::time::timestamp_millis() - 120_000);
        db.upsert_fire(&done).unwrap();

        let stale = db.find_stale_dispatched_fires(30).unwrap();
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].fire_id, "sched@1000");
    }
}
