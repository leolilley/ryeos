//! Native custody shared by the external supervisor and guest journals.
//!
//! Both journals open through the vendored SQLite's native gate, on tickets of
//! the one native host the supervisor process funds from its node-signed
//! occurrence envelope. Every journal is a WAL store with `synchronous=FULL`.
//!
//! Crash semantics. Under WAL with `synchronous=FULL`, a commit appends its
//! frames to `{DB}-wal` and fsyncs that file before the commit returns, so a
//! returned commit is durable once its frames are on disk; the main file is
//! only rewritten by a checkpoint, in place, on the same inode. A crash
//! between a commit and its checkpoint leaves committed frames in `{DB}-wal`.
//! The next native `Existing` open pins the same main inode (its identity is
//! checked against the independently retained outer anchor before and after
//! the open), pins the surviving `-wal` and `-shm` as distinct regular files,
//! and SQLite replays every frame up to the last valid commit frame when the
//! first read rebuilds the WAL index. Frames past the last commit, or a torn
//! tail whose salt or checksum chain does not match, are discarded by that
//! same checksum walk, so an uncommitted transaction never becomes visible.
//! `{DB}-shm` is only an index of `{DB}-wal`: the sole connection rebuilds it
//! from the WAL, so a stale one is harmless. The `{DB}-wal` directory entry is
//! made durable by a directory fsync right after every open configures its
//! connection (whose first read creates the sidecars), before any commit on
//! that connection, schema creation included, can rely on it; SQLite never
//! deletes or recreates it afterwards because the bound VFS refuses deletion. A `{DB}-journal` file can only be
//! left by a foreign rollback-mode writer, which none of these stores ever
//! is, so its presence is refused rather than replayed.

use std::ffi::OsStr;
use std::sync::Arc;

use anyhow::{Result, ensure};
use lillux_sqlite::{
    OriginalNativeBoundInputs, OriginalNativeBoundMode, OriginalNativeCloseFailure,
    OriginalNativeConnection, OriginalNativeConnectionTerminal, OriginalNativeDestinationAuthority,
    OriginalNativeDomain, OriginalNativeFactory, OriginalNativeTransientInputs,
};
use rusqlite::Connection;

use crate::operational::{CarriedOwners, RetainTerminal, release_schema_reference_failure};
use crate::sqlite_schema::{self, SchemaSqlDefinition};

/// Open the journal database `name` beneath `directory` on one bound ticket
/// of `factory`. A refused open releases its partial owner here; the released
/// terminal, or the owner whose release was refused, rides the returned error
/// inside [`CarriedOwners`].
///
/// # Safety
/// The caller holds the exclusive lifetime lock of `directory` for as long as
/// the returned connection lives, and `factory` and `destination` come from
/// the one native host funded for this process.
pub(super) unsafe fn open_bound(
    directory: Arc<lillux::PinnedDirectory>,
    name: &OsStr,
    mode: OriginalNativeBoundMode,
    domain: OriginalNativeDomain,
    factory: &Arc<OriginalNativeFactory>,
    destination: Arc<dyn OriginalNativeDestinationAuthority>,
) -> Result<OriginalNativeConnection> {
    factory.require_runtime_binding(&directory)?;
    // SAFETY: forwarded from this function's contract; the fixed journal
    // name, mode and domain are chosen by the journal, not by input.
    let inputs = unsafe {
        OriginalNativeBoundInputs::issued(directory, name, mode, domain, destination, None)
    }?;
    match factory.open_bound(inputs) {
        Ok(connection) => Ok(connection),
        Err(failure) => {
            let original = anyhow::anyhow!("open native external journal: {failure}");
            Err(match failure.retry_close() {
                Ok(released) => CarriedOwners::carry(
                    original,
                    OriginalNativeConnectionTerminal::Released(released),
                ),
                Err(failure) => CarriedOwners::carry(
                    original.context("refused journal acquisition release retained"),
                    failure,
                ),
            })
        }
    }
}

