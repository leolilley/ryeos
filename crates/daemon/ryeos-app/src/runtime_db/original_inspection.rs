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

pub fn quote_inspection_copy_lease_parts(source_pin_count: usize)
    -> anyhow::Result<lillux_sqlite::original_physical_custody::OutsideWork> {
    anyhow::ensure!(source_pin_count <= 4,
        "inspection source metadata pins exceed their four fixed roles");
    let bytes = std::mem::size_of::<OriginalInspectionCopyLeaseParts>()
        .checked_add(source_pin_count.checked_mul(
            std::mem::size_of::<lillux_sqlite::RetainedOriginalMetadataPin>()
                .checked_add(4 * std::mem::size_of::<Arc<lillux_sqlite::RetainedOriginalMetadataPin>>())
                .context("source metadata pin quote overflow")?,
        ).context("source metadata pin quote overflow")?)
        .context("copy lease transfer quote overflow")?;
    Ok(lillux_sqlite::original_physical_custody::OutsideWork {
        bytes: u64::try_from(bytes)?, steps: 24,
    })
}

pub(super) fn same_terminal_owner(a: &OriginalNativeConnectionTerminal, b: &OriginalNativeConnectionTerminal) -> bool {
    match (a, b) {
        (OriginalNativeConnectionTerminal::Closed(a), OriginalNativeConnectionTerminal::Closed(b)) => Arc::ptr_eq(a, b),
        (OriginalNativeConnectionTerminal::Released(a), OriginalNativeConnectionTerminal::Released(b)) => Arc::ptr_eq(a, b),
        _ => false,
    }
}
pub(super) fn terminal_snapshot(
    process: u32,
    native: &std::sync::Mutex<Option<(Arc<OriginalNativeFactory>, OriginalNativeConnectionTerminal)>>,
) -> Result<Option<OriginalNativeConnectionTerminal>> {
    ensure!(process == std::process::id(), "inspection terminal belongs to original process");
    let native = native.try_lock().map_err(|_| anyhow!("original inspection native custody busy or poisoned"))?;
    Ok(native.as_ref().map(|(_, terminal)| terminal.clone()))
}
/// Remove only a redundant native receipt/factory alias after the original
/// outside supervisor retained this exact canonical receipt. Physical copy,
/// namespace, proof and work owners remain in their original wrapper.
pub(super) fn retire_terminal_alias(
    process: u32,
    native: &std::sync::Mutex<Option<(Arc<OriginalNativeFactory>, OriginalNativeConnectionTerminal)>>,
    factory: &Arc<OriginalNativeFactory>,
    aggregate: &lillux_sqlite::OriginalNativeTerminal,
    canonical: &OriginalNativeConnectionTerminal,
) -> Result<()> {
    ensure!(process == std::process::id(), "inspection native alias belongs to original process");
    factory.require_original_process()?;
    aggregate.require_original(factory)?;
    canonical.require_original(factory)?;
    let mut native = native.try_lock().map_err(|_| anyhow!("original inspection alias retirement busy or poisoned"))?;
    if let Some((original_factory, original)) = &*native {
        ensure!(Arc::ptr_eq(original_factory, factory) && same_terminal_owner(original, canonical),
            "inspection alias retirement selects a different actual receipt/factory");
    } else { return Ok(()); }
    let redundant = native.take();
    drop(native);
    // The canonical caller receipt and factory remain retained throughout;
    // this cannot be the last body/resource reference or native disposal.
    drop(redundant);
    Ok(())
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
            process: std::process::id(), leaf: leaf.clone(), joined_leaf: None,
            copy: std::mem::ManuallyDrop::new(Some(copy)),
            sql: std::mem::ManuallyDrop::new(Some(sql)),
            work: std::mem::ManuallyDrop::new(Some(work)),
        });
        let sources = vec![
            lillux_sqlite::OriginalDestinationPhysicalSource::Directory(runtime),
            lillux_sqlite::OriginalDestinationPhysicalSource::Directory(root),
            lillux_sqlite::OriginalDestinationPhysicalSource::Directory(source),
            lillux_sqlite::OriginalDestinationPhysicalSource::Namespace(namespace),
        ];
        // SAFETY: genuine exact leaf/account checks and original prepayment
        // precede allocation. Every copy/SQL/work holder remains in MD custody.
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
        self.sql.checkpoint()?;
        self.work.checkpoint()
    }
}

