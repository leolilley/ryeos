use std::sync::{Arc, Mutex};

use anyhow::Context;
use lillux_sqlite::{
    ClosedOriginalNativeConnection, OriginalNativeCloseFailure, OriginalNativeConnection,
    OriginalNativeConnectionTerminal, OriginalNativeFactory, OriginalNativeOpenFailure,
    OriginalNativeTransientInputs,
};
use super::ProjectionDb;

/// The native connection of a projection and the original factory that
/// issued its ticket.
pub(super) struct ProjectionConnection {
    pub(super) connection: OriginalNativeConnection,
    pub(super) factory: Arc<OriginalNativeFactory>,
}

struct OriginalProjectionFilesystem {
    directory: lillux::PinnedDirectory,
    name: std::ffi::OsString,
    lease: Arc<super::OriginalGenerationLease>,
}

/// Actual filesystem holders moved out of a terminal projection. No native
/// receipt/factory remains here, and retaining these descriptors is not their
/// disposal. The enclosing original controller must finish their obligations.
pub(crate) struct OutsideOriginalProjectionCustody {
    _filesystem: Option<OriginalProjectionFilesystem>,
    // Directory of a generation whose sidecar setup failed before any native
    // lease owner existed; it joins root registration without a lease.
    _setup_directory: Option<lillux::PinnedDirectory>,
}

impl OutsideOriginalProjectionCustody {
    /// Actual directory holder of a failed generation-lease setup. No lease
    /// owner or native receipt exists for it.
    pub(crate) fn lease_setup_directory(directory: lillux::PinnedDirectory) -> Self {
        Self { _filesystem: None, _setup_directory: Some(directory) }
    }
    /// Transfer actual filesystem holders without closing or duplicating them.
    /// Custody holding both a generation and a setup directory is refused.
    pub(crate) fn try_into_original_filesystem(
        self,
    ) -> std::result::Result<
        Option<(lillux::PinnedDirectory, Option<Arc<super::OriginalGenerationLease>>)>,
        Self,
    > {
        match (self._filesystem, self._setup_directory) {
            (None, None) => Ok(None),
            (None, Some(directory)) => Ok(Some((directory, None))),
            (Some(OriginalProjectionFilesystem {
                directory,
                name: _,
                lease,
            }), None) => Ok(Some((directory, Some(lease)))),
            (filesystem, setup) => Err(Self { _filesystem: filesystem, _setup_directory: setup }),
        }
    }
}

/// Actual native close evidence with the generation's filesystem custody.
/// A SQL terminal does not acknowledge closing the retained lease descriptor.
pub(crate) struct ClosedOriginalProjection {
    proof: Arc<ClosedOriginalNativeConnection>,
    filesystem: Option<OriginalProjectionFilesystem>,
}
impl ClosedOriginalProjection {
    /// Only the consuming State post-ACK boundary may call this, after genuine
    /// aggregate terminal verification and generation lease holder retirement.
    pub(crate) fn into_outside_custody(
        self,
    ) -> (
        OutsideOriginalProjectionCustody,
        Arc<ClosedOriginalNativeConnection>,
    ) {
        let Self { proof, filesystem } = self;
        (
            OutsideOriginalProjectionCustody {
                _filesystem: filesystem,
                _setup_directory: None,
            },
            proof,
        )
    }
    pub(crate) fn native_proof(&self) -> Arc<ClosedOriginalNativeConnection> {
        Arc::clone(&self.proof)
    }
    pub(crate) fn require_original(
        &self,
        factory: &Arc<OriginalNativeFactory>,
    ) -> anyhow::Result<()> {
        self.proof.require_original(factory)?;
        if let Some(filesystem) = &self.filesystem {
            filesystem.directory.ensure_path_binding()?;
            let observed = filesystem
                .directory
                .entry_no_follow(&filesystem.name)?
                .context("closed original projection target absent")?;
            anyhow::ensure!(
                observed.entry_type == lillux::secure_fs::PinnedEntryType::Regular
                    && self.proof.bound_main_identity()
                        == Some((observed.containing_device, observed.inode)),
                "closed original projection differs from retired Bound keeper"
            );
            anyhow::ensure!(
                filesystem.lease.metadata()?.is_file(),
                "closed original generation lease not regular"
            );
        }
        Ok(())
    }
}

