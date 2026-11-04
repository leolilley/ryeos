//! Dedicated session admission, commands, approvals and worker scopes.
use super::*;

impl StateStore {
    pub fn admit_dedicated_session(&self, session: NewDedicatedSession<'_>) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.admit_dedicated_session(session)
    }

    pub fn admit_dedicated_session_with_remote(
        &self,
        session: NewDedicatedSession<'_>,
        remote_thread_id: &str,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db
            .admit_dedicated_session_with_remote(session, Some(remote_thread_id))
    }

    pub fn admit_dedicated_session_from_reservation(
        &self,
        session: NewDedicatedSession<'_>,
        remote_thread_id: Option<&str>,
        reservation_id: &str,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.admit_dedicated_session_from_reservation(
            session,
            remote_thread_id,
            reservation_id,
        )
    }

    pub fn dedicated_session(
        &self,
        placement_thread_id: &str,
    ) -> Result<Option<DedicatedSessionRecord>> {
        let g = self.lock()?;
        g.runtime_db.dedicated_session(placement_thread_id)
    }

    pub fn dedicated_sessions_in_state(
        &self,
        state: &str,
    ) -> Result<Vec<runtime_db::DedicatedSessionRecord>> {
        let g = self.lock()?;
        g.runtime_db.dedicated_sessions_in_state(state)
    }

    pub fn terminalize_unattached_dedicated_session(
        &self,
        placement_thread_id: &str,
        reason: &str,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db
            .terminalize_unattached_dedicated_session(placement_thread_id, reason)?;
        drop(g);
        self.note_closed_worker_scope_retirement_for_session(placement_thread_id);
        Ok(())
    }

    pub fn fail_dedicated_session_start(
        &self,
        placement_thread_id: &str,
        worker_instance_id: &str,
        worker_boot_epoch: u64,
        reason: &str,
        cleanup_proved: bool,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.fail_dedicated_session_start(
            placement_thread_id,
            worker_instance_id,
            worker_boot_epoch,
            reason,
            cleanup_proved,
        )?;
        drop(g);
        if cleanup_proved {
            self.note_closed_worker_scope_retirement_for_session(placement_thread_id);
        }
        Ok(())
    }

    pub fn bind_dedicated_remote_thread(
        &self,
        placement_thread_id: &str,
        worker_instance_id: &str,
        worker_boot_epoch: u64,
        remote_thread_id: &str,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.bind_dedicated_remote_thread(
            placement_thread_id,
            worker_instance_id,
            worker_boot_epoch,
            remote_thread_id,
        )
    }

    pub fn observe_dedicated_remote_reattach(
        &self,
        placement_thread_id: &str,
        worker_boot_epoch: u64,
        remote_thread_id: &str,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.observe_dedicated_remote_reattach(
            placement_thread_id,
            worker_boot_epoch,
            remote_thread_id,
        )
    }

    pub fn settle_dedicated_remote_recovery_status(
        &self,
        placement_thread_id: &str,
        worker_boot_epoch: u64,
        remote_thread_id: &str,
        remote_status: &str,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.settle_dedicated_remote_recovery_status(
            placement_thread_id,
            worker_boot_epoch,
            remote_thread_id,
            remote_status,
        )
    }

    pub fn prepare_dedicated_session_recovery(
        &self,
        placement_thread_id: &str,
        credential_generation: u64,
        credential_lock_owner: &str,
        admitted_workspace_id: &str,
    ) -> Result<u64> {
        let g = self.lock()?;
        self.authorize_dedicated_workspace_contact_locked(
            &g,
            placement_thread_id,
            admitted_workspace_id,
        )?;
        g.runtime_db.prepare_dedicated_session_recovery(
            placement_thread_id,
            credential_generation,
            credential_lock_owner,
            admitted_workspace_id,
        )
    }

    pub fn fail_dedicated_candidate_disposition(
        &self,
        placement_thread_id: &str,
        reserved_state: &str,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db
            .fail_dedicated_candidate_disposition(placement_thread_id, reserved_state)
    }

    pub fn cancel_dedicated_candidate_for_root_stop(
        &self,
        placement_thread_id: &str,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db
            .cancel_dedicated_candidate_for_root_stop(placement_thread_id)
    }

