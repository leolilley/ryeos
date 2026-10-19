//! Scheduler SQLite database — `scheduler.sqlite3`.
//!
//! Separate from the selected thread-projection instance (which has strict
//! schema validation and its own recovery-generation pointer).
//! Own `application_id`, own schema, own rebuild path.

use lillux::PinnedDirectory;
use lillux::custody::Held;
use lillux_sqlite::{
    BoundSqliteOrigin, OriginalNativeBoundInputs, OriginalNativeConnection,
    OriginalNativeConnectionTerminal, OriginalNativeDomain, OriginalNativeFactory,
    OriginalNativeOpenFailure, OriginalNativeCloseFailure, OriginalNativeTransientInputs,
    OriginalSchedulerRetirement, OriginalSchedulerRetirementFailure, OriginalSchedulerRetirementInputs, CapturedSqliteRoleFacts, DrainedSqliteProjection,
    DrainedSqliteRoleFacts, NativeRetentionReason, SqliteProjectionRetirementFailure, SqliteRole,
    SqliteRoleMutationError, SqliteRoleMutationOutcome,
};
use std::{path::Path, sync::Arc};

use anyhow::{Context, Result, anyhow};
use rusqlite::{Connection, OptionalExtension, params};
use ryeos_scheduler_contract::{
    SchedulerFireHistoryAccounting, SchedulerFireHistoryDiscardReport, validate_schedule_id,
};

use super::planning;
use super::types::{
    FireRecord, ScheduleCursorRecord, ScheduleSpecRecord, validate_schedule_spec_record,
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
    execution             TEXT NOT NULL,
    lateness_grace_secs   INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS schedule_fires (
    fire_id            TEXT PRIMARY KEY,
    schedule_id        TEXT NOT NULL,
    scheduled_at       INTEGER NOT NULL,
    reserved_at        INTEGER NOT NULL,
    dispatched_at      INTEGER,
    completed_at       INTEGER,
    thread_id          TEXT,
    status             TEXT NOT NULL,
    trigger_reason     TEXT NOT NULL,
    outcome            TEXT,
    signer_fingerprint TEXT NOT NULL,
    schedule_spec_hash TEXT NOT NULL,
    project_authority  TEXT,
    admitted_capsule_hash TEXT
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
                        name: "execution",
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
                        name: "reserved_at",
                        col_type: "INTEGER",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "dispatched_at",
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
                    sqlite_schema::ColumnSpec {
                        name: "schedule_spec_hash",
                        col_type: "TEXT",
                        pk: false,
                        not_null: true,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "project_authority",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
                    },
                    sqlite_schema::ColumnSpec {
                        name: "admitted_capsule_hash",
                        col_type: "TEXT",
                        pk: false,
                        not_null: false,
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

fn assert_recognized_stale_scheduler_projection(conn: &Connection, path: &Path) -> Result<()> {
    let unsupported: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type IN ('view', 'trigger')",
        [],
        |row| row.get(0),
    )?;
    if unsupported != 0 {
        anyhow::bail!(
            "refusing to replace scheduler database with views or triggers at {}",
            path.display()
        );
    }
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

// ── Database wrapper ────────────────────────────────────────────────

/// The scheduler inspection's native owner after a refusal: the refused
/// acquisition, the refused close, or the terminal a same-owner retry reached.
enum InspectionCustody {
    Open(OriginalNativeOpenFailure),
    Close(OriginalNativeCloseFailure),
    Terminal(OriginalNativeConnectionTerminal),
}
/// Refused acquisition or close of a scheduler inspection copy on its native
/// ticket. The SAME native owner stays here until a same-owner retry reaches
/// its terminal, which then leaves once for the caller's census.
pub struct SchedulerInspectionCloseFailure {
    custody: std::sync::Mutex<Option<InspectionCustody>>,
    error: String,
    inspection_error: Option<anyhow::Error>,
}
impl SchedulerInspectionCloseFailure {
    /// Retry the close of the same native owner; never reopens.
    pub fn retry_close(&self) -> Result<()> {
        let mut custody = self
            .custody
            .lock()
            .map_err(|_| anyhow!("inspection close lock poisoned"))?;
        let result = match custody.take() {
            Some(InspectionCustody::Open(mut owner)) => {
                if owner.retained_admission().is_some() && owner.recover_retained_admission().is_err() {
                    Err(InspectionCustody::Open(owner))
                } else {
                    owner
                        .retry_close()
                        .map(OriginalNativeConnectionTerminal::Released)
                        .map_err(InspectionCustody::Open)
                }
            }
            Some(InspectionCustody::Close(owner)) => owner
                .retry()
                .map(OriginalNativeConnectionTerminal::Closed)
                .map_err(InspectionCustody::Close),
            Some(InspectionCustody::Terminal(terminal)) => {
                *custody = Some(InspectionCustody::Terminal(terminal));
                return Ok(());
            }
            None => anyhow::bail!("inspection terminal already transferred"),
        };
        match result {
            Ok(terminal) => {
                *custody = Some(InspectionCustody::Terminal(terminal));
                Ok(())
            }
            Err(owner) => {
                let error = match &owner {
                    InspectionCustody::Open(failure) => format!("{failure}"),
                    InspectionCustody::Close(failure) => format!("{failure}"),
                    InspectionCustody::Terminal(_) => unreachable!("only refused custody here"),
                };
                *custody = Some(owner);
                anyhow::bail!("inspection still retains same native copy owner: {error}")
            }
        }
    }
    /// Transfer the terminal once, after a same-owner retry reached it. A
    /// refused extraction keeps the SAME pending native owner.
    pub fn take_terminal(&self) -> Result<OriginalNativeConnectionTerminal> {
        let mut custody = self
            .custody
            .lock()
            .map_err(|_| anyhow!("inspection close lock poisoned"))?;
        match custody.take() {
            Some(InspectionCustody::Terminal(terminal)) => Ok(terminal),
            pending => {
                *custody = pending;
                anyhow::bail!("inspection has no retained native terminal")
            }
        }
    }
}
impl std::fmt::Debug for SchedulerInspectionCloseFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SchedulerInspectionCloseFailure")
            .field("error", &self.error)
            .field("inspection_error", &self.inspection_error)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Display for SchedulerInspectionCloseFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "scheduler copy owner retained: {}", self.error)
    }
}
impl std::error::Error for SchedulerInspectionCloseFailure {}
fn inspect_history_reset_conn(
    conn: &Connection,
    path: &Path,
    reference: &sqlite_schema::SchemaSqlDefinition,
) -> Result<SchedulerFireHistoryAccounting> {
    let integrity: String = conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        anyhow::bail!("scheduler database integrity check failed: {integrity}");
    }
    let spec = scheduler_schema_spec();
    let app_id: i32 = conn.query_row("PRAGMA application_id", [], |row| row.get(0))?;
    if app_id != spec.application_id {
        anyhow::bail!("foreign scheduler database application_id {app_id}");
    }
    let exact = sqlite_schema::assert_owned(conn, &spec, path)
        .and_then(|_| reference.require_matches(conn, path));
    if exact.is_err() {
        assert_recognized_stale_scheduler_projection(conn, path)?;
        return Ok(SchedulerFireHistoryAccounting::UnavailableIncompatibleSchema {});
    }
    fn count(conn: &Connection, table: &str) -> Result<usize> {
        let count: i64 = conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })?;
        usize::try_from(count).context("scheduler fire-history row count is invalid")
    }
    Ok(SchedulerFireHistoryAccounting::Exact {
        counts: SchedulerFireHistoryDiscardReport {
            fires: count(conn, "schedule_fires")?,
            outbox_snapshots: count(conn, "schedule_fire_outbox")?,
            cursors: count(conn, "schedule_cursors")?,
        },
    })
}

enum RetirementPhase {
    Drained(DrainedSqliteProjection),
    Failed(SqliteProjectionRetirementFailure),
}
enum NativeRetirementPhase {
    Owned(OriginalSchedulerRetirement),
    Failed(OriginalSchedulerRetirementFailure),
}
enum DbOwner {
    // An effect has exclusive custody outside the slot; never disposal proof.
    CheckedOut,
    BeforeOpen(OriginalNativeBoundInputs),
    NeverOpened(OriginalNativeBoundInputs),
    Live(OriginalNativeConnection),
    Stale(OriginalNativeConnection),
    Opening(OriginalNativeOpenFailure),
    Closing(OriginalNativeCloseFailure),
    Terminal(OriginalNativeConnectionTerminal),
    NativeRetirement { generation: [u8; 16], phase: NativeRetirementPhase },
    // Authenticated SQL-free stopped/offline source; no live native ticket.
    Retirement { generation: [u8; 16], phase: RetirementPhase },
    Closed,
}

/// A scope that ended in refusal with its transaction still open leaves the
/// live connection unusable for service. Close it in place now, which rolls
/// the open transaction back; a refused close keeps its owner for the
/// ordinary close retry.
fn close_if_transaction_retained(owner: &mut DbOwner) {
    if !matches!(&*owner, DbOwner::Live(live) if live.transaction_retained()) {
        return;
    }
    let DbOwner::Live(live) = std::mem::replace(owner, DbOwner::CheckedOut) else {
        unreachable!("checked live owner above")
    };
    *owner = match live.close() {
        Ok(closed) => DbOwner::Terminal(OriginalNativeConnectionTerminal::Closed(closed)),
        Err(failure) => DbOwner::Closing(failure),
    };
}
thread_local! {
    /// Stores whose SQL owner this thread currently holds.
    static SQL_SCOPES: std::cell::RefCell<Vec<usize>> = const { std::cell::RefCell::new(Vec::new()) };
}
/// Records that this thread is inside one store's SQL scope, so a nested
/// scope on the same store is refused before it would wait on itself.
struct SqlScopeMark {
    store: usize,
}
impl SqlScopeMark {
    fn enter(db: &SchedulerDb) -> Result<Self> {
        let store = std::ptr::from_ref(db) as usize;
        SQL_SCOPES.with(|scopes| {
            let mut scopes = scopes.borrow_mut();
            anyhow::ensure!(
                !scopes.contains(&store),
                "scheduler SQL scope cannot re-enter its own store"
            );
            scopes.push(store);
            Ok(Self { store })
        })
    }
}
impl Drop for SqlScopeMark {
    fn drop(&mut self) {
        SQL_SCOPES.with(|scopes| {
            let mut scopes = scopes.borrow_mut();
            if let Some(position) = scopes.iter().rposition(|store| *store == self.store) {
                scopes.remove(position);
            }
        });
    }
}
/// One thread's hold on a store's SQL owner. The guard releases before
/// the mark, so the store is free by the time the thread may enter again.
struct SqlOwnerScope<'a> {
    guard: std::sync::MutexGuard<'a, DbOwner>,
    _mark: SqlScopeMark,
}
struct OriginalSchedulerContext {
    factory: Arc<OriginalNativeFactory>,
    reference: sqlite_schema::ClosedNativeSchemaSqlReference,
}
/// The parts of a store that stay in custody together, in the order they end.
struct SchedulerDbParts {
    inner: std::sync::Mutex<DbOwner>,
    original: Option<OriginalSchedulerContext>,
}
/// Settled exactly when a stopped/offline retirement has released or aborted
/// its source: no original native context and no owner left in the slot. A
/// live store settles only through its consuming transfers, so any other
/// phase, a checked-out slot included, stays retained. A slot a thread still
/// holds is never settled.
fn scheduler_db_settled(parts: &SchedulerDbParts) -> bool {
    let closed = match parts.inner.try_lock() {
        Ok(owner) => matches!(*owner, DbOwner::Closed),
        Err(std::sync::TryLockError::Poisoned(poisoned)) => {
            matches!(*poisoned.into_inner(), DbOwner::Closed)
        }
        Err(std::sync::TryLockError::WouldBlock) => false,
    };
    closed && parts.original.is_none()
}
/// Dropping a store never closes or disposes its owners, in this process or
/// a forked copy: native and stopped/offline owners end only through their
/// explicit close or release and the factory census. A store dropped while
/// it still holds any of them keeps them retained until the process exits.
pub struct SchedulerDb {
    held: Held<SchedulerDbParts>,
    outbox_drain: std::sync::Mutex<()>,
}
impl SchedulerDb {
    fn from_parts(inner: DbOwner, original: Option<OriginalSchedulerContext>) -> Self {
        Self {
            held: Held::new(
                "ryeos-scheduler::store",
                SchedulerDbParts { inner: std::sync::Mutex::new(inner), original },
                scheduler_db_settled,
            ),
            outbox_drain: std::sync::Mutex::new(()),
        }
    }
}

/// The exact stopped live Scheduler owner after its database terminal has
/// joined the native acknowledgement. This transfers its real DB and schema
/// owners to the node finalizer; dropping it does not close or settle them.
pub struct SchedulerNativeFinalizationOwner {
    held: Held<SchedulerNativeFinalizationParts>,
    outside_handoff_id: Option<SchedulerNativeStoreHandoffId>,
}
/// Everything a finalization owner retains, in the order it ends.
struct SchedulerNativeFinalizationParts {
    inner: DbOwner,
    factory: Option<Arc<OriginalNativeFactory>>,
    schema_reference: Option<sqlite_schema::ClosedNativeSchemaSqlReference>,
    database_outside: Option<lillux_sqlite::OriginalNativeOutsideConnectionParts>,
    database_outside_custody: Option<lillux_sqlite::OriginalNativeOutsideConnectionCustody>,
    database_never_opened: Option<OriginalNativeBoundInputs>,
    database_refusal: Option<lillux_sqlite::OriginalNativeOutsideCustodyFailure>,
    schema_outside: Option<lillux_sqlite::OriginalNativeOutsideConnectionParts>,
    schema_outside_custody: Option<lillux_sqlite::OriginalNativeOutsideConnectionCustody>,
    schema_refusal: Option<sqlite_schema::NativeSchemaSqlReferenceOutsideFailure>,
}
impl SchedulerNativeFinalizationParts {
    /// Whether no outside source, custody packet, refusal, factory alias or
    /// schema reference remains.
    fn outside_sources_cleared(&self) -> bool {
        self.factory.is_none()
            && self.schema_reference.is_none()
            && self.database_outside.is_none()
            && self.database_outside_custody.is_none()
            && self.database_never_opened.is_none()
            && self.database_refusal.is_none()
            && self.schema_outside.is_none()
            && self.schema_outside_custody.is_none()
            && self.schema_refusal.is_none()
    }

    /// Settled exactly when finish_outside_handoff would accept this owner:
    /// every source, custody packet and refusal has transferred and the
    /// database phase slot is checked out. Otherwise the database phase,
    /// factory alias, schema reference and every outside source or refusal
    /// stay retained until the process exits.
    fn settled(&self) -> bool {
        self.outside_sources_cleared() && matches!(self.inner, DbOwner::CheckedOut)
    }
}
fn scheduler_native_finalization_settled(parts: &SchedulerNativeFinalizationParts) -> bool {
    parts.settled()
}

/// Opaque identity shared by one Scheduler finalization owner and the exact
/// Store source progress that accepts its database and schema owners.
#[derive(Clone)]
pub struct SchedulerNativeStoreHandoffId {
    process: u32,
    identity: Arc<()>,
}

impl SchedulerNativeStoreHandoffId {
    fn new(process: u32) -> Self {
        Self { process, identity: Arc::new(()) }
    }

    pub fn same_handoff(&self, other: &Self) -> bool {
        self.process == other.process && Arc::ptr_eq(&self.identity, &other.identity)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SchedulerNativeStoreSourceRole {
    Database,
    Schema,
}

/// Store-issued proof that this owner's actual database and schema source
/// frames were both accepted by the same retained Store outside graph.
pub struct SchedulerNativeStoreHandoffReceipt<'store> {
    id: SchedulerNativeStoreHandoffId,
    _store: std::marker::PhantomData<&'store ()>,
}

impl<'store> SchedulerNativeStoreHandoffReceipt<'store> {
    /// # Safety
    /// The borrowed value must be the exact live Store outside owner whose
    /// retained source graph was verified for this handoff. This lifetime
    /// keeps that Store owner borrowed until the affine receipt is consumed by
    /// `finish_outside_handoff`; the caller must also have verified that the
    /// handoff id labels one retained database source (opened or never-opened)
    /// and one retained schema source in that graph, with associated failed-
    /// admission and unused schema inputs joined.
    pub unsafe fn from_store_source_join<Store: ?Sized>(
        _store: &'store Store,
        id: SchedulerNativeStoreHandoffId,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(id.process == std::process::id(), "foreign Scheduler Store handoff proof");
        Ok(SchedulerNativeStoreHandoffReceipt {
            id,
            _store: std::marker::PhantomData,
        })
    }
}

/// Exact outside source phases extracted from one selected Scheduler. These
/// remain affine owners until the node finalizer joins them to the Store's
/// original physical inventory. No drop settles a database source: outside
/// its handoff wrapper it always holds the closed connection's parts or the
/// never-opened inputs, which only the Store progress accepts, so a dropped
/// source stays retained until the process exits and a copy in another
/// process is relinquished.
pub struct SchedulerNativeDatabaseSource {
    held: Held<SchedulerNativeDatabaseSourceParts>,
}
/// The one database source payload a source holds.
enum SchedulerNativeDatabaseSourceParts {
    Connection(lillux_sqlite::OriginalNativeOutsideConnectionParts),
    NeverOpened(OriginalNativeBoundInputs),
}
/// The payload of one database source moved out of its retaining owner,
/// for the Store progress that accepts it or hands it back.
pub enum TakenSchedulerNativeDatabaseSource {
    Connection(lillux_sqlite::OriginalNativeOutsideConnectionParts),
    NeverOpened(OriginalNativeBoundInputs),
}
impl SchedulerNativeDatabaseSource {
    const KIND: &'static str = "ryeos-scheduler::native-database-source";

    /// A source holding a closed connection's parts.
    pub fn from_connection(parts: lillux_sqlite::OriginalNativeOutsideConnectionParts) -> Self {
        Self {
            held: Held::until_taken(
                Self::KIND,
                SchedulerNativeDatabaseSourceParts::Connection(parts),
            ),
        }
    }

    /// A source holding the inputs of a database that never opened.
    pub fn from_never_opened(inputs: OriginalNativeBoundInputs) -> Self {
        Self {
            held: Held::until_taken(
                Self::KIND,
                SchedulerNativeDatabaseSourceParts::NeverOpened(inputs),
            ),
        }
    }

