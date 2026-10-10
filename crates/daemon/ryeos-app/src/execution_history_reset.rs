//! Explicit offline retirement of the local execution-history epoch.
//!
//! Normal maintenance GC deliberately preserves authoritative chain heads.
//! This module owns the separate operator-authorized path used when an entire
//! local thread-history epoch is disposable. It coordinates every store while
//! the daemon state lock, runtime namespace lock, and exclusive CAS mutation
//! guard are held.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, ensure};
use serde::{Deserialize, Serialize};

mod creation_evidence;
mod inspection_destination;
mod inspection_plan;
mod offline_policy;
mod original_operation;
mod original_quota_claim;
mod operational_owner;
mod native_backend;
#[cfg(test)]
mod fixture_operational_source;
use operational_owner::OperationalOwnerPhase;
use offline_policy::OfflineResetPolicyOwner;
use original_operation::OriginalOperation;
use original_quota_claim::{OriginalInspectionSettlement, OriginalQuotaClaim, QuotaFailurePhase};

use ryeos_node_runtime::config::{Config, ConfigSources};
use crate::runtime_db::{
    InspectionCopyCleanup, InspectionCopyDisposal, RuntimeDb, RuntimeInspectionNativeFailure,
    RuntimeInspectionRelease, RuntimeThreadHistoryDiscardReport,
    SqliteInspectionAcquisitionReleased, SqliteInspectionClosed, SqliteInspectionCopyCustodyError,
    SqliteInspectionCopyLease,
};
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
        &self, closed: &crate::runtime_db::OriginalRuntimeClosed,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
    ) -> Result<()> {
        self.state_lock.ensure_protects_app_root(self.app_root)?;
        self.namespace_lock.ensure_protects(self.runtime_directory)?;
        ensure!(self.phase == OfflineResetAuthorityPhase::IntentPublished,
            "final reset acknowledgement requires original published intent");
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

/// Native close and partial-acquisition release are distinct terminal outcomes.
/// Neither admits physical cleanup or settles the original reservation.
pub enum SqliteInspectionOutcome<T> {
    Closed {
        accounting: Result<T>,
        proof: lillux_sqlite::ClosedSqliteInspection,
    },
    AcquisitionReleased {
        error: anyhow::Error,
        proof: lillux_sqlite::ReleasedSqliteInspection,
    },
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
    /// The selected pinned copy and an erased strong lease for its original
    /// TempDir guard are passed to the callback. A native owner must retain the
    /// lease through its full-release/reaper boundary. On callback error, this
    /// error retains both the callback owner and the same lease, in drop order.
    pub fn inspect_database<T>(
        &self,
        inspect: impl FnOnce(
            Arc<lillux::PinnedDirectory>,
            Arc<dyn Send + Sync>,
            Arc<dyn lillux_sqlite::SqliteInspectionSqlWork>,
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
        let opaque_lease: Arc<dyn Send + Sync> = lease.clone();
        // Claim the one inspection before invoking external decoder code. A
        // reentrant call cannot replace the original accounting provenance.
        *self.inspection_custody.borrow_mut() = Some(lease.clone());
        let sql_work = self
            .copy_work
            .clone()
            .issue_sql_work(&inspection_directory)
            .map_err(|error| {
                self.quota_claim.retain_failure(
                    SqliteInspectionCopyCustodyError::retain(error, lease.clone()),
                    QuotaFailurePhase::Admission,
                )
            })?;
        match inspect(
            Arc::clone(&inspection_directory),
            opaque_lease,
            sql_work.clone(),
        ) {
            Ok(SqliteInspectionOutcome::Closed { accounting, proof }) => {
                let closed = Arc::new(SqliteInspectionClosed::retain(
                    proof,
                    lease.clone(),
                    sql_work,
                    name.to_os_string(),
                    None,
                ));
                // Retain the actual proof BEFORE either join validation or
                // accounting can fail. No claim/operation back-edge owns it.
                *self.inspection_closed.borrow_mut() = Some(closed.clone());
                if let Some(controller) = self.controller_custody {
                    controller.retain(closed.clone());
                }
                closed
                    .require_original()
                    .and_then(|()| accounting)
                    .map(Some)
                    .map_err(|error| {
                        self.quota_claim.retain_failure(
                            closed.retain_failure(error),
                            QuotaFailurePhase::NativeClose,
                        )
                    })
            }
            Ok(SqliteInspectionOutcome::AcquisitionReleased { error, proof }) => {
                let released = Arc::new(SqliteInspectionAcquisitionReleased::retain(
                    proof,
                    lease,
                    sql_work,
                    name.to_os_string(),
                    None,
                ));
                if let Some(controller) = self.controller_custody {
                    controller.retain_acquisition(released.clone());
                }
                // The refusal and proof are preserved even if the context join
                // fails. An acquisition release never supplies accounting.
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
                        _released: released,
                    }),
                    QuotaFailurePhase::NativeClose,
                ))
            }
            Err(error) => Err(self.quota_claim.retain_failure(
                SqliteInspectionCopyCustodyError::retain(error, lease),
                QuotaFailurePhase::Acquisition,
            )),
        }
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
        self.operation
            .require_active(self.state_authority, self.cas_guard)?;
        Ok(self.authority.runtime_directory)
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
    pub runtime_rows: RuntimeThreadHistoryAccounting,
    pub thread_runtime_artifacts: usize,
    pub scheduler_journal_artifacts: usize,
    pub scheduler_rows: ryeos_scheduler_contract::SchedulerFireHistoryAccounting,
    pub replay_indexes: ryeos_state::operational::ReplayIndexResetReport,
    pub projection: ProjectionDiscardReport,
    // In-process native evidence, never serializable cleanup/settlement authority.
    #[serde(skip)]
    inspection_custody: Arc<InspectionControllerCustody>,
}

impl ExecutionHistoryResetReport {
    pub fn total_discarded_rows(&self) -> Option<usize> {
        self.runtime_rows
            .total_rows()?
            .checked_add(self.scheduler_rows.total_rows()?)
    }
}

/// Inspect or execute the explicit all-execution-history reset transaction.
pub fn run_execution_history_reset(
    options: &ExecutionHistoryResetOptions,
    participant: Option<&dyn SchedulerHistoryResetParticipant>,
) -> Result<ExecutionHistoryResetReport> {
    run_execution_history_reset_inner(options, participant, None)
}

pub fn run_execution_history_reset_with_progress(
    options: &ExecutionHistoryResetOptions,
    participant: Option<&dyn SchedulerHistoryResetParticipant>,
    observer: &mut dyn FnMut(&ExecutionHistoryResetProgress),
) -> Result<ExecutionHistoryResetReport> {
    run_execution_history_reset_inner(options, participant, Some(observer))
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
            .field("retained", &self.retained.is_some()).finish()
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
        self.custody.state_projection.lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()).factory = Some(factory);
        OfflineNativeStateProjectionPermit { custody: self.custody }
    }
}
struct OfflineNativeStateProjectionPermit {
    custody: Arc<InspectionControllerCustody>,
}
impl OfflineNativeStateProjectionPermit {
    fn adopt(self, state: ryeos_state::StateDb) -> OfflineStateProjectionLoan {
        // Infallible actual owner handoff after acquisition. The permit is
        // private, noncopyable and issued once before its first native effect.
        OfflineStateProjectionLoan { state: Some(state), custody: self.custody }
    }
}
struct OfflineStateProjectionLoan {
    state: Option<ryeos_state::StateDb>,
    custody: Arc<InspectionControllerCustody>,
}
impl OfflineStateProjectionLoan {
    fn close(mut self, factory: Arc<lillux_sqlite::OriginalNativeFactory>,
        guard: &ryeos_state::recovery::CasMutationGuard,
        operation: Option<&ryeos_state::recovery::ThreadHistoryDiscardOperationRef>,
    ) -> Result<Arc<ryeos_state::state_db::ClosedOriginalState>> {
        let state = self.state.take().expect("live one-use original State loan");
        match state.close_original_projection(factory, guard, operation) {
            Ok(closed) => {
                let closed = Arc::new(closed);
                self.custody.state_projection.lock().unwrap_or_else(|p| p.into_inner())
                    .closed = Some(Arc::clone(&closed));
                Ok(closed)
            }
            Err(failure) => {
                let message = format!("original State retirement remains retained: {failure}");
                self.custody.state_projection.lock().unwrap_or_else(|p| p.into_inner())
                    .close_failure = Some(failure);
                Err(self.custody.retain_failure(anyhow::anyhow!(message)))
            }
        }
    }
}
impl std::ops::Deref for OfflineStateProjectionLoan {
    type Target = ryeos_state::StateDb;
    fn deref(&self) -> &Self::Target {
        self.state.as_ref().expect("live original State projection loan")
    }
}
impl std::ops::DerefMut for OfflineStateProjectionLoan {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.state.as_mut().expect("live original State projection loan")
    }
}
impl Drop for OfflineStateProjectionLoan {
    fn drop(&mut self) {
        if let Some(state) = self.state.take() {
            // Transfer the SAME actual service into the enclosing result/error
            // owner. This performs no native close, terminal ACK or refund.
            // The one-use permit prevents a second safe loan/slot overwrite.
            self.custody.state_projection.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()).retained = Some(state);
        }
    }
}

// One original Runtime service loan; Drop transfers custody only, never SQL.
#[derive(Default)]
struct OfflineRuntimeSlot {
    issued: bool,
    retained: Option<RuntimeDb>,
    closed: Option<Arc<crate::runtime_db::OriginalRuntimeClosed>>,
    finalizer: Option<Arc<crate::runtime_db::OriginalRuntimeFinalizer>>,
    close_failure: Option<crate::runtime_db::OriginalRuntimeCloseFailure>,
}
impl std::fmt::Debug for OfflineRuntimeSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OfflineRuntimeSlot").field("issued", &self.issued)
            .field("retained", &self.retained.is_some()).finish()
    }
}
struct OfflineRuntimePermit { custody: Arc<InspectionControllerCustody> }
impl OfflineRuntimePermit {
    fn adopt(self, runtime: RuntimeDb) -> OfflineRuntimeLoan {
        OfflineRuntimeLoan { runtime: Some(runtime), custody: self.custody }
    }
}
struct OfflineRuntimeLoan {
    runtime: Option<RuntimeDb>,
    custody: Arc<InspectionControllerCustody>,
}
impl OfflineRuntimeLoan {
    fn close(mut self, factory: Arc<lillux_sqlite::OriginalNativeFactory>)
        -> Result<Arc<crate::runtime_db::OriginalRuntimeClosed>> {
        let runtime = self.runtime.take().expect("live one-use Runtime loan");
        match runtime.close_original_runtime(factory) {
            Ok(closed) => {
                let closed = Arc::new(closed);
                // Actual terminal custody is transferred before caller checks.
                self.custody.runtime_mutation.lock().unwrap_or_else(|p| p.into_inner())
                    .closed = Some(Arc::clone(&closed));
                Ok(closed)
            }
            Err(failure) => {
                let message = format!("original Runtime close remains retained: {failure}");
                self.custody.runtime_mutation.lock().unwrap_or_else(|p| p.into_inner())
                    .close_failure = Some(failure);
                Err(self.custody.retain_failure(anyhow::anyhow!(message)))
            }
        }
    }
}
impl std::ops::Deref for OfflineRuntimeLoan {
    type Target = RuntimeDb;
    fn deref(&self) -> &RuntimeDb { self.runtime.as_ref().expect("live original Runtime loan") }
}
impl std::ops::DerefMut for OfflineRuntimeLoan {
    fn deref_mut(&mut self) -> &mut RuntimeDb { self.runtime.as_mut().expect("live original Runtime loan") }
}
impl Drop for OfflineRuntimeLoan {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            self.custody.runtime_mutation.lock().unwrap_or_else(|p| p.into_inner()).retained = Some(runtime);
        }
    }
}