    pub fn observe_dedicated_session_state(
        &self,
        placement_thread_id: &str,
        worker_boot_epoch: u64,
        expected: &str,
        next: &str,
        expected_turn_id: Option<&str>,
        next_turn_id: Option<&str>,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.observe_dedicated_session_state(
            placement_thread_id,
            worker_boot_epoch,
            expected,
            next,
            expected_turn_id,
            next_turn_id,
        )
    }

    pub fn reserve_dedicated_session_command(
        &self,
        command: NewDedicatedSessionCommand<'_>,
    ) -> Result<DedicatedSessionCommandRecord> {
        let g = self.lock()?;
        g.runtime_db.reserve_dedicated_session_command(command)
    }

    pub fn reserve_dedicated_session_bounded_outcome(
        &self,
        placement_thread_id: &str,
        outcome: &ryeos_runtime::callback::DedicatedSessionBoundedOutcome,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db
            .reserve_dedicated_session_bounded_outcome(placement_thread_id, outcome)
    }

    pub fn require_bounded_route_contact_admission(
        &self,
        placement_thread_id: &str,
        worker_boot_epoch: u64,
        command_kind: &str,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.require_bounded_route_contact_admission(
            placement_thread_id,
            worker_boot_epoch,
            command_kind,
        )
    }

    pub fn settled_dedicated_session_command_replay(
        &self,
        placement_thread_id: &str,
        idempotency_key: &str,
        command_kind: &str,
        request_digest: &str,
        payload: &serde_json::Value,
    ) -> Result<Option<DedicatedSessionCommandRecord>> {
        let g = self.lock()?;
        g.runtime_db.settled_dedicated_session_command_replay(
            placement_thread_id,
            idempotency_key,
            command_kind,
            request_digest,
            payload,
        )
    }

    pub fn dedicated_session_command(
        &self,
        placement_thread_id: &str,
        command_sequence: u64,
    ) -> Result<Option<DedicatedSessionCommandRecord>> {
        let g = self.lock()?;
        g.runtime_db
            .dedicated_session_command(placement_thread_id, command_sequence)
    }

    pub fn dedicated_session_command_by_key(
        &self,
        placement_thread_id: &str,
        idempotency_key: &str,
    ) -> Result<Option<DedicatedSessionCommandRecord>> {
        let g = self.lock()?;
        g.runtime_db
            .dedicated_session_command_by_key(placement_thread_id, idempotency_key)
    }

    pub fn dedicated_session_commands(
        &self,
        placement_thread_id: &str,
    ) -> Result<Vec<DedicatedSessionCommandRecord>> {
        let g = self.lock()?;
        g.runtime_db.dedicated_session_commands(placement_thread_id)
    }

    pub fn dedicated_session_checkpoint_settlement_digest(
        &self,
        placement_thread_id: &str,
    ) -> Result<String> {
        let g = self.lock()?;
        g.runtime_db
            .dedicated_session_checkpoint_settlement_digest(placement_thread_id)
    }

    pub fn dedicated_command_outbox_records(&self) -> Result<Vec<DedicatedSessionCommandRecord>> {
        let g = self.lock()?;
        g.runtime_db.dedicated_command_outbox_records()
    }

    pub fn mark_dedicated_command_contacted(
        &self,
        placement_thread_id: &str,
        command_sequence: u64,
        worker_boot_epoch: u64,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.mark_dedicated_command_contacted(
            placement_thread_id,
            command_sequence,
            worker_boot_epoch,
        )
    }

    pub fn settle_dedicated_command_uncontacted(
        &self,
        placement_thread_id: &str,
        command_sequence: u64,
        worker_boot_epoch: u64,
        result: &Value,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.settle_dedicated_command_uncontacted(
            placement_thread_id,
            command_sequence,
            worker_boot_epoch,
            result,
        )
    }

    pub fn repair_fenced_dedicated_command_budget_refusal(
        &self,
        placement_thread_id: &str,
        command_sequence: u64,
        worker_boot_epoch: u64,
        result: &Value,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.repair_fenced_dedicated_command_budget_refusal(
            placement_thread_id,
            command_sequence,
            worker_boot_epoch,
            result,
        )
    }

    pub fn settle_dedicated_command(
        &self,
        placement_thread_id: &str,
        command_sequence: u64,
        worker_boot_epoch: u64,
        succeeded: bool,
        result: &Value,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.settle_dedicated_command(
            placement_thread_id,
            command_sequence,
            worker_boot_epoch,
            succeeded,
            result,
        )
    }