    /// Move the payload out once. A copy in another process is refused and
    /// comes back still held.
    pub fn into_taken(self) -> Result<TakenSchedulerNativeDatabaseSource, Self> {
        match self.held.take_whole() {
            Ok(SchedulerNativeDatabaseSourceParts::Connection(parts)) => {
                Ok(TakenSchedulerNativeDatabaseSource::Connection(parts))
            }
            Ok(SchedulerNativeDatabaseSourceParts::NeverOpened(inputs)) => {
                Ok(TakenSchedulerNativeDatabaseSource::NeverOpened(inputs))
            }
            Err(held) => Err(Self { held }),
        }
    }
}

#[must_use = "Scheduler sources must remain in custody until Store acceptance is proved"]
pub struct SchedulerNativeOutsideSources {
    handoff_id: SchedulerNativeStoreHandoffId,
    held: Held<SchedulerNativeOutsideSourceParts>,
}
/// The two source slots, in the order they end.
struct SchedulerNativeOutsideSourceParts {
    database: Option<SchedulerNativeDatabaseSource>,
    schema: Option<lillux_sqlite::OriginalNativeOutsideConnectionParts>,
}
/// Settled exactly when both sources were accepted by the Store progress and
/// never restored. A source still in a slot stays retained until the process
/// exits.
fn scheduler_native_outside_sources_settled(parts: &SchedulerNativeOutsideSourceParts) -> bool {
    parts.database.is_none() && parts.schema.is_none()
}

impl SchedulerNativeOutsideSources {
    fn new(
        handoff_id: SchedulerNativeStoreHandoffId,
        database: Option<SchedulerNativeDatabaseSource>,
        schema: Option<lillux_sqlite::OriginalNativeOutsideConnectionParts>,
    ) -> Self {
        Self {
            handoff_id,
            held: Held::new(
                "ryeos-scheduler::outside-sources",
                SchedulerNativeOutsideSourceParts { database, schema },
                scheduler_native_outside_sources_settled,
            ),
        }
    }

    pub fn handoff_id(&self) -> Result<&SchedulerNativeStoreHandoffId> {
        anyhow::ensure!(!self.held.is_foreign(),
            "foreign Scheduler outside source owner");
        Ok(&self.handoff_id)
    }

    pub fn is_empty(&self) -> bool {
        !self.held.is_foreign()
            && self.held.database.is_none() && self.held.schema.is_none()
    }

    /// # Safety
    /// The returned source must stay in live custody and must either be
    /// accepted by this handoff's Store progress or restored to this wrapper.
    pub unsafe fn take_database(&mut self) -> Result<Option<SchedulerNativeDatabaseSource>> {
        anyhow::ensure!(!self.held.is_foreign(),
            "foreign Scheduler outside source owner");
        Ok(self.held.get_mut().database.take())
    }

    /// # Safety
    /// `source` is the exact database owner previously returned by this
    /// handoff wrapper, was not accepted by a Store source frame, and the
    /// wrapper's database slot is still empty in this process.
    pub unsafe fn restore_database(&mut self, source: SchedulerNativeDatabaseSource) {
        self.held.get_mut().database = Some(source);
    }

    /// # Safety
    /// The returned source must stay in live custody and must either be
    /// accepted by this handoff's Store progress or restored to this wrapper.
    pub unsafe fn take_schema(
        &mut self,
    ) -> Result<Option<lillux_sqlite::OriginalNativeOutsideConnectionParts>> {
        anyhow::ensure!(!self.held.is_foreign(),
            "foreign Scheduler outside source owner");
        Ok(self.held.get_mut().schema.take())
    }

    /// # Safety
    /// `source` is the exact schema owner previously returned by this
    /// handoff wrapper, was not accepted by a Store source frame, and the
    /// wrapper's schema slot is still empty in this process.
    pub unsafe fn restore_schema(
        &mut self,
        source: lillux_sqlite::OriginalNativeOutsideConnectionParts,
    ) {
        self.held.get_mut().schema = Some(source);
    }
}

impl SchedulerNativeFinalizationOwner {
    pub fn require_original(&self, factory: &Arc<OriginalNativeFactory>) -> Result<()> {
        anyhow::ensure!(!self.held.is_foreign(),
            "foreign Scheduler finalization owner");
        let original_factory = self.held.factory.as_ref()
            .context("Scheduler finalization factory has already transferred")?;
        factory.require_original_process()?;
        anyhow::ensure!(Arc::ptr_eq(factory, original_factory),
            "Scheduler finalization changed its original factory");
        if let Some(reference) = self.held.schema_reference.as_ref() {
            reference.terminal().require_original(factory)?;
        }
        Ok(())
    }

    /// Actual scheduler terminal receipts used in the same factory ACK. A
    /// never-opened admission contributes no native receipt; its bound inputs
    /// remain retained inside this owner for the later physical handoff.
    pub fn retained_native_terminals(&self) -> Result<Vec<OriginalNativeConnectionTerminal>> {
        let factory = self.held.factory.as_ref()
            .context("Scheduler finalization factory has already transferred")?;
        self.require_original(factory)?;
        anyhow::ensure!(self.held.database_outside.is_none() && self.held.database_outside_custody.is_none()
            && self.held.database_never_opened.is_none()
            && self.held.database_refusal.is_none() && self.held.schema_outside.is_none()
            && self.held.schema_outside_custody.is_none()
            && self.held.schema_refusal.is_none(),
            "Scheduler native sources already entered outside finalization");
        let reference = self.held.schema_reference.as_ref()
            .context("Scheduler schema reference has already transferred")?;
        let mut terminals = vec![reference.terminal().clone()];
        match &self.held.inner {
            DbOwner::Terminal(terminal) => {
                terminal.require_original(factory)?;
                if !matches!(terminal,
                    OriginalNativeConnectionTerminal::Released(receipt)
                        if receipt.native_ticket().is_none())
                {
                    terminals.push(terminal.clone());
                }
            }
            DbOwner::NeverOpened(_) => {}
            _ => anyhow::bail!("Scheduler finalization owner is not at a closed native phase"),
        }
        Ok(terminals)
    }

    /// After aggregate acknowledgement, consume the exact Scheduler DB and
    /// schema terminals into outside source packets. Every refusal remains in
    /// this owner for same-phase retry; success releases this owner's factory
    /// alias while keeping the real directory/authority sources retained.
    pub fn prepare_outside_sources(
        &mut self,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
    ) -> Result<()> {
        anyhow::ensure!(!self.held.is_foreign(),
            "foreign Scheduler finalization owner");
        let factory = self.held.factory.as_ref()
            .context("Scheduler finalization factory already transferred")?;
        factory.require_original_process()?;
        aggregate.require_original(factory)?;

        if let Some(failure) = self.held.get_mut().database_refusal.take() {
            match failure.into_original() {
                Ok(terminal) => self.held.get_mut().inner = DbOwner::Terminal(terminal),
                Err(failure) => {
                    self.held.get_mut().database_refusal = Some(failure);
                    anyhow::bail!("Scheduler database outside-custody refusal remains retained");
                }
            }
        }
        if let Some(custody) = self.held.get_mut().database_outside_custody.take() {
            match custody.into_original_parts() {
                Ok(parts) => self.held.get_mut().database_outside = Some(parts),
                Err(custody) => {
                    self.held.get_mut().database_outside_custody = Some(custody);
                    anyhow::bail!("Scheduler database outside source extraction remains retained");
                }
            }
        }
        if self.held.database_outside.is_none() && self.held.database_never_opened.is_none() {
            let previous = std::mem::replace(&mut self.held.get_mut().inner, DbOwner::CheckedOut);
            match previous {
                DbOwner::Terminal(terminal) => {
                    match terminal.into_outside_custody(aggregate) {
                        Ok(custody) => match custody.into_original_parts() {
                            Ok(parts) => self.held.get_mut().database_outside = Some(parts),
                            Err(custody) => {
                                self.held.get_mut().database_outside_custody = Some(custody);
                                anyhow::bail!("Scheduler database outside source extraction remains retained");
                            }
                        },
                        Err(failure) => match failure.into_original() {
                            Ok(terminal) => {
                                self.held.get_mut().inner = DbOwner::Terminal(terminal);
                                anyhow::bail!("Scheduler database outside transfer refused; terminal restored");
                            }
                            Err(failure) => {
                                self.held.get_mut().database_refusal = Some(failure);
                                anyhow::bail!("Scheduler database outside refusal remains retained");
                            }
                        },
                    }
                }
                DbOwner::NeverOpened(inputs) => {
                    self.held.get_mut().database_never_opened = Some(inputs);
                }
                other => {
                    self.held.get_mut().inner = other;
                    anyhow::bail!("Scheduler database has not reached a finalizable native phase");
                }
            }
        }

        if let Some(failure) = self.held.get_mut().schema_refusal.take() {
            match failure.into_original() {
                Ok(reference) => self.held.get_mut().schema_reference = Some(reference),
                Err((failure, error)) => {
                    self.held.get_mut().schema_refusal = Some(failure);
                    return Err(error.context("Scheduler schema-reference outside refusal remains retained"));
                }
            }
        }
        if let Some(custody) = self.held.get_mut().schema_outside_custody.take() {
            match custody.into_original_parts() {
                Ok(parts) => self.held.get_mut().schema_outside = Some(parts),
                Err(custody) => {
                    self.held.get_mut().schema_outside_custody = Some(custody);
                    anyhow::bail!("Scheduler schema outside source extraction remains retained");
                }
            }
        }
        if self.held.schema_outside.is_none() {
            let reference = self.held.get_mut().schema_reference.take()
                .context("Scheduler schema reference already transferred")?;
            match reference.into_outside_custody(aggregate) {
                Ok(custody) => match custody.into_original_parts() {
                    Ok(parts) => self.held.get_mut().schema_outside = Some(parts),
                    Err(custody) => {
                        self.held.get_mut().schema_outside_custody = Some(custody);
                        anyhow::bail!("Scheduler schema outside source extraction remains retained");
                    }
                },
                Err(failure) => {
                    let error = anyhow::anyhow!(failure.to_string());
                    self.held.get_mut().schema_refusal = Some(failure);
                    return Err(error.context("Scheduler schema-reference outside transfer refused"));
                }
            }
        }

        anyhow::ensure!(self.held.database_outside.is_some() || self.held.database_never_opened.is_some(),
            "Scheduler database source was not retained outside");
        anyhow::ensure!(self.held.schema_outside.is_some(),
            "Scheduler schema-reference source was not retained outside");
        // The exact terminals have moved into sources whose authority edges are
        // weak. No later finalization stage may consult resident factory state.
        drop(self.held.get_mut().factory.take());
        Ok(())
    }

    /// # Safety
    /// The returned source owner must remain retained until the exact Store
    /// progress accepts both source legs and issues its handoff receipt.
    pub unsafe fn take_outside_sources(
        &mut self,
    ) -> Result<SchedulerNativeOutsideSources> {
        anyhow::ensure!(!self.held.is_foreign(),
            "foreign Scheduler finalization owner");
        anyhow::ensure!(self.held.factory.is_none()
            && self.held.database_outside_custody.is_none()
            && self.held.schema_outside_custody.is_none()
            && self.held.database_refusal.is_none() && self.held.schema_refusal.is_none(),
            "Scheduler outside sources have not completed their native handoff");
        let issuer = self.held.issuer();
        let handoff_id = self.outside_handoff_id.get_or_insert_with(||
            SchedulerNativeStoreHandoffId::new(issuer)).clone();
        let database = if let Some(parts) = self.held.get_mut().database_outside.take() {
            SchedulerNativeDatabaseSource::from_connection(parts)
        } else {
            SchedulerNativeDatabaseSource::from_never_opened(
                self.held.get_mut().database_never_opened.take()
                    .context("Scheduler database source absent after finalization")?,
            )
        };
        let schema = self.held.get_mut().schema_outside.take()
            .context("Scheduler schema-reference source absent after finalization")?;
        Ok(SchedulerNativeOutsideSources::new(handoff_id, Some(database), Some(schema)))
    }

    /// Retire this emptied wrapper only after the node finalizer accepted both
    /// actual Scheduler sources into its retained Store inventory.
    pub fn finish_outside_handoff(
        self,
        receipt: SchedulerNativeStoreHandoffReceipt<'_>,
    ) -> std::result::Result<(), (Self, anyhow::Error)> {
        let checked = (|| -> Result<()> {
            anyhow::ensure!(!self.held.is_foreign(),
                "foreign Scheduler finalization owner");
            let handoff_id = self.outside_handoff_id.as_ref()
                .context("Scheduler Store handoff identity absent")?;
            anyhow::ensure!(receipt.id.same_handoff(handoff_id),
                "Scheduler Store handoff proof names another source set");
            anyhow::ensure!(self.held.outside_sources_cleared(),
                "Scheduler finalization owner still retains outside sources");
            anyhow::ensure!(matches!(&self.held.inner, DbOwner::CheckedOut),
                "Scheduler finalization owner still retains its database phase");
            Ok(())
        })();
        if let Err(error) = checked { return Err((self, error)); }
        // Every source has transferred and the phase slot is checked out, so
        // the held parts settle and release their emptied slots normally.
        match self.held.settle() {
            Ok(settled) => {
                let _settled = settled;
                Ok(())
            }
            Err(held) => Err((
                Self { held, outside_handoff_id: self.outside_handoff_id },
                anyhow!("Scheduler finalization owner did not settle"),
            )),
        }
    }
}
#[derive(Debug)]
pub enum SchedulerDbCloseError {
    NativeCustody(NativeRetentionReason),
    Poisoned,
    RetirementPending,
    OriginalProcess,
    Native(String),
}
impl std::fmt::Display for SchedulerDbCloseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for SchedulerDbCloseError {}
/// Errors contain diagnostics; the actual owner remains in SchedulerDb.
#[derive(Debug)]
pub enum SchedulerDbRetirementError {
    InvalidPhase,
    GenerationMismatch,
    Poisoned,
    Host(String),
    Role(SqliteRoleMutationError),
    Resolution(lillux_sqlite::SqliteRoleResolutionError),
    Unstarted(RetainedSchedulerRetirementInputs),
}
impl std::fmt::Display for SchedulerDbRetirementError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for SchedulerDbRetirementError {}
/// Actual input custody on a refusal before the DB transfers its owner.
/// Discarding this diagnostic cannot drop the original authority lifelines.
pub struct RetainedSchedulerRetirementInputs {
    held: Held<RetainedRetirementInputsParts>,
}
struct RetainedRetirementInputsParts {
    inputs: Option<OriginalSchedulerRetirementInputs>,
}
/// Settled exactly when the inputs have transferred out of the slot. Inputs
/// still retained keep their authority lifeline until the process exits.
fn retained_retirement_inputs_settled(parts: &RetainedRetirementInputsParts) -> bool {
    parts.inputs.is_none()
}
impl std::fmt::Debug for RetainedSchedulerRetirementInputs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("original scheduler retirement inputs retained before transfer")
    }
}
impl RetainedSchedulerRetirementInputs {
    fn new(inputs: OriginalSchedulerRetirementInputs) -> Self {
        Self {
            held: Held::new(
                "ryeos-scheduler::retirement-inputs",
                RetainedRetirementInputsParts { inputs: Some(inputs) },
                retained_retirement_inputs_settled,
            ),
        }
    }
    pub fn into_inputs(mut self) -> std::result::Result<OriginalSchedulerRetirementInputs, Self> {
        if self.held.is_foreign() { return Err(self); }
        // Exclusive original-process transfer, with no authority effect. The
        // slot is left empty, so the held parts then settle releasing nothing.
        let inputs = self.held.get_mut().inputs.take();
        Ok(inputs.expect("actual retained retirement inputs"))
    }
}
enum OpenCustody {
    Database(SchedulerDb),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchedulerDbOpenDisposition {
    StoppedProjectionIntake,
    Refused,
    RecognizedStaleProjection,
}
pub struct SchedulerDbOpenFailure {
    disposition: SchedulerDbOpenDisposition,
    error: anyhow::Error,
    custody: Option<OpenCustody>,
    close_error: Option<SchedulerDbCloseError>,
}
impl SchedulerDbOpenFailure {
    pub fn observe_stopped_projection(
        &self,
    ) -> std::result::Result<
        lillux_sqlite::StoppedSqliteProjectionObservation,
        SchedulerDbRetirementError,
    > {
        match &self.custody {
            Some(OpenCustody::Database(db))
                if self.disposition == SchedulerDbOpenDisposition::StoppedProjectionIntake =>
            {
                db.observe_stopped_projection()
            }
            _ => Err(SchedulerDbRetirementError::InvalidPhase),
        }
    }
    /// Consume only this exact failed stopped DB, never normal-open fallback.
    pub fn retry_stopped_projection(
        mut self,
        plan: &lillux_sqlite::StoppedSqliteProjectionPlan,
    ) -> std::result::Result<SchedulerDb, Self> {
        if self.disposition != SchedulerDbOpenDisposition::StoppedProjectionIntake {
            return Err(self);
        }
        let db = match self.custody.take() {
            Some(OpenCustody::Database(db)) => db,
            other => {
                self.custody = other;
                return Err(self);
            }
        };
        match db.retry_stopped_projection(plan) {
            Ok(_) => Ok(db),
            Err(error) => {
                self.error = anyhow::Error::new(error);
                self.custody = Some(OpenCustody::Database(db));
                Err(self)
            }
        }
    }
    /// Explicit cleanup-only settlement; generic close never aborts retirement.
    pub fn abort_stopped_projection(mut self) -> std::result::Result<(), Self> {
        if self.disposition != SchedulerDbOpenDisposition::StoppedProjectionIntake {
            return Err(self);
        }
        let db = match self.custody.take() {
            Some(OpenCustody::Database(db)) => db,
            other => {
                self.custody = other;
                return Err(self);
            }
        };
        match db.abort_projection_retirement() {
            Ok(()) => Ok(()),
            Err(error) => {
                self.error = anyhow::Error::new(error);
                self.custody = Some(OpenCustody::Database(db));
                Err(self)
            }
        }
    }
    pub fn disposition(&self) -> SchedulerDbOpenDisposition {
        self.disposition
    }
    /// Transfer the original recognized stale owner to the stopped controller.
    /// No SQL close, reopen, or reconstruction of physical authority occurs.
    pub fn into_stale_projection(mut self) -> std::result::Result<SchedulerDb, Self> {
        if self.disposition != SchedulerDbOpenDisposition::RecognizedStaleProjection {
            return Err(self);
        }
        match self.custody.take() {
            Some(OpenCustody::Database(db)) => Ok(db),
            other => {
                self.custody = other;
                Err(self)
            }
        }
    }
    pub fn error(&self) -> &anyhow::Error {
        &self.error
    }
    pub fn close_error(&self) -> Option<&SchedulerDbCloseError> {
        self.close_error.as_ref()
    }
    pub fn has_retained_owner(&self) -> bool {
        self.custody.is_some()
    }
    /// Successful cleanup preserves this owner and its actual receipts for
    /// the original factory census; success never discards the paid reference.
    pub fn retry_close(mut self) -> std::result::Result<Self, Self> {
        let result = match self.custody.as_ref() {
            Some(OpenCustody::Database(db)) => db.close(),
            None => return Ok(self),
        };
        match result {
            Ok(()) => Ok(self),
            Err(error) => { self.close_error = Some(error); Err(self) }
        }
    }
    /// Consume a live failed-open owner only after its caller has reached the
    /// same typed terminal phase as an ordinary Scheduler shutdown. The
    /// original open error has already been returned to the startup caller;
    /// this transfer preserves the database and paid schema source, not a
    /// second diagnostic owner.
    pub fn into_native_finalization_owner(
        mut self,
    ) -> std::result::Result<SchedulerNativeFinalizationOwner, (Self, anyhow::Error)> {
        let db = match self.custody.take() {
            Some(OpenCustody::Database(db)) => db,
            other => {
                self.custody = other;
                return Err((self, anyhow::anyhow!(
                    "Scheduler open failure lacks its original database owner")));
            }
        };
        match db.into_native_finalization() {
            Ok(owner) => Ok(owner),
            Err((db, error)) => {
                self.custody = Some(OpenCustody::Database(db));
                Err((self, error))
            }
        }
    }
    pub fn retained_native_terminals(&self) -> Result<Vec<OriginalNativeConnectionTerminal>> {
        match self.custody.as_ref() {
            Some(OpenCustody::Database(db)) => db.retained_native_terminals(),
            None => anyhow::bail!("scheduler failure has no retained database custody"),
        }
    }
    /// Consume a refused opening whose store has closed and whose terminals
    /// are already filed in a test host's census, ending the store's own
    /// aliases of the host's factory. Refusal returns the same failure with
    /// its store still in custody.
    #[cfg(any(test, feature = "test-support"))]
    pub fn release_settled_for_test(mut self) -> std::result::Result<(), (Self, anyhow::Error)> {
        let db = match self.custody.take() {
            Some(OpenCustody::Database(db)) => db,
            None => {
                return Err((self, anyhow!("scheduler failure has no retained database custody")));
            }
        };
        match db.release_settled_into_census() {
            Ok(()) => Ok(()),
            Err((db, error)) => {
                self.custody = Some(OpenCustody::Database(db));
                Err((self, error))
            }
        }
    }

}
impl std::fmt::Debug for SchedulerDbOpenFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SchedulerDbOpenFailure")
            .field("error", &self.error)
            .field("retained_owner", &self.has_retained_owner())
            .finish()
    }
}
impl std::fmt::Display for SchedulerDbOpenFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.error, f)
    }
}
impl std::error::Error for SchedulerDbOpenFailure {}