#[derive(Default)]
struct OriginalNativeCensusSlot {
    terminals: Option<Vec<lillux_sqlite::OriginalNativeConnectionTerminal>>,
    cleanup_inputs: Option<lillux_sqlite::OriginalNativeCleanupInputs>,
}
impl std::fmt::Debug for OriginalNativeCensusSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OriginalNativeCensusSlot").field("retained", &self.terminals.is_some()).finish()
    }
}

/// Operation-level custody has no strong back-edge from a claim or operation.
/// Both successful reports and errors retain the same actual proof objects.
#[derive(Debug, Default)]
struct InspectionControllerCustody {
    native_backend: std::sync::Mutex<native_backend::OriginalBackendSlot>,
    operational_inspection: std::sync::Mutex<operational_owner::OfflineOperationalSlot>,
    operational_mutation: std::sync::Mutex<operational_owner::OfflineOperationalSlot>,
    operational_outside: std::sync::Mutex<[operational_owner::OfflineOperationalOutsideSlot; 2]>,
    state_projection: std::sync::Mutex<OfflineStateProjectionSlot>,
    runtime_mutation: std::sync::Mutex<OfflineRuntimeSlot>,
    runtime_outside: std::sync::Mutex<Option<crate::runtime_db::OriginalOutsideRuntimeFinalizer>>,
    native_census: std::sync::Mutex<OriginalNativeCensusSlot>,
    closed: std::sync::Mutex<Vec<Arc<SqliteInspectionClosed>>>,
    cleanup: std::sync::Mutex<Vec<Arc<InspectionCopyCleanup>>>,
    completed_disposals: std::sync::Mutex<Vec<Arc<InspectionCopyDisposal>>>,
    settlement: std::sync::Mutex<Option<Arc<OriginalInspectionSettlement>>>,
    cleanup_failures: std::sync::Mutex<Vec<anyhow::Error>>,
    runtime_released: std::sync::Mutex<Vec<Arc<RuntimeInspectionRelease>>>,
    acquisition_released: std::sync::Mutex<Vec<Arc<SqliteInspectionAcquisitionReleased>>>,
}
impl InspectionControllerCustody {
    fn retire_redundant_census_aliases(
        &self,
        post_ack: &original_quota_claim::OriginalPostAckFinalization,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        aggregate: &Arc<lillux_sqlite::OriginalNativeTerminal>,
        canonical: &lillux_sqlite::OriginalNativeConnectionTerminal,
    ) -> Result<()> {
        factory.require_original_process()?;
        aggregate.require_original(factory)?;
        canonical.require_original(factory)?;
        post_ack.retire_preparation_terminal_alias(factory, aggregate, canonical)?;
        let same = |original: &lillux_sqlite::OriginalNativeConnectionTerminal| match (original, canonical) {
            (lillux_sqlite::OriginalNativeConnectionTerminal::Closed(a),
             lillux_sqlite::OriginalNativeConnectionTerminal::Closed(b)) => Arc::ptr_eq(a, b),
            (lillux_sqlite::OriginalNativeConnectionTerminal::Released(a),
             lillux_sqlite::OriginalNativeConnectionTerminal::Released(b)) => Arc::ptr_eq(a, b),
            _ => false,
        };
        let census_count = self.native_census.try_lock()
            .map_err(|_| anyhow!("actual native census alias quote busy or poisoned"))?
            .terminals.as_ref().map_or(0, Vec::len);
        let closed_count = self.closed.try_lock().map_err(|_| anyhow!("inspection alias quote busy or poisoned"))?.len();
        let acquired_count = self.acquisition_released.try_lock().map_err(|_| anyhow!("acquisition alias quote busy or poisoned"))?.len();
        let runtime_count = self.runtime_released.try_lock().map_err(|_| anyhow!("Runtime inspection alias quote busy or poisoned"))?.len();
        let wrappers = closed_count.checked_add(acquired_count).and_then(|count| count.checked_add(runtime_count))
            .context("original inspection alias census overflow")?;
        // Quote both census partition vectors, all three wrapper snapshots,
        // and their actual scans before any clone or retirement. The checks
        // below hold all inventory gates until the bounded snapshots exist.
        let bytes = u64::try_from(census_count)?.checked_mul(64)
            .and_then(|bytes| bytes.checked_add(u64::try_from(wrappers).ok()?.checked_mul(32)?))
            .and_then(|bytes| bytes.checked_add(1024))
            .context("original census alias storage overflow")?;
        let steps = u64::try_from(census_count)?.checked_mul(4)
            .and_then(|steps| steps.checked_add(u64::try_from(wrappers).ok()?.checked_mul(8)?))
            .and_then(|steps| steps.checked_add(8))
            .context("original census alias work overflow")?;
        post_ack.prepay_outside_continuation(bytes, steps)?;
        let census_guard = self.native_census.try_lock().map_err(|_| anyhow!("native alias snapshot busy or poisoned"))?;
        let closed_guard = self.closed.try_lock().map_err(|_| anyhow!("inspection alias snapshot busy or poisoned"))?;
        let acquired_guard = self.acquisition_released.try_lock().map_err(|_| anyhow!("acquisition alias snapshot busy or poisoned"))?;
        let runtime_guard = self.runtime_released.try_lock().map_err(|_| anyhow!("Runtime inspection alias snapshot busy or poisoned"))?;
        ensure!(census_guard.terminals.as_ref().map_or(0, Vec::len) == census_count
            && closed_guard.len() == closed_count && acquired_guard.len() == acquired_count
            && runtime_guard.len() == runtime_count,
            "actual alias census changed after its original precharge");
        let closed = closed_guard.clone();
        let acquired = acquired_guard.clone();
        let runtime = runtime_guard.clone();
        drop(runtime_guard);
        drop(acquired_guard);
        drop(closed_guard);
        drop(census_guard);
        for owner in closed {
            let actual = owner.original_native_terminal()?;
            if actual.as_ref().is_some_and(same) { owner.retire_redundant_native_alias(factory, aggregate, canonical)?; }
        }
        for owner in acquired {
            let actual = owner.original_native_terminal()?;
            if actual.as_ref().is_some_and(same) { owner.retire_redundant_native_alias(factory, aggregate, canonical)?; }
        }
        for owner in runtime {
            match owner.as_ref() {
                RuntimeInspectionRelease::Closed(owner) => {
                    let actual = owner.original_native_terminal()?;
                    if actual.as_ref().is_some_and(same) { owner.retire_redundant_native_alias(factory, aggregate, canonical)?; }
                }
                RuntimeInspectionRelease::Acquisition(owner) => {
                    let actual = owner.original_native_terminal()?;
                    if actual.as_ref().is_some_and(same) { owner.retire_redundant_native_alias(factory, aggregate, canonical)?; }
                }
            }
        }
        let mut census = self.native_census.try_lock()
            .map_err(|_| anyhow!("actual native alias partition busy or poisoned"))?;
        ensure!(census.terminals.as_ref().map_or(0, Vec::len) <= census_count,
            "actual native census grew beyond original alias quote");
        let original = census.terminals.take().context("actual original native terminal census absent")?;
        let mut retained = Vec::with_capacity(original.len());
        let mut redundant = Vec::with_capacity(original.len());
        for original in original {
            if same(&original) { redundant.push(original); } else { retained.push(original); }
        }
        census.terminals = Some(retained);
        let cleanup_alias = if census.cleanup_inputs.as_ref().is_some_and(|inputs| same(inputs.terminal())) {
            census.cleanup_inputs.take()
        } else { None };
        drop(census);
        // The actual canonical terminal still owns every body/resource. Only
        // matching redundant aliases end here, outside bookkeeping gates.
        drop(cleanup_alias);
        drop(redundant);
        Ok(())
    }
    fn retire_runtime_native_references(
        &self,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        aggregate: &Arc<lillux_sqlite::OriginalNativeTerminal>,
        retire_aliases: &dyn Fn(&lillux_sqlite::OriginalNativeConnectionTerminal) -> Result<()>,
    ) -> Result<()> {
        factory.require_original_process()?;
        let mut outside = self.runtime_outside.try_lock()
            .map_err(|_| anyhow!("actual outside Runtime checkout busy or poisoned"))?
            .take().context("actual outside Runtime token absent")?;
        let result = outside.retire_native_references(factory, aggregate, retire_aliases);
        *self.runtime_outside.lock().unwrap_or_else(|p| p.into_inner()) = Some(outside);
        result
    }
    fn require_runtime_and_operational_roots_registered(
        &self, post_ack: &original_quota_claim::OriginalPostAckFinalization,
    ) -> Result<()> {
        post_ack.with_original_physical_registry(&mut |registry, prepay| {
            post_ack.require_state_shared_roots_registered(registry, prepay)?;
            let runtime = self.runtime_outside.try_lock()
                .map_err(|_| anyhow!("Runtime registration census busy or poisoned"))?;
            runtime.as_ref().context("actual Runtime outside continuation absent")?
                .require_shared_roots_registered(registry, prepay)?;
            let operational = self.operational_outside.try_lock()
                .map_err(|_| anyhow!("Operational registration census busy or poisoned"))?;
            for index in [0usize, 1usize] {
                operational[index].owner.as_ref().context("actual Operational phase continuation absent")?
                    .require_shared_roots_registered(registry, prepay)?;
            }
            Ok(())
        })
    }
    fn register_runtime_shared_roots(
        &self,
        post_ack: &original_quota_claim::OriginalPostAckFinalization,
    ) -> Result<()> {
        let mut actual = self.runtime_outside.try_lock()
            .map_err(|_| anyhow!("actual Runtime physical registration busy or poisoned"))?
            .take().context("actual outside Runtime token absent")?;
        let result = (|| -> Result<()> {
            // SAFETY: the same retained original supervisor caches the whole
            // original resource packet before any namespace registration.
            let packets = unsafe { actual.take_original_sql_custody() }?;
            post_ack.retain_original_sql_custody(packets)?;
            post_ack.with_original_physical_registry(&mut |registry, prepay| {
                // SAFETY: same actual original account/clock and finite
                // registry, with both owners outside their bookkeeping gates.
                unsafe { actual.register_shared_roots(registry, prepay) }
            })
        })();
        *self.runtime_outside.lock().unwrap_or_else(|p| p.into_inner()) = Some(actual);
        result
    }
    fn transfer_runtime_outside(
        &self,
        finalizer: Arc<crate::runtime_db::OriginalRuntimeFinalizer>,
        factory: &Arc<lillux_sqlite::OriginalNativeFactory>,
        terminal: &Arc<lillux_sqlite::OriginalNativeTerminal>,
    ) -> Result<()> {
        factory.require_original_process()?;
        terminal.require_original(factory)?;
        let mut destination = self.runtime_outside.try_lock()
            .map_err(|_| anyhow!("outside Runtime registration busy or poisoned"))?;
        ensure!(destination.is_none(), "actual outside Runtime already registered");
        let mut slot = self.runtime_mutation.try_lock()
            .map_err(|_| anyhow!("original Runtime handoff custody busy or poisoned"))?;
        ensure!(slot.retained.is_none() && slot.closed.is_none() && slot.close_failure.is_none(),
            "Runtime still has actual live/refused/closed consumer custody");
        ensure!(slot.finalizer.as_ref().is_some_and(|actual| Arc::ptr_eq(actual, &finalizer)),
            "Runtime handoff selects another actual finalizer");
        let alias = slot.finalizer.take();
        drop(slot);
        drop(destination);
        drop(alias);
        // SAFETY: the same original post-ACK envelope prepaid this consuming
        // owner, Runtime is synchronously quiescent, its real directory receipt
        // is retained, and only the redundant controller alias was removed.
        match unsafe { finalizer.into_outside_finalizer(terminal) } {
            Ok(outside) => {
                *self.runtime_outside.lock().unwrap_or_else(|p| p.into_inner()) = Some(outside);
                Ok(())
            }
            Err((actual, error)) => {
                self.runtime_mutation.lock().unwrap_or_else(|p| p.into_inner()).finalizer = Some(actual);
                Err(error)
            }
        }
    }
    fn retire_redundant_closed_state_alias(
        &self,
        state: &Arc<ryeos_state::state_db::ClosedOriginalState>,
        finalizer: &Arc<ryeos_state::state_db::OriginalStateFinalizer>,
    ) -> Result<()> {
        state.require_original_finalizer(finalizer)?;
        let mut slot = self.state_projection.try_lock()
            .map_err(|_| anyhow!("original State controller alias custody busy or poisoned"))?;
        ensure!(slot.retained.is_none() && slot.close_failure.is_none(),
            "State controller still owns a live or refused consumer");
        ensure!(slot.closed.as_ref().is_some_and(|actual| Arc::ptr_eq(actual, state)),
            "State outside handoff selects another controller owner");
        // The genuine post-ACK producer already retains this exact State and
        // ACK. Removing this redundant Arc closes no descriptor or resource.
        let original_alias = slot.closed.take();
        drop(slot);
        drop(original_alias);
        Ok(())
    }
    // Called only after actual State ACK has been cached and this producer's
    // finite storage/transfer work prepaid on the original post-ACK envelope.
    fn begin_runtime_finalizer(self: &Arc<Self>) -> Result<Arc<crate::runtime_db::OriginalRuntimeFinalizer>> {
        let closed = {
            let mut slot = self.runtime_mutation.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(finalizer) = &slot.finalizer { return Ok(finalizer.clone()); }
            slot.closed.take().context("actual closed Runtime finalizer owner absent")?
        };
        // This obtains an actual consuming owner; uniqueness is NOT a close
        // receipt. Refusal restores the SAME closed owner without effects.
        let closed = match Arc::try_unwrap(closed) {
            Ok(closed) => closed,
            Err(closed) => {
                self.runtime_mutation.lock().unwrap_or_else(|p| p.into_inner()).closed = Some(closed);
                return Err(self.retain_failure(anyhow!("actual closed Runtime still has other owners")));
            }
        };
        match closed.into_finalizer() {
            Ok(finalizer) => {
                self.runtime_mutation.lock().unwrap_or_else(|p| p.into_inner()).finalizer = Some(finalizer.clone());
                Ok(finalizer)
            }
            Err((closed, error)) => {
                self.runtime_mutation.lock().unwrap_or_else(|p| p.into_inner()).closed = Some(Arc::new(closed));
                Err(self.retain_failure(error))
            }
        }
    }
    fn retain_native_census(&self, mut terminals: Vec<lillux_sqlite::OriginalNativeConnectionTerminal>) -> Result<()> {
        self.append_operational_terminals(&mut terminals)?;
        let closed = self.closed.lock().unwrap_or_else(|p| p.into_inner()).clone();
        for owner in closed { if let Some(terminal) = owner.original_native_terminal()? { terminals.push(terminal); } }
        let acquired = self.acquisition_released.lock().unwrap_or_else(|p| p.into_inner()).clone();
        for owner in acquired { if let Some(terminal) = owner.original_native_terminal()? { terminals.push(terminal); } }
        let released = self.runtime_released.lock().unwrap_or_else(|p| p.into_inner()).clone();
        for owner in released {
            let terminal = match owner.as_ref() {
                RuntimeInspectionRelease::Closed(owner) => owner.original_native_terminal()?,
                RuntimeInspectionRelease::Acquisition(owner) => owner.original_native_terminal()?,
            };
            if let Some(terminal) = terminal { terminals.push(terminal); }
        }
        let mut slot = self.native_census.lock().unwrap_or_else(|p| p.into_inner());
        ensure!(slot.terminals.is_none(), "original native census already retained");
        // Store genuine role terminals only; this is not cardinality approval,
        // a backend terminal, physical disposal or original settlement.
        slot.terminals = Some(terminals);
        Ok(())
    }
    fn begin_runtime_mutation(self: &Arc<Self>) -> Result<OfflineRuntimePermit> {
        let mut slot = self.runtime_mutation.lock().unwrap_or_else(|p| p.into_inner());
        ensure!(!slot.issued && slot.retained.is_none(), "original Runtime owner already issued");
        slot.issued = true;
        Ok(OfflineRuntimePermit { custody: Arc::clone(self) })
    }
    fn begin_state_projection(self: &Arc<Self>) -> Result<OfflineStateProjectionPermit> {
        let mut slot = self.state_projection.lock().unwrap_or_else(|p| p.into_inner());
        ensure!(!slot.issued && slot.retained.is_none(), "original State projection owner already issued");
        slot.issued = true;
        Ok(OfflineStateProjectionPermit { custody: Arc::clone(self) })
    }
    fn retain(&self, closed: Arc<SqliteInspectionClosed>) {
        // Bookkeeping only; poisoning cannot discard acquired native evidence.
        self.closed
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push(closed);
    }
    fn retain_acquisition(&self, released: Arc<SqliteInspectionAcquisitionReleased>) {
        self.acquisition_released
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push(released);
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
                    .and_then(|release| self.retain_runtime_release(release))
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
        let acquired = self
            .acquisition_released
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
        for owner in acquired {
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
        let acquired = self
            .acquisition_released
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone();
        let runtime = self
            .runtime_released
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone();
        let mut cleanup = Vec::with_capacity(closed.len() + acquired.len() + runtime.len());
        for owner in closed {
            let owner = InspectionCopyCleanup::for_closed(owner)?;
            self.cleanup
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .push(owner.clone());
            cleanup.push(owner);
        }
        for owner in acquired {
            let owner = InspectionCopyCleanup::for_acquisition(owner)?;
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
        let retained = self.native_census.lock().unwrap_or_else(|p| p.into_inner())
            .cleanup_inputs.clone();
        let inputs = if let Some(inputs) = retained {
            inputs.require_original(factory)?;
            inputs
        } else {
            self.acknowledge_producer_terminals()?;
            let preparations = claim.close_native_admission_and_retire_preparations(factory)?;
            {
                let mut census = self.native_census.lock().unwrap_or_else(|p| p.into_inner());
                let terminals = census.terminals.as_mut()
                    .context("actual original terminal census absent before producer drain")?;
                for preparation in preparations {
                    let present = terminals.iter().any(|original| match (original, &preparation) {
                        (lillux_sqlite::OriginalNativeConnectionTerminal::Closed(a),
                         lillux_sqlite::OriginalNativeConnectionTerminal::Closed(b)) => Arc::ptr_eq(a, b),
                        (lillux_sqlite::OriginalNativeConnectionTerminal::Released(a),
                         lillux_sqlite::OriginalNativeConnectionTerminal::Released(b)) => Arc::ptr_eq(a, b),
                        _ => false,
                    });
                    if !present { terminals.push(preparation); }
                }
            }
            let seal = claim.seal_production(work)?;
            let terminal = self.native_census.lock().unwrap_or_else(|p| p.into_inner())
                .terminals.as_ref().and_then(|terminals| terminals.first()).cloned()
                .context("original native cleanup lacks a genuine closed role terminal")?;
            let inputs = seal.issue_native_cleanup(factory, terminal)?;
            // Retain the actual signed seal/clock/factory inputs before any
            // stopped Binding capture or native cleanup effect can occur.
            self.native_census.lock().unwrap_or_else(|p| p.into_inner())
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
                self.cleanup_failures.lock().unwrap_or_else(|p| p.into_inner())
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
        settlement.prepare_terminal()?;
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

#[derive(Debug)]
struct SchedulerAcquisitionReleaseFailure {
    error: anyhow::Error,
    _released: Arc<SqliteInspectionAcquisitionReleased>,
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
) -> Result<ExecutionHistoryResetReport> {
    let custody = Arc::new(InspectionControllerCustody::default());
    run_execution_history_reset_owned(options, participant, observer, &custody)
        .map_err(|error| custody.retain_failure(error))
}

fn run_execution_history_reset_owned(
    options: &ExecutionHistoryResetOptions,
    participant: Option<&dyn SchedulerHistoryResetParticipant>,
    mut observer: Option<&mut dyn FnMut(&ExecutionHistoryResetProgress)>,
    inspection_custody: &Arc<InspectionControllerCustody>,
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

    // This is the outer ownership proof. A running daemon and every
    // cooperating standalone state service hold the same lock.
    let state_lock_path = default_lock_path(&config.app_root);
    let state_lock = if options.dry_run {
        StateLock::acquire_existing_read_only(&state_lock_path)
    } else {
        StateLock::acquire(&state_lock_path)
    }
    .context("execution-history reset requires the daemon to be stopped")?;
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
        &state_lock,
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
    quota_claim
        .require_service_window()
        .map_err(|error| quota_claim.retain_failure(error, QuotaFailurePhase::Admission))?;
    // Only the private authentic issuer can initialize the original native
    // owner. Its exact claim, generation, clock and aggregate budget are shared
    // by State, Runtime and scheduler; no second envelope/default is issued.
    let state_projection_permit = inspection_custody.begin_state_projection()
        .map_err(|error| quota_claim.retain_failure(error, QuotaFailurePhase::Admission))?;
    let native_factory = quota_claim
        .issue_native_factory(&offline_policy, &identity)
        .map_err(|error| inspection_custody.retry_native_initialization_refusal(
            quota_claim.retain_failure(error, QuotaFailurePhase::Admission)))?;
    let state_projection_permit = state_projection_permit
        .retain_factory(Arc::clone(&native_factory));
    let offline_lease_authority = quota_claim.native_projection_lease_authority(&native_factory)
        .map_err(|error| quota_claim.retain_failure(error, QuotaFailurePhase::Admission))?;
    // SAFETY: the genuine distinct offline claim/admission retains its actual
    // signed debit, state/namespace/policy, sole resident budget and original
    // service/cleanup endpoints. This exact authority selects the same factory;
    // State owns actual generation consumers and the continuously held CAS guard.
    let state_db = unsafe { prepared_state.open_projection(
        Arc::clone(&native_factory), offline_lease_authority, &cas_guard) }
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
    let (
        mut runtime_db,
        observed_runtime_schema_epoch,
        runtime_schema_reset_required,
        inspected_runtime_rows,
    ) = if options.dry_run {
        let db = open_inspection_runtime_db(
            &config,
            &runtime_directory,
            &runtime_directory_lock,
            copy_work.clone(),
            Arc::clone(&native_factory),
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
            Ok((epoch, reset_required, accounting))
        })();
        match db.close_owned_inspection() {
            Ok(closed) => inspection_custody.retain(closed),
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
        let (epoch, reset_required, accounting) = inspected
            .map_err(|error| quota_claim.retain_failure(error, QuotaFailurePhase::Acquisition))?;
        (None, epoch, reset_required, Some(accounting))
    } else {
        let permit = inspection_custody.begin_runtime_mutation()
            .map_err(|error| quota_claim.retain_failure(error, QuotaFailurePhase::Admission))?;
        let db = open_original_runtime_db(&config, &runtime_directory, &runtime_directory_lock,
            &quota_claim, Arc::clone(&native_factory))
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
        &inspection_custody, &quota_claim, &native_factory, &runtime_directory,
        &runtime_directory_lock, OperationalOwnerPhase::Inspection,
    ).context("admit strict readonly Operational inspection before intent")?;
    let replay_indexes = operational_inspection.report()?;
    let stable_profiles = operational_inspection.credential_profiles()
        .context("validate stable credential-profile authority before history retirement")?;
    // Keep this actual inspector's terminal. A later writable owner is a
    // distinct original ticket, not a readonly-to-write phase conversion.
    operational_inspection.close(Arc::clone(&native_factory))
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
        let runtime_terminal = if let Some(runtime) = runtime_db.take() {
            Some(runtime.close(Arc::clone(&native_factory))
                .context("close original Runtime for already-current schema cut")?.terminal())
        } else { None };
        let closed_state = state_db.close(Arc::clone(&native_factory), &cas_guard,
            active_operation.as_ref().map(|operation| operation.reference()))
            .context("close original State for preview/already-current report")?;
        // SAFETY: the stopped-node State/namespace owner runs this operation
        // synchronously. Its consuming State close has retired selected and
        // generated SQL, and preview Runtime has closed above; no generation
        // consumer escapes this controller. Keep the actual owner and CAS frame.
        let leases = unsafe { closed_state.close_generation_leases_after_consumers(&cas_guard) }?;
        inspection_custody.retain_native_leases(&native_factory, leases)?;
        let mut native_terminals = closed_state.native_terminals()?;
        if let Some(terminal) = runtime_terminal { native_terminals.push(terminal); }
        inspection_custody.retain_native_census(native_terminals)?;
        let settlement = inspection_custody
            .prepare_inspection_settlement(&quota_claim, &copy_work, &native_factory)
            .context("prepare original two-store inspection settlement")?;
        inspection_custody.finish_native_backend(&native_factory)
            .context("finish actual original backend after preview cleanup scope")?;
        settlement
            .require_completed()
            .context("original inspection settlement is not complete")?;
        publish_progress(&mut observer, ExecutionHistoryResetPhase::Complete, None);
        return Ok(ExecutionHistoryResetReport {
            inspection_custody: inspection_custody.clone(),
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
            runtime_rows: runtime_preview,
            thread_runtime_artifacts: thread_runtime_preview,
            scheduler_journal_artifacts: scheduler_journal_preview,
            scheduler_rows: scheduler_preview,
            replay_indexes,
            projection: ProjectionDiscardReport {
                superseded_instances_deleted: authoritative_preview.superseded_projection_instances,
                ..ProjectionDiscardReport::default()
            },
        });
    }

    let mut runtime_db = runtime_db
        .take()
        .context("mutating reset lost original runtime owner")?;
    let mut reset_authority = OfflineResetAuthority::capture(
        &state_lock,
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
    operation.require_quota_claim(&quota_claim)
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
                    .require_active(&state_authority, &cas_guard)
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
    // Only after durable intent may any replay rows or enrolling ceremonies
    // be retired. A crash leaves the existing discard marker for safe retry.
    let operational_db = {
        let verify_operational_mutation = || -> Result<()> {
            operation.require_quota_claim(&quota_claim)?;
            operation.require_active(&state_authority, &cas_guard)?;
            reset_authority.authorize_mutation(&runtime_db)
        };
        // SAFETY: this borrowed verifier retains the authenticated staged
        // operation, its exact funded claim, real offline mutation frame and
        // exclusive State/CAS guard. It validates the actual durable marker,
        // and none of these owners is moved into a native callback.
        let authority = unsafe {
            ryeos_state::OriginalOperationalMutationAuthority::issued(
                &runtime_directory, &native_factory, &verify_operational_mutation,
            )
        }?;
        // No writable Operational native open is permitted before the actual
        // intent check above. The original claim stays Producing; counters and
        // factory are not renewed between readonly and mutating tickets.
        let mut owner = operational_owner::open_original_operational(
            &inspection_custody, &quota_claim, &native_factory, &runtime_directory,
            &runtime_directory_lock, OperationalOwnerPhase::Mutation,
        ).context("admit original Operational mutation after durable intent")?;
        anyhow::ensure!(owner.report()? == replay_indexes,
            "Operational retirement decision changed between original inspection and intent");
        loop {
            quota_claim.require_service_window()?;
            verify_operational_mutation()?;
            match owner.activate(&authority, replay_indexes) {
                Ok(()) => break,
                Err(error) => {
                    eprintln!("execution-history reset retains its original replay owner and offline frame while retrying activation: {error:#}");
                    std::thread::sleep(std::time::Duration::from_secs(1));
                }
            }
        }
        for profile in &stable_profiles {
            owner.merge_credential_profile(&authority, profile)
                .context("preserve stable credential authority and invalidate retired enrollment")?;
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
    };
    let mut scheduler_retirement_attempt = None;
    let mut scheduler_retirement = match participant {
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
            // CAS guard. An attempt error is diagnostic only; retry that same
            // cursor and never stringify/drop its native owner.
            let mut last_retry_notice: Option<std::time::Instant> = None;
            let retirement = loop {
                match attempt.advance(&permit) {
                    Ok(Some(retirement)) => break retirement,
                    Ok(None) => std::thread::sleep(std::time::Duration::from_millis(10)),
                    Err(error) => {
                        if last_retry_notice
                            .as_ref()
                            .is_none_or(|last| last.elapsed() >= std::time::Duration::from_secs(5))
                        {
                            eprintln!(
                                "execution-history reset retains its original State/CAS authority while retrying scheduler retirement: {error:#}"
                            );
                            last_retry_notice = Some(std::time::Instant::now());
                        }
                        std::thread::sleep(std::time::Duration::from_secs(1));
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
                    eprintln!(
                        "execution-history reset retains its original State/CAS authority and scheduler attempt while retrying terminal completion: {error:#}"
                    );
                    std::thread::sleep(std::time::Duration::from_secs(1));
                }
            }
        }
    }
    // End every borrowed scheduler frame before consuming the actual Runtime.
    // Its own terminal completion succeeded above; failed completion still
    // retains the original live frame and never reaches this handoff.
    drop(scheduler_retirement_attempt);
    drop(permit);
    operational_db.close(Arc::clone(&native_factory))
        .context("close original Operational mutation before terminal census")?;
    let closed_runtime = runtime_db.close(Arc::clone(&native_factory))
        .context("close original Runtime before final reset acknowledgement")?;
    reset_authority.require_closed_runtime(&closed_runtime, &native_factory)?;
    let closed_state = state_db.close(Arc::clone(&native_factory), &cas_guard, Some(operation.reference()))
        .context("consume original State projection before terminal cleanup")?;
    // SAFETY: the actual offline owner stopped all synchronous State work;
    // scheduler/Operational/Runtime consuming closes completed above, selected
    // and generated State SQL is in this genuine closed owner, and no escaped
    // generation task remains. Retain that same owner and exclusive CAS frame.
    let leases = unsafe { closed_state.close_generation_leases_after_consumers(&cas_guard) }?;
    inspection_custody.retain_native_leases(&native_factory, leases)?;
    let mut native_terminals = closed_state.native_terminals()?;
    native_terminals.push(closed_runtime.terminal());
    inspection_custody.retain_native_census(native_terminals)?;

    // Copied-role cleanup must finish under original custody BEFORE removing
    // the active cross-store marker. Failure keeps the actual operation active.
    let inspection_settlement = inspection_custody
        .prepare_inspection_settlement(&quota_claim, &copy_work, &native_factory)
        .context(
            "prepare original two-store inspection settlement before final reset acknowledgement",
        )?;
    let native_terminal = inspection_custody.finish_native_backend(&native_factory)
        .context("finish actual original backend after mutating cleanup scope")?;
    let ready_for_final_ack = inspection_settlement
        .prepare_final_ack(&native_factory, &native_terminal)
        .context("prepare actual source retirement and retained funded final-ACK lifelines")?;
    // Runtime/copy close alone cannot acknowledge the complete original
    // native owner. Keep the marker until the genuine exhaustive census,
    // physical arena release and original issuer acknowledgement are joined.
    native_terminal.require_original(&native_factory)
        .context("original native issuer terminal unacknowledged; retain active State marker")?;

    // All store mutations have committed above, but the cross-store discard
    // marker must remain until scheduler terminal evidence is durable. A crash
    // during completion must not advertise a finished reset while leaving an
    // interrupted scheduler generation. Marker removal is the final ack.
    let verify_final_ack = || -> Result<()> {
        ready_for_final_ack.require_original(&quota_claim, &copy_work, &native_factory, &native_terminal)?;
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
            &closed_state, &cas_guard, &native_terminal, operation.reference(),
            &verify_final_ack, &prepay_final_ack,
        )
    }?;
    let state_acknowledgement = loop {
        // Original cleanup custody bounds retries; never renew service or
        // replace the failed owner with a new operation/funding envelope.
        verify_final_ack()?;
        match closed_state.finish_thread_history_discard_admitted(&final_ack_authority) {
            Ok(receipt) => break receipt,
            Err(error) => {
                eprintln!("execution-history reset retains its original State/CAS authority and funded finalizer while retrying State acknowledgement: {error:#}");
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        }
    };
    state_acknowledgement.require_original(&closed_state, &native_factory, &native_terminal, operation.reference())?;
    // Retain the actual State acknowledgement and its still-funded owners in
    // the existing settlement controller before fallible post-ACK checks.
    // This join does not close a root, release a reservation or erase a debt.
    let post_ack_finalization = inspection_settlement.begin_post_ack_finalization(
        &ready_for_final_ack,
        &closed_state,
        &state_acknowledgement,
        operation.reference(),
    ).context("retain actual State ACK under original post-ACK finalizer custody")?;
    post_ack_finalization.require_original(
        &ready_for_final_ack,
        &closed_state,
        &state_acknowledgement,
        operation.reference(),
    )?;
    // End the borrowed ACK verifier before consuming Runtime's actual roots.
    // The same State/ACK/Ready proof remains cached in settlement custody.
    drop(final_ack_authority);
    drop(closed_runtime);
    post_ack_finalization.prepay_finalizer_transfer(2048, 32)?;
    let runtime_finalizer = inspection_custody.begin_runtime_finalizer()?;
    let runtime_directory = runtime_finalizer.transfer_directory()?;
    let runtime_directory_inputs = post_ack_finalization.issue_runtime_retirement_inputs(
        &runtime_finalizer, &runtime_directory,
    )?;
    // Actual native close occurs outside all controller/producer bookkeeping
    // locks. A refusal retains the same transferred owner in the producer.
    let runtime_directory_receipt = runtime_directory.retire_after_backend(
        &native_factory, &native_terminal, &runtime_directory_inputs,
    )?;
    runtime_finalizer.retain_directory_retirement(Arc::new(runtime_directory_receipt))?;
    drop(runtime_directory_inputs);
    // Its namespace still has actual original work/native-authority leases.
    // Keep that obligation; closing this one directory cannot imply settlement.

    let prepay_state_finalizer = |memory_bytes, cleanup_steps| {
        post_ack_finalization.prepay_finalizer_transfer(memory_bytes, cleanup_steps)
    };
    // SAFETY: the actual ACK, operation, native terminal and continuously held
    // CAS frame select this State. The callback charges the same retained
    // post-ACK funding/clock before producer allocation or owner handoff.
    let state_finalizer = unsafe {
        closed_state.begin_finalizer_retirement(
            &state_acknowledgement, &native_terminal, operation.reference(),
            &cas_guard, &prepay_state_finalizer,
        )
    }?;
    closed_state.require_original_finalizer(&state_finalizer)?;
    for index in 0..5 {
        post_ack_finalization.prepay_finalizer_transfer(512, 4)?;
        // SAFETY: each finite root owner/receipt allocation is prepaid above,
        // and the same actual State caches this genuine retained producer.
        let owner = unsafe { state_finalizer.transfer_root(index) }?;
        let inputs = post_ack_finalization.issue_state_retirement_inputs(&state_finalizer, &owner)?;
        // Actual close runs outside State/controller/producer bookkeeping.
        // The producer already retains each same original descriptor owner.
        let receipt = owner.retire_after_backend(&native_factory, &native_terminal, &inputs)?;
        state_finalizer.retain_root_retirement(index, Arc::new(receipt))?;
    }
    // Recovery/preparation roots, generated holders, projection leases and
    // namespace ownership remain in the State producer. These five real close
    // receipts cannot stand in for those other owners or financial settlement.

    // Retain the same native admission's actual State lease, namespace and
    // policy in post-ACK custody. Its old service/native callbacks now refuse
    // access; ownership transfer alone is not physical closure or settlement.
    let native_admission_lifelines = post_ack_finalization.transfer_native_admission_lifelines()?;
    native_admission_lifelines.require_original(&post_ack_finalization)?;

    let finalizer_master = post_ack_finalization.enter_finalizer_master()?;
    finalizer_master.require_original(&post_ack_finalization)?;
    // SAFETY: this actual ACKed State retains the same genuine producer.
    // The remaining original ledger/reservation/policy/clock master charges
    // the real preparation handoff; no service/native admission is renewed.
    unsafe { state_finalizer.transfer_preparation(&prepay_state_finalizer) }?;
    closed_state.require_original_finalizer(&state_finalizer)?;
    for index in 5..10 {
        post_ack_finalization.prepay_finalizer_transfer(512, 4)?;
        // SAFETY: each owner/receipt allocation is paid by the same original
        // post-ACK master before moving its private actual preparation slot.
        let owner = unsafe { state_finalizer.transfer_root(index) }?;
        let inputs = post_ack_finalization.issue_state_retirement_inputs(&state_finalizer, &owner)?;
        let receipt = owner.retire_after_backend(&native_factory, &native_terminal, &inputs)?;
        state_finalizer.retain_root_retirement(index, Arc::new(receipt))?;
    }
    // Native preparation callbacks now refuse; its namespace/recovery/trust
    // still belong to the State producer and are not retired by these roots.

    // Consume the exact ACKed State after its ten actual root retirements.
    // The post-ACK producer is retained in the settlement/controller before
    // removing redundant caller/controller aliases. Refusal stores the whole
    // authentic State/ACK/finalizer tuple for an original-custody retry.
    inspection_custody.retire_redundant_closed_state_alias(&closed_state, &state_finalizer)?;
    drop(state_acknowledgement);
    drop(closed_state);
    post_ack_finalization.transfer_state_outside(Some(state_finalizer), &cas_guard)?;
    // The same signed envelope prepays Runtime's actual outside packet before
    // consuming its sole finalizer; controller custody precedes late effects.
    post_ack_finalization.prepay_outside_continuation(4096, 32)?;
    inspection_custody.transfer_runtime_outside(runtime_finalizer, &native_factory, &native_terminal)?;
    let retire_aliases = |canonical: &lillux_sqlite::OriginalNativeConnectionTerminal| {
        inspection_custody.retire_redundant_census_aliases(
            &post_ack_finalization, &native_factory, &native_terminal, canonical,
        )
    };
    let prepay_operational = |bytes, steps| post_ack_finalization.prepay_outside_continuation(bytes, steps);
    inspection_custody.transfer_operational_outside(OperationalOwnerPhase::Inspection,
        &native_factory, &native_terminal, &prepay_operational)?;
    inspection_custody.transfer_operational_outside(OperationalOwnerPhase::Mutation,
        &native_factory, &native_terminal, &prepay_operational)?;
    post_ack_finalization.retire_outside_state_native_references(&retire_aliases)?;
    inspection_custody.retire_runtime_native_references(&native_factory, &native_terminal, &retire_aliases)?;
    post_ack_finalization.prepare_state_physical_custody()?;
    post_ack_finalization.retire_state_metadata_pins()?;
    inspection_custody.retire_operational_native_references(OperationalOwnerPhase::Inspection,
        &native_factory, &native_terminal, &retire_aliases)?;
    inspection_custody.retire_operational_native_references(OperationalOwnerPhase::Mutation,
        &native_factory, &native_terminal, &retire_aliases)?;
    inspection_custody.register_runtime_shared_roots(&post_ack_finalization)?;
    inspection_custody.register_operational_shared_roots(OperationalOwnerPhase::Inspection, &post_ack_finalization)?;
    inspection_custody.register_operational_shared_roots(OperationalOwnerPhase::Mutation, &post_ack_finalization)?;
    post_ack_finalization.transfer_original_destination_packets()?;
    post_ack_finalization.retain_original_inspection_group(&inspection_custody)?;
    post_ack_finalization.coalesce_original_inspection_group()?;
    post_ack_finalization.advance_original_inspection_group()?;
    post_ack_finalization.join_original_inspection_contexts()?;
    post_ack_finalization.advance_state_destination_join()?;
    post_ack_finalization.register_state_shared_roots()?;
    post_ack_finalization.register_original_destination_and_inspection_roots()?;
    post_ack_finalization.register_original_account_state_lock()?;
    // Actual State ACK and all mutation effects are retained above. End only
    // borrowed mutation authority, preserving every policy/root/lock owner in
    // the original outside controller before its first fallible registration.
    reset_authority.phase = OfflineResetAuthorityPhase::PostAckCustody;
    drop(reset_authority);
    let offline_policy = offline_policy.into_original_outside();
    post_ack_finalization.retain_original_offline_parent_inputs(state_lock, offline_policy)?;
    post_ack_finalization.register_original_offline_policy_root()?;
    post_ack_finalization.register_original_primary_state_lock()?;
    post_ack_finalization.register_original_reservation_removal_parent()?;
    inspection_custody.require_runtime_and_operational_roots_registered(&post_ack_finalization)?;
    post_ack_finalization.publish_original_account_settlement_intent(&inspection_settlement)?;
    // State's typed destination join closes its actual leaf census only after
    // real consuming packet transfers, before physical inventory sealing.

    // Lower SQL payloads, inspection and original account/lifeline owners
    // still register below before inventory sealing; no partial census grants
    // physical closure, native exclusion or durable account settlement.


    // State ACK is not finalizer/reservation settlement. Until actual
    // descriptor retirement and completion publication are connected, the
    // unchanged full gate retains obligations and refuses a completed report.
    inspection_settlement.require_completed()
        .context("original finalizer/reservation settlement remains incomplete after State acknowledgement")?;

    let rebuilt = authoritative.rebuilt_projection.unwrap_or_default();
    publish_progress(&mut observer, ExecutionHistoryResetPhase::Complete, None);
    Ok(ExecutionHistoryResetReport {
        inspection_custody: inspection_custody.clone(),
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
    })
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
) -> Result<RuntimeDb> {
    let parent_path = config.db_path.parent().unwrap_or_else(|| Path::new("."));
    let root = Arc::new(lillux::PinnedDirectory::open(parent_path)?
        .context("original Runtime parent absent; no create fallback")?);
    let namespace = if runtime_directory.is_same_directory(&root)? {
        runtime_directory_lock.clone()
    } else {
        root.lock_exclusive_with_timeout(claim.require_service_window()?
            .min(std::time::Duration::from_secs(1)))?
    };
    let inputs = claim.issue_runtime_mutation_inputs(&factory, Arc::clone(&root),
        &namespace, &config.db_path)?;
    RuntimeDb::open_original_history_reset_mutation(&config.db_path, root, namespace, factory, inputs)
        .map_err(anyhow::Error::new)
        .with_context(|| format!("open original Runtime database {}", config.db_path.display()))
}

#[cfg(test)]
fn open_mutating_runtime_db(
    config: &Config,
    runtime_directory: &lillux::PinnedDirectory,
    runtime_directory_lock: &lillux::PinnedDirectoryLock,
) -> Result<RuntimeDb> {
    let parent_path = config.db_path.parent().unwrap_or_else(|| Path::new("."));
    let parent = lillux::PinnedDirectory::open_or_create(parent_path)?;
    if runtime_directory.is_same_directory(&parent)? {
        RuntimeDb::open_for_explicit_history_reset_with_namespace_authority(
            &config.db_path,
            parent,
            runtime_directory_lock.clone(),
        )
    } else {
        RuntimeDb::open_for_explicit_history_reset(&config.db_path)
    }
    .with_context(|| format!("open runtime database {}", config.db_path.display()))
}

fn open_inspection_runtime_db(
    config: &Config,
    runtime_directory: &lillux::PinnedDirectory,
    runtime_directory_lock: &lillux::PinnedDirectoryLock,
    copy_work: Arc<dyn lillux_sqlite::SqliteInspectionCopyWork>,
    factory: Arc<lillux_sqlite::OriginalNativeFactory>,
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
    RuntimeDb::open_original_history_reset_inspection(&config.db_path, parent, parent_lock,
        Arc::new(runtime_directory.try_clone()?), copy_work, factory)
        .with_context(|| format!("inspect runtime database {}", config.db_path.display()))
}

#[cfg(test)]
fn open_mechanism_inspection_runtime_db(
    config: &Config,
    runtime_directory: &lillux::PinnedDirectory,
    runtime_directory_lock: &lillux::PinnedDirectoryLock,
    copy_work: Arc<dyn lillux_sqlite::SqliteInspectionCopyWork>,
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
    RuntimeDb::open_owned_history_reset_inspection(&config.db_path, parent, parent_lock, copy_work)
        .with_context(|| format!("inspect runtime database {}", config.db_path.display()))
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
            anyhow::bail!("scheduler history requires its selected reset participant");
        }
    }
    if let Some(node) = lillux::PinnedDirectory::open(&config.runtime_node_dir())? {
        for name in node.entry_names()? {
            if name == std::ffi::OsStr::new("schedules") {
                anyhow::bail!("signed scheduler sources require their selected reset participant");
            }
        }
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
pub(super) fn fixture_inspection_claim(
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

    fn install_offline_policy(
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
            .validated_generation(&ryeos_node_runtime::node_policy::NodePolicyTable::new(), &source)
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

    #[test]
    fn real_reset_retains_published_claim_when_runtime_inspection_cannot_open() {
        use super::original_quota_claim::QuotaCustodyFailure;
        let tmp = TempDir::new().unwrap();
        let config = Config::load(&ConfigSources {
            app_root: Some(tmp.path().to_owned()),
            ..Default::default()
        })
        .unwrap();
        let identity =
            ryeos_node_runtime::identity::NodeIdentity::create(&config.node_signing_key_path).unwrap();
        install_offline_policy(&config, &identity, true);
        let mut trust = ryeos_state::refs::TrustStore::new();
        trust.insert(identity.fingerprint().to_owned(), *identity.verifying_key());
        drop(ryeos_state::StateDb::open(&config.runtime_state_dir(), Arc::new(trust)).unwrap());
        assert!(!config.db_path.exists());
        let error = run_execution_history_reset(
            &ExecutionHistoryResetOptions {
                app_root: Some(tmp.path().to_owned()),
                dry_run: true,
                ..Default::default()
            },
            None,
        )
        .unwrap_err();
        let custody = error.downcast_ref::<QuotaCustodyFailure>().unwrap();
        assert_eq!(custody.phase(), QuotaFailurePhase::Acquisition);
        let claim = Arc::clone(custody.claim());
        let path = config
            .app_root
            .join(".ai/node/offline-reset-quota")
            .join(format!("reserved.{}", claim.operation_id()));
        let original = std::fs::read(&path).unwrap();
        assert!(
            !config.db_path.exists(),
            "failed inspection cannot create the absent original"
        );
        // The error kept the ORIGINAL claim; a new stopped-node observation
        // frame may authenticate it without constructing a replacement owner.
        let state_lock = StateLock::acquire(&default_lock_path(tmp.path())).unwrap();
        let runtime = lillux::PinnedDirectory::open(&config.runtime_state_dir())
            .unwrap()
            .unwrap();
        let namespace = runtime.lock_exclusive().unwrap();
        let policy =
            OfflineResetPolicyOwner::load(&config, &identity, &state_lock, &runtime, &namespace)
                .unwrap();
        claim.require_published(&policy, &identity).unwrap();
        drop(error);
        assert_eq!(std::fs::read(&path).unwrap(), original);
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
            ryeos_node_runtime::identity::NodeIdentity::create(&config.node_signing_key_path).unwrap();
        std::fs::create_dir_all(config.runtime_state_dir()).unwrap();
        install_offline_policy(&config, &identity, true);
        let state_lock = StateLock::acquire(&default_lock_path(tmp.path())).unwrap();
        let runtime = lillux::PinnedDirectory::open(&config.runtime_state_dir())
            .unwrap()
            .unwrap();
        let namespace = runtime.lock_exclusive().unwrap();
        let policy =
            OfflineResetPolicyOwner::load(&config, &identity, &state_lock, &runtime, &namespace)
                .unwrap();
        let first = OriginalQuotaClaim::prepare(
            &policy,
            &identity,
            None,
            inspection_destination::OriginalInspectionMode::DryRun,
            false,
        )
        .unwrap();
        assert!(
            first.issue_copy_work(&policy, &identity).is_err(),
            "prepared policy/record is not an issued copy owner"
        );
        super::original_quota_claim::fail_next_publication_ack();
        let uncertain = first.publish(&policy, &identity).unwrap_err();
        assert!(
            uncertain
                .downcast_ref::<QuotaCustodyFailure>()
                .unwrap()
                .retains(&first)
        );
        let ledger_path = config.app_root.join(".ai/node/offline-reset-quota");
        let first_path = ledger_path.join(format!("reserved.{}", first.operation_id()));
        let exact_signed = std::fs::read(&first_path).unwrap();
        first.require_published(&policy, &identity).unwrap();
        assert!(
            first.issue_copy_work(&policy, &identity).is_err(),
            "uncertain publication bytes cannot substitute for original acknowledgement"
        );
        first.publish(&policy, &identity).unwrap();
        let work_one = first.issue_copy_work(&policy, &identity).unwrap();
        let work_two = first.issue_copy_work(&policy, &identity).unwrap();
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
        assert!(
            lillux_sqlite::copy_regular_without_sqlite_alias(
                &runtime,
                absent,
                destination.root.as_ref(),
                0,
                work_two.as_ref()
            )
            .is_err(),
            "a second issued view cannot reset original role charges"
        );
        drop(work_one);
        drop(work_two);
        let unknown_copy = config
            .app_root
            .join(".ai/node/offline-reset-inspections/unclassified");
        std::fs::create_dir(&unknown_copy).unwrap();
        let blocked = first.publish(&policy, &identity).unwrap_err();
        assert!(format!("{blocked:#}").contains("unclassified inspection destination"));
        assert!(unknown_copy.is_dir());
        assert_eq!(std::fs::read(&first_path).unwrap(), exact_signed);
        std::fs::remove_dir(&unknown_copy).unwrap(); // only this test's injected empty object
        drop(blocked);
        drop(uncertain);
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
        // One real charge + one visible partial +63 extra deposits =65.
        // Refuse before authentication on an over-capacity census, regardless
        // of whether the selected original record appears in its first64.
        for index in 0..63 {
            std::fs::write(
                ledger_path.join(format!("held.partial.{index:02}")),
                b"retained",
            )
            .unwrap();
        }
        let capacity = first.publish(&policy, &identity).unwrap_err();
        assert!(format!("{capacity:#}").contains("complete inventory capacity"));
        assert!(
            capacity
                .downcast_ref::<QuotaCustodyFailure>()
                .unwrap()
                .retains(&first)
        );
        assert_eq!(std::fs::read_dir(&ledger_path).unwrap().count(), 65);
        drop(capacity);
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
            ryeos_node_runtime::identity::NodeIdentity::create(&config.node_signing_key_path).unwrap();
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
        assert_eq!(owner.quota().unwrap().max_source_roles, ryeos_scheduler_contract::OFFLINE_RESET_SOURCE_ROLES);
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
                ryeos_node_runtime::identity::NodeIdentity::create(&config.node_signing_key_path).unwrap();
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
    fn offline_capability_requires_actual_locks_exact_opened_db_and_published_intent() {
        let tmp = TempDir::new().unwrap();
        let config = Config::load(&ConfigSources {
            app_root: Some(tmp.path().to_owned()),
            ..Default::default()
        })
        .unwrap();
        let identity =
            ryeos_node_runtime::identity::NodeIdentity::create(&config.node_signing_key_path).unwrap();
        install_offline_policy(&config, &identity, true);
        let mut trust = ryeos_state::refs::TrustStore::new();
        trust.insert(identity.fingerprint().to_owned(), *identity.verifying_key());
        let trust = Arc::new(trust);
        drop(ryeos_state::StateDb::open(&config.runtime_state_dir(), Arc::clone(&trust)).unwrap());
        drop(RuntimeDb::open(&config.db_path).unwrap());
        let state_lock = StateLock::acquire(&default_lock_path(tmp.path())).unwrap();
        let directory = lillux::PinnedDirectory::open(&config.runtime_state_dir())
            .unwrap()
            .unwrap();
        let namespace = directory.lock_exclusive().unwrap();
        let mut state =
            ryeos_state::StateDb::open_for_projection_rebuild(&config.runtime_state_dir(), trust)
                .unwrap();
        let pinned = state.pinned_authority().unwrap();
        let cas_guard = pinned.acquire_exclusive_guard(true).unwrap();
        let mut db = open_mutating_runtime_db(&config, &directory, &namespace).unwrap();
        let mut authority =
            OfflineResetAuthority::capture(&state_lock, &directory, &namespace, &config, &db)
                .unwrap();
        assert!(
            db.apply_explicit_history_reset(&authority, &config.db_path)
                .is_err()
        );
        assert!(db.discard_all_thread_history(&authority).is_err());
        let policy =
            OfflineResetPolicyOwner::load(&config, &identity, &state_lock, &directory, &namespace)
                .unwrap();
        let quota_claim = OriginalQuotaClaim::prepare(
            &policy,
            &identity,
            None,
            inspection_destination::OriginalInspectionMode::Mutating,
            false,
        )
        .unwrap();
        quota_claim.publish(&policy, &identity).unwrap();
        let copy_work = quota_claim.issue_copy_work(&policy, &identity).unwrap();
        let operation = OriginalOperation::prepare(
            &authority,
            &db,
            &pinned,
            &cas_guard,
            &identity,
            &ExecutionHistoryResetOptions::default(),
            false,
            &ryeos_scheduler_contract::SchedulerFireHistoryAccounting::Exact {
                counts: Default::default(),
            },
            None,
            &quota_claim,
        )
        .unwrap();
        assert_eq!(
            operation.reference().operation_id,
            quota_claim.operation_id()
        );
        assert!(operation.retains_quota_claim(&quota_claim));
        let different_runtime_root = TempDir::new().unwrap();
        let different_runtime = lillux::PinnedDirectory::open(different_runtime_root.path())
            .unwrap()
            .unwrap();
        assert!(
            quota_claim
                .require_original_identity(&different_runtime, &identity, &config.app_root)
                .is_err()
        );
        let stage_error = operation.stage(&different_runtime).unwrap_err();
        let retained = stage_error
            .downcast_ref::<original_operation::OperationPublicationFailure>()
            .unwrap();
        assert!(Arc::ptr_eq(retained.operation(), &operation));
        assert!(retained.operation().retains_quota_claim(&quota_claim));
        assert_eq!(
            retained.phase(),
            original_operation::PublicationPhase::Staging
        );
        assert!(
            different_runtime
                .entry_names_bounded(4096)
                .unwrap()
                .is_empty()
        );
        drop(stage_error);
        operation.stage(&directory).unwrap();
        let original_bytes = operation.signed_bytes().to_vec();
        operation.stage(&directory).unwrap(); // Same memoized bytes on retry.
        assert_eq!(operation.signed_bytes(), original_bytes);
        assert!(
            OriginalOperation::read_active(
                &config.app_root,
                &pinned,
                &cas_guard,
                &directory,
                &namespace,
                &identity,
                &ExecutionHistoryResetOptions::default(),
                false,
            )
            .unwrap()
            .is_none()
        ); // Staging alone never activates authority.
        assert!(operation.require_active(&pinned, &cas_guard).is_err());
        let permit = SchedulerResetRetirement {
            authority: &authority,
            runtime_db: &db,
            cas_guard: &cas_guard,
            state_authority: &pinned,
            identity: &identity,
            operation: &operation,
        };
        assert!(permit.runtime_directory().is_err());
        let other = open_mutating_runtime_db(&config, &directory, &namespace).unwrap();
        assert!(authority.require_held(&other).is_err());
        drop(other);
        let wrong_directory_fixture = TempDir::new().unwrap();
        let wrong_directory = lillux::PinnedDirectory::open(wrong_directory_fixture.path())
            .unwrap()
            .unwrap();
        let wrong_namespace = wrong_directory.lock_exclusive().unwrap();
        assert!(
            OfflineResetAuthority::capture(&state_lock, &directory, &wrong_namespace, &config, &db)
                .is_err()
        );
        let wrong_preview = SchedulerResetPreview {
            runtime_directory: &directory,
            namespace_lock: &wrong_namespace,
            identity: None,
            inspection_custody: std::cell::RefCell::new(None),
            inspection_closed: std::cell::RefCell::new(None),
            controller_custody: None,
            original_operation: None,
            quota_claim: Arc::clone(&quota_claim),
            copy_work: copy_work.clone(),
        };
        assert!(wrong_preview.runtime_directory().is_err());
        assert!(
            open_mechanism_inspection_runtime_db(&config, &directory, &namespace, copy_work.clone()).is_err(),
            "inspection must not copy a source held by an original live native SQLite binding"
        );
        state
            .begin_thread_history_discard_admitted(&cas_guard, operation.reference())
            .unwrap();
        let other_app_root = TempDir::new().unwrap();
        assert!(
            OriginalOperation::read_active(
                other_app_root.path(),
                &pinned,
                &cas_guard,
                &directory,
                &namespace,
                &identity,
                &ExecutionHistoryResetOptions::default(),
                false,
            )
            .is_err(),
            "the same runtime cannot activate an operation under another app root"
        );
        let reloaded = OriginalOperation::read_active(
            &config.app_root,
            &pinned,
            &cas_guard,
            &directory,
            &namespace,
            &identity,
            &ExecutionHistoryResetOptions::default(),
            false,
        )
        .unwrap()
        .unwrap();
        assert_eq!(reloaded.reference(), operation.reference());
        assert_eq!(reloaded.signed_bytes(), original_bytes);
        assert_eq!(reloaded.accounting(), operation.accounting());
        assert!(
            OriginalOperation::read_active(
                &config.app_root,
                &pinned,
                &cas_guard,
                &directory,
                &namespace,
                &identity,
                &ExecutionHistoryResetOptions {
                    discard_project_heads: true,
                    ..Default::default()
                },
                false,
            )
            .is_err()
        );
        assert!(
            OriginalOperation::read_active(
                &config.app_root,
                &pinned,
                &cas_guard,
                &directory,
                &namespace,
                &identity,
                &ExecutionHistoryResetOptions::default(),
                true,
            )
            .is_err()
        );
        authority.phase = OfflineResetAuthorityPhase::IntentPublished;
        let permit = SchedulerResetRetirement {
            authority: &authority,
            runtime_db: &db,
            cas_guard: &cas_guard,
            state_authority: &pinned,
            identity: &identity,
            operation: &operation,
        };
        assert!(permit.runtime_directory().is_ok());
        let other_state_root = TempDir::new().unwrap();
        let other_state = ryeos_state::StateDb::open(
            other_state_root.path(),
            Arc::new(ryeos_state::refs::TrustStore::new()),
        )
        .unwrap();
        let other_state_authority = other_state.pinned_authority().unwrap();
        let wrong_state_owner = SchedulerResetRetirement {
            authority: &authority,
            runtime_db: &db,
            cas_guard: &cas_guard,
            state_authority: &other_state_authority,
            identity: &identity,
            operation: &operation,
        };
        assert!(wrong_state_owner.runtime_directory().is_err());
        let activation_error = operation
            .activate(&other_state, &other_state_authority, &cas_guard)
            .unwrap_err();
        let retained = activation_error
            .downcast_ref::<original_operation::OperationPublicationFailure>()
            .unwrap();
        assert_eq!(
            retained.phase(),
            original_operation::PublicationPhase::StateActivation
        );
        assert!(Arc::ptr_eq(retained.operation(), &operation));
        operation.require_active(&pinned, &cas_guard).unwrap();
        let admission_error = operation
            .validate_pre_activation(&pinned, &cas_guard, || {
                anyhow::bail!("injected domain namespace reservation refusal")
            })
            .unwrap_err();
        let retained = admission_error
            .downcast_ref::<original_operation::OperationPublicationFailure>()
            .unwrap();
        assert_eq!(
            retained.phase(),
            original_operation::PublicationPhase::PreActivationAdmission
        );
        assert!(Arc::ptr_eq(retained.operation(), &operation));
        operation.require_active(&pinned, &cas_guard).unwrap();
        drop(admission_error);
        assert!(
            other_state_authority
                .require_recovery()
                .unwrap()
                .thread_history_discard_operation()
                .unwrap()
                .is_none()
        );
        drop(activation_error);
        assert!(
            db.apply_explicit_history_reset(&authority, Path::new("wrong-target"))
                .is_err()
        );
        db.apply_explicit_history_reset(&authority, &config.db_path)
            .unwrap();
        assert!(authority.require_held(&db).is_ok());
        db.discard_all_thread_history(&authority).unwrap();
        // Authority still borrows original locks; later opener has a new marker.
        drop(db);
        let reopened = open_mutating_runtime_db(&config, &directory, &namespace).unwrap();
        assert!(authority.authorize_mutation(&reopened).is_err());
        let wrong_incarnation = SchedulerResetRetirement {
            authority: &authority,
            runtime_db: &reopened,
            cas_guard: &cas_guard,
            state_authority: &pinned,
            identity: &identity,
            operation: &operation,
        };
        assert!(wrong_incarnation.runtime_directory().is_err());
        assert!(StateLock::acquire(&default_lock_path(tmp.path())).is_err());
    }

    #[test]
    fn offline_capability_wrong_root_refuses_before_intent() {
        let tmp = TempDir::new().unwrap();
        let config = Config::load(&ConfigSources {
            app_root: Some(tmp.path().to_owned()),
            ..Default::default()
        })
        .unwrap();
        drop(RuntimeDb::open(&config.db_path).unwrap());
        let state_lock = StateLock::acquire(&default_lock_path(tmp.path())).unwrap();
        let directory = lillux::PinnedDirectory::open(&config.runtime_state_dir())
            .unwrap()
            .unwrap();
        let namespace = directory.lock_exclusive().unwrap();
        let db = open_mutating_runtime_db(&config, &directory, &namespace).unwrap();
        let other_root = TempDir::new().unwrap();
        let other_config = Config::load(&ConfigSources {
            app_root: Some(other_root.path().to_owned()),
            ..Default::default()
        })
        .unwrap();
        assert!(
            OfflineResetAuthority::capture(&state_lock, &directory, &namespace, &other_config, &db)
                .is_err()
        );
        assert!(db.inspect_thread_history_discard().is_ok());
    }

    #[test]
    fn reset_acknowledges_scheduler_before_removing_cross_store_marker() {
        struct Participant {
            completed: std::sync::atomic::AtomicBool,
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
                let recovery = runtime
                    .open_child_directory(std::ffi::OsStr::new("recovery"))?
                    .context("recovery namespace absent")?;
                assert!(
                    recovery
                        .entry_no_follow(std::ffi::OsStr::new("thread-history-discard.json"))?
                        .is_none(),
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
                let recovery = permit
                    .runtime_directory()?
                    .open_child_directory(std::ffi::OsStr::new("recovery"))?
                    .context("recovery namespace absent")?;
                assert!(
                    recovery
                        .entry_no_follow(std::ffi::OsStr::new("thread-history-discard.json"))?
                        .is_some(),
                    "cross-store marker must remain through terminal scheduler acknowledgement"
                );
                self.completed
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
            ryeos_node_runtime::identity::NodeIdentity::create(&config.node_signing_key_path).unwrap();
        install_offline_policy(&config, &identity, true);
        let mut trust = ryeos_state::refs::TrustStore::new();
        trust.insert(identity.fingerprint().to_owned(), *identity.verifying_key());
        drop(ryeos_state::StateDb::open(&config.runtime_state_dir(), Arc::new(trust)).unwrap());
        drop(RuntimeDb::open(&config.db_path).unwrap());
        drop(StateLock::acquire(&default_lock_path(tmp.path())).unwrap());
        let participant = Participant {
            completed: std::sync::atomic::AtomicBool::new(false),
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
            !config
                .runtime_state_dir()
                .join("recovery/thread-history-discard.json")
                .exists()
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
    fn history_reset_composes_replay_cutover_and_preserves_stable_credentials() {
        let tmp = TempDir::new().unwrap();
        let config = Config::load(&ConfigSources {
            app_root: Some(tmp.path().to_owned()),
            ..Default::default()
        })
        .unwrap();
        let identity =
            ryeos_node_runtime::identity::NodeIdentity::create(&config.node_signing_key_path).unwrap();
        install_offline_policy(&config, &identity, true);
        let identity_bytes = std::fs::read(&config.node_signing_key_path).unwrap();
        let mut trust = ryeos_state::refs::TrustStore::new();
        trust.insert(identity.fingerprint().to_owned(), *identity.verifying_key());
        let trust = Arc::new(trust);
        drop(ryeos_state::StateDb::open(&config.runtime_state_dir(), trust.clone()).unwrap());
        drop(RuntimeDb::open(&config.db_path).unwrap());
        drop(StateLock::acquire(&default_lock_path(tmp.path())).unwrap());
        // Raw imports are durable CAS staging authority, outside the retired
        // thread/runtime rows. A/B qualification recovery relies on reopening
        // these exact uploads under the new source generation.
        let import_owner = identity.fingerprint().to_owned();
        let import_key =
            ryeos_state::DurableCasPublicationKey::external_content_import(&"a".repeat(64))
                .unwrap();
        let (import_stage_id, import_object_hash) = {
            let state =
                ryeos_state::StateDb::open(&config.runtime_state_dir(), trust.clone()).unwrap();
            let authority = state.pinned_authority().unwrap();
            let guard = authority.acquire_shared_guard().unwrap();
            let object_hash = authority
                .cas_store()
                .unwrap()
                .store_object(&serde_json::json!({"kind": "raw-import-reset-fixture"}))
                .unwrap();
            let mut stage = authority
                .require_recovery()
                .unwrap()
                .begin_durable_cas_upload_admitted(
                    &guard,
                    &import_owner,
                    "external-content-import",
                    &import_key,
                    None,
                )
                .unwrap();
            stage
                .protect_cas_closure(&guard, [object_hash.as_str()], std::iter::empty())
                .unwrap();
            (stage.staging_id().to_owned(), object_hash)
        };
        let operational_path = config
            .runtime_state_dir()
            .join(ryeos_state::operational::OPERATIONAL_DB_FILENAME);
        let profile = ryeos_state::operational::OperationalCredentialProfileRecord {
            profile_id: "retained-active".to_owned(),
            owner_principal: "operator".to_owned(),
            home_id: "home".to_owned(),
            authority_revision: 3,
            credential_generation: 2,
            state: "active".to_owned(),
            active_login_id: None,
            login_epoch: 1,
            login_expires_at_ms: None,
            sanitized_account: Some(serde_json::json!({"account_id":"retained-account"})),
            created_at_ms: 1,
            updated_at_ms: 2,
        };
        let mut enrolling = profile.clone();
        enrolling.profile_id = "retired-enrollment".to_owned();
        enrolling.home_id = "enrollment-home".to_owned();
        enrolling.state = "enrolling".to_owned();
        enrolling.active_login_id = Some("old-login".to_owned());
        enrolling.login_expires_at_ms = Some(100);
        {
            let db = ryeos_state::OperationalDb::open_existing_current(&operational_path).unwrap();
            db.merge_credential_profile(&profile).unwrap();
            db.merge_credential_profile(&enrolling).unwrap();
        }
        let current_replay_epoch = {
            let conn = rusqlite::Connection::open(&operational_path).unwrap();
            let epoch: i32 = conn
                .query_row("SELECT epoch FROM replay_index_epoch", [], |r| r.get(0))
                .unwrap();
            conn.execute_batch("UPDATE replay_index_epoch SET epoch = epoch - 2;")
                .unwrap();
            epoch
        };
        {
            let conn = rusqlite::Connection::open(&config.db_path).unwrap();
            let app_id: i32 = conn
                .query_row("PRAGMA application_id", [], |r| r.get(0))
                .unwrap();
            conn.pragma_update(None, "application_id", app_id - 1)
                .unwrap();
        }
        let options = ExecutionHistoryResetOptions {
            app_root: Some(tmp.path().to_owned()),
            dry_run: true,
            discard_project_heads: false,
            schema_cut: Some(ExecutionHistorySchemaCut {
                from: RuntimeDb::current_operator_schema_epoch() - 1,
                to: RuntimeDb::current_operator_schema_epoch(),
            }),
        };
        let runtime_before = std::fs::read(&config.db_path).unwrap();
        let operational_before = std::fs::read(&operational_path).unwrap();
        let preview = run_execution_history_reset(&options, None).unwrap();
        assert!(!preview.performed);
        assert_eq!(
            preview.schema_cut.as_ref().unwrap().status,
            ExecutionHistorySchemaCutStatus::Required
        );
        assert_eq!(
            preview.replay_indexes.scope,
            ryeos_state::operational::ReplayIndexResetScope::AllReplayRecords
        );
        assert_eq!(
            preview.replay_indexes.stored_epoch,
            current_replay_epoch - 2
        );
        assert_eq!(std::fs::read(&config.db_path).unwrap(), runtime_before);
        assert_eq!(
            std::fs::read(&operational_path).unwrap(),
            operational_before
        );
        assert!(ryeos_state::OperationalDb::open_existing_current(&operational_path).is_err());

        // Model resumption after a process dies with the existing durable
        // discard intent published but before replay/runtime activation.
        {
            let state = ryeos_state::StateDb::open_for_projection_rebuild(
                &config.runtime_state_dir(),
                trust.clone(),
            )
            .unwrap();
            let authority = state.pinned_authority().unwrap();
            let guard = authority.acquire_exclusive_guard(true).unwrap();
            let state_lock = StateLock::acquire(&default_lock_path(tmp.path())).unwrap();
            let directory = lillux::PinnedDirectory::open(&config.runtime_state_dir())
                .unwrap()
                .unwrap();
            let namespace = directory.lock_exclusive().unwrap();
            let db = open_mutating_runtime_db(&config, &directory, &namespace).unwrap();
            let offline =
                OfflineResetAuthority::capture(&state_lock, &directory, &namespace, &config, &db)
                    .unwrap();
            let policy = OfflineResetPolicyOwner::load(
                &config,
                &identity,
                &state_lock,
                &directory,
                &namespace,
            )
            .unwrap();
            let quota_claim = OriginalQuotaClaim::prepare(
                &policy,
                &identity,
                None,
                inspection_destination::OriginalInspectionMode::Mutating,
                false,
            )
            .unwrap();
            let operation = OriginalOperation::prepare(
                &offline,
                &db,
                &authority,
                &guard,
                &identity,
                &options,
                false,
                &ryeos_scheduler_contract::SchedulerFireHistoryAccounting::Exact {
                    counts: Default::default(),
                },
                None,
                &quota_claim,
            )
            .unwrap();
            operation.stage(&directory).unwrap();
            state
                .begin_thread_history_discard_admitted(&guard, operation.reference())
                .unwrap();
        }
        let options = ExecutionHistoryResetOptions {
            dry_run: false,
            ..options
        };
        let report = run_execution_history_reset(&options, None).unwrap();
        assert!(report.performed);
        assert_eq!(
            report.schema_cut.as_ref().unwrap().status,
            ExecutionHistorySchemaCutStatus::Completed
        );
        assert_eq!(report.replay_indexes, preview.replay_indexes);
        let db = ryeos_state::OperationalDb::open_existing_current(&operational_path).unwrap();
        assert_eq!(
            db.credential_profile(&profile.profile_id).unwrap(),
            Some(profile)
        );
        let retired = db
            .credential_profile(&enrolling.profile_id)
            .unwrap()
            .unwrap();
        assert_eq!(retired.state, "unauthenticated");
        assert_eq!(retired.authority_revision, enrolling.authority_revision + 1);
        assert_eq!(
            retired.credential_generation,
            enrolling.credential_generation
        );
        assert_eq!(retired.home_id, enrolling.home_id);
        assert!(retired.active_login_id.is_none());
        assert!(retired.sanitized_account.is_none());
        drop(db);
        drop(RuntimeDb::open(&config.db_path).unwrap());
        let state = ryeos_state::StateDb::open(&config.runtime_state_dir(), trust).unwrap();
        let authority = state.pinned_authority().unwrap();
        let guard = authority.acquire_shared_guard().unwrap();
        let stage = authority
            .require_recovery()
            .unwrap()
            .open_durable_cas_upload_admitted(&guard, &import_stage_id, &import_owner)
            .unwrap();
        stage
            .ensure_publication_contract(&import_key, None)
            .unwrap();
        stage.ensure_protects_object(&import_object_hash).unwrap();
        drop(stage);
        drop(guard);
        drop(authority);
        drop(state);
        let repeated = run_execution_history_reset(&options, None).unwrap();
        assert!(!repeated.performed);
        assert!(!repeated.dry_run);
        assert_eq!(
            repeated.schema_cut.as_ref().unwrap().status,
            ExecutionHistorySchemaCutStatus::AlreadyCurrent
        );
        assert_eq!(
            repeated.replay_indexes.scope,
            ryeos_state::operational::ReplayIndexResetScope::Unchanged
        );
        assert_eq!(
            std::fs::read(&config.node_signing_key_path).unwrap(),
            identity_bytes
        );
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
        assert!(require_absent_scheduler_history(&config, &directory).is_err());
        assert_eq!(std::fs::read(file).unwrap(), b"unknown schema");
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
        let mut config = Config::load(&ConfigSources {
            app_root: Some(directory.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();
        config.db_path = db_target.to_path_buf();
        let identity =
            ryeos_node_runtime::identity::NodeIdentity::create(&config.node_signing_key_path).unwrap();
        install_offline_policy(&config, &identity, true);
        let state_lock = StateLock::acquire(&default_lock_path(&config.app_root)).unwrap();
        fixture_operational_source::establish(directory, namespace, &identity).unwrap();
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
        let work = claim.issue_copy_work(&policy, &identity).unwrap();
        drop(policy);
        (state_lock, claim, work)
    }

    #[test]
    fn scheduler_preview_retains_real_native_close_proof_without_mutating_source() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("scheduler.sqlite3");
        let conn = rusqlite::Connection::open(&source).unwrap();
        conn.execute_batch("CREATE TABLE sample(value INTEGER); INSERT INTO sample VALUES(7);")
            .unwrap();
        conn.close().unwrap();
        let original = std::fs::read(&source).unwrap();
        let directory = lillux::PinnedDirectory::open(tmp.path()).unwrap().unwrap();
        let namespace = directory.lock_exclusive().unwrap();
        let (_state_lock, quota_claim, copy_work) = issue_inspection_claim(
            &directory,
            &namespace,
            &directory.path().join("runtime.sqlite3"),
        );
        let controller = Arc::new(InspectionControllerCustody::default());
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
            .inspect_database(|directory, lease, sql_work| {
                let copy = directory.path().join("scheduler.sqlite3");
                assert_ne!(copy, source);
                let owner = lillux_sqlite::ReadonlySqliteCopy::open(
                    directory,
                    std::ffi::OsStr::new("scheduler.sqlite3"),
                    lease,
                    sql_work,
                )
                .map_err(anyhow::Error::new)?;
                // Never open/drop a filesystem alias while SQLite owns the inode.
                let value: i64 =
                    owner
                        .connection()
                        .query_row("SELECT value FROM sample", [], |row| row.get(0))?;
                assert_eq!(value, 7);
                assert!(
                    owner
                        .connection()
                        .execute_batch("DELETE FROM sample")
                        .is_err()
                );
                let proof = owner.close().map_err(anyhow::Error::new)?;
                Ok(SqliteInspectionOutcome::Closed {
                    accounting: Ok(copy),
                    proof,
                })
            })
            .unwrap()
            .unwrap();
        let closed = permit.inspection_closed.borrow().clone().unwrap();
        closed.require_original().unwrap();
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
    }

    #[test]
    fn scheduler_accounting_failure_retains_actual_native_close_proof() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("scheduler.sqlite3");
        let conn = rusqlite::Connection::open(&source).unwrap();
        conn.execute_batch("CREATE TABLE sample(value INTEGER);")
            .unwrap();
        conn.close().unwrap();
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
            quota_claim,
            copy_work,
        };
        let error = permit
            .inspect_database::<()>(|directory, lease, sql_work| {
                let owner = lillux_sqlite::ReadonlySqliteCopy::open(
                    directory,
                    std::ffi::OsStr::new("scheduler.sqlite3"),
                    lease,
                    sql_work,
                )
                .map_err(anyhow::Error::new)?;
                let proof = owner.close().map_err(anyhow::Error::new)?;
                Ok(SqliteInspectionOutcome::Closed {
                    accounting: Err(anyhow::anyhow!("injected accounting refusal")),
                    proof,
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
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn scheduler_partial_acquisition_release_never_supplies_completed_accounting() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("scheduler.sqlite3"), b"retained main").unwrap();
        let directory = lillux::PinnedDirectory::open(tmp.path()).unwrap().unwrap();
        let namespace = directory.lock_exclusive().unwrap();
        let (_state_lock, quota_claim, copy_work) = issue_inspection_claim(
            &directory,
            &namespace,
            &directory.path().join("runtime.sqlite3"),
        );
        let controller = Arc::new(InspectionControllerCustody::default());
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
            .inspect_database::<()>(|directory, lease, sql_work| {
                // Inject an invalid sidecar into the isolated destination. The
                // original source is untouched; cleanup must retain this deposit.
                std::fs::create_dir(directory.path().join("scheduler.sqlite3-wal"))?;
                let failure = lillux_sqlite::ReadonlySqliteCopy::open(
                    directory,
                    std::ffi::OsStr::new("scheduler.sqlite3"),
                    lease,
                    sql_work,
                )
                .err()
                .expect("nonregular WAL must refuse acquisition");
                let proof = failure.retry_close().map_err(anyhow::Error::new)?;
                Ok(SqliteInspectionOutcome::AcquisitionReleased {
                    error: anyhow::anyhow!("injected acquisition refusal"),
                    proof,
                })
            })
            .unwrap_err();
        assert!(format!("{error:#}").contains("injected acquisition refusal"));
        assert!(permit.inspection_closed.borrow().is_none());
        assert!(controller.closed.lock().unwrap().is_empty());
        let released = controller.acquisition_released.lock().unwrap()[0].clone();
        released.require_original().unwrap();
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
        assert!(format!("{error:#}").contains("already allocated"));
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
            assert!(!tmp.path().join("recovery").exists());
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
            assert!(!tmp.path().join("recovery").exists());
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
            assert!(!tmp.path().join("recovery").exists());
        }
    }
}
