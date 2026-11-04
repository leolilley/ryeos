//! Authoritative snapshot, anchor, replay and project-head reads.
use super::*;

/// Where a recorded thread lives: its own chain root, an inline in-process
/// child with proven lineage in another thread's chain, or any other member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordedThreadPlacement {
    Root,
    InProcessMember {
        chain_root_id: String,
        upstream_thread_id: String,
    },
    OtherMember {
        chain_root_id: String,
    },
}

impl StateStore {
    /// Read an in-process handler thread's authoritative snapshot at the chain
    /// its reservation was born into: its own id for a direct invocation
    /// root, its parent's chain for an inline in-process child. `None` when
    /// the chain head does not carry the thread (an unpublished birth).
    pub fn get_authoritative_in_process_handler_snapshot(
        &self,
        thread_id: &str,
    ) -> Result<Option<ThreadSnapshot>> {
        let g = self.lock()?;
        let chain_root_id = in_process_handler_chain_root_locked(&g, thread_id)?;
        g.state_db
            .read_authoritative_thread_snapshot(&chain_root_id, thread_id)
    }

    /// Read a root snapshot directly from authoritative CAS without requiring
    /// its replaceable projection row to be current.
    pub fn get_authoritative_root_thread_snapshot(
        &self,
        thread_id: &str,
    ) -> Result<Option<ThreadSnapshot>> {
        let g = self.lock()?;
        g.state_db
            .read_authoritative_thread_snapshot(thread_id, thread_id)
    }

    /// Read the authoritative root snapshot and its exact last CAS event
    /// without depending on the rebuildable projection.
    pub fn get_authoritative_root_thread_snapshot_with_last_event(
        &self,
        thread_id: &str,
    ) -> Result<Option<(ThreadSnapshot, Option<PersistedEventRecord>)>> {
        let g = self.lock()?;
        let Some(readback) = g
            .state_db
            .read_authoritative_thread_snapshot_with_last_event(thread_id, thread_id)?
        else {
            return Ok(None);
        };
        let snapshot = readback.snapshot;
        let last_event = readback
            .last_event
            .map(|(event_hash, event)| persisted_event_from_authoritative(event_hash, event))
            .transpose()?;
        Ok(Some((snapshot, last_event)))
    }

    /// Read any placement member at an exact authoritative chain position.
    /// Callers must already know both the stable chain and placement identity;
    /// this never treats a rebuildable projection row as continuation
    /// authority.
    pub fn get_authoritative_thread_snapshot_with_last_event(
        &self,
        chain_root_id: &str,
        thread_id: &str,
    ) -> Result<Option<(ThreadSnapshot, Option<PersistedEventRecord>, String)>> {
        let g = self.lock()?;
        let Some(readback) = g
            .state_db
            .read_authoritative_thread_snapshot_with_last_event(chain_root_id, thread_id)?
        else {
            return Ok(None);
        };
        let snapshot = readback.snapshot;
        let last_event = readback
            .last_event
            .map(|(event_hash, event)| persisted_event_from_authoritative(event_hash, event))
            .transpose()?;
        Ok(Some((snapshot, last_event, readback.chain_head_hash)))
    }

    /// A terminal snapshot can remain immutable after continuation. Inspect
    /// the exact thread history rather than mistaking its latest event for a
    /// complete successor proof; deferred accounting may follow continuation.
    pub fn get_authoritative_thread_snapshot_with_continuation_presence(
        &self,
        chain_root_id: &str,
        thread_id: &str,
    ) -> Result<Option<(ThreadSnapshot, bool, String)>> {
        let guard = self.state_authority.acquire_shared_guard()?;
        self.state_authority
            .read_thread_snapshot_with_event_presence(
                chain_root_id,
                thread_id,
                ryeos_state::event_types::THREAD_CONTINUED,
                65_536,
                64 * 1024 * 1024,
                &guard,
            )
            .map(|readback| readback.map(|(head, snapshot, continued)| (snapshot, continued, head)))
    }

    pub fn get_authoritative_machine_continuation_lineage(
        &self,
        chain_root_id: &str,
        terminal_thread_id: &str,
        guard: &ryeos_state::CasMutationGuard,
    ) -> Result<Option<(String, Vec<ThreadSnapshot>)>> {
        self.state_authority.read_machine_continuation_lineage(
            chain_root_id,
            terminal_thread_id,
            guard,
        )
    }

