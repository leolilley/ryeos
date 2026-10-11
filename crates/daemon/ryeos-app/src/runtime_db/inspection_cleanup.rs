//! Same-owner, finite copied-role namespace disposal. This does not close the
//! creation witnesses or settle/refund a claim. Native role retirement uses
//! the actual terminal owner plus removal receipt; no physical-byte claim.
//! Empty-root removal uses the original preserved guard and retained phases.

use super::{
    RuntimeInspectionRelease, SqliteInspectionAcquisitionReleased, SqliteInspectionClosed,
    SqliteInspectionCopyLease,
};
use anyhow::{Context, Result, ensure};
use lillux::secure_fs::PlannedQuarantineStage;
use lillux::time::{Duration, MonotonicDeadline};
use std::ffi::OsString;
use std::sync::{Arc, Mutex};

const MAX_INSPECTION_GROUP_DIRECTORY_REGISTRATIONS: usize = 2;
const MAX_INSPECTION_GROUP_NAMESPACE_REGISTRATIONS: usize = 2;
const MAX_INSPECTION_GROUP_ROOT_REGISTRATIONS: usize =
    MAX_INSPECTION_GROUP_DIRECTORY_REGISTRATIONS + MAX_INSPECTION_GROUP_NAMESPACE_REGISTRATIONS;
const MAX_INSPECTION_SQL_ROOT_REGISTRATIONS: usize = 1;
/// One live inspection group registers its two copy directories, two namespace
/// owners, one SQL-work root, and the four descriptors retained by its finite
/// scratch owner. These are the actual arrays/registration loops below, not a
/// multiplier inferred from packet count.
pub const MAX_INSPECTION_COPY_GROUP_PHYSICAL_REGISTRATIONS: usize =
    MAX_INSPECTION_GROUP_ROOT_REGISTRATIONS
        + MAX_INSPECTION_SQL_ROOT_REGISTRATIONS
        + crate::temp_dir_guard::MAX_FINITE_SCRATCH_SHARED_ROOTS;

pub fn quote_inspection_copy_group()
    -> anyhow::Result<lillux_sqlite::original_physical_custody::OutsideWork> {
    let bytes = std::mem::size_of::<OriginalInspectionCopyGroup>()
        .checked_add(std::mem::size_of::<OriginalInspectionCleanupParts>())
        .context("actual inspection copy group quote overflow")?;
    Ok(lillux_sqlite::original_physical_custody::OutsideWork {
        bytes: u64::try_from(bytes)?, steps: 24,
    })
}

pub fn quote_inspection_source_pin_retirements(count: usize)
    -> anyhow::Result<lillux_sqlite::original_physical_custody::OutsideWork> {
    anyhow::ensure!(count <= 4, "inspection source pin quote exceeds its four fixed roles");
    let per_pin = std::mem::size_of::<OriginalInspectionSourceRetirement>()
        .checked_add(std::mem::size_of::<OriginalInspectionSourceAuthority>())
        .and_then(|n| n.checked_add(std::mem::size_of::<lillux_sqlite::OriginalMetadataPinRetirementInputs>()))
        .and_then(|n| n.checked_add(8 * std::mem::size_of::<usize>()))
        .context("actual source retirement owner quote overflow")?;
    let bytes = count.checked_mul(per_pin).context("original source pin census quote overflow")?;
    let steps = u64::try_from(count)?.checked_mul(8)
        .and_then(|n| n.checked_add(8))
        .context("original source retirement work quote overflow")?;
    Ok(lillux_sqlite::original_physical_custody::OutsideWork {
        bytes: u64::try_from(bytes)?, steps,
    })
}

#[cfg(all(test, target_os = "linux"))]
thread_local! {
    static FAIL_AFTER_FIRST_ROLE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static FAIL_AFTER_ROOT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[derive(Clone)]
enum TerminalOwner {
    Closed(Arc<SqliteInspectionClosed>),
    Acquisition(Arc<SqliteInspectionAcquisitionReleased>),
    Runtime(Arc<RuntimeInspectionRelease>),
}
impl TerminalOwner {
    fn into_outside_wrapper(self)
        -> std::result::Result<super::original_inspection::OriginalInspectionWrapperParts, (Self, anyhow::Error)> {
        match self {
            Self::Closed(original) => super::original_inspection::consume_closed_wrapper(original)
                .map_err(|(original, error)| (Self::Closed(original), error)),
            Self::Acquisition(original) => super::original_inspection::consume_acquisition_wrapper(original)
                .map_err(|(original, error)| (Self::Acquisition(original), error)),
            Self::Runtime(original) => {
                let checked = match original.as_ref() {
                    RuntimeInspectionRelease::Closed(owner) => super::original_inspection::require_wrapper_transfer(owner.process, &owner.native),
                    RuntimeInspectionRelease::Acquisition(owner) => super::original_inspection::require_wrapper_transfer(owner.process, &owner.native),
                };
                if let Err(error) = checked { return Err((Self::Runtime(original), error)); }
                let original = match Arc::try_unwrap(original) {
                    Ok(original) => original,
                    Err(original) => return Err((Self::Runtime(original), anyhow::anyhow!("original Runtime inspection wrapper still has aliases"))),
                };
                match original {
                    RuntimeInspectionRelease::Closed(owner) => super::original_inspection::consume_closed_wrapper(owner)
                        // Runtime enum carries no additional owner/authority;
                        // on inner alias refusal retain its SAME actual child.
                        .map_err(|(owner, error)| (Self::Closed(owner), error)),
                    RuntimeInspectionRelease::Acquisition(owner) => Ok(super::original_inspection::acquisition_wrapper_parts(owner)),
                }
            }
        }
    }

    fn context(&self) -> (&Arc<SqliteInspectionCopyLease>, &std::ffi::OsStr) {
        match self {
            Self::Closed(owner) => (&owner.lease, &owner.database_name),
            Self::Acquisition(owner) => (&owner.lease, &owner.database_name),
            Self::Runtime(owner) => match owner.as_ref() {
                RuntimeInspectionRelease::Closed(owner) => (&owner.lease, &owner.database_name),
                RuntimeInspectionRelease::Acquisition(owner) => {
                    (&owner.lease, &owner.database_name)
                }
            },
        }
    }
    fn authorize_body(
        &self,
        roles: [Option<Arc<lillux_sqlite::RetainedSqliteDestinationRole>>; 4],
        deadline: MonotonicDeadline,
    ) -> Result<Arc<lillux_sqlite::SqliteInspectionBodyAuthority>> {
        macro_rules! bind {
            ($owner:expr) => {{
                let owner = $owner;
                let lease: lillux_sqlite::SqliteCopyLease = owner.lease.clone();
                owner.proof.authorize_body_disposal(
                    &lease,
                    &owner.sql,
                    owner.lease._copy_work.clone(),
                    roles,
                    deadline,
                )
            }};
        }
        match self {
            Self::Closed(owner) => bind!(owner),
            Self::Acquisition(owner) => bind!(owner),
            Self::Runtime(owner) => match owner.as_ref() {
                RuntimeInspectionRelease::Closed(owner) => bind!(owner),
                RuntimeInspectionRelease::Acquisition(owner) => bind!(owner),
            },
        }
    }
    fn authorize_disposal(
        &self,
        receipt: Arc<lillux::secure_fs::RemovedEmptyChild>,
        body: &Arc<lillux_sqlite::SqliteInspectionBodyAuthority>,
    ) -> Result<Arc<lillux_sqlite::SqliteInspectionDisposalAuthority>> {
        match self {
            Self::Closed(owner) => owner.proof.authorize_disposal(receipt, body),
            Self::Acquisition(owner) => owner.proof.authorize_disposal(receipt, body),
            Self::Runtime(owner) => match owner.as_ref() {
                RuntimeInspectionRelease::Closed(owner) => {
                    owner.proof.authorize_disposal(receipt, body)
                }
                RuntimeInspectionRelease::Acquisition(owner) => {
                    owner.proof.authorize_disposal(receipt, body)
                }
            },
        }
    }
    fn require_original(&self) -> Result<()> {
        match self {
            Self::Closed(owner) => owner.require_original(),
            Self::Acquisition(owner) => owner.require_original(),
            Self::Runtime(owner) => match owner.as_ref() {
                RuntimeInspectionRelease::Closed(owner) => owner.require_original(),
                RuntimeInspectionRelease::Acquisition(owner) => owner.require_original(),
            },
        }
    }
}
#[derive(Clone, Copy)]
enum RolePhase {
    Original,
    AtOriginal,
    AtQuarantine,
    UnlinkedPendingSync,
    Complete,
    ForeignConflict,
}
struct Role {
    name: OsString,
    quarantine: OsString,
    pin: Option<Arc<lillux_sqlite::RetainedSqliteDestinationRole>>,
}
struct Progress {
    roles: [RolePhase; 4],
    body_absence_synced: bool,
    root_removal_entered: bool,
    root_removed: bool,
    root_receipt: Option<Arc<lillux::secure_fs::RemovedEmptyChild>>,
    disposal: Option<Arc<lillux_sqlite::SqliteInspectionDisposalAuthority>>,
    retired: [Option<Arc<lillux_sqlite::RetiredSqliteDestinationRole>>; 4],
}

pub(crate) struct InspectionCopyCleanup {
    terminal: TerminalOwner,
    body: Arc<lillux_sqlite::SqliteInspectionBodyAuthority>,
    roles: [Role; 4],
    deadline: MonotonicDeadline,
    progress: Mutex<Progress>,
}
/// Retains the ACTUAL completed cleanup owner, not a serialized terminal or
/// an accounting result. Only a fully verified original cleanup can issue it.
/// The aggregate still needs the original producer seal and both-store plan.
enum CompletedRoleDisposition {
    AuthenticatedAbsent {
        role_index: usize,
        body: Arc<lillux_sqlite::SqliteInspectionBodyAuthority>,
    },
    Retired {
        original: Arc<lillux_sqlite::RetainedSqliteDestinationRole>,
        proof: Arc<lillux_sqlite::RetiredSqliteDestinationRole>,
    },
}
pub(crate) struct InspectionCopyDisposal {
    owner: Arc<InspectionCopyCleanup>,
    removed: Arc<lillux::secure_fs::RemovedEmptyChild>,
    _disposal: Arc<lillux_sqlite::SqliteInspectionDisposalAuthority>,
    roles: [CompletedRoleDisposition; 4],
}
impl std::fmt::Debug for InspectionCopyDisposal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InspectionCopyDisposal")
            .finish_non_exhaustive()
    }
}
impl InspectionCopyDisposal {
    pub(crate) fn into_outside_group(self: Arc<Self>,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        aggregate: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> Result<()>)
        -> std::result::Result<OriginalInspectionCopyGroup, (Arc<Self>, anyhow::Error)> {
        let process = std::process::id();
        let checked = lillux_sqlite::original_physical_custody::retain_callback_refusal(&|| {
            factory.require_original_process()?;
            aggregate.require_original(factory)?;
            let lease = self.owner.terminal.context().0;
            lease._copy_work.require_original_outside_master(factory, aggregate)?;
            for role in &self.roles {
                if let CompletedRoleDisposition::Retired { original, proof } = role {
                    proof.require_original(original)?;
                }
            }
            let quote = quote_inspection_copy_group()?;
            prepay(quote.bytes, quote.steps)?;
            factory.require_original_process()
        });
        if let Err(error) = checked { return Err((self, error)); }
        let original = match Arc::try_unwrap(self) {
            Ok(original) => original,
            Err(original) => return Err((original, anyhow::anyhow!("actual disposal still has aliases"))),
        };
        let Self { owner, removed, _disposal, roles } = original;
        Ok(OriginalInspectionCopyGroup {
            process, leaf: None, cleanup: std::mem::ManuallyDrop::new(Some(owner)),
            cleanup_parts: std::mem::ManuallyDrop::new(None),
            wrapper_parts: std::mem::ManuallyDrop::new(None),
            primitive_parts: std::mem::ManuallyDrop::new(None),
            copy_context: std::mem::ManuallyDrop::new(None),
            lease_parts: std::mem::ManuallyDrop::new(None),
            scratch: std::mem::ManuallyDrop::new(None),
            source_retirements: std::mem::ManuallyDrop::new(None),
            registrations: std::array::from_fn(|_| None),
            removed: std::mem::ManuallyDrop::new(Some(removed)),
            disposal: std::mem::ManuallyDrop::new(Some(_disposal)),
            disposal_parts: std::mem::ManuallyDrop::new(None),
            body_parts: std::mem::ManuallyDrop::new(None),
            role_parts: std::mem::ManuallyDrop::new(None),
            roles: std::mem::ManuallyDrop::new(Some(roles)),
        })
    }

