//! Application custody for one qualification-only local Worker start.
//!
//! The executor supplies original descriptor owners, while the app retains
//! the Q allocation and its one-use workspace, scope, and process journal.

use std::ffi::OsStr;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeVerificationCoordinate;

use crate::operator_external_content::product_qualification::PreparedExternalQualificationRoot;
use crate::qualification_workspace_custody::{
    QualificationUnregisteredRollbackCustody, QualificationUnregisteredWorkspaceCustody,
    QualificationWorkspaceCustody, QualificationWorkspaceCustodyCoordinate,
};
use crate::runtime_db::external_execution::{
    ExternalAllocationOwner, ExternalAllocationPhase, QualificationLocalStartPhase,
    QualificationLocalStartRecord, QualificationWorkerProcessPhase,
};
use crate::state::AppState;
use crate::temp_dir_guard::TempDirGuard;

/// This borrowed token exists only while the existing qualification occurrence
/// contact gate is held. Its methods still recheck the durable Q journal.
pub struct QualificationLocalOwner<'a> {
    state: &'a AppState,
    placement: &'a str,
    root: &'a PreparedExternalQualificationRoot,
}

fn fence_precreate_cleanup(
    owner: &QualificationLocalOwner<'_>,
    error: anyhow::Error,
) -> anyhow::Error {
    match owner.mark_cleanup_unproved("Q original runtime root creation or rollback is unproved") {
        Ok(()) => error,
        Err(fence) => error.context(format!("Q pre-Create cleanup fence failed: {fence:#}")),
    }
}

fn rollback_precreate_workspace(
    owner: &QualificationLocalOwner<'_>,
    record: &QualificationLocalStartRecord,
    error: anyhow::Error,
    workspace: &Arc<TempDirGuard>,
    uncertain_creation: bool,
) -> anyhow::Error {
    let mut error = error;
    let mut unproved = uncertain_creation;
    if let Err(fence) =
        owner.mark_cleanup_unproved("Q original root creation needs no-Create cleanup")
    {
        return retain_partial_workspace(
            owner,
            record,
            workspace,
            error.context(format!("Q pre-Create cleanup fence failed: {fence:#}")),
        );
    }
    if let Err(cleanup) = workspace.remove_uncreated_workspace_now() {
        unproved = true;
        error = error.context(format!("new Q workspace rollback failed: {cleanup:#}"));
        error = retain_partial_workspace(owner, record, workspace, error);
    }
    let error = if unproved {
        error.context("Q original root rollback remains unproved")
    } else {
        error.context("Q original roots were removed before backend Create")
    };
    error
}

fn retain_partial_workspace(
    owner: &QualificationLocalOwner<'_>,
    record: &QualificationLocalStartRecord,
    workspace: &Arc<TempDirGuard>,
    error: anyhow::Error,
) -> anyhow::Error {
    workspace.preserve_for_explicit_cleanup();
    let custody =
        match QualificationUnregisteredWorkspaceCustody::new(record, Arc::clone(workspace)) {
            Ok(custody) => custody,
            Err(capture) => {
                return error.context(format!(
                    "Q partial workspace custody capture failed: {capture:#}"
                ));
            }
        };
    match owner
        .state
        .qualification_workspace_custody
        .retain_unregistered_workspace(Arc::new(custody))
    {
        Ok(()) => error,
        Err(retain) => error.context(format!(
            "Q partial workspace custody retain failed: {retain:#}"
        )),
    }
}

