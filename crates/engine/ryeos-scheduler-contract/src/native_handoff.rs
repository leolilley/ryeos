//! Custody types for the Scheduler's native database and schema sources as they
//! move from a stopped Scheduler to the node finalizer and its Store
//! inventory, plus the Scheduler schema reference every composition prepares.

use std::sync::Arc;

use anyhow::Result;
use lillux::custody::Held;
use lillux_sqlite::{
    OriginalNativeBoundInputs, OriginalNativeDomain, OriginalNativeFactory,
    OriginalNativeTransientInputs,
};
use ryeos_state::sqlite_schema;

pub const SCHEDULER_SCHEMA_SQL: &str = r#"
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

/// Opaque identity shared by one Scheduler finalization owner and the exact
/// Store source progress that accepts its database and schema owners.
#[derive(Clone)]
pub struct SchedulerNativeStoreHandoffId {
    process: u32,
    identity: Arc<()>,
}

impl SchedulerNativeStoreHandoffId {
    /// # Safety
    ///
    /// A handoff id names one live outside-sources custody minted by the
    /// scheduler store that owns the native connection. Minting one anywhere
    /// else produces an identity no receipt will ever match; the caller must
    /// be the store path that also issues the matching outside sources.
    pub unsafe fn new(process: u32) -> Self {
        Self {
            process,
            identity: Arc::new(()),
        }
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
    /// The handoff identity this receipt proves.
    pub fn id(&self) -> &SchedulerNativeStoreHandoffId {
        &self.id
    }

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
        anyhow::ensure!(
            id.process == std::process::id(),
            "foreign Scheduler Store handoff proof"
        );
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
    /// # Safety
    ///
    /// The parts must be the native connection and schema reference the
    /// scheduler store took out of its own custody under `handoff_id`; the
    /// settle check at drop assumes nothing else ever held them.
    pub unsafe fn new(
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
        anyhow::ensure!(
            !self.held.is_foreign(),
            "foreign Scheduler outside source owner"
        );
        Ok(&self.handoff_id)
    }

    pub fn is_empty(&self) -> bool {
        !self.held.is_foreign() && self.held.database.is_none() && self.held.schema.is_none()
    }

    /// # Safety
    /// The returned source must stay in live custody and must either be
    /// accepted by this handoff's Store progress or restored to this wrapper.
    pub unsafe fn take_database(&mut self) -> Result<Option<SchedulerNativeDatabaseSource>> {
        anyhow::ensure!(
            !self.held.is_foreign(),
            "foreign Scheduler outside source owner"
        );
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
        anyhow::ensure!(
            !self.held.is_foreign(),
            "foreign Scheduler outside source owner"
        );
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

/// Build only the Scheduler's declared schema in the SAME paid factory.
/// Every typed preparation failure must remain in composition custody.
pub fn prepare_scheduler_schema_reference(
    factory: Arc<OriginalNativeFactory>,
    inputs: OriginalNativeTransientInputs,
) -> std::result::Result<
    sqlite_schema::ClosedNativeSchemaSqlReference,
    sqlite_schema::NativeSchemaSqlReferenceFailure,
> {
    sqlite_schema::prepare_native_schema_sql_reference(
        factory,
        inputs,
        OriginalNativeDomain::NodeSchedulerSchemaReference,
        SCHEDULER_SCHEMA_SQL,
    )
}
