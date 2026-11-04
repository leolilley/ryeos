//! Thread and execution workspace bindings and settlement.
use super::*;

pub(super) enum WorkspaceProcessSettlementOwner {
    Live,
    Abandoned,
}

impl StateStore {
    pub fn thread_workspace_binding(
        &self,
        thread_id: &str,
    ) -> Result<Option<runtime_db::RuntimeWorkspaceBinding>> {
        self.lock()?.runtime_db.thread_workspace_binding(thread_id)
    }

    /// Revalidate an already-bound view immediately before borrowing its
    /// descriptor. This is contact admission, not another binding: an active
    /// root may borrow its original view without replaying Ready-only birth.
    pub fn authorize_thread_workspace_contact(
        &self,
        thread_id: &str,
        expected_binding: &runtime_db::RuntimeWorkspaceBinding,
    ) -> Result<()> {
        let g = self.lock()?;
        if !self
            .process_attachment_admission_open
            .load(Ordering::Acquire)
        {
            bail!("workspace contact is fenced during daemon shutdown");
        }
        if g.runtime_db.thread_workspace_binding(thread_id)?.as_ref() != Some(expected_binding) {
            bail!("workspace contact does not match the exact retained binding");
        }
        let thread = g
            .state_db
            .get_thread(thread_id)?
            .ok_or_else(|| anyhow!("workspace contact thread is absent"))?;
        if is_terminal_status(&thread.status) {
            bail!("terminal thread cannot acquire workspace contact");
        }
        let claim = g
            .runtime_db
            .get_launch_claim(thread_id)?
            .ok_or_else(|| anyhow!("workspace contact has no current launch owner"))?;
        if !g.active_launch_owners.contains(&claim.claimed_by) {
            bail!("workspace contact requires its active launch owner");
        }
        Self::authorize_thread_workspace_contact_locked(&g, thread_id)
    }

