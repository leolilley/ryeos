//! Runtime readonly copies admitted by the same original native factory.
//! Actual copy/SQL lifelines and partial native owners survive every refusal.
use super::*;
use lillux_sqlite::{
    OriginalNativeBoundInputs, OriginalNativeBoundMode, OriginalNativeCloseFailure,
    OriginalNativeConnection, OriginalNativeConnectionTerminal, OriginalNativeDestinationAuthority,
    OriginalNativeDomain, OriginalNativeFactory, OriginalNativeOpenFailure,
};
use std::sync::{Arc, Weak};

pub fn quote_inspection_outside_continuation()
    -> anyhow::Result<lillux_sqlite::original_physical_custody::OutsideWork> {
    let bytes = std::mem::size_of::<InspectionOutsideContinuation>()
        .checked_add(std::mem::size_of::<lillux_sqlite::OriginalDestinationOutsideCustody>())
        .and_then(|n| n.checked_add(4 * std::mem::size_of::<lillux_sqlite::OriginalDestinationPhysicalSource>()))
        .context("inspection outside custody quote overflow")?;
    Ok(lillux_sqlite::original_physical_custody::OutsideWork {
        bytes: u64::try_from(bytes)?, steps: 24,
    })
}

pub(super) fn terminal_snapshot(
    process: u32,
    native: &std::sync::Mutex<Option<(Arc<OriginalNativeFactory>, OriginalNativeConnectionTerminal)>>,
) -> Result<Option<OriginalNativeConnectionTerminal>> {
    ensure!(process == std::process::id(), "inspection terminal belongs to original process");
    let native = native.try_lock().map_err(|_| anyhow!("original inspection native custody busy or poisoned"))?;
    Ok(native.as_ref().map(|(_, terminal)| terminal.clone()))
}

