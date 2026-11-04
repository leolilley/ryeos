//! Launch planning, launch claims and launch metadata.
use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchCancellationResolution {
    Cancelled,
    Bound {
        thread_id: String,
    },
    Terminal {
        state: String,
        outcome_code: Option<String>,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum LaunchTaskAbortRegistrationError {
    #[error("active launch task signal registry reached its bounded capacity")]
    CapacityExceeded,
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum LaunchPlanningReservationError {
    #[error(transparent)]
    AlreadyReserved(#[from] LaunchPlanningAlreadyReserved),
    #[error(transparent)]
    CapacityExceeded(#[from] LaunchPlanningCapacityExceeded),
    #[error(transparent)]
    Internal(anyhow::Error),
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct LaunchPlanningStatus {
    pub launch_id: String,
    pub thread_id: Option<String>,
    pub status: String,
    pub outcome_code: Option<String>,
}

pub fn is_canonical_launch_id(launch_id: &str) -> bool {
    launch_id.starts_with("L-")
        && launch_id.len() == 34
        && launch_id[2..].bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub(super) fn validate_launch_planning_owner(
    record: &runtime_db::LaunchPlanningRecord,
    thread_requested_by: Option<&str>,
) -> Result<()> {
    if thread_requested_by != Some(record.requested_by.as_str()) {
        bail!(
            "launch planning owner disagrees with authoritative thread requester for reserved thread {}",
            record.reserved_thread_id
        );
    }
    Ok(())
}

pub(super) fn map_launch_planning_reservation_error(
    error: anyhow::Error,
) -> LaunchPlanningReservationError {
    if error
        .chain()
        .any(|cause| cause.is::<LaunchPlanningAlreadyReserved>())
    {
        LaunchPlanningReservationError::AlreadyReserved(LaunchPlanningAlreadyReserved)
    } else if error
        .chain()
        .any(|cause| cause.is::<LaunchPlanningCapacityExceeded>())
    {
        LaunchPlanningReservationError::CapacityExceeded(LaunchPlanningCapacityExceeded)
    } else {
        LaunchPlanningReservationError::Internal(error)
    }
}

/// A durable planning cancel or daemon-generation fence won before the
/// authoritative root or continuation row could be published.
///
/// This typed marker may travel through `anyhow` context so route-independent
/// launchers can preserve the stable public `launch_cancelled` contract without
/// inspecting an error message.
#[derive(Debug, thiserror::Error)]
#[error("launch planning admission is no longer active")]
pub struct LaunchPlanningInactive;

/// An event append returned an error and authoritative signed-head readback
/// could prove neither the exact commit nor its absence. Callers retaining
/// retryable input must not replay it: doing so could duplicate an already
/// committed event batch.
#[derive(Debug, thiserror::Error)]
#[error("event append commit state is uncertain")]
pub struct EventAppendCommitUncertain;

impl StateStore {
    /// Reserve a caller-retained opaque launch coordinate before background
    /// execution starts. The coordinate is not authority by itself: every
    /// later read/cancel also proves the authenticated owner.
    pub fn reserve_launch_planning_with_id(
        &self,
        launch_id: &str,
        reserved_thread_id: &str,
        requested_by: &str,
    ) -> std::result::Result<(), LaunchPlanningReservationError> {
        if !is_canonical_launch_id(launch_id) {
            return Err(LaunchPlanningReservationError::Internal(anyhow!(
                "launch id must be L- followed by exactly 32 hexadecimal characters"
            )));
        }
        let _permit = self
            .acquire_write_permit()
            .map_err(LaunchPlanningReservationError::Internal)?;
        let g = self
            .lock()
            .map_err(LaunchPlanningReservationError::Internal)?;
        g.runtime_db
            .reserve_launch_planning(launch_id, reserved_thread_id, requested_by)
            .map_err(map_launch_planning_reservation_error)?;
        Ok(())
    }

    pub fn launch_planning_status(
        &self,
        launch_id: &str,
        requested_by: &str,
    ) -> Result<Option<LaunchPlanningStatus>> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let Some(mut record) = g.runtime_db.launch_planning_by_id(launch_id)? else {
            return Ok(None);
        };
        if record.requested_by != requested_by {
            return Ok(None);
        }
        // Root publication is authoritative; the planning row is auxiliary.
        // Repair an interrupted bind while answering the exact owner rather
        // than leaving the caller unable to distinguish admission from loss.
        if record.state == "planning"
            && let Some(thread) = g.state_db.get_thread(&record.reserved_thread_id)?
        {
            validate_launch_planning_owner(&record, thread.requested_by.as_deref())?;
            g.runtime_db
                .bind_launch_planning(&record.reserved_thread_id)?;
            record = g
                .runtime_db
                .launch_planning_by_id(launch_id)?
                .ok_or_else(|| anyhow!("launch planning disappeared during status repair"))?;
        }
        let thread_id = match record.state.as_str() {
            "bound" => {
                let bound_thread_id = record.bound_thread_id.ok_or_else(|| {
                    anyhow!(
                        "bound launch planning record `{launch_id}` has no authoritative thread binding"
                    )
                })?;
                if bound_thread_id != record.reserved_thread_id {
                    bail!(
                        "bound launch planning record `{launch_id}` has divergent reserved and authoritative thread identities"
                    );
                }
                Some(bound_thread_id)
            }
            "qualified" | "planning" | "cancelled" | "failed" | "expired" => None,
            other => bail!("launch planning record `{launch_id}` has unknown state `{other}`"),
        };
        Ok(Some(LaunchPlanningStatus {
            launch_id: record.launch_id,
            thread_id,
            status: record.state,
            outcome_code: record.outcome_code,
        }))
    }

    /// Read one owner-bound launch-planning row without hiding its pre-minted
    /// thread coordinate. Candidate integration uses this only after the same
    /// coordinate was atomically committed with owner qualification.
    pub fn launch_planning_record_for_owner(
        &self,
        launch_id: &str,
        requested_by: &str,
    ) -> Result<Option<runtime_db::LaunchPlanningRecord>> {
        let g = self.lock()?;
        Ok(g.runtime_db
            .launch_planning_by_id(launch_id)?
            .filter(|record| record.requested_by == requested_by))
    }

    pub fn activate_qualified_launch_planning(
        &self,
        launch_id: &str,
        reserved_thread_id: &str,
        requested_by: &str,
    ) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        if !g.runtime_db.activate_qualified_launch_planning(
            launch_id,
            reserved_thread_id,
            requested_by,
        )? {
            bail!("qualified launch planning coordinate is no longer startable");
        }
        Ok(())
    }

    pub fn ensure_launch_planning_active(&self, reserved_thread_id: &str) -> Result<()> {
        let g = self.lock()?;
        let Some(record) = g.runtime_db.launch_planning_by_thread(reserved_thread_id)? else {
            return Ok(());
        };
        if record.state != "planning"
            || record.daemon_generation_id != runtime_db::daemon_generation_id()
        {
            return Err(LaunchPlanningInactive.into());
        }
        Ok(())
    }

    pub fn register_launch_task_abort(
        &self,
        reserved_thread_id: &str,
        abort_handle: tokio::task::AbortHandle,
    ) -> std::result::Result<(), LaunchTaskAbortRegistrationError> {
        self.register_launch_task_abort_bounded(
            reserved_thread_id,
            abort_handle,
            MAX_ACTIVE_LAUNCH_SIGNALS,
        )
    }

    pub(super) fn register_launch_task_abort_bounded(
        &self,
        reserved_thread_id: &str,
        abort_handle: tokio::task::AbortHandle,
        max_active_signals: usize,
    ) -> std::result::Result<(), LaunchTaskAbortRegistrationError> {
        let g = self.lock()?;
        let planning = g.runtime_db.launch_planning_by_thread(reserved_thread_id)?;
        let Some(record) = planning else {
            return Ok(());
        };
        if record.state == "bound" {
            return Ok(());
        }
        if record.state != "planning"
            || record.daemon_generation_id != runtime_db::daemon_generation_id()
        {
            abort_handle.abort();
            return Ok(());
        }
        let launch_id = record.launch_id;
        let mut handles = self
            .launch_task_abort_handles
            .lock()
            .map_err(|_| anyhow!("launch task abort registry lock poisoned"))?;
        if handles.len() >= max_active_signals && !handles.contains_key(&launch_id) {
            abort_handle.abort();
            return Err(LaunchTaskAbortRegistrationError::CapacityExceeded);
        }
        if let Some(previous) = handles.insert(launch_id, abort_handle) {
            previous.abort();
        }
        drop(handles);
        drop(g);
        Ok(())
    }

    pub fn unregister_launch_task_abort(&self, reserved_thread_id: &str) -> Result<()> {
        let launch_id = {
            let g = self.lock()?;
            g.runtime_db
                .launch_planning_by_thread(reserved_thread_id)?
                .map(|record| record.launch_id)
        };
        if let Some(launch_id) = launch_id {
            self.launch_task_abort_handles
                .lock()
                .map_err(|_| anyhow!("launch task abort registry lock poisoned"))?
                .remove(&launch_id);
        }
        Ok(())
    }

    /// Settle a pre-bind task exit, including unwind/abort paths. If the
    /// authoritative row committed first, repair the planning record to bound;
    /// otherwise mark it terminal failed. Already-terminal records are a no-op.
    pub fn settle_launch_planning_task_exit(&self, reserved_thread_id: &str) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let Some(record) = g.runtime_db.launch_planning_by_thread(reserved_thread_id)? else {
            return Ok(());
        };
        if record.state != "planning" {
            return Ok(());
        }
        if let Some(thread) = g.state_db.get_thread(reserved_thread_id)? {
            validate_launch_planning_owner(&record, thread.requested_by.as_deref())?;
            g.runtime_db.bind_launch_planning(reserved_thread_id)?;
        } else {
            g.runtime_db.fail_launch_planning(reserved_thread_id)?;
        }
        Ok(())
    }

    /// Settle an accepted request that was durably reserved but refused before
    /// its background task took ownership. The exact coordinate remains
    /// queryable, proving that no authoritative thread was launched.
    pub fn settle_launch_planning_admission_exit(&self, reserved_thread_id: &str) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let Some(record) = g.runtime_db.launch_planning_by_thread(reserved_thread_id)? else {
            return Ok(());
        };
        if record.state != "planning" {
            return Ok(());
        }
        if let Some(thread) = g.state_db.get_thread(reserved_thread_id)? {
            validate_launch_planning_owner(&record, thread.requested_by.as_deref())?;
            g.runtime_db.bind_launch_planning(reserved_thread_id)?;
        } else {
            g.runtime_db
                .fail_launch_planning_admission(reserved_thread_id)?;
        }
        Ok(())
    }

    /// Resolve cancel-vs-bind while holding the same store mutex used by root
    /// and continuation publication. A committed authoritative row always wins
    /// and is repaired to `bound` before the caller delegates to normal durable
    /// thread cancel.
    pub fn cancel_launch_planning(
        &self,
        launch_id: &str,
        requested_by: &str,
    ) -> Result<Option<LaunchCancellationResolution>> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let Some(record) = g.runtime_db.launch_planning_by_id(launch_id)? else {
            return Ok(None);
        };
        if record.requested_by != requested_by {
            return Ok(None);
        }
        if record.state == "qualified" {
            if g.state_db.get_thread(&record.reserved_thread_id)?.is_some() {
                bail!(
                    "qualified launch planning record `{launch_id}` unexpectedly has an authoritative thread"
                );
            }
            if g.runtime_db.cancel_unbound_launch_planning(launch_id)? {
                return Ok(Some(LaunchCancellationResolution::Cancelled));
            }
        }
        if record.state == "planning" {
            if let Some(thread) = g.state_db.get_thread(&record.reserved_thread_id)? {
                validate_launch_planning_owner(&record, thread.requested_by.as_deref())?;
                g.runtime_db
                    .bind_launch_planning(&record.reserved_thread_id)?;
                self.launch_task_abort_handles
                    .lock()
                    .map_err(|_| anyhow!("launch task abort registry lock poisoned"))?
                    .remove(launch_id);
                return Ok(Some(LaunchCancellationResolution::Bound {
                    thread_id: record.reserved_thread_id,
                }));
            }
            if g.runtime_db.cancel_unbound_launch_planning(launch_id)? {
                drop(g);
                if let Some(abort_handle) = self
                    .launch_task_abort_handles
                    .lock()
                    .map_err(|_| anyhow!("launch task abort registry lock poisoned"))?
                    .remove(launch_id)
                {
                    abort_handle.abort();
                }
                return Ok(Some(LaunchCancellationResolution::Cancelled));
            }
        }
        if record.state == "bound" {
            let thread_id = record.bound_thread_id.ok_or_else(|| {
                anyhow!(
                    "bound launch planning record `{launch_id}` has no authoritative thread binding"
                )
            })?;
            if thread_id != record.reserved_thread_id {
                bail!(
                    "bound launch planning record `{launch_id}` has divergent reserved and authoritative thread identities"
                );
            }
            return Ok(Some(LaunchCancellationResolution::Bound { thread_id }));
        }
        Ok(Some(LaunchCancellationResolution::Terminal {
            state: record.state,
            outcome_code: record.outcome_code,
        }))
    }

    pub fn reconcile_launch_planning(&self) -> Result<usize> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let mut repaired = 0usize;
        for record in g.runtime_db.pending_launch_planning()? {
            if let Some(thread) = g.state_db.get_thread(&record.reserved_thread_id)? {
                validate_launch_planning_owner(&record, thread.requested_by.as_deref())?;
                if g.runtime_db
                    .bind_launch_planning(&record.reserved_thread_id)?
                {
                    repaired += 1;
                }
            }
        }
        repaired += g.runtime_db.expire_stale_launch_planning()?;
        Ok(repaired)
    }

    /// Load at most two exact thread subjects under one state-store lock.
    /// Results retain input order and use `None` for a genuinely absent
    /// subject. Callers must collapse absence and authorization refusal before
    /// returning anything externally.
    pub fn authoritative_thread_subjects(
        &self,
        thread_ids: &[&str],
    ) -> Result<Vec<Option<AuthoritativeThreadSubject>>> {
        if thread_ids.is_empty() || thread_ids.len() > 2 {
            bail!("exact thread authority reads require one or two subjects");
        }
        let g = self.lock()?;
        thread_ids
            .iter()
            .map(|thread_id| {
                let Some(row) = g.state_db.get_thread(thread_id)? else {
                    return Ok(None);
                };
                let Some(snapshot) = g
                    .state_db
                    .read_authoritative_thread_snapshot(&row.chain_root_id, thread_id)?
                else {
                    bail!("thread {thread_id} is missing its authoritative signed snapshot");
                };
                if snapshot.thread_id.as_str() != *thread_id
                    || snapshot.chain_root_id != row.chain_root_id
                {
                    bail!("thread {thread_id} authoritative subject identity is inconsistent");
                }
                Ok(Some(AuthoritativeThreadSubject {
                    thread_id: snapshot.thread_id,
                    chain_root_id: snapshot.chain_root_id,
                    kind: snapshot.kind_name,
                    item_ref: snapshot.item_ref,
                    executor_ref: snapshot.executor_ref,
                    launch_mode: snapshot.launch_mode,
                    current_site_id: snapshot.current_site_id,
                    origin_site_id: snapshot.origin_site_id,
                    upstream_thread_id: snapshot.upstream_thread_id,
                    project_root: snapshot.project_root,
                    created_at: snapshot.created_at,
                    updated_at: snapshot.updated_at,
                    started_at: snapshot.started_at,
                    finished_at: snapshot.finished_at,
                    status: snapshot.status,
                    requested_by: snapshot.requested_by,
                    project_authority: snapshot.project_authority,
                    admitted_launch_capsule_hash: snapshot.admitted_launch_capsule_hash,
                    result_project_snapshot_hash: snapshot.result_project_snapshot_hash,
                    result_workspace_output_capture_hash: snapshot
                        .result_workspace_output_capture_hash,
                }))
            })
            .collect()
    }

    /// Seed a thread's launch identity (resume context / continuation spec) at
    /// spawn time so a continuation successor can be relaunched later with no
    /// live request. Metadata-only (does not touch pid/pgid); the
    /// clobber-preserving attach keeps it against a later empty self-attach.
    pub fn seed_launch_metadata(
        &self,
        thread_id: &str,
        launch_metadata: &crate::launch_metadata::RuntimeLaunchMetadata,
    ) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let _admission = Self::authorize_runtime_pin_for_thread(&g, thread_id)?;
        g.runtime_db.set_launch_metadata(thread_id, launch_metadata)
    }

    /// Atomically add the exact isolation generation/plan identity to a
    /// thread's already-seeded launch metadata without disturbing resume or
    /// cancellation authority.
    pub fn seed_isolation_provenance(
        &self,
        thread_id: &str,
        provenance: ryeos_engine::isolation::IsolationLaunchProvenance,
    ) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let _admission = Self::authorize_runtime_pin_for_thread(&g, thread_id)?;
        let mut metadata = g
            .runtime_db
            .get_runtime_info(thread_id)?
            .and_then(|info| info.launch_metadata)
            .unwrap_or_default();
        metadata.isolation = Some(provenance);
        g.runtime_db.set_launch_metadata(thread_id, &metadata)
    }

    /// Bind exact node-local workspace membership before process input
    /// preparation. Callers establish the existing child link first; no OS or
    /// view contact may occur between that linkage and this admission.
    pub fn bind_thread_workspace(
        &self,
        thread_id: &str,
        binding: &runtime_db::RuntimeWorkspaceBinding,
    ) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        if !self
            .process_attachment_admission_open
            .load(Ordering::Acquire)
        {
            bail!("workspace borrowing is fenced during daemon shutdown");
        }
        let thread = g
            .state_db
            .get_thread(thread_id)?
            .ok_or_else(|| anyhow!("workspace borrower thread is absent"))?;
        if is_terminal_status(&thread.status) {
            bail!("terminal thread cannot borrow a workspace");
        }
        let _admission = Self::authorize_runtime_pin_for_thread(&g, thread_id)?;
        if let Some(existing) = g.runtime_db.thread_workspace_binding(thread_id)?
            && existing != *binding
        {
            bail!("thread {thread_id} retains another exact workspace binding");
        }
        Self::authorize_workspace_root_lifecycle_locked(&g, binding)?;
        g.runtime_db.bind_thread_workspace(thread_id, binding)
    }

    /// Atomically bump the auto-resume counter and return the
    /// post-increment value.
    pub fn bump_resume_attempts(&self, thread_id: &str) -> Result<u32> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let _admission = Self::authorize_runtime_pin_for_thread(&g, thread_id)?;
        g.runtime_db.bump_resume_attempts(thread_id)
    }

    /// Startup sweep of launch claims left by dead daemon generations — see
    /// [`runtime_db::RuntimeDb::clear_stale_launch_claims`]. Must run before
    /// recovery drives any launch, or every stranded row skips as
    /// `AlreadyClaimed` forever.
    pub fn clear_stale_launch_claims(
        &self,
        current_daemon_generation_id: &str,
    ) -> Result<Vec<runtime_db::StaleLaunchClaimCleared>> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        g.runtime_db.clear_stale_launch_claims(
            &self.selected_mutation_permit(
                &g,
                SelectedMutationOperation::ClearStale(current_daemon_generation_id),
            )?,
            current_daemon_generation_id,
        )
    }

    /// Atomically claim the right to launch a thread. The sole authorization for
    /// a spawn — see [`runtime_db::RuntimeDb::claim_thread_launch`].
    pub fn claim_thread_launch(
        &self,
        thread_id: &str,
        claim_id: &str,
        claimed_by: &str,
    ) -> Result<runtime_db::LaunchClaimOutcome> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let _admission = Self::authorize_runtime_pin_for_thread(&g, thread_id)?;
        g.runtime_db.claim_thread_launch(
            &self.selected_mutation_permit(
                &g,
                SelectedMutationOperation::Claim(thread_id, claim_id, claimed_by),
            )?,
            thread_id,
            claim_id,
            claimed_by,
        )
    }

    /// Claim an existing thread and publish its in-process owner before the
    /// StateStore lock is released. Live reconciliation reads durable claims
    /// through this same lock, so it can never observe the new claim without
    /// also observing the active owner that is responsible for it.
    pub fn claim_thread_launch_active(
        &self,
        thread_id: &str,
        claim_id: &str,
        daemon_generation_id: &str,
    ) -> Result<Option<runtime_db::LaunchClaim>> {
        let _permit = self.acquire_write_permit()?;
        let mut g = self.lock()?;
        let _admission = Self::authorize_runtime_pin_for_thread(&g, thread_id)?;
        let outcome = g.runtime_db.claim_thread_launch(
            &self.selected_mutation_permit(
                &g,
                SelectedMutationOperation::Claim(thread_id, claim_id, daemon_generation_id),
            )?,
            thread_id,
            claim_id,
            daemon_generation_id,
        )?;
        match outcome {
            runtime_db::LaunchClaimOutcome::AlreadyClaimed => Ok(None),
            runtime_db::LaunchClaimOutcome::Claimed => {
                let claim = g
                    .runtime_db
                    .get_launch_claim(thread_id)?
                    .ok_or_else(|| anyhow!("launch owner disappeared after claim"))?;
                let active = &mut g.active_launch_owners;
                if !active.insert(claim.claimed_by.clone()) {
                    // No owner mutation happened; the mutable field borrow ends
                    // before exact rollback beneath the same Store guard.
                    g.runtime_db.release_thread_launch_claim(
                        &self.selected_mutation_permit(
                            &g,
                            SelectedMutationOperation::Release(thread_id, claim_id),
                        )?,
                        thread_id,
                        claim_id,
                    )?;
                    bail!("launch owner is already registered in this daemon");
                }
                Ok(Some(claim))
            }
        }
    }

    /// Reserve launch ownership for a pre-minted thread ID before its row is
    /// published. Fresh execution paths use this to make creation visible only
    /// after durable spawn ownership exists. The owned executor guard removes
    /// the reservation if creation fails; daemon startup clears reservations
    /// left by an interrupted process.
    pub fn reserve_fresh_thread_launch(
        &self,
        thread_id: &str,
        claim_id: &str,
        claimed_by: &str,
    ) -> Result<runtime_db::LaunchClaimOutcome> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        if g.state_db.get_thread(thread_id)?.is_some() {
            bail!(
                "fresh launch reservation requires an unpublished thread ID; thread already exists: {thread_id}"
            );
        }
        g.runtime_db.claim_thread_launch(
            &self.selected_mutation_permit(
                &g,
                SelectedMutationOperation::Claim(thread_id, claim_id, claimed_by),
            )?,
            thread_id,
            claim_id,
            claimed_by,
        )
    }

    /// Fresh-thread counterpart to [`Self::claim_thread_launch_active`]. The
    /// unpublished ID and its active durable owner become observable as one
    /// StateStore operation.
    pub fn reserve_fresh_thread_launch_active(
        &self,
        thread_id: &str,
        claim_id: &str,
        daemon_generation_id: &str,
    ) -> Result<Option<runtime_db::LaunchClaim>> {
        let _permit = self.acquire_write_permit()?;
        let mut g = self.lock()?;
        if g.state_db.get_thread(thread_id)?.is_some() {
            bail!(
                "fresh launch reservation requires an unpublished thread ID; thread already exists: {thread_id}"
            );
        }
        let outcome = g.runtime_db.claim_thread_launch(
            &self.selected_mutation_permit(
                &g,
                SelectedMutationOperation::Claim(thread_id, claim_id, daemon_generation_id),
            )?,
            thread_id,
            claim_id,
            daemon_generation_id,
        )?;
        match outcome {
            runtime_db::LaunchClaimOutcome::AlreadyClaimed => Ok(None),
            runtime_db::LaunchClaimOutcome::Claimed => {
                let claim = g
                    .runtime_db
                    .get_launch_claim(thread_id)?
                    .ok_or_else(|| anyhow!("fresh launch owner disappeared"))?;
                let active = &mut g.active_launch_owners;
                if !active.insert(claim.claimed_by.clone()) {
                    // No owner mutation happened; the mutable field borrow ends
                    // before exact rollback beneath the same Store guard.
                    g.runtime_db.release_thread_launch_claim(
                        &self.selected_mutation_permit(
                            &g,
                            SelectedMutationOperation::Release(thread_id, claim_id),
                        )?,
                        thread_id,
                        claim_id,
                    )?;
                    bail!("launch owner is already registered in this daemon");
                }
                Ok(Some(claim))
            }
        }
    }

    /// Register the actual process generation in the shared metadata domain.
    /// This does not admit a selected invocation or install any native owner.
    pub fn register_selected_bookkeeping_generation(&self) -> Result<crate::SelectedGeneration> {
        if self.read_only {
            bail!("selected bookkeeping is unavailable on a read-only store");
        }
        let _store = self.lock()?;
        let mut selected = self
            .selected_invocations
            .lock()
            .map_err(|_| anyhow!("selected invocation registry poisoned"))?;
        if !self
            .process_attachment_admission_open
            .load(Ordering::Acquire)
        {
            bail!("selected bookkeeping admission is closed");
        }
        selected
            .register_generation(runtime_db::daemon_generation_id())
            .map_err(|error| anyhow!("selected generation registration refused: {error:?}"))
    }

    /// Atomically retire the in-process owner and its exact durable claim with
    /// respect to live reconciliation. A replacement claim cannot be confused
    /// with the owner being dropped because deletion remains claim-id-qualified.
    pub fn release_active_thread_launch_claim(
        &self,
        thread_id: &str,
        claim_id: &str,
        launch_owner: &str,
    ) -> Result<bool> {
        let mut g = self.lock()?;
        let selected_permit = self.selected_mutation_permit(
            &g,
            SelectedMutationOperation::Release(thread_id, claim_id),
        )?;
        let Some(claim) = g.runtime_db.get_launch_claim(thread_id)? else {
            return Ok(false);
        };
        if claim.thread_id != thread_id
            || claim.claim_id != claim_id
            || claim.claimed_by != launch_owner
        {
            return Ok(false);
        }
        let deleted =
            g.runtime_db
                .release_thread_launch_claim(&selected_permit, thread_id, claim_id)?;
        if deleted {
            // Same Store guard owns this set: no lock, allocation or fallible
            // acknowledgment separates durable deletion and exact retirement.
            g.active_launch_owners.remove(launch_owner);
        }
        Ok(deleted)
    }

    /// Consume a completed runtime recovery attempt and replace the current
    /// in-process launch owner as one StateStore-visible operation.
    pub fn rotate_active_thread_launch_claim_after_runtime_recovery(
        &self,
        thread_id: &str,
        expected_launch_owner: &str,
        next_claim_id: &str,
        daemon_generation_id: &str,
        max_attempts: u32,
    ) -> Result<runtime_db::RuntimeRecoveryClaimRotation> {
        let _permit = self.acquire_write_permit()?;
        let mut g = self.lock()?;
        // Occupancy refusal precedes every LostOwner/Exhausted shortcut.
        self.selected_mutation_permit(
            &g,
            SelectedMutationOperation::Rotate(
                thread_id,
                expected_launch_owner,
                next_claim_id,
                daemon_generation_id,
                max_attempts,
            ),
        )?;
        let _admission = Self::authorize_runtime_pin_for_thread(&g, thread_id)?;
        if !g.active_launch_owners.contains(expected_launch_owner) {
            return Ok(runtime_db::RuntimeRecoveryClaimRotation::LostOwner);
        }
        let Some(current) = g.runtime_db.get_launch_claim(thread_id)? else {
            return Ok(runtime_db::RuntimeRecoveryClaimRotation::LostOwner);
        };
        if current.claimed_by != expected_launch_owner {
            return Ok(runtime_db::RuntimeRecoveryClaimRotation::LostOwner);
        }
        let next_owner = runtime_db::LaunchOwner {
            thread_id: thread_id.to_owned(),
            monotonic_launch_epoch: current
                .owner
                .monotonic_launch_epoch
                .checked_add(1)
                .ok_or_else(|| anyhow!("launch epoch overflow"))?,
            unpredictable_nonce: next_claim_id.to_owned(),
            daemon_generation_id: daemon_generation_id.to_owned(),
        };
        let next_active_owner = lillux::canonical_json(&serde_json::to_value(&next_owner)?)?;
        if g.active_launch_owners.contains(&next_active_owner) {
            bail!("rotated launch owner collides with an active daemon owner");
        }
        // Reserve the Store-owned set, then revalidate occupancy under this
        // unchanged guard.
        g.active_launch_owners
            .try_reserve(1)
            .map_err(|error| anyhow!("cannot reserve replacement active owner: {error}"))?;
        let selected_permit = self.selected_mutation_permit(
            &g,
            SelectedMutationOperation::Rotate(
                thread_id,
                expected_launch_owner,
                next_claim_id,
                daemon_generation_id,
                max_attempts,
            ),
        )?;
        let outcome = g
            .runtime_db
            .rotate_thread_launch_claim_after_runtime_recovery(
                &selected_permit,
                thread_id,
                expected_launch_owner,
                next_claim_id,
                daemon_generation_id,
                max_attempts,
                &next_owner,
            )?;
        if matches!(
            &outcome,
            runtime_db::RuntimeRecoveryClaimRotation::Rotated { .. }
        ) {
            // RuntimeDb validates next_owner before commit. Capacity and owned
            // key are ready; same guard prevents a competing owner insertion.
            g.active_launch_owners.remove(expected_launch_owner);
            g.active_launch_owners.insert(next_active_owner);
        }
        Ok(outcome)
    }

    /// Read the current launch claim, if any — distinguishes an unlaunched
    /// successor from one mid-launch for the reconciler.
    pub fn get_launch_claim(&self, thread_id: &str) -> Result<Option<runtime_db::LaunchClaim>> {
        let g = self.lock()?;
        g.runtime_db.get_launch_claim(thread_id)
    }

    /// Revalidate the actual original callback owner under its existing window.
    /// Reuses the finite RuntimeDb connection and StateStore acquisition seam.
    pub(crate) fn assert_launch_owner_until(
        &self,
        thread_id: &str,
        launch_owner: &str,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<()> {
        self.with_runtime_original_deadline(deadline, |db| {
            let claim = db
                .get_launch_claim(thread_id)?
                .ok_or_else(|| anyhow!("thread {thread_id} has no current launch owner"))?;
            if claim.claimed_by != launch_owner {
                bail!("stale launch owner for thread {thread_id}");
            }
            Ok(())
        })
    }

    pub fn assert_launch_owner(&self, thread_id: &str, launch_owner: &str) -> Result<()> {
        let g = self.lock()?;
        let claim = g
            .runtime_db
            .get_launch_claim(thread_id)?
            .ok_or_else(|| anyhow!("thread {thread_id} has no current launch owner"))?;
        if claim.claimed_by != launch_owner {
            bail!("stale launch owner for thread {thread_id}");
        }
        Ok(())
    }

    /// Linearize an already prepared historical response with the enclosing
    /// callback's current root and launch authority. The value is prepared
    /// before this gate so disclosure performs no provider or chunk I/O.
    pub(crate) fn disclose_to_current_consumer_callback<T>(
        &self,
        admission: &crate::operator_external_content::product_qualification::AuthenticatedConsumerRoot,
        value: T,
    ) -> Result<T> {
        let launch_owner = admission
            .callback_launch_owner()
            .context("consumer disclosure requires its actual callback admission")?;
        admission.with_live_callback(|| {
            let g = self.lock()?;
            let coordinate = admission.coordinate();
            let root = authoritative_snapshot_for_transition(
                &g,
                &coordinate.accepted_root_id,
                &coordinate.accepted_root_id,
            )?;
            anyhow::ensure!(
                root.status == ThreadStatus::Running
                    && root.error.is_none()
                    && root.thread_id == coordinate.accepted_root_id
                    && root.chain_root_id == coordinate.accepted_root_id
                    && root.requested_by.as_deref() == Some(admission.owner_principal())
                    && root.project_authority == *admission.accepted_project_authority()
                    && root.admitted_launch_capsule_hash.as_deref()
                        == Some(coordinate.accepted_capsule_hash.as_str()),
                "consumer disclosure lost its exact running accepted root"
            );
            g.runtime_db.disclose_to_current_consumer_callback(
                &coordinate.accepted_root_id,
                launch_owner,
                value,
            )
        })
    }

    /// Return the descriptor-stable process identity only when the caller is
    /// still the thread's exact launch owner. Owner validation and identity
    /// read share the StateStore lock so callback quiescence cannot borrow a
    /// replacement launch's process.
    pub fn execution_process_identity_owned(
        &self,
        thread_id: &str,
        launch_owner: &str,
    ) -> Result<crate::process::ExecutionProcessIdentity> {
        let g = self.lock()?;
        let claim = g
            .runtime_db
            .get_launch_claim(thread_id)?
            .ok_or_else(|| anyhow!("thread {thread_id} has no current launch owner"))?;
        if claim.claimed_by != launch_owner {
            bail!("stale launch owner for thread {thread_id}");
        }
        g.runtime_db
            .get_runtime_info(thread_id)?
            .and_then(|runtime| runtime.process_identity)
            .ok_or_else(|| anyhow!("thread {thread_id} has no attached process identity"))
    }
}
