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
        self.owner
            .with_sql_deadline(self.deadline.min(deadline), operation)
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
            owner.with_sql_deadline(deadline, |connection| {
                connection.execute_batch("PRAGMA foreign_keys=ON;")?;
                if created {
                    connection.execute_batch("PRAGMA journal_mode=WAL;")?;
                    initialize_current_runtime_schema(connection, path, Some(schema_reference.definition()))?;
                }
                let epoch = runtime_operator_schema_epoch(connection, path)?;
                ensure!(epoch == RUNTIME_OPERATOR_SCHEMA_EPOCH, "live Runtime requires current operator epoch; explicit stopped history retirement required");
                validate_current_runtime_store(connection, path, Some(schema_reference.definition()))?;
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
            _database_file: None,
            _wal_file: None,
            _shm_file: None,
            _inspection_copy: None,
            _inspection_sql: None,
        })
    }
}
