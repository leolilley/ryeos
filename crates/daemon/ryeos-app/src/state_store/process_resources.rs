//! Process resource owners, scratch, thread scope reservations and worker processes.
use super::*;

/// Minted only after actual exact scratch removal/absence by this StateStore.
/// No Clone, serde constructor, public observations or worker input bridge.
pub(crate) struct ResourceScratchPhysicalReceipt {
    pub(super) expected: runtime_db::ProcessResourceReservationRecord,
}

impl ResourceScratchPhysicalReceipt {
    pub(crate) fn into_expected(self) -> runtime_db::ProcessResourceReservationRecord {
        self.expected
    }
}

/// Authoritative original no-birth + physical removal join. No Clone/serde,
/// decoded-record constructor, process proof or driver retirement authority.
pub(crate) struct NeverBornResourceFundingReceipt {
    pub(super) state_store: Arc<StateStore>,
    pub(super) removed: runtime_db::ProcessResourceReservationRecord,
}

impl NeverBornResourceFundingReceipt {
    pub(crate) fn matches_original(
        self,
        state: &Arc<StateStore>,
        original: &runtime_db::ProcessResourceReservationRecord,
    ) -> Result<()> {
        self.matches_original_of(&Arc::downgrade(state), original)
    }

    /// [`Self::matches_original`] against the identity of a Store owner:
    /// every owner but the Store that minted the receipt is refused.
    pub(super) fn matches_original_of(
        self,
        state: &std::sync::Weak<StateStore>,
        original: &runtime_db::ProcessResourceReservationRecord,
    ) -> Result<()> {
        if !std::ptr::eq(Arc::as_ptr(&self.state_store), state.as_ptr()) {
            bail!("no-birth funding receipt belongs to another StateStore owner");
        }
        let custody =
            runtime_db::process_resource_custody::never_issued_scratch_custody(&self.removed)?;
        if custody.physical_state
            != runtime_db::process_resource_custody::ProcessCustodyPhysicalState::PhysicallyRemoved
        {
            bail!("no-birth funding receipt lacks actual original physical removal");
        }
        let mut expected = original.clone();
        expected
            .runtime_custody
            .as_mut()
            .context("original funding lacks physical custody")?
            .physical_state =
            runtime_db::process_resource_custody::ProcessCustodyPhysicalState::PhysicallyRemoved;
        if self.removed != expected {
            bail!("no-birth funding receipt differs from exact original reservation");
        }
        Ok(())
    }
}

impl StateStore {
    /// Wrap the existing winning never-issued Reserved/CleanupIntent physical
    /// owner, rather than accepting a raw receipt JSON or PID absence. The
    /// existing physical removal path's finite acquisition/SQL/traversal join
    /// is still required before enabling the trusted executable route.
    pub(crate) fn remove_never_born_scratch_for_funding(
        self: &Arc<Self>,
        original: &runtime_db::ProcessResourceReservationRecord,
        scratch: &crate::temp_dir_guard::TempDirGuard,
    ) -> Result<NeverBornResourceFundingReceipt> {
        let removed = self.remove_never_issued_resource_scratch(original, scratch)?;
        Ok(NeverBornResourceFundingReceipt {
            state_store: Arc::clone(self),
            removed,
        })
    }
}

impl StateStore {
    pub fn attach_worker_process(&self, record: &WorkerProcessRecord) -> Result<()> {
        let g = self.lock()?;
        if !self
            .process_attachment_admission_open
            .load(Ordering::Acquire)
        {
            anyhow::bail!("worker process attachment admission is closed for daemon shutdown");
        }
        let session = g
            .runtime_db
            .dedicated_session(&record.placement_thread_id)?
            .ok_or_else(|| anyhow!("worker attachment has no admitted dedicated session"))?;
        self.authorize_dedicated_workspace_contact_locked(
            &g,
            &record.placement_thread_id,
            &session.workspace_id,
        )?;
        g.runtime_db.attach_worker_process(record)?;
        self.live_worker_registry()
            .insert(record.worker_instance_id.clone(), record.clone());
        Ok(())
    }