    /// An indexed bounded page of all unsettled members, not just attached
    /// processes. Pre-attachment ambiguity must remain visible to recovery.
    pub fn workspace_members_after(
        &self,
        workspace_id: &str,
        view_identity: &str,
        after_thread_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<runtime_db::RuntimeWorkspaceMember>> {
        self.lock()?.runtime_db.workspace_members_after(
            workspace_id,
            view_identity,
            after_thread_id,
            limit,
        )
    }

    pub fn workspace_members_for_recovery_after(
        &self,
        workspace_id: &str,
        after_thread_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<runtime_db::RuntimeWorkspaceMember>> {
        self.lock()?
            .runtime_db
            .workspace_members_for_recovery_after(workspace_id, after_thread_id, limit)
    }

    pub fn execution_workspace_has_members(&self, workspace_id: &str) -> Result<bool> {
        self.lock()?
            .runtime_db
            .execution_workspace_has_members(workspace_id)
    }

    /// Classify an ownerless root from its existing durable workspace journal.
    /// This grants neither execution nor cleanup: membership and unfinished
    /// freeze authority stay fenced until their exact owners settle them.
    /// A NULL PID, terminal lifecycle, or arbitrary lookup failure is not proof.
    pub fn has_retained_workspace_quarantine(&self, thread_id: &str) -> Result<bool> {
        let g = self.lock()?;
        let Some(workspace) = g.runtime_db.workspace_for_thread(thread_id)? else {
            return Ok(false);
        };
        let Some(raw_owner) = workspace.launch_owner.as_deref() else {
            return Ok(false);
        };
        let owner: runtime_db::LaunchOwner = serde_json::from_str(raw_owner)?;
        let active = &g.active_launch_owners;
        if owner.thread_id != thread_id || active.contains(raw_owner) {
            return Ok(false);
        }
        if g.runtime_db
            .get_launch_claim(thread_id)?
            .is_some_and(|claim| claim.owner != owner || active.contains(&claim.claimed_by))
        {
            return Ok(false);
        }
        Ok(g.runtime_db
            .execution_workspace_has_members(&workspace.workspace_id)?
            || (workspace.state == runtime_db::WorkspaceState::Freezing
                && workspace.frozen_snapshot_hash.is_none()))
    }

    pub(super) fn authorize_thread_workspace_contact_locked(
        g: &Inner,
        thread_id: &str,
    ) -> Result<()> {
        if let Some(binding) = g.runtime_db.thread_workspace_binding(thread_id)? {
            Self::authorize_workspace_root_lifecycle_locked(g, &binding)?;
            g.runtime_db
                .authorize_workspace_binding(thread_id, &binding, false)?;
        }
        // Absence does not manufacture membership. Immutable-input/projectless
        // launches legitimately have none; enforced view delivery separately
        // requires its exact binding before selecting a view descriptor.
        Ok(())
    }

    pub(super) fn authorize_workspace_root_lifecycle_locked(
        g: &Inner,
        binding: &runtime_db::RuntimeWorkspaceBinding,
    ) -> Result<()> {
        let workspace = g
            .runtime_db
            .workspace(&binding.workspace_id)?
            .ok_or_else(|| anyhow!("workspace owner is absent"))?;
        let root = workspace
            .thread_id
            .as_deref()
            .ok_or_else(|| anyhow!("workspace has no root thread"))?;
        let root_thread = g
            .state_db
            .get_thread(root)?
            .ok_or_else(|| anyhow!("workspace root thread is absent"))?;
        if is_terminal_status(&root_thread.status) {
            bail!("workspace root is terminal and cannot admit process contact");
        }
        Ok(())
    }

    /// Explicit settlement by the still-live exact launch owner. The caller
    /// MUST already have settled its request/held-launch lifelines and any
    /// unrecorded pre-attachment process. This method cannot infer that proof
    /// from a NULL PID, terminal status, or a removed stale claim.
    ///
    /// Cold recovery cannot use this API: uncertain old contact remains bound.
    /// Removing membership also does not prove physical view closure; the
    /// original workspace slot retains that separate last-owner obligation.
    pub fn settle_thread_workspace_owned(
        &self,
        thread_id: &str,
        binding: &runtime_db::RuntimeWorkspaceBinding,
    ) -> Result<bool> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let claim = g
            .runtime_db
            .get_launch_claim(thread_id)?
            .ok_or_else(|| anyhow!("workspace settlement requires its live launch claim"))?;
        let active = &g.active_launch_owners;
        if claim.owner != binding.borrower_launch_owner || !active.contains(&claim.claimed_by) {
            bail!("workspace settlement requires the exact active launch owner");
        }
        let thread = g
            .state_db
            .get_thread(thread_id)?
            .ok_or_else(|| anyhow!("workspace settlement thread is absent"))?;
        if !is_terminal_status(&thread.status) {
            return Ok(false);
        }
        let runtime = g
            .runtime_db
            .get_runtime_info(thread_id)?
            .ok_or_else(|| anyhow!("workspace settlement runtime row is absent"))?;
        if runtime.process_identity.is_some()
            || g.runtime_db
                .in_process_handler_reservation(thread_id)?
                .is_some()
        {
            return Ok(false);
        }
        match g.runtime_db.thread_workspace_binding(thread_id)? {
            None => Ok(true),
            Some(existing) if existing == *binding => g
                .runtime_db
                .clear_thread_workspace_owned(thread_id, binding),
            Some(_) => bail!("workspace settlement cannot erase another exact binding"),
        }
    }

    /// Retire the root's workspace-view membership after an externally hosted
    /// candidate has completed and its exact provider occurrence is proved
    /// terminal. There is no node-local PID to reap for this execution class;
    /// the retained external allocation and completion fence are its process-
    /// lifetime authority. The ordinary workspace owner still performs the
    /// separate freeze and physical view closure.
    pub fn settle_completed_external_workspace_owned(
        &self,
        placement_thread_id: &str,
    ) -> Result<bool> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let binding = match g.runtime_db.thread_workspace_binding(placement_thread_id)? {
            Some(binding) => binding,
            None => return Ok(true),
        };
        let claim = g
            .runtime_db
            .get_launch_claim(placement_thread_id)?
            .ok_or_else(|| anyhow!("external workspace settlement has no live launch claim"))?;
        let active = &g.active_launch_owners;
        if claim.owner != binding.borrower_launch_owner || !active.contains(&claim.claimed_by) {
            bail!("external workspace settlement requires its exact active launch owner");
        }
        let session = g
            .runtime_db
            .dedicated_session(placement_thread_id)?
            .ok_or_else(|| anyhow!("external workspace settlement has no dedicated session"))?;
        if session.state != "freezing"
            || session.terminal_reason.as_deref() != Some("completed")
            || session.completion_fence.is_none()
        {
            bail!("external workspace settlement has no completed freeze boundary");
        }
        let allocation = g
            .runtime_db
            .external_allocation(placement_thread_id)?
            .ok_or_else(|| anyhow!("external workspace settlement lost its allocation"))?;
        if !allocation.phase.is_settled() {
            bail!("external workspace settlement retains unresolved provider contact");
        }
        let runtime = g
            .runtime_db
            .get_runtime_info(placement_thread_id)?
            .ok_or_else(|| anyhow!("external workspace settlement runtime row is absent"))?;
        if runtime.process_identity.is_some()
            || g.runtime_db
                .in_process_handler_reservation(placement_thread_id)?
                .is_some()
        {
            bail!("external workspace settlement retains a local execution owner");
        }
        g.runtime_db
            .clear_thread_workspace_owned(placement_thread_id, &binding)
    }