    pub(crate) fn retains_cleanup_alias(&self, owner: &Arc<InspectionCopyCleanup>) -> bool {
        Arc::ptr_eq(&self.owner, owner)
    }
    pub(crate) fn retains_closed_alias(&self, owner: &Arc<SqliteInspectionClosed>) -> bool {
        match &self.owner.terminal {
            TerminalOwner::Closed(actual) => Arc::ptr_eq(actual, owner),
            TerminalOwner::Runtime(actual) => match actual.as_ref() {
                RuntimeInspectionRelease::Closed(actual) => Arc::ptr_eq(actual, owner),
                _ => false,
            },
            _ => false,
        }
    }
    pub(crate) fn retains_acquisition_alias(&self, owner: &Arc<SqliteInspectionAcquisitionReleased>) -> bool {
        matches!(&self.owner.terminal, TerminalOwner::Acquisition(actual) if Arc::ptr_eq(actual, owner))
    }
    pub(crate) fn retains_runtime_alias(&self, owner: &Arc<RuntimeInspectionRelease>) -> bool {
        matches!(&self.owner.terminal, TerminalOwner::Runtime(actual) if Arc::ptr_eq(actual, owner))
    }

    /// Identity-only comparison for idempotent retention of the same actual
    /// completed owner. It cannot equate unrelated slots by copied fields.
    pub(crate) fn same_original(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.owner, &other.owner)
    }

    /// Alias the retained ACTUAL removal receipt for the original witness
    /// inode join. This does not synthesize a receipt from pathname absence.
    pub(crate) fn removed_root(&self) -> Result<Arc<lillux::secure_fs::RemovedEmptyChild>> {
        Ok(self.removed.clone())
    }

    /// Select a candidate by immutable original coordinates only. This is not
    /// disposal authority: the selected proof must still pass require_original.
    /// Keeping selection separate preserves native refusal errors at the join.
    pub(crate) fn matches_original_slot(
        &self,
        work: &Arc<dyn lillux_sqlite::SqliteInspectionCopyWork>,
        name: &std::ffi::OsStr,
    ) -> bool {
        Arc::ptr_eq(work, &self.owner.terminal.context().0._copy_work)
            && self.removed.name() == name
    }

    /// Record the actual retired destination identities, not source-plan DTOs.
    /// Native proof validation provides facts only, never disposal authority.
    pub(crate) fn role_identities(&self) -> Result<[Option<(u64, u64)>; 4]> {
        let mut identities = [None; 4];
        for (index, role) in self.roles.iter().enumerate() {
            if let CompletedRoleDisposition::Retired { original, proof } = role {
                proof.require_original(original)?;
                identities[index] = Some(proof.original_identity()?);
            }
        }
        Ok(identities)
    }

    /// Join immutable native disposal facts to the aggregate's authenticated
    /// original slot. This does not revalidate a deliberately removed root as
    /// present, reopen a descriptor or authorize witness/ledger mutation.
    pub(crate) fn require_original(
        &self,
        work: &Arc<dyn lillux_sqlite::SqliteInspectionCopyWork>,
        parent: &lillux::PinnedDirectory,
        name: &std::ffi::OsStr,
        expected_present: [bool; 4],
    ) -> Result<()> {
        let lease = self.owner.terminal.context().0;
        ensure!(
            Arc::ptr_eq(work, &lease._copy_work),
            "disposal selects foreign original copy work"
        );
        let receipt = &self.removed;
        ensure!(
            parent.is_same_directory(receipt.parent())?
                && receipt.name() == name
                && lease.directory()?.is_same_directory(receipt.child())?,
            "disposal selects foreign original parent/root/name"
        );
        for (index, disposition) in self.roles.iter().enumerate() {
            match (expected_present[index], disposition) {
                (false, CompletedRoleDisposition::AuthenticatedAbsent { role_index, body }) => {
                    ensure!(
                        *role_index == index && Arc::ptr_eq(body, &self.owner.body),
                        "absence selects another original body or role"
                    );
                }
                (true, CompletedRoleDisposition::Retired { original, proof }) => {
                    ensure!(
                        self.owner.roles[index]
                            .pin
                            .as_ref()
                            .is_some_and(|pin| Arc::ptr_eq(pin, original)),
                        "retirement substitutes original role owner"
                    );
                    proof.require_original(original)?;
                }
                _ => anyhow::bail!(
                    "original role has no authenticated absent-or-retired disposition"
                ),
            }
        }
        Ok(())
    }
}