impl SchedulerDb {
    /// Consume a closed original store whose terminals an offline host's
    /// census already holds, ending its factory alias, schema reference and
    /// closed owner so the host can release its arena. This is the settlement
    /// of a store opened on a process-scoped offline host (a test host or a
    /// stopped-node maintenance host); a live node's store instead hands its
    /// owners to the node finalizer. A store that is not closed, or not this
    /// process's original, comes back unchanged with the refusal.
    pub fn release_settled_into_census(self) -> std::result::Result<(), (Self, anyhow::Error)> {
        let checked = (|| -> Result<()> {
            self.require_process()?;
            let original = self.held.original.as_ref()
                .context("Scheduler has no original live native context")?;
            original.reference.terminal().require_original(&original.factory)?;
            let inner = self.held.inner.try_lock()
                .map_err(|_| anyhow!("Scheduler owner is busy or poisoned"))?;
            match &*inner {
                DbOwner::Terminal(terminal) => terminal.require_original(&original.factory)?,
                DbOwner::NeverOpened(_) => {}
                _ => anyhow::bail!("Scheduler has not closed"),
            }
            Ok(())
        })();
        if let Err(error) = checked {
            return Err((self, error));
        }
        let Self { held, outbox_drain } = self;
        // The owners are terminal and already filed in the host's census, so
        // dropping them ends only aliases.
        match held.take_whole() {
            Ok(parts) => {
                drop(parts);
                drop(outbox_drain);
                Ok(())
            }
            Err(held) => Err((
                Self { held, outbox_drain },
                anyhow!("Scheduler owner is not this process's original"),
            )),
        }
    }

