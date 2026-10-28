//! Explicit offline retirement of the local execution-history epoch.
//!
//! Normal maintenance GC deliberately preserves authoritative chain heads.
//! This module owns the separate operator-authorized path used when an entire
//! local thread-history epoch is disposable. It coordinates every store while
//! the daemon state lock, runtime namespace lock, and exclusive CAS mutation
//! guard are held.

use std::path::{Path, PathBuf};

/// Owner token for the secure temporaries the reset itself publishes: its
/// ledger, its inspection witnesses and its staged operation record. State,
/// scheduler and Runtime writes driven by a reset keep their owners' tokens.
pub(crate) const RESET_TEMP_PURPOSE: lillux::TempPurpose = lillux::TempPurpose::new("reset");
/// Cleanup steps charged for removing the acknowledged staged operation
/// file through the descriptor retained at staging: two entry lookups, one
/// guarded read, the quarantine rename, the unlink and the parent sync.
const STAGED_OPERATION_REMOVAL_STEPS: u64 = 8;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, ensure};
use serde::{Deserialize, Serialize};

mod creation_evidence;
#[cfg(test)]
mod fixture_operational_source;
mod inspection_destination;
mod inspection_plan;
mod native_backend;
#[cfg(test)]
mod native_composition_tests;
mod offline_policy;
mod operational_owner;
mod original_operation;
mod original_quota_claim;
mod scratch;
mod stale_recovery;
use offline_policy::OfflineResetPolicyOwner;
use operational_owner::OperationalOwnerPhase;
use original_operation::OriginalOperation;
use original_quota_claim::{OriginalInspectionSettlement, OriginalQuotaClaim, QuotaFailurePhase};
pub use scratch::{
    ExecutionHistoryScratchInspection, ExecutionHistoryScratchRepairOptions,
    ExecutionHistoryScratchRepairReport, ScratchEntry, ScratchLocation,
    execution_history_scratch_repair_command, inspect_execution_history_scratch,
    repair_execution_history_scratch, repair_execution_history_scratch_locked,
};

use crate::runtime_db::{
    InspectionCopyCleanup, InspectionCopyDisposal, RuntimeDb, RuntimeInspectionNativeFailure,
    RuntimeInspectionRelease, RuntimeThreadHistoryDiscardReport, SqliteInspectionClosed,
    SqliteInspectionCopyCustodyError, SqliteInspectionCopyLease,
};
use ryeos_node_runtime::config::{Config, ConfigSources};
use ryeos_node_runtime::state_lock::{StateLock, default_lock_path};

/// Only this module captures actual offline exclusion and publishes intent.
/// The token owns connection identity without borrowing the mutable RuntimeDb.
pub(crate) struct OfflineResetAuthority<'locks> {
    state_lock: &'locks StateLock,
    runtime_directory: &'locks lillux::PinnedDirectory,
    namespace_lock: &'locks lillux::PinnedDirectoryLock,
    incarnation: crate::runtime_db::RuntimeDbIncarnation,
    app_root: &'locks Path,
    db_target: &'locks Path,
    phase: OfflineResetAuthorityPhase,
}

#[derive(PartialEq, Eq)]
enum OfflineResetAuthorityPhase {
    Captured,
    IntentPublished,
    PostAckCustody,
}

impl<'locks> OfflineResetAuthority<'locks> {
    fn capture(
        state_lock: &'locks StateLock,
        runtime_directory: &'locks lillux::PinnedDirectory,
        namespace_lock: &'locks lillux::PinnedDirectoryLock,
        config: &'locks Config,
        db: &RuntimeDb,
    ) -> Result<Self> {
        state_lock.ensure_protects_app_root(&config.app_root)?;
        namespace_lock.ensure_protects(runtime_directory)?;
        let incarnation = db.capture_reset_incarnation(&config.db_path)?;
        // Capture only a mutating offline database; inspection obtains no issuer.
        db.require_reset_incarnation(&incarnation, &config.db_path)?;
        Ok(Self {
            state_lock,
            runtime_directory,
            namespace_lock,
            incarnation,
            app_root: &config.app_root,
            db_target: &config.db_path,
            phase: OfflineResetAuthorityPhase::Captured,
        })
    }

    fn require_held(&self, db: &RuntimeDb) -> Result<()> {
        self.state_lock.ensure_protects_app_root(self.app_root)?;
        self.namespace_lock
            .ensure_protects(self.runtime_directory)?;
        db.require_reset_incarnation(&self.incarnation, self.db_target)
    }

    fn require_closed_runtime(
        &self,
        closed: &crate::runtime_db::OriginalRuntimeClosed,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
    ) -> Result<()> {
        self.state_lock.ensure_protects_app_root(self.app_root)?;
        self.namespace_lock
            .ensure_protects(self.runtime_directory)?;
        ensure!(
            self.phase == OfflineResetAuthorityPhase::IntentPublished,
            "final reset acknowledgement requires original published intent"
        );
        closed.require_reset_incarnation(factory, &self.incarnation, self.db_target)
    }

    pub(crate) fn authorize_mutation(&self, db: &RuntimeDb) -> Result<()> {
        self.require_held(db)?;
        if self.phase != OfflineResetAuthorityPhase::IntentPublished {
            anyhow::bail!("offline runtime mutation requires actual published discard intent");
        }
        Ok(())
    }
}

/// What a scheduler inspection callback hands back: its accounting and the
/// terminal its native ticket reached. A terminal that only released the
/// acquisition never supplies accounting, whatever the callback reports.
/// Neither outcome admits physical cleanup or settles the reservation.
pub struct SqliteInspectionOutcome<T> {
    pub accounting: Result<T>,
    pub terminal: lillux_sqlite::OriginalNativeConnectionTerminal,
}

/// Borrowed inspection namespace. Only this controller issues the permit.
pub struct SchedulerResetPreview<'a> {
    runtime_directory: &'a lillux::PinnedDirectory,
    namespace_lock: &'a lillux::PinnedDirectoryLock,
    identity: Option<&'a ryeos_node_runtime::identity::NodeIdentity>,
    // The same producer lease retains source pins/copy through accounting and
    // original-operation issuance; copied data is evidence, not a grant.
    inspection_custody: std::cell::RefCell<Option<Arc<SqliteInspectionCopyLease>>>,
    // Keep native evidence outside the claim: SQL/copy work points back to it.
    inspection_closed: std::cell::RefCell<Option<Arc<SqliteInspectionClosed>>>,
    controller_custody: Option<&'a Arc<InspectionControllerCustody>>,
    original_operation: Option<&'a OriginalOperation>,
    quota_claim: Arc<OriginalQuotaClaim>,
    copy_work: Arc<dyn lillux_sqlite::SqliteInspectionCopyWork>,
}
impl SchedulerResetPreview<'_> {
    /// Inspect only an isolated copy of the fixed scheduler database. The
    /// original DB and sidecars are never opened by the supplied decoder.
    /// The callback receives the claim's own native factory and readonly
    /// inspection inputs for the copy; it opens the copy on one ticket of
    /// that factory and returns the terminal the ticket reached. The callback
    /// also receives the current scheduler schema definition, captured from a
    /// native reference on the same host before the copy exists.
    ///
    /// Every terminal joins the controller's census before any check runs on
    /// it: the closed or released copy with its proof, or a terminal that
    /// carries no copy proof on its own. On callback error, this error
    /// retains the callback's owner and the same copy lease, in drop order.
    pub fn inspect_database<T>(
        &self,
        inspect: impl FnOnce(
            &Arc<lillux_sqlite::OriginalNativeFactory>,
            lillux_sqlite::OriginalNativeBoundInputs,
            &ryeos_state::sqlite_schema::SchemaSqlDefinition,
        ) -> Result<SqliteInspectionOutcome<T>>,
    ) -> Result<Option<T>> {
        let runtime = self.runtime_directory().map_err(|error| {
            self.quota_claim
                .retain_failure(error, QuotaFailurePhase::Admission)
        })?;
        self.quota_claim
            .require_scheduler_inspection_source(runtime)
            .map_err(|error| {
                self.quota_claim
                    .retain_failure(error, QuotaFailurePhase::Admission)
            })?;
        let name = std::ffi::OsStr::new("scheduler.sqlite3");
        let absent = (|| -> Result<bool> {
            if runtime.entry_no_follow(name)?.is_some() {
                return Ok(false);
            }
            // Metadata probes can fail or observe a newly introduced orphan
            // after the plan check. Retain the original claim for every refusal.
            for sidecar in [
                "scheduler.sqlite3-wal",
                "scheduler.sqlite3-shm",
                "scheduler.sqlite3-journal",
            ] {
                ensure!(
                    runtime
                        .entry_no_follow(std::ffi::OsStr::new(sidecar))?
                        .is_none(),
                    "scheduler database is absent but retained sidecar {sidecar} requires recovery"
                );
            }
            Ok(true)
        })()
        .map_err(|error| {
            self.quota_claim
                .retain_failure(error, QuotaFailurePhase::Admission)
        })?;
        if absent {
            return Ok(None);
        }
        ensure!(
            self.inspection_custody.borrow().is_none(),
            "scheduler accounting was already inspected"
        );
        let controller = self.controller_custody.ok_or_else(|| {
            self.quota_claim.retain_failure(
                anyhow!("scheduler inspection has no controller census for its native terminals"),
                QuotaFailurePhase::Admission,
            )
        })?;
        let schema_definition = self.capture_scheduler_schema_definition(runtime)?;
        let (inspection_directory, lease) = crate::runtime_db::create_sqlite_inspection_copy(
            runtime,
            name,
            self.namespace_lock,
            self.copy_work.clone(),
        )
        .map_err(|error| {
            self.quota_claim
                .retain_failure(error, QuotaFailurePhase::Acquisition)
        })?;
        let inspection_directory = Arc::new(inspection_directory);
        // Claim the one inspection before invoking external decoder code. A
        // reentrant call cannot replace the original accounting provenance.
        *self.inspection_custody.borrow_mut() = Some(lease.clone());
        // Every refusal from here keeps the copy lease, and the copy root's
        // namespace and SQL work once they exist.
        let refuse = |error: anyhow::Error,
                      namespace: Option<&lillux::PinnedDirectoryLock>,
                      sql: Option<&Arc<dyn lillux_sqlite::SqliteInspectionSqlWork>>,
                      phase: QuotaFailurePhase| {
            let error = anyhow::Error::new(SchedulerCopyRefusal {
                error,
                _sql: sql.cloned(),
                _namespace: namespace.cloned(),
            });
            self.quota_claim.retain_failure(
                SqliteInspectionCopyCustodyError::retain(error, lease.clone()),
                phase,
            )
        };
        // The copy root's own namespace stays held through the native
        // terminal and the copy's later cleanup.
        let copy_namespace = self
            .copy_work
            .remaining_service_window()
            .and_then(|window| inspection_directory.lock_exclusive_with_timeout(window))
            .map_err(|error| refuse(error, None, None, QuotaFailurePhase::Admission))?;
        let sql_work = self
            .copy_work
            .clone()
            .issue_sql_work(&inspection_directory)
            .map_err(|error| {
                refuse(
                    error,
                    Some(&copy_namespace),
                    None,
                    QuotaFailurePhase::Admission,
                )
            })?;
        let (factory, inputs) = self
            .quota_claim
            .issue_scheduler_inspection_inputs(
                runtime,
                self.namespace_lock,
                original_quota_claim::SchedulerInspectionCopyOwners {
                    root: Arc::clone(&inspection_directory),
                    namespace: copy_namespace.clone(),
                    copy: Arc::clone(&lease),
                    sql: Arc::clone(&sql_work),
                    work: self.copy_work.clone(),
                },
            )
            .map_err(|error| {
                refuse(
                    error,
                    Some(&copy_namespace),
                    Some(&sql_work),
                    QuotaFailurePhase::Admission,
                )
            })?;
        let SqliteInspectionOutcome {
            accounting,
            terminal,
        } = match inspect(&factory, inputs, &schema_definition) {
            Ok(outcome) => outcome,
            Err(error) => {
                return Err(refuse(
                    error,
                    Some(&copy_namespace),
                    Some(&sql_work),
                    QuotaFailurePhase::Acquisition,
                ));
            }
        };
        let release = match crate::runtime_db::retain_copy_inspection_terminal(
            factory,
            terminal,
            Arc::clone(&lease),
            Arc::clone(&sql_work),
            copy_namespace.clone(),
            name.to_os_string(),
        ) {
            Ok(release) => release,
            Err((factory, terminal)) => {
                // A terminal without a copy proof still ends a ticket; it
                // joins the census and the refusal keeps the copy owners.
                controller.retain_unproven_scheduler_terminal(factory, terminal);
                let refusal = anyhow!("scheduler inspection terminal carries no copy proof");
                let refusal = match accounting {
                    Err(error) => lillux_sqlite::RefusalSupersedes::superseding(refusal, error),
                    Ok(_) => refusal,
                };
                return Err(refuse(
                    refusal,
                    Some(&copy_namespace),
                    Some(&sql_work),
                    QuotaFailurePhase::NativeClose,
                ));
            }
        };
        // The release joins the controller before any check: a refused join
        // cannot drop the one terminal proof the native owner produced.
        let release = controller.retain_scheduler_release(release);
        match release.as_ref() {
            RuntimeInspectionRelease::Closed(closed) => {
                *self.inspection_closed.borrow_mut() = Some(Arc::clone(closed));
                closed
                    .require_original()
                    .and(accounting)
                    .map(Some)
                    .map_err(|error| {
                        self.quota_claim.retain_failure(
                            closed.retain_failure(error),
                            QuotaFailurePhase::NativeClose,
                        )
                    })
            }
            RuntimeInspectionRelease::Acquisition(released) => {
                // An acquisition release never supplies accounting. The
                // refusal and its proof are kept even if the join fails.
                let error = match accounting {
                    Err(error) => error,
                    Ok(_) => {
                        anyhow!("scheduler inspection released its acquisition without a close")
                    }
                };
                let refusal = match released.require_original() {
                    Ok(()) => error,
                    Err(retry) => anyhow::Error::new(NativeInspectionRetryFailure {
                        original: error,
                        retry,
                    }),
                };
                Err(self.quota_claim.retain_failure(
                    anyhow::Error::new(SchedulerAcquisitionReleaseFailure {
                        error: refusal,
                        _released: Arc::clone(&release),
                    }),
                    QuotaFailurePhase::NativeClose,
                ))
            }
        }
    }

    /// Build and close the current scheduler schema reference on one ticket
    /// of the claim's own issued host, outside any SQL scope. The closed
    /// reference, or a refused one after its same-owner close, is handed to
    /// the controller's census before its definition is used; a reference
    /// whose close stays refused rides the returned error. Without controller
    /// custody no ticket is taken, since its terminal would have no census.
    fn capture_scheduler_schema_definition(
        &self,
        runtime: &lillux::PinnedDirectory,
    ) -> Result<ryeos_state::sqlite_schema::SchemaSqlDefinition> {
        let controller = self.controller_custody.ok_or_else(|| {
            self.quota_claim.retain_failure(
                anyhow!("scheduler inspection has no controller census for its schema reference"),
                QuotaFailurePhase::Admission,
            )
        })?;
        let (factory, inputs) = self
            .quota_claim
            .issue_scheduler_reference_inputs(runtime, self.namespace_lock)
            .map_err(|error| {
                self.quota_claim
                    .retain_failure(error, QuotaFailurePhase::Admission)
            })?;
        let reference = match ryeos_scheduler_contract::prepare_scheduler_schema_reference(
            Arc::clone(&factory),
            inputs,
        ) {
            Ok(reference) => reference,
            Err(refused) => {
                let original =
                    anyhow!("scheduler inspection schema reference refused: {refused:#}");
                let error = match refused.close_retained() {
                    Ok(released) => {
                        controller.retain_runtime_inspection_reference(
                            crate::runtime_db::RuntimeInspectionReferenceRelease::Released {
                                factory,
                                released,
                            },
                        );
                        original
                    }
                    Err(refused) => anyhow::Error::new(refused).context(original),
                };
                return Err(self
                    .quota_claim
                    .retain_failure(error, QuotaFailurePhase::Acquisition));
            }
        };
        let checked = reference.require_original_closed(
            &factory,
            lillux_sqlite::OriginalNativeDomain::NodeSchedulerSchemaReference,
        );
        let definition = reference.definition().clone();
        controller.retain_runtime_inspection_reference(
            crate::runtime_db::RuntimeInspectionReferenceRelease::Closed(reference),
        );
        checked.map_err(|error| {
            self.quota_claim
                .retain_failure(error, QuotaFailurePhase::Acquisition)
        })?;
        Ok(definition)
    }

    pub fn runtime_directory(&self) -> Result<&lillux::PinnedDirectory> {
        self.namespace_lock
            .ensure_protects(self.runtime_directory)?;
        Ok(self.runtime_directory)
    }

    fn checked_original_operation(&self) -> Result<Option<&OriginalOperation>> {
        let runtime = self.runtime_directory()?;
        if let Some(operation) = self.original_operation {
            operation.require_staged(runtime)?;
        }
        Ok(self.original_operation)
    }

    /// Read-only authenticated candidate facts, never a mutation permit.
    /// None means the fresh operation has not yet been prepared/staged.
    pub fn original_operation_reference(
        &self,
    ) -> Result<Option<&ryeos_state::ThreadHistoryDiscardOperationRef>> {
        Ok(self
            .checked_original_operation()?
            .map(OriginalOperation::reference))
    }

    pub fn original_operation_bytes(&self) -> Result<Option<&[u8]>> {
        Ok(self
            .checked_original_operation()?
            .map(OriginalOperation::signed_bytes))
    }

    pub fn original_scheduler_accounting(
        &self,
    ) -> Result<Option<&ryeos_scheduler_contract::SchedulerFireHistoryAccounting>> {
        Ok(self
            .checked_original_operation()?
            .map(OriginalOperation::accounting))
    }

    pub fn original_scheduler_sources(
        &self,
    ) -> Result<Option<&[ryeos_scheduler_contract::SchedulerRetirementStoppedRolePlan; 3]>> {
        Ok(self
            .checked_original_operation()?
            .map(OriginalOperation::scheduler_sources))
    }

    pub fn node_identity(&self) -> Option<&ryeos_node_runtime::identity::NodeIdentity> {
        self.identity
    }
}

/// Borrowed phase authority: cannot outlive actual offline locks, DB or CAS guard.
pub struct SchedulerResetRetirement<'a, 'locks> {
    authority: &'a OfflineResetAuthority<'locks>,
    runtime_db: &'a RuntimeDb,
    cas_guard: &'a ryeos_state::CasMutationGuard,
    state_authority: &'a ryeos_state::PinnedStateAuthority,
    identity: &'a ryeos_node_runtime::identity::NodeIdentity,
    operation: &'a OriginalOperation,
    /// The reset's native factory. Guarded reads made under this permit run
    /// in its scope, since the rebuilt projection's generation lease is
    /// listed while the scheduler retires.
    native_factory: Option<&'a Arc<lillux_sqlite::OriginalNativeFactory>>,
}
impl SchedulerResetRetirement<'_, '_> {
    pub fn runtime_directory(&self) -> Result<&lillux::PinnedDirectory> {
        self.authority.authorize_mutation(self.runtime_db)?;
        if !self.cas_guard.is_exclusive() {
            anyhow::bail!("scheduler history retirement requires exclusive CAS exclusion");
        }
        // The original State owner must select the same descriptor-bound
        // runtime generation. A namespace path or a detached CAS guard is
        // not a substitute for that owner through begin/advance/complete.
        ensure!(
            self.state_authority
                .runtime_directory()
                .is_same_directory(self.authority.runtime_directory)?,
            "scheduler retirement State authority selects a different runtime generation"
        );
        self.state_authority.ensure_guard(self.cas_guard)?;
        self.operation.require_active_in(
            self.native_factory.map(Arc::as_ref),
            self.state_authority,
            self.cas_guard,
        )?;
        Ok(self.authority.runtime_directory)
    }

    /// The native factory whose scope the participant's guarded reads use,
    /// once this permit's phase and exclusion are revalidated. Absent only
    /// for a permit issued without a native host.
    pub fn native_factory(&self) -> Result<Option<&Arc<lillux_sqlite::OriginalNativeFactory>>> {
        self.runtime_directory()?;
        Ok(self.native_factory)
    }

    /// The selected node identity is the only signer admitted for retirement
    /// records; publisher and operator keys are separate trust domains.
    pub fn node_identity(&self) -> &ryeos_node_runtime::identity::NodeIdentity {
        self.identity
    }

    /// Original signed evidence is available only under the actual activated
    /// State owner and this same offline retirement frame.
    pub fn operation_reference(&self) -> Result<&ryeos_state::ThreadHistoryDiscardOperationRef> {
        self.runtime_directory()?;
        Ok(self.operation.reference())
    }

    pub fn original_operation_bytes(&self) -> Result<&[u8]> {
        self.runtime_directory()?;
        Ok(self.operation.signed_bytes())
    }

    pub fn original_scheduler_accounting(
        &self,
    ) -> Result<&ryeos_scheduler_contract::SchedulerFireHistoryAccounting> {
        self.runtime_directory()?;
        Ok(self.operation.accounting())
    }

    pub fn original_scheduler_sources(
        &self,
    ) -> Result<&[ryeos_scheduler_contract::SchedulerRetirementStoppedRolePlan; 3]> {
        self.runtime_directory()?;
        Ok(self.operation.scheduler_sources())
    }

    pub fn initial_scheduler_generation(&self) -> Result<[u8; 16]> {
        self.runtime_directory()?;
        self.operation.initial_generation()
    }

    /// Canonical opaque serialization of the pinned runtime namespace identity.
    pub fn runtime_identity(&self) -> Result<String> {
        Ok(serde_json::to_string(
            &self.runtime_directory()?.identity()?,
        )?)
    }
}

/// Borrowed authority after State has acknowledged one exact reset
/// operation. The offline State lock, the runtime namespace lock and the
/// exclusive CAS guard are still held, and State records this operation as
/// the last acknowledged discard with none active. It authorizes a
/// participant to remove only its own evidence of completed work: nothing
/// a resumed operation could still need survives that acknowledgement.
pub struct SchedulerAcknowledgedReset<'a> {
    state_lock: &'a StateLock,
    app_root: &'a Path,
    runtime_directory: &'a lillux::PinnedDirectory,
    namespace_lock: &'a lillux::PinnedDirectoryLock,
    state_authority: &'a ryeos_state::PinnedStateAuthority,
    cas_guard: &'a ryeos_state::CasMutationGuard,
    identity: &'a ryeos_node_runtime::identity::NodeIdentity,
    operation: &'a OriginalOperation,
}
impl SchedulerAcknowledgedReset<'_> {
    /// The held runtime namespace, once every lock and the acknowledgement
    /// are revalidated.
    pub fn runtime_directory(&self) -> Result<&lillux::PinnedDirectory> {
        self.state_lock.ensure_protects_app_root(self.app_root)?;
        self.namespace_lock
            .ensure_protects(self.runtime_directory)?;
        ensure!(
            self.cas_guard.is_exclusive(),
            "acknowledged scheduler evidence retirement requires exclusive CAS exclusion"
        );
        self.state_authority.ensure_guard(self.cas_guard)?;
        ensure!(
            self.state_authority
                .runtime_directory()
                .is_same_directory(self.runtime_directory)?,
            "acknowledged scheduler evidence retirement selects a different runtime generation"
        );
        let recovery = self.state_authority.require_recovery()?;
        ensure!(
            recovery.thread_history_discard_operation()?.is_none()
                && recovery
                    .last_acknowledged_thread_history_discard()?
                    .as_ref()
                    == Some(self.operation.reference()),
            "scheduler evidence retirement requires State to have acknowledged this exact operation"
        );
        Ok(self.runtime_directory)
    }

    /// The selected node identity, the only signer of retirement records.
    pub fn node_identity(&self) -> &ryeos_node_runtime::identity::NodeIdentity {
        self.identity
    }
}

pub trait SchedulerHistoryResetParticipant: Send + Sync {
    /// Authenticate the retained namespace and reserve its complete bounded
    /// journal path without inspecting/copying the scheduler database. Runs
    /// on fresh and restarted operations before cross-store activation.
    fn preflight(&self, permit: &SchedulerResetPreview<'_>) -> Result<()>;
    fn preview(
        &self,
        permit: &SchedulerResetPreview<'_>,
    ) -> Result<ryeos_scheduler_contract::SchedulerFireHistoryAccounting>;
    fn begin_retirement<'a>(
        &self,
        permit: &'a SchedulerResetRetirement<'_, '_>,
    ) -> Result<Box<dyn SchedulerHistoryRetirementAttempt + 'a>>;

    /// Publish the terminal journal record only after every other reset owner
    /// has committed while the original State/CAS authority is still held.
    fn complete(
        &self,
        permit: &SchedulerResetRetirement<'_, '_>,
        retirement: &SchedulerHistoryRetirement,
    ) -> Result<usize>;

    /// After State has acknowledged the operation, remove the participant's
    /// own evidence of completed retirement work, so a completed reset
    /// leaves no scheduler state. Runs on every acknowledged reset, so work
    /// an earlier reset left unfinished settles here too. Returns how many
    /// entries were removed.
    fn retire_acknowledged_evidence(
        &self,
        permit: &SchedulerAcknowledgedReset<'_>,
    ) -> Result<usize>;
}

/// A same-thread phase cursor for scheduler retirement. The attempt is
/// intentionally not `Send`: it may own native descriptors and remains
/// coupled to the exact State/CAS permit held by the reset caller.
pub trait SchedulerHistoryRetirementAttempt {
    /// Advance one journal/effect boundary. On error, the caller retains this
    /// same attempt and retries it before releasing the original permit.
    fn advance(
        &mut self,
        permit: &SchedulerResetRetirement<'_, '_>,
    ) -> Result<Option<SchedulerHistoryRetirement>>;
}

pub struct SchedulerHistoryRetirement {
    pub journal_artifacts: usize,
    pub rows: ryeos_scheduler_contract::SchedulerFireHistoryDiscardReport,
    pub generation: Option<[u8; 16]>,
}

/// How long the reset retries one refused scheduler retirement stage,
/// counted from its last progress: about one minute, as the scheduler
/// service retries its own held stages (240 attempts at 250 ms). A refusal
/// that outlasts it is not a transient one. Tests shorten it so a refusal
/// is observable.
const SCHEDULER_SETTLEMENT_WINDOW: std::time::Duration =
    std::time::Duration::from_secs(if cfg!(test) { 2 } else { 60 });
