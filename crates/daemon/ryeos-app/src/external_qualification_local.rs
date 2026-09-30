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

    /// Create and retain the original runtime workspace only after its Q
    /// coordinate is durable. Even a failed later checkout leaves the exact
    /// directory for explicit cleanup instead of dropping it implicitly.
    pub fn create_original_workspace_root(
        &self,
        record: &QualificationLocalStartRecord,
        state_root: &lillux::PinnedDirectory,
    ) -> Result<(std::path::PathBuf, Arc<TempDirGuard>)> {
        let current = self.require_original(record)?;
        ensure!(
            current.phase == QualificationLocalStartPhase::Reserved,
            "Q runtime workspace must follow its original reservation"
        );
        let authority = self.state.state_store.pinned_state_authority()?;
        let cache = authority
            .runtime_directory()
            .open_or_create_child(OsStr::new("cache"), 0o700)?;
        cache.require_owner_private_directory()?;
        let (project, guard) = crate::temp_dir_guard::create_runtime_workspace_in_pinned_cache(
            &cache,
            &current.workspace_id,
        )?;
        ensure!(
            guard.path().as_deref() == Some(Path::new(&current.workspace_root))
                && guard.owns_effective_path(&project)
                && state_root.path() == Path::new(&current.state_root),
            "Q runtime workspace differs from its reserved original"
        );
        // The daemon keeps the original view and state-root descriptors from
        // this point onward. Every later start error can leave a durable
        // cleanup fence without losing its physical close owner.
        self.state.qualification_workspace_custody.insert_exact(
            &current,
            Arc::clone(&guard),
            state_root,
        )?;
        guard.preserve_for_explicit_cleanup();
        Ok((project, guard))
    }

    /// Create the one private provider state directory by a pinned parent and
    /// the journaled name. An existing child is never adopted for a new start.
    pub fn create_state_root(
        &self,
        record: &QualificationLocalStartRecord,
    ) -> Result<lillux::PinnedDirectory> {
        let current = self.require_original(record)?;
        ensure!(
            current.phase == QualificationLocalStartPhase::Reserved,
            "Q provider state root must precede workspace Create"
        );
        let authority = self.state.state_store.pinned_state_authority()?;
        let cache = authority
            .runtime_directory()
            .open_or_create_child(OsStr::new("cache"), 0o700)?;
        cache.require_owner_private_directory()?;
        let parent = cache.open_or_create_child(OsStr::new("qualification-local"), 0o700)?;
        parent.require_owner_private_directory()?;
        let root = parent.create_child(OsStr::new(&current.workspace_id), 0o700)?;
        root.require_owner_private_directory()?;
        root.ensure_path_binding()?;
        ensure!(
            root.path() == Path::new(&current.state_root),
            "Q provider state root differs from its reserved coordinate"
        );
        Ok(root)
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
