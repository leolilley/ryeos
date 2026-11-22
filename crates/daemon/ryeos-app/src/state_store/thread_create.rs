//! Thread creation, in-process birth and trace branches.
use super::*;

pub(super) fn build_snapshot(thread: &NewThreadRecord) -> ThreadSnapshot {
    let now = lillux::time::iso8601_now();
    ThreadSnapshot {
        schema: ryeos_state::objects::THREAD_SNAPSHOT_SCHEMA_VERSION,
        kind: "thread_snapshot".to_string(),
        thread_id: thread.thread_id.clone(),
        chain_root_id: thread.chain_root_id.clone(),
        status: ThreadStatus::Created,
        kind_name: thread.kind.clone(),
        item_ref: thread.item_ref.clone(),
        executor_ref: thread.executor_ref.clone(),
        launch_mode: thread.launch_mode.clone(),
        current_site_id: thread.current_site_id.clone(),
        origin_site_id: thread.origin_site_id.clone(),
        upstream_thread_id: thread.upstream_thread_id.clone(),
        requested_by: thread.requested_by.clone(),
        project_root: thread.project_root.clone(),
        project_authority: thread.project_authority.clone(),
        admitted_launch_capsule_hash: None,
        base_project_snapshot_hash: thread.base_project_snapshot_hash.clone(),
        result_project_snapshot_hash: None,
        result_workspace_output_capture_hash: None,
        created_at: now.clone(),
        updated_at: now,
        started_at: None,
        finished_at: None,
        result: None,
        outcome_code: None,
        error: None,
        budget: None,
        artifacts: vec![],
        managed_runtime_terminal: None,
        captured_history_policy: thread.captured_history_policy.clone(),
        facets: Default::default(),
        last_event_hash: None,
        last_chain_seq: 0,
        last_thread_seq: 0,
    }
}

impl StateStore {
    /// Raw root persistence is crate-private. The only production callers are
    /// lifecycle methods which first consume an opaque, current admission.
    pub(crate) fn create_admitted_root_thread(
        &self,
        thread: &NewThreadRecord,
    ) -> Result<CreatedThreadPublication> {
        if thread.thread_id != thread.chain_root_id {
            bail!("admitted root persistence requires thread_id == chain_root_id");
        }
        if thread.captured_history_policy.is_none() {
            bail!(
                "new chain root {} has no verified captured history policy",
                thread.thread_id
            );
        }
        self.create_root_thread_with_events_and_launch_metadata(thread, Vec::new(), None)
    }