enum FailureCustody {
    Open(OriginalNativeOpenFailure),
    Projection(ProjectionDb),
    Close(OriginalNativeCloseFailure),
    Terminal(OriginalNativeConnectionTerminal),
    Durability(lillux_sqlite::OriginalNativeDurabilityFailure),
}

/// An actual failed acquisition/initialization/close, retaining its native
/// owner. Cleanup of this owner never changes the original operation to success.
pub struct OriginalProjectionFailure {
    error: anyhow::Error,
    factory: Arc<OriginalNativeFactory>,
    custody: Mutex<FailureCustody>,
    filesystem: Option<OriginalProjectionFilesystem>,
}

impl OriginalProjectionFailure {
    pub(super) fn retain_projection(db: ProjectionDb, error: anyhow::Error) -> Self {
        let factory = Arc::clone(&db.conn.factory);
        Self {
            error,
            factory,
            custody: Mutex::new(FailureCustody::Projection(db)),
            filesystem: None,
        }
    }

    /// Retry the retained filesystem durability owner without reopening SQL.
    /// The initial operation remains refused even when durability completes.
    pub fn retry_durable(
        self,
        factory: &Arc<OriginalNativeFactory>,
    ) -> Result<RecoveredOriginalProjectionDurability, Self> {
        if factory.require_original_process().is_err() {
            return Err(self);
        }
        if !self.factory.same_original(factory) || self.filesystem.is_none()
        {
            return Err(self);
        }
        let Self {
            error,
            factory: original,
            custody,
            filesystem,
        } = self;
        let custody = custody.into_inner().unwrap_or_else(|p| p.into_inner());
        let FailureCustody::Durability(failure) = custody else {
            return Err(Self {
                error,
                factory: original,
                custody: Mutex::new(custody),
                filesystem,
            });
        };
        match failure.retry() {
            Ok(proof) => Ok(RecoveredOriginalProjectionDurability {
                error,
                proof: Arc::new(DurableOriginalProjection {
                    proof,
                    filesystem: filesystem.expect("checked retained generation filesystem"),
                }),
            }),
            Err(failure) => Err(Self {
                error,
                factory: original,
                custody: Mutex::new(FailureCustody::Durability(failure)),
                filesystem,
            }),
        }
    }

    /// Retry the SAME owner under the SAME original factory. No re-open, new
    /// ticket or renewed cleanup budget is issued here.
    pub fn retry_close(
        self,
        factory: &Arc<OriginalNativeFactory>,
    ) -> Result<ReleasedOriginalProjectionAcquisition, Self> {
        if factory.require_original_process().is_err() {
            return Err(self);
        }
        if !self.factory.same_original(factory) {
            return Err(self);
        }
        let Self {
            mut error,
            factory: original,
            custody,
            mut filesystem,
        } = self;
        let custody = custody
            .into_inner()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let result = match custody {
            FailureCustody::Open(mut failure) => {
                if failure.retained_admission().is_some() {
                    if let Err(refusal) = failure.recover_retained_admission() {
                        error = refusal.context(error);
                        Err(FailureCustody::Open(failure))
                    } else {
                        failure.retry_close().map(OriginalNativeConnectionTerminal::Released).map_err(FailureCustody::Open)
                    }
                } else {
                    failure.retry_close().map(OriginalNativeConnectionTerminal::Released).map_err(FailureCustody::Open)
                }
            }
            FailureCustody::Projection(mut db) => {
                // Keep the real generation lease even when SQL-close succeeds;
                // close_original otherwise releases its local filesystem owner.
                if db._instance_directory.is_some()
                    && db._instance_name.is_some()
                    && db._instance_lease.is_some()
                {
                    filesystem = Some(OriginalProjectionFilesystem {
                        directory: db
                            ._instance_directory
                            .take()
                            .expect("retained original directory"),
                        name: db._instance_name.take().expect("retained original name"),
                        lease: db._instance_lease.take().expect("retained original lease"),
                    });
                }
                db.close_original(factory)
                    .map(|closed| OriginalNativeConnectionTerminal::Closed(closed.native_proof()))
                    .map_err(|failure| {
                        if failure.filesystem.is_some() {
                            filesystem = failure.filesystem;
                        }
                        failure
                            .custody
                            .into_inner()
                            .unwrap_or_else(|p| p.into_inner())
                    })
            }
            FailureCustody::Close(failure) => failure
                .retry()
                .map(OriginalNativeConnectionTerminal::Closed)
                .map_err(FailureCustody::Close),
            FailureCustody::Terminal(terminal) => Ok(terminal),
            FailureCustody::Durability(failure) => {
                // Cleanup returns true SQL-close evidence only; it does not
                // turn the refused publication into durable success.
                Ok(OriginalNativeConnectionTerminal::Closed(failure.closed()))
            }
        };
        let terminal = match result {
            Ok(terminal) => terminal,
            Err(custody) => {
                return Err(Self {
                    error,
                    factory: original,
                    custody: Mutex::new(custody),
                    filesystem,
                });
            }
        };
        let verified = match &terminal {
            OriginalNativeConnectionTerminal::Closed(proof) => proof.require_original(factory),
            OriginalNativeConnectionTerminal::Released(proof) => proof.require_original(factory),
        };
        if verified.is_err() {
            return Err(Self {
                error,
                factory: original,
                custody: Mutex::new(FailureCustody::Terminal(terminal)),
                filesystem,
            });
        }
        Ok(ReleasedOriginalProjectionAcquisition {
            error,
            terminal,
            filesystem,
        })
    }
}

