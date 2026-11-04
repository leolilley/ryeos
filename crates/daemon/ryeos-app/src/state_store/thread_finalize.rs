//! Marking threads running and finalizing them.
use super::*;

impl StateStore {
    /// Atomically append launch-attempt audit and cross a created thread into
    /// `running`. Existing running recovery attempts append only their new
    /// audit; they never emit a duplicate `thread_started` event.
    pub fn mark_thread_running_with_events(
        &self,
        thread_id: &str,
        base_project_snapshot_hash: Option<&str>,
        mut initial_events: Vec<NewEventRecord>,
    ) -> Result<Vec<PersistedEventRecord>> {
        let permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let thread_row = g
            .state_db
            .get_thread(thread_id)?
            .ok_or_else(|| anyhow!("thread not found: {thread_id}"))?;
        if !self
            .process_attachment_admission_open
            .load(Ordering::Acquire)
        {
            bail!("mark_running is fenced during daemon shutdown");
        }
        let runtime = g
            .runtime_db
            .get_runtime_info(thread_id)?
            .ok_or_else(|| anyhow!("runtime row missing while marking running: {thread_id}"))?;
        if let Some(intent) = runtime.stop_intent {
            bail!(
                "mark_running is fenced after {} request for thread {thread_id}",
                intent.as_str()
            );
        }
        let authoritative_snapshot = authoritative_snapshot_for_transition(
            &g,
            &thread_row.chain_root_id,
            &thread_row.thread_id,
        )?;
        verify_admitted_launch_capsule(
            &self.state_authority,
            &self.bundle_admission_trust,
            &authoritative_snapshot,
            runtime.launch_metadata.as_ref(),
        )?;

        let transition_created = match thread_row.status.as_str() {
            // Fresh launch: fall through to the created -> running transition
            // (appends `thread_started`, sets `started_at`).
            "created" => true,
            // Same-thread crash recovery re-spawns a row that is still `running`,
            // and the resumed runtime calls `mark_running` again. Idempotent
            // no-op: do NOT append a second `thread_started` or rewrite
            // `started_at` — an empty persisted-events list means "already
            // running". (`drain_running_threads` still sees `running`, so the
            // shutdown kill window stays intact — no transient non-running state.)
            "running" if initial_events.is_empty() => return Ok(Vec::new()),
            "running" => false,
            other => {
                bail!("invalid status transition: {other} -> running");
            }
        };

        let snapshot_updates = if transition_created {
            let mut updated_snapshot = authoritative_snapshot;
            let authoritative_base = updated_snapshot
                .project_authority
                .operational_snapshot_projection()
                .map(str::to_owned);
            if let Some(requested_base) = base_project_snapshot_hash
                && Some(requested_base) != authoritative_base.as_deref()
            {
                bail!(
                    "mark_running project snapshot mismatch for {thread_id}: authoritative {:?}, requested {:?}",
                    authoritative_base,
                    requested_base,
                );
            }

            let now = lillux::time::iso8601_now();
            updated_snapshot.status = ThreadStatus::Running;
            updated_snapshot.updated_at.clone_from(&now);
            updated_snapshot.started_at = Some(now);
            updated_snapshot.finished_at = None;
            updated_snapshot.base_project_snapshot_hash = authoritative_base;
            initial_events.push(NewEventRecord {
                event_type: "thread_started".to_string(),
                storage_class: "indexed".to_string(),
                payload: json!({}),
            });
            vec![SnapshotUpdate {
                thread_id: thread_id.to_string(),
                new_snapshot: updated_snapshot,
            }]
        } else {
            Vec::new()
        };

        let te = convert_events(&initial_events, &thread_row.chain_root_id, thread_id);
        let result = committed_value(g.state_db.append_events_admitted(
            &thread_row.chain_root_id,
            thread_id,
            te,
            snapshot_updates,
            g.signer.as_ref(),
            &g.runtime_db,
            permit.cas_guard(),
        )?);

        persisted_from_append(&result, &initial_events)
    }