    pub fn settle_recovered_dedicated_command(
        &self,
        placement_thread_id: &str,
        command_sequence: u64,
        worker_boot_epoch: u64,
        result: &Value,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.settle_recovered_dedicated_command(
            placement_thread_id,
            command_sequence,
            worker_boot_epoch,
            result,
        )
    }

    pub fn settle_terminal_recovered_dedicated_command(
        &self,
        placement_thread_id: &str,
        command_sequence: u64,
        worker_boot_epoch: u64,
        result: &Value,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.settle_terminal_recovered_dedicated_command(
            placement_thread_id,
            command_sequence,
            worker_boot_epoch,
            result,
        )
    }

    pub fn mark_dedicated_command_outcome_unknown(
        &self,
        placement_thread_id: &str,
        command_sequence: u64,
        worker_boot_epoch: u64,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.mark_dedicated_command_outcome_unknown(
            placement_thread_id,
            command_sequence,
            worker_boot_epoch,
        )
    }

    pub fn mark_committed_dedicated_command_contact_unknown(
        &self,
        placement_thread_id: &str,
        command_sequence: u64,
        worker_boot_epoch: u64,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db
            .mark_committed_dedicated_command_contact_unknown(
                placement_thread_id,
                command_sequence,
                worker_boot_epoch,
            )
    }

    pub fn reserve_dedicated_observation_batch(
        &self,
        placement_thread_id: &str,
        worker_boot_epoch: u64,
        first_sequence: u64,
        through_sequence: u64,
        previous_digest: Option<&str>,
        batch_digest: &str,
        canonical_batch: &serde_json::Value,
    ) -> Result<ObservationBatchReservation> {
        let g = self.lock()?;
        g.runtime_db.reserve_dedicated_observation_batch(
            placement_thread_id,
            worker_boot_epoch,
            first_sequence,
            through_sequence,
            previous_digest,
            batch_digest,
            canonical_batch,
        )
    }

    pub fn exact_dedicated_observation_batch_exists(
        &self,
        placement_thread_id: &str,
        worker_boot_epoch: u64,
        first_sequence: u64,
        through_sequence: u64,
        previous_digest: Option<&str>,
        batch_digest: &str,
        canonical_batch: &serde_json::Value,
    ) -> Result<bool> {
        let g = self.lock()?;
        g.runtime_db.exact_dedicated_observation_batch_exists(
            placement_thread_id,
            worker_boot_epoch,
            first_sequence,
            through_sequence,
            previous_digest,
            batch_digest,
            canonical_batch,
        )
    }

    pub fn settle_dedicated_observation_batch(
        &self,
        placement_thread_id: &str,
        worker_boot_epoch: u64,
        first_sequence: u64,
        batch_digest: &str,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.settle_dedicated_observation_batch(
            placement_thread_id,
            worker_boot_epoch,
            first_sequence,
            batch_digest,
        )
    }

    pub fn dedicated_observation_outbox_records(
        &self,
    ) -> Result<Vec<runtime_db::DedicatedObservationBatchRecord>> {
        let g = self.lock()?;
        g.runtime_db.dedicated_observation_outbox_records()
    }

    pub fn discard_unappended_dedicated_observation_batch(
        &self,
        placement_thread_id: &str,
        worker_boot_epoch: u64,
        first_sequence: u64,
        batch_digest: &str,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.discard_unappended_dedicated_observation_batch(
            placement_thread_id,
            worker_boot_epoch,
            first_sequence,
            batch_digest,
        )
    }

    pub fn mark_dedicated_observation_batch_unknown(
        &self,
        placement_thread_id: &str,
        worker_boot_epoch: u64,
        first_sequence: u64,
        batch_digest: &str,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.mark_dedicated_observation_batch_unknown(
            placement_thread_id,
            worker_boot_epoch,
            first_sequence,
            batch_digest,
        )
    }

    pub fn create_dedicated_session_approval(
        &self,
        approval: NewDedicatedSessionApproval<'_>,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.create_dedicated_session_approval(approval)
    }

    pub fn pending_dedicated_session_approvals(
        &self,
        placement_thread_id: &str,
    ) -> Result<Vec<runtime_db::DedicatedSessionApprovalRecord>> {
        let g = self.lock()?;
        g.runtime_db
            .pending_dedicated_session_approvals(placement_thread_id)
    }