    /// Transfer a closed original Scheduler into its retained node-finalizer
    /// owner. Callers must first consume every Arc alias (normally with
    /// `Arc::try_unwrap`) after stopping the scheduler service. Refusal returns
    /// the same complete Scheduler with no owner movement.
    pub fn into_native_finalization(
        self,
    ) -> std::result::Result<SchedulerNativeFinalizationOwner, (Self, anyhow::Error)> {
        let checked = (|| -> Result<()> {
            self.require_process()?;
            let original = self.held.original.as_ref()
                .context("Scheduler has no original live native context")?;
            original.factory.require_original_process()?;
            original.reference.terminal().require_original(&original.factory)?;
            let inner = self.held.inner.try_lock()
                .map_err(|_| anyhow!("Scheduler finalization owner is busy or poisoned"))?;
            match &*inner {
                DbOwner::Terminal(terminal) => terminal.require_original(&original.factory)?,
                DbOwner::NeverOpened(_) => {}
                _ => anyhow::bail!("Scheduler close has not reached a consuming finalization phase"),
            }
            Ok(())
        })();
        if let Err(error) = checked {
            return Err((self, error));
        }

        // The complete original Scheduler value is consumed: the validated
        // Mutex state and schema context move together into the inert
        // finalization owner. The outbox mutex is ordinary coordination state,
        // not a retained native owner; no outstanding guard can survive
        // consumption of the Scheduler's unique owner.
        let Self { held, outbox_drain } = self;
        let parts = match held.take_whole() {
            Ok(parts) => parts,
            Err(held) => {
                return Err((
                    Self { held, outbox_drain },
                    anyhow!("Scheduler is not this process's original"),
                ));
            }
        };
        drop(outbox_drain);
        let SchedulerDbParts { inner, original } = parts;
        let owner = inner.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner);
        let context = original.expect("validated original Scheduler context");
        let OriginalSchedulerContext { factory, reference } = context;
        Ok(SchedulerNativeFinalizationOwner {
            held: Held::new(
                "ryeos-scheduler::native-finalization-owner",
                SchedulerNativeFinalizationParts {
                    inner: owner,
                    factory: Some(factory),
                    schema_reference: Some(reference),
                    database_outside: None,
                    database_outside_custody: None,
                    database_never_opened: None,
                    database_refusal: None,
                    schema_outside: None,
                    schema_outside_custody: None,
                    schema_refusal: None,
                },
                scheduler_native_finalization_settled,
            ),
            outside_handoff_id: None,
        })
    }

    /// No SQL construction; signed stopped current-object intent precedes intake.
    /// Original copied-source retirement and recovered remaining-source retirement
    /// both use this owner. Complete original accounting/schema admission belongs
    /// to the independently authenticated disposable-copy caller, not a live open.
    /// Success retains verified current pins/barriers; failure retains the SAME
    /// native intake owner for exact-plan retry rather than falling back to SQL.
    pub fn capture_stopped_projection(
        runtime: Arc<PinnedDirectory>,
        plan: lillux_sqlite::StoppedSqliteProjectionPlan,
    ) -> std::result::Result<Self, SchedulerDbOpenFailure> {
        let generation = plan.generation;
        let (phase, error) =
            match DrainedSqliteProjection::capture_stopped_projection(runtime, plan) {
                Ok(owner) => (RetirementPhase::Drained(owner), None),
                Err(failure) => {
                    let error = anyhow!("stopped scheduler intake retained: {failure}");
                    (RetirementPhase::Failed(failure), Some(error))
                }
            };
        let db = Self::from_parts(DbOwner::Retirement { generation, phase }, None);
        match error {
            None => Ok(db),
            Some(error) => Err(SchedulerDbOpenFailure {
                disposition: SchedulerDbOpenDisposition::StoppedProjectionIntake,
                error,
                custody: Some(OpenCustody::Database(db)),
                close_error: None,
            }),
        }
    }
    pub fn retry_stopped_projection(
        &self,
        plan: &lillux_sqlite::StoppedSqliteProjectionPlan,
    ) -> std::result::Result<DrainedSqliteRoleFacts, SchedulerDbRetirementError> {
        self.retirement_transition(|phase| {
            let result = match phase {
                RetirementPhase::Failed(failure) => failure.retry_stopped_projection(plan),
                RetirementPhase::Drained(owner)
                    if owner
                        .observe_stopped_projection()
                        .is_ok_and(|observed| observed.plan == *plan && observed.ready) =>
                {
                    let facts = owner.role_facts().clone();
                    return Ok((Some(RetirementPhase::Drained(owner)), facts));
                }
                phase => return Err((phase, SchedulerDbRetirementError::InvalidPhase)),
            };
            match result {
                Ok(owner) => {
                    let facts = owner.role_facts().clone();
                    Ok((Some(RetirementPhase::Drained(owner)), facts))
                }
                Err(failure) => {
                    let error = SchedulerDbRetirementError::Host(failure.to_string());
                    Err((RetirementPhase::Failed(failure), error))
                }
            }
        })
    }
    /// Read-only stopped observations from the SAME partial or ready owner.
    pub fn observe_stopped_projection(
        &self,
    ) -> std::result::Result<
        lillux_sqlite::StoppedSqliteProjectionObservation,
        SchedulerDbRetirementError,
    > {
        self.retirement_transition(|phase| {
            let result = match &phase {
                RetirementPhase::Drained(owner) => owner.observe_stopped_projection(),
                RetirementPhase::Failed(failure) => match failure.owner() {
                    lillux_sqlite::SqliteProjectionRetirementOwner::Drained(owner) => {
                        owner.observe_stopped_projection()
                    }
                    _ => return Err((phase, SchedulerDbRetirementError::InvalidPhase)),
                },
            };
            match result {
                Ok(observed) => Ok((Some(phase), observed)),
                Err(error) => Err((phase, SchedulerDbRetirementError::Host(error.to_string()))),
            }
        })
    }
    /// Inspect a disposable copy for explicit offline history retirement.
    /// Reuse the owning store's stale-projection recognition, but never rebuild
    /// the copy or decode predecessor rows to manufacture an empty count.
    /// Unlike retention reads, retirement does not require a complete fire
    /// projection: all fire history has been selected for discard. The
    /// complete schema is compared with `reference`, a definition captured
    /// from a closed native scheduler schema reference.
    ///
    /// `inputs` are the readonly inspection inputs the caller issued for the
    /// copy under `factory`; the copy opens on one native ticket of that
    /// factory. The closed terminal returns with the accounting for the
    /// caller's census. A refused acquisition or close keeps its native
    /// owner in a [`SchedulerInspectionCloseFailure`].
    pub fn inspect_history_reset(
        factory: &Arc<OriginalNativeFactory>,
        inputs: OriginalNativeBoundInputs,
        reference: &sqlite_schema::SchemaSqlDefinition,
    ) -> Result<(
        Result<SchedulerFireHistoryAccounting>,
        OriginalNativeConnectionTerminal,
    )> {
        let owner = match factory.open_bound(inputs) {
            Ok(owner) => owner,
            Err(failure) => {
                return Err(anyhow::Error::new(SchedulerInspectionCloseFailure {
                    error: format!("{failure}"),
                    inspection_error: None,
                    custody: std::sync::Mutex::new(Some(InspectionCustody::Open(failure))),
                }));
            }
        };
        let accounting = owner.with_sql(|connection| {
            inspect_history_reset_conn(connection, Path::new("scheduler.sqlite3"), reference)
        });
        match owner.close() {
            Ok(closed) => Ok((accounting, OriginalNativeConnectionTerminal::Closed(closed))),
            Err(failure) => Err(anyhow::Error::new(SchedulerInspectionCloseFailure {
                error: format!("{failure}"),
                inspection_error: accounting.err(),
                custody: std::sync::Mutex::new(Some(InspectionCustody::Close(failure))),
            })),
        }
    }

    /// Build only this module's declared schema in the SAME paid factory.
    /// Every typed preparation failure must remain in composition custody.
    pub fn prepare_scheduler_schema_reference(
        factory: Arc<OriginalNativeFactory>, inputs: OriginalNativeTransientInputs,
    ) -> std::result::Result<sqlite_schema::ClosedNativeSchemaSqlReference, sqlite_schema::NativeSchemaSqlReferenceFailure> {
        sqlite_schema::prepare_native_schema_sql_reference(
            factory, inputs, OriginalNativeDomain::NodeSchedulerSchemaReference, SCHEMA_SQL,
        )
    }
    /// Genuine fixed-role inputs and actual paid closed reference are required.
    /// No ordinary Bound or reference database fallback exists on this path.
    pub fn open_original_scheduler(
        factory: Arc<OriginalNativeFactory>, inputs: OriginalNativeBoundInputs,
        reference: sqlite_schema::ClosedNativeSchemaSqlReference,
    ) -> std::result::Result<Self, SchedulerDbOpenFailure> {
        let db = Self::from_parts(
            DbOwner::BeforeOpen(inputs),
            Some(OriginalSchedulerContext { factory, reference }),
        );
        let mut disposition = SchedulerDbOpenDisposition::Refused;
        let preparation = (|| -> Result<()> {
            db.require_process()?;
            let original = db.held.original.as_ref().context("original scheduler factory absent")?;
            original.factory.require_ready()?;
            original.reference.terminal().require_original(&original.factory)?;
            anyhow::ensure!(matches!(original.reference.terminal(),
                OriginalNativeConnectionTerminal::Closed(closed)
                    if closed.domain() == OriginalNativeDomain::NodeSchedulerSchemaReference),
                "scheduler reference has foreign actual purpose");
            {
                let mut guard = db.held.inner.try_lock().map_err(|_| anyhow!("scheduler open owner busy"))?;
                let DbOwner::BeforeOpen(inputs) = &*guard else { anyhow::bail!("scheduler open already attempted") };
                inputs.require_scheduler_coordinate()?;
                let DbOwner::BeforeOpen(inputs) = std::mem::replace(&mut *guard, DbOwner::CheckedOut) else { unreachable!() };
                match original.factory.open_bound(inputs) {
                    Ok(owner) => *guard = DbOwner::Live(owner),
                    Err(failure) => {
                        // Actual partial owner precedes diagnostic allocation.
                        *guard = DbOwner::Opening(failure);
                        anyhow::bail!("original scheduler acquisition retained")
                    }
                }
            }
            let existed = db.with_live_owner(|owner| owner.scheduler_bound_origin())? == BoundSqliteOrigin::Existing;
            db.with_sql(|conn| {
                let path = Path::new("scheduler.sqlite3");
                let spec = scheduler_schema_spec();
                if existed {
                    let integrity: String = conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
                    anyhow::ensure!(integrity == "ok", "scheduler database integrity check failed: {integrity}");
                    let app_id: i32 = conn.query_row("PRAGMA application_id", [], |row| row.get(0))?;
                    if app_id == spec.application_id {
                        let exact = sqlite_schema::assert_owned(conn, &spec, path)
                            .and_then(|_| original.reference.definition().require_matches(conn, path));
                        if exact.is_err() {
                            assert_recognized_stale_scheduler_projection(conn, path)?;
                            disposition = SchedulerDbOpenDisposition::RecognizedStaleProjection;
                            anyhow::bail!("recognized stale scheduler projection requires authenticated host retirement")
                        }
                    } else if app_id != 0 { anyhow::bail!("foreign scheduler database application_id {app_id}") }
                }
                sqlite_schema::prepare_owned(conn, &spec, SCHEMA_SQL, path)?;
                original.reference.definition().require_matches(conn, path)
            })?;
            db.with_live_owner(|owner| owner.establish_scheduler_wal_binding())
        })();
        match preparation {
            Ok(()) => Ok(db),
            Err(error) => {
                if disposition == SchedulerDbOpenDisposition::RecognizedStaleProjection {
                    let mut guard = db.held.inner.try_lock().expect("private scheduler constructor owner");
                    if matches!(&*guard, DbOwner::Live(_)) {
                        let DbOwner::Live(owner) = std::mem::replace(&mut *guard, DbOwner::CheckedOut) else { unreachable!() };
                        *guard = DbOwner::Stale(owner);
                    }
                }
                Err(SchedulerDbOpenFailure { disposition, error,
                    custody: Some(OpenCustody::Database(db)), close_error: None })
            }
        }
    }
    /// Caller retains actual signed immutable intent and original authority.
    /// Refusals before transfer return the actual input owner in Unstarted.
    pub fn capture_projection_retirement(
        &self, generation: [u8; 16], inputs: OriginalSchedulerRetirementInputs,
    ) -> std::result::Result<CapturedSqliteRoleFacts, SchedulerDbRetirementError> {
        let mut retained = RetainedSchedulerRetirementInputs::new(inputs);
        if self.require_process().is_err()
            || retained.held.inputs.as_ref().expect("actual input").generation() != generation {
            return Err(SchedulerDbRetirementError::Unstarted(retained));
        }
        let mut guard = match self.held.inner.try_lock() {
            Ok(guard) => guard,
            Err(_) => return Err(SchedulerDbRetirementError::Unstarted(retained)),
        };
        if !matches!(&*guard, DbOwner::Live(_) | DbOwner::Stale(_)) {
            return Err(SchedulerDbRetirementError::Unstarted(retained));
        }
        let previous = std::mem::replace(&mut *guard, DbOwner::CheckedOut);
        let owner = match previous { DbOwner::Live(owner) | DbOwner::Stale(owner) => owner, _ => unreachable!() };
        // The original-process input transfers exactly once to the same
        // owner, leaving the retained slot empty so it settles.
        let inputs = retained.held.get_mut().inputs.take().expect("actual input");
        match owner.begin_scheduler_retirement(inputs) {
            Ok((owner, facts)) => {
                *guard = DbOwner::NativeRetirement { generation, phase: NativeRetirementPhase::Owned(owner) };
                Ok(facts)
            }
            Err(failure) => {
                *guard = DbOwner::NativeRetirement { generation, phase: NativeRetirementPhase::Failed(failure) };
                Err(SchedulerDbRetirementError::Host("original native scheduler capture retained".into()))
            }
        }
    }
    pub fn retry_projection_capture(&self, generation: [u8; 16])
        -> std::result::Result<CapturedSqliteRoleFacts, SchedulerDbRetirementError> {
        self.native_retirement_transition(Some(generation), |owner| owner.capture())
    }
    fn has_native_retirement(&self) -> std::result::Result<bool, SchedulerDbRetirementError> {
        self.require_process().map_err(|_| SchedulerDbRetirementError::InvalidPhase)?;
        let guard = self.held.inner.try_lock().map_err(|_| SchedulerDbRetirementError::Poisoned)?;
        Ok(matches!(&*guard, DbOwner::NativeRetirement { .. }))
    }
    fn native_retirement_transition<T>(&self, expected: Option<[u8; 16]>,
        operation: impl FnOnce(OriginalSchedulerRetirement)
            -> std::result::Result<(OriginalSchedulerRetirement, T), OriginalSchedulerRetirementFailure>,
    ) -> std::result::Result<T, SchedulerDbRetirementError> {
        self.require_process().map_err(|_| SchedulerDbRetirementError::InvalidPhase)?;
        let mut guard = self.held.inner.try_lock().map_err(|_| SchedulerDbRetirementError::Poisoned)?;
        let previous = std::mem::replace(&mut *guard, DbOwner::CheckedOut);
        let DbOwner::NativeRetirement { generation, phase } = previous else {
            *guard = previous; return Err(SchedulerDbRetirementError::InvalidPhase);
        };
        if expected.is_some_and(|expected| expected != generation) {
            *guard = DbOwner::NativeRetirement { generation, phase };
            return Err(SchedulerDbRetirementError::GenerationMismatch);
        }
        let owner = match phase {
            NativeRetirementPhase::Owned(owner) => owner,
            NativeRetirementPhase::Failed(failure) => match failure.into_owner() {
                Ok(owner) => owner,
                Err(failure) => {
                    *guard = DbOwner::NativeRetirement { generation, phase: NativeRetirementPhase::Failed(failure) };
                    return Err(SchedulerDbRetirementError::Host("native retirement custody transfer refused".into()));
                }
            },
        };
        match operation(owner) {
            Ok((owner, value)) => {
                *guard = DbOwner::NativeRetirement { generation, phase: NativeRetirementPhase::Owned(owner) }; Ok(value)
            }
            Err(failure) => {
                *guard = DbOwner::NativeRetirement { generation, phase: NativeRetirementPhase::Failed(failure) };
                Err(SchedulerDbRetirementError::Host("native retirement effect retained".into()))
            }
        }
    }
    fn finish_native_retirement(&self, abort: bool) -> std::result::Result<(), SchedulerDbRetirementError> {
        self.require_process().map_err(|_| SchedulerDbRetirementError::InvalidPhase)?;
        let mut guard = self.held.inner.try_lock().map_err(|_| SchedulerDbRetirementError::Poisoned)?;
        let previous = std::mem::replace(&mut *guard, DbOwner::CheckedOut);
        let DbOwner::NativeRetirement { generation, phase } = previous else {
            *guard = previous; return Err(SchedulerDbRetirementError::InvalidPhase);
        };
        let owner = match phase {
            NativeRetirementPhase::Owned(owner) => owner,
            NativeRetirementPhase::Failed(failure) => match failure.into_owner() {
                Ok(owner) => owner,
                Err(failure) => {
                    *guard = DbOwner::NativeRetirement { generation, phase: NativeRetirementPhase::Failed(failure) };
                    return Err(SchedulerDbRetirementError::Host("native retirement custody transfer refused".into()));
                }
            },
        };
        match if abort { owner.abort() } else { owner.release() } {
            Ok(receipt) => { *guard = DbOwner::Terminal(OriginalNativeConnectionTerminal::Closed(receipt)); Ok(()) }
            Err(failure) => {
                *guard = DbOwner::NativeRetirement { generation, phase: NativeRetirementPhase::Failed(failure) };
                Err(SchedulerDbRetirementError::Host("native retirement release retained".into()))
            }
        }
    }
    fn retirement_transition<T>(
        &self,
        transition: impl FnOnce(
            RetirementPhase,
        ) -> std::result::Result<
            (Option<RetirementPhase>, T),
            (RetirementPhase, SchedulerDbRetirementError),
        >,
    ) -> std::result::Result<T, SchedulerDbRetirementError> {
        self.require_process().map_err(|_| SchedulerDbRetirementError::InvalidPhase)?;
        let mut guard = self
            .held.inner
            .try_lock()
            .map_err(|_| SchedulerDbRetirementError::Poisoned)?;
        let previous = std::mem::replace(&mut *guard, DbOwner::CheckedOut);
        let DbOwner::Retirement { generation, phase } = previous else {
            *guard = previous;
            return Err(SchedulerDbRetirementError::InvalidPhase);
        };
        match transition(phase) {
            Ok((phase, value)) => {
                if let Some(phase) = phase {
                    *guard = DbOwner::Retirement { generation, phase };
                } else {
                    // The stopped transition returned actual successful
                    // release/abort; checkout itself never proves disposal.
                    *guard = DbOwner::Closed;
                }
                Ok(value)
            }
            Err((phase, error)) => {
                *guard = DbOwner::Retirement { generation, phase };
                Err(error)
            }
        }
    }
    pub fn drain_projection_retirement(
        &self,
    ) -> std::result::Result<DrainedSqliteRoleFacts, SchedulerDbRetirementError> {
        if self.has_native_retirement()? { return self.native_retirement_transition(None, |owner| owner.drain()); }
        self.retirement_transition(|phase| {
            let result = match phase {
                RetirementPhase::Failed(failure) => failure.retry_drain(),
                RetirementPhase::Drained(owner) => {
                    // Direct stopped intake is already SQL-free and barrier-held.
                    // A drain acknowledgement retrieves this SAME owner snapshot.
                    let facts = owner.role_facts().clone();
                    return Ok((Some(RetirementPhase::Drained(owner)), facts));
                }
            };
            match result {
                Ok(owner) => {
                    let facts = owner.role_facts().clone();
                    Ok((Some(RetirementPhase::Drained(owner)), facts))
                }
                Err(failure) => {
                    let error = SchedulerDbRetirementError::Host(failure.to_string());
                    Err((RetirementPhase::Failed(failure), error))
                }
            }
        })
    }
    /// Copied, non-authorizing observations; SAME native owner and barriers stay private.
    pub fn observe_projection_role_resolution(
        &self,
        role: SqliteRole,
    ) -> std::result::Result<
        lillux_sqlite::SqliteRoleResolutionObservation,
        SchedulerDbRetirementError,
    > {
        if self.has_native_retirement()? { return self.native_retirement_transition(None, |owner| owner.observe_role_resolution(role)); }
        self.retirement_transition(|phase| {
            let RetirementPhase::Drained(mut owner) = phase else {
                return Err((phase, SchedulerDbRetirementError::InvalidPhase));
            };
            match owner.observe_role_resolution(role) {
                Ok(observation) => Ok((Some(RetirementPhase::Drained(owner)), observation)),
                Err(error) => Err((
                    RetirementPhase::Drained(owner),
                    SchedulerDbRetirementError::Host(error.to_string()),
                )),
            }
        })
    }
    /// Domain must acknowledge signed resolution capacity and intent first.
    pub fn resolve_projection_role(
        &self,
        request: lillux_sqlite::SqliteRoleResolutionRequest,
    ) -> std::result::Result<lillux_sqlite::SqliteRoleResolutionOutcome, SchedulerDbRetirementError>
    {
        if self.has_native_retirement()? { return self.native_retirement_transition(None, |owner| owner.resolve(request)); }
        self.retirement_transition(|phase| {
            let RetirementPhase::Drained(mut owner) = phase else {
                return Err((phase, SchedulerDbRetirementError::InvalidPhase));
            };
            match owner.resolve_role(request) {
                Ok(outcome) => Ok((Some(RetirementPhase::Drained(owner)), outcome)),
                Err(error) => Err((
                    RetirementPhase::Drained(owner),
                    SchedulerDbRetirementError::Resolution(error),
                )),
            }
        })
    }
    pub fn quarantine_projection_role(
        &self,
        role: SqliteRole,
    ) -> std::result::Result<SqliteRoleMutationOutcome, SchedulerDbRetirementError> {
        self.mutate_projection_role(role, false)
    }
    pub fn purge_projection_role(
        &self,
        role: SqliteRole,
    ) -> std::result::Result<SqliteRoleMutationOutcome, SchedulerDbRetirementError> {
        self.mutate_projection_role(role, true)
    }
    fn mutate_projection_role(
        &self,
        role: SqliteRole,
        purge: bool,
    ) -> std::result::Result<SqliteRoleMutationOutcome, SchedulerDbRetirementError> {
        if self.has_native_retirement()? {
            return self.native_retirement_transition(None, |owner| if purge { owner.purge(role) } else { owner.quarantine(role) });
        }
        self.retirement_transition(|phase| {
            let RetirementPhase::Drained(mut owner) = phase else {
                return Err((phase, SchedulerDbRetirementError::InvalidPhase));
            };
            let result = if purge {
                owner.purge_role(role)
            } else {
                owner.quarantine_role(role)
            };
            match result {
                Ok(outcome) => Ok((Some(RetirementPhase::Drained(owner)), outcome)),
                Err(error) => Err((
                    RetirementPhase::Drained(owner),
                    SchedulerDbRetirementError::Role(error),
                )),
            }
        })
    }
    /// Native release remains separate from durable journal acknowledgement.
    pub fn release_projection_retirement(
        &self,
    ) -> std::result::Result<(), SchedulerDbRetirementError> {
        self.finish_projection_retirement(false)
    }
    /// Cleanup-only abort refuses any partially mutated namespace in the host.
    pub fn abort_projection_retirement(
        &self,
    ) -> std::result::Result<(), SchedulerDbRetirementError> {
        self.finish_projection_retirement(true)
    }
    fn finish_projection_retirement(
        &self,
        abort: bool,
    ) -> std::result::Result<(), SchedulerDbRetirementError> {
        if self.has_native_retirement()? { return self.finish_native_retirement(abort); }
        self.retirement_transition(|phase| {
            let result = match phase {
                RetirementPhase::Drained(owner) if abort => owner.abort(),
                RetirementPhase::Drained(owner) => owner.release(),
                RetirementPhase::Failed(failure) if abort => failure.abort(),
                RetirementPhase::Failed(failure) => failure.retry_release(),
            };
            match result {
                Ok(()) => Ok((None, ())),
                Err(failure) => {
                    let error = SchedulerDbRetirementError::Host(failure.to_string());
                    Err((RetirementPhase::Failed(failure), error))
                }
            }
        })
    }
    pub fn require_runtime(&self, runtime: &PinnedDirectory) -> Result<()> {
        self.with_live_owner(|owner| owner.require_scheduler_runtime(runtime))
    }
    /// Actual receipts stay cached for the original factory census, including
    /// failed acquisition releases. Retry never reopens or restores admission.
    pub fn close(&self) -> std::result::Result<(), SchedulerDbCloseError> {
        self.require_process().map_err(|_| SchedulerDbCloseError::OriginalProcess)?;
        let mut guard = self.held.inner.try_lock().map_err(|_| SchedulerDbCloseError::Poisoned)?;
        let previous = std::mem::replace(&mut *guard, DbOwner::CheckedOut);
        let result = match previous {
            DbOwner::Live(owner) | DbOwner::Stale(owner) => owner.close(),
            DbOwner::Closing(failure) => failure.retry(),
            DbOwner::Opening(failure) => {
                match failure.retry_close() {
                    Ok(receipt) => *guard = DbOwner::Terminal(OriginalNativeConnectionTerminal::Released(receipt)),
                    Err(failure) => {
                        *guard = DbOwner::Opening(failure);
                        return Err(SchedulerDbCloseError::Native("original scheduler acquisition release retained".into()));
                    }
                }
                return Ok(());
            }
            terminal @ DbOwner::Terminal(_) | terminal @ DbOwner::NeverOpened(_) => {
                *guard = terminal; return Ok(());
            }
            DbOwner::BeforeOpen(inputs) => { *guard = DbOwner::NeverOpened(inputs); return Ok(()); }
            retirement @ DbOwner::Retirement { .. } | retirement @ DbOwner::NativeRetirement { .. } => {
                *guard = retirement; return Err(SchedulerDbCloseError::RetirementPending);
            }
            DbOwner::CheckedOut => {
                *guard = DbOwner::CheckedOut;
                return Err(SchedulerDbCloseError::Native("scheduler effect custody remains checked out".into()));
            }
            DbOwner::Closed => { *guard = DbOwner::Closed; return Ok(()); }
        };
        match result {
            Ok(receipt) => { *guard = DbOwner::Terminal(OriginalNativeConnectionTerminal::Closed(receipt)); Ok(()) }
            Err(failure) => {
                *guard = DbOwner::Closing(failure);
                Err(SchedulerDbCloseError::Native("original scheduler close retained".into()))
            }
        }
    }
    /// Exact actual receipts only; an unissued acquisition disposition is
    /// retained but excluded from the cumulative native ticket census.
    pub fn retained_native_terminals(&self) -> Result<Vec<OriginalNativeConnectionTerminal>> {
        self.require_process()?;
        let original = self.held.original.as_ref().context("scheduler has no original native factory")?;
        let guard = self.held.inner.try_lock().map_err(|_| anyhow!("scheduler terminal owner busy"))?;
        let mut terminals = vec![original.reference.terminal().clone()];
        match &*guard {
            DbOwner::Terminal(terminal) => {
                terminal.require_original(&original.factory)?;
                if !matches!(terminal, OriginalNativeConnectionTerminal::Released(receipt) if receipt.native_ticket().is_none()) {
                    terminals.push(terminal.clone());
                }
            }
            DbOwner::NeverOpened(_) => {},
            _ => anyhow::bail!("scheduler native owner is not terminal"),
        }
        original.reference.terminal().require_original(&original.factory)?;
        Ok(terminals)
    }
    /// Clear only execution-derived scheduler history. Signed schedule YAML
    /// remains authoritative, and `schedule_specs` is deliberately preserved.
    /// The projection marker is published current and empty in the same
    /// transaction so a crash cannot expose a partially cleared fire view.
    pub fn discard_fire_history(&self, dry_run: bool) -> Result<SchedulerFireHistoryDiscardReport> {
        self.with_sql_mut(|conn| {
            fn count(conn: &Connection, table: &str) -> Result<usize> {
                let rows: i64 =
                    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                        row.get(0)
                    })?;
                usize::try_from(rows).context("scheduler fire-history row count is invalid")
            }
            if dry_run {
                return Ok(SchedulerFireHistoryDiscardReport {
                    fires: count(conn, "schedule_fires")?,
                    outbox_snapshots: count(conn, "schedule_fire_outbox")?,
                    cursors: count(conn, "schedule_cursors")?,
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
        })
    }

    fn require_process(&self) -> Result<()> {
        anyhow::ensure!(!self.held.is_foreign(), "inherited scheduler owner");
        if let Some(original) = self.held.original.as_ref() { original.factory.require_original_process()?; }
        Ok(())
    }
    /// Wait for this store's single SQL owner. The timer, detached fire
    /// admission, terminal finalizers and API readers all share one store
    /// across threads, so a scope in progress on another thread is waited
    /// out rather than refused. A scope that re-enters the same store on its
    /// own thread is refused instead of deadlocking.
    fn lock_sql_owner(&self) -> Result<SqlOwnerScope<'_>> {
        let mark = SqlScopeMark::enter(self)?;
        let guard = self
            .held.inner
            .lock()
            .map_err(|_| anyhow!("scheduler SQL owner poisoned"))?;
        Ok(SqlOwnerScope { guard, _mark: mark })
    }
    fn with_live_owner<T>(&self, operation: impl FnOnce(&OriginalNativeConnection) -> Result<T>) -> Result<T> {
        self.require_process()?;
        let scope = self.lock_sql_owner()?;
        match &*scope.guard {
            DbOwner::Live(owner) => operation(owner),
            _ => anyhow::bail!("scheduler SQL admission is closed"),
        }
    }
    fn with_sql<T>(&self, operation: impl for<'c> FnOnce(&'c Connection) -> Result<T>) -> Result<T> {
        self.require_process()?;
        let mut scope = self.lock_sql_owner()?;
        let result = match &*scope.guard {
            DbOwner::Live(owner) => owner.with_sql(operation),
            _ => anyhow::bail!("scheduler SQL admission is closed"),
        };
        close_if_transaction_retained(&mut scope.guard);
        result
    }
    fn with_sql_mut<T>(&self, operation: impl for<'c> FnOnce(&'c mut Connection) -> Result<T>) -> Result<T> {
        self.require_process()?;
        let mut scope = self.lock_sql_owner()?;
        let result = match &mut *scope.guard {
            DbOwner::Live(owner) => owner.with_sql_mut(operation),
            _ => anyhow::bail!("scheduler SQL admission is closed"),
        };
        close_if_transaction_retained(&mut scope.guard);
        result
    }
    // ── schedule_specs ──────────────────────────────────────────

    pub fn upsert_spec(&self, rec: &ScheduleSpecRecord) -> Result<()> {
        self.with_sql(|conn| {
            validate_schedule_spec_record(rec)?;
            upsert_spec_conn(conn, rec)
                .with_context(|| format!("upsert_spec failed for {}", rec.schedule_id))?;
            Ok(())
        })
    }

    /// Atomically replace the complete schedule-spec projection.
    ///
    /// Callers must fully verify and validate every signed source before this
    /// method is entered. One malformed source therefore leaves the previous
    /// complete projection untouched; a successful transaction publishes the
    /// whole new set and removes stale cursors/specs together.
    pub fn replace_specs(&self, records: &[ScheduleSpecRecord]) -> Result<usize> {
        self.with_sql_mut(|conn| {
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
        })
    }

    pub fn delete_spec(&self, schedule_id: &str) -> Result<bool> {
        self.with_sql(|conn| {
            conn.execute(
                "DELETE FROM schedule_cursors WHERE schedule_id = ?1",
                params![schedule_id],
            )?;
            let n = conn.execute(
                "DELETE FROM schedule_specs WHERE schedule_id = ?1",
                params![schedule_id],
            )?;
            Ok(n > 0)
        })
    }

    pub fn get_spec(&self, schedule_id: &str) -> Result<Option<ScheduleSpecRecord>> {
        self.with_sql(|conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT schedule_id, item_ref, ref_bindings, params, schedule_type, expression,
                        timezone, misfire_policy, overlap_policy, enabled,
                        project_root, signer_fingerprint, spec_hash, registered_at,
                        execution, lateness_grace_secs
                 FROM schedule_specs WHERE schedule_id = ?1",
            )?;
            stmt.query_row(params![schedule_id], row_to_spec)
                .optional()
                .map_err(Into::into)
        })
    }

    pub fn load_enabled_specs(&self) -> Result<Vec<ScheduleSpecRecord>> {
        self.with_sql(|conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT schedule_id, item_ref, ref_bindings, params, schedule_type, expression,
                        timezone, misfire_policy, overlap_policy, enabled,
                        project_root, signer_fingerprint, spec_hash, registered_at,
                        execution, lateness_grace_secs
                 FROM schedule_specs WHERE enabled = 1",
            )?;
            let rows = stmt.query_map([], row_to_spec)?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
    }

    pub fn list_specs(
        &self,
        enabled_only: bool,
        schedule_type: Option<&str>,
    ) -> Result<Vec<ScheduleSpecRecord>> {
        self.with_sql(|conn| {
            let sel = "SELECT schedule_id, item_ref, ref_bindings, params, schedule_type, expression,
                              timezone, misfire_policy, overlap_policy, enabled,
                              project_root, signer_fingerprint, spec_hash, registered_at,
                              execution, lateness_grace_secs";
            let sql = match (enabled_only, schedule_type) {
                (true, Some(_)) => {
                    format!("{sel} FROM schedule_specs WHERE enabled = 1 AND schedule_type = ?")
                }
                (true, None) => format!("{sel} FROM schedule_specs WHERE enabled = 1"),
                (false, Some(_)) => format!("{sel} FROM schedule_specs WHERE schedule_type = ?"),
                (false, None) => format!("{sel} FROM schedule_specs"),
            };
            let mut stmt = conn.prepare_cached(&sql)?;
            let rows: Vec<ScheduleSpecRecord> = if let Some(st) = schedule_type {
                stmt.query_map(params![st], row_to_spec)?
                    .collect::<Result<Vec<_>, _>>()?
            } else {
                stmt.query_map([], row_to_spec)?
                    .collect::<Result<Vec<_>, _>>()?
            };
            Ok(rows)
        })
    }

    /// List specs with optional acting-principal filtering.
    ///
    /// When `filter_requester` is `Some(fp)`, only schedules with
    /// `execution.authority.principal_id() = fp` are returned. `None` returns all
    /// schedules (internal callers that intentionally request an unfiltered view).
    pub fn list_specs_filtered(
        &self,
        enabled_only: bool,
        schedule_type: Option<&str>,
        filter_requester: Option<&str>,
    ) -> Result<Vec<ScheduleSpecRecord>> {
        self.with_sql(|conn| {
            let sel = "SELECT schedule_id, item_ref, ref_bindings, params, schedule_type, expression,
                              timezone, misfire_policy, overlap_policy, enabled,
                              project_root, signer_fingerprint, spec_hash, registered_at,
                              execution, lateness_grace_secs";

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
            let where_clause = if conditions.is_empty() {
                String::new()
            } else {
                format!(" WHERE {}", conditions.join(" AND "))
            };
            let sql = format!("{sel} FROM schedule_specs{where_clause}");
            let mut stmt = conn.prepare_cached(&sql)?;

            let params: Vec<&dyn rusqlite::ToSql> = param_values
                .iter()
                .map(|v| v as &dyn rusqlite::ToSql)
                .collect();
            let mut rows: Vec<ScheduleSpecRecord> = stmt
                .query_map(params.as_slice(), row_to_spec)?
                .collect::<Result<Vec<_>, _>>()?;
            if let Some(fingerprint) = filter_requester {
                rows.retain(|record| record.execution.authority.principal_id() == fingerprint);
            }
            Ok(rows)
        })
    }

    // ── schedule_cursors ────────────────────────────────────────

    pub fn get_cursor(&self, schedule_id: &str) -> Result<Option<ScheduleCursorRecord>> {
        self.with_sql(|conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT schedule_id, spec_hash, last_scheduled_at, next_fire_at,
                        last_evaluated_at, updated_at
                 FROM schedule_cursors WHERE schedule_id = ?1",
            )?;
            stmt.query_row(params![schedule_id], row_to_cursor)
                .optional()
                .map_err(Into::into)
        })
    }

    pub fn get_cursors_batch(
        &self,
        schedule_ids: &[String],
    ) -> Result<std::collections::HashMap<String, ScheduleCursorRecord>> {
        self.with_sql(|conn| {
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
        })
    }

    pub fn upsert_cursor(&self, rec: &ScheduleCursorRecord) -> Result<()> {
        self.with_sql(|conn| {
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
                    rec.schedule_id,
                    rec.spec_hash,
                    rec.last_scheduled_at,
                    rec.next_fire_at,
                    rec.last_evaluated_at,
                    rec.updated_at,
                ],
            )?;
            Ok(())
        })
    }

    pub fn delete_cursor(&self, schedule_id: &str) -> Result<bool> {
        self.with_sql(|conn| {
            let n = conn.execute(
                "DELETE FROM schedule_cursors WHERE schedule_id = ?1",
                params![schedule_id],
            )?;
            Ok(n > 0)
        })
    }

    /// Whether the persisted fire projection completed its last full replay.
    /// Normal daemon startup trusts the indexed fire/cursor state only when
    /// this marker is set. A missing, duplicate, or non-boolean marker fails
    /// closed and routes startup through the explicit full replay path.
    pub fn fire_projection_is_current(&self) -> Result<bool> {
        self.with_sql(|conn| {
            fire_projection_is_current_conn(conn)
        })
    }

    /// Invalidate the fire projection before a destructive full replay.
    /// Committing the invalid marker and clear together ensures a crash can
    /// never leave a partial replay looking current on the next startup.
    pub fn begin_fire_projection_rebuild(&self) -> Result<()> {
        self.with_sql_mut(|conn| {
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
        })
    }

    /// Publish a successfully replayed fire projection. Cursor repair remains
    /// incremental: missing/stale schedule cursors are reconstructed from the
    /// indexed latest fire and do not require another JSONL walk.
    pub fn finish_fire_projection_rebuild(&self) -> Result<()> {
        self.with_sql(|conn| {
            let changed = conn.execute(
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
        })
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
        self.with_sql_mut(|conn| {
            validate_fire_record(rec)?;
            let tx = conn.transaction()?;
            require_current_fire_projection_conn(&tx)?;
            if let Some(previous) = get_fire_conn(&tx, &rec.fire_id)? {
                // The completion hook and repair sweep can observe the same
                // dispatched fire. Preserve the first terminal snapshot atomically
                // when a repeated finalizer differs only in completion time.
                // Identity, authority, status and outcome must still match exactly.
                if matches!(
                    previous.status.as_str(),
                    "completed" | "failed" | "cancelled"
                ) {
                    let mut repeated = rec.clone();
                    repeated.completed_at = previous.completed_at;
                    if repeated == previous {
                        return Ok(());
                    }
                }
                rec.validate_transition_from(&previous)?;
            }
            upsert_fire_conn(&tx, rec)
                .with_context(|| format!("upsert_fire failed for {}", rec.fire_id))?;
            enqueue_current_fire_snapshot_conn(&tx, &rec.fire_id)?;
            refresh_cursor_for_schedule_conn(&tx, &rec.schedule_id, lillux::time::timestamp_millis())?;
            tx.commit()?;
            Ok(())
        })
    }

    /// Apply one already-durable JSONL snapshot during a full projection
    /// replay. Rebuild must not enqueue the authority back into its own outbox.
    pub(crate) fn project_fire_from_journal(&self, rec: &FireRecord) -> Result<()> {
        self.with_sql(|conn| {
            validate_fire_record(rec)?;
            upsert_fire_conn(conn, rec)
                .with_context(|| format!("project fire from journal failed for {}", rec.fire_id))
        })
    }

    /// Atomic claim: INSERT if absent. Returns true if the insert succeeded
    /// (fire was claimed), false if it already existed.
    pub(crate) fn claim_fire(&self, rec: &FireRecord) -> Result<bool> {
        self.with_sql_mut(|conn| {
            validate_fire_record(rec)?;
            let project_authority = rec
                .project_authority
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?;
            let tx = conn.transaction()?;
            require_current_fire_projection_conn(&tx)?;
            let changed = tx
                .execute(
                    "INSERT OR IGNORE INTO schedule_fires
                    (fire_id, schedule_id, scheduled_at, reserved_at, dispatched_at,
                     completed_at, thread_id, status, trigger_reason, outcome,
                     signer_fingerprint, schedule_spec_hash, project_authority,
                     admitted_capsule_hash)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
                    params![
                        rec.fire_id,
                        rec.schedule_id,
                        rec.scheduled_at,
                        rec.reserved_at,
                        rec.dispatched_at,
                        rec.completed_at,
                        rec.thread_id,
                        rec.status,
                        rec.trigger_reason,
                        rec.outcome,
                        rec.signer_fingerprint,
                        rec.schedule_spec_hash,
                        project_authority,
                        rec.admitted_capsule_hash,
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
        })
    }

    /// Reclaim a fire that was persisted but never got a running thread.
    /// The caller has already proved the deterministic thread is absent. Fire
    /// dispatch identity is immutable, so reclaim is an eligibility check; it
    /// never rewrites reservation identity, thread id, trigger reason, or the journal.
    pub(crate) fn reclaim_fire(&self, fire_id: &str) -> Result<bool> {
        self.with_sql(|conn| {
            require_current_fire_projection_conn(conn)?;
            let Some(record) = get_fire_conn(conn, fire_id)? else {
                return Ok(false);
            };
            record.validate()?;
            Ok(matches!(record.status.as_str(), "reserved" | "dispatched"))
        })
    }

    /// Number of committed fire snapshots not yet synced into JSONL.
    pub fn pending_fire_outbox(&self) -> Result<usize> {
        self.with_sql(|conn| {
            let count: i64 =
                conn
                    .query_row("SELECT COUNT(*) FROM schedule_fire_outbox", [], |row| {
                        row.get(0)
                    })?;
            usize::try_from(count).context("negative scheduler fire outbox count")
        })
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
        self.require_process()?;
        // Concurrent drains queue behind one another so each snapshot is
        // appended to its journal exactly once and in commit order.
        let _drain = self
            .outbox_drain
            .lock()
            .map_err(|error| anyhow!("scheduler outbox drain lock poisoned: {error}"))?;
        let schedules_directory =
            runtime_directory.open_or_create_child(std::ffi::OsStr::new("schedules"), 0o700)?;
        let mut schedule_directories = std::collections::BTreeMap::new();
        let mut drained = 0usize;

        loop {
            let next: Option<(i64, String, String, String)> = self.with_sql(|conn| {
                Ok(conn.query_row(
                    "SELECT sequence, fire_id, schedule_id, snapshot_json
                     FROM schedule_fire_outbox
                     ORDER BY sequence ASC
                     LIMIT 1",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .optional()?)
            })?;
            let Some((sequence, fire_id, schedule_id, snapshot_json)) = next else {
                break;
            };

            validate_schedule_id(&schedule_id).with_context(|| {
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

            let changed = self.with_sql(|conn| Ok(conn.execute(
                "DELETE FROM schedule_fire_outbox
                 WHERE sequence = ?1 AND fire_id = ?2 AND schedule_id = ?3
                   AND snapshot_json = ?4",
                params![sequence, fire_id, schedule_id, snapshot_json],
            )?))?;
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
        self.with_sql(|conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT fire_id, schedule_id, scheduled_at, reserved_at, dispatched_at,
                        completed_at, thread_id, status, trigger_reason, outcome,
                        signer_fingerprint, schedule_spec_hash, project_authority,
                        admitted_capsule_hash
                 FROM schedule_fires WHERE fire_id = ?1",
            )?;
            stmt.query_row(params![fire_id], row_to_fire)
                .optional()
                .map_err(Into::into)
        })
    }

    /// Batch-get last fire for multiple schedules. Returns a map from
    /// schedule_id → FireRecord. Used by scheduler_list to avoid N+1 queries.
    pub fn get_last_fires_batch(
        &self,
        schedule_ids: &[String],
    ) -> Result<std::collections::HashMap<String, FireRecord>> {
        self.with_sql(|conn| {
            if schedule_ids.is_empty() {
                return Ok(std::collections::HashMap::new());
            }
            let placeholders: Vec<String> = schedule_ids
                .iter()
                .enumerate()
                .map(|(i, _)| format!("?{}", i + 1))
                .collect();
            let sql = format!(
                "SELECT fire_id, schedule_id, scheduled_at, reserved_at, dispatched_at,
                        completed_at, thread_id, status, trigger_reason, outcome,
                        signer_fingerprint, schedule_spec_hash, project_authority,
                        admitted_capsule_hash
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
        })
    }

    pub fn get_last_fire(&self, schedule_id: &str) -> Result<Option<FireRecord>> {
        self.with_sql(|conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT fire_id, schedule_id, scheduled_at, reserved_at, dispatched_at,
                        completed_at, thread_id, status, trigger_reason, outcome,
                        signer_fingerprint, schedule_spec_hash, project_authority,
                        admitted_capsule_hash
                 FROM schedule_fires
                 WHERE schedule_id = ?1
                 ORDER BY scheduled_at DESC, fire_id DESC LIMIT 1",
            )?;
            stmt.query_row(params![schedule_id], row_to_fire)
                .optional()
                .map_err(Into::into)
        })
    }

    pub fn get_inflight_fires(&self) -> Result<Vec<FireRecord>> {
        self.with_sql(|conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT fire_id, schedule_id, scheduled_at, reserved_at, dispatched_at,
                        completed_at, thread_id, status, trigger_reason, outcome,
                        signer_fingerprint, schedule_spec_hash, project_authority,
                        admitted_capsule_hash
                 FROM schedule_fires WHERE status IN ('reserved', 'dispatched')",
            )?;
            let rows = stmt.query_map([], row_to_fire)?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
    }

    /// Immutable generations referenced only by scheduler policy or by a
    /// pre-launch fire binding. These are operational GC roots until the
    /// schedule is disabled or the fire acquires ordinary thread authority.
    pub fn operational_snapshot_roots(&self) -> Result<Vec<String>> {
        let mut roots = std::collections::BTreeSet::new();
        for spec in self.load_enabled_specs()? {
            if let ryeos_engine::execution_contract::ProjectExecutionPolicy::Pinned {
                source: ryeos_engine::execution_contract::PinnedSource::Snapshot { hash },
                ..
            } = spec.execution.policy.project
            {
                if !lillux::valid_hash(&hash) {
                    anyhow::bail!(
                        "enabled schedule {} carries a noncanonical snapshot root",
                        spec.schedule_id
                    );
                }
                roots.insert(hash);
            }
        }
        for fire in self.get_inflight_fires()? {
            if let Some(ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
                base_snapshot_hash,
                snapshot_hash,
                ..
            }) = fire.project_authority
            {
                roots.insert(base_snapshot_hash);
                roots.insert(snapshot_hash);
            }
        }
        Ok(roots.into_iter().collect())
    }

    pub fn get_inflight_for_schedule(&self, schedule_id: &str) -> Result<Option<FireRecord>> {
        self.with_sql(|conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT fire_id, schedule_id, scheduled_at, reserved_at, dispatched_at,
                        completed_at, thread_id, status, trigger_reason, outcome,
                        signer_fingerprint, schedule_spec_hash, project_authority,
                        admitted_capsule_hash
                 FROM schedule_fires
                 WHERE schedule_id = ?1 AND status IN ('reserved', 'dispatched')
                 ORDER BY scheduled_at DESC, fire_id DESC LIMIT 1",
            )?;
            stmt.query_row(params![schedule_id], row_to_fire)
                .optional()
                .map_err(Into::into)
        })
    }

    pub fn find_fire_by_thread(&self, thread_id: &str) -> Result<Option<FireRecord>> {
        self.with_sql(|conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT fire_id, schedule_id, scheduled_at, reserved_at, dispatched_at,
                        completed_at, thread_id, status, trigger_reason, outcome,
                        signer_fingerprint, schedule_spec_hash, project_authority,
                        admitted_capsule_hash
                 FROM schedule_fires
                 WHERE thread_id = ?1 AND status IN ('reserved', 'dispatched')",
            )?;
            stmt.query_row(params![thread_id], row_to_fire)
                .optional()
                .map_err(Into::into)
        })
    }

    /// Find dispatched fires older than `threshold_secs` that may need repair.
    /// Used by the periodic repair sweep to finalize stale fires.
    pub fn find_stale_dispatched_fires(&self, threshold_secs: i64) -> Result<Vec<FireRecord>> {
        self.with_sql(|conn| {
            let cutoff = lillux::time::timestamp_millis() - (threshold_secs * 1000);
            let mut stmt = conn.prepare_cached(
                "SELECT fire_id, schedule_id, scheduled_at, reserved_at, dispatched_at,
                        completed_at, thread_id, status, trigger_reason, outcome,
                        signer_fingerprint, schedule_spec_hash, project_authority,
                        admitted_capsule_hash
                 FROM schedule_fires
                 WHERE status = 'dispatched' AND dispatched_at IS NOT NULL AND dispatched_at < ?1",
            )?;
            let rows = stmt.query_map(params![cutoff], row_to_fire)?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
    }

    /// Count fires for a schedule (optionally filtered by status).
    pub fn count_fires(&self, schedule_id: &str, status_filter: Option<&str>) -> Result<usize> {
        self.with_sql(|conn| {
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
        })
    }

    /// Commit an invalid projection marker before a non-dry fire-journal
    /// retention rewrite. If the process stops before the final transaction,
    /// startup sees this marker and rebuilds SQLite from exactly the retained
    /// journals.
    pub fn begin_fire_retention(&self) -> Result<()> {
        self.with_sql_mut(|conn| {
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
        })
    }

    /// Read-only proof that the exact portable-journal retention selection can
    /// be applied to the current SQLite projection. Dry-run uses this same
    /// target validation as the destructive transaction without invalidating
    /// the projection marker or deleting rows.
    pub fn verify_fire_retention_targets(
        &self,
        targets: &[ryeos_state::gc::retention::FireRetentionTarget],
    ) -> Result<()> {
        self.with_sql(|conn| {
            if !fire_projection_is_current_conn(conn)? {
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
            validate_fire_retention_targets_conn(conn, targets)?;
            Ok(())
        })
    }

    /// Delete exactly the identities chosen by the journal retention planner,
    /// refresh affected cursors, and republish the current marker in one final
    /// transaction. No SQL age/count selection is performed here.
    pub fn finish_fire_retention(
        &self,
        targets: &[ryeos_state::gc::retention::FireRetentionTarget],
    ) -> Result<usize> {
        self.with_sql_mut(|conn| {
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
        })
    }

    /// Finish an explicit schedule-history purge after its directory has been
    /// removed under the scheduler write gate. This is not retention selection:
    /// the operator requested the entire named history be removed. The invalid
    /// marker established by [`Self::begin_fire_retention`] still makes a crash
    /// between filesystem removal and this transaction rebuild-safe.
    pub fn finish_schedule_fire_purge(&self, schedule_id: &str) -> Result<usize> {
        self.with_sql_mut(|conn| {
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
        })
    }

    pub fn list_fires(
        &self,
        schedule_id: &str,
        status_filter: Option<&str>,
        limit: usize,
    ) -> Result<(Vec<FireRecord>, usize)> {
        self.with_sql(|conn| {

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
                    "SELECT fire_id, schedule_id, scheduled_at, reserved_at, dispatched_at,
                                   completed_at, thread_id, status, trigger_reason, outcome,
                                   signer_fingerprint, schedule_spec_hash, project_authority,
                                   admitted_capsule_hash
                            FROM schedule_fires
                            WHERE schedule_id = ?1 AND status = ?2
                            ORDER BY scheduled_at DESC, fire_id DESC LIMIT ?3"
                }
                None => {
                    "SELECT fire_id, schedule_id, scheduled_at, reserved_at, dispatched_at,
                                completed_at, thread_id, status, trigger_reason, outcome,
                                signer_fingerprint, schedule_spec_hash, project_authority,
                                admitted_capsule_hash
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
        })
    }
}

// ── Row mappers ─────────────────────────────────────────────────────

fn upsert_spec_conn(conn: &Connection, rec: &ScheduleSpecRecord) -> Result<()> {
    let execution_json = serde_json::to_string(&rec.execution)?;
    let ref_bindings_json = serde_json::to_string(&rec.ref_bindings)?;
    conn.execute(
        "INSERT INTO schedule_specs
            (schedule_id, item_ref, ref_bindings, params, schedule_type, expression,
             timezone, misfire_policy, overlap_policy, enabled,
             project_root, signer_fingerprint, spec_hash, registered_at,
             execution, lateness_grace_secs)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)
         ON CONFLICT(schedule_id) DO UPDATE SET
            item_ref=excluded.item_ref, ref_bindings=excluded.ref_bindings,
            params=excluded.params,
            schedule_type=excluded.schedule_type, expression=excluded.expression,
            timezone=excluded.timezone, misfire_policy=excluded.misfire_policy,
            overlap_policy=excluded.overlap_policy, enabled=excluded.enabled,
            project_root=excluded.project_root, signer_fingerprint=excluded.signer_fingerprint,
            spec_hash=excluded.spec_hash, registered_at=excluded.registered_at,
            execution=excluded.execution,
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
            execution_json,
            rec.lateness_grace_secs,
        ],
    )?;
    Ok(())
}