/// Actual inspection group receiver. Opaque copy/SQL/work interfaces still
/// retain source pins, guard, cleanup and account owners; no Drop disposes them.
struct InspectionOutsideContinuation {
    process: u32,
    leaf: lillux_sqlite::OriginalNativeOutsideLeafEvidence,
    joined_leaf: Option<lillux_sqlite::OriginalNativeOutsideLeafEvidence>,
    copy: std::mem::ManuallyDrop<Option<Arc<SqliteInspectionCopyLease>>>,
    sql: std::mem::ManuallyDrop<Option<Arc<dyn lillux_sqlite::SqliteInspectionSqlWork>>>,
    work: std::mem::ManuallyDrop<Option<Arc<dyn lillux_sqlite::SqliteInspectionCopyWork>>>,
}
// SAFETY: all actual resources remain owned, including on unwind/refusal.
// Incomplete group consumption explicitly blocks physical retirement.
unsafe impl lillux_sqlite::OriginalOutsideDestinationContinuation for InspectionOutsideContinuation {
    fn join_original_inspection_aliases(&mut self,
        factory: &Arc<OriginalNativeFactory>, aggregate: &lillux_sqlite::OriginalNativeTerminal,
        receiver: &dyn lillux_sqlite::OriginalOutsideInspectionAliasReceiver) -> Result<bool> {
        self.require_original_process()?;
        self.leaf.require_original(factory, aggregate)?;
        if let Some(original) = &self.joined_leaf {
            ensure!(original.same_original_leaf(&self.leaf)? && self.copy.is_none()
                && self.sql.is_none() && self.work.is_none(), "original inspection join memo has unresolved aliases");
            return Ok(true);
        }
        let copy: lillux_sqlite::SqliteCopyLease = self.copy.as_ref()
            .context("actual original inspection copy absent during join")?.clone();
        receiver.require_original_context(factory, aggregate, &self.leaf, &copy,
            self.sql.as_ref().context("actual original inspection SQL owner absent during join")?,
            self.work.as_ref().context("actual original inspection work absent during join")?)?;
        // Receiver holds each SAME real owner before these redundant aliases
        // end. Neither this operation nor an empty field proves FD retirement.
        self.joined_leaf = Some(self.leaf.clone());
        self.copy.take(); self.sql.take(); self.work.take();
        Ok(true)
    }
    fn require_original_process(&self) -> Result<()> {
        ensure!(self.process == std::process::id(), "foreign inspection outside continuation");
        Ok(())
    }
    fn retire_native_references(&mut self, factory: &Arc<OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        _prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()> {
        self.require_original_process()?;
        self.leaf.require_original(factory, aggregate)?;
        if self.joined_leaf.as_ref().is_some_and(|original| original.same_original_leaf(&self.leaf).unwrap_or(false))
            && self.copy.is_none() && self.sql.is_none() && self.work.is_none() { return Ok(()); }
        self.work.as_ref().context("original inspection work absent without completed join")?
            .require_original_outside_master(factory, aggregate)?;
        // Copy work still owns the original account/native controls. This is
        // not a completed retirement until the canonical copy group consumes it.
        anyhow::bail!("inspection copy group native/account holders await consuming join")
    }
    unsafe fn register_shared_roots(&mut self,
        _registry: &mut lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        _prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()> {
        self.require_original_process()?;
        ensure!(self.joined_leaf.is_some() && self.copy.is_none() && self.sql.is_none() && self.work.is_none(),
            "inspection source pins, scratch guard and cleanup proof await consuming copy group");
        Ok(())
    }
    fn physical_holders_retired(&self) -> Result<bool> {
        self.require_original_process()?;
        Ok(self.joined_leaf.is_some() && self.copy.is_none() && self.sql.is_none() && self.work.is_none())
    }
    fn physical_holders_retirement_quote(&self) -> Result<lillux_sqlite::original_physical_custody::OutsideWork> {
        self.require_original_process()?;
        Ok(lillux_sqlite::original_physical_custody::OutsideWork { bytes: 0, steps: 8 })
    }
}

pub(super) struct OriginalInspectionCopyLeaseParts {
    pub(super) process: u32,
    pub(super) source_pins: std::mem::ManuallyDrop<Vec<Arc<lillux_sqlite::RetainedOriginalMetadataPin>>>,
    pub(super) guard: std::mem::ManuallyDrop<Option<crate::temp_dir_guard::TempDirGuard>>,
    pub(super) source_lock: std::mem::ManuallyDrop<Option<lillux::PinnedDirectoryLock>>,
    pub(super) work: std::mem::ManuallyDrop<Arc<dyn lillux_sqlite::SqliteInspectionCopyWork>>,
    pub(super) receipts: std::mem::ManuallyDrop<Option<Vec<(OsString, Option<lillux_sqlite::SqliteInspectionCopyReceipt>)>>>,
    cleanup: std::mem::ManuallyDrop<Option<Weak<super::inspection_cleanup::InspectionCopyCleanup>>>,
}
pub(super) fn consume_copy_lease(original: Arc<SqliteInspectionCopyLease>,
    factory: &Arc<OriginalNativeFactory>, aggregate: &lillux_sqlite::OriginalNativeTerminal,
    leaf: &lillux_sqlite::OriginalNativeOutsideLeafEvidence,
    prepay: &dyn Fn(u64, u64) -> Result<()>)
    -> std::result::Result<OriginalInspectionCopyLeaseParts, (Arc<SqliteInspectionCopyLease>, anyhow::Error)> {
    let process = std::process::id();
    let checked = lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| {
        factory.require_original_process()?;
        leaf.require_original(factory, aggregate)?;
        original._copy_work.require_original_outside_master(factory, aggregate)?;
        { let _original = original.cleanup.try_lock().map_err(|_| anyhow!("original copy cleanup custody busy or poisoned"))?; }
        ensure!(original._source_pins.len() <= 4, "original source pin inventory exceeds fixed roles");
        let quote = quote_inspection_copy_lease_parts(original._source_pins.len())?;
        prepay(quote.bytes, quote.steps)?;
        factory.require_original_process()
    });
    if let Err(error) = checked { return Err((original, error)); }
    let original = match Arc::try_unwrap(original) {
        Ok(original) => original,
        Err(original) => return Err((original, anyhow!("original copy lease still has aliases"))),
    };
    let SqliteInspectionCopyLease { cleanup, _source_pins, source_receipts, _guard, _source_lock, _copy_work } = original;
    let mut files = std::mem::ManuallyDrop::new(_source_pins);
    let mut pins = std::mem::ManuallyDrop::new(Vec::with_capacity(files.len()));
    while let Some(file) = files.pop() {
        // SAFETY: same original O_PATH source pin, moved once after its native
        // context completed; concrete owner/control storage prepaid above.
        pins.push(unsafe { lillux_sqlite::RetainedOriginalMetadataPin::issued(file) });
    }
    drop(std::mem::ManuallyDrop::into_inner(files));
    Ok(OriginalInspectionCopyLeaseParts {
        process, source_pins: pins,
        guard: std::mem::ManuallyDrop::new(Some(_guard)),
        source_lock: std::mem::ManuallyDrop::new(Some(_source_lock)),
        work: std::mem::ManuallyDrop::new(_copy_work),
        receipts: std::mem::ManuallyDrop::new(source_receipts.into_inner()),
        cleanup: std::mem::ManuallyDrop::new(cleanup.into_inner().unwrap_or_else(|p| p.into_inner())),
    })
}

pub(super) enum OriginalInspectionPrimitiveProof {
    Closed(std::mem::ManuallyDrop<Arc<lillux_sqlite::ClosedSqliteInspection>>),
    Released(std::mem::ManuallyDrop<Arc<lillux_sqlite::ReleasedSqliteInspection>>),
}
pub(super) struct OriginalInspectionWrapperParts {
    pub(super) process: u32,
    pub(super) proof: std::mem::ManuallyDrop<Option<OriginalInspectionPrimitiveProof>>,
    pub(super) lease: std::mem::ManuallyDrop<Arc<SqliteInspectionCopyLease>>,
    pub(super) sql: std::mem::ManuallyDrop<Arc<dyn lillux_sqlite::SqliteInspectionSqlWork>>,
    pub(super) database_name: OsString,
    pub(super) namespace: std::mem::ManuallyDrop<Option<lillux::PinnedDirectoryLock>>,
}
pub(super) fn require_wrapper_transfer(process: u32,
    native: &std::sync::Mutex<Option<(Arc<OriginalNativeFactory>, OriginalNativeConnectionTerminal)>>) -> Result<()> {
    ensure!(process == std::process::id(), "foreign original inspection wrapper");
    ensure!(native.try_lock().map_err(|_| anyhow!("original wrapper native custody busy or poisoned"))?.is_none(),
        "inspection wrapper native receipt has not transferred to its real canonical receiver");
    Ok(())
}
pub(super) fn consume_closed_wrapper(original: Arc<SqliteInspectionClosed>)
    -> std::result::Result<OriginalInspectionWrapperParts, (Arc<SqliteInspectionClosed>, anyhow::Error)> {
    if let Err(error) = require_wrapper_transfer(original.process, &original.native) {
        return Err((original, error));
    }
    let original = match Arc::try_unwrap(original) {
        Ok(original) => original,
        Err(original) => return Err((original, anyhow!("original closed inspection wrapper still has aliases"))),
    };
    let SqliteInspectionClosed { process, proof, native: _, lease, sql, database_name, _namespace } = original;
    Ok(OriginalInspectionWrapperParts { process, proof: std::mem::ManuallyDrop::new(Some(OriginalInspectionPrimitiveProof::Closed(std::mem::ManuallyDrop::new(proof)))),
        lease: std::mem::ManuallyDrop::new(lease), sql: std::mem::ManuallyDrop::new(sql), database_name,
        namespace: std::mem::ManuallyDrop::new(_namespace) })
}
pub(super) fn consume_acquisition_wrapper(original: Arc<SqliteInspectionAcquisitionReleased>)
    -> std::result::Result<OriginalInspectionWrapperParts, (Arc<SqliteInspectionAcquisitionReleased>, anyhow::Error)> {
    if let Err(error) = require_wrapper_transfer(original.process, &original.native) {
        return Err((original, error));
    }
    let original = match Arc::try_unwrap(original) {
        Ok(original) => original,
        Err(original) => return Err((original, anyhow!("original released inspection wrapper still has aliases"))),
    };
    Ok(acquisition_wrapper_parts(original))
}
pub(super) fn acquisition_wrapper_parts(original: SqliteInspectionAcquisitionReleased) -> OriginalInspectionWrapperParts {
    let SqliteInspectionAcquisitionReleased { process, proof, native: _, lease, sql, database_name, _namespace } = original;
    OriginalInspectionWrapperParts { process, proof: std::mem::ManuallyDrop::new(Some(OriginalInspectionPrimitiveProof::Released(std::mem::ManuallyDrop::new(proof)))),
        lease: std::mem::ManuallyDrop::new(lease), sql: std::mem::ManuallyDrop::new(sql), database_name,
        namespace: std::mem::ManuallyDrop::new(_namespace) }
}

enum NativeBody {
    Open(OriginalNativeOpenFailure),
    Live(OriginalNativeConnection),
    Close(OriginalNativeCloseFailure),
    Terminal(OriginalNativeConnectionTerminal),
}
pub(super) struct OriginalInspectionCustody {
    factory: Arc<OriginalNativeFactory>,
    body: NativeBody,
}
impl OriginalInspectionCustody {
    pub(super) fn retry_close(self) -> std::result::Result<Self, Self> {
        let Self { factory, body } = self;
        let attempted = match body {
            NativeBody::Open(mut owner) => {
                if owner.retained_admission().is_some() {
                    if owner.recover_retained_admission().is_err() {
                        return Err(Self { factory, body: NativeBody::Open(owner) });
                    }
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
                body: NativeBody::Terminal(terminal),
            }),
            Err(body) => Err(Self { factory, body }),
        }
    }
    /// Share only the actual primitive proof owned by the actual native ticket.
    /// Before-admission release without a copy proof remains retained/refused.
    pub(super) fn into_release(
        self,
        lease: Arc<SqliteInspectionCopyLease>,
        sql: Arc<dyn lillux_sqlite::SqliteInspectionSqlWork>,
        namespace: lillux::PinnedDirectoryLock,
        database_name: OsString,
    ) -> std::result::Result<RuntimeInspectionRelease, Self> {
        let NativeBody::Terminal(terminal) = &self.body else {
            return Err(self);
        };
        if terminal.require_original(&self.factory).is_err() {
            return Err(self);
        }
        let (closed, released) = match terminal {
            OriginalNativeConnectionTerminal::Closed(owner) => (owner.inspection_closed(), None),
            OriginalNativeConnectionTerminal::Released(owner) => {
                (owner.inspection_closed(), owner.inspection_released())
            }
        };
        if let Some(proof) = closed {
            return Ok(RuntimeInspectionRelease::Closed(Arc::new(
                SqliteInspectionClosed {
                    process: std::process::id(),
                    proof,
                    native: std::sync::Mutex::new(Some((self.factory, terminal.clone()))),
                    lease,
                    sql,
                    database_name,
                    _namespace: Some(namespace),
                },
            )));
        }
        if let Some(proof) = released {
            return Ok(RuntimeInspectionRelease::Acquisition(
                SqliteInspectionAcquisitionReleased {
                    process: std::process::id(),
                    proof,
                    native: std::sync::Mutex::new(Some((self.factory, terminal.clone()))),
                    lease,
                    sql,
                    database_name,
                    _namespace: Some(namespace),
                },
            ));
        }
        Err(self)
    }
}

fn failure(
    error: anyhow::Error,
    body: NativeBody,
    factory: Arc<OriginalNativeFactory>,
    copy: Arc<SqliteInspectionCopyLease>,
    sql: Arc<dyn lillux_sqlite::SqliteInspectionSqlWork>,
    namespace: lillux::PinnedDirectoryLock,
    name: OsString,
) -> anyhow::Error {
    anyhow::Error::new(RuntimeInspectionNativeFailure {
        error,
        custody: std::sync::Mutex::new(Some(RuntimeInspectionNativeCustody::Original(
            OriginalInspectionCustody { factory, body },
        ))),
        _copy: copy,
        _sql: sql,
        _namespace: namespace,
        database_name: name,
    })
}

pub(super) fn close_inspection(
    owner: OriginalNativeConnection,
    factory: Arc<OriginalNativeFactory>,
    copy: Arc<SqliteInspectionCopyLease>,
    sql: Arc<dyn lillux_sqlite::SqliteInspectionSqlWork>,
    namespace: lillux::PinnedDirectoryLock,
    name: OsString,
) -> Result<Arc<SqliteInspectionClosed>> {
    let terminal = match owner.close() {
        Ok(proof) => OriginalNativeConnectionTerminal::Closed(proof),
        Err(owner) => {
            return Err(failure(
                anyhow!("original Runtime inspection close pending"),
                NativeBody::Close(owner),
                factory,
                copy,
                sql,
                namespace,
                name,
            ));
        }
    };
    let retained = OriginalInspectionCustody {
        factory,
        body: NativeBody::Terminal(terminal),
    };
    match retained.into_release(copy.clone(), sql.clone(), namespace.clone(), name.clone()) {
        Ok(RuntimeInspectionRelease::Closed(closed)) => {
            closed
                .require_original()
                .map_err(|error| closed.retain_failure(error))?;
            Ok(closed)
        }
        Ok(RuntimeInspectionRelease::Acquisition(_)) => {
            unreachable!("successful close yields closed proof")
        }
        Err(owner) => Err(failure(
            anyhow!("original Runtime inspection closed proof refused"),
            owner.body,
            owner.factory,
            copy,
            sql,
            namespace,
            name,
        )),
    }
}

impl RuntimeDb {
    pub(crate) fn open_original_history_reset_inspection(
        path: &Path,
        directory: lillux::PinnedDirectory,
        directory_lock: lillux::PinnedDirectoryLock,
        runtime: Arc<lillux::PinnedDirectory>,
        work: Arc<dyn lillux_sqlite::SqliteInspectionCopyWork>,
        factory: Arc<OriginalNativeFactory>,
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
            let opaque: lillux_sqlite::SqliteCopyLease = copy.clone();
            let inputs = unsafe {
                OriginalNativeBoundInputs::issued(
                    root,
                    &name,
                    OriginalNativeBoundMode::ReadonlyInspection,
                    OriginalNativeDomain::RuntimeInspection,
                    authority,
                    Some((opaque, sql.clone())),
                )?
            };
            let owner = match factory.open_bound(inputs) {
                Ok(owner) => owner,
                Err(owner) => {
                    return Err(failure(
                        anyhow!("original Runtime inspection acquisition: {}", owner.error()),
                        NativeBody::Open(owner),
                        factory.clone(),
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
                ensure_scope_lifetime_resettable(connection, epoch)?;
                let reset_required = epoch < RUNTIME_OPERATOR_SCHEMA_EPOCH;
                if !reset_required {
                    validate_current_runtime_store(connection, &inspection_path, None)?;
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
                        NativeBody::Live(owner),
                        factory.clone(),
                        copy.clone(),
                        sql,
                        namespace,
                        name.clone(),
                    ));
                }
            };
            Ok(Self {
                conn: RuntimeConnection::OriginalInspection { owner, factory },
                incarnation: RuntimeDbIncarnation(Arc::new(PrivateRuntimeDbIncarnation)),
                reset_target: Some(inspection_path),
                original_deadline_active: std::cell::Cell::new(false),
                reset_required,
                open_mode: RuntimeDbOpenMode::ExplicitHistoryResetInspection,
                _directory: Some(retained_directory),
                _directory_lock: Some(namespace),
                _database_file: None,
                _wal_file: None,
                _shm_file: None,
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
    fn inspection_outside_quotes_cover_continuation_and_bounded_pin_parts() {
        let continuation = quote_inspection_outside_continuation().unwrap();
        let continuation_bytes = size_of::<InspectionOutsideContinuation>()
            + size_of::<lillux_sqlite::OriginalDestinationOutsideCustody>()
            + 4 * size_of::<lillux_sqlite::OriginalDestinationPhysicalSource>();
        assert_eq!(continuation, OutsideWork { bytes: continuation_bytes as u64, steps: 24 });

        let pins = quote_inspection_copy_lease_parts(3).unwrap();
        let pins_bytes = size_of::<OriginalInspectionCopyLeaseParts>()
            + 3 * (size_of::<lillux_sqlite::RetainedOriginalMetadataPin>()
                + 4 * size_of::<Arc<lillux_sqlite::RetainedOriginalMetadataPin>>());
        assert_eq!(pins, OutsideWork { bytes: pins_bytes as u64, steps: 24 });
        assert!(quote_inspection_copy_lease_parts(5).is_err());
    }
}