    /// State-layer fixture for tests which exercise persistence below the
    /// engine admission boundary. It is absent from production builds and
    /// deliberately names the bypass; application/runtime tests should prefer
    /// a real lifecycle admission.
    #[cfg(any(test, feature = "test-support"))]
    pub fn create_thread_for_test(
        &self,
        thread: &NewThreadRecord,
    ) -> Result<Vec<PersistedEventRecord>> {
        if thread.thread_id == thread.chain_root_id {
            self.create_admitted_root_thread(thread)
                .map(|publication| publication.persisted)
        } else {
            self.create_child_thread_admitted(thread)
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn create_root_thread_for_test_with_launch_metadata(
        &self,
        thread: &NewThreadRecord,
        launch_metadata: &crate::launch_metadata::RuntimeLaunchMetadata,
    ) -> Result<Vec<PersistedEventRecord>> {
        anyhow::ensure!(
            thread.thread_id == thread.chain_root_id,
            "launch-metadata fixture must be a chain root"
        );
        self.create_root_thread_with_events_and_launch_metadata(
            thread,
            Vec::new(),
            Some(launch_metadata),
        )
        .map(|publication| publication.persisted)
    }

    /// Deliberately corrupt only the replaceable thread projection so recovery
    /// tests can prove authoritative reservation reconciliation does not
    /// depend on projected status.
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_thread_projection_status_for_test(
        &self,
        thread_id: &str,
        status: &str,
    ) -> Result<()> {
        let g = self.lock()?;
        let updated = g.state_db.projection().with_sql(|connection| {
            Ok(connection.execute(
                "UPDATE threads SET status = ?2 WHERE thread_id = ?1",
                rusqlite::params![thread_id, status],
            )?)
        })?;
        if updated != 1 {
            bail!("test projection mutation found no exact thread {thread_id}");
        }
        Ok(())
    }

    /// Run raw SQL on this Store's own live Runtime connection, so recovery
    /// tests can forge crash-window rows that no public transition produces.
    /// A second connection to the Runtime file is refused by the native host.
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_runtime_sql_for_test<T>(
        &self,
        operation: impl for<'connection> FnOnce(&'connection rusqlite::Connection) -> Result<T>,
    ) -> Result<T> {
        let g = self.lock()?;
        g.runtime_db.with_sql_for_test(operation)
    }

    /// Remove only projected events while retaining authoritative CAS, used to
    /// prove terminal postcommit repair does not depend on projection freshness.
    #[cfg(any(test, feature = "test-support"))]
    pub fn delete_thread_projection_events_for_test(&self, thread_id: &str) -> Result<usize> {
        let g = self.lock()?;
        g.state_db.projection().with_sql(|connection| {
            Ok(connection.execute(
                "DELETE FROM events WHERE thread_id = ?1",
                rusqlite::params![thread_id],
            )?)
        })
    }

    /// Remove the replaceable thread row after its projected events have been
    /// removed. Authoritative CAS remains intact so terminal postcommit repair
    /// can prove it has no projection-row dependency.
    #[cfg(any(test, feature = "test-support"))]
    pub fn delete_thread_projection_row_for_test(&self, thread_id: &str) -> Result<bool> {
        let g = self.lock()?;
        Ok(g.state_db.projection().with_sql(|connection| {
            Ok(connection.execute(
                "DELETE FROM threads WHERE thread_id = ?1",
                rusqlite::params![thread_id],
            )?)
        })? == 1)
    }

    pub(super) fn create_thread_inner(
        &self,
        thread: &NewThreadRecord,
    ) -> Result<Vec<PersistedEventRecord>> {
        if thread.thread_id == thread.chain_root_id {
            return self
                .create_root_thread_with_events_and_launch_metadata(thread, Vec::new(), None)
                .map(|publication| publication.persisted);
        }
        let permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        if !self
            .process_attachment_admission_open
            .load(Ordering::Acquire)
        {
            bail!("thread creation is closed for daemon shutdown");
        }
        let mut payload = json!({
            "kind": &thread.kind,
            "item_ref": &thread.item_ref,
            "executor_ref": &thread.executor_ref,
            "launch_mode": &thread.launch_mode,
        });
        if let Some(usage_subject) = &thread.usage_subject {
            usage_subject.validate()?;
            payload["usage_subject"] =
                serde_json::to_value(usage_subject).context("failed to encode usage_subject")?;
            if let Some(asserted_by) = &thread.usage_subject_asserted_by {
                payload["usage_subject_asserted_by"] = json!(asserted_by);
            }
        }

        let create_event = NewEventRecord {
            event_type: "thread_created".to_string(),
            storage_class: "indexed".to_string(),
            payload,
        };

        let te = convert_events(
            std::slice::from_ref(&create_event),
            &thread.chain_root_id,
            &thread.thread_id,
        );
        {
            let _admission = g.state_db.authorize_runtime_pin(&thread.chain_root_id)?;
            g.runtime_db
                .insert_thread_runtime(&thread.thread_id, &thread.chain_root_id)?;
        }
        let committed = g.state_db.add_thread_with_events_admitted(
            &thread.chain_root_id,
            build_snapshot(thread),
            te,
            g.signer.as_ref(),
            &g.runtime_db,
            permit.cas_guard(),
        );
        let result = match committed {
            Ok(committed) => committed_value(committed),
            Err(error) => {
                let _ = self.delete_thread_runtime_locked(&g, &thread.thread_id);
                return Err(error);
            }
        };

        persisted_from_add_thread_with_events(&result, &[create_event])
    }

    /// Create a root thread together with caller-supplied initial durable
    /// events. The snapshot, `thread_created`, and supplied events share one
    /// authoritative chain-head commit and projection transaction.
    pub fn create_root_thread_with_events(
        &self,
        thread: &NewThreadRecord,
        initial_events: Vec<NewEventRecord>,
    ) -> Result<Vec<PersistedEventRecord>> {
        self.create_root_thread_with_events_and_launch_metadata(thread, initial_events, None)
            .map(|publication| publication.persisted)
    }

    /// Create a managed-launch root with its resume identity installed before
    /// the authoritative chain head becomes visible. The runtime row is
    /// auxiliary, so it is prepared first and removed if chain creation fails.
    pub(crate) fn create_root_thread_with_events_and_launch_metadata(
        &self,
        thread: &NewThreadRecord,
        initial_events: Vec<NewEventRecord>,
        launch_metadata: Option<&crate::launch_metadata::RuntimeLaunchMetadata>,
    ) -> Result<CreatedThreadPublication> {
        self.create_root_thread_with_events_and_launch_metadata_owned(
            thread,
            initial_events,
            launch_metadata,
            RootBirthOwner::External,
        )
    }

    /// Publish an in-process root only while the exact daemon task owner is
    /// registered. The synchronous owner task cannot drop its guard during
    /// this call; the registry lock itself is deliberately released before
    /// the authoritative chain/runtime commit.
    pub fn create_in_process_root_with_events_and_launch_metadata(
        &self,
        thread: &NewThreadRecord,
        initial_events: Vec<NewEventRecord>,
        launch_metadata: &crate::launch_metadata::RuntimeLaunchMetadata,
        owner: &InProcessHandlerControl,
    ) -> Result<Vec<PersistedEventRecord>> {
        self.create_root_thread_with_events_and_launch_metadata_owned(
            thread,
            initial_events,
            Some(launch_metadata),
            RootBirthOwner::InProcess(owner),
        )
        .map(|publication| publication.persisted)
    }

    /// Publish an in-process handler thread as a member of its parent's chain
    /// while its exact daemon task owner is registered. An inline in-process
    /// child is a thread in its parent's chain with the parent as upstream,
    /// exactly like an inline tool leaf; this is the birth that makes it so.
    /// The owner contract is the root birth's: the durable reservation is
    /// established before the commit, the created→running transition commits
    /// with the birth under the parent chain's signed head, and the owner's
    /// birth proof is marked only once that head carries the member. The
    /// registry lock is released before the authoritative commit, as above.
    ///
    /// The returned witness is the only way to link the child while it may
    /// still inherit a parent stop landed since the birth; see
    /// [`Self::link_unspawned_in_process_child`].
    pub fn create_in_process_child_with_events_and_launch_metadata(
        &self,
        thread: &NewThreadRecord,
        initial_events: Vec<NewEventRecord>,
        launch_metadata: &crate::launch_metadata::RuntimeLaunchMetadata,
        owner: &InProcessHandlerControl,
    ) -> Result<(Vec<PersistedEventRecord>, UnspawnedInProcessChild)> {
        let persisted = self.create_in_process_child_birth(
            thread,
            initial_events,
            launch_metadata,
            owner,
            true,
        )?;
        let parent_thread_id = thread.upstream_thread_id.clone().ok_or_else(|| {
            anyhow!(
                "in-process child {} names no upstream parent",
                thread.thread_id
            )
        })?;
        Ok((
            persisted,
            UnspawnedInProcessChild {
                parent_thread_id,
                thread_id: thread.thread_id.clone(),
            },
        ))
    }

    /// The same birth, interrupted exactly as a crash after the authoritative
    /// commit would leave it: the member is published, its reservation is
    /// still pending, and no dispatch link has been written.
    #[cfg(test)]
    pub(super) fn fixture_create_in_process_child_interrupted_before_link(
        &self,
        thread: &NewThreadRecord,
        initial_events: Vec<NewEventRecord>,
        launch_metadata: &crate::launch_metadata::RuntimeLaunchMetadata,
        owner: &InProcessHandlerControl,
    ) -> Result<Vec<PersistedEventRecord>> {
        self.create_in_process_child_birth(thread, initial_events, launch_metadata, owner, false)
    }

    pub(super) fn create_in_process_child_birth(
        &self,
        thread: &NewThreadRecord,
        initial_events: Vec<NewEventRecord>,
        launch_metadata: &crate::launch_metadata::RuntimeLaunchMetadata,
        owner: &InProcessHandlerControl,
        link_and_advance: bool,
    ) -> Result<Vec<PersistedEventRecord>> {
        if thread.thread_id == thread.chain_root_id || thread.upstream_thread_id.is_none() {
            bail!("in-process child birth requires a chain member record with an upstream parent");
        }
        if thread.captured_history_policy.is_some() {
            bail!("non-root threads cannot carry a captured history policy");
        }
        if initial_events
            .iter()
            .any(|event| event.storage_class == "ephemeral")
        {
            bail!("in-process child initial events must be durable");
        }
        if initial_events.len() != 1
            || initial_events[0].event_type != ryeos_state::event_types::THREAD_STARTED
        {
            bail!("in-process child birth requires exactly one thread_started successor event");
        }
        if launch_metadata.launch_driver
            != Some(ryeos_state::objects::ExecutionLaunchDriver::InProcessHandler)
        {
            bail!("in-process child birth requires the in-process handler launch driver");
        }
        let permit = self.acquire_write_permit()?;
        self.ensure_in_process_handler_owner(&thread.thread_id, owner)?;
        let birth_snapshot = attach_admitted_launch_capsule(
            &self.state_authority,
            permit.cas_guard(),
            build_snapshot(thread),
            Some(launch_metadata),
        )?;
        let g = self.lock()?;
        if !self
            .process_attachment_admission_open
            .load(Ordering::Acquire)
        {
            bail!("thread creation is closed for daemon shutdown");
        }
        // The parent must be a live member of the chain before anything of
        // the child is written. The store lock is held from here through the
        // link, so no store finalizer can settle the parent in between; the
        // chain commit repeats the check against the head under its own lock.
        let parent_thread_id = thread
            .upstream_thread_id
            .as_deref()
            .expect("in-process child upstream checked above");
        let parent = g
            .state_db
            .read_authoritative_thread_snapshot(&thread.chain_root_id, parent_thread_id)?
            .ok_or_else(|| {
                anyhow!(
                    "in-process child {} names parent {parent_thread_id}, which is not a member of chain {}",
                    thread.thread_id,
                    thread.chain_root_id
                )
            })?;
        if parent.status.is_terminal() {
            bail!(
                "refusing to birth in-process child {} under terminal parent {parent_thread_id} ({})",
                thread.thread_id,
                parent.status.as_str()
            );
        }
        validate_facet_event_admission(&g, &thread.thread_id, &initial_events)?;
        let mut payload = json!({
            "kind": &thread.kind,
            "item_ref": &thread.item_ref,
            "executor_ref": &thread.executor_ref,
            "launch_mode": &thread.launch_mode,
        });
        if let Some(usage_subject) = &thread.usage_subject {
            usage_subject.validate()?;
            payload["usage_subject"] =
                serde_json::to_value(usage_subject).context("failed to encode usage_subject")?;
            if let Some(asserted_by) = &thread.usage_subject_asserted_by {
                payload["usage_subject_asserted_by"] = json!(asserted_by);
            }
        }
        let mut events = Vec::with_capacity(initial_events.len() + 1);
        events.push(NewEventRecord {
            event_type: ryeos_state::event_types::THREAD_CREATED.to_string(),
            storage_class: "indexed".to_string(),
            payload,
        });
        events.extend(initial_events);
        let thread_events = convert_events(&events, &thread.chain_root_id, &thread.thread_id);
        let mut running = birth_snapshot.clone();
        running.status = ThreadStatus::Running;
        let started_at = lillux::time::iso8601_now();
        running.started_at = Some(started_at.clone());
        running.updated_at = started_at;
        // The auxiliary runtime row and the pending reservation precede the
        // authoritative commit, inside the parent chain's runtime pin, like
        // every other member birth. A proven pre-commit failure removes them;
        // an ambiguous acknowledgement or an observable committed member
        // preserves them for exact settlement.
        {
            let _admission = g.state_db.authorize_runtime_pin(&thread.chain_root_id)?;
            g.runtime_db.reserve_in_process_handler_birth(
                &thread.thread_id,
                &thread.chain_root_id,
                launch_metadata,
            )?;
        }
        let committed = g.state_db.add_thread_with_event_successor_admitted(
            AddThreadWithEventSuccessorRequest {
                chain_root_id: &thread.chain_root_id,
                genesis_snapshot: birth_snapshot,
                event_successor_snapshot: running,
                events: thread_events,
                signer: g.signer.as_ref(),
                runtime_liveness: &g.runtime_db,
                cas_mutation_guard: permit.cas_guard(),
            },
        );
        let result = match committed {
            Ok(committed) => committed_value(committed),
            Err(error) => {
                // The parent chain head is the only authority on whether the
                // member was published. A readable head without the member
                // proves the write never landed; a member at running status
                // is the exact birth this owner authored; anything else keeps
                // ownership for reconciliation.
                match g
                    .state_db
                    .read_authoritative_thread_snapshot(&thread.chain_root_id, &thread.thread_id)
                {
                    Ok(Some(snapshot)) if snapshot.status == ThreadStatus::Running => {
                        owner.mark_birth_committed();
                        // The caller receives an error and its owner settles
                        // the member with its own failure terminal, so only
                        // lineage is written here, not an inherited stop that
                        // would contradict that terminal.
                        if let Err(link_error) = link_in_process_chain_member_locked(
                            &g,
                            &thread.chain_root_id,
                            &snapshot,
                            false,
                        ) {
                            return Err(anyhow!(
                                "in-process child birth committed despite its returned error, but its dispatch link could not be written; reservation settlement repairs it: {link_error:#}; original error: {error:#}"
                            ));
                        }
                        if let Err(reservation_error) = g
                            .runtime_db
                            .mark_in_process_handler_birth_running(&thread.thread_id)
                        {
                            return Err(anyhow!(
                                "in-process child birth committed despite its returned error, but its reservation could not advance: {reservation_error:#}; original error: {error:#}"
                            ));
                        }
                        return Err(anyhow!(
                            "in-process child birth committed authoritative state despite its returned error; its armed owner must settle the row: {error:#}"
                        ));
                    }
                    Ok(Some(snapshot)) => {
                        owner.mark_birth_committed();
                        return Err(anyhow!(
                            "in-process child birth left member {} at status {} after its returned error; preserving runtime ownership for reconciliation: {error:#}",
                            thread.thread_id,
                            snapshot.status.as_str()
                        ));
                    }
                    Ok(None) => {}
                    Err(readback_error) => {
                        owner.mark_birth_committed();
                        return Err(anyhow!(
                            "in-process child birth failed and authoritative readback was unavailable; preserving runtime ownership for reconciliation: write error: {error:#}; readback error: {readback_error:#}"
                        ));
                    }
                }
                let cleanup = self
                    .selected_mutation_permit(
                        &g,
                        SelectedMutationOperation::DiscardBirth(&thread.thread_id),
                    )
                    .and_then(|selected| {
                        g.runtime_db
                            .discard_pending_in_process_handler_birth(&selected, &thread.thread_id)
                    })
                    .and_then(|deleted| {
                        if deleted {
                            Ok(())
                        } else {
                            bail!(
                                "pending in-process birth reservation disappeared before failed-member cleanup"
                            )
                        }
                    });
                if let Err(cleanup_error) = cleanup {
                    tracing::error!(
                        thread_id = %thread.thread_id,
                        chain_root_id = %thread.chain_root_id,
                        error = %cleanup_error,
                        "failed to remove auxiliary runtime row after in-process child birth failed"
                    );
                }
                return Err(error);
            }
        };
        owner.mark_birth_committed();
        if !link_and_advance {
            return persisted_from_add_thread_with_events(&result, &events)
                .context("reconstruct committed in-process child events");
        }
        // The member is published; its dispatch link and any stop the parent
        // already carries are written in one runtime transaction before the
        // reservation leaves `pending`, so a running reservation always has
        // its link and a crash in between is repaired from the upstream id
        // when reconciliation advances the reservation.
        link_in_process_chain_member_locked(&g, &thread.chain_root_id, &result.snapshot, true)
            .with_context(|| {
                format!(
                    "record dispatch link of in-process child {} to parent {parent_thread_id}",
                    thread.thread_id
                )
            })?;
        g.runtime_db
            .mark_in_process_handler_birth_running(&thread.thread_id)
            .with_context(|| {
                format!(
                    "advance committed in-process birth reservation for {}",
                    thread.thread_id
                )
            })?;
        persisted_from_add_thread_with_events(&result, &events)
            .context("reconstruct committed in-process child events before handler effects")
    }

    pub(super) fn create_root_thread_with_events_and_launch_metadata_owned(
        &self,
        thread: &NewThreadRecord,
        initial_events: Vec<NewEventRecord>,
        launch_metadata: Option<&crate::launch_metadata::RuntimeLaunchMetadata>,
        birth_owner: RootBirthOwner<'_>,
    ) -> Result<CreatedThreadPublication> {
        if thread.thread_id != thread.chain_root_id || thread.upstream_thread_id.is_some() {
            bail!("create_root_thread_with_events requires a root thread record");
        }
        if initial_events
            .iter()
            .any(|event| event.storage_class == "ephemeral")
        {
            bail!("root initial events must be durable");
        }
        let permit = self.acquire_write_permit()?;
        let is_in_process_launch = launch_metadata.and_then(|metadata| metadata.launch_driver)
            == Some(ryeos_state::objects::ExecutionLaunchDriver::InProcessHandler);
        match (&birth_owner, is_in_process_launch) {
            (RootBirthOwner::External, false) => {
                let active = self
                    .active_in_process_handlers
                    .lock()
                    .map_err(|_| anyhow!("active in-process-handler registry poisoned"))?;
                if active.contains_key(&thread.thread_id) {
                    bail!(
                        "registered in-process owner cannot publish a non-in-process root for thread {}",
                        thread.thread_id
                    );
                }
            }
            (RootBirthOwner::External, true) => {
                bail!("in-process root birth requires an exact registered daemon owner");
            }
            (RootBirthOwner::InProcess(_), false) => {
                bail!("in-process root owner cannot authorize a different launch driver");
            }
            (RootBirthOwner::InProcess(owner), true) => {
                // As above, do not serialize every StateStore writer behind a
                // volatile registry mutex while the durable birth commits.
                self.ensure_in_process_handler_owner(&thread.thread_id, owner)?;
            }
        }
        // Capsule verification and CAS publication are protected by the
        // mutation permit above, not by the daemon's process-local store
        // mutex. Do them before taking that mutex so independent root births
        // do not serialize every runtime/projection operation behind content
        // verification. Admission state, the auxiliary runtime row, the
        // authoritative chain commit, and its projection remain one fenced
        // critical section below.
        let birth_snapshot = attach_admitted_launch_capsule(
            &self.state_authority,
            permit.cas_guard(),
            build_snapshot(thread),
            launch_metadata,
        )?;
        let expected_birth_snapshot = birth_snapshot.clone();
        let thread_runtime = RuntimeInfo {
            launch_metadata: launch_metadata.cloned(),
            ..RuntimeInfo::default()
        };
        let g = self.lock()?;
        if !self
            .process_attachment_admission_open
            .load(Ordering::Acquire)
        {
            bail!("thread creation is closed for daemon shutdown");
        }
        let launch_planning = g.runtime_db.launch_planning_by_thread(&thread.thread_id)?;
        if let Some(planning) = launch_planning.as_ref() {
            if planning.state != "planning"
                || planning.daemon_generation_id != runtime_db::daemon_generation_id()
            {
                return Err(LaunchPlanningInactive.into());
            }
            validate_launch_planning_owner(planning, thread.requested_by.as_deref())?;
        }
        // Initial facet events are subject to the same collection/key/value
        // limits as ordinary appends. The new thread is not projected yet, so
        // the validator correctly evaluates this batch against an empty set.
        validate_facet_event_admission(&g, &thread.thread_id, &initial_events)?;
        let mut payload = json!({
            "kind": &thread.kind,
            "item_ref": &thread.item_ref,
            "executor_ref": &thread.executor_ref,
            "launch_mode": &thread.launch_mode,
        });
        if let Some(usage_subject) = &thread.usage_subject {
            usage_subject.validate()?;
            payload["usage_subject"] =
                serde_json::to_value(usage_subject).context("failed to encode usage_subject")?;
            if let Some(asserted_by) = &thread.usage_subject_asserted_by {
                payload["usage_subject_asserted_by"] = json!(asserted_by);
            }
        }
        let mut events = Vec::with_capacity(initial_events.len() + 1);
        events.push(NewEventRecord {
            event_type: ryeos_state::event_types::THREAD_CREATED.to_string(),
            storage_class: "indexed".to_string(),
            payload,
        });
        events.extend(initial_events);
        let thread_events = convert_events(&events, &thread.chain_root_id, &thread.thread_id);
        let event_successor_snapshot = if launch_metadata
            .and_then(|metadata| metadata.launch_driver)
            == Some(ryeos_state::objects::ExecutionLaunchDriver::InProcessHandler)
        {
            if events.len() != 2 || events[1].event_type != ryeos_state::event_types::THREAD_STARTED
            {
                bail!("in-process root birth requires exactly one thread_started successor event");
            }
            let mut running = birth_snapshot.clone();
            running.status = ThreadStatus::Running;
            let started_at = lillux::time::iso8601_now();
            running.started_at = Some(started_at.clone());
            running.updated_at = started_at;
            Some(running)
        } else {
            None
        };
        let expected_event_successor_snapshot = event_successor_snapshot.clone();
        let expected_thread_events = thread_events.clone();
        // Establish the auxiliary runtime row before the authoritative commit.
        // A proven pre-commit failure removes it. Ambiguous acknowledgement or
        // an observable committed root preserves it for exact settlement;
        // deleting uncertain ownership would make recovery impossible.
        if is_in_process_launch {
            g.runtime_db.reserve_in_process_handler_birth(
                &thread.thread_id,
                &thread.chain_root_id,
                launch_metadata.expect("in-process launch metadata is present"),
            )?;
        } else {
            g.runtime_db
                .insert_thread_runtime(&thread.thread_id, &thread.chain_root_id)?;
            if let Some(launch_metadata) = launch_metadata
                && let Err(error) = g
                    .runtime_db
                    .set_launch_metadata(&thread.thread_id, launch_metadata)
            {
                let _ = self.delete_thread_runtime_locked(&g, &thread.thread_id);
                return Err(error);
            }
        }
        let committed = match event_successor_snapshot {
            Some(successor) => g.state_db.create_chain_with_event_successor_admitted(
                CreateChainWithEventSuccessorRequest {
                    chain_root_id: &thread.chain_root_id,
                    genesis_snapshot: birth_snapshot,
                    event_successor_snapshot: successor,
                    events: thread_events,
                    signer: g.signer.as_ref(),
                    runtime_liveness: &g.runtime_db,
                    cas_mutation_guard: permit.cas_guard(),
                },
            ),
            None => g.state_db.create_chain_with_events_admitted(
                &thread.chain_root_id,
                birth_snapshot,
                thread_events,
                g.signer.as_ref(),
                &g.runtime_db,
                permit.cas_guard(),
            ),
        };
        let result = match committed {
            Ok(committed) => committed_value(committed),
            Err(error) => {
                match read_exact_committed_root(
                    &g,
                    &thread.chain_root_id,
                    ExpectedRootCommit {
                        birth_snapshot: &expected_birth_snapshot,
                        event_successor_snapshot: expected_event_successor_snapshot.as_ref(),
                        events: &expected_thread_events,
                    },
                ) {
                    Ok(RootCommitReadback::Exact(snapshot)) => {
                        if let RootBirthOwner::InProcess(owner) = &birth_owner {
                            owner.mark_birth_committed();
                            if let Err(reservation_error) = g
                                .runtime_db
                                .mark_in_process_handler_birth_running(&thread.thread_id)
                            {
                                return Err(anyhow!(
                                    "in-process root birth committed despite its returned error, but its reservation could not advance: {reservation_error:#}; original error: {error:#}"
                                ));
                            }
                            return Err(anyhow!(
                                "in-process root birth committed authoritative state despite its returned error; its armed owner must settle the row: {error:#}"
                            ));
                        }
                        // External managed publication can continue from the
                        // exact signed snapshot. Returning the write error here
                        // would strand a committed root before launch.
                        if let Some(planning) = launch_planning.as_ref() {
                            match g.runtime_db.bind_launch_planning(&thread.thread_id) {
                                Ok(true) => {}
                                Ok(false) => tracing::error!(
                                    thread_id = %thread.thread_id,
                                    launch_id = %planning.launch_id,
                                    "authoritative root committed but launch planning bind changed no row"
                                ),
                                Err(bind_error) => tracing::error!(
                                    thread_id = %thread.thread_id,
                                    launch_id = %planning.launch_id,
                                    error = %bind_error,
                                    "authoritative root committed but launch planning bind failed"
                                ),
                            }
                            match self.launch_task_abort_handles.lock() {
                                Ok(mut handles) => {
                                    handles.remove(&planning.launch_id);
                                }
                                Err(poisoned) => {
                                    tracing::error!(
                                        thread_id = %thread.thread_id,
                                        launch_id = %planning.launch_id,
                                        "launch task abort registry was poisoned after root commit"
                                    );
                                    poisoned.into_inner().remove(&planning.launch_id);
                                }
                            }
                        }
                        tracing::error!(
                            thread_id = %thread.thread_id,
                            error = %error,
                            "root birth returned an error after publishing authoritative state; continuing from exact committed snapshot"
                        );
                        let successor =
                            thread_detail_from_committed_snapshot(*snapshot, thread_runtime);
                        return Ok(CreatedThreadPublication {
                            persisted: Vec::new(),
                            successor,
                            remote_writer_grant_hash: None,
                        });
                    }
                    Ok(RootCommitReadback::Ambiguous(reason)) => {
                        if let RootBirthOwner::InProcess(owner) = &birth_owner {
                            owner.mark_birth_committed();
                        }
                        return Err(anyhow!(
                            "root birth failed with ambiguous authoritative state; preserving runtime ownership for reconciliation ({reason}): {error:#}"
                        ));
                    }
                    Err(authority_error) => {
                        if let RootBirthOwner::InProcess(owner) = &birth_owner {
                            // The write acknowledgement and authoritative
                            // readback are both ambiguous. Treat birth as
                            // possibly committed so guard-drop exact
                            // terminalization is attempted; startup can
                            // discard the pending reservation if no root was
                            // actually published.
                            owner.mark_birth_committed();
                        }
                        return Err(anyhow!(
                            "root birth failed and authoritative readback was unavailable; preserving runtime ownership for reconciliation: write error: {error:#}; readback error: {authority_error:#}"
                        ));
                    }
                    Ok(RootCommitReadback::ProvenAbsent) => {}
                }
                let _selected_exclusion = self.selected_mutation_permit(
                    &g,
                    SelectedMutationOperation::DeleteThread(&thread.thread_id),
                )?;
                if let Err(settle_error) = g.runtime_db.fail_launch_planning(&thread.thread_id) {
                    tracing::error!(
                        thread_id = %thread.thread_id,
                        error = %settle_error,
                        "failed to settle launch planning after root-chain creation failed"
                    );
                }
                let cleanup = if is_in_process_launch {
                    g.runtime_db
                        .discard_pending_in_process_handler_birth(&self.selected_mutation_permit(&g, SelectedMutationOperation::DiscardBirth(&thread.thread_id))?, &thread.thread_id)
                        .and_then(|deleted| {
                            if deleted {
                                Ok(())
                            } else {
                                bail!(
                                    "pending in-process birth reservation disappeared before failed-root cleanup"
                                )
                            }
                        })
                } else {
                    self.delete_thread_runtime_locked(&g, &thread.thread_id)
                        .map(|_| ())
                };
                if let Err(cleanup_error) = cleanup {
                    tracing::error!(
                        thread_id = %thread.thread_id,
                        error = %cleanup_error,
                        "failed to remove auxiliary runtime row after root-chain creation failed"
                    );
                }
                return Err(error);
            }
        };
        // The signed root birth is authoritative from this point onward.
        // Recorded in-process ownership deliberately retains one fallible
        // boundary: if the durable reservation cannot advance, its already
        // armed owner guard must settle the committed row before handler
        // effects proceed. External managed roots have no such guard, so all
        // of their remaining publication work is best-effort and infallible.
        if let RootBirthOwner::InProcess(owner) = &birth_owner {
            owner.mark_birth_committed();
            g.runtime_db
                .mark_in_process_handler_birth_running(&thread.thread_id)
                .with_context(|| {
                    format!(
                        "advance committed in-process birth reservation for {}",
                        thread.thread_id
                    )
                })?;
        }
        if let Some(planning) = launch_planning {
            match g.runtime_db.bind_launch_planning(&thread.thread_id) {
                Ok(true) => {}
                Ok(false) => tracing::error!(
                    thread_id = %thread.thread_id,
                    launch_id = %planning.launch_id,
                    "authoritative root committed but launch planning bind changed no row"
                ),
                Err(error) => tracing::error!(
                    thread_id = %thread.thread_id,
                    launch_id = %planning.launch_id,
                    error = %error,
                    "authoritative root committed but launch planning bind failed"
                ),
            }
            match self.launch_task_abort_handles.lock() {
                Ok(mut handles) => {
                    handles.remove(&planning.launch_id);
                }
                Err(poisoned) => {
                    tracing::error!(
                        thread_id = %thread.thread_id,
                        launch_id = %planning.launch_id,
                        "launch task abort registry was poisoned after root commit"
                    );
                    poisoned.into_inner().remove(&planning.launch_id);
                }
            }
        }
        let persisted = match persisted_from_add_thread_with_events(&result, &events) {
            Ok(persisted) => persisted,
            Err(error) => {
                if matches!(&birth_owner, RootBirthOwner::InProcess(_)) {
                    return Err(error.context(
                        "reconstruct committed in-process root events before handler effects",
                    ));
                }
                tracing::error!(
                    thread_id = %thread.thread_id,
                    chain_root_id = %thread.chain_root_id,
                    error = %error,
                    "authoritative root committed but live event reconstruction failed"
                );
                Vec::new()
            }
        };
        let successor = thread_detail_from_committed_snapshot(result.snapshot, thread_runtime);
        Ok(CreatedThreadPublication {
            persisted,
            successor,
            remote_writer_grant_hash: None,
        })
    }

    #[tracing::instrument(
        name = "state:create_trace_branch",
        skip(self, thread, branch_payload),
        fields(
            thread_id = %thread.thread_id,
            chain_root_id = %thread.chain_root_id,
            item_ref = %thread.item_ref,
        )
    )]
    pub fn create_trace_branch(
        &self,
        thread: &NewThreadRecord,
        branch_payload: Value,
    ) -> Result<Vec<PersistedEventRecord>> {
        let permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        if !self
            .process_attachment_admission_open
            .load(Ordering::Acquire)
        {
            bail!("trace-branch creation is closed for daemon shutdown");
        }

        if thread.thread_id == thread.chain_root_id {
            bail!("trace branch child must not be a chain root thread");
        }
        if thread.upstream_thread_id.is_some() {
            bail!("trace branch child must not use upstream_thread_id");
        }
        if g.state_db.get_thread(&thread.thread_id)?.is_some() {
            bail!("thread already exists: {}", thread.thread_id);
        }

        let create_event = NewEventRecord {
            event_type: ryeos_state::event_types::THREAD_CREATED.to_string(),
            storage_class: "indexed".to_string(),
            payload: json!({
                "kind": &thread.kind,
                "item_ref": &thread.item_ref,
                "executor_ref": &thread.executor_ref,
                "launch_mode": &thread.launch_mode,
                "trace_branch": true,
            }),
        };
        let branch_event = NewEventRecord {
            event_type: ryeos_state::event_types::EDGE_RECORDED.to_string(),
            storage_class: "indexed".to_string(),
            payload: branch_payload,
        };
        let events_to_append = vec![create_event, branch_event];
        let te = convert_events(&events_to_append, &thread.chain_root_id, &thread.thread_id);
        let result = committed_value(g.state_db.add_thread_with_events_admitted(
            &thread.chain_root_id,
            build_snapshot(thread),
            te,
            g.signer.as_ref(),
            &g.runtime_db,
            permit.cas_guard(),
        )?);

        {
            let _admission = g.state_db.authorize_runtime_pin(&thread.chain_root_id)?;
            g.runtime_db
                .insert_thread_runtime(&thread.thread_id, &thread.chain_root_id)?;
        }

        persisted_from_add_thread_with_events(&result, &events_to_append)
    }

    #[tracing::instrument(
        name = "state:mark_thread_running",
        skip(self),
        fields(thread_id = %thread_id)
    )]
    pub fn mark_thread_running(
        &self,
        thread_id: &str,
        base_project_snapshot_hash: Option<&str>,
    ) -> Result<Vec<PersistedEventRecord>> {
        self.mark_thread_running_with_events(thread_id, base_project_snapshot_hash, Vec::new())
    }
}