impl std::fmt::Debug for OriginalProjectionFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OriginalProjectionFailure")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Display for OriginalProjectionFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}
impl std::error::Error for OriginalProjectionFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}

/// Genuine native cleanup evidence for an acquisition that remains refused.
pub struct ReleasedOriginalProjectionAcquisition {
    error: anyhow::Error,
    terminal: OriginalNativeConnectionTerminal,
    // Physical generation custody survives cleanup-close success. Native
    // terminal evidence alone is not a filesystem-disposal acknowledgement.
    filesystem: Option<OriginalProjectionFilesystem>,
}
impl ReleasedOriginalProjectionAcquisition {
    // Only the genuine post-ACK State continuation consumes this original
    // failure. Keep its exact refusal and filesystem owners beside the receipt.
    pub(crate) fn into_outside_custody(self) -> (OutsideOriginalProjectionCustody, OriginalNativeConnectionTerminal, anyhow::Error) {
        let Self { error, terminal, filesystem } = self;
        (OutsideOriginalProjectionCustody { _filesystem: filesystem, _setup_directory: None }, terminal, error)
    }
    pub fn original_error(&self) -> &anyhow::Error {
        &self.error
    }
    pub fn terminal(&self) -> &OriginalNativeConnectionTerminal {
        &self.terminal
    }
}

impl ProjectionDb {
    pub(crate) fn original_factory(&self) -> Arc<OriginalNativeFactory> {
        Arc::clone(&self.conn.factory)
    }

    /// Transfer the actual original connection on a caller refusal. The
    /// original error remains visible; no close, terminal or refund is implied.
    pub(crate) fn retain_operation_failure(self, error: anyhow::Error) -> anyhow::Error {
        anyhow::Error::new(OriginalProjectionFailure::retain_projection(self, error))
    }

    pub(crate) fn open_original_transient(
        factory: Arc<OriginalNativeFactory>,
        inputs: OriginalNativeTransientInputs,
    ) -> anyhow::Result<Self> {
        let connection = factory.open_transient(inputs).map_err(|failure| {
            anyhow::Error::new(OriginalProjectionFailure {
                error: anyhow::anyhow!("open original projection: {failure}"),
                factory: Arc::clone(&factory),
                custody: Mutex::new(FailureCustody::Open(failure)),
                filesystem: None,
            })
        })?;
        let db = Self {
            conn: ProjectionConnection {
                connection,
                factory,
            },
            path: ":memory:".into(),
            _instance_directory: None,
            _instance_name: None,
            _instance_lease: None,
        };
        let initialized = db.with_sql(|connection| {
            connection
                .pragma_update(None, "synchronous", "FULL")
                .context("configure original transient projection")?;
            super::init_fresh_transient_projection_schema(
                connection,
                &super::projection_schema_spec(),
                db.path(),
            )
        });
        match initialized {
            Ok(()) => Ok(db),
            Err(error) => Err(anyhow::Error::new(
                OriginalProjectionFailure::retain_projection(db, error),
            )),
        }
    }