/// Canonical complete physical copy graph. Partial alias refusals retain the
/// actual cleanup, body, terminal context, receipts and roles without Drop.
pub(crate) struct OriginalInspectionCopyGroup {
    process: u32,
    leaf: Option<lillux_sqlite::OriginalNativeOutsideLeafEvidence>,
    cleanup: std::mem::ManuallyDrop<Option<Arc<InspectionCopyCleanup>>>,
    cleanup_parts: std::mem::ManuallyDrop<Option<OriginalInspectionCleanupParts>>,
    wrapper_parts: std::mem::ManuallyDrop<Option<super::original_inspection::OriginalInspectionWrapperParts>>,
    primitive_parts: std::mem::ManuallyDrop<Option<lillux_sqlite::OriginalOutsideSqliteInspectionParts>>,
    copy_context: std::mem::ManuallyDrop<Option<OriginalInspectionCopyContext>>,
    lease_parts: std::mem::ManuallyDrop<Option<super::original_inspection::OriginalInspectionCopyLeaseParts>>,
    scratch: std::mem::ManuallyDrop<Option<crate::temp_dir_guard::OriginalFiniteScratchCustody>>,
    source_retirements: std::mem::ManuallyDrop<Option<Vec<OriginalInspectionSourceRetirement>>>,
    registrations: [Option<lillux_sqlite::original_physical_custody::OriginalPhysicalRegistration>;
        MAX_INSPECTION_GROUP_ROOT_REGISTRATIONS],
    removed: std::mem::ManuallyDrop<Option<Arc<lillux::secure_fs::RemovedEmptyChild>>>,
    disposal: std::mem::ManuallyDrop<Option<Arc<lillux_sqlite::SqliteInspectionDisposalAuthority>>>,
    disposal_parts: std::mem::ManuallyDrop<Option<lillux_sqlite::OriginalOutsideSqliteDisposalParts>>,
    body_parts: std::mem::ManuallyDrop<Option<lillux_sqlite::OriginalOutsideSqliteBodyParts>>,
    role_parts: std::mem::ManuallyDrop<Option<[OriginalOutsideRoleDisposition; 4]>>,
    roles: std::mem::ManuallyDrop<Option<[CompletedRoleDisposition; 4]>>,
}
enum OriginalOutsideRoleDisposition {
    AuthenticatedAbsent { role_index: usize },
    Retired {
        original: std::mem::ManuallyDrop<Arc<lillux_sqlite::RetainedSqliteDestinationRole>>,
        proof: std::mem::ManuallyDrop<Arc<lillux_sqlite::RetiredSqliteDestinationRole>>,
    },
}
struct OriginalInspectionSourceRetirement {
    pin: Arc<lillux_sqlite::RetainedOriginalMetadataPin>,
    inputs: Option<Arc<lillux_sqlite::OriginalMetadataPinRetirementInputs>>,
    receipt: Option<Arc<lillux_sqlite::RetiredOriginalMetadataPin>>,
}
struct OriginalInspectionSourceAuthority {
    process: u32,
    factory: std::sync::Weak<lillux_sqlite::OriginalNativeFactory>,
    terminal: std::sync::Weak<lillux_sqlite::OriginalNativeTerminal>,
    work: std::sync::Weak<dyn lillux_sqlite::SqliteInspectionCopyWork>,
    leaf: lillux_sqlite::OriginalNativeOutsideLeafEvidence,
    pin: Arc<lillux_sqlite::RetainedOriginalMetadataPin>,
}
impl lillux_sqlite::OriginalMetadataPinRetirementAuthority for OriginalInspectionSourceAuthority {
    fn require_original(&self, factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        terminal: &Arc<lillux_sqlite::OriginalNativeTerminal>, pin: &Arc<lillux_sqlite::RetainedOriginalMetadataPin>) -> Result<()> {
        ensure!(self.process == std::process::id(), "foreign original inspection source retirement");
        let original_factory = self.factory.upgrade().context("actual original source factory absent")?;
        let original_terminal = self.terminal.upgrade().context("actual original source terminal absent")?;
        ensure!(Arc::ptr_eq(factory, &original_factory) && Arc::ptr_eq(terminal, &original_terminal)
            && Arc::ptr_eq(pin, &self.pin), "source retirement changes actual original inputs/pin");
        self.leaf.require_original(factory, terminal)?;
        self.work.upgrade().context("actual original source work absent")?
            .require_original_outside_master(factory, terminal)
    }
    fn debit_original_steps(&self, steps: u64) -> Result<()> {
        ensure!(self.process == std::process::id(), "foreign original inspection source debit");
        let factory = self.factory.upgrade().context("actual original source factory absent")?;
        let terminal = self.terminal.upgrade().context("actual original source terminal absent")?;
        self.work.upgrade().context("actual original source work absent")?
            .debit_original_outside_steps(&factory, &terminal, steps)
    }
}
struct OriginalInspectionCopyContext {
    process: u32,
    lease: std::mem::ManuallyDrop<Option<Arc<SqliteInspectionCopyLease>>>,
    sql: std::mem::ManuallyDrop<Arc<dyn lillux_sqlite::SqliteInspectionSqlWork>>,
    work: std::mem::ManuallyDrop<Arc<dyn lillux_sqlite::SqliteInspectionCopyWork>>,
    namespace: std::mem::ManuallyDrop<Option<lillux::PinnedDirectoryLock>>,
    database_name: OsString,
    body_root: std::mem::ManuallyDrop<Option<Arc<lillux::PinnedDirectory>>>,
    primitive_root: std::mem::ManuallyDrop<Option<Arc<lillux::PinnedDirectory>>>,
    removed: std::mem::ManuallyDrop<Option<Arc<lillux::secure_fs::RemovedEmptyChild>>>,
    roles: std::mem::ManuallyDrop<[Option<Arc<lillux_sqlite::RetainedSqliteDestinationRole>>; 4]>,
    deadline: MonotonicDeadline,
}
struct OriginalInspectionGroupAliasReceiver<'a>(&'a OriginalInspectionCopyGroup);
// SAFETY: borrowed actual domain group retains its complete original graph
// throughout callback, including the SAME real opaque context owners. No graph
// discovery, Any/downcast, decoded metadata or independent funding is used.
unsafe impl lillux_sqlite::OriginalOutsideInspectionAliasReceiver for OriginalInspectionGroupAliasReceiver<'_> {
    fn require_original_context(&self,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>, aggregate: &lillux_sqlite::OriginalNativeTerminal,
        leaf: &lillux_sqlite::OriginalNativeOutsideLeafEvidence,
        lease: &lillux_sqlite::SqliteCopyLease,
        sql: &Arc<dyn lillux_sqlite::SqliteInspectionSqlWork>,
        work: &Arc<dyn lillux_sqlite::SqliteInspectionCopyWork>) -> Result<()> {
        let group = self.0;
        ensure!(group.process == std::process::id(), "foreign canonical inspection alias receiver");
        leaf.require_original(factory, aggregate)?;
        let original = group.leaf.as_ref().context("actual original inspection leaf not joined")?;
        ensure!(original.same_original_leaf(leaf)?, "inspection receiver selects different actual leaf");
        let body = group.body_parts.as_ref().context("actual canonical inspection body absent")?;
        ensure!(body.process == group.process && Arc::ptr_eq(lease, &body.lease)
            && Arc::ptr_eq(sql, &body.sql) && Arc::ptr_eq(work, &body.copy_work),
            "inspection alias receiver changes actual original lease/SQL/work");
        body.copy_work.require_original_outside_master(factory, aggregate)
    }
}
struct OriginalInspectionCleanupParts {
    terminal: std::mem::ManuallyDrop<Option<TerminalOwner>>,
    body: std::mem::ManuallyDrop<Option<Arc<lillux_sqlite::SqliteInspectionBodyAuthority>>>,
    roles: std::mem::ManuallyDrop<[Role; 4]>,
    deadline: MonotonicDeadline,
    progress: std::mem::ManuallyDrop<Progress>,
}
impl OriginalInspectionCopyGroup {
    pub(crate) fn coalesce_native_packet_context(&mut self,
        packet: &mut lillux_sqlite::OriginalNativeOutsideConnectionParts,
        bound: Option<&mut Option<(lillux_sqlite::SqliteCopyLease, Arc<dyn lillux_sqlite::SqliteInspectionSqlWork>)>>,
    ) -> Result<bool> {
        ensure!(self.process == std::process::id(), "foreign canonical inspection context census");
        if self.copy_context.is_some() { return Ok(false); }
        let wrapper = self.wrapper_parts.as_ref().context("actual wrapper context not consumed")?;
        let matched = match wrapper.proof.as_ref() {
            Some(super::original_inspection::OriginalInspectionPrimitiveProof::Closed(original)) =>
                packet.inspection.as_ref().is_some_and(|alias| Arc::ptr_eq(original, alias)),
            Some(super::original_inspection::OriginalInspectionPrimitiveProof::Released(original)) =>
                packet.released_inspection.as_ref().is_some_and(|alias| Arc::ptr_eq(original, alias)),
            None => false,
        };
        if !matched { return Ok(false); }
        if let Some(original) = &self.leaf {
            ensure!(original.same_original_leaf(&packet.leaf_evidence)?,
                "inspection context aliases select another actual native leaf");
        }
        if let Some(bound) = bound {
            if let Some((lease, sql)) = bound.as_ref() {
                let body = self.body_parts.as_ref().context("actual canonical body not consumed")?;
                ensure!(Arc::ptr_eq(lease, &body.lease) && Arc::ptr_eq(sql, &body.sql),
                    "native packet inspection interfaces select another original copy group");
            }
            // Same actual canonical body and typed wrapper retain each owner.
            bound.take();
        }
        self.leaf = Some(packet.leaf_evidence.clone());
        match wrapper.proof.as_ref() {
            Some(super::original_inspection::OriginalInspectionPrimitiveProof::Closed(_)) => { packet.inspection.take(); }
            Some(super::original_inspection::OriginalInspectionPrimitiveProof::Released(_)) => { packet.released_inspection.take(); }
            None => unreachable!(),
        }
        Ok(true)
    }
    pub(crate) fn join_destination_context(&self,
        destination: &mut lillux_sqlite::OriginalDestinationOutsideCustody,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>, aggregate: &lillux_sqlite::OriginalNativeTerminal,
        leaf: &lillux_sqlite::OriginalNativeOutsideLeafEvidence) -> Result<bool> {
        ensure!(self.process == std::process::id(), "foreign inspection destination join");
        let Some(original) = &self.leaf else { return Ok(false); };
        if !original.same_original_leaf(leaf)? { return Ok(false); }
        let receiver = OriginalInspectionGroupAliasReceiver(self);
        ensure!(destination.join_original_inspection_aliases(factory, aggregate, &receiver)?,
            "actual inspection destination did not join its canonical group");
        Ok(true)
    }