    /// Settle one completed process attempt before its active claim rotates or
    /// its exact attachment is compare-cleared. The caller MUST have completed
    /// the owned wait/reap and settled held-launch/request lifelines. Unlike
    /// terminal cleanup this may run for a nonterminal runtime retry, but only
    /// with the exact still-attached process identity and active launch owner.
    /// Never invoke this from an unconditional Drop or after a failed abort.
    pub fn settle_reaped_thread_workspace_owned(
        &self,
        thread_id: &str,
        binding: &runtime_db::RuntimeWorkspaceBinding,
        identity: &crate::process::ExecutionProcessIdentity,
    ) -> Result<bool> {
        self.settle_workspace_process_after_death(
            thread_id,
            binding,
            identity,
            WorkspaceProcessSettlementOwner::Live,
        )
    }

    /// Recovery's explicit exact-identity settlement after existing process
    /// authority proves group death/reap or a host-boot boundary. Neither a
    /// vanished PID nor a vanished claim constitutes that proof. The caller
    /// must retain the exact identity until this atomic mutation succeeds;
    /// unknown pre-attachment contact remains bound.
    pub fn settle_dead_thread_workspace_if_matches(
        &self,
        thread_id: &str,
        binding: &runtime_db::RuntimeWorkspaceBinding,
        identity: &crate::process::ExecutionProcessIdentity,
    ) -> Result<bool> {
        self.settle_workspace_process_after_death(
            thread_id,
            binding,
            identity,
            WorkspaceProcessSettlementOwner::Abandoned,
        )
    }