    pub(crate) fn open_original_generated(
        factory: Arc<OriginalNativeFactory>,
        inputs: lillux_sqlite::OriginalNativeBoundInputs,
        directory: lillux::PinnedDirectory,
        name: std::ffi::OsString,
        lease: Arc<super::OriginalGenerationLease>,
        create: bool,
        expected_identity: Option<(u64, u64)>,
        schema_reference: &crate::sqlite_schema::SchemaSqlDefinition,
    ) -> anyhow::Result<Self> {
        // All roots and the real lease are acquired before SQL; the native
        // constructor retains every acquired data/sidecar owner on refusal.
        let path = directory.path().join(&name);
        let filesystem = OriginalProjectionFilesystem {
            directory,
            name,
            lease,
        };
        let connection = match factory.open_bound(inputs) {
            Ok(connection) => connection,
            Err(failure) => {
                return Err(anyhow::Error::new(OriginalProjectionFailure {
                    error: anyhow::anyhow!("open original generated projection: {failure}"),
                    factory,
                    custody: Mutex::new(FailureCustody::Open(failure)),
                    filesystem: Some(filesystem),
                }));
            }
        };
        let db = Self {
            conn: ProjectionConnection {
                connection,
                factory,
            },
            path,
            _instance_directory: Some(filesystem.directory),
            _instance_name: Some(filesystem.name),
            _instance_lease: Some(filesystem.lease),
        };
        let initialized = (|| -> anyhow::Result<()> {
            let identity = db.conn.connection.bound_main_identity()?;
            if let Some(expected) = expected_identity {
                anyhow::ensure!(
                    identity == expected,
                    "selected original projection inode changed"
                );
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                let lease = db
                    ._instance_lease
                    .as_ref()
                    .expect("actual original lease")
                    .metadata()?;
                anyhow::ensure!(
                    (lease.dev(), lease.ino()) != identity,
                    "original projection lease aliases database"
                );
            }
            db.with_sql(|connection| {
                if create {
                    connection.pragma_update(None, "synchronous", "FULL")?;
                    super::init_current_projection_schema(
                        connection,
                        &super::projection_schema_spec(),
                        db.path(),
                        schema_reference,
                    )
                } else {
                    super::validate_selected_current(connection, db.path(), schema_reference)
                }
            })
        })();
        match initialized {
            Ok(()) => Ok(db),
            Err(error) => Err(db.retain_operation_failure(error)),
        }
    }

    pub(crate) fn close_original_generated(
        mut self,
        factory: &Arc<OriginalNativeFactory>,
    ) -> anyhow::Result<Arc<DurableOriginalProjection>> {
        let checked = (|| -> anyhow::Result<()> {
            anyhow::ensure!(
                self.conn.factory.same_original(factory),
                "generated projection has foreign original factory"
            );
            anyhow::ensure!(
                self._instance_directory.is_some()
                    && self._instance_name.is_some()
                    && self._instance_lease.is_some(),
                "generated projection filesystem custody absent"
            );
            self.with_sql(|connection| {
                let (busy, log_frames, checkpointed): (i64, i64, i64) =
                    connection.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                    })?;
                anyhow::ensure!(
                    busy == 0 && log_frames == checkpointed,
                    "original generated projection WAL checkpoint incomplete"
                );
                Ok(())
            })
        })();
        if let Err(error) = checked {
            return Err(self.retain_operation_failure(error));
        }
        let filesystem = OriginalProjectionFilesystem {
            directory: self
                ._instance_directory
                .take()
                .expect("checked original directory"),
            name: self._instance_name.take().expect("checked original name"),
            lease: self._instance_lease.take().expect("checked original lease"),
        };
        let closed = match self.close_original(factory) {
            Ok(closed) => closed,
            Err(mut failure) => {
                failure.filesystem = Some(filesystem);
                return Err(anyhow::Error::new(failure));
            }
        };
        let proof = match closed.native_proof().sync_generated_bound() {
            Ok(proof) => proof,
            Err(failure) => {
                return Err(anyhow::Error::new(OriginalProjectionFailure {
                    error: anyhow::anyhow!(
                        "original generated projection durability pending: {failure}"
                    ),
                    factory: Arc::clone(factory),
                    custody: Mutex::new(FailureCustody::Durability(failure)),
                    filesystem: Some(filesystem),
                }));
            }
        };
        // Keep the exact lease and native proof together through publication
        // and selected reopen. The caller must retain this before later checks.
        Ok(Arc::new(DurableOriginalProjection { proof, filesystem }))
    }

    pub(crate) fn close_original(
        self,
        factory: &Arc<OriginalNativeFactory>,
    ) -> Result<ClosedOriginalProjection, OriginalProjectionFailure> {
        let selected = self.conn.factory.same_original(factory);
        // Original persistent projections let the native Bound owner retain
        // data/WAL/SHM descriptors. Only the namespace and generation lease
        // are owned here; never drop that lease on a pending native close.
        if !selected
            || (self._instance_directory.is_some() != self._instance_name.is_some())
            || (self._instance_directory.is_some() != self._instance_lease.is_some())
        {
            return Err(OriginalProjectionFailure::retain_projection(
                self,
                anyhow::anyhow!(
                    "original projection close requires exact native/filesystem custody"
                ),
            ));
        }
        let Self {
            conn,
            path: _,
            _instance_directory: directory,
            _instance_name: name,
            _instance_lease: lease,
        } = self;
        let filesystem = match (directory, name, lease) {
            (Some(directory), Some(name), Some(lease)) => Some(OriginalProjectionFilesystem {
                directory,
                name,
                lease,
            }),
            (None, None, None) => None,
            _ => unreachable!("checked original projection filesystem custody"),
        };
        let ProjectionConnection { connection, factory: original } = conn;
        match connection.close() {
            Ok(proof) => Ok(ClosedOriginalProjection { proof, filesystem }),
            Err(failure) => Err(OriginalProjectionFailure {
                error: anyhow::anyhow!("original projection native close pending: {failure}"),
                factory: original,
                custody: Mutex::new(FailureCustody::Close(failure)),
                filesystem,
            }),
        }
    }
}