    /// Same pooled attachment under the actual original launch window.
    pub fn attach_pooled_resource_owner_until(
        &self,
        owner_coordinate: &str,
        process_identity: &crate::process::ExecutionProcessIdentity,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<()> {
        self.with_runtime_original_deadline(deadline, |db| {
            db.attach_pooled_resource_owner(owner_coordinate, process_identity)
        })
    }

    pub fn attach_pooled_resource_owner(
        &self,
        owner_coordinate: &str,
        process_identity: &crate::process::ExecutionProcessIdentity,
    ) -> Result<()> {
        let g = self.lock()?;
        if !self
            .process_attachment_admission_open
            .load(Ordering::Acquire)
        {
            bail!("pooled resource-owner attachment admission is closed for daemon shutdown");
        }
        g.runtime_db
            .attach_pooled_resource_owner(owner_coordinate, process_identity)
    }

    /// Linearization point between a durable pooled-owner attachment and held
    /// process release. Shutdown closes the same gate under this lock; only the
    /// exact current process incarnation may consume its one-shot fence.
    /// Consume the SAME release fence under the original launch window.
    /// Existing native/scope qualification checks are not bypassed.
    pub fn authorize_pooled_resource_owner_release_until(
        &self,
        owner_coordinate: &str,
        process_identity: &crate::process::ExecutionProcessIdentity,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<()> {
        self.with_runtime_original_deadline(deadline, |db| {
            db.consume_pooled_resource_owner_release_fence(owner_coordinate, process_identity)
        })
    }

    pub fn authorize_pooled_resource_owner_release(
        &self,
        owner_coordinate: &str,
        process_identity: &crate::process::ExecutionProcessIdentity,
    ) -> Result<()> {
        let g = self.lock()?;
        if !self
            .process_attachment_admission_open
            .load(Ordering::Acquire)
        {
            bail!("pooled resource-owner release is fenced during daemon shutdown");
        }
        g.runtime_db
            .consume_pooled_resource_owner_release_fence(owner_coordinate, process_identity)
    }

    /// Dedicated-worker counterpart to pooled release authorization. The
    /// worker journal must still name the exact attached occurrence before its
    /// resource-owner fence can be consumed.
    pub fn authorize_dedicated_resource_owner_release(
        &self,
        worker_instance_id: &str,
        process_identity: &crate::process::ExecutionProcessIdentity,
    ) -> Result<()> {
        let g = self.lock()?;
        if !self
            .process_attachment_admission_open
            .load(Ordering::Acquire)
        {
            bail!("dedicated resource-owner release is fenced during daemon shutdown");
        }
        let worker = g
            .runtime_db
            .worker_process(worker_instance_id)?
            .ok_or_else(|| anyhow!("dedicated resource-owner release has no worker journal"))?;
        if worker.state != runtime_db::WorkerProcessState::Attached
            || worker.cleanup_state != "owned"
            || worker.process_identity != *process_identity
        {
            bail!("dedicated resource-owner release contradicts its attached worker journal");
        }
        g.runtime_db
            .consume_dedicated_resource_owner_release_fence(worker_instance_id, process_identity)
    }

    pub fn clear_pooled_resource_owner(
        &self,
        owner_coordinate: &str,
        process_identity: &crate::process::ExecutionProcessIdentity,
    ) -> Result<()> {
        self.lock()?
            .runtime_db
            .clear_pooled_resource_owner(owner_coordinate, process_identity)
    }

    pub fn prove_pooled_resource_owner_cleanup(
        &self,
        owner_coordinate: &str,
        process_identity: &crate::process::ExecutionProcessIdentity,
        evidence: &runtime_db::ProcessResourceCleanupEvidence,
    ) -> Result<runtime_db::ProcessResourceCleanupEvidence> {
        self.lock()?.runtime_db.prove_pooled_resource_owner_cleanup(
            owner_coordinate,
            process_identity,
            evidence,
        )
    }

    pub fn prove_thread_resource_owner_cleanup(
        &self,
        thread_id: &str,
        process_identity: &crate::process::ExecutionProcessIdentity,
        evidence: &runtime_db::ProcessResourceCleanupEvidence,
    ) -> Result<runtime_db::ProcessResourceCleanupEvidence> {
        self.lock()?.runtime_db.prove_thread_resource_owner_cleanup(
            thread_id,
            process_identity,
            evidence,
        )
    }

    pub fn prove_dedicated_resource_owner_cleanup(
        &self,
        worker_instance_id: &str,
        process_identity: &crate::process::ExecutionProcessIdentity,
        evidence: &runtime_db::ProcessResourceCleanupEvidence,
    ) -> Result<runtime_db::ProcessResourceCleanupEvidence> {
        self.lock()?
            .runtime_db
            .prove_dedicated_resource_owner_cleanup(worker_instance_id, process_identity, evidence)
    }

    pub fn process_resource_owners(&self) -> Result<Vec<runtime_db::ProcessResourceOwnerRecord>> {
        self.lock()?.runtime_db.process_resource_owners()
    }

    /// Deletion must hold this pin critical section THROUGH unlink, after
    /// acquiring the exact construction token and exclusive generation lease.
    /// Do not reenter StateStore or acquire a CAS lock in the callback. A pin
    /// snapshot outside this guard is not deletion authority. This API alone
    /// does not wire any cache deletion owner to the retained custody contract.
    pub fn with_process_resource_materialization_pins<T>(
        &self,
        operation: impl FnOnce(
            &std::collections::BTreeSet<
                runtime_db::process_resource_custody::ProcessMaterializationCustody,
            >,
        ) -> Result<T>,
    ) -> Result<T> {
        let guard = self.lock()?;
        let pins = guard.runtime_db.process_resource_materialization_pins()?;
        operation(&pins)
    }

    pub fn reserve_process_resource_launch(
        &self,
        reservation: &runtime_db::ProcessResourceReservationRecord,
    ) -> Result<()> {
        if reservation
            .runtime_custody
            .as_ref()
            .is_some_and(|custody| custody.workspace.is_resource_owned_scratch())
        {
            bail!("resource-owned scratch reservation requires its ORIGINAL guard");
        }
        let g = self.lock()?;
        if !self
            .process_attachment_admission_open
            .load(Ordering::Acquire)
        {
            bail!("process resource reservation is closed for daemon shutdown");
        }
        g.runtime_db.reserve_process_resource_launch(reservation)
    }

    /// Preserve the ORIGINAL guard BEFORE the first possible reservation
    /// commit. Every error, including a lost commit ACK, leaves it preserved;
    /// this method neither blindly retries insertion nor removes the scratch.
    pub fn reserve_process_resource_launch_with_scratch(
        &self,
        reservation: &runtime_db::ProcessResourceReservationRecord,
        original: &crate::temp_dir_guard::TempDirGuard,
    ) -> Result<()> {
        if self.read_only {
            bail!("read-only StateStore cannot own scratch reservation");
        }
        let custody =
            runtime_db::process_resource_custody::never_issued_scratch_custody(reservation)?;
        custody.require_retained()?;
        let parent = self.resource_scratch_parent(custody)?;
        self.require_original_resource_scratch(custody, &parent, original)?;
        original.preserve_for_explicit_cleanup();
        let g = self.lock()?;
        if !self
            .process_attachment_admission_open
            .load(Ordering::Acquire)
        {
            bail!("scratch reservation is closed for daemon shutdown; original guard retained");
        }
        g.runtime_db.reserve_process_resource_launch(reservation)
    }

    /// SAME original scratch reservation under the original connection window.
    /// Physical guard validation is still the existing separate unbounded seam.
    pub(crate) fn reserve_process_resource_launch_with_scratch_until(
        &self,
        reservation: &runtime_db::ProcessResourceReservationRecord,
        original: &crate::temp_dir_guard::TempDirGuard,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<()> {
        if self.read_only || deadline.has_elapsed() {
            bail!("original scratch reservation unavailable or expired");
        }
        let custody =
            runtime_db::process_resource_custody::never_issued_scratch_custody(reservation)?;
        custody.require_retained()?;
        let parent = self.resource_scratch_parent(custody)?;
        self.require_original_resource_scratch(custody, &parent, original)?;
        original.preserve_for_explicit_cleanup();
        self.with_runtime_original_deadline(deadline, |db| {
            db.reserve_process_resource_launch(reservation)
        })
    }

    /// Only a stored NEVER-ISSUED reservation can use this finite first cut.
    /// The receipt records physical disposal; pins/capacity remain retained.
    pub fn remove_never_issued_resource_scratch(
        &self,
        expected: &runtime_db::ProcessResourceReservationRecord,
        original: &crate::temp_dir_guard::TempDirGuard,
    ) -> Result<runtime_db::ProcessResourceReservationRecord> {
        self.remove_or_reconcile_never_issued_resource_scratch(expected, Some(original))
    }

    /// Recovery keeps ORIGINAL birth coordinates and permits a new daemon
    /// incarnation. It never rewrites that generation or creates a new owner.
    pub fn recover_never_issued_resource_scratch(
        &self,
        expected: &runtime_db::ProcessResourceReservationRecord,
    ) -> Result<runtime_db::ProcessResourceReservationRecord> {
        self.remove_or_reconcile_never_issued_resource_scratch(expected, None)
    }

    pub(super) fn remove_or_reconcile_never_issued_resource_scratch(
        &self,
        expected: &runtime_db::ProcessResourceReservationRecord,
        original: Option<&crate::temp_dir_guard::TempDirGuard>,
    ) -> Result<runtime_db::ProcessResourceReservationRecord> {
        use runtime_db::process_resource_custody::{
            ProcessCustodyPhysicalState, ProcessWorkspaceCustody, never_issued_scratch_custody,
        };
        if self.read_only {
            bail!("read-only StateStore cannot remove resource scratch");
        }
        let custody = never_issued_scratch_custody(expected)?;
        let parent = self.resource_scratch_parent(custody)?;
        let ProcessWorkspaceCustody::ResourceOwnedScratch {
            scratch_name,
            workspace_identity,
            ..
        } = &custody.workspace
        else {
            unreachable!("validated scratch")
        };
        let name = std::ffi::OsStr::new(scratch_name);
        // Validate ORIGINAL armed authority BEFORE any transition or removal.
        if let Some(original) = original {
            self.require_original_resource_scratch(custody, &parent, original)?;
        }
        let g = self.lock()?;
        let stored = g
            .runtime_db
            .process_resource_reservation(&expected.owner_kind, &expected.owner_coordinate)?
            .ok_or_else(|| anyhow!("scratch reservation absent; outcome must be reconciled"))?;
        if &stored != expected {
            bail!("scratch recovery lost its exact birth reservation");
        }
        if custody.physical_state == ProcessCustodyPhysicalState::PhysicallyRemoved {
            if parent.open_entry(name, false)?.is_some() {
                bail!("recorded removed scratch has a replacement entry; refuse further removal");
            }
            parent.ensure_path_binding()?;
            return Ok(stored);
        }
        if custody.physical_state == ProcessCustodyPhysicalState::Retained {
            match parent.open_entry(name, false)? {
                Some(lillux::PinnedDirectoryEntry::Directory(directory))
                    if directory.identity()? == *workspace_identity => {}
                _ => bail!("scratch absence/replacement has no PRE-EXISTING cleanup intent"),
            }
        }
        let intent = g.runtime_db.claim_never_issued_scratch_cleanup(expected)?;
        match parent.open_entry(name, false)? {
            Some(lillux::PinnedDirectoryEntry::Directory(directory)) => {
                directory.ensure_path_binding()?;
                if directory.identity()? != *workspace_identity {
                    bail!("scratch root incarnation changed; cleanup intent remains pinned");
                }
                if let Some(original) = original {
                    original.remove_now()?;
                } else {
                    directory.remove_contents_recursive()?;
                    if !parent
                        .remove_empty_child_if_same(name, &directory)
                        .map(|receipt| receipt.is_some())?
                    {
                        bail!("recovered scratch remained nonempty");
                    }
                }
            }
            Some(_) => bail!("scratch cleanup entry is no longer a directory"),
            None => {
                // Exact absence is accepted only AFTER retained cleanup intent
                // under this original namespace, never from an Ok no-op guard.
                if original.is_some() {
                    bail!("original armed scratch disappeared before removal");
                }
            }
        }
        parent.ensure_path_binding()?;
        if parent.open_entry(name, false)?.is_some() {
            bail!("scratch physical removal did not produce exact parent absence");
        }
        let receipt = ResourceScratchPhysicalReceipt { expected: intent };
        g.runtime_db.record_never_issued_scratch_removal(receipt)
    }

    pub(super) fn resource_scratch_parent(
        &self,
        custody: &runtime_db::process_resource_custody::ProcessResourceRuntimeCustody,
    ) -> Result<lillux::PinnedDirectory> {
        use runtime_db::process_resource_custody::ProcessWorkspaceCustody;
        let ProcessWorkspaceCustody::ResourceOwnedScratch {
            runtime_directory_identity,
            parent_directory_identity,
            ..
        } = &custody.workspace
        else {
            bail!("physical scratch owner is absent")
        };
        let runtime = self.state_authority.runtime_directory();
        runtime.ensure_path_binding()?;
        if runtime.identity()? != *runtime_directory_identity {
            bail!("scratch runtime namespace differs from ORIGINAL owner");
        }
        let cache = runtime
            .open_child_directory(std::ffi::OsStr::new("cache"))?
            .ok_or_else(|| anyhow!("scratch runtime cache is missing"))?;
        let parent = cache
            .open_child_directory(std::ffi::OsStr::new("executions"))?
            .ok_or_else(|| anyhow!("scratch original parent is missing"))?;
        parent.ensure_path_binding()?;
        if parent.identity()? != *parent_directory_identity {
            bail!("scratch parent incarnation differs from ORIGINAL owner");
        }
        Ok(parent)
    }

    pub(super) fn require_original_resource_scratch(
        &self,
        custody: &runtime_db::process_resource_custody::ProcessResourceRuntimeCustody,
        expected_parent: &lillux::PinnedDirectory,
        original: &crate::temp_dir_guard::TempDirGuard,
    ) -> Result<()> {
        use runtime_db::process_resource_custody::ProcessWorkspaceCustody;
        let ProcessWorkspaceCustody::ResourceOwnedScratch {
            scratch_name,
            workspace_identity,
            ..
        } = &custody.workspace
        else {
            bail!("original scratch owner is absent")
        };
        let (parent, name) = original.owned_scratch_parent()?;
        let root = original.owned_scratch_root()?;
        parent.ensure_path_binding()?;
        root.ensure_path_binding()?;
        if original.path().is_none()
            || !original.owns_effective_path(root.path())
            || name != std::ffi::OsStr::new(scratch_name)
            || parent.identity()? != expected_parent.identity()?
            || root.identity()? != *workspace_identity
        {
            bail!("scratch guard is disarmed or differs from its ORIGINAL physical owner");
        }
        let bound = expected_parent
            .open_child_directory(name)?
            .ok_or_else(|| anyhow!("original scratch owner is absent under its parent"))?;
        if bound.identity()? != *workspace_identity {
            bail!("original scratch parent binding changed");
        }
        Ok(())
    }

    pub fn begin_trusted_process_resource_spawn(
        &self,
        expected: &runtime_db::ProcessResourceReservationRecord,
    ) -> Result<runtime_db::process_resource_launch::TrustedResourceSpawnPermit> {
        let g = self.lock()?;
        if !self
            .process_attachment_admission_open
            .load(Ordering::Acquire)
        {
            bail!("trusted resource spawn intent is closed for daemon shutdown");
        }
        g.runtime_db.begin_trusted_process_resource_spawn(expected)
    }

    pub(crate) fn begin_trusted_process_resource_spawn_until(
        &self,
        expected: &runtime_db::ProcessResourceReservationRecord,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<runtime_db::process_resource_launch::TrustedResourceSpawnPermit> {
        self.with_runtime_original_deadline(deadline, |db| {
            db.begin_trusted_process_resource_spawn(expected)
        })
    }

    pub fn reserve_thread_process_scope(
        &self,
        reservation: &runtime_db::ThreadProcessScopeReservationRecord,
    ) -> Result<()> {
        let g = self.lock()?;
        if !self
            .process_attachment_admission_open
            .load(Ordering::Acquire)
        {
            bail!("thread scope reservation is closed for daemon shutdown");
        }
        g.runtime_db.reserve_thread_process_scope(reservation)
    }

    pub fn bind_thread_process_scope(
        &self,
        thread_id: &str,
        launch_owner: &str,
        recovery: &lillux::ProcessScopeRecovery,
    ) -> Result<()> {
        let g = self.lock()?;
        if !self
            .process_attachment_admission_open
            .load(Ordering::Acquire)
        {
            bail!("thread scope binding is closed for daemon shutdown");
        }
        g.runtime_db
            .bind_thread_process_scope(thread_id, launch_owner, recovery)
    }

    pub fn thread_process_scope_reservation(
        &self,
        thread_id: &str,
    ) -> Result<Option<runtime_db::ThreadProcessScopeReservationRecord>> {
        self.lock()?
            .runtime_db
            .thread_process_scope_reservation(thread_id)
    }

    pub fn thread_process_scope_reservations(
        &self,
    ) -> Result<Vec<runtime_db::ThreadProcessScopeReservationRecord>> {
        self.lock()?.runtime_db.thread_process_scope_reservations()
    }

    pub fn fence_thread_scope_recovery(
        &self,
        reservation: &runtime_db::ThreadProcessScopeReservationRecord,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db
            .fence_thread_process_scope_recovery(reservation)
    }

    pub fn clear_thread_process_scope_reservation(
        &self,
        reservation: &runtime_db::ThreadProcessScopeReservationRecord,
    ) -> Result<()> {
        self.lock()?
            .runtime_db
            .clear_thread_process_scope_reservation(reservation)
    }

    pub fn bind_process_resource_scope(
        &self,
        owner_kind: &str,
        owner_coordinate: &str,
        recovery: &lillux::ProcessScopeRecovery,
    ) -> Result<()> {
        let g = self.lock()?;
        if !self
            .process_attachment_admission_open
            .load(Ordering::Acquire)
        {
            bail!("process resource scope binding is closed for daemon shutdown");
        }
        g.runtime_db
            .bind_process_resource_scope(owner_kind, owner_coordinate, recovery)
    }

    pub fn process_resource_reservations(
        &self,
    ) -> Result<Vec<runtime_db::ProcessResourceReservationRecord>> {
        self.lock()?.runtime_db.process_resource_reservations()
    }

    pub fn process_resource_reservation(
        &self,
        owner_kind: &str,
        owner_coordinate: &str,
    ) -> Result<Option<runtime_db::ProcessResourceReservationRecord>> {
        self.lock()?
            .runtime_db
            .process_resource_reservation(owner_kind, owner_coordinate)
    }

    pub fn clear_process_resource_reservation(
        &self,
        reservation: &runtime_db::ProcessResourceReservationRecord,
    ) -> Result<()> {
        self.lock()?
            .runtime_db
            .clear_process_resource_reservation(reservation)
    }

    /// Existing durable session/worker owners supplement thread membership:
    /// failed held launches can retain unknown worker contact without writing
    /// execution_workspace.process_identity. Caller holds the root operation
    /// barrier and must separately compare the process-local pool owner. This
    /// proves process identity, not workspace phase admission: capture/close
    /// own their phase transition and may already have made its no-contact cut.
    pub fn workspace_worker_capture_record(
        &self,
        expected: &runtime_db::WorkspaceRecord,
    ) -> Result<Option<WorkerProcessRecord>> {
        let g = self.lock()?;
        let current = g
            .runtime_db
            .workspace(&expected.workspace_id)?
            .ok_or_else(|| anyhow!("capture workspace journal disappeared"))?;
        if current.thread_id != expected.thread_id
            || current.launch_owner != expected.launch_owner
            || current.mount_identity != expected.mount_identity
            || current.process_identity != expected.process_identity
        {
            bail!("workspace worker capture coordinate changed");
        }
        let root = current
            .thread_id
            .as_deref()
            .ok_or_else(|| anyhow!("capture workspace has no root"))?;
        let session = g.runtime_db.dedicated_session(root)?;
        let worker_id = session
            .as_ref()
            .and_then(|session| session.worker_instance_id.as_deref());
        if g.runtime_db
            .placement_has_unsettled_worker_except(root, worker_id)?
        {
            bail!("workspace capture retains another unsettled worker boot");
        }
        let Some(session) = session else {
            return Ok(None);
        };
        if session.workspace_id != current.workspace_id {
            bail!("dedicated session does not name the capture workspace");
        }
        let Some(worker_id) = session.worker_instance_id.as_deref() else {
            let retired_for_recovery = matches!(
                (session.state.as_str(), session.send_boundary.as_str()),
                ("recovering", "none") | ("outcome_unknown", "outcome_unknown")
            ) && g
                .runtime_db
                .placement_has_reaped_worker_history(root, &session.admitted_capsule_hash)?;
            let failed_start_settled =
                session.state == "terminal" && session.send_boundary != "outcome_unknown";
            if session.worker_boot_epoch.is_some()
                || (!retired_for_recovery && !failed_start_settled)
            {
                bail!("workspace capture has pending or identity-unknown dedicated worker contact");
            }
            // A terminal failed start with no worker row is explicit durable
            // cleanup testimony. Recovery instead requires indexed old-boot
            // retirement. Admission reserves each newer ID/epoch before contact,
            // so an old reaped boot cannot conceal a newer pending attempt.
            // A contacted command can remain outcome-unknown after this exact
            // process is proved reaped. Capture consumes only cleanup testimony;
            // it must not settle or replay that command to obtain quiescence.
            return Ok(None);
        };
        let worker = g
            .runtime_db
            .worker_process(worker_id)?
            .ok_or_else(|| anyhow!("capture dedicated worker record is absent"))?;
        if worker.placement_thread_id != root
            || Some(worker.boot_epoch) != session.worker_boot_epoch
            || worker.session_capsule_hash != session.admitted_capsule_hash
            || !matches!(
                (worker.state, worker.cleanup_state.as_str()),
                (runtime_db::WorkerProcessState::Live, "owned")
                    | (runtime_db::WorkerProcessState::Dead, "reaped")
            )
        {
            bail!("workspace capture worker identity is pending or cleanup is unproved");
        }
        Ok(Some(worker))
    }

    pub(super) fn authorize_dedicated_workspace_contact_locked(
        &self,
        g: &Inner,
        thread_id: &str,
        workspace_id: &str,
    ) -> Result<()> {
        if !self
            .process_attachment_admission_open
            .load(Ordering::Acquire)
        {
            bail!("dedicated workspace contact is closed for daemon shutdown");
        }
        let binding = g
            .runtime_db
            .thread_workspace_binding(thread_id)?
            .ok_or_else(|| anyhow!("dedicated workspace contact has no retained root binding"))?;
        if binding.workspace_id != workspace_id {
            bail!("dedicated session does not name the admitted current workspace");
        }
        let workspace = g
            .runtime_db
            .workspace(workspace_id)?
            .ok_or_else(|| anyhow!("dedicated session workspace is absent"))?;
        if workspace.thread_id.as_deref() != Some(thread_id) {
            bail!("dedicated workspace contact requires its exact root owner");
        }
        let claim = g
            .runtime_db
            .get_launch_claim(thread_id)?
            .ok_or_else(|| anyhow!("dedicated workspace contact has no launch owner"))?;
        if !Self::is_launch_owner_active_locked(g, &claim.claimed_by) {
            bail!("dedicated workspace contact requires its active launch owner");
        }
        Self::authorize_thread_workspace_contact_locked(g, thread_id)
    }

    pub fn fence_unproved_worker_start(
        &self,
        record: &WorkerProcessRecord,
        reason: &str,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.fence_unproved_worker_start(record, reason)
    }

    pub fn worker_process(&self, worker_instance_id: &str) -> Result<Option<WorkerProcessRecord>> {
        let g = self.lock()?;
        g.runtime_db.worker_process(worker_instance_id)
    }

    pub fn live_worker_processes(&self) -> Result<Vec<WorkerProcessRecord>> {
        let g = self.lock()?;
        g.runtime_db.live_worker_processes()
    }

    /// Indexed cleanup guard, including orphaned boots no longer in the
    /// session's attachment slot. Absence of that slot alone proves no cleanup.
    pub fn placement_has_unsettled_worker(&self, placement_thread_id: &str) -> Result<bool> {
        self.lock()?
            .runtime_db
            .placement_has_unsettled_worker_except(placement_thread_id, None)
    }

    pub fn fence_abandoned_worker_process(
        &self,
        worker_instance_id: &str,
        placement_thread_id: &str,
        boot_epoch: u64,
        cleanup_state: &str,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.fence_abandoned_worker_process(
            worker_instance_id,
            placement_thread_id,
            boot_epoch,
            cleanup_state,
        )?;
        self.forget_live_worker(worker_instance_id, boot_epoch);
        Ok(())
    }

    /// Drop the in-memory live record of one exact worker epoch after its
    /// durable settle or fence committed.
    pub(super) fn forget_live_worker(&self, worker_instance_id: &str, boot_epoch: u64) {
        let mut workers = self.live_worker_registry();
        if workers
            .get(worker_instance_id)
            .is_some_and(|record| record.boot_epoch == boot_epoch)
        {
            workers.remove(worker_instance_id);
        }
    }

    pub fn complete_worker_binding(
        &self,
        worker_instance_id: &str,
        placement_thread_id: &str,
        boot_epoch: u64,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db
            .complete_worker_binding(worker_instance_id, placement_thread_id, boot_epoch)
    }

    pub fn settle_worker_process(
        &self,
        worker_instance_id: &str,
        placement_thread_id: &str,
        boot_epoch: u64,
        cleanup_state: &str,
        terminal_reason: &str,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.settle_worker_process(
            worker_instance_id,
            placement_thread_id,
            boot_epoch,
            cleanup_state,
            terminal_reason,
        )?;
        self.forget_live_worker(worker_instance_id, boot_epoch);
        Ok(())
    }
}