/// Build the declared journal schema `ddl` on one transient ticket of
/// `factory` as `domain`, close that reference, and hand its terminal to
/// `retain` before returning the captured definition. The journal's own
/// connection compares its complete schema SQL against this definition, so
/// no reference database is opened while a native SQL scope is entered.
/// A refused reference, or a terminal `retain` refuses, keeps its owner
/// inside the returned error.
///
/// # Safety
/// The caller holds the exclusive lifetime lock of the journal directory
/// for the whole call, and `factory` and `destination` come from the one
/// native host funded for this process.
pub(super) unsafe fn capture_schema_reference(
    domain: OriginalNativeDomain,
    ddl: &str,
    factory: &Arc<OriginalNativeFactory>,
    destination: Arc<dyn OriginalNativeDestinationAuthority>,
    retain: RetainTerminal<'_>,
) -> Result<SchemaSqlDefinition> {
    ensure!(
        matches!(
            domain,
            OriginalNativeDomain::ExternalSupervisorJournalSchemaReference
                | OriginalNativeDomain::ExternalGuestJournalSchemaReference
        ),
        "requested purpose is not an external journal schema reference"
    );
    // SAFETY: forwarded from this function's contract; the fixed reference
    // purpose and the declared DDL are chosen by the journal, not by input.
    let inputs = unsafe { OriginalNativeTransientInputs::issued(domain, destination) }?;
    let reference = match sqlite_schema::prepare_native_schema_sql_reference(
        Arc::clone(factory),
        inputs,
        domain,
        ddl,
    ) {
        Ok(reference) => reference,
        Err(failure) => {
            return Err(release_schema_reference_failure(
                factory,
                failure,
                retain,
                "external journal",
            ));
        }
    };
    let checked = reference.require_original_closed(factory, domain);
    let (definition, terminal) = reference.into_original_parts();
    match (checked, retain(terminal)) {
        (Ok(()), Ok(())) => Ok(definition),
        (Err(error), Ok(())) => Err(error.context("external journal schema reference refused")),
        (checked, Err((terminal, refusal))) => {
            let error = match checked {
                Ok(()) => refusal,
                Err(error) => error.context(format!("{refusal:#}")),
            };
            Err(CarriedOwners::carry(
                error.context("external journal schema reference terminal not retained"),
                terminal,
            ))
        }
    }
}

/// One journal's native connection: live, or a close the native layer
/// refused, kept until a retry closes it.
pub(super) enum NativeJournalConnection {
    Live(OriginalNativeConnection),
    CloseRetained(OriginalNativeCloseFailure),
}

impl NativeJournalConnection {
    pub(super) fn with_sql<T>(
        &self,
        operation: impl for<'c> FnOnce(&'c Connection) -> Result<T>,
    ) -> Result<T> {
        match self {
            Self::Live(connection) => connection.with_sql(operation),
            Self::CloseRetained(_) => {
                anyhow::bail!("external journal close is retained; no SQL runs on it")
            }
        }
    }

    /// The durable identity of the main file the native keeper holds, computed
    /// from the keeper's descriptor after it revalidated that its name still
    /// binds that inode.
    pub(super) fn main_identity(&self) -> Result<lillux::PinnedRegularFileIdentity> {
        match self {
            Self::Live(connection) => connection.bound_main_file_identity(),
            Self::CloseRetained(_) => {
                anyhow::bail!("external journal close is retained; no identity is observed")
            }
        }
    }

    fn close_owner(
        self,
    ) -> std::result::Result<
        OriginalNativeConnectionTerminal,
        (OriginalNativeCloseFailure, anyhow::Error),
    > {
        let failure = match self {
            Self::Live(connection) => match connection.close() {
                Ok(closed) => return Ok(OriginalNativeConnectionTerminal::Closed(closed)),
                Err(failure) => failure,
            },
            Self::CloseRetained(failure) => failure,
        };
        match failure.retry() {
            Ok(closed) => Ok(OriginalNativeConnectionTerminal::Closed(closed)),
            Err(failure) => {
                let error = anyhow::anyhow!("external journal native close retained: {failure}");
                Err((failure, error))
            }
        }
    }

    /// Close, retrying once; a refusal keeps the close owner.
    pub(super) fn close(
        self,
    ) -> std::result::Result<OriginalNativeConnectionTerminal, (Self, anyhow::Error)> {
        self.close_owner()
            .map_err(|(failure, error)| (Self::CloseRetained(failure), error))
    }

    /// Close this connection because `error` refused its journal, and carry
    /// the terminal, or the refused close owner, inside the returned error.
    pub(super) fn refuse(self, error: anyhow::Error) -> anyhow::Error {
        match self.close_owner() {
            Ok(terminal) => CarriedOwners::carry(error, terminal),
            Err((failure, close)) => {
                CarriedOwners::carry(error.context(format!("{close:#}")), failure)
            }
        }
    }
}