    /// # Safety
    /// Actual original controller census and same paid shared registry. This
    /// registers only real sources; it never seals the registry or the region.
    pub(crate) unsafe fn register_original_shared_roots(&mut self,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>, aggregate: &lillux_sqlite::OriginalNativeTerminal,
        registry: &mut lillux_sqlite::original_physical_custody::OriginalPhysicalRegistry,
        prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()> {
        ensure!(self.process == std::process::id(), "foreign original inspection physical group");
        let leaf = self.leaf.as_ref().context("actual original physical inspection leaf absent")?;
        leaf.require_original(factory, aggregate)?;
        let context = self.copy_context.as_mut().context("actual original copy context absent")?;
        let lease = self.lease_parts.as_mut().context("actual original copy lease not consumed")?;
        let retirements = self.source_retirements.as_ref().context("original source retirement receipts absent")?;
        ensure!(retirements.len() == lease.source_pins.len(), "original source pin receipt inventory incomplete");
        for original in retirements {
            original.receipt.as_ref().context("actual original source pin close receipt absent")?
                .require_original(&original.pin)?;
        }
        let cached_checks = self.registrations.iter().filter(|binding| binding.is_some()).count();
        if cached_checks > 0 {
            // A partial retry revalidates each prior transfer before moving a
            // remaining inspection root or lock. Registration itself only
            // funds a new alias scan, so fund these cached checks separately.
            let quote = registry.membership_work_bound(u64::try_from(cached_checks)?, 0)?;
            let work = quote.steps.checked_add(8)
                .context("inspection membership validation work overflow")?;
            registry.prepay_transfer(prepay, quote.bytes, work)?;
        }
        // SQL work has its own actual root alias; transfer it, without a
        // service checkpoint, before the registry can demand unique FD owners.
        unsafe { context.sql.register_original_outside_roots(factory, aggregate, leaf, registry, prepay) }?;
        for index in 0..MAX_INSPECTION_GROUP_DIRECTORY_REGISTRATIONS {
            if let Some(binding) = &self.registrations[index] { registry.require_membership(binding)?; continue; }
            let source = if index == 0 { &mut context.body_root } else { &mut context.primitive_root };
            let original = source.take().context("actual original inspection directory absent")?;
            match unsafe { registry.register_directory(original, prepay) } {
                Ok(binding) => self.registrations[index] = Some(binding),
                Err((original, error)) => { **source = Some(original); return Err(error); }
            }
        }
        for index in MAX_INSPECTION_GROUP_DIRECTORY_REGISTRATIONS
            ..MAX_INSPECTION_GROUP_ROOT_REGISTRATIONS
        {
            if let Some(binding) = &self.registrations[index] { registry.require_membership(binding)?; continue; }
            let source = if index == 2 { &mut context.namespace } else { &mut lease.source_lock };
            let original = source.take().context("actual original inspection namespace owner absent")?;
            match unsafe { registry.register_namespace(original, prepay) } {
                Ok(binding) => self.registrations[index] = Some(binding),
                Err((original, error)) => { **source = Some(original); return Err(error); }
            }
        }
        let scratch = self.scratch.as_mut().context("actual original finite scratch custody absent")?;
        if let Some(alias) = self.removed.as_ref() { scratch.require_removal_alias(alias)?; }
        if let Some(alias) = context.removed.as_ref() { scratch.require_removal_alias(alias)?; }
        // Scratch retains the canonical SAME removal receipt before removing
        // exact completed redundant aliases, enabling its consuming transfer.
        self.removed.take(); context.removed.take();
        unsafe { scratch.register_shared_roots(registry, prepay) }
    }
    pub(crate) fn actual_physical_holders_retired(&self) -> Result<bool> {
        ensure!(self.process == std::process::id(), "foreign original inspection physical receipts");
        let Some(context) = self.copy_context.as_ref() else { return Ok(false); };
        let Some(lease) = self.lease_parts.as_ref() else { return Ok(false); };
        let Some(retirements) = self.source_retirements.as_ref() else { return Ok(false); };
        if retirements.len() != lease.source_pins.len() { return Ok(false); }
        for original in retirements {
            let Some(receipt) = &original.receipt else { return Ok(false); };
            receipt.require_original(&original.pin)?;
        }
        if context.lease.is_some() || context.body_root.is_some() || context.primitive_root.is_some()
            || context.namespace.is_some() || context.removed.is_some() || lease.source_lock.is_some()
            || lease.guard.is_some() || self.removed.is_some() { return Ok(false); }
        for binding in &self.registrations {
            let Some(binding) = binding else { return Ok(false); };
            if !binding.retired() { return Ok(false); }
        }
        if !context.sql.original_outside_roots_retired()? { return Ok(false); }
        let Some(scratch) = self.scratch.as_ref() else { return Ok(false); };
        if !scratch.descriptors_retired()? { return Ok(false); }
        // The SAME original account is still strongly retained through SQL/
        // copy work. Its StateLockLease and claim directory owners have not
        // entered an actual outside physical receiver yet. Do not relabel this
        // complete local descriptor prefix as the group's complete custody.
        Ok(false)
    }

    pub(crate) fn retire_original_source_pins(&mut self,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>, terminal: &Arc<lillux_sqlite::OriginalNativeTerminal>,
        prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()> {
        ensure!(self.process == std::process::id(), "foreign original source pin group");
        self.leaf.as_ref().context("actual original source leaf absent")?.require_original(factory, terminal)?;
        let lease = self.lease_parts.as_ref().context("actual original source pin owners not transferred")?;
        lease.work.require_original_outside_master(factory, terminal)?;
        if self.source_retirements.is_none() {
            let count = lease.source_pins.len();
            let quote = quote_inspection_source_pin_retirements(count)?;
            prepay(quote.bytes, quote.steps)?;
            *self.source_retirements = Some(Vec::with_capacity(count));
        }
        let retained = self.source_retirements.as_mut().expect("prepaid actual source retirement vector");
        for pin in lease.source_pins.iter().skip(retained.len()) {
            let authority: Arc<dyn lillux_sqlite::OriginalMetadataPinRetirementAuthority> = Arc::new(OriginalInspectionSourceAuthority {
                process: self.process, factory: Arc::downgrade(factory), terminal: Arc::downgrade(terminal),
                work: Arc::downgrade(&lease.work), leaf: self.leaf.as_ref().unwrap().clone(), pin: pin.clone(),
            });
            // SAFETY: actual original O_PATH pin transferred from this genuine
            // completed copy group, original master/clock/debit and exact leaf;
            // all concrete controls/vector/Arc storage prepaid before creation.
            let inputs = Arc::new(unsafe { lillux_sqlite::OriginalMetadataPinRetirementInputs::issued(
                factory.clone(), terminal.clone(), vec![pin.clone()], authority) }?);
            retained.push(OriginalInspectionSourceRetirement { pin: pin.clone(), inputs: Some(inputs), receipt: None });
        }
        ensure!(retained.len() == lease.source_pins.len(), "actual source pin inventory changed after original quote");
        for original in retained {
            if let Some(receipt) = &original.receipt {
                receipt.require_original(&original.pin)?;
                continue;
            }
            let inputs = original.inputs.as_ref().context("actual original source inputs absent without receipt")?;
            let receipt = Arc::new(original.pin.retire_after_backend(factory, terminal, inputs)?);
            original.receipt = Some(receipt);
            // Memo the real close receipt before ending only completed native
            // input references. Original source/account custody remains owned.
            original.inputs.take();
        }
        Ok(())
    }

    pub(crate) fn consume_copy_lease(&mut self,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>, aggregate: &lillux_sqlite::OriginalNativeTerminal,
        prepay: &dyn Fn(u64, u64) -> Result<()>) -> Result<()> {
        ensure!(self.process == std::process::id(), "foreign original copy lease group");
        if self.lease_parts.is_none() {
            let leaf = self.leaf.as_ref().context("actual original copy leaf absent")?;
            let context = self.copy_context.as_mut().context("actual original copy context absent")?;
            let original = context.lease.take().context("actual original copy lease absent")?;
            match super::original_inspection::consume_copy_lease(original, factory, aggregate,
                leaf, prepay) {
                Ok(parts) => *self.lease_parts = Some(parts),
                Err((original, error)) => {
                    *context.lease = Some(original);
                    return Err(error);
                }
            }
        }
        if self.scratch.is_none() {
            let lease = self.lease_parts.as_mut().expect("actual original copy lease parts");
            ensure!(lease.process == self.process, "source pin/guard owner changes original process");
            let receipt = self.removed.as_ref().context("actual canonical removal receipt absent")?;
            let guard = lease.guard.take().context("actual original scratch guard absent")?;
            // SAFETY: actual canonical removal receipt and original completed
            // preserved guard from SAME copy lease; same original prepayment.
            match unsafe { guard.into_outside_finite_removal(receipt, prepay) } {
                Ok(scratch) => *self.scratch = Some(scratch),
                Err((guard, error)) => {
                    *lease.guard = Some(guard);
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    pub(crate) fn consume_copy_context(&mut self) -> Result<()> {
        ensure!(self.process == std::process::id(), "foreign original copy context graph");
        if self.copy_context.is_some() { return Ok(()); }
        let wrapper = self.wrapper_parts.as_ref().context("actual terminal wrapper parts absent")?;
        let body = self.body_parts.as_ref().context("actual completed body parts absent")?;
        let primitive = self.primitive_parts.as_ref().context("actual primitive context parts absent")?;
        let lease: lillux_sqlite::SqliteCopyLease = Arc::<SqliteInspectionCopyLease>::clone(&wrapper.lease);
        ensure!(wrapper.process == self.process && body.process == self.process && primitive.process == self.process
            && wrapper.proof.is_none()
            && Arc::ptr_eq(&lease, &body.lease) && Arc::ptr_eq(&lease, &primitive.lease)
            && Arc::ptr_eq(&wrapper.sql, &body.sql) && Arc::ptr_eq(&wrapper.sql, &primitive.work)
            && Arc::ptr_eq(&wrapper.lease._copy_work, &body.copy_work)
            && wrapper.database_name == body.database_name && wrapper.database_name == primitive.database_name
            && Arc::ptr_eq(&body.root, &primitive.root),
            "copy context changes original primitive/body/lease/SQL/work owners");
        ensure!(!body.deadline.has_elapsed(), "original copy body deadline elapsed");
        drop(lease);
        let wrapper = self.wrapper_parts.take().expect("actual wrapper parts checked");
        let body = self.body_parts.take().expect("actual body parts checked");
        let primitive = self.primitive_parts.take().expect("actual primitive parts checked");
        let super::original_inspection::OriginalInspectionWrapperParts {
            process, proof: _, lease, sql, database_name, namespace } = wrapper;
        let lillux_sqlite::OriginalOutsideSqliteBodyParts { process: _, root, database_name: _,
            lease: body_lease, sql: body_sql, copy_work, roles, removed, deadline } = body;
        let lillux_sqlite::OriginalOutsideSqliteInspectionParts { process: _, root: primitive_root,
            database_name: _, lease: primitive_lease, work: primitive_sql } = primitive;
        // Canonical typed lease and SQL work have moved into this new context
        // before their exact redundant opaque aliases are surrendered.
        *self.copy_context = Some(OriginalInspectionCopyContext {
            process, lease: std::mem::ManuallyDrop::new(Some(std::mem::ManuallyDrop::into_inner(lease))),
            sql, work: copy_work, namespace, database_name, body_root: std::mem::ManuallyDrop::new(Some(std::mem::ManuallyDrop::into_inner(root))),
            primitive_root: std::mem::ManuallyDrop::new(Some(std::mem::ManuallyDrop::into_inner(primitive_root))), removed: std::mem::ManuallyDrop::new(Some(std::mem::ManuallyDrop::into_inner(removed))), roles, deadline,
        });
        drop(std::mem::ManuallyDrop::into_inner(body_lease));
        drop(std::mem::ManuallyDrop::into_inner(primitive_lease));
        drop(std::mem::ManuallyDrop::into_inner(body_sql));
        drop(std::mem::ManuallyDrop::into_inner(primitive_sql));
        Ok(())
    }

    pub(crate) fn consume_primitive_context(&mut self) -> Result<()> {
        ensure!(self.process == std::process::id(), "foreign original primitive inspection graph");
        if self.primitive_parts.is_some() || self.copy_context.is_some() { return Ok(()); }
        let wrapper = self.wrapper_parts.as_mut().context("actual wrapper parts absent")?;
        ensure!(wrapper.process == self.process, "primitive wrapper has different original process");
        let proof = wrapper.proof.take().context("actual original primitive proof absent")?;
        let consumed = match proof {
            super::original_inspection::OriginalInspectionPrimitiveProof::Closed(original) => {
                let original = std::mem::ManuallyDrop::into_inner(original);
                original.into_outside_parts().map_err(|(original, error)|
                    (super::original_inspection::OriginalInspectionPrimitiveProof::Closed(std::mem::ManuallyDrop::new(original)), error))
            }
            super::original_inspection::OriginalInspectionPrimitiveProof::Released(original) => {
                let original = std::mem::ManuallyDrop::into_inner(original);
                original.into_outside_parts().map_err(|(original, error)|
                    (super::original_inspection::OriginalInspectionPrimitiveProof::Released(std::mem::ManuallyDrop::new(original)), error))
            }
        };
        match consumed {
            Ok(parts) => *self.primitive_parts = Some(parts),
            Err((proof, error)) => {
                *wrapper.proof = Some(proof);
                return Err(error);
            }
        }
        Ok(())
    }

    pub(crate) fn consume_terminal_wrapper(&mut self) -> Result<()> {
        ensure!(self.process == std::process::id(), "foreign original inspection terminal group");
        if self.wrapper_parts.is_some() || self.copy_context.is_some() { return Ok(()); }
        let cleanup = self.cleanup_parts.as_mut().context("actual original cleanup parts absent")?;
        let original = cleanup.terminal.take().context("actual original terminal context absent")?;
        match original.into_outside_wrapper() {
            Ok(parts) => *self.wrapper_parts = Some(parts),
            Err((original, error)) => {
                *cleanup.terminal = Some(original);
                return Err(error);
            }
        }
        Ok(())
    }

    pub(crate) fn consume_completed_body(&mut self) -> Result<()> {
        ensure!(self.process == std::process::id(), "foreign original inspection body group");
        if self.body_parts.is_some() || self.copy_context.is_some() { return Ok(()); }
        let cleanup = self.cleanup_parts.as_mut().context("actual completed cleanup not consumed")?;
        ensure!(!cleanup.deadline.has_elapsed(), "original cleanup deadline elapsed");
        if self.disposal_parts.is_none() {
            let original = self.disposal.as_ref().context("actual original disposal authority absent")?;
            ensure!(cleanup.progress.disposal.as_ref().is_none_or(|alias| Arc::ptr_eq(alias, original)),
                "cleanup disposal alias changes actual canonical authority");
            ensure!(cleanup.progress.root_receipt.as_ref().is_none_or(|alias| self.removed.as_ref().is_some_and(|original| Arc::ptr_eq(alias, original))),
                "cleanup root receipt alias changes actual canonical receipt");
            // Canonical real authority/receipt remain retained before removal
            // of these exact completed aliases; no descriptor is released.
            cleanup.progress.disposal.take();
            cleanup.progress.root_receipt.take();
            let original = self.disposal.take().expect("actual canonical disposal checked");
            match original.into_outside_parts() {
                Ok(parts) => *self.disposal_parts = Some(parts),
                Err((original, error)) => {
                    *self.disposal = Some(original);
                    return Err(error);
                }
            }
        }
        let disposal = self.disposal_parts.as_ref().expect("actual canonical disposal parts");
        if let Some(body) = cleanup.body.as_ref() {
            disposal.require_body_alias(body)?;
        }
        if let Some(roles) = self.roles.as_ref() {
            for role in roles {
                if let CompletedRoleDisposition::AuthenticatedAbsent { body, .. } = role {
                    disposal.require_body_alias(body)?;
                }
            }
        }
        // The actual disposal parts own this body, including every physical
        // field, before any redundant absence/cleanup body alias is dropped.
        cleanup.body.take();
        if let Some(roles) = self.roles.take() {
            *self.role_parts = Some(roles.map(|role| match role {
                CompletedRoleDisposition::AuthenticatedAbsent { role_index, body } => {
                    drop(body);
                    OriginalOutsideRoleDisposition::AuthenticatedAbsent { role_index }
                }
                CompletedRoleDisposition::Retired { original, proof } => OriginalOutsideRoleDisposition::Retired {
                    original: std::mem::ManuallyDrop::new(original), proof: std::mem::ManuallyDrop::new(proof),
                },
            }));
        }
        let disposal = self.disposal_parts.take().expect("actual disposal parts retained");
        match disposal.into_body_parts() {
            Ok(body) => *self.body_parts = Some(body),
            Err((disposal, error)) => {
                *self.disposal_parts = Some(disposal);
                return Err(error);
            }
        }
        Ok(())
    }

    pub(crate) fn consume_completed_cleanup(&mut self) -> Result<()> {
        ensure!(self.process == std::process::id(), "foreign canonical inspection copy graph");
        if self.cleanup_parts.is_some() { return Ok(()); }
        let owner = self.cleanup.as_ref().context("actual original cleanup absent")?;
        {
            let progress = owner.progress.try_lock()
                .map_err(|_| anyhow::anyhow!("original completed cleanup busy or poisoned"))?;
            ensure!(progress.body_absence_synced && progress.root_removed
                && progress.roles.iter().all(|phase| matches!(phase, RolePhase::Complete))
                && progress.root_receipt.as_ref().is_some_and(|actual| self.removed.as_ref().is_some_and(|original| Arc::ptr_eq(actual, original)))
                && progress.disposal.as_ref().is_some_and(|actual| self.disposal.as_ref().is_some_and(|original| Arc::ptr_eq(actual, original))),
                "canonical cleanup has no same original completed receipts");
        }
        ensure!(!owner.deadline.has_elapsed(), "original copy cleanup deadline elapsed");
        let owner = self.cleanup.take().expect("actual cleanup checked");
        let owner = match Arc::try_unwrap(owner) {
            Ok(owner) => owner,
            Err(owner) => {
                *self.cleanup = Some(owner);
                anyhow::bail!("original completed cleanup still has aliases");
            }
        };
        let InspectionCopyCleanup { terminal, body, roles, deadline, progress } = owner;
        // All gates were validated before exclusive extraction. Preserve even
        // poison payload ownership; this does not relabel it as completion.
        let progress = progress.into_inner().unwrap_or_else(|poison| poison.into_inner());
        *self.cleanup_parts = Some(OriginalInspectionCleanupParts {
            terminal: std::mem::ManuallyDrop::new(Some(terminal)),
            body: std::mem::ManuallyDrop::new(Some(body)), roles: std::mem::ManuallyDrop::new(roles),
            deadline, progress: std::mem::ManuallyDrop::new(progress),
        });
        Ok(())
    }
}

impl std::fmt::Debug for InspectionCopyCleanup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InspectionCopyCleanup")
            .finish_non_exhaustive()
    }
}
struct CleanupFailure {
    error: anyhow::Error,
    _owner: Arc<InspectionCopyCleanup>,
}
impl std::fmt::Debug for CleanupFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CleanupFailure")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Display for CleanupFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}
impl std::error::Error for CleanupFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}
struct CleanupAcknowledgementFailure {
    error: anyhow::Error,
}
impl std::fmt::Debug for CleanupAcknowledgementFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("CleanupAcknowledgementFailure")
            .field(&self.error)
            .finish()
    }
}
impl std::fmt::Display for CleanupAcknowledgementFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}
impl std::error::Error for CleanupAcknowledgementFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}
fn lost_cleanup_acknowledgement(error: anyhow::Error) -> anyhow::Error {
    anyhow::Error::new(CleanupAcknowledgementFailure { error })
}

