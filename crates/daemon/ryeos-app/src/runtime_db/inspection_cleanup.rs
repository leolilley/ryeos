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
/// The physical registrations one inspection copy group can hold: its two
/// copy directories, two namespace owners, one SQL-work root, and the
/// descriptors retained by its finite scratch owner. A fixed per-group bound,
/// not a multiplier inferred from packet count.
pub const MAX_INSPECTION_COPY_GROUP_PHYSICAL_REGISTRATIONS: usize =
    MAX_INSPECTION_GROUP_ROOT_REGISTRATIONS
        + MAX_INSPECTION_SQL_ROOT_REGISTRATIONS
        + crate::temp_dir_guard::MAX_FINITE_SCRATCH_SHARED_ROOTS;

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
