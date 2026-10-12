//! Live Runtime schema and SQL under the original paid native owner. No raw
//! SQLite connection, destructive reset authority or pathname fallback exists
//! in this constructor. Namespace acquisition remains the actual caller's job.

use super::original_native::{AcquisitionCustody, OriginalRuntimeAcquisitionFailure};
use super::*;
use lillux::time::MonotonicDeadline;
use lillux_sqlite::{
    OriginalNativeBoundInputs, OriginalNativeConnection, OriginalNativeDomain,
    OriginalNativeFactory,
};
use std::cell::Cell;
use std::sync::Arc;

pub(super) struct LiveRuntimeConnection {
    owner: OriginalNativeConnection,
    deadline: MonotonicDeadline,
    scope_active: Cell<bool>,
    _schema_reference: sqlite_schema::ClosedNativeSchemaSqlReference,
}

struct LiveDeadlineScope<'a> {
    active: &'a Cell<bool>,
    owner: &'a OriginalNativeConnection,
}
impl Drop for LiveDeadlineScope<'_> {
    fn drop(&mut self) {
        if self.owner.require_original_process().is_err() {
            return;
        }
        self.active.set(false);
    }
}

impl LiveRuntimeConnection {
    pub(super) fn schema_reference(&self) -> &sqlite_schema::SchemaSqlDefinition {
        self._schema_reference.definition()
    }
    pub(super) fn require_close_ready(&self) -> Result<()> {
        self.owner.require_original_process()?;
        ensure!(!self.scope_active.get(), "live Runtime SQL scope remains active");
        Ok(())
    }
    /// The consuming Runtime close retains the exact schema receipt alongside
    /// native close custody. This transfers no SQL access or terminal grant.
    pub(super) fn into_close_parts(self) -> (OriginalNativeConnection, sqlite_schema::ClosedNativeSchemaSqlReference) {
        (self.owner, self._schema_reference)
    }

    pub(super) fn owner(&self) -> &OriginalNativeConnection {
        &self.owner
    }

    pub(super) fn with_sql<T>(
        &self,
        operation: impl for<'c> FnOnce(&'c Connection) -> Result<T>,
    ) -> Result<T> {
        if self.scope_active.get() {
            // An outer SAME connection scope owns the accounting hook. Do
            // not replace it, nest another deadline or return a SQL borrow.
            self.owner.with_sql(operation)
        } else {
            self.with_deadline(self.deadline, operation)
        }
    }

    pub(super) fn with_deadline<T>(
        &self,
        deadline: MonotonicDeadline,
        operation: impl for<'c> FnOnce(&'c Connection) -> Result<T>,
    ) -> Result<T> {
        self.owner.require_original_process()?;
        ensure!(
            !self.scope_active.get(),
            "live Runtime deadline cannot nest"
        );
        self.scope_active.set(true);
        let _scope = LiveDeadlineScope {
            active: &self.scope_active,
            owner: &self.owner,
        };
        // Per-operation: an elapsed window ends only this operation, never
        // the daemon's single Runtime connection.
        self.owner
            .with_sql_until(self.deadline.min(deadline), operation)
    }
}