pub fn with_qualification_local_owner<T>(
    state: &AppState,
    placement: &str,
    coordinate: &ConsumerRuntimeVerificationCoordinate,
    binding_digest: &str,
    root: &PreparedExternalQualificationRoot,
    run: impl FnOnce(&QualificationLocalOwner<'_>) -> Result<T>,
) -> Result<T> {
    coordinate.validate()?;
    ensure!(
        lillux::valid_hash(binding_digest),
        "Q local channel digest is invalid"
    );
    state
        .controller_lifetime
        .ensure_protects_app_root(&state.config.app_root)?;
    let allocation = state
        .state_store
        .external_allocation(placement)?
        .context("Q local Worker has no retained allocation")?;
    let ExternalAllocationOwner::QualificationCandidate(owner) = &allocation.reservation.owner
    else {
        bail!("Q local Worker has no qualification owner")
    };
    ensure!(
        allocation.phase == ExternalAllocationPhase::Bound
            && owner.coordinate == *coordinate
            && owner.program == *root.program(),
        "Q local Worker changed its accepted occurrence"
    );
    let operation_id = owner.qualification_operation_id.clone();
    crate::hosted_operation::with_qualification_occurrence_contact(&operation_id, || {
        let current = state
            .state_store
            .external_allocation(placement)?
            .context("Q local Worker allocation disappeared")?;
        ensure!(
            current == allocation && current.phase == ExternalAllocationPhase::Bound,
            "Q local Worker changed its reserved occurrence"
        );
        let binding = state.state_store.external_execution_channel(placement)?;
        ensure!(
            binding.digest()? == binding_digest
                && binding.execution_binding_hash == current.reservation.binding_hash
                && binding.placement_thread_id == placement
                && matches!(
                    binding.execution_mode,
                    ryeos_state::external_execution::ExternalExecutionMode::QualificationStructuredSession {}
                )
                && binding.candidate_export_max_bytes == 0,
            "Q local Worker changed its released zero-export channel"
        );
        let (accepted_root, capsule_hash, capsule) = state
            .state_store
            .admitted_launch_capsule_with_coordinates(&coordinate.accepted_root_id)?
            .context("Q local Worker lost its accepted root capsule")?;
        ensure!(
            accepted_root == coordinate.accepted_root_id
                && capsule_hash == coordinate.accepted_capsule_hash
                && capsule == *root.capsule(),
            "Q local Worker changed its authenticated accepted root"
        );
        root.program().validate_against_purpose(root.purpose())?;
        state.engine.with_checked_bundle_generation(|generation| {
            root.current_local_worker_resolution(generation).map(|_| ())
        })?;
        run(&QualificationLocalOwner {
            state,
            placement,
            root,
        })
    })
}

/// Continue only an already journaled no-Create rollback with the same
/// daemon's retained original descriptor pair. Cleanup does not need a new
/// current Bundle resolution or permission to contact the provider.
pub fn resume_unregistered_qualification_roots(
    state: &AppState,
    placement: &str,
) -> Result<QualificationLocalStartRecord> {
    let initial = state
        .state_store
        .qualification_local_start(placement)?
        .context("Q rollback has no retained original start")?;
    crate::hosted_operation::with_qualification_occurrence_contact(
        &initial.qualification_operation_id,
        || {
            ensure!(
                state.state_store.qualification_local_start(placement)? == Some(initial.clone()),
                "Q rollback changed its original start before contact gate"
            );
            let closed = state
                .state_store
                .resume_unregistered_qualification_rollback(state, placement)?;
            if closed.phase == QualificationLocalStartPhase::Closed {
                state
                    .qualification_workspace_custody
                    .release_closed_unregistered_rollback(&closed)?;
            }
            Ok(closed)
        },
    )
}

impl QualificationLocalOwner<'_> {
    pub fn reserve_start(&self) -> Result<QualificationLocalStartRecord> {
        self.state
            .state_store
            .reserve_qualification_local_start(self.placement)
    }

    pub fn start_record(&self) -> Result<QualificationLocalStartRecord> {
        self.state
            .state_store
            .qualification_local_start(self.placement)?
            .context("Q local Worker has no original start record")
    }

    fn require_original(
        &self,
        record: &QualificationLocalStartRecord,
    ) -> Result<QualificationLocalStartRecord> {
        let current = self.start_record()?;
        ensure!(
            &current == record && record.placement_thread_id == self.placement,
            "Q local Worker changed its original start record"
        );
        Ok(current)
    }

    fn require_original_workspace_custody(
        &self,
        record: &QualificationLocalStartRecord,
        guard: &Arc<TempDirGuard>,
    ) -> Result<Arc<QualificationWorkspaceCustody>> {
        let coordinate = QualificationWorkspaceCustodyCoordinate::from_start(record)?;
        let custody = self
            .state
            .qualification_workspace_custody
            .borrow_exact(&coordinate)?;
        ensure!(
            Arc::ptr_eq(custody.workspace(), guard),
            "Q original workspace guard changed after custody registration"
        );
        Ok(custody)
    }

    /// Create both original private roots under pinned parents and register
    /// their exact descriptors before returning. No backend Create or view
    /// borrow may follow a partial registration. A failed preparation removes
    /// only children whose newly created identity is still held here.
    pub fn create_original_runtime_roots(
        &self,
        record: &QualificationLocalStartRecord,
    ) -> Result<(
        std::path::PathBuf,
        Arc<TempDirGuard>,
        lillux::PinnedDirectory,
    )> {
        let current = self.require_original(record)?;
        ensure!(
            current.phase == QualificationLocalStartPhase::Reserved,
            "Q runtime roots must follow their original reservation"
        );
        let (cache, parent) = (|| -> Result<_> {
            QualificationWorkspaceCustodyCoordinate::from_start(&current)?;
            let authority = self.state.state_store.pinned_state_authority()?;
            let cache = authority
                .runtime_directory()
                .open_or_create_child(OsStr::new("cache"), 0o700)?;
            cache.require_owner_private_directory()?;
            let parent = cache.open_or_create_child(OsStr::new("qualification-local"), 0o700)?;
            parent.require_owner_private_directory()?;
            ensure!(
                parent.path()
                    == Path::new(&current.state_root)
                        .parent()
                        .context("Q state root has no parent")?,
                "Q state-root parent differs from its reserved coordinate"
            );
            Ok((cache, parent))
        })()
        .map_err(|error| fence_precreate_cleanup(self, error))?;
        let (project, guard) = match crate::temp_dir_guard::create_runtime_workspace_in_pinned_cache(
            &cache,
            &current.workspace_id,
        ) {
            Ok(roots) => roots,
            Err(failure) => {
                if let Some(guard) = failure.retained_workspace {
                    return Err(rollback_precreate_workspace(
                        self,
                        &current,
                        failure.error,
                        &guard,
                        false,
                    ));
                }
                return Err(fence_precreate_cleanup(self, failure.error));
            }
        };
        let root = match parent.create_child(OsStr::new(&current.workspace_id), 0o700) {
            Ok(root) => root,
            Err(error) => {
                // EEXIST proves no new child was returned by this call. Any
                // other low-level failure could follow mkdir but precede the
                // descriptor return, so leave that child untouched and fence.
                let uncertain = !error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|cause| cause.kind() == std::io::ErrorKind::AlreadyExists);
                return Err(rollback_precreate_workspace(
                    self, &current, error, &guard, uncertain,
                ));
            }
        };
        let registered = (|| -> Result<()> {
            root.require_owner_private_directory()?;
            root.ensure_path_binding()?;
            ensure!(
                guard.path().as_deref() == Some(Path::new(&current.workspace_root))
                    && guard.owns_effective_path(&project)
                    && root.path() == Path::new(&current.state_root),
                "Q runtime roots differ from their reserved originals"
            );
            self.state.qualification_workspace_custody.insert_exact(
                &current,
                Arc::clone(&guard),
                &root,
                &parent,
            )?;
            Ok(())
        })();
        if let Err(error) = registered {
            let rollback = match QualificationUnregisteredRollbackCustody::new(
                &current,
                Arc::clone(&guard),
                parent,
                root,
            ) {
                Ok(custody) => Arc::new(custody),
                Err(capture) => {
                    return Err(fence_precreate_cleanup(
                        self,
                        error.context(format!(
                            "Q original rollback custody capture failed: {capture:#}"
                        )),
                    ));
                }
            };
            if let Err(retain) = self
                .state
                .qualification_workspace_custody
                .retain_unregistered_rollback(Arc::clone(&rollback))
            {
                return Err(fence_precreate_cleanup(
                    self,
                    error.context(format!(
                        "Q original rollback custody retain failed: {retain:#}"
                    )),
                ));
            }
            let closed = self
                .state
                .state_store
                .rollback_unregistered_qualification_roots(
                    self.state,
                    &current,
                    rollback.workspace(),
                    rollback.state_root_parent(),
                    rollback.state_root(),
                );
            return Err(match closed {
                Ok(closed) => {
                    self.state
                        .qualification_workspace_custody
                        .release_closed_unregistered_rollback(&closed)?;
                    error.context("Q original roots were rolled back before backend Create")
                }
                Err(cleanup) => {
                    let failure = error.context(format!(
                        "Q original-root rollback remains in protected custody: {cleanup:#}"
                    ));
                    match self.start_record() {
                        Ok(record) if record.phase == QualificationLocalStartPhase::Reserved => {
                            fence_precreate_cleanup(self, failure)
                        }
                        Ok(_) => failure,
                        Err(read) => failure
                            .context(format!("Q rollback phase could not be rechecked: {read:#}")),
                    }
                }
            });
        }
        guard.preserve_for_explicit_cleanup();
        Ok((project, guard, root))
    }

    /// Backend Create consumes only the retained base snapshot. The pinned
    /// materialization proves the exact project bytes before first contact.
    pub fn create_original_workspace(
        &self,
        record: &QualificationLocalStartRecord,
        guard: &Arc<TempDirGuard>,
        project_path: &Path,
        materialization: &ryeos_state::PinnedProjectMaterialization,
    ) -> Result<QualificationLocalStartRecord> {
        let current = self.require_original(record)?;
        let custody = self.require_original_workspace_custody(&current, guard)?;
        ensure!(
            current.phase == QualificationLocalStartPhase::Reserved
                && guard.path().as_deref() == Some(Path::new(&current.workspace_root))
                && guard.owns_effective_path(project_path)
                && materialization.snapshot_hash() == current.base_snapshot_hash
                && materialization.owns_path(project_path)?,
            "Q workspace Create changed its original base or project owner"
        );
        materialization.ensure_path_binding()?;
        guard.owned_scratch_root()?.ensure_path_binding()?;
        // A durable Q start now owns this workspace. Dropping a process-local
        // Arc cannot silently remove its backend state or original view.
        guard.preserve_for_explicit_cleanup();
        let creating = self
            .state
            .state_store
            .begin_qualification_workspace_create(self.state, self.placement)?;
        let created = self
            .state
            .isolation
            .create_workspace(
                ryeos_engine::isolation::WorkspaceLifecycleInvocation {
                    operation: ryeos_isolation_protocol::WorkspaceLifecycleOperation::Create,
                    workspace_id: &creating.workspace_id,
                    launch_owner: &creating.launch_owner,
                    base_snapshot: &creating.base_snapshot_hash,
                    project_path,
                    mount_identity: None,
                },
                &|held| {
                    let identity = crate::process::execution_process_identity_from_lillux(
                        held.exact_process_identity()
                            .map_err(|error| format!("capture Q workspace creator: {error}"))?,
                        None,
                    )
                    .map_err(|error| error.to_string())?;
                    self.state
                        .state_store
                        .attach_qualification_workspace_creator(self.placement, &identity)
                        .map(|_| ())
                        .map_err(|error| error.to_string())
                },
            )
            .map_err(|error| anyhow!("create Q original workspace: {error}"))?;
        let view = created
            .created_view
            .context("Q workspace Create omitted its original view")?;
        if let Err((view, error)) =
            guard.install_workspace_view_retaining_authority(&created.evidence, view)
        {
            custody.retain_uninstalled_view(created.evidence, view);
            return Err(error.context("Q workspace Create view was retained for cleanup"));
        }
        self.state
            .state_store
            .complete_qualification_workspace_create(self.placement, &created.evidence, guard)
    }

    /// Allocate one scope only after its exact intent is durable. A failed
    /// allocation or bind leaves the Q start fenced for cleanup review.
    pub fn allocate_original_scope(
        &self,
        record: &QualificationLocalStartRecord,
    ) -> Result<(lillux::ProcessScopeAllocation, lillux::ProcessScope)> {
        let current = self.require_original(record)?;
        ensure!(
            current.phase == QualificationLocalStartPhase::WorkspaceReady,
            "Q scope requires its original ready workspace"
        );
        let name = format!(
            "qualification-{}",
            &lillux::sha256_hex(current.workspace_id.as_bytes())[..32]
        );
        let allocation = self.state.isolation.plan_process_scope(&name)?;
        self.state
            .state_store
            .begin_qualification_scope_allocation(self.placement, &allocation)?;
        let scope = self.state.isolation.allocate_process_scope(&allocation)?;
        let recovery = scope.recovery().clone();
        if let Err(error) = self
            .state
            .state_store
            .bind_qualification_scope(self.placement, &recovery)
        {
            let retained = self
                .state
                .state_store
                .retain_failed_qualification_scope_recovery(self.placement, &recovery);
            // Use the original admitted control ceiling. Reading mutable node
            // policy here could fail after rotation and skip retirement.
            let retirement = scope.retire_unlaunched(recovery.control_timeout());
            return Err(error.context(format!(
                "Q scope bind failed; retained recovery: {retained:?}; retirement: {retirement:?}; exact recovery: {recovery:?}"
            )));
        }
        Ok((allocation, scope))
    }

    /// The original guard may lend its view only after the Q journal has
    /// reached the exact ScopeBound owner. No path lookup supplies authority.
    pub fn borrow_workspace_view(
        &self,
        record: &QualificationLocalStartRecord,
        guard: &Arc<TempDirGuard>,
    ) -> Result<lillux::InheritedDescriptorAuthority> {
        let current = self.require_original(record)?;
        self.require_original_workspace_custody(&current, guard)?;
        ensure!(
            current.phase == QualificationLocalStartPhase::ScopeBound
                && current.view_borrow_intent.is_none()
                && guard.path().as_deref() == Some(Path::new(&current.workspace_root)),
            "Q workspace view has no scope-bound original owner"
        );
        self.state
            .engine
            .with_checked_bundle_generation(|generation| {
                self.root
                    .current_local_worker_resolution(generation)
                    .map(|_| ())
            })?;
        let receipt = current
            .workspace_receipt
            .as_ref()
            .context("Q workspace view has no accepted Create receipt")?;
        ensure!(
            guard.workspace_view_identity()?.as_ref()
                == Some(&(current.workspace_id.clone(), receipt.view_identity.clone())),
            "Q workspace view differs from its retained receipt"
        );
        let admitted = self
            .state
            .state_store
            .reserve_qualification_workspace_view_borrow(self.placement)?;
        ensure!(
            admitted.view_borrow_intent.as_ref().is_some_and(|intent| {
                intent.workspace_id == current.workspace_id
                    && intent.view_identity == receipt.view_identity
            }),
            "Q workspace view borrow changed its original intent"
        );
        guard
            .borrow_workspace_view(&current.workspace_id, &receipt.view_identity)?
            .context("Q requires an enforced original workspace view")
    }

    /// Recheck the live, original Q authority at the last point before the
    /// held process is created. Material and connector preparation may have
    /// taken time after the view was first borrowed.
    pub fn authorize_spawn(&self) -> Result<()> {
        self.state
            .state_store
            .authorize_qualification_local_spawn(self.placement)?;
        self.state
            .engine
            .with_checked_bundle_generation(|generation| {
                self.root
                    .current_local_worker_resolution(generation)
                    .map(|_| ())
            })
    }

    pub fn attach_worker_process(
        &self,
        identity: &crate::process::ExecutionProcessIdentity,
    ) -> Result<()> {
        self.state.state_store.attach_qualification_worker_process(
            self.placement,
            identity,
            crate::runtime_db::daemon_generation_id(),
        )?;
        Ok(())
    }

    pub fn mark_cleanup_unproved(&self, reason: &str) -> Result<()> {
        self.state
            .state_store
            .mark_qualification_local_cleanup_unproved(self.placement, reason)?;
        Ok(())
    }
}

