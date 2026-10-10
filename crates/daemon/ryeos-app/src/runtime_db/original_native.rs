//! Runtime acquisition under the same authentic original native factory.
//! Lower Bound custody owns the SQLite files; this path opens no data aliases.
use super::*;
use lillux_sqlite::{
    OriginalNativeBoundInputs, OriginalNativeCloseFailure, OriginalNativeConnection,
    OriginalNativeConnectionTerminal, OriginalNativeFactory, OriginalNativeOpenFailure,
};
use std::sync::{Arc, Mutex};

pub(super) enum AcquisitionCustody {
    BeforeOpen(Option<OriginalNativeBoundInputs>),
    // Actual unused Bound inputs retired without entering a native producer.
    NotAcquired,
    Open(OriginalNativeOpenFailure),
    Live(OriginalNativeConnection),
    Close(OriginalNativeCloseFailure),
    Terminal(OriginalNativeConnectionTerminal),
}

/// A refused acquisition retains its actual factory, native body and namespace.
/// Neither an error nor dropping it constitutes a cleanup acknowledgement.
pub(crate) struct OriginalRuntimeAcquisitionFailure {
    pub(super) error: anyhow::Error,
    pub(super) factory: std::mem::ManuallyDrop<Arc<OriginalNativeFactory>>,
    pub(super) root: std::mem::ManuallyDrop<Arc<lillux::PinnedDirectory>>,
    pub(super) namespace: std::mem::ManuallyDrop<lillux::PinnedDirectoryLock>,
    pub(super) custody: std::mem::ManuallyDrop<Mutex<AcquisitionCustody>>,
    pub(super) schema_reference: std::mem::ManuallyDrop<Option<sqlite_schema::ClosedNativeSchemaSqlReference>>,
}
impl std::fmt::Debug for OriginalRuntimeAcquisitionFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OriginalRuntimeAcquisitionFailure")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Display for OriginalRuntimeAcquisitionFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}
impl std::error::Error for OriginalRuntimeAcquisitionFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}