struct InspectionDestination {
    factory: Weak<OriginalNativeFactory>,
    runtime: Arc<lillux::PinnedDirectory>,
    root: Arc<lillux::PinnedDirectory>,
    source: Arc<lillux::PinnedDirectory>,
    namespace: lillux::PinnedDirectoryLock,
    copy: Arc<SqliteInspectionCopyLease>,
    sql: Arc<dyn lillux_sqlite::SqliteInspectionSqlWork>,
    work: Arc<dyn lillux_sqlite::SqliteInspectionCopyWork>,
}
impl OriginalNativeDestinationAuthority for InspectionDestination {
    fn into_outside_custody(
        self: Arc<Self>,
        factory: &Arc<OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        leaf: &lillux_sqlite::OriginalNativeOutsideLeafEvidence,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> std::result::Result<lillux_sqlite::OriginalDestinationOutsideCustody,
        lillux_sqlite::OriginalDestinationCustodyFailure> {
        let checked = (|| -> Result<()> {
            let authority: Arc<dyn OriginalNativeDestinationAuthority> = self.clone();
            leaf.require_original_authority(&authority, factory, aggregate)?;
            drop(authority);
            let original = self.factory.upgrade().context("original inspection factory absent")?;
            ensure!(Arc::ptr_eq(&original, factory), "foreign inspection outside factory");
            ensure!(Arc::ptr_eq(&self.copy._copy_work, &self.work),
                "inspection outside handoff changes actual copy work");
            // Service checkpoint and runtime Ready checks are forbidden here.
            self.work.require_original_outside_master(factory, aggregate)?;
            ensure!(Arc::strong_count(&self) == 1,
                "inspection destination still has actual authority aliases");
            let quote = quote_inspection_outside_continuation()?;
            lillux_sqlite::original_physical_custody::retain_callback_refusal(
                &|| prepay(quote.bytes, quote.steps))?;
            self.work.require_original_outside_master(factory, aggregate)
        })();
        if let Err(error) = checked {
            return Err(lillux_sqlite::OriginalDestinationCustodyFailure::retain(factory.clone(), self, error));
        }
        let original = match Arc::try_unwrap(self) {
            Ok(original) => original,
            Err(original) => return Err(lillux_sqlite::OriginalDestinationCustodyFailure::retain(
                factory.clone(), original, anyhow!("inspection authority alias appeared during handoff"))),
        };
        let Self { factory: _, runtime, root, source, namespace, copy, sql, work } = original;
        let continuation = Box::new(InspectionOutsideContinuation {
            process: std::process::id(), leaf: leaf.clone(),
            _copy: copy, _sql: sql, work,
        });
        let sources = vec![
            lillux_sqlite::OriginalDestinationPhysicalSource::Directory(runtime),
            lillux_sqlite::OriginalDestinationPhysicalSource::Directory(root),
            lillux_sqlite::OriginalDestinationPhysicalSource::Directory(source),
            lillux_sqlite::OriginalDestinationPhysicalSource::Namespace(namespace),
        ];
        // SAFETY: genuine exact leaf/account checks and original prepayment
        // precede allocation. Every copy/SQL/work holder moves into the
        // continuation, which keeps it until the consuming group join.
        Ok(unsafe { lillux_sqlite::OriginalDestinationOutsideCustody::issued(sources, continuation) })
    }

    fn require_original(&self, factory: &Arc<OriginalNativeFactory>) -> Result<()> {
        let original = self
            .factory
            .upgrade()
            .context("original inspection factory absent")?;
        ensure!(
            Arc::ptr_eq(&original, factory),
            "foreign Runtime inspection factory"
        );
        ensure!(
            Arc::ptr_eq(&self.copy._copy_work, &self.work),
            "foreign inspection copy producer"
        );
        self.work.require_original_native_factory(factory)?;
        factory.require_runtime_binding(&self.runtime)?;
        self.root.ensure_path_binding()?;
        self.namespace.ensure_protects(&self.root)?;
        self.source.ensure_path_binding()?;
        self.copy._source_lock.ensure_protects(&self.source)?;
        ensure!(
            self.root.is_same_directory(self.copy.directory()?)?,
            "foreign inspection copy root"
        );
        // The SQL owner's own checkpoint requires the native producer
        // interval, which only the native open begins and its terminal ends;
        // this authority is checked before and after that interval as well.
        self.sql.require_copy_work(&self.work)?;
        self.work.checkpoint()
    }
}

/// Destination of the Runtime inspection phase's schema reference. It holds
/// independent runtime and source descriptors and the source namespace, so
/// the reference terminal aliases no copy, SQL or copy-work owner.
struct InspectionReferenceDestination {
    factory: Weak<OriginalNativeFactory>,
    runtime: Arc<lillux::PinnedDirectory>,
    source: Arc<lillux::PinnedDirectory>,
    namespace: lillux::PinnedDirectoryLock,
}
impl OriginalNativeDestinationAuthority for InspectionReferenceDestination {
    fn into_outside_custody(
        self: Arc<Self>,
        factory: &Arc<OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        leaf: &lillux_sqlite::OriginalNativeOutsideLeafEvidence,
        prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> std::result::Result<lillux_sqlite::OriginalDestinationOutsideCustody,
        lillux_sqlite::OriginalDestinationCustodyFailure> {
        let checked = (|| -> Result<()> {
            let authority: Arc<dyn OriginalNativeDestinationAuthority> = self.clone();
            leaf.require_original_authority(&authority, factory, aggregate)?;
            drop(authority);
            let original = self.factory.upgrade().context("original inspection reference factory absent")?;
            ensure!(Arc::ptr_eq(&original, factory), "foreign inspection reference outside factory");
            ensure!(Arc::strong_count(&self) == 1,
                "inspection reference destination still has authority aliases");
            let bytes = std::mem::size_of::<InspectionReferenceContinuation>()
                .checked_add(std::mem::size_of::<lillux_sqlite::OriginalDestinationOutsideCustody>())
                .and_then(|n| n.checked_add(3 * std::mem::size_of::<lillux_sqlite::OriginalDestinationPhysicalSource>()))
                .context("inspection reference outside custody quote overflow")?;
            lillux_sqlite::original_physical_custody::retain_callback_refusal(
                &|| prepay(u64::try_from(bytes)?, 16))
        })();
        if let Err(error) = checked {
            return Err(lillux_sqlite::OriginalDestinationCustodyFailure::retain(factory.clone(), self, error));
        }
        let original = match Arc::try_unwrap(self) {
            Ok(original) => original,
            Err(original) => return Err(lillux_sqlite::OriginalDestinationCustodyFailure::retain(
                factory.clone(), original, anyhow!("inspection reference authority alias appeared during handoff"))),
        };
        let Self { factory: _, runtime, source, namespace } = original;
        let continuation = Box::new(InspectionReferenceContinuation {
            process: std::process::id(), leaf: leaf.clone(),
        });
        let sources = vec![
            lillux_sqlite::OriginalDestinationPhysicalSource::Directory(runtime),
            lillux_sqlite::OriginalDestinationPhysicalSource::Directory(source),
            lillux_sqlite::OriginalDestinationPhysicalSource::Namespace(namespace),
        ];
        // SAFETY: genuine leaf and factory checks and original prepayment
        // precede allocation; every descriptor moves to a physical source.
        Ok(unsafe { lillux_sqlite::OriginalDestinationOutsideCustody::issued(sources, continuation) })
    }

    fn require_original(&self, factory: &Arc<OriginalNativeFactory>) -> Result<()> {
        let original = self
            .factory
            .upgrade()
            .context("original inspection reference factory absent")?;
        ensure!(
            Arc::ptr_eq(&original, factory),
            "foreign Runtime inspection reference factory"
        );
        factory.require_runtime_binding(&self.runtime)?;
        self.source.ensure_path_binding()?;
        self.namespace.ensure_protects(&self.source)
    }
}

/// Semantic-only receiver of the inspection reference destination. Every
/// descriptor moved to a physical source; this holds only the leaf evidence.
struct InspectionReferenceContinuation {
    process: u32,
    leaf: lillux_sqlite::OriginalNativeOutsideLeafEvidence,
}
// SAFETY: no resource is owned here; the leaf binds the same original factory.
unsafe impl lillux_sqlite::OriginalOutsideDestinationContinuation for InspectionReferenceContinuation {
    fn require_original_process(&self) -> Result<()> {
        ensure!(self.process == std::process::id(), "foreign inspection reference continuation");
        Ok(())
    }
    fn retire_native_references(&mut self, factory: &Arc<OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        _prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()> {
        self.require_original_process()?;
        self.leaf.require_original(factory, aggregate)
    }
    unsafe fn register_shared_roots(&mut self,
        _registry: &mut lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        _prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()> {
        self.require_original_process()
    }
    fn physical_holders_retired(&self) -> Result<bool> {
        self.require_original_process()?;
        Ok(true)
    }
    fn physical_holders_retirement_quote(&self) -> Result<lillux_sqlite::original_physical_custody::OutsideWork> {
        self.require_original_process()?;
        Ok(lillux_sqlite::original_physical_custody::OutsideWork { bytes: 0, steps: 4 })
    }
}

/// Inputs for the inspection phase's own schema-reference ticket, under a
/// destination built from independent descriptors of the runtime and source.
pub(super) fn issue_inspection_reference_inputs(
    factory: &Arc<OriginalNativeFactory>,
    runtime: &lillux::PinnedDirectory,
    source: &lillux::PinnedDirectory,
    namespace: &lillux::PinnedDirectoryLock,
) -> Result<lillux_sqlite::OriginalNativeTransientInputs> {
    let authority = Arc::new(InspectionReferenceDestination {
        factory: Arc::downgrade(factory),
        runtime: Arc::new(runtime.try_clone()?),
        source: Arc::new(source.try_clone()?),
        namespace: namespace.clone(),
    });
    authority.require_original(factory)?;
    // SAFETY: the same original Runtime binding, source namespace and paid
    // factory as the readonly inspection; it builds only the declared Runtime
    // schema and grants no data, copy or SQL authority.
    unsafe {
        lillux_sqlite::OriginalNativeTransientInputs::issued(
            OriginalNativeDomain::NodeRuntimeSchemaReference,
            authority,
        )
    }
}

/// Outside continuation of an inspection destination. Opaque copy/SQL/work
/// interfaces still retain source pins, guard, cleanup and account owners;
/// these are plain aliases of owners the destination authority already
/// held, dropped in the authority's own order (copy, SQL, work). No outside
/// receiver consumes them, so this continuation never reports its physical
/// holders retired; inspection copies are disposed in process instead.
struct InspectionOutsideContinuation {
    process: u32,
    leaf: lillux_sqlite::OriginalNativeOutsideLeafEvidence,
    _copy: Arc<SqliteInspectionCopyLease>,
    _sql: Arc<dyn lillux_sqlite::SqliteInspectionSqlWork>,
    work: Arc<dyn lillux_sqlite::SqliteInspectionCopyWork>,
}
// SAFETY: all actual resources remain owned, including on unwind/refusal.
// Incomplete group consumption explicitly blocks physical retirement.
unsafe impl lillux_sqlite::OriginalOutsideDestinationContinuation for InspectionOutsideContinuation {
    fn require_original_process(&self) -> Result<()> {
        ensure!(self.process == std::process::id(), "foreign inspection outside continuation");
        Ok(())
    }
    fn retire_native_references(&mut self, factory: &Arc<OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        _prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()> {
        self.require_original_process()?;
        self.leaf.require_original(factory, aggregate)?;
        self.work.require_original_outside_master(factory, aggregate)?;
        // Copy work still owns the original account/native controls; with no
        // outside receiver this is never a completed retirement.
        anyhow::bail!("inspection copy native/account holders have no outside retirement")
    }
    unsafe fn register_shared_roots(&mut self,
        _registry: &mut lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        _prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()> {
        self.require_original_process()?;
        anyhow::bail!("inspection source pins, scratch guard and cleanup proof have no outside retirement")
    }
    fn physical_holders_retired(&self) -> Result<bool> {
        self.require_original_process()?;
        Ok(false)
    }
    fn physical_holders_retirement_quote(&self) -> Result<lillux_sqlite::original_physical_custody::OutsideWork> {
        self.require_original_process()?;
        Ok(lillux_sqlite::original_physical_custody::OutsideWork { bytes: 0, steps: 8 })
    }
}

enum NativeBody {
    Open(OriginalNativeOpenFailure),
    Live(OriginalNativeConnection),
    Close(OriginalNativeCloseFailure),
    Terminal(OriginalNativeConnectionTerminal),
}
/// The inspection phase's own Runtime schema reference, kept with the main
/// owner. A refused reference keeps its owner until a same-owner close.
pub(super) enum InspectionSchemaReference {
    Closed(sqlite_schema::ClosedNativeSchemaSqlReference),
    Refused(sqlite_schema::NativeSchemaSqlReferenceFailure),
    Released(sqlite_schema::ReleasedNativeSchemaSqlReference),
}
pub(super) struct OriginalInspectionCustody {
    factory: Arc<OriginalNativeFactory>,
    /// Absent only when the reference refused before the main open.
    body: Option<NativeBody>,
    reference: InspectionSchemaReference,
}
impl OriginalInspectionCustody {
    pub(super) fn retry_close(self) -> std::result::Result<Self, Self> {
        let Self { factory, body, reference } = self;
        let reference = match reference {
            InspectionSchemaReference::Refused(failure) => match failure.close_retained() {
                Ok(released) => InspectionSchemaReference::Released(released),
                Err(failure) => {
                    return Err(Self { factory, body, reference: InspectionSchemaReference::Refused(failure) });
                }
            },
            reference @ (InspectionSchemaReference::Closed(_) | InspectionSchemaReference::Released(_)) => reference,
        };
        let Some(body) = body else {
            return Ok(Self { factory, body: None, reference });
        };
        let attempted = match body {
            NativeBody::Open(mut owner) => {
                if owner.retained_admission().is_some()
                    && owner.recover_retained_admission().is_err() {
                        return Err(Self { factory, body: Some(NativeBody::Open(owner)), reference });
                    }
                owner.retry_close()
                    .map(OriginalNativeConnectionTerminal::Released)
                    .map_err(NativeBody::Open)
            }
            NativeBody::Live(owner) => owner
                .close()
                .map(OriginalNativeConnectionTerminal::Closed)
                .map_err(NativeBody::Close),
            NativeBody::Close(owner) => owner
                .retry()
                .map(OriginalNativeConnectionTerminal::Closed)
                .map_err(NativeBody::Close),
            NativeBody::Terminal(terminal) => Ok(terminal),
        };
        match attempted {
            Ok(terminal) => Ok(Self {
                factory,
                body: Some(NativeBody::Terminal(terminal)),
                reference,
            }),
            Err(body) => Err(Self { factory, body: Some(body), reference }),
        }
    }
    /// Share only the actual primitive proof owned by the actual native ticket.
    /// Before-admission release without a copy proof remains retained/refused.
    /// The schema reference always leaves with the release for the census; a
    /// phase that never opened its main owner releases its reference alone.
    pub(super) fn into_release(
        self,
        lease: Arc<SqliteInspectionCopyLease>,
        sql: Arc<dyn lillux_sqlite::SqliteInspectionSqlWork>,
        namespace: lillux::PinnedDirectoryLock,
        database_name: OsString,
    ) -> std::result::Result<(Option<RuntimeInspectionRelease>, RuntimeInspectionReferenceRelease), Self> {
        let reference_ready = match &self.reference {
            InspectionSchemaReference::Closed(reference) => reference
                .require_original_closed(&self.factory, OriginalNativeDomain::NodeRuntimeSchemaReference)
                .is_ok(),
            InspectionSchemaReference::Released(released) => released.terminal(&self.factory).is_ok(),
            InspectionSchemaReference::Refused(_) => false,
        };
        if !reference_ready {
            return Err(self);
        }
        let proofs = match &self.body {
            None => None,
            Some(NativeBody::Terminal(terminal)) => {
                if terminal.require_original(&self.factory).is_err() {
                    return Err(self);
                }
                let proofs = match terminal {
                    OriginalNativeConnectionTerminal::Closed(owner) => (owner.inspection_closed(), None),
                    OriginalNativeConnectionTerminal::Released(owner) => {
                        (owner.inspection_closed(), owner.inspection_released())
                    }
                };
                if proofs.0.is_none() && proofs.1.is_none() {
                    return Err(self);
                }
                Some(proofs)
            }
            Some(NativeBody::Open(_) | NativeBody::Live(_) | NativeBody::Close(_)) => return Err(self),
        };
        let Self { factory, body, reference } = self;
        let reference = match reference {
            InspectionSchemaReference::Closed(reference) => RuntimeInspectionReferenceRelease::Closed(reference),
            InspectionSchemaReference::Released(released) => RuntimeInspectionReferenceRelease::Released {
                factory: Arc::clone(&factory),
                released,
            },
            InspectionSchemaReference::Refused(failure) => {
                return Err(Self { factory, body, reference: InspectionSchemaReference::Refused(failure) });
            }
        };
        let (terminal, (closed, released)) = match (body, proofs) {
            (Some(NativeBody::Terminal(terminal)), Some(proofs)) => (terminal, proofs),
            (None, None) => return Ok((None, reference)),
            _ => unreachable!("main terminal and proofs checked above"),
        };
        if let Some(proof) = closed {
            return Ok((Some(RuntimeInspectionRelease::Closed(Arc::new(
                SqliteInspectionClosed {
                    process: std::process::id(),
                    proof,
                    native: std::sync::Mutex::new(Some((factory, terminal))),
                    lease,
                    sql,
                    database_name,
                    _namespace: Some(namespace),
                },
            ))), reference));
        }
        let proof = released.expect("released inspection proof checked above");
        Ok((Some(RuntimeInspectionRelease::Acquisition(
            SqliteInspectionAcquisitionReleased {
                process: std::process::id(),
                proof,
                native: std::sync::Mutex::new(Some((factory, terminal))),
                lease,
                sql,
                database_name,
                _namespace: Some(namespace),
            },
        )), reference))
    }
}

/// Another store's readonly copy inspection, ended on its own native ticket:
/// the terminal with the copy proof it carries, filed like a Runtime
/// inspection release for the census and copy cleanup. A terminal carrying
/// no copy proof, or failing its factory check, comes back with its factory.
pub(crate) fn retain_copy_inspection_terminal(
    factory: Arc<OriginalNativeFactory>,
    terminal: OriginalNativeConnectionTerminal,
    lease: Arc<SqliteInspectionCopyLease>,
    sql: Arc<dyn lillux_sqlite::SqliteInspectionSqlWork>,
    namespace: lillux::PinnedDirectoryLock,
    database_name: OsString,
) -> std::result::Result<
    RuntimeInspectionRelease,
    (Arc<OriginalNativeFactory>, OriginalNativeConnectionTerminal),
> {
    if terminal.require_original(&factory).is_err() {
        return Err((factory, terminal));
    }
    let (closed, released) = match &terminal {
        OriginalNativeConnectionTerminal::Closed(owner) => (owner.inspection_closed(), None),
        OriginalNativeConnectionTerminal::Released(owner) => {
            (owner.inspection_closed(), owner.inspection_released())
        }
    };
    if let Some(proof) = closed {
        return Ok(RuntimeInspectionRelease::Closed(Arc::new(SqliteInspectionClosed {
            process: std::process::id(),
            proof,
            native: std::sync::Mutex::new(Some((factory, terminal))),
            lease,
            sql,
            database_name,
            _namespace: Some(namespace),
        })));
    }
    let Some(proof) = released else {
        return Err((factory, terminal));
    };
    Ok(RuntimeInspectionRelease::Acquisition(SqliteInspectionAcquisitionReleased {
        process: std::process::id(),
        proof,
        native: std::sync::Mutex::new(Some((factory, terminal))),
        lease,
        sql,
        database_name,
        _namespace: Some(namespace),
    }))
}

/// The inspection phase's schema reference after its phase ended: closed,
/// or a refused reference after its same-owner close. A released reference
/// joins the census only if it paid a ticket.
pub(crate) enum RuntimeInspectionReferenceRelease {
    Closed(sqlite_schema::ClosedNativeSchemaSqlReference),
    Released {
        factory: Arc<OriginalNativeFactory>,
        released: sqlite_schema::ReleasedNativeSchemaSqlReference,
    },
}
impl RuntimeInspectionReferenceRelease {
    /// The census terminal of this reference, if it paid a ticket.
    pub(crate) fn census_terminal(&self) -> Result<Option<OriginalNativeConnectionTerminal>> {
        match self {
            Self::Closed(reference) => Ok(Some(reference.terminal().clone())),
            Self::Released { factory, released } => Ok(released
                .terminal(factory)?
                .filter(|terminal| terminal.ticket().is_some())
                .cloned()),
        }
    }
}

fn failure(
    error: anyhow::Error,
    custody: OriginalInspectionCustody,
    copy: Arc<SqliteInspectionCopyLease>,
    sql: Arc<dyn lillux_sqlite::SqliteInspectionSqlWork>,
    namespace: lillux::PinnedDirectoryLock,
    name: OsString,
) -> anyhow::Error {
    anyhow::Error::new(RuntimeInspectionNativeFailure {
        error,
        custody: std::sync::Mutex::new(Some(RuntimeInspectionNativeCustody::Original(custody))),
        _copy: copy,
        _sql: sql,
        _namespace: namespace,
        database_name: name,
    })
}

/// Close the inspection owner; the closed proof and its schema reference
/// leave together for the census. Every refusal keeps both owners.
pub(super) fn close_inspection(
    owner: OriginalNativeConnection,
    factory: Arc<OriginalNativeFactory>,
    schema_reference: sqlite_schema::ClosedNativeSchemaSqlReference,
    copy: Arc<SqliteInspectionCopyLease>,
    sql: Arc<dyn lillux_sqlite::SqliteInspectionSqlWork>,
    namespace: lillux::PinnedDirectoryLock,
    name: OsString,
) -> Result<(Arc<SqliteInspectionClosed>, sqlite_schema::ClosedNativeSchemaSqlReference)> {
    let reference = InspectionSchemaReference::Closed(schema_reference);
    let terminal = match owner.close() {
        Ok(proof) => OriginalNativeConnectionTerminal::Closed(proof),
        Err(owner) => {
            return Err(failure(
                anyhow!("original Runtime inspection close pending"),
                OriginalInspectionCustody { factory, body: Some(NativeBody::Close(owner)), reference },
                copy,
                sql,
                namespace,
                name,
            ));
        }
    };
    let retained = OriginalInspectionCustody {
        factory,
        body: Some(NativeBody::Terminal(terminal)),
        reference,
    };
    match retained.into_release(copy.clone(), sql.clone(), namespace.clone(), name.clone()) {
        Ok((
            Some(RuntimeInspectionRelease::Closed(closed)),
            RuntimeInspectionReferenceRelease::Closed(reference),
        )) => {
            if let Err(error) = closed.require_original() {
                return Err(closed.retain_failure_with_reference(error, reference));
            }
            Ok((closed, reference))
        }
        Ok((release, reference)) => {
            // A successful close yields a closed proof and a closed reference;
            // keep any other actual owners with the refusal, never discard them.
            Err(anyhow::Error::new(RuntimeInspectionUnexpectedRelease {
                _release: release,
                _schema_reference: reference,
            }))
        }
        Err(owner) => Err(failure(
            anyhow!("original Runtime inspection closed proof refused"),
            owner,
            copy,
            sql,
            namespace,
            name,
        )),
    }
}

/// A close that yielded something other than a closed proof and reference.
struct RuntimeInspectionUnexpectedRelease {
    _release: Option<RuntimeInspectionRelease>,
    _schema_reference: RuntimeInspectionReferenceRelease,
}
impl std::fmt::Debug for RuntimeInspectionUnexpectedRelease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeInspectionUnexpectedRelease").finish_non_exhaustive()
    }
}
impl std::fmt::Display for RuntimeInspectionUnexpectedRelease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("original Runtime inspection close yielded no closed proof and reference")
    }
}
impl std::error::Error for RuntimeInspectionUnexpectedRelease {}

impl RuntimeDb {
    pub(crate) fn open_original_history_reset_inspection(
        path: &Path,
        directory: lillux::PinnedDirectory,
        directory_lock: lillux::PinnedDirectoryLock,
        runtime: Arc<lillux::PinnedDirectory>,
        work: Arc<dyn lillux_sqlite::SqliteInspectionCopyWork>,
        factory: Arc<OriginalNativeFactory>,
        fence: UnreadableLifetimeFence,
    ) -> Result<Self> {
        work.require_original_native_factory(&factory)?;
        factory.require_runtime_binding(&runtime)?;
        ensure!(
            path.parent() == Some(directory.path()),
            "inspection namespace mismatch"
        );
        directory.ensure_path_binding()?;
        directory_lock.ensure_protects(&directory)?;
        let name = path
            .file_name()
            .context("inspection lacks database basename")?
            .to_os_string();
        let (root, copy) =
            create_sqlite_inspection_copy(&directory, &name, &directory_lock, work.clone())?;
        let opened = (|| -> Result<Self> {
            work.require_original_native_factory(&factory)?;
            let namespace = root.lock_exclusive_with_timeout(work.remaining_service_window()?)?;
            let retained_directory = root.try_clone()?;
            let inspection_path = root.path().join(&name);
            let sql = work.clone().issue_sql_work(&root)?;
            let root = Arc::new(root);
            let authority = Arc::new(InspectionDestination {
                factory: Arc::downgrade(&factory),
                runtime,
                root: root.clone(),
                source: Arc::new(directory),
                namespace: namespace.clone(),
                copy: copy.clone(),
                sql: sql.clone(),
                work,
            });
            // The exact authority rechecks the same authentic factory immediately
            // before issuing readonly inputs and again inside native admission.
            authority.require_original(&factory)?;
            // This inspection phase pays its own schema-reference ticket. Its
            // destination holds only independent runtime/source descriptors and
            // the source namespace, never the copy, SQL or copy-work owners.
            let reference_inputs = issue_inspection_reference_inputs(
                &factory, &authority.runtime, &authority.source, &directory_lock)?;
            let schema_reference = match sqlite_schema::prepare_native_schema_sql_reference(
                factory.clone(),
                reference_inputs,
                OriginalNativeDomain::NodeRuntimeSchemaReference,
                &runtime_schema_sql(),
            ) {
                Ok(reference) => reference,
                Err(refused) => {
                    return Err(failure(
                        anyhow!("original Runtime inspection schema reference refused"),
                        OriginalInspectionCustody {
                            factory: factory.clone(),
                            body: None,
                            reference: InspectionSchemaReference::Refused(refused),
                        },
                        copy.clone(),
                        sql,
                        namespace,
                        name.clone(),
                    ));
                }
            };
            let opaque: lillux_sqlite::SqliteCopyLease = copy.clone();
            let inputs = match unsafe {
                OriginalNativeBoundInputs::issued(
                    root,
                    &name,
                    OriginalNativeBoundMode::ReadonlyInspection,
                    OriginalNativeDomain::RuntimeInspection,
                    authority,
                    Some((opaque, sql.clone())),
                )
            } {
                Ok(inputs) => inputs,
                Err(error) => {
                    return Err(failure(
                        error,
                        OriginalInspectionCustody {
                            factory: factory.clone(),
                            body: None,
                            reference: InspectionSchemaReference::Closed(schema_reference),
                        },
                        copy.clone(),
                        sql,
                        namespace,
                        name.clone(),
                    ));
                }
            };
            let owner = match factory.open_bound(inputs) {
                Ok(owner) => owner,
                Err(owner) => {
                    return Err(failure(
                        anyhow!("original Runtime inspection acquisition: {}", owner.error()),
                        OriginalInspectionCustody {
                            factory: factory.clone(),
                            body: Some(NativeBody::Open(owner)),
                            reference: InspectionSchemaReference::Closed(schema_reference),
                        },
                        copy.clone(),
                        sql,
                        namespace,
                        name.clone(),
                    ));
                }
            };
            let validation = owner.with_sql(|connection| {
                let epoch = runtime_operator_schema_epoch(connection, &inspection_path)?;
                ensure!(
                    epoch <= RUNTIME_OPERATOR_SCHEMA_EPOCH,
                    "inspection belongs to a newer operator epoch"
                );
                ensure_scope_lifetime_resettable_with(connection, epoch, fence)?;
                let reset_required = epoch < RUNTIME_OPERATOR_SCHEMA_EPOCH;
                if !reset_required {
                    validate_current_runtime_store_with(
                        connection,
                        &inspection_path,
                        schema_reference.definition(),
                        fence,
                    )?;
                }
                let integrity: String =
                    connection.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
                ensure!(
                    integrity == "ok",
                    "Runtime inspection integrity failed: {integrity}"
                );
                if !reset_required {
                    let mode: String =
                        connection.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
                    ensure!(mode == "wal", "Runtime inspection journal mode is not WAL");
                }
                Ok(reset_required)
            });
            let reset_required = match validation {
                Ok(value) => value,
                Err(error) => {
                    return Err(failure(
                        error,
                        OriginalInspectionCustody {
                            factory: factory.clone(),
                            body: Some(NativeBody::Live(owner)),
                            reference: InspectionSchemaReference::Closed(schema_reference),
                        },
                        copy.clone(),
                        sql,
                        namespace,
                        name.clone(),
                    ));
                }
            };
            Ok(Self {
                conn: RuntimeConnection::OriginalInspection { owner, factory, schema_reference },
                incarnation: RuntimeDbIncarnation(Arc::new(PrivateRuntimeDbIncarnation)),
                reset_target: Some(inspection_path),
                original_deadline_active: std::cell::Cell::new(false),
                reset_required,
                open_mode: RuntimeDbOpenMode::ExplicitHistoryResetInspection,
                _directory: Some(retained_directory),
                _directory_lock: Some(namespace),
                _inspection_copy: Some(copy.clone()),
                _inspection_sql: Some(sql),
            })
        })();
        opened.map_err(|error| SqliteInspectionCopyCustodyError::retain(error, copy))
    }
}

#[cfg(test)]
mod outside_quote_tests {
    use super::*;
    use lillux_sqlite::original_physical_custody::OutsideWork;
    use std::mem::size_of;