/// Reap proof is a later process-retirement observation, so this method does
/// not reenter the contact gate held during held-launch attachment.
pub fn settle_qualification_worker_process(state: &AppState, placement: &str) -> Result<()> {
    state
        .state_store
        .settle_qualification_worker_process(placement)?;
    Ok(())
}

/// A second process-local retirement observation can verify the already
/// settled Q row without replaying its one-use terminal transition. This is
/// only an exact death read; it confers no native or qualification claim.
pub fn ensure_qualification_worker_terminal(
    state: &AppState,
    placement: &str,
    expected: &crate::process::ExecutionProcessIdentity,
) -> Result<()> {
    state
        .controller_lifetime
        .ensure_protects_app_root(&state.config.app_root)?;
    let process = state
        .state_store
        .qualification_worker_process(placement)?
        .context("Q local Worker has no retained process")?;
    let start = state
        .state_store
        .qualification_local_start(placement)?
        .context("Q local Worker has no original start")?;
    let expected_incarnation = expected.owner_incarnation_digest()?;
    ensure!(
        process.phase == QualificationWorkerProcessPhase::Terminal
            && process.process_identity == *expected
            && process.placement_thread_id == placement
            && process.accepted_root_id == start.accepted_root_id
            && process.accepted_capsule_hash == start.accepted_capsule_hash
            && process.qualification_operation_id == start.qualification_operation_id
            && process.qualification_occurrence_id == start.qualification_occurrence_id
            && process.channel_binding_digest == start.channel_binding_digest
            && process.daemon_generation_id == start.daemon_generation_id
            && start.worker_incarnation_digest.as_deref() == Some(expected_incarnation.as_str()),
        "Q local Worker terminal row changed its exact original process"
    );
    Ok(())
}