/// Actual native disposition after a refused acquisition, retaining the
/// original error, namespace and unused inputs. A never-acquired source has no
/// native terminal; neither case proves physical disposal, success or refund.
pub(crate) struct OriginalRuntimeAcquisitionReleased {
    pub(super) error: anyhow::Error,
    pub(super) factory: Arc<OriginalNativeFactory>,
    pub(super) terminal: Option<OriginalNativeConnectionTerminal>,
    pub(super) root: Arc<lillux::PinnedDirectory>,
    pub(super) namespace: lillux::PinnedDirectoryLock,
    pub(super) _schema_reference: Option<sqlite_schema::ClosedNativeSchemaSqlReference>,
    pub(super) _unused_inputs: Option<OriginalNativeBoundInputs>,
}
impl OriginalRuntimeAcquisitionReleased {
    pub(crate) fn original_error(&self) -> &anyhow::Error {
        &self.error
    }
    pub(crate) fn native_terminals(&self, factory: &Arc<OriginalNativeFactory>)
        -> Result<Vec<OriginalNativeConnectionTerminal>> {
        self.require_native_custody(factory)?;
        let mut terminals: Vec<_> = self.terminal.iter().cloned().collect();
        if let Some(reference) = &self._schema_reference {
            reference.require_original_closed(factory, lillux_sqlite::OriginalNativeDomain::NodeRuntimeSchemaReference)?;
            terminals.push(reference.terminal().clone());
        }
        Ok(terminals)
    }
    pub(crate) fn require_native_custody(&self, factory: &Arc<OriginalNativeFactory>) -> Result<()> {
        factory.require_original_process()?;
        self.factory.require_original_process()?;
        ensure!(
            self.factory.same_original(factory),
            "foreign Runtime acquisition cleanup factory"
        );
        if let Some(terminal) = &self.terminal { terminal.require_original(factory)?; }
        // This is only the native census. Original filesystem/unused-input
        // holders remain retained for their independent outside disposition.
        Ok(())
    }
}
impl OriginalRuntimeAcquisitionFailure {
    pub(super) fn retained(
        error: anyhow::Error,
        factory: Arc<OriginalNativeFactory>,
        root: Arc<lillux::PinnedDirectory>,
        namespace: lillux::PinnedDirectoryLock,
        custody: AcquisitionCustody,
        schema_reference: Option<sqlite_schema::ClosedNativeSchemaSqlReference>,
    ) -> Self {
        // Discard is not disposition: keep original namespace and admission
        // owners inert until the explicit same-process recovery checkout.
        Self {
            error,
            factory: std::mem::ManuallyDrop::new(factory),
            root: std::mem::ManuallyDrop::new(root),
            namespace: std::mem::ManuallyDrop::new(namespace),
            custody: std::mem::ManuallyDrop::new(Mutex::new(custody)),
            schema_reference: std::mem::ManuallyDrop::new(schema_reference),
        }
    }
    /// Consume retained custody before effects; no bookkeeping mutex is held
    /// across native close. Retry the actual partial owner, never reopen SQL.
    pub(crate) fn retry_close(
        self,
    ) -> std::result::Result<OriginalRuntimeAcquisitionReleased, Self> {
        if self.factory.require_original_process().is_err() {
            return Err(self);
        }
        let Self {
            mut error,
            factory,
            root,
            namespace,
            custody,
            schema_reference,
        } = self;
        let factory = std::mem::ManuallyDrop::into_inner(factory);
        let root = std::mem::ManuallyDrop::into_inner(root);
        let namespace = std::mem::ManuallyDrop::into_inner(namespace);
        let schema_reference = std::mem::ManuallyDrop::into_inner(schema_reference);
        let custody = std::mem::ManuallyDrop::into_inner(custody);
        let custody = custody
            .into_inner()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let attempted = match custody {
            AcquisitionCustody::BeforeOpen(Some(inputs)) => {
                // open_bound never consumed these actual inputs. Retain their
                // real authority/root/inspection holders for explicit outside
                // retirement; native absence is not physical disposition.
                Ok((None, Some(inputs)))
            }
            AcquisitionCustody::BeforeOpen(None) => Err(AcquisitionCustody::BeforeOpen(None)),
            AcquisitionCustody::NotAcquired => Ok((None, None)),
            AcquisitionCustody::Open(mut owner) => {
                if owner.retained_admission().is_some() {
                    if let Err(refusal) = owner.recover_retained_admission() {
                        error = refusal.context(error);
                        Err(AcquisitionCustody::Open(owner))
                    } else {
                        owner.retry_close()
                            .map(|proof| (Some(OriginalNativeConnectionTerminal::Released(proof)), None))
                            .map_err(AcquisitionCustody::Open)
                    }
                } else {
                    owner.retry_close()
                        .map(|proof| (Some(OriginalNativeConnectionTerminal::Released(proof)), None))
                        .map_err(AcquisitionCustody::Open)
                }
            }
            AcquisitionCustody::Live(owner) => owner
                .close()
                .map(|proof| (Some(OriginalNativeConnectionTerminal::Closed(proof)), None))
                .map_err(AcquisitionCustody::Close),
            AcquisitionCustody::Close(owner) => owner
                .retry()
                .map(|proof| (Some(OriginalNativeConnectionTerminal::Closed(proof)), None))
                .map_err(AcquisitionCustody::Close),
            AcquisitionCustody::Terminal(terminal) => Ok((Some(terminal), None)),
        };
        let (terminal, unused_inputs) = match attempted {
            Ok(terminal) => terminal,
            Err(custody) => {
                return Err(Self::retained(error, factory, root, namespace,
                    custody, schema_reference));
            }
        };
        // Invalid native evidence cannot become a released frame. Preserve
        // the entire original source, including unused precontact inputs.
        if let Err(refusal) = terminal.as_ref().map_or(Ok(()), |proof| proof.require_original(&factory)) {
            let custody = match terminal {
                Some(terminal) => AcquisitionCustody::Terminal(terminal),
                None => match unused_inputs {
                    Some(inputs) => AcquisitionCustody::BeforeOpen(Some(inputs)),
                    None => AcquisitionCustody::NotAcquired,
                },
            };
            return Err(Self::retained(refusal.context(error), factory, root, namespace,
                custody, schema_reference));
        }
        // A namespace refusal does not undo an actual native close. Retain it
        // as failure evidence alongside the same physical owners, allowing the
        // outside supervisor to retire those holders without admitting use.
        if let Err(refusal) = namespace.ensure_protects(&root).and_then(|()| root.ensure_path_binding()) {
            error = refusal.context(error);
        }
        Ok(OriginalRuntimeAcquisitionReleased {
            error,
            factory,
            terminal,
            root,
            namespace,
            _schema_reference: schema_reference,
            _unused_inputs: unused_inputs,
        })
    }
}

