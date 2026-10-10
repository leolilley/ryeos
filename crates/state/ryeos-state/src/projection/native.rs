use std::sync::{Arc, Mutex};

use anyhow::Context;
use lillux_sqlite::{
    ClosedOriginalNativeConnection, OriginalNativeCloseFailure, OriginalNativeConnection,
    OriginalNativeConnectionTerminal, OriginalNativeFactory, OriginalNativeOpenFailure,
    OriginalNativeTransientInputs,
};
use rusqlite::Connection;

use super::ProjectionDb;

pub(super) enum ProjectionConnection {
    Standard(Connection),
    Original {
        connection: OriginalNativeConnection,
        factory: Arc<OriginalNativeFactory>,
    },
}

struct OriginalProjectionFilesystem {
    directory: lillux::PinnedDirectory,
    name: std::ffi::OsString,
    lease: super::ProjectionInstanceLease,
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
    /// A host lease is not an original native generation owner and is refused.
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
                name,
                lease,
            }), None) => match lease {
                super::ProjectionInstanceLease::Original(owner) => Ok(Some((directory, Some(owner)))),
                lease => Err(Self {
                    _filesystem: Some(OriginalProjectionFilesystem {
                        directory,
                        name,
                        lease,
                    }),
                    _setup_directory: None,
                }),
            },
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
    factory: Option<Arc<OriginalNativeFactory>>,
    custody: Mutex<FailureCustody>,
    filesystem: Option<OriginalProjectionFilesystem>,
}

impl OriginalProjectionFailure {
    pub(super) fn retain_projection(db: ProjectionDb, error: anyhow::Error) -> Self {
        let factory = match &db.conn {
            ProjectionConnection::Original { factory, .. } => Some(Arc::clone(factory)),
            ProjectionConnection::Standard(_) => None,
        };
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
        if !self
            .factory
            .as_ref()
            .is_some_and(|owner| owner.same_original(factory))
            || self.filesystem.is_none()
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
        if !self
            .factory
            .as_ref()
            .is_some_and(|original| original.same_original(factory))
        {
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
                    && db._instance_file.is_none()
                    && db._wal_file.is_none()
                    && db._shm_file.is_none()
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
    pub(crate) fn original_factory(&self) -> Option<Arc<OriginalNativeFactory>> {
        match &self.conn {
            ProjectionConnection::Original { factory, .. } => Some(Arc::clone(factory)),
            ProjectionConnection::Standard(_) => None,
        }
    }

    /// Transfer the actual original connection on a caller refusal. The
    /// original error remains visible; no close, terminal or refund is implied.
    pub(crate) fn retain_operation_failure(self, error: anyhow::Error) -> anyhow::Error {
        if self.original_factory().is_some() {
            anyhow::Error::new(OriginalProjectionFailure::retain_projection(self, error))
        } else {
            error
        }
    }

    pub(crate) fn open_original_transient(
        factory: Arc<OriginalNativeFactory>,
        inputs: OriginalNativeTransientInputs,
    ) -> anyhow::Result<Self> {
        let connection = factory.open_transient(inputs).map_err(|failure| {
            anyhow::Error::new(OriginalProjectionFailure {
                error: anyhow::anyhow!("open original projection: {failure}"),
                factory: Some(Arc::clone(&factory)),
                custody: Mutex::new(FailureCustody::Open(failure)),
                filesystem: None,
            })
        })?;
        let db = Self {
            conn: ProjectionConnection::Original {
                connection,
                factory,
            },
            path: ":memory:".into(),
            _instance_directory: None,
            _instance_name: None,
            _instance_file: None,
            _wal_file: None,
            _shm_file: None,
            _instance_lease: None,
        };
        let initialized = db.with_sql(|connection| {
            connection
                .pragma_update(None, "synchronous", "FULL")
                .context("configure original transient projection")?;
            super::init_current_projection_schema(
                connection,
                &super::projection_schema_spec(),
                db.path(),
                None,
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
        lease: super::ProjectionInstanceLease,
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
                    factory: Some(factory),
                    custody: Mutex::new(FailureCustody::Open(failure)),
                    filesystem: Some(filesystem),
                }));
            }
        };
        let db = Self {
            conn: ProjectionConnection::Original {
                connection,
                factory,
            },
            path,
            _instance_directory: Some(filesystem.directory),
            _instance_name: Some(filesystem.name),
            _instance_lease: Some(filesystem.lease),
            _instance_file: None,
            _wal_file: None,
            _shm_file: None,
        };
        let initialized = (|| -> anyhow::Result<()> {
            let ProjectionConnection::Original { connection, .. } = &db.conn else {
                unreachable!("private original generated constructor")
            };
            let identity = connection.bound_main_identity()?;
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
                        Some(schema_reference),
                    )
                } else {
                    super::validate_selected_current(connection, db.path(), Some(schema_reference))
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
                self.original_factory()
                    .is_some_and(|owner| owner.same_original(factory)),
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
                    factory: Some(Arc::clone(factory)),
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
        let selected = match &self.conn {
            ProjectionConnection::Original {
                factory: original, ..
            } => original.same_original(factory),
            ProjectionConnection::Standard(_) => false,
        };
        // Original persistent projections let the native Bound owner retain
        // data/WAL/SHM descriptors. Only the namespace and generation lease
        // are owned here; never drop that lease on a pending native close.
        if !selected
            || self._instance_file.is_some()
            || self._wal_file.is_some()
            || self._shm_file.is_some()
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
            _instance_directory: directory,
            _instance_name: name,
            _instance_lease: lease,
            ..
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
        match conn {
            ProjectionConnection::Original {
                connection,
                factory: original,
            } => match connection.close() {
                Ok(proof) => Ok(ClosedOriginalProjection { proof, filesystem }),
                Err(failure) => Err(OriginalProjectionFailure {
                    error: anyhow::anyhow!("original projection native close pending: {failure}"),
                    factory: Some(original),
                    custody: Mutex::new(FailureCustody::Close(failure)),
                    filesystem,
                }),
            },
            ProjectionConnection::Standard(connection) => {
                // Defensive owner retention, even if the selected check above
                // is changed in a future caller migration.
                let db = Self {
                    conn: ProjectionConnection::Standard(connection),
                    path: ":memory:".into(),
                    _instance_directory: None,
                    _instance_name: None,
                    _instance_file: None,
                    _wal_file: None,
                    _shm_file: None,
                    _instance_lease: None,
                };
                Err(OriginalProjectionFailure::retain_projection(
                    db,
                    anyhow::anyhow!("ordinary projection cannot issue an original native terminal"),
                ))
            }
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
    pub(crate) fn clone_lease(&self) -> anyhow::Result<super::ProjectionInstanceLease> {
        self.filesystem
            .lease
            .share_original()
            .context("retain same original generation lease")
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