    #[test]
    fn inspection_outside_quote_covers_its_continuation() {
        let continuation = quote_inspection_outside_continuation().unwrap();
        let continuation_bytes = size_of::<InspectionOutsideContinuation>()
            + size_of::<lillux_sqlite::OriginalDestinationOutsideCustody>()
            + 4 * size_of::<lillux_sqlite::OriginalDestinationPhysicalSource>();
        assert_eq!(continuation, OutsideWork { bytes: continuation_bytes as u64, steps: 24 });
    }
}

#[cfg(all(test, target_os = "linux"))]
mod reference_destination_tests {
    use super::*;
    use lillux_sqlite::test_host::TestNativeHost;

    /// Copy and SQL work that refuse everything: only the owner identity of
    /// the lease matters here, no copy or SQL runs.
    struct InertWork;
    impl lillux_sqlite::SqliteInspectionCopyWork for InertWork {
        fn require_original_native_factory(&self, _: &Arc<OriginalNativeFactory>) -> Result<()> { anyhow::bail!("inert") }
        fn begin_copy_production(&self, _: &lillux::PinnedDirectory, _: &lillux::PinnedDirectory, _: &std::ffi::OsStr)
            -> Result<lillux_sqlite::SqliteInspectionProductionPermit> { anyhow::bail!("inert") }
        fn issue_sql_work(self: Arc<Self>, _: &lillux::PinnedDirectory)
            -> Result<Arc<dyn lillux_sqlite::SqliteInspectionSqlWork>> { anyhow::bail!("inert") }
        fn create_destination(self: Arc<Self>, _: &lillux::PinnedDirectory, _: &std::ffi::OsStr)
            -> Result<lillux_sqlite::SqliteInspectionCopyDestination> { anyhow::bail!("inert") }
        fn admit_role(&self, _: &lillux::PinnedDirectory, _: &lillux::PinnedDirectory, _: &std::ffi::OsStr,
            _: Option<&lillux_sqlite::SqliteRoleIdentity>, _: u64, _: u64) -> Result<()> { anyhow::bail!("inert") }
        fn retain_destination_role(&self, _: &lillux::PinnedDirectory, _: &std::ffi::OsStr,
            _: Arc<lillux_sqlite::RetainedSqliteDestinationRole>) -> Result<()> { anyhow::bail!("inert") }
        fn cleanup_checkpoint(&self) -> Result<std::time::Duration> { anyhow::bail!("inert") }
        fn remaining_cleanup_steps(&self) -> Result<u64> { anyhow::bail!("inert") }
        fn debit_destination_retirement(&self, _: u32) -> Result<()> { anyhow::bail!("inert") }
        fn debit_cleanup_steps(&self, _: u64) -> Result<()> { anyhow::bail!("inert") }
        fn retained_destination_role(&self, _: &lillux::PinnedDirectory, _: &std::ffi::OsStr)
            -> Result<Option<Arc<lillux_sqlite::RetainedSqliteDestinationRole>>> { anyhow::bail!("inert") }
        fn checkpoint(&self) -> Result<()> { anyhow::bail!("inert") }
        fn remaining_service_window(&self) -> Result<std::time::Duration> { anyhow::bail!("inert") }
    }
    impl lillux_sqlite::SqliteInspectionSqlWork for InertWork {
        fn begin_native_production(&self, _: &lillux::PinnedDirectory, _: &std::ffi::OsStr)
            -> Result<lillux_sqlite::SqliteInspectionProductionPermit> { anyhow::bail!("inert") }
        fn require_copy_work(&self, _: &Arc<dyn lillux_sqlite::SqliteInspectionCopyWork>) -> Result<()> { anyhow::bail!("inert") }
        fn require_destination(&self, _: &lillux::PinnedDirectory) -> Result<()> { anyhow::bail!("inert") }
        fn checkpoint(&self) -> Result<()> { anyhow::bail!("inert") }
        fn debit_vm_steps(&self, _: u64) -> Result<()> { anyhow::bail!("inert") }
    }