impl RuntimeDb {
    /// Inputs must come from the private original issuer retaining the actual
    /// source reservation and namespace. Existing only: no create, fallback,
    /// new claim, ordinary SQLite connection or schema mutation at acquisition.
    pub(crate) fn open_original_history_reset_mutation(
        path: &Path,
        root: Arc<lillux::PinnedDirectory>,
        namespace: lillux::PinnedDirectoryLock,
        factory: Arc<OriginalNativeFactory>,
        inputs: OriginalNativeBoundInputs,
    ) -> std::result::Result<Self, OriginalRuntimeAcquisitionFailure> {
        let mut custody = AcquisitionCustody::BeforeOpen(Some(inputs));
        let opened = (|| -> Result<(lillux::PinnedDirectory, bool)> {
            factory.require_original_process()?;
            ensure!(
                path.parent() == Some(root.path()),
                "original Runtime target namespace mismatch"
            );
            root.ensure_path_binding()?;
            namespace.ensure_protects(&root)?;
            let retained_root = root.try_clone()?;
            let inputs = match &mut custody {
                AcquisitionCustody::BeforeOpen(inputs) => inputs.take()
                    .context("original Runtime admission inputs already consumed")?,
                _ => anyhow::bail!("original Runtime admission phase changed"),
            };
            let owner = match factory.open_bound(inputs) {
                Ok(owner) => owner,
                Err(failure) => {
                    custody = AcquisitionCustody::Open(failure);
                    return Err(anyhow!("original Runtime native acquisition retained"));
                }
            };
            // Transfer the actual live native owner before any validation.
            custody = AcquisitionCustody::Live(owner);
            let AcquisitionCustody::Live(owner) = &custody else {
                unreachable!()
            };
            let reset_required = owner.with_sql(|connection| {
                connection.execute_batch("PRAGMA foreign_keys=ON;")?;
                let epoch = runtime_operator_schema_epoch(connection, path)?;
                ensure!(
                    epoch <= RUNTIME_OPERATOR_SCHEMA_EPOCH,
                    "original Runtime belongs to a newer operator epoch; destructive reset refused"
                );
                ensure_scope_lifetime_resettable(connection, epoch)?;
                let reset_required = epoch < RUNTIME_OPERATOR_SCHEMA_EPOCH;
                if !reset_required {
                    validate_current_runtime_store(connection, path, None)?;
                }
                let integrity: String =
                    connection.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
                ensure!(
                    integrity == "ok",
                    "original Runtime integrity failed: {integrity}"
                );
                if !reset_required {
                    let mode: String =
                        connection.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
                    ensure!(mode == "wal", "original Runtime journal mode is not WAL");
                }
                Ok(reset_required)
            })?;
            namespace.ensure_protects(&root)?;
            root.ensure_path_binding()?;
            Ok((retained_root, reset_required))
        })();
        let (directory, reset_required) = match opened {
            Ok(value) => value,
            Err(error) => {
                return Err(OriginalRuntimeAcquisitionFailure::retained(
                    error, factory, root, namespace, custody, None,
                ));
            }
        };
        let AcquisitionCustody::Live(owner) = custody else {
            unreachable!()
        };
        Ok(Self {
            conn: RuntimeConnection::OriginalMutation(owner),
            incarnation: RuntimeDbIncarnation(Arc::new(PrivateRuntimeDbIncarnation)),
            reset_target: Some(path.to_path_buf()),
            original_deadline_active: std::cell::Cell::new(false),
            reset_required,
            open_mode: RuntimeDbOpenMode::ExplicitHistoryReset,
            _directory: Some(directory),
            _directory_lock: Some(namespace),
            // The genuine lower Bound keeper exclusively retains these files.
            _database_file: None,
            _wal_file: None,
            _shm_file: None,
            _inspection_copy: None,
            _inspection_sql: None,
        })
    }
}