/// The actual staged generation after native close and filesystem durability.
/// No public constructor, serialization or path-only replacement is admitted.
pub(crate) struct DurableOriginalProjection {
    proof: Arc<lillux_sqlite::DurableOriginalNativeBound>,
    filesystem: OriginalProjectionFilesystem,
}
impl DurableOriginalProjection {
    pub(crate) fn require_original(
        &self,
        factory: &Arc<OriginalNativeFactory>,
    ) -> anyhow::Result<()> {
        self.proof.require_original(factory)?;
        self.filesystem.directory.ensure_path_binding()
    }
    pub(crate) fn require_coordinate(
        &self,
        directory: &lillux::PinnedDirectory,
        name: &std::ffi::OsStr,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.filesystem.name == name
                && self.filesystem.directory.is_same_directory(directory)?,
            "original durable projection coordinate changed"
        );
        let file = directory
            .open_mount_entry(name)?
            .context("original durable generation coordinate absent")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let metadata = file.metadata()?;
            anyhow::ensure!(
                metadata.file_type().is_file()
                    && (metadata.dev(), metadata.ino()) == self.proof.main_identity(),
                "original durable generation coordinate changed"
            );
        }
        #[cfg(not(unix))]
        anyhow::bail!("original durable coordinate identity unavailable");
        Ok(())
    }
    pub(crate) fn main_identity(&self) -> (u64, u64) {
        self.proof.main_identity()
    }
    pub(crate) fn native_proof(&self) -> Arc<lillux_sqlite::DurableOriginalNativeBound> {
        Arc::clone(&self.proof)
    }
    pub(crate) fn closed(&self) -> Arc<ClosedOriginalNativeConnection> {
        self.proof.closed()
    }
    /// Only the consuming post-ACK State boundary may call this, after the
    /// aggregate native terminal and generation lease holder retirement. The
    /// durable proof stays an alias of the context's canonical durable owner.
    pub(crate) fn into_outside_custody(
        self,
    ) -> (
        OutsideOriginalProjectionCustody,
        Arc<lillux_sqlite::DurableOriginalNativeBound>,
    ) {
        let Self { proof, filesystem } = self;
        (
            OutsideOriginalProjectionCustody {
                _filesystem: Some(filesystem),
                _setup_directory: None,
            },
            proof,
        )
    }
    pub(crate) fn clone_lease(&self) -> anyhow::Result<Arc<super::OriginalGenerationLease>> {
        let lease = &self.filesystem.lease;
        lease
            .require_active(&lease.factory()?)
            .context("retain same original generation lease")?;
        Ok(Arc::clone(lease))
    }
}