impl InspectionCopyCleanup {
    /// Retry only actual retained post-effect acknowledgement/sync stages.
    /// Foreign entries, identity disagreement, poison, expiry and admission
    /// refusal must return immediately without another attempt's debit.
    pub(crate) fn retryable_failure(error: &anyhow::Error) -> bool {
        error.chain().any(|cause| {
            cause.is::<CleanupAcknowledgementFailure>()
                || cause
                    .downcast_ref::<Arc<lillux::secure_fs::EmptyChildRemovalFailure>>()
                    .is_some_and(|failure| {
                        failure.stage()
                            == lillux::secure_fs::EmptyChildRemovalStage::RemovedPendingSync
                    })
                || cause
                    .downcast_ref::<lillux::secure_fs::EmptyChildRemovalFailure>()
                    .is_some_and(|failure| {
                        failure.stage()
                            == lillux::secure_fs::EmptyChildRemovalStage::RemovedPendingSync
                    })
                || cause
                    .downcast_ref::<lillux::secure_fs::PlannedQuarantineFailure>()
                    .is_some_and(|failure| {
                        matches!(
                            failure.stage,
                            PlannedQuarantineStage::QuarantinedOriginal
                                | PlannedQuarantineStage::UnlinkedOriginal
                        )
                    })
        })
    }
    pub(crate) fn for_closed(owner: Arc<SqliteInspectionClosed>) -> Result<Arc<Self>> {
        Self::issue(TerminalOwner::Closed(owner))
    }
    pub(crate) fn for_acquisition(
        owner: Arc<SqliteInspectionAcquisitionReleased>,
    ) -> Result<Arc<Self>> {
        Self::issue(TerminalOwner::Acquisition(owner))
    }
    pub(crate) fn for_runtime(owner: Arc<RuntimeInspectionRelease>) -> Result<Arc<Self>> {
        Self::issue(TerminalOwner::Runtime(owner))
    }
    fn issue(terminal: TerminalOwner) -> Result<Arc<Self>> {
        let (lease, database_name) = terminal.context();
        let mut issued = lease
            .cleanup
            .lock()
            .map_err(|_| anyhow::anyhow!("original cleanup issuance poisoned"))?;
        if let Some(original) = issued.as_ref() {
            return original
                .upgrade()
                .context("original cleanup owner was released; cannot reissue");
        }
        // Native proof validates BEFORE acquiring cleanup inputs. This is the
        // only first-issue path; retries use retained progress, not path proof.
        terminal.require_original()?;
        let deadline = MonotonicDeadline::after(lease._copy_work.cleanup_checkpoint()?);
        let root = lease.directory()?;
        let names = ["", "-wal", "-shm", "-journal"].map(|suffix| {
            let mut name = database_name.to_os_string();
            name.push(suffix);
            name
        });
        let mut roles = Vec::with_capacity(4);
        for name in names {
            lease._copy_work.debit_cleanup_steps(1)?;
            let pin = lease._copy_work.retained_destination_role(root, &name)?;
            let mut quarantine = OsString::from("cleanup.");
            quarantine.push(&name);
            roles.push(Role {
                name,
                quarantine,
                pin,
            });
        }
        let roles: [Role; 4] = roles
            .try_into()
            .map_err(|_| anyhow::anyhow!("cleanup fixed role count changed"))?;
        let body = terminal.authorize_body(
            std::array::from_fn(|index| roles[index].pin.clone()),
            deadline,
        )?;
        let owner = Arc::new(Self {
            terminal: terminal.clone(),
            body,
            roles,
            deadline,
            progress: Mutex::new(Progress {
                roles: [RolePhase::Original; 4],
                body_absence_synced: false,
                root_removal_entered: false,
                root_removed: false,
                root_receipt: None,
                disposal: None,
                retired: std::array::from_fn(|_| None),
            }),
        });
        *issued = Some(Arc::downgrade(&owner));
        Ok(owner)
    }
    /// Export actual retained facts for the original aggregate, never an
    /// empty collection's success or an inferred no-acquisition proof.
    pub(crate) fn completed_disposal(self: &Arc<Self>) -> Result<Arc<InspectionCopyDisposal>> {
        let lease = self.terminal.context().0;
        let (parent, name) = lease._guard.owned_scratch_parent()?;
        let (removed, disposal, retired) = {
            let progress = self.progress.try_lock().map_err(|_| {
                anyhow::anyhow!("original completed disposal custody busy or poisoned")
            })?;
            ensure!(
                progress.body_absence_synced
                    && progress.root_removed
                    && progress
                        .roles
                        .iter()
                        .all(|phase| matches!(phase, RolePhase::Complete)),
                "original copied body/root disposal is incomplete"
            );
            (
                progress
                    .root_receipt
                    .clone()
                    .context("actual removed-root receipt missing")?,
                progress
                    .disposal
                    .clone()
                    .context("actual native disposal authority missing")?,
                progress.retired.clone(),
            )
        };
        let mut roles = Vec::with_capacity(4);
        for (index, role) in self.roles.iter().enumerate() {
            // This same body authority authenticated all four admitted role
            // presences before any mutation; absence cannot be reconstructed
            // from a missing pin or current pathname here.
            roles.push(match (&role.pin, &retired[index]) {
                (None, None) => CompletedRoleDisposition::AuthenticatedAbsent {
                    role_index: index,
                    body: self.body.clone(),
                },
                (Some(original), Some(proof)) => {
                    proof.require_original(original)?;
                    CompletedRoleDisposition::Retired {
                        original: original.clone(),
                        proof: proof.clone(),
                    }
                }
                _ => anyhow::bail!("completed original role disposition missing"),
            });
        }
        let proof = Arc::new(InspectionCopyDisposal {
            owner: self.clone(),
            removed,
            _disposal: disposal,
            roles: roles
                .try_into()
                .map_err(|_| anyhow::anyhow!("original fixed role count changed"))?,
        });
        proof.require_original(
            &lease._copy_work,
            parent,
            name,
            std::array::from_fn(|index| self.roles[index].pin.is_some()),
        )?;
        Ok(proof)
    }

