//! Hook dispatch, runtime action intents and workspace operations.
use super::*;

impl StateStore {
    pub fn complete_hook_dispatch(
        &self,
        dispatch_key: &str,
        request_hash: &str,
        response: &Value,
    ) -> Result<runtime_db::CompletedHookDispatch> {
        let g = self.lock()?;
        g.runtime_db
            .complete_hook_dispatch(dispatch_key, request_hash, response)
    }

    /// Append the daemon-authored durable outcome for one completed
    /// observation hook. The completed response is re-read from the
    /// operational ledger under the same state lock; caller-supplied result
    /// bytes are never accepted as evidence.
    pub fn append_completed_hook_outcome(
        &self,
        current_thread_id: &str,
        identity: &ryeos_runtime::callback::HookDispatchIdentity,
        dispatch_key: &str,
        request_hash: &str,
    ) -> Result<PersistedEventRecord> {
        use ryeos_runtime::envelope::{
            HookDispatchFailureKind, normalize_hook_dispatch_result, normalize_hook_observation,
        };
        use ryeos_runtime::{
            HOOK_OBSERVATION_SCHEMA, HookEvidenceDescriptor, HookFailureClass,
            HookObservationRecordedPayload, HookResultMode,
        };

        if identity.result_mode != HookResultMode::Observation {
            bail!("durable hook outcome requires result mode `observation`");
        }
        let permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let completed = g
            .runtime_db
            .completed_hook_dispatch(dispatch_key, request_hash)?;
        if completed.event != identity.occurrence.event() || completed.hook_id != identity.hook_id {
            bail!("completed hook dispatch identity does not match callback identity");
        }
        let current_thread = g
            .state_db
            .get_thread(current_thread_id)?
            .ok_or_else(|| anyhow!("hook outcome thread not found: {current_thread_id}"))?;
        if current_thread.chain_root_id != completed.chain_root_id {
            bail!(
                "hook outcome thread {current_thread_id} belongs to chain {}, not {}",
                current_thread.chain_root_id,
                completed.chain_root_id
            );
        }
        if current_thread.status != ThreadStatus::Running.as_str() {
            bail!(
                "hook outcome append requires a running caller; {current_thread_id} is '{}'",
                current_thread.status
            );
        }
        let occurrence_thread = g
            .state_db
            .get_thread(&completed.occurrence_thread_id)?
            .ok_or_else(|| {
                anyhow!(
                    "hook occurrence thread not found: {}",
                    completed.occurrence_thread_id
                )
            })?;
        if occurrence_thread.chain_root_id != completed.chain_root_id {
            bail!("hook occurrence thread does not belong to the completed dispatch chain");
        }
        let runtime = g
            .runtime_db
            .get_runtime_info(current_thread_id)?
            .ok_or_else(|| anyhow!("runtime row missing for hook outcome: {current_thread_id}"))?;
        if runtime.stop_intent.is_some()
            || !self
                .process_attachment_admission_open
                .load(Ordering::Acquire)
        {
            bail!("hook outcome append refused after runtime admission closed");
        }

        let callback: ryeos_runtime::callback_contract::CallbackDispatchResponse =
            serde_json::from_value(completed.response.clone())
                .context("completed hook response violates callback contract")?;
        let descriptor = HookEvidenceDescriptor {
            id: identity.hook_id.clone(),
            layer: identity.layer,
            event: identity.occurrence.event().to_string(),
            occurrence: identity.occurrence.clone(),
        };
        let outcome = normalize_hook_dispatch_result(callback.result);
        let event = match outcome {
            Ok(output) if output.failure.is_none() => {
                match normalize_hook_observation(output.value) {
                    Ok(observation) => NewEventRecord {
                        event_type: ryeos_state::event_types::HOOK_OBSERVATION_RECORDED.to_string(),
                        storage_class: "indexed".to_string(),
                        payload: serde_json::to_value(HookObservationRecordedPayload {
                            schema_version: HOOK_OBSERVATION_SCHEMA.to_string(),
                            observation_id: format!("hook-observation:{dispatch_key}"),
                            dispatch_key: dispatch_key.to_string(),
                            occurrence_thread_id: completed.occurrence_thread_id.clone(),
                            hook: descriptor,
                            context_hash: identity.context_hash.clone(),
                            response_hash: completed.response_hash.clone(),
                            observation,
                        })?,
                    },
                    Err(_) => hook_failed_event(
                        &completed,
                        identity,
                        descriptor,
                        HookFailureClass::ObservationInvalid,
                    )?,
                }
            }
            Ok(output) => {
                let failure_class = match output.failure.expect("checked failure").kind {
                    HookDispatchFailureKind::Child => HookFailureClass::Child,
                    HookDispatchFailureKind::Integrity => HookFailureClass::ChildIntegrity,
                };
                hook_failed_event(&completed, identity, descriptor, failure_class)?
            }
            Err(_) => hook_failed_event(
                &completed,
                identity,
                descriptor,
                HookFailureClass::DispatchEnvelopeIntegrity,
            )?,
        };
        let mut persisted = append_events_locked(
            &g,
            Some(permit.cas_guard()),
            &completed.chain_root_id,
            current_thread_id,
            &[event],
        )?;
        persisted
            .pop()
            .ok_or_else(|| anyhow!("hook outcome append returned no event"))
    }