    pub fn pending_dedicated_session_approval_attention(
        &self,
        owner_principal: &str,
        placement_thread_ids: Option<&BTreeSet<String>>,
        limit: usize,
    ) -> Result<Vec<runtime_db::DedicatedSessionApprovalProjection>> {
        let g = self.lock()?;
        g.runtime_db.pending_dedicated_session_approval_attention(
            owner_principal,
            placement_thread_ids,
            limit,
        )
    }

    pub fn dedicated_session_candidate_attention(
        &self,
        owner_principal: &str,
        placement_thread_ids: Option<&BTreeSet<String>>,
        limit: usize,
    ) -> Result<Vec<runtime_db::DedicatedSessionCandidateAttention>> {
        let g = self.lock()?;
        g.runtime_db.dedicated_session_candidate_attention(
            owner_principal,
            placement_thread_ids,
            limit,
        )
    }

    pub fn dedicated_session_approval_history(
        &self,
        owner_principal: &str,
        placement_thread_ids: Option<&BTreeSet<String>>,
        limit: usize,
    ) -> Result<Vec<runtime_db::DedicatedSessionApprovalHistory>> {
        let g = self.lock()?;
        g.runtime_db.dedicated_session_approval_history(
            owner_principal,
            placement_thread_ids,
            limit,
        )
    }

    pub fn dedicated_session_approval(
        &self,
        placement_thread_id: &str,
        approval_id: &str,
    ) -> Result<Option<runtime_db::DedicatedSessionApprovalRecord>> {
        let g = self.lock()?;
        g.runtime_db
            .dedicated_session_approval(placement_thread_id, approval_id)
    }

    pub fn dedicated_approval_has_exact_state(
        &self,
        placement_thread_id: &str,
        approval_id: &str,
        worker_boot_epoch: u64,
        request_digest: &str,
        reservation_token: &str,
        decision_digest: &str,
        approval_state: &str,
    ) -> Result<bool> {
        let g = self.lock()?;
        g.runtime_db.dedicated_approval_has_exact_state(
            placement_thread_id,
            approval_id,
            worker_boot_epoch,
            request_digest,
            reservation_token,
            decision_digest,
            approval_state,
        )
    }

    pub fn dedicated_approval_outbox_session_ids(&self) -> Result<Vec<String>> {
        let g = self.lock()?;
        g.runtime_db.dedicated_approval_outbox_session_ids()
    }

    pub fn reconcile_dedicated_approval_delivery_unknown(
        &self,
        placement_thread_id: &str,
        approval_id: &str,
        worker_boot_epoch: u64,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.reconcile_dedicated_approval_delivery_unknown(
            placement_thread_id,
            approval_id,
            worker_boot_epoch,
        )
    }

    pub fn reconcile_dedicated_approval_stale_epoch(
        &self,
        placement_thread_id: &str,
        approval_id: &str,
        worker_boot_epoch: u64,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.reconcile_dedicated_approval_stale_epoch(
            placement_thread_id,
            approval_id,
            worker_boot_epoch,
        )
    }

    pub fn reserve_dedicated_session_completion(
        &self,
        placement_thread_id: &str,
        worker_boot_epoch: u64,
        completion_fence: &ryeos_runtime::callback::HostedCommandCompletionFence,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.reserve_dedicated_session_completion(
            placement_thread_id,
            worker_boot_epoch,
            completion_fence,
        )
    }

    pub fn reserve_dedicated_session_bounded_completion(
        &self,
        placement_thread_id: &str,
        worker_boot_epoch: u64,
        completion_fence: &ryeos_runtime::callback::HostedCommandCompletionFence,
        outcome: &ryeos_runtime::callback::DedicatedSessionBoundedOutcome,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.reserve_dedicated_session_bounded_completion(
            placement_thread_id,
            worker_boot_epoch,
            completion_fence,
            outcome,
        )
    }

    pub fn require_dedicated_session_route_frontier(
        &self,
        placement_thread_id: &str,
        expected_route_command_sequence: u64,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.require_dedicated_session_route_frontier(
            placement_thread_id,
            expected_route_command_sequence,
        )
    }