    pub fn get_authoritative_machine_continuation_segment(
        &self,
        chain_root_id: &str,
        start_thread_id: &str,
        terminal_thread_id: &str,
        guard: &ryeos_state::CasMutationGuard,
    ) -> Result<Option<(String, Vec<ThreadSnapshot>)>> {
        self.state_authority.read_machine_continuation_segment(
            chain_root_id,
            start_thread_id,
            terminal_thread_id,
            guard,
        )
    }

    /// Resolve the latest state-anchor through the ordinary event projection,
    /// then verify the exact event object from CAS before exposing it as
    /// checkpoint authority.
    pub fn latest_verified_state_anchor(
        &self,
        thread_id: &str,
    ) -> Result<Option<(String, ryeos_state::objects::StateAnchorMilestone)>> {
        let g = self.lock()?;
        let Some(row) = g.state_db.latest_state_anchor_event(thread_id)? else {
            return Ok(None);
        };
        let value = self
            .state_authority
            .cas_store()?
            .get_object(&row.event_hash)?
            .ok_or_else(|| anyhow!("projected state-anchor event is missing from CAS"))?;
        let event: ryeos_state::objects::ThreadEvent = serde_json::from_value(value)?;
        event.validate()?;
        if event.thread_id != row.thread_id
            || event.chain_root_id != row.chain_root_id
            || event.chain_seq != u64::try_from(row.chain_seq)?
            || event.thread_seq != u64::try_from(row.thread_seq)?
            || event.event_type != ryeos_state::event_types::MILESTONE
            || event.durability != ryeos_state::objects::EventDurability::Durable
            || lillux::sha256_hex(lillux::canonical_json(&event.to_value())?.as_bytes())
                != row.event_hash
        {
            bail!("projected state-anchor row contradicts its authoritative CAS event");
        }
        let anchor = ryeos_state::objects::StateAnchorMilestone::from_value(event.payload)?;
        Ok(Some((row.event_hash, anchor)))
    }

    pub fn latest_verified_chain_state_anchor(
        &self,
        chain_root_id: &str,
    ) -> Result<Option<(String, ryeos_state::objects::StateAnchorMilestone)>> {
        let g = self.lock()?;
        let Some(row) = g.state_db.latest_chain_state_anchor_event(chain_root_id)? else {
            return Ok(None);
        };
        if row.chain_root_id != chain_root_id {
            bail!("projected chain state anchor escaped its requested chain");
        }
        let value = self
            .state_authority
            .cas_store()?
            .get_object(&row.event_hash)?
            .ok_or_else(|| anyhow!("projected chain state-anchor event is missing from CAS"))?;
        let event: ryeos_state::objects::ThreadEvent = serde_json::from_value(value)?;
        event.validate()?;
        if event.thread_id != row.thread_id
            || event.chain_root_id != row.chain_root_id
            || event.chain_seq != u64::try_from(row.chain_seq)?
            || event.thread_seq != u64::try_from(row.thread_seq)?
            || event.event_type != ryeos_state::event_types::MILESTONE
            || event.durability != ryeos_state::objects::EventDurability::Durable
            || lillux::sha256_hex(lillux::canonical_json(&event.to_value())?.as_bytes())
                != row.event_hash
        {
            bail!("projected chain state-anchor row contradicts its authoritative CAS event");
        }
        let anchor = ryeos_state::objects::StateAnchorMilestone::from_value(event.payload)?;
        Ok(Some((row.event_hash, anchor)))
    }