pub fn mark_qualification_local_cleanup_unproved(
    state: &AppState,
    placement: &str,
    reason: &str,
) -> Result<()> {
    let allocation = state
        .state_store
        .external_allocation(placement)?
        .context("Q cleanup has no retained allocation")?;
    let ExternalAllocationOwner::QualificationCandidate(owner) = &allocation.reservation.owner
    else {
        bail!("Q cleanup has no qualification owner")
    };
    crate::hosted_operation::with_qualification_occurrence_contact(
        &owner.qualification_operation_id,
        || {
            state
                .state_store
                .mark_qualification_local_cleanup_unproved(placement, reason)?;
            Ok(())
        },
    )
}

/// Retire the original Q local workspace after the one-shot Worker and
/// connector have settled. Every effect follows its durable Q cleanup phase;
/// an interrupted phase can be resumed only with this daemon's original
/// workspace and state-root descriptors. This grants no qualification claim.
pub fn cleanup_qualification_local_worker(
    state: &AppState,
    placement: &str,
    view_close_deadline: lillux::time::MonotonicDeadline,
) -> Result<QualificationLocalStartRecord> {
    state
        .controller_lifetime
        .ensure_protects_app_root(&state.config.app_root)?;
    let initial = state
        .state_store
        .qualification_local_start(placement)?
        .context("Q local cleanup has no original start")?;
    crate::hosted_operation::with_qualification_occurrence_contact(
        &initial.qualification_operation_id,
        || {
            let mut current = state
                .state_store
                .qualification_local_start(placement)?
                .context("Q local cleanup lost its original start")?;
            ensure!(
                current == initial,
                "Q local cleanup changed its original start before contact gate"
            );
            if current.phase == QualificationLocalStartPhase::Closed {
                state
                    .qualification_workspace_custody
                    .release_closed_original(&current)?;
                return Ok(current);
            }
            let coordinate = QualificationWorkspaceCustodyCoordinate::from_start(&current)?;
            let custody = state
                .qualification_workspace_custody
                .borrow_exact(&coordinate)?;
            if let Some(process) = state.state_store.qualification_worker_process(placement)? {
                ensure_qualification_worker_terminal(state, placement, &process.process_identity)?;
                crate::process::assert_reaped_process_group_absent(&process.process_identity)?;
            } else {
                ensure!(
                    current.worker_incarnation_digest.is_none(),
                    "Q local cleanup lost an attached Worker process"
                );
            }
            ensure!(
                state.state_store.external_connector_retired(placement)?,
                "Q local cleanup retains a live connector"
            );
            // The generic signed backend protocol has no idempotent-Destroy
            // contract. A later retry is allowed only for the captured Lillux
            // backend, whose exact-state Destroy accepts a missing or partly
            // removed overlay after a lost response.
            let mut fresh_destroy_intent = false;
            loop {
                current = match current.phase {
                    QualificationLocalStartPhase::Reserved
                    | QualificationLocalStartPhase::WorkspaceReady
                    | QualificationLocalStartPhase::ScopeAllocating
                    | QualificationLocalStartPhase::ScopeBound
                    | QualificationLocalStartPhase::WorkerAttached
                    | QualificationLocalStartPhase::CleanupUnproved => state
                        .state_store
                        .begin_qualification_local_cleanup(state, placement, &custody)?,
                    QualificationLocalStartPhase::CleanupIntent => {
                        if current.workspace_receipt.is_some() {
                            state
                                .state_store
                                .reserve_qualification_local_scope_retirement(&current)?
                        } else {
                            state
                                .state_store
                                .remove_qualification_workspace_root(state, &current, &custody)?
                        }
                    }
                    QualificationLocalStartPhase::ScopeRetirementPending => state
                        .state_store
                        .complete_qualification_local_scope_retirement(&current)?,
                    QualificationLocalStartPhase::ScopeRetired => {
                        state.state_store.close_qualification_local_view(
                            state,
                            &current,
                            &custody,
                            view_close_deadline,
                        )?
                    }
                    QualificationLocalStartPhase::ViewClosed => {
                        let pending = state
                            .state_store
                            .reserve_qualification_workspace_destroy(&current)?;
                        fresh_destroy_intent = true;
                        pending
                    }
                    QualificationLocalStartPhase::DestroyPending => {
                        let receipt = current
                            .workspace_receipt
                            .as_ref()
                            .context("Q Destroy lost its original Create receipt")?;
                        let (backend_id, backend_version) =
                            state.isolation.workspace_backend_identity()?;
                        ensure!(
                            backend_id == receipt.backend_id
                                && backend_version == receipt.backend_version
                                && receipt.mount_identity == receipt.view_identity,
                            "Q Destroy changed its retained signed backend"
                        );
                        ensure!(
                            fresh_destroy_intent || backend_id == "linux-lillux",
                            "Q Destroy outcome is unknown for a backend without proven repeatable Destroy"
                        );
                        fresh_destroy_intent = false;
                        let workspace = custody.workspace();
                        let original = workspace.owned_scratch_root()?;
                        original.ensure_path_binding()?;
                        ensure!(
                            workspace.path().as_deref() == Some(Path::new(&current.workspace_root)),
                            "Q Destroy lost its original workspace guard"
                        );
                        let project = original
                            .open_child_directory(OsStr::new(
                                ryeos_engine::execution_workspace::PROJECT_DIR,
                            ))?
                            .context("Q Destroy lost its original project directory")?;
                        project.ensure_path_binding()?;
                        ensure!(
                            workspace.owns_effective_path(project.path()),
                            "Q Destroy changed its original project view"
                        );
                        let evidence = state
                            .isolation
                            .workspace_lifecycle(ryeos_engine::isolation::WorkspaceLifecycleInvocation {
                                operation: ryeos_isolation_protocol::WorkspaceLifecycleOperation::Destroy,
                                workspace_id: &current.workspace_id,
                                launch_owner: &current.launch_owner,
                                base_snapshot: &current.base_snapshot_hash,
                                project_path: project.path(),
                                mount_identity: Some(&receipt.mount_identity),
                            })
                            .map_err(|error| anyhow!("destroy Q original workspace: {error}"))?;
                        state
                            .state_store
                            .record_qualification_workspace_destroyed(&current, &evidence)?
                    }
                    QualificationLocalStartPhase::Destroyed => state
                        .state_store
                        .remove_qualification_workspace_root(state, &current, &custody)?,
                    QualificationLocalStartPhase::WorkspaceRemoved => state
                        .state_store
                        .remove_qualification_state_root(state, &current, &custody)?,
                    QualificationLocalStartPhase::StateRemoved => state
                        .state_store
                        .close_qualification_local_start(state, &current, &custody)?,
                    QualificationLocalStartPhase::Closed => {
                        state
                            .qualification_workspace_custody
                            .release_closed_original(&current)?;
                        return Ok(current);
                    }
                    QualificationLocalStartPhase::WorkspaceCreating => {
                        bail!("Q local cleanup lacks completed Create or no-Create proof")
                    }
                };
            }
        },
    )
}