/// A completed same-owner durability retry, preserving the initial refusal.
/// This is not successful publication or acknowledgement of the operation.
pub struct RecoveredOriginalProjectionDurability {
    error: anyhow::Error,
    proof: Arc<DurableOriginalProjection>,
}
impl RecoveredOriginalProjectionDurability {
    pub fn original_error(&self) -> &anyhow::Error {
        &self.error
    }
    pub fn terminal(&self) -> Arc<ClosedOriginalNativeConnection> {
        self.proof.closed()
    }
    pub(crate) fn proof(&self) -> &Arc<DurableOriginalProjection> {
        &self.proof
    }
}

#[cfg(all(test, feature = "test-support"))]
mod native_host_tests {
    use super::*;
    use lillux_sqlite::test_host::TestNativeHost;
    use lillux_sqlite::{OriginalNativeConnectionTerminal, OriginalNativeDomain, OriginalNativeTransientInputs};

    /// The transient skips the reference comparison; its schema must still
    /// equal a reference prepared natively from the same DDL.
    #[test]
    fn fresh_transient_schema_equals_native_projection_reference() {
        let host = TestNativeHost::acquire();
        // SAFETY: test-only issuance under the host's own funded factory.
        let inputs = unsafe {
            OriginalNativeTransientInputs::issued(OriginalNativeDomain::StateTransient, host.destination())
        }
        .unwrap();
        let db = ProjectionDb::open_original_transient(Arc::clone(host.factory()), inputs).unwrap();
        // SAFETY: as above, for the reference purpose.
        let reference_inputs = unsafe {
            OriginalNativeTransientInputs::issued(
                OriginalNativeDomain::NodeProjectionSchemaReference,
                host.destination(),
            )
        }
        .unwrap();
        let reference = crate::sqlite_schema::prepare_native_schema_sql_reference(
            Arc::clone(host.factory()),
            reference_inputs,
            OriginalNativeDomain::NodeProjectionSchemaReference,
            crate::projection::projection_schema_sql(),
        )
        .unwrap_or_else(|failure| panic!("schema reference retained: {failure}"));
        host.retain_terminal(reference.terminal().clone());
        db.with_sql(|connection| reference.definition().require_matches(connection, db.path()))
            .unwrap();
        let closed = db
            .close_original(host.factory())
            .unwrap_or_else(|failure| panic!("transient close retained: {failure}"));
        host.retain_terminal(OriginalNativeConnectionTerminal::Closed(closed.native_proof()));
        assert_eq!(host.ticket_census(), (2, 2));
    }

    /// Under the signed 64 MiB / 20M-step incarnation envelope, the schema
    /// reference must still build while other connections hold their schemas
    /// live. A per-allocation walk over every live block once exhausted the
    /// service steps on the fourth build.
    #[test]
    fn schema_reference_cost_is_flat_with_live_connections_under_production_envelope() {
        use lillux_sqlite::test_host::TestNativeEnvelope;
        let host = TestNativeHost::acquire_with(TestNativeEnvelope {
            storage_bytes: 64 * 1024 * 1024,
            allocator_steps: 20_000_000,
            tickets: 64,
        });
        let steps = || host.factory().budget_snapshot().unwrap().allocator_steps;
        let mut held = Vec::new();
        let mut costs = Vec::new();
        for _ in 0..4 {
            let before = steps();
            let connection = host.open_transient();
            connection
                .with_sql(|sql| Ok(sql.execute_batch(crate::projection::projection_schema_sql())?))
                .unwrap();
            costs.push(steps() - before);
            held.push(connection);
        }
        // SAFETY: test-only issuance under the host's own funded factory.
        let reference_inputs = unsafe {
            OriginalNativeTransientInputs::issued(
                OriginalNativeDomain::NodeProjectionSchemaReference,
                host.destination(),
            )
        }
        .unwrap();
        let before = steps();
        let reference = crate::sqlite_schema::prepare_native_schema_sql_reference(
            Arc::clone(host.factory()),
            reference_inputs,
            OriginalNativeDomain::NodeProjectionSchemaReference,
            crate::projection::projection_schema_sql(),
        )
        .unwrap_or_else(|failure| panic!("schema reference retained: {failure}"));
        costs.push(steps() - before);
        host.retain_terminal(reference.terminal().clone());
        for connection in held {
            host.close(connection);
        }
        let first = costs[0];
        assert!(costs.iter().all(|cost| *cost <= first * 2), "per-build cost grew with live blocks: {costs:?}");
        assert!(first < 200_000, "one projection schema build costs {first} steps");
    }
}