    #[tracing::instrument(
        name = "state:finalize_thread",
        skip(self, update),
        fields(thread_id = %thread_id, status = %update.status)
    )]
    pub fn finalize_thread(
        &self,
        thread_id: &str,
        update: &FinalizeThreadRecord,
    ) -> Result<Vec<PersistedEventRecord>> {
        self.finalize_thread_locked(thread_id, update)
            .map(|(events, _)| events)
    }

    /// Generic lifecycle finalization that also returns the effective record
    /// after global durable-stop dominance. Higher layers must use this form so
    /// scheduler/command/follow side effects match the persisted terminal event.
    pub fn finalize_thread_effective(
        &self,
        thread_id: &str,
        update: &FinalizeThreadRecord,
    ) -> Result<(Vec<PersistedEventRecord>, FinalizeThreadRecord)> {
        self.finalize_thread_locked(thread_id, update)
    }

    pub fn finalize_thread_effective_owned(
        &self,
        thread_id: &str,
        launch_owner: &str,
        update: &FinalizeThreadRecord,
    ) -> Result<(Vec<PersistedEventRecord>, FinalizeThreadRecord)> {
        let permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let claim = g
            .runtime_db
            .get_launch_claim(thread_id)?
            .ok_or_else(|| anyhow::anyhow!("thread {thread_id} has no launch owner"))?;
        if claim.claimed_by != launch_owner {
            anyhow::bail!("stale launch owner cannot finalize thread {thread_id}");
        }
        self.finalize_thread_with_guard(
            &g,
            permit.cas_guard(),
            thread_id,
            update,
            false,
            Some(&claim.owner),
        )
    }

    pub(super) fn finalize_thread_locked(
        &self,
        thread_id: &str,
        update: &FinalizeThreadRecord,
    ) -> Result<(Vec<PersistedEventRecord>, FinalizeThreadRecord)> {
        let permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        self.finalize_thread_with_guard(&g, permit.cas_guard(), thread_id, update, false, None)
    }

    /// Atomically finalize a child-link failure only while the child is still a
    /// never-launched row. This conditional transition is deliberately allowed
    /// after shutdown admission closes: unlike a generic finalizer, the guarded
    /// row has no process for shutdown to own and no launcher entitled to attach
    /// one. A durable stop still dominates the requested failure outcome.
    pub fn finalize_created_unattached_if_current(
        &self,
        thread_id: &str,
        update: &FinalizeThreadRecord,
    ) -> Result<FinalizeCreatedUnattachedOutcome> {
        let permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let thread_row = g
            .state_db
            .get_thread(thread_id)?
            .ok_or_else(|| anyhow!("thread not found: {thread_id}"))?;

        if is_terminal_status(&thread_row.status) {
            return Ok(FinalizeCreatedUnattachedOutcome::AlreadyTerminal);
        }

        // Birth can commit before the runtime row is installed. Absence is an
        // exact unattached state for this conditional transition, not an
        // error; the launch-claim check below still fences a concurrent owner.
        let runtime = g
            .runtime_db
            .get_runtime_info(thread_id)?
            .unwrap_or_default();
        let process_attached =
            runtime.pid.is_some() || runtime.pgid.is_some() || runtime.process_identity.is_some();
        let launch_claimed = g.runtime_db.get_launch_claim(thread_id)?.is_some();
        if thread_row.status != ThreadStatus::Created.as_str() || process_attached || launch_claimed
        {
            return Ok(FinalizeCreatedUnattachedOutcome::NotCurrent {
                status: thread_row.status,
                process_attached,
                launch_claimed,
            });
        }

        let (persisted, effective) = self.finalize_thread_with_rows(
            &g,
            permit.cas_guard(),
            thread_id,
            thread_row,
            runtime,
            update,
            true,
            None,
        )?;
        Ok(FinalizeCreatedUnattachedOutcome::Finalized {
            persisted,
            effective: Box::new(effective),
        })
    }

    /// Atomically finalize a nonterminal row, or report the terminal/shutdown
    /// winner without a check-then-write race. A durable Cancel/Kill tombstone
    /// is folded into the effective terminal record by
    /// `Self::finalize_thread_with_rows`.
    pub fn finalize_if_nonterminal(
        &self,
        thread_id: &str,
        update: &FinalizeThreadRecord,
    ) -> Result<FinalizeIfNonterminalOutcome> {
        let permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let thread_row = g
            .state_db
            .get_thread(thread_id)?
            .ok_or_else(|| anyhow!("thread not found: {thread_id}"))?;
        if is_terminal_status(&thread_row.status) {
            return Ok(FinalizeIfNonterminalOutcome::AlreadyTerminal {
                status: thread_row.status,
            });
        }
        let runtime = g
            .runtime_db
            .get_runtime_info(thread_id)?
            .ok_or_else(|| anyhow!("runtime row missing during finalization: {thread_id}"))?;
        if runtime.stop_intent.is_none()
            && !self
                .process_attachment_admission_open
                .load(Ordering::Acquire)
        {
            g.runtime_db.reset_resume_attempts(thread_id)?;
            return Ok(FinalizeIfNonterminalOutcome::PreservedForShutdown);
        }
        let (persisted, effective) = self.finalize_thread_with_rows(
            &g,
            permit.cas_guard(),
            thread_id,
            thread_row,
            runtime,
            update,
            false,
            None,
        )?;
        Ok(FinalizeIfNonterminalOutcome::Finalized {
            persisted,
            effective: Box::new(effective),
        })
    }

    /// Reservation-qualified no-replay finalization driven exclusively by the
    /// authoritative root snapshot. Replaceable projection status must not
    /// decide whether an ownerless in-process service remains running.
    ///
    /// `None` means a live owner appeared before the mutation lock and retains
    /// terminal authority.
    pub fn finalize_ownerless_in_process_handler_if_nonterminal(
        &self,
        thread_id: &str,
        update: &FinalizeThreadRecord,
    ) -> Result<Option<FinalizeIfNonterminalOutcome>> {
        let permit = self.acquire_write_permit()?;
        let active = self
            .active_in_process_handlers
            .lock()
            .map_err(|_| anyhow!("active in-process-handler registry poisoned"))?;
        if active.contains_key(thread_id) {
            return Ok(None);
        }
        let g = self.lock()?;
        let reservation = g
            .runtime_db
            .in_process_handler_reservation(thread_id)?
            .ok_or_else(|| {
                anyhow!("ownerless in-process handler {thread_id} has no durable reservation")
            })?;
        let chain_root_id = in_process_handler_chain_root_locked(&g, thread_id)?;
        if reservation.phase == runtime_db::InProcessHandlerReservationPhase::TerminalConfirmed {
            let snapshot = g
                .state_db
                .read_authoritative_thread_snapshot(&chain_root_id, thread_id)?
                .ok_or_else(|| {
                    anyhow!(
                        "terminal-confirmed in-process handler {thread_id} has no authoritative thread"
                    )
                })?;
            if !snapshot.status.is_terminal() {
                bail!(
                    "terminal-confirmed in-process handler {thread_id} contradicts authoritative status {}",
                    snapshot.status.as_str()
                );
            }
            return Ok(Some(FinalizeIfNonterminalOutcome::AlreadyTerminal {
                status: snapshot.status.as_str().to_string(),
            }));
        }
        let snapshot = g
            .state_db
            .read_authoritative_thread_snapshot(&chain_root_id, thread_id)?
            .ok_or_else(|| {
                anyhow!("ownerless in-process handler {thread_id} has no authoritative thread")
            })?;
        if snapshot.status.is_terminal() {
            return Ok(Some(FinalizeIfNonterminalOutcome::AlreadyTerminal {
                status: snapshot.status.as_str().to_string(),
            }));
        }
        if snapshot.status != ThreadStatus::Running {
            bail!(
                "ownerless in-process handler {thread_id} has authoritative status {} instead of running",
                snapshot.status.as_str()
            );
        }
        let runtime = g
            .runtime_db
            .get_runtime_info(thread_id)?
            .ok_or_else(|| anyhow!("runtime row missing during finalization: {thread_id}"))?;
        let metadata = runtime
            .launch_metadata
            .as_ref()
            .ok_or_else(|| anyhow!("ownerless in-process handler has no launch metadata"))?;
        if metadata.launch_driver
            != Some(ryeos_state::objects::ExecutionLaunchDriver::InProcessHandler)
            || metadata.lifecycle_authority()?
                != Some(ryeos_state::objects::ExecutionLifecycleAuthority::DAEMON_NON_RECOVERABLE)
        {
            bail!("ownerless in-process handler runtime authority is inconsistent");
        }
        let thread_row = queries::ThreadRow {
            thread_id: snapshot.thread_id.clone(),
            chain_root_id: snapshot.chain_root_id.clone(),
            kind: snapshot.kind_name.clone(),
            status: snapshot.status.as_str().to_string(),
            item_ref: snapshot.item_ref.clone(),
            executor_ref: snapshot.executor_ref.clone(),
            launch_mode: snapshot.launch_mode.clone(),
            current_site_id: snapshot.current_site_id.clone(),
            origin_site_id: snapshot.origin_site_id.clone(),
            upstream_thread_id: snapshot.upstream_thread_id.clone(),
            requested_by: snapshot.requested_by.clone(),
            project_root: snapshot
                .project_root
                .as_ref()
                .map(|path| path.display().to_string()),
            project_authority: snapshot.project_authority.clone(),
            admitted_launch_capsule_hash: snapshot.admitted_launch_capsule_hash.clone(),
            base_project_snapshot_hash: snapshot.base_project_snapshot_hash.clone(),
            result_project_snapshot_hash: snapshot.result_project_snapshot_hash.clone(),
            result_workspace_output_capture_hash: snapshot
                .result_workspace_output_capture_hash
                .clone(),
            captured_history_policy: snapshot.captured_history_policy.clone(),
            created_at: snapshot.created_at.clone(),
            updated_at: snapshot.updated_at.clone(),
            started_at: snapshot.started_at.clone(),
            finished_at: snapshot.finished_at.clone(),
        };
        let (persisted, effective) = self.finalize_thread_with_rows(
            &g,
            permit.cas_guard(),
            thread_id,
            thread_row,
            runtime,
            update,
            false,
            None,
        )?;
        Ok(Some(FinalizeIfNonterminalOutcome::Finalized {
            persisted,
            effective: Box::new(effective),
        }))
    }

    /// Owner-qualified form of [`Self::finalize_if_nonterminal`] for daemon
    /// supervised method/runtime completion. The terminal check, exact owner
    /// comparison, stop dominance, and winning write share one StateStore lock.
    pub fn finalize_if_nonterminal_owned(
        &self,
        thread_id: &str,
        launch_owner: &str,
        update: &FinalizeThreadRecord,
    ) -> Result<FinalizeIfNonterminalOutcome> {
        let permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let thread_row = g
            .state_db
            .get_thread(thread_id)?
            .ok_or_else(|| anyhow!("thread not found: {thread_id}"))?;
        if is_terminal_status(&thread_row.status) {
            return Ok(FinalizeIfNonterminalOutcome::AlreadyTerminal {
                status: thread_row.status,
            });
        }
        let claim = g
            .runtime_db
            .get_launch_claim(thread_id)?
            .ok_or_else(|| anyhow!("thread {thread_id} has no launch owner"))?;
        if claim.claimed_by != launch_owner {
            bail!("stale launch owner cannot finalize thread {thread_id}");
        }
        let runtime = g
            .runtime_db
            .get_runtime_info(thread_id)?
            .ok_or_else(|| anyhow!("runtime row missing during finalization: {thread_id}"))?;
        if runtime.stop_intent.is_none()
            && !self
                .process_attachment_admission_open
                .load(Ordering::Acquire)
        {
            g.runtime_db.reset_resume_attempts(thread_id)?;
            return Ok(FinalizeIfNonterminalOutcome::PreservedForShutdown);
        }
        let (persisted, effective) = self.finalize_thread_with_rows(
            &g,
            permit.cas_guard(),
            thread_id,
            thread_row,
            runtime,
            update,
            false,
            Some(&claim.owner),
        )?;
        Ok(FinalizeIfNonterminalOutcome::Finalized {
            persisted,
            effective: Box::new(effective),
        })
    }

    pub(super) fn finalize_thread_with_guard(
        &self,
        g: &Inner,
        cas_mutation_guard: &ryeos_state::CasMutationGuard,
        thread_id: &str,
        update: &FinalizeThreadRecord,
        allow_closed_admission: bool,
        owned_launch_owner: Option<&runtime_db::LaunchOwner>,
    ) -> Result<(Vec<PersistedEventRecord>, FinalizeThreadRecord)> {
        let thread_row = g
            .state_db
            .get_thread(thread_id)?
            .ok_or_else(|| anyhow!("thread not found: {thread_id}"))?;
        let runtime = g
            .runtime_db
            .get_runtime_info(thread_id)?
            .ok_or_else(|| anyhow!("runtime row missing during finalization: {thread_id}"))?;
        self.finalize_thread_with_rows(
            g,
            cas_mutation_guard,
            thread_id,
            thread_row,
            runtime,
            update,
            allow_closed_admission,
            owned_launch_owner,
        )
    }

    /// This runs inside the existing StateStore writer exclusion, retained
    /// through the signed terminal CAS commit below. Every production runtime
    /// mutation uses the same private Inner guard; a returned output projection
    /// is not a reusable finalization capability outside this critical section.
    pub(super) fn validate_external_direct_finalization_locked(
        &self,
        inner: &Inner,
        thread_id: &str,
        chain_root_id: &str,
        update: &FinalizeThreadRecord,
    ) -> Result<()> {
        use runtime_db::external_execution::ExternalAllocationOwner;
        let snapshot = authoritative_snapshot_for_transition(inner, chain_root_id, thread_id)?;
        let capsule = snapshot
            .admitted_launch_capsule_hash
            .as_deref()
            .map(|hash| {
                load_admitted_launch_capsule(
                    &self.state_authority,
                    &self.bundle_admission_trust,
                    hash,
                )
            })
            .transpose()?;
        let is_external = capsule
            .as_ref()
            .map(crate::thread_lifecycle::capsule_requires_external_direct)
            .transpose()?
            .unwrap_or(false);
        let allocation = inner.runtime_db.external_allocation(thread_id)?;
        if !is_external {
            anyhow::ensure!(
                !allocation.as_ref().is_some_and(|allocation| matches!(
                    allocation.reservation.owner,
                    ExternalAllocationOwner::DirectThread { .. }
                )),
                "external direct allocation has no authoritative external capsule"
            );
            return Ok(());
        }
        anyhow::ensure!(
            update.status != ThreadStatus::Continued.as_str(),
            "one-shot external direct execution cannot author a continuation"
        );
        anyhow::ensure!(
            update.result_project_snapshot_hash.is_none()
                && update.result_workspace_output_capture_hash.is_none()
                && update.managed_envelope.is_none()
                && update.artifacts.is_empty()
                && update.final_cost.is_none(),
            "external direct execution cannot add unadmitted artifacts, cost or project publication"
        );
        // A substrate/administrative failure may settle the thread without
        // inventing a target result. The external cleanup obligation remains in
        // its existing journal; a null local PID is never its death proof.
        if update.status != ThreadStatus::Completed.as_str() && update.result_json.is_none() {
            return Ok(());
        }
        let allocation = allocation.context("external direct result has no allocation evidence")?;
        let ExternalAllocationOwner::DirectThread {
            chain_root_id: retained_root,
            program,
            ..
        } = &allocation.reservation.owner
        else {
            bail!("external direct result has a different allocation owner");
        };
        anyhow::ensure!(
            snapshot.thread_id == thread_id
                && snapshot.chain_root_id == *retained_root
                && snapshot.admitted_launch_capsule_hash.as_deref()
                    == Some(allocation.reservation.admitted_capsule_hash.as_str()),
            "external direct finalization changed its authoritative born capsule"
        );
        let capsule = capsule.context("external direct result lost its authoritative capsule")?;
        let endpoint =
            crate::thread_lifecycle::validate_retained_external_direct_program(&capsule, program)?;
        anyhow::ensure!(
            capsule.project_authority == snapshot.project_authority
                && capsule.executor_ref == snapshot.executor_ref
                && endpoint.binding_digest == allocation.reservation.binding_hash,
            "external direct finalization changed its admitted program or project authority"
        );
        anyhow::ensure!(
            matches!(&capsule.project_authority,
                ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
                    snapshot_hash,
                    realization: ryeos_state::objects::PinnedProjectRealization::ReadOnly,
                    environment: ryeos_state::objects::EnvironmentAuthority::None,
                    workspace_outputs: None, ..
                } if snapshot_hash == &allocation.reservation.base_snapshot_hash),
            "external direct finalization lost its immutable admitted generation"
        );
        let claim = inner
            .runtime_db
            .get_launch_claim(thread_id)?
            .context("external direct finalization has no current settlement owner")?;
        let output = inner
            .runtime_db
            .external_direct_finalization_output(thread_id, &claim.claimed_by)?;
        validate_external_direct_result(update, &output)
    }

    pub(super) fn finalize_thread_with_rows(
        &self,
        g: &Inner,
        cas_mutation_guard: &ryeos_state::CasMutationGuard,
        thread_id: &str,
        thread_row: queries::ThreadRow,
        runtime: RuntimeInfo,
        update: &FinalizeThreadRecord,
        allow_closed_admission: bool,
        owned_launch_owner: Option<&runtime_db::LaunchOwner>,
    ) -> Result<(Vec<PersistedEventRecord>, FinalizeThreadRecord)> {
        if g.runtime_db
            .has_unsettled_scoped_child_for_thread(thread_id)?
        {
            bail!("thread finalization requires exact scoped child retirement");
        }
        let validated_final_cost = update
            .final_cost
            .as_ref()
            .map(validate_final_cost)
            .transpose()?;
        let mut effective_update = update.clone();
        // Stop intent dominates every later finalizer, including administrative
        // failure nets. This is intentionally global: a check-then-finalize
        // caller must not be able to overwrite a stop that committed between
        // its check and this lock acquisition.
        if let Some(intent) = runtime.stop_intent {
            let status = match intent {
                StopIntent::Cancel => ThreadStatus::Cancelled,
                StopIntent::Kill => ThreadStatus::Killed,
            };
            effective_update.status = status.as_str().to_string();
            effective_update.outcome_code = Some(status.as_str().to_string());
            effective_update.result_json = None;
            effective_update.error_json = Some(json!({
                "reason": "durable_stop_intent",
                "intent": intent.as_str(),
            }));
            // The runtime supplied envelope describes its reported outcome,
            // not the daemon-owned durable-stop winner above. Never sign that
            // contradictory process claim into the effective terminal
            // snapshot. Incurred cost remains authoritative and is retained.
            effective_update.managed_envelope = None;
            effective_update.result_project_snapshot_hash = None;
            effective_update.result_workspace_output_capture_hash = None;
        } else if !allow_closed_admission
            && !self
                .process_attachment_admission_open
                .load(Ordering::Acquire)
        {
            // Every terminal writer shares the shutdown fence. Otherwise an
            // execution-result fallback could turn a shutdown-owned kill into a
            // terminal failure after drain had taken ownership of the process.
            // A durable stop remains the one exception: it must be allowed to
            // settle to its dominant cancelled/killed outcome while draining.
            bail!("thread finalization is fenced during daemon shutdown");
        }
        let update = &effective_update;

        if is_terminal_status(&thread_row.status) {
            bail!(
                "invalid status transition: {} -> {}",
                thread_row.status,
                update.status
            );
        }

        let terminal_status = ThreadStatus::from_str_lossy(&update.status)
            .ok_or_else(|| anyhow!("invalid terminal status: {}", update.status))?;
        if !terminal_status.is_terminal() {
            bail!("finalize_thread requires a terminal status");
        }
        self.validate_external_direct_finalization_locked(
            g,
            thread_id,
            &thread_row.chain_root_id,
            update,
        )?;
        if let Some(cost) = update.final_cost.as_ref() {
            validate_final_cost_for_settlement(cost)?;
        }
        let (result_size_bytes, error_size_bytes) =
            ryeos_state::objects::validate_thread_result_content(
                update.result_json.as_ref(),
                update.error_json.as_ref(),
            )?;
        if let Some(envelope) = update.managed_envelope.as_ref() {
            validate_managed_terminal_envelope(
                envelope,
                thread_id,
                terminal_status,
                update.result_json.as_ref(),
                update.error_json.as_ref(),
                update.final_cost.as_ref(),
            )?;
        }

        let (additional_artifact_kind_bytes, additional_artifact_metadata_bytes) =
            update.artifacts.iter().try_fold(
                (0usize, 0usize),
                |(kind_total, metadata_total), artifact| {
                    let metadata_bytes = validate_new_artifact_shape(
                        &artifact.artifact_type,
                        artifact.metadata.as_ref(),
                    )?;
                    Ok::<_, anyhow::Error>((
                        kind_total
                            .checked_add(artifact.artifact_type.len())
                            .ok_or_else(|| anyhow!("terminal artifact kind byte total overflow"))?,
                        metadata_total.checked_add(metadata_bytes).ok_or_else(|| {
                            anyhow!("terminal artifact metadata byte total overflow")
                        })?,
                    ))
                },
            )?;
        if !update.artifacts.is_empty() {
            if !self.projection_health.is_current() {
                bail!("artifact admission requires a current thread projection");
            }
            ensure_artifact_projection_capacity(
                g,
                thread_id,
                update.artifacts.len(),
                additional_artifact_kind_bytes,
                additional_artifact_metadata_bytes,
            )?;
        }

        let now = lillux::time::iso8601_now();
        let facets = terminal_facets(update.final_cost.as_ref())?;
        let managed_runtime_terminal =
            managed_runtime_terminal_supplement(update.managed_envelope.as_ref())?;

        let artifacts_json: Vec<Value> = update
            .artifacts
            .iter()
            .map(|a| serde_json::to_value(a).unwrap())
            .collect();

        let mut updated_snapshot = authoritative_snapshot_for_transition(
            g,
            &thread_row.chain_root_id,
            &thread_row.thread_id,
        )?;
        let usage_started_at = updated_snapshot
            .started_at
            .clone()
            .unwrap_or_else(|| updated_snapshot.created_at.clone());
        updated_snapshot.status = terminal_status;
        updated_snapshot.updated_at.clone_from(&now);
        updated_snapshot.finished_at = Some(now.clone());
        updated_snapshot.result.clone_from(&update.result_json);
        updated_snapshot
            .outcome_code
            .clone_from(&update.outcome_code);
        updated_snapshot.error.clone_from(&update.error_json);
        updated_snapshot.budget = validated_final_cost.as_ref().map(|cost| {
            ThreadUsage {
                completed_turns: cost.completed_turns,
                input_tokens: cost.input_tokens,
                output_tokens: cost.output_tokens,
                spend_usd: cost.spend_usd,
                spawns_used: 0, // not tracked in FinalCost
                started_at: usage_started_at.clone(),
                settled_at: now.clone(),
                last_settled_turn_seq: u64::from(cost.completed_turns),
                elapsed_ms: 0, // daemon doesn't track wall-clock time
                provider_id: None,
                model: None,
                profile: None,
            }
        });
        updated_snapshot.artifacts = artifacts_json;
        updated_snapshot.managed_runtime_terminal = managed_runtime_terminal;
        updated_snapshot.facets = facets;
        updated_snapshot
            .result_project_snapshot_hash
            .clone_from(&update.result_project_snapshot_hash);
        updated_snapshot
            .result_workspace_output_capture_hash
            .clone_from(&update.result_workspace_output_capture_hash);
        updated_snapshot
            .verify_result_workspace_output_capture(&self.state_authority.cas_store()?)?;

        let snapshot_update = SnapshotUpdate {
            thread_id: thread_id.to_string(),
            new_snapshot: updated_snapshot,
        };

        let mut events_to_append = Vec::new();

        for artifact in &update.artifacts {
            events_to_append.push(NewEventRecord {
                event_type: "artifact_published".to_string(),
                storage_class: "indexed".to_string(),
                payload: json!({
                    "artifact_type": artifact.artifact_type,
                    "uri": artifact.uri,
                    "content_hash": artifact.content_hash,
                    "metadata": artifact.metadata,
                }),
            });
        }

        // A terminal event is a bounded lifecycle fact, not a second result
        // store. The complete result/error remain authoritative in the thread
        // snapshot committed by this same head transition. Hashes let an event
        // consumer correlate those values without embedding them again.
        let mut terminal_payload = json!({
            "result_project_snapshot_hash": update.result_project_snapshot_hash,
            "result_workspace_output_capture_hash": update.result_workspace_output_capture_hash,
            "outcome_code": update.outcome_code,
            "result_present": update.result_json.is_some(),
            "result_size_bytes": result_size_bytes,
            "result_digest": terminal_value_digest(update.result_json.as_ref(), "result")?,
            "has_error": update.error_json.is_some(),
            "error_size_bytes": error_size_bytes,
            "error_digest": terminal_value_digest(update.error_json.as_ref(), "error")?,
            "artifact_count": update.artifacts.len(),
        });
        // This reference is daemon-derived under the same launch-owner lock
        // as the signed terminal. The operational slot alone is mutable and
        // must never qualify a later attempt; an unowned or non-completed
        // finalizer cannot mint this bridge.
        if terminal_status == ThreadStatus::Completed
            && runtime.process_identity.is_none()
            && let Some(owner) = owned_launch_owner
            && let Some(settlement) = g.runtime_db.latest_thread_process_settlement(thread_id)?
            && settlement.launch_owner == *owner
            && matches!(
                settlement.kind,
                runtime_db::ThreadProcessSettlementKind::ReapedScopeEmpty
                    | runtime_db::ThreadProcessSettlementKind::ReapedGroupAbsent
            )
        {
            terminal_payload["process_settlement_witness_digest"] =
                Value::String(settlement.digest()?);
        }
        if let Some(err) = &update.error_json
            && let Some(map) = terminal_payload.as_object_mut()
        {
            if error_size_bytes <= MAX_TERMINAL_EVENT_INLINE_ERROR_BYTES {
                map.insert("error".to_string(), err.clone());
            } else {
                map.insert("error_omitted".to_string(), Value::Bool(true));
                map.insert(
                    "error_preview".to_string(),
                    Value::String(
                        "terminal error exceeds the event inline limit; inspect the thread for the full error"
                            .to_string(),
                    ),
                );
            }
        }
        events_to_append.push(NewEventRecord {
            event_type: terminal_event_type(&update.status)?.to_string(),
            storage_class: "indexed".to_string(),
            payload: terminal_payload,
        });

        let te = convert_events(&events_to_append, &thread_row.chain_root_id, thread_id);
        let result = committed_value(g.state_db.append_events_admitted(
            &thread_row.chain_root_id,
            thread_id,
            te,
            vec![snapshot_update],
            g.signer.as_ref(),
            &g.runtime_db,
            cas_mutation_guard,
        )?);

        Ok((
            persisted_from_append(&result, &events_to_append)?,
            effective_update,
        ))
    }

    #[tracing::instrument(
        name = "state:create_continuation",
        skip(self, successor),
        fields(
            thread_id = %successor.thread_id,
            chain_root_id = %chain_root_id,
            source_thread_id = %source_thread_id,
        )
    )]
    pub(crate) fn create_continuation_admitted(
        &self,
        successor: &NewThreadRecord,
        source_thread_id: &str,
        chain_root_id: &str,
        reason: Option<&str>,
        initial_events: Vec<NewEventRecord>,
        launch_metadata: Option<&crate::launch_metadata::RuntimeLaunchMetadata>,
    ) -> Result<CreatedThreadPublication> {
        let permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let launch_planning = g
            .runtime_db
            .launch_planning_by_thread(&successor.thread_id)?;
        if let Some(planning) = launch_planning.as_ref() {
            if planning.state != "planning"
                || planning.daemon_generation_id != runtime_db::daemon_generation_id()
            {
                return Err(LaunchPlanningInactive.into());
            }
            validate_launch_planning_owner(planning, successor.requested_by.as_deref())?;
        }
        validate_facet_event_admission(&g, &successor.thread_id, &initial_events)?;
        if !self
            .process_attachment_admission_open
            .load(Ordering::Acquire)
        {
            bail!("continuation creation is closed for daemon shutdown");
        }
        let source_row = g
            .state_db
            .get_thread(source_thread_id)?
            .ok_or_else(|| anyhow!("source thread not found: {source_thread_id}"))?;

        if is_terminal_status(&source_row.status)
            && source_row.status != ThreadStatus::Failed.as_str()
            && source_row.status != ThreadStatus::Completed.as_str()
        {
            bail!(
                "cannot continue thread in terminal status '{}'",
                source_row.status
            );
        }

        // Single-successor invariant: a thread is continued at most once. The
        // write permit + lock held here serialize `create_continuation`, so this
        // check-then-create is atomic — a double-submit or race cannot mint
        // sibling successors (which would make `successor_thread_id` ambiguous).
        if let Some(existing) =
            queries::continuation_successor(g.state_db.projection(), source_thread_id)?
        {
            bail!("thread {source_thread_id} already continued as {existing}");
        }

        // Ensure successor has upstream_thread_id set to source for edge derivation
        let mut successor_with_upstream = successor.clone();
        if successor_with_upstream.upstream_thread_id.is_none() {
            successor_with_upstream.upstream_thread_id = Some(source_thread_id.to_string());
        }
        let (successor_snapshot, source_snapshot_before) = attach_continuation_launch_capsule(
            &self.state_authority,
            &self.bundle_admission_trust,
            permit.cas_guard(),
            &g,
            build_snapshot(&successor_with_upstream),
            ContinuationCapsuleTransition {
                chain_root_id,
                source_thread_id,
                launch_metadata,
                expected_result_generation: None,
                kind: crate::launch_metadata::ContinuationAuthorityTransitionKind::Inherit,
            },
        )?;
        // Predecessor-immutability contract: terminal sources retain their
        // exact snapshot, while a running source is settled to `continued` in
        // the same signed head as the successor.
        let source_snapshot_updated = !is_terminal_status(&source_row.status);
        let source_snapshot_after = if source_snapshot_updated {
            continued_snapshot_from_authoritative(
                source_snapshot_before.clone(),
                &lillux::time::iso8601_now(),
            )
        } else {
            source_snapshot_before.clone()
        };
        let source_snapshot_updates = source_snapshot_updated
            .then(|| SnapshotUpdate {
                thread_id: source_thread_id.to_string(),
                new_snapshot: source_snapshot_after.clone(),
            })
            .into_iter()
            .collect();
        let successor_runtime = RuntimeInfo {
            launch_metadata: launch_metadata.cloned(),
            ..RuntimeInfo::default()
        };
        {
            let _admission = g.state_db.authorize_runtime_pin(chain_root_id)?;
            g.runtime_db
                .insert_thread_runtime(&successor.thread_id, chain_root_id)?;
            if let Some(launch_metadata) = launch_metadata
                && let Err(error) = g
                    .runtime_db
                    .set_launch_metadata(&successor.thread_id, launch_metadata)
            {
                let _ = self.delete_thread_runtime_locked(&g, &successor.thread_id);
                return Err(error);
            }
        }

        let successor_event = NewEventRecord {
            event_type: "thread_created".to_string(),
            storage_class: "indexed".to_string(),
            payload: json!({
                "kind": &successor.kind,
                "item_ref": &successor.item_ref,
                "continuation_from": source_thread_id,
            }),
        };
        let mut successor_events = Vec::with_capacity(initial_events.len() + 1);
        successor_events.push(successor_event);
        successor_events.extend(initial_events);
        let successor_thread_events =
            convert_events(&successor_events, chain_root_id, &successor.thread_id);
        let expected_successor_events = successor_thread_events.clone();
        let source_event = NewEventRecord {
            event_type: "thread_continued".to_string(),
            storage_class: "indexed".to_string(),
            payload: json!({
                "successor_thread_id": &successor.thread_id,
                "reason": reason,
            }),
        };
        let ste = convert_events(
            std::slice::from_ref(&source_event),
            chain_root_id,
            source_thread_id,
        );
        let expected_source_event = ste
            .first()
            .cloned()
            .expect("continuation source event batch is non-empty");
        let expected_successor_snapshot = successor_snapshot.clone();
        let successor_commit = g.state_db.add_thread_with_events_and_append_admitted(
            chain_root_id,
            successor_snapshot,
            successor_thread_events,
            source_thread_id,
            ste,
            source_snapshot_updates,
            g.signer.as_ref(),
            &g.runtime_db,
            permit.cas_guard(),
        );
        let successor_result = match successor_commit {
            Ok(committed) => committed_value(committed),
            Err(error) => {
                match read_exact_committed_continuation(
                    &g,
                    chain_root_id,
                    ExpectedContinuationCommit {
                        source_thread_id,
                        source_before: &source_snapshot_before,
                        source_after: &source_snapshot_after,
                        source_snapshot_updated,
                        source_event: &expected_source_event,
                        successor_snapshot: &expected_successor_snapshot,
                        successor_events: &expected_successor_events,
                    },
                ) {
                    Ok(ContinuationCommitReadback::Exact(snapshot)) => {
                        // `StateDb` can report a journal phase-advance error
                        // after the signed head is already visible. Continue
                        // from the exact source/successor pair instead of
                        // deleting the runtime authority needed to launch it.
                        if let Some(planning) = launch_planning.as_ref() {
                            match g.runtime_db.bind_launch_planning(&successor.thread_id) {
                                Ok(true) => {}
                                Ok(false) => tracing::error!(
                                    thread_id = %successor.thread_id,
                                    launch_id = %planning.launch_id,
                                    "authoritative continuation committed but launch planning bind changed no row"
                                ),
                                Err(bind_error) => tracing::error!(
                                    thread_id = %successor.thread_id,
                                    launch_id = %planning.launch_id,
                                    error = %bind_error,
                                    "authoritative continuation committed but launch planning bind failed"
                                ),
                            }
                            match self.launch_task_abort_handles.lock() {
                                Ok(mut handles) => {
                                    handles.remove(&planning.launch_id);
                                }
                                Err(poisoned) => {
                                    tracing::error!(
                                        thread_id = %successor.thread_id,
                                        launch_id = %planning.launch_id,
                                        "launch task abort registry was poisoned after continuation commit"
                                    );
                                    poisoned.into_inner().remove(&planning.launch_id);
                                }
                            }
                        }
                        tracing::error!(
                            thread_id = %successor.thread_id,
                            source_thread_id,
                            error = %error,
                            "continuation creation returned an error after publishing authoritative state; continuing from exact committed pair"
                        );
                        return Ok(CreatedThreadPublication {
                            persisted: Vec::new(),
                            successor: thread_detail_from_committed_snapshot(
                                *snapshot,
                                successor_runtime,
                            ),
                            remote_writer_grant_hash: None,
                        });
                    }
                    Err(authority_error) => {
                        return Err(anyhow!(
                            "continuation creation failed and authoritative pair readback was unavailable; preserving runtime ownership for reconciliation: write error: {error:#}; readback error: {authority_error:#}"
                        ));
                    }
                    Ok(ContinuationCommitReadback::Ambiguous(reason)) => {
                        return Err(anyhow!(
                            "continuation creation failed with ambiguous authoritative readback; preserving runtime ownership for reconciliation: {reason}; write error: {error:#}"
                        ));
                    }
                    Ok(ContinuationCommitReadback::ProvenAbsent) => {}
                }
                let _selected_exclusion = self.selected_mutation_permit(
                    &g,
                    SelectedMutationOperation::DeleteThread(&successor.thread_id),
                )?;
                if let Err(settle_error) = g.runtime_db.fail_launch_planning(&successor.thread_id) {
                    tracing::error!(
                        thread_id = %successor.thread_id,
                        error = %settle_error,
                        "failed to settle launch planning after continuation creation failed"
                    );
                }
                if let Err(cleanup_error) =
                    self.delete_thread_runtime_locked(&g, &successor.thread_id)
                {
                    tracing::error!(
                        thread_id = %successor.thread_id,
                        error = %cleanup_error,
                        "failed to remove runtime row after atomic continuation birth failed"
                    );
                }
                return Err(error);
            }
        };
        // The signed successor/source transition is authoritative from this
        // point onward. Auxiliary planning and live-notification bookkeeping
        // must not turn that committed continuation into a pre-launch error.
        if let Some(planning) = launch_planning {
            match g.runtime_db.bind_launch_planning(&successor.thread_id) {
                Ok(true) => {}
                Ok(false) => tracing::error!(
                    thread_id = %successor.thread_id,
                    launch_id = %planning.launch_id,
                    "authoritative continuation committed but launch planning bind changed no row"
                ),
                Err(error) => tracing::error!(
                    thread_id = %successor.thread_id,
                    launch_id = %planning.launch_id,
                    error = %error,
                    "authoritative continuation committed but launch planning bind failed"
                ),
            }
            match self.launch_task_abort_handles.lock() {
                Ok(mut handles) => {
                    handles.remove(&planning.launch_id);
                }
                Err(poisoned) => {
                    tracing::error!(
                        thread_id = %successor.thread_id,
                        launch_id = %planning.launch_id,
                        "launch task abort registry was poisoned after continuation commit"
                    );
                    poisoned.into_inner().remove(&planning.launch_id);
                }
            }
        }
        let mut all_input_events = successor_events;
        all_input_events.push(source_event);
        let persisted =
            match persisted_from_add_thread_with_events(&successor_result, &all_input_events) {
                Ok(persisted) => persisted,
                Err(error) => {
                    tracing::error!(
                        thread_id = %successor.thread_id,
                        source_thread_id,
                        chain_root_id,
                        error = %error,
                        "authoritative continuation committed but live event reconstruction failed"
                    );
                    Vec::new()
                }
            };
        let successor =
            thread_detail_from_committed_snapshot(successor_result.snapshot, successor_runtime);
        Ok(CreatedThreadPublication {
            persisted,
            successor,
            remote_writer_grant_hash: None,
        })
    }
}

/// Whether a thread's persisted `status` is terminal. The canonical predicate —
/// `ThreadTerminalStatus`'s string mapping omits daemon-owned `timed_out`, so it
/// is not usable for this; callers outside this module (e.g. the cancel cascade)
/// reuse this rather than re-listing the statuses.
pub fn is_terminal_status(status: &str) -> bool {
    matches!(
        status,
        "completed" | "failed" | "cancelled" | "killed" | "timed_out" | "continued"
    )
}

pub(super) fn terminal_event_type(status: &str) -> Result<&'static str> {
    match status {
        "completed" => Ok("thread_completed"),
        "failed" => Ok("thread_failed"),
        "cancelled" => Ok("thread_cancelled"),
        "killed" => Ok("thread_killed"),
        "timed_out" => Ok("thread_timed_out"),
        "continued" => Ok("thread_continued"),
        other => bail!("invalid terminal event status: {other}"),
    }
}