    pub fn reserve_dedicated_session_approval_decision(
        &self,
        placement_thread_id: &str,
        approval_id: &str,
        worker_boot_epoch: u64,
        request_digest: &str,
        decision_principal: &str,
        decision: &Value,
        decision_digest: &str,
        reservation_token: &str,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.reserve_dedicated_session_approval_decision(
            placement_thread_id,
            approval_id,
            worker_boot_epoch,
            request_digest,
            decision_principal,
            decision,
            decision_digest,
            reservation_token,
        )
    }

    pub fn mark_dedicated_approval_delivery_contacting(
        &self,
        placement_thread_id: &str,
        approval_id: &str,
        worker_boot_epoch: u64,
        reservation_token: &str,
        decision_digest: &str,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.mark_dedicated_approval_delivery_contacting(
            placement_thread_id,
            approval_id,
            worker_boot_epoch,
            reservation_token,
            decision_digest,
        )
    }

    pub fn settle_dedicated_approval_delivery(
        &self,
        placement_thread_id: &str,
        approval_id: &str,
        worker_boot_epoch: u64,
        reservation_token: &str,
        decision_digest: &str,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.settle_dedicated_approval_delivery(
            placement_thread_id,
            approval_id,
            worker_boot_epoch,
            reservation_token,
            decision_digest,
        )
    }

    pub fn settle_recovered_dedicated_approval_delivery(
        &self,
        placement_thread_id: &str,
        approval_id: &str,
        worker_boot_epoch: u64,
        reservation_token: &str,
        decision_digest: &str,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.settle_recovered_dedicated_approval_delivery(
            placement_thread_id,
            approval_id,
            worker_boot_epoch,
            reservation_token,
            decision_digest,
        )
    }

    pub fn mark_dedicated_approval_delivery_unknown(
        &self,
        placement_thread_id: &str,
        approval_id: &str,
        worker_boot_epoch: u64,
        reservation_token: &str,
        decision_digest: &str,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.mark_dedicated_approval_delivery_unknown(
            placement_thread_id,
            approval_id,
            worker_boot_epoch,
            reservation_token,
            decision_digest,
        )
    }

    pub fn expire_dedicated_session_approval(
        &self,
        placement_thread_id: &str,
        approval_id: &str,
        worker_boot_epoch: u64,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.expire_dedicated_session_approval(
            placement_thread_id,
            approval_id,
            worker_boot_epoch,
        )
    }

    pub fn observe_dedicated_session_approval_expiry(
        &self,
        placement_thread_id: &str,
        approval_id: &str,
        worker_boot_epoch: u64,
        request_digest: &str,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.observe_dedicated_session_approval_expiry(
            placement_thread_id,
            approval_id,
            worker_boot_epoch,
            request_digest,
        )
    }