fn upsert_fire_conn(conn: &Connection, rec: &FireRecord) -> Result<()> {
    let project_authority = rec
        .project_authority
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?;
    conn.execute(
        "INSERT INTO schedule_fires
            (fire_id, schedule_id, scheduled_at, reserved_at, dispatched_at,
             completed_at, thread_id, status, trigger_reason, outcome,
             signer_fingerprint, schedule_spec_hash, project_authority,
             admitted_capsule_hash)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)
         ON CONFLICT(fire_id) DO UPDATE SET
            schedule_id=excluded.schedule_id,
            scheduled_at=excluded.scheduled_at,
            reserved_at=excluded.reserved_at,
            dispatched_at=excluded.dispatched_at,
            completed_at=excluded.completed_at,
            thread_id=excluded.thread_id,
            status=excluded.status,
            trigger_reason=excluded.trigger_reason,
            outcome=excluded.outcome,
            signer_fingerprint=excluded.signer_fingerprint,
            schedule_spec_hash=excluded.schedule_spec_hash,
            project_authority=excluded.project_authority,
            admitted_capsule_hash=excluded.admitted_capsule_hash",
        params![
            rec.fire_id,
            rec.schedule_id,
            rec.scheduled_at,
            rec.reserved_at,
            rec.dispatched_at,
            rec.completed_at,
            rec.thread_id,
            rec.status,
            rec.trigger_reason,
            rec.outcome,
            rec.signer_fingerprint,
            rec.schedule_spec_hash,
            project_authority,
            rec.admitted_capsule_hash,
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
    let execution_json: String = row.get("execution")?;
    let execution = serde_json::from_str(&execution_json).map_err(|e| {
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
        execution,
        lateness_grace_secs: row.get("lateness_grace_secs")?,
    })
}

fn row_to_fire(row: &rusqlite::Row<'_>) -> Result<FireRecord, rusqlite::Error> {
    let project_authority_json: Option<String> = row.get("project_authority")?;
    let project_authority = project_authority_json
        .map(|value| {
            serde_json::from_str(&value).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })
        })
        .transpose()?;
    Ok(FireRecord {
        fire_id: row.get("fire_id")?,
        schedule_id: row.get("schedule_id")?,
        scheduled_at: row.get("scheduled_at")?,
        reserved_at: row.get("reserved_at")?,
        dispatched_at: row.get("dispatched_at")?,
        completed_at: row.get("completed_at")?,
        thread_id: row.get("thread_id")?,
        status: row.get("status")?,
        trigger_reason: row.get("trigger_reason")?,
        outcome: row.get("outcome")?,
        signer_fingerprint: row.get("signer_fingerprint")?,
        schedule_spec_hash: row.get("schedule_spec_hash")?,
        project_authority,
        admitted_capsule_hash: row.get("admitted_capsule_hash")?,
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
        "SELECT fire_id, schedule_id, scheduled_at, reserved_at, dispatched_at,
                completed_at, thread_id, status, trigger_reason, outcome,
                signer_fingerprint, schedule_spec_hash, project_authority,
                admitted_capsule_hash
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
                execution, lateness_grace_secs
         FROM schedule_specs WHERE schedule_id = ?1",
    )?;
    stmt.query_row(params![schedule_id], row_to_spec)
        .optional()
        .map_err(Into::into)
}