    fn require_window(&self) -> Result<()> {
        ensure!(
            !self.deadline.has_elapsed(),
            "original cleanup deadline expired; custody retained"
        );
        // Check the original admitted clock/custody as well as this owner's
        // nonrenewing deadline. Observation issues no new cleanup authority.
        self.terminal.context().0._copy_work.cleanup_checkpoint()?;
        ensure!(
            !self.deadline.has_elapsed(),
            "original cleanup deadline expired during custody observation"
        );
        Ok(())
    }
    fn step(&self) -> Result<()> {
        self.require_window()?;
        self.terminal
            .context()
            .0
            ._copy_work
            .debit_cleanup_steps(1)?;
        self.require_window()
    }
    /// Caller retains the actual inspection-parent lock via original CopyWork.
    /// The fixed role/quarantine inventory is exhaustive; foreign entries refuse.
    pub(crate) fn dispose_namespace(self: &Arc<Self>) -> Result<()> {
        self.dispose_namespace_inner().map_err(|error| {
            anyhow::Error::new(CleanupFailure {
                error,
                _owner: self.clone(),
            })
        })
    }
    fn dispose_namespace_inner(&self) -> Result<()> {
        let mut progress = loop {
            self.step()?;
            match self.progress.try_lock() {
                Ok(progress) => break progress,
                Err(std::sync::TryLockError::Poisoned(_)) => {
                    anyhow::bail!("original cleanup progress poisoned")
                }
                Err(std::sync::TryLockError::WouldBlock) => {
                    lillux::time::sleep(Duration::from_millis(1))
                }
            }
        };
        if progress.root_removal_entered {
            // The original guard may already have removed the root before a
            // failed acknowledgement. Never revalidate its disappeared path
            // or redo copied-body mutation to resume that guard's phase.
            return self.remove_root(&mut progress);
        }
        let root = self.terminal.context().0.directory()?;
        self.step()?;
        root.ensure_path_binding()?;
        self.step()?;
        let entries = root.entry_names_bounded(8)?;
        ensure!(
            entries.iter().all(|name| self
                .roles
                .iter()
                .any(|role| &role.name == name || &role.quarantine == name)),
            "unknown copied-root entry requires retained recovery"
        );
        for (index, role) in self.roles.iter().enumerate() {
            let phase = &mut progress.roles[index];
            ensure!(
                !matches!(phase, RolePhase::ForeignConflict),
                "cleanup retains a foreign namespace conflict"
            );
            self.step()?;
            let public = root.entry_no_follow(&role.name)?;
            self.step()?;
            let quarantine = root.entry_no_follow(&role.quarantine)?;
            if matches!(phase, RolePhase::Complete | RolePhase::UnlinkedPendingSync) {
                ensure!(
                    public.is_none() && quarantine.is_none(),
                    "removed role coordinate reappeared"
                );
                if matches!(phase, RolePhase::UnlinkedPendingSync) {
                    self.step()?;
                    root.sync().map_err(lost_cleanup_acknowledgement)?;
                    *phase = RolePhase::Complete;
                }
                continue;
            }
            let Some(pin) = &role.pin else {
                ensure!(
                    public.is_none() && quarantine.is_none(),
                    "originally absent copied role appeared"
                );
                *phase = RolePhase::Complete;
                continue;
            };
            let (source, target) = if matches!(phase, RolePhase::AtQuarantine) {
                ensure!(
                    public.is_none(),
                    "original role destination has a replacement"
                );
                (&role.quarantine, &role.name)
            } else {
                ensure!(
                    quarantine.is_none(),
                    "planned cleanup quarantine is occupied"
                );
                (&role.name, &role.quarantine)
            };
            self.step()?;
            match pin.remove_original_planned(&self.body, root, source, target) {
                Ok(()) => {
                    *phase = RolePhase::Complete;
                    #[cfg(all(test, target_os = "linux"))]
                    if index == 0 && FAIL_AFTER_FIRST_ROLE.with(|slot| slot.replace(false)) {
                        return Err(lost_cleanup_acknowledgement(anyhow::anyhow!(
                            "injected cleanup refusal after original first-role removal"
                        )));
                    }
                }
                Err(failure) => {
                    *phase = match failure.stage {
                        PlannedQuarantineStage::UnlinkedOriginal => RolePhase::UnlinkedPendingSync,
                        PlannedQuarantineStage::QuarantinedOriginal
                            if target == &role.quarantine =>
                        {
                            RolePhase::AtQuarantine
                        }
                        PlannedQuarantineStage::QuarantinedOriginal => RolePhase::AtOriginal,
                        PlannedQuarantineStage::Unchanged
                        | PlannedQuarantineStage::RestoredUnexpected => *phase,
                        PlannedQuarantineStage::PreservedUnexpected => RolePhase::ForeignConflict,
                    };
                    return Err(anyhow::Error::new(failure));
                }
            }
        }
        self.step()?;
        ensure!(
            root.entry_names_bounded(1)?.is_empty(),
            "copied-root body absence is incomplete"
        );
        self.step()?;
        root.sync().map_err(lost_cleanup_acknowledgement)?;
        // Retain the actual completed effect BEFORE the post-effect window
        // check. A sync crossing expiry must return error with this same owner;
        // it cannot report bounded completion or erase the known sync stage.
        progress.body_absence_synced = true;
        self.require_window()?;
        progress.root_removal_entered = true;
        self.remove_root(&mut progress)
    }
    fn retire_roles(&self, progress: &mut Progress) -> Result<()> {
        let authority = progress
            .disposal
            .as_ref()
            .context("actual root-removal receipt not joined to original native terminal owner")?;
        for (index, role) in self.roles.iter().enumerate() {
            let Some(pin) = role.pin.as_ref() else {
                continue;
            };
            if let Some(proof) = progress.retired[index].as_ref() {
                proof.require_original(pin)?;
                continue;
            }
            self.require_window()?;
            let available = self
                .terminal
                .context()
                .0
                ._copy_work
                .remaining_cleanup_steps()?;
            // Eight original output slots share this one aggregate. Request a
            // finite fraction rather than spending all remaining allowance on
            // the first store. Native precharge rechecks atomically; this read
            // is neither a new grant nor an accounting reservation.
            let bound = (available / 8)
                .checked_sub(1)
                .filter(|bound| *bound > 0)
                .context("original cleanup budget cannot fund bounded retirement scan")?;
            let bound = u32::try_from(bound.min(i32::MAX as u64))?;
            let proof = pin.retire_disposed_original(authority, bound)?;
            // Retain actual retirement proof before every late observation or
            // sibling failure; retries acknowledge, never close this FD again.
            progress.retired[index] = Some(Arc::new(proof));
            self.require_window()?;
        }
        self.require_window()
    }
    fn remove_root(&self, progress: &mut Progress) -> Result<()> {
        #[cfg(not(target_os = "linux"))]
        {
            let _ = progress;
            anyhow::bail!("original inspection empty-root removal requires Linux");
        }
        #[cfg(target_os = "linux")]
        {
            ensure!(
                progress.body_absence_synced,
                "original body absence must precede root removal"
            );
            self.step()?;
            let guard = &self.terminal.context().0._guard;
            if !progress.root_removed {
                guard.remove_empty_now_until(self.deadline)?;
                // Retain actual guard completion before every later observation.
                progress.root_removed = true;
                #[cfg(all(test, target_os = "linux"))]
                if FAIL_AFTER_ROOT.with(|slot| slot.replace(false)) {
                    return Err(lost_cleanup_acknowledgement(anyhow::anyhow!(
                        "injected acknowledgement failure after original root removal"
                    )));
                }
            }
            self.step()?;
            let receipt = guard.acknowledge_finite_removal_until(self.deadline)?;
            progress.root_receipt = Some(receipt.clone());
            self.require_window()?;
            if progress.disposal.is_none() {
                progress.disposal = Some(self.terminal.authorize_disposal(receipt, &self.body)?);
            }
            self.retire_roles(progress)
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod controls {
    use super::*;
    use crate::runtime_db::RuntimeDb;

    #[test]
    fn inspection_cleanup_quotes_cover_group_and_bounded_pin_retirements() {
        let group = quote_inspection_copy_group().unwrap();
        let group_bytes = std::mem::size_of::<OriginalInspectionCopyGroup>()
            + std::mem::size_of::<OriginalInspectionCleanupParts>();
        assert_eq!(group.bytes, group_bytes as u64);
        assert_eq!(group.steps, 24);

        let pins = quote_inspection_source_pin_retirements(4).unwrap();
        let per_pin = std::mem::size_of::<OriginalInspectionSourceRetirement>()
            + std::mem::size_of::<OriginalInspectionSourceAuthority>()
            + std::mem::size_of::<lillux_sqlite::OriginalMetadataPinRetirementInputs>()
            + 8 * std::mem::size_of::<usize>();
        assert_eq!(pins.bytes, u64::try_from(4 * per_pin).unwrap());
        assert_eq!(pins.steps, 40);
        assert!(quote_inspection_source_pin_retirements(5).is_err());
    }

    #[test]
    fn actual_closed_copy_body_disposal_retains_same_progress_and_charged_pins() {
        for inject in [0, 1, 2] {
            let tmp = tempfile::tempdir().unwrap();
            let source_path = tmp.path().join("runtime.sqlite3");
            drop(RuntimeDb::open(&source_path).unwrap());
            let source_bytes = std::fs::read(&source_path).unwrap();
            let source = lillux::PinnedDirectory::open(tmp.path()).unwrap().unwrap();
            let namespace = source.lock_exclusive().unwrap();
            let (_state_lock, work) = crate::execution_history_reset::fixture_inspection_copy_work(
                &source,
                &namespace,
                &source_path,
            );
            let db = RuntimeDb::open_owned_history_reset_inspection(
                &source_path,
                source.try_clone().unwrap(),
                namespace.clone(),
                work.clone(),
            )
            .unwrap();
            let (closed, reference) = db.close_owned_inspection().unwrap();
            assert!(reference.is_none(), "a stock inspection carries no native reference");
            let root = closed.lease.directory().unwrap();
            let main = work
                .retained_destination_role(root, std::ffi::OsStr::new("runtime.sqlite3"))
                .unwrap()
                .unwrap();
            let owner = InspectionCopyCleanup::for_closed(closed.clone()).unwrap();
            assert!(
                owner.completed_disposal().is_err(),
                "live namespace cannot issue completed facts"
            );
            if inject == 1 {
                FAIL_AFTER_FIRST_ROLE.with(|slot| slot.set(true));
                let error = owner.dispose_namespace().unwrap_err();
                assert!(format!("{error:#}").contains("after original first-role removal"));
                assert!(
                    root.entry_no_follow(std::ffi::OsStr::new("runtime.sqlite3"))
                        .unwrap()
                        .is_none()
                );
                let resumed = InspectionCopyCleanup::for_closed(closed.clone()).unwrap();
                assert!(
                    Arc::ptr_eq(&resumed, &owner),
                    "retry selects original progress, no new issuer"
                );
                resumed.dispose_namespace().unwrap();
                drop(error);
            } else if inject == 2 {
                FAIL_AFTER_ROOT.with(|slot| slot.set(true));
                crate::execution_history_reset::fixture_retry_inspection_cleanup(&owner).unwrap();
                assert!(!root.path().exists());
                assert!(owner.progress.lock().unwrap().root_removed);
                let resumed = InspectionCopyCleanup::for_closed(closed.clone()).unwrap();
                assert!(Arc::ptr_eq(&resumed, &owner));
            } else {
                let foreign = root.path().join("foreign");
                std::fs::write(&foreign, b"foreign obligation").unwrap();
                let refusal = owner.dispose_namespace().unwrap_err();
                assert!(!InspectionCopyCleanup::retryable_failure(&refusal));
                assert_eq!(std::fs::read(&foreign).unwrap(), b"foreign obligation");
                assert!(
                    root.entry_no_follow(std::ffi::OsStr::new("runtime.sqlite3"))
                        .unwrap()
                        .is_some()
                );
                // Remove only this control's injected foreign file, then retry
                // the same cleanup object and its irreversibly charged counter.
                std::fs::remove_file(foreign).unwrap();
                owner.dispose_namespace().unwrap();
            }
            assert!(root.entry_names_bounded(1).unwrap().is_empty());
            assert!(owner.progress.lock().unwrap().body_absence_synced);
            let disposal = owner.completed_disposal().unwrap();
            let retained_retirement = owner.progress.lock().unwrap().retired[0].take().unwrap();
            assert!(
                owner.completed_disposal().is_err(),
                "missing actual retirement cannot become authenticated absence"
            );
            owner.progress.lock().unwrap().retired[0] = Some(retained_retirement);
            let (parent, name) = closed.lease._guard.owned_scratch_parent().unwrap();
            let present = std::array::from_fn(|index| owner.roles[index].pin.is_some());
            disposal
                .require_original(&work, parent, name, present)
                .unwrap();
            let mut wrong_presence = present;
            wrong_presence[0] = false;
            assert!(
                disposal
                    .require_original(&work, parent, name, wrong_presence)
                    .is_err()
            );
            assert!(
                disposal
                    .require_original(&work, parent, std::ffi::OsStr::new("other-root"), present)
                    .is_err()
            );
            assert!(
                main.metadata_original().is_err(),
                "retired owner exposes no live descriptor"
            );
            owner.progress.lock().unwrap().retired[0]
                .as_ref()
                .unwrap()
                .require_original(&main)
                .unwrap();
            // This proves THIS original descriptor retired, not exclusion of
            // other-process references, global storage settlement or refund.
            assert_eq!(std::fs::read(&source_path).unwrap(), source_bytes);
            assert!(
                !root.path().exists(),
                "original empty-root namespace removal completed"
            );
            // A completed owner may acknowledge again, but it cannot erase a
            // new occupant of the old root coordinate under that acknowledgement.
            owner.dispose_namespace().unwrap();
            std::fs::create_dir(root.path()).unwrap();
            let replacement = root.path().join("foreign-after-root");
            std::fs::write(&replacement, b"replacement obligation").unwrap();
            assert!(owner.dispose_namespace().is_err());
            assert_eq!(
                std::fs::read(replacement).unwrap(),
                b"replacement obligation"
            );
        }
    }
}