    /// Reserve the stable child identity for one runtime callback occurrence
    /// while the authoritative caller chain is still admitted.
    pub fn reserve_runtime_action_intent(
        &self,
        operation_id: &str,
        caller_thread_id: &str,
        mode: runtime_db::RuntimeActionMode,
        request_hash: &str,
        proposed_child_thread_id: &str,
        child_project_authority: Option<&ryeos_state::objects::ExecutionProjectAuthority>,
    ) -> Result<String> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let caller = g
            .state_db
            .get_thread(caller_thread_id)?
            .ok_or_else(|| anyhow!("runtime action caller {caller_thread_id} does not exist"))?;
        let _admission = g.state_db.authorize_runtime_pin(&caller.chain_root_id)?;
        g.runtime_db.reserve_runtime_action_intent(
            operation_id,
            &caller.chain_root_id,
            caller_thread_id,
            mode,
            request_hash,
            proposed_child_thread_id,
            child_project_authority,
        )
    }

    /// Reserve the shared-workspace relationship in the existing
    /// RuntimeActionIntent transaction. `execution_workspace` remains the
    /// materialization journal; it must not be repurposed as a transient lease
    /// state machine, and no parallel workspace-lock table may be introduced.
    pub fn reserve_runtime_action_intent_with_workspace(
        &self,
        operation_id: &str,
        caller_thread_id: &str,
        mode: runtime_db::RuntimeActionMode,
        request_hash: &str,
        proposed_child_thread_id: &str,
        child_project_authority: Option<&ryeos_state::objects::ExecutionProjectAuthority>,
        workspace_operation: &runtime_db::NewRuntimeWorkspaceOperation<'_>,
    ) -> Result<String> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let caller = g
            .state_db
            .get_thread(caller_thread_id)?
            .ok_or_else(|| anyhow!("runtime action caller {caller_thread_id} does not exist"))?;
        let _admission = g.state_db.authorize_runtime_pin(&caller.chain_root_id)?;
        let workspace = g
            .runtime_db
            .workspace(workspace_operation.workspace_id)?
            .ok_or_else(|| anyhow!("runtime action workspace is absent"))?;
        if workspace.thread_id.as_deref() != Some(caller_thread_id)
            || workspace.state != runtime_db::WorkspaceState::Active
        {
            bail!("runtime action workspace is not the caller's active workspace");
        }
        let session = g
            .runtime_db
            .dedicated_session(caller_thread_id)?
            .ok_or_else(|| anyhow!("workspace operation caller has no hosted session"))?;
        if session.chain_root_id != caller.chain_root_id
            || session.workspace_id != workspace_operation.workspace_id
            || session.worker_instance_id.as_deref() != Some(workspace_operation.worker_instance_id)
            || session.worker_boot_epoch != Some(workspace_operation.worker_boot_epoch)
            || !matches!(
                session.state.as_str(),
                "idle" | "turn_running" | "awaiting_approval"
            )
        {
            bail!("runtime workspace operation contradicts the current hosted placement");
        }
        if let ryeos_runtime::workload_client::WorkloadInvocationSource::StructuredSession {
            upstream_session_id,
            operation_id: upstream_operation_id,
            ..
        } = &workspace_operation.invocation
        {
            // Only a new callback requires the currently running turn. An
            // already-reserved exact action may replay its retained outcome;
            // the DB still compares its complete request/workspace authority.
            let retained = g.runtime_db.runtime_action_for_protocol_invocation(
                caller_thread_id,
                &workspace_operation.invocation,
            )?;
            if let Some(retained) = retained {
                if retained.operation_id != operation_id {
                    bail!("protocol occurrence already belongs to an earlier runtime action");
                }
            } else if session.remote_thread_id.as_ref() != Some(upstream_session_id)
                || session.current_turn_id.as_ref() != Some(upstream_operation_id)
                || !matches!(session.state.as_str(), "turn_running" | "awaiting_approval")
            {
                bail!("new workload callback does not target the current hosted turn");
            }
        }
        let worker = g
            .runtime_db
            .worker_process(workspace_operation.worker_instance_id)?
            .ok_or_else(|| anyhow!("runtime workspace operation worker is absent"))?;
        if worker.placement_thread_id != caller_thread_id
            || worker.boot_epoch != workspace_operation.worker_boot_epoch
            || worker.boot_identity_hash != workspace_operation.worker_boot_identity_hash
            || worker.daemon_generation_id != runtime_db::daemon_generation_id()
            || worker.session_capsule_hash != session.admitted_capsule_hash
            || worker.state != runtime_db::WorkerProcessState::Live
            || worker.cleanup_state != "owned"
        {
            bail!("runtime workspace operation worker boot is not current and live");
        }
        g.runtime_db.reserve_runtime_action_intent_with_workspace(
            operation_id,
            &caller.chain_root_id,
            caller_thread_id,
            mode,
            request_hash,
            proposed_child_thread_id,
            child_project_authority,
            Some(workspace_operation),
        )
    }

    pub fn runtime_action_for_protocol_invocation(
        &self,
        placement_thread_id: &str,
        source: &ryeos_runtime::workload_client::WorkloadInvocationSource,
    ) -> Result<Option<runtime_db::RuntimeActionIntent>> {
        self.lock()?
            .runtime_db
            .runtime_action_for_protocol_invocation(placement_thread_id, source)
    }

    pub fn runtime_action_intents(&self) -> Result<Vec<runtime_db::RuntimeActionIntent>> {
        self.lock()?.runtime_db.runtime_action_intents()
    }

    /// Retained workload-client dispatch summaries for one exact
    /// structured-session turn. Observation projection only: reads existing
    /// typed intents, mints nothing.
    pub fn workload_child_dispatches(
        &self,
        first_caller_thread_id: &str,
        upstream_session_id: &str,
        upstream_operation_id: &str,
    ) -> Result<Vec<runtime_db::WorkloadChildDispatch>> {
        self.lock()?.runtime_db.workload_child_dispatches(
            first_caller_thread_id,
            upstream_session_id,
            upstream_operation_id,
        )
    }

    /// Durable, non-released shared-workspace operations for one hosted root.
    /// This derives from RuntimeActionIntent; callers must not create a second
    /// lease registry for boot, freeze, handoff, or capability-mint fencing.
    pub fn active_runtime_workspace_operations_for_chain(
        &self,
        chain_root_id: &str,
    ) -> Result<Vec<runtime_db::RuntimeActionIntent>> {
        Ok(self
            .lock()?
            .runtime_db
            .runtime_action_intents()?
            .into_iter()
            .filter(|intent| {
                intent.chain_root_id == chain_root_id
                    && intent
                        .workspace_operation
                        .as_ref()
                        .is_some_and(|operation| {
                            operation.phase != runtime_db::RuntimeWorkspaceOperationPhase::Released
                        })
            })
            .collect())
    }

    pub fn assert_no_active_runtime_workspace_operation_for_chain(
        &self,
        chain_root_id: &str,
    ) -> Result<()> {
        let active = self.active_runtime_workspace_operations_for_chain(chain_root_id)?;
        if !active.is_empty() {
            bail!(
                "hosted execution root `{chain_root_id}` retains {} unsettled workspace operation(s)",
                active.len()
            );
        }
        Ok(())
    }

    /// Placement-local form used by the existing root terminalization gate.
    /// A continued chain's placement thread is deliberately not assumed to be
    /// equal to its stable chain root.
    pub fn assert_no_active_runtime_workspace_operation_for_placement(
        &self,
        placement_thread_id: &str,
    ) -> Result<()> {
        let active = self
            .lock()?
            .runtime_db
            .runtime_action_intents()?
            .into_iter()
            .filter(|intent| {
                intent.first_caller_thread_id == placement_thread_id
                    && intent
                        .workspace_operation
                        .as_ref()
                        .is_some_and(|operation| {
                            operation.phase != runtime_db::RuntimeWorkspaceOperationPhase::Released
                        })
            })
            .count();
        if active != 0 {
            bail!(
                "hosted execution placement `{placement_thread_id}` retains {active} unsettled workspace operation(s)"
            );
        }
        Ok(())
    }

    pub fn get_runtime_action_intent(
        &self,
        operation_id: &str,
    ) -> Result<Option<runtime_db::RuntimeActionIntent>> {
        self.lock()?
            .runtime_db
            .get_runtime_action_intent(operation_id)
    }

    pub fn get_runtime_action_intent_by_child(
        &self,
        child_thread_id: &str,
    ) -> Result<Option<runtime_db::RuntimeActionIntent>> {
        self.lock()?
            .runtime_db
            .get_runtime_action_intent_by_child(child_thread_id)
    }

    pub fn transition_runtime_workspace_operation(
        &self,
        operation_id: &str,
        expected: &[runtime_db::RuntimeWorkspaceOperationPhase],
        next: runtime_db::RuntimeWorkspaceOperationPhase,
    ) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .transition_runtime_workspace_operation(operation_id, expected, next)
    }

    pub fn bind_runtime_workspace_input_generation(
        &self,
        operation_id: &str,
        generation: &ryeos_state::objects::WorkspaceGenerationPair,
    ) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .bind_runtime_workspace_input_generation(operation_id, generation)
    }

    pub(super) fn runtime_workspace_subtree_is_settled_locked(
        g: &Inner,
        intent: &runtime_db::RuntimeActionIntent,
    ) -> Result<bool> {
        let operation = intent
            .workspace_operation
            .as_ref()
            .ok_or_else(|| anyhow!("runtime action has no workspace operation"))?;
        let workspace = g
            .runtime_db
            .workspace(&operation.workspace_id)?
            .ok_or_else(|| anyhow!("workspace operation lost its workspace owner"))?;
        if workspace.thread_id.as_deref() != Some(intent.first_caller_thread_id.as_str()) {
            bail!("workspace operation root owner changed before settlement");
        }
        let view_identity = workspace
            .mount_identity
            .as_deref()
            .ok_or_else(|| anyhow!("workspace operation lost its exact view incarnation"))?;
        Ok(!g.runtime_db.workspace_subtree_has_members(
            &intent.child_thread_id,
            &operation.workspace_id,
            view_identity,
        )?)
    }

    /// Release an operation only after existing thread/launcher authorities
    /// prove that its exact child is absent or terminal and process-detached,
    /// and every same-view descendant has explicitly settled membership.
    /// Signal delivery or terminal status alone is never reap evidence.
    pub fn begin_runtime_workspace_operation_settlement(&self, operation_id: &str) -> Result<bool> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let intent = g
            .runtime_db
            .get_runtime_action_intent(operation_id)?
            .ok_or_else(|| anyhow!("runtime workspace operation `{operation_id}` is absent"))?;
        let operation = intent
            .workspace_operation
            .as_ref()
            .ok_or_else(|| anyhow!("runtime action `{operation_id}` has no workspace operation"))?;
        if operation.phase == runtime_db::RuntimeWorkspaceOperationPhase::Settling
            || operation.phase == runtime_db::RuntimeWorkspaceOperationPhase::Released
        {
            return Ok(true);
        }
        if operation.phase != runtime_db::RuntimeWorkspaceOperationPhase::ChildRunning {
            bail!(
                "runtime workspace operation `{operation_id}` cannot settle from {:?}",
                operation.phase
            );
        }
        let child = g.state_db.get_thread(&intent.child_thread_id)?;
        let launch_claim = g.runtime_db.get_launch_claim(&intent.child_thread_id)?;
        let in_process = g
            .runtime_db
            .in_process_handler_reservation(&intent.child_thread_id)?;
        let process_attached = g
            .runtime_db
            .get_runtime_info(&intent.child_thread_id)?
            .and_then(|runtime| runtime.process_identity)
            .is_some();
        let settled = match child.as_ref() {
            None => launch_claim.is_none() && in_process.is_none() && !process_attached,
            Some(child) => {
                is_terminal_status(&child.status)
                    && launch_claim.is_none()
                    && in_process.is_none()
                    && !process_attached
            }
        };
        if !settled || !Self::runtime_workspace_subtree_is_settled_locked(&g, &intent)? {
            return Ok(false);
        }
        g.runtime_db.transition_runtime_workspace_operation(
            operation_id,
            &[runtime_db::RuntimeWorkspaceOperationPhase::ChildRunning],
            runtime_db::RuntimeWorkspaceOperationPhase::Settling,
        )?;
        Ok(true)
    }

    /// Release an operation only after existing thread/launcher authorities
    /// prove that its exact child is absent or terminal and process-detached,
    /// and every same-view descendant has explicitly settled membership.
    /// Signal delivery or terminal status alone is never reap evidence.
    pub fn settle_runtime_workspace_operation(&self, operation_id: &str) -> Result<bool> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let Some(intent) = g.runtime_db.get_runtime_action_intent(operation_id)? else {
            bail!("runtime workspace operation `{operation_id}` is absent");
        };
        let Some(operation) = intent.workspace_operation.as_ref() else {
            bail!("runtime action `{operation_id}` has no workspace operation");
        };
        if operation.phase == runtime_db::RuntimeWorkspaceOperationPhase::Released {
            return Ok(true);
        }
        let root_worker = g
            .runtime_db
            .worker_process(&operation.worker_instance_id)?
            .ok_or_else(|| {
                anyhow!(
                    "runtime workspace operation `{operation_id}` lost its exact root-worker boot"
                )
            })?;
        let exact_root_boot = root_worker.placement_thread_id == intent.first_caller_thread_id
            && root_worker.boot_epoch == operation.worker_boot_epoch
            && root_worker.boot_identity_hash == operation.worker_boot_identity_hash;
        let current_live_root = exact_root_boot
            && root_worker.daemon_generation_id == runtime_db::daemon_generation_id()
            && root_worker.state == runtime_db::WorkerProcessState::Live
            && root_worker.cleanup_state == "owned";
        let reaped_root = exact_root_boot
            && root_worker.state == runtime_db::WorkerProcessState::Dead
            && root_worker.cleanup_state == "reaped";
        if !current_live_root && !reaped_root {
            // Startup recovery must not release this barrier while the old
            // hosted worker may still own the workspace. A current callback
            // may release only while it retains the exact current boot; a
            // later daemon requires the existing worker-cleanup authority's
            // durable death proof.
            return Ok(false);
        }
        let child = g.state_db.get_thread(&intent.child_thread_id)?;
        let launch_claim = g.runtime_db.get_launch_claim(&intent.child_thread_id)?;
        let in_process = g
            .runtime_db
            .in_process_handler_reservation(&intent.child_thread_id)?;
        let process_attached = g
            .runtime_db
            .get_runtime_info(&intent.child_thread_id)?
            .and_then(|runtime| runtime.process_identity)
            .is_some();
        let settled = match child.as_ref() {
            None => launch_claim.is_none() && in_process.is_none() && !process_attached,
            Some(child) => {
                is_terminal_status(&child.status)
                    && launch_claim.is_none()
                    && in_process.is_none()
                    && !process_attached
            }
        };
        if !settled || !Self::runtime_workspace_subtree_is_settled_locked(&g, &intent)? {
            return Ok(false);
        }
        if operation.phase == runtime_db::RuntimeWorkspaceOperationPhase::ChildRunning {
            g.runtime_db.transition_runtime_workspace_operation(
                operation_id,
                &[runtime_db::RuntimeWorkspaceOperationPhase::ChildRunning],
                runtime_db::RuntimeWorkspaceOperationPhase::Settling,
            )?;
        }
        let refreshed = g
            .runtime_db
            .get_runtime_action_intent(operation_id)?
            .and_then(|intent| intent.workspace_operation)
            .ok_or_else(|| anyhow!("runtime workspace operation disappeared during settlement"))?;
        g.runtime_db.transition_runtime_workspace_operation(
            operation_id,
            &[refreshed.phase],
            runtime_db::RuntimeWorkspaceOperationPhase::Released,
        )?;
        Ok(true)
    }

    pub fn bind_root_action_project_authority(
        &self,
        operation_id: &str,
        child_project_authority: &ryeos_state::objects::ExecutionProjectAuthority,
    ) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let result = g
            .runtime_db
            .bind_root_action_project_authority(operation_id, child_project_authority);
        drop(g);
        result
    }

    pub fn seal_root_action_intent(
        &self,
        operation_id: &str,
        child_project_authority: &ryeos_state::objects::ExecutionProjectAuthority,
        launch_metadata: &crate::launch_metadata::RuntimeLaunchMetadata,
        initial_events: &[NewEventRecord],
    ) -> Result<()> {
        let permit = self.acquire_write_permit()?;
        let capsule = launch_metadata
            .admitted_launch_capsule()?
            .ok_or_else(|| anyhow!("root action cannot seal without an admitted capsule"))?;
        if &capsule.project_authority != child_project_authority {
            bail!("root action capsule contradicts selected child project authority");
        }
        capsule
            .verify_retained_execution_realization(
                &self.state_authority.cas_store()?,
                &self.state_authority.large_object_store()?,
                self.state_authority.trust_store(),
            )
            .context("verify root action admitted execution realization")?;
        self.state_authority.ensure_guard(permit.cas_guard())?;
        let canonical_capsule = capsule.to_canonical_bytes()?;
        let expected_capsule_hash = lillux::sha256_hex(&canonical_capsule);
        let admitted_launch_capsule_hash = self
            .state_authority
            .cas_store()?
            .store_canonical_object_bytes_bounded(
                &canonical_capsule,
                ryeos_state::objects::MAX_ADMITTED_LAUNCH_CAPSULE_BYTES,
            )
            .context("store detached admitted launch capsule")?;
        if admitted_launch_capsule_hash != expected_capsule_hash {
            bail!(
                "detached admitted capsule hash mismatch: expected {expected_capsule_hash}, stored {admitted_launch_capsule_hash}"
            );
        }
        let g = self.lock()?;
        let result = g.runtime_db.seal_root_action_intent(
            operation_id,
            child_project_authority,
            &admitted_launch_capsule_hash,
            launch_metadata,
            initial_events,
        );
        drop(g);
        result
    }

    // ── Follow waiters ───────────────────────────────────────────────────

    pub fn reserve_follow(
        &self,
        seed: &runtime_db::NewFollowWaiter,
    ) -> Result<runtime_db::FollowWaiter> {
        validate_follow_reservation_shape(seed)?;
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let _admission = g
            .state_db
            .authorize_runtime_pin(&seed.parent_chain_root_id)?;
        g.runtime_db.reserve_follow(seed)
    }
}