/// Pause between retries of one refused scheduler retirement stage.
const SCHEDULER_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

/// End a scheduler retirement attempt in the reset factory's service scope,
/// so the native frees of the stopped intake it owns run in this arena, and
/// return that scope's refusal, if any. A refused scope ends the attempt by
/// a plain drop. That is safe: a native free made outside a scope of this
/// arena is not performed but kept as a deferred block, which the arena's
/// supervised reclaim settles when the backend finishes, so no memory is
/// freed outside the arena's accounting.
fn end_scheduler_attempt(
    factory: &lillux_sqlite::OriginalNativeFactory,
    attempt: Box<dyn SchedulerHistoryRetirementAttempt + '_>,
) -> Result<()> {
    let mut attempt = Some(attempt);
    let ended = factory.with_original_service_scope(|_| drop(attempt.take()));
    drop(attempt);
    ended.map_err(|failure| failure.into_parts().0)
}

/// `error` after ending the scheduler attempt it abandons, noting a refused
/// native scope that ended the attempt by a plain drop.
fn with_scheduler_attempt_ended(
    error: anyhow::Error,
    factory: &lillux_sqlite::OriginalNativeFactory,
    attempt: Box<dyn SchedulerHistoryRetirementAttempt + '_>,
) -> anyhow::Error {
    match end_scheduler_attempt(factory, attempt) {
        Ok(()) => error,
        Err(refused) => error.context(format!(
            "the scheduler retirement attempt ended by a plain drop after its native scope refused: {refused:#}"
        )),
    }
}

/// A scheduler participant refusal that no retry within the settlement
/// window can change, such as a foreign entry the operator must remove by
/// hand. The reset returns it at once instead of retrying; the discard
/// operation stays Active, the refused entry stays in place, and the next
/// reset resumes once the operator has acted. The message names the entry.
#[derive(Debug)]
pub struct SchedulerPermanentRefusal(pub String);
impl std::fmt::Display for SchedulerPermanentRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for SchedulerPermanentRefusal {}

/// Whether `error` carries a [`SchedulerPermanentRefusal`] anywhere in its
/// chain.
fn is_scheduler_permanent_refusal(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.downcast_ref::<SchedulerPermanentRefusal>().is_some())
}

/// The refusal of a scheduler stage still refused after its window. The
/// State discard marker stays Active and every durable record of the
/// attempt stays, so the next reset settles this attempt and resumes it.
fn scheduler_settlement_pending(error: anyhow::Error, stage: &str) -> anyhow::Error {
    error.context(format!(
        "execution-history reset could not {stage} within {}s; settlement pending; the next \
         `ryeos node reset execution-history` recovers it",
        SCHEDULER_SETTLEMENT_WINDOW.as_secs()
    ))
}

pub const EXECUTION_SCHEMA_CUTOVER_COMMAND: &str =
    "ryeos node reset execution-history --include-project-heads --confirm --confirm-project-heads";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionHistorySchemaCut {
    pub from: u32,
    pub to: u32,
}

#[derive(Debug, Clone, Default)]
pub struct ExecutionHistoryResetOptions {
    pub app_root: Option<PathBuf>,
    pub dry_run: bool,
    /// Explicit immutable project-schema cutover for principal and deployed
    /// project HEADs. This is accepted only with the all-thread-history discard
    /// because live history may reference either namespace.
    pub discard_project_heads: bool,
    /// When present, reset only the exact named predecessor epoch. A repeated
    /// invocation against the named current epoch is a non-destructive no-op;
    /// every other epoch pairing is refused.
    pub schema_cut: Option<ExecutionHistorySchemaCut>,
    /// The operator attests that the host has rebooted since the Runtime
    /// store's scope lifetime fence was written. A fence this binary cannot
    /// read as a current witness is then discarded (after checking its boot
    /// id differs from the current boot, where that id is still readable)
    /// instead of refusing the reset.
    pub discard_unreadable_lifetime_fence_after_reboot: bool,
}

impl ExecutionHistoryResetOptions {
    fn unreadable_lifetime_fence(&self) -> crate::runtime_db::UnreadableLifetimeFence {
        if self.discard_unreadable_lifetime_fence_after_reboot {
            crate::runtime_db::UnreadableLifetimeFence::DiscardAfterAttestedReboot
        } else {
            crate::runtime_db::UnreadableLifetimeFence::Refuse
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionHistoryResetPhase {
    CapturingAuthority,
    InspectingHistory,
    PublishingIntent,
    RetiringChainHeads,
    RetiringProjectHeads,
    RebuildingProjection,
    ClearingRuntime,
    ClearingScheduler,
    Finalizing,
    Complete,
}

impl ExecutionHistoryResetPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CapturingAuthority => "capturing_authority",
            Self::InspectingHistory => "inspecting_history",
            Self::PublishingIntent => "publishing_intent",
            Self::RetiringChainHeads => "retiring_chain_heads",
            Self::RetiringProjectHeads => "retiring_project_heads",
            Self::RebuildingProjection => "rebuilding_projection",
            Self::ClearingRuntime => "clearing_runtime",
            Self::ClearingScheduler => "clearing_scheduler",
            Self::Finalizing => "finalizing",
            Self::Complete => "complete",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExecutionHistoryResetProgress {
    pub phase: ExecutionHistoryResetPhase,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<usize>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ProjectionDiscardReport {
    pub chains_rebuilt: usize,
    pub threads_restored: usize,
    pub events_projected: usize,
    pub superseded_instances_deleted: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RuntimeThreadHistoryAccounting {
    Exact {
        counts: RuntimeThreadHistoryDiscardReport,
    },
    UnavailableIncompatibleSchema,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionHistorySchemaCutStatus {
    Required,
    Completed,
    AlreadyCurrent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExecutionHistorySchemaCutReport {
    pub from: u32,
    pub to: u32,
    pub observed: u32,
    pub status: ExecutionHistorySchemaCutStatus,
}

impl RuntimeThreadHistoryAccounting {
    pub fn total_rows(&self) -> Option<usize> {
        match self {
            Self::Exact { counts } => counts.total_rows(),
            Self::UnavailableIncompatibleSchema => None,
        }
    }
}

fn completed_runtime_accounting(
    schema_reset_required: bool,
    counts: RuntimeThreadHistoryDiscardReport,
) -> RuntimeThreadHistoryAccounting {
    if schema_reset_required {
        RuntimeThreadHistoryAccounting::UnavailableIncompatibleSchema
    } else {
        RuntimeThreadHistoryAccounting::Exact { counts }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ExecutionHistoryResetReport {
    pub app_root: PathBuf,
    pub dry_run: bool,
    pub performed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema_cut: Option<ExecutionHistorySchemaCutReport>,
    pub chain_heads: usize,
    pub project_heads: usize,
    pub chain_ref_artifacts: usize,
    pub pending_transitions: usize,
    pub retirement_fences: usize,
    pub evidence_obligations: usize,
    pub runtime_rows: RuntimeThreadHistoryAccounting,
    pub thread_runtime_artifacts: usize,
    pub scheduler_journal_artifacts: usize,
    pub scheduler_rows: ryeos_scheduler_contract::SchedulerFireHistoryAccounting,
    pub replay_indexes: ryeos_state::operational::ReplayIndexResetReport,
    pub projection: ProjectionDiscardReport,
    /// SHA-256 of the signed record that released this reset's reservation.
    /// The record itself is unlinked once the release completes.
    pub released_sha256: String,
    /// Stale attempts of earlier processes this reset settled at admission.
    pub recovered: Vec<RecoveredReservation>,
    /// How the native host's release ended its census durability pins.
    pub durability_pins: DurabilityPinsReport,
    /// The unreadable scope lifetime fence this reset discarded (or, in a
    /// preview, would discard) under the operator's attested reboot.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lifetime_fence_discard: Option<crate::runtime_db::LifetimeFenceDiscard>,
}

/// Census durability pins the native host's release ended.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct DurabilityPinsReport {
    pub closed: usize,
    /// Closes that consumed the descriptor but reported an error; nothing
    /// is left open, and the count is reported rather than refused.
    pub consumed_with_error: usize,
}

/// How a stale attempt's release ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveredReleaseOutcome {
    /// The attempt had committed its own completed release; recovery
    /// finished unlinking it.
    Completed,
    /// Recovery recorded the attempt as abandoned and removed its deposits.
    Abandoned,
}

/// Whether an abandoned attempt's history change was applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveredApplied {
    /// State acknowledged the attempt's discard: the history reset applied.
    Applied,
    NotApplied,
    /// State still marks the attempt's operation active; this reset resumes
    /// it.
    Resumed,
}

/// One stale attempt settled by recovery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecoveredReservation {
    pub operation_id: String,
    pub outcome: RecoveredReleaseOutcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub applied: Option<RecoveredApplied>,
    pub deposits_removed: usize,
    pub records_unlinked: usize,
}

impl ExecutionHistoryResetReport {
    pub fn total_discarded_rows(&self) -> Option<usize> {
        self.runtime_rows
            .total_rows()?
            .checked_add(self.scheduler_rows.total_rows()?)
    }
}

/// Inspect or execute the explicit all-execution-history reset transaction,
/// taking the node's state lock for the call. A dry run takes the existing
/// lock read-only; a mutation takes it exclusively.
pub fn run_execution_history_reset(
    options: &ExecutionHistoryResetOptions,
    participant: Option<&dyn SchedulerHistoryResetParticipant>,
) -> Result<ExecutionHistoryResetReport> {
    let state_lock = acquire_reset_state_lock(options)?;
    run_execution_history_reset_locked(options, participant, &state_lock)
}

pub fn run_execution_history_reset_with_progress(
    options: &ExecutionHistoryResetOptions,
    participant: Option<&dyn SchedulerHistoryResetParticipant>,
    observer: &mut dyn FnMut(&ExecutionHistoryResetProgress),
) -> Result<ExecutionHistoryResetReport> {
    let state_lock = acquire_reset_state_lock(options)?;
    run_execution_history_reset_with_progress_locked(options, participant, observer, &state_lock)
}

/// The reset transaction under a state lock the caller already holds. The
/// lock must protect the app root the options name; a dry run needs only a
/// read-only hold, a mutation the exclusive one.
pub fn run_execution_history_reset_locked(
    options: &ExecutionHistoryResetOptions,
    participant: Option<&dyn SchedulerHistoryResetParticipant>,
    state_lock: &StateLock,
) -> Result<ExecutionHistoryResetReport> {
    run_execution_history_reset_inner(options, participant, None, state_lock)
}

pub fn run_execution_history_reset_with_progress_locked(
    options: &ExecutionHistoryResetOptions,
    participant: Option<&dyn SchedulerHistoryResetParticipant>,
    observer: &mut dyn FnMut(&ExecutionHistoryResetProgress),
    state_lock: &StateLock,
) -> Result<ExecutionHistoryResetReport> {
    run_execution_history_reset_inner(options, participant, Some(observer), state_lock)
}

/// The outer ownership proof for a stand-alone reset: a running daemon and
/// every cooperating standalone state service hold the same lock.
fn acquire_reset_state_lock(options: &ExecutionHistoryResetOptions) -> Result<StateLock> {
    let config = Config::load(&ConfigSources {
        app_root: options.app_root.clone(),
        ..ConfigSources::default()
    })
    .context("load local node configuration for execution-history reset")?;
    let state_lock_path = default_lock_path(&config.app_root);
    if options.dry_run {
        StateLock::acquire_existing_read_only(&state_lock_path)
    } else {
        StateLock::acquire(&state_lock_path)
    }
    .context("execution-history reset requires the daemon to be stopped")
}

/// A closed scheduler copy inspection made the way a preview makes it: the
/// claim's own native host, a readonly native open and close of the copy of
/// `scheduler.sqlite3` (seeded by `sql`) beneath `root`, and the closed proof
/// its controller retained, with the pinned and locked `root`. The last
/// element holds the claim, its host and the host's serial guard; the host
/// ends only with the process, so only an isolated child may call this.
#[cfg(all(test, target_os = "linux"))]
pub(crate) fn fixture_closed_scheduler_inspection(
    root: &Path,
    sql: &str,
) -> (
    lillux::PinnedDirectory,
    lillux::PinnedDirectoryLock,
    StateLock,
    Arc<dyn lillux_sqlite::SqliteInspectionCopyWork>,
    Arc<SqliteInspectionClosed>,
    Box<dyn std::any::Any>,
) {
    tests::closed_scheduler_inspection(root, sql)
}

#[cfg(all(test, target_os = "linux"))]
pub(crate) fn fixture_retry_inspection_cleanup(owner: &Arc<InspectionCopyCleanup>) -> Result<()> {
    let controller = InspectionControllerCustody::default();
    controller.cleanup.lock().unwrap().push(owner.clone());
    controller.dispose_cleanup_owner(owner)?;
    let failures = controller.cleanup_failures.lock().unwrap();
    ensure!(
        failures.len() == 1,
        "real controller must retain original lost acknowledgement"
    );
    ensure!(
        format!("{:#}", failures[0]).contains("after original root removal"),
        "controller lost original native cleanup failure"
    );
    Ok(())
}

// One-use service-owner slot. It is not a native terminal or a no-start proof.
// Bookkeeping locks are held only during ownership transfer, never during SQL.
#[derive(Default)]
struct OfflineStateProjectionSlot {
    issued: bool,
    factory: Option<Arc<lillux_sqlite::OriginalNativeFactory>>,
    retained: Option<ryeos_state::StateDb>,
    closed: Option<Arc<ryeos_state::state_db::ClosedOriginalState>>,
    close_failure: Option<ryeos_state::state_db::OriginalStateRetirementFailure>,
}
impl std::fmt::Debug for OfflineStateProjectionSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OfflineStateProjectionSlot")
            .field("issued", &self.issued)
            .field("factory_retained", &self.factory.is_some())
            .field("retained", &self.retained.is_some())
            .finish()
    }
}
struct OfflineStateProjectionPermit {
    custody: Arc<InspectionControllerCustody>,
}
impl OfflineStateProjectionPermit {
    fn retain_factory(
        self,
        factory: Arc<lillux_sqlite::OriginalNativeFactory>,
    ) -> OfflineNativeStateProjectionPermit {
        // The noncopyable, one-use permit transfers the initialized owner
        // before any fallible State preparation/open step. A refusal retains
        // the SAME factory for eventual terminal census and arena disposal.
        // Bookkeeping only: no native effects or terminal acknowledgement.
        self.custody
            .state_projection
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .factory = Some(factory);
        OfflineNativeStateProjectionPermit {
            custody: self.custody,
        }
    }
}
struct OfflineNativeStateProjectionPermit {
    custody: Arc<InspectionControllerCustody>,
}
impl OfflineNativeStateProjectionPermit {
    fn adopt(self, state: ryeos_state::StateDb) -> OfflineStateProjectionLoan {
        // Infallible actual owner handoff after acquisition. The permit is
        // private, noncopyable and issued once before its first native effect.
        OfflineStateProjectionLoan {
            state: Some(state),
            custody: self.custody,
        }
    }
}
struct OfflineStateProjectionLoan {
    state: Option<ryeos_state::StateDb>,
    custody: Arc<InspectionControllerCustody>,
}
impl OfflineStateProjectionLoan {
    fn close(
        mut self,
        factory: Arc<lillux_sqlite::OriginalNativeFactory>,
        guard: &ryeos_state::recovery::CasMutationGuard,
        operation: Option<&ryeos_state::recovery::ThreadHistoryDiscardOperationRef>,
    ) -> Result<Arc<ryeos_state::state_db::ClosedOriginalState>> {
        let state = self.state.take().expect("live one-use original State loan");
        match state.close_original_projection(factory, guard, operation) {
            Ok(closed) => {
                let closed = Arc::new(closed);
                self.custody
                    .state_projection
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .closed = Some(Arc::clone(&closed));
                Ok(closed)
            }
            Err(failure) => {
                let message = format!("original State retirement remains retained: {failure}");
                self.custody
                    .state_projection
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .close_failure = Some(failure);
                Err(self.custody.retain_failure(anyhow::anyhow!(message)))
            }
        }
    }
}
impl std::ops::Deref for OfflineStateProjectionLoan {
    type Target = ryeos_state::StateDb;
    fn deref(&self) -> &Self::Target {
        self.state
            .as_ref()
            .expect("live original State projection loan")
    }
}
impl std::ops::DerefMut for OfflineStateProjectionLoan {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.state
            .as_mut()
            .expect("live original State projection loan")
    }
}
impl Drop for OfflineStateProjectionLoan {
    fn drop(&mut self) {
        if let Some(state) = self.state.take() {
            // Transfer the SAME actual service into the enclosing result/error
            // owner. This performs no native close, terminal ACK or refund.
            // The one-use permit prevents a second safe loan/slot overwrite.
            self.custody
                .state_projection
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .retained = Some(state);
        }
    }
}

// One original Runtime service loan; Drop transfers custody only, never SQL.
#[derive(Default)]
struct OfflineRuntimeSlot {
    issued: bool,
    retained: Option<RuntimeDb>,
    closed: Option<Arc<crate::runtime_db::OriginalRuntimeClosed>>,
    close_failure: Option<crate::runtime_db::OriginalRuntimeCloseFailure>,
    /// Closed mutation schema reference whose main inputs were refused
    /// before any open; its terminal belongs to the census.
    reference: Option<ryeos_state::sqlite_schema::ClosedNativeSchemaSqlReference>,
    /// A refused mutation schema reference after its same-owner close, with
    /// the factory its terminal is checked against.
    released_reference: Option<(
        Arc<lillux_sqlite::OriginalNativeFactory>,
        ryeos_state::sqlite_schema::ReleasedNativeSchemaSqlReference,
    )>,
    /// A refused mutation schema reference whose same-owner close refused.
    refused_reference: Option<ryeos_state::sqlite_schema::NativeSchemaSqlReferenceFailure>,
}
impl std::fmt::Debug for OfflineRuntimeSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OfflineRuntimeSlot")
            .field("issued", &self.issued)
            .field("retained", &self.retained.is_some())
            .field("reference", &self.reference.is_some())
            .field("released_reference", &self.released_reference.is_some())
            .field("refused_reference", &self.refused_reference.is_some())
            .finish()
    }
}
/// A Runtime schema reference the controller could not slot: it stays with
/// the refusal that reports it, never dropped.
enum RuntimeReferenceOwner {
    Closed {
        _reference: ryeos_state::sqlite_schema::ClosedNativeSchemaSqlReference,
    },
    Released {
        _factory: Arc<lillux_sqlite::OriginalNativeFactory>,
        _released: ryeos_state::sqlite_schema::ReleasedNativeSchemaSqlReference,
    },
    Refused {
        _refused: ryeos_state::sqlite_schema::NativeSchemaSqlReferenceFailure,
    },
}
struct RetainedRuntimeReference {
    error: anyhow::Error,
    _owner: RuntimeReferenceOwner,
}
impl RetainedRuntimeReference {
    fn error(original: anyhow::Error, owner: RuntimeReferenceOwner) -> anyhow::Error {
        anyhow::Error::new(Self {
            error: original
                .context("Runtime schema reference slot already occupied; owner retained"),
            _owner: owner,
        })
    }
}
impl std::fmt::Debug for RetainedRuntimeReference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RetainedRuntimeReference")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Display for RetainedRuntimeReference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", self.error)
    }
}
impl std::error::Error for RetainedRuntimeReference {}
/// The closed schema references of the Runtime and scheduler inspection
/// phases, held for the census after each reference connection closed.
#[derive(Default)]
struct OfflineRuntimeInspectionReferenceSlot {
    references: Vec<crate::runtime_db::RuntimeInspectionReferenceRelease>,
}
impl std::fmt::Debug for OfflineRuntimeInspectionReferenceSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OfflineRuntimeInspectionReferenceSlot")
            .field("references", &self.references.len())
            .finish()
    }
}
struct OfflineRuntimePermit {
    custody: Arc<InspectionControllerCustody>,
}
impl OfflineRuntimePermit {
    fn adopt(self, runtime: RuntimeDb) -> OfflineRuntimeLoan {
        OfflineRuntimeLoan {
            runtime: Some(runtime),
            custody: self.custody,
        }
    }
}
struct OfflineRuntimeLoan {
    runtime: Option<RuntimeDb>,
    custody: Arc<InspectionControllerCustody>,
}
impl OfflineRuntimeLoan {
    fn close(
        mut self,
        factory: Arc<lillux_sqlite::OriginalNativeFactory>,
    ) -> Result<Arc<crate::runtime_db::OriginalRuntimeClosed>> {
        let runtime = self.runtime.take().expect("live one-use Runtime loan");
        match runtime.close_original_runtime(factory) {
            Ok(closed) => {
                let closed = Arc::new(closed);
                // Actual terminal custody is transferred before caller checks.
                self.custody
                    .runtime_mutation
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .closed = Some(Arc::clone(&closed));
                Ok(closed)
            }
            Err(failure) => {
                let message = format!("original Runtime close remains retained: {failure}");
                self.custody
                    .runtime_mutation
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .close_failure = Some(failure);
                Err(self.custody.retain_failure(anyhow::anyhow!(message)))
            }
        }
    }
}
impl std::ops::Deref for OfflineRuntimeLoan {
    type Target = RuntimeDb;
    fn deref(&self) -> &RuntimeDb {
        self.runtime.as_ref().expect("live original Runtime loan")
    }
}
impl std::ops::DerefMut for OfflineRuntimeLoan {
    fn deref_mut(&mut self) -> &mut RuntimeDb {
        self.runtime.as_mut().expect("live original Runtime loan")
    }
}
impl Drop for OfflineRuntimeLoan {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            self.custody
                .runtime_mutation
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .retained = Some(runtime);
        }
    }
}

/// Scheduler copy terminals that carry no copy proof, each with the factory
/// it is checked against. They join the census; no cleanup reaches them.
#[derive(Default)]
struct UnprovenSchedulerTerminalSlot {
    terminals: Vec<(
        Arc<lillux_sqlite::OriginalNativeFactory>,
        lillux_sqlite::OriginalNativeConnectionTerminal,
    )>,
}
impl std::fmt::Debug for UnprovenSchedulerTerminalSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnprovenSchedulerTerminalSlot")
            .field("terminals", &self.terminals.len())
            .finish()
    }
}

#[derive(Default)]
struct OriginalNativeCensusSlot {
    terminals: Option<Vec<lillux_sqlite::OriginalNativeConnectionTerminal>>,
    cleanup_inputs: Option<lillux_sqlite::OriginalNativeCleanupInputs>,
}
impl std::fmt::Debug for OriginalNativeCensusSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OriginalNativeCensusSlot")
            .field("retained", &self.terminals.is_some())
            .finish()
    }
}