    pub(super) fn settle_workspace_process_after_death(
        &self,
        thread_id: &str,
        binding: &runtime_db::RuntimeWorkspaceBinding,
        identity: &crate::process::ExecutionProcessIdentity,
        owner: WorkspaceProcessSettlementOwner,
    ) -> Result<bool> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let active = &g.active_launch_owners;
        let claim = g.runtime_db.get_launch_claim(thread_id)?;
        let binding_owner =
            lillux::canonical_json(&serde_json::to_value(&binding.borrower_launch_owner)?)?;
        match owner {
            WorkspaceProcessSettlementOwner::Live => {
                if !claim.as_ref().is_some_and(|claim| {
                    claim.owner == binding.borrower_launch_owner
                        && active.contains(&claim.claimed_by)
                }) {
                    return Ok(false);
                }
            }
            WorkspaceProcessSettlementOwner::Abandoned => {
                if active.contains(&binding_owner)
                    || claim.as_ref().is_some_and(|claim| {
                        claim.owner != binding.borrower_launch_owner
                            || active.contains(&claim.claimed_by)
                    })
                {
                    return Ok(false);
                }
            }
        }
        if g.state_db.get_thread(thread_id)?.is_none()
            || g.runtime_db.thread_workspace_binding(thread_id)?.as_ref() != Some(binding)
        {
            return Ok(false);
        }
        let Some(runtime) = g.runtime_db.get_runtime_info(thread_id)? else {
            return Ok(false);
        };
        if runtime.process_identity.as_ref() != Some(identity)
            || g.runtime_db
                .in_process_handler_reservation(thread_id)?
                .is_some()
        {
            return Ok(false);
        }
        let settlement_kind = match owner {
            WorkspaceProcessSettlementOwner::Live if identity.process_scope.is_some() => {
                runtime_db::ThreadProcessSettlementKind::ReapedScopeEmpty
            }
            WorkspaceProcessSettlementOwner::Live => {
                runtime_db::ThreadProcessSettlementKind::ReapedGroupAbsent
            }
            WorkspaceProcessSettlementOwner::Abandoned => {
                runtime_db::ThreadProcessSettlementKind::RecoveryDeath
            }
        };
        let settlement = runtime_db::ThreadProcessSettlement::new(
            thread_id,
            &binding.borrower_launch_owner,
            identity,
            Some(binding),
            settlement_kind,
        )?;
        let cleared = g.runtime_db.clear_reaped_workspace_process_if_matches(
            thread_id,
            binding,
            identity,
            &settlement,
        )?;
        if cleared {
            self.attached_process_registry().remove(thread_id);
        }
        Ok(cleared)
    }

    #[tracing::instrument(
        name = "state:attach_thread_process",
        skip(self, launch_metadata),
        fields(thread_id = %thread_id, pid = pid, pgid = pgid)
    )]
    pub fn attach_thread_process(
        &self,
        thread_id: &str,
        pid: i64,
        pgid: i64,
        process_identity: &crate::process::ExecutionProcessIdentity,
        launch_metadata: &crate::launch_metadata::RuntimeLaunchMetadata,
        expected_launch_owner: Option<&str>,
    ) -> Result<()> {
        self.attach_thread_process_with_mode(
            thread_id,
            pid,
            pgid,
            process_identity,
            launch_metadata,
            ProcessAttachmentMode::Idempotent {
                launch_owner: expected_launch_owner,
            },
        )
    }

    pub fn reserve_execution_workspace(
        &self,
        workspace_id: &str,
        base_snapshot: &str,
        root_path: &str,
    ) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        g.runtime_db
            .reserve_workspace(workspace_id, base_snapshot, root_path)
    }

    pub fn bind_execution_workspace(
        &self,
        binding: runtime_db::WorkspaceBinding<'_>,
    ) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let _admission = Self::authorize_runtime_pin_for_thread(&g, binding.thread_id)?;
        g.runtime_db.bind_workspace(binding)
    }

    /// Attach a held workspace creator through the construction journal before
    /// releasing it. Its PID is never installed as the thread's target PID.
    pub fn attach_workspace_creator(
        &self,
        workspace_id: &str,
        thread_id: &str,
        launch_owner: &str,
        identity: &crate::process::ExecutionProcessIdentity,
    ) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        if !self
            .process_attachment_admission_open
            .load(Ordering::Acquire)
        {
            bail!("workspace creator attachment is fenced during daemon shutdown");
        }
        let _admission = Self::authorize_runtime_pin_for_thread(&g, thread_id)?;
        let thread = g
            .state_db
            .get_thread(thread_id)?
            .ok_or_else(|| anyhow!("workspace creator thread is absent"))?;
        if is_terminal_status(&thread.status) {
            bail!("terminal thread cannot release a workspace creator");
        }
        let runtime = g
            .runtime_db
            .get_runtime_info(thread_id)?
            .ok_or_else(|| anyhow!("workspace creator runtime row is absent"))?;
        if runtime.stop_intent.is_some() {
            bail!("workspace creator attachment is fenced by a durable stop");
        }
        let claim = g
            .runtime_db
            .get_launch_claim(thread_id)?
            .ok_or_else(|| anyhow!("workspace creator has no current launch claim"))?;
        if claim.claimed_by != launch_owner {
            bail!("workspace creator launch owner has changed");
        }
        g.runtime_db
            .attach_workspace_creator(workspace_id, thread_id, launch_owner, identity)
    }

    /// A successful adapter response is not process-group death evidence.
    /// Verify the exact held creator retained by this construction before a
    /// Ready bind clears its identity. Missing identity is not pre-contact proof.
    pub fn assert_execution_workspace_creator_reaped(
        &self,
        workspace_id: &str,
        thread_id: &str,
        launch_owner: &str,
    ) -> Result<()> {
        let g = self.lock()?;
        let workspace = g
            .runtime_db
            .workspace(workspace_id)?
            .ok_or_else(|| anyhow!("workspace creator journal disappeared"))?;
        let claim = g
            .runtime_db
            .get_launch_claim(thread_id)?
            .ok_or_else(|| anyhow!("workspace creator lost its current launch claim"))?;
        if workspace.state != runtime_db::WorkspaceState::Constructing
            || workspace.thread_id.as_deref() != Some(thread_id)
            || workspace.launch_owner.as_deref() != Some(launch_owner)
            || claim.claimed_by != launch_owner
            || !Self::is_launch_owner_active_locked(&g, launch_owner)
        {
            bail!("workspace creator reap check lost its exact constructing owner");
        }
        let identity = workspace
            .process_identity
            .as_deref()
            .ok_or_else(|| anyhow!("workspace construction has no exact held creator identity"))?;
        crate::process::assert_reaped_process_group_absent(&serde_json::from_str(identity)?)
    }

    pub fn claim_execution_workspace_construction(
        &self,
        workspace_id: &str,
        thread_id: &str,
        launch_owner: &str,
    ) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let _admission = Self::authorize_runtime_pin_for_thread(&g, thread_id)?;
        let claim = g
            .runtime_db
            .get_launch_claim(thread_id)?
            .ok_or_else(|| anyhow!("thread {thread_id} has no current launch owner"))?;
        if claim.claimed_by != launch_owner {
            bail!("stale launch owner for thread {thread_id}");
        }
        g.runtime_db
            .claim_workspace_construction(workspace_id, thread_id, launch_owner)
    }

    pub fn prepare_execution_workspace_backend(
        &self,
        workspace_id: &str,
        thread_id: &str,
        launch_owner: &str,
        backend_id: &str,
        backend_version: &str,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.prepare_workspace_backend(
            workspace_id,
            thread_id,
            launch_owner,
            backend_id,
            backend_version,
        )
    }

    pub fn transition_execution_workspace(
        &self,
        workspace_id: &str,
        expected: &[runtime_db::WorkspaceState],
        next: runtime_db::WorkspaceState,
        process_identity: Option<&str>,
    ) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        g.runtime_db
            .transition_workspace(workspace_id, expected, next, process_identity)?;
        drop(g);
        drop(_permit);
        if next == runtime_db::WorkspaceState::Closed {
            self.note_closed_worker_scope_retirement(workspace_id);
        }
        Ok(())
    }

    pub fn transition_execution_workspace_owned(
        &self,
        workspace_id: &str,
        thread_id: &str,
        launch_owner: &str,
        expected: &[runtime_db::WorkspaceState],
        next: runtime_db::WorkspaceState,
        process_identity: Option<&str>,
    ) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let _admission = Self::authorize_runtime_pin_for_thread(&g, thread_id)?;
        let claim = g
            .runtime_db
            .get_launch_claim(thread_id)?
            .ok_or_else(|| anyhow!("thread {thread_id} has no current launch owner"))?;
        if claim.claimed_by != launch_owner {
            bail!("stale launch owner for thread {thread_id}");
        }
        g.runtime_db.transition_workspace_owned(
            workspace_id,
            thread_id,
            launch_owner,
            expected,
            next,
            process_identity,
        )?;
        drop(_admission);
        drop(g);
        drop(_permit);
        if next == runtime_db::WorkspaceState::Closed {
            self.note_closed_worker_scope_retirement(workspace_id);
        }
        Ok(())
    }

    pub fn rebind_execution_workspace_for_recovery(
        &self,
        workspace_id: &str,
        thread_id: &str,
        previous_launch_owner: &str,
        recovery_launch_owner: &str,
        expected_state: runtime_db::WorkspaceState,
        expected_process_identity: Option<&str>,
    ) -> Result<()> {
        self.handoff_execution_workspace_owner(
            workspace_id,
            thread_id,
            previous_launch_owner,
            recovery_launch_owner,
            expected_state,
            expected_process_identity,
            None,
        )
    }

    /// Same-daemon retry retains the original live view slot. The caller must
    /// prove that exact slot is still held and all request/process aliases are
    /// settled; neither an inactive claim nor a filesystem path is that proof.
    pub fn handoff_execution_workspace_for_live_retry(
        &self,
        workspace_id: &str,
        thread_id: &str,
        previous_launch_owner: &str,
        recovery_launch_owner: &str,
        expected_state: runtime_db::WorkspaceState,
        expected_process_identity: Option<&str>,
        view_identity: &str,
    ) -> Result<()> {
        self.handoff_execution_workspace_owner(
            workspace_id,
            thread_id,
            previous_launch_owner,
            recovery_launch_owner,
            expected_state,
            expected_process_identity,
            Some(view_identity),
        )
    }

    pub(super) fn handoff_execution_workspace_owner(
        &self,
        workspace_id: &str,
        thread_id: &str,
        previous_launch_owner: &str,
        recovery_launch_owner: &str,
        expected_state: runtime_db::WorkspaceState,
        expected_process_identity: Option<&str>,
        retained_live_view: Option<&str>,
    ) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let _admission = Self::authorize_runtime_pin_for_thread(&g, thread_id)?;
        let claim = g
            .runtime_db
            .get_launch_claim(thread_id)?
            .ok_or_else(|| anyhow!("thread {thread_id} has no recovery launch owner"))?;
        if claim.claimed_by != recovery_launch_owner {
            bail!("stale recovery launch owner for thread {thread_id}");
        }
        if !Self::is_launch_owner_active_locked(&g, recovery_launch_owner)
            || Self::is_launch_owner_active_locked(&g, previous_launch_owner)
        {
            bail!("workspace owner transfer requires an active successor and settled predecessor");
        }
        let previous: runtime_db::LaunchOwner = serde_json::from_str(previous_launch_owner)?;
        let recovery: runtime_db::LaunchOwner = serde_json::from_str(recovery_launch_owner)?;
        if previous.thread_id != thread_id
            || recovery.thread_id != thread_id
            || recovery.daemon_generation_id != runtime_db::daemon_generation_id()
            || (previous.daemon_generation_id == recovery.daemon_generation_id)
                != retained_live_view.is_some()
        {
            bail!("workspace transfer mode does not match its exact daemon-generation owners");
        }
        if let Some(raw_identity) = expected_process_identity {
            let identity: crate::process::ExecutionProcessIdentity =
                serde_json::from_str(raw_identity)?;
            crate::process::assert_reaped_process_group_absent(&identity)?;
        }
        match retained_live_view {
            Some(view_identity) => g.runtime_db.handoff_workspace_for_live_retry(
                workspace_id,
                thread_id,
                previous_launch_owner,
                recovery_launch_owner,
                expected_state,
                expected_process_identity,
                view_identity,
            ),
            None => g.runtime_db.rebind_workspace_for_recovery(
                workspace_id,
                thread_id,
                previous_launch_owner,
                recovery_launch_owner,
                expected_state,
                expected_process_identity,
            ),
        }
    }

    /// Transition an exact dead owner's workspace during reconciliation. A
    /// replacement launch may exist for the same thread, so this fence is the
    /// immutable workspace owner tuple rather than the thread's current claim.
    pub fn transition_abandoned_execution_workspace_owned(
        &self,
        workspace_id: &str,
        thread_id: &str,
        launch_owner: &str,
        expected: &[runtime_db::WorkspaceState],
        next: runtime_db::WorkspaceState,
    ) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let workspace = g
            .runtime_db
            .workspace(workspace_id)?
            .ok_or_else(|| anyhow!("workspace {workspace_id} disappeared"))?;
        if workspace.thread_id.as_deref() != Some(thread_id)
            || workspace.launch_owner.as_deref() != Some(launch_owner)
        {
            bail!("abandoned workspace owner tuple changed during reconciliation");
        }
        if g.runtime_db
            .get_launch_claim(thread_id)?
            .is_some_and(|claim| {
                claim.claimed_by == launch_owner
                    && Self::is_launch_owner_active_locked(&g, &claim.claimed_by)
            })
        {
            bail!("workspace launch owner is still active in this daemon");
        }
        g.runtime_db.transition_workspace_owned(
            workspace_id,
            thread_id,
            launch_owner,
            expected,
            next,
            None,
        )?;
        drop(g);
        drop(_permit);
        if next == runtime_db::WorkspaceState::Closed {
            self.note_closed_worker_scope_retirement(workspace_id);
        }
        Ok(())
    }

    /// Publish a callback-frozen generation under the same StateStore lock as
    /// launch-owner fencing. RuntimeDb binds the snapshot/output-capture pair
    /// in one immediate transaction without rewriting ResumeContext.
    pub fn bind_frozen_execution_workspace(
        &self,
        workspace_id: &str,
        thread_id: &str,
        launch_owner: &str,
        generation: &ryeos_state::objects::WorkspaceGenerationPair,
    ) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let _admission = Self::authorize_runtime_pin_for_thread(&g, thread_id)?;
        g.runtime_db.bind_frozen_workspace_generation(
            workspace_id,
            thread_id,
            launch_owner,
            generation,
        )
    }

    /// Finish a callback freeze during startup after the dead generation's
    /// launch claim has been cleared. This does not relax the live bind path:
    /// the old owner must be inactive, no replacement claim may exist, and the
    /// runtime database additionally requires an exact dead-generation owner
    /// and workspace tuple.
    pub fn bind_abandoned_frozen_execution_workspace(
        &self,
        workspace_id: &str,
        thread_id: &str,
        launch_owner: &str,
        generation: &ryeos_state::objects::WorkspaceGenerationPair,
    ) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let _admission = Self::authorize_runtime_pin_for_thread(&g, thread_id)?;
        if Self::is_launch_owner_active_locked(&g, launch_owner) {
            bail!("active launch owner cannot use abandoned freeze recovery");
        }
        if g.runtime_db.get_launch_claim(thread_id)?.is_some() {
            bail!("abandoned freeze recovery refuses a replacement launch claim");
        }
        g.runtime_db.bind_abandoned_frozen_workspace_generation(
            workspace_id,
            thread_id,
            launch_owner,
            generation,
        )
    }

    pub fn open_execution_workspaces(&self) -> Result<Vec<runtime_db::WorkspaceRecord>> {
        let g = self.lock()?;
        g.runtime_db.open_workspaces()
    }

    pub fn execution_workspace(
        &self,
        workspace_id: &str,
    ) -> Result<Option<runtime_db::WorkspaceRecord>> {
        let g = self.lock()?;
        g.runtime_db.workspace(workspace_id)
    }

    pub fn execution_workspace_for_thread(
        &self,
        thread_id: &str,
    ) -> Result<Option<runtime_db::WorkspaceRecord>> {
        let g = self.lock()?;
        g.runtime_db.workspace_for_thread(thread_id)
    }

    // ── Hook dispatch ledger ─────────────────────────────────────────────

    pub fn reserve_hook_dispatch(
        &self,
        seed: &runtime_db::NewHookDispatch,
    ) -> Result<runtime_db::HookDispatchReservation> {
        let g = self.lock()?;
        g.runtime_db.reserve_hook_dispatch(seed)
    }
}