impl RuntimeDb {
    /// The inputs must be issued by the actual live State namespace owner;
    /// this method does not authorize a destination from an enum/path. All
    /// partial open/schema outcomes retain the same native and namespace owners.
    pub(crate) fn open_live_with_namespace_authority(
        path: &Path,
        root: Arc<lillux::PinnedDirectory>,
        namespace: lillux::PinnedDirectoryLock,
        factory: Arc<OriginalNativeFactory>,
        inputs: OriginalNativeBoundInputs,
        deadline: MonotonicDeadline,
        schema_reference: sqlite_schema::ClosedNativeSchemaSqlReference,
    ) -> std::result::Result<Self, OriginalRuntimeAcquisitionFailure> {
        let mut custody = AcquisitionCustody::BeforeOpen(Some(inputs));
        let result = (|| -> Result<lillux::PinnedDirectory> {
            // Validate the actual retained schema receipt before namespace
            // traversal or native acquisition; its definition is not authority.
            factory.require_original_process()?;
            schema_reference.require_original_closed(
                &factory, OriginalNativeDomain::NodeRuntimeSchemaReference,
            )?;
            ensure!(
                matches!(&custody, AcquisitionCustody::BeforeOpen(Some(inputs))
                    if inputs.domain() == OriginalNativeDomain::NodeRuntime),
                "live Runtime requires its actual NodeRuntime grant"
            );
            ensure!(
                path.parent() == Some(root.path()),
                "live Runtime target namespace mismatch"
            );
            let name = path.file_name().context("live Runtime basename absent")?;
            root.ensure_path_binding()?;
            namespace.ensure_protects(&root)?;
            factory.require_runtime_binding(&root)?;
            let directory = root.try_clone()?;
            // Metadata-only observation: do not open an aliased SQLite inode.
            // The original caller's namespace exclusion spans this and open.
            let created = root.entry_no_follow(name)?.is_none();
            let inputs = match &mut custody {
                AcquisitionCustody::BeforeOpen(inputs) => inputs.take()
                    .context("live Runtime admission inputs already consumed")?,
                _ => anyhow::bail!("live Runtime admission phase changed"),
            };
            let owner = match factory.open_bound(inputs) {
                Ok(owner) => owner,
                Err(failure) => {
                    // The original partial owner precedes any diagnostic
                    // allocation. Propagation never reconstructs its authority.
                    custody = AcquisitionCustody::Open(failure);
                    return Err(anyhow!("live Runtime native acquisition retained"));
                }
            };
            custody = AcquisitionCustody::Live(owner);
            let AcquisitionCustody::Live(owner) = &custody else {
                unreachable!()
            };
            let observed = root
                .entry_no_follow(name)?
                .context("live Runtime acquired target absent")?;
            ensure!(
                observed.entry_type == lillux::PinnedEntryType::Regular
                    && owner.bound_main_identity()? == (observed.containing_device, observed.inode),
                "live Runtime differs from SAME acquired Bound keeper"
            );
            owner.with_sql_until(deadline, |connection| {
                connection.execute_batch("PRAGMA foreign_keys=ON;")?;
                if created {
                    connection.execute_batch("PRAGMA journal_mode=WAL;")?;
                    initialize_current_runtime_schema(connection, path, schema_reference.definition())?;
                }
                let epoch = runtime_operator_schema_epoch(connection, path)?;
                if epoch != RUNTIME_OPERATOR_SCHEMA_EPOCH {
                    let error = incompatible_runtime_operator_schema(epoch);
                    if epoch > RUNTIME_OPERATOR_SCHEMA_EPOCH {
                        return Err(error);
                    }
                    return Err(error).context(format!(
                        "runtime database contains predecessor execution authority and requires the explicit no-backcompat reset ({})",
                        path.display()
                    ));
                }
                validate_current_runtime_store(connection, path, schema_reference.definition())?;
                let integrity: String = connection.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
                ensure!(integrity == "ok", "live Runtime integrity refused: {integrity}");
                let mode: String = connection.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
                ensure!(mode == "wal", "live Runtime requires established WAL");
                connection.execute_batch("BEGIN IMMEDIATE; ROLLBACK;")?;
                Ok(())
            })?;
            namespace.ensure_protects(&root)?;
            root.ensure_path_binding()?;
            Ok(directory)
        })();
        let directory = match result {
            Ok(directory) => directory,
            Err(error) => {
                return Err(OriginalRuntimeAcquisitionFailure::retained(
                    error, factory, root, namespace, custody, Some(schema_reference),
                ));
            }
        };
        let AcquisitionCustody::Live(owner) = custody else {
            unreachable!()
        };
        Ok(Self {
            conn: RuntimeConnection::Live(LiveRuntimeConnection {
                owner,
                deadline,
                scope_active: Cell::new(false),
                _schema_reference: schema_reference,
            }),
            incarnation: RuntimeDbIncarnation(Arc::new(PrivateRuntimeDbIncarnation)),
            reset_target: Some(path.to_owned()),
            original_deadline_active: Cell::new(false),
            reset_required: false,
            open_mode: RuntimeDbOpenMode::Current,
            _directory: Some(directory),
            _directory_lock: Some(namespace),
            _inspection_copy: None,
            _inspection_sql: None,
        })
    }
}

impl RuntimeDb {
    /// Every chain whose Runtime rows still pin live execution, by the same
    /// recovery-pin predicate as [`RuntimeDb::chain_has_live_state`]: chains
    /// named by a `thread_runtime` row or a `runtime_action_intent` row.
    pub(crate) fn chains_with_live_execution(&self) -> Result<Vec<String>> {
        let chains = self.conn.with_sql(|conn| {
            let mut statement = conn.prepare(
                "SELECT chain_root_id FROM thread_runtime
                 UNION SELECT chain_root_id FROM runtime_action_intent
                 ORDER BY 1",
            )?;
            let chains = statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(chains)
        })?;
        let mut live = Vec::new();
        for chain in chains {
            if self.chain_has_live_state(&chain)? {
                live.push(chain);
            }
        }
        Ok(live)
    }
}

