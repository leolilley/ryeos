//! Lillux-owned SQLite host backend. Native acquisition, adoption, locking,
//! mapping, mutation and close remain below schema/service/recovery policy.
//! Provisional source: no qualification until independent review and controls.
/// Owner token carried by the secure temporaries this crate publishes.
pub const SQLITE_TEMP_PURPOSE: lillux::TempPurpose = lillux::TempPurpose::new("sqlite");

mod binding;
mod native;
pub mod offline_host;
pub mod original_host_owner;
pub mod original_physical_custody;
#[cfg(any(test, feature = "test-support"))]
pub mod test_host;
use anyhow::{Context, Result, ensure};
use binding::Binding;
use lillux::PinnedDirectory;
pub use native::*;
use rusqlite::{Connection, OpenFlags};
use std::{ffi::OsStr, sync::Arc};

/// Whether the running process is a copy of the process recorded as an
/// owner's issuer. The child of a protected fork is recognized by lillux's
/// fork barrier, which holds even when a nested PID namespace hands the child
/// the same pid its parent had; the pid comparison additionally refuses a copy
/// made by a fork lillux did not supervise. Reads one atomic and makes one
/// `getpid` call: no allocation and no lock, so a destructor may consult it
/// before touching anything copied.
pub(crate) fn foreign_process(process: u32) -> bool {
    lillux::exec::in_forked_child() || process != std::process::id()
}

/// Read bounded evidence from an exact no-follow child without disturbing
/// any descriptor tracked by this process's SQLite backend. The native gate
/// excludes role aliases and spans the data open, read and close; only bytes
/// escape. This is not signature verification or namespace mutation authority.
pub fn read_regular_without_sqlite_alias(
    runtime: &PinnedDirectory,
    name: &OsStr,
    maximum_bytes: usize,
) -> Result<Option<Vec<u8>>> {
    ensure!(
        maximum_bytes > 0 && maximum_bytes <= 262_144,
        "guarded SQLite evidence read exceeds its bound"
    );
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (runtime, name);
        anyhow::bail!("guarded SQLite evidence reads are unavailable on this platform")
    }
    #[cfg(target_os = "linux")]
    {
        let Some(pin) = runtime.open_mount_entry(name)? else {
            return Ok(None);
        };
        ensure!(
            pin.metadata()?.is_file(),
            "guarded SQLite evidence is not a regular file"
        );
        let bytes = binding::read_unbound_regular(&pin, maximum_bytes)?;
        ensure_guarded_read_name(runtime, name, &pin)?;
        Ok(Some(bytes))
    }
}

/// Reobserve the current coordinate without a data-capable open. This is a
/// same-inode namespace snapshot, not exclusion against a later writer or an
/// effect grant. The caller still owns its operation/authority join.
#[cfg(target_os = "linux")]
fn ensure_guarded_read_name(
    runtime: &PinnedDirectory,
    name: &OsStr,
    pin: &std::fs::File,
) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let expected = pin.metadata()?;
    let current = runtime
        .entry_no_follow(name)?
        .ok_or_else(|| anyhow::anyhow!("guarded SQLite evidence coordinate disappeared"))?;
    ensure!(
        current.entry_type == lillux::PinnedEntryType::Regular
            && current.containing_device == expected.dev()
            && current.inode == expected.ino(),
        "guarded SQLite evidence coordinate changed during intake"
    );
    Ok(())
}

/// Stable copied facts from one original inspection source. No descriptor or
/// mutation/signing authority is conveyed by this receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqliteInspectionCopyReceipt {
    pub source_identity: SqliteRoleIdentity,
    pub copied_byte_length: u64,
    pub copied_sha256: String,
}

#[cfg(all(test, target_os = "linux"))]
thread_local! {
    static COPY_AFTER_FIRST_CHUNK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

/// Original created destination and its exact removal parent. This holder
/// does not restore a directory or descriptors from cold evidence.
pub struct SqliteInspectionCopyDestination {
    pub parent: PinnedDirectory,
    pub name: std::ffi::OsString,
    pub root: Arc<PinnedDirectory>,
}

/// Caller-owned admission for one original copy operation. Implementations
/// must retain the original resource owner; neither this trait nor a measured
/// extent issues authority. All calls occur outside native alias gates.
/// Failed/partial work stays charged; this API has no refund callback.
/// Original issuer bookkeeping for one complete producer interval. The actual
/// issuer supplies this private control; no native proof can be reconstructed.
pub trait SqliteInspectionProductionControl: Send + Sync {
    fn checkpoint(&self) -> Result<()>;
    fn complete(&self) -> Result<()>;
    fn abandoned(&self);
}
/// Host-backed control observation for the actual affine permit lifetime.
/// Reference retirement is separate from a native terminal acknowledgement.
pub trait RetainedSqliteInspectionProductionControl: SqliteInspectionProductionControl {
    fn permit_reference_retired(&self);
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqliteInspectionProductionKind {
    FactoryIssuer,
    Namespace,
    Copy,
    Native,
    ProjectionLease,
}
enum ProductionControlOwner {
    Shared(Arc<dyn SqliteInspectionProductionControl>),
    Region {
        control: Arc<dyn SqliteInspectionProductionControl>,
        core: OriginalNativeProducerControl,
    },
}
impl ProductionControlOwner {
    fn control(&self) -> &dyn SqliteInspectionProductionControl {
        match self {
            Self::Shared(control) => control.as_ref(),
            Self::Region { control, .. } => control.as_ref(),
        }
    }
    fn complete(&self) -> Result<()> {
        self.control().complete()?;
        if let Self::Region { core, .. } = self {
            core.completed()?;
        }
        Ok(())
    }
}
pub struct SqliteInspectionProductionPermit {
    kind: SqliteInspectionProductionKind,
    control: ProductionControlOwner,
    completed: std::sync::atomic::AtomicBool,
}
impl SqliteInspectionProductionPermit {
    pub fn issued(
        kind: SqliteInspectionProductionKind,
        control: Arc<dyn SqliteInspectionProductionControl>,
    ) -> Self {
        Self {
            kind,
            control: ProductionControlOwner::Shared(control),
            completed: std::sync::atomic::AtomicBool::new(false),
        }
    }
    /// # Safety
    /// Only the genuine original issuer transfers its actual prepaid interval.
    /// The callback wrapper belongs to the bounded process; core holds a real
    /// counted region handle and the exact resident producer cell. No mapped
    /// static reference or decoded kind creates funding/admission authority.
    pub unsafe fn issued_from_region(
        kind: SqliteInspectionProductionKind,
        control: Arc<dyn SqliteInspectionProductionControl>,
        core: OriginalNativeProducerControl,
    ) -> std::result::Result<Self, SqliteInspectionProductionPublicationFailure> {
        let retained = lillux::custody::Held::until_taken(
            PRODUCTION_PUBLICATION_FAILURE_KIND,
            ProductionPublicationFailureParts {
                permit: Self {
                    kind,
                    control: ProductionControlOwner::Region { control, core },
                    completed: std::sync::atomic::AtomicBool::new(false),
                },
            },
        );
        let result = match &retained.permit.control {
            ProductionControlOwner::Region { core, .. } => {
                core.require_kind(kind).and_then(|()| core.published())
            }
            _ => unreachable!("constructed concrete region control"),
        };
        match result {
            Ok(()) => match retained.take_whole() {
                Ok(parts) => Ok(parts.permit),
                Err(inner) => Err(SqliteInspectionProductionPublicationFailure {
                    refusal: original_host_owner::FixedHostRefusal::Process,
                    inner,
                }),
            },
            Err(refusal) => Err(SqliteInspectionProductionPublicationFailure {
                refusal,
                inner: retained,
            }),
        }
    }
    fn require_original_region(
        &self,
        region: &original_host_owner::OriginalNativeRegionHandle,
    ) -> Result<()> {
        let ProductionControlOwner::Region { core, .. } = &self.control else {
            anyhow::bail!("native producer requires the actual resident region");
        };
        ensure!(
            core.same_region(region),
            "native producer selects foreign region"
        );
        core.require_kind(self.kind)?;
        core.checkpoint()?;
        Ok(())
    }
    fn complete(&self) -> Result<()> {
        // Check process before calling an outside control which may itself
        // hold mapped authority. Completion records an actual receipt only.
        if let ProductionControlOwner::Region { core, .. } = &self.control {
            core.observation()?;
        }
        self.control.complete()?;
        self.completed
            .store(true, std::sync::atomic::Ordering::Release);
        Ok(())
    }
    pub fn checkpoint(&self) -> Result<()> {
        if let ProductionControlOwner::Region { core, .. } = &self.control {
            core.checkpoint()?;
        }
        self.control.control().checkpoint()
    }
    /// The callback owner, for a weak reference held by the commit gate.
    pub(crate) fn control_owner(&self) -> &Arc<dyn SqliteInspectionProductionControl> {
        match &self.control {
            ProductionControlOwner::Shared(control) => control,
            ProductionControlOwner::Region { control, core: _ } => control,
        }
    }
    /// The resident producer cell index, copied; grants no admission.
    pub(crate) fn region_cell(&self) -> Option<OriginalNativeProducerCellIndex> {
        match &self.control {
            ProductionControlOwner::Shared(_) => None,
            ProductionControlOwner::Region { control: _, core } => Some(core.index()),
        }
    }
    /// Only synchronous creation/copy intervals use this handoff. Native
    /// intervals acknowledge exclusively through actual Closed/Released proof.
    pub fn acknowledge_synchronous(&self) -> Result<()> {
        ensure!(
            self.kind != SqliteInspectionProductionKind::Native
                && self.kind != SqliteInspectionProductionKind::FactoryIssuer
                && self.kind != SqliteInspectionProductionKind::ProjectionLease,
            "native producer requires actual terminal proof"
        );
        self.complete()
    }
    /// Pre-installation recovery only. The outside owner has closed admission
    /// and retains mapping/account custody. The concrete gate verifies that
    /// no factory or native effect exists before ending this issuer interval.
    /// A refused callback leaves this same permit accessible for retry.
    pub fn retire_uninstalled_factory_issuer(&self) -> Result<()> {
        ensure!(
            self.kind == SqliteInspectionProductionKind::FactoryIssuer,
            "uninstalled recovery requires the actual issuer interval"
        );
        let ProductionControlOwner::Region { core, .. } = &self.control else {
            anyhow::bail!("issuer recovery requires its actual resident region");
        };
        core.retire_uninstalled_factory_issuer()?;
        self.control.control().complete()?;
        self.completed
            .store(true, std::sync::atomic::Ordering::Release);
        Ok(())
    }
    fn acknowledge_factory_handoff(&self) -> Result<()> {
        ensure!(
            self.kind == SqliteInspectionProductionKind::FactoryIssuer,
            "actual original factory handoff requires its issuer interval"
        );
        self.complete()
    }
    // Called only after this genuine factory retained an actual NotAcquired
    // or Released backend body. It never acknowledges Ready.
    fn acknowledge_factory_refusal(&self) -> Result<()> {
        ensure!(
            self.kind == SqliteInspectionProductionKind::FactoryIssuer,
            "factory refusal requires its genuine issuer interval"
        );
        self.complete()
    }
    // Only the actual opaque native lease terminal may consume this interval.
    fn acknowledge_projection_lease_terminal(&self) -> Result<()> {
        ensure!(
            self.kind == SqliteInspectionProductionKind::ProjectionLease,
            "foreign projection lease producer interval"
        );
        self.complete()
    }
    fn acknowledge_native_terminal(&self) -> Result<()> {
        ensure!(
            self.kind == SqliteInspectionProductionKind::Native,
            "foreign native producer interval"
        );
        self.complete()
    }
}
const PRODUCTION_PUBLICATION_FAILURE_KIND: &str = "lillux-sqlite::production-publication-failure";
/// The permit of a refused publication. Constructible only in this module.
struct ProductionPublicationFailureParts {
    permit: SqliteInspectionProductionPermit,
}
/// A control that does nothing, left in a permit whose real control moved out.
struct SpentProductionControl;
impl SqliteInspectionProductionControl for SpentProductionControl {
    fn checkpoint(&self) -> Result<()> {
        Ok(())
    }
    fn complete(&self) -> Result<()> {
        Ok(())
    }
    fn abandoned(&self) {}
}
/// Only `into_parts` settles a publication refusal. Until then the permit
/// holds its resident producer cell and region handle; releasing them at an
/// unsupervised end would bypass the supervised recovery, so any other end
/// retains the whole permit, counted, until the process exits.
pub struct SqliteInspectionProductionPublicationFailure {
    pub refusal: original_host_owner::FixedHostRefusal,
    inner: lillux::custody::Held<ProductionPublicationFailureParts>,
}
impl SqliteInspectionProductionPublicationFailure {
    /// Retains the exact publication failure and all actual handles on every
    /// refusal. After success the outside caller must consume into_parts and
    /// retire the returned callback/core wrappers before attempting final seal.
    pub fn retire_uninstalled_factory_issuer(&self) -> Result<()> {
        self.inner.permit.retire_uninstalled_factory_issuer()
    }
    pub fn into_parts(
        self,
    ) -> std::result::Result<
        (
            SqliteInspectionProductionKind,
            Arc<dyn SqliteInspectionProductionControl>,
            OriginalNativeProducerControl,
        ),
        Self,
    > {
        let ProductionControlOwner::Region { core, .. } = &self.inner.permit.control else {
            return Err(self);
        };
        if core.observation().is_err() {
            return Err(self);
        }
        let Self { refusal, inner } = self;
        let mut permit = match inner.take_whole() {
            Ok(parts) => parts.permit,
            Err(inner) => return Err(Self { refusal, inner }),
        };
        // The control moves out; the emptied permit is marked completed so its
        // own end neither reports an abandoned interval nor retires the
        // reference the caller now holds.
        // Invariant: `SqliteInspectionProductionPermit::drop` is a no-op for a
        // `Shared` control whose permit is `completed`, so the placeholder
        // left behind here can never call the spent control or touch a region.
        let kind = permit.kind;
        permit
            .completed
            .store(true, std::sync::atomic::Ordering::Release);
        let control = std::mem::replace(
            &mut permit.control,
            ProductionControlOwner::Shared(Arc::new(SpentProductionControl)),
        );
        let ProductionControlOwner::Region { control, core } = control else {
            unreachable!("same retained region control")
        };
        Ok((kind, control, core))
    }
}
impl Drop for SqliteInspectionProductionPermit {
    fn drop(&mut self) {
        let completed = self.completed.load(std::sync::atomic::Ordering::Acquire);
        match &self.control {
            ProductionControlOwner::Region { core, .. } => {
                let Ok(observation) = core.observation() else {
                    return;
                };
                // A completed schema/connection interval must not poison the
                // enclosing live issuer when its affine permit is dropped.
                if !completed && observation.phase == 1 {
                    self.control.control().abandoned();
                    core.abandoned();
                }
                core.reference_retired();
            }
            ProductionControlOwner::Shared(control) => {
                if !completed {
                    control.abandoned();
                }
            }
        }
    }
}
impl std::fmt::Debug for SqliteInspectionProductionPermit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteInspectionProductionPermit")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}
#[derive(Debug)]
pub struct SqliteInspectionProductionFailure {
    error: anyhow::Error,
    _production: SqliteInspectionProductionPermit,
}
impl std::fmt::Display for SqliteInspectionProductionFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.error, f)
    }
}
impl std::error::Error for SqliteInspectionProductionFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}

pub trait SqliteInspectionCopyWork: Send + Sync {
    /// Verify the retained original post-ACK account, without service re-entry.
    /// Non-original copy implementations have no such authority.
    fn require_original_outside_master(
        &self,
        _factory: &Arc<OriginalNativeFactory>,
        _aggregate: &OriginalNativeTerminal,
    ) -> Result<()> {
        anyhow::bail!("copy work has no original outside finalizer master")
    }

    /// Require the actual native factory issued by this SAME authentic original
    /// copy claim. Equal paths, receipts or numeric budgets cannot establish it.
    fn require_original_native_factory(&self, factory: &Arc<OriginalNativeFactory>) -> Result<()>;

    fn begin_copy_production(
        &self,
        source: &PinnedDirectory,
        destination: &PinnedDirectory,
        name: &OsStr,
    ) -> Result<SqliteInspectionProductionPermit>;

    /// One SQL inspection owner for an actual original created destination.
    fn issue_sql_work(
        self: Arc<Self>,
        destination: &PinnedDirectory,
    ) -> Result<Arc<dyn SqliteInspectionSqlWork>>;

    /// Create only the namespace reserved by this original issued operation.
    /// Failure must retain original work/custody, including uncertain mkdir.
    fn create_destination(
        self: Arc<Self>,
        source: &PinnedDirectory,
        database_name: &OsStr,
    ) -> Result<SqliteInspectionCopyDestination>;

    fn admit_role(
        &self,
        source: &PinnedDirectory,
        destination: &PinnedDirectory,
        name: &OsStr,
        original: Option<&SqliteRoleIdentity>,
        maximum_read_bytes: u64,
        working_memory_bytes: u64,
    ) -> Result<()>;

    /// Retain the SAME exclusively created output before any fallible body
    /// work. Errors must retain the file too; no cleanup or refund is issued.
    fn retain_destination_role(
        &self,
        destination: &PinnedDirectory,
        name: &OsStr,
        file: Arc<RetainedSqliteDestinationRole>,
    ) -> Result<()>;

    /// Original remaining service-plus-cleanup interval, never a new clock.
    /// This is custody/time validation, not global cleanup-work or funding.
    fn cleanup_checkpoint(&self) -> Result<std::time::Duration>;

    /// Read the original shared remaining budget; this does not reserve or renew it.
    fn remaining_cleanup_steps(&self) -> Result<u64>;

    /// Charge the exact finite identity-scan bound plus one close boundary.
    fn debit_destination_retirement(&self, max_identity_checks: u32) -> Result<()>;

    /// Irreversibly charge the original aggregate before a cleanup effect.
    fn debit_cleanup_steps(&self, steps: u64) -> Result<()>;

    /// Return only an already-retained original role owner under cleanup custody.
    /// Missing present owners/unexpected absent owners refuse; no path reconstruction.
    fn retained_destination_role(
        &self,
        destination: &PinnedDirectory,
        name: &OsStr,
    ) -> Result<Option<Arc<RetainedSqliteDestinationRole>>>;

    /// Refuse work beyond the original service window; retry cannot renew it.
    fn checkpoint(&self) -> Result<()>;

    /// Original remaining interval for caller-owned namespace waits.
    fn remaining_service_window(&self) -> Result<std::time::Duration>;
}

/// One original output descriptor. Arc aliases refer to this owner, never File.
pub struct RetainedSqliteDestinationRole {
    identity: std::sync::OnceLock<(u64, u64)>,
    inner: lillux::custody::Held<DestinationRoleParts>,
}
/// The role's coordinate and descriptor state. Constructible only in this
/// module.
struct DestinationRoleParts {
    root: lillux::PinnedDirectoryIdentity,
    name: std::ffi::OsString,
    state: std::sync::Mutex<DestinationRoleState>,
}
/// Unsupervised last-drop is preservation, never alias-close evidence. A role
/// whose descriptor is not open (retired, consumed or pending a raw close)
/// settles and is dropped; an unverified numeric descriptor is never closed.
/// A busy lock is unsettled, and a poisoned one is read through.
fn destination_role_settled(parts: &DestinationRoleParts) -> bool {
    use std::sync::TryLockError;
    match parts.state.try_lock() {
        Ok(state) => !matches!(*state, DestinationRoleState::Open(_)),
        Err(TryLockError::Poisoned(poisoned)) => {
            !matches!(*poisoned.into_inner(), DestinationRoleState::Open(_))
        }
        Err(TryLockError::WouldBlock) => false,
    }
}
enum DestinationRoleState {
    Open(std::fs::File),
    PendingRaw(i32),
    Retired,
    ConsumedError { code: i32, errno: i32 },
}
impl std::fmt::Debug for RetainedSqliteDestinationRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RetainedSqliteDestinationRole")
            .finish_non_exhaustive()
    }
}
impl RetainedSqliteDestinationRole {
    fn created(
        file: std::fs::File,
        root: lillux::PinnedDirectoryIdentity,
        name: std::ffi::OsString,
    ) -> Self {
        Self {
            identity: std::sync::OnceLock::new(),
            inner: lillux::custody::Held::new(
                AbandonedSqliteOwnerKind::DestinationRole.custody_kind(),
                DestinationRoleParts {
                    root,
                    name,
                    state: std::sync::Mutex::new(DestinationRoleState::Open(file)),
                },
                destination_role_settled,
            ),
        }
    }
    fn state(&self) -> Result<std::sync::MutexGuard<'_, DestinationRoleState>> {
        self.inner
            .state
            .try_lock()
            .map_err(|_| anyhow::anyhow!("original output state busy or poisoned"))
    }
    fn with_file<T>(&self, op: impl FnOnce(&std::fs::File) -> Result<T>) -> Result<T> {
        let state = self.state()?;
        let DestinationRoleState::Open(file) = &*state else {
            anyhow::bail!("original output is retiring or retired")
        };
        op(file)
    }
    pub fn require_original(&self, root: &PinnedDirectory, name: &OsStr) -> Result<()> {
        ensure!(
            root.identity()? == self.inner.root && *self.inner.name == *name,
            "output selects another original root or role"
        );
        Ok(())
    }
    pub fn metadata_original(&self) -> Result<std::fs::Metadata> {
        self.with_file(|file| {
            let metadata = file.metadata()?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                let identity = (metadata.dev(), metadata.ino());
                ensure!(
                    self.identity.get_or_init(|| identity) == &identity,
                    "original output descriptor changed identity"
                );
            }
            Ok(metadata)
        })
    }
    fn write_chunk_at(&self, offset: u64, bytes: &[u8]) -> Result<usize> {
        ensure!(
            bytes.len() <= 65_536 && offset <= i64::MAX as u64,
            "output write exceeds bounded copy chunk"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            self.with_file(|file| Ok(file.write_at(bytes, offset)?))
        }
        #[cfg(not(unix))]
        {
            let _ = (offset, bytes);
            anyhow::bail!("original output write unavailable")
        }
    }
    fn read_hash_chunk_at(&self, offset: u64, bytes: &mut [u8]) -> Result<usize> {
        ensure!(
            bytes.len() <= 65_536 && offset <= i64::MAX as u64,
            "output hash exceeds bounded chunk"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            self.with_file(|file| Ok(file.read_at(bytes, offset)?))
        }
        #[cfg(not(unix))]
        {
            let _ = (offset, bytes);
            anyhow::bail!("original output hash unavailable")
        }
    }
    fn sync_original(&self) -> Result<()> {
        self.with_file(|file| Ok(file.sync_all()?))
    }
    fn require_copy_unchanged(
        &self,
        root: &PinnedDirectory,
        name: &OsStr,
        expected: &std::fs::Metadata,
    ) -> Result<()> {
        self.require_original(root, name)?;
        #[cfg(unix)]
        {
            self.with_file(|file| ensure_copy_source_unchanged(root, name, file, expected))
        }
        #[cfg(not(unix))]
        {
            let _ = expected;
            anyhow::bail!("original output identity unavailable")
        }
    }
    pub fn remove_original_planned(
        &self,
        authority: &SqliteInspectionBodyAuthority,
        root: &PinnedDirectory,
        source: &OsStr,
        quarantine: &OsStr,
    ) -> std::result::Result<(), lillux::secure_fs::PlannedQuarantineFailure> {
        use lillux::secure_fs::{
            PlannedQuarantineDisposition, PlannedQuarantineFailure, PlannedQuarantineStage,
        };
        let fail = |error| PlannedQuarantineFailure {
            error,
            disposition: PlannedQuarantineDisposition::BeforeCommit,
            stage: PlannedQuarantineStage::Unchanged,
            quarantine_name: quarantine.to_os_string(),
        };
        authority.require_role(self).map_err(fail)?;
        ensure_role_cleanup_names(&self.inner.name, source, quarantine).map_err(fail)?;
        (|| -> Result<()> {
            ensure!(
                root.identity()? == self.inner.root,
                "planned removal selects foreign root"
            );
            Ok(())
        })()
        .map_err(fail)?;
        authority.copy_work.cleanup_checkpoint().map_err(fail)?;
        // Retention can precede a failed producer metadata checkpoint. Admit
        // this original-descriptor observation before any planned mutation.
        authority.copy_work.debit_cleanup_steps(1).map_err(fail)?;
        self.metadata_original().map_err(fail)?;
        if authority.deadline.has_elapsed() {
            return Err(fail(anyhow::anyhow!("original body deadline expired")));
        }
        let state = self.state().map_err(fail)?;
        let DestinationRoleState::Open(file) = &*state else {
            return Err(fail(anyhow::anyhow!("original output already retiring")));
        };
        root.remove_if_same_planned_atomic(source, file, quarantine)
    }
    pub fn retire_disposed_original(
        self: &Arc<Self>,
        authority: &SqliteInspectionDisposalAuthority,
        maximum: u32,
    ) -> Result<RetiredSqliteDestinationRole> {
        authority.body.require_role(self)?;
        ensure!(
            maximum > 0 && maximum <= i32::MAX as u32,
            "invalid original retirement scan bound"
        );
        ensure!(
            !authority.body.deadline.has_elapsed(),
            "original retirement deadline expired"
        );
        authority
            .body
            .copy_work
            .debit_destination_retirement(maximum)?;
        ensure!(
            !authority.body.deadline.has_elapsed(),
            "original retirement deadline expired after charge"
        );
        let mut state = self.state()?;
        if matches!(*state, DestinationRoleState::Retired) {
            return Ok(RetiredSqliteDestinationRole {
                owner: self.clone(),
            });
        }
        if let DestinationRoleState::ConsumedError { code, errno } = &*state {
            return Err(anyhow::Error::new(SqliteDestinationRetirementFailure {
                owner: self.clone(),
                code: *code,
                consumed: true,
                observed_errno: *errno,
            }));
        }
        let (device, inode) = *self
            .identity
            .get()
            .ok_or_else(|| anyhow::anyhow!("original output identity unacknowledged"))?;
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::IntoRawFd;
            if matches!(*state, DestinationRoleState::Open(_)) {
                let DestinationRoleState::Open(file) =
                    std::mem::replace(&mut *state, DestinationRoleState::Retired)
                else {
                    unreachable!()
                };
                *state = DestinationRoleState::PendingRaw(file.into_raw_fd());
            }
            let DestinationRoleState::PendingRaw(fd) = &*state else {
                unreachable!()
            };
            let result = binding::close_original_output(*fd, device, inode, maximum);
            if result.consumed {
                *state = if result.code == libsqlite3_sys::SQLITE_OK && result.observed_errno == 0 {
                    DestinationRoleState::Retired
                } else {
                    DestinationRoleState::ConsumedError {
                        code: result.code,
                        errno: result.observed_errno,
                    }
                };
            }
            if !result.consumed
                || result.code != libsqlite3_sys::SQLITE_OK
                || result.observed_errno != 0
            {
                return Err(anyhow::Error::new(SqliteDestinationRetirementFailure {
                    owner: self.clone(),
                    code: result.code,
                    consumed: result.consumed,
                    observed_errno: result.observed_errno,
                }));
            }
            Ok(RetiredSqliteDestinationRole {
                owner: self.clone(),
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (device, inode);
            anyhow::bail!("original output retirement unavailable")
        }
    }
}
/// What kind of SQLite owner was retained without supervised close.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AbandonedSqliteOwnerKind {
    DestinationRole,
    BoundConnection,
    NativeConnection,
    NativeCloseFailure,
    NativeOpenFailure,
}
impl AbandonedSqliteOwnerKind {
    /// The kind under which the process custody holder counts this owner.
    const fn custody_kind(self) -> &'static str {
        match self {
            Self::DestinationRole => "lillux-sqlite::destination-role",
            Self::BoundConnection => "lillux-sqlite::bound-connection",
            Self::NativeConnection => "lillux-sqlite::native-connection",
            Self::NativeCloseFailure => "lillux-sqlite::native-close-failure",
            Self::NativeOpenFailure => "lillux-sqlite::native-open-failure",
        }
    }
}

/// Retain a SQLite owner discarded without supervised close. Its native
/// handles or descriptors may still be live, and closing them from a
/// destructor would bypass the supervised terminal, so the owner stays
/// reachable in the process custody holder until the process exits. In the
/// child of a protected fork the copy is relinquished without allocating.
pub(crate) fn retain_abandoned_sqlite_owner<T: Send + 'static>(
    kind: AbandonedSqliteOwnerKind,
    owner: T,
) {
    lillux::custody::abandon(kind.custody_kind(), owner);
}

/// Count of SQLite owners this process retains because they were discarded
/// without supervised close. Any nonzero count means native custody stays
/// held until the process exits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AbandonedSqliteOwners {
    pub destination_roles: usize,
    pub bound_connections: usize,
    pub native_connections: usize,
    pub native_close_failures: usize,
    pub native_open_failures: usize,
}

/// Report the abandoned SQLite owners this process retains: the SQLite
/// connection and destination kinds of the process custody holder.
pub fn abandoned_sqlite_owners() -> AbandonedSqliteOwners {
    let retained = lillux::custody::abandoned_custody();
    AbandonedSqliteOwners {
        destination_roles: retained.count(AbandonedSqliteOwnerKind::DestinationRole.custody_kind()),
        bound_connections: retained.count(AbandonedSqliteOwnerKind::BoundConnection.custody_kind()),
        native_connections: retained
            .count(AbandonedSqliteOwnerKind::NativeConnection.custody_kind()),
        native_close_failures: retained
            .count(AbandonedSqliteOwnerKind::NativeCloseFailure.custody_kind()),
        native_open_failures: retained
            .count(AbandonedSqliteOwnerKind::NativeOpenFailure.custody_kind()),
    }
}

/// Native refusal/error retains the exact original owner and observed disposition.
#[derive(Debug)]
pub struct SqliteDestinationRetirementFailure {
    owner: Arc<RetainedSqliteDestinationRole>,
    code: i32,
    consumed: bool,
    observed_errno: i32,
}
impl SqliteDestinationRetirementFailure {
    pub fn owner(&self) -> &Arc<RetainedSqliteDestinationRole> {
        &self.owner
    }
    pub fn code(&self) -> i32 {
        self.code
    }
    pub fn consumed(&self) -> bool {
        self.consumed
    }
    pub fn observed_errno(&self) -> i32 {
        self.observed_errno
    }
}
impl std::fmt::Display for SqliteDestinationRetirementFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "original output close refused: code {}, consumed {}, errno {}",
            self.code, self.consumed, self.observed_errno
        )
    }
}
impl std::error::Error for SqliteDestinationRetirementFailure {}

pub struct RetiredSqliteDestinationRole {
    owner: Arc<RetainedSqliteDestinationRole>,
}
impl RetiredSqliteDestinationRole {
    pub fn original_identity(&self) -> Result<(u64, u64)> {
        self.require_original(&self.owner)?;
        self.owner
            .identity
            .get()
            .copied()
            .context("retired original output identity missing")
    }
    pub fn require_original(&self, owner: &Arc<RetainedSqliteDestinationRole>) -> Result<()> {
        ensure!(
            Arc::ptr_eq(&self.owner, owner),
            "retirement proof selects foreign original owner"
        );
        ensure!(
            matches!(*self.owner.state()?, DestinationRoleState::Retired),
            "original role has no successful close"
        );
        Ok(())
    }
}
impl std::fmt::Debug for RetiredSqliteDestinationRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RetiredSqliteDestinationRole")
            .finish_non_exhaustive()
    }
}
fn ensure_role_cleanup_names(original: &OsStr, source: &OsStr, target: &OsStr) -> Result<()> {
    let mut quarantine = std::ffi::OsString::from("cleanup.");
    quarantine.push(original);
    ensure!(
        (source == original && target == quarantine)
            || (source == quarantine && target == original),
        "cleanup changes fixed role/quarantine coordinates"
    );
    Ok(())
}
pub struct SqliteInspectionBodyAuthority {
    root: Arc<PinnedDirectory>,
    database_name: std::ffi::OsString,
    lease: SqliteCopyLease,
    sql: Arc<dyn SqliteInspectionSqlWork>,
    copy_work: Arc<dyn SqliteInspectionCopyWork>,
    roles: [Option<Arc<RetainedSqliteDestinationRole>>; 4],
    deadline: lillux::time::MonotonicDeadline,
}
impl SqliteInspectionBodyAuthority {
    fn require_role(&self, role: &RetainedSqliteDestinationRole) -> Result<()> {
        ensure!(
            self.roles
                .iter()
                .flatten()
                .any(|original| std::ptr::eq(original.as_ref(), role)),
            "cleanup selects nonoriginal role owner"
        );
        ensure!(
            role.inner.root == self.root.identity()?,
            "cleanup selects foreign original inode root"
        );
        Ok(())
    }
}
pub struct SqliteInspectionDisposalAuthority {
    body: Arc<SqliteInspectionBodyAuthority>,
    _removed: Arc<lillux::secure_fs::RemovedEmptyChild>,
}

/// Distinct original ordinary-witness work; never copied-role disposal authority.
pub trait OriginalWitnessCleanupWork: Send + Sync {
    fn retain_original_witness_progress(
        &self,
        _progress: &OriginalWitnessRemovalProgress,
    ) -> Result<()> {
        Ok(())
    }
    fn prepay_original_removal_parent(&self, _bytes: u64, _steps: u64) -> Result<()> {
        anyhow::bail!("witness work has no original parent storage allowance")
    }
    /// Retain a genuine durable effect before the post-effect account check.
    /// Default observation grants no allowance or account release authority.
    fn retain_original_removed_witness(&self, _removed: &RemovedOriginalWitnessFile) -> Result<()> {
        Ok(())
    }
    fn checkpoint(&self) -> Result<()>;
    fn require_namespace(&self, parent: &PinnedDirectory) -> Result<()>;
    fn debit_steps(&self, steps: u64) -> Result<()>;
    fn debit_retirement(&self, maximum: u32) -> Result<()>;
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OriginalWitnessRemovalPhase {
    Original,
    Quarantined,
    UnlinkedPendingSync,
    RemovedDurable,
    Conflict,
}
struct OriginalWitnessRemovalState {
    quarantine: Option<std::ffi::OsString>,
    parent: Option<Arc<PinnedDirectory>>,
    deadline: Option<lillux::time::MonotonicDeadline>,
    phase: OriginalWitnessRemovalPhase,
    parent_transferred: bool,
    work: Option<std::sync::Weak<dyn OriginalWitnessCleanupWork>>,
}
const WITNESS_REMOVAL_KIND: &str = "lillux-sqlite::witness-removal-parent";
/// A cleanup parent still present at the end belongs to a removal that never
/// settled: the owning witness releases it first once the original is durably
/// removed and its descriptor retired. An unsettled parent directory stays
/// held, counted, until the process exits, together with the rest of the
/// removal state. A copy in another process relinquishes the whole state, its
/// name and work handle included, without allocating or running a destructor.
fn witness_removal_settled(state: &OriginalWitnessRemovalState) -> bool {
    state.parent.is_none()
}
fn held_removal_state(
    state: OriginalWitnessRemovalState,
) -> lillux::custody::Held<OriginalWitnessRemovalState> {
    lillux::custody::Held::new(WITNESS_REMOVAL_KIND, state, witness_removal_settled)
}
/// Transfers the actual publisher File; no File/FD/dup borrow can escape.
pub struct RetainedOriginalWitnessFile {
    process: u32,
    original: RetainedSqliteDestinationRole,
    removal: std::sync::Mutex<Option<lillux::custody::Held<OriginalWitnessRemovalState>>>,
}
impl Drop for RetainedOriginalWitnessFile {
    fn drop(&mut self) {
        // Settled exactly when the original is durably removed and its
        // descriptor retired: the cleanup parent has no remaining use and is
        // released here. Any other removal leaves the parent in place, so the
        // removal state's own end retains it. A copy in another process
        // touches nothing; its removal state relinquishes itself.
        if crate::foreign_process(self.process) {
            return;
        }
        let retired = destination_role_retired(&self.original.inner);
        let removal = self
            .removal
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(removal) = removal
            && retired
            && removal.phase == OriginalWitnessRemovalPhase::RemovedDurable
        {
            drop(removal.get_mut().parent.take());
        }
    }
}
/// Whether the role's descriptor is retired. A busy lock reads as not retired,
/// and a poisoned one is read through.
fn destination_role_retired(parts: &DestinationRoleParts) -> bool {
    use std::sync::TryLockError;
    match parts.state.try_lock() {
        Ok(state) => matches!(*state, DestinationRoleState::Retired),
        Err(TryLockError::Poisoned(poisoned)) => {
            matches!(*poisoned.into_inner(), DestinationRoleState::Retired)
        }
        Err(TryLockError::WouldBlock) => false,
    }
}
impl std::fmt::Debug for RetainedOriginalWitnessFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RetainedOriginalWitnessFile")
            .finish_non_exhaustive()
    }
}
impl RetainedOriginalWitnessFile {
    pub fn created(
        file: std::fs::File,
        parent: lillux::PinnedDirectoryIdentity,
        name: std::ffi::OsString,
    ) -> Self {
        Self {
            process: std::process::id(),
            original: RetainedSqliteDestinationRole::created(file, parent, name),
            removal: std::sync::Mutex::new(Some(held_removal_state(OriginalWitnessRemovalState {
                parent: None,
                quarantine: None,
                deadline: None,
                phase: OriginalWitnessRemovalPhase::Original,
                parent_transferred: false,
                work: None,
            }))),
        }
    }
    fn require_original_process(&self) -> Result<()> {
        ensure!(
            !crate::foreign_process(self.process),
            "foreign original witness owner"
        );
        Ok(())
    }
    pub fn identity_original(&self) -> Result<lillux::OpenFileIdentity> {
        self.require_original_process()?;
        self.original.metadata_original()?;
        self.original.with_file(lillux::observe_open_file_identity)
    }
    pub fn metadata_original(&self) -> Result<std::fs::Metadata> {
        self.require_original_process()?;
        self.original.metadata_original()
    }
    pub fn require_original(&self, parent: &PinnedDirectory, name: &OsStr) -> Result<()> {
        self.require_original_process()?;
        self.original.require_original(parent, name)
    }
    pub fn read_original(&self, maximum: usize) -> Result<Vec<u8>> {
        self.require_original_process()?;
        ensure!(
            maximum > 0 && maximum <= 8192,
            "original witness read exceeds admitted record capacity"
        );
        #[cfg(target_os = "linux")]
        {
            // The publisher retains a writable descriptor; the guarded read
            // takes a path-only pin of the same inode and reopens it itself.
            self.original.with_file(|file| {
                let pin = lillux::path_pin_open_regular_file(file)?;
                binding::read_unbound_witness(&pin, maximum)
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            anyhow::bail!("original witness read unavailable")
        }
    }
    /// Actual SAME-owner staged unlink/sync only; missing names alone cannot mint.
    /// Retain the actual cleanup parent before the supervisor seals its
    /// inventory. This performs no rename, unlink or descriptor retirement.
    pub fn prepare_original_removal_parent(
        self: &Arc<Self>,
        work: Arc<dyn OriginalWitnessCleanupWork>,
        parent: &PinnedDirectory,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<Arc<PinnedDirectory>> {
        self.require_original_process()?;
        let mut original = self
            .removal
            .try_lock()
            .map_err(|_| anyhow::anyhow!("original witness parent preparation busy or poisoned"))?
            .take()
            .context("original witness removal already transferring")?;
        let state = std::cell::RefCell::new(&mut original);
        let result = original_physical_custody::retain_callback_refusal(&|| {
            let mut state = state.borrow_mut();
            let original = (**state).get_mut();
            ensure!(
                !original.parent_transferred,
                "original witness preparation parent already transferred"
            );
            if let Some(prior) = &original.work {
                ensure!(
                    std::sync::Weak::ptr_eq(prior, &Arc::downgrade(&work)),
                    "foreign original witness preparation work"
                );
            } else {
                original.work = Some(Arc::downgrade(&work));
            }
            let deadline = *original.deadline.get_or_insert(deadline);
            ensure_original_deadline(deadline)?;
            work.checkpoint()?;
            work.require_namespace(parent)?;
            self.require_original_process()?;
            self.original
                .require_original(parent, &self.original.inner.name)?;
            ensure!(
                original.phase == OriginalWitnessRemovalPhase::Original,
                "original witness preparation follows namespace effects"
            );
            if original.parent.is_none() {
                let bytes = std::mem::size_of::<PinnedDirectory>()
                    .checked_add(2 * std::mem::size_of::<usize>())
                    .and_then(|n| {
                        self.original
                            .inner
                            .name
                            .len()
                            .checked_mul(2)
                            .and_then(|names| names.checked_add(16))
                            .and_then(|names| n.checked_add(names))
                    })
                    .context("original witness parent storage overflow")?;
                work.prepay_original_removal_parent(u64::try_from(bytes)?, 2)?;
                self.require_original_process()?;
                original.parent = Some(Arc::new(parent.try_clone()?));
            }
            if original.quarantine.is_none() {
                let mut quarantine = std::ffi::OsString::from("cleanup.");
                quarantine.push(&*self.original.inner.name);
                original.quarantine = Some(quarantine);
            }
            Ok(())
        });
        let parent = if result.is_ok() {
            original.parent.as_ref().cloned()
        } else {
            None
        };
        *self.removal.lock().unwrap_or_else(|p| p.into_inner()) = Some(original);
        result?;
        parent.context("actual prepared witness parent absent")
    }
    pub fn remove_original_planned(
        self: &Arc<Self>,
        work: Arc<dyn OriginalWitnessCleanupWork>,
        parent: &PinnedDirectory,
        deadline: lillux::time::MonotonicDeadline,
    ) -> std::result::Result<RemovedOriginalWitnessFile, lillux::secure_fs::PlannedQuarantineFailure>
    {
        use lillux::secure_fs::{
            PlannedQuarantineDisposition, PlannedQuarantineFailure, PlannedQuarantineStage,
        };
        let unchanged = |error| PlannedQuarantineFailure {
            error,
            disposition: PlannedQuarantineDisposition::BeforeCommit,
            stage: PlannedQuarantineStage::Unchanged,
            quarantine_name: std::ffi::OsString::new(),
        };
        self.require_original_process().map_err(unchanged)?;
        // None excludes a parallel attempt. The complete original phase/parent
        // is checked out; no work callback, C call or filesystem effect runs
        // under this bookkeeping gate.
        let mut original = self
            .removal
            .try_lock()
            .map_err(|_| {
                unchanged(anyhow::anyhow!(
                    "original witness removal custody busy or poisoned"
                ))
            })?
            .take()
            .ok_or_else(|| {
                unchanged(anyhow::anyhow!(
                    "original witness removal already transferring"
                ))
            })?;
        let progress = std::cell::RefCell::new(&mut original);
        let result = original_physical_custody::retain_callback_refusal(&|| {
            let mut original = progress.borrow_mut();
            let original = (**original).get_mut();
            ensure!(
                !original.parent_transferred,
                "original witness removal parent already transferred"
            );
            if let Some(prior) = &original.work {
                ensure!(
                    std::sync::Weak::ptr_eq(prior, &Arc::downgrade(&work)),
                    "foreign original witness removal work"
                );
            } else {
                original.work = Some(Arc::downgrade(&work));
            }
            let deadline = *original.deadline.get_or_insert(deadline);
            if let Some(progress) = self.original_removal_progress(original, &work) {
                work.retain_original_witness_progress(&progress)?;
            }
            ensure_original_deadline(deadline)?;
            work.checkpoint()?;
            work.require_namespace(parent)?;
            self.require_original_process()?;
            self.original
                .require_original(parent, &self.original.inner.name)?;
            ensure!(
                original.phase != OriginalWitnessRemovalPhase::Conflict,
                "original witness retains namespace conflict"
            );
            if original.parent.is_none() {
                let bytes = std::mem::size_of::<PinnedDirectory>()
                    .checked_add(2 * std::mem::size_of::<usize>())
                    .and_then(|n| {
                        self.original
                            .inner
                            .name
                            .len()
                            .checked_mul(2)
                            .and_then(|names| names.checked_add(16))
                            .and_then(|names| n.checked_add(names))
                    })
                    .context("original witness parent storage overflow")?;
                work.prepay_original_removal_parent(u64::try_from(bytes)?, 2)?;
                self.require_original_process()?;
                original.parent = Some(Arc::new(parent.try_clone()?));
                let mut quarantine = std::ffi::OsString::from("cleanup.");
                quarantine.push(&*self.original.inner.name);
                original.quarantine = Some(quarantine);
            }
            let retained_parent = original
                .parent
                .as_ref()
                .expect("cached original witness parent")
                .clone();
            let quarantine = original
                .quarantine
                .as_ref()
                .context("original witness quarantine coordinate absent")?;
            work.debit_steps(1)?;
            self.original.metadata_original()?;
            if matches!(
                original.phase,
                OriginalWitnessRemovalPhase::UnlinkedPendingSync
                    | OriginalWitnessRemovalPhase::RemovedDurable
            ) {
                work.debit_steps(2)?;
                ensure!(
                    retained_parent
                        .entry_no_follow(&self.original.inner.name)?
                        .is_none()
                        && retained_parent.entry_no_follow(quarantine)?.is_none(),
                    "removed witness coordinate reappeared"
                );
                if original.phase == OriginalWitnessRemovalPhase::UnlinkedPendingSync {
                    work.debit_steps(1)?;
                    retained_parent.sync()?;
                    original.phase = OriginalWitnessRemovalPhase::RemovedDurable;
                }
            } else {
                let (source, target) = if original.phase == OriginalWitnessRemovalPhase::Quarantined
                {
                    (quarantine.as_os_str(), self.original.inner.name.as_os_str())
                } else {
                    (self.original.inner.name.as_os_str(), quarantine.as_os_str())
                };
                work.debit_steps(1)?;
                ensure_original_deadline(deadline)?;
                self.require_original_process()?;
                let effect = self.original.with_file(|file| {
                    Ok(retained_parent.remove_if_same_planned_atomic(source, file, target))
                })?;
                match effect {
                    Ok(()) => original.phase = OriginalWitnessRemovalPhase::RemovedDurable,
                    Err(error) => {
                        original.phase = match error.stage {
                            PlannedQuarantineStage::UnlinkedOriginal => {
                                OriginalWitnessRemovalPhase::UnlinkedPendingSync
                            }
                            PlannedQuarantineStage::QuarantinedOriginal
                                if target == quarantine.as_os_str() =>
                            {
                                OriginalWitnessRemovalPhase::Quarantined
                            }
                            PlannedQuarantineStage::QuarantinedOriginal => {
                                OriginalWitnessRemovalPhase::Original
                            }
                            PlannedQuarantineStage::PreservedUnexpected => {
                                OriginalWitnessRemovalPhase::Conflict
                            }
                            _ => original.phase,
                        };
                        if let Some(progress) = self.original_removal_progress(original, &work)
                            && let Err(callback) = work.retain_original_witness_progress(&progress)
                        {
                            return Err(crate::RefusalSupersedes::superseding(
                                callback,
                                anyhow::Error::new(error),
                            ));
                        }
                        return Err(anyhow::Error::new(error));
                    }
                }
            }
            let removed = RemovedOriginalWitnessFile {
                owner: self.clone(),
                work: work.clone(),
                parent: Arc::downgrade(&retained_parent),
                deadline,
            };
            if let Some(progress) = self.original_removal_progress(original, &work) {
                work.retain_original_witness_progress(&progress)?;
            }
            // Actual durable prefix exists before either post-effect callback.
            // The original parent stays cached even if a callback refuses.
            work.retain_original_removed_witness(&removed)?;
            work.checkpoint()?;
            ensure_original_deadline(deadline)?;
            Ok(())
        });
        let phase = original.phase;
        let quarantine_name = if result.is_err() {
            original.quarantine.as_ref().cloned().unwrap_or_default()
        } else {
            std::ffi::OsString::new()
        };
        let removed = if result.is_ok() {
            Some(RemovedOriginalWitnessFile {
                owner: self.clone(),
                work,
                parent: Arc::downgrade(
                    original
                        .parent
                        .as_ref()
                        .expect("cached original witness parent"),
                ),
                deadline: original.deadline.expect("first original removal deadline"),
            })
        } else {
            None
        };
        // Restore all original custody before reporting any error or panic.
        *self.removal.lock().unwrap_or_else(|p| p.into_inner()) = Some(original);
        match result {
            Ok(()) => Ok(removed.expect("actual durable removal receipt")),
            Err(error) => {
                let stage = match phase {
                    OriginalWitnessRemovalPhase::Quarantined => {
                        PlannedQuarantineStage::QuarantinedOriginal
                    }
                    OriginalWitnessRemovalPhase::UnlinkedPendingSync
                    | OriginalWitnessRemovalPhase::RemovedDurable => {
                        PlannedQuarantineStage::UnlinkedOriginal
                    }
                    OriginalWitnessRemovalPhase::Conflict => {
                        PlannedQuarantineStage::PreservedUnexpected
                    }
                    OriginalWitnessRemovalPhase::Original => PlannedQuarantineStage::Unchanged,
                };
                Err(PlannedQuarantineFailure {
                    error,
                    stage,
                    quarantine_name,
                    disposition: if stage == PlannedQuarantineStage::Unchanged {
                        PlannedQuarantineDisposition::BeforeCommit
                    } else {
                        PlannedQuarantineDisposition::NamespaceChanged
                    },
                })
            }
        }
    }
    fn original_removal_progress(
        self: &Arc<Self>,
        original: &OriginalWitnessRemovalState,
        work: &Arc<dyn OriginalWitnessCleanupWork>,
    ) -> Option<OriginalWitnessRemovalProgress> {
        if original.phase == OriginalWitnessRemovalPhase::Conflict {
            return None;
        }
        Some(OriginalWitnessRemovalProgress {
            owner: self.clone(),
            work: work.clone(),
            parent: Arc::downgrade(original.parent.as_ref()?),
            deadline: original.deadline?,
            phase: original.phase,
        })
    }
    /// The caller prepays the registry scan from this SAME original account.
    /// Only a genuine durable removal and actual successful witness close can
    /// end this duplicate parent; the registry retains its exact canonical Arc.
    pub fn transfer_original_removal_parent(
        self: &Arc<Self>,
        removed: &RemovedOriginalWitnessFile,
        retired: &RetiredOriginalWitnessFile,
        work: &Arc<dyn OriginalWitnessCleanupWork>,
        registry: &original_physical_custody::OriginalPhysicalRegistry,
        binding: &original_physical_custody::OriginalPhysicalRegistration,
    ) -> Result<()> {
        self.require_original_process()?;
        removed.require_original(self, work)?;
        retired.require_original(self)?;
        let parent = {
            let state = self.removal.try_lock().map_err(|_| {
                anyhow::anyhow!("original witness parent transfer busy or poisoned")
            })?;
            let state = state
                .as_ref()
                .context("original witness removal transferring")?;
            ensure!(
                state.phase == OriginalWitnessRemovalPhase::RemovedDurable,
                "original witness parent transfer precedes durable removal"
            );
            ensure!(
                !state.parent_transferred,
                "original removal parent transfer already memoized"
            );
            state
                .parent
                .as_ref()
                .cloned()
                .context("actual original removal parent absent")?
        };
        registry.require_directory_source(binding, &parent)?;
        let ended = {
            let mut state = self.removal.try_lock().map_err(|_| {
                anyhow::anyhow!("original witness parent transfer memo busy or poisoned")
            })?;
            let state = state
                .as_mut()
                .context("original witness removal transferring")?
                .get_mut();
            ensure!(
                !state.parent_transferred
                    && state.phase == OriginalWitnessRemovalPhase::RemovedDurable
                    && state
                        .parent
                        .as_ref()
                        .is_some_and(|actual| Arc::ptr_eq(actual, &parent)),
                "original witness parent transfer changed custody"
            );
            state.parent_transferred = true;
            state
                .parent
                .take()
                .expect("same actual removal parent checked")
        };
        drop(ended);
        drop(parent);
        Ok(())
    }
    pub fn retire_removed_original(
        self: &Arc<Self>,
        removed: &RemovedOriginalWitnessFile,
        maximum: u32,
    ) -> Result<RetiredOriginalWitnessFile> {
        self.require_original_process()?;
        ensure!(
            Arc::ptr_eq(self, &removed.owner),
            "witness removal selects foreign original descriptor"
        );
        ensure!(
            maximum > 0 && maximum <= i32::MAX as u32,
            "invalid original witness retirement scan bound"
        );
        removed.work.checkpoint()?;
        let parent = removed
            .parent
            .upgrade()
            .context("actual original removal parent custody absent")?;
        removed.work.require_namespace(&parent)?;
        ensure_original_deadline(removed.deadline)?;
        removed.work.debit_retirement(maximum)?;
        ensure_original_deadline(removed.deadline)?;
        let (device, inode) = *self
            .original
            .identity
            .get()
            .context("original witness identity unacknowledged")?;
        let mut state = self.original.state()?;
        if matches!(*state, DestinationRoleState::Retired) {
            return Ok(RetiredOriginalWitnessFile {
                owner: self.clone(),
            });
        }
        if let DestinationRoleState::ConsumedError { code, errno } = &*state {
            return Err(anyhow::Error::new(OriginalWitnessRetirementFailure {
                owner: self.clone(),
                code: *code,
                consumed: true,
                observed_errno: *errno,
            }));
        }
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::IntoRawFd;
            if matches!(*state, DestinationRoleState::Open(_)) {
                let DestinationRoleState::Open(file) =
                    std::mem::replace(&mut *state, DestinationRoleState::Retired)
                else {
                    unreachable!()
                };
                *state = DestinationRoleState::PendingRaw(file.into_raw_fd());
            }
            let DestinationRoleState::PendingRaw(fd) = &*state else {
                unreachable!()
            };
            let result = binding::close_original_output(*fd, device, inode, maximum);
            if result.consumed {
                *state = if result.code == libsqlite3_sys::SQLITE_OK && result.observed_errno == 0 {
                    DestinationRoleState::Retired
                } else {
                    DestinationRoleState::ConsumedError {
                        code: result.code,
                        errno: result.observed_errno,
                    }
                };
            }
            if !result.consumed
                || result.code != libsqlite3_sys::SQLITE_OK
                || result.observed_errno != 0
            {
                return Err(anyhow::Error::new(OriginalWitnessRetirementFailure {
                    owner: self.clone(),
                    code: result.code,
                    consumed: result.consumed,
                    observed_errno: result.observed_errno,
                }));
            }
            Ok(RetiredOriginalWitnessFile {
                owner: self.clone(),
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (device, inode);
            anyhow::bail!("original witness retirement unavailable")
        }
    }
    pub fn original_retirement_receipt(
        self: &Arc<Self>,
    ) -> Result<Option<RetiredOriginalWitnessFile>> {
        self.require_original_process()?;
        Ok(
            if matches!(*self.original.state()?, DestinationRoleState::Retired) {
                Some(RetiredOriginalWitnessFile {
                    owner: self.clone(),
                })
            } else {
                None
            },
        )
    }
}
fn ensure_original_deadline(deadline: lillux::time::MonotonicDeadline) -> Result<()> {
    ensure!(
        !deadline.has_elapsed(),
        "original witness cleanup deadline expired"
    );
    Ok(())
}
#[derive(Clone)]
pub struct RemovedOriginalWitnessFile {
    owner: Arc<RetainedOriginalWitnessFile>,
    work: Arc<dyn OriginalWitnessCleanupWork>,
    parent: std::sync::Weak<PinnedDirectory>,
    deadline: lillux::time::MonotonicDeadline,
}
/// Genuine producer prefix only. Intermediate progress retains cleanup debt;
/// only the actual durable phase can accompany a removal receipt.
#[derive(Clone)]
pub struct OriginalWitnessRemovalProgress {
    owner: Arc<RetainedOriginalWitnessFile>,
    work: Arc<dyn OriginalWitnessCleanupWork>,
    parent: std::sync::Weak<PinnedDirectory>,
    deadline: lillux::time::MonotonicDeadline,
    phase: OriginalWitnessRemovalPhase,
}
impl OriginalWitnessRemovalProgress {
    pub fn is_original_owner(&self, owner: &Arc<RetainedOriginalWitnessFile>) -> Result<bool> {
        self.owner.require_original_process()?;
        owner.require_original_process()?;
        Ok(Arc::ptr_eq(owner, &self.owner))
    }
    pub fn require_original(
        &self,
        owner: &Arc<RetainedOriginalWitnessFile>,
        work: &Arc<dyn OriginalWitnessCleanupWork>,
    ) -> Result<()> {
        ensure!(
            self.is_original_owner(owner)? && Arc::ptr_eq(work, &self.work),
            "witness progress selects another original owner or work"
        );
        Ok(())
    }
    pub fn durable(&self) -> Result<bool> {
        self.owner.require_original_process()?;
        Ok(self.phase == OriginalWitnessRemovalPhase::RemovedDurable)
    }
    pub fn requires_original_coordinate(&self) -> Result<bool> {
        self.owner.require_original_process()?;
        Ok(self.phase == OriginalWitnessRemovalPhase::Original)
    }
}
impl RemovedOriginalWitnessFile {
    pub fn is_original_owner(&self, owner: &Arc<RetainedOriginalWitnessFile>) -> Result<bool> {
        self.owner.require_original_process()?;
        owner.require_original_process()?;
        Ok(Arc::ptr_eq(owner, &self.owner))
    }
    pub fn require_original(
        &self,
        owner: &Arc<RetainedOriginalWitnessFile>,
        work: &Arc<dyn OriginalWitnessCleanupWork>,
    ) -> Result<()> {
        owner.require_original_process()?;
        ensure!(
            Arc::ptr_eq(owner, &self.owner) && Arc::ptr_eq(work, &self.work),
            "durable witness removal selects another original owner or work"
        );
        Ok(())
    }
}
pub struct RetiredOriginalWitnessFile {
    owner: Arc<RetainedOriginalWitnessFile>,
}
impl RetiredOriginalWitnessFile {
    pub fn original_identity(&self) -> Result<(u64, u64)> {
        self.require_original(&self.owner)?;
        self.owner
            .original
            .identity
            .get()
            .copied()
            .context("actual retired witness identity absent")
    }
    pub fn require_original(&self, owner: &Arc<RetainedOriginalWitnessFile>) -> Result<()> {
        self.owner.require_original_process()?;
        owner.require_original_process()?;
        ensure!(
            Arc::ptr_eq(owner, &self.owner)
                && matches!(*self.owner.original.state()?, DestinationRoleState::Retired),
            "original witness lacks successful same-owner retirement"
        );
        Ok(())
    }
}
#[derive(Debug)]
pub struct OriginalWitnessRetirementFailure {
    owner: Arc<RetainedOriginalWitnessFile>,
    code: i32,
    consumed: bool,
    observed_errno: i32,
}
impl OriginalWitnessRetirementFailure {
    pub fn owner(&self) -> &Arc<RetainedOriginalWitnessFile> {
        &self.owner
    }
    pub fn code(&self) -> i32 {
        self.code
    }
    pub fn consumed(&self) -> bool {
        self.consumed
    }
    pub fn observed_errno(&self) -> i32 {
        self.observed_errno
    }
}
impl std::fmt::Display for OriginalWitnessRetirementFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "original witness close refused: code {}, consumed {}, errno {}",
            self.code, self.consumed, self.observed_errno
        )
    }
}
impl std::error::Error for OriginalWitnessRetirementFailure {}

/// Original aggregate SQL work, retained by the same inspection connection.
/// Bounds executable VM work; it does not imply total SQLite/native memory.
pub trait SqliteInspectionSqlWork: Send + Sync {
    /// # Safety
    /// SAME genuine sealed original controller and paid physical registry.
    /// Implementations move real sources and cache partial effects on refusal.
    unsafe fn register_original_outside_roots(
        &self,
        _factory: &Arc<OriginalNativeFactory>,
        _aggregate: &OriginalNativeTerminal,
        _leaf: &OriginalNativeOutsideLeafEvidence,
        _registry: &mut original_physical_custody::OriginalPhysicalRegistry,
        _prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> Result<()> {
        anyhow::bail!("SQL work has no original outside root transfer")
    }
    fn original_outside_roots_retired(&self) -> Result<bool> {
        Ok(false)
    }

    fn begin_native_production(
        &self,
        root: &PinnedDirectory,
        name: &OsStr,
    ) -> Result<SqliteInspectionProductionPermit>;

    fn require_copy_work(&self, copy_work: &Arc<dyn SqliteInspectionCopyWork>) -> Result<()>;
    fn require_destination(&self, destination: &PinnedDirectory) -> Result<()>;
    fn checkpoint(&self) -> Result<()>;
    fn debit_vm_steps(&self, steps: u64) -> Result<()>;
}

/// Install before SQL prepare/validation on a new inspection connection.
/// Interval one avoids uncharged sub-quantum statements. The original issuer
/// meters both stores together; connection recreation does not renew counters.
pub fn install_inspection_sql_work(
    connection: &Connection,
    destination: &PinnedDirectory,
    work: Arc<dyn SqliteInspectionSqlWork>,
) -> Result<()> {
    work.require_destination(destination)?;
    work.checkpoint()?;
    work.debit_vm_steps(1)?;
    connection.busy_timeout(std::time::Duration::ZERO)?;
    // The step hook catches a debit panic and interrupts the statement; a
    // panicking opcode is never run uncharged.
    connection.progress_handler(
        1,
        Some(native::native_step_hook(
            move || work.debit_vm_steps(1),
            None,
            None,
        )),
    );
    Ok(())
}

/// Copy an original no-follow regular child into an exclusively fresh child
/// of the caller's private inspection namespace. Source data descriptors stay
/// inside bounded native exclusion gates; only bytes cross into Rust. A fixed
/// buffer and one exact same-descriptor destination SHA pass bound body work.
/// Partial destination cleanup remains with the caller's original copy lease.
/// `maximum_bytes` bounds the measured extent; required `work` admission
/// separately binds it to the original caller's reservation. Neither issues
/// authority merely by describing an extent.
pub fn copy_regular_without_sqlite_alias(
    source: &PinnedDirectory,
    name: &OsStr,
    destination: &PinnedDirectory,
    maximum_bytes: u64,
    work: &dyn SqliteInspectionCopyWork,
) -> Result<Option<SqliteInspectionCopyReceipt>> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (source, name, destination, maximum_bytes, work);
        anyhow::bail!("guarded SQLite inspection copying is unavailable on this platform")
    }
    #[cfg(target_os = "linux")]
    {
        let production = work.begin_copy_production(source, destination, name)?;
        let copied = (|| -> Result<Option<SqliteInspectionCopyReceipt>> {
            production.checkpoint()?;
            use sha2::{Digest, Sha256};
            use std::os::unix::fs::MetadataExt;
            work.checkpoint()?;
            let Some(pin) = source.open_mount_entry(name)? else {
                work.admit_role(source, destination, name, None, 0, 0)?;
                work.checkpoint()?;
                return Ok(None);
            };
            let original = pin.metadata()?;
            ensure!(
                original.is_file(),
                "SQLite inspection source is not regular"
            );
            ensure!(
                original.len() <= maximum_bytes && original.len() <= i64::MAX as u64,
                "SQLite inspection source exceeds admitted copy length"
            );
            let maximum_read_bytes = original
                .len()
                .checked_mul(2)
                .and_then(|bytes| bytes.checked_add(1))
                .ok_or_else(|| anyhow::anyhow!("SQLite copy/hash work overflow"))?;
            work.admit_role(
                source,
                destination,
                name,
                Some(&SqliteRoleIdentity {
                    device: original.dev(),
                    inode: original.ino(),
                    byte_length: original.len(),
                    content_sha256: None,
                }),
                maximum_read_bytes,
                65_536,
            )?;
            work.checkpoint()?;
            ensure_copy_source_unchanged(source, name, &pin, &original)?;
            let mut buffer = [0u8; 65_536];
            // Audit aliases even for an empty source, before creating any output.
            let first = binding::read_unbound_regular_chunk(&pin, 0, original.len(), &mut buffer)?;
            ensure_copy_source_unchanged(source, name, &pin, &original)?;
            work.checkpoint()?;
            let output_root = destination.identity()?;
            let output_name = name.to_os_string();
            let output = Arc::new(RetainedSqliteDestinationRole::created(
                destination.open_regular_create(name, true, true, 0o600)?,
                output_root,
                output_name,
            ));
            work.retain_destination_role(destination, name, output.clone())?;
            let created = output.metadata_original()?;
            ensure!(
                created.is_file()
                    && created.len() == 0
                    && created.nlink() == 1
                    && (created.dev(), created.ino()) != (original.dev(), original.ino()),
                "SQLite inspection destination is not an exclusive fresh inode"
            );
            work.checkpoint()?;
            let mut offset = 0u64;
            while offset < first as u64 {
                // The first write follows the checkpoint above; additional short
                // writes each recheck the same original window.
                if offset > 0 {
                    work.checkpoint()?;
                }
                let written = output.write_chunk_at(offset, &buffer[offset as usize..first])?;
                ensure!(written > 0, "original output short write made no progress");
                offset += written as u64;
            }
            #[cfg(test)]
            COPY_AFTER_FIRST_CHUNK.with(|hook| {
                let callback = hook.borrow_mut().take();
                if let Some(callback) = callback {
                    callback();
                }
            });
            while offset < original.len() {
                work.checkpoint()?;
                let copied =
                    binding::read_unbound_regular_chunk(&pin, offset, original.len(), &mut buffer)?;
                ensure!(
                    copied > 0,
                    "SQLite inspection source ended before its admitted extent"
                );
                work.checkpoint()?;
                let mut written = 0usize;
                while written < copied {
                    work.checkpoint()?;
                    let count =
                        output.write_chunk_at(offset + written as u64, &buffer[written..copied])?;
                    ensure!(count > 0, "original output short write made no progress");
                    written += count;
                }
                offset = offset
                    .checked_add(copied as u64)
                    .ok_or_else(|| anyhow::anyhow!("SQLite inspection copy offset overflow"))?;
            }
            ensure!(
                offset == original.len(),
                "SQLite inspection copy length changed"
            );
            ensure_copy_source_unchanged(source, name, &pin, &original)?;
            work.checkpoint()?;
            output.sync_original()?;
            work.checkpoint()?;
            let written_snapshot = output.metadata_original()?;
            // Hash the SAME exclusively created output descriptor, never an
            // independently reopened source or destination observer descriptor.
            // One exact positional pass, not a retrying digest helper: reserve
            // source N + destination N + growth sentinel 1 before any body I/O.
            // Reuse the same fixed buffer and check the original window per read.
            let mut hash = Sha256::new();
            let mut hash_offset = 0u64;
            while hash_offset < original.len() {
                work.checkpoint()?;
                let wanted =
                    usize::try_from((original.len() - hash_offset).min(buffer.len() as u64))?;
                let read = output.read_hash_chunk_at(hash_offset, &mut buffer[..wanted])?;
                ensure!(
                    read > 0,
                    "SQLite inspection output ended before admitted extent"
                );
                hash.update(&buffer[..read]);
                hash_offset = hash_offset
                    .checked_add(read as u64)
                    .ok_or_else(|| anyhow::anyhow!("SQLite inspection hash offset overflow"))?;
            }
            work.checkpoint()?;
            ensure!(
                output.read_hash_chunk_at(hash_offset, &mut buffer[..1])? == 0,
                "SQLite inspection output grew beyond admitted extent"
            );
            let copied_sha256 = format!("{:x}", hash.finalize());
            let copied_metadata = output.metadata_original()?;
            work.checkpoint()?;
            ensure!(
                copied_metadata.dev() == created.dev()
                    && copied_metadata.ino() == created.ino()
                    && copied_metadata.nlink() == 1
                    && copied_metadata.len() == original.len(),
                "SQLite inspection destination identity changed"
            );
            output.require_copy_unchanged(destination, name, &written_snapshot)?;
            // Hashing is a bounded extra pass; source/name must remain stable
            // throughout that pass as well, not just while its chunks were read.
            ensure_copy_source_unchanged(source, name, &pin, &original)?;
            work.checkpoint()?;
            destination.sync()?;
            work.checkpoint()?;
            output.require_copy_unchanged(destination, name, &written_snapshot)?;
            ensure_copy_source_unchanged(source, name, &pin, &original)?;
            Ok(Some(SqliteInspectionCopyReceipt {
                source_identity: SqliteRoleIdentity {
                    device: original.dev(),
                    inode: original.ino(),
                    byte_length: original.len(),
                    content_sha256: Some(copied_sha256.clone()),
                },
                copied_byte_length: original.len(),
                copied_sha256,
            }))
        })();
        match copied {
            Ok(receipt) => match production.acknowledge_synchronous() {
                Ok(()) => Ok(receipt),
                Err(error) => Err(anyhow::Error::new(SqliteInspectionProductionFailure {
                    error,
                    _production: production,
                })),
            },
            Err(error) => Err(anyhow::Error::new(SqliteInspectionProductionFailure {
                error,
                _production: production,
            })),
        }
    }
}

/// Verify exact signed source facts without creating a new copy or exposing
/// a source data descriptor. A fixed buffer streams through the existing
/// native alias-exclusion gate. Expected extents are evidence, not a resource
/// grant: the caller must admit aggregate verification work before calling.
pub fn verify_regular_without_sqlite_alias(
    source: &PinnedDirectory,
    name: &OsStr,
    expected: Option<&SqliteRoleIdentity>,
) -> Result<()> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (source, name, expected);
        anyhow::bail!("guarded SQLite source verification is unavailable on this platform")
    }
    #[cfg(target_os = "linux")]
    {
        use sha2::{Digest, Sha256};
        use std::os::unix::fs::MetadataExt;
        let Some(expected) = expected else {
            ensure!(
                source.entry_no_follow(name)?.is_none(),
                "original absent SQLite role appeared"
            );
            return Ok(());
        };
        let digest = expected
            .content_sha256
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("original SQLite source lacks a digest"))?;
        ensure!(
            lillux::valid_hash(digest) && expected.byte_length <= i64::MAX as u64,
            "invalid original SQLite source digest or extent"
        );
        let pin = source
            .open_mount_entry(name)?
            .ok_or_else(|| anyhow::anyhow!("original SQLite source disappeared"))?;
        let original = pin.metadata()?;
        ensure!(
            original.is_file()
                && original.dev() == expected.device
                && original.ino() == expected.inode
                && original.len() == expected.byte_length,
            "original SQLite source identity or extent changed"
        );
        ensure_copy_source_unchanged(source, name, &pin, &original)?;
        let mut buffer = [0u8; 65_536];
        let mut offset = 0u64;
        let mut hash = Sha256::new();
        // The empty source still enters the native alias gate once.
        loop {
            let count = binding::read_unbound_regular_chunk(
                &pin,
                offset,
                expected.byte_length,
                &mut buffer,
            )?;
            ensure!(
                count > 0 || offset == expected.byte_length,
                "original SQLite source ended before its signed extent"
            );
            hash.update(&buffer[..count]);
            offset = offset
                .checked_add(count as u64)
                .ok_or_else(|| anyhow::anyhow!("original SQLite verification offset overflow"))?;
            ensure!(
                offset <= expected.byte_length,
                "original SQLite verification exceeded its extent"
            );
            #[cfg(test)]
            VERIFY_AFTER_FIRST_CHUNK.with(|hook| {
                let callback = hook.borrow_mut().take();
                if let Some(callback) = callback {
                    callback();
                }
            });
            if offset == expected.byte_length {
                break;
            }
        }
        ensure_copy_source_unchanged(source, name, &pin, &original)?;
        ensure!(
            format!("{:x}", hash.finalize()) == digest,
            "original SQLite source bytes differ from signed preview"
        );
        Ok(())
    }
}

#[cfg(all(test, target_os = "linux"))]
thread_local! {
    static VERIFY_AFTER_FIRST_CHUNK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(target_os = "linux")]
fn ensure_copy_source_unchanged(
    source: &PinnedDirectory,
    name: &OsStr,
    pin: &std::fs::File,
    expected: &std::fs::Metadata,
) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let current = pin.metadata()?;
    ensure!(
        current.is_file()
            && current.dev() == expected.dev()
            && current.ino() == expected.ino()
            && current.len() == expected.len()
            && current.mode() == expected.mode()
            && current.nlink() == expected.nlink()
            && current.mtime() == expected.mtime()
            && current.mtime_nsec() == expected.mtime_nsec()
            && current.ctime() == expected.ctime()
            && current.ctime_nsec() == expected.ctime_nsec(),
        "SQLite inspection source changed during copy"
    );
    ensure_guarded_read_name(source, name, pin)
}

/// Original disposable-copy lifetime owner, supplied by its producer. Native
/// custody retains this opaque owner through partial failure and abandonment.
pub type SqliteCopyLease = Arc<dyn Send + Sync>;
/// Original inspection context. Strong owners stay outside the original claim.
struct SqliteInspectionContext {
    root: Arc<PinnedDirectory>,
    database_name: std::ffi::OsString,
    lease: SqliteCopyLease,
    work: Arc<dyn SqliteInspectionSqlWork>,
    production: Option<SqliteInspectionProductionPermit>,
}
impl SqliteInspectionContext {
    fn require_original(
        &self,
        root: &PinnedDirectory,
        database_name: &OsStr,
        lease: &SqliteCopyLease,
        work: &Arc<dyn SqliteInspectionSqlWork>,
    ) -> Result<()> {
        self.root.ensure_path_binding()?;
        root.ensure_path_binding()?;
        ensure!(
            self.root.is_same_directory(root)?
                && self.database_name == database_name
                && Arc::ptr_eq(&self.lease, lease)
                && Arc::ptr_eq(&self.work, work),
            "inspection release selects another original root, basename, lease or SQL owner"
        );
        // Closure is not service work or permission to dispose the copy.
        Ok(())
    }
}

impl SqliteInspectionContext {
    fn authorize_body_disposal(
        &self,
        lease: &SqliteCopyLease,
        sql: &Arc<dyn SqliteInspectionSqlWork>,
        copy_work: Arc<dyn SqliteInspectionCopyWork>,
        roles: [Option<Arc<RetainedSqliteDestinationRole>>; 4],
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<Arc<SqliteInspectionBodyAuthority>> {
        self.require_original(&self.root, &self.database_name, lease, sql)?;
        self.work.require_copy_work(&copy_work)?;
        ensure!(
            !deadline.has_elapsed(),
            "original disposal deadline expired before authorization"
        );
        copy_work.cleanup_checkpoint()?;
        for (index, suffix) in ["", "-wal", "-shm", "-journal"].iter().enumerate() {
            let mut name = self.database_name.clone();
            name.push(suffix);
            copy_work.debit_cleanup_steps(1)?;
            let original = copy_work.retained_destination_role(&self.root, &name)?;
            match (&original, &roles[index]) {
                (Some(original), Some(role)) => {
                    ensure!(
                        Arc::ptr_eq(original, role),
                        "body authorization substitutes original role owner"
                    );
                    role.require_original(&self.root, &name)?;
                }
                (None, None) => {}
                _ => anyhow::bail!("body authorization changes original role presence"),
            }
        }
        Ok(Arc::new(SqliteInspectionBodyAuthority {
            root: self.root.clone(),
            database_name: self.database_name.clone(),
            lease: lease.clone(),
            sql: sql.clone(),
            copy_work,
            roles,
            deadline,
        }))
    }
    fn authorize_disposal(
        &self,
        removed: Arc<lillux::secure_fs::RemovedEmptyChild>,
        body: &Arc<SqliteInspectionBodyAuthority>,
    ) -> Result<Arc<SqliteInspectionDisposalAuthority>> {
        // The original root is deliberately removed; only retained identity
        // and the ACTUAL native removal receipt participate in this join.
        ensure!(
            self.root.is_same_directory(&body.root)?
                && self.root.is_same_directory(removed.child())?
                && self.database_name == body.database_name
                && Arc::ptr_eq(&self.lease, &body.lease)
                && Arc::ptr_eq(&self.work, &body.sql),
            "disposal receipt changes original native terminal context"
        );
        ensure!(
            self.root.path().parent() == Some(removed.parent().path())
                && self.root.path().file_name() == Some(removed.name()),
            "disposal receipt changes original parent/leaf"
        );
        removed.parent().ensure_path_binding()?;
        ensure!(
            removed.parent().entry_no_follow(removed.name())?.is_none(),
            "removed original root coordinate reappeared"
        );
        ensure!(
            !body.deadline.has_elapsed(),
            "original disposal deadline expired before role retirement"
        );
        body.copy_work.cleanup_checkpoint()?;
        Ok(Arc::new(SqliteInspectionDisposalAuthority {
            body: body.clone(),
            _removed: removed,
        }))
    }
}

pub struct ClosedSqliteInspection {
    context: SqliteInspectionContext,
}
impl ClosedSqliteInspection {
    pub fn acknowledge_producer_terminal(&self) -> Result<()> {
        if let Some(production) = &self.context.production {
            production.acknowledge_native_terminal()?;
        }
        Ok(())
    }
    pub fn authorize_body_disposal(
        &self,
        lease: &SqliteCopyLease,
        sql: &Arc<dyn SqliteInspectionSqlWork>,
        copy_work: Arc<dyn SqliteInspectionCopyWork>,
        roles: [Option<Arc<RetainedSqliteDestinationRole>>; 4],
        first_deadline: lillux::time::MonotonicDeadline,
    ) -> Result<Arc<SqliteInspectionBodyAuthority>> {
        self.context
            .authorize_body_disposal(lease, sql, copy_work, roles, first_deadline)
    }
    pub fn authorize_disposal(
        &self,
        removed: Arc<lillux::secure_fs::RemovedEmptyChild>,
        body: &Arc<SqliteInspectionBodyAuthority>,
    ) -> Result<Arc<SqliteInspectionDisposalAuthority>> {
        self.context.authorize_disposal(removed, body)
    }
    /// Borrowed validation preserves this proof on refusal. No cleanup grant.
    pub fn require_original(
        &self,
        root: &PinnedDirectory,
        database_name: &OsStr,
        lease: &SqliteCopyLease,
        work: &Arc<dyn SqliteInspectionSqlWork>,
    ) -> Result<()> {
        self.context
            .require_original(root, database_name, lease, work)
    }
}
impl std::fmt::Debug for ClosedSqliteInspection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClosedSqliteInspection")
            .finish_non_exhaustive()
    }
}

/// Explicit release after failed acquisition, not a completed inspection.
pub struct ReleasedSqliteInspection {
    context: SqliteInspectionContext,
}
impl ReleasedSqliteInspection {
    pub fn acknowledge_producer_terminal(&self) -> Result<()> {
        if let Some(production) = &self.context.production {
            production.acknowledge_native_terminal()?;
        }
        Ok(())
    }
    pub fn authorize_body_disposal(
        &self,
        lease: &SqliteCopyLease,
        sql: &Arc<dyn SqliteInspectionSqlWork>,
        copy_work: Arc<dyn SqliteInspectionCopyWork>,
        roles: [Option<Arc<RetainedSqliteDestinationRole>>; 4],
        first_deadline: lillux::time::MonotonicDeadline,
    ) -> Result<Arc<SqliteInspectionBodyAuthority>> {
        self.context
            .authorize_body_disposal(lease, sql, copy_work, roles, first_deadline)
    }
    pub fn authorize_disposal(
        &self,
        removed: Arc<lillux::secure_fs::RemovedEmptyChild>,
        body: &Arc<SqliteInspectionBodyAuthority>,
    ) -> Result<Arc<SqliteInspectionDisposalAuthority>> {
        self.context.authorize_disposal(removed, body)
    }
    pub fn require_original(
        &self,
        root: &PinnedDirectory,
        database_name: &OsStr,
        lease: &SqliteCopyLease,
        work: &Arc<dyn SqliteInspectionSqlWork>,
    ) -> Result<()> {
        self.context
            .require_original(root, database_name, lease, work)
    }
}
impl std::fmt::Debug for ReleasedSqliteInspection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReleasedSqliteInspection")
            .finish_non_exhaustive()
    }
}

/// Failed readonly acquisition retains its original context and native owner.
pub struct ReadonlySqliteOpenFailure {
    native: BoundSqliteOpenFailure,
    context: SqliteInspectionContext,
}
impl ReadonlySqliteOpenFailure {
    pub fn error(&self) -> &anyhow::Error {
        self.native.error()
    }
    pub fn owner(&self) -> Option<&BoundSqliteCloseOwner> {
        self.native.owner()
    }
    pub fn close_error(&self) -> Option<&BoundSqliteCloseError> {
        self.native.close_error()
    }
    pub fn retry_close(self) -> std::result::Result<ReleasedSqliteInspection, Self> {
        let Self { native, context } = self;
        match native.retry_close() {
            Ok(()) => Ok(ReleasedSqliteInspection { context }),
            Err(native) => Err(Self { native, context }),
        }
    }
}
impl std::fmt::Debug for ReadonlySqliteOpenFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadonlySqliteOpenFailure")
            .field("native", &self.native)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Display for ReadonlySqliteOpenFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.native, f)
    }
}
impl std::error::Error for ReadonlySqliteOpenFailure {}

/// Failed close retains the SAME live/retiring native owner and context.
pub struct ReadonlySqliteCloseFailure {
    native: BoundSqliteCloseFailure,
    context: SqliteInspectionContext,
}
impl ReadonlySqliteCloseFailure {
    pub fn error(&self) -> &BoundSqliteCloseError {
        self.native.error()
    }
    pub fn owner(&self) -> &BoundSqliteCloseOwner {
        self.native.owner()
    }
    pub fn retry_close(self) -> std::result::Result<ClosedSqliteInspection, Self> {
        let Self { native, context } = self;
        match native.retry() {
            Ok(()) => Ok(ClosedSqliteInspection { context }),
            Err(native) => Err(Self { native, context }),
        }
    }
}
impl std::fmt::Debug for ReadonlySqliteCloseFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadonlySqliteCloseFailure")
            .field("native", &self.native)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Display for ReadonlySqliteCloseFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "readonly inspection retains native close owner: {:?}",
            self.native.error()
        )
    }
}
impl std::error::Error for ReadonlySqliteCloseFailure {}

/// Existing readonly disposable copy; no main creation or schema migration.
pub struct ReadonlySqliteCopy {
    owner: BoundSqliteConnection,
    context: SqliteInspectionContext,
}
impl ReadonlySqliteCopy {
    pub fn open(
        runtime: Arc<PinnedDirectory>,
        database_name: &OsStr,
        lease: SqliteCopyLease,
        work: Arc<dyn SqliteInspectionSqlWork>,
    ) -> std::result::Result<Self, ReadonlySqliteOpenFailure> {
        let mut context = SqliteInspectionContext {
            root: runtime.clone(),
            database_name: database_name.to_os_string(),
            lease: lease.clone(),
            work: work.clone(),
            production: None,
        };
        match work.begin_native_production(&runtime, database_name) {
            Ok(production) => context.production = Some(production),
            Err(error) => {
                return Err(ReadonlySqliteOpenFailure {
                    native: BoundSqliteOpenFailure {
                        error,
                        owner: None,
                        close_error: None,
                    },
                    context,
                });
            }
        }
        match BoundSqliteConnection::acquire(
            runtime,
            database_name,
            3,
            Some(lease),
            Some(work),
            context.production.as_ref(),
        ) {
            Ok(owner) => Ok(Self { owner, context }),
            Err(native) => Err(ReadonlySqliteOpenFailure { native, context }),
        }
    }
    pub fn connection(&self) -> &Connection {
        self.owner.connection()
    }
    pub fn close(self) -> std::result::Result<ClosedSqliteInspection, ReadonlySqliteCloseFailure> {
        let Self { owner, context } = self;
        match owner.close() {
            Ok(()) => Ok(ClosedSqliteInspection { context }),
            Err(native) => Err(ReadonlySqliteCloseFailure { native, context }),
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum SqliteRole {
    Main = 0,
    Wal = 1,
    Shm = 2,
}
impl SqliteRole {
    fn all() -> [Self; 3] {
        [Self::Main, Self::Wal, Self::Shm]
    }
    fn label(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Wal => "wal",
            Self::Shm => "shm",
        }
    }
    fn public_name(self) -> &'static str {
        match self {
            Self::Main => "scheduler.sqlite3",
            Self::Wal => "scheduler.sqlite3-wal",
            Self::Shm => "scheduler.sqlite3-shm",
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqliteRoleIdentity {
    pub device: u64,
    pub inode: u64,
    pub byte_length: u64,
    pub content_sha256: Option<String>,
}
#[derive(Debug, Clone)]
pub struct SqliteRoleFacts {
    pub role: SqliteRole,
    pub original: Option<SqliteRoleIdentity>,
    pub quarantine_name: String,
    pub purge_name: String,
}
#[derive(Debug, Clone)]
pub struct CapturedSqliteRoleFacts {
    pub generation: [u8; 16],
    pub roles: [SqliteRoleFacts; 3],
}
pub type DrainedSqliteRoleFacts = CapturedSqliteRoleFacts;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqliteRoleMutationOutcome {
    OriginallyAbsent,
    QuarantinedDurable,
    RemovedDurable,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqliteRoleMutationError {
    pub role: SqliteRole,
    pub disposition: lillux::secure_fs::PlannedQuarantineDisposition,
    pub stage: lillux::secure_fs::PlannedQuarantineStage,
    pub message: String,
}
impl std::fmt::Display for SqliteRoleMutationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:?}/{:?}/{:?}: {}",
            self.role, self.disposition, self.stage, self.message
        )
    }
}
impl std::error::Error for SqliteRoleMutationError {}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoppedSqliteRoleSource {
    Absent,
    Canonical,
    Quarantine { generation: [u8; 16] },
    Purge { generation: [u8; 16] },
    Resolution { generation: [u8; 16], ordinal: u8 },
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoppedSqliteRolePlan {
    pub source: StoppedSqliteRoleSource,
    pub current: Option<SqliteRoleIdentity>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoppedSqliteProjectionPlan {
    pub generation: [u8; 16],
    pub generations: Vec<[u8; 16]>,
    pub runtime_identity: lillux::secure_fs::PinnedDirectoryIdentity,
    pub roles: [StoppedSqliteRolePlan; 3],
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoppedSqliteCoordinateObservation {
    pub role: Option<SqliteRole>, // None is a derived main rollback journal
    pub name: String,
    pub observation: SqliteRoleCoordinateObservation,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoppedSqliteProjectionObservation {
    pub plan: StoppedSqliteProjectionPlan,
    pub ready: bool,
    pub roles: [Option<SqliteRoleIdentity>; 3],
    pub original_link_counts: [Option<u64>; 3],
    pub coordinates: Vec<StoppedSqliteCoordinateObservation>,
}
fn generation_hex(generation: [u8; 16]) -> String {
    generation
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
impl StoppedSqliteProjectionPlan {
    fn source_name(&self, role: SqliteRole) -> Option<String> {
        let label = role.label();
        Some(match self.roles[role as usize].source {
            StoppedSqliteRoleSource::Absent => return None,
            StoppedSqliteRoleSource::Canonical => role.public_name().into(),
            StoppedSqliteRoleSource::Quarantine { generation } => format!(
                "scheduler-retirement.role.{}.{label}.quarantine",
                generation_hex(generation)
            ),
            StoppedSqliteRoleSource::Purge { generation } => format!(
                "scheduler-retirement.role.{}.{label}.purge",
                generation_hex(generation)
            ),
            StoppedSqliteRoleSource::Resolution {
                generation,
                ordinal,
            } => format!(
                "scheduler-retirement.resolve.{}.{label}.{ordinal:02}.quarantine",
                generation_hex(generation)
            ),
        })
    }
    fn coordinates(&self) -> Vec<(Option<SqliteRole>, String)> {
        let mut coordinates = Vec::with_capacity(100);
        for role in SqliteRole::all() {
            coordinates.push((Some(role), role.public_name().into()));
            for generation in &self.generations {
                let generation = generation_hex(*generation);
                for suffix in ["quarantine", "purge"] {
                    coordinates.push((
                        Some(role),
                        format!(
                            "scheduler-retirement.role.{generation}.{}.{suffix}",
                            role.label()
                        ),
                    ));
                }
                for ordinal in 0..SQLITE_ROLE_RESOLUTION_LIMIT {
                    coordinates.push((
                        Some(role),
                        format!(
                            "scheduler-retirement.resolve.{generation}.{}.{ordinal:02}.quarantine",
                            role.label()
                        ),
                    ));
                }
            }
        }
        let journals = coordinates
            .iter()
            .filter(|(role, _)| *role == Some(SqliteRole::Main))
            .map(|(_, name)| (None, format!("{name}-journal")))
            .collect::<Vec<_>>();
        coordinates.extend(journals);
        coordinates
    }
    fn validate(&self, runtime: &PinnedDirectory) -> Result<()> {
        ensure!(
            runtime.identity()? == self.runtime_identity,
            "stopped intake runtime identity mismatch"
        );
        ensure!(
            !self.generations.is_empty()
                && self.generations.len() <= 4
                && self.generations.windows(2).all(|pair| pair[0] < pair[1])
                && self.generations.contains(&self.generation),
            "stopped intake generations must be sorted unique 1..4 including new generation"
        );
        for role in SqliteRole::all() {
            let plan = &self.roles[role as usize];
            match plan.source {
                StoppedSqliteRoleSource::Absent => {
                    ensure!(plan.current.is_none(), "absent role has current identity")
                }
                _ => {
                    let current = plan
                        .current
                        .as_ref()
                        .ok_or_else(|| anyhow::anyhow!("selected current role lacks identity"))?;
                    let digest = current.content_sha256.as_deref().ok_or_else(|| {
                        anyhow::anyhow!("selected current role lacks authorized digest")
                    })?;
                    ensure!(
                        digest.len() == 64
                            && digest
                                .bytes()
                                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
                        "current role digest is not lowercase SHA256"
                    );
                }
            }
            let old = match plan.source {
                StoppedSqliteRoleSource::Quarantine { generation }
                | StoppedSqliteRoleSource::Purge { generation } => Some(generation),
                StoppedSqliteRoleSource::Resolution {
                    generation,
                    ordinal,
                } => {
                    ensure!(
                        (ordinal as usize) < SQLITE_ROLE_RESOLUTION_LIMIT,
                        "stopped source ordinal out of bounds"
                    );
                    Some(generation)
                }
                _ => None,
            };
            if let Some(old) = old {
                ensure!(
                    old != self.generation && self.generations.contains(&old),
                    "stopped source must reference listed old generation"
                );
            }
        }
        for i in 0..3 {
            for j in i + 1..3 {
                if let (Some(left), Some(right)) = (&self.roles[i].current, &self.roles[j].current)
                {
                    ensure!(
                        left.device != right.device || left.inode != right.inode,
                        "stopped role sources alias one physical object"
                    );
                }
            }
        }
        Ok(())
    }
}
/// Copied observations only; these values grant no mutation authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqliteRoleCoordinateObservation {
    Absent,
    SameOriginal { device: u64, inode: u64 },
    ForeignRegular { device: u64, inode: u64 },
    NonRegular(lillux::secure_fs::PinnedEntryType),
    Unreadable(String),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqliteRoleMutationOperation {
    Quarantine,
    Purge,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqliteRoleMutationFailureObservation {
    pub operation: SqliteRoleMutationOperation,
    pub error: SqliteRoleMutationError,
    pub substep_observed: bool,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqliteRoleResolutionObservation {
    pub generation: [u8; 16],
    pub role: SqliteRole,
    pub runtime_identity: lillux::secure_fs::PinnedDirectoryIdentity,
    pub epoch: u64,
    pub barriers_held: bool,
    pub recovery_required: bool,
    pub original: Option<SqliteRoleIdentity>,
    pub original_link_count: Option<u64>,
    pub public: SqliteRoleCoordinateObservation,
    pub source: SqliteRoleCoordinateObservation,
    pub selected_source: Option<StoppedSqliteRoleSource>,
    pub stopped_coordinates: Vec<StoppedSqliteCoordinateObservation>,
    pub stopped_plan: Option<StoppedSqliteProjectionPlan>,
    pub quarantine: SqliteRoleCoordinateObservation,
    pub purge: SqliteRoleCoordinateObservation,
    pub quarantine_name: String,
    pub purge_name: String,
    pub original_failure: Option<SqliteRoleMutationFailureObservation>,
    pub latest_failure: Option<SqliteRoleMutationFailureObservation>,
    pub resolutions: Vec<SqliteRoleResolutionAttempt>,
    pub resolution_coordinates: Vec<SqliteRoleResolutionCoordinate>,
}
#[cfg(test)]
thread_local! {
    static FAIL_RESOLUTION_SYNC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static FAIL_RESOLUTION_AFTER_MOVE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static FAIL_RESOLUTION_AFTER_UNLINK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
pub const SQLITE_ROLE_RESOLUTION_LIMIT: usize = 4;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqliteRoleResolutionAction {
    SyncQuarantine,
    SyncRemoved,
    RearmOriginal,
    RearmQuarantined,
    FinishPurgeAtU,
    FinishPurgeAtResolution { source_ordinal: u8 },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqliteRoleResolutionOutcome {
    QuarantinedDurable,
    RemovedDurable,
    RearmedOriginal,
    RearmedQuarantined,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqliteRoleResolutionRequest {
    pub expected: SqliteRoleResolutionObservation,
    pub ordinal: u8,
    pub action: SqliteRoleResolutionAction,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqliteRoleResolutionCoordinate {
    pub ordinal: u8,
    pub name: String,
    pub observation: SqliteRoleCoordinateObservation,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqliteRoleResolutionAttempt {
    pub ordinal: u8,
    pub action: SqliteRoleResolutionAction,
    pub outcome: Option<SqliteRoleResolutionOutcome>,
    pub effect_failure: Option<SqliteRoleMutationError>,
    pub substep: Option<lillux::secure_fs::PlannedQuarantineStage>,
    pub original_unlink_observed: bool,
    pub error: Option<String>,
}
#[derive(Debug, Clone)]
pub enum SqliteRoleResolutionError {
    Refused(String),
    BudgetExhausted,
    Held(SqliteRoleResolutionAttempt),
}
impl std::fmt::Display for SqliteRoleResolutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for SqliteRoleResolutionError {}
#[derive(Clone)]
struct ResolutionMemo {
    request: SqliteRoleResolutionRequest,
    attempt: SqliteRoleResolutionAttempt,
}
#[derive(Debug, Clone, Default)]
struct RoleFailureHistory {
    original: Option<SqliteRoleMutationFailureObservation>,
    latest: Option<SqliteRoleMutationFailureObservation>,
}
#[derive(Debug)]
pub enum SqliteProjectionRetirementError {
    Capture(anyhow::Error),
    Connection(rusqlite::Error),
    Barrier(anyhow::Error),
    Snapshot(anyhow::Error),
    IncompleteRoles,
    NativeRelease(BoundSqliteCloseError),
}
pub struct CapturedSqliteProjection {
    owner: BoundSqliteConnection,
    facts: CapturedSqliteRoleFacts,
}
#[derive(Clone, Copy)]
enum RetirementRoleState {
    Absent,
    Original,
    Quarantined,
    Removed,
    RecoveryRequired,
}
pub struct DrainedSqliteProjection {
    binding: Option<Binding>,
    runtime: Arc<PinnedDirectory>,
    facts: DrainedSqliteRoleFacts,
    barriers: bool,
    states: [RetirementRoleState; 3],
    last_error: Option<SqliteRoleMutationError>,
    role_failures: [RoleFailureHistory; 3],
    observation_epoch: u64,
    resolutions: [Vec<ResolutionMemo>; 3],
    stopped_plan: Option<StoppedSqliteProjectionPlan>,
    stopped_ready: bool,
}
impl std::fmt::Debug for DrainedSqliteProjection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DrainedSqliteProjection")
            .field("generation", &self.facts.generation)
            .field("barriers", &self.barriers)
            .field("stopped_ready", &self.stopped_ready)
            .finish_non_exhaustive()
    }
}
pub struct PendingCapturedSqliteProjection {
    owner: BoundSqliteConnection,
    generation: [u8; 16],
}
pub struct CleanupSqliteProjection {
    binding: Option<Binding>,
    runtime: Arc<PinnedDirectory>,
    generation: [u8; 16],
}
pub enum SqliteProjectionRetirementOwner {
    UnacquiredStopped {
        runtime: Arc<PinnedDirectory>,
        plan: StoppedSqliteProjectionPlan,
    },
    Uncaptured(BoundSqliteConnection),
    CapturePending(PendingCapturedSqliteProjection),
    Cleanup(CleanupSqliteProjection),
    Captured(CapturedSqliteProjection),
    Drained(DrainedSqliteProjection),
    Releasing(BoundSqliteCloseOwner),
}
pub struct SqliteProjectionRetirementFailure {
    error: SqliteProjectionRetirementError,
    owner: SqliteProjectionRetirementOwner,
}
impl std::fmt::Debug for SqliteProjectionRetirementFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteProjectionRetirementFailure")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Display for SqliteProjectionRetirementFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "same original projection owner retained: {:?}",
            self.error
        )
    }
}
impl std::error::Error for SqliteProjectionRetirementFailure {}
impl SqliteProjectionRetirementFailure {
    // Exact private physical-disposition seam for an enrolled live native
    // ticket. Stopped/reconstructed source owners cannot clear that ticket.
    fn original_live_sql_consumed(&self) -> Result<bool> {
        fn live(owner: &BoundSqliteConnection) -> Result<bool> {
            ensure!(
                owner.connection.is_some(),
                "contradictory live retirement SQL custody"
            );
            Ok(false)
        }
        match &self.owner {
            SqliteProjectionRetirementOwner::Uncaptured(owner) => live(owner),
            SqliteProjectionRetirementOwner::CapturePending(owner) => live(&owner.owner),
            SqliteProjectionRetirementOwner::Captured(owner) => live(&owner.owner),
            SqliteProjectionRetirementOwner::Cleanup(_) => Ok(true),
            SqliteProjectionRetirementOwner::Drained(owner) => {
                ensure!(
                    owner.stopped_plan.is_none(),
                    "stopped owner cannot clear live SQL ticket"
                );
                Ok(true)
            }
            SqliteProjectionRetirementOwner::Releasing(BoundSqliteCloseOwner::Live(owner)) => {
                live(owner)
            }
            SqliteProjectionRetirementOwner::Releasing(BoundSqliteCloseOwner::Retiring(_)) => {
                Ok(true)
            }
            SqliteProjectionRetirementOwner::UnacquiredStopped { .. } => {
                anyhow::bail!("unacquired stopped owner cannot clear live SQL ticket")
            }
        }
    }
    pub fn error(&self) -> &SqliteProjectionRetirementError {
        &self.error
    }
    pub fn owner(&self) -> &SqliteProjectionRetirementOwner {
        &self.owner
    }
    pub fn retry_stopped_projection(
        self,
        plan: &StoppedSqliteProjectionPlan,
    ) -> std::result::Result<DrainedSqliteProjection, Self> {
        match self.owner {
            SqliteProjectionRetirementOwner::UnacquiredStopped {
                runtime,
                plan: expected,
            } if expected == *plan => {
                DrainedSqliteProjection::capture_stopped_projection(runtime, expected)
            }
            SqliteProjectionRetirementOwner::Drained(owner)
                if owner.stopped_plan.as_ref() == Some(plan) =>
            {
                owner.finish_stopped()
            }
            _ => Err(self),
        }
    }
    pub fn retry_drain(self) -> std::result::Result<DrainedSqliteProjection, Self> {
        match self.owner {
            SqliteProjectionRetirementOwner::Captured(owner) => owner.drain_sql(),
            SqliteProjectionRetirementOwner::Drained(owner) if owner.stopped_plan.is_some() => {
                owner.finish_stopped()
            }
            SqliteProjectionRetirementOwner::Drained(owner) => owner.finish_barriers(),
            _ => Err(self),
        }
    }
    pub fn retry_release(self) -> std::result::Result<(), Self> {
        match self.owner {
            SqliteProjectionRetirementOwner::Releasing(owner) => owner.retry().map_err(|failure| {
                let BoundSqliteCloseFailure { owner, error } = failure;
                Self {
                    owner: SqliteProjectionRetirementOwner::Releasing(owner),
                    error: SqliteProjectionRetirementError::NativeRelease(error),
                }
            }),
            SqliteProjectionRetirementOwner::Drained(owner) => owner.release(),
            _ => Err(self),
        }
    }
    pub fn retry_capture(
        self,
        generation: [u8; 16],
    ) -> std::result::Result<CapturedSqliteProjection, Self> {
        match self.owner {
            SqliteProjectionRetirementOwner::Uncaptured(owner) => {
                owner.capture_projection_retirement(generation)
            }
            SqliteProjectionRetirementOwner::CapturePending(owner)
                if owner.generation == generation =>
            {
                owner.finish_snapshot()
            }
            _ => Err(self),
        }
    }
}
#[cfg(test)]
thread_local! {
    static FAIL_CAPTURE_OBSERVATION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static FAIL_STOPPED_AFTER_ACQUIRE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
fn snapshot_role_facts(
    binding: &Binding,
    generation: [u8; 16],
    digest: bool,
) -> Result<CapturedSqliteRoleFacts> {
    #[cfg(test)]
    if !digest && FAIL_CAPTURE_OBSERVATION.with(|flag| flag.replace(false)) {
        anyhow::bail!("injected post-capture descriptor observation failure");
    }
    let generation_hex: String = generation.iter().map(|b| format!("{b:02x}")).collect();
    let mut roles = Vec::with_capacity(3);
    for role in SqliteRole::all() {
        let original = binding.with_retirement_role(role as usize, |file| {
            let Some(file) = file else {
                return Ok(None);
            };
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                let metadata = file.metadata()?;
                ensure!(metadata.is_file(), "retained SQLite role is not regular");
                let (content_sha256, metadata) = if digest {
                    let (hash, after) =
                        lillux::digest_open_regular_file_stable_exact(file, metadata.len())?;
                    (Some(hash), after)
                } else {
                    (None, metadata)
                };
                Ok(Some(SqliteRoleIdentity {
                    device: metadata.dev(),
                    inode: metadata.ino(),
                    byte_length: metadata.len(),
                    content_sha256,
                }))
            }
            #[cfg(not(unix))]
            anyhow::bail!("SQLite retirement observations unavailable")
        })?;
        roles.push(SqliteRoleFacts {
            role,
            original,
            quarantine_name: format!(
                "scheduler-retirement.role.{generation_hex}.{}.quarantine",
                role.label()
            ),
            purge_name: format!(
                "scheduler-retirement.role.{generation_hex}.{}.purge",
                role.label()
            ),
        });
    }
    Ok(CapturedSqliteRoleFacts {
        generation,
        roles: roles.try_into().expect("three fixed roles"),
    })
}
impl PendingCapturedSqliteProjection {
    fn finish_snapshot(
        self,
    ) -> std::result::Result<CapturedSqliteProjection, SqliteProjectionRetirementFailure> {
        match snapshot_role_facts(
            self.owner.binding.as_ref().expect("same captured owner"),
            self.generation,
            false,
        ) {
            Ok(facts) => Ok(CapturedSqliteProjection {
                owner: self.owner,
                facts,
            }),
            Err(error) => Err(SqliteProjectionRetirementFailure {
                error: SqliteProjectionRetirementError::Snapshot(error),
                owner: SqliteProjectionRetirementOwner::CapturePending(self),
            }),
        }
    }
}
impl CapturedSqliteProjection {
    pub fn captured_roles(&self) -> &CapturedSqliteRoleFacts {
        &self.facts
    }
    /// Caller has durably published immutable intent before invoking this.
    pub fn drain_sql(
        mut self,
    ) -> std::result::Result<DrainedSqliteProjection, SqliteProjectionRetirementFailure> {
        let connection = self
            .owner
            .connection
            .take()
            .expect("captured live SQL owner");
        if let Err((connection, error)) = connection.close() {
            self.owner.connection = Some(connection);
            return Err(SqliteProjectionRetirementFailure {
                error: SqliteProjectionRetirementError::Connection(error),
                owner: SqliteProjectionRetirementOwner::Captured(self),
            });
        }
        let states = std::array::from_fn(|i| {
            if self.facts.roles[i].original.is_some() {
                RetirementRoleState::Original
            } else {
                RetirementRoleState::Absent
            }
        });
        DrainedSqliteProjection {
            binding: self.owner.binding.take(),
            runtime: self.owner.runtime.clone(),
            facts: self.facts,
            barriers: false,
            states,
            last_error: None,
            role_failures: std::array::from_fn(|_| RoleFailureHistory::default()),
            observation_epoch: 0,
            resolutions: std::array::from_fn(|_| Vec::new()),
            stopped_plan: None,
            stopped_ready: false,
        }
        .finish_barriers()
    }
}
fn scan_stopped_artifacts(
    runtime: &PinnedDirectory,
    plan: &StoppedSqliteProjectionPlan,
) -> Result<()> {
    let coordinates = plan.coordinates();
    for name in runtime.entry_names_bounded(4096)? {
        let name = name
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("non-UTF8 stopped namespace entry"))?;
        match lillux::classify_secure_temp_name(OsStr::new(name), crate::SQLITE_TEMP_PURPOSE) {
            lillux::SecureTempOwnership::NotTemp => {}
            lillux::SecureTempOwnership::Own => {
                anyhow::bail!("stopped namespace temporary artifact")
            }
            // The stopped namespace is shared; another owner's interrupted
            // publication is that owner's to settle.
            lillux::SecureTempOwnership::Foreign => continue,
            lillux::SecureTempOwnership::Malformed => anyhow::bail!(
                "stopped namespace holds a secure temporary without an exact owner token: {name}; \
                 remove that entry while its owner is stopped"
            ),
        }
        ensure!(
            !name.starts_with("etilqs_"),
            "stopped namespace temporary artifact"
        );
        if coordinates.iter().any(|(_, coordinate)| coordinate == name) {
            continue;
        }
        if name.starts_with("scheduler.sqlite3-") {
            anyhow::bail!("untracked scheduler SQLite artifact");
        }
        if name.starts_with("scheduler-retirement.") {
            let inert_record = name == "scheduler-retirement.active.json" || plan.generations.iter().any(|generation| {
                let generation = generation_hex(*generation);
                name == format!("scheduler-retirement.intent.{generation}")
                    || (0..32).any(|sequence| name == format!("scheduler-retirement.phase.{generation}.{sequence:02}"))
                    || SqliteRole::all().iter().any(|role| (0..4).any(|ordinal| ["intent", "outcome"].iter().any(|suffix|
                        name == format!("scheduler-retirement.resolution.{generation}.{}.{ordinal:02}.{suffix}", role.label()))))
            });
            // Signed evidence is authenticated by B; lexical allowance is not authority.
            ensure!(inert_record, "unknown reserved stopped namespace artifact");
        }
    }
    #[cfg(unix)]
    for (_, name) in coordinates.iter().filter(|(role, _)| role.is_none()) {
        ensure!(
            runtime.entry_no_follow(OsStr::new(name))?.is_none(),
            "stopped rollback journal present"
        );
    }
    Ok(())
}
impl DrainedSqliteProjection {
    /// No SQL open. Domain has durably signed NEW current-object intake first.
    pub fn capture_stopped_projection(
        runtime: Arc<PinnedDirectory>,
        plan: StoppedSqliteProjectionPlan,
    ) -> std::result::Result<Self, SqliteProjectionRetirementFailure> {
        let preparation = (|| -> Result<_> {
            plan.validate(&runtime)?;
            scan_stopped_artifacts(&runtime, &plan)?;
            #[cfg(target_os = "linux")]
            {
                let root = std::fs::File::open(runtime.descriptor_path()?)?;
                let coordinates = plan.coordinates();
                let names = coordinates
                    .iter()
                    .map(|(_, name)| name.clone())
                    .collect::<Vec<_>>();
                let selected = std::array::from_fn(|i| {
                    plan.source_name(SqliteRole::all()[i]).map_or(-1, |source| {
                        names
                            .iter()
                            .position(|name| *name == source)
                            .expect("derived source") as i32
                    })
                });
                let expected = std::array::from_fn(|i| plan.roles[i].current.clone());
                Ok(Binding::create_stopped(&root, &names, selected, &expected))
            }
            #[cfg(not(target_os = "linux"))]
            anyhow::bail!("stopped SQLite intake requires Linux");
        })();
        let result = match preparation {
            Ok(result) => result,
            Err(error) => {
                return Err(SqliteProjectionRetirementFailure {
                    error: SqliteProjectionRetirementError::Capture(error),
                    owner: SqliteProjectionRetirementOwner::UnacquiredStopped { runtime, plan },
                });
            }
        };
        let (binding, allocation_error) = match result {
            Ok(binding) => (binding, None),
            Err((error, Some(binding))) => (binding, Some(error)),
            Err((error, None)) => {
                return Err(SqliteProjectionRetirementFailure {
                    error: SqliteProjectionRetirementError::Capture(error),
                    owner: SqliteProjectionRetirementOwner::UnacquiredStopped { runtime, plan },
                });
            }
        };
        let generation = plan.generation;
        let facts = CapturedSqliteRoleFacts {
            generation,
            roles: std::array::from_fn(|i| {
                let role = SqliteRole::all()[i];
                SqliteRoleFacts {
                    role,
                    original: None,
                    quarantine_name: format!(
                        "scheduler-retirement.role.{}.{}.quarantine",
                        generation_hex(generation),
                        role.label()
                    ),
                    purge_name: format!(
                        "scheduler-retirement.role.{}.{}.purge",
                        generation_hex(generation),
                        role.label()
                    ),
                }
            }),
        };
        let owner = Self {
            binding: Some(binding),
            runtime,
            facts,
            barriers: false,
            states: std::array::from_fn(|i| {
                if plan.roles[i].current.is_some() {
                    RetirementRoleState::Original
                } else {
                    RetirementRoleState::Absent
                }
            }),
            last_error: None,
            role_failures: std::array::from_fn(|_| RoleFailureHistory::default()),
            observation_epoch: 0,
            resolutions: std::array::from_fn(|_| Vec::new()),
            stopped_plan: Some(plan),
            stopped_ready: false,
        };
        if let Some(error) = allocation_error {
            return Err(SqliteProjectionRetirementFailure {
                error: SqliteProjectionRetirementError::Capture(error),
                owner: SqliteProjectionRetirementOwner::Drained(owner),
            });
        }
        owner.finish_stopped()
    }
    fn finish_stopped(mut self) -> std::result::Result<Self, SqliteProjectionRetirementFailure> {
        if self.stopped_ready {
            return Ok(self);
        } // never reacquire after mutations
        let result = (|| -> Result<_> {
            let plan = self
                .stopped_plan
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("not stopped intake"))?;
            plan.validate(&self.runtime)?;
            scan_stopped_artifacts(&self.runtime, plan)?;
            let binding = self.binding.as_ref().expect("same stopped context");
            binding.acquire_stopped()?;
            #[cfg(test)]
            if FAIL_STOPPED_AFTER_ACQUIRE.with(|flag| flag.replace(false)) {
                anyhow::bail!("injected stopped failure after actual acquisition before barriers");
            }
            binding.retirement_barriers()?;
            let facts = snapshot_role_facts(binding, plan.generation, true)?;
            for i in 0..3 {
                ensure!(
                    facts.roles[i].original == plan.roles[i].current,
                    "stopped current-object identity/content changed after barriers"
                );
            }
            Ok(facts)
        })();
        match result {
            Ok(facts) => {
                self.facts = facts;
                self.barriers = true;
                self.stopped_ready = true;
                if let Err(error) = self.validate_stopped_effect_namespace() {
                    self.stopped_ready = false;
                    return Err(SqliteProjectionRetirementFailure {
                        error: SqliteProjectionRetirementError::Snapshot(error),
                        owner: SqliteProjectionRetirementOwner::Drained(self),
                    });
                }
                Ok(self)
            }
            Err(error) => Err(SqliteProjectionRetirementFailure {
                error: SqliteProjectionRetirementError::Capture(error),
                owner: SqliteProjectionRetirementOwner::Drained(self),
            }),
        }
    }
    fn selected_source_name(&self, role: SqliteRole) -> String {
        self.stopped_plan
            .as_ref()
            .and_then(|plan| plan.source_name(role))
            .unwrap_or_else(|| role.public_name().into())
    }
    /// Copied current pins/facts even during partial acquisition; no effects.
    pub fn observe_stopped_projection(&self) -> Result<StoppedSqliteProjectionObservation> {
        #[cfg(not(unix))]
        anyhow::bail!("stopped SQLite observation requires Unix");
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let plan = self
                .stopped_plan
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("not stopped intake"))?;
            let binding = self.binding.as_ref().expect("same stopped context");
            let facts = snapshot_role_facts(binding, plan.generation, self.stopped_ready)?;
            let mut links = [None; 3];
            for (i, link) in links.iter_mut().enumerate() {
                *link = binding.with_retirement_role(i, |file| {
                    file.map(|file| {
                        file.metadata()
                            .map(|metadata| metadata.nlink())
                            .map_err(anyhow::Error::new)
                    })
                    .transpose()
                })?;
            }
            let coordinates = plan
                .coordinates()
                .into_iter()
                .map(|(role, name)| {
                    let observation = match self.runtime.entry_no_follow(OsStr::new(&name)) {
                        Ok(None) => SqliteRoleCoordinateObservation::Absent,
                        Ok(Some(entry))
                            if entry.entry_type != lillux::secure_fs::PinnedEntryType::Regular =>
                        {
                            SqliteRoleCoordinateObservation::NonRegular(entry.entry_type)
                        }
                        Ok(Some(entry)) => {
                            let identity =
                                role.and_then(|role| facts.roles[role as usize].original.as_ref());
                            if identity.is_some_and(|identity| {
                                identity.device == entry.containing_device
                                    && identity.inode == entry.inode
                            }) {
                                SqliteRoleCoordinateObservation::SameOriginal {
                                    device: entry.containing_device,
                                    inode: entry.inode,
                                }
                            } else {
                                SqliteRoleCoordinateObservation::ForeignRegular {
                                    device: entry.containing_device,
                                    inode: entry.inode,
                                }
                            }
                        }
                        Err(error) => {
                            SqliteRoleCoordinateObservation::Unreadable(error.to_string())
                        }
                    };
                    StoppedSqliteCoordinateObservation {
                        role,
                        name,
                        observation,
                    }
                })
                .collect();
            Ok(StoppedSqliteProjectionObservation {
                plan: plan.clone(),
                ready: self.stopped_ready,
                roles: std::array::from_fn(|i| facts.roles[i].original.clone()),
                original_link_counts: links,
                coordinates,
            })
        }
    }
    fn validate_stopped_effect_namespace(&self) -> Result<()> {
        ensure!(
            self.stopped_ready && self.barriers,
            "stopped intake not ready for effects"
        );
        let plan = self.stopped_plan.as_ref().expect("stopped plan");
        plan.validate(&self.runtime)?;
        scan_stopped_artifacts(&self.runtime, plan)?;
        let observed = self.observe_stopped_projection()?;
        let mut copies = [0; 3];
        for entry in observed.coordinates {
            match entry.observation {
                SqliteRoleCoordinateObservation::Absent => {}
                SqliteRoleCoordinateObservation::SameOriginal { .. } => {
                    let role = entry
                        .role
                        .ok_or_else(|| anyhow::anyhow!("rollback journal present"))?;
                    copies[role as usize] += 1;
                }
                _ => anyhow::bail!(
                    "foreign/nonregular/unreadable stopped coordinate {}",
                    entry.name
                ),
            }
        }
        for (i, copy) in copies.iter().enumerate() {
            ensure!(*copy <= 1, "duplicate stopped role candidates");
            if matches!(
                self.states[i],
                RetirementRoleState::Absent | RetirementRoleState::Removed
            ) {
                ensure!(*copy == 0, "terminal stopped role coordinate reappeared");
            }
        }
        Ok(())
    }
    fn finish_barriers(mut self) -> std::result::Result<Self, SqliteProjectionRetirementFailure> {
        let binding = self.binding.as_ref().expect("same retained native owner");
        if let Err(error) = binding.retirement_barriers() {
            return Err(SqliteProjectionRetirementFailure {
                error: SqliteProjectionRetirementError::Barrier(error),
                owner: SqliteProjectionRetirementOwner::Drained(self),
            });
        }
        self.barriers = true;
        match snapshot_role_facts(binding, self.facts.generation, true) {
            Ok(facts) => {
                self.facts = facts;
                Ok(self)
            }
            Err(error) => Err(SqliteProjectionRetirementFailure {
                error: SqliteProjectionRetirementError::Snapshot(error),
                owner: SqliteProjectionRetirementOwner::Drained(self),
            }),
        }
    }
    pub fn role_facts(&self) -> &DrainedSqliteRoleFacts {
        &self.facts
    }
    pub fn last_error(&self) -> Option<&SqliteRoleMutationError> {
        self.last_error.as_ref()
    }
    fn retain_role_failure(
        &mut self,
        role: SqliteRole,
        operation: SqliteRoleMutationOperation,
        error: &SqliteRoleMutationError,
        substep_observed: bool,
    ) {
        let failure = SqliteRoleMutationFailureObservation {
            operation,
            error: error.clone(),
            substep_observed,
        };
        let history = &mut self.role_failures[role as usize];
        history.original.get_or_insert_with(|| failure.clone());
        history.latest = Some(failure);
        self.last_error = Some(error.clone());
    }
    /// Read-only, non-atomic namespace observations against SAME retained pins.
    /// No role open/dup, sync, effect, phase promotion or resolution is performed.
    pub fn observe_role_resolution(
        &mut self,
        role: SqliteRole,
    ) -> Result<SqliteRoleResolutionObservation> {
        #[cfg(not(unix))]
        {
            let _ = role;
            anyhow::bail!("SQLite retirement observation requires Unix");
        }
        #[cfg(unix)]
        {
            use lillux::secure_fs::PinnedEntryType;
            use std::os::unix::fs::MetadataExt;
            let index = role as usize;
            let next_epoch = self
                .observation_epoch
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("retirement observation epoch exhausted"))?;
            let runtime_identity = self.runtime.identity()?;
            let (original, original_link_count) = self
                .binding
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("retained native owner unavailable"))?
                .with_retirement_role(index, |file| match file {
                    None => {
                        ensure!(
                            self.facts.roles[index].original.is_none(),
                            "original role descriptor unavailable"
                        );
                        Ok((None, None))
                    }
                    Some(file) => {
                        let metadata = file.metadata()?;
                        ensure!(metadata.is_file(), "retained original is not regular");
                        let expected =
                            self.facts.roles[index].original.as_ref().ok_or_else(|| {
                                anyhow::anyhow!("unexpected retained role descriptor")
                            })?;
                        ensure!(
                            metadata.dev() == expected.device && metadata.ino() == expected.inode,
                            "retained original role identity changed"
                        );
                        Ok((
                            Some(SqliteRoleIdentity {
                                device: metadata.dev(),
                                inode: metadata.ino(),
                                byte_length: metadata.len(),
                                content_sha256: None,
                            }),
                            Some(metadata.nlink()),
                        ))
                    }
                })?;
            let inspect = |name: &str| match self.runtime.entry_no_follow(OsStr::new(name)) {
                Ok(None) => SqliteRoleCoordinateObservation::Absent,
                Ok(Some(entry)) if entry.entry_type != PinnedEntryType::Regular => {
                    SqliteRoleCoordinateObservation::NonRegular(entry.entry_type)
                }
                Ok(Some(entry)) => {
                    if original.as_ref().is_some_and(|original| {
                        original.device == entry.containing_device && original.inode == entry.inode
                    }) {
                        SqliteRoleCoordinateObservation::SameOriginal {
                            device: entry.containing_device,
                            inode: entry.inode,
                        }
                    } else {
                        SqliteRoleCoordinateObservation::ForeignRegular {
                            device: entry.containing_device,
                            inode: entry.inode,
                        }
                    }
                }
                Err(error) => SqliteRoleCoordinateObservation::Unreadable(error.to_string()),
            };
            let facts = &self.facts.roles[index];
            let public = inspect(role.public_name());
            let source_name = self.selected_source_name(role);
            let source = inspect(&source_name);
            let stopped_coordinates = if self.stopped_plan.is_some() {
                self.observe_stopped_projection()?.coordinates
            } else {
                Vec::new()
            };
            let quarantine = inspect(&facts.quarantine_name);
            let purge = inspect(&facts.purge_name);
            let history = &self.role_failures[index];
            let resolution_coordinates = self.resolutions[index]
                .iter()
                .map(|memo| {
                    let name = self.resolution_name(role, memo.attempt.ordinal);
                    SqliteRoleResolutionCoordinate {
                        ordinal: memo.attempt.ordinal,
                        observation: inspect(&name),
                        name,
                    }
                })
                .collect();
            let observation = SqliteRoleResolutionObservation {
                generation: self.facts.generation,
                role,
                runtime_identity,
                epoch: next_epoch,
                barriers_held: self.barriers,
                recovery_required: matches!(
                    self.states[index],
                    RetirementRoleState::RecoveryRequired
                ),
                original,
                original_link_count,
                public,
                source,
                selected_source: self
                    .stopped_plan
                    .as_ref()
                    .map(|plan| plan.roles[index].source),
                stopped_coordinates,
                stopped_plan: self.stopped_plan.clone(),
                quarantine,
                purge,
                quarantine_name: facts.quarantine_name.clone(),
                purge_name: facts.purge_name.clone(),
                original_failure: history.original.clone(),
                latest_failure: history.latest.clone(),
                resolutions: self.resolutions[index]
                    .iter()
                    .map(|memo| memo.attempt.clone())
                    .collect(),
                resolution_coordinates,
            };
            self.observation_epoch = next_epoch;
            Ok(observation)
        }
    }
    fn resolution_sync(&self) -> Result<()> {
        #[cfg(test)]
        if FAIL_RESOLUTION_SYNC.with(|flag| flag.replace(false)) {
            anyhow::bail!("injected resolution directory sync failure");
        }
        self.runtime.sync()
    }
    fn resolution_name(&self, role: SqliteRole, ordinal: u8) -> String {
        let generation = self
            .facts
            .generation
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        format!(
            "scheduler-retirement.resolve.{generation}.{}.{ordinal:02}.quarantine",
            role.label()
        )
    }
    /// Trusted domain caller must durably acknowledge its signed capacity/intent
    /// before invoking this. Facts/requests are not bearer authorization tokens.
    pub fn resolve_role(
        &mut self,
        request: SqliteRoleResolutionRequest,
    ) -> std::result::Result<SqliteRoleResolutionOutcome, SqliteRoleResolutionError> {
        let role = request.expected.role;
        let index = role as usize;
        let refuse = |message: &str| SqliteRoleResolutionError::Refused(message.into());
        if request.expected.generation != self.facts.generation {
            return Err(refuse("resolution generation mismatch"));
        }
        if let Some(memo) = self.resolutions[index].get(request.ordinal as usize) {
            if memo.request != request {
                return Err(refuse(
                    "resolution ordinal already bound to different request",
                ));
            }
            return match memo.attempt.outcome {
                Some(outcome) => Ok(outcome),
                None => Err(SqliteRoleResolutionError::Held(memo.attempt.clone())),
            };
        }
        if self.resolutions[index].len() >= SQLITE_ROLE_RESOLUTION_LIMIT {
            return Err(SqliteRoleResolutionError::BudgetExhausted);
        }
        if request.ordinal as usize != self.resolutions[index].len() {
            return Err(refuse("resolution ordinal must be dense"));
        }
        if self.stopped_plan.is_some() {
            self.validate_stopped_effect_namespace()
                .map_err(|error| refuse(&error.to_string()))?;
        }
        if !self.barriers || !matches!(self.states[index], RetirementRoleState::RecoveryRequired) {
            return Err(refuse(
                "resolution requires held drained owner and barriers",
            ));
        }
        if request.expected.epoch != self.observation_epoch {
            return Err(refuse("resolution observation epoch stale"));
        }
        let mut current = self
            .observe_role_resolution(role)
            .map_err(|error| refuse(&error.to_string()))?;
        current.epoch = request.expected.epoch;
        if current != request.expected {
            return Err(refuse("resolution observations changed"));
        }
        let absent = |coordinate: &SqliteRoleCoordinateObservation| {
            matches!(coordinate, SqliteRoleCoordinateObservation::Absent)
        };
        let original = |coordinate: &SqliteRoleCoordinateObservation| {
            matches!(
                coordinate,
                SqliteRoleCoordinateObservation::SameOriginal { .. }
            )
        };
        let all_r_absent = current
            .resolution_coordinates
            .iter()
            .all(|coordinate| absent(&coordinate.observation));
        let latest = current.latest_failure.as_ref();
        use lillux::secure_fs::{
            PlannedQuarantineDisposition as Disposition, PlannedQuarantineStage as Stage,
        };
        let no_original_move = |operation| {
            latest.is_some_and(|failure| {
                failure.substep_observed
                    && failure.operation == operation
                    && matches!(
                        (failure.error.disposition, failure.error.stage),
                        (
                            Disposition::BeforeCommit,
                            Stage::Unchanged | Stage::RestoredUnexpected
                        ) | (Disposition::DurabilityUncertain, Stage::RestoredUnexpected)
                    )
            })
        };
        let ordinary_unlink = latest.is_some_and(|failure| {
            failure.substep_observed
                && failure.operation == SqliteRoleMutationOperation::Purge
                && failure.error.disposition == Disposition::DurabilityUncertain
                && failure.error.stage == Stage::UnlinkedOriginal
        });
        let last_resolution_effect = current.resolutions.iter().rev().find(|attempt| {
            matches!(
                attempt.action,
                SqliteRoleResolutionAction::FinishPurgeAtU
                    | SqliteRoleResolutionAction::FinishPurgeAtResolution { .. }
            )
        });
        let resolution_unlink = last_resolution_effect.is_some_and(|attempt| {
            attempt.original_unlink_observed
                && attempt.substep == Some(Stage::UnlinkedOriginal)
                && attempt.effect_failure.as_ref().is_none_or(|error| {
                    error.disposition == Disposition::DurabilityUncertain
                        && error.stage == Stage::UnlinkedOriginal
                })
        });
        let ordinary_q = latest.is_some_and(|failure| {
            failure.substep_observed
                && failure.operation == SqliteRoleMutationOperation::Quarantine
                && failure.error.disposition == Disposition::DurabilityUncertain
                && failure.error.stage == Stage::QuarantinedOriginal
        });
        let ordinary_u = latest.is_some_and(|failure| {
            failure.substep_observed
                && failure.operation == SqliteRoleMutationOperation::Purge
                && failure.error.disposition == Disposition::DurabilityUncertain
                && failure.error.stage == Stage::QuarantinedOriginal
        });
        // Once resolution effects exist their latest substep, rather than old
        // ordinary testimony, determines any permissible continuation.
        let no_resolution_effect = current.resolutions.iter().all(|attempt| {
            !matches!(
                attempt.action,
                SqliteRoleResolutionAction::FinishPurgeAtU
                    | SqliteRoleResolutionAction::FinishPurgeAtResolution { .. }
            )
        });
        let held_without_move = |attempt: &SqliteRoleResolutionAttempt| {
            attempt.effect_failure.as_ref().is_some_and(|error| {
                matches!(
                    (error.disposition, error.stage),
                    (
                        Disposition::BeforeCommit,
                        Stage::Unchanged | Stage::RestoredUnexpected
                    ) | (Disposition::DurabilityUncertain, Stage::RestoredUnexpected)
                )
            })
        };
        let held_at_u = (no_resolution_effect && ordinary_u)
            || last_resolution_effect.is_some_and(|attempt| {
                attempt.action == SqliteRoleResolutionAction::FinishPurgeAtU
                    && held_without_move(attempt)
            });
        let permissible = match request.action {
            SqliteRoleResolutionAction::SyncQuarantine => {
                no_resolution_effect
                    && ordinary_q
                    && absent(&current.public)
                    && original(&current.quarantine)
                    && absent(&current.purge)
                    && all_r_absent
            }
            SqliteRoleResolutionAction::SyncRemoved => {
                (ordinary_unlink && no_resolution_effect || resolution_unlink)
                    && current.original_link_count == Some(0)
                    && absent(&current.public)
                    && absent(&current.quarantine)
                    && absent(&current.purge)
                    && all_r_absent
            }
            SqliteRoleResolutionAction::RearmOriginal => {
                no_resolution_effect
                    && no_original_move(SqliteRoleMutationOperation::Quarantine)
                    && original(&current.source)
                    && absent(&current.quarantine)
                    && absent(&current.purge)
                    && all_r_absent
            }
            SqliteRoleResolutionAction::RearmQuarantined => {
                no_resolution_effect
                    && no_original_move(SqliteRoleMutationOperation::Purge)
                    && absent(&current.public)
                    && original(&current.quarantine)
                    && absent(&current.purge)
                    && all_r_absent
            }
            SqliteRoleResolutionAction::FinishPurgeAtU => {
                held_at_u
                    && absent(&current.public)
                    && absent(&current.quarantine)
                    && original(&current.purge)
                    && all_r_absent
            }
            SqliteRoleResolutionAction::FinishPurgeAtResolution { source_ordinal } => {
                let latest_effect = last_resolution_effect;
                latest_effect.is_some_and(|attempt| {
                    (attempt.ordinal == source_ordinal
                        && attempt.effect_failure.as_ref().is_some_and(|error| {
                            error.disposition == Disposition::DurabilityUncertain
                                && error.stage == Stage::QuarantinedOriginal
                        }))
                        || (attempt.action
                            == SqliteRoleResolutionAction::FinishPurgeAtResolution {
                                source_ordinal,
                            }
                            && held_without_move(attempt))
                }) && absent(&current.public)
                    && absent(&current.quarantine)
                    && absent(&current.purge)
                    && current.resolution_coordinates.iter().all(|coordinate| {
                        if coordinate.ordinal == source_ordinal {
                            original(&coordinate.observation)
                        } else {
                            absent(&coordinate.observation)
                        }
                    })
            }
        };
        if !permissible {
            return Err(refuse(
                "held testimony/coordinates do not authorize requested mechanical action",
            ));
        }
        let sink = self.resolution_name(role, request.ordinal);
        // Refuse an occupied next sink BEFORE adding it or any physical effect.
        #[cfg(unix)]
        match self.runtime.entry_no_follow(OsStr::new(&sink)) {
            Ok(None) => {}
            _ => return Err(refuse("resolution sink occupied or unreadable")),
        }
        let attempt = SqliteRoleResolutionAttempt {
            ordinal: request.ordinal,
            action: request.action,
            outcome: None,
            effect_failure: None,
            substep: None,
            original_unlink_observed: false,
            error: None,
        };
        self.resolutions[index].push(ResolutionMemo {
            request: request.clone(),
            attempt,
        });
        let effect = match request.action {
            SqliteRoleResolutionAction::FinishPurgeAtU
            | SqliteRoleResolutionAction::FinishPurgeAtResolution { .. } => {
                let source = match request.action {
                    SqliteRoleResolutionAction::FinishPurgeAtResolution { source_ordinal } => {
                        self.resolution_name(role, source_ordinal)
                    }
                    _ => self.facts.roles[index].purge_name.clone(),
                };
                self.binding
                    .as_ref()
                    .expect("retained owner")
                    .with_retirement_role(index, |file| {
                        let file =
                            file.ok_or_else(|| anyhow::anyhow!("original pin unavailable"))?;
                        #[cfg(test)]
                        if FAIL_RESOLUTION_AFTER_MOVE.with(|flag| flag.replace(false)) {
                            return Ok(
                                match self.runtime.quarantine_if_same_planned_atomic(
                                    OsStr::new(&source),
                                    file,
                                    OsStr::new(&sink),
                                ) {
                                    Ok(()) => Err(lillux::secure_fs::PlannedQuarantineFailure {
                                        disposition: Disposition::DurabilityUncertain,
                                        stage: Stage::QuarantinedOriginal,
                                        quarantine_name: sink.clone().into(),
                                        error: anyhow::anyhow!(
                                            "injected after resolution move, before unlink"
                                        ),
                                    }),
                                    Err(failure) => Err(failure),
                                },
                            );
                        }
                        let result = self.runtime.remove_if_same_planned_atomic(
                            OsStr::new(&source),
                            file,
                            OsStr::new(&sink),
                        );
                        #[cfg(test)]
                        if result.is_ok()
                            && FAIL_RESOLUTION_AFTER_UNLINK.with(|flag| flag.replace(false))
                        {
                            return Ok(Err(lillux::secure_fs::PlannedQuarantineFailure {
                                disposition: Disposition::DurabilityUncertain,
                                stage: Stage::UnlinkedOriginal,
                                quarantine_name: sink.clone().into(),
                                error: anyhow::anyhow!(
                                    "injected after resolution unlink, before acknowledgement"
                                ),
                            }));
                        }
                        Ok(result)
                    })
            }
            _ => Ok(self.resolution_sync().map_err(|error| {
                lillux::secure_fs::PlannedQuarantineFailure {
                    disposition: Disposition::DurabilityUncertain,
                    stage: Stage::Unchanged,
                    quarantine_name: sink.clone().into(),
                    error,
                }
            })),
        };
        let ordinal = request.ordinal as usize;
        match effect {
            Ok(Ok(())) => {}
            Ok(Err(failure)) => {
                let attempt = &mut self.resolutions[index][ordinal].attempt;
                attempt.substep = Some(failure.stage);
                attempt.original_unlink_observed = failure.disposition
                    == Disposition::DurabilityUncertain
                    && failure.stage == Stage::UnlinkedOriginal
                    && matches!(
                        request.action,
                        SqliteRoleResolutionAction::FinishPurgeAtU
                            | SqliteRoleResolutionAction::FinishPurgeAtResolution { .. }
                    );
                attempt.effect_failure = Some(SqliteRoleMutationError {
                    role,
                    disposition: failure.disposition,
                    stage: failure.stage,
                    message: failure.error.to_string(),
                });
                attempt.error = Some(failure.error.to_string());
                return Err(SqliteRoleResolutionError::Held(attempt.clone()));
            }
            Err(error) => {
                let attempt = &mut self.resolutions[index][ordinal].attempt;
                attempt.error = Some(error.to_string());
                return Err(SqliteRoleResolutionError::Held(attempt.clone()));
            }
        }
        // Re-observe after sync/effect, while retaining exact success testimony.
        let outcome = match request.action {
            SqliteRoleResolutionAction::SyncQuarantine => {
                SqliteRoleResolutionOutcome::QuarantinedDurable
            }
            SqliteRoleResolutionAction::RearmOriginal => {
                SqliteRoleResolutionOutcome::RearmedOriginal
            }
            SqliteRoleResolutionAction::RearmQuarantined => {
                SqliteRoleResolutionOutcome::RearmedQuarantined
            }
            _ => SqliteRoleResolutionOutcome::RemovedDurable,
        };
        if matches!(outcome, SqliteRoleResolutionOutcome::RemovedDurable)
            && matches!(
                request.action,
                SqliteRoleResolutionAction::FinishPurgeAtU
                    | SqliteRoleResolutionAction::FinishPurgeAtResolution { .. }
            )
        {
            self.resolutions[index][ordinal].attempt.substep = Some(Stage::UnlinkedOriginal);
            self.resolutions[index][ordinal]
                .attempt
                .original_unlink_observed = true;
        }
        let after = self.observe_role_resolution(role).map_err(|error| {
            self.resolutions[index][ordinal].attempt.error = Some(error.to_string());
            SqliteRoleResolutionError::Held(self.resolutions[index][ordinal].attempt.clone())
        })?;
        let r_absent = after
            .resolution_coordinates
            .iter()
            .all(|coordinate| absent(&coordinate.observation));
        let valid = match outcome {
            SqliteRoleResolutionOutcome::QuarantinedDurable
            | SqliteRoleResolutionOutcome::RearmedQuarantined => {
                absent(&after.public)
                    && original(&after.quarantine)
                    && absent(&after.purge)
                    && r_absent
            }
            SqliteRoleResolutionOutcome::RearmedOriginal => {
                original(&after.source)
                    && absent(&after.quarantine)
                    && absent(&after.purge)
                    && r_absent
            }
            SqliteRoleResolutionOutcome::RemovedDurable => {
                after.original_link_count == Some(0)
                    && absent(&after.public)
                    && absent(&after.quarantine)
                    && absent(&after.purge)
                    && r_absent
            }
        };
        if !valid {
            self.resolutions[index][ordinal].attempt.error =
                Some("post-resolution coordinates changed; owner held".into());
            return Err(SqliteRoleResolutionError::Held(
                self.resolutions[index][ordinal].attempt.clone(),
            ));
        }
        self.states[index] = match outcome {
            SqliteRoleResolutionOutcome::QuarantinedDurable
            | SqliteRoleResolutionOutcome::RearmedQuarantined => RetirementRoleState::Quarantined,
            SqliteRoleResolutionOutcome::RearmedOriginal => RetirementRoleState::Original,
            SqliteRoleResolutionOutcome::RemovedDurable => RetirementRoleState::Removed,
        };
        self.resolutions[index][ordinal].attempt.outcome = Some(outcome);
        Ok(outcome)
    }
    fn phase_error(&self, role: SqliteRole) -> SqliteRoleMutationError {
        if matches!(
            self.states[role as usize],
            RetirementRoleState::RecoveryRequired
        ) && let Some(failure) = &self.role_failures[role as usize].latest
        {
            return failure.error.clone();
        }

        SqliteRoleMutationError {
            role,
            disposition: lillux::secure_fs::PlannedQuarantineDisposition::BeforeCommit,
            stage: lillux::secure_fs::PlannedQuarantineStage::Unchanged,
            message: "role phase does not authorize this effect".into(),
        }
    }
    pub fn quarantine_role(
        &mut self,
        role: SqliteRole,
    ) -> std::result::Result<SqliteRoleMutationOutcome, SqliteRoleMutationError> {
        if self.stopped_plan.is_some()
            && let Err(error) = self.validate_stopped_effect_namespace()
        {
            let mut failure = self.phase_error(role);
            failure.message = error.to_string();
            return Err(failure);
        }
        use lillux::secure_fs::PlannedQuarantineDisposition;
        let index = role as usize;
        if !self.barriers {
            return Err(self.phase_error(role));
        }
        match self.states[index] {
            RetirementRoleState::Absent => return Ok(SqliteRoleMutationOutcome::OriginallyAbsent),
            RetirementRoleState::Original => {}
            _ => return Err(self.phase_error(role)),
        }
        let result = self
            .binding
            .as_ref()
            .expect("same owner")
            .with_retirement_role(index, |file| {
                let file =
                    file.ok_or_else(|| anyhow::anyhow!("original role descriptor unavailable"))?;
                Ok(self.runtime.quarantine_if_same_planned_atomic(
                    OsStr::new(&self.selected_source_name(role)),
                    file,
                    OsStr::new(&self.facts.roles[index].quarantine_name),
                ))
            });
        match result {
            Ok(Ok(())) => {
                self.states[index] = RetirementRoleState::Quarantined;
                Ok(SqliteRoleMutationOutcome::QuarantinedDurable)
            }
            Ok(Err(failure)) => {
                if failure.disposition != PlannedQuarantineDisposition::BeforeCommit {
                    self.states[index] = RetirementRoleState::RecoveryRequired;
                }
                let error = SqliteRoleMutationError {
                    role,
                    disposition: failure.disposition,
                    stage: failure.stage,
                    message: failure.error.to_string(),
                };
                self.retain_role_failure(
                    role,
                    SqliteRoleMutationOperation::Quarantine,
                    &error,
                    true,
                );
                Err(error)
            }
            Err(failure) => {
                self.states[index] = RetirementRoleState::RecoveryRequired;
                let mut error = self.phase_error(role);
                error.message = failure.to_string();
                self.retain_role_failure(
                    role,
                    SqliteRoleMutationOperation::Quarantine,
                    &error,
                    false,
                );
                Err(error)
            }
        }
    }
    pub fn purge_role(
        &mut self,
        role: SqliteRole,
    ) -> std::result::Result<SqliteRoleMutationOutcome, SqliteRoleMutationError> {
        if self.stopped_plan.is_some()
            && let Err(error) = self.validate_stopped_effect_namespace()
        {
            let mut failure = self.phase_error(role);
            failure.message = error.to_string();
            return Err(failure);
        }
        use lillux::secure_fs::PlannedQuarantineDisposition;
        let index = role as usize;
        if !self.barriers {
            return Err(self.phase_error(role));
        }
        if matches!(self.states[index], RetirementRoleState::Absent) {
            return Ok(SqliteRoleMutationOutcome::OriginallyAbsent);
        }
        if !matches!(self.states[index], RetirementRoleState::Quarantined) {
            return Err(self.phase_error(role));
        }
        let result = self
            .binding
            .as_ref()
            .expect("same owner")
            .with_retirement_role(index, |file| {
                let file =
                    file.ok_or_else(|| anyhow::anyhow!("original role descriptor unavailable"))?;
                Ok(self.runtime.remove_if_same_planned_atomic(
                    OsStr::new(&self.facts.roles[index].quarantine_name),
                    file,
                    OsStr::new(&self.facts.roles[index].purge_name),
                ))
            });
        match result {
            Ok(Ok(())) => {
                self.states[index] = RetirementRoleState::Removed;
                Ok(SqliteRoleMutationOutcome::RemovedDurable)
            }
            Ok(Err(failure)) => {
                if failure.disposition != PlannedQuarantineDisposition::BeforeCommit {
                    self.states[index] = RetirementRoleState::RecoveryRequired;
                }
                let error = SqliteRoleMutationError {
                    role,
                    disposition: failure.disposition,
                    stage: failure.stage,
                    message: failure.error.to_string(),
                };
                self.retain_role_failure(role, SqliteRoleMutationOperation::Purge, &error, true);
                Err(error)
            }
            Err(failure) => {
                self.states[index] = RetirementRoleState::RecoveryRequired;
                let mut error = self.phase_error(role);
                error.message = failure.to_string();
                self.retain_role_failure(role, SqliteRoleMutationOperation::Purge, &error, false);
                Err(error)
            }
        }
    }
    pub fn release(mut self) -> std::result::Result<(), SqliteProjectionRetirementFailure> {
        if self.stopped_plan.is_some() && !self.stopped_ready {
            return Err(SqliteProjectionRetirementFailure {
                error: SqliteProjectionRetirementError::IncompleteRoles,
                owner: SqliteProjectionRetirementOwner::Drained(self),
            });
        }

        if !self.states.iter().all(|state| {
            matches!(
                state,
                RetirementRoleState::Absent | RetirementRoleState::Removed
            )
        }) {
            return Err(SqliteProjectionRetirementFailure {
                error: SqliteProjectionRetirementError::IncompleteRoles,
                owner: SqliteProjectionRetirementOwner::Drained(self),
            });
        }
        if let Err(error) = self
            .binding
            .as_ref()
            .expect("same owner")
            .allow_retirement_release()
        {
            return Err(SqliteProjectionRetirementFailure {
                error: SqliteProjectionRetirementError::Barrier(error),
                owner: SqliteProjectionRetirementOwner::Drained(self),
            });
        }
        BoundSqliteRetirement {
            binding: self.binding.take().expect("same owner"),
            runtime: self.runtime.clone(),
        }
        .retry()
        .map_err(|failure| {
            let BoundSqliteCloseFailure { owner, error } = failure;
            SqliteProjectionRetirementFailure {
                error: SqliteProjectionRetirementError::NativeRelease(error),
                owner: SqliteProjectionRetirementOwner::Releasing(owner),
            }
        })
    }
}

fn release_projection_binding(
    binding: Binding,
    runtime: Arc<PinnedDirectory>,
) -> std::result::Result<(), SqliteProjectionRetirementFailure> {
    BoundSqliteRetirement { binding, runtime }
        .retry()
        .map_err(|failure| {
            let BoundSqliteCloseFailure { owner, error } = failure;
            SqliteProjectionRetirementFailure {
                error: SqliteProjectionRetirementError::NativeRelease(error),
                owner: SqliteProjectionRetirementOwner::Releasing(owner),
            }
        })
}
impl PendingCapturedSqliteProjection {
    pub fn abort(mut self) -> std::result::Result<(), SqliteProjectionRetirementFailure> {
        let connection = self.owner.connection.take().expect("captured SQL owner");
        if let Err((connection, error)) = connection.close() {
            self.owner.connection = Some(connection);
            return Err(SqliteProjectionRetirementFailure {
                error: SqliteProjectionRetirementError::Connection(error),
                owner: SqliteProjectionRetirementOwner::CapturePending(self),
            });
        }
        let binding = self.owner.binding.take().expect("same native owner");
        let cleanup = CleanupSqliteProjection {
            binding: Some(binding),
            runtime: self.owner.runtime.clone(),
            generation: self.generation,
        };
        cleanup.abort()
    }
}
impl CleanupSqliteProjection {
    pub fn generation(&self) -> [u8; 16] {
        self.generation
    }
    pub fn abort(mut self) -> std::result::Result<(), SqliteProjectionRetirementFailure> {
        if let Err(error) = self
            .binding
            .as_ref()
            .expect("same cleanup owner")
            .allow_retirement_release()
        {
            return Err(SqliteProjectionRetirementFailure {
                error: SqliteProjectionRetirementError::Barrier(error),
                owner: SqliteProjectionRetirementOwner::Cleanup(self),
            });
        }
        release_projection_binding(
            self.binding.take().expect("same owner"),
            self.runtime.clone(),
        )
    }
}
impl CapturedSqliteProjection {
    pub fn abort(self) -> std::result::Result<(), SqliteProjectionRetirementFailure> {
        PendingCapturedSqliteProjection {
            owner: self.owner,
            generation: self.facts.generation,
        }
        .abort()
    }
}
impl DrainedSqliteProjection {
    pub fn abort(mut self) -> std::result::Result<(), SqliteProjectionRetirementFailure> {
        if !self.states.iter().all(|state| {
            matches!(
                state,
                RetirementRoleState::Absent | RetirementRoleState::Original
            )
        }) {
            return Err(SqliteProjectionRetirementFailure {
                error: SqliteProjectionRetirementError::IncompleteRoles,
                owner: SqliteProjectionRetirementOwner::Drained(self),
            });
        }
        if let Err(error) = self
            .binding
            .as_ref()
            .expect("same owner")
            .allow_retirement_release()
        {
            return Err(SqliteProjectionRetirementFailure {
                error: SqliteProjectionRetirementError::Barrier(error),
                owner: SqliteProjectionRetirementOwner::Drained(self),
            });
        }
        release_projection_binding(
            self.binding.take().expect("same owner"),
            self.runtime.clone(),
        )
    }
}
impl SqliteProjectionRetirementFailure {
    /// Cleanup only. Partial namespace mutation cannot use this to abandon its
    /// original lifelines. Caller keeps original State/CAS authority throughout.
    pub fn abort(self) -> std::result::Result<(), Self> {
        match self.owner {
            SqliteProjectionRetirementOwner::UnacquiredStopped { .. } => Ok(()),
            SqliteProjectionRetirementOwner::Uncaptured(owner) => {
                owner.close().map_err(|failure| {
                    let BoundSqliteCloseFailure { owner, error } = failure;
                    Self {
                        owner: SqliteProjectionRetirementOwner::Releasing(owner),
                        error: SqliteProjectionRetirementError::NativeRelease(error),
                    }
                })
            }
            SqliteProjectionRetirementOwner::CapturePending(owner) => owner.abort(),
            SqliteProjectionRetirementOwner::Cleanup(owner) => owner.abort(),
            SqliteProjectionRetirementOwner::Captured(owner) => owner.abort(),
            SqliteProjectionRetirementOwner::Drained(owner) => owner.abort(),
            SqliteProjectionRetirementOwner::Releasing(owner) => owner.retry().map_err(|failure| {
                let BoundSqliteCloseFailure { owner, error } = failure;
                Self {
                    owner: SqliteProjectionRetirementOwner::Releasing(owner),
                    error: SqliteProjectionRetirementError::NativeRelease(error),
                }
            }),
        }
    }
}

/// One bound connection and native retained authority. No injected connection
/// can acquire this type. Schema and authenticated recovery stay with caller.
pub struct BoundSqliteConnection {
    connection: Option<Connection>,
    binding: Option<Binding>,
    runtime: Arc<PinnedDirectory>,
}
/// Observed native custody preventing physical release of this same context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum NativeRetentionReason {
    TrackedPeer,
    UnknownIdentity,
    LiveNativeFile,
    /// Linux freed the FD coordinate but reported an observed close error.
    /// Retain original error custody; never retry a reused numeric FD.
    CloseError,
}
#[derive(Debug)]
pub enum BoundSqliteCloseError {
    Connection(rusqlite::Error),
    NativeCustody(NativeRetentionReason),
}
/// Retirement retains the same native context/root without a SQL connection.
pub struct BoundSqliteRetirement {
    binding: Binding,
    runtime: Arc<PinnedDirectory>,
}
impl BoundSqliteRetirement {
    pub fn runtime(&self) -> &PinnedDirectory {
        &self.runtime
    }
    pub fn retry(mut self) -> std::result::Result<(), BoundSqliteCloseFailure> {
        match self.binding.release() {
            None => Ok(()),
            Some(reason) => Err(BoundSqliteCloseFailure {
                owner: BoundSqliteCloseOwner::Retiring(self),
                error: BoundSqliteCloseError::NativeCustody(reason),
            }),
        }
    }
}
/// Explicit phase prevents using a closed connection after native Pending.
pub enum BoundSqliteCloseOwner {
    Live(BoundSqliteConnection),
    Retiring(BoundSqliteRetirement),
}
impl BoundSqliteCloseOwner {
    pub fn retry(self) -> std::result::Result<(), BoundSqliteCloseFailure> {
        match self {
            Self::Live(owner) => owner.close(),
            Self::Retiring(owner) => owner.retry(),
        }
    }
}
/// Failed supervised close retains the exact live/retiring owner for retry.
pub struct BoundSqliteCloseFailure {
    owner: BoundSqliteCloseOwner,
    error: BoundSqliteCloseError,
}
impl BoundSqliteCloseFailure {
    pub fn error(&self) -> &BoundSqliteCloseError {
        &self.error
    }
    pub fn owner(&self) -> &BoundSqliteCloseOwner {
        &self.owner
    }
    pub fn retry(self) -> std::result::Result<(), Self> {
        self.owner.retry()
    }
    pub fn into_owner(self) -> BoundSqliteCloseOwner {
        self.owner
    }
}
impl std::fmt::Debug for BoundSqliteCloseFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoundSqliteCloseFailure")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}
/// Constructor failure retains every acquired native resource with its origin.
pub struct BoundSqliteOpenFailure {
    error: anyhow::Error,
    owner: Option<BoundSqliteCloseOwner>,
    close_error: Option<BoundSqliteCloseError>,
}
impl BoundSqliteOpenFailure {
    pub fn error(&self) -> &anyhow::Error {
        &self.error
    }
    pub fn owner(&self) -> Option<&BoundSqliteCloseOwner> {
        self.owner.as_ref()
    }
    pub fn close_error(&self) -> Option<&BoundSqliteCloseError> {
        self.close_error.as_ref()
    }
    pub fn retry_close(mut self) -> std::result::Result<(), Self> {
        if let Some(owner) = self.owner.take()
            && let Err(failure) = owner.retry()
        {
            let BoundSqliteCloseFailure { owner, error } = failure;
            self.owner = Some(owner);
            self.close_error = Some(error);
            return Err(self);
        }
        Ok(())
    }
}
impl std::fmt::Debug for BoundSqliteOpenFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoundSqliteOpenFailure")
            .field("error", &self.error)
            .field("retained_owner", &self.owner.is_some())
            .finish()
    }
}
impl std::fmt::Display for BoundSqliteOpenFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.error, f)
    }
}
impl std::error::Error for BoundSqliteOpenFailure {}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundSqliteOrigin {
    Existing,
    Created,
}
impl BoundSqliteConnection {
    /// Existing regular main, read/write and noncreating.
    pub fn open_existing(
        runtime: Arc<PinnedDirectory>,
        name: &OsStr,
    ) -> std::result::Result<Self, BoundSqliteOpenFailure> {
        Self::acquire(runtime, name, 0, None, None, None)
    }
    /// Exclusive fresh main acquisition inside retained native custody.
    pub fn create_new(
        runtime: Arc<PinnedDirectory>,
        name: &OsStr,
    ) -> std::result::Result<Self, BoundSqliteOpenFailure> {
        Self::acquire(runtime, name, 1, None, None, None)
    }
    /// Existing or exclusive fresh acquisition; a create collision refuses.
    pub fn open_or_create(
        runtime: Arc<PinnedDirectory>,
        name: &OsStr,
    ) -> std::result::Result<Self, BoundSqliteOpenFailure> {
        Self::acquire(runtime, name, 2, None, None, None)
    }
    fn acquire(
        runtime: Arc<PinnedDirectory>,
        name: &OsStr,
        mode: i32,
        lease: Option<SqliteCopyLease>,
        sql_work: Option<Arc<dyn SqliteInspectionSqlWork>>,
        production: Option<&SqliteInspectionProductionPermit>,
    ) -> std::result::Result<Self, BoundSqliteOpenFailure> {
        if (mode == 3) != sql_work.is_some() || (mode == 3) != production.is_some() {
            return Err(BoundSqliteOpenFailure {
                error: anyhow::anyhow!("inspection SQL requires its original work owner"),
                owner: None,
                close_error: None,
            });
        }
        if let Some(production) = production
            && let Err(error) = production.checkpoint()
        {
            return Err(BoundSqliteOpenFailure {
                error,
                owner: None,
                close_error: None,
            });
        }
        if let Some(work) = &sql_work
            && let Err(error) = work.require_destination(&runtime)
        {
            return Err(BoundSqliteOpenFailure {
                error,
                owner: None,
                close_error: None,
            });
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (runtime, name, mode, lease, sql_work);
            Err(BoundSqliteOpenFailure {
                error: anyhow::anyhow!("bound SQLite unavailable"),
                owner: None,
                close_error: None,
            })
        }
        #[cfg(target_os = "linux")]
        {
            // Duplicate the exact retained directory inode directly. The C
            // binding uses descriptor-relative operations, not directory cursor
            // reads; no allocating /proc pathname or ambient reopen is needed.
            let root = runtime
                .try_clone_descriptor()
                .map_err(|error| BoundSqliteOpenFailure {
                    error,
                    owner: None,
                    close_error: None,
                })?;
            let binding =
                // Mode4 is strict readonly stable-source acquisition. C keeps
                // every role readonly and refuses absent-sidecar creation;
                // copied inspection mode3 retains its separate semantics.
                Binding::create(&root, name, mode, lease)
                    .map_err(|(error, binding)| {
                    BoundSqliteOpenFailure {
                        error,
                        owner: binding.map(|binding| {
                            BoundSqliteCloseOwner::Retiring(BoundSqliteRetirement {
                                binding,
                                runtime: runtime.clone(),
                            })
                        }),
                        close_error: None,
                    }
                })?;
            let result = (|| -> Result<Connection> {
                Ok(Connection::open_with_flags_and_vfs(
                    binding.path()?,
                    (if mode == 3 || mode == 4 {
                        OpenFlags::SQLITE_OPEN_READ_ONLY
                    } else {
                        OpenFlags::SQLITE_OPEN_READ_WRITE
                    }) | OpenFlags::SQLITE_OPEN_FULL_MUTEX,
                    binding.name()?,
                )?)
            })();
            let connection = match result {
                Ok(connection) => connection,
                Err(error) => {
                    return Err(BoundSqliteOpenFailure {
                        error,
                        owner: Some(BoundSqliteCloseOwner::Retiring(BoundSqliteRetirement {
                            binding,
                            runtime,
                        })),
                        close_error: None,
                    });
                }
            };
            let owned = Self {
                connection: Some(connection),
                binding: Some(binding),
                runtime,
            };
            if let Some(work) = &sql_work
                && let Err(error) =
                    install_inspection_sql_work(owned.connection(), &owned.runtime, work.clone())
            {
                return Err(BoundSqliteOpenFailure {
                    error,
                    owner: Some(BoundSqliteCloseOwner::Live(owned)),
                    close_error: None,
                });
            }
            if let Err(error) = owned
                .binding
                .as_ref()
                .expect("owned binding")
                .validate(owned.connection(), false)
            {
                return Err(BoundSqliteOpenFailure {
                    error,
                    owner: Some(BoundSqliteCloseOwner::Live(owned)),
                    close_error: None,
                });
            }
            Ok(owned)
        }
    }
    pub fn origin(&self) -> BoundSqliteOrigin {
        if self.binding.as_ref().expect("owned binding").created() {
            BoundSqliteOrigin::Created
        } else {
            BoundSqliteOrigin::Existing
        }
    }
    pub fn transaction(&mut self) -> rusqlite::Result<rusqlite::Transaction<'_>> {
        self.connection
            .as_mut()
            .expect("owned live connection")
            .transaction()
    }
    pub fn capture_projection_retirement(
        self,
        generation: [u8; 16],
    ) -> std::result::Result<CapturedSqliteProjection, SqliteProjectionRetirementFailure> {
        if let Err(error) = self
            .binding
            .as_ref()
            .expect("same owner")
            .capture_retirement(generation)
        {
            return Err(SqliteProjectionRetirementFailure {
                error: SqliteProjectionRetirementError::Capture(error),
                owner: SqliteProjectionRetirementOwner::Uncaptured(self),
            });
        }
        PendingCapturedSqliteProjection {
            owner: self,
            generation,
        }
        .finish_snapshot()
    }
    /// Success proves SQL close AND all native keeper/root/context release.
    /// Pending returns the same Retiring owner; retry never reopens. Drop is
    /// a native fallback retention path, never supervisor settlement evidence.
    pub fn close(mut self) -> std::result::Result<(), BoundSqliteCloseFailure> {
        let connection = self.connection.take().expect("owned live connection");
        match connection.close() {
            Ok(()) => BoundSqliteRetirement {
                binding: self.binding.take().expect("owned binding"),
                runtime: self.runtime.clone(),
            }
            .retry(),
            Err((connection, error)) => {
                self.connection = Some(connection);
                Err(BoundSqliteCloseFailure {
                    owner: BoundSqliteCloseOwner::Live(self),
                    error: BoundSqliteCloseError::Connection(error),
                })
            }
        }
    }
    pub fn connection(&self) -> &Connection {
        self.connection.as_ref().expect("owned live connection")
    }
    pub fn runtime(&self) -> &PinnedDirectory {
        &self.runtime
    }
    /// The caller first establishes its supported WAL schema/mode. Eager
    /// sidecar effects remain guarded inside the native adoption path.
    pub fn establish_wal_binding(&self) -> Result<()> {
        let mode: String = self
            .connection()
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
        ensure!(
            mode == "wal",
            "bound SQLite requires caller-established WAL mode"
        );
        self.connection()
            .execute_batch("BEGIN IMMEDIATE; ROLLBACK;")?;
        self.require_binding()
    }
    pub fn require_binding(&self) -> Result<()> {
        self.binding
            .as_ref()
            .expect("owned live binding")
            .validate(self.connection(), true)
    }
}
impl Drop for BoundSqliteConnection {
    // This owner holds no arena handle, so Drop cannot enter a cleanup scope.
    // A last WAL user dropped outside any scope charges its SHM unmap to the
    // service budget; if that is exhausted the mapping stays recorded and
    // marks the arena refused, so arena release fails closed.
    fn drop(&mut self) {
        if let Some(connection) = self.connection.take()
            && let Err((refused, _error)) = connection.close()
        {
            // An outstanding SQLite resource must not outlive its VFS
            // context. Retain both rather than create use-after-free.
            if let Some(mut binding) = self.binding.take() {
                binding.abandon(&refused);
            }
            retain_abandoned_sqlite_owner(AbandonedSqliteOwnerKind::BoundConnection, refused);
        }
        // Binding closes retained host descriptors only after connection close.
    }
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    #[test]
    fn guarded_evidence_read_is_bounded_and_refuses_nonregular_children() {
        let _serial = crate::test_host::serial_guard();
        let tmp = tempfile::tempdir().unwrap();
        let runtime = PinnedDirectory::open(tmp.path()).unwrap().unwrap();
        std::fs::write(tmp.path().join("record"), b"exact").unwrap();
        assert_eq!(
            read_regular_without_sqlite_alias(&runtime, OsStr::new("record"), 5).unwrap(),
            Some(b"exact".to_vec())
        );
        assert!(read_regular_without_sqlite_alias(&runtime, OsStr::new("record"), 4).is_err());
        assert_eq!(
            read_regular_without_sqlite_alias(&runtime, OsStr::new("absent"), 5).unwrap(),
            None
        );
        std::os::unix::fs::symlink("record", tmp.path().join("link")).unwrap();
        std::fs::create_dir(tmp.path().join("directory")).unwrap();
        for name in ["link", "directory", "../record"] {
            assert!(read_regular_without_sqlite_alias(&runtime, OsStr::new(name), 5).is_err());
        }
        // Preserve the existing legacy marker bound above one copy chunk.
        let legacy_marker = vec![b'x'; 262_144];
        std::fs::write(tmp.path().join("legacy-marker"), &legacy_marker).unwrap();
        assert_eq!(
            read_regular_without_sqlite_alias(&runtime, OsStr::new("legacy-marker"), 262_144)
                .unwrap(),
            Some(legacy_marker)
        );
        assert!(
            read_regular_without_sqlite_alias(&runtime, OsStr::new("legacy-marker"), 65_536)
                .is_err()
        );
        for bound in [0, 262_145, usize::MAX] {
            assert!(
                read_regular_without_sqlite_alias(&runtime, OsStr::new("record"), bound).is_err()
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn guarded_evidence_read_rejects_live_sqlite_role_aliases() {
        let host = crate::test_host::TestNativeHost::acquire();
        let tmp = tempfile::tempdir().unwrap();
        seed_wal_on(&host, tmp.path());
        let runtime = Arc::new(PinnedDirectory::open(tmp.path()).unwrap().unwrap());
        let (owner, ticket) = armed_bound(&host, &runtime);
        host.scope(|| owner.establish_wal_binding().unwrap());
        for (index, role) in [
            "scheduler.sqlite3",
            "scheduler.sqlite3-wal",
            "scheduler.sqlite3-shm",
        ]
        .into_iter()
        .enumerate()
        {
            let name = format!("evidence-{index}");
            std::fs::hard_link(tmp.path().join(role), tmp.path().join(&name)).unwrap();
            assert!(
                read_regular_without_sqlite_alias(&runtime, OsStr::new(&name), 65_536).is_err()
            );
        }
        host.scope(|| owner.require_binding().unwrap());
        close_bound(&host, owner, ticket);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn guarded_evidence_read_uses_pin_after_path_is_replaced_by_sqlite_alias() {
        let host = crate::test_host::TestNativeHost::acquire();
        let tmp = tempfile::tempdir().unwrap();
        seed_wal_on(&host, tmp.path());
        let runtime = Arc::new(PinnedDirectory::open(tmp.path()).unwrap().unwrap());
        let (owner, ticket) = armed_bound(&host, &runtime);
        host.scope(|| owner.establish_wal_binding().unwrap());
        std::fs::write(tmp.path().join("record"), b"original").unwrap();
        let pin = runtime
            .open_mount_entry(OsStr::new("record"))
            .unwrap()
            .unwrap();
        std::fs::remove_file(tmp.path().join("record")).unwrap();
        std::fs::hard_link(
            tmp.path().join("scheduler.sqlite3"),
            tmp.path().join("record"),
        )
        .unwrap();
        assert_eq!(binding::read_unbound_regular(&pin, 8).unwrap(), b"original");
        assert!(ensure_guarded_read_name(&runtime, OsStr::new("record"), &pin).is_err());
        assert!(read_regular_without_sqlite_alias(&runtime, OsStr::new("record"), 65_536).is_err());
        host.scope(|| owner.require_binding().unwrap());
        close_bound(&host, owner, ticket);
    }

    use super::*;
    #[test]
    #[cfg(target_os = "linux")]
    fn stock_wal_and_shm_remain_operational() {
        let host = crate::test_host::TestNativeHost::acquire();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("stock.sqlite3");
        let (writer, writer_ticket) = armed_stock(&host, &path);
        host.scope(|| {
            writer
                .execute_batch(
                    "PRAGMA journal_mode=WAL; CREATE TABLE value(n); INSERT INTO value VALUES(7);",
                )
                .unwrap()
        });
        let (reader, reader_ticket) = armed_stock(&host, &path);
        host.scope(|| {
            assert_eq!(
                reader
                    .query_row("SELECT n FROM value", [], |row| row.get::<_, i64>(0))
                    .unwrap(),
                7
            );
            writer.execute_batch("UPDATE value SET n=9;").unwrap();
            assert_eq!(
                reader
                    .query_row("SELECT n FROM value", [], |row| row.get::<_, i64>(0))
                    .unwrap(),
                9
            );
        });
        assert!(tmp.path().join("stock.sqlite3-wal").is_file());
        assert!(tmp.path().join("stock.sqlite3-shm").is_file());
        close_armed(&host, reader, reader_ticket);
        close_armed(&host, writer, writer_ticket);
    }
    #[test]
    #[cfg(target_os = "linux")]
    fn stock_shm_nofollow_preserves_symlink_target() {
        let host = crate::test_host::TestNativeHost::acquire();
        let tmp = tempfile::tempdir().unwrap();
        seed_wal_on(&host, tmp.path());
        let target = tmp.path().join("foreign-shm");
        let bytes = vec![0x5a; 32768];
        std::fs::write(&target, &bytes).unwrap();
        std::os::unix::fs::symlink(&target, tmp.path().join("scheduler.sqlite3-shm")).unwrap();
        let (connection, ticket) = armed_stock(&host, &tmp.path().join("scheduler.sqlite3"));
        host.scope(|| {
            assert!(
                connection
                    .query_row("SELECT value FROM custody", [], |row| row.get::<_, i64>(0))
                    .is_err()
            )
        });
        close_armed(&host, connection, ticket);
        assert_eq!(std::fs::read(target).unwrap(), bytes);
        assert!(
            std::fs::symlink_metadata(tmp.path().join("scheduler.sqlite3-shm"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
    #[test]
    #[cfg(target_os = "linux")]
    fn captured_observation_failure_keeps_sealed_generation_and_claim() {
        let host = crate::test_host::TestNativeHost::acquire();
        let tmp = tempfile::tempdir().unwrap();
        seed_wal_on(&host, tmp.path());
        let runtime = Arc::new(PinnedDirectory::open(tmp.path()).unwrap().unwrap());
        let (owner, owner_ticket) = host.armed(|| {
            BoundSqliteConnection::open_existing(runtime, OsStr::new("scheduler.sqlite3")).unwrap()
        });
        host.scope(|| owner.establish_wal_binding().unwrap());
        FAIL_CAPTURE_OBSERVATION.with(|flag| flag.set(true));
        let failure = host.scope(|| {
            owner
                .capture_projection_retirement([0x41; 16])
                .err()
                .expect("post-claim failure")
        });
        assert!(matches!(
            failure.owner(),
            SqliteProjectionRetirementOwner::CapturePending(_)
        ));
        let failure = host.scope(|| {
            failure
                .retry_capture([0x42; 16])
                .err()
                .expect("generation substitution refused")
        });
        assert!(matches!(
            failure.owner(),
            SqliteProjectionRetirementOwner::CapturePending(_)
        ));
        let refused = armed_stock_refusal(&host, &tmp.path().join("scheduler.sqlite3"));
        assert_eq!(
            refused.sqlite_error_code(),
            Some(STOCK_ALIAS_REFUSAL),
            "{refused}"
        );
        let captured = host.scope(|| failure.retry_capture([0x41; 16]).unwrap());
        assert_eq!(captured.captured_roles().generation, [0x41; 16]);
        captured.abort().unwrap();
        host.retire_raw(owner_ticket);
        let (connection, ticket) = armed_stock(&host, &tmp.path().join("scheduler.sqlite3"));
        host.scope(|| {
            assert_eq!(
                connection
                    .query_row("SELECT value FROM custody", [], |row| row.get::<_, i64>(0))
                    .unwrap(),
                1
            )
        });
        close_armed(&host, connection, ticket);
    }
    /// A bound VFS context dropped outside every allocator scope must still be
    /// unlinked from SQLite's VFS list before its memory is freed; otherwise
    /// the next arena's initialization walks the list into the old mapping.
    #[test]
    #[cfg(target_os = "linux")]
    fn binding_dropped_outside_scope_unlinks_before_arena_release() {
        let mut host = crate::test_host::TestNativeHost::acquire();
        let tmp = tempfile::tempdir().unwrap();
        // Rollback-journal database: no shared-memory mapping is involved, so
        // the only out-of-scope native effect under test is the VFS unlink.
        let (seed, seed_ticket) =
            host.armed(|| Connection::open(tmp.path().join("scheduler.sqlite3")).unwrap());
        host.scope(|| {
            seed.execute_batch(
                "CREATE TABLE custody(value INTEGER); INSERT INTO custody VALUES(1);",
            )
            .unwrap()
        });
        close_armed(&host, seed, seed_ticket);
        let runtime = Arc::new(PinnedDirectory::open(tmp.path()).unwrap().unwrap());
        let (owner, owner_ticket) = host.armed(|| {
            BoundSqliteConnection::open_existing(runtime, OsStr::new("scheduler.sqlite3")).unwrap()
        });
        host.scope(|| {
            let value: i64 = owner
                .connection()
                .query_row("SELECT value FROM custody", [], |row| row.get(0))
                .unwrap();
            assert_eq!(value, 1);
        });
        drop(owner); // Drop for BoundSqliteConnection, no native scope active
        host.retire_raw(owner_ticket);
        // Reclaim and release the arena, then install and initialize a fresh one.
        host.cycle();
        let connection = host.open_transient();
        let value: i64 = connection
            .with_sql(|sql| Ok(sql.query_row("SELECT 6 * 7", [], |row| row.get(0))?))
            .unwrap();
        assert_eq!(value, 42);
        host.close(connection);
        let (stock, ticket) = armed_stock(&host, &tmp.path().join("scheduler.sqlite3"));
        host.scope(|| {
            assert_eq!(
                stock
                    .query_row("SELECT value FROM custody", [], |row| row.get::<_, i64>(0))
                    .unwrap(),
                1
            )
        });
        close_armed(&host, stock, ticket);
    }
    /// A WAL connection dropped outside every allocator scope releases its
    /// shared-memory mapping there; later opens and the arena cycle proceed.
    #[test]
    #[cfg(target_os = "linux")]
    fn wal_binding_dropped_outside_scope_releases_shared_memory() {
        let mut host = crate::test_host::TestNativeHost::acquire();
        let tmp = tempfile::tempdir().unwrap();
        seed_wal_on(&host, tmp.path());
        let runtime = Arc::new(PinnedDirectory::open(tmp.path()).unwrap().unwrap());
        let (owner, owner_ticket) = host.armed(|| {
            BoundSqliteConnection::open_existing(runtime, OsStr::new("scheduler.sqlite3")).unwrap()
        });
        host.scope(|| {
            owner.establish_wal_binding().unwrap();
            let value: i64 = owner
                .connection()
                .query_row("SELECT value FROM custody", [], |row| row.get(0))
                .unwrap();
            assert_eq!(value, 1);
        });
        let (mapped_bytes, mapped_slots) = host.shared_census();
        assert!(mapped_bytes > 0 && mapped_slots > 0);
        drop(owner); // last WAL user: SHM purge runs with no native scope active
        assert_eq!(
            host.shared_census(),
            (0, 0),
            "out-of-scope purge clears bytes and slots"
        );
        host.retire_raw(owner_ticket);
        let (stock, ticket) = armed_stock(&host, &tmp.path().join("scheduler.sqlite3"));
        host.scope(|| {
            assert_eq!(
                stock
                    .query_row("SELECT value FROM custody", [], |row| row.get::<_, i64>(0))
                    .unwrap(),
                1
            )
        });
        close_armed(&host, stock, ticket);
        host.cycle();
        let connection = host.open_transient();
        host.close(connection);
    }
    /// A second in-process WAL user keeps the shared node alive: no mapping
    /// is released until the last close, which here runs in a cleanup scope
    /// and releases every mapping group. Bound owners refuse a peer on their
    /// SHM node, so both users are armed stock connections.
    #[test]
    #[cfg(target_os = "linux")]
    fn wal_shared_memory_unmaps_every_group_only_at_last_close_in_cleanup_scope() {
        let host = crate::test_host::TestNativeHost::acquire();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("scheduler.sqlite3");
        let (first, first_ticket) = armed_stock(&host, &path);
        host.scope(|| {
            first
                .execute_batch(
                    "PRAGMA page_size=512; PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; \
                     CREATE TABLE custody(value INTEGER); INSERT INTO custody VALUES(1); \
                     CREATE TABLE filler(body BLOB);",
                )
                .unwrap()
        });
        let (second, second_ticket) = armed_stock(&host, &path);
        host.scope(|| {
            // Grow the WAL index past its first 32 KiB region: no checkpoint
            // runs, so every frame stays indexed.
            for _ in 0..150 {
                first
                    .execute("INSERT INTO filler(body) VALUES (zeroblob(16000))", [])
                    .unwrap();
            }
            let rows: i64 = second
                .query_row("SELECT COUNT(*) FROM filler", [], |row| row.get(0))
                .unwrap();
            assert_eq!(rows, 150);
        });
        let (grown_bytes, grown_slots) = host.shared_census();
        assert!(
            grown_slots >= 2,
            "multiple mapping groups recorded: {grown_slots}"
        );
        assert!(grown_bytes >= 2 * 32768, "{grown_bytes}");
        close_armed(&host, first, first_ticket);
        assert_eq!(
            host.shared_census(),
            (grown_bytes, grown_slots),
            "a live peer keeps every mapping recorded"
        );
        host.scope(|| {
            second
                .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
                .unwrap()
        });
        host.cleanup_scope(|| second.close().unwrap());
        assert_eq!(
            host.shared_census(),
            (0, 0),
            "last close clears bytes and slots"
        );
        host.retire_raw(second_ticket);
        let (stock, ticket) = armed_stock(&host, &path);
        host.scope(|| {
            assert_eq!(
                stock
                    .query_row("SELECT value FROM custody", [], |row| row.get::<_, i64>(0))
                    .unwrap(),
                1
            )
        });
        close_armed(&host, stock, ticket);
    }
    #[test]
    #[cfg(target_os = "linux")]
    fn retirement_barriers_survive_stock_alias_refusal_and_purge() {
        let host = crate::test_host::TestNativeHost::acquire();
        let tmp = tempfile::tempdir().unwrap();
        seed_wal_on(&host, tmp.path());
        let runtime = Arc::new(PinnedDirectory::open(tmp.path()).unwrap().unwrap());
        let (owner, owner_ticket) = host.armed(|| {
            BoundSqliteConnection::open_existing(runtime, OsStr::new("scheduler.sqlite3")).unwrap()
        });
        host.scope(|| owner.establish_wal_binding().unwrap());
        let main_alias = tmp.path().join("alias.sqlite3");
        std::fs::hard_link(tmp.path().join("scheduler.sqlite3"), &main_alias).unwrap();
        let mut drained = host.scope(|| {
            owner
                .capture_projection_retirement([0x51; 16])
                .unwrap()
                .drain_sql()
                .unwrap()
        });
        assert_external_write_busy(&main_alias);
        for role in SqliteRole::all() {
            let facts = drained.role_facts().roles[role as usize].clone();
            assert!(facts.original.is_some());
            assert!(facts.original.as_ref().unwrap().content_sha256.is_some());
            let alias = tmp.path().join(format!("role-{}.sqlite3", role.label()));
            std::fs::hard_link(tmp.path().join(role.public_name()), &alias).unwrap();
            let refused = armed_stock_refusal(&host, &alias);
            assert_eq!(
                refused.sqlite_error_code(),
                Some(STOCK_ALIAS_REFUSAL),
                "{refused}"
            );
            assert_external_write_busy(&main_alias); // refusal close must not release main barrier
            assert_eq!(
                host.scope(|| drained.quarantine_role(role).unwrap()),
                SqliteRoleMutationOutcome::QuarantinedDurable
            );
            assert!(!tmp.path().join(role.public_name()).exists());
            let refused = armed_stock_refusal(&host, &tmp.path().join(&facts.quarantine_name));
            assert_eq!(
                refused.sqlite_error_code(),
                Some(STOCK_ALIAS_REFUSAL),
                "{refused}"
            );
            assert_eq!(
                host.scope(|| drained.purge_role(role).unwrap()),
                SqliteRoleMutationOutcome::RemovedDurable
            );
            assert!(!tmp.path().join(&facts.quarantine_name).exists());
            assert!(!tmp.path().join(&facts.purge_name).exists());
            assert_external_write_busy(&main_alias);
        }
        drained.release().unwrap();
        host.retire_raw(owner_ticket);
        let (connection, ticket) = armed_stock(&host, &main_alias);
        host.scope(|| {
            connection.busy_timeout(std::time::Duration::ZERO).unwrap();
            connection
                .execute_batch("BEGIN IMMEDIATE; ROLLBACK;")
                .unwrap();
        });
        close_armed(&host, connection, ticket);
    }
    #[test]
    #[cfg(target_os = "linux")]
    fn retirement_reader_probe_child() {
        let Some(path) = std::env::var_os("LILLUX_SQLITE_RETIREMENT_READER") else {
            return;
        };
        let host = crate::test_host::TestNativeHost::acquire();
        let ready = std::env::var_os("LILLUX_SQLITE_RETIREMENT_READY").unwrap();
        let (connection, ticket) = armed_stock(&host, std::path::Path::new(&path));
        host.scope(|| {
            connection.busy_timeout(std::time::Duration::ZERO).unwrap();
            connection
                .execute_batch("BEGIN; SELECT value FROM custody;")
                .unwrap();
        });
        std::fs::write(ready, b"reader holds actual SQLite transaction").unwrap();
        // Bounded fallback even if supervising parent crashes.
        std::thread::sleep(std::time::Duration::from_secs(20));
        host.scope(|| connection.execute_batch("ROLLBACK;").unwrap());
        close_armed(&host, connection, ticket);
    }
    #[test]
    #[cfg(target_os = "linux")]
    fn external_reader_holds_drain_without_role_effect_then_same_owner_retries() {
        let host = crate::test_host::TestNativeHost::acquire();
        struct Supervised(std::process::Child);
        impl Drop for Supervised {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let tmp = tempfile::tempdir().unwrap();
        seed_wal_on(&host, tmp.path());
        let runtime = Arc::new(PinnedDirectory::open(tmp.path()).unwrap().unwrap());
        let (owner, ticket) = armed_bound(&host, &runtime);
        host.scope(|| owner.establish_wal_binding().unwrap());
        let ready = tmp.path().join("reader-ready");
        let mut child = Supervised(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "tests::retirement_reader_probe_child",
                    "--nocapture",
                ])
                .env(
                    "LILLUX_SQLITE_RETIREMENT_READER",
                    tmp.path().join("scheduler.sqlite3"),
                )
                .env("LILLUX_SQLITE_RETIREMENT_READY", &ready)
                .spawn()
                .unwrap(),
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !ready.exists() {
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "reader probe exited before acquiring transaction"
            );
            assert!(
                std::time::Instant::now() < deadline,
                "reader acquisition timed out"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let captured = host.scope(|| owner.capture_projection_retirement([0x71; 16]).unwrap());
        let coordinates = captured.captured_roles().clone();
        let failure = host.scope(|| {
            captured
                .drain_sql()
                .expect_err("external SHM read lock holds retirement")
        });
        assert!(matches!(
            failure.error(),
            SqliteProjectionRetirementError::Barrier(_)
        ));
        assert!(matches!(
            failure.owner(),
            SqliteProjectionRetirementOwner::Drained(_)
        ));
        for role in &coordinates.roles {
            assert!(tmp.path().join(role.role.public_name()).exists());
            assert!(!tmp.path().join(&role.quarantine_name).exists());
            assert!(!tmp.path().join(&role.purge_name).exists());
        }
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        let drained = host.scope(|| failure.retry_drain().unwrap());
        // The drain closed the SQL connection this ticket opened.
        host.retire_raw(ticket);
        assert_eq!(drained.role_facts().generation, [0x71; 16]);
        assert_external_write_busy(&tmp.path().join("scheduler.sqlite3"));
        drained.abort().unwrap();
    }
    #[test]
    #[cfg(target_os = "linux")]
    fn stock_peer_descriptor_parked_by_live_owner_refuses_capture_until_owner_closes() {
        let host = crate::test_host::TestNativeHost::acquire();
        let tmp = tempfile::tempdir().unwrap();
        seed_wal_on(&host, tmp.path());
        let runtime = Arc::new(PinnedDirectory::open(tmp.path()).unwrap().unwrap());
        let (owner, ticket) = armed_bound(&host, &runtime);
        host.scope(|| owner.establish_wal_binding().unwrap());
        // 1. A live in-process stock peer on the main role refuses capture.
        let (peer, peer_ticket) = armed_stock(&host, &tmp.path().join("scheduler.sqlite3"));
        host.scope(|| {
            peer.execute_batch("BEGIN; SELECT value FROM custody;")
                .unwrap()
        });
        let failure = host.scope(|| {
            owner
                .capture_projection_retirement([0x61; 16])
                .err()
                .expect("physical peer refuses capture")
        });
        assert!(matches!(
            failure.owner(),
            SqliteProjectionRetirementOwner::Uncaptured(_)
        ));
        host.scope(|| peer.execute_batch("ROLLBACK;").unwrap());
        close_armed(&host, peer, peer_ticket);
        // 2. Closing the peer does not close its main-role descriptor. A WAL
        // connection keeps a SHARED lock on its main file for its whole life,
        // so the live owner holds a POSIX lock on the main inode. POSIX locks
        // belong to the process and any close() of that inode drops them, so
        // SQLite's unixClose parks the peer's main descriptor on the inode's
        // pending list instead of closing it, until the inode's last lock is
        // released. The gate still tracks that open descriptor as a foreign
        // descriptor on the role, and capture fails closed while it exists.
        let failure = match host.scope(|| failure.retry_capture([0x61; 16])) {
            Ok(_) => panic!("capture admitted while the peer's main descriptor is parked"),
            Err(failure) => failure,
        };
        assert!(matches!(
            failure.error(),
            SqliteProjectionRetirementError::Capture(_)
        ));
        // 3. Closing the owner releases its lock, and SQLite closes the parked
        // descriptor with it. A fresh owner then captures, drains and aborts
        // the role cleanly.
        let SqliteProjectionRetirementFailure {
            owner: SqliteProjectionRetirementOwner::Uncaptured(owner),
            ..
        } = failure
        else {
            panic!("refused capture keeps the uncaptured owner");
        };
        close_bound(&host, owner, ticket);
        let (owner, ticket) = armed_bound(&host, &runtime);
        host.scope(|| owner.establish_wal_binding().unwrap());
        let drained = host.scope(|| {
            owner
                .capture_projection_retirement([0x62; 16])
                .unwrap()
                .drain_sql()
                .unwrap()
        });
        // The drain closed the SQL connection this ticket opened.
        host.retire_raw(ticket);
        drained.abort().unwrap();
    }
    #[test]
    #[cfg(target_os = "linux")]
    fn absent_main_refuses_without_creating_database_or_sidecars() {
        let host = crate::test_host::TestNativeHost::acquire();
        let tmp = tempfile::TempDir::new().unwrap();
        let pin = Arc::new(PinnedDirectory::open(tmp.path()).unwrap().unwrap());
        armed_bound_refusal(&host, &pin, "scheduler.sqlite3");
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
    }
    #[test]
    #[cfg(target_os = "linux")]
    fn journal_sources_refuse_without_mutation() {
        let host = crate::test_host::TestNativeHost::acquire();
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("scheduler.sqlite3"), b"retained source").unwrap();
        std::fs::write(tmp.path().join("scheduler.sqlite3-journal"), b"recovery").unwrap();
        let pin = Arc::new(PinnedDirectory::open(tmp.path()).unwrap().unwrap());
        armed_bound_refusal(&host, &pin, "scheduler.sqlite3");
        assert_eq!(
            std::fs::read(tmp.path().join("scheduler.sqlite3")).unwrap(),
            b"retained source"
        );
        assert_eq!(
            std::fs::read(tmp.path().join("scheduler.sqlite3-journal")).unwrap(),
            b"recovery"
        );
    }
    #[cfg(target_os = "linux")]
    #[cfg(target_os = "linux")]
    fn resolution_fixture(
        host: &crate::test_host::TestNativeHost,
    ) -> (tempfile::TempDir, DrainedSqliteProjection) {
        let tmp = tempfile::tempdir().unwrap();
        seed_wal_on(host, tmp.path());
        let runtime = Arc::new(PinnedDirectory::open(tmp.path()).unwrap().unwrap());
        let (owner, ticket) = host.armed(|| {
            BoundSqliteConnection::open_existing(runtime, OsStr::new("scheduler.sqlite3")).unwrap()
        });
        let drained = host.scope(|| {
            owner.establish_wal_binding().unwrap();
            owner
                .capture_projection_retirement([0xc1; 16])
                .unwrap()
                .drain_sql()
                .unwrap()
        });
        // The drain closed the SQL connection this ticket opened.
        host.retire_raw(ticket);
        (tmp, drained)
    }
    #[cfg(target_os = "linux")]
    fn simulate_held_quarantine(drained: &mut DrainedSqliteProjection) {
        use lillux::secure_fs::{PlannedQuarantineDisposition as D, PlannedQuarantineStage as S};
        drained.quarantine_role(SqliteRole::Main).unwrap();
        let error = SqliteRoleMutationError {
            role: SqliteRole::Main,
            disposition: D::DurabilityUncertain,
            stage: S::QuarantinedOriginal,
            message: "test: original rename succeeded; ack lost".into(),
        };
        drained.retain_role_failure(
            SqliteRole::Main,
            SqliteRoleMutationOperation::Quarantine,
            &error,
            true,
        );
        drained.states[0] = RetirementRoleState::RecoveryRequired;
    }
    #[cfg(target_os = "linux")]
    fn simulate_held_purge_at_u(drained: &mut DrainedSqliteProjection) {
        use lillux::secure_fs::{PlannedQuarantineDisposition as D, PlannedQuarantineStage as S};
        drained.quarantine_role(SqliteRole::Main).unwrap();
        drained
            .binding
            .as_ref()
            .unwrap()
            .with_retirement_role(0, |file| {
                drained
                    .runtime
                    .quarantine_if_same_planned_atomic(
                        OsStr::new(&drained.facts.roles[0].quarantine_name),
                        file.unwrap(),
                        OsStr::new(&drained.facts.roles[0].purge_name),
                    )
                    .map_err(anyhow::Error::new)
            })
            .unwrap();
        let error = SqliteRoleMutationError {
            role: SqliteRole::Main,
            disposition: D::DurabilityUncertain,
            stage: S::QuarantinedOriginal,
            message: "test: Q-to-U succeeded; unlink not attempted".into(),
        };
        drained.retain_role_failure(
            SqliteRole::Main,
            SqliteRoleMutationOperation::Purge,
            &error,
            true,
        );
        drained.states[0] = RetirementRoleState::RecoveryRequired;
    }
    #[cfg(target_os = "linux")]
    fn resolution_request(
        drained: &mut DrainedSqliteProjection,
        ordinal: u8,
        action: SqliteRoleResolutionAction,
    ) -> SqliteRoleResolutionRequest {
        SqliteRoleResolutionRequest {
            expected: drained.observe_role_resolution(SqliteRole::Main).unwrap(),
            ordinal,
            action,
        }
    }
    #[cfg(target_os = "linux")]
    fn finish_resolution_fixture(mut drained: DrainedSqliteProjection) {
        if matches!(drained.states[0], RetirementRoleState::Quarantined) {
            drained.purge_role(SqliteRole::Main).unwrap();
        }
        for role in [SqliteRole::Wal, SqliteRole::Shm] {
            drained.quarantine_role(role).unwrap();
            drained.purge_role(role).unwrap();
        }
        drained.release().unwrap();
    }
    #[test]
    #[cfg(target_os = "linux")]
    fn resolution_sync_failure_retains_testimony_and_memoizes_success() {
        let host = crate::test_host::TestNativeHost::acquire();
        let (_tmp, mut drained) = resolution_fixture(&host);
        host.scope(|| {
            simulate_held_quarantine(&mut drained);
            let original_error = drained.role_failures[0].original.clone().unwrap();
            drained.role_failures[0]
                .latest
                .as_mut()
                .unwrap()
                .error
                .disposition = lillux::secure_fs::PlannedQuarantineDisposition::BeforeCommit;
            let contradictory =
                resolution_request(&mut drained, 0, SqliteRoleResolutionAction::SyncQuarantine);
            assert!(matches!(
                drained.resolve_role(contradictory),
                Err(SqliteRoleResolutionError::Refused(_))
            ));
            drained.role_failures[0]
                .latest
                .as_mut()
                .unwrap()
                .error
                .disposition = lillux::secure_fs::PlannedQuarantineDisposition::DurabilityUncertain;

            let first =
                resolution_request(&mut drained, 0, SqliteRoleResolutionAction::SyncQuarantine);
            FAIL_RESOLUTION_SYNC.with(|flag| flag.set(true));
            assert!(matches!(
                drained.resolve_role(first.clone()),
                Err(SqliteRoleResolutionError::Held(_))
            ));
            assert_eq!(
                drained.role_failures[0].original.as_ref(),
                Some(&original_error)
            );
            assert!(
                matches!(drained.quarantine_role(SqliteRole::Main), Err(error)
                if error.disposition == original_error.error.disposition)
            );
            let second =
                resolution_request(&mut drained, 1, SqliteRoleResolutionAction::SyncQuarantine);
            assert_eq!(second.expected.resolution_coordinates.len(), 1);
            assert!(matches!(
                second.expected.resolution_coordinates[0].observation,
                SqliteRoleCoordinateObservation::Absent
            ));
            assert_eq!(
                drained.resolve_role(second.clone()).unwrap(),
                SqliteRoleResolutionOutcome::QuarantinedDurable
            );
            // Arm sync fault: an acknowledgement retry must NOT call sync again.
            FAIL_RESOLUTION_SYNC.with(|flag| flag.set(true));
            assert_eq!(
                drained.resolve_role(second).unwrap(),
                SqliteRoleResolutionOutcome::QuarantinedDurable
            );
            assert!(FAIL_RESOLUTION_SYNC.with(|flag| flag.replace(false)));
            assert!(matches!(
                drained.resolve_role(first),
                Err(SqliteRoleResolutionError::Held(_))
            ));
            finish_resolution_fixture(drained);
        });
    }
    #[test]
    #[cfg(target_os = "linux")]
    fn resolution_partial_move_tracks_active_r_and_finishes_without_u_replay() {
        let host = crate::test_host::TestNativeHost::acquire();
        let (tmp, mut drained) = resolution_fixture(&host);
        host.scope(|| {
            simulate_held_purge_at_u(&mut drained);
            drained.role_failures[0]
                .latest
                .as_mut()
                .unwrap()
                .error
                .disposition = lillux::secure_fs::PlannedQuarantineDisposition::BeforeCommit;
            let contradictory =
                resolution_request(&mut drained, 0, SqliteRoleResolutionAction::FinishPurgeAtU);
            assert!(matches!(
                drained.resolve_role(contradictory),
                Err(SqliteRoleResolutionError::Refused(_))
            ));
            drained.role_failures[0]
                .latest
                .as_mut()
                .unwrap()
                .error
                .disposition = lillux::secure_fs::PlannedQuarantineDisposition::DurabilityUncertain;

            let first =
                resolution_request(&mut drained, 0, SqliteRoleResolutionAction::FinishPurgeAtU);
            FAIL_RESOLUTION_AFTER_MOVE.with(|flag| flag.set(true));
            assert!(matches!(
                drained.resolve_role(first.clone()),
                Err(SqliteRoleResolutionError::Held(_))
            ));
            let after = drained.observe_role_resolution(SqliteRole::Main).unwrap();
            assert!(matches!(
                after.purge,
                SqliteRoleCoordinateObservation::Absent
            ));
            assert_eq!(after.resolution_coordinates.len(), 1);
            assert!(matches!(
                after.resolution_coordinates[0].observation,
                SqliteRoleCoordinateObservation::SameOriginal { .. }
            ));
            let r = tmp.path().join(&after.resolution_coordinates[0].name);
            assert!(r.is_file());
            assert!(matches!(
                drained.resolve_role(first),
                Err(SqliteRoleResolutionError::Held(_))
            ));
            let wrong =
                resolution_request(&mut drained, 1, SqliteRoleResolutionAction::FinishPurgeAtU);
            assert!(matches!(
                drained.resolve_role(wrong),
                Err(SqliteRoleResolutionError::Refused(_))
            ));
            drained.resolutions[0][0]
                .attempt
                .effect_failure
                .as_mut()
                .unwrap()
                .disposition = lillux::secure_fs::PlannedQuarantineDisposition::NamespaceChanged;
            let contradictory = resolution_request(
                &mut drained,
                1,
                SqliteRoleResolutionAction::FinishPurgeAtResolution { source_ordinal: 0 },
            );
            assert!(matches!(
                drained.resolve_role(contradictory),
                Err(SqliteRoleResolutionError::Refused(_))
            ));
            assert!(r.is_file());
            drained.resolutions[0][0]
                .attempt
                .effect_failure
                .as_mut()
                .unwrap()
                .disposition = lillux::secure_fs::PlannedQuarantineDisposition::DurabilityUncertain;
            let next = resolution_request(
                &mut drained,
                1,
                SqliteRoleResolutionAction::FinishPurgeAtResolution { source_ordinal: 0 },
            );
            assert_eq!(
                drained.resolve_role(next.clone()).unwrap(),
                SqliteRoleResolutionOutcome::RemovedDurable
            );
            assert!(!r.exists());
            let after = drained.observe_role_resolution(SqliteRole::Main).unwrap();
            assert_eq!(after.resolution_coordinates.len(), 2);
            assert!(
                after
                    .resolution_coordinates
                    .iter()
                    .all(|coordinate| matches!(
                        coordinate.observation,
                        SqliteRoleCoordinateObservation::Absent
                    ))
            );
            assert_eq!(
                drained.resolve_role(next).unwrap(),
                SqliteRoleResolutionOutcome::RemovedDurable
            );
            finish_resolution_fixture(drained);
        });
    }
    #[test]
    #[cfg(target_os = "linux")]
    fn resolution_unlink_testimony_survives_sync_failure_and_budget_exhaustion() {
        let host = crate::test_host::TestNativeHost::acquire();
        let (_positive_tmp, mut positive) = resolution_fixture(&host);
        host.scope(|| {
            simulate_held_purge_at_u(&mut positive);
            let request =
                resolution_request(&mut positive, 0, SqliteRoleResolutionAction::FinishPurgeAtU);
            FAIL_RESOLUTION_AFTER_UNLINK.with(|flag| flag.set(true));
            assert!(matches!(
                positive.resolve_role(request),
                Err(SqliteRoleResolutionError::Held(_))
            ));
            positive.resolutions[0][0]
                .attempt
                .effect_failure
                .as_mut()
                .unwrap()
                .disposition = lillux::secure_fs::PlannedQuarantineDisposition::BeforeCommit;
            let contradictory =
                resolution_request(&mut positive, 1, SqliteRoleResolutionAction::SyncRemoved);
            assert!(matches!(
                positive.resolve_role(contradictory),
                Err(SqliteRoleResolutionError::Refused(_))
            ));
            positive.resolutions[0][0]
                .attempt
                .effect_failure
                .as_mut()
                .unwrap()
                .disposition = lillux::secure_fs::PlannedQuarantineDisposition::DurabilityUncertain;
            let request =
                resolution_request(&mut positive, 1, SqliteRoleResolutionAction::SyncRemoved);
            FAIL_RESOLUTION_SYNC.with(|flag| flag.set(true));
            assert!(matches!(
                positive.resolve_role(request),
                Err(SqliteRoleResolutionError::Held(_))
            ));
            let request =
                resolution_request(&mut positive, 2, SqliteRoleResolutionAction::SyncRemoved);
            assert_eq!(
                positive.resolve_role(request.clone()).unwrap(),
                SqliteRoleResolutionOutcome::RemovedDurable
            );
            FAIL_RESOLUTION_SYNC.with(|flag| flag.set(true));
            assert_eq!(
                positive.resolve_role(request).unwrap(),
                SqliteRoleResolutionOutcome::RemovedDurable
            );
            assert!(FAIL_RESOLUTION_SYNC.with(|flag| flag.replace(false)));
            finish_resolution_fixture(positive);
        });
        let (_tmp, mut drained) = resolution_fixture(&host);
        host.scope(|| {
            simulate_held_purge_at_u(&mut drained);
            let first =
                resolution_request(&mut drained, 0, SqliteRoleResolutionAction::FinishPurgeAtU);
            FAIL_RESOLUTION_AFTER_UNLINK.with(|flag| flag.set(true));
            assert!(matches!(
                drained.resolve_role(first),
                Err(SqliteRoleResolutionError::Held(_))
            ));
            for ordinal in 1..4 {
                let request = resolution_request(
                    &mut drained,
                    ordinal,
                    SqliteRoleResolutionAction::SyncRemoved,
                );
                assert_eq!(request.expected.original_link_count, Some(0));
                FAIL_RESOLUTION_SYNC.with(|flag| flag.set(true));
                assert!(matches!(
                    drained.resolve_role(request),
                    Err(SqliteRoleResolutionError::Held(_))
                ));
            }
            let request =
                resolution_request(&mut drained, 4, SqliteRoleResolutionAction::SyncRemoved);
            assert_eq!(
                request.expected.resolution_coordinates.len(),
                SQLITE_ROLE_RESOLUTION_LIMIT
            );
            assert!(matches!(
                drained.resolve_role(request),
                Err(SqliteRoleResolutionError::BudgetExhausted)
            ));
            assert!(matches!(
                drained.states[0],
                RetirementRoleState::RecoveryRequired
            ));
        });
        let refused = armed_stock_refusal(&host, &_tmp.path().join("scheduler.sqlite3"));
        assert_eq!(
            refused.sqlite_error_code(),
            Some(STOCK_ALIAS_REFUSAL),
            "{refused}"
        );
        host.scope(|| {
            let failure = drained.release().unwrap_err();
            let SqliteProjectionRetirementOwner::Drained(mut drained) = failure.owner else {
                panic!("same drained owner")
            };
            // Test-only teardown after proving refusal: original is actually unlinked.
            // This bypass is never part of the public resolution API.
            drained.states[0] = RetirementRoleState::Removed;
            finish_resolution_fixture(drained);
        });
    }
    #[test]
    #[cfg(target_os = "linux")]
    fn resolution_refuses_foreign_coordinates_and_absence_without_unlink_testimony() {
        let host = crate::test_host::TestNativeHost::acquire();
        let (tmp, mut drained) = resolution_fixture(&host);
        host.scope(|| {
            simulate_held_quarantine(&mut drained);
            let request =
                resolution_request(&mut drained, 0, SqliteRoleResolutionAction::SyncQuarantine);
            let foreign = tmp.path().join("scheduler.sqlite3");
            std::fs::write(&foreign, b"foreign").unwrap();
            assert!(matches!(
                drained.resolve_role(request),
                Err(SqliteRoleResolutionError::Refused(_))
            ));
            assert_eq!(std::fs::read(&foreign).unwrap(), b"foreign");
            std::fs::remove_file(&foreign).unwrap(); // test owns foreign fixture
            // Test-only removal creates all-absent+nlink0 but NO original unlink testimony.
            std::fs::remove_file(tmp.path().join(&drained.facts.roles[0].quarantine_name)).unwrap();
            let request =
                resolution_request(&mut drained, 0, SqliteRoleResolutionAction::SyncRemoved);
            assert_eq!(request.expected.original_link_count, Some(0));
            assert!(matches!(
                drained.resolve_role(request),
                Err(SqliteRoleResolutionError::Refused(_))
            ));
            assert!(drained.resolutions[0].is_empty());
            let failure = drained.abort().unwrap_err();
            let SqliteProjectionRetirementOwner::Drained(mut drained) = failure.owner else {
                panic!("same drained owner")
            };
            // Test-only teardown; the public resolver correctly refused fabricated testimony.
            drained.states[0] = RetirementRoleState::Removed;
            finish_resolution_fixture(drained);
        });
    }
    #[test]
    #[cfg(target_os = "linux")]
    fn resolution_rearm_refuses_namespacechanged_restore_and_unknown_bridge_testimony() {
        let host = crate::test_host::TestNativeHost::acquire();
        use lillux::secure_fs::{PlannedQuarantineDisposition as D, PlannedQuarantineStage as S};
        let (_tmp, mut drained) = resolution_fixture(&host);
        host.scope(|| {
            let error = SqliteRoleMutationError {
                role: SqliteRole::Main,
                disposition: D::NamespaceChanged,
                stage: S::RestoredUnexpected,
                message: "contradictory fixture".into(),
            };
            drained.states[0] = RetirementRoleState::RecoveryRequired;
            drained.retain_role_failure(
                SqliteRole::Main,
                SqliteRoleMutationOperation::Quarantine,
                &error,
                true,
            );
            let request =
                resolution_request(&mut drained, 0, SqliteRoleResolutionAction::RearmOriginal);
            assert!(matches!(
                drained.resolve_role(request),
                Err(SqliteRoleResolutionError::Refused(_))
            ));
            let error = SqliteRoleMutationError {
                disposition: D::BeforeCommit,
                stage: S::Unchanged,
                ..error
            };
            drained.retain_role_failure(
                SqliteRole::Main,
                SqliteRoleMutationOperation::Quarantine,
                &error,
                false,
            );
            let request =
                resolution_request(&mut drained, 0, SqliteRoleResolutionAction::RearmOriginal);
            assert!(matches!(
                drained.resolve_role(request),
                Err(SqliteRoleResolutionError::Refused(_))
            ));
            drained.retain_role_failure(
                SqliteRole::Main,
                SqliteRoleMutationOperation::Quarantine,
                &error,
                true,
            );
            let request =
                resolution_request(&mut drained, 0, SqliteRoleResolutionAction::RearmOriginal);
            assert_eq!(
                drained.resolve_role(request).unwrap(),
                SqliteRoleResolutionOutcome::RearmedOriginal
            );
            drained.quarantine_role(SqliteRole::Main).unwrap();
            drained.purge_role(SqliteRole::Main).unwrap();
            drained.states[0] = RetirementRoleState::RecoveryRequired;
            let contradictory = SqliteRoleMutationError {
                role: SqliteRole::Main,
                disposition: D::BeforeCommit,
                stage: S::UnlinkedOriginal,
                message: "contradictory ordinary unlink disposition".into(),
            };
            drained.retain_role_failure(
                SqliteRole::Main,
                SqliteRoleMutationOperation::Purge,
                &contradictory,
                true,
            );
            let request =
                resolution_request(&mut drained, 1, SqliteRoleResolutionAction::SyncRemoved);
            assert!(matches!(
                drained.resolve_role(request),
                Err(SqliteRoleResolutionError::Refused(_))
            ));
            let observed = SqliteRoleMutationError {
                disposition: D::DurabilityUncertain,
                ..contradictory
            };
            drained.retain_role_failure(
                SqliteRole::Main,
                SqliteRoleMutationOperation::Purge,
                &observed,
                true,
            );
            let request =
                resolution_request(&mut drained, 1, SqliteRoleResolutionAction::SyncRemoved);
            assert_eq!(
                drained.resolve_role(request).unwrap(),
                SqliteRoleResolutionOutcome::RemovedDurable
            );
            finish_resolution_fixture(drained);
        });
    }
    #[cfg(target_os = "linux")]
    pub(super) fn stopped_plan_fixture(
        runtime: &PinnedDirectory,
        root: &std::path::Path,
        sources: [StoppedSqliteRoleSource; 3],
    ) -> StoppedSqliteProjectionPlan {
        let mut plan = StoppedSqliteProjectionPlan {
            generation: [4; 16],
            generations: vec![[1; 16], [2; 16], [3; 16], [4; 16]],
            runtime_identity: runtime.identity().unwrap(),
            roles: std::array::from_fn(|i| StoppedSqliteRolePlan {
                source: sources[i],
                current: None,
            }),
        };
        for role in SqliteRole::all() {
            if let Some(name) = plan.source_name(role) {
                let file = std::fs::File::open(root.join(name)).unwrap();
                let metadata = file.metadata().unwrap();
                use std::os::unix::fs::MetadataExt;
                let (digest, after) =
                    lillux::digest_open_regular_file_stable_exact(&file, metadata.len()).unwrap();
                plan.roles[role as usize].current = Some(SqliteRoleIdentity {
                    device: after.dev(),
                    inode: after.ino(),
                    byte_length: after.len(),
                    content_sha256: Some(digest),
                });
            }
        }
        plan
    }
    #[test]
    #[cfg(target_os = "linux")]
    fn stopped_q_u_r_sources_move_to_new_q_without_canonical_sql_open() {
        let host = crate::test_host::TestNativeHost::acquire();
        for source in [
            StoppedSqliteRoleSource::Quarantine {
                generation: [1; 16],
            },
            StoppedSqliteRoleSource::Purge {
                generation: [2; 16],
            },
            StoppedSqliteRoleSource::Resolution {
                generation: [3; 16],
                ordinal: 2,
            },
        ] {
            let tmp = tempfile::tempdir().unwrap();
            seed_wal_on(&host, tmp.path());
            let runtime = Arc::new(PinnedDirectory::open(tmp.path()).unwrap().unwrap());
            let mut skeleton = stopped_plan_fixture(
                &runtime,
                tmp.path(),
                [
                    StoppedSqliteRoleSource::Canonical,
                    StoppedSqliteRoleSource::Absent,
                    StoppedSqliteRoleSource::Absent,
                ],
            );
            skeleton.roles[0].source = source;
            let selected = skeleton.source_name(SqliteRole::Main).unwrap();
            std::fs::rename(
                tmp.path().join("scheduler.sqlite3"),
                tmp.path().join(&selected),
            )
            .unwrap();
            let plan = stopped_plan_fixture(
                &runtime,
                tmp.path(),
                [
                    source,
                    StoppedSqliteRoleSource::Absent,
                    StoppedSqliteRoleSource::Absent,
                ],
            );
            let mut owner = host.cleanup_scope(|| {
                DrainedSqliteProjection::capture_stopped_projection(runtime, plan.clone()).unwrap()
            });
            let observed = host.scope(|| owner.observe_stopped_projection().unwrap());
            assert!(observed.ready);
            assert_eq!(observed.plan, plan);
            assert_eq!(observed.coordinates.len(), 100);
            assert_eq!(
                observed
                    .coordinates
                    .iter()
                    .filter(|item| item.role.is_none())
                    .count(),
                25
            );
            assert!(!tmp.path().join("scheduler.sqlite3").exists());
            assert!(tmp.path().join(&selected).exists());
            host.scope(|| owner.quarantine_role(SqliteRole::Main).unwrap());
            assert!(!tmp.path().join(&selected).exists());
            assert!(
                tmp.path()
                    .join(&owner.role_facts().roles[0].quarantine_name)
                    .exists()
            );
            assert!(!tmp.path().join("scheduler.sqlite3").exists());
            host.scope(|| {
                owner.purge_role(SqliteRole::Main).unwrap();
                owner.release().unwrap();
            });
        }
    }
    #[test]
    #[cfg(target_os = "linux")]
    fn stopped_all_derived_journals_and_temporary_artifacts_refuse_before_intake() {
        let _serial = crate::test_host::serial_guard();
        let tmp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(PinnedDirectory::open(tmp.path()).unwrap().unwrap());
        let plan = stopped_plan_fixture(&runtime, tmp.path(), [StoppedSqliteRoleSource::Absent; 3]);
        let mut names = plan
            .coordinates()
            .into_iter()
            .filter(|(role, _)| role.is_none())
            .map(|(_, name)| name)
            .collect::<Vec<_>>();
        assert_eq!(names.len(), 25);
        names.extend([
            ".secure.tmp.interrupted".into(),
            ".secure.tmp.4242.7".into(),
            format!(".secure.tmp.{}.4242.8", crate::SQLITE_TEMP_PURPOSE.as_str()),
            "etilqs_interrupted".into(),
            "scheduler-retirement.unknown".into(),
            "scheduler-retirement.role.01010101010101010101010101010101.main.quarantine-wal".into(),
        ]);
        for name in names {
            let path = tmp.path().join(name);
            std::fs::write(&path, b"preserve").unwrap();
            let failure = DrainedSqliteProjection::capture_stopped_projection(
                Arc::clone(&runtime),
                plan.clone(),
            )
            .unwrap_err();
            assert!(matches!(
                failure.owner(),
                SqliteProjectionRetirementOwner::UnacquiredStopped { .. }
            ));
            failure.abort().unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), b"preserve");
            assert!(!tmp.path().join("scheduler.sqlite3").exists());
            std::fs::remove_file(path).unwrap();
        }
    }
    #[test]
    #[cfg(target_os = "linux")]
    fn stopped_artifact_scan_leaves_other_owners_secure_temporaries() {
        let _serial = crate::test_host::serial_guard();
        let tmp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(PinnedDirectory::open(tmp.path()).unwrap().unwrap());
        let plan = stopped_plan_fixture(&runtime, tmp.path(), [StoppedSqliteRoleSource::Absent; 3]);
        scan_stopped_artifacts(&runtime, &plan).unwrap();
        for name in [".secure.tmp.scheduler.4242.1", ".secure.tmp.reset.4242.2"] {
            std::fs::write(tmp.path().join(name), b"another owner's debris").unwrap();
        }
        scan_stopped_artifacts(&runtime, &plan).unwrap();
        std::fs::write(tmp.path().join(".secure.tmp.4242.3"), b"unattributed").unwrap();
        let error = scan_stopped_artifacts(&runtime, &plan).unwrap_err();
        assert!(
            error.to_string().contains("without an exact owner token"),
            "{error:#}"
        );
        for name in [
            ".secure.tmp.scheduler.4242.1",
            ".secure.tmp.reset.4242.2",
            ".secure.tmp.4242.3",
        ] {
            assert!(tmp.path().join(name).is_file(), "{name}");
        }
    }
    #[test]
    #[cfg(target_os = "linux")]
    fn stopped_independent_sidecars_and_all_absent_never_construct_main() {
        let host = crate::test_host::TestNativeHost::acquire();
        for present in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let runtime = Arc::new(PinnedDirectory::open(tmp.path()).unwrap().unwrap());
            let sources = if present {
                std::fs::write(
                    tmp.path().join("scheduler.sqlite3-wal"),
                    b"authorized-current-wal",
                )
                .unwrap();
                std::fs::write(
                    tmp.path().join("scheduler.sqlite3-shm"),
                    b"authorized-current-shm",
                )
                .unwrap();
                [
                    StoppedSqliteRoleSource::Absent,
                    StoppedSqliteRoleSource::Canonical,
                    StoppedSqliteRoleSource::Canonical,
                ]
            } else {
                [StoppedSqliteRoleSource::Absent; 3]
            };
            let plan = stopped_plan_fixture(&runtime, tmp.path(), sources);
            let mut owner = host.cleanup_scope(|| {
                DrainedSqliteProjection::capture_stopped_projection(runtime, plan).unwrap()
            });
            assert!(owner.role_facts().roles[0].original.is_none());
            assert!(!tmp.path().join("scheduler.sqlite3").exists());
            host.scope(|| {
                for role in SqliteRole::all() {
                    owner.quarantine_role(role).unwrap();
                    owner.purge_role(role).unwrap();
                }
                owner.release().unwrap();
            });
            assert!(!tmp.path().join("scheduler.sqlite3").exists());
        }
    }
    #[test]
    #[cfg(target_os = "linux")]
    fn stopped_pending_alias_admission_refuses_while_peer_close_allows_same_pin_retry() {
        let host = crate::test_host::TestNativeHost::acquire();
        let tmp = tempfile::tempdir().unwrap();
        seed_wal_on(&host, tmp.path());
        let runtime = Arc::new(PinnedDirectory::open(tmp.path()).unwrap().unwrap());
        let source = StoppedSqliteRoleSource::Quarantine {
            generation: [1; 16],
        };
        let mut skeleton = stopped_plan_fixture(
            &runtime,
            tmp.path(),
            [
                StoppedSqliteRoleSource::Canonical,
                StoppedSqliteRoleSource::Absent,
                StoppedSqliteRoleSource::Absent,
            ],
        );
        skeleton.roles[0].source = source;
        let selected = skeleton.source_name(SqliteRole::Main).unwrap();
        std::fs::rename(
            tmp.path().join("scheduler.sqlite3"),
            tmp.path().join(&selected),
        )
        .unwrap();
        let plan = stopped_plan_fixture(
            &runtime,
            tmp.path(),
            [
                source,
                StoppedSqliteRoleSource::Absent,
                StoppedSqliteRoleSource::Absent,
            ],
        );
        let peer_path = tmp.path().join("peer.sqlite3");
        std::fs::hard_link(tmp.path().join(&selected), &peer_path).unwrap();
        let (peer, peer_ticket) = armed_stock(&host, &peer_path);
        host.scope(|| peer.execute_batch("BEGIN; SELECT * FROM custody;").unwrap());
        let (cached_peer, cached_ticket) = armed_stock(&host, &peer_path);
        host.scope(|| {
            cached_peer
                .query_row("SELECT value FROM custody", [], |row| row.get::<_, i64>(0))
                .unwrap()
        });
        // Closing this second Unix handle while the first holds shared locks
        // leaves a cached/unused descriptor: pending audit must include it.
        close_armed(&host, cached_peer, cached_ticket);

        let failure = host.cleanup_scope(|| {
            DrainedSqliteProjection::capture_stopped_projection(runtime, plan.clone()).unwrap_err()
        });
        let before = host.scope(|| match failure.owner() {
            SqliteProjectionRetirementOwner::Drained(owner) => {
                owner.observe_stopped_projection().unwrap()
            }
            _ => panic!("same partial context"),
        });
        assert!(!before.ready);
        assert_eq!(
            before.roles[0].as_ref().unwrap().inode,
            plan.roles[0].current.as_ref().unwrap().inode
        );
        // xAccess is metadata only: retain ordinary relative and claimed-name
        // existence semantics while opening/reusing the claimed inode refuses.
        let selected_c =
            std::ffi::CString::new(tmp.path().join(&selected).as_os_str().as_encoded_bytes())
                .unwrap();
        host.scope(|| {
            assert_eq!(stock_vfs_exists(c"."), 1);
            assert_eq!(stock_vfs_exists(&selected_c), 1);
        });
        let refused = armed_stock_refusal(&host, &peer_path);
        assert_eq!(
            refused.sqlite_error_code(),
            Some(STOCK_ALIAS_REFUSAL),
            "{refused}"
        );
        let refused = armed_stock_refusal(&host, &tmp.path().join(&selected));
        assert_eq!(
            refused.sqlite_error_code(),
            Some(STOCK_ALIAS_REFUSAL),
            "{refused}"
        );
        assert!(!tmp.path().join("scheduler.sqlite3").exists());
        let mut wrong = plan.clone();
        wrong.roles[0].source = StoppedSqliteRoleSource::Canonical;
        let failure = host.scope(|| failure.retry_stopped_projection(&wrong).unwrap_err());
        host.scope(|| peer.execute_batch("ROLLBACK;").unwrap());
        close_armed(&host, peer, peer_ticket);
        host.scope(|| {
            let owner = failure.retry_stopped_projection(&plan).unwrap();
            let after = owner.observe_stopped_projection().unwrap();
            assert!(after.ready);
            assert_eq!(
                after.roles[0].as_ref().unwrap().inode,
                before.roles[0].as_ref().unwrap().inode
            );
            owner.abort().unwrap();
        });
        assert!(tmp.path().join(&selected).exists());
    }
    #[test]
    #[cfg(target_os = "linux")]
    fn stopped_partial_pin_replacement_refuses_retry_and_cleanup_preserves_foreign() {
        let host = crate::test_host::TestNativeHost::acquire();
        let tmp = tempfile::tempdir().unwrap();
        seed_wal_on(&host, tmp.path());
        let runtime = Arc::new(PinnedDirectory::open(tmp.path()).unwrap().unwrap());
        let plan = stopped_plan_fixture(
            &runtime,
            tmp.path(),
            [
                StoppedSqliteRoleSource::Canonical,
                StoppedSqliteRoleSource::Absent,
                StoppedSqliteRoleSource::Absent,
            ],
        );
        FAIL_STOPPED_AFTER_ACQUIRE.with(|flag| flag.set(true));
        let failure = host.cleanup_scope(|| {
            DrainedSqliteProjection::capture_stopped_projection(runtime, plan.clone()).unwrap_err()
        });
        let failure = host.scope(|| failure.retry_release().unwrap_err()); // pending normal release refuses
        std::fs::rename(
            tmp.path().join("scheduler.sqlite3"),
            tmp.path().join("retained-original.fixture"),
        )
        .unwrap();
        std::fs::write(tmp.path().join("scheduler.sqlite3"), b"foreign replacement").unwrap();
        let failure = host.scope(|| failure.retry_stopped_projection(&plan).unwrap_err());
        let observed = host.scope(|| match failure.owner() {
            SqliteProjectionRetirementOwner::Drained(owner) => {
                owner.observe_stopped_projection().unwrap()
            }
            _ => panic!("same context"),
        });
        assert!(!observed.ready);
        assert_eq!(
            observed.roles[0].as_ref().unwrap().inode,
            plan.roles[0].current.as_ref().unwrap().inode
        );
        assert!(
            observed
                .coordinates
                .iter()
                .any(|item| item.name == "scheduler.sqlite3"
                    && matches!(
                        item.observation,
                        SqliteRoleCoordinateObservation::ForeignRegular { .. }
                    ))
        );
        host.scope(|| failure.abort().unwrap());
        assert_eq!(
            std::fs::read(tmp.path().join("scheduler.sqlite3")).unwrap(),
            b"foreign replacement"
        );
    }
    #[test]
    #[cfg(target_os = "linux")]
    fn stopped_duplicate_candidates_digest_mismatch_and_generation_exhaustion_hold_unchanged() {
        let host = crate::test_host::TestNativeHost::acquire();
        let tmp = tempfile::tempdir().unwrap();
        seed_wal_on(&host, tmp.path());
        let runtime = Arc::new(PinnedDirectory::open(tmp.path()).unwrap().unwrap());
        let plan = stopped_plan_fixture(
            &runtime,
            tmp.path(),
            [
                StoppedSqliteRoleSource::Canonical,
                StoppedSqliteRoleSource::Absent,
                StoppedSqliteRoleSource::Absent,
            ],
        );
        let canonical = tmp.path().join("scheduler.sqlite3");
        let before = std::fs::read(&canonical).unwrap();
        let duplicate = tmp
            .path()
            .join("scheduler-retirement.role.01010101010101010101010101010101.main.quarantine");
        std::fs::hard_link(&canonical, &duplicate).unwrap();
        let failure = host.cleanup_scope(|| {
            DrainedSqliteProjection::capture_stopped_projection(Arc::clone(&runtime), plan.clone())
                .unwrap_err()
        });
        host.scope(|| failure.abort().unwrap());
        assert_eq!(std::fs::read(&duplicate).unwrap(), before);
        std::fs::remove_file(duplicate).unwrap();
        let mut exhausted = plan.clone();
        exhausted.generations.push([5; 16]);
        let failure = host.cleanup_scope(|| {
            DrainedSqliteProjection::capture_stopped_projection(Arc::clone(&runtime), exhausted)
                .unwrap_err()
        });
        assert!(matches!(
            failure.owner(),
            SqliteProjectionRetirementOwner::UnacquiredStopped { .. }
        ));
        host.scope(|| failure.abort().unwrap());
        let mut wrong = plan.clone();
        wrong.roles[0].current.as_mut().unwrap().content_sha256 = Some("0".repeat(64));
        let failure = host.cleanup_scope(|| {
            DrainedSqliteProjection::capture_stopped_projection(runtime, wrong.clone()).unwrap_err()
        });
        assert!(matches!(
            failure.owner(),
            SqliteProjectionRetirementOwner::Drained(_)
        ));
        // Digest refusal occurs AFTER actual barrier acquisition; same wrong
        // plan retries that owner at native phase2, never a constructor.
        host.scope(|| {
            let failure = failure.retry_stopped_projection(&wrong).unwrap_err();
            let failure = failure.retry_stopped_projection(&plan).unwrap_err(); // changed plan cannot substitute authority
            failure.abort().unwrap();
        });
        assert_eq!(std::fs::read(canonical).unwrap(), before);
    }
    #[cfg(target_os = "linux")]
    fn stock_vfs_exists(path: &std::ffi::CStr) -> i32 {
        unsafe {
            let vfs = libsqlite3_sys::sqlite3_vfs_find(c"unix".as_ptr());
            assert!(!vfs.is_null());
            let mut exists = -1;
            let status = (*vfs).xAccess.unwrap()(
                vfs,
                path.as_ptr(),
                libsqlite3_sys::SQLITE_ACCESS_EXISTS,
                &mut exists,
            );
            assert_eq!(status, libsqlite3_sys::SQLITE_OK);
            exists
        }
    }
    #[cfg(target_os = "linux")]
    thread_local! {
        static TEMP_PROBE_ORIGINAL_OPEN: std::cell::Cell<Option<unsafe extern "C" fn(*const std::ffi::c_char,i32,i32)->i32>> = const { std::cell::Cell::new(None) };
        static TEMP_PROBE_SAW_RELATIVE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    #[cfg(target_os = "linux")]
    unsafe extern "C" fn temp_probe_open(
        path: *const std::ffi::c_char,
        flags: i32,
        mode: i32,
    ) -> i32 {
        if !path.is_null()
            && unsafe { std::ffi::CStr::from_ptr(path) }
                .to_bytes()
                .starts_with(b"relative-temp/etilqs_")
        {
            TEMP_PROBE_SAW_RELATIVE.with(|flag| flag.set(true));
        }
        TEMP_PROBE_ORIGINAL_OPEN.with(|original| match original.get() {
            Some(open) => unsafe { open(path, flags, mode) },
            None => -1,
        })
    }
    #[test]
    #[cfg(target_os = "linux")]
    fn stopped_relative_temp_and_xaccess_child() {
        if std::env::var_os("RYEOS_STOPPED_RELATIVE_TEMP_PROBE").is_none() {
            return;
        }
        let host = crate::test_host::TestNativeHost::acquire();
        assert_eq!(std::env::var("SQLITE_TMPDIR").unwrap(), "relative-temp");
        assert_eq!(std::env::var("TMPDIR").unwrap(), "relative-temp");
        let tmp = tempfile::tempdir().unwrap();
        seed_wal_on(&host, tmp.path());
        let runtime = Arc::new(PinnedDirectory::open(tmp.path()).unwrap().unwrap());
        let plan = stopped_plan_fixture(
            &runtime,
            tmp.path(),
            [
                StoppedSqliteRoleSource::Canonical,
                StoppedSqliteRoleSource::Absent,
                StoppedSqliteRoleSource::Absent,
            ],
        );
        let owner = host.cleanup_scope(|| {
            DrainedSqliteProjection::capture_stopped_projection(runtime, plan).unwrap()
        });
        host.scope(|| {
            assert_eq!(stock_vfs_exists(c"."), 1);
            assert_eq!(stock_vfs_exists(c"relative-temp"), 1);
        });
        host.scope(|| unsafe {
            let vfs = libsqlite3_sys::sqlite3_vfs_find(c"unix".as_ptr());
            assert!(!vfs.is_null());
            // Child isolation keeps the configured syscall override out of
            // other tests. Restore the EXACT original callback after probe.
            let original = (*vfs).xGetSystemCall.unwrap()(vfs, c"open".as_ptr()).unwrap();
            // SQLite's `sqlite3_syscall_ptr` is a type-erased fn pointer, so only a
            // transmute can restate it as the `open` signature.
            let typed = std::mem::transmute::<
                unsafe extern "C" fn(),
                unsafe extern "C" fn(*const std::ffi::c_char, i32, i32) -> i32,
            >(original);
            TEMP_PROBE_ORIGINAL_OPEN.with(|slot| slot.set(Some(typed)));
            let hook = std::mem::transmute::<
                unsafe extern "C" fn(*const std::ffi::c_char, i32, i32) -> i32,
                unsafe extern "C" fn(),
            >(temp_probe_open);
            assert_eq!(
                (*vfs).xSetSystemCall.unwrap()(vfs, c"open".as_ptr(), Some(hook)),
                libsqlite3_sys::SQLITE_OK
            );
            let layout = std::alloc::Layout::from_size_align((*vfs).szOsFile as usize, 16).unwrap();
            let storage = std::alloc::alloc_zeroed(layout);
            assert!(!storage.is_null());
            let file = storage.cast::<libsqlite3_sys::sqlite3_file>();
            let mut out_flags = 0;
            let status = (*vfs).xOpen.unwrap()(
                vfs,
                std::ptr::null(),
                file,
                libsqlite3_sys::SQLITE_OPEN_READWRITE
                    | libsqlite3_sys::SQLITE_OPEN_CREATE
                    | libsqlite3_sys::SQLITE_OPEN_DELETEONCLOSE
                    | libsqlite3_sys::SQLITE_OPEN_TEMP_DB,
                &mut out_flags,
            );
            assert_eq!(
                (*vfs).xSetSystemCall.unwrap()(vfs, c"open".as_ptr(), Some(original)),
                libsqlite3_sys::SQLITE_OK
            );
            assert_eq!(status, libsqlite3_sys::SQLITE_OK);
            assert!(!(*file).pMethods.is_null());
            assert_eq!(
                (*(*file).pMethods).xClose.unwrap()(file),
                libsqlite3_sys::SQLITE_OK
            );
            std::alloc::dealloc(storage, layout);
        });
        assert!(
            TEMP_PROBE_SAW_RELATIVE.with(|flag| flag.get()),
            "SQLite must use configured relative temp directory while retirement claim is active"
        );
        host.scope(|| owner.abort().unwrap());
    }
    #[test]
    #[cfg(target_os = "linux")]
    fn stopped_relative_temp_configuration_preserves_ordinary_unix_stat_semantics() {
        let _serial = crate::test_host::serial_guard();
        let cwd = tempfile::tempdir().unwrap();
        std::fs::create_dir(cwd.path().join("relative-temp")).unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::stopped_relative_temp_and_xaccess_child",
                "--nocapture",
            ])
            .current_dir(cwd.path())
            .env("RYEOS_STOPPED_RELATIVE_TEMP_PROBE", "1")
            .env("SQLITE_TMPDIR", "relative-temp")
            .env("TMPDIR", "relative-temp")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "relative temporary/xAccess child failed");
                break;
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("relative temporary child timeout");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    /// Bound owner of `scheduler.sqlite3` beneath `runtime` on one armed
    /// ticket; close it with `close_bound`.
    #[cfg(target_os = "linux")]
    pub(super) fn armed_bound(
        host: &crate::test_host::TestNativeHost,
        runtime: &Arc<PinnedDirectory>,
    ) -> (BoundSqliteConnection, crate::native::RawTestTicket) {
        host.armed(|| {
            BoundSqliteConnection::open_existing(runtime.clone(), OsStr::new("scheduler.sqlite3"))
                .unwrap()
        })
    }
    /// Bound open of `name` beneath `runtime` on one armed ticket that the
    /// binding itself must refuse, never the unarmed gate. The refused open's
    /// retained owner is released and the ticket retired before returning.
    #[cfg(target_os = "linux")]
    fn armed_bound_refusal(
        host: &crate::test_host::TestNativeHost,
        runtime: &Arc<PinnedDirectory>,
        name: &str,
    ) -> String {
        let (opened, ticket) =
            host.armed(|| BoundSqliteConnection::open_existing(runtime.clone(), OsStr::new(name)));
        let failure = match opened {
            Ok(owner) => {
                close_bound(host, owner, ticket);
                panic!("armed bound open of {name} was admitted");
            }
            Err(failure) => failure,
        };
        let error = failure.error().to_string();
        failure.retry_close().unwrap();
        host.retire_raw(ticket);
        assert_ne!(
            error, "bound SQLite acquisition refused (21)",
            "armed bound open reached the unarmed gate"
        );
        error
    }
    #[cfg(target_os = "linux")]
    pub(super) fn close_bound(
        host: &crate::test_host::TestNativeHost,
        owner: BoundSqliteConnection,
        ticket: crate::native::RawTestTicket,
    ) {
        host.scope(|| owner.close().unwrap());
        host.retire_raw(ticket);
    }
    /// The binding's refusal of a stock open that would alias a held role.
    #[cfg(target_os = "linux")]
    const STOCK_ALIAS_REFUSAL: rusqlite::ErrorCode = rusqlite::ErrorCode::CannotOpen;
    #[cfg(target_os = "linux")]
    pub(super) fn seed_wal_on(host: &crate::test_host::TestNativeHost, root: &std::path::Path) {
        let (connection, ticket) =
            host.armed(|| Connection::open(root.join("scheduler.sqlite3")).unwrap());
        host.scope(|| {
            connection.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE custody(value INTEGER); INSERT INTO custody VALUES(1);").unwrap();
            connection.close().unwrap();
        });
        host.retire_raw(ticket);
    }
    /// Stock open of `path` on one armed ticket; the open must be refused by
    /// the binding, not by the unarmed gate.
    #[cfg(target_os = "linux")]
    fn armed_stock_refusal(
        host: &crate::test_host::TestNativeHost,
        path: &std::path::Path,
    ) -> rusqlite::Error {
        let (opened, ticket) = host.armed(|| Connection::open(path));
        let error = match opened {
            Ok(connection) => {
                host.scope(|| connection.close().unwrap());
                host.retire_raw(ticket);
                panic!("armed stock open of {} was admitted", path.display());
            }
            Err(error) => error,
        };
        host.retire_raw(ticket);
        assert_ne!(
            error.sqlite_error_code(),
            Some(rusqlite::ErrorCode::ApiMisuse),
            "armed stock open reached the unarmed gate: {error}"
        );
        error
    }
    /// Stock connection on one armed ticket; close it with `close_armed`.
    #[cfg(target_os = "linux")]
    pub(super) fn armed_stock(
        host: &crate::test_host::TestNativeHost,
        path: &std::path::Path,
    ) -> (Connection, crate::native::RawTestTicket) {
        host.armed(|| Connection::open(path).unwrap())
    }
    #[cfg(target_os = "linux")]
    pub(super) fn close_armed(
        host: &crate::test_host::TestNativeHost,
        connection: Connection,
        ticket: crate::native::RawTestTicket,
    ) {
        host.scope(|| connection.close().unwrap());
        host.retire_raw(ticket);
    }
    #[test]
    #[cfg(target_os = "linux")]
    fn owned_wal_connection_has_all_three_bindings() {
        let host = crate::test_host::TestNativeHost::acquire();
        let tmp = tempfile::TempDir::new().unwrap();
        seed_wal_on(&host, tmp.path());
        let pin = Arc::new(PinnedDirectory::open(tmp.path()).unwrap().unwrap());
        let (owned, ticket) = armed_bound(&host, &pin);
        host.scope(|| {
            owned.establish_wal_binding().unwrap();
            owned.require_binding().unwrap();
            owned
                .connection()
                .execute("UPDATE custody SET value=2", [])
                .unwrap();
            owned.require_binding().unwrap();
        });
        close_bound(&host, owned, ticket);
    }
    #[test]
    #[cfg(target_os = "linux")]
    fn replacement_main_refuses_before_foreign_write() {
        const TEST: &str = concat!(
            module_path!(),
            "::replacement_main_refuses_before_foreign_write"
        );
        // A replaced main poisons the owner's native keeper, which no close
        // may settle; the scenario runs and ends in its own child process.
        if !crate::test_host::is_isolated_child(TEST) {
            let outcome = crate::test_host::run_isolated(TEST);
            assert!(outcome.contains("foreign main preserved"), "{outcome}");
            return;
        }
        let host = crate::test_host::TestNativeHost::acquire();
        let tmp = tempfile::TempDir::new().unwrap();
        seed_wal_on(&host, tmp.path());
        let pin = Arc::new(PinnedDirectory::open(tmp.path()).unwrap().unwrap());
        let (owned, ticket) = armed_bound(&host, &pin);
        host.scope(|| owned.establish_wal_binding().unwrap());
        std::fs::rename(
            tmp.path().join("scheduler.sqlite3"),
            tmp.path().join("retained-main"),
        )
        .unwrap();
        std::fs::write(tmp.path().join("scheduler.sqlite3"), b"foreign main").unwrap();
        host.scope(|| {
            assert!(
                owned
                    .connection()
                    .execute("UPDATE custody SET value=3", [])
                    .is_err()
            );
            assert!(owned.require_binding().is_err());
        });
        assert_eq!(
            std::fs::read(tmp.path().join("scheduler.sqlite3")).unwrap(),
            b"foreign main"
        );
        let closed = host.scope(|| owned.close());
        assert_eq!(
            std::fs::read(tmp.path().join("scheduler.sqlite3")).unwrap(),
            b"foreign main"
        );
        match closed {
            Ok(()) => {
                host.retire_raw(ticket);
                host.release();
                crate::test_host::end_isolated_process(tmp, "closed; foreign main preserved")
            }
            Err(failure) => {
                let outcome = format!(
                    "close retained: {:?}; foreign main preserved",
                    failure.error()
                );
                crate::test_host::end_isolated_process((failure, ticket, host, tmp), &outcome)
            }
        }
    }
    #[test]
    #[cfg(target_os = "linux")]
    fn replacement_sidecars_refuse_mutation_and_survive_close() {
        const TEST: &str = concat!(
            module_path!(),
            "::replacement_sidecars_refuse_mutation_and_survive_close"
        );
        // A replaced sidecar poisons the owner's native keeper, which no
        // close may settle; the scenario runs and ends in its own child.
        if !crate::test_host::is_isolated_child(TEST) {
            let outcome = crate::test_host::run_isolated(TEST);
            assert!(
                outcome.contains("-wal: foreign sidecar preserved"),
                "{outcome}"
            );
            assert!(
                outcome.contains("-shm: foreign sidecar preserved"),
                "{outcome}"
            );
            return;
        }
        let host = crate::test_host::TestNativeHost::acquire();
        let mut retained = Vec::new();
        let mut directories = Vec::new();
        let mut outcome = String::new();
        for suffix in ["-wal", "-shm"] {
            let tmp = tempfile::TempDir::new().unwrap();
            seed_wal_on(&host, tmp.path());
            let pin = Arc::new(PinnedDirectory::open(tmp.path()).unwrap().unwrap());
            let (owned, ticket) = armed_bound(&host, &pin);
            host.scope(|| owned.establish_wal_binding().unwrap());
            let child = tmp.path().join(format!("scheduler.sqlite3{suffix}"));
            std::fs::rename(&child, tmp.path().join("retained-sidecar")).unwrap();
            std::fs::write(&child, b"foreign sidecar").unwrap();
            host.scope(|| {
                assert!(
                    owned
                        .connection()
                        .execute("UPDATE custody SET value=3", [])
                        .is_err()
                );
                assert!(owned.require_binding().is_err());
            });
            let closed = host.scope(|| owned.close());
            assert_eq!(std::fs::read(&child).unwrap(), b"foreign sidecar");
            match closed {
                Ok(()) => {
                    host.retire_raw(ticket);
                    outcome.push_str(&format!("{suffix}: closed; "));
                }
                Err(failure) => {
                    outcome.push_str(&format!("{suffix}: close retained {:?}; ", failure.error()));
                    retained.push((failure, ticket));
                }
            }
            outcome.push_str(&format!("{suffix}: foreign sidecar preserved; "));
            directories.push(tmp);
        }
        if retained.is_empty() {
            host.release();
            crate::test_host::end_isolated_process(directories, &outcome)
        }
        crate::test_host::end_isolated_process((retained, host, directories), &outcome)
    }
    #[test]
    #[cfg(target_os = "linux")]
    fn symlink_main_refuses_without_touching_target() {
        let host = crate::test_host::TestNativeHost::acquire();
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("foreign"), b"foreign").unwrap();
        std::os::unix::fs::symlink("foreign", tmp.path().join("scheduler.sqlite3")).unwrap();
        let pin = Arc::new(PinnedDirectory::open(tmp.path()).unwrap().unwrap());
        armed_bound_refusal(&host, &pin, "scheduler.sqlite3");
        assert_eq!(
            std::fs::read(tmp.path().join("foreign")).unwrap(),
            b"foreign"
        );
    }
    #[cfg(target_os = "linux")]
    fn assert_external_write_busy(path: &std::path::Path) {
        struct Supervised(std::process::Child);
        impl Drop for Supervised {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut probe = Supervised(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "tests::keeper_probe_child", "--nocapture"])
                .env("LILLUX_SQLITE_KEEPER_PROBE", path)
                .spawn()
                .unwrap(),
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if let Some(status) = probe.0.try_wait().unwrap() {
                assert!(
                    status.success(),
                    "external kernel-lock probe failed: {status}"
                );
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "external kernel-lock probe timed out"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    #[test]
    #[cfg(target_os = "linux")]
    fn keeper_probe_child() {
        let Some(path) = std::env::var_os("LILLUX_SQLITE_KEEPER_PROBE") else {
            return;
        };
        let host = crate::test_host::TestNativeHost::acquire();
        let (connection, ticket) = armed_stock(&host, std::path::Path::new(&path));
        let error = host.scope(|| {
            connection.busy_timeout(std::time::Duration::ZERO).unwrap();
            connection.execute_batch("BEGIN IMMEDIATE;").unwrap_err()
        });
        assert!(
            matches!(error, rusqlite::Error::SqliteFailure(code, _) if code.code == rusqlite::ErrorCode::DatabaseBusy)
        );
        close_armed(&host, connection, ticket);
    }
    #[test]
    #[cfg(target_os = "linux")]
    fn closing_second_owner_preserves_first_owners_process_locks() {
        let host = crate::test_host::TestNativeHost::acquire();
        let tmp = tempfile::TempDir::new().unwrap();
        seed_wal_on(&host, tmp.path());
        let pin = Arc::new(PinnedDirectory::open(tmp.path()).unwrap().unwrap());
        let (first, first_ticket) = armed_bound(&host, &pin);
        host.scope(|| {
            first.establish_wal_binding().unwrap();
            first
                .connection()
                .execute_batch("BEGIN IMMEDIATE;")
                .unwrap();
        });
        let (second, second_ticket) = armed_bound(&host, &pin);
        drop(second);
        host.retire_raw(second_ticket);
        // A separate process observes kernel locks, unlike a same-process
        // SQLite bookkeeping assertion which can miss POSIX close unlocks.
        assert_external_write_busy(&tmp.path().join("scheduler.sqlite3"));
        host.scope(|| {
            first.connection().execute_batch("ROLLBACK;").unwrap();
            first.require_binding().unwrap();
        });
        close_bound(&host, first, first_ticket);
    }
    #[test]
    #[cfg(target_os = "linux")]
    fn hardlinked_shm_refusal_preserves_other_nodes_bytes_and_kernel_locks() {
        let host = crate::test_host::TestNativeHost::acquire();
        use std::os::unix::fs::FileExt;
        fn snapshot(observer: &std::fs::File) -> Vec<u8> {
            let length = observer.metadata().unwrap().len();
            assert!(
                length <= 4 * 1024 * 1024,
                "SHM observation exceeds control bound"
            );
            let mut bytes = vec![0; usize::try_from(length).unwrap()];
            observer.read_exact_at(&mut bytes, 0).unwrap();
            bytes
        }
        let first_root = tempfile::TempDir::new().unwrap();
        let second_root = tempfile::TempDir::new().unwrap();
        seed_wal_on(&host, first_root.path());
        seed_wal_on(&host, second_root.path());
        // Declaration order also closes the SQL owner before this observer
        // on control unwind, avoiding fixture-induced POSIX lock release.
        let observer: std::fs::File;
        let first_pin = Arc::new(PinnedDirectory::open(first_root.path()).unwrap().unwrap());
        let (first, first_ticket) = armed_bound(&host, &first_pin);
        host.scope(|| first.establish_wal_binding().unwrap());
        let source_shm = first_root.path().join("scheduler.sqlite3-shm");
        // An observer must not open/close SHM while process locks are live:
        // closing any same-inode FD releases POSIX locks. Retain this exact
        // observer through explicit final native close; snapshots use pread.
        observer = std::fs::File::open(&source_shm).unwrap();
        host.scope(|| {
            first
                .connection()
                .execute_batch("BEGIN IMMEDIATE;")
                .unwrap()
        });
        assert_external_write_busy(&first_root.path().join("scheduler.sqlite3"));
        std::fs::hard_link(
            &source_shm,
            second_root.path().join("scheduler.sqlite3-shm"),
        )
        .unwrap();
        let before = snapshot(&observer);
        let second_pin = Arc::new(PinnedDirectory::open(second_root.path()).unwrap().unwrap());
        let mut retained_open = None;
        let mut retained_close = None;
        let (second, second_ticket) = host.armed(|| {
            BoundSqliteConnection::open_existing(second_pin, OsStr::new("scheduler.sqlite3"))
        });
        let refusal_code = match second {
            Ok(second) => {
                let refusal = host.scope(|| second.establish_wal_binding().unwrap_err());
                let code = refusal
                    .downcast_ref::<rusqlite::Error>()
                    .and_then(|e| match e {
                        rusqlite::Error::SqliteFailure(code, _) => Some(code.extended_code),
                        _ => None,
                    });
                retained_close = host.scope(|| second.close().err());
                code
            }
            Err(refusal) => {
                let code =
                    refusal
                        .error()
                        .downcast_ref::<rusqlite::Error>()
                        .and_then(|e| match e {
                            rusqlite::Error::SqliteFailure(code, _) => Some(code.extended_code),
                            _ => None,
                        });
                retained_open = Some(refusal);
                code
            }
        };
        assert_eq!(refusal_code, Some(libsqlite3_sys::SQLITE_IOERR_SHMOPEN));
        assert_eq!(snapshot(&observer), before);
        assert_external_write_busy(&first_root.path().join("scheduler.sqlite3"));
        host.scope(|| first.connection().execute_batch("ROLLBACK;").unwrap());
        close_bound(&host, first, first_ticket);
        if let Some(failure) = retained_open {
            host.scope(|| failure.retry_close().unwrap());
        }
        if let Some(failure) = retained_close {
            host.scope(|| failure.retry().unwrap());
        }
        host.retire_raw(second_ticket);
        drop(observer);
    }
    #[test]
    #[cfg(target_os = "linux")]
    fn hardlinked_main_sidecar_roles_refuse_before_bytes_or_metadata_change() {
        let host = crate::test_host::TestNativeHost::acquire();
        use std::os::unix::fs::MetadataExt;
        for suffix in ["-wal", "-shm"] {
            let tmp = tempfile::TempDir::new().unwrap();
            seed_wal_on(&host, tmp.path());
            let main = tmp.path().join("scheduler.sqlite3");
            std::fs::hard_link(&main, tmp.path().join(format!("scheduler.sqlite3{suffix}")))
                .unwrap();
            let before = std::fs::read(&main).unwrap();
            let metadata = std::fs::metadata(&main).unwrap();
            let identity_and_mutation = (
                metadata.dev(),
                metadata.ino(),
                metadata.len(),
                metadata.mode(),
                metadata.nlink(),
                metadata.mtime(),
                metadata.mtime_nsec(),
                metadata.ctime(),
                metadata.ctime_nsec(),
            );
            let pin = Arc::new(PinnedDirectory::open(tmp.path()).unwrap().unwrap());
            armed_bound_refusal(&host, &pin, "scheduler.sqlite3");
            assert_eq!(std::fs::read(&main).unwrap(), before);
            let after = std::fs::metadata(&main).unwrap();
            assert_eq!(
                (
                    after.dev(),
                    after.ino(),
                    after.len(),
                    after.mode(),
                    after.nlink(),
                    after.mtime(),
                    after.mtime_nsec(),
                    after.ctime(),
                    after.ctime_nsec()
                ),
                identity_and_mutation
            );
        }
    }
    #[test]
    #[cfg(target_os = "linux")]
    fn supervised_close_returns_same_busy_owner_and_can_retry() {
        let host = crate::test_host::TestNativeHost::acquire();
        let tmp = tempfile::TempDir::new().unwrap();
        seed_wal_on(&host, tmp.path());
        let pin = Arc::new(PinnedDirectory::open(tmp.path()).unwrap().unwrap());
        let (owned, ticket) = armed_bound(&host, &pin);
        host.scope(|| owned.establish_wal_binding().unwrap());
        let sql = std::ffi::CString::new("SELECT value FROM custody").unwrap();
        let mut statement = std::ptr::null_mut();
        // SAFETY: owner remains live and SQL is NUL terminated; retain the
        // statement deliberately to exercise SQLITE_BUSY connection close.
        let handle = unsafe { owned.connection().handle() };
        let status = host.scope(|| unsafe {
            libsqlite3_sys::sqlite3_prepare_v2(
                handle,
                sql.as_ptr(),
                -1,
                &mut statement,
                std::ptr::null_mut(),
            )
        });
        assert_eq!(status, libsqlite3_sys::SQLITE_OK);
        let failure = host.scope(|| owned.close().unwrap_err());
        let BoundSqliteCloseOwner::Live(live) = failure.owner() else {
            panic!("busy close lost live phase");
        };
        assert_eq!(unsafe { live.connection().handle() }, handle);
        assert!(
            matches!(failure.error(), BoundSqliteCloseError::Connection(rusqlite::Error::SqliteFailure(code, _)) if code.code == rusqlite::ErrorCode::DatabaseBusy)
        );
        // SAFETY: failed close retained the exact connection; finalize its
        // outstanding statement before the explicit retry.
        assert_eq!(
            host.scope(|| unsafe { libsqlite3_sys::sqlite3_finalize(statement) }),
            libsqlite3_sys::SQLITE_OK
        );
        host.scope(|| failure.retry().unwrap());
        host.retire_raw(ticket);
    }
    #[test]
    #[cfg(target_os = "linux")]
    fn nonregular_fifo_and_directory_refuse_before_data_open() {
        let host = crate::test_host::TestNativeHost::acquire();
        use std::os::unix::ffi::OsStrExt;
        unsafe extern "C" {
            fn mkfifo(path: *const std::ffi::c_char, mode: u32) -> i32;
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let child = tmp.path().join("scheduler.sqlite3");
        let name = std::ffi::CString::new(child.as_os_str().as_bytes()).unwrap();
        // SAFETY: valid NUL-terminated temporary path; creates only this test's FIFO.
        assert_eq!(unsafe { mkfifo(name.as_ptr(), 0o600) }, 0);
        let pin = Arc::new(PinnedDirectory::open(tmp.path()).unwrap().unwrap());
        armed_bound_refusal(&host, &pin, "scheduler.sqlite3");
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 1);
        std::fs::remove_file(&child).unwrap();
        std::fs::create_dir(&child).unwrap();
        armed_bound_refusal(&host, &pin, "scheduler.sqlite3");
        assert_eq!(std::fs::read_dir(&child).unwrap().count(), 0);
    }
    #[test]
    #[cfg(target_os = "linux")]
    fn native_pending_returns_same_retiring_owner_until_actual_release() {
        let host = crate::test_host::TestNativeHost::acquire();
        let tmp = tempfile::TempDir::new().unwrap();
        seed_wal_on(&host, tmp.path());
        let pin = Arc::new(PinnedDirectory::open(tmp.path()).unwrap().unwrap());
        let (first, first_ticket) = armed_bound(&host, &pin);
        host.scope(|| first.establish_wal_binding().unwrap());
        let original_runtime = first.runtime() as *const PinnedDirectory;
        let (second, second_ticket) = armed_bound(&host, &pin);
        host.scope(|| {
            second.establish_wal_binding().unwrap();
            second
                .connection()
                .execute_batch("BEGIN IMMEDIATE;")
                .unwrap();
        });
        let pending = host.scope(|| first.close().unwrap_err());
        assert!(matches!(
            pending.error(),
            BoundSqliteCloseError::NativeCustody(NativeRetentionReason::TrackedPeer)
        ));
        let BoundSqliteCloseOwner::Retiring(retiring) = pending.owner() else {
            panic!("SQL success must return native retirement phase");
        };
        assert_eq!(
            retiring.runtime() as *const PinnedDirectory,
            original_runtime
        );
        assert_external_write_busy(&tmp.path().join("scheduler.sqlite3"));
        host.scope(|| second.connection().execute_batch("ROLLBACK;").unwrap());
        close_bound(&host, second, second_ticket);
        // This retries the original context after automatic FD progress; no
        // connection accessor or reopen exists on the retirement type.
        host.scope(|| pending.retry().unwrap());
        host.retire_raw(first_ticket);
    }
    #[cfg(target_os = "linux")]
    fn retained_snapshot(file: &std::fs::File) -> Vec<u8> {
        use std::os::unix::fs::FileExt;
        let length = usize::try_from(file.metadata().unwrap().len()).unwrap();
        assert!(length <= 4 * 1024 * 1024, "bounded control snapshot");
        let mut bytes = vec![0; length];
        file.read_exact_at(&mut bytes, 0).unwrap();
        bytes
    }

    /// Snapshot a fixture whose latest value is only in WAL. Source observers
    /// remain open until its SQL owner closes; no same-inode observer close
    /// can erase locks while a source connection is live.
    #[cfg(target_os = "linux")]
    fn readonly_wal_fixture(host: &crate::test_host::TestNativeHost) -> Arc<tempfile::TempDir> {
        let source = tempfile::tempdir().unwrap();
        let main = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(source.path().join("scheduler.sqlite3"))
            .unwrap();
        let mut observers = Vec::with_capacity(3);
        observers.push(main);
        // Declare SQL owner after observers: unwind closes SQL first.
        let (connection, ticket) = armed_stock(host, &source.path().join("scheduler.sqlite3"));
        let (busy, frames, checkpointed): (i64, i64, i64) = host.scope(|| {
            connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE custody(value INTEGER); INSERT INTO custody VALUES(1);").unwrap();
            connection
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })
                .unwrap()
        });
        assert_eq!((busy, frames, checkpointed), (0, 0, 0));
        observers.push(std::fs::File::open(source.path().join("scheduler.sqlite3-wal")).unwrap());
        observers.push(std::fs::File::open(source.path().join("scheduler.sqlite3-shm")).unwrap());
        let main_before = retained_snapshot(&observers[0]);
        host.scope(|| {
            connection
                .execute("UPDATE custody SET value=42", [])
                .unwrap()
        });
        assert_eq!(
            retained_snapshot(&observers[0]),
            main_before,
            "latest row must remain WAL-backed"
        );
        let snapshot = Arc::new(tempfile::tempdir().unwrap());
        for (name, file) in [
            ("scheduler.sqlite3", &observers[0]),
            ("scheduler.sqlite3-wal", &observers[1]),
            ("scheduler.sqlite3-shm", &observers[2]),
        ] {
            std::fs::write(snapshot.path().join(name), retained_snapshot(file)).unwrap();
        }
        close_armed(host, connection, ticket);
        drop(observers);
        snapshot
    }

    /// Readonly copy of `name` beneath `runtime` on one armed ticket, with
    /// the standalone mechanism SQL work unless `work` is given.
    #[cfg(target_os = "linux")]
    fn armed_copy(
        host: &crate::test_host::TestNativeHost,
        runtime: Arc<PinnedDirectory>,
        name: &str,
        lease: SqliteCopyLease,
        work: Arc<dyn SqliteInspectionSqlWork>,
    ) -> (
        std::result::Result<ReadonlySqliteCopy, ReadonlySqliteOpenFailure>,
        crate::native::RawTestTicket,
    ) {
        host.armed(|| ReadonlySqliteCopy::open(runtime, OsStr::new(name), lease, work))
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn named_readonly_copy_reads_its_wal_history_and_refuses_main_mutation() {
        let host = crate::test_host::TestNativeHost::acquire();
        let snapshot = readonly_wal_fixture(&host);
        for suffix in ["", "-wal", "-shm"] {
            std::fs::rename(
                snapshot.path().join(format!("scheduler.sqlite3{suffix}")),
                snapshot.path().join(format!("runtime.sqlite3{suffix}")),
            )
            .unwrap();
        }
        let main = snapshot.path().join("runtime.sqlite3");
        let before = std::fs::read(&main).unwrap(); // before any copy SQL owner
        let pin = Arc::new(PinnedDirectory::open(snapshot.path()).unwrap().unwrap());
        let lease: SqliteCopyLease = snapshot.clone();
        let work = mechanism_sql_work();
        let (copy, ticket) = armed_copy(
            &host,
            pin.clone(),
            "runtime.sqlite3",
            lease.clone(),
            work.clone(),
        );
        let copy = copy.unwrap();
        let (value, refusal) = host.scope(|| {
            let value: i64 = copy
                .connection()
                .query_row("SELECT value FROM custody", [], |row| row.get(0))
                .unwrap();
            let refusal = copy
                .connection()
                .execute("UPDATE custody SET value=99", [])
                .unwrap_err();
            (value, refusal)
        });
        assert_eq!(value, 42);
        assert!(
            matches!(refusal, rusqlite::Error::SqliteFailure(code, _) if code.code == rusqlite::ErrorCode::ReadOnly)
        );
        let proof = host.scope(|| copy.close().unwrap());
        host.retire_raw(ticket);
        proof
            .require_original(&pin, OsStr::new("runtime.sqlite3"), &lease, &work)
            .unwrap();
        assert!(
            proof
                .require_original(&pin, OsStr::new("scheduler.sqlite3"), &lease, &work)
                .is_err()
        );
        assert!(
            proof
                .require_original(
                    &pin,
                    OsStr::new("runtime.sqlite3"),
                    &lease,
                    &mechanism_sql_work()
                )
                .is_err()
        );
        let foreign = Arc::new(tempfile::tempdir().unwrap());
        let foreign_root = PinnedDirectory::open(foreign.path()).unwrap().unwrap();
        let foreign_lease: SqliteCopyLease = foreign;
        assert!(
            proof
                .require_original(&pin, OsStr::new("runtime.sqlite3"), &foreign_lease, &work)
                .is_err()
        );
        assert!(
            proof
                .require_original(&foreign_root, OsStr::new("runtime.sqlite3"), &lease, &work)
                .is_err()
        );
        // Every borrowed refusal leaves the SAME original proof usable.
        proof
            .require_original(&pin, OsStr::new("runtime.sqlite3"), &lease, &work)
            .unwrap();
        assert_eq!(std::fs::read(&main).unwrap(), before); // after full release
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn readonly_copy_journal_refusal_preserves_source_and_copy() {
        let host = crate::test_host::TestNativeHost::acquire();
        let source = tempfile::tempdir().unwrap();
        std::fs::write(source.path().join("scheduler.sqlite3"), b"original main").unwrap();
        std::fs::write(
            source.path().join("scheduler.sqlite3-journal"),
            b"cold or hot journal",
        )
        .unwrap();
        let snapshot = Arc::new(tempfile::tempdir().unwrap());
        for name in ["scheduler.sqlite3", "scheduler.sqlite3-journal"] {
            std::fs::copy(source.path().join(name), snapshot.path().join(name)).unwrap();
        }
        let pin = Arc::new(PinnedDirectory::open(snapshot.path()).unwrap().unwrap());
        let (opened, ticket) = armed_copy(
            &host,
            pin,
            "scheduler.sqlite3",
            snapshot.clone(),
            mechanism_sql_work(),
        );
        let failure = opened.err().expect("journal must refuse");
        assert_ne!(
            failure.error().to_string(),
            "bound SQLite acquisition refused (21)",
            "armed copy open reached the unarmed gate"
        );
        failure.retry_close().unwrap();
        host.retire_raw(ticket);
        for (name, expected) in [
            ("scheduler.sqlite3", b"original main".as_slice()),
            (
                "scheduler.sqlite3-journal",
                b"cold or hot journal".as_slice(),
            ),
        ] {
            assert_eq!(std::fs::read(source.path().join(name)).unwrap(), expected);
            assert_eq!(std::fs::read(snapshot.path().join(name)).unwrap(), expected);
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn copy_lease_preallocation_failure_remains_rust_owned() {
        let _serial = crate::test_host::serial_guard();
        let snapshot = Arc::new(tempfile::tempdir().unwrap());
        let weak = Arc::downgrade(&snapshot);
        let pin = PinnedDirectory::open(snapshot.path()).unwrap().unwrap();
        let root = std::fs::File::open(pin.descriptor_path().unwrap()).unwrap();
        let lease: SqliteCopyLease = snapshot.clone();
        drop(snapshot);
        let (_, owner) = Binding::create(&root, OsStr::new("invalid/name"), 3, Some(lease))
            .err()
            .expect("invalid name refuses before allocation");
        assert!(owner.is_none());
        assert!(weak.upgrade().is_none());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn copy_lease_survives_partial_native_acquisition_failure() {
        let host = crate::test_host::TestNativeHost::acquire();
        let snapshot = Arc::new(tempfile::tempdir().unwrap());
        std::fs::write(snapshot.path().join("scheduler.sqlite3"), b"retained main").unwrap();
        std::fs::create_dir(snapshot.path().join("scheduler.sqlite3-wal")).unwrap();
        let weak = Arc::downgrade(&snapshot);
        let path = snapshot.path().to_owned();
        let pin = Arc::new(PinnedDirectory::open(&path).unwrap().unwrap());
        let lease: SqliteCopyLease = snapshot.clone();
        drop(snapshot);
        let (opened, ticket) =
            armed_copy(&host, pin, "scheduler.sqlite3", lease, mechanism_sql_work());
        let failure = opened.err().expect("nonregular WAL refuses");
        assert!(matches!(
            failure.owner(),
            Some(BoundSqliteCloseOwner::Retiring(_))
        ));
        assert!(
            weak.upgrade().is_some(),
            "partial context must retain original guard"
        );
        let released = failure.retry_close().unwrap();
        host.retire_raw(ticket);
        assert!(
            weak.upgrade().is_some(),
            "partial release retains original cleanup obligation"
        );
        assert!(path.exists());
        drop(released); // fixture guard only, not platform settlement
        assert!(weak.upgrade().is_none());
        assert!(!path.exists());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn readonly_copy_pending_close_keeps_guard_until_original_retry() {
        let host = crate::test_host::TestNativeHost::acquire();
        let snapshot = Arc::new(tempfile::tempdir().unwrap());
        seed_wal_on(&host, snapshot.path());
        let weak = Arc::downgrade(&snapshot);
        let path = snapshot.path().to_owned();
        let pin = Arc::new(PinnedDirectory::open(&path).unwrap().unwrap());
        let (peer, peer_ticket) = armed_bound(&host, &pin);
        host.scope(|| peer.establish_wal_binding().unwrap());
        let (copy, copy_ticket) = armed_copy(
            &host,
            pin,
            "scheduler.sqlite3",
            snapshot.clone(),
            mechanism_sql_work(),
        );
        let copy = copy.unwrap();
        let _: i64 = host.scope(|| {
            copy.connection()
                .query_row("SELECT value FROM custody", [], |row| row.get(0))
                .unwrap()
        });
        drop(snapshot);
        host.scope(|| peer.connection().execute_batch("BEGIN IMMEDIATE;").unwrap());
        let failure = host.scope(|| copy.close().unwrap_err());
        assert!(matches!(
            failure.error(),
            BoundSqliteCloseError::NativeCustody(NativeRetentionReason::TrackedPeer)
        ));
        assert!(matches!(
            failure.owner(),
            BoundSqliteCloseOwner::Retiring(_)
        ));
        assert!(weak.upgrade().is_some());
        assert_external_write_busy(&path.join("scheduler.sqlite3"));
        let failure = host.scope(|| failure.retry_close().unwrap_err());
        assert!(weak.upgrade().is_some());
        host.scope(|| peer.connection().execute_batch("ROLLBACK;").unwrap());
        close_bound(&host, peer, peer_ticket);
        assert!(
            weak.upgrade().is_some(),
            "caller-held retirement must still own guard"
        );
        let closed = host.scope(|| failure.retry_close().unwrap());
        host.retire_raw(copy_ticket);
        assert!(
            weak.upgrade().is_some(),
            "closed proof retains original cleanup obligation"
        );
        assert!(path.exists());
        drop(closed); // fixture guard only, not platform settlement
        assert!(weak.upgrade().is_none());
        assert!(!path.exists());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn readonly_copy_fallback_reaper_keeps_guard_through_peer_release() {
        let host = crate::test_host::TestNativeHost::acquire();
        let snapshot = Arc::new(tempfile::tempdir().unwrap());
        seed_wal_on(&host, snapshot.path());
        let weak = Arc::downgrade(&snapshot);
        let path = snapshot.path().to_owned();
        let pin = Arc::new(PinnedDirectory::open(&path).unwrap().unwrap());
        let (peer, peer_ticket) = armed_bound(&host, &pin);
        host.scope(|| peer.establish_wal_binding().unwrap());
        let (copy, copy_ticket) = armed_copy(
            &host,
            pin,
            "scheduler.sqlite3",
            snapshot.clone(),
            mechanism_sql_work(),
        );
        let copy = copy.unwrap();
        let _: i64 = host.scope(|| {
            copy.connection()
                .query_row("SELECT value FROM custody", [], |row| row.get(0))
                .unwrap()
        });
        drop(snapshot);
        host.scope(|| peer.connection().execute_batch("BEGIN IMMEDIATE;").unwrap());
        drop(copy);
        host.retire_raw(copy_ticket);
        assert!(weak.upgrade().is_some());
        assert_external_write_busy(&path.join("scheduler.sqlite3"));
        host.scope(|| peer.connection().execute_batch("ROLLBACK;").unwrap());
        // Outside-mutex host drain destroys the ready lease.
        close_bound(&host, peer, peer_ticket);
        assert!(weak.upgrade().is_none());
        assert!(!path.exists());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn abandoned_busy_copy_keeps_native_database_and_original_guard() {
        const TEST: &str = concat!(
            module_path!(),
            "::abandoned_busy_copy_keeps_native_database_and_original_guard"
        );
        // The abandoned database and its context stay in native custody for
        // the life of the process, so no arena may release under them: the
        // scenario runs and ends in its own child process.
        if !crate::test_host::is_isolated_child(TEST) {
            let outcome = crate::test_host::run_isolated(TEST);
            assert!(
                outcome.contains("abandoned copy retained with its guard"),
                "{outcome}"
            );
            return;
        }
        let host = crate::test_host::TestNativeHost::acquire();
        let snapshot = Arc::new(tempfile::tempdir().unwrap());
        seed_wal_on(&host, snapshot.path());
        let weak = Arc::downgrade(&snapshot);
        let path = snapshot.path().to_owned();
        let pin = Arc::new(PinnedDirectory::open(&path).unwrap().unwrap());
        let (copy, ticket) = armed_copy(
            &host,
            pin,
            "scheduler.sqlite3",
            snapshot.clone(),
            mechanism_sql_work(),
        );
        let copy = copy.unwrap();
        drop(snapshot);
        let sql = std::ffi::CString::new("SELECT value FROM custody").unwrap();
        let handle = unsafe { copy.connection().handle() };
        let mut statement = std::ptr::null_mut();
        assert_eq!(
            host.scope(|| unsafe {
                libsqlite3_sys::sqlite3_prepare_v2(
                    handle,
                    sql.as_ptr(),
                    -1,
                    &mut statement,
                    std::ptr::null_mut(),
                )
            }),
            libsqlite3_sys::SQLITE_OK
        );
        let failure = host.scope(|| copy.close().unwrap_err());
        let BoundSqliteCloseOwner::Live(live) = failure.owner() else {
            panic!("busy copy must preserve live owner");
        };
        assert_eq!(unsafe { live.connection().handle() }, handle);
        drop(failure); // reachable native abandon custody; not settlement
        assert_eq!(
            host.scope(|| unsafe { libsqlite3_sys::sqlite3_finalize(statement) }),
            libsqlite3_sys::SQLITE_OK
        );
        // Finalizing the statement does not authorize automatic SQL close or
        // release the abandoned actual DB/context. The bounded fixture stays
        // intentionally retained in native custody for the process lifetime.
        assert!(weak.upgrade().is_some());
        assert!(path.exists());
        crate::test_host::end_isolated_process(
            (ticket, host, path),
            "abandoned copy retained with its guard",
        )
    }
}

#[cfg(test)]
struct MechanismProductionControl(std::sync::atomic::AtomicBool);
#[cfg(test)]
impl SqliteInspectionProductionControl for MechanismProductionControl {
    fn checkpoint(&self) -> Result<()> {
        ensure!(
            !self.0.load(std::sync::atomic::Ordering::SeqCst),
            "fixture producer completed"
        );
        Ok(())
    }
    fn complete(&self) -> Result<()> {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
    fn abandoned(&self) {}
}
#[cfg(test)]
fn mechanism_production(kind: SqliteInspectionProductionKind) -> SqliteInspectionProductionPermit {
    SqliteInspectionProductionPermit::issued(
        kind,
        Arc::new(MechanismProductionControl(
            std::sync::atomic::AtomicBool::new(false),
        )),
    )
}

#[cfg(test)]
struct MechanismSqlWork {
    steps: std::sync::atomic::AtomicU64,
    copy: Option<Arc<dyn SqliteInspectionCopyWork>>,
    root: Option<Arc<PinnedDirectory>>,
}
#[cfg(test)]
impl SqliteInspectionSqlWork for MechanismSqlWork {
    fn begin_native_production(
        &self,
        root: &PinnedDirectory,
        _: &OsStr,
    ) -> Result<SqliteInspectionProductionPermit> {
        self.require_destination(root)?;
        Ok(mechanism_production(SqliteInspectionProductionKind::Native))
    }
    fn require_copy_work(&self, supplied: &Arc<dyn SqliteInspectionCopyWork>) -> Result<()> {
        let original = self
            .copy
            .as_ref()
            .context("standalone SQL fixture lacks copy attestation")?;
        ensure!(
            Arc::ptr_eq(original, supplied),
            "foreign mechanism copy work"
        );
        Ok(())
    }
    fn require_destination(&self, destination: &PinnedDirectory) -> Result<()> {
        destination.ensure_path_binding()?;
        if let Some(root) = &self.root {
            ensure!(
                root.is_same_directory(destination)?,
                "foreign mechanism SQL destination"
            );
        }
        Ok(())
    }
    fn checkpoint(&self) -> Result<()> {
        Ok(())
    }
    fn debit_vm_steps(&self, steps: u64) -> Result<()> {
        self.steps
            .fetch_update(
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
                |prior| prior.checked_add(steps).filter(|total| *total <= 1_000_000),
            )
            .map_err(|_| anyhow::anyhow!("bounded mechanism SQL fixture exhausted"))?;
        Ok(())
    }
}
#[cfg(test)]
fn mechanism_sql_work() -> Arc<dyn SqliteInspectionSqlWork> {
    Arc::new(MechanismSqlWork {
        steps: std::sync::atomic::AtomicU64::new(0),
        copy: None,
        root: None,
    })
}

#[cfg(all(test, target_os = "linux"))]
mod guarded_inspection_copy_controls {
    use super::tests::{armed_stock, close_armed, close_bound};
    use super::*;
    use anyhow::Context;
    use std::os::unix::fs::MetadataExt;

    // Native mechanism control only: this does not issue platform authority.
    // Production callers must obtain their work owner from authenticated issue.
    #[derive(Default)]
    struct MechanismCopyWork {
        fail_checkpoint_at: Option<usize>,
        checkpoints: std::sync::atomic::AtomicUsize,
        admissions: std::sync::atomic::AtomicUsize,
        deny_admission: bool,
        outputs: std::sync::Mutex<Vec<Arc<RetainedSqliteDestinationRole>>>,
        output_coordinate: std::sync::Mutex<Option<(PinnedDirectory, std::ffi::OsString)>>,
        cleanup_deadline: std::sync::OnceLock<std::time::Instant>,
        admitted_present: std::sync::OnceLock<bool>,
        cleanup_steps: std::sync::atomic::AtomicU64,
    }
    impl SqliteInspectionCopyWork for MechanismCopyWork {
        fn require_original_native_factory(&self, _: &Arc<OriginalNativeFactory>) -> Result<()> {
            anyhow::bail!("mechanism-only fixture has no original native factory authority")
        }
        fn begin_copy_production(
            &self,
            _: &PinnedDirectory,
            _: &PinnedDirectory,
            _: &OsStr,
        ) -> Result<SqliteInspectionProductionPermit> {
            Ok(mechanism_production(SqliteInspectionProductionKind::Copy))
        }
        fn issue_sql_work(
            self: Arc<Self>,
            root: &PinnedDirectory,
        ) -> Result<Arc<dyn SqliteInspectionSqlWork>> {
            self.checkpoint()?;
            let coordinate = self
                .output_coordinate
                .lock()
                .map_err(|_| anyhow::anyhow!("mechanism coordinate poisoned"))?;
            let (original, _) = coordinate
                .as_ref()
                .context("missing original mechanism destination")?;
            ensure!(
                original.is_same_directory(root)?,
                "foreign mechanism SQL destination"
            );
            let retained = Arc::new(root.try_clone()?);
            drop(coordinate);
            let copy: Arc<dyn SqliteInspectionCopyWork> = self;
            Ok(Arc::new(MechanismSqlWork {
                steps: std::sync::atomic::AtomicU64::new(0),
                copy: Some(copy),
                root: Some(retained),
            }))
        }

        fn create_destination(
            self: Arc<Self>,
            _: &PinnedDirectory,
            _: &OsStr,
        ) -> Result<SqliteInspectionCopyDestination> {
            anyhow::bail!("mechanism-only fixture has no platform namespace authority")
        }
        fn admit_role(
            &self,
            _: &PinnedDirectory,
            root: &PinnedDirectory,
            name: &OsStr,
            role: Option<&SqliteRoleIdentity>,
            reads: u64,
            memory: u64,
        ) -> Result<()> {
            self.cleanup_deadline
                .get_or_init(|| std::time::Instant::now() + std::time::Duration::from_secs(60));
            use std::sync::atomic::Ordering;
            ensure!(!self.deny_admission, "injected original admission refusal");
            ensure!(
                self.admissions.fetch_add(1, Ordering::SeqCst) == 0,
                "original mechanism role is already charged"
            );
            let expected = role
                .map(|r| r.byte_length.checked_mul(2).and_then(|n| n.checked_add(1)))
                .unwrap_or(Some(0))
                .ok_or_else(|| anyhow::anyhow!("fixture read bound overflow"))?;
            ensure!(
                reads == expected && memory == if role.is_some() { 65_536 } else { 0 },
                "native copy advertised unbounded work"
            );
            self.admitted_present
                .set(role.is_some())
                .map_err(|_| anyhow::anyhow!("fixture admission already fixed"))?;
            *self
                .output_coordinate
                .lock()
                .map_err(|_| anyhow::anyhow!("fixture coordinate poisoned"))? =
                Some((root.try_clone()?, name.to_os_string()));
            Ok(())
        }
        fn retain_destination_role(
            &self,
            root: &PinnedDirectory,
            name: &OsStr,
            file: Arc<RetainedSqliteDestinationRole>,
        ) -> Result<()> {
            match self.outputs.lock() {
                Ok(mut outputs) => outputs.push(file),
                Err(poisoned) => {
                    poisoned.into_inner().push(file);
                    anyhow::bail!("fixture outputs poisoned; original output retained")
                }
            }
            let coordinate = self
                .output_coordinate
                .lock()
                .map_err(|_| anyhow::anyhow!("fixture coordinate poisoned"))?;
            let (original, original_name) = coordinate
                .as_ref()
                .context("fixture lacks admitted coordinate")?;
            ensure!(
                original.is_same_directory(root)?
                    && original_name == name
                    && self.admitted_present.get() == Some(&true),
                "fixture retention selects foreign/absent role"
            );
            Ok(())
        }
        fn cleanup_checkpoint(&self) -> Result<std::time::Duration> {
            self.cleanup_deadline
                .get()
                .context("fixture has no original cleanup start")?
                .checked_duration_since(std::time::Instant::now())
                .filter(|remaining| !remaining.is_zero())
                .context("fixture original cleanup window expired")
        }
        fn remaining_cleanup_steps(&self) -> Result<u64> {
            self.cleanup_checkpoint()?;
            1024u64
                .checked_sub(self.cleanup_steps.load(std::sync::atomic::Ordering::SeqCst))
                .context("original mechanism cleanup counter exceeds reservation")
        }

        fn debit_destination_retirement(&self, maximum: u32) -> Result<()> {
            ensure!(
                maximum > 0 && maximum <= i32::MAX as u32,
                "fixture retirement scan bound invalid"
            );
            self.debit_cleanup_steps(
                u64::from(maximum)
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("retirement charge overflow"))?,
            )
        }
        fn debit_cleanup_steps(&self, steps: u64) -> Result<()> {
            self.cleanup_checkpoint()?;
            ensure!(steps > 0, "fixture cleanup debit is empty");
            self.cleanup_steps
                .fetch_update(
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                    |prior| prior.checked_add(steps).filter(|total| *total <= 1024),
                )
                .map_err(|_| anyhow::anyhow!("fixture original cleanup steps exhausted"))?;
            Ok(())
        }
        fn retained_destination_role(
            &self,
            root: &PinnedDirectory,
            name: &OsStr,
        ) -> Result<Option<Arc<RetainedSqliteDestinationRole>>> {
            self.cleanup_checkpoint()?;
            let coordinate = self
                .output_coordinate
                .lock()
                .map_err(|_| anyhow::anyhow!("fixture coordinate poisoned"))?;
            let (original, original_name) = coordinate
                .as_ref()
                .context("fixture lacks original coordinate")?;
            ensure!(
                original.is_same_directory(root)? && original_name == name,
                "fixture selects foreign cleanup coordinate"
            );
            let outputs = self
                .outputs
                .lock()
                .map_err(|_| anyhow::anyhow!("fixture outputs poisoned"))?;
            let present = *self
                .admitted_present
                .get()
                .context("fixture lacks original presence")?;
            ensure!(
                outputs.len() == usize::from(present),
                "fixture missing present pin or unexpected absent pin"
            );
            Ok(outputs.first().cloned())
        }
        fn checkpoint(&self) -> Result<()> {
            use std::sync::atomic::Ordering;
            let count = self.checkpoints.fetch_add(1, Ordering::SeqCst) + 1;
            ensure!(
                self.fail_checkpoint_at.is_none_or(|limit| count < limit),
                "injected original service expiry"
            );
            Ok(())
        }
        fn remaining_service_window(&self) -> Result<std::time::Duration> {
            self.checkpoint()?;
            Ok(std::time::Duration::from_secs(1))
        }
    }

    fn pin(path: &std::path::Path) -> PinnedDirectory {
        PinnedDirectory::open(path).unwrap().unwrap()
    }

    struct MechanismWitnessWork {
        parent: PinnedDirectory,
        deadline: lillux::time::MonotonicDeadline,
        steps: std::sync::atomic::AtomicU64,
    }
    impl OriginalWitnessCleanupWork for MechanismWitnessWork {
        fn prepay_original_removal_parent(&self, bytes: u64, steps: u64) -> Result<()> {
            ensure!(bytes > 0 && steps == 2, "invalid mechanism parent quote");
            self.debit_steps(steps)
        }
        fn checkpoint(&self) -> Result<()> {
            ensure!(
                !self.deadline.has_elapsed(),
                "original witness fixture expired"
            );
            self.parent.ensure_path_binding()
        }
        fn require_namespace(&self, parent: &PinnedDirectory) -> Result<()> {
            ensure!(
                self.parent.is_same_directory(parent)?,
                "foreign original witness fixture namespace"
            );
            Ok(())
        }
        fn debit_steps(&self, steps: u64) -> Result<()> {
            self.checkpoint()?;
            ensure!(steps > 0, "empty original witness fixture debit");
            self.steps
                .fetch_update(
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                    |prior| prior.checked_add(steps).filter(|total| *total <= 2048),
                )
                .map_err(|_| anyhow::anyhow!("original witness fixture budget exhausted"))?;
            Ok(())
        }
        fn debit_retirement(&self, maximum: u32) -> Result<()> {
            ensure!(
                maximum > 0 && maximum <= i32::MAX as u32,
                "invalid witness fixture scan"
            );
            self.debit_steps(
                u64::from(maximum)
                    .checked_add(1)
                    .context("witness fixture charge overflow")?,
            )
        }
    }
    #[test]
    fn dropped_witness_with_prepared_parent_retains_the_parent() {
        // The witness tests that count this abandoned kind run under the same
        // native-host serial guard, so no count here races another.
        let _host = crate::test_host::TestNativeHost::acquire();
        let tmp = tempfile::tempdir().unwrap();
        let root = pin(tmp.path());
        let name = OsStr::new("prepared.original");
        let owner = Arc::new(RetainedOriginalWitnessFile::created(
            root.atomic_create_regular(
                name,
                b"prepared original witness",
                0o600,
                crate::SQLITE_TEMP_PURPOSE,
            )
            .unwrap()
            .unwrap(),
            root.identity().unwrap(),
            name.to_os_string(),
        ));
        let work = Arc::new(MechanismWitnessWork {
            parent: root.try_clone().unwrap(),
            deadline: lillux::time::MonotonicDeadline::after(std::time::Duration::from_secs(60)),
            steps: std::sync::atomic::AtomicU64::new(0),
        });
        let cleanup: Arc<dyn OriginalWitnessCleanupWork> = work.clone();
        let prepared = owner
            .prepare_original_removal_parent(cleanup, &root, work.deadline)
            .unwrap();
        let kind = "lillux-sqlite::witness-removal-parent";
        let before = lillux::custody::abandoned_custody().count(kind);
        drop(prepared);
        drop(owner);
        assert_eq!(lillux::custody::abandoned_custody().count(kind), before + 1);
    }
    // Actual ordinary witness mechanics; this fixture has no platform authority.
    #[test]
    fn original_witness_removal_proof_and_alias_retirement_preserve_foreign_owner() {
        let host = crate::test_host::TestNativeHost::acquire();
        let tmp = tempfile::tempdir().unwrap();
        let root = pin(tmp.path());
        let (initialize, ticket) = armed_stock(&host, &tmp.path().join("initialize.sqlite3"));
        close_armed(&host, initialize, ticket);
        let name = OsStr::new("created.original");
        let owner = Arc::new(RetainedOriginalWitnessFile::created(
            root.atomic_create_regular(
                name,
                b"exact original witness",
                0o600,
                crate::SQLITE_TEMP_PURPOSE,
            )
            .unwrap()
            .unwrap(),
            root.identity().unwrap(),
            name.to_os_string(),
        ));
        let alias = owner.clone();
        let original_identity = owner.identity_original().unwrap();
        let work = Arc::new(MechanismWitnessWork {
            parent: root.try_clone().unwrap(),
            deadline: lillux::time::MonotonicDeadline::after(std::time::Duration::from_secs(60)),
            steps: std::sync::atomic::AtomicU64::new(0),
        });
        let cleanup: Arc<dyn OriginalWitnessCleanupWork> = work.clone();
        let kind = "lillux-sqlite::witness-removal-parent";
        let retained_before = lillux::custody::abandoned_custody().count(kind);
        let removed = owner
            .remove_original_planned(cleanup, &root, work.deadline)
            .unwrap();
        assert!(root.entry_no_follow(name).unwrap().is_none());
        assert_eq!(
            alias.identity_original().unwrap().inode(),
            original_identity.inode()
        );
        let foreign = Arc::new(RetainedOriginalWitnessFile::created(
            root.atomic_create_regular(
                OsStr::new("foreign"),
                b"foreign witness",
                0o600,
                crate::SQLITE_TEMP_PURPOSE,
            )
            .unwrap()
            .unwrap(),
            root.identity().unwrap(),
            std::ffi::OsString::from("foreign"),
        ));
        let charged = work.steps.load(std::sync::atomic::Ordering::SeqCst);
        assert!(foreign.retire_removed_original(&removed, 512).is_err());
        assert_eq!(
            work.steps.load(std::sync::atomic::Ordering::SeqCst),
            charged
        );
        assert_eq!(foreign.read_original(8192).unwrap(), b"foreign witness");
        let retired = owner.retire_removed_original(&removed, 512).unwrap();
        retired.require_original(&alias).unwrap();
        assert!(alias.metadata_original().is_err());
        assert!(alias.read_original(8192).is_err());
        assert_eq!(foreign.read_original(8192).unwrap(), b"foreign witness");
        // Durably removed and retired is settled: the last drop releases the
        // cleanup parent and nothing is retained for the process lifetime.
        drop(retired);
        drop(removed);
        drop(alias);
        drop(owner);
        assert_eq!(
            lillux::custody::abandoned_custody().count(kind),
            retained_before
        );
        // The never-removed foreign fixture still holds its open original and
        // is dropped only after the settled-owner assertion.
        drop(foreign);
    }
    #[test]
    fn dropped_witness_removed_but_unretired_retains_the_parent() {
        let host = crate::test_host::TestNativeHost::acquire();
        let tmp = tempfile::tempdir().unwrap();
        let root = pin(tmp.path());
        let (initialize, ticket) = armed_stock(&host, &tmp.path().join("initialize.sqlite3"));
        close_armed(&host, initialize, ticket);
        let name = OsStr::new("unretired.original");
        let owner = Arc::new(RetainedOriginalWitnessFile::created(
            root.atomic_create_regular(
                name,
                b"unretired witness",
                0o600,
                crate::SQLITE_TEMP_PURPOSE,
            )
            .unwrap()
            .unwrap(),
            root.identity().unwrap(),
            name.to_os_string(),
        ));
        let work = Arc::new(MechanismWitnessWork {
            parent: root.try_clone().unwrap(),
            deadline: lillux::time::MonotonicDeadline::after(std::time::Duration::from_secs(60)),
            steps: std::sync::atomic::AtomicU64::new(0),
        });
        let cleanup: Arc<dyn OriginalWitnessCleanupWork> = work.clone();
        let removed = owner
            .remove_original_planned(cleanup, &root, work.deadline)
            .unwrap();
        assert!(root.entry_no_follow(name).unwrap().is_none());
        let kind = "lillux-sqlite::witness-removal-parent";
        let before = lillux::custody::abandoned_custody().count(kind);
        drop(removed);
        drop(owner);
        assert_eq!(lillux::custody::abandoned_custody().count(kind), before + 1);
    }
    #[test]
    fn native_production_requires_genuine_terminal_acknowledgement() {
        let host = crate::test_host::TestNativeHost::acquire();
        let tmp = tempfile::tempdir().unwrap();
        let name = OsStr::new("runtime.sqlite3");
        let (source, source_ticket) = armed_stock(&host, &tmp.path().join(name));
        host.scope(|| {
            source
                .execute_batch(
                    "CREATE TABLE original(value INTEGER); INSERT INTO original VALUES(11)",
                )
                .unwrap()
        });
        close_armed(&host, source, source_ticket);
        let root = Arc::new(pin(tmp.path()));
        let lease: SqliteCopyLease = Arc::new(());
        let work = mechanism_sql_work();
        let (owner, ticket) = host.armed(|| {
            ReadonlySqliteCopy::open(root.clone(), name, lease.clone(), work.clone()).unwrap()
        });
        let permit = owner.context.production.as_ref().unwrap();
        permit.checkpoint().unwrap();
        assert!(permit.acknowledge_synchronous().is_err());
        assert_eq!(
            host.scope(|| {
                owner
                    .connection()
                    .query_row("SELECT value FROM original", [], |row| row.get::<_, i64>(0))
                    .unwrap()
            }),
            11
        );
        let closed = host.scope(|| owner.close().unwrap());
        host.retire_raw(ticket);
        closed.acknowledge_producer_terminal().unwrap();
        closed.acknowledge_producer_terminal().unwrap();
        assert!(
            closed
                .context
                .production
                .as_ref()
                .unwrap()
                .checkpoint()
                .is_err()
        );
        closed.require_original(&root, name, &lease, &work).unwrap();
    }

    // Native mechanism controls only. These do not mint disposal authority.
    #[test]
    fn native_output_close_refuses_linked_inode_and_live_stock_alias() {
        let host = crate::test_host::TestNativeHost::acquire();
        use std::os::fd::AsRawFd;
        let tmp = tempfile::tempdir().unwrap();
        let root = pin(tmp.path());
        let name = OsStr::new("original.sqlite3");
        let (peer, peer_ticket) = armed_stock(&host, &tmp.path().join(name));
        host.scope(|| {
            peer.execute_batch(
                "CREATE TABLE original(value INTEGER); INSERT INTO original VALUES(7)",
            )
            .unwrap()
        });
        let owner = Arc::new(RetainedSqliteDestinationRole::created(
            root.open_regular(name, false).unwrap().unwrap(),
            root.identity().unwrap(),
            name.to_os_string(),
        ));
        let identity = owner.metadata_original().unwrap();
        let alias = owner.clone();
        let linked = owner
            .with_file(|file| {
                Ok(binding::close_original_output(
                    file.as_raw_fd(),
                    identity.dev(),
                    identity.ino(),
                    1023,
                ))
            })
            .unwrap();
        assert!(!linked.consumed);
        assert_ne!(linked.code, libsqlite3_sys::SQLITE_OK);
        std::fs::remove_file(tmp.path().join(name)).unwrap();
        let live = alias
            .with_file(|file| {
                Ok(binding::close_original_output(
                    file.as_raw_fd(),
                    identity.dev(),
                    identity.ino(),
                    1023,
                ))
            })
            .unwrap();
        assert!(!live.consumed);
        assert_ne!(live.code, libsqlite3_sys::SQLITE_OK);
        assert_eq!(alias.metadata_original().unwrap().ino(), identity.ino());
        assert_eq!(
            host.scope(|| {
                peer.query_row("SELECT value FROM original", [], |row| row.get::<_, i64>(0))
                    .unwrap()
            }),
            7
        );
        close_armed(&host, peer, peer_ticket);
        // Last-drop preserves the original; this fixture grants no settlement.
    }

    #[test]
    fn native_output_close_consumes_only_original_unlinked_coordinate() {
        let host = crate::test_host::TestNativeHost::acquire();
        use std::os::fd::{AsRawFd, IntoRawFd};
        let tmp = tempfile::tempdir().unwrap();
        // An unrelated live inode makes a one-check scan insufficient.
        let (unrelated, unrelated_ticket) =
            armed_stock(&host, &tmp.path().join("initialize.sqlite3"));
        let root = pin(tmp.path());
        let name = OsStr::new("output");
        let owner = Arc::new(RetainedSqliteDestinationRole::created(
            root.open_regular_create(name, true, true, 0o600).unwrap(),
            root.identity().unwrap(),
            name.to_os_string(),
        ));
        let alias = owner.clone();
        let metadata = owner.metadata_original().unwrap();
        std::fs::remove_file(tmp.path().join(name)).unwrap();
        let refused = owner
            .with_file(|file| {
                Ok(binding::close_original_output(
                    file.as_raw_fd(),
                    metadata.dev(),
                    metadata.ino().wrapping_add(1),
                    1023,
                ))
            })
            .unwrap();
        assert!(!refused.consumed);
        assert_eq!(alias.metadata_original().unwrap().ino(), metadata.ino());
        let bounded = owner
            .with_file(|file| {
                Ok(binding::close_original_output(
                    file.as_raw_fd(),
                    metadata.dev(),
                    metadata.ino(),
                    1,
                ))
            })
            .unwrap();
        assert!(!bounded.consumed);
        assert_ne!(bounded.code, libsqlite3_sys::SQLITE_OK);
        assert_eq!(alias.metadata_original().unwrap().ino(), metadata.ino());
        let invalid = owner
            .with_file(|file| {
                Ok(binding::close_original_output(
                    file.as_raw_fd(),
                    metadata.dev(),
                    metadata.ino(),
                    u32::MAX,
                ))
            })
            .unwrap();
        assert!(!invalid.consumed);
        assert_eq!(invalid.code, libsqlite3_sys::SQLITE_MISUSE);
        let mut state = owner.state().unwrap();
        let DestinationRoleState::Open(file) =
            std::mem::replace(&mut *state, DestinationRoleState::Retired)
        else {
            panic!("missing original output")
        };
        let fd = file.into_raw_fd();
        *state = DestinationRoleState::PendingRaw(fd);
        let result = binding::close_original_output(fd, metadata.dev(), metadata.ino(), 1023);
        assert!(result.consumed);
        assert_eq!(result.code, libsqlite3_sys::SQLITE_OK);
        assert_eq!(result.observed_errno, 0);
        *state = DestinationRoleState::Retired;
        drop(state);
        assert!(alias.metadata_original().is_err());
        close_armed(&host, unrelated, unrelated_ticket);
        // No public retirement proof is constructed by this native-layer control.
    }

    #[test]
    fn actual_copy_sql_fixture_attests_original_copy_work_and_closed_context() {
        let host = crate::test_host::TestNativeHost::acquire();
        let source_dir = tempfile::tempdir().unwrap();
        let output_dir = tempfile::tempdir().unwrap();
        let name = OsStr::new("runtime.sqlite3");
        let (original, original_ticket) = armed_stock(&host, &source_dir.path().join(name));
        host.scope(|| {
            original
                .execute_batch(
                    "CREATE TABLE evidence(value INTEGER); INSERT INTO evidence VALUES(9)",
                )
                .unwrap()
        });
        close_armed(&host, original, original_ticket);
        let source = pin(source_dir.path());
        let root = Arc::new(pin(output_dir.path()));
        let work = Arc::new(MechanismCopyWork::default());
        let length = std::fs::metadata(source_dir.path().join(name))
            .unwrap()
            .len();
        copy_regular_without_sqlite_alias(&source, name, &root, length, work.as_ref())
            .unwrap()
            .unwrap();
        let sql = work.clone().issue_sql_work(&root).unwrap();
        let copy: Arc<dyn SqliteInspectionCopyWork> = work.clone();
        sql.require_copy_work(&copy).unwrap();
        let foreign: Arc<dyn SqliteInspectionCopyWork> = Arc::new(MechanismCopyWork::default());
        assert!(sql.require_copy_work(&foreign).is_err());
        let lease: SqliteCopyLease = Arc::new(());
        let (inspection, ticket) = host.armed(|| {
            ReadonlySqliteCopy::open(root.clone(), name, lease.clone(), sql.clone()).unwrap()
        });
        assert_eq!(
            host.scope(|| {
                inspection
                    .connection()
                    .query_row("SELECT value FROM evidence", [], |row| row.get::<_, i64>(0))
                    .unwrap()
            }),
            9
        );
        let closed = host.scope(|| inspection.close().unwrap());
        host.retire_raw(ticket);
        closed.require_original(&root, name, &lease, &sql).unwrap();
        assert!(work.remaining_cleanup_steps().unwrap() == 1024);
        work.debit_destination_retirement(3).unwrap();
        assert_eq!(work.remaining_cleanup_steps().unwrap(), 1020);
        assert!(work.debit_destination_retirement(u32::MAX).is_err());
        assert_eq!(work.remaining_cleanup_steps().unwrap(), 1020);
    }

    #[test]
    fn multichunk_copy_receipt_binds_original_and_copied_bytes() {
        let _serial = crate::test_host::serial_guard();
        let source_dir = tempfile::tempdir().unwrap();
        let output_dir = tempfile::tempdir().unwrap();
        let source = pin(source_dir.path());
        let destination = pin(output_dir.path());
        for length in [0, 65_536, 65_537, 3 * 65_536 + 19] {
            let name = format!("source-{length}");
            let bytes = (0..length).map(|i| (i % 251) as u8).collect::<Vec<_>>();
            std::fs::write(source_dir.path().join(&name), &bytes).unwrap();
            let original = std::fs::metadata(source_dir.path().join(&name)).unwrap();
            let receipt = copy_regular_without_sqlite_alias(
                &source,
                OsStr::new(&name),
                &destination,
                length as u64,
                &MechanismCopyWork::default(),
            )
            .unwrap()
            .unwrap();
            assert_eq!(receipt.source_identity.device, original.dev());
            assert_eq!(receipt.source_identity.inode, original.ino());
            assert_eq!(receipt.source_identity.byte_length, length as u64);
            assert_eq!(receipt.copied_byte_length, length as u64);
            assert_eq!(receipt.copied_sha256, lillux::sha256_hex(&bytes));
            assert_eq!(
                receipt.source_identity.content_sha256.as_ref(),
                Some(&receipt.copied_sha256)
            );
            assert_eq!(std::fs::read(output_dir.path().join(&name)).unwrap(), bytes);
        }
    }

    #[test]
    fn denied_work_never_creates_destination_and_partial_work_never_refunds() {
        let _serial = crate::test_host::serial_guard();
        let source_dir = tempfile::tempdir().unwrap();
        let output_dir = tempfile::tempdir().unwrap();
        let source = pin(source_dir.path());
        let destination = pin(output_dir.path());
        let name = OsStr::new("source");
        let bytes = vec![7u8; 2 * 65_536 + 1];
        std::fs::write(source_dir.path().join(name), &bytes).unwrap();
        let refused = MechanismCopyWork {
            deny_admission: true,
            ..Default::default()
        };
        assert!(
            copy_regular_without_sqlite_alias(
                &source,
                name,
                &destination,
                bytes.len() as u64,
                &refused
            )
            .is_err()
        );
        assert!(!output_dir.path().join(name).exists());
        let interrupted = MechanismCopyWork {
            fail_checkpoint_at: Some(5),
            ..Default::default()
        };
        assert!(
            copy_regular_without_sqlite_alias(
                &source,
                name,
                &destination,
                bytes.len() as u64,
                &interrupted
            )
            .is_err()
        );
        assert_eq!(
            std::fs::metadata(output_dir.path().join(name))
                .unwrap()
                .len(),
            65_536
        );
        assert_eq!(
            interrupted
                .admissions
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert!(
            copy_regular_without_sqlite_alias(
                &source,
                name,
                &destination,
                bytes.len() as u64,
                &interrupted
            )
            .is_err()
        );
        assert_eq!(std::fs::read(source_dir.path().join(name)).unwrap(), bytes);
        let output = {
            let outputs = interrupted.outputs.lock().unwrap();
            assert_eq!(
                outputs.len(),
                1,
                "partial failure retains exact created output once"
            );
            outputs[0].clone()
        };
        let from_work = interrupted
            .retained_destination_role(&destination, name)
            .unwrap()
            .unwrap();
        assert!(Arc::ptr_eq(&from_work, &output));
        assert!(
            interrupted
                .retained_destination_role(&source, name)
                .is_err()
        );
        assert!(
            interrupted
                .retained_destination_role(&destination, OsStr::new("foreign"))
                .is_err()
        );
        let original = output.metadata_original().unwrap();
        assert_eq!(original.len(), 65_536);
        let named = std::fs::metadata(output_dir.path().join(name)).unwrap();
        assert_eq!((original.dev(), original.ino()), (named.dev(), named.ino()));
        let retained = Arc::downgrade(&output);
        std::fs::rename(
            output_dir.path().join(name),
            output_dir.path().join("partial-original"),
        )
        .unwrap();
        std::fs::write(output_dir.path().join(name), b"foreign replacement").unwrap();
        let foreign = std::fs::metadata(output_dir.path().join(name)).unwrap();
        assert_ne!(
            (original.dev(), original.ino()),
            (foreign.dev(), foreign.ino())
        );
        assert_eq!(
            (
                output.metadata_original().unwrap().dev(),
                output.metadata_original().unwrap().ino()
            ),
            (original.dev(), original.ino()),
            "retention never reopens the replaced name"
        );
        let after_replacement = interrupted
            .retained_destination_role(&destination, name)
            .unwrap()
            .unwrap();
        assert!(
            Arc::ptr_eq(&after_replacement, &output),
            "getter returns original pin without name reconstruction"
        );
        drop(output);
        assert!(
            retained.upgrade().is_some(),
            "same work owns partial output without receipt/refund"
        );
    }

    #[test]
    fn cleanup_fixture_preserves_absence_missing_pin_and_original_expiry() {
        let _serial = crate::test_host::serial_guard();
        let source_dir = tempfile::tempdir().unwrap();
        let output_dir = tempfile::tempdir().unwrap();
        let source = pin(source_dir.path());
        let destination = pin(output_dir.path());
        let absent = MechanismCopyWork::default();
        assert!(
            copy_regular_without_sqlite_alias(
                &source,
                OsStr::new("absent"),
                &destination,
                0,
                &absent
            )
            .unwrap()
            .is_none()
        );
        assert!(
            absent
                .retained_destination_role(&destination, OsStr::new("absent"))
                .unwrap()
                .is_none()
        );
        absent.debit_cleanup_steps(1024).unwrap();
        assert!(absent.debit_cleanup_steps(1).is_err());
        assert!(absent.debit_cleanup_steps(0).is_err());
        assert!(absent.debit_cleanup_steps(u64::MAX).is_err());
        assert_eq!(
            absent
                .cleanup_steps
                .load(std::sync::atomic::Ordering::SeqCst),
            1024
        );
        let foreign = Arc::new(RetainedSqliteDestinationRole::created(
            destination
                .open_regular_create(OsStr::new("absent"), true, true, 0o600)
                .unwrap(),
            destination.identity().unwrap(),
            std::ffi::OsString::from("absent"),
        ));
        assert!(
            absent
                .retain_destination_role(&destination, OsStr::new("absent"), foreign)
                .is_err()
        );
        assert!(
            absent
                .retained_destination_role(&destination, OsStr::new("absent"))
                .is_err()
        );
        let missing = MechanismCopyWork::default();
        missing
            .admit_role(
                &source,
                &destination,
                OsStr::new("present"),
                Some(&SqliteRoleIdentity {
                    device: 1,
                    inode: 1,
                    byte_length: 0,
                    content_sha256: None,
                }),
                1,
                65_536,
            )
            .unwrap();
        assert!(
            missing
                .retained_destination_role(&destination, OsStr::new("present"))
                .is_err()
        );
        let expired = MechanismCopyWork::default();
        expired
            .cleanup_deadline
            .set(std::time::Instant::now() - std::time::Duration::from_secs(1))
            .unwrap();
        assert!(expired.debit_cleanup_steps(1).is_err());
        assert_eq!(
            expired
                .cleanup_steps
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert!(expired.cleanup_checkpoint().is_err());
        assert!(expired.cleanup_checkpoint().is_err()); // same start, no renewal
    }

    #[test]
    fn original_expiry_before_output_hash_retains_created_output_without_receipt() {
        let _serial = crate::test_host::serial_guard();
        let source_dir = tempfile::tempdir().unwrap();
        let output_dir = tempfile::tempdir().unwrap();
        let source = pin(source_dir.path());
        let destination = pin(output_dir.path());
        std::fs::write(source_dir.path().join("source"), b"exact bytes").unwrap();
        let interrupted = MechanismCopyWork {
            fail_checkpoint_at: Some(7),
            ..Default::default()
        };
        assert!(
            copy_regular_without_sqlite_alias(
                &source,
                OsStr::new("source"),
                &destination,
                11,
                &interrupted
            )
            .is_err()
        );
        assert_eq!(
            std::fs::read(output_dir.path().join("source")).unwrap(),
            b"exact bytes"
        );
        assert_eq!(
            interrupted
                .admissions
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }

    #[test]
    fn native_chunks_refuse_offset_length_and_buffer_changes() {
        let _serial = crate::test_host::serial_guard();
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("source"), vec![3u8; 65_537]).unwrap();
        let source = pin(directory.path());
        let retained = source
            .open_mount_entry(OsStr::new("source"))
            .unwrap()
            .unwrap();
        let mut buffer = [0u8; 65_536];
        assert_eq!(
            binding::read_unbound_regular_chunk(&retained, 65_536, 65_537, &mut buffer).unwrap(),
            1
        );
        assert_eq!(buffer[0], 3);
        assert!(
            binding::read_unbound_regular_chunk(&retained, 65_538, 65_537, &mut buffer).is_err()
        );
        assert!(binding::read_unbound_regular_chunk(&retained, 0, 65_536, &mut buffer).is_err());
        assert!(binding::read_unbound_regular_chunk(&retained, 0, 65_537, &mut []).is_err());
        assert!(
            binding::read_unbound_regular_chunk(&retained, 0, 65_537, &mut vec![0; 65_537])
                .is_err()
        );
        std::fs::OpenOptions::new()
            .write(true)
            .open(directory.path().join("source"))
            .unwrap()
            .set_len(65_536)
            .unwrap();
        assert!(binding::read_unbound_regular_chunk(&retained, 0, 65_537, &mut buffer).is_err());
    }

    #[test]
    fn copy_refuses_bounds_nonregular_and_incumbent_without_overwrite() {
        let _serial = crate::test_host::serial_guard();
        let source_dir = tempfile::tempdir().unwrap();
        let output_dir = tempfile::tempdir().unwrap();
        let source = pin(source_dir.path());
        let destination = pin(output_dir.path());
        assert!(
            copy_regular_without_sqlite_alias(
                &source,
                OsStr::new("absent"),
                &destination,
                0,
                &MechanismCopyWork::default()
            )
            .unwrap()
            .is_none()
        );
        std::fs::create_dir(source_dir.path().join("directory")).unwrap();
        assert!(
            copy_regular_without_sqlite_alias(
                &source,
                OsStr::new("directory"),
                &destination,
                100,
                &MechanismCopyWork::default()
            )
            .is_err()
        );
        std::fs::write(source_dir.path().join("source"), b"original source").unwrap();
        assert!(
            copy_regular_without_sqlite_alias(
                &source,
                OsStr::new("source"),
                &destination,
                1,
                &MechanismCopyWork::default()
            )
            .is_err()
        );
        assert!(!output_dir.path().join("source").exists());
        std::fs::write(output_dir.path().join("source"), b"incumbent").unwrap();
        assert!(
            copy_regular_without_sqlite_alias(
                &source,
                OsStr::new("source"),
                &destination,
                100,
                &MechanismCopyWork::default()
            )
            .is_err()
        );
        assert_eq!(
            std::fs::read(output_dir.path().join("source")).unwrap(),
            b"incumbent"
        );
    }

    #[test]
    fn copy_refuses_live_sqlite_alias_before_destination_creation() {
        let host = crate::test_host::TestNativeHost::acquire();
        let source_dir = tempfile::tempdir().unwrap();
        let runtime = Arc::new(pin(source_dir.path()));
        let (owner, ticket) = host.armed(|| {
            BoundSqliteConnection::open_or_create(runtime.clone(), OsStr::new("scheduler.sqlite3"))
                .unwrap()
        });
        host.scope(|| {
            // The bound VFS admits no rollback journal, so the created main
            // enters WAL through an in-memory journal, as the original bound
            // open stages it.
            owner.connection().execute_batch("PRAGMA journal_mode=MEMORY; PRAGMA journal_mode=WAL; CREATE TABLE copy_probe(value INTEGER); INSERT INTO copy_probe VALUES(7);").unwrap();
            owner.establish_wal_binding().unwrap();
        });
        for (index, role) in [
            "scheduler.sqlite3",
            "scheduler.sqlite3-wal",
            "scheduler.sqlite3-shm",
        ]
        .into_iter()
        .enumerate()
        {
            let output_dir = tempfile::tempdir().unwrap();
            let destination = pin(output_dir.path());
            let alias = format!("copy-source-{index}");
            std::fs::hard_link(source_dir.path().join(role), source_dir.path().join(&alias))
                .unwrap();
            assert!(
                copy_regular_without_sqlite_alias(
                    &runtime,
                    OsStr::new(&alias),
                    &destination,
                    1_048_576,
                    &MechanismCopyWork::default()
                )
                .is_err()
            );
            assert!(!output_dir.path().join(&alias).exists());
            let value: i64 = host.scope(|| {
                owner.require_binding().unwrap();
                owner
                    .connection()
                    .query_row("SELECT value FROM copy_probe", [], |row| row.get(0))
                    .unwrap()
            });
            assert_eq!(value, 7);
        }
        close_bound(&host, owner, ticket);
    }

    #[test]
    fn copy_refuses_source_mutation_or_source_destination_replacement_between_chunks() {
        let _serial = crate::test_host::serial_guard();
        for action in ["mutate", "source-replace", "destination-replace"] {
            let source_dir = tempfile::tempdir().unwrap();
            let output_dir = tempfile::tempdir().unwrap();
            let source = pin(source_dir.path());
            let destination = pin(output_dir.path());
            let input_path = source_dir.path().join("source");
            let output_path = output_dir.path().join("source");
            std::fs::write(&input_path, vec![7u8; 131_073]).unwrap();
            let source_hook = input_path.clone();
            let output_hook = output_path.clone();
            COPY_AFTER_FIRST_CHUNK.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(move || match action {
                    "mutate" => std::fs::write(&source_hook, vec![8u8; 131_073]).unwrap(),
                    "source-replace" => {
                        std::fs::remove_file(&source_hook).unwrap();
                        std::fs::write(&source_hook, b"foreign source").unwrap();
                    }
                    _ => {
                        std::fs::remove_file(&output_hook).unwrap();
                        std::fs::write(&output_hook, b"foreign destination").unwrap();
                    }
                }))
            });
            assert!(
                copy_regular_without_sqlite_alias(
                    &source,
                    OsStr::new("source"),
                    &destination,
                    131_073,
                    &MechanismCopyWork::default()
                )
                .is_err()
            );
            if action == "source-replace" {
                assert_eq!(std::fs::read(&input_path).unwrap(), b"foreign source");
            }
            if action == "destination-replace" {
                assert_eq!(std::fs::read(&output_path).unwrap(), b"foreign destination");
            }
        }
    }

    #[test]
    fn next_chunk_refuses_stock_sqlite_adoption_and_keeps_caller_copy_directory() {
        let host = std::rc::Rc::new(crate::test_host::TestNativeHost::acquire());
        let source_dir = tempfile::tempdir().unwrap();
        let output_dir = tempfile::tempdir().unwrap();
        let source = pin(source_dir.path());
        let destination = pin(output_dir.path());
        let path = source_dir.path().join("source");
        // A real SQLite file larger than one chunk, fully closed before copying.
        let (seed, seed_ticket) = armed_stock(&host, &path);
        host.scope(|| {
            seed.execute_batch(
                "CREATE TABLE payload(value BLOB); INSERT INTO payload VALUES(zeroblob(200000));",
            )
            .unwrap()
        });
        close_armed(&host, seed, seed_ticket);
        let peer = std::rc::Rc::new(std::cell::RefCell::new(
            None::<(Connection, crate::native::RawTestTicket)>,
        ));
        let retained_peer = peer.clone();
        let hook_host = host.clone();
        COPY_AFTER_FIRST_CHUNK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                let (connection, ticket) = armed_stock(&hook_host, &path);
                hook_host.scope(|| connection.execute_batch("BEGIN IMMEDIATE;").unwrap());
                *retained_peer.borrow_mut() = Some((connection, ticket));
            }))
        });
        assert!(
            copy_regular_without_sqlite_alias(
                &source,
                OsStr::new("source"),
                &destination,
                1_048_576,
                &MechanismCopyWork::default()
            )
            .is_err()
        );
        assert!(output_dir.path().join("source").exists());
        let (connection, ticket) = peer.borrow_mut().take().unwrap();
        let length: i64 = host.scope(|| {
            connection
                .query_row("SELECT length(value) FROM payload", [], |row| row.get(0))
                .unwrap()
        });
        assert_eq!(length, 200000);
        host.scope(|| connection.execute_batch("ROLLBACK;").unwrap());
        close_armed(&host, connection, ticket);
        // The caller still owns its original directory lease and partial file;
        // this function never unlinks it or substitutes a cleanup authority.
        assert!(output_dir.path().join("source").exists());
    }
}

#[cfg(all(test, target_os = "linux"))]
mod original_source_verification_controls {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    fn original(path: &std::path::Path, bytes: &[u8]) -> SqliteRoleIdentity {
        let metadata = std::fs::metadata(path).unwrap();
        SqliteRoleIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
            byte_length: metadata.len(),
            content_sha256: Some(lillux::sha256_hex(bytes)),
        }
    }

    #[test]
    fn multi_chunk_verification_refuses_same_inode_same_length_rewrite() {
        let _serial = crate::test_host::serial_guard();
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("original");
        let bytes = vec![0x51; 3 * 65_536 + 17];
        std::fs::write(&path, &bytes).unwrap();
        let expected = original(&path, &bytes);
        let root = PinnedDirectory::open(temp.path()).unwrap().unwrap();
        verify_regular_without_sqlite_alias(&root, OsStr::new("original"), Some(&expected))
            .unwrap();
        let mut changed = bytes.clone();
        changed[65_536 + 3] ^= 1;
        std::fs::write(&path, &changed).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), expected.inode);
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            expected.byte_length
        );
        let error =
            verify_regular_without_sqlite_alias(&root, OsStr::new("original"), Some(&expected))
                .unwrap_err();
        assert!(format!("{error:#}").contains("bytes differ from signed preview"));
        assert_eq!(std::fs::read(&path).unwrap(), changed);
        assert_eq!(root.entry_names_bounded(4).unwrap().len(), 1);
    }

    #[test]
    fn verification_refuses_in_place_change_between_native_chunks() {
        let _serial = crate::test_host::serial_guard();
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("original");
        let bytes = vec![0x33; 2 * 65_536 + 1];
        std::fs::write(&path, &bytes).unwrap();
        let expected = original(&path, &bytes);
        let root = PinnedDirectory::open(temp.path()).unwrap().unwrap();
        let edit = path.clone();
        VERIFY_AFTER_FIRST_CHUNK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                use std::io::{Seek, SeekFrom, Write};
                let mut file = std::fs::OpenOptions::new().write(true).open(&edit).unwrap();
                file.seek(SeekFrom::Start(65_536)).unwrap();
                file.write_all(&[0x44]).unwrap();
                file.sync_all().unwrap();
            }))
        });
        assert!(
            verify_regular_without_sqlite_alias(&root, OsStr::new("original"), Some(&expected))
                .is_err()
        );
        VERIFY_AFTER_FIRST_CHUNK.with(|hook| assert!(hook.borrow().is_none()));
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), expected.inode);
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            expected.byte_length
        );
    }

    #[test]
    fn verification_distinguishes_absent_empty_and_replaced_sources() {
        let _serial = crate::test_host::serial_guard();
        let temp = tempfile::TempDir::new().unwrap();
        let root = PinnedDirectory::open(temp.path()).unwrap().unwrap();
        let name = OsStr::new("original");
        verify_regular_without_sqlite_alias(&root, name, None).unwrap();
        let path = temp.path().join(name);
        std::fs::write(&path, b"").unwrap();
        let expected = original(&path, b"");
        verify_regular_without_sqlite_alias(&root, name, Some(&expected)).unwrap();
        assert!(verify_regular_without_sqlite_alias(&root, name, None).is_err());
        // Keep the original inode alive so the replacement cannot reuse it.
        let original_pin = root.open_mount_entry(name).unwrap().unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"").unwrap();
        assert_ne!(std::fs::metadata(&path).unwrap().ino(), expected.inode);
        assert!(verify_regular_without_sqlite_alias(&root, name, Some(&expected)).is_err());
        drop(original_pin);
        std::fs::remove_file(&path).unwrap();
        assert!(verify_regular_without_sqlite_alias(&root, name, Some(&expected)).is_err());
        verify_regular_without_sqlite_alias(&root, name, None).unwrap();
    }
}

#[cfg(all(test, target_os = "linux"))]
mod inspection_sql_controls {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    struct BoundedWork {
        steps: AtomicU64,
        expired: AtomicBool,
    }
    impl SqliteInspectionSqlWork for BoundedWork {
        fn begin_native_production(
            &self,
            root: &PinnedDirectory,
            _: &OsStr,
        ) -> Result<SqliteInspectionProductionPermit> {
            self.require_destination(root)?;
            self.checkpoint()?;
            Ok(mechanism_production(SqliteInspectionProductionKind::Native))
        }
        fn require_copy_work(&self, _: &Arc<dyn SqliteInspectionCopyWork>) -> Result<()> {
            anyhow::bail!("standalone SQL fixture lacks original-copy attestation")
        }
        fn require_destination(&self, root: &PinnedDirectory) -> Result<()> {
            root.ensure_path_binding()
        }
        fn checkpoint(&self) -> Result<()> {
            ensure!(
                !self.expired.load(Ordering::SeqCst),
                "original fixture expired"
            );
            Ok(())
        }
        fn debit_vm_steps(&self, steps: u64) -> Result<()> {
            self.checkpoint()?;
            self.steps
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |prior| {
                    prior.checked_add(steps).filter(|total| *total <= 128)
                })
                .map_err(|_| anyhow::anyhow!("original fixture VM work exhausted"))?;
            Ok(())
        }
    }
    fn fixture() -> Arc<BoundedWork> {
        Arc::new(BoundedWork {
            steps: AtomicU64::new(0),
            expired: AtomicBool::new(false),
        })
    }
    // Mechanical SQL controls: these bounded fixtures are not issued platform authority.
    /// In-memory stock connection on one armed ticket of `host`.
    fn armed_memory(
        host: &crate::test_host::TestNativeHost,
    ) -> (Connection, crate::native::RawTestTicket) {
        host.armed(|| Connection::open_in_memory().unwrap())
    }
    fn close_memory(
        host: &crate::test_host::TestNativeHost,
        connection: Connection,
        ticket: crate::native::RawTestTicket,
    ) {
        host.scope(|| connection.close().unwrap());
        host.retire_raw(ticket);
    }
    #[test]
    fn recursive_and_short_statements_share_irreversible_vm_budget() {
        let host = crate::test_host::TestNativeHost::acquire();
        let tmp = tempfile::tempdir().unwrap();
        let root = PinnedDirectory::open(tmp.path()).unwrap().unwrap();
        let work = fixture();
        let (first, first_ticket) = armed_memory(&host);
        host.scope(|| {
            install_inspection_sql_work(&first, &root, work.clone()).unwrap();
            let mut successes = 0;
            loop {
                match first.query_row("SELECT 1", [], |r| r.get::<_, i64>(0)) {
                    Ok(1) => successes += 1,
                    Ok(_) => panic!("wrong scalar"),
                    Err(_) => break,
                }
                assert!(successes < 128, "short statements cannot escape metering");
            }
            assert!(successes > 0);
        });
        assert_eq!(work.steps.load(Ordering::SeqCst), 128);
        close_memory(&host, first, first_ticket);
        let (next, next_ticket) = armed_memory(&host);
        host.scope(|| {
            assert!(install_inspection_sql_work(&next, &root, work.clone()).is_err());
        });
        assert_eq!(work.steps.load(Ordering::SeqCst), 128);
        let recursive_work = fixture();
        host.scope(|| {
            install_inspection_sql_work(&next, &root, recursive_work.clone()).unwrap();
            assert!(next.query_row("WITH RECURSIVE n(x) AS (VALUES(0) UNION ALL SELECT x+1 FROM n WHERE x<1000000) SELECT SUM(x) FROM n", [], |r| r.get::<_, i64>(0)).is_err());
        });
        assert_eq!(recursive_work.steps.load(Ordering::SeqCst), 128);
        close_memory(&host, next, next_ticket);
    }
    #[test]
    fn refused_first_opcode_executes_zero_vm_steps() {
        let host = crate::test_host::TestNativeHost::acquire();
        let (conn, ticket) = armed_memory(&host);
        host.scope(|| {
            // Prepare BEFORE installing refusal, to distinguish prospective opcode
            // admission from prepare-time or retrospective loop/end callbacks.
            let mut statement = conn.prepare("SELECT 1").unwrap();
            conn.progress_handler(1, Some(|| true));
            assert!(statement.query_row([], |r| r.get::<_, i64>(0)).is_err());
            assert_eq!(statement.get_status(rusqlite::StatementStatus::VmStep), 0);
        });
        close_memory(&host, conn, ticket);
    }

    #[test]
    fn original_expiry_interrupts_installed_connection_without_renewal() {
        let host = crate::test_host::TestNativeHost::acquire();
        let tmp = tempfile::tempdir().unwrap();
        let root = PinnedDirectory::open(tmp.path()).unwrap().unwrap();
        let work = fixture();
        let (conn, ticket) = armed_memory(&host);
        host.scope(|| install_inspection_sql_work(&conn, &root, work.clone()).unwrap());
        work.expired.store(true, Ordering::SeqCst);
        host.scope(|| {
            assert!(
                conn.query_row("SELECT 1", [], |r| r.get::<_, i64>(0))
                    .is_err()
            );
            assert!(install_inspection_sql_work(&conn, &root, work).is_err());
        });
        close_memory(&host, conn, ticket);
    }
}

/// The original factory's stopped-intake and service scopes: the stopped
/// SQLite intake runs only in the first, every refusal leaves the caller's
/// owners with the caller, and the cleanup reserve still settles the arena
/// after a held intake is retried to its bound.
#[cfg(all(test, target_os = "linux"))]
mod stopped_intake_scope_controls {
    use super::tests::{armed_stock, close_armed, seed_wal_on, stopped_plan_fixture};
    use super::*;
    use crate::offline_host::{OfflineNativeAuthority, OfflineNativeHost};
    use crate::test_host::{TestNativeEnvelope, TestNativeHost, issue_offline_host};
    use std::sync::atomic::{AtomicBool, Ordering};

    /// SQLite's BUSY code, as a refused nested scope entry reports it.
    const NESTED_SCOPE_BUSY: &str = "sqlite code 5";

    /// An operation window the test can close; the cleanup window stays open
    /// so the host can always finish.
    struct OperationWindow {
        open: AtomicBool,
    }
    impl OperationWindow {
        fn open() -> Arc<Self> {
            Arc::new(Self {
                open: AtomicBool::new(true),
            })
        }
    }
    impl OfflineNativeAuthority for OperationWindow {
        fn checkpoint(&self, cleanup: bool) -> Result<()> {
            ensure!(
                cleanup || self.open.load(Ordering::Acquire),
                "test operation window closed"
            );
            Ok(())
        }
        fn require_runtime(&self, _runtime: &PinnedDirectory) -> Result<()> {
            Ok(())
        }
        // No SQL quota of its own; the host's raw budget meters SQL.
        fn charge_sql_vm_steps(&self, _steps: u64) -> Result<()> {
            Ok(())
        }
    }

    fn absent_plan(
        runtime: &PinnedDirectory,
        root: &std::path::Path,
    ) -> StoppedSqliteProjectionPlan {
        stopped_plan_fixture(runtime, root, [StoppedSqliteRoleSource::Absent; 3])
    }

    /// Mutate and release an all-absent stopped owner, the way the scheduler
    /// retires one. The caller runs this inside an allocator scope.
    fn retire_absent(mut owner: DrainedSqliteProjection) {
        for role in SqliteRole::all() {
            owner.quarantine_role(role).unwrap();
            owner.purge_role(role).unwrap();
        }
        owner.release().unwrap();
    }

    fn transient(host: &OfflineNativeHost) -> OriginalNativeConnection {
        // SAFETY: test-only issuance under this host's own funded factory.
        let inputs = unsafe {
            OriginalNativeTransientInputs::issued(
                OriginalNativeDomain::StateTransient,
                host.destination().unwrap(),
            )
        }
        .unwrap();
        match host.factory().unwrap().open_transient(inputs) {
            Ok(connection) => connection,
            Err(failure) => panic!("test transient open refused: {failure}"),
        }
    }

    fn finish(host: OfflineNativeHost) {
        if let Err(retained) = host.finish() {
            panic!("offline test host finish retained: {:#}", retained.error);
        }
    }

    #[test]
    fn stopped_intake_scope_admits_the_stopped_create_an_unscoped_intake_refuses() {
        let host = TestNativeHost::acquire();
        let factory = host.factory().clone();
        let tmp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(PinnedDirectory::open(tmp.path()).unwrap().unwrap());
        let plan = absent_plan(&runtime, tmp.path());
        // Outside every native scope the stopped create refuses MISUSE
        // before it allocates or claims anything.
        let refused =
            DrainedSqliteProjection::capture_stopped_projection(Arc::clone(&runtime), plan.clone())
                .expect_err("stopped intake outside every native scope refuses");
        assert!(refused.to_string().contains("refused (21)"), "{refused}");
        assert!(matches!(
            refused.owner(),
            SqliteProjectionRetirementOwner::UnacquiredStopped { .. }
        ));
        refused.abort().unwrap();

        let before = factory.budget_snapshot().unwrap().cleanup_steps;
        let owner = factory
            .with_original_stopped_intake(|_| {
                DrainedSqliteProjection::capture_stopped_projection(
                    Arc::clone(&runtime),
                    plan.clone(),
                )
            })
            .unwrap()
            .unwrap();
        assert!(
            factory.budget_snapshot().unwrap().cleanup_steps > before,
            "the stopped intake allocates inside the cleanup scope"
        );
        let observed = factory
            .with_original_service_scope(|_| owner.observe_stopped_projection())
            .unwrap()
            .unwrap();
        assert!(observed.ready);
        assert_eq!(observed.plan, plan);
        factory
            .with_original_service_scope(|_| retire_absent(owner))
            .unwrap();
        assert!(!tmp.path().join("scheduler.sqlite3").exists());
        drop(factory);
    }

    #[test]
    fn stopped_intake_scope_refuses_inside_an_open_ticket_and_refuses_a_nested_scope() {
        let host = TestNativeHost::acquire();
        let factory = host.factory().clone();
        // A thread inside a ticketed open scope cannot open the cleanup scope.
        let mut inside_ticket = None;
        let (connection, ticket) = host.armed(|| {
            inside_ticket = Some(
                factory
                    .with_original_stopped_intake(|_| ())
                    .map_err(|failure| failure.to_string()),
            );
            rusqlite::Connection::open_in_memory().unwrap()
        });
        close_armed(&host, connection, ticket);
        let error = inside_ticket
            .expect("intake attempted")
            .expect_err("intake inside an open ticket refuses");
        assert!(error.contains(NESTED_SCOPE_BUSY), "{error}");

        // A service scope nested in the intake scope does not match it.
        let nested = factory
            .with_original_stopped_intake(|_| {
                factory
                    .with_original_service_scope(|_| ())
                    .map_err(|failure| failure.to_string())
            })
            .unwrap();
        let error = nested.expect_err("service scope nested in the intake scope refuses");
        assert!(error.contains(NESTED_SCOPE_BUSY), "{error}");
        // The held effect gate refuses a second intake nested in the first.
        let again = factory
            .with_original_stopped_intake(|_| {
                factory
                    .with_original_stopped_intake(|_| ())
                    .map_err(|failure| failure.to_string())
            })
            .unwrap();
        let error = again.expect_err("intake nested in an intake refuses");
        assert!(error.contains("effect in progress"), "{error}");
        // Both scopes left: each opens again on this thread.
        factory.with_original_service_scope(|_| ()).unwrap();
        factory.with_original_stopped_intake(|_| ()).unwrap();
        drop(factory);
    }

    #[test]
    fn stopped_intake_scope_refuses_a_finishing_or_shut_down_backend() {
        let host = TestNativeHost::acquire();
        let factory = host.factory().clone();
        factory.mark_lifecycle_for_test(true, false).unwrap();
        let error = factory.with_original_stopped_intake(|_| ()).unwrap_err();
        assert!(error.to_string().contains("draining"), "{error}");
        let error = factory.with_original_service_scope(|_| ()).unwrap_err();
        assert!(error.to_string().contains("draining"), "{error}");
        factory.mark_lifecycle_for_test(false, true).unwrap();
        let error = factory.with_original_stopped_intake(|_| ()).unwrap_err();
        assert!(error.to_string().contains("shutdown"), "{error}");
        factory.mark_lifecycle_for_test(false, false).unwrap();
        factory.with_original_stopped_intake(|_| ()).unwrap();
        drop(factory);
    }

    #[test]
    fn stopped_intake_scope_refuses_a_closed_window_and_closed_admission() {
        {
            let authority = OperationWindow::open();
            let (host, serial) =
                issue_offline_host(TestNativeEnvelope::default(), authority.clone());
            let factory = host.factory().unwrap().clone();
            factory.with_original_stopped_intake(|_| ()).unwrap();
            authority.open.store(false, Ordering::Release);
            let refused = factory.with_original_stopped_intake(|_| ()).unwrap_err();
            assert!(
                format!("{:#}", refused.error()).contains("test operation window closed"),
                "{refused}"
            );
            // The closed window latched service refusal: reopening it does
            // not readmit the intake or a service scope.
            authority.open.store(true, Ordering::Release);
            let refused = factory.with_original_stopped_intake(|_| ()).unwrap_err();
            assert!(refused.to_string().contains("service refused"), "{refused}");
            let refused = factory.with_original_service_scope(|_| ()).unwrap_err();
            assert!(refused.to_string().contains("service refused"), "{refused}");
            drop(factory);
            finish(host);
            drop(serial);
        }
        let (mut host, serial) =
            issue_offline_host(TestNativeEnvelope::default(), OperationWindow::open());
        let factory = host.factory().unwrap().clone();
        // A connection the census has not recorded holds the finish after it
        // closed admission and marked the factory finishing.
        let connection = transient(&host);
        assert!(
            host.finish_backend().is_err(),
            "an unrecorded ticket holds the finish"
        );
        let refused = factory.with_original_stopped_intake(|_| ()).unwrap_err();
        assert!(
            refused.to_string().contains("admission closed"),
            "{refused}"
        );
        let closed = connection
            .close()
            .unwrap_or_else(|failure| panic!("close in cleanup window refused: {failure}"));
        if let Err((_terminal, error)) =
            host.retain_terminal(OriginalNativeConnectionTerminal::Closed(closed))
        {
            panic!("terminal refused: {error:#}");
        }
        drop(host.finish_backend().unwrap());
        // The backend terminal exists and SQLite has shut down.
        let post = host.post_backend_factory().unwrap().clone();
        assert!(post.with_original_stopped_intake(|_| ()).is_err());
        assert!(post.with_original_service_scope(|_| ()).is_err());
        drop((post, factory));
        if let Err(retained) = host.release() {
            panic!("offline test host release retained: {:#}", retained.error);
        }
        drop(serial);
    }

    #[test]
    fn stopped_intake_late_refusal_hands_the_owner_back() {
        let authority = OperationWindow::open();
        let (host, serial) = issue_offline_host(TestNativeEnvelope::default(), authority.clone());
        let factory = host.factory().unwrap().clone();
        let tmp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(PinnedDirectory::open(tmp.path()).unwrap().unwrap());
        let plan = absent_plan(&runtime, tmp.path());
        let mut refused = factory
            .with_original_stopped_intake(|_| {
                let captured = DrainedSqliteProjection::capture_stopped_projection(
                    Arc::clone(&runtime),
                    plan.clone(),
                );
                authority.open.store(false, Ordering::Release);
                captured
            })
            .expect_err("the window closed during the intake refuses after it");
        assert!(
            format!("{:#}", refused.error()).contains("test operation window closed"),
            "{refused}"
        );
        let owner = refused
            .completed_owner()
            .expect("the late refusal carries the callback's owner")
            .unwrap();
        assert!(
            refused.completed_owner().is_none(),
            "the owner is handed back once"
        );
        // Service refusal is latched; the owner still settles through this
        // arena's own allocator scope.
        factory.raw_scope_for_test(|| retire_absent(owner)).unwrap();
        drop(factory);
        finish(host);
        drop(serial);
    }

    /// The stopped intake allocates inside the cleanup scope, so it spends
    /// part of the cleanup reserve. Only its stopped create allocates:
    /// retries of the held owner, here to the scheduler reset's bound of 240
    /// after the first attempt, charge the reserve nothing more. The whole
    /// intake stays within its named bound, which rides the layout quote's
    /// fixed margin; what remains settles the stopped owner and finishes and
    /// releases the arena.
    #[test]
    fn cleanup_reserve_settles_and_finishes_after_maximal_stopped_intake_retries() {
        const RETRIES: usize = 240;
        let envelope = TestNativeEnvelope::default();
        let host = TestNativeHost::acquire_with(envelope);
        let factory = host.factory().clone();
        let tmp = tempfile::tempdir().unwrap();
        seed_wal_on(&host, tmp.path());
        let runtime = Arc::new(PinnedDirectory::open(tmp.path()).unwrap().unwrap());
        let source = StoppedSqliteRoleSource::Quarantine {
            generation: [1; 16],
        };
        let mut skeleton = stopped_plan_fixture(
            &runtime,
            tmp.path(),
            [
                StoppedSqliteRoleSource::Canonical,
                StoppedSqliteRoleSource::Absent,
                StoppedSqliteRoleSource::Absent,
            ],
        );
        skeleton.roles[0].source = source;
        let selected = skeleton.source_name(SqliteRole::Main).unwrap();
        std::fs::rename(
            tmp.path().join("scheduler.sqlite3"),
            tmp.path().join(&selected),
        )
        .unwrap();
        let plan = stopped_plan_fixture(
            &runtime,
            tmp.path(),
            [
                source,
                StoppedSqliteRoleSource::Absent,
                StoppedSqliteRoleSource::Absent,
            ],
        );
        // A peer holding the selected inode keeps every intake attempt held.
        let peer_path = tmp.path().join("peer.sqlite3");
        std::fs::hard_link(tmp.path().join(&selected), &peer_path).unwrap();
        let (peer, peer_ticket) = armed_stock(&host, &peer_path);
        host.scope(|| peer.execute_batch("BEGIN; SELECT * FROM custody;").unwrap());

        let reserved = factory.budget_snapshot().unwrap().cleanup_reserved;
        let before = factory.budget_snapshot().unwrap().cleanup_steps;
        let mut failure = factory
            .with_original_stopped_intake(|_| {
                DrainedSqliteProjection::capture_stopped_projection(
                    Arc::clone(&runtime),
                    plan.clone(),
                )
            })
            .unwrap()
            .expect_err("the held peer refuses the intake");
        let after_first = factory.budget_snapshot().unwrap().cleanup_steps;
        for _ in 0..RETRIES {
            failure = factory
                .with_original_stopped_intake(|_| failure.retry_stopped_projection(&plan))
                .unwrap()
                .expect_err("the held peer refuses each retry");
        }
        let after_retries = factory.budget_snapshot().unwrap().cleanup_steps;
        let intake = after_retries - before;
        let first = after_first - before;

        host.scope(|| peer.execute_batch("ROLLBACK;").unwrap());
        close_armed(&host, peer, peer_ticket);
        let owner = factory
            .with_original_stopped_intake(|_| failure.retry_stopped_projection(&plan))
            .unwrap()
            .unwrap();
        factory
            .with_original_service_scope(|_| owner.abort())
            .unwrap()
            .unwrap();
        assert!(tmp.path().join(&selected).exists());
        drop(factory);

        let required = crate::offline_host::offline_native_required_cleanup_steps(
            envelope.storage_bytes,
            envelope.tickets,
        )
        .unwrap();
        let released = host.release();
        let facts = released.facts.expect("finished backend facts");
        let settled = facts.budget.cleanup_steps;
        let teardown = settled - after_retries;
        eprintln!(
            "cleanup reserve {reserved}, required quote {required}, first intake {first}, intake with {RETRIES} retries {intake}, \
             settlement and finish {teardown}, total {settled}"
        );
        assert_eq!(facts.arena, crate::OriginalNativeArenaDisposition::Released);
        assert!(
            first > 0,
            "the stopped create allocates inside the cleanup scope"
        );
        assert_eq!(
            intake, first,
            "retries of the held owner charge the cleanup reserve nothing"
        );
        assert!(
            intake <= crate::STOPPED_INTAKE_CLEANUP_STEPS,
            "intake charged {intake} cleanup steps, beyond its bound {}",
            crate::STOPPED_INTAKE_CLEANUP_STEPS
        );
        const _: () = assert!(
            crate::STOPPED_INTAKE_CLEANUP_STEPS <= crate::NATIVE_LAYOUT_CLEANUP_MARGIN_STEPS
        );
        assert_eq!(
            required, reserved,
            "the host reserves exactly the layout quote"
        );
    }
}