    pub fn release_project_head_fence(&self, reservation_id: &str) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.release_project_head_fence(reservation_id)
    }

    pub fn write_project_head_ref(
        &self,
        principal_key: &str,
        project_hash: &str,
        project_snapshot_hash: &str,
        signer: &dyn Signer,
        cas_mutation_guard: &ryeos_state::recovery::CasMutationGuard,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db
            .require_project_head_mutation_unfenced(principal_key, project_hash)?;
        g.state_db.write_project_head_ref(
            principal_key,
            project_hash,
            project_snapshot_hash,
            signer,
            cas_mutation_guard,
        )
    }

    pub fn advance_project_head_ref(
        &self,
        principal_key: &str,
        project_hash: &str,
        new_snapshot_hash: &str,
        expected_current_hash: &str,
        signer: &dyn Signer,
        cas_mutation_guard: &ryeos_state::recovery::CasMutationGuard,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db
            .require_project_head_mutation_unfenced(principal_key, project_hash)?;
        g.state_db.advance_project_head_ref(
            principal_key,
            project_hash,
            new_snapshot_hash,
            expected_current_hash,
            signer,
            cas_mutation_guard,
        )
    }

    pub fn advance_project_head_ref_owned(
        &self,
        thread_id: &str,
        launch_owner: &str,
        principal_key: &str,
        project_hash: &str,
        new_snapshot_hash: &str,
        expected_current_hash: &str,
        signer: &dyn Signer,
        cas_mutation_guard: &ryeos_state::recovery::CasMutationGuard,
    ) -> Result<()> {
        let g = self.lock()?;
        let claim = g
            .runtime_db
            .get_launch_claim(thread_id)?
            .ok_or_else(|| anyhow!("thread {thread_id} has no current launch owner"))?;
        if claim.claimed_by != launch_owner {
            bail!("stale launch owner cannot publish state for thread {thread_id}");
        }
        g.runtime_db
            .require_project_head_mutation_unfenced(principal_key, project_hash)?;
        g.state_db.advance_project_head_ref(
            principal_key,
            project_hash,
            new_snapshot_hash,
            expected_current_hash,
            signer,
            cas_mutation_guard,
        )
    }

    /// Run online project compaction while preventing any handoff from
    /// acquiring a target project-HEAD fence. Compaction still owns the
    /// cross-process CAS guard and write barrier at its outer maintenance
    /// boundary; this mutex closes the RuntimeDb reservation race.
    pub fn with_project_head_compaction_authority<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce() -> Result<T>,
    {
        let g = self.lock()?;
        g.runtime_db.require_no_active_project_head_fences()?;
        f()
    }

    /// Immutable project snapshots required by active or queued runtimes.
    ///
    /// These runtime-DB references are not signed CAS heads, so online GC must
    /// add them as daemon-authoritative transient roots. Retain both fields
    /// defensively if a record carries both; resume selection intentionally
    /// chooses one, while reachability must not collect either active pin.
    pub fn active_resume_snapshot_roots(&self) -> Result<Vec<String>> {
        let g = self.lock()?;
        let statuses = [
            ThreadStatus::Created.as_str(),
            ThreadStatus::Running.as_str(),
        ];
        let rows = queries::list_threads_by_status(g.state_db.projection(), &statuses)?;
        let mut roots = std::collections::BTreeSet::new();
        for row in rows {
            let Some(metadata) = g
                .runtime_db
                .get_runtime_info(&row.thread_id)?
                .and_then(|info| info.launch_metadata)
            else {
                continue;
            };
            let Some(resume) = metadata.resume_context else {
                continue;
            };
            if let Some(hash) = resume.original_snapshot_hash {
                roots.insert(hash);
            }
            if let Some(pushed) = resume.original_pushed_head_ref {
                roots.insert(pushed.snapshot_hash);
            }
        }
        // Workspace reservation precedes thread birth so a crash cannot leave
        // an unjournaled mount. Those lower generations are therefore not
        // always reachable through authoritative thread history yet. Keep
        // every non-closed journal generation as an operational GC root until
        // verified backend cleanup closes the record.
        for workspace in g.runtime_db.open_workspaces()? {
            roots.insert(workspace.base_snapshot);
            if let Some(capture) = workspace.base_output_capture_hash {
                roots.insert(capture);
            }
            if let Some(frozen) = workspace.frozen_snapshot_hash {
                roots.insert(frozen);
            }
            if let Some(capture) = workspace.frozen_output_capture_hash {
                roots.insert(capture);
            }
        }
        roots.extend(g.runtime_db.retained_candidate_snapshot_roots()?);
        roots.extend(g.runtime_db.runtime_child_cas_object_roots()?);
        roots.extend(g.runtime_db.process_resource_cas_roots()?);
        roots.extend(g.runtime_db.external_execution_cas_roots()?);
        roots.extend(g.runtime_db.scoped_child_observation_cas_roots()?);
        roots.extend(g.runtime_db.snapshot_bootstrap_lifecycle_roots()?);
        Ok(roots.into_iter().collect())
    }

    /// Read a thread's persisted launch metadata (resume context), if any.
    pub fn get_launch_metadata(
        &self,
        thread_id: &str,
    ) -> Result<Option<crate::launch_metadata::RuntimeLaunchMetadata>> {
        let g = self.lock()?;
        Ok(g.runtime_db
            .get_runtime_info(thread_id)?
            .and_then(|info| info.launch_metadata))
    }
}