    /// Existing session/workspace journals authorize removal before Lillux
    /// touches the kernel resource. No StateStore lock spans OS operations;
    /// restart replays the same exact intent after a removal/commit crash.
    pub fn retire_closed_worker_scopes_for_workspace(&self, workspace_id: &str) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        let reserved = self
            .lock()?
            .runtime_db
            .reserve_closed_worker_scope_retirement(workspace_id)?;
        let Some((placement, retirement)) = reserved else {
            return Ok(());
        };
        if retirement.state == runtime_db::ScopeRetirementState::Retired {
            return Ok(());
        }
        for scope in &retirement.scopes {
            scope
                .retire_after_settlement()
                .map_err(anyhow::Error::msg)
                .context("retire exactly settled worker scope; durable intent retained")?;
        }
        self.lock()?
            .runtime_db
            .complete_closed_worker_scope_retirement(&placement, &retirement)
    }

    /// Startup recovery only, not a recurring SQLite poll. Ordinary closure
    /// drives retirement directly; failed removals keep their exact intents.
    pub fn resume_closed_worker_scope_retirements(&self) -> Result<()> {
        let workspaces = self
            .lock()?
            .runtime_db
            .closed_worker_scope_retirement_workspaces()?;
        for workspace in workspaces {
            self.note_closed_worker_scope_retirement(&workspace);
        }
        Ok(())
    }

    pub(super) fn note_closed_worker_scope_retirement(&self, workspace_id: &str) {
        if let Err(error) = self.retire_closed_worker_scopes_for_workspace(workspace_id) {
            // A retained resource is not failed candidate capture, nor is a
            // failed removal proof of process death. Preserve the closed
            // workspace and the retryable resource obligation independently.
            tracing::warn!(workspace_id, error = %error, "closed worker scope retirement remains pending");
        }
    }

    pub(super) fn note_closed_worker_scope_retirement_for_session(&self, placement: &str) {
        // Failed-start settlement can follow workspace closure. Drive that
        // ordering from the settlement event too; do not wait for a restart
        // or introduce a timer. The reservation rechecks every owner below.
        let workspace = (|| -> Result<Option<String>> {
            let g = self.lock()?;
            let Some(session) = g.runtime_db.dedicated_session(placement)? else {
                return Ok(None);
            };
            Ok(g.runtime_db
                .workspace(&session.workspace_id)?
                .filter(|workspace| workspace.state == runtime_db::WorkspaceState::Closed)
                .map(|workspace| workspace.workspace_id))
        })();
        match workspace {
            Ok(Some(workspace)) => self.note_closed_worker_scope_retirement(&workspace),
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(placement, error = %error, "closed worker scope retirement lookup remains pending")
            }
        }
    }

    /// Re-drive the existing exact retirement owner after an idempotent
    /// terminal disposition retry. The disposition service has already
    /// reverified its immutable owner fact; this method neither discovers a
    /// scope nor grants removal authority of its own.
    pub fn resume_closed_worker_scope_retirement_for_session(&self, placement: &str) -> Result<()> {
        let workspace = {
            let g = self.lock()?;
            let session = g
                .runtime_db
                .dedicated_session(placement)?
                .ok_or_else(|| anyhow!("dedicated session disappeared before scope retirement"))?;
            g.runtime_db
                .workspace(&session.workspace_id)?
                .filter(|workspace| workspace.state == runtime_db::WorkspaceState::Closed)
                .map(|workspace| workspace.workspace_id)
                .ok_or_else(|| anyhow!("dedicated session workspace is not closed"))?
        };
        self.retire_closed_worker_scopes_for_workspace(&workspace)
    }

    /// Scope reservation uses the same workspace/shutdown admission owner as
    /// attachment. No child may be spawned until this pre-contact write commits.
    pub fn reserve_dedicated_worker_scope(
        &self,
        placement_thread_id: &str,
        worker_instance_id: &str,
        boot_epoch: u64,
        scope: &lillux::ProcessScopeAllocation,
    ) -> Result<()> {
        let g = self.lock()?;
        if !self
            .process_attachment_admission_open
            .load(Ordering::Acquire)
        {
            bail!("worker scope admission is closed for daemon shutdown");
        }
        let session = g
            .runtime_db
            .dedicated_session(placement_thread_id)?
            .ok_or_else(|| anyhow!("worker scope has no admitted dedicated session"))?;
        self.authorize_dedicated_workspace_contact_locked(
            &g,
            placement_thread_id,
            &session.workspace_id,
        )?;
        g.runtime_db.reserve_dedicated_worker_scope(
            placement_thread_id,
            worker_instance_id,
            boot_epoch,
            scope,
        )
    }

    pub fn bind_dedicated_worker_scope(
        &self,
        placement_thread_id: &str,
        worker_instance_id: &str,
        boot_epoch: u64,
        recovery: &lillux::ProcessScopeRecovery,
    ) -> Result<()> {
        let g = self.lock()?;
        if !self
            .process_attachment_admission_open
            .load(Ordering::Acquire)
        {
            bail!("worker scope binding is closed for daemon shutdown");
        }
        let session = g
            .runtime_db
            .dedicated_session(placement_thread_id)?
            .ok_or_else(|| anyhow!("worker scope has no admitted dedicated session"))?;
        self.authorize_dedicated_workspace_contact_locked(
            &g,
            placement_thread_id,
            &session.workspace_id,
        )?;
        g.runtime_db.bind_dedicated_worker_scope(
            placement_thread_id,
            worker_instance_id,
            boot_epoch,
            recovery,
        )
    }

    pub fn dedicated_worker_scope(
        &self,
        placement_thread_id: &str,
        worker_instance_id: &str,
        boot_epoch: u64,
    ) -> Result<Option<runtime_db::DedicatedWorkerScopeReservation>> {
        self.lock()?.runtime_db.dedicated_worker_scope(
            placement_thread_id,
            worker_instance_id,
            boot_epoch,
        )
    }
}