/// Read, on a stopped node's native host, every chain whose Runtime rows
/// still pin live execution. The established current-schema Runtime store at
/// `path` opens on the host's tickets, is read, and closes; every terminal
/// reaches `retain`. A store at another schema epoch refuses at open. Any
/// owner a release or the census refuses is carried in the returned error.
///
/// # Safety
/// The caller holds the exclusive node StateLock and `namespace` for the
/// whole call, and `factory` and `destination` come from one host funded
/// under that same exclusion.
pub(crate) unsafe fn offline_live_execution_chains(
    path: &Path,
    root: Arc<lillux::PinnedDirectory>,
    namespace: lillux::PinnedDirectoryLock,
    factory: &Arc<OriginalNativeFactory>,
    destination: Arc<dyn lillux_sqlite::OriginalNativeDestinationAuthority>,
    deadline: MonotonicDeadline,
    retain: ryeos_state::operational::RetainTerminal<'_>,
) -> Result<Vec<String>> {
    use ryeos_state::operational::CarriedOwners;
    let name = path.file_name().context("Runtime database path has no basename")?;
    // SAFETY: the caller's funded host issues this ticket under its locks.
    let reference_inputs = unsafe {
        lillux_sqlite::OriginalNativeTransientInputs::issued(
            OriginalNativeDomain::NodeRuntimeSchemaReference,
            destination.clone(),
        )
    }?;
    let reference = match sqlite_schema::prepare_native_schema_sql_reference(
        Arc::clone(factory),
        reference_inputs,
        OriginalNativeDomain::NodeRuntimeSchemaReference,
        &runtime_schema_sql(),
    ) {
        Ok(reference) => reference,
        Err(failure) => {
            return Err(ryeos_state::operational::release_schema_reference_failure(
                factory, failure, retain, "Runtime",
            ));
        }
    };
    // SAFETY: the caller's funded host issues this ticket under its locks;
    // the fixed Runtime role and the existing-only mode are chosen here.
    let issued = unsafe {
        OriginalNativeBoundInputs::issued(
            Arc::clone(&root),
            name,
            lillux_sqlite::OriginalNativeBoundMode::Existing,
            OriginalNativeDomain::NodeRuntime,
            destination,
            None,
        )
    };
    let inputs = match issued {
        Ok(inputs) => inputs,
        Err(error) => {
            // The closed reference's terminal joins the census before the
            // refusal returns.
            let (_definition, terminal) = reference.into_original_parts();
            return Err(match retain(terminal) {
                Ok(()) => error,
                Err((terminal, reason)) => CarriedOwners::carry(
                    error.context(format!("Runtime schema reference terminal not retained: {reason:#}")),
                    terminal,
                ),
            });
        }
    };
    let db = match RuntimeDb::open_live_with_namespace_authority(
        path,
        root,
        namespace,
        Arc::clone(factory),
        inputs,
        deadline,
        reference,
    ) {
        Ok(db) => db,
        Err(failure) => {
            let original = anyhow!("open Runtime store for live-work inspection: {failure:#}");
            return Err(match failure.retry_close() {
                Ok(released) => match released.native_terminals(factory) {
                    Ok(terminals) => match retain_all(terminals, retain) {
                        // Its terminals are in the census; the released owner
                        // ends here with its roots and unused inputs.
                        Ok(()) => {
                            drop(released);
                            original
                        }
                        Err((refused, reasons)) => CarriedOwners::carry(
                            original.context(format!("Runtime terminal not retained: {reasons}")),
                            (released, refused),
                        ),
                    },
                    Err(refusal) => CarriedOwners::carry(
                        original.context(format!("released Runtime census refused: {refusal:#}")),
                        released,
                    ),
                },
                Err(failure) => CarriedOwners::carry(
                    original.context("refused Runtime acquisition release retained"),
                    failure,
                ),
            });
        }
    };
    let chains = db.chains_with_live_execution();
    let closed = match db.close_original_runtime(Arc::clone(factory)) {
        Ok(closed) => closed,
        Err(failure) => {
            let error = anyhow!("close Runtime store after live-work inspection: {failure:#}");
            return Err(CarriedOwners::carry(error, failure));
        }
    };
    let terminals = match closed.native_terminals() {
        Ok(terminals) => terminals,
        Err(refusal) => {
            return Err(CarriedOwners::carry(refusal.context("closed Runtime census refused"), closed));
        }
    };
    if let Err((refused, reasons)) = retain_all(terminals, retain) {
        return Err(CarriedOwners::carry(
            anyhow!("Runtime terminal not retained: {reasons}"),
            (closed, refused),
        ));
    }
    // Its terminals are in the census; the closed facade ends here.
    drop(closed);
    chains
}

/// Hand every terminal to the census; return the refused ones with reasons.
fn retain_all(
    terminals: Vec<lillux_sqlite::OriginalNativeConnectionTerminal>,
    retain: ryeos_state::operational::RetainTerminal<'_>,
) -> std::result::Result<(), (Vec<lillux_sqlite::OriginalNativeConnectionTerminal>, String)> {
    let mut refused = Vec::new();
    let mut reasons = Vec::new();
    for terminal in terminals {
        if let Err((terminal, reason)) = retain(terminal) {
            refused.push(terminal);
            reasons.push(format!("{reason:#}"));
        }
    }
    if refused.is_empty() { Ok(()) } else { Err((refused, reasons.join("; "))) }
}