/// Operation-level custody has no strong back-edge from a claim or operation.
/// Both successful reports and errors retain the same actual proof objects.
#[derive(Debug, Default)]
struct InspectionControllerCustody {
    native_backend: std::sync::Mutex<native_backend::OriginalBackendSlot>,
    operational_inspection: std::sync::Mutex<operational_owner::OfflineOperationalSlot>,
    operational_mutation: std::sync::Mutex<operational_owner::OfflineOperationalSlot>,
    state_projection: std::sync::Mutex<OfflineStateProjectionSlot>,
    runtime_mutation: std::sync::Mutex<OfflineRuntimeSlot>,
    runtime_inspection_reference: std::sync::Mutex<OfflineRuntimeInspectionReferenceSlot>,
    native_census: std::sync::Mutex<OriginalNativeCensusSlot>,
    closed: std::sync::Mutex<Vec<Arc<SqliteInspectionClosed>>>,
    cleanup: std::sync::Mutex<Vec<Arc<InspectionCopyCleanup>>>,
    completed_disposals: std::sync::Mutex<Vec<Arc<InspectionCopyDisposal>>>,
    settlement: std::sync::Mutex<Option<Arc<OriginalInspectionSettlement>>>,
    cleanup_failures: std::sync::Mutex<Vec<anyhow::Error>>,
    runtime_released: std::sync::Mutex<Vec<Arc<RuntimeInspectionRelease>>>,
    scheduler_unproven: std::sync::Mutex<UnprovenSchedulerTerminalSlot>,
    /// Set once this attempt's reservation is published: from then on a
    /// failure leaves scratch that the next reset settles.
    reservation_published: std::sync::atomic::AtomicBool,
}
impl InspectionControllerCustody {
    fn retain_native_census(
        &self,
        mut terminals: Vec<lillux_sqlite::OriginalNativeConnectionTerminal>,
    ) -> Result<()> {
        self.append_operational_terminals(&mut terminals)?;
        self.append_runtime_reference_terminals(&mut terminals)?;
        let closed = self
            .closed
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        for owner in closed {
            if let Some(terminal) = owner.original_native_terminal()? {
                terminals.push(terminal);
            }
        }
        self.append_unproven_scheduler_terminals(&mut terminals)?;
        let released = self
            .runtime_released
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        for owner in released {
            let terminal = match owner.as_ref() {
                RuntimeInspectionRelease::Closed(owner) => owner.original_native_terminal()?,
                RuntimeInspectionRelease::Acquisition(owner) => owner.original_native_terminal()?,
            };
            if let Some(terminal) = terminal {
                terminals.push(terminal);
            }
        }
        ensure!(
            self.native_census
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .terminals
                .is_none(),
            "original native census already retained"
        );
        // The host's own census is the exhaustive one its backend finishes
        // against. Its handover skips terminals already entered, so after a
        // partial refusal this whole call can be retried; the controller's
        // copy, kept for the cleanup inputs, is filled only once the host
        // holds every terminal.
        self.retain_host_terminals(&terminals)?;
        let mut slot = self.native_census.lock().unwrap_or_else(|p| p.into_inner());
        ensure!(
            slot.terminals.is_none(),
            "original native census already retained"
        );
        // Store genuine role terminals only; this is not cardinality approval,
        // a backend terminal, physical disposal or original settlement.
        slot.terminals = Some(terminals);
        Ok(())
    }
    /// Each Runtime phase paid its own schema-reference ticket. The closed
    /// mutation reference rides its closed Runtime; the others join here.
    fn append_runtime_reference_terminals(
        &self,
        terminals: &mut Vec<lillux_sqlite::OriginalNativeConnectionTerminal>,
    ) -> Result<()> {
        for reference in &self
            .runtime_inspection_reference
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .references
        {
            terminals.extend(reference.census_terminal()?);
        }
        let slot = self
            .runtime_mutation
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        ensure!(
            slot.refused_reference.is_none(),
            "Runtime schema reference still retained; terminal census incomplete"
        );
        if let Some(reference) = &slot.reference {
            terminals.push(reference.terminal().clone());
        }
        if let Some((factory, released)) = &slot.released_reference {
            // A reference refused before any ticket has no census entry.
            if let Some(terminal) = released.terminal(factory)?
                && terminal.ticket().is_some()
            {
                terminals.push(terminal.clone());
            }
        }
        Ok(())
    }
    fn retain_runtime_inspection_reference(
        &self,
        reference: crate::runtime_db::RuntimeInspectionReferenceRelease,
    ) {
        // Bookkeeping only; the census validates every retained terminal.
        self.runtime_inspection_reference
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .references
            .push(reference);
    }
    /// Same-owner close of a refused Runtime mutation schema reference. A
    /// released reference joins the census; a still-refused one stays here.
    /// An occupied slot refuses and the incoming owner rides the error.
    fn retain_runtime_reference_refusal(
        &self,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        refused: ryeos_state::sqlite_schema::NativeSchemaSqlReferenceFailure,
    ) -> anyhow::Error {
        let original = anyhow!("original Runtime mutation schema reference refused: {refused:#}");
        match refused.close_retained() {
            Ok(released) => {
                let mut slot = self
                    .runtime_mutation
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                if slot.released_reference.is_some() {
                    return RetainedRuntimeReference::error(
                        original,
                        RuntimeReferenceOwner::Released {
                            _factory: Arc::clone(factory),
                            _released: released,
                        },
                    );
                }
                slot.released_reference = Some((Arc::clone(factory), released));
                original
            }
            Err(refused) => {
                let original = original
                    .context("same-owner Runtime schema reference cleanup remains retained");
                let mut slot = self
                    .runtime_mutation
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                if slot.refused_reference.is_some() {
                    return RetainedRuntimeReference::error(
                        original,
                        RuntimeReferenceOwner::Refused { _refused: refused },
                    );
                }
                slot.refused_reference = Some(refused);
                original
            }
        }
    }
    /// A closed mutation reference whose main inputs were refused before open.
    /// An occupied slot refuses and the incoming owner rides the error.
    fn retain_unused_runtime_reference(
        &self,
        reference: ryeos_state::sqlite_schema::ClosedNativeSchemaSqlReference,
        original: anyhow::Error,
    ) -> anyhow::Error {
        let mut slot = self
            .runtime_mutation
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if slot.reference.is_some() {
            return RetainedRuntimeReference::error(
                original,
                RuntimeReferenceOwner::Closed {
                    _reference: reference,
                },
            );
        }
        slot.reference = Some(reference);
        original
    }
    fn begin_runtime_mutation(self: &Arc<Self>) -> Result<OfflineRuntimePermit> {
        let mut slot = self
            .runtime_mutation
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        ensure!(
            !slot.issued && slot.retained.is_none(),
            "original Runtime owner already issued"
        );
        slot.issued = true;
        Ok(OfflineRuntimePermit {
            custody: Arc::clone(self),
        })
    }
    fn begin_state_projection(self: &Arc<Self>) -> Result<OfflineStateProjectionPermit> {
        let mut slot = self
            .state_projection
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        ensure!(
            !slot.issued && slot.retained.is_none(),
            "original State projection owner already issued"
        );
        slot.issued = true;
        Ok(OfflineStateProjectionPermit {
            custody: Arc::clone(self),
        })
    }
    fn retain(&self, closed: Arc<SqliteInspectionClosed>) {
        // Bookkeeping only; poisoning cannot discard acquired native evidence.
        self.closed
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push(closed);
    }
    /// File the scheduler copy's terminal release with the other inspection
    /// releases, BEFORE the caller checks it, and hand back the filed owner.
    fn retain_scheduler_release(
        &self,
        release: RuntimeInspectionRelease,
    ) -> Arc<RuntimeInspectionRelease> {
        let release = Arc::new(release);
        // Bookkeeping only; poisoning cannot discard acquired native evidence.
        self.runtime_released
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push(Arc::clone(&release));
        release
    }
    /// A scheduler copy terminal that carries no copy proof. It still ended
    /// a ticket of the host, so it waits here for the census.
    fn retain_unproven_scheduler_terminal(
        &self,
        factory: Arc<lillux_sqlite::OriginalNativeFactory>,
        terminal: lillux_sqlite::OriginalNativeConnectionTerminal,
    ) {
        self.scheduler_unproven
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .terminals
            .push((factory, terminal));
    }
    /// Ticketed scheduler copy terminals without a copy proof, each checked
    /// against the factory it was filed with.
    fn append_unproven_scheduler_terminals(
        &self,
        terminals: &mut Vec<lillux_sqlite::OriginalNativeConnectionTerminal>,
    ) -> Result<()> {
        for (factory, terminal) in &self
            .scheduler_unproven
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .terminals
        {
            terminal.require_original(factory)?;
            if terminal.ticket().is_some() {
                terminals.push(terminal.clone());
            }
        }
        Ok(())
    }
    fn retain_runtime_release(&self, release: RuntimeInspectionRelease) -> Result<()> {
        let release = Arc::new(release);
        // Store BEFORE validating the transfer. A refused join cannot destroy
        // the one real terminal proof that the native owner just transferred.
        self.runtime_released
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push(release.clone());
        match release.as_ref() {
            RuntimeInspectionRelease::Closed(closed) => closed.require_original(),
            RuntimeInspectionRelease::Acquisition(released) => released.require_original(),
        }
    }
    fn retry_runtime_failure(&self, original: anyhow::Error) -> anyhow::Error {
        let recovery = original
            .chain()
            .find_map(|cause| cause.downcast_ref::<RuntimeInspectionNativeFailure>())
            .map(|failure| {
                failure
                    .retry_close()
                    .and_then(|()| failure.take_release())
                    .and_then(|(release, reference)| {
                        // Retain the reference before validating the release;
                        // a refused join keeps both actual owners. A phase that
                        // never opened its main owner releases its reference only.
                        if let Some(reference) = reference {
                            self.retain_runtime_inspection_reference(reference);
                        }
                        match release {
                            Some(release) => self.retain_runtime_release(release),
                            None => Ok(()),
                        }
                    })
            });
        match recovery {
            Some(Err(retry)) => {
                anyhow::Error::new(NativeInspectionRetryFailure { original, retry })
            }
            // Successful release still leaves the original operation refused.
            // Partial acquisition is never relabelled as completed accounting.
            Some(Ok(())) | None => original,
        }
    }
    /// Native proof custody is retained BEFORE acknowledging its original
    /// production interval. A refused or partial acquisition remains a refusal,
    /// even when its actual native owner has reached terminal release.
    fn acknowledge_producer_terminals(&self) -> Result<()> {
        let closed = self
            .closed
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let runtime = self
            .runtime_released
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        for owner in closed {
            owner.acknowledge_producer_terminal()?;
        }
        for owner in runtime {
            match owner.as_ref() {
                RuntimeInspectionRelease::Closed(owner) => owner.acknowledge_producer_terminal()?,
                RuntimeInspectionRelease::Acquisition(owner) => {
                    owner.acknowledge_producer_terminal()?
                }
            }
        }
        Ok(())
    }

    fn dispose_inspection_namespace(&self) -> Result<()> {
        self.acknowledge_producer_terminals()?;
        // Snapshot retained terminal owners outside bookkeeping locks. Actual
        // work/namespace exclusion belongs to each original copy issuer.
        let closed = self
            .closed
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone();
        let runtime = self
            .runtime_released
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone();
        let mut cleanup = Vec::with_capacity(closed.len() + runtime.len());
        for owner in closed {
            let owner = InspectionCopyCleanup::for_closed(owner)?;
            self.cleanup
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .push(owner.clone());
            cleanup.push(owner);
        }
        for owner in runtime {
            let owner = InspectionCopyCleanup::for_runtime(owner)?;
            self.cleanup
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .push(owner.clone());
            cleanup.push(owner);
        }
        for owner in cleanup {
            let proof = self.dispose_cleanup_owner(&owner)?;
            // Preserve actual completed sibling evidence BEFORE proceeding.
            // The later two-store gate must authenticate the exact inventory;
            // this collection alone is not aggregate completion or settlement.
            let mut completed = self
                .completed_disposals
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            if !completed
                .iter()
                .any(|original| original.same_original(&proof))
            {
                completed.push(proof);
            }
        }
        Ok(())
    }
    /// Prepare and retain the SAME original settlement before any durable
    /// acknowledgement or witness effect. No bookkeeping lock spans effects.
    fn prepare_inspection_settlement(
        &self,
        claim: &Arc<OriginalQuotaClaim>,
        work: &Arc<dyn lillux_sqlite::SqliteInspectionCopyWork>,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
    ) -> Result<Arc<OriginalInspectionSettlement>> {
        let retained = self
            .native_census
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .cleanup_inputs
            .clone();
        let inputs = if let Some(inputs) = retained {
            inputs.require_original(factory)?;
            inputs
        } else {
            self.acknowledge_producer_terminals()?;
            // Debited but unpublished host tickets are released into the
            // host's own census when its backend finishes; sealing the claim's
            // producers here also closes the host to new service tickets.
            let seal = claim.seal_production(work)?;
            let terminal = self
                .native_census
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .terminals
                .as_ref()
                .and_then(|terminals| terminals.first())
                .cloned()
                .context("original native cleanup lacks a genuine closed role terminal")?;
            let inputs = seal.issue_native_cleanup(factory, terminal)?;
            // Retain the actual signed seal/clock/factory inputs before any
            // stopped Binding capture or native cleanup effect can occur.
            self.native_census
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .cleanup_inputs = Some(inputs.clone());
            inputs
        };
        match factory.with_original_cleanup(&inputs, |_scope| {
            self.prepare_inspection_settlement_scoped(claim, work)
        }) {
            Ok(settlement) => Ok(settlement),
            Err(failure) => {
                // The typed failure retains the actual factory, inputs and
                // callback T on late refusal. Inner transfers also keep every
                // completed sibling and settlement in this controller.
                let message = format!("original native cleanup scope refused: {}", failure.error());
                self.cleanup_failures
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push(anyhow::Error::new(failure));
                Err(anyhow::anyhow!(message))
            }
        }
    }

    fn prepare_inspection_settlement_scoped(
        &self,
        claim: &Arc<OriginalQuotaClaim>,
        work: &Arc<dyn lillux_sqlite::SqliteInspectionCopyWork>,
    ) -> Result<Arc<OriginalInspectionSettlement>> {
        let retained = self
            .settlement
            .lock()
            .map_err(|_| anyhow::anyhow!("original settlement custody poisoned"))?
            .clone();
        let settlement = if let Some(original) = retained {
            original.require_original(claim, work)?;
            original
        } else {
            self.acknowledge_producer_terminals()?;
            let seal = claim.seal_production(work)?;
            self.dispose_inspection_namespace()?;
            let disposals = self
                .completed_disposals
                .lock()
                .map_err(|_| anyhow::anyhow!("original disposal inventory poisoned"))?
                .clone();
            let original = claim.begin_inspection_settlement(seal, disposals)?;
            let mut retained = self
                .settlement
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            if let Some(existing) = retained.as_ref() {
                if !Arc::ptr_eq(existing, &original) {
                    // Preserve the ACTUAL newly issued owner even on a refused
                    // transfer. The existing owner stays in controller custody.
                    return Err(anyhow::Error::new(InspectionSettlementTransferFailure {
                        _owner: original,
                    }));
                }
            } else {
                *retained = Some(original.clone());
            }
            drop(retained);
            original
        };
        settlement.disposition_record()?;
        settlement.dispose_witnesses()?;
        settlement.seal_disposition()?;
        Ok(settlement)
    }

    fn dispose_cleanup_owner(
        &self,
        owner: &Arc<InspectionCopyCleanup>,
    ) -> Result<Arc<InspectionCopyDisposal>> {
        if let Err(original) = owner.dispose_namespace() {
            if !InspectionCopyCleanup::retryable_failure(&original) {
                return Err(original);
            }
            // Preserve the original typed error BEFORE the single production
            // retry. No controller lock spans native cleanup work. This is the
            // same progress/guard/clock/debit owner, never a fresh operation.
            self.cleanup_failures
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .push(original);
            owner
                .dispose_namespace()
                .context("retry same original inspection cleanup owner after retained failure")?;
        }
        owner.completed_disposal()
    }
    fn retain_failure(self: &Arc<Self>, error: anyhow::Error) -> anyhow::Error {
        anyhow::Error::new(InspectionControllerFailure {
            error,
            _custody: self.clone(),
        })
    }
}
struct InspectionSettlementTransferFailure {
    _owner: Arc<OriginalInspectionSettlement>,
}
impl std::fmt::Debug for InspectionSettlementTransferFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InspectionSettlementTransferFailure")
            .finish_non_exhaustive()
    }
}
impl std::fmt::Display for InspectionSettlementTransferFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("original settlement already transferred; conflicting issued owner retained")
    }
}
impl std::error::Error for InspectionSettlementTransferFailure {}

#[derive(Debug)]
struct InspectionControllerFailure {
    error: anyhow::Error,
    _custody: Arc<InspectionControllerCustody>,
}
impl std::fmt::Display for InspectionControllerFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.error, f)
    }
}
impl std::error::Error for InspectionControllerFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}

/// A scheduler copy refusal with the copy root's namespace and the SQL work
/// issued for the copy, once they exist; the copy lease rides the enclosing
/// custody error.
struct SchedulerCopyRefusal {
    error: anyhow::Error,
    _sql: Option<Arc<dyn lillux_sqlite::SqliteInspectionSqlWork>>,
    _namespace: Option<lillux::PinnedDirectoryLock>,
}
impl std::fmt::Debug for SchedulerCopyRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SchedulerCopyRefusal")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Display for SchedulerCopyRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.error, f)
    }
}
impl std::error::Error for SchedulerCopyRefusal {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}

#[derive(Debug)]
struct SchedulerAcquisitionReleaseFailure {
    error: anyhow::Error,
    _released: Arc<RuntimeInspectionRelease>,
}
impl std::fmt::Display for SchedulerAcquisitionReleaseFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.error, f)
    }
}
impl std::error::Error for SchedulerAcquisitionReleaseFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}

#[derive(Debug)]
struct NativeInspectionRetryFailure {
    original: anyhow::Error,
    retry: anyhow::Error,
}
impl std::fmt::Display for NativeInspectionRetryFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}; same-owner native release retry refused: {}",
            self.original, self.retry
        )
    }
}
impl std::error::Error for NativeInspectionRetryFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.original.as_ref())
    }
}

/// Retain both the read refusal and the SAME native close failure. A close
/// failure does not turn failed accounting into a successful report.
#[derive(Debug)]
struct RuntimeInspectionReadFailure {
    close: anyhow::Error,
    inspection: Option<anyhow::Error>,
}
impl std::fmt::Display for RuntimeInspectionReadFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(inspection) = &self.inspection {
            write!(f, "runtime inspection refused: {inspection}; ")?;
        }
        write!(f, "runtime inspection native close: {}", self.close)
    }
}
impl std::error::Error for RuntimeInspectionReadFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.close.as_ref())
    }
}

fn run_execution_history_reset_inner(
    options: &ExecutionHistoryResetOptions,
    participant: Option<&dyn SchedulerHistoryResetParticipant>,
    observer: Option<&mut dyn FnMut(&ExecutionHistoryResetProgress)>,
    state_lock: &StateLock,
) -> Result<ExecutionHistoryResetReport> {
    let custody = Arc::new(InspectionControllerCustody::default());
    run_execution_history_reset_owned(options, participant, observer, &custody, state_lock).map_err(|error| {
        let error = if custody.reservation_published.load(std::sync::atomic::Ordering::Acquire) {
            error.context(
                "this attempt's reservation stays charged; the next `ryeos node reset execution-history` settles it",
            )
        } else {
            error
        };
        custody.retain_failure(error)
    })
}