fn get_last_fire_conn(conn: &Connection, schedule_id: &str) -> Result<Option<FireRecord>> {
    let mut stmt = conn.prepare_cached(
        "SELECT fire_id, schedule_id, scheduled_at, reserved_at, dispatched_at,
                completed_at, thread_id, status, trigger_reason, outcome,
                signer_fingerprint, schedule_spec_hash, project_authority,
                admitted_capsule_hash
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
    use crate::test_native::TestSchedulerHost;
    use anyhow::ensure;
    use lillux_sqlite::{OriginalNativeBoundInputs, OriginalNativeDomain, OriginalNativeTransientInputs};

    /// Open, creating it if absent, the store at `path` on `host`.
    fn open_test_path(
        host: &TestSchedulerHost,
        path: &Path,
    ) -> std::result::Result<SchedulerDb, SchedulerDbOpenFailure> {
        host.open_at(path.parent().unwrap())
    }

    /// Create the store at `path` and close it into the census, leaving its
    /// files for the test to inspect or alter.
    fn create_closed_test_path(host: &TestSchedulerHost, path: &Path) {
        let db = open_test_path(host, path).unwrap();
        host.settle(db);
    }

    fn inspect_test_path(path: &Path) -> Result<SchedulerFireHistoryAccounting> {
        let snapshot = Arc::new(tempfile::tempdir()?);
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let source = path.with_file_name(format!("scheduler.sqlite3{suffix}"));
            if source.exists() {
                std::fs::copy(&source, snapshot.path().join(source.file_name().unwrap()))?;
            }
        }
        let pin = Arc::new(PinnedDirectory::open(snapshot.path())?.unwrap());
        // Explicit bounded mechanism fixture, not platform quota authority.
        struct FixtureProduction(std::sync::atomic::AtomicU8);
        impl lillux_sqlite::SqliteInspectionProductionControl for FixtureProduction {
            fn checkpoint(&self) -> Result<()> {
                ensure!(
                    self.0.load(std::sync::atomic::Ordering::SeqCst) == 1,
                    "standalone fixture native interval inactive"
                );
                Ok(())
            }
            fn complete(&self) -> Result<()> {
                match self.0.compare_exchange(
                    1,
                    2,
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                ) {
                    Ok(_) | Err(2) => Ok(()),
                    _ => anyhow::bail!("standalone fixture native interval unresolved"),
                }
            }
            fn abandoned(&self) {
                let _ = self.0.compare_exchange(
                    1,
                    3,
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                );
            }
        }
        struct FixtureSqlWork {
            root: Arc<PinnedDirectory>,
            steps: std::sync::atomic::AtomicU64,
            production: Arc<FixtureProduction>,
        }
        impl lillux_sqlite::SqliteInspectionSqlWork for FixtureSqlWork {
            fn begin_native_production(
                &self,
                root: &PinnedDirectory,
                name: &std::ffi::OsStr,
            ) -> Result<lillux_sqlite::SqliteInspectionProductionPermit> {
                self.require_destination(root)?;
                ensure!(
                    name == std::ffi::OsStr::new("scheduler.sqlite3"),
                    "fixture native production selects another basename"
                );
                self.production
                    .0
                    .compare_exchange(
                        0,
                        1,
                        std::sync::atomic::Ordering::SeqCst,
                        std::sync::atomic::Ordering::SeqCst,
                    )
                    .map_err(|_| anyhow!("standalone fixture native interval already admitted"))?;
                Ok(lillux_sqlite::SqliteInspectionProductionPermit::issued(
                    lillux_sqlite::SqliteInspectionProductionKind::Native,
                    self.production.clone(),
                ))
            }
            fn require_copy_work(
                &self,
                _work: &Arc<dyn lillux_sqlite::SqliteInspectionCopyWork>,
            ) -> Result<()> {
                anyhow::bail!("standalone SQL fixture has no attested original copy-work owner")
            }
            fn require_destination(&self, root: &PinnedDirectory) -> Result<()> {
                self.root.ensure_path_binding()?;
                ensure!(
                    self.root.is_same_directory(root)?,
                    "fixture selects another copy"
                );
                Ok(())
            }
            fn checkpoint(&self) -> Result<()> {
                self.root.ensure_path_binding()
            }
            fn debit_vm_steps(&self, steps: u64) -> Result<()> {
                self.steps
                    .fetch_update(
                        std::sync::atomic::Ordering::SeqCst,
                        std::sync::atomic::Ordering::SeqCst,
                        |prior| prior.checked_add(steps).filter(|total| *total <= 1_000_000),
                    )
                    .map_err(|_| anyhow!("bounded fixture SQL work exhausted"))?;
                Ok(())
            }
        }
        let work: Arc<dyn lillux_sqlite::SqliteInspectionSqlWork> = Arc::new(FixtureSqlWork {
            root: pin.clone(),
            steps: std::sync::atomic::AtomicU64::new(0),
            production: Arc::new(FixtureProduction(std::sync::atomic::AtomicU8::new(0))),
        });
        let lease: lillux_sqlite::SqliteCopyLease = snapshot;
        let host = ryeos_state::test_support::shared_test_host();
        let reference = scheduler_schema_definition();
        let name = std::ffi::OsStr::new("scheduler.sqlite3");
        // SAFETY: test-only issuance under the shared host's own funded factory.
        let inputs = unsafe {
            OriginalNativeBoundInputs::issued(
                pin.clone(),
                name,
                lillux_sqlite::OriginalNativeBoundMode::ReadonlyInspection,
                OriginalNativeDomain::SchedulerInspection,
                host.destination(),
                Some((lease.clone(), work.clone())),
            )
        }
        .expect("scheduler inspection inputs");
        let (accounting, terminal) =
            match SchedulerDb::inspect_history_reset(host.factory(), inputs, &reference) {
                Ok(inspected) => inspected,
                Err(error) => {
                    // A refused acquisition or close reaches the census
                    // through a same-owner retry of its native owner.
                    let failure = error
                        .chain()
                        .find_map(|cause| cause.downcast_ref::<SchedulerInspectionCloseFailure>());
                    if let Some(failure) = failure {
                        let settled = failure.retry_close().and_then(|()| failure.take_terminal());
                        if let Ok(terminal) = settled {
                            host.retain_terminal(terminal);
                        }
                    }
                    ryeos_state::test_support::release_shared_test_host(host);
                    return Err(error);
                }
            };
        host.retain_terminal(terminal.clone());
        let proven = match &terminal {
            OriginalNativeConnectionTerminal::Closed(closed) => closed
                .inspection_closed()
                .context("closed scheduler inspection lacks its copy proof")
                .and_then(|proof| {
                    proof.acknowledge_producer_terminal()?;
                    proof.require_original(&pin, name, &lease, &work)
                }),
            OriginalNativeConnectionTerminal::Released(_) => {
                Err(anyhow!("scheduler inspection released without a close"))
            }
        };
        drop(terminal);
        ryeos_state::test_support::release_shared_test_host(host);
        proven?;
        accounting
    }

    /// The current scheduler schema definition, captured from a native
    /// reference built and closed on this thread's shared test host; the
    /// reference terminal joins that host's census.
    fn scheduler_schema_definition() -> sqlite_schema::SchemaSqlDefinition {
        let host = ryeos_state::test_support::shared_test_host();
        // SAFETY: test-only issuance under the shared host's own funded factory.
        let inputs = unsafe {
            OriginalNativeTransientInputs::issued(
                OriginalNativeDomain::NodeSchedulerSchemaReference,
                host.destination(),
            )
        }
        .expect("scheduler schema reference inputs");
        let reference =
            match SchedulerDb::prepare_scheduler_schema_reference(Arc::clone(host.factory()), inputs) {
                Ok(reference) => reference,
                Err(failure) => panic!("scheduler schema reference refused: {failure}"),
            };
        let (definition, terminal) = reference.into_original_parts();
        host.retain_terminal(terminal);
        ryeos_state::test_support::release_shared_test_host(host);
        definition
    }

    fn test_db() -> crate::test_native::TestSchedulerDb {
        crate::test_native::TestSchedulerDb::open()
    }

    fn fresh_test_db() -> crate::test_native::TestSchedulerDb {
        crate::test_native::TestSchedulerDb::open_fresh()
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn retirement_closes_all_clone_admission_and_preserves_before_commit_retry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scheduler.sqlite3");
        let host = TestSchedulerHost::acquire();
        let db = Arc::new(open_test_path(&host, &path).unwrap());
        let other = db.clone();
        assert!(!other.fire_projection_is_current().unwrap());
        let captured = db
            .capture_projection_retirement([0x81; 16], host.retirement_inputs([0x81; 16]))
            .unwrap();
        assert!(other.fire_projection_is_current().is_err());
        assert!(matches!(
            other.close(),
            Err(SchedulerDbCloseError::RetirementPending)
        ));
        // A second capture finds no live owner; its inputs come back unspent.
        assert!(matches!(
            other.capture_projection_retirement([0x82; 16], host.retirement_inputs([0x82; 16])),
            Err(SchedulerDbRetirementError::Unstarted(_))
        ));
        assert!(matches!(
            other.retry_projection_capture([0x82; 16]),
            Err(SchedulerDbRetirementError::GenerationMismatch)
        ));
        let drained = other.drain_projection_retirement().unwrap();
        assert_eq!(drained.generation, captured.generation);
        let quarantine = dir
            .path()
            .join(&drained.roles[SqliteRole::Main as usize].quarantine_name);
        std::fs::write(&quarantine, b"foreign quarantine occupant").unwrap();
        // The native owner keeps the refused quarantine as its failed phase.
        assert!(matches!(
            db.quarantine_projection_role(SqliteRole::Main),
            Err(SchedulerDbRetirementError::Host(_))
        ));
        assert_eq!(
            std::fs::read(&quarantine).unwrap(),
            b"foreign quarantine occupant"
        );
        assert!(path.exists());
        assert!(db.release_projection_retirement().is_err());
        // Release refusal retains Drained in the failure owner; drain retries
        // observations/barriers on that same owner, never native recapture.
        other.drain_projection_retirement().unwrap();
        std::fs::remove_file(quarantine).unwrap(); // fixture-owned foreign control artifact
        for role in [SqliteRole::Main, SqliteRole::Wal, SqliteRole::Shm] {
            let expected = if drained.roles[role as usize].original.is_some() {
                SqliteRoleMutationOutcome::QuarantinedDurable
            } else {
                SqliteRoleMutationOutcome::OriginallyAbsent
            };
            assert_eq!(db.quarantine_projection_role(role).unwrap(), expected);
            let expected = if drained.roles[role as usize].original.is_some() {
                SqliteRoleMutationOutcome::RemovedDurable
            } else {
                SqliteRoleMutationOutcome::OriginallyAbsent
            };
            assert_eq!(other.purge_projection_role(role).unwrap(), expected);
        }
        other.release_projection_retirement().unwrap();
        db.close().unwrap();
        assert!(db.fire_projection_is_current().is_err());
        drop(other);
        host.settle_shared(db);
        for role in &drained.roles {
            assert!(!dir.path().join(&role.quarantine_name).exists());
            assert!(!dir.path().join(&role.purge_name).exists());
        }
        assert!(!path.exists());
    }
    #[test]
    #[cfg(target_os = "linux")]
    fn typed_stale_transfer_preserves_owner_and_denies_ordinary_sql() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scheduler.sqlite3");
        let host = TestSchedulerHost::acquire();
        create_closed_test_path(&host, &path);
        host.with_fixture_sql(dir.path(), "scheduler.sqlite3", false, |connection| {
            Ok(connection.execute_batch("ALTER TABLE schedule_specs ADD COLUMN retired_field TEXT;")?)
        });
        let failure = open_test_path(&host, &path)
            .err()
            .expect("recognized stale source");
        assert_eq!(
            failure.disposition(),
            SchedulerDbOpenDisposition::RecognizedStaleProjection
        );
        let db = failure.into_stale_projection().unwrap();
        assert!(db.fire_projection_is_current().is_err());
        let captured = db
            .capture_projection_retirement([0x91; 16], host.retirement_inputs([0x91; 16]))
            .unwrap();
        assert!(captured.roles[SqliteRole::Main as usize].original.is_some());
        db.abort_projection_retirement().unwrap();
        assert!(path.exists()); // cleanup-only abort never removes stale projection
        host.settle(db);
    }
    #[test]
    #[cfg(target_os = "linux")]
    fn resolution_refusal_keeps_same_db_owner_and_all_clone_admission_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scheduler.sqlite3");
        let host = TestSchedulerHost::acquire();
        let db = Arc::new(open_test_path(&host, &path).unwrap());
        let peer = Arc::clone(&db);
        db.capture_projection_retirement([0xd1; 16], host.retirement_inputs([0xd1; 16]))
            .unwrap();
        db.drain_projection_retirement().unwrap();
        let mut expected = peer
            .observe_projection_role_resolution(SqliteRole::Main)
            .unwrap();
        let original = expected.original.clone();
        expected.generation = [0xd2; 16];
        let request = lillux_sqlite::SqliteRoleResolutionRequest {
            expected,
            ordinal: 0,
            action: lillux_sqlite::SqliteRoleResolutionAction::SyncRemoved,
        };
        // The native owner keeps the refused resolution as its failed phase.
        assert!(matches!(
            db.resolve_projection_role(request),
            Err(SchedulerDbRetirementError::Host(_))
        ));
        assert!(peer.fire_projection_is_current().is_err());
        assert!(matches!(
            peer.close(),
            Err(SchedulerDbCloseError::RetirementPending)
        ));
        let after = db
            .observe_projection_role_resolution(SqliteRole::Main)
            .unwrap();
        assert_eq!(after.original, original);
        assert!(after.resolutions.is_empty());
        assert!(after.barriers_held);
        db.abort_projection_retirement().unwrap();
        assert!(path.exists());
        drop(peer);
        host.settle_shared(db);
    }
    /// The identity a stopped plan records for the file `name` beneath
    /// `directory`, or `None` when no such file exists.
    #[cfg(target_os = "linux")]
    fn stopped_role_identity(directory: &Path, name: &str) -> Option<lillux_sqlite::SqliteRoleIdentity> {
        use std::os::unix::fs::MetadataExt;
        let file = match std::fs::File::open(directory.join(name)) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
            Err(error) => panic!("open stopped fixture role {name}: {error}"),
        };
        let metadata = file.metadata().unwrap();
        let (digest, metadata) =
            lillux::digest_open_regular_file_stable_exact(&file, metadata.len()).unwrap();
        Some(lillux_sqlite::SqliteRoleIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
            byte_length: metadata.len(),
            content_sha256: Some(digest),
        })
    }

    /// The file a stopped fixture takes `role` from under `source`: its
    /// canonical name, or the name an earlier generation quarantined it to.
    #[cfg(target_os = "linux")]
    fn stopped_source_name(
        source: lillux_sqlite::StoppedSqliteRoleSource,
        role: SqliteRole,
    ) -> Option<String> {
        use lillux_sqlite::StoppedSqliteRoleSource;
        let (canonical, label) = match role {
            SqliteRole::Main => ("scheduler.sqlite3", "main"),
            SqliteRole::Wal => ("scheduler.sqlite3-wal", "wal"),
            SqliteRole::Shm => ("scheduler.sqlite3-shm", "shm"),
        };
        match source {
            StoppedSqliteRoleSource::Absent => None,
            StoppedSqliteRoleSource::Canonical => Some(canonical.to_owned()),
            StoppedSqliteRoleSource::Quarantine { generation } => {
                let generation: String = generation.iter().map(|byte| format!("{byte:02x}")).collect();
                Some(format!("scheduler-retirement.role.{generation}.{label}.quarantine"))
            }
            other => panic!("stopped fixture does not take roles from {other:?}"),
        }
    }

    /// A stopped plan for generation 4 over `sources`, each role's current
    /// identity read from the file its source names. A closed scheduler
    /// store keeps its WAL and shared-memory files, so a role is planned
    /// from `sources` only where its file exists and as absent otherwise.
    #[cfg(target_os = "linux")]
    fn stopped_fixture_plan(
        runtime: &PinnedDirectory,
        directory: &Path,
        sources: [lillux_sqlite::StoppedSqliteRoleSource; 3],
    ) -> lillux_sqlite::StoppedSqliteProjectionPlan {
        use lillux_sqlite::{StoppedSqliteProjectionPlan, StoppedSqliteRolePlan, StoppedSqliteRoleSource};
        let mut plan = StoppedSqliteProjectionPlan {
            generation: [4; 16],
            generations: vec![[1; 16], [2; 16], [3; 16], [4; 16]],
            runtime_identity: runtime.identity().unwrap(),
            roles: std::array::from_fn(|index| StoppedSqliteRolePlan {
                source: sources[index],
                current: None,
            }),
        };
        for role in [SqliteRole::Main, SqliteRole::Wal, SqliteRole::Shm] {
            let current = stopped_source_name(sources[role as usize], role)
                .and_then(|name| stopped_role_identity(directory, &name));
            plan.roles[role as usize] = match current {
                Some(current) => StoppedSqliteRolePlan {
                    source: sources[role as usize],
                    current: Some(current),
                },
                None => StoppedSqliteRolePlan {
                    source: StoppedSqliteRoleSource::Absent,
                    current: None,
                },
            };
        }
        plan
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn original_canonical_stopped_owner_drains_without_sql_and_retires_all_roles() {
        use lillux_sqlite::{SqliteRoleMutationOutcome, StoppedSqliteRoleSource};
        let directory = tempfile::tempdir().unwrap();
        let canonical = directory.path().join("scheduler.sqlite3");
        let host = TestSchedulerHost::acquire();
        create_closed_test_path(&host, &canonical); // setup only, before retirement intake
        let runtime = Arc::new(PinnedDirectory::open(directory.path()).unwrap().unwrap());
        let plan = stopped_fixture_plan(
            &runtime,
            directory.path(),
            [StoppedSqliteRoleSource::Canonical; 3],
        );
        let original = plan.roles[0].current.clone().expect("closed store keeps its main file");
        // A node takes stopped intake inside its original cleanup scope and
        // runs the stopped owner's later steps inside its allocator scope.
        let db = host.cleanup_scope(|| {
            SchedulerDb::capture_stopped_projection(runtime, plan.clone()).unwrap()
        });
        assert!(db.fire_projection_is_current().is_err()); // no live SQL handle
        host.scope(|| {
            let before = db.observe_stopped_projection().unwrap();
            assert!(before.ready);
            assert_eq!(before.coordinates.len(), 100);
            assert_eq!(before.roles[0], Some(original.clone()));
            let drained = db.drain_projection_retirement().unwrap();
            assert_eq!(drained.roles[0].original, Some(original));
            let after = db.observe_stopped_projection().unwrap();
            assert_eq!(after.roles, before.roles);
            for role in [SqliteRole::Main, SqliteRole::Wal, SqliteRole::Shm] {
                let present = plan.roles[role as usize].current.is_some();
                assert_eq!(drained.roles[role as usize].original.is_some(), present);
                let quarantine = db.quarantine_projection_role(role).unwrap();
                let purge = db.purge_projection_role(role).unwrap();
                if present {
                    assert_eq!(quarantine, SqliteRoleMutationOutcome::QuarantinedDurable);
                    assert_eq!(purge, SqliteRoleMutationOutcome::RemovedDurable);
                } else {
                    assert_eq!(quarantine, SqliteRoleMutationOutcome::OriginallyAbsent);
                    assert_eq!(purge, SqliteRoleMutationOutcome::OriginallyAbsent);
                }
            }
            db.release_projection_retirement().unwrap();
        });
        assert!(!canonical.exists());
        assert!(!directory.path().join("scheduler.sqlite3-wal").exists());
        assert!(!directory.path().join("scheduler.sqlite3-shm").exists());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn stopped_db_partial_owner_retry_never_opens_sql_or_constructs_main() {
        use lillux_sqlite::StoppedSqliteRoleSource;
        let dir = tempfile::tempdir().unwrap();
        let canonical = dir.path().join("scheduler.sqlite3");
        let host = TestSchedulerHost::acquire();
        create_closed_test_path(&host, &canonical);
        // An earlier generation quarantined every role the closed store
        // left; this generation takes them from those quarantine names.
        let quarantined = StoppedSqliteRoleSource::Quarantine {
            generation: [1; 16],
        };
        let runtime = Arc::new(PinnedDirectory::open(dir.path()).unwrap().unwrap());
        for (role, name) in [
            (SqliteRole::Main, "scheduler.sqlite3"),
            (SqliteRole::Wal, "scheduler.sqlite3-wal"),
            (SqliteRole::Shm, "scheduler.sqlite3-shm"),
        ] {
            if dir.path().join(name).exists() {
                let selected = stopped_source_name(quarantined, role).unwrap();
                std::fs::rename(dir.path().join(name), dir.path().join(selected)).unwrap();
            }
        }
        // Fixture sampling happens BEFORE any peer or retirement barrier.
        let plan = stopped_fixture_plan(&runtime, dir.path(), [quarantined; 3]);
        let selected = stopped_source_name(quarantined, SqliteRole::Main).unwrap();
        assert!(plan.roles[0].current.is_some());
        let peer_path = dir.path().join("peer.sqlite3");
        std::fs::hard_link(dir.path().join(&selected), &peer_path).unwrap();
        let peer = host.open_fixture_peer(dir.path(), "peer.sqlite3");
        peer.with_sql(|peer| Ok(peer.execute_batch("BEGIN; SELECT * FROM schedule_specs;")?))
            .unwrap();
        // A node takes stopped intake inside its original cleanup scope and
        // runs the stopped owner's later steps inside its allocator scope.
        let failure = host.cleanup_scope(|| {
            SchedulerDb::capture_stopped_projection(Arc::clone(&runtime), plan.clone())
                .err()
                .expect("same stopped failed DB")
        });
        assert_eq!(
            failure.disposition(),
            SchedulerDbOpenDisposition::StoppedProjectionIntake
        );
        let failure = host.scope(|| {
            let before = failure.observe_stopped_projection().unwrap();
            assert!(!before.ready);
            let failure = failure.retry_close().unwrap_err(); // no implicit abort/reopen
            assert!(failure.has_retained_owner());
            let mut wrong = plan.clone();
            wrong.generation = [3; 16];
            failure
                .retry_stopped_projection(&wrong)
                .err()
                .expect("wrong plan preserves failed DB")
        });
        assert!(!canonical.exists());
        peer.with_sql(|peer| Ok(peer.execute_batch("ROLLBACK;")?)).unwrap();
        host.close_fixture(peer);
        let db = Arc::new(host.scope(|| failure.retry_stopped_projection(&plan).unwrap()));
        let clone = Arc::clone(&db);
        assert!(clone.fire_projection_is_current().is_err());
        assert!(matches!(
            clone.close(),
            Err(SchedulerDbCloseError::RetirementPending)
        ));
        host.scope(|| {
            let after = db.observe_stopped_projection().unwrap();
            assert!(after.ready);
            assert_eq!(after.roles[0], plan.roles[0].current);
            assert_eq!(after.coordinates.len(), 100);
            assert!(!canonical.exists());
            db.abort_projection_retirement().unwrap();
        });
        assert!(dir.path().join(&selected).exists());
        assert!(!canonical.exists());
    }
    /// The same canonical retirement through the factory's production
    /// scopes: intake in the stopped-intake scope, every later step and the
    /// owner's end in the service scope.
    #[test]
    #[cfg(target_os = "linux")]
    fn original_canonical_stopped_owner_retires_all_roles_in_production_native_scopes() {
        use lillux_sqlite::{SqliteRoleMutationOutcome, StoppedSqliteRoleSource};
        let directory = tempfile::tempdir().unwrap();
        let canonical = directory.path().join("scheduler.sqlite3");
        let host = TestSchedulerHost::acquire();
        create_closed_test_path(&host, &canonical); // setup only, before retirement intake
        let factory = Arc::clone(host.factory());
        let runtime = Arc::new(PinnedDirectory::open(directory.path()).unwrap().unwrap());
        let plan = stopped_fixture_plan(
            &runtime,
            directory.path(),
            [StoppedSqliteRoleSource::Canonical; 3],
        );
        let original = plan.roles[0].current.clone().expect("closed store keeps its main file");
        let db = factory
            .with_original_stopped_intake(|_| SchedulerDb::capture_stopped_projection(runtime, plan.clone()))
            .unwrap()
            .unwrap();
        factory
            .with_original_service_scope(|_| {
                let before = db.observe_stopped_projection().unwrap();
                assert!(before.ready);
                assert_eq!(before.roles[0], Some(original.clone()));
                let drained = db.retry_stopped_projection(&plan).unwrap();
                assert_eq!(drained.roles[0].original, Some(original.clone()));
                let drained = db.drain_projection_retirement().unwrap();
                assert_eq!(drained.roles[0].original, Some(original));
                for role in [SqliteRole::Main, SqliteRole::Wal, SqliteRole::Shm] {
                    let present = plan.roles[role as usize].current.is_some();
                    let quarantine = db.quarantine_projection_role(role).unwrap();
                    let purge = db.purge_projection_role(role).unwrap();
                    if present {
                        assert_eq!(quarantine, SqliteRoleMutationOutcome::QuarantinedDurable);
                        assert_eq!(purge, SqliteRoleMutationOutcome::RemovedDurable);
                    } else {
                        assert_eq!(quarantine, SqliteRoleMutationOutcome::OriginallyAbsent);
                        assert_eq!(purge, SqliteRoleMutationOutcome::OriginallyAbsent);
                    }
                }
                db.release_projection_retirement().unwrap();
            })
            .unwrap();
        factory.with_original_service_scope(move |_| drop(db)).unwrap();
        drop(factory);
        assert!(!canonical.exists());
        assert!(!directory.path().join("scheduler.sqlite3-wal").exists());
        assert!(!directory.path().join("scheduler.sqlite3-shm").exists());
    }

    /// The same held partial owner through the factory's production scopes:
    /// every intake attempt in the stopped-intake scope, observations, the
    /// refused wrong-plan retry and the abort in the service scope.
    #[test]
    #[cfg(target_os = "linux")]
    fn stopped_db_partial_owner_retries_in_production_native_scopes() {
        use lillux_sqlite::StoppedSqliteRoleSource;
        let dir = tempfile::tempdir().unwrap();
        let canonical = dir.path().join("scheduler.sqlite3");
        let host = TestSchedulerHost::acquire();
        create_closed_test_path(&host, &canonical);
        let factory = Arc::clone(host.factory());
        let quarantined = StoppedSqliteRoleSource::Quarantine {
            generation: [1; 16],
        };
        let runtime = Arc::new(PinnedDirectory::open(dir.path()).unwrap().unwrap());
        for (role, name) in [
            (SqliteRole::Main, "scheduler.sqlite3"),
            (SqliteRole::Wal, "scheduler.sqlite3-wal"),
            (SqliteRole::Shm, "scheduler.sqlite3-shm"),
        ] {
            if dir.path().join(name).exists() {
                let selected = stopped_source_name(quarantined, role).unwrap();
                std::fs::rename(dir.path().join(name), dir.path().join(selected)).unwrap();
            }
        }
        let plan = stopped_fixture_plan(&runtime, dir.path(), [quarantined; 3]);
        let selected = stopped_source_name(quarantined, SqliteRole::Main).unwrap();
        let peer_path = dir.path().join("peer.sqlite3");
        std::fs::hard_link(dir.path().join(&selected), &peer_path).unwrap();
        let peer = host.open_fixture_peer(dir.path(), "peer.sqlite3");
        peer.with_sql(|peer| Ok(peer.execute_batch("BEGIN; SELECT * FROM schedule_specs;")?))
            .unwrap();
        let failure = factory
            .with_original_stopped_intake(|_| SchedulerDb::capture_stopped_projection(Arc::clone(&runtime), plan.clone()))
            .unwrap()
            .err()
            .expect("same stopped failed DB");
        assert_eq!(failure.disposition(), SchedulerDbOpenDisposition::StoppedProjectionIntake);
        let failure = factory
            .with_original_service_scope(|_| {
                assert!(!failure.observe_stopped_projection().unwrap().ready);
                let mut wrong = plan.clone();
                wrong.generation = [3; 16];
                failure
                    .retry_stopped_projection(&wrong)
                    .err()
                    .expect("wrong plan preserves failed DB")
            })
            .unwrap();
        // Still held: the retry in the intake scope hands the same owner back.
        let failure = factory
            .with_original_stopped_intake(|_| failure.retry_stopped_projection(&plan))
            .unwrap()
            .err()
            .expect("held peer refuses the retry");
        peer.with_sql(|peer| Ok(peer.execute_batch("ROLLBACK;")?)).unwrap();
        host.close_fixture(peer);
        let db = factory
            .with_original_stopped_intake(|_| failure.retry_stopped_projection(&plan))
            .unwrap()
            .unwrap();
        factory
            .with_original_service_scope(|_| {
                let after = db.observe_stopped_projection().unwrap();
                assert!(after.ready);
                assert_eq!(after.roles[0], plan.roles[0].current);
                db.abort_projection_retirement().unwrap();
            })
            .unwrap();
        factory.with_original_service_scope(move |_| drop(db)).unwrap();
        drop(factory);
        assert!(dir.path().join(&selected).exists());
        assert!(!canonical.exists());
    }

    #[test]
    fn retirement_input_mismatch_preserves_live_database() {
        let db = test_db();
        let host = TestSchedulerHost::acquire();
        let refused = db.capture_projection_retirement([0xa1; 16], host.retirement_inputs([0xa2; 16]));
        let Err(SchedulerDbRetirementError::Unstarted(retained)) = refused else {
            panic!("mismatched retirement inputs must come back unspent");
        };
        assert_eq!(retained.into_inputs().unwrap().generation(), [0xa2; 16]);
        assert!(db.fire_projection_is_current().unwrap());
        db.close().unwrap();
    }
    #[test]
    fn history_reset_inspection_accepts_incomplete_current_projection() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scheduler.sqlite3");
        let host = TestSchedulerHost::acquire();
        let db = open_test_path(&host, &path).unwrap();
        assert!(!db.fire_projection_is_current().unwrap());
        host.settle(db);
        assert_eq!(inspect_test_path(&path).unwrap().total_rows(), Some(0));
    }

    #[test]
    fn history_reset_inspection_keeps_stale_projection_opaque() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scheduler.sqlite3");
        let host = TestSchedulerHost::acquire();
        create_closed_test_path(&host, &path);
        host.with_fixture_sql(dir.path(), "scheduler.sqlite3", false, |conn| {
            Ok(conn.execute_batch("ALTER TABLE schedule_specs ADD COLUMN retired_field TEXT;")?)
        });
        let before = std::fs::read(&path).unwrap();
        assert_eq!(inspect_test_path(&path).unwrap().total_rows(), None);
        assert_eq!(std::fs::read(&path).unwrap(), before);
        // Retirement mutations are held pending authenticated durable recovery.
        let failure = open_test_path(&host, &path)
            .err()
            .expect("stale retirement must refuse");
        assert!(
            failure
                .error()
                .to_string()
                .contains("authenticated host retirement")
        );
        host.settle_refusal(failure);
    }

    #[test]
    fn history_reset_inspection_rejects_foreign_and_unrecognized_stores() {
        for mutation in [
            "PRAGMA application_id=1234;",
            "PRAGMA application_id=0;",
            "CREATE TABLE unrelated (value TEXT);",
            "CREATE VIEW unrelated AS SELECT * FROM schedule_specs;",
            "CREATE TRIGGER unrelated AFTER DELETE ON schedule_specs BEGIN DELETE FROM schedule_fires; END;",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("scheduler.sqlite3");
            let host = TestSchedulerHost::acquire();
            create_closed_test_path(&host, &path);
            host.with_fixture_sql(dir.path(), "scheduler.sqlite3", false, |conn| {
                Ok(conn.execute_batch(mutation)?)
            });
            let before = std::fs::read(&path).unwrap();
            assert!(inspect_test_path(&path).is_err(), "{mutation}");
            assert_eq!(std::fs::read(&path).unwrap(), before);
        }
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
        db.with_sql(|conn| {
            conn.execute(
                "INSERT INTO scheduler_projection_state
                    (projection_name, rebuild_complete) VALUES ('unexpected', 1)",
                [],
            )?;
            Ok(())
        })
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
        assert_eq!(db.discard_fire_history(true).unwrap().total_rows(), Some(3));

        let removed = db.discard_fire_history(false).unwrap();
        assert_eq!(removed.total_rows(), Some(3));
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

        assert!(
            db.verify_fire_retention_targets(&[ryeos_state::gc::retention::FireRetentionTarget {
                schedule_id: "sched".to_string(),
                fire_id: "sched@2000".to_string(),
            }])
            .is_err()
        );
        assert!(
            db.verify_fire_retention_targets(&[ryeos_state::gc::retention::FireRetentionTarget {
                schedule_id: "sched".to_string(),
                fire_id: "sched@missing".to_string(),
            }])
            .is_err()
        );
        assert!(
            db.verify_fire_retention_targets(&[old.clone(), old])
                .is_err()
        );
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
            execution: ryeos_scheduler_contract::ScheduleExecution {
                authority: ryeos_scheduler_contract::ScheduleExecutionAuthority::Node {
                    principal_id: format!("fp:{}", "33".repeat(32)),
                    effective_origin_site_id: "site:test".to_string(),
                },
                capabilities: vec!["ryeos.execute.*".to_string()],
                policy: ryeos_engine::execution_contract::ExecutionPolicy::projectless(
                    ryeos_engine::execution_contract::ExecutionResponse::Accepted,
                ),
            },
        }
    }

    fn make_fire(schedule_id: &str, scheduled_at: i64, status: &str) -> FireRecord {
        let fire_id = format!("{}@{}", schedule_id, scheduled_at);
        let terminal = status != "dispatched";
        let launched = status != "skipped";
        FireRecord {
            thread_id: launched.then(|| crate::types::thread_id_from_fire(&fire_id)),
            fire_id,
            schedule_id: schedule_id.to_string(),
            scheduled_at,
            reserved_at: scheduled_at,
            dispatched_at: launched.then_some(scheduled_at),
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
            schedule_spec_hash: "22".repeat(32),
            project_authority: launched
                .then(|| {
                    ryeos_state::objects::ExecutionProjectAuthority::projectless(
                        ryeos_state::objects::EnvironmentAuthority::None,
                    )
                })
                .transpose()
                .unwrap(),
            admitted_capsule_hash: launched.then(|| "44".repeat(32)),
        }
    }

    #[test]
    fn open_retains_recognized_stale_owned_projection_without_reading_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scheduler.sqlite3");
        let host = TestSchedulerHost::acquire();
        host.with_fixture_sql(dir.path(), "scheduler.sqlite3", true, |conn| {
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
                    execution             TEXT NOT NULL,
                    lateness_grace_secs   INTEGER NOT NULL
                );
                CREATE TABLE schedule_fires (
                    fire_id            TEXT PRIMARY KEY,
                    schedule_id        TEXT NOT NULL,
                    scheduled_at       INTEGER NOT NULL,
                    reserved_at        INTEGER NOT NULL,
                    dispatched_at      INTEGER,
                    completed_at       INTEGER,
                    thread_id          TEXT,
                    status             TEXT NOT NULL,
                    trigger_reason     TEXT NOT NULL DEFAULT 'normal',
                    outcome            TEXT,
                    signer_fingerprint TEXT NOT NULL,
                    schedule_spec_hash TEXT NOT NULL,
                    project_authority  TEXT,
                    admitted_capsule_hash TEXT
                );
                CREATE INDEX idx_fires_schedule_id ON schedule_fires(schedule_id);
                CREATE INDEX idx_fires_status ON schedule_fires(status);
                "#,
            )?;
            conn.execute_batch(&format!("PRAGMA application_id = {};", SCHEDULER_APP_ID))?;
            conn.execute(
                "INSERT INTO schedule_specs
                 (schedule_id, item_ref, params, schedule_type, expression, timezone,
                  misfire_policy, overlap_policy, enabled, project_root, signer_fingerprint,
                  spec_hash, registered_at, execution, lateness_grace_secs)
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
                    "11".repeat(32),
                    "hash",
                    1000,
                    serde_json::to_string(&make_spec("old-sched").execution).unwrap(),
                    60,
                ],
            )?;
            Ok(())
        });

        let failure = open_test_path(&host, &path)
            .err()
            .expect("stale retirement must refuse");
        assert!(failure.has_retained_owner());
        host.settle_refusal(failure);
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
        assert!(
            error
                .to_string()
                .contains("signer_fingerprint must be lowercase SHA-256 hex")
        );
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

        let completed_at = fire.dispatched_at.map(|dispatched_at| dispatched_at + 1);
        let updated = FireRecord {
            status: "completed".to_string(),
            outcome: Some("success".to_string()),
            completed_at,
            ..fire
        };
        db.upsert_fire(&updated).unwrap();

        let got = db.get_fire("sched@1000").unwrap().unwrap();
        assert_eq!(got.status, "completed");
        assert_eq!(got.outcome.unwrap(), "success");
    }

    #[test]
    fn duplicate_terminal_completion_preserves_first_snapshot() {
        let db = test_db();
        for status in ["completed", "failed", "cancelled"] {
            let first = make_fire(status, 1000, status);
            db.upsert_fire(&first).unwrap();
            let outbox_before = db.pending_fire_outbox().unwrap();
            let mut repeated = first.clone();
            repeated.completed_at = Some(2000);
            db.upsert_fire(&repeated).unwrap();
            assert_eq!(db.get_fire(&first.fire_id).unwrap().unwrap(), first);
            assert_eq!(db.pending_fire_outbox().unwrap(), outbox_before);
        }
    }

    #[test]
    fn duplicate_terminal_completion_rejects_conflicting_status_and_authority() {
        let db = test_db();
        let first = make_fire("sched", 1000, "completed");
        db.upsert_fire(&first).unwrap();
        let mut conflict = first.clone();
        conflict.completed_at = Some(2000);
        conflict.status = "failed".to_string();
        conflict.outcome = Some("thread_failed".to_string());
        assert!(db.upsert_fire(&conflict).is_err());
        let mut conflict = first.clone();
        conflict.completed_at = Some(2000);
        conflict.signer_fingerprint = "22".repeat(32);
        assert!(db.upsert_fire(&conflict).is_err());
        assert_eq!(db.get_fire(&first.fire_id).unwrap().unwrap(), first);
    }

    #[test]
    fn competing_terminal_finalizers_preserve_winner() {
        let store = test_db();
        let db = store.shared();
        let dispatched = make_fire("sched", 1000, "dispatched");
        db.upsert_fire(&dispatched).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let handles: Vec<_> = [2000, 3000]
            .into_iter()
            .map(|completed_at| {
                let db = db.clone();
                let barrier = barrier.clone();
                let mut terminal = dispatched.clone();
                terminal.status = "completed".to_string();
                terminal.outcome = Some("success".to_string());
                terminal.completed_at = Some(completed_at);
                std::thread::spawn(move || {
                    barrier.wait();
                    db.upsert_fire(&terminal).unwrap();
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        let winner = db.get_fire(&dispatched.fire_id).unwrap().unwrap();
        assert_eq!(winner.status, "completed");
        assert!(matches!(winner.completed_at, Some(2000 | 3000)));
        assert_eq!(db.pending_fire_outbox().unwrap(), 2);
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
        let expected_spec_hash = spec.spec_hash.clone();
        db.upsert_spec(&spec).unwrap();
        db.upsert_fire(&make_fire("sched", 61_000, "completed"))
            .unwrap();
        db.upsert_fire(&make_fire("sched", 121_000, "skipped"))
            .unwrap();

        db.rebuild_cursors_for_specs(&[spec], 122_000).unwrap();
        let cursor = db.get_cursor("sched").unwrap().unwrap();

        assert_eq!(cursor.spec_hash, expected_spec_hash);
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
    fn dispatched_fire_without_deterministic_thread_is_rejected() {
        let db = test_db();
        let mut fire = make_fire("sched", 1000, "dispatched");
        fire.thread_id = None;
        let error = db.upsert_fire(&fire).unwrap_err();
        assert!(
            format!("{error:#}")
                .contains("dispatched scheduler fire must have its deterministic thread_id")
        );
    }

    #[test]
    fn reclaim_fire_succeeds_for_dispatched_with_thread() {
        let db = test_db();
        let fire = make_fire("sched", 1000, "dispatched");
        db.upsert_fire(&fire).unwrap();

        // Thread existence is checked by the caller (reconciler). Reclaim is
        // only an eligibility check and never rewrites immutable dispatch
        // identity.
        let thread_id = fire.thread_id.clone();
        let reclaimed = db.reclaim_fire("sched@1000").unwrap();
        assert!(
            reclaimed,
            "dispatched fire should be reclaimable even with thread_id set"
        );
        let reclaimed_fire = db.get_fire("sched@1000").unwrap().unwrap();
        assert_eq!(reclaimed_fire.thread_id, thread_id);
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

        // Create a dispatched fire with dispatched_at in the past.
        let mut old = make_fire("sched", 1000, "dispatched");
        old.reserved_at = lillux::time::timestamp_millis() - 60_001;
        old.dispatched_at = Some(lillux::time::timestamp_millis() - 60_000);
        db.upsert_fire(&old).unwrap();

        // Create a recent dispatched fire (should not be stale)
        let mut recent = make_fire("sched", 2000, "dispatched");
        recent.reserved_at = lillux::time::timestamp_millis();
        recent.dispatched_at = Some(recent.reserved_at);
        db.upsert_fire(&recent).unwrap();

        // Create a completed fire (should not show up even if old)
        let mut done = make_fire("sched", 3000, "completed");
        done.reserved_at = lillux::time::timestamp_millis() - 120_001;
        done.dispatched_at = Some(lillux::time::timestamp_millis() - 120_000);
        done.completed_at = done.dispatched_at.map(|dispatched_at| dispatched_at + 1);
        db.upsert_fire(&done).unwrap();

        let stale = db.find_stale_dispatched_fires(30).unwrap();
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].fire_id, "sched@1000");
    }

    #[test]
    fn dropped_released_stopped_store_settles_without_abandoning() {
        const KIND: &str = "ryeos-scheduler::store";
        let before = lillux::custody::abandoned_custody().count(KIND);
        let db = SchedulerDb::from_parts(DbOwner::Closed, None);
        drop(db);
        assert_eq!(lillux::custody::abandoned_custody().count(KIND), before);
    }

    #[test]
    fn dropped_checked_out_store_is_abandoned() {
        const KIND: &str = "ryeos-scheduler::store";
        let before = lillux::custody::abandoned_custody().count(KIND);
        let db = SchedulerDb::from_parts(DbOwner::CheckedOut, None);
        drop(db);
        assert_eq!(lillux::custody::abandoned_custody().count(KIND), before + 1);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn released_stopped_retirement_store_drops_without_abandoning() {
        use lillux_sqlite::StoppedSqliteRoleSource;
        const KIND: &str = "ryeos-scheduler::store";
        let directory = tempfile::tempdir().unwrap();
        let canonical = directory.path().join("scheduler.sqlite3");
        let host = TestSchedulerHost::acquire();
        create_closed_test_path(&host, &canonical);
        let runtime = Arc::new(PinnedDirectory::open(directory.path()).unwrap().unwrap());
        let plan = stopped_fixture_plan(
            &runtime,
            directory.path(),
            [StoppedSqliteRoleSource::Canonical; 3],
        );
        let db = host.cleanup_scope(|| {
            SchedulerDb::capture_stopped_projection(runtime, plan).unwrap()
        });
        host.scope(|| {
            db.drain_projection_retirement().unwrap();
            for role in [SqliteRole::Main, SqliteRole::Wal, SqliteRole::Shm] {
                db.quarantine_projection_role(role).unwrap();
                db.purge_projection_role(role).unwrap();
            }
            db.release_projection_retirement().unwrap();
        });
        let before = lillux::custody::abandoned_custody().count(KIND);
        drop(db);
        assert_eq!(lillux::custody::abandoned_custody().count(KIND), before);
    }

    fn emptied_finalization_owner() -> SchedulerNativeFinalizationOwner {
        SchedulerNativeFinalizationOwner {
            held: Held::new(
                "ryeos-scheduler::native-finalization-owner",
                SchedulerNativeFinalizationParts {
                    inner: DbOwner::CheckedOut,
                    factory: None,
                    schema_reference: None,
                    database_outside: None,
                    database_outside_custody: None,
                    database_never_opened: None,
                    database_refusal: None,
                    schema_outside: None,
                    schema_outside_custody: None,
                    schema_refusal: None,
                },
                scheduler_native_finalization_settled,
            ),
            outside_handoff_id: None,
        }
    }

    #[test]
    fn dropped_transferred_finalization_owner_settles_without_abandoning() {
        const KIND: &str = "ryeos-scheduler::native-finalization-owner";
        let before = lillux::custody::abandoned_custody().count(KIND);
        drop(emptied_finalization_owner());
        assert_eq!(lillux::custody::abandoned_custody().count(KIND), before);
    }

    #[test]
    fn dropped_finalization_owner_with_database_phase_is_abandoned() {
        const KIND: &str = "ryeos-scheduler::native-finalization-owner";
        let before = lillux::custody::abandoned_custody().count(KIND);
        let mut owner = emptied_finalization_owner();
        owner.held.get_mut().inner = DbOwner::Closed;
        drop(owner);
        assert_eq!(lillux::custody::abandoned_custody().count(KIND), before + 1);
    }

    #[test]
    fn finish_outside_handoff_refusal_keeps_its_owner_unabandoned() {
        const KIND: &str = "ryeos-scheduler::native-finalization-owner";
        let before = lillux::custody::abandoned_custody().count(KIND);
        let owner = emptied_finalization_owner();
        let id = SchedulerNativeStoreHandoffId::new(std::process::id());
        let receipt = unsafe { SchedulerNativeStoreHandoffReceipt::from_store_source_join(&(), id) }.unwrap();
        // No handoff identity was ever issued for this owner, so the receipt
        // names another source set and the owner comes back whole.
        let (mut owner, _error) = owner.finish_outside_handoff(receipt).unwrap_err();
        assert!(owner.held.settled());
        assert_eq!(lillux::custody::abandoned_custody().count(KIND), before);
        let id = owner.outside_handoff_id.get_or_insert_with(||
            SchedulerNativeStoreHandoffId::new(std::process::id())).clone();
        let receipt = unsafe { SchedulerNativeStoreHandoffReceipt::from_store_source_join(&(), id) }.unwrap();
        owner.finish_outside_handoff(receipt).map_err(|(_, error)| error).unwrap();
        assert_eq!(lillux::custody::abandoned_custody().count(KIND), before);
    }

    #[test]
    fn dropped_empty_outside_sources_settle_without_abandoning() {
        const KIND: &str = "ryeos-scheduler::outside-sources";
        let before = lillux::custody::abandoned_custody().count(KIND);
        let sources = SchedulerNativeOutsideSources::new(
            SchedulerNativeStoreHandoffId::new(std::process::id()),
            None,
            None,
        );
        assert!(sources.is_empty());
        drop(sources);
        assert_eq!(lillux::custody::abandoned_custody().count(KIND), before);
    }

    #[test]
    fn dropped_outside_sources_with_unaccepted_database_are_abandoned() {
        const KIND: &str = "ryeos-scheduler::outside-sources";
        let dir = tempfile::tempdir().unwrap();
        let host = ryeos_state::test_support::shared_test_host();
        let runtime = Arc::new(PinnedDirectory::open(dir.path()).unwrap().unwrap());
        // Never-opened inputs hold no ticket, so the host still releases
        // its arena while the abandoned source keeps them.
        // SAFETY: test-only issuance under the shared host's own funded factory.
        let inputs = unsafe {
            OriginalNativeBoundInputs::issued(
                runtime,
                std::ffi::OsStr::new("scheduler.sqlite3"),
                lillux_sqlite::OriginalNativeBoundMode::OpenOrCreate,
                OriginalNativeDomain::NodeScheduler,
                host.destination(),
                None,
            )
        }
        .unwrap();
        let before = lillux::custody::abandoned_custody().count(KIND);
        let sources = SchedulerNativeOutsideSources::new(
            SchedulerNativeStoreHandoffId::new(std::process::id()),
            Some(SchedulerNativeDatabaseSource::from_never_opened(inputs)),
            None,
        );
        assert!(!sources.is_empty());
        drop(sources);
        assert_eq!(lillux::custody::abandoned_custody().count(KIND), before + 1);
        ryeos_state::test_support::release_shared_test_host(host);
    }

    #[test]
    fn dropped_database_source_outside_its_wrapper_is_abandoned() {
        const KIND: &str = "ryeos-scheduler::native-database-source";
        let dir = tempfile::tempdir().unwrap();
        let host = ryeos_state::test_support::shared_test_host();
        let runtime = Arc::new(PinnedDirectory::open(dir.path()).unwrap().unwrap());
        // SAFETY: test-only issuance under the shared host's own funded factory.
        let inputs = unsafe {
            OriginalNativeBoundInputs::issued(
                runtime,
                std::ffi::OsStr::new("scheduler.sqlite3"),
                lillux_sqlite::OriginalNativeBoundMode::OpenOrCreate,
                OriginalNativeDomain::NodeScheduler,
                host.destination(),
                None,
            )
        }
        .unwrap();
        let before = lillux::custody::abandoned_custody().count(KIND);
        // Taking the payload leaves nothing for the shell to retain.
        let taken = SchedulerNativeDatabaseSource::from_never_opened(inputs)
            .into_taken()
            .map_err(|_| "refused")
            .unwrap();
        let TakenSchedulerNativeDatabaseSource::NeverOpened(inputs) = taken else {
            panic!("never-opened inputs come back as never-opened");
        };
        assert_eq!(lillux::custody::abandoned_custody().count(KIND), before);
        drop(SchedulerNativeDatabaseSource::from_never_opened(inputs));
        assert_eq!(lillux::custody::abandoned_custody().count(KIND), before + 1);
        ryeos_state::test_support::release_shared_test_host(host);
    }

    #[test]
    fn transferred_retirement_inputs_drop_without_abandoning() {
        const KIND: &str = "ryeos-scheduler::retirement-inputs";
        let host = TestSchedulerHost::acquire();
        let before = lillux::custody::abandoned_custody().count(KIND);
        let retained = RetainedSchedulerRetirementInputs::new(host.retirement_inputs([7; 16]));
        let inputs = retained.into_inputs().map_err(|_| "refused").unwrap();
        assert_eq!(inputs.generation(), [7; 16]);
        assert_eq!(lillux::custody::abandoned_custody().count(KIND), before);
    }

    #[test]
    fn dropped_retained_retirement_inputs_are_abandoned() {
        const KIND: &str = "ryeos-scheduler::retirement-inputs";
        let host = TestSchedulerHost::acquire();
        let before = lillux::custody::abandoned_custody().count(KIND);
        let retained = RetainedSchedulerRetirementInputs::new(host.retirement_inputs([8; 16]));
        drop(retained);
        assert_eq!(lillux::custody::abandoned_custody().count(KIND), before + 1);
    }
}