    #[test]
    fn inspection_reference_aliases_no_copy_owner_and_releases_alone() {
        let dir = tempfile::tempdir().unwrap();
        let source = lillux::PinnedDirectory::open(dir.path()).unwrap().unwrap();
        let namespace = source.lock_exclusive().unwrap();
        std::fs::create_dir(dir.path().join("scratch")).unwrap();
        let scratch = Arc::new(lillux::PinnedDirectory::open(&dir.path().join("scratch")).unwrap().unwrap());
        let copy = Arc::new(SqliteInspectionCopyLease {
            cleanup: std::sync::Mutex::new(None),
            _source_pins: Vec::new(),
            source_receipts: std::sync::OnceLock::new(),
            _guard: crate::temp_dir_guard::TempDirGuard::new_pinned(
                source.try_clone().unwrap(), OsString::from("scratch"), scratch),
            _source_lock: namespace.clone(),
            _copy_work: Arc::new(InertWork),
        });
        let sql: Arc<dyn lillux_sqlite::SqliteInspectionSqlWork> = Arc::new(InertWork);
        let host = TestNativeHost::acquire();

        let inputs = issue_inspection_reference_inputs(host.factory(), &source, &source, &namespace)
            .expect("inspection reference inputs");
        let reference = match sqlite_schema::prepare_native_schema_sql_reference(
            Arc::clone(host.factory()),
            inputs,
            OriginalNativeDomain::NodeRuntimeSchemaReference,
            &runtime_schema_sql(),
        ) {
            Ok(reference) => reference,
            Err(failure) => panic!("inspection reference refused under the gate: {failure}"),
        };
        // The live reference terminal holds no alias of the copy lease.
        assert_eq!(Arc::strong_count(&copy), 1, "copy lease stays unique while the reference lives");

        // A phase whose main owner never opened releases its reference alone,
        // and the paid ticket reaches the census.
        let custody = OriginalInspectionCustody {
            factory: Arc::clone(host.factory()),
            body: None,
            reference: InspectionSchemaReference::Closed(reference),
        };
        let (release, reference) = match custody.into_release(
            copy, sql, namespace.clone(), OsString::from("runtime.sqlite3"))
        {
            Ok(released) => released,
            Err(_) => panic!("reference-only phase must release its reference"),
        };
        assert!(release.is_none(), "no main owner, no inspection release");
        let terminal = reference.census_terminal().unwrap().expect("paid reference ticket");
        host.retain_terminal(terminal);
        drop(reference);
        assert_eq!(host.ticket_census(), (1, 1));
    }
}