/// Establish and verify the journal's durability contract on one connection:
/// an established WAL store (never converted from rollback mode),
/// `synchronous=FULL` and enforced foreign keys, each read back.
pub(super) fn configure(conn: &Connection, label: &str) -> Result<()> {
    // Reading the schema first makes SQLite observe the WAL header, so the
    // mode reported below is the stored one rather than the pager default.
    let _: i64 = conn.query_row("SELECT COUNT(*) FROM sqlite_schema", [], |row| row.get(0))?;
    let established: String = conn.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
    ensure!(
        established == "wal",
        "{label} is not an established WAL store ({established})"
    );
    let mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
    conn.pragma_update(None, "synchronous", "FULL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    let synchronous: i64 = conn.query_row("PRAGMA synchronous", [], |row| row.get(0))?;
    let foreign_keys: i64 = conn.query_row("PRAGMA foreign_keys", [], |row| row.get(0))?;
    ensure!(
        mode == "wal" && synchronous == 2 && foreign_keys == 1,
        "{label} refused WAL, synchronous=FULL or foreign keys ({mode}, {synchronous}, {foreign_keys})"
    );
    Ok(())
}

/// Admit only the database and its WAL sidecars in a journal directory, each
/// a regular file when present, and refuse any rollback journal.
pub(super) fn require_wal_entries(
    directory: &lillux::PinnedDirectory,
    database: &str,
    label: &str,
) -> Result<()> {
    let wal = format!("{database}-wal");
    let shm = format!("{database}-shm");
    let rollback = format!("{database}-journal");
    ensure!(
        directory.entry_no_follow(OsStr::new(&rollback))?.is_none(),
        "{label} has a rollback journal; only a foreign rollback-mode writer leaves one"
    );
    for name in directory.entry_names()? {
        ensure!(
            name == OsStr::new(database) || name == OsStr::new(&wal) || name == OsStr::new(&shm),
            "{label} has an ambient entry: {}",
            directory.path().join(name).display()
        );
    }
    for name in [wal, shm] {
        if let Some(entry) = directory.entry_no_follow(OsStr::new(&name))? {
            ensure!(
                entry.entry_type == lillux::PinnedEntryType::Regular,
                "{label} has a non-regular sidecar {name}"
            );
        }
    }
    Ok(())
}

/// Hand every journal terminal a refusal carries to `retain`, retrying any
/// carried close first. The returned error is the refusal itself, still
/// carrying whatever owner could not be settled. A journal refusal raised
/// after its native open carries its closed terminal this way; call this
/// before the native host finishes so the census stays exhaustive.
pub fn settle_journal_refusal(error: anyhow::Error, retain: RetainTerminal<'_>) -> anyhow::Error {
    let top_level = error
        .chain()
        .next()
        .is_some_and(|top| top.is::<CarriedOwners>());
    if !top_level {
        return error;
    }
    let carried = match error.downcast::<CarriedOwners>() {
        Ok(carried) => carried,
        Err(error) => return error,
    };
    let (mut error, owners) = carried.into_parts();
    for owner in owners {
        let owner = match owner.downcast::<OriginalNativeConnectionTerminal>() {
            Ok(terminal) => match retain(*terminal) {
                Ok(()) => continue,
                Err((terminal, reason)) => {
                    error = CarriedOwners::carry(
                        error.context(format!("journal terminal not retained: {reason:#}")),
                        terminal,
                    );
                    continue;
                }
            },
            Err(owner) => owner,
        };
        let owner = match owner.downcast::<OriginalNativeCloseFailure>() {
            Ok(failure) => match failure.retry() {
                Ok(closed) => match retain(OriginalNativeConnectionTerminal::Closed(closed)) {
                    Ok(()) => continue,
                    Err((terminal, reason)) => {
                        error = CarriedOwners::carry(
                            error.context(format!("journal terminal not retained: {reason:#}")),
                            terminal,
                        );
                        continue;
                    }
                },
                Err(failure) => {
                    error = CarriedOwners::carry(error, failure);
                    continue;
                }
            },
            Err(owner) => owner,
        };
        error = CarriedOwners::carry(error, owner);
    }
    error
}

/// Test settlement of journal terminals into a test native host's census.
#[cfg(any(test, feature = "test-support"))]
pub mod testing {
    use super::*;
    use lillux_sqlite::test_host::TestNativeHost;

    /// Settle a refused journal call into `host`'s census and return its text.
    pub fn refusal<T>(host: &TestNativeHost, result: Result<T>) -> String {
        let Err(error) = result else {
            panic!("journal call was admitted where a refusal was expected")
        };
        let message = format!("{error:#}");
        let left = settle_journal_refusal(error, &|terminal| {
            host.retain_terminal(terminal);
            Ok(())
        });
        assert!(
            !left
                .chain()
                .next()
                .is_some_and(|top| top.is::<CarriedOwners>()),
            "journal refusal kept an unsettled owner: {left:#}"
        );
        message
    }

    /// A census retention into `host` for journal opens that capture their
    /// schema reference.
    pub fn census(
        host: &TestNativeHost,
    ) -> impl Fn(
        OriginalNativeConnectionTerminal,
    ) -> std::result::Result<(), (OriginalNativeConnectionTerminal, anyhow::Error)>
    + '_ {
        move |terminal| {
            host.retain_terminal(terminal);
            Ok(())
        }
    }

    /// Record a journal close in `host`'s census.
    pub fn retire<J>(
        host: &TestNativeHost,
        closed: std::result::Result<OriginalNativeConnectionTerminal, (J, anyhow::Error)>,
    ) {
        match closed {
            Ok(terminal) => host.retain_terminal(terminal),
            Err((_journal, error)) => panic!("journal close retained: {error:#}"),
        }
    }

    /// A pinned journal directory for a test's direct read or mutation of a
    /// closed journal through the native gate.
    pub fn root(path: &std::path::Path) -> Arc<lillux::PinnedDirectory> {
        Arc::new(lillux::PinnedDirectory::open(path).unwrap().unwrap())
    }
}
