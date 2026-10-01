//! Application custody for one qualification-only local Worker start.
//!
//! The executor supplies original descriptor owners, while the app retains
//! the Q allocation and its one-use private workspace and process journal.

use std::ffi::OsStr;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context as _, Result, bail, ensure};
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

#[path = "external_qualification_local/controller_canary.rs"]
mod controller_canary;
pub(crate) use controller_canary::PreparedQualificationControllerCanary;

/// This borrowed token exists only while the existing qualification occurrence
/// contact gate is held. Its methods still recheck the durable Q journal.
pub struct QualificationLocalOwner<'a> {
    state: &'a AppState,
    placement: &'a str,
    root: &'a PreparedExternalQualificationRoot,
}

fn require_credential_contract(
    record: &QualificationLocalStartRecord,
    root: &PreparedExternalQualificationRoot,
) -> Result<()> {
    let custody = record
        .credential_custody
        .as_ref()
        .context("Q has no credential profile custody")?;
    let contract = root.worker_launch_contract();
    ensure!(
        custody.released_at_ms.is_none()
            && custody.profile_id == contract.credential_profile_id
            && custody.owner_principal == root.purpose().owner_fingerprint
            && custody.contract_digest == contract.digest()?
            && custody.workload_family == contract.credential_requirement.workload_family
            && custody.subject_projection_contract
                == contract.credential_requirement.subject_projection_contract,
        "Q credential custody differs from its accepted launch contract"
    );
    Ok(())
}

/// Read the live credential fence while awaiting the admitted turn. A changed
/// generation/revocation refuses further execution; cleanup uses historical
/// custody and does not depend on this check.
pub fn ensure_qualification_worker_credential_current(
    state: &AppState,
    placement: &str,
    root: &PreparedExternalQualificationRoot,
) -> Result<()> {
    let record = state
        .state_store
        .require_live_qualification_local_credential_custody(placement)?;
    require_credential_contract(&record, root)
}