fn run_execution_history_reset_owned(
    options: &ExecutionHistoryResetOptions,
    participant: Option<&dyn SchedulerHistoryResetParticipant>,
    mut observer: Option<&mut dyn FnMut(&ExecutionHistoryResetProgress)>,
    inspection_custody: &Arc<InspectionControllerCustody>,
    state_lock: &StateLock,
) -> Result<ExecutionHistoryResetReport> {
    crate::object_contracts::install()
        .context("install application object contracts for execution-history reset")?;
    publish_progress(
        &mut observer,
        ExecutionHistoryResetPhase::CapturingAuthority,
        None,
    );
    let config = Config::load(&ConfigSources {
        app_root: options.app_root.clone(),
        ..ConfigSources::default()
    })
    .context("load local node configuration for execution-history reset")?;
    let runtime_state_dir = config.runtime_state_dir();

    // The caller's lock is the outer ownership proof; it must be this node's.
    state_lock
        .ensure_protects_app_root(&config.app_root)
        .context("execution-history reset needs the state lock of the node it resets")?;
    let runtime_directory =
        lillux::PinnedDirectory::open(&runtime_state_dir)?.ok_or_else(|| {
            anyhow::anyhow!("runtime state is absent: {}", runtime_state_dir.display())
        })?;
    let runtime_directory_lock = runtime_directory
        .lock_exclusive()
        .context("lock offline runtime-state namespace")?;

    // Signed chain heads belong to the node-identity trust domain, matching
    // ordinary daemon startup. Item/publisher trust is a separate domain and
    // must never be substituted for head authority here.
    let identity = ryeos_node_runtime::identity::NodeIdentity::load(&config.node_signing_key_path)
        .context("load node identity for signed-head verification")?;
    // Resolve signed operator authority before State/Runtime SQL or any copy.
    // This captures policy inputs only; a durable aggregate claim is still
    // required before resource acquisition can be qualified.
    let offline_policy = OfflineResetPolicyOwner::load(
        &config,
        &identity,
        state_lock,
        &runtime_directory,
        &runtime_directory_lock,
    )?;
    let mut head_trust = ryeos_state::refs::TrustStore::new();
    head_trust.insert(
        identity.fingerprint().to_string(),
        *identity.verifying_key(),
    );
    let head_trust = Arc::new(head_trust);
    // Capture all required State roots/trust/fences without opening SQLite.
    // The SAME preparation survives original quota publication below.
    let prepared_state = ryeos_state::PreparedOfflineProjection::capture(
        runtime_directory.try_clone()?,
        Arc::clone(&head_trust),
        &runtime_directory_lock,
    )
    .context("prepare pinned State authority before original native admission")?;
    let state_authority = prepared_state.authority()?;
    if !state_authority
        .runtime_directory()
        .is_same_directory(&runtime_directory)?
    {
        anyhow::bail!(
            "runtime-state path changed while execution-history reset captured authority"
        );
    }
    let cas_guard = state_authority
        .acquire_exclusive_guard(!options.dry_run)
        .context("acquire exclusive head mutation guard for execution-history reset")?;

    // Settle what earlier attempts' processes left before reading the active
    // operation: a stale claim on a resumable operation is released first,
    // so this attempt's claim can resume it.
    let recovered =
        stale_recovery::recover_stale(&offline_policy, &identity, &state_authority, &cas_guard)
            .context("settle stale execution-history reset attempts")?;
    // Resolve activated evidence before any scheduler preview. A retry must
    // not replace the original counts with a rebuilt or absent projection.
    let active_operation = OriginalOperation::read_active(
        &config.app_root,
        &state_authority,
        &cas_guard,
        &runtime_directory,
        &runtime_directory_lock,
        &identity,
        options,
        participant.is_some(),
    )?;
    // One original identity before Runtime inspection. Preparation is not a
    // native acquisition grant; work/cleanup enforcement remains required.
    let inspection_mode = if options.dry_run {
        inspection_destination::OriginalInspectionMode::DryRun
    } else {
        inspection_destination::OriginalInspectionMode::Mutating
    };
    let quota_claim = OriginalQuotaClaim::prepare(
        &offline_policy,
        &identity,
        active_operation.as_ref(),
        inspection_mode,
        participant.is_some(),
    )?;
    // A resumed operation runs under the new claim prepared for it.
    if let Some(operation) = &active_operation {
        operation
            .adopt_claim(&quota_claim)
            .map_err(|error| quota_claim.retain_failure(error, QuotaFailurePhase::Admission))?;
    }
    // Reserve the original aggregate envelope before Runtime inspection/copy.
    // This acknowledgement is not the unfinished native work/debit bridge.
    loop {
        match quota_claim.publish(&offline_policy, &identity) {
            Ok(()) => break,
            Err(error) => {
                // Cold/new nonce, absent/conflicting record or expired service
                // cannot mint a replacement. Retain the original typed owner.
                if quota_claim
                    .require_published(&offline_policy, &identity)
                    .is_err()
                {
                    return Err(error);
                }
                tracing::warn!(error = %error, "retaining original quota reservation after uncertain acknowledgement");
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }
    inspection_custody
        .reservation_published
        .store(true, std::sync::atomic::Ordering::Release);
    quota_claim
        .require_service_window()
        .map_err(|error| quota_claim.retain_failure(error, QuotaFailurePhase::Admission))?;
    // The published claim funds one offline native host; the controller
    // keeps it across every refusal. Its factory, budget and windows are
    // shared by State, Runtime and scheduler; no second envelope is issued.
    let state_projection_permit = inspection_custody
        .begin_state_projection()
        .map_err(|error| quota_claim.retain_failure(error, QuotaFailurePhase::Admission))?;
    let native_factory = inspection_custody
        .adopt_native_host(quota_claim.issue_native_host(&offline_policy, &identity))
        .map_err(|error| quota_claim.retain_failure(error, QuotaFailurePhase::Admission))?;
    let state_projection_permit =
        state_projection_permit.retain_factory(Arc::clone(&native_factory));
    let offline_lease_authority = quota_claim
        .native_projection_lease_authority(&native_factory)
        .map_err(|error| quota_claim.retain_failure(error, QuotaFailurePhase::Admission))?;
    // SAFETY: the claim's host authority retains its actual signed debit,
    // State lease, namespace lock and policy, the host's sole budget and the original
    // service/cleanup endpoints. This exact authority selects the same factory;
    // State owns actual generation consumers and the continuously held CAS guard.
    let state_db = unsafe {
        prepared_state.open_projection(
            Arc::clone(&native_factory),
            offline_lease_authority,
            &cas_guard,
        )
    }
    .map_err(|error| quota_claim.retain_failure(error, QuotaFailurePhase::Acquisition))?;
    let mut state_db = state_projection_permit.adopt(state_db);
    if options.dry_run {
        quota_claim
            .require_runtime_inspection_target(&config.db_path)
            .map_err(|error| quota_claim.retain_failure(error, QuotaFailurePhase::Admission))?;
    }
    let copy_work = quota_claim
        .issue_copy_work(&offline_policy, &identity)
        .map_err(|error| quota_claim.retain_failure(error, QuotaFailurePhase::Admission))?;
    let mut lifetime_fence_preview = None;
    let (
        mut runtime_db,
        observed_runtime_schema_epoch,
        runtime_schema_reset_required,
        inspected_runtime_rows,
    ) = if options.dry_run {
        // A preview reports the attested fence discard it would perform.
        let db = open_inspection_runtime_db(
            &config,
            &runtime_directory,
            &runtime_directory_lock,
            copy_work.clone(),
            Arc::clone(&native_factory),
            options.unreadable_lifetime_fence(),
        )
        .map_err(|error| {
            quota_claim.retain_failure(
                inspection_custody.retry_runtime_failure(error),
                QuotaFailurePhase::Acquisition,
            )
        })?;
        // Every ordinary read error must still attempt explicit close of the
        // same native owner. No live inspection survives into later previews.
        let inspected: Result<_> = (|| {
            let epoch = db
                .operator_schema_epoch()
                .context("inspect runtime operator schema epoch")?;
            let reset_required = db.requires_explicit_history_reset();
            let accounting = if reset_required {
                RuntimeThreadHistoryAccounting::UnavailableIncompatibleSchema
            } else {
                RuntimeThreadHistoryAccounting::Exact {
                    counts: db
                        .inspect_thread_history_discard()
                        .context("inspect daemon runtime thread rows")?,
                }
            };
            let fence = db
                .unreadable_lifetime_fence_decision(options.unreadable_lifetime_fence())
                .context("inspect the scope lifetime fence")?;
            Ok((epoch, reset_required, accounting, fence))
        })();
        match db.close_owned_inspection() {
            Ok((closed, reference)) => {
                inspection_custody.retain(closed);
                inspection_custody.retain_runtime_inspection_reference(
                    crate::runtime_db::RuntimeInspectionReferenceRelease::Closed(reference),
                );
            }
            Err(close) => {
                let close = inspection_custody.retry_runtime_failure(close);
                return Err(quota_claim.retain_failure(
                    anyhow::Error::new(RuntimeInspectionReadFailure {
                        close,
                        inspection: inspected.err(),
                    }),
                    QuotaFailurePhase::NativeClose,
                ));
            }
        }
        let (epoch, reset_required, accounting, fence) = inspected
            .map_err(|error| quota_claim.retain_failure(error, QuotaFailurePhase::Acquisition))?;
        lifetime_fence_preview = fence;
        (None, epoch, reset_required, Some(accounting))
    } else {
        let permit = inspection_custody
            .begin_runtime_mutation()
            .map_err(|error| quota_claim.retain_failure(error, QuotaFailurePhase::Admission))?;
        let db = open_original_runtime_db(
            &config,
            &runtime_directory,
            &runtime_directory_lock,
            &quota_claim,
            Arc::clone(&native_factory),
            inspection_custody,
            options.unreadable_lifetime_fence(),
        )
        .map_err(|error| quota_claim.retain_failure(error, QuotaFailurePhase::Acquisition))?;
        // Adopt before the first caller query: later errors transfer the SAME
        // live native Runtime into the report/error controller, never Drop SQL.
        let db = permit.adopt(db);
        let epoch = db
            .operator_schema_epoch()
            .context("inspect runtime operator schema epoch")?;
        let reset_required = db.requires_explicit_history_reset();
        (Some(db), epoch, reset_required, None)
    };
    offline_policy
        .require_held()
        .map_err(|error| quota_claim.retain_failure(error, QuotaFailurePhase::Admission))?;
    let current_runtime_schema_epoch = RuntimeDb::current_operator_schema_epoch();
    let schema_cut_already_current = if let Some(schema_cut) = options.schema_cut {
        if schema_cut.from == schema_cut.to {
            anyhow::bail!("execution-history schema cut must name distinct epochs");
        }
        if schema_cut.to != current_runtime_schema_epoch {
            anyhow::bail!(
                "execution-history schema cut targets epoch {}, but this RyeOS build owns epoch {}",
                schema_cut.to,
                current_runtime_schema_epoch
            );
        }
        if observed_runtime_schema_epoch != schema_cut.from
            && observed_runtime_schema_epoch != schema_cut.to
        {
            anyhow::bail!(
                "execution-history schema cut expects stored epoch {} or completed epoch {}, but observed epoch {}",
                schema_cut.from,
                schema_cut.to,
                observed_runtime_schema_epoch
            );
        }
        observed_runtime_schema_epoch == schema_cut.to && active_operation.is_none()
    } else {
        false
    };
    let scheduler_preview_permit = SchedulerResetPreview {
        runtime_directory: &runtime_directory,
        namespace_lock: &runtime_directory_lock,
        identity: Some(&identity),
        inspection_custody: std::cell::RefCell::new(None),
        inspection_closed: std::cell::RefCell::new(None),
        controller_custody: Some(inspection_custody),
        original_operation: active_operation.as_deref(),
        quota_claim: Arc::clone(&quota_claim),
        copy_work: copy_work.clone(),
    };
    match participant {
        Some(participant) => participant
            .preflight(&scheduler_preview_permit)
            .context("validate scheduler namespace before reset inspection or restart")?,
        None => require_absent_scheduler_history(&config, &runtime_directory)?,
    }
    let scheduler_preview = if let Some(operation) = &active_operation {
        operation.accounting().clone()
    } else {
        match participant {
            Some(participant) => participant
                .preview(&scheduler_preview_permit)
                .context("inspect scheduler history through selected participant")?,
            None => {
                require_absent_scheduler_history(
                    &config,
                    scheduler_preview_permit.runtime_directory()?,
                )?;
                ryeos_scheduler_contract::SchedulerFireHistoryAccounting::Exact {
                    counts: Default::default(),
                }
            }
        }
    };

    // Validate every participating namespace before publishing destructive
    // intent. The second pass performs the same descriptor-rooted checks while
    // mutating; no pathname-only deletion is used.
    publish_progress(
        &mut observer,
        ExecutionHistoryResetPhase::InspectingHistory,
        None,
    );
    let authoritative_preview = state_db
        .discard_authoritative_thread_history_admitted(&cas_guard, true)
        .context("inspect authoritative thread history")?;
    let project_heads_preview = if options.discard_project_heads {
        state_db
            .discard_all_project_heads_admitted(&cas_guard, true)
            .context("inspect principal and deployed project HEADs")?
    } else {
        0
    };
    let runtime_preview = if let Some(accounting) = inspected_runtime_rows {
        accounting
    } else if runtime_schema_reset_required {
        // Incompatible layouts are never decoded to manufacture exact counts.
        RuntimeThreadHistoryAccounting::UnavailableIncompatibleSchema
    } else {
        RuntimeThreadHistoryAccounting::Exact {
            counts: runtime_db
                .as_ref()
                .context("mutating reset lost runtime owner")?
                .inspect_thread_history_discard()
                .context("inspect daemon runtime thread rows")?,
        }
    };
    let thread_runtime_preview = crate::state_store::discard_all_thread_runtime_files(
        &config.app_root,
        &runtime_directory,
        true,
    )
    .context("inspect per-thread runtime files")?;
    let scheduler_journal_preview = inspect_scheduler_fire_journals(&runtime_directory)
        .context("inspect scheduler fire journals")?;

    // Replay indexes are disposable execution authority; credentials are not.
    // Inspect both through the existing operational owner before dry-run can
    // succeed. Never use ordinary current-open here: it correctly rejects
    // predecessor replay epochs, even though this command explicitly retires
    // them. The restricted preparation cannot service replay or retire rows.
    let operational_inspection = operational_owner::open_original_operational(
        inspection_custody,
        &quota_claim,
        &native_factory,
        &runtime_directory,
        &runtime_directory_lock,
        OperationalOwnerPhase::Inspection,
    )
    .context("admit strict readonly Operational inspection before intent")?;
    let replay_indexes = operational_inspection.report()?;
    let stable_profiles = operational_inspection
        .credential_profiles()
        .context("validate stable credential-profile authority before history retirement")?;
    // Keep this actual inspector's terminal. A later writable owner is a
    // distinct original ticket, not a readonly-to-write phase conversion.
    operational_inspection
        .close(Arc::clone(&native_factory))
        .context("close original Operational inspector before durable intent")?;

    // Credential-profile lifecycle authority is stable node state, not
    // execution history. OperationalDb is the only authority carried across
    // this cutover; a predecessor RuntimeDb is deliberately opaque and its
    // credential projection is never decoded as a compatibility format.
    // An enrolling ceremony is owned by worker/session history that this
    // operation retires. Its invalidation is itself a monotonic transition in
    // the stable authority and happens before any runtime-schema mutation.
    let stable_profiles = stable_profiles
        .into_iter()
        .map(|profile| -> Result<_> {
            if profile.state != "enrolling" {
                return Ok(profile);
            }
            let mut invalidated = profile;
            invalidated.authority_revision = invalidated
                .authority_revision
                .checked_add(1)
                .context("credential authority revision overflow during history retirement")?;
            invalidated.state = "unauthenticated".to_owned();
            invalidated.active_login_id = None;
            invalidated.login_expires_at_ms = None;
            invalidated.sanitized_account = None;
            invalidated.updated_at_ms = invalidated
                .updated_at_ms
                .max(lillux::time::timestamp_millis() as i64);
            Ok(invalidated)
        })
        .collect::<Result<Vec<_>>>()?;

    if options.dry_run || schema_cut_already_current {
        let runtime_terminals = if let Some(runtime) = runtime_db.take() {
            runtime
                .close(Arc::clone(&native_factory))
                .context("close original Runtime for already-current schema cut")?
                .native_terminals()?
        } else {
            Vec::new()
        };
        let closed_state = state_db
            .close(
                Arc::clone(&native_factory),
                &cas_guard,
                active_operation
                    .as_ref()
                    .map(|operation| operation.reference()),
            )
            .context("close original State for preview/already-current report")?;
        // SAFETY: the stopped-node State/namespace owner runs this operation
        // synchronously. Its consuming State close has retired selected and
        // generated SQL, and preview Runtime has closed above; no generation
        // consumer escapes this controller. Keep the actual owner and CAS frame.
        let leases = unsafe { closed_state.close_generation_leases_after_consumers(&cas_guard) }?;
        inspection_custody.retain_native_leases(&native_factory, leases)?;
        let mut native_terminals = closed_state.native_terminals()?;
        native_terminals.extend(runtime_terminals);
        inspection_custody.retain_native_census(native_terminals)?;
        // Seal production, dispose the copies, publish the disposition and
        // retire the creation witnesses.
        let settlement = inspection_custody
            .prepare_inspection_settlement(&quota_claim, &copy_work, &native_factory)
            .context("prepare original two-store inspection settlement")?;
        // Finish the host's backend and capture its facts.
        let native_terminal = inspection_custody
            .finish_native_backend()
            .context("finish the offline native host backend after preview cleanup scope")?;
        settlement.retain_native_facts(inspection_custody.native_facts()?)?;
        // Retire the source and reservation pins against the finished
        // backend; their identities are kept as plain facts.
        let post_backend = inspection_custody
            .post_backend_factory()
            .context("offline native host post-backend factory")?;
        drop(
            settlement
                .retire_release_pins(&post_backend, &native_terminal)
                .context("retire the release pins after the backend finished")?,
        );
        // A generation lease keeps the factory through its closed receipt
        // until retired, so every one is retired against the finished backend.
        closed_state
            .retire_generation_leases_after_backend(&native_terminal)
            .context("retire State generation leases against the finished native backend")?;
        // Every local alias of the factory, the backend terminal and the
        // closed owners ends before the quiesce.
        drop(post_backend);
        drop(native_terminal);
        drop(closed_state);
        drop(native_factory);
        drop(scheduler_preview_permit);
        let (released_sha256, durability_pins) =
            settle_reservation_release(inspection_custody, &settlement)?;
        publish_progress(&mut observer, ExecutionHistoryResetPhase::Complete, None);
        let report = ExecutionHistoryResetReport {
            released_sha256,
            recovered,
            durability_pins,
            lifetime_fence_discard: lifetime_fence_preview,
            app_root: config.app_root,
            dry_run: options.dry_run,
            performed: false,
            schema_cut: options
                .schema_cut
                .map(|schema_cut| ExecutionHistorySchemaCutReport {
                    from: schema_cut.from,
                    to: schema_cut.to,
                    observed: observed_runtime_schema_epoch,
                    status: if schema_cut_already_current {
                        ExecutionHistorySchemaCutStatus::AlreadyCurrent
                    } else {
                        ExecutionHistorySchemaCutStatus::Required
                    },
                }),
            chain_heads: authoritative_preview.chain_heads,
            project_heads: project_heads_preview,
            chain_ref_artifacts: authoritative_preview.chain_ref_artifacts,
            pending_transitions: authoritative_preview.pending_transitions,
            retirement_fences: authoritative_preview.retirement_fences,
            evidence_obligations: authoritative_preview.evidence_obligations,
            runtime_rows: runtime_preview,
            thread_runtime_artifacts: thread_runtime_preview,
            scheduler_journal_artifacts: scheduler_journal_preview,
            scheduler_rows: scheduler_preview,
            replay_indexes,
            projection: ProjectionDiscardReport {
                superseded_instances_deleted: authoritative_preview.superseded_projection_instances,
                ..ProjectionDiscardReport::default()
            },
        };
        // The copy work and the settlement (with its ledger lock) end
        // before the claim, whose StateLock lease ends with this frame.
        release_settlement_lifelines(inspection_custody, copy_work, settlement);
        return Ok(report);
    }

    let mut runtime_db = runtime_db
        .take()
        .context("mutating reset lost original runtime owner")?;
    let mut reset_authority = OfflineResetAuthority::capture(
        state_lock,
        &runtime_directory,
        &runtime_directory_lock,
        &config,
        &runtime_db,
    )?;
    reset_authority.require_held(&runtime_db)?;
    let operation = match &active_operation {
        Some(operation) => Arc::clone(operation),
        None => OriginalOperation::prepare(
            &reset_authority,
            &runtime_db,
            &state_authority,
            &cas_guard,
            &identity,
            options,
            participant.is_some(),
            &scheduler_preview,
            scheduler_preview_permit.inspection_custody.borrow().clone(),
            &quota_claim,
        )?,
    };
    operation
        .require_quota_claim(&quota_claim)
        .context("join original operation to its actual funded allocation owner")?;
    // Installation is inert; only the exact durable State reference activates
    // this memoized record before replay, runtime or scheduler effects.
    loop {
        match operation.stage(&runtime_directory) {
            Ok(()) => break,
            Err(error) => {
                // Retry only an exactly identified existing deposit. Absence,
                // replacement or failed verification never authorizes replay.
                if operation.require_staged(&runtime_directory).is_err() {
                    return Err(error);
                }
                operation.validate_pre_activation(&state_authority, &cas_guard, || {
                    reset_authority.require_held(&runtime_db)
                })?;
                eprintln!(
                    "execution-history reset retains its original offline frame while retrying exact operation staging acknowledgement: {error:#}"
                );
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        }
    }
    // The staged operation is now part of the signed namespace inventory.
    // Recheck before activating ANY cross-store effect, including on restart.
    let staged_preflight_permit = SchedulerResetPreview {
        runtime_directory: &runtime_directory,
        namespace_lock: &runtime_directory_lock,
        identity: Some(&identity),
        inspection_custody: std::cell::RefCell::new(None),
        inspection_closed: std::cell::RefCell::new(None),
        controller_custody: Some(inspection_custody),
        original_operation: Some(&operation),
        quota_claim: Arc::clone(&quota_claim),
        copy_work: copy_work.clone(),
    };
    operation.validate_pre_activation(&state_authority, &cas_guard, || {
        reset_authority.require_held(&runtime_db)?;
        match participant {
            Some(participant) => participant.preflight(&staged_preflight_permit),
            None => require_absent_scheduler_history(&config, &runtime_directory),
        }
    })?;
    publish_progress(
        &mut observer,
        ExecutionHistoryResetPhase::PublishingIntent,
        None,
    );
    loop {
        match operation.activate(&state_db, &state_authority, &cas_guard) {
            Ok(()) => break,
            Err(error) => {
                // The exact marker and record may have been installed while
                // their sync acknowledgement failed. Keep the same State/CAS,
                // runtime and source owners; no preview/signing/new ID here.
                if operation
                    .require_active_in(Some(native_factory.as_ref()), &state_authority, &cas_guard)
                    .is_err()
                {
                    return Err(error)
                        .context("publish exact offline thread-history discard intent");
                }
                operation.validate_pre_activation(&state_authority, &cas_guard, || {
                    reset_authority.require_held(&runtime_db)
                })?;
                eprintln!(
                    "execution-history reset retains its original offline frame while retrying exact State activation acknowledgement: {error:#}"
                );
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        }
    }
    // Only actual durable publication can advance this private capability.
    reset_authority.phase = OfflineResetAuthorityPhase::IntentPublished;
    reset_authority.authorize_mutation(&runtime_db)?;
    // The attested discard of an unreadable scope lifetime fence is the first
    // Runtime mutation after durable intent, so no later step reads the old
    // witness. A current or empty fence is left untouched.
    let lifetime_fence_discard = runtime_db
        .discard_unreadable_lifetime_fence(&reset_authority, options.unreadable_lifetime_fence())
        .context("discard the unreadable scope lifetime fence under the attested reboot")?;
    // Only after durable intent may any replay rows or enrolling ceremonies
    // be retired. A crash leaves the existing discard marker for safe retry.
    let operational_db = {
        let verify_operational_mutation = || -> Result<()> {
            operation.require_quota_claim(&quota_claim)?;
            operation.require_active_in(
                Some(native_factory.as_ref()),
                &state_authority,
                &cas_guard,
            )?;
            reset_authority.authorize_mutation(&runtime_db)
        };
        // SAFETY: this borrowed verifier retains the authenticated staged
        // operation, its exact funded claim, real offline mutation frame and
        // exclusive State/CAS guard. It validates the actual durable marker,
        // and none of these owners is moved into a native callback.
        let authority = unsafe {
            ryeos_state::OriginalOperationalMutationAuthority::issued(
                &runtime_directory,
                &native_factory,
                &verify_operational_mutation,
            )
        }?;
        // No writable Operational native open is permitted before the actual
        // intent check above. The original claim stays Producing; counters and
        // factory are not renewed between readonly and mutating tickets.
        let mut owner = operational_owner::open_original_operational(
            inspection_custody,
            &quota_claim,
            &native_factory,
            &runtime_directory,
            &runtime_directory_lock,
            OperationalOwnerPhase::Mutation,
        )
        .context("admit original Operational mutation after durable intent")?;
        anyhow::ensure!(
            owner.report()? == replay_indexes,
            "Operational retirement decision changed between original inspection and intent"
        );
        loop {
            quota_claim.require_service_window()?;
            verify_operational_mutation()?;
            match owner.activate(&authority, replay_indexes) {
                Ok(()) => break,
                Err(error) => {
                    eprintln!(
                        "execution-history reset retains its original replay owner and offline frame while retrying activation: {error:#}"
                    );
                    std::thread::sleep(std::time::Duration::from_secs(1));
                }
            }
        }
        for profile in &stable_profiles {
            owner
                .merge_credential_profile(&authority, profile)
                .context(
                    "preserve stable credential authority and invalidate retired enrollment",
                )?;
        }
        owner
    };
    runtime_db
        .apply_explicit_history_reset(&reset_authority, &config.db_path)
        .context("apply runtime schema cutover after durable discard intent")?;
    for profile in stable_profiles {
        runtime_db
            .reconcile_credential_profile_projection(&profile)
            .context("restore credential-profile runtime projection")?;
    }

    reset_authority.authorize_mutation(&runtime_db)?;
    let authoritative = {
        let mut authoritative_progress = |progress| match progress {
            ryeos_state::AuthoritativeThreadHistoryDiscardProgress::RetiringChainHeads {
                completed,
                total,
            } => publish_progress(
                &mut observer,
                ExecutionHistoryResetPhase::RetiringChainHeads,
                Some((completed, total)),
            ),
            ryeos_state::AuthoritativeThreadHistoryDiscardProgress::RebuildingProjection => {
                publish_progress(
                    &mut observer,
                    ExecutionHistoryResetPhase::RebuildingProjection,
                    None,
                )
            }
        };
        state_db
            .discard_authoritative_thread_history_admitted_with_progress(
                &cas_guard,
                false,
                &mut authoritative_progress,
            )
            .context("discard authoritative thread history")?
    };

    reset_authority.authorize_mutation(&runtime_db)?;
    let project_heads = if options.discard_project_heads {
        publish_progress(
            &mut observer,
            ExecutionHistoryResetPhase::RetiringProjectHeads,
            None,
        );
        state_db
            .discard_all_project_heads_admitted(&cas_guard, false)
            .context("discard principal and deployed project HEADs for schema cutover")?
    } else {
        0
    };

    publish_progress(
        &mut observer,
        ExecutionHistoryResetPhase::ClearingRuntime,
        None,
    );
    let runtime_rows = runtime_db
        .discard_all_thread_history(&reset_authority)
        .context("discard daemon runtime thread rows")?;
    reset_authority.authorize_mutation(&runtime_db)?;
    let thread_runtime_artifacts = crate::state_store::discard_all_thread_runtime_files(
        &config.app_root,
        &runtime_directory,
        false,
    )
    .context("discard per-thread runtime files")?;

    publish_progress(
        &mut observer,
        ExecutionHistoryResetPhase::ClearingScheduler,
        None,
    );
    reset_authority.authorize_mutation(&runtime_db)?;
    let permit = SchedulerResetRetirement {
        authority: &reset_authority,
        runtime_db: &runtime_db,
        cas_guard: &cas_guard,
        state_authority: &state_authority,
        identity: &identity,
        operation: &operation,
        native_factory: Some(&native_factory),
    };
    let mut scheduler_retirement_attempt = None;
    let scheduler_retirement = match participant {
        Some(participant) => {
            // Revalidate actual phase and exclusion before service entry too.
            permit.runtime_directory()?;
            // Report preserves pre-retirement accounting, including unknown
            // predecessor schema. A rebuilt empty projection cannot replace
            // that history estimate with fabricated exact zero counts.
            let mut attempt = participant
                .begin_retirement(&permit)
                .context("begin scheduler journal and projection retirement")?;
            // Keep this original same-thread frame: it owns the State lock,
            // runtime namespace pin/lock, StateDb, RuntimeDb and exclusive
            // CAS guard. Within the window an attempt error is diagnostic
            // only: retry that same cursor and never stringify its native
            // owner. Only an advance that follows another successful advance
            // is progress and restarts the window, so an error that resolves
            // and recurs cannot hold the reset open. A refusal past the
            // window returns, which ends the attempt before the permit and
            // before the frame owners declared ahead of it.
            let mut last_retry_notice: Option<std::time::Instant> = None;
            let mut window = lillux::time::MonotonicDeadline::after(SCHEDULER_SETTLEMENT_WINDOW);
            let mut previous_ok = false;
            let retirement = loop {
                match attempt.advance(&permit) {
                    Ok(Some(retirement)) => break retirement,
                    Ok(None) => {
                        if previous_ok {
                            window =
                                lillux::time::MonotonicDeadline::after(SCHEDULER_SETTLEMENT_WINDOW);
                        }
                        previous_ok = true;
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(error) => {
                        previous_ok = false;
                        if window.has_elapsed() {
                            let error =
                                scheduler_settlement_pending(error, "advance scheduler retirement");
                            return Err(with_scheduler_attempt_ended(
                                error,
                                &native_factory,
                                attempt,
                            ));
                        }
                        if last_retry_notice
                            .as_ref()
                            .is_none_or(|last| last.elapsed() >= std::time::Duration::from_secs(5))
                        {
                            eprintln!(
                                "execution-history reset retains its original State/CAS authority while retrying scheduler retirement: {error:#}"
                            );
                            last_retry_notice = Some(std::time::Instant::now());
                        }
                        std::thread::sleep(SCHEDULER_RETRY_INTERVAL);
                    }
                }
            };
            scheduler_retirement_attempt = Some(attempt);
            Some(retirement)
        }
        None => {
            require_absent_scheduler_history(&config, &runtime_directory)?;
            None
        }
    };
    let mut scheduler_journal_artifacts = scheduler_retirement
        .as_ref()
        .map_or(0, |retirement| retirement.journal_artifacts);

    publish_progress(&mut observer, ExecutionHistoryResetPhase::Finalizing, None);
    if let (Some(participant), Some(retirement)) = (participant, scheduler_retirement.as_ref()) {
        // Bounded like the advance loop. A refusal past the window returns,
        // which ends the scheduler attempt before the permit; the State
        // marker stays Active, so the next reset resumes the operation.
        let window = lillux::time::MonotonicDeadline::after(SCHEDULER_SETTLEMENT_WINDOW);
        loop {
            match permit
                .runtime_directory()
                .and_then(|_| participant.complete(&permit, retirement))
            {
                Ok(artifacts) => {
                    scheduler_journal_artifacts = artifacts;
                    break;
                }
                Err(error) => {
                    let permanent = is_scheduler_permanent_refusal(&error);
                    if permanent || window.has_elapsed() {
                        let error = if permanent {
                            error.context(
                                "execution-history reset stopped on a scheduler refusal no retry can \
                                 change; the operation stays Active and the next `ryeos node reset \
                                 execution-history` resumes it once the named entry is resolved",
                            )
                        } else {
                            scheduler_settlement_pending(error, "complete scheduler retirement")
                        };
                        return Err(match scheduler_retirement_attempt.take() {
                            Some(attempt) => {
                                with_scheduler_attempt_ended(error, &native_factory, attempt)
                            }
                            None => error,
                        });
                    }
                    eprintln!(
                        "execution-history reset retains its original State/CAS authority and scheduler attempt while retrying terminal completion: {error:#}"
                    );
                    std::thread::sleep(SCHEDULER_RETRY_INTERVAL);
                }
            }
        }
    }
    // End every borrowed scheduler frame before consuming the actual Runtime.
    // Its own terminal completion succeeded above; a completion refused past
    // its window returned there, ending the attempt before the permit just
    // as this handoff does. The attempt owns the stopped scheduler intake,
    // whose native context is freed only inside a scope of this arena: it
    // ends in a service scope, so the arena's binding census is empty before
    // the backend finishes. A scope refused before it ran leaves the attempt
    // here, where the refusal returns and ends it.
    {
        // Moved into this block so the attempt's borrows end here.
        let ending = scheduler_retirement_attempt;
        if let Some(attempt) = ending {
            end_scheduler_attempt(&native_factory, attempt).context(
                "end the scheduler retirement attempt in the reset's native service scope",
            )?;
        }
    }
    operational_db
        .close(Arc::clone(&native_factory))
        .context("close original Operational mutation before terminal census")?;
    let closed_runtime = runtime_db
        .close(Arc::clone(&native_factory))
        .context("close original Runtime before final reset acknowledgement")?;
    reset_authority.require_closed_runtime(&closed_runtime, &native_factory)?;
    let closed_state = state_db
        .close(
            Arc::clone(&native_factory),
            &cas_guard,
            Some(operation.reference()),
        )
        .context("consume original State projection before terminal cleanup")?;
    // SAFETY: the actual offline owner stopped all synchronous State work;
    // scheduler/Operational/Runtime consuming closes completed above, selected
    // and generated State SQL is in this genuine closed owner, and no escaped
    // generation task remains. Retain that same owner and exclusive CAS frame.
    let leases = unsafe { closed_state.close_generation_leases_after_consumers(&cas_guard) }?;
    inspection_custody.retain_native_leases(&native_factory, leases)?;
    let mut native_terminals = closed_state.native_terminals()?;
    native_terminals.extend(closed_runtime.native_terminals()?);
    inspection_custody.retain_native_census(native_terminals)?;

    // Copied-role cleanup must finish under original custody BEFORE removing
    // the active cross-store marker. Failure keeps the actual operation active.
    let inspection_settlement = inspection_custody
        .prepare_inspection_settlement(&quota_claim, &copy_work, &native_factory)
        .context(
            "prepare original two-store inspection settlement before final reset acknowledgement",
        )?;
    let native_terminal = inspection_custody
        .finish_native_backend()
        .context("finish the offline native host backend after mutating cleanup scope")?;
    inspection_settlement.retain_native_facts(inspection_custody.native_facts()?)?;
    // Post-backend work joins the host's finished factory: the State final
    // ACK and the retirements after the backend. The admitting alias ends
    // here, so only the post-backend one remains to quiesce.
    drop(native_factory);
    let native_factory = inspection_custody
        .post_backend_factory()
        .context("offline native host post-backend factory")?;
    // The final-ACK preparation retires every source pin and the issued
    // reservation pin before the State acknowledgement.
    let ready_for_final_ack = inspection_settlement
        .prepare_final_ack(&native_factory, &native_terminal, &operation)
        .context("prepare actual source retirement and retained funded final-ACK lifelines")?;
    // Runtime/copy close alone cannot acknowledge the complete original
    // native owner. Keep the marker until the genuine exhaustive census,
    // physical arena release and original issuer acknowledgement are joined.
    native_terminal
        .require_original(&native_factory)
        .context("original native issuer terminal unacknowledged; retain active State marker")?;
    // The projection rebuild issued a generation lease whose closed receipt
    // holds the factory until the lease is retired; dropping the State owner
    // never releases it. Retire every lease against the finished backend
    // while the State marker is still active, so a refusal keeps the
    // operation resumable.
    closed_state.retire_generation_leases_after_backend(&native_terminal)
        .context("retire State generation leases against the finished native backend; retain active State marker")?;

    // All store mutations have committed above, but the cross-store discard
    // marker must remain until scheduler terminal evidence is durable. A crash
    // during completion must not advertise a finished reset while leaving an
    // interrupted scheduler generation. Marker removal is the final ack.
    let verify_final_ack = || -> Result<()> {
        ready_for_final_ack.require_original(
            &quota_claim,
            &copy_work,
            &native_factory,
            &native_terminal,
        )?;
        operation.require_quota_claim(&quota_claim)?;
        reset_authority.require_closed_runtime(&closed_runtime, &native_factory)
    };
    let prepay_final_ack = |read_bytes, memory_bytes, cleanup_steps| {
        ready_for_final_ack.debit_state_ack_attempt(read_bytes, memory_bytes, cleanup_steps)
    };
    // SAFETY: actual ready proof retains the original sealed claim/work,
    // genuine native and source-retirement receipts, charged reservation and
    // cleanup clock. The same continuously held exclusive State/CAS frame and
    // authenticated operation are retained here; no retired source is reread.
    let final_ack_authority = unsafe {
        ryeos_state::state_db::OriginalStateFinalAckAuthority::issued(
            &closed_state,
            &cas_guard,
            &native_terminal,
            operation.reference(),
            &verify_final_ack,
            &prepay_final_ack,
        )
    }?;
    let state_acknowledgement = loop {
        // Original cleanup custody bounds retries; never renew service or
        // replace the failed owner with a new operation/funding envelope.
        verify_final_ack()?;
        match closed_state.finish_thread_history_discard_admitted(&final_ack_authority) {
            Ok(receipt) => break receipt,
            // A refusal no retry can change ends the loop; the error keeps
            // every owner through the controller.
            Err(error) if permanent_acknowledgement_refusal(&error) => {
                return Err(error.context(
                    "State refused the final acknowledgement permanently; the reset keeps its operation active",
                ));
            }
            Err(error) => {
                eprintln!(
                    "execution-history reset retains its original State/CAS authority and funded finalizer while retrying State acknowledgement: {error:#}"
                );
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        }
    };
    state_acknowledgement.require_original(
        &closed_state,
        &native_factory,
        &native_terminal,
        operation.reference(),
    )?;
    // Keep the State acknowledgement as settlement data before any fallible
    // later step. This join closes no root and releases no reservation.
    let post_ack_finalization = inspection_settlement
        .begin_post_ack_finalization(
            &ready_for_final_ack,
            &closed_state,
            &state_acknowledgement,
            operation.reference(),
        )
        .context("retain the actual State acknowledgement in the original settlement")?;
    post_ack_finalization.require_original(
        &ready_for_final_ack,
        &closed_state,
        &state_acknowledgement,
        operation.reference(),
    )?;
    // Every mutation is acknowledged. End only the borrowed mutation
    // authority; the policy owner, the StateLock and the claim's StateLock
    // lease stay held until this function returns.
    reset_authority.phase = OfflineResetAuthorityPhase::PostAckCustody;
    drop(reset_authority);
    // The offline host finished every native connection in-process when its
    // backend finished. Each State, Runtime and Operational terminal stays in
    // the host's census and ends with the host release, as on the preview
    // path. The closed owners hold descriptors and aliases of this reset's
    // own lifelines (namespace lock, recovery roots), which end at the
    // quiesce and with this frame, so none is handed to an outside receiver.
    // The acknowledged staged operation file goes first, under the same
    // settlement admission and cleanup window.
    post_ack_finalization
        .debit_post_ack_cleanup(STAGED_OPERATION_REMOVAL_STEPS)
        .and_then(|()| {
            operation
                .remove_staged_after_acknowledgement(&state_authority, &cas_guard)
                .context("remove the acknowledged staged operation file")
        })
        .context("history reset applied; settlement pending; the next reset settles it")?;
    // Every local alias of the factory, the backend terminal, the release
    // pins and the closed owners ends before the quiesce, as do the
    // scheduler permits that borrow the claim and copy work and hold closed
    // inspection owners.
    drop(post_ack_finalization);
    drop(state_acknowledgement);
    drop(closed_state);
    drop(closed_runtime);
    drop(ready_for_final_ack);
    drop(native_terminal);
    drop(native_factory);
    drop(staged_preflight_permit);
    drop(scheduler_preview_permit);
    operation.release_source_lease();
    let (released_sha256, durability_pins) =
        settle_reservation_release(inspection_custody, &inspection_settlement)
            .context("history reset applied; settlement pending; the next reset settles it")?;
    // The scheduler journals were the only evidence a resumed operation had
    // that its scheduler roles were retired; with the operation
    // acknowledged they are no longer needed, and a completed reset leaves
    // no scheduler state. The same locks and exclusive guard are held.
    if let Some(participant) = participant {
        participant
            .retire_acknowledged_evidence(&SchedulerAcknowledgedReset {
                state_lock,
                app_root: &config.app_root,
                runtime_directory: &runtime_directory,
                namespace_lock: &runtime_directory_lock,
                state_authority: &state_authority,
                cas_guard: &cas_guard,
                identity: &identity,
                operation: &operation,
            })
            .context(
                "history reset applied and acknowledged; scheduler evidence removal pending; \
                 the next `ryeos node reset execution-history` from a scheduling-enabled build \
                 finishes it",
            )?;
    }

    let rebuilt = authoritative.rebuilt_projection.unwrap_or_default();
    publish_progress(&mut observer, ExecutionHistoryResetPhase::Complete, None);
    let report = ExecutionHistoryResetReport {
        released_sha256,
        recovered,
        durability_pins,
        lifetime_fence_discard,
        app_root: config.app_root,
        dry_run: false,
        performed: true,
        schema_cut: options
            .schema_cut
            .map(|schema_cut| ExecutionHistorySchemaCutReport {
                from: schema_cut.from,
                to: schema_cut.to,
                observed: observed_runtime_schema_epoch,
                status: ExecutionHistorySchemaCutStatus::Completed,
            }),
        chain_heads: authoritative.chain_heads,
        project_heads,
        chain_ref_artifacts: authoritative.chain_ref_artifacts,
        pending_transitions: authoritative.pending_transitions,
        retirement_fences: authoritative.retirement_fences,
        evidence_obligations: authoritative.evidence_obligations,
        runtime_rows: completed_runtime_accounting(runtime_schema_reset_required, runtime_rows),
        thread_runtime_artifacts,
        scheduler_journal_artifacts,
        scheduler_rows: scheduler_preview,
        replay_indexes,
        projection: ProjectionDiscardReport {
            chains_rebuilt: rebuilt.chains_rebuilt,
            threads_restored: rebuilt.threads_restored,
            events_projected: rebuilt.events_projected,
            superseded_instances_deleted: authoritative.superseded_projection_instances,
        },
    };
    // The copy work and the settlement (with its ledger lock) end
    // before the claim, whose StateLock lease ends with this frame.
    release_settlement_lifelines(inspection_custody, copy_work, inspection_settlement);
    Ok(report)
}

/// Whether `error` is a final-acknowledgement refusal that no retry can
/// change: the marker is gone, names another operation, or uses a retired
/// schema.
fn permanent_acknowledgement_refusal(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<ryeos_state::ThreadHistoryDiscardAcknowledgementRefused>()
            .is_some()
            || cause
                .downcast_ref::<ryeos_state::RetiredThreadHistoryDiscardMarker>()
                .is_some()
    })
}

/// The release tail from the alias quiesce to the completion gate, shared
/// by both modes; the caller has ended its own aliases of the factory, the
/// backend terminal and the closed owners. Every refusal keeps the host and the settlement in the
/// controller, so a retry on the same custody resumes at the refused step.
fn settle_reservation_release(
    custody: &Arc<InspectionControllerCustody>,
    settlement: &Arc<OriginalInspectionSettlement>,
) -> Result<(String, DurabilityPinsReport)> {
    // End every controller and settlement alias of native owners.
    custody
        .quiesce_native_aliases()
        .context("quiesce native aliases before the host release")?;
    // Release the host; no factory, terminal or region API after this.
    let region = custody.release_native_host()?;
    settlement.retain_region_release(region)?;
    // Commit `released.<op>` from captured facts.
    settlement.commit_release()?;
    // Unlink the reservation, the disposition and the released record.
    settlement
        .unlink_release()
        .context("unlink the released reservation records")?;
    // The completion gate, over captured facts and receipts only.
    settlement
        .require_completed()
        .context("original inspection settlement is not complete")?;
    let lillux_sqlite::offline_host::DurabilityPinRelease {
        closed,
        consumed_with_error,
    } = custody.released_durability_pins()?;
    Ok((
        settlement.released_sha256()?,
        DurabilityPinsReport {
            closed,
            consumed_with_error,
        },
    ))
}

/// End the copy work and then the settlement, whose admission holds the
/// ledger lock, before the caller's claim and StateLock lease.
fn release_settlement_lifelines(
    custody: &Arc<InspectionControllerCustody>,
    copy_work: Arc<dyn lillux_sqlite::SqliteInspectionCopyWork>,
    settlement: Arc<OriginalInspectionSettlement>,
) {
    let retained = custody
        .settlement
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .take();
    drop(copy_work);
    drop(settlement);
    drop(retained);
}

fn publish_progress(
    observer: &mut Option<&mut dyn FnMut(&ExecutionHistoryResetProgress)>,
    phase: ExecutionHistoryResetPhase,
    counts: Option<(usize, usize)>,
) {
    if let Some(observer) = observer.as_deref_mut() {
        let (completed, total) = counts
            .map(|(completed, total)| (Some(completed), Some(total)))
            .unwrap_or((None, None));
        observer(&ExecutionHistoryResetProgress {
            phase,
            completed,
            total,
        });
    }
}

fn open_original_runtime_db(
    config: &Config,
    runtime_directory: &lillux::PinnedDirectory,
    runtime_directory_lock: &lillux::PinnedDirectoryLock,
    claim: &Arc<OriginalQuotaClaim>,
    factory: Arc<lillux_sqlite::OriginalNativeFactory>,
    custody: &Arc<InspectionControllerCustody>,
    fence: crate::runtime_db::UnreadableLifetimeFence,
) -> Result<RuntimeDb> {
    let parent_path = config.db_path.parent().unwrap_or_else(|| Path::new("."));
    let root = Arc::new(
        lillux::PinnedDirectory::open(parent_path)?
            .context("original Runtime parent absent; no create fallback")?,
    );
    let namespace = if runtime_directory.is_same_directory(&root)? {
        runtime_directory_lock.clone()
    } else {
        root.lock_exclusive_with_timeout(
            claim
                .require_service_window()?
                .min(std::time::Duration::from_secs(1)),
        )?
    };
    // The mutation phase pays its own reference ticket before the main open.
    let reference_inputs = claim.issue_runtime_reference_inputs(
        &factory,
        Arc::clone(&root),
        &namespace,
        &config.db_path,
    )?;
    let schema_reference = match ryeos_state::sqlite_schema::prepare_native_schema_sql_reference(
        Arc::clone(&factory),
        reference_inputs,
        lillux_sqlite::OriginalNativeDomain::NodeRuntimeSchemaReference,
        &crate::runtime_db::runtime_schema_sql(),
    ) {
        Ok(reference) => reference,
        Err(refused) => return Err(custody.retain_runtime_reference_refusal(&factory, refused)),
    };
    let inputs = match claim.issue_runtime_mutation_inputs(
        &factory,
        Arc::clone(&root),
        &namespace,
        &config.db_path,
    ) {
        Ok(inputs) => inputs,
        Err(error) => return Err(custody.retain_unused_runtime_reference(schema_reference, error)),
    };
    RuntimeDb::open_original_history_reset_mutation(
        &config.db_path,
        root,
        namespace,
        factory,
        inputs,
        schema_reference,
        fence,
    )
    .map_err(anyhow::Error::new)
    .with_context(|| {
        format!(
            "open original Runtime database {}",
            config.db_path.display()
        )
    })
}

fn open_inspection_runtime_db(
    config: &Config,
    runtime_directory: &lillux::PinnedDirectory,
    runtime_directory_lock: &lillux::PinnedDirectoryLock,
    copy_work: Arc<dyn lillux_sqlite::SqliteInspectionCopyWork>,
    factory: Arc<lillux_sqlite::OriginalNativeFactory>,
    fence: crate::runtime_db::UnreadableLifetimeFence,
) -> Result<RuntimeDb> {
    let parent_path = config.db_path.parent().unwrap_or_else(|| Path::new("."));
    let parent = lillux::PinnedDirectory::open(parent_path)?
        .context("runtime inspection parent is absent")?;
    let parent_lock = if runtime_directory.is_same_directory(&parent)? {
        runtime_directory_lock.clone()
    } else {
        parent.lock_exclusive_with_timeout(
            copy_work
                .remaining_service_window()?
                .min(std::time::Duration::from_secs(1)),
        )?
    };
    RuntimeDb::open_original_history_reset_inspection(
        &config.db_path,
        parent,
        parent_lock,
        Arc::new(runtime_directory.try_clone()?),
        copy_work,
        factory,
        fence,
    )
    .with_context(|| format!("inspect runtime database {}", config.db_path.display()))
}

/// How an operator retires scheduler state a node's composition no longer
/// selects. Every refusal of such state names this same remedy, so following
/// it from any refusal converges: the node first switches to a publisher-signed
/// profile whose composition includes the scheduling bundle (the stopped-node
/// policy-generation reset installs that bundle and grants its command
/// registration in one locked transaction; a bare bundle install would be
/// refused by the Core command-registration policy), signed schedule sources
/// are retired by the scheduler itself, fire history and scheduler journals by
/// a scheduling-enabled reset, and only then does the node switch back to its
/// scheduler-free profile, which removes the scheduling bundle.
pub const SCHEDULER_STATE_REMEDY: &str = "this node's installed composition (or this build) does not select the scheduler, so retire that state through the scheduler participant. Run steps 1-3 from a scheduling-enabled build whose --source holds the scheduling bundle (the core-scheduling package or image), with <host-class> as recorded in .ai/node/substrate-identity.json: 1) with the daemon stopped, switch to a publisher-signed profile that includes the scheduling bundle (`ryeos node reset policy-generation --node-profile core-scheduling --host-class <host-class> --source <source> --confirm` for a Core node); 2) start the daemon and deregister every signed schedule under .ai/node/schedules (`ryeos scheduler deregister <schedule_id>`); 3) stop it and run `ryeos node reset execution-history`; 4) switch back the same way to the node's own scheduler-free profile, which removes the scheduling bundle (`ryeos node reset policy-generation --node-profile core --host-class <host-class> --source <source> --confirm` for a Core node). Each switch replaces the whole node policy generation: sections changed with `ryeos node policy-apply` revert to the profile's defaults and must be re-applied. A hosted container follows the same steps from a shell, with a scheduling-enabled build and the scheduling bundle as --source; its image is not swapped";

/// Prove the node configuration namespace holds no signed schedule source.
///
/// Only an entry inside `.ai/node/schedules` is a schedule source. The
/// directory itself carries no schedule: earlier scheduling builds created it
/// unconditionally, and deregistering every schedule leaves it behind empty,
/// so an empty directory is accepted. Any entry at all, whatever its name or
/// type, is refused unread, and a `schedules` entry that is not a real
/// directory (a regular file, a symlink, anything else) is refused too, each
/// naming the remedy.
pub fn require_no_signed_schedule_sources(node: &lillux::PinnedDirectory) -> Result<()> {
    let name = std::ffi::OsStr::new("schedules");
    let not_a_directory = || {
        anyhow::anyhow!(
            "signed scheduler sources cannot be proved absent: {} is not a directory; {}",
            node.path().join(name).display(),
            SCHEDULER_STATE_REMEDY
        )
    };
    // Classify the entry without following it, so a symlink refuses with
    // the remedy rather than as an opaque open failure.
    let Some(entry) = node.entry_no_follow(name)? else {
        return Ok(());
    };
    if !matches!(entry.entry_type, lillux::PinnedEntryType::Directory) {
        return Err(not_a_directory());
    }
    let schedules = match node.open_entry(name, false)? {
        Some(lillux::PinnedDirectoryEntry::Directory(schedules)) => schedules,
        Some(lillux::PinnedDirectoryEntry::Regular(_)) | None => return Err(not_a_directory()),
    };
    if !schedules.entry_names_bounded(1)?.is_empty() {
        anyhow::bail!(
            "signed scheduler sources require their selected reset participant; {}",
            SCHEDULER_STATE_REMEDY
        );
    }
    Ok(())
}

/// Absence is proved from held namespaces, never a zero-row fallback DB.
fn require_absent_scheduler_history(
    config: &Config,
    runtime_directory: &lillux::PinnedDirectory,
) -> Result<()> {
    for name in runtime_directory.entry_names()? {
        let Some(name) = name.to_str() else {
            anyhow::bail!("cannot interpret runtime namespace without scheduler participant");
        };
        if name.starts_with("scheduler") || name == "schedules" {
            anyhow::bail!(
                "scheduler history requires its selected reset participant; {}",
                SCHEDULER_STATE_REMEDY
            );
        }
    }
    if let Some(node) = lillux::PinnedDirectory::open(&config.runtime_node_dir())? {
        require_no_signed_schedule_sources(&node)?;
    }
    Ok(())
}

fn inspect_scheduler_fire_journals(runtime_directory: &lillux::PinnedDirectory) -> Result<usize> {
    let Some(schedules) =
        runtime_directory.open_child_directory(std::ffi::OsStr::new("schedules"))?
    else {
        return Ok(0);
    };
    let mut artifacts = 0usize;
    for schedule_name in schedules.entry_names()? {
        let schedule_id = schedule_name
            .to_str()
            .context("scheduler fire directory name is not UTF-8")?;
        ryeos_scheduler_contract::validate_schedule_id(schedule_id)?;
        let schedule = schedules
            .open_child_directory(&schedule_name)?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "scheduler fire entry is not a directory: {}",
                    schedules.path().join(&schedule_name).display()
                )
            })?;
        for entry_name in schedule.entry_names()? {
            if !matches!(
                entry_name.to_str(),
                Some("fires.jsonl" | ".fires.jsonl.lock")
            ) {
                anyhow::bail!(
                    "unexpected scheduler fire-history entry: {}",
                    schedule.path().join(&entry_name).display()
                );
            }
            let file = schedule.open_regular(&entry_name, false)?.ok_or_else(|| {
                anyhow::anyhow!(
                    "scheduler fire-history entry is not a regular file: {}",
                    schedule.path().join(&entry_name).display()
                )
            })?;
            drop(file);
            artifacts = artifacts
                .checked_add(1)
                .context("scheduler fire-history artifact count overflow")?;
        }
        artifacts = artifacts
            .checked_add(1)
            .context("scheduler fire-history artifact count overflow")?;
    }
    Ok(artifacts)
}

/// Preserve the actual signed claim as well as its original issued work for
/// production-gate controls. This test-only path is not a synthetic issuer.
#[cfg(test)]
fn fixture_inspection_claim(
    directory: &lillux::PinnedDirectory,
    namespace: &lillux::PinnedDirectoryLock,
    db_target: &Path,
) -> (
    StateLock,
    Arc<OriginalQuotaClaim>,
    Arc<dyn lillux_sqlite::SqliteInspectionCopyWork>,
) {
    tests::issue_inspection_claim(directory, namespace, db_target)
}

/// Genuine signed-policy/durable-reservation fixture for sibling source controls.
/// This is not an unsigned quota or production acquisition entry point.
#[cfg(test)]
pub(crate) fn fixture_inspection_copy_work(
    directory: &lillux::PinnedDirectory,
    namespace: &lillux::PinnedDirectoryLock,
    db_target: &Path,
) -> (StateLock, Arc<dyn lillux_sqlite::SqliteInspectionCopyWork>) {
    let (lock, _claim, work) = fixture_inspection_claim(directory, namespace, db_target);
    (lock, work)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_effect_contract::EffectClass;
    use ryeos_provider_contract::{
        AdmittedLocalWorkerFinal, AdmittedLocalWorkerUsage, FirstObservation,
        LocalWorkerObservation, ObservationClass, PreparedRequestProjection, ProviderCallAnswer,
        ProviderCallRecord, RecordedMessage, RequestAuthority, RequestCoordinate,
        TransportCoordinate,
    };
    use tempfile::TempDir;

    #[cfg(target_os = "linux")]
    #[test]
    fn runtime_phase_references_join_the_native_census() {
        use lillux_sqlite::test_host::TestNativeHost;
        let host = TestNativeHost::acquire();
        let reference = || {
            // SAFETY: test-only issuance under this host's own funded factory.
            let inputs = unsafe {
                lillux_sqlite::OriginalNativeTransientInputs::issued(
                    lillux_sqlite::OriginalNativeDomain::NodeRuntimeSchemaReference,
                    host.destination(),
                )
            }
            .expect("Runtime reference inputs");
            match ryeos_state::sqlite_schema::prepare_native_schema_sql_reference(
                Arc::clone(host.factory()),
                inputs,
                lillux_sqlite::OriginalNativeDomain::NodeRuntimeSchemaReference,
                &crate::runtime_db::runtime_schema_sql(),
            ) {
                Ok(reference) => reference,
                Err(failure) => {
                    panic!("Runtime schema reference refused under the gate: {failure}")
                }
            }
        };
        let custody = InspectionControllerCustody::default();
        // The inspection phase reference after its connection closed, and a
        // mutation reference whose main inputs were refused before open.
        custody.retain_runtime_inspection_reference(
            crate::runtime_db::RuntimeInspectionReferenceRelease::Closed(reference()),
        );
        let slotted =
            custody.retain_unused_runtime_reference(reference(), anyhow!("main inputs refused"));
        assert_eq!(
            format!("{slotted}"),
            "main inputs refused",
            "an empty slot takes the owner"
        );
        // An occupied slot refuses and hands the incoming owner back.
        let refused = custody.retain_unused_runtime_reference(reference(), anyhow!("second phase"));
        let kept = match refused.downcast::<RetainedRuntimeReference>() {
            Ok(RetainedRuntimeReference {
                error: _,
                _owner: RuntimeReferenceOwner::Closed { _reference: kept },
            }) => kept,
            Ok(_) | Err(_) => panic!("occupied slot must return the incoming owner"),
        };
        host.retain_terminal(kept.terminal().clone());
        drop(kept);
        let mut terminals = Vec::new();
        custody
            .append_runtime_reference_terminals(&mut terminals)
            .unwrap();
        assert_eq!(
            terminals.len(),
            2,
            "each Runtime phase reference is one census ticket"
        );
        assert!(terminals.iter().all(|terminal| terminal.ticket().is_some()));
        for terminal in terminals {
            host.retain_terminal(terminal);
        }
        drop(custody);
        assert_eq!(host.ticket_census(), (3, 3));
    }

    fn local_coordinate() -> RequestCoordinate {
        RequestCoordinate::build(
            RequestAuthority {
                outer_effective_definition_digest: "1".repeat(64),
                provider_family: "chat_completions".to_owned(),
                provider_config_hash: "provider-config".to_owned(),
                provider_config_value_digest: "2".repeat(64),
                provider_id: "fixture-local-recorded".to_owned(),
                profile_id: None,
                model_name: "qwen3-0.6b".to_owned(),
                credential_binding_hmac: "3".repeat(64),
                credential_authority_generation: "none".to_owned(),
                authority_digest: "4".repeat(64),
                admitted_effect_class: Some(EffectClass::Recorded),
            },
            TransportCoordinate::AdmittedLocalWorker {
                worker_ref: "worker:fixtures/local-recorded".to_owned(),
                effective_definition_digest: "5".repeat(64),
                capsule_hash: "6".repeat(64),
                execution_realization_hash: "7".repeat(64),
            },
            PreparedRequestProjection::new(
                std::iter::empty(),
                std::iter::empty(),
                "8".repeat(64),
                16,
            )
            .unwrap(),
        )
        .unwrap()
    }

    fn local_terminal() -> AdmittedLocalWorkerFinal {
        AdmittedLocalWorkerFinal {
            answer: ProviderCallAnswer {
                message: RecordedMessage {
                    role: "assistant".to_owned(),
                    content: Some(serde_json::Value::String("done".to_owned())),
                    tool_calls: None,
                    tool_call_id: None,
                    reasoning_content: None,
                },
                finish_reason: Some("stop".to_owned()),
            },
            usage: AdmittedLocalWorkerUsage {
                input_tokens: 5,
                output_tokens: 1,
                reasoning_tokens: None,
            },
            response_id: Some("response-1".to_owned()),
        }
    }

    pub(super) fn install_offline_policy(
        config: &Config,
        identity: &ryeos_node_runtime::identity::NodeIdentity,
        enabled: bool,
    ) {
        use ryeos_node_runtime::node_policy::generation::{
            NodeInitProfile, NodePolicyUpdate, publish_policy_update,
        };
        let source = ryeos_engine::test_support::workspace_root()
            .join("bundles/.ai/node/init/profiles/core.yaml");
        let raw = std::fs::read_to_string(&source).unwrap();
        let mut profile: NodeInitProfile =
            serde_yaml::from_str(&lillux::signature::strip_signature_lines(&raw)).unwrap();
        if !enabled {
            profile.policies.get_mut("maintenance").unwrap()["offline_reset"] =
                serde_json::Value::Null;
        }
        let generation = profile
            .for_host_class("local")
            .unwrap()
            .validated_generation(
                &ryeos_node_runtime::node_policy::NodePolicyTable::new(),
                &source,
            )
            .unwrap();
        let trust = ryeos_engine::trust::TrustStore::from_signers(vec![
            ryeos_engine::trust::TrustedSigner {
                fingerprint: identity.fingerprint().to_owned(),
                verifying_key: *identity.verifying_key(),
                label: None,
            },
        ]);
        let lock = StateLock::acquire(&default_lock_path(&config.app_root)).unwrap();
        publish_policy_update(
            &config.app_root,
            &NodePolicyUpdate::initial(generation),
            identity,
            &trust,
            &lock,
        )
        .unwrap();
    }

    /// Names the app root of the failed acquisition that
    /// [`failed_acquisition_attempt_child`] runs in its own process.
    const FAILED_ACQUISITION_ROOT: &str = "RYEOS_TEST_FAILED_ACQUISITION_ROOT";

    /// The failed attempt of
    /// [`real_reset_retains_published_claim_when_runtime_inspection_cannot_open`],
    /// run in a child process; without its root variable it does nothing.
    #[test]
    fn failed_acquisition_attempt_child() {
        use super::original_quota_claim::QuotaCustodyFailure;
        let Some(root) = std::env::var_os(FAILED_ACQUISITION_ROOT) else {
            return;
        };
        let root = PathBuf::from(root);
        let config = Config::load(&ConfigSources {
            app_root: Some(root.clone()),
            ..Default::default()
        })
        .unwrap();
        let _serial = lillux_sqlite::test_host::serial();
        let error = run_execution_history_reset(
            &ExecutionHistoryResetOptions {
                app_root: Some(root.clone()),
                dry_run: true,
                ..Default::default()
            },
            None,
        )
        .unwrap_err();
        // The controller's failure wraps the quota custody it retains.
        let custody = error
            .chain()
            .find_map(|cause| cause.downcast_ref::<QuotaCustodyFailure>())
            .unwrap_or_else(|| panic!("reset refusal retains its quota custody: {error:#}"));
        assert_eq!(custody.phase(), QuotaFailurePhase::Acquisition);
        let path = config
            .app_root
            .join(".ai/node/offline-reset-quota")
            .join(format!("reserved.{}", custody.claim().operation_id()));
        assert!(
            path.exists(),
            "the failed acquisition's reservation is published"
        );
        assert!(
            !config.db_path.exists(),
            "failed inspection cannot create the absent original"
        );
        // The error retains the ORIGINAL claim, and a live claim holds the
        // StateLock: no other stopped-node frame can open meanwhile.
        assert!(StateLock::acquire(&default_lock_path(&root)).is_err());
    }

    /// A dry run whose Runtime inspection cannot open fails in Acquisition
    /// with its published claim retained. The attempt runs in its own
    /// process, as each `ryeos node reset` does: its retained custody ends
    /// only with that process, and ending it refunds nothing.
    #[test]
    fn real_reset_retains_published_claim_when_runtime_inspection_cannot_open() {
        let tmp = TempDir::new().unwrap();
        let config = Config::load(&ConfigSources {
            app_root: Some(tmp.path().to_owned()),
            ..Default::default()
        })
        .unwrap();
        let identity =
            ryeos_node_runtime::identity::NodeIdentity::create(&config.node_signing_key_path)
                .unwrap();
        install_offline_policy(&config, &identity, true);
        crate::runtime_db::test_native::seed_node(&config, false);
        assert!(!config.db_path.exists());
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "execution_history_reset::tests::failed_acquisition_attempt_child",
                "--nocapture",
            ])
            .env(FAILED_ACQUISITION_ROOT, tmp.path())
            .output()
            .unwrap();
        assert!(
            child.status.success(),
            "failed acquisition child failed: {}{}",
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr)
        );
        let ledger = config.app_root.join(".ai/node/offline-reset-quota");
        let reserved: Vec<_> = std::fs::read_dir(&ledger)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("reserved.")
            })
            .collect();
        assert_eq!(reserved.len(), 1, "{reserved:?}");
        let original = std::fs::read(&reserved[0]).unwrap();
        assert!(!config.db_path.exists());

        let state_lock = StateLock::acquire(&default_lock_path(tmp.path())).unwrap();
        let runtime = lillux::PinnedDirectory::open(&config.runtime_state_dir())
            .unwrap()
            .unwrap();
        let namespace = runtime.lock_exclusive().unwrap();
        let policy =
            OfflineResetPolicyOwner::load(&config, &identity, &state_lock, &runtime, &namespace)
                .unwrap();
        let replacement = OriginalQuotaClaim::prepare(
            &policy,
            &identity,
            None,
            inspection_destination::OriginalInspectionMode::DryRun,
            false,
        )
        .unwrap();
        assert!(
            replacement.publish(&policy, &identity).is_err(),
            "failed acquisition cannot refund its charge"
        );
        assert_eq!(std::fs::read(&reserved[0]).unwrap(), original);
    }

    #[test]
    fn original_quota_reservation_retains_exact_owner_and_refuses_overcommit() {
        use super::original_quota_claim::{OriginalQuotaClaim, QuotaCustodyFailure};
        let tmp = TempDir::new().unwrap();
        let config = Config::load(&ConfigSources {
            app_root: Some(tmp.path().to_owned()),
            ..Default::default()
        })
        .unwrap();
        let identity =
            ryeos_node_runtime::identity::NodeIdentity::create(&config.node_signing_key_path)
                .unwrap();
        crate::runtime_db::test_native::seed_node(&config, false);
        install_offline_policy(&config, &identity, true);
        let state_lock = StateLock::acquire(&default_lock_path(tmp.path())).unwrap();
        let runtime = lillux::PinnedDirectory::open(&config.runtime_state_dir())
            .unwrap()
            .unwrap();
        let namespace = runtime.lock_exclusive().unwrap();
        let policy =
            OfflineResetPolicyOwner::load(&config, &identity, &state_lock, &runtime, &namespace)
                .unwrap();
        // A refused issuance drops its production permit and drains its
        // claim, by design. The deliberately refused attempts therefore run
        // on a probe claim of a separate node; the successful issuances
        // below run on a fresh claim.
        {
            let probe_tmp = TempDir::new().unwrap();
            let probe_config = Config::load(&ConfigSources {
                app_root: Some(probe_tmp.path().to_owned()),
                ..Default::default()
            })
            .unwrap();
            let probe_identity = ryeos_node_runtime::identity::NodeIdentity::create(
                &probe_config.node_signing_key_path,
            )
            .unwrap();
            crate::runtime_db::test_native::seed_node(&probe_config, false);
            install_offline_policy(&probe_config, &probe_identity, true);
            let probe_lock = StateLock::acquire(&default_lock_path(probe_tmp.path())).unwrap();
            let probe_runtime = lillux::PinnedDirectory::open(&probe_config.runtime_state_dir())
                .unwrap()
                .unwrap();
            let probe_namespace = probe_runtime.lock_exclusive().unwrap();
            let probe_policy = OfflineResetPolicyOwner::load(
                &probe_config,
                &probe_identity,
                &probe_lock,
                &probe_runtime,
                &probe_namespace,
            )
            .unwrap();
            let probe = OriginalQuotaClaim::prepare(
                &probe_policy,
                &probe_identity,
                None,
                inspection_destination::OriginalInspectionMode::DryRun,
                false,
            )
            .unwrap();
            assert!(
                probe
                    .issue_copy_work(&probe_policy, &probe_identity)
                    .is_err(),
                "prepared policy/record is not an issued copy owner"
            );
            super::original_quota_claim::fail_next_publication_ack();
            let uncertain = probe.publish(&probe_policy, &probe_identity).unwrap_err();
            assert!(
                uncertain
                    .downcast_ref::<QuotaCustodyFailure>()
                    .unwrap()
                    .retains(&probe)
            );
            probe
                .require_published(&probe_policy, &probe_identity)
                .unwrap();
            assert!(
                probe
                    .issue_copy_work(&probe_policy, &probe_identity)
                    .is_err(),
                "uncertain publication bytes cannot substitute for original acknowledgement"
            );
        }
        let first = OriginalQuotaClaim::prepare(
            &policy,
            &identity,
            None,
            inspection_destination::OriginalInspectionMode::DryRun,
            false,
        )
        .unwrap();
        let ledger_path = config.app_root.join(".ai/node/offline-reset-quota");
        let first_path = ledger_path.join(format!("reserved.{}", first.operation_id()));
        first.publish(&policy, &identity).unwrap();
        let exact_signed = std::fs::read(&first_path).unwrap();
        let work_one = first.issue_copy_work(&policy, &identity).unwrap();
        let reissued = match first.issue_copy_work(&policy, &identity) {
            Ok(_) => panic!("a second issued view could reset original role charges"),
            Err(error) => error,
        };
        assert!(
            format!("{reissued:#}").contains("issuer already entered"),
            "{reissued:#}"
        );
        let absent = std::ffi::OsStr::new("scheduler.sqlite3");
        let destination = work_one
            .clone()
            .create_destination(&runtime, absent)
            .unwrap();
        assert!(
            lillux_sqlite::copy_regular_without_sqlite_alias(
                &runtime,
                absent,
                destination.root.as_ref(),
                0,
                work_one.as_ref()
            )
            .unwrap()
            .is_none()
        );
        drop(work_one);
        let unknown_copy = config
            .app_root
            .join(".ai/node/offline-reset-inspections/unclassified");
        std::fs::create_dir(&unknown_copy).unwrap();
        let blocked = first.publish(&policy, &identity).unwrap_err();
        assert!(
            format!("{blocked:#}").contains("`unclassified`"),
            "{blocked:#}"
        );
        assert!(unknown_copy.is_dir());
        assert_eq!(std::fs::read(&first_path).unwrap(), exact_signed);
        std::fs::remove_dir(&unknown_copy).unwrap(); // only this test's injected empty object
        drop(blocked);
        assert_eq!(std::fs::read(&first_path).unwrap(), exact_signed);
        let second = OriginalQuotaClaim::prepare(
            &policy,
            &identity,
            None,
            inspection_destination::OriginalInspectionMode::DryRun,
            false,
        )
        .unwrap();
        assert!(second.require_published(&policy, &identity).is_err());
        let error = second.publish(&policy, &identity).unwrap_err();
        let retained = error.downcast_ref::<QuotaCustodyFailure>().unwrap();
        assert!(retained.retains(&second));
        assert!(
            !ledger_path
                .join(format!("reserved.{}", second.operation_id()))
                .exists()
        );
        assert_eq!(std::fs::read(&first_path).unwrap(), exact_signed);
        drop(error);
        assert_eq!(
            std::fs::read(&first_path).unwrap(),
            exact_signed,
            "Drop cannot refund a retained reservation"
        );
        // Foreign/unclassified partial deposits keep admission held. Never
        // remove them or treat their absence of a valid signature as zero debit.
        std::fs::write(ledger_path.join("unclassified-partial"), b"retained").unwrap();
        assert!(second.publish(&policy, &identity).is_err());
        assert!(ledger_path.join("unclassified-partial").exists());
        drop(second);
        // Beside its own reservation, any other ledger name refuses the
        // retried publication, naming the repair, and is kept.
        for index in 0..63 {
            std::fs::write(
                ledger_path.join(format!("held.partial.{index:02}")),
                b"retained",
            )
            .unwrap();
        }
        let foreign = first.publish(&policy, &identity).unwrap_err();
        assert!(
            format!("{foreign:#}").contains(ryeos_state::EXECUTION_HISTORY_SCRATCH_REPAIR),
            "{foreign:#}"
        );
        assert!(
            foreign
                .downcast_ref::<QuotaCustodyFailure>()
                .unwrap()
                .retains(&first)
        );
        assert_eq!(std::fs::read_dir(&ledger_path).unwrap().count(), 65);
        drop(foreign);
        drop(first);
        assert_eq!(std::fs::read(&first_path).unwrap(), exact_signed);
        assert!(
            !config.db_path.exists(),
            "reservation alone must not acquire SQL/copy resources"
        );
    }

    #[test]
    fn offline_policy_keeps_actual_signed_generation_and_original_root_bindings() {
        let tmp = TempDir::new().unwrap();
        let config = Config::load(&ConfigSources {
            app_root: Some(tmp.path().join("node")),
            ..Default::default()
        })
        .unwrap();
        let identity =
            ryeos_node_runtime::identity::NodeIdentity::create(&config.node_signing_key_path)
                .unwrap();
        std::fs::create_dir_all(config.runtime_state_dir()).unwrap();
        install_offline_policy(&config, &identity, true);
        let state_lock = StateLock::acquire(&default_lock_path(&config.app_root)).unwrap();
        let runtime = lillux::PinnedDirectory::open(&config.runtime_state_dir())
            .unwrap()
            .unwrap();
        let namespace = runtime.lock_exclusive().unwrap();
        let owner =
            OfflineResetPolicyOwner::load(&config, &identity, &state_lock, &runtime, &namespace)
                .unwrap();
        assert_eq!(owner.signer_fingerprint().unwrap(), identity.fingerprint());
        assert_eq!(owner.generation_digest().unwrap().len(), 64);
        assert_eq!(
            owner.quota().unwrap().max_source_roles,
            ryeos_scheduler_contract::OFFLINE_RESET_SOURCE_ROLES
        );
        assert!(
            !config.db_path.exists(),
            "policy capture must not acquire a database"
        );
        let other = tmp.path().join("other");
        std::fs::create_dir_all(other.join(".ai/node")).unwrap();
        let wrong_lock = StateLock::acquire(&default_lock_path(&other)).unwrap();
        assert!(
            OfflineResetPolicyOwner::load(&config, &identity, &wrong_lock, &runtime, &namespace)
                .is_err()
        );
        let original_digest = owner.generation_digest().unwrap().to_owned();
        let original_quota = owner.quota().unwrap().clone();
        // A corrupt later policy pathname cannot replace the retained, already
        // authenticated snapshot. This still grants no acquisition permission.
        std::fs::write(
            ryeos_node_runtime::node_policy::generation::policy_directory(&config.app_root)
                .join("maintenance.yaml"),
            "schema: 2\noffline_reset: null\nschedules: []\n",
        )
        .unwrap();
        assert!(
            OfflineResetPolicyOwner::load(&config, &identity, &state_lock, &runtime, &namespace)
                .is_err()
        );
        assert_eq!(owner.generation_digest().unwrap(), original_digest);
        assert_eq!(owner.quota().unwrap(), &original_quota);
        std::fs::rename(&config.app_root, tmp.path().join("original-node")).unwrap();
        std::fs::create_dir_all(&config.app_root).unwrap();
        assert!(owner.require_held().is_err());
        assert!(owner.quota().is_err());
    }

    #[test]
    fn reset_requires_real_signed_enabled_policy_before_database_acquisition() {
        for disabled in [false, true] {
            let tmp = TempDir::new().unwrap();
            let config = Config::load(&ConfigSources {
                app_root: Some(tmp.path().to_owned()),
                ..Default::default()
            })
            .unwrap();
            let identity =
                ryeos_node_runtime::identity::NodeIdentity::create(&config.node_signing_key_path)
                    .unwrap();
            std::fs::create_dir_all(config.runtime_state_dir()).unwrap();
            if disabled {
                install_offline_policy(&config, &identity, false);
            } else {
                drop(StateLock::acquire(&default_lock_path(tmp.path())).unwrap());
            }
            let runtime_names = || {
                let mut names = std::fs::read_dir(config.runtime_state_dir())
                    .unwrap()
                    .map(|entry| entry.unwrap().file_name())
                    .collect::<Vec<_>>();
                names.sort();
                names
            };
            let original_names = runtime_names();
            for dry_run in [false, true] {
                let error = run_execution_history_reset(
                    &ExecutionHistoryResetOptions {
                        app_root: Some(tmp.path().to_owned()),
                        dry_run,
                        ..Default::default()
                    },
                    None,
                )
                .unwrap_err();
                let error = format!("{error:#}");
                assert!(
                    error.contains(if disabled {
                        "disables offline"
                    } else {
                        "policy generation"
                    }),
                    "{error}"
                );
                assert!(
                    !config.db_path.exists(),
                    "refusal must precede Runtime SQL/copy"
                );
                assert_eq!(
                    runtime_names(),
                    original_names,
                    "refusal must preserve original operator.lock without creating State SQL/effect records"
                );
            }
        }
    }

    fn write_object(cas_root: &Path, value: &serde_json::Value) -> String {
        let canonical = lillux::canonical_json(value).unwrap();
        let hash = lillux::sha256_hex(canonical.as_bytes());
        let path = lillux::shard_path(cas_root, "objects", &hash, ".json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        lillux::atomic_write(&path, canonical.as_bytes()).unwrap();
        hash
    }

    #[test]
    fn incompatible_runtime_accounting_is_explicitly_unavailable_not_zero() {
        let accounting = RuntimeThreadHistoryAccounting::UnavailableIncompatibleSchema;
        assert_eq!(accounting.total_rows(), None);
        assert_eq!(
            serde_json::to_value(accounting).unwrap(),
            serde_json::json!({"status": "unavailable_incompatible_schema"})
        );
    }

    #[test]
    fn current_runtime_accounting_reports_exact_counts() {
        let accounting = RuntimeThreadHistoryAccounting::Exact {
            counts: RuntimeThreadHistoryDiscardReport {
                thread_runtime: 3,
                follow_waiters: 2,
                ..RuntimeThreadHistoryDiscardReport::default()
            },
        };
        assert_eq!(accounting.total_rows(), Some(5));
        assert_eq!(serde_json::to_value(accounting).unwrap()["status"], "exact");
    }

    #[test]
    fn execution_history_reset_closure_knows_provider_records_and_local_observations() {
        crate::object_contracts::install().unwrap();
        let coordinate = local_coordinate();
        let terminal = local_terminal();
        let observation = LocalWorkerObservation {
            schema: ryeos_provider_contract::LOCAL_WORKER_OBSERVATION_SCHEMA_VERSION,
            kind: ryeos_provider_contract::LOCAL_WORKER_OBSERVATION_KIND.to_owned(),
            attempt_id: "attempt-1".to_owned(),
            request_hash: "d".repeat(64),
            coordinate_key: coordinate.cache_key().unwrap(),
            capsule_hash: "6".repeat(64),
            admitted_execution_realization_hash: "7".repeat(64),
            observed_execution_realization_hash: None,
            observed_at: "2026-08-09T00:00:00.000Z".to_owned(),
            terminal_digest: terminal.digest().unwrap(),
            terminal: terminal.clone(),
            execution_identity_digest: "9".repeat(64),
            execution_identity_attestation_hash: "a".repeat(64),
        };
        let answer_digest = terminal.answer.digest().unwrap();
        let record = ProviderCallRecord {
            schema: ryeos_provider_contract::PROVIDER_CALL_RECORD_SCHEMA_VERSION,
            kind: ryeos_provider_contract::PROVIDER_CALL_RECORD_KIND.to_owned(),
            cache_key: coordinate.cache_key().unwrap(),
            coordinate,
            answer_digest: answer_digest.clone(),
            answer: terminal.answer,
            first_observation: FirstObservation {
                produced_by_thread: "T-local".to_owned(),
                attempt_id: "attempt-1".to_owned(),
                response_digest: answer_digest,
                observed_at: "2026-08-09T00:00:00.000Z".to_owned(),
                observation_class: ObservationClass::DaemonWorkerObserved,
                provider_accounting: serde_json::json!({
                    "input_tokens": 5,
                    "output_tokens": 1,
                }),
                execution_identity_digest: Some("9".repeat(64)),
                execution_identity_attestation_hash: Some("a".repeat(64)),
                admitted_execution_realization_hash: Some("7".repeat(64)),
                observed_execution_realization_hash: None,
            },
        };

        let temp = TempDir::new().unwrap();
        let cas_root = temp.path().join("cas");
        let observation_hash = write_object(&cas_root, &observation.to_value().unwrap());
        let record_hash = write_object(&cas_root, &record.to_value().unwrap());
        let report = ryeos_state::object_closure::collect_object_closure(
            &cas_root,
            [observation_hash.clone(), record_hash.clone()],
        )
        .unwrap();

        assert!(report.unsupported_objects.is_empty(), "{report:?}");
        assert!(report.malformed_objects.is_empty(), "{report:?}");
        assert!(report.object_hashes.contains(&observation_hash));
        assert!(report.object_hashes.contains(&record_hash));
        let missing = report
            .missing_objects
            .iter()
            .map(|entry| entry.hash.clone())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            missing,
            std::collections::BTreeSet::from(["6".repeat(64), "7".repeat(64), "a".repeat(64),])
        );
    }

    #[test]
    fn reset_acknowledges_scheduler_before_removing_cross_store_marker() {
        struct Participant {
            completed: std::sync::atomic::AtomicBool,
            retired: std::sync::atomic::AtomicBool,
            preflights: std::sync::atomic::AtomicUsize,
        }
        struct Attempt;
        impl SchedulerHistoryRetirementAttempt for Attempt {
            fn advance(
                &mut self,
                permit: &SchedulerResetRetirement<'_, '_>,
            ) -> Result<Option<SchedulerHistoryRetirement>> {
                permit.runtime_directory()?;
                Ok(Some(SchedulerHistoryRetirement {
                    journal_artifacts: 0,
                    rows: ryeos_scheduler_contract::SchedulerFireHistoryDiscardReport::default(),
                    generation: None,
                }))
            }
        }
        impl SchedulerHistoryResetParticipant for Participant {
            fn preflight(&self, permit: &SchedulerResetPreview<'_>) -> Result<()> {
                let runtime = permit.runtime_directory()?;
                let recovery =
                    ryeos_state::RecoveryStore::from_refs_root(&runtime.path().join("refs"))?;
                assert!(
                    !recovery.thread_history_discard_in_progress()?,
                    "preflight must precede State activation"
                );
                let call = self
                    .preflights
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                assert!(call < 2);
                let staged = runtime
                    .entry_names_bounded(4096)?
                    .iter()
                    .filter(|name| {
                        name.to_string_lossy()
                            .starts_with("execution-history-reset.operation.")
                    })
                    .count();
                assert_eq!(
                    staged, call,
                    "second preflight must count the staged operation"
                );
                Ok(())
            }
            fn preview(
                &self,
                _permit: &SchedulerResetPreview<'_>,
            ) -> Result<ryeos_scheduler_contract::SchedulerFireHistoryAccounting> {
                assert_eq!(self.preflights.load(std::sync::atomic::Ordering::SeqCst), 1);
                Ok(
                    ryeos_scheduler_contract::SchedulerFireHistoryAccounting::Exact {
                        counts: Default::default(),
                    },
                )
            }
            fn begin_retirement<'a>(
                &self,
                permit: &'a SchedulerResetRetirement<'_, '_>,
            ) -> Result<Box<dyn SchedulerHistoryRetirementAttempt + 'a>> {
                permit.runtime_directory()?;
                assert_eq!(self.preflights.load(std::sync::atomic::Ordering::SeqCst), 2);
                Ok(Box::new(Attempt))
            }
            fn complete(
                &self,
                permit: &SchedulerResetRetirement<'_, '_>,
                _retirement: &SchedulerHistoryRetirement,
            ) -> Result<usize> {
                let recovery = ryeos_state::RecoveryStore::from_refs_root(
                    &permit.runtime_directory()?.path().join("refs"),
                )?;
                // The reset holds its projection leases here, so the marker
                // is read in the scope of the permit's native factory.
                let factory = permit
                    .native_factory()?
                    .context("reset permit native factory")?;
                assert!(
                    recovery
                        .thread_history_discard_operation_scoped(factory)?
                        .is_some(),
                    "cross-store marker must remain Active through terminal scheduler acknowledgement"
                );
                self.completed
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(0)
            }
            fn retire_acknowledged_evidence(
                &self,
                permit: &SchedulerAcknowledgedReset<'_>,
            ) -> Result<usize> {
                let recovery = ryeos_state::RecoveryStore::from_refs_root(
                    &permit.runtime_directory()?.path().join("refs"),
                )?;
                assert!(
                    self.completed.load(std::sync::atomic::Ordering::SeqCst),
                    "evidence retirement must follow terminal scheduler acknowledgement"
                );
                assert!(
                    !recovery.thread_history_discard_in_progress()?
                        && recovery
                            .last_acknowledged_thread_history_discard()?
                            .is_some(),
                    "evidence retirement must follow the State acknowledgement"
                );
                self.retired
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(0)
            }
        }
        let tmp = TempDir::new().unwrap();
        let config = Config::load(&ConfigSources {
            app_root: Some(tmp.path().to_owned()),
            ..Default::default()
        })
        .unwrap();
        let identity =
            ryeos_node_runtime::identity::NodeIdentity::create(&config.node_signing_key_path)
                .unwrap();
        install_offline_policy(&config, &identity, true);
        crate::runtime_db::test_native::seed_node(&config, true);
        drop(StateLock::acquire(&default_lock_path(tmp.path())).unwrap());
        let _serial = lillux_sqlite::test_host::serial();
        let participant = Participant {
            completed: std::sync::atomic::AtomicBool::new(false),
            retired: std::sync::atomic::AtomicBool::new(false),
            preflights: std::sync::atomic::AtomicUsize::new(0),
        };
        let report = run_execution_history_reset(
            &ExecutionHistoryResetOptions {
                app_root: Some(tmp.path().to_owned()),
                ..Default::default()
            },
            Some(&participant),
        )
        .unwrap();
        assert!(report.performed);
        assert!(
            participant
                .completed
                .load(std::sync::atomic::Ordering::SeqCst)
        );
        assert!(
            participant
                .retired
                .load(std::sync::atomic::Ordering::SeqCst),
            "a completed reset must retire the participant's acknowledged evidence"
        );
        let recovery =
            ryeos_state::RecoveryStore::from_runtime_state_dir(&config.runtime_state_dir())
                .unwrap();
        assert!(!recovery.thread_history_discard_in_progress().unwrap());
        assert!(
            recovery
                .last_acknowledged_thread_history_discard()
                .unwrap()
                .is_some()
        );
    }

    /// Scheduler participant whose terminal completion refuses while
    /// `refuse` is set.
    struct RefusingParticipant {
        refuse: bool,
    }
    struct CompletedAttempt;
    impl SchedulerHistoryRetirementAttempt for CompletedAttempt {
        fn advance(
            &mut self,
            permit: &SchedulerResetRetirement<'_, '_>,
        ) -> Result<Option<SchedulerHistoryRetirement>> {
            permit.runtime_directory()?;
            Ok(Some(SchedulerHistoryRetirement {
                journal_artifacts: 0,
                rows: ryeos_scheduler_contract::SchedulerFireHistoryDiscardReport::default(),
                generation: None,
            }))
        }
    }
    impl SchedulerHistoryResetParticipant for RefusingParticipant {
        fn preflight(&self, permit: &SchedulerResetPreview<'_>) -> Result<()> {
            permit.runtime_directory().map(|_| ())
        }
        fn preview(
            &self,
            _permit: &SchedulerResetPreview<'_>,
        ) -> Result<ryeos_scheduler_contract::SchedulerFireHistoryAccounting> {
            Ok(
                ryeos_scheduler_contract::SchedulerFireHistoryAccounting::Exact {
                    counts: Default::default(),
                },
            )
        }
        fn begin_retirement<'a>(
            &self,
            permit: &'a SchedulerResetRetirement<'_, '_>,
        ) -> Result<Box<dyn SchedulerHistoryRetirementAttempt + 'a>> {
            permit.runtime_directory()?;
            Ok(Box::new(CompletedAttempt))
        }
        fn complete(
            &self,
            permit: &SchedulerResetRetirement<'_, '_>,
            _retirement: &SchedulerHistoryRetirement,
        ) -> Result<usize> {
            permit.runtime_directory()?;
            anyhow::ensure!(!self.refuse, "scheduler terminal journal refused");
            Ok(0)
        }
        fn retire_acknowledged_evidence(
            &self,
            permit: &SchedulerAcknowledgedReset<'_>,
        ) -> Result<usize> {
            permit.runtime_directory()?;
            Ok(0)
        }
    }

    /// Names the app root of the refused attempt that
    /// [`scheduler_refused_attempt_child`] runs in its own process.
    const REFUSED_ATTEMPT_ROOT: &str = "RYEOS_TEST_REFUSED_RESET_ROOT";

    /// The refused attempt of
    /// [`scheduler_refusal_past_its_window_leaves_a_settlement_the_next_reset_resumes`],
    /// run in a child process; without its root variable it does nothing.
    #[test]
    fn scheduler_refused_attempt_child() {
        let Some(root) = std::env::var_os(REFUSED_ATTEMPT_ROOT) else {
            return;
        };
        let _serial = lillux_sqlite::test_host::serial();
        let options = ExecutionHistoryResetOptions {
            app_root: Some(PathBuf::from(root)),
            ..Default::default()
        };
        let error =
            run_execution_history_reset(&options, Some(&RefusingParticipant { refuse: true }))
                .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("settlement pending"), "{message}");
        assert!(
            message.contains("the next `ryeos node reset execution-history` recovers it"),
            "{message}"
        );
        assert!(
            message.contains("scheduler terminal journal refused"),
            "{message}"
        );
    }

    /// A scheduler stage refused past its window ends the reset with the
    /// settlement pending: State still marks the operation Active, the
    /// attempt's reservation stays, and the next reset settles that stale
    /// attempt as resumed and completes the operation. The refused attempt
    /// runs in its own process, as each `ryeos node reset` does: its
    /// retained custody, including the StateLock and the native lease
    /// registration, ends only with that process.
    #[test]
    fn scheduler_refusal_past_its_window_leaves_a_settlement_the_next_reset_resumes() {
        let tmp = TempDir::new().unwrap();
        let config = Config::load(&ConfigSources {
            app_root: Some(tmp.path().to_owned()),
            ..Default::default()
        })
        .unwrap();
        let identity =
            ryeos_node_runtime::identity::NodeIdentity::create(&config.node_signing_key_path)
                .unwrap();
        install_offline_policy(&config, &identity, true);
        crate::runtime_db::test_native::seed_node(&config, true);
        drop(StateLock::acquire(&default_lock_path(tmp.path())).unwrap());

        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "execution_history_reset::tests::scheduler_refused_attempt_child",
                "--nocapture",
            ])
            .env(REFUSED_ATTEMPT_ROOT, tmp.path())
            .output()
            .unwrap();
        assert!(
            child.status.success(),
            "refused attempt child failed: {}{}",
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr)
        );

        let _serial = lillux_sqlite::test_host::serial();
        let recovery =
            ryeos_state::RecoveryStore::from_runtime_state_dir(&config.runtime_state_dir())
                .unwrap();
        let active = recovery
            .thread_history_discard_operation()
            .unwrap()
            .expect("the refused operation stays Active");
        let ledger = config
            .app_root
            .join(".ai/node")
            .join(stale_recovery::LEDGER_DIRECTORY);
        assert!(
            ledger
                .join(format!("reserved.{}", active.operation_id))
                .exists(),
            "the refused attempt's reservation stays for recovery"
        );

        let options = ExecutionHistoryResetOptions {
            app_root: Some(tmp.path().to_owned()),
            ..Default::default()
        };
        let report =
            run_execution_history_reset(&options, Some(&RefusingParticipant { refuse: false }))
                .unwrap();
        assert!(report.performed);
        let resumed: Vec<_> = report
            .recovered
            .iter()
            .filter(|recovered| recovered.operation_id == active.operation_id)
            .collect();
        assert_eq!(resumed.len(), 1, "{:?}", report.recovered);
        assert_eq!(resumed[0].outcome, RecoveredReleaseOutcome::Abandoned);
        assert_eq!(resumed[0].applied, Some(RecoveredApplied::Resumed));
        let recovery =
            ryeos_state::RecoveryStore::from_runtime_state_dir(&config.runtime_state_dir())
                .unwrap();
        assert!(!recovery.thread_history_discard_in_progress().unwrap());
        assert_eq!(
            recovery
                .last_acknowledged_thread_history_discard()
                .unwrap()
                .map(|acknowledged| acknowledged.operation_id),
            Some(active.operation_id)
        );
    }

    #[test]
    fn confirmed_schema_reset_never_reports_post_reset_zero_as_exact() {
        let accounting =
            completed_runtime_accounting(true, RuntimeThreadHistoryDiscardReport::default());
        assert!(matches!(
            accounting,
            RuntimeThreadHistoryAccounting::UnavailableIncompatibleSchema
        ));
    }

    #[test]
    fn absent_scheduler_participant_refuses_artifacts_without_mutation() {
        let tmp = TempDir::new().unwrap();
        let config = Config::load(&ConfigSources {
            app_root: Some(tmp.path().to_owned()),
            ..Default::default()
        })
        .unwrap();
        std::fs::create_dir_all(config.runtime_state_dir()).unwrap();
        let directory = lillux::PinnedDirectory::open(&config.runtime_state_dir())
            .unwrap()
            .unwrap();
        require_absent_scheduler_history(&config, &directory).unwrap();
        let file = config.runtime_state_dir().join("scheduler.sqlite3");
        std::fs::write(&file, b"unknown schema").unwrap();
        let refusal = require_absent_scheduler_history(&config, &directory)
            .unwrap_err()
            .to_string();
        assert!(refusal.contains(SCHEDULER_STATE_REMEDY), "{refusal}");
        assert_eq!(std::fs::read(file).unwrap(), b"unknown schema");
    }

    #[test]
    fn absent_scheduler_participant_accepts_only_an_empty_node_schedules_directory() {
        let tmp = TempDir::new().unwrap();
        let config = Config::load(&ConfigSources {
            app_root: Some(tmp.path().to_owned()),
            ..Default::default()
        })
        .unwrap();
        std::fs::create_dir_all(config.runtime_state_dir()).unwrap();
        let directory = lillux::PinnedDirectory::open(&config.runtime_state_dir())
            .unwrap()
            .unwrap();
        let schedules = config.runtime_node_dir().join("schedules");
        std::fs::create_dir_all(&schedules).unwrap();
        require_absent_scheduler_history(&config, &directory).unwrap();

        let source = schedules.join("nightly.yaml");
        std::fs::write(&source, b"signed schedule").unwrap();
        let refusal = require_absent_scheduler_history(&config, &directory)
            .unwrap_err()
            .to_string();
        assert!(refusal.contains(SCHEDULER_STATE_REMEDY), "{refusal}");
        assert_eq!(std::fs::read(&source).unwrap(), b"signed schedule");

        std::fs::remove_file(&source).unwrap();
        std::fs::remove_dir(&schedules).unwrap();
        std::fs::write(&schedules, b"not a directory").unwrap();
        let refusal = require_absent_scheduler_history(&config, &directory)
            .unwrap_err()
            .to_string();
        assert!(refusal.contains(SCHEDULER_STATE_REMEDY), "{refusal}");

        // A symlink, even to an empty directory, is not the schedules
        // directory and refuses with the remedy without being followed.
        std::fs::remove_file(&schedules).unwrap();
        let target = tmp.path().join("elsewhere");
        std::fs::create_dir(&target).unwrap();
        lillux::PinnedDirectory::open(&config.runtime_node_dir())
            .unwrap()
            .unwrap()
            .create_symlink(
                std::ffi::OsStr::new("schedules"),
                target.as_os_str().as_encoded_bytes(),
            )
            .unwrap();
        let refusal = require_absent_scheduler_history(&config, &directory)
            .unwrap_err()
            .to_string();
        assert!(refusal.contains(SCHEDULER_STATE_REMEDY), "{refusal}");
    }
    /// Every entry of the State recovery tree beneath `root`, sorted.
    fn recovery_entries(root: &Path) -> Vec<PathBuf> {
        let mut entries = Vec::new();
        let mut pending = vec![root.join("recovery")];
        while let Some(directory) = pending.pop() {
            let Ok(children) = std::fs::read_dir(&directory) else {
                continue;
            };
            for child in children {
                let path = child.unwrap().path();
                if path.is_dir() {
                    pending.push(path.clone());
                }
                entries.push(path);
            }
        }
        entries.sort();
        entries
    }

    /// A scheduler source database at `root`, created through the native
    /// gate on a test host released before return.
    fn seed_scheduler_source(root: &Path, sql: &str) {
        let host = lillux_sqlite::test_host::TestNativeHost::acquire();
        let directory = Arc::new(lillux::PinnedDirectory::open(root).unwrap().unwrap());
        host.create_bound(
            directory,
            std::ffi::OsStr::new("scheduler.sqlite3"),
            lillux_sqlite::OriginalNativeDomain::NodeScheduler,
            sql,
        );
        let _ = host.release();
    }

    #[cfg(target_os = "linux")]
    pub(super) fn closed_scheduler_inspection(
        root: &Path,
        sql: &str,
    ) -> (
        lillux::PinnedDirectory,
        lillux::PinnedDirectoryLock,
        StateLock,
        Arc<dyn lillux_sqlite::SqliteInspectionCopyWork>,
        Arc<SqliteInspectionClosed>,
        Box<dyn std::any::Any>,
    ) {
        seed_scheduler_source(root, sql);
        let directory = lillux::PinnedDirectory::open(root).unwrap().unwrap();
        let namespace = directory.lock_exclusive().unwrap();
        let (state_lock, quota_claim, copy_work, host) = issue_inspection_claim_with_host(
            &directory,
            &namespace,
            &directory.path().join("runtime.sqlite3"),
        );
        let permit = SchedulerResetPreview {
            runtime_directory: &directory,
            namespace_lock: &namespace,
            identity: None,
            inspection_custody: std::cell::RefCell::new(None),
            inspection_closed: std::cell::RefCell::new(None),
            controller_custody: Some(&host.controller),
            original_operation: None,
            quota_claim: Arc::clone(&quota_claim),
            copy_work: Arc::clone(&copy_work),
        };
        permit
            .inspect_database(|factory, inputs, _schema| {
                let owner = match factory.open_bound(inputs) {
                    Ok(owner) => owner,
                    Err(failure) => panic!("scheduler copy refused its native ticket: {failure}"),
                };
                let closed = match owner.close() {
                    Ok(closed) => closed,
                    Err(failure) => panic!("scheduler copy close refused: {failure}"),
                };
                Ok(SqliteInspectionOutcome {
                    accounting: Ok(()),
                    terminal: lillux_sqlite::OriginalNativeConnectionTerminal::Closed(closed),
                })
            })
            .unwrap()
            .unwrap();
        let closed = permit.inspection_closed.borrow().clone().unwrap();
        drop(permit);
        (
            directory,
            namespace,
            state_lock,
            copy_work,
            closed,
            Box::new((quota_claim, host)),
        )
    }

    pub(super) fn issue_inspection_claim(
        directory: &lillux::PinnedDirectory,
        namespace: &lillux::PinnedDirectoryLock,
        db_target: &Path,
    ) -> (
        StateLock,
        Arc<OriginalQuotaClaim>,
        Arc<dyn lillux_sqlite::SqliteInspectionCopyWork>,
    ) {
        let (state_lock, claim, work, host) =
            prepare_inspection_claim(directory, namespace, db_target, false);
        assert!(host.is_none(), "no native host was requested");
        (state_lock, claim, work)
    }

    /// The claim's own offline native host, filed in a fresh controller the
    /// way a reset files it, with the serial guard that excludes every test
    /// host while it lives. Nothing here releases that host, so a test that
    /// issues one runs as an isolated child and ends with its process.
    struct IssuedInspectionHost {
        controller: Arc<InspectionControllerCustody>,
        serial: lillux_sqlite::test_host::TestHostSerial,
    }

    /// [`issue_inspection_claim`] whose claim has also issued its own native
    /// host, before its copy work, as a reset issues them.
    fn issue_inspection_claim_with_host(
        directory: &lillux::PinnedDirectory,
        namespace: &lillux::PinnedDirectoryLock,
        db_target: &Path,
    ) -> (
        StateLock,
        Arc<OriginalQuotaClaim>,
        Arc<dyn lillux_sqlite::SqliteInspectionCopyWork>,
        IssuedInspectionHost,
    ) {
        let (state_lock, claim, work, host) =
            prepare_inspection_claim(directory, namespace, db_target, true);
        (
            state_lock,
            claim,
            work,
            host.expect("claim native host issued"),
        )
    }

    fn prepare_inspection_claim(
        directory: &lillux::PinnedDirectory,
        namespace: &lillux::PinnedDirectoryLock,
        db_target: &Path,
        native_host: bool,
    ) -> (
        StateLock,
        Arc<OriginalQuotaClaim>,
        Arc<dyn lillux_sqlite::SqliteInspectionCopyWork>,
        Option<IssuedInspectionHost>,
    ) {
        let mut config = Config::load(&ConfigSources {
            app_root: Some(directory.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();
        config.db_path = db_target.to_path_buf();
        let identity =
            ryeos_node_runtime::identity::NodeIdentity::create(&config.node_signing_key_path)
                .unwrap();
        install_offline_policy(&config, &identity, true);
        let state_lock = StateLock::acquire(&default_lock_path(&config.app_root)).unwrap();
        fixture_operational_source::establish(directory, namespace).unwrap();
        let policy =
            OfflineResetPolicyOwner::load(&config, &identity, &state_lock, directory, namespace)
                .unwrap();
        let scheduler_selected = directory
            .entry_no_follow(std::ffi::OsStr::new("scheduler.sqlite3"))
            .unwrap()
            .is_some();
        let claim = OriginalQuotaClaim::prepare(
            &policy,
            &identity,
            None,
            inspection_destination::OriginalInspectionMode::DryRun,
            scheduler_selected,
        )
        .unwrap();
        claim.publish(&policy, &identity).unwrap();
        let host = native_host.then(|| {
            // Every test host this thread used is released by now; the
            // claim's own host is the only arena while the guard is held.
            let serial = lillux_sqlite::test_host::serial();
            let controller = Arc::new(InspectionControllerCustody::default());
            let factory = controller
                .adopt_native_host(claim.issue_native_host(&policy, &identity))
                .unwrap();
            drop(factory);
            IssuedInspectionHost { controller, serial }
        });
        let work = claim.issue_copy_work(&policy, &identity).unwrap();
        drop(policy);
        (state_lock, claim, work, host)
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn scheduler_preview_retains_real_native_close_proof_without_mutating_source() {
        const TEST: &str = concat!(
            module_path!(),
            "::scheduler_preview_retains_real_native_close_proof_without_mutating_source"
        );
        // The claim's own native host ends only with its process, so the
        // scenario runs in a child and this process checks what it saw.
        if !lillux_sqlite::test_host::is_isolated_child(TEST) {
            let outcome = lillux_sqlite::test_host::run_isolated(TEST);
            assert_eq!(
                outcome,
                "closed scheduler copy proof retained by its controller"
            );
            return;
        }
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("scheduler.sqlite3");
        seed_scheduler_source(
            tmp.path(),
            "CREATE TABLE sample(value INTEGER); INSERT INTO sample VALUES(7);",
        );
        let original = std::fs::read(&source).unwrap();
        let directory = lillux::PinnedDirectory::open(tmp.path()).unwrap().unwrap();
        let namespace = directory.lock_exclusive().unwrap();
        let (state_lock, quota_claim, copy_work, host) = issue_inspection_claim_with_host(
            &directory,
            &namespace,
            &directory.path().join("runtime.sqlite3"),
        );
        let IssuedInspectionHost { controller, serial } = host;
        let permit = SchedulerResetPreview {
            runtime_directory: &directory,
            namespace_lock: &namespace,
            identity: None,
            inspection_custody: std::cell::RefCell::new(None),
            inspection_closed: std::cell::RefCell::new(None),
            controller_custody: Some(&controller),
            original_operation: None,
            quota_claim: Arc::clone(&quota_claim),
            copy_work,
        };
        let copied_path = permit
            .inspect_database(|factory, inputs, _schema| {
                let copy = permit
                    .inspection_custody
                    .borrow()
                    .as_ref()
                    .expect("copy lease claimed before the callback")
                    .directory()?
                    .path()
                    .join("scheduler.sqlite3");
                assert_ne!(copy, source);
                let owner = match factory.open_bound(inputs) {
                    Ok(owner) => owner,
                    Err(failure) => panic!("scheduler copy refused its native ticket: {failure}"),
                };
                // Never open/drop a filesystem alias while SQLite owns the inode.
                let read = owner.with_sql(|connection| {
                    let value: i64 =
                        connection.query_row("SELECT value FROM sample", [], |row| row.get(0))?;
                    let refused = connection.execute_batch("DELETE FROM sample").is_err();
                    Ok((value, refused))
                });
                let closed = match owner.close() {
                    Ok(closed) => closed,
                    Err(failure) => panic!("scheduler copy close refused: {failure}"),
                };
                let (value, refused) = read.expect("read the copied sample row");
                assert_eq!(value, 7);
                assert!(refused, "the readonly copy must refuse a write");
                Ok(SqliteInspectionOutcome {
                    accounting: Ok(copy),
                    terminal: lillux_sqlite::OriginalNativeConnectionTerminal::Closed(closed),
                })
            })
            .unwrap()
            .unwrap();
        let closed = permit.inspection_closed.borrow().clone().unwrap();
        closed.require_original().unwrap();
        assert!(
            closed.original_native_terminal().unwrap().is_some(),
            "the census sees its ticket"
        );
        assert!(Arc::ptr_eq(
            closed.lease(),
            permit.inspection_custody.borrow().as_ref().unwrap()
        ));
        assert!(
            permit
                .inspect_database::<()>(|_, _, _| panic!("second inspection must refuse"))
                .is_err()
        );
        drop(permit);
        assert_eq!(std::fs::read(&copied_path).unwrap(), original);
        assert_eq!(std::fs::read(&source).unwrap(), original);
        let weak = Arc::downgrade(&closed);
        drop(closed);
        let later_error = controller.retain_failure(anyhow::anyhow!("later controller refusal"));
        drop(controller);
        assert!(
            weak.upgrade().is_some(),
            "later controller error must retain successful native proof"
        );
        drop(later_error);
        assert!(
            weak.upgrade().is_none(),
            "controller custody must not cycle back from a claim"
        );
        assert!(
            copied_path.exists(),
            "native closure does not grant physical disposal"
        );
        lillux_sqlite::test_host::end_isolated_process(
            (quota_claim, state_lock, namespace, directory, tmp, serial),
            "closed scheduler copy proof retained by its controller",
        )
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn genuine_native_interval_blocks_seal_until_original_closed_ack() {
        const TEST: &str = concat!(
            module_path!(),
            "::genuine_native_interval_blocks_seal_until_original_closed_ack"
        );
        if !lillux_sqlite::test_host::is_isolated_child(TEST) {
            let outcome = lillux_sqlite::test_host::run_isolated(TEST);
            assert_eq!(outcome, "seal waited for the acknowledged native close");
            return;
        }
        let tmp = TempDir::new().unwrap();
        seed_scheduler_source(
            tmp.path(),
            "CREATE TABLE sample(value INTEGER); INSERT INTO sample VALUES(19);",
        );
        let directory = lillux::PinnedDirectory::open(tmp.path()).unwrap().unwrap();
        let namespace = directory.lock_exclusive().unwrap();
        let (state_lock, quota_claim, copy_work, host) = issue_inspection_claim_with_host(
            &directory,
            &namespace,
            &directory.path().join("runtime.sqlite3"),
        );
        let IssuedInspectionHost { controller, serial } = host;
        let permit = SchedulerResetPreview {
            runtime_directory: &directory,
            namespace_lock: &namespace,
            identity: None,
            inspection_custody: std::cell::RefCell::new(None),
            inspection_closed: std::cell::RefCell::new(None),
            controller_custody: Some(&controller),
            original_operation: None,
            quota_claim: Arc::clone(&quota_claim),
            copy_work: Arc::clone(&copy_work),
        };
        permit
            .inspect_database(|factory, inputs, _schema| {
                let owner = match factory.open_bound(inputs) {
                    Ok(owner) => owner,
                    Err(failure) => panic!("scheduler copy refused its native ticket: {failure}"),
                };
                let value = owner.with_sql(|connection| {
                    Ok(connection
                        .query_row("SELECT value FROM sample", [], |row| row.get::<_, i64>(0))?)
                });
                // A refused seal still begins the drain, so it is attempted
                // only after the last SQL on this connection.
                assert!(
                    quota_claim.seal_production(&copy_work).is_err(),
                    "an open native interval blocks the seal"
                );
                let closed = match owner.close() {
                    Ok(closed) => closed,
                    Err(failure) => panic!("scheduler copy close refused: {failure}"),
                };
                assert_eq!(value.expect("read the copied sample row"), 19);
                Ok(SqliteInspectionOutcome {
                    accounting: Ok(()),
                    terminal: lillux_sqlite::OriginalNativeConnectionTerminal::Closed(closed),
                })
            })
            .unwrap()
            .unwrap();
        // The preview acknowledges the interval only from the closed proof
        // its controller retained; that proof is what admits the seal.
        let closed = permit.inspection_closed.borrow().clone().unwrap();
        closed.require_original().unwrap();
        let seal = quota_claim.seal_production(&copy_work).unwrap();
        assert!(
            seal.slot(0).unwrap().never_started,
            "the Runtime store was never produced"
        );
        assert!(
            !seal.slot(1).unwrap().never_started,
            "the scheduler store was produced"
        );
        assert!(
            quota_claim
                .begin_inspection_settlement(seal, Vec::new())
                .is_err(),
            "attempted store cannot be replaced by an empty disposal aggregate"
        );
        drop(permit);
        lillux_sqlite::test_host::end_isolated_process(
            (
                closed,
                copy_work,
                quota_claim,
                controller,
                state_lock,
                namespace,
                directory,
                tmp,
                serial,
            ),
            "seal waited for the acknowledged native close",
        )
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn genuine_never_started_seal_rejects_foreign_issuer_and_retains_charge() {
        const TEST: &str = concat!(
            module_path!(),
            "::genuine_never_started_seal_rejects_foreign_issuer_and_retains_charge"
        );
        // Disposal records the claim's native quota, so the claim issues its
        // own host, which ends only with this child's process.
        if !lillux_sqlite::test_host::is_isolated_child(TEST) {
            let outcome = lillux_sqlite::test_host::run_isolated(TEST);
            assert_eq!(outcome, "never-started seal kept its charge");
            return;
        }
        let first = TempDir::new().unwrap();
        let second = TempDir::new().unwrap();
        let a = lillux::PinnedDirectory::open(first.path())
            .unwrap()
            .unwrap();
        let b = lillux::PinnedDirectory::open(second.path())
            .unwrap()
            .unwrap();
        let al = a.lock_exclusive().unwrap();
        let bl = b.lock_exclusive().unwrap();
        // The foreign claim issues no host; it is prepared first because the
        // serial guard the issuing claim holds excludes every later test host.
        let (state_b, other, foreign) =
            issue_inspection_claim(&b, &bl, &second.path().join("runtime.sqlite3"));
        let (state_a, claim, work, host) =
            issue_inspection_claim_with_host(&a, &al, &first.path().join("runtime.sqlite3"));
        assert!(claim.seal_production(&foreign).is_err());
        let seal = claim.seal_production(&work).unwrap();
        assert!(seal.slot(0).unwrap().never_started && seal.slot(1).unwrap().never_started);
        let mut config = Config::load(&ConfigSources {
            app_root: Some(first.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();
        config.db_path = first.path().join("runtime.sqlite3");
        let identity =
            ryeos_node_runtime::identity::NodeIdentity::load(&config.node_signing_key_path)
                .unwrap();
        let policy = OfflineResetPolicyOwner::load(&config, &identity, &state_a, &a, &al).unwrap();
        assert!(
            claim.issue_copy_work(&policy, &identity).is_err(),
            "sealed original issuer cannot be reissued"
        );
        drop(policy);
        assert!(
            other
                .begin_inspection_settlement(seal.clone(), Vec::new())
                .is_err()
        );
        let settlement = claim.begin_inspection_settlement(seal, Vec::new()).unwrap();
        settlement.disposition_record().unwrap();
        settlement.dispose_witnesses().unwrap();
        settlement.seal_disposition().unwrap();
        // A retry of the same sealed step is idempotent.
        settlement.seal_disposition().unwrap();
        let error = settlement.require_completed().unwrap_err();
        assert!(format!("{error:#}").contains("not captured"), "{error:#}");
        assert!(
            first
                .path()
                .join(".ai/node/offline-reset-quota")
                .join(format!("reserved.{}", claim.operation_id()))
                .exists()
        );
        assert!(
            work.clone()
                .create_destination(&a, std::ffi::OsStr::new("runtime.sqlite3"))
                .is_err()
        );
        lillux_sqlite::test_host::end_isolated_process(
            (
                settlement, work, claim, host, state_a, other, foreign, state_b, al, bl, a, b,
                first, second,
            ),
            "never-started seal kept its charge",
        )
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn scheduler_accounting_failure_retains_actual_native_close_proof() {
        const TEST: &str = concat!(
            module_path!(),
            "::scheduler_accounting_failure_retains_actual_native_close_proof"
        );
        if !lillux_sqlite::test_host::is_isolated_child(TEST) {
            let outcome = lillux_sqlite::test_host::run_isolated(TEST);
            assert_eq!(
                outcome,
                "refused accounting kept its closed scheduler copy proof"
            );
            return;
        }
        let tmp = TempDir::new().unwrap();
        seed_scheduler_source(tmp.path(), "CREATE TABLE sample(value INTEGER);");
        let directory = lillux::PinnedDirectory::open(tmp.path()).unwrap().unwrap();
        let namespace = directory.lock_exclusive().unwrap();
        let (state_lock, quota_claim, copy_work, host) = issue_inspection_claim_with_host(
            &directory,
            &namespace,
            &directory.path().join("runtime.sqlite3"),
        );
        let IssuedInspectionHost { controller, serial } = host;
        let permit = SchedulerResetPreview {
            runtime_directory: &directory,
            namespace_lock: &namespace,
            identity: None,
            inspection_custody: std::cell::RefCell::new(None),
            inspection_closed: std::cell::RefCell::new(None),
            controller_custody: Some(&controller),
            original_operation: None,
            quota_claim,
            copy_work,
        };
        let error = permit
            .inspect_database::<()>(|factory, inputs, _schema| {
                let owner = match factory.open_bound(inputs) {
                    Ok(owner) => owner,
                    Err(failure) => panic!("scheduler copy refused its native ticket: {failure}"),
                };
                let closed = match owner.close() {
                    Ok(closed) => closed,
                    Err(failure) => panic!("scheduler copy close refused: {failure}"),
                };
                Ok(SqliteInspectionOutcome {
                    accounting: Err(anyhow::anyhow!("injected accounting refusal")),
                    terminal: lillux_sqlite::OriginalNativeConnectionTerminal::Closed(closed),
                })
            })
            .unwrap_err();
        assert!(format!("{error:#}").contains("injected accounting refusal"));
        let closed = permit.inspection_closed.borrow().clone().unwrap();
        closed.require_original().unwrap();
        let weak = Arc::downgrade(&closed);
        let copy = closed
            .lease()
            .directory()
            .unwrap()
            .path()
            .join("scheduler.sqlite3");
        drop(closed);
        drop(permit);
        drop(controller);
        assert!(
            weak.upgrade().is_some(),
            "error must own the actual closed proof"
        );
        drop(error);
        assert!(
            weak.upgrade().is_none(),
            "proof custody must not form a claim cycle"
        );
        assert!(
            copy.exists(),
            "dropping evidence does not settle physical obligations"
        );
        lillux_sqlite::test_host::end_isolated_process(
            (state_lock, namespace, directory, tmp, serial),
            "refused accounting kept its closed scheduler copy proof",
        )
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn scheduler_partial_acquisition_release_never_supplies_completed_accounting() {
        const TEST: &str = concat!(
            module_path!(),
            "::scheduler_partial_acquisition_release_never_supplies_completed_accounting"
        );
        if !lillux_sqlite::test_host::is_isolated_child(TEST) {
            let outcome = lillux_sqlite::test_host::run_isolated(TEST);
            assert_eq!(
                outcome,
                "released scheduler acquisition supplied no accounting"
            );
            return;
        }
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("scheduler.sqlite3"), b"retained main").unwrap();
        let directory = lillux::PinnedDirectory::open(tmp.path()).unwrap().unwrap();
        let namespace = directory.lock_exclusive().unwrap();
        let (state_lock, quota_claim, copy_work, host) = issue_inspection_claim_with_host(
            &directory,
            &namespace,
            &directory.path().join("runtime.sqlite3"),
        );
        let IssuedInspectionHost { controller, serial } = host;
        let permit = SchedulerResetPreview {
            runtime_directory: &directory,
            namespace_lock: &namespace,
            identity: None,
            inspection_custody: std::cell::RefCell::new(None),
            inspection_closed: std::cell::RefCell::new(None),
            controller_custody: Some(&controller),
            original_operation: None,
            quota_claim,
            copy_work,
        };
        let error = permit
            .inspect_database::<()>(|factory, inputs, _schema| {
                // Inject an invalid sidecar into the isolated destination. The
                // original source is untouched; cleanup must retain this deposit.
                let copy = permit
                    .inspection_custody
                    .borrow()
                    .as_ref()
                    .expect("copy lease claimed before the callback")
                    .directory()?
                    .path()
                    .to_path_buf();
                std::fs::create_dir(copy.join("scheduler.sqlite3-wal"))?;
                let failure = match factory.open_bound(inputs) {
                    Ok(owner) => {
                        let closed = owner
                            .close()
                            .map(|_| ())
                            .map_err(|failure| failure.to_string());
                        panic!("nonregular WAL must refuse acquisition; close: {closed:?}");
                    }
                    Err(failure) => failure,
                };
                let released = match failure.retry_close() {
                    Ok(released) => released,
                    Err(failure) => {
                        panic!("refused scheduler acquisition kept its owner: {failure}")
                    }
                };
                Ok(SqliteInspectionOutcome {
                    accounting: Err(anyhow::anyhow!("injected acquisition refusal")),
                    terminal: lillux_sqlite::OriginalNativeConnectionTerminal::Released(released),
                })
            })
            .unwrap_err();
        assert!(format!("{error:#}").contains("injected acquisition refusal"));
        assert!(permit.inspection_closed.borrow().is_none());
        assert!(controller.closed.lock().unwrap().is_empty());
        let released = controller.runtime_released.lock().unwrap()[0].clone();
        match released.as_ref() {
            RuntimeInspectionRelease::Acquisition(owner) => owner.require_original().unwrap(),
            RuntimeInspectionRelease::Closed(_) => {
                panic!("a refused acquisition has no closed proof")
            }
        }
        let weak = Arc::downgrade(&released);
        let copy = permit
            .inspection_custody
            .borrow()
            .as_ref()
            .unwrap()
            .directory()
            .unwrap()
            .path()
            .to_path_buf();
        drop(released);
        drop(permit);
        drop(controller);
        assert!(
            weak.upgrade().is_some(),
            "returned refusal must retain actual released evidence"
        );
        drop(error);
        assert!(
            weak.upgrade().is_none(),
            "released evidence must not cycle into its claim"
        );
        assert!(copy.join("scheduler.sqlite3").exists());
        assert!(copy.join("scheduler.sqlite3-wal").is_dir());
        assert_eq!(
            std::fs::read(tmp.path().join("scheduler.sqlite3")).unwrap(),
            b"retained main"
        );
        lillux_sqlite::test_host::end_isolated_process(
            (state_lock, namespace, directory, tmp, serial),
            "released scheduler acquisition supplied no accounting",
        )
    }

    #[test]
    fn actual_copy_drop_does_not_refund_original_role_or_reservation() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("scheduler.sqlite3"), b"original").unwrap();
        let source = lillux::PinnedDirectory::open(tmp.path()).unwrap().unwrap();
        let namespace = source.lock_exclusive().unwrap();
        let (_state_lock, claim, work) =
            issue_inspection_claim(&source, &namespace, &tmp.path().join("runtime.sqlite3"));
        let record = tmp
            .path()
            .join(".ai/node/offline-reset-quota")
            .join(format!("reserved.{}", claim.operation_id()));
        let charged = std::fs::read(&record).unwrap();
        let (copy, lease) = crate::runtime_db::create_sqlite_inspection_copy(
            &source,
            std::ffi::OsStr::new("scheduler.sqlite3"),
            &namespace,
            work.clone(),
        )
        .unwrap();
        let copy_path = copy.path().to_owned();
        drop(copy);
        drop(lease);
        assert_eq!(
            std::fs::read(copy_path.join("scheduler.sqlite3")).unwrap(),
            b"original",
            "original copy remains a physical obligation after Drop"
        );
        assert_eq!(std::fs::read(&record).unwrap(), charged);
        let error = match crate::runtime_db::create_sqlite_inspection_copy(
            &source,
            std::ffi::OsStr::new("scheduler.sqlite3"),
            &namespace,
            work.clone(),
        ) {
            Ok(_) => panic!("original role must stay charged after copy Drop"),
            Err(error) => error,
        };
        // The namespace producer admits one attempt per store, so the
        // second copy is refused before any allocation is considered.
        assert!(
            format!("{error:#}").contains("namespace producer already attempted"),
            "{error:#}"
        );
        assert_eq!(std::fs::read(&record).unwrap(), charged);
        drop(error);
        std::fs::rename(&record, record.with_extension("original-retained")).unwrap();
        std::fs::write(&record, &charged).unwrap();
        assert!(
            work.checkpoint().is_err(),
            "identical replacement does not recreate charged inode custody"
        );
    }

    #[test]
    fn both_real_inspection_destinations_share_original_sql_budget_without_refund() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("scheduler.sqlite3"), b"scheduler bytes").unwrap();
        std::fs::write(tmp.path().join("runtime.sqlite3"), b"runtime bytes").unwrap();
        let source = lillux::PinnedDirectory::open(tmp.path()).unwrap().unwrap();
        let namespace = source.lock_exclusive().unwrap();
        let (_state_lock, claim, work) =
            issue_inspection_claim(&source, &namespace, &tmp.path().join("runtime.sqlite3"));
        let (scheduler, scheduler_lease) = crate::runtime_db::create_sqlite_inspection_copy(
            &source,
            std::ffi::OsStr::new("scheduler.sqlite3"),
            &namespace,
            work.clone(),
        )
        .unwrap();
        let (runtime, runtime_lease) = crate::runtime_db::create_sqlite_inspection_copy(
            &source,
            std::ffi::OsStr::new("runtime.sqlite3"),
            &namespace,
            work.clone(),
        )
        .unwrap();
        let scheduler_sql = work.clone().issue_sql_work(&scheduler).unwrap();
        let runtime_sql = work.clone().issue_sql_work(&runtime).unwrap();
        let foreign = TempDir::new().unwrap();
        let foreign = lillux::PinnedDirectory::open(foreign.path())
            .unwrap()
            .unwrap();
        // SQL service runs only inside the store's native producer interval.
        assert!(scheduler_sql.require_destination(&scheduler).is_err());
        let _production = scheduler_sql
            .begin_native_production(&scheduler, std::ffi::OsStr::new("scheduler.sqlite3"))
            .unwrap();
        assert!(scheduler_sql.require_destination(&foreign).is_err());
        assert!(scheduler_sql.require_destination(&scheduler).is_ok());
        let record = tmp
            .path()
            .join(".ai/node/offline-reset-quota")
            .join(format!("reserved.{}", claim.operation_id()));
        let signed = std::fs::read(&record).unwrap();
        let body = lillux::signature::strip_signature_lines(std::str::from_utf8(&signed).unwrap());
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        let maximum = value["reserved"]["max_sql_vm_steps"].as_u64().unwrap();
        scheduler_sql.debit_vm_steps(maximum - 1).unwrap();
        runtime_sql.debit_vm_steps(1).unwrap();
        assert!(scheduler_sql.debit_vm_steps(1).is_err());
        assert!(runtime_sql.debit_vm_steps(1).is_err());
        drop(scheduler_sql);
        drop(runtime_sql);
        assert!(work.clone().issue_sql_work(&scheduler).is_err());
        assert!(work.issue_sql_work(&runtime).is_err());
        drop(scheduler);
        drop(runtime);
        drop(scheduler_lease);
        drop(runtime_lease);
        assert_eq!(std::fs::read(&record).unwrap(), signed);
    }

    #[test]
    fn original_inspection_parent_lock_survives_copy_views_and_guard_custody() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("scheduler.sqlite3"), b"original").unwrap();
        let source = lillux::PinnedDirectory::open(tmp.path()).unwrap().unwrap();
        let namespace = source.lock_exclusive().unwrap();
        let (_state_lock, claim, work) =
            issue_inspection_claim(&source, &namespace, &tmp.path().join("runtime.sqlite3"));
        let parent_path = tmp.path().join(".ai/node/offline-reset-inspections");
        let parent = lillux::PinnedDirectory::open(&parent_path)
            .unwrap()
            .unwrap();
        assert!(parent.try_lock_exclusive().unwrap().is_none());
        let (copy, lease) = crate::runtime_db::create_sqlite_inspection_copy(
            &source,
            std::ffi::OsStr::new("scheduler.sqlite3"),
            &namespace,
            work.clone(),
        )
        .unwrap();
        drop(work);
        drop(claim);
        assert!(parent.try_lock_exclusive().unwrap().is_none());
        drop(copy);
        assert!(parent.try_lock_exclusive().unwrap().is_none());
        drop(lease);
        assert!(parent.try_lock_exclusive().unwrap().is_some());
        // Lock release is not quota settlement or erasure of signed evidence.
        assert_eq!(
            std::fs::read_dir(tmp.path().join(".ai/node/offline-reset-quota"))
                .unwrap()
                .count(),
            1
        );
        assert!(std::fs::read_dir(&parent_path).unwrap().any(|e| {
            e.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("created.")
        }));
    }

    #[test]
    fn created_root_evidence_binds_actual_inode_before_body_and_survives_drop() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("scheduler.sqlite3"), b"original").unwrap();
        let source = lillux::PinnedDirectory::open(tmp.path()).unwrap().unwrap();
        let namespace = source.lock_exclusive().unwrap();
        let (_state_lock, claim, work) =
            issue_inspection_claim(&source, &namespace, &tmp.path().join("runtime.sqlite3"));
        let (copy, lease) = crate::runtime_db::create_sqlite_inspection_copy(
            &source,
            std::ffi::OsStr::new("scheduler.sqlite3"),
            &namespace,
            work.clone(),
        )
        .unwrap();
        let destination = copy.path().file_name().unwrap().to_str().unwrap();
        let name = creation_evidence::evidence_name(destination);
        let evidence_path = copy.path().parent().unwrap().join(&name);
        let signed = std::fs::read_to_string(&evidence_path).unwrap();
        let evidence: creation_evidence::CreatedDirectoryRecord =
            serde_json::from_str(&lillux::signature::strip_signature_lines(&signed)).unwrap();
        let claim_path = tmp
            .path()
            .join(".ai/node/offline-reset-quota")
            .join(format!("reserved.{}", claim.operation_id()));
        assert_eq!(
            evidence.claim_sha256,
            lillux::sha256_hex(&std::fs::read(&claim_path).unwrap())
        );
        assert_eq!(evidence.operation_id, claim.operation_id());
        assert_eq!(evidence.created_identity, copy.identity().unwrap());
        assert_eq!(evidence.destination_name, destination);
        assert_eq!(
            std::fs::read(copy.path().join("scheduler.sqlite3")).unwrap(),
            b"original"
        );
        let mut missing = serde_json::to_value(&evidence).unwrap();
        missing.as_object_mut().unwrap().remove("created_identity");
        assert!(
            serde_json::from_value::<creation_evidence::CreatedDirectoryRecord>(missing).is_err()
        );
        let mut unknown = serde_json::to_value(&evidence).unwrap();
        unknown["native_owner_grant"] = serde_json::json!(true);
        assert!(
            serde_json::from_value::<creation_evidence::CreatedDirectoryRecord>(unknown).is_err()
        );
        let copied_path = copy.path().join("scheduler.sqlite3");
        drop(copy);
        drop(lease);
        assert_eq!(std::fs::read(&copied_path).unwrap(), b"original");
        assert_eq!(
            std::fs::read_to_string(&evidence_path).unwrap(),
            signed,
            "physical Drop cannot erase durable custody evidence or refund"
        );
        assert!(claim_path.exists());
    }

    #[test]
    fn uncertain_created_evidence_ack_retains_root_before_any_body_work() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("scheduler.sqlite3"), b"original").unwrap();
        let source = lillux::PinnedDirectory::open(tmp.path()).unwrap().unwrap();
        let namespace = source.lock_exclusive().unwrap();
        let (_state_lock, claim, work) =
            issue_inspection_claim(&source, &namespace, &tmp.path().join("runtime.sqlite3"));
        original_quota_claim::fail_next_created_evidence_ack();
        let error = match crate::runtime_db::create_sqlite_inspection_copy(
            &source,
            std::ffi::OsStr::new("scheduler.sqlite3"),
            &namespace,
            work.clone(),
        ) {
            Ok(_) => panic!("created evidence uncertain acknowledgement must refuse body"),
            Err(error) => error,
        };
        assert!(format!("{error:#}").contains("created evidence acknowledgement failure"));
        let parent = tmp.path().join(".ai/node/offline-reset-inspections");
        let name = format!("inspection.{}.scheduler", claim.operation_id());
        let created = parent.join(&name);
        assert!(created.is_dir());
        assert_eq!(
            std::fs::read_dir(&created).unwrap().count(),
            0,
            "no source body before evidence acknowledgement"
        );
        assert!(
            parent
                .join(creation_evidence::evidence_name(&name))
                .is_file()
        );
        assert!(
            work.create_destination(&source, std::ffi::OsStr::new("scheduler.sqlite3"))
                .is_err(),
            "uncertainty cannot recreate original allocation"
        );
        drop(error);
        assert!(
            created.is_dir(),
            "no inspection-copy guard was issued or credited with cleanup"
        );
    }

    #[test]
    fn identical_created_evidence_at_new_inode_refuses_sql_issue() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("scheduler.sqlite3"), b"original").unwrap();
        let source = lillux::PinnedDirectory::open(tmp.path()).unwrap().unwrap();
        let namespace = source.lock_exclusive().unwrap();
        let (_state_lock, _claim, work) =
            issue_inspection_claim(&source, &namespace, &tmp.path().join("runtime.sqlite3"));
        let (copy, _lease) = crate::runtime_db::create_sqlite_inspection_copy(
            &source,
            std::ffi::OsStr::new("scheduler.sqlite3"),
            &namespace,
            work.clone(),
        )
        .unwrap();
        let name =
            creation_evidence::evidence_name(copy.path().file_name().unwrap().to_str().unwrap());
        let evidence_path = copy.path().parent().unwrap().join(&name);
        let signed = std::fs::read(&evidence_path).unwrap();
        std::fs::rename(
            &evidence_path,
            tmp.path().join("retained-original-evidence"),
        )
        .unwrap();
        std::fs::write(&evidence_path, &signed).unwrap();
        assert!(
            work.issue_sql_work(&copy).is_err(),
            "signed bytes do not recreate original published inode custody"
        );
        assert_eq!(
            std::fs::read(copy.path().join("scheduler.sqlite3")).unwrap(),
            b"original"
        );
    }

    #[test]
    fn issued_copy_refuses_foreign_output_before_creating_data() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("scheduler.sqlite3"), b"original").unwrap();
        let source = lillux::PinnedDirectory::open(tmp.path()).unwrap().unwrap();
        let namespace = source.lock_exclusive().unwrap();
        let (_state_lock, _claim, work) =
            issue_inspection_claim(&source, &namespace, &tmp.path().join("runtime.sqlite3"));
        let name = std::ffi::OsStr::new("scheduler.sqlite3");
        let allocated = work.clone().create_destination(&source, name).unwrap();
        let foreign = TempDir::new().unwrap();
        let foreign_root = lillux::PinnedDirectory::open(foreign.path())
            .unwrap()
            .unwrap();
        assert!(
            lillux_sqlite::copy_regular_without_sqlite_alias(
                &source,
                name,
                &foreign_root,
                8,
                work.as_ref()
            )
            .is_err()
        );
        assert!(!foreign.path().join(name).exists());
        assert!(!allocated.root.path().join(name).exists());
        assert!(
            lillux_sqlite::copy_regular_without_sqlite_alias(
                &source,
                name,
                allocated.root.as_ref(),
                8,
                work.as_ref()
            )
            .unwrap()
            .is_some(),
            "wrong destination refusal must precede the role debit"
        );
    }

    #[test]
    fn scheduler_preview_rejects_replacement_before_copy_and_retains_claim() {
        use super::original_quota_claim::QuotaCustodyFailure;
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("scheduler.sqlite3");
        std::fs::write(&path, b"database").unwrap();
        let directory = lillux::PinnedDirectory::open(tmp.path()).unwrap().unwrap();
        let namespace = directory.lock_exclusive().unwrap();
        let (_state_lock, quota_claim, copy_work) = issue_inspection_claim(
            &directory,
            &namespace,
            &directory.path().join("runtime.sqlite3"),
        );
        std::fs::rename(&path, tmp.path().join("original-retained")).unwrap();
        std::fs::write(&path, b"database").unwrap();
        let permit = SchedulerResetPreview {
            runtime_directory: &directory,
            namespace_lock: &namespace,
            identity: None,
            inspection_custody: std::cell::RefCell::new(None),
            inspection_closed: std::cell::RefCell::new(None),
            controller_custody: None,
            original_operation: None,
            quota_claim: Arc::clone(&quota_claim),
            copy_work: copy_work.clone(),
        };
        let error = permit
            .inspect_database::<()>(|_, _, _| panic!("replacement must not reach decoder"))
            .unwrap_err();
        let held = error.downcast_ref::<QuotaCustodyFailure>().unwrap();
        assert_eq!(held.phase(), QuotaFailurePhase::Admission);
        assert!(Arc::ptr_eq(held.claim(), &quota_claim));
        assert!(permit.inspection_custody.borrow().is_none());
        assert_eq!(std::fs::read(&path).unwrap(), b"database");
        assert_eq!(
            std::fs::read(tmp.path().join("original-retained")).unwrap(),
            b"database"
        );
    }

    #[test]
    fn scheduler_preview_absent_source_never_invokes_decoder() {
        let tmp = TempDir::new().unwrap();
        let directory = lillux::PinnedDirectory::open(tmp.path()).unwrap().unwrap();
        let namespace = directory.lock_exclusive().unwrap();
        let (_state_lock, quota_claim, copy_work) = issue_inspection_claim(
            &directory,
            &namespace,
            &directory.path().join("runtime.sqlite3"),
        );
        let permit = SchedulerResetPreview {
            runtime_directory: &directory,
            namespace_lock: &namespace,
            identity: None,
            inspection_custody: std::cell::RefCell::new(None),
            inspection_closed: std::cell::RefCell::new(None),
            controller_custody: None,
            original_operation: None,
            quota_claim: Arc::clone(&quota_claim),
            copy_work: copy_work.clone(),
        };
        let result = permit
            .inspect_database::<()>(|_, _, _| panic!("absent database has no decoder input"))
            .unwrap();
        assert!(result.is_none());
        assert!(!tmp.path().join("scheduler.sqlite3").exists());
    }

    #[test]
    fn scheduler_preview_absent_main_refuses_each_orphan_sidecar_unchanged() {
        for name in [
            "scheduler.sqlite3-wal",
            "scheduler.sqlite3-shm",
            "scheduler.sqlite3-journal",
        ] {
            let tmp = TempDir::new().unwrap();
            let sidecar = tmp.path().join(name);
            let directory = lillux::PinnedDirectory::open(tmp.path()).unwrap().unwrap();
            let namespace = directory.lock_exclusive().unwrap();
            let (_state_lock, quota_claim, copy_work) = issue_inspection_claim(
                &directory,
                &namespace,
                &directory.path().join("runtime.sqlite3"),
            );
            let recovery_before = recovery_entries(tmp.path());
            std::fs::write(&sidecar, b"retained history").unwrap();
            let permit = SchedulerResetPreview {
                runtime_directory: &directory,
                namespace_lock: &namespace,
                identity: None,
                inspection_custody: std::cell::RefCell::new(None),
                inspection_closed: std::cell::RefCell::new(None),
                controller_custody: None,
                original_operation: None,
                quota_claim: Arc::clone(&quota_claim),
                copy_work: copy_work.clone(),
            };
            assert!(
                permit
                    .inspect_database::<()>(|_, _, _| panic!(
                        "orphan history must refuse before decoder"
                    ))
                    .is_err()
            );
            assert_eq!(std::fs::read(sidecar).unwrap(), b"retained history");
            assert!(!tmp.path().join("scheduler.sqlite3").exists());
            assert_eq!(
                recovery_entries(tmp.path()),
                recovery_before,
                "refusal records no recovery state"
            );
        }
    }

    #[test]
    fn scheduler_preview_absent_main_refuses_nonregular_sidecars_unchanged() {
        for name in [
            "scheduler.sqlite3-wal",
            "scheduler.sqlite3-shm",
            "scheduler.sqlite3-journal",
        ] {
            let tmp = TempDir::new().unwrap();
            let sidecar = tmp.path().join(name);
            let directory = lillux::PinnedDirectory::open(tmp.path()).unwrap().unwrap();
            let namespace = directory.lock_exclusive().unwrap();
            let (_state_lock, quota_claim, copy_work) = issue_inspection_claim(
                &directory,
                &namespace,
                &directory.path().join("runtime.sqlite3"),
            );
            let recovery_before = recovery_entries(tmp.path());
            std::fs::create_dir(&sidecar).unwrap();
            let permit = SchedulerResetPreview {
                runtime_directory: &directory,
                namespace_lock: &namespace,
                identity: None,
                inspection_custody: std::cell::RefCell::new(None),
                inspection_closed: std::cell::RefCell::new(None),
                controller_custody: None,
                original_operation: None,
                quota_claim: Arc::clone(&quota_claim),
                copy_work: copy_work.clone(),
            };
            assert!(
                permit
                    .inspect_database::<()>(|_, _, _| panic!(
                        "nonregular sidecar must refuse before decoder"
                    ))
                    .is_err()
            );
            assert!(sidecar.is_dir());
            assert!(!tmp.path().join("scheduler.sqlite3").exists());
            assert_eq!(
                recovery_entries(tmp.path()),
                recovery_before,
                "refusal records no recovery state"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn scheduler_preview_absent_main_refuses_symlink_sidecars_unchanged() {
        for name in [
            "scheduler.sqlite3-wal",
            "scheduler.sqlite3-shm",
            "scheduler.sqlite3-journal",
        ] {
            let tmp = TempDir::new().unwrap();
            let target = tmp.path().join("retained");
            std::fs::write(&target, b"retained history").unwrap();
            let sidecar = tmp.path().join(name);
            let directory = lillux::PinnedDirectory::open(tmp.path()).unwrap().unwrap();
            let namespace = directory.lock_exclusive().unwrap();
            let (_state_lock, quota_claim, copy_work) = issue_inspection_claim(
                &directory,
                &namespace,
                &directory.path().join("runtime.sqlite3"),
            );
            let recovery_before = recovery_entries(tmp.path());
            std::os::unix::fs::symlink(&target, &sidecar).unwrap();
            let permit = SchedulerResetPreview {
                runtime_directory: &directory,
                namespace_lock: &namespace,
                identity: None,
                inspection_custody: std::cell::RefCell::new(None),
                inspection_closed: std::cell::RefCell::new(None),
                controller_custody: None,
                original_operation: None,
                quota_claim: Arc::clone(&quota_claim),
                copy_work: copy_work.clone(),
            };
            assert!(
                permit
                    .inspect_database::<()>(|_, _, _| panic!(
                        "symlink sidecar must refuse before decoder"
                    ))
                    .is_err()
            );
            assert_eq!(std::fs::read_link(sidecar).unwrap(), target);
            assert_eq!(std::fs::read(target).unwrap(), b"retained history");
            assert!(!tmp.path().join("scheduler.sqlite3").exists());
            assert_eq!(
                recovery_entries(tmp.path()),
                recovery_before,
                "refusal records no recovery state"
            );
        }
    }
}

#[cfg(test)]
mod refusal_layer_tests {
    use super::SchedulerCopyRefusal;
    use lillux_sqlite::original_host_owner::FixedHostRefusal;
    use lillux_sqlite::{NativeCheckContended, RefusalSupersedes, native_refusal_is_fact};

    /// A terminal that carries no copy proof is a later refusal over the
    /// inspection's own accounting refusal: the copy refusal that carries
    /// both is classified by the later refusal, and still names the earlier.
    #[test]
    fn a_missing_copy_proof_classifies_over_the_accounting_refusal() {
        let wrap = |error: anyhow::Error| {
            anyhow::Error::new(SchedulerCopyRefusal {
                error,
                _sql: None,
                _namespace: None,
            })
        };
        let accounting =
            || anyhow::Error::new(FixedHostRefusal::Contended).context("scheduler accounting read");
        let refusal = || anyhow::anyhow!("scheduler inspection terminal carries no copy proof");
        let fact = wrap(refusal().superseding(accounting()));
        assert!(native_refusal_is_fact(&fact), "{fact:#}");
        assert!(
            format!("{fact:#}").contains("scheduler accounting read"),
            "{fact:#}"
        );
        let contention = wrap(anyhow::Error::new(NativeCheckContended).superseding(refusal()));
        assert!(!native_refusal_is_fact(&contention), "{contention:#}");
    }
}