/// Release this credential fence only after the existing exact process and
/// connector settlement. It neither recreates lost Q directories nor requires
/// a live callback/current credential generation. No profile-home IO occurs.
pub fn release_qualification_worker_credential_custody(
    state: &AppState,
    placement: &str,
) -> Result<QualificationLocalStartRecord> {
    let record = state
        .state_store
        .qualification_local_start(placement)?
        .context("Q credential cleanup has no original start")?;
    if record
        .credential_custody
        .as_ref()
        .is_none_or(|c| c.released_at_ms.is_some())
    {
        return Ok(record);
    }
    if let Some(process) = state.state_store.qualification_worker_process(placement)? {
        crate::process::assert_reaped_process_group_absent(&process.process_identity)?;
        if process.phase == QualificationWorkerProcessPhase::Attached {
            state
                .state_store
                .settle_qualification_worker_process(placement)?;
        }
    }
    state
        .state_store
        .release_qualification_local_credential_custody(placement)
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
        error.context("Q original roots were removed before private workspace preparation")
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
    pub fn select_signed_worker_request(
        &self,
    ) -> Result<crate::external_qualification_command::SignedQualificationWorkerRequest> {
        crate::external_qualification_command::select_for_original_owner(
            self.state,
            self.placement,
            self.root,
            false,
        )
    }

    pub fn reserve_start(&self) -> Result<QualificationLocalStartRecord> {
        self.state
            .state_store
            .reserve_qualification_local_start(self.placement)?;
        let contract = self.root.worker_launch_contract();
        let admission = self.root.authenticate_invocation(self.state)?;
        self.state
            .state_store
            .claim_qualification_local_credential_custody(
                self.state,
                self.placement,
                &contract.credential_profile_id,
                &self.root.purpose().owner_fingerprint,
                &contract.digest()?,
                &contract.credential_requirement.workload_family,
                &contract.credential_requirement.subject_projection_contract,
                admission,
            )
    }

    /// Open the existing private profile home after exact credential admission.
    /// It is separately owned; Q's disposable state cleanup never removes it.
    pub fn credential_home(&self) -> Result<lillux::PinnedDirectory> {
        let record = self
            .state
            .state_store
            .require_live_qualification_local_credential_custody(self.placement)?;
        require_credential_contract(&record, self.root)?;
        let credential = record
            .credential_custody
            .as_ref()
            .context("Q has no admitted credential custody")?;
        crate::private_artifact_home::require_within_default_limit(
            &self.state.config.runtime_state_dir(),
            &credential.home_id,
        )?;
        let path = crate::private_artifact_home::home_path(
            &self.state.config.runtime_state_dir(),
            &credential.home_id,
        )?;
        let home = lillux::PinnedDirectory::open(&path)?
            .context("Q credential profile home is missing")?;
        home.require_owner_private_directory()?;
        home.ensure_path_binding()?;
        Ok(home)
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
    /// their exact descriptors before returning. No private-directory borrow
    /// may follow a partial registration. A failed preparation removes
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
                    error.context(
                        "Q original roots were rolled back before private workspace preparation",
                    )
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

    /// Retain the exact privately copied project before borrowing it for the
    /// trusted controller Worker. No backend view or kernel scope is created.
    pub fn prepare_original_workspace(
        &self,
        record: &QualificationLocalStartRecord,
        guard: &Arc<TempDirGuard>,
        project_path: &Path,
        materialization: &ryeos_state::PinnedProjectMaterialization,
    ) -> Result<QualificationLocalStartRecord> {
        let current = self.require_original(record)?;
        self.require_original_workspace_custody(&current, guard)?;
        guard.require_uncreated_private_workspace()?;
        ensure!(
            current.phase == QualificationLocalStartPhase::Reserved
                && guard.path().as_deref() == Some(Path::new(&current.workspace_root))
                && guard.owns_effective_path(project_path)
                && materialization.snapshot_hash() == current.base_snapshot_hash
                && materialization.owns_path(project_path)?,
            "Q private workspace changed its original base or project owner"
        );
        materialization.ensure_path_binding()?;
        let root = guard.owned_scratch_root()?;
        root.ensure_path_binding()?;
        let project = root
            .open_child_directory(OsStr::new(ryeos_engine::execution_workspace::PROJECT_DIR))?
            .context("Q original private project is absent")?;
        project.ensure_path_binding()?;
        ensure!(
            project.path() == project_path
                && project.is_same_directory(&materialization.try_clone_root()?)?,
            "Q private project changed its materialized directory identity"
        );
        guard.preserve_for_explicit_cleanup();
        self.state
            .state_store
            .complete_qualification_private_workspace(self.placement, project.identity()?)
    }

    /// Lend only the original private project's descriptor after journaling
    /// the one-use borrow. A persisted identity is never a reopening permit.
    pub fn borrow_workspace_directory(
        &self,
        record: &QualificationLocalStartRecord,
        guard: &Arc<TempDirGuard>,
    ) -> Result<lillux::InheritedDescriptorAuthority> {
        let current = self.require_original(record)?;
        self.require_original_workspace_custody(&current, guard)?;
        guard.require_uncreated_private_workspace()?;
        ensure!(
            current.phase == QualificationLocalStartPhase::WorkspaceReady
                && current.directory_borrow_intent.is_none()
                && guard.path().as_deref() == Some(Path::new(&current.workspace_root)),
            "Q directory borrow has no original private workspace owner"
        );
        self.state
            .engine
            .with_checked_bundle_generation(|generation| {
                self.root
                    .current_local_worker_resolution(generation)
                    .map(|_| ())
            })?;
        let root = guard.owned_scratch_root()?;
        root.ensure_path_binding()?;
        let project = root
            .open_child_directory(OsStr::new(ryeos_engine::execution_workspace::PROJECT_DIR))?
            .context("Q original private project is absent")?;
        project.ensure_path_binding()?;
        let identity = project.identity()?;
        ensure!(
            current.workspace_identity.as_ref() == Some(&identity)
                && guard.owns_effective_path(project.path()),
            "Q private project differs from its retained identity"
        );
        let admitted = self
            .state
            .state_store
            .reserve_qualification_workspace_directory_borrow(self.placement)?;
        ensure!(
            admitted
                .directory_borrow_intent
                .as_ref()
                .is_some_and(|intent| {
                    intent.workspace_id == current.workspace_id
                        && intent.directory_identity == identity
                }),
            "Q directory borrow changed its original intent"
        );
        project.inherited_descriptor_authority()
    }

    /// Recheck the live, original Q authority at the last point before the
    /// held process is created. Material and connector preparation may have
    /// taken time after the directory was first borrowed.
    pub fn authorize_spawn(&self) -> Result<()> {
        self.state
            .engine
            .with_checked_bundle_generation(|generation| {
                self.root
                    .current_local_worker_resolution(generation)
                    .map(|_| ())
            })?;
        let admission = self.root.authenticate_invocation(self.state)?;
        self.state
            .state_store
            .authorize_qualification_local_spawn(self.state, self.placement, admission)
            .map(|_| ())
    }

    /// Attachment retains an already-created process even after caller expiry.
    /// Executable release separately requires fresh invoking authority; a
    /// refused release leaves the exact held process available for retirement.
    pub fn authorize_worker_release(&self) -> Result<()> {
        let admission = self.root.authenticate_invocation(self.state)?;
        self.state
            .state_store
            .authorize_qualification_worker_release(self.state, self.placement, admission)
    }

    pub fn attach_worker_process(
        &self,
        identity: &crate::process::ExecutionProcessIdentity,
        configuration: &crate::external_connector::PreparedProviderConfigurationObservation,
    ) -> Result<()> {
        self.state.state_store.attach_qualification_worker_process(
            self.placement,
            identity,
            crate::runtime_db::daemon_generation_id(),
            configuration,
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
            // Same cleanup gate as normal retirement: preserve the original
            // terminal observation before releasing any credential/root custody.
            // Missing live custody is a refusal, never a reason to recreate it.
            final_canary_attempt_then_cleanup(
                || retain_controller_canary_terminal_under_gate(state, &initial),
                || {
                    // Credential retirement can succeed even when this daemon has
                    // lost original disposable-directory descriptors after restart.
                    current = release_qualification_worker_credential_custody(state, placement)?;
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
                    if let Some(process) =
                        state.state_store.qualification_worker_process(placement)?
                    {
                        ensure_qualification_worker_terminal(
                            state,
                            placement,
                            &process.process_identity,
                        )?;
                        crate::process::assert_reaped_process_group_absent(
                            &process.process_identity,
                        )?;
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
                    loop {
                        current = match current.phase {
                            QualificationLocalStartPhase::Reserved
                            | QualificationLocalStartPhase::WorkspaceReady
                            | QualificationLocalStartPhase::WorkerAttached
                            | QualificationLocalStartPhase::CleanupUnproved => state
                                .state_store
                                .begin_qualification_local_cleanup(state, placement, &custody)?,
                            QualificationLocalStartPhase::CleanupIntent => state
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
                        };
                    }
                },
            )
        },
    )
}

/// Retain original post-death canary facts before the caller releases credential
/// custody. Failure is a semantic refusal, not permission to skip already-proven
/// credential/process/provider cleanup. This function requires no live callback.
pub fn retain_qualification_controller_canary_terminal(
    state: &AppState,
    placement: &str,
) -> Result<()> {
    state
        .controller_lifetime
        .ensure_protects_app_root(&state.config.app_root)?;
    let initial = state
        .state_store
        .qualification_local_start(placement)?
        .context("Q terminal canary has no original local start")?;
    crate::hosted_operation::with_qualification_occurrence_contact(
        &initial.qualification_operation_id,
        || {
            let current = state
                .state_store
                .qualification_local_start(placement)?
                .context("Q terminal canary lost original local start")?;
            ensure!(
                current == initial,
                "Q terminal canary changed before its cleanup gate"
            );
            retain_controller_canary_terminal_under_gate(state, &current)
        },
    )
}

/// Both callers already hold the original occurrence contact/removal gate.
/// This method never renews callback authority or reopens a lost canary path.
fn retain_controller_canary_terminal_under_gate(
    state: &AppState,
    current: &QualificationLocalStartRecord,
) -> Result<()> {
    if current.controller_canary_intent.is_none() || current.controller_canary_terminal.is_some() {
        return Ok(());
    }
    // A preparation that never bound or attached cannot publish a successful
    // transcript. Its cleanup must not demand nonexistent terminal file proof.
    if current.controller_canary_binding.is_none() || current.worker_incarnation_digest.is_none() {
        return Ok(());
    }
    let placement = &current.placement_thread_id;
    let process = state
        .state_store
        .qualification_worker_process(placement)?
        .context("Q terminal canary has no exact process")?;
    crate::process::assert_reaped_process_group_absent(&process.process_identity)?;
    ensure_qualification_worker_terminal(state, placement, &process.process_identity)?;
    ensure!(
        state.state_store.external_connector_retired(placement)?,
        "Q terminal canary retains a live connector"
    );
    let coordinate = QualificationWorkspaceCustodyCoordinate::from_start(current)?;
    let custody = state
        .qualification_workspace_custody
        .borrow_exact(&coordinate)?;
    state
        .state_store
        .retain_qualification_controller_canary_terminal(state, current, &custody)?;
    Ok(())
}

/// Keep the finite semantic attempt ahead of cleanup, but never let its error
/// short-circuit the existing independent process/credential/removal proofs.
fn final_canary_attempt_then_cleanup<T>(
    attempt: impl FnOnce() -> Result<()>,
    cleanup: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let observation = attempt();
    let cleanup = cleanup();
    match (observation, cleanup) {
        (Ok(()), Ok(value)) => Ok(value),
        (Err(error), Ok(_)) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(cleanup)) => Err(error.context(format!(
            "Q canary final observation refused and local cleanup remains incomplete: {cleanup:#}"
        ))),
    }
}

#[cfg(test)]
mod canary_cleanup_tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn final_observation_precedes_cleanup_and_refusal_does_not_skip_cleanup() {
        for refuse_observation in [false, true] {
            for refuse_cleanup in [false, true] {
                let steps = RefCell::new(Vec::new());
                let result = final_canary_attempt_then_cleanup(
                    || {
                        steps.borrow_mut().push("original terminal observation");
                        if refuse_observation {
                            bail!("missing original custody");
                        }
                        Ok(())
                    },
                    || {
                        steps.borrow_mut().push("independently proven cleanup");
                        if refuse_cleanup {
                            bail!("resource removal incomplete");
                        }
                        Ok(7)
                    },
                );
                assert_eq!(
                    *steps.borrow(),
                    vec![
                        "original terminal observation",
                        "independently proven cleanup"
                    ]
                );
                match (refuse_observation, refuse_cleanup) {
                    (false, false) => assert_eq!(result.unwrap(), 7),
                    (true, false) => assert!(
                        format!("{:#}", result.unwrap_err()).contains("missing original custody")
                    ),
                    (false, true) => assert!(
                        format!("{:#}", result.unwrap_err())
                            .contains("resource removal incomplete")
                    ),
                    (true, true) => {
                        let message = format!("{:#}", result.unwrap_err());
                        assert!(
                            message.contains("missing original custody")
                                && message.contains("resource removal incomplete")
                        );
                    }
                }
            }
        }
    }
}
