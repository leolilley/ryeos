//! Event append, replay, read pages and bundle events.
use super::*;

impl StateStore {
    #[tracing::instrument(
        name = "state:append_events",
        skip(self, events),
        fields(
            thread_id = %thread_id,
            chain_root_id = %chain_root_id,
            event_count = events.len(),
        )
    )]
    pub fn append_events(
        &self,
        chain_root_id: &str,
        thread_id: &str,
        events: &[NewEventRecord],
    ) -> Result<Vec<PersistedEventRecord>> {
        let has_cas_events = events
            .iter()
            .any(|event| event.storage_class != "ephemeral");
        let permit = if has_cas_events {
            Some(self.acquire_write_permit()?)
        } else {
            None
        };
        let g = self.lock()?;
        if has_indexed_collection_events(events) && !self.projection_health.is_current() {
            bail!("collection event admission requires a current thread projection");
        }
        append_events_locked(
            &g,
            permit.as_ref().map(StateMutationPermit::cas_guard),
            chain_root_id,
            thread_id,
            events,
        )
    }

    #[tracing::instrument(
        name = "state:append_events_if_thread_running",
        skip(self, events),
        fields(
            thread_id = %thread_id,
            chain_root_id = %chain_root_id,
            event_count = events.len(),
        )
    )]
    pub fn append_events_if_thread_running(
        &self,
        chain_root_id: &str,
        thread_id: &str,
        events: &[NewEventRecord],
    ) -> Result<Option<Vec<PersistedEventRecord>>> {
        self.append_events_if_thread_running_inner(chain_root_id, thread_id, events, None)
    }

    /// Process-observer admission must fence the exact launch owner under the
    /// same lock as the journal append. A payload's launch_id is testimony,
    /// not permission for a stale observer to write into a successor launch.
    pub fn append_events_if_thread_running_owned(
        &self,
        chain_root_id: &str,
        thread_id: &str,
        events: &[NewEventRecord],
        launch_owner: &str,
    ) -> Result<Option<Vec<PersistedEventRecord>>> {
        self.append_events_if_thread_running_inner(
            chain_root_id,
            thread_id,
            events,
            Some(launch_owner),
        )
    }

    pub(super) fn append_events_if_thread_running_inner(
        &self,
        chain_root_id: &str,
        thread_id: &str,
        events: &[NewEventRecord],
        launch_owner: Option<&str>,
    ) -> Result<Option<Vec<PersistedEventRecord>>> {
        let has_cas_events = events
            .iter()
            .any(|event| event.storage_class != "ephemeral");
        let permit = if has_cas_events {
            Some(self.acquire_write_permit()?)
        } else {
            None
        };
        let g = self.lock()?;
        if has_indexed_collection_events(events) && !self.projection_health.is_current() {
            bail!("collection event admission requires a current thread projection");
        }
        let Some(thread) = g.state_db.get_thread(thread_id)? else {
            return Ok(None);
        };
        if thread.status != ThreadStatus::Running.as_str() {
            return Ok(None);
        }
        if let Some(expected) = launch_owner {
            let claim = g.runtime_db.get_launch_claim(thread_id)?;
            if claim.is_none_or(|claim| claim.claimed_by != expected) {
                return Ok(None);
            }
        }
        let runtime = g
            .runtime_db
            .get_runtime_info(thread_id)?
            .ok_or_else(|| anyhow!("runtime row missing while appending events: {thread_id}"))?;
        if runtime.stop_intent.is_some()
            || !self
                .process_attachment_admission_open
                .load(Ordering::Acquire)
        {
            return Ok(None);
        }

        append_events_locked(
            &g,
            permit.as_ref().map(StateMutationPermit::cas_guard),
            chain_root_id,
            thread_id,
            events,
        )
        .map(Some)
    }

    /// Append the exact daemon-authored launch-attempt audit immediately before
    /// a claimed managed-runtime spawn. Unlike the runtime callback event
    /// boundary, this deliberately admits a still-`created` row: continuation
    /// and follow successors must remain `created` until their process is
    /// attached, but their recomputed attempt authority must be durable before
    /// the spawn handoff becomes observable.
    ///
    /// Every lifecycle and operational guard is re-checked under the same state
    /// lock as the append. This remains a daemon-only boundary; runtime-authored
    /// events continue to require an already-`running` thread through
    /// [`Self::append_events_if_thread_running`].
    #[tracing::instrument(
        name = "state:append_launch_attempt_audit",
        skip(self, events),
        fields(
            thread_id = %thread_id,
            chain_root_id = %chain_root_id,
            event_count = events.len(),
        )
    )]
    pub(crate) fn append_launch_attempt_audit(
        &self,
        chain_root_id: &str,
        thread_id: &str,
        events: &[NewEventRecord],
    ) -> Result<Vec<PersistedEventRecord>> {
        validate_launch_attempt_audit(events)?;
        let permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let thread = g
            .state_db
            .get_thread(thread_id)?
            .ok_or_else(|| anyhow!("launch attempt audit thread not found: {thread_id}"))?;
        if thread.chain_root_id != chain_root_id {
            bail!(
                "launch attempt audit chain mismatch for {thread_id}: expected {}, received {chain_root_id}",
                thread.chain_root_id
            );
        }
        if !matches!(
            ThreadStatus::from_str_lossy(&thread.status),
            Some(ThreadStatus::Created | ThreadStatus::Running)
        ) {
            bail!(
                "launch attempt audit requires a created or running thread; {thread_id} is '{}'",
                thread.status
            );
        }
        if !self
            .process_attachment_admission_open
            .load(Ordering::Acquire)
        {
            bail!("launch attempt audit refused: process attachment admission is closed");
        }
        let runtime = g.runtime_db.get_runtime_info(thread_id)?.ok_or_else(|| {
            anyhow!("runtime row missing while appending launch audit: {thread_id}")
        })?;
        if let Some(intent) = runtime.stop_intent {
            bail!(
                "launch attempt audit refused: thread {thread_id} has durable stop intent '{}'",
                intent.as_str()
            );
        }
        if runtime.pid.is_some() || runtime.pgid.is_some() || runtime.process_identity.is_some() {
            bail!(
                "launch attempt audit refused: thread {thread_id} already has a process attachment"
            );
        }
        if g.runtime_db.get_launch_claim(thread_id)?.is_none() {
            bail!("launch attempt audit refused: thread {thread_id} has no active launch claim");
        }

        append_events_locked(
            &g,
            Some(permit.cas_guard()),
            chain_root_id,
            thread_id,
            events,
        )
    }

    /// The thread a live tail of `chain_root_id` should currently follow: the
    /// owner of the chain's highest-`chain_seq` event. `None` when the chain
    /// has no events yet.
    pub fn chain_head_thread(&self, chain_root_id: &str) -> Result<Option<String>> {
        let g = self.lock()?;
        queries::chain_head_thread(g.state_db.projection(), chain_root_id)
    }

    pub fn replay_events(
        &self,
        chain_root_id: &str,
        thread_id: Option<&str>,
        after_seq: Option<i64>,
        limit: usize,
        max_serialized_bytes: usize,
    ) -> Result<PersistedEventPage> {
        let g = self.lock()?;
        let page = queries::replay_events_bounded(
            g.state_db.projection(),
            chain_root_id,
            thread_id,
            after_seq,
            limit,
            max_serialized_bytes,
        )?;
        drop(g);
        let events = page
            .rows
            .into_iter()
            .map(persisted_event_from_projection_row)
            .collect::<Result<Vec<_>>>()?;
        Ok(PersistedEventPage {
            events,
            has_more: page.has_more,
        })
    }

    pub fn replay_events_through(
        &self,
        chain_root_id: &str,
        thread_id: Option<&str>,
        after_seq: Option<i64>,
        through_seq: i64,
        limit: usize,
        max_serialized_bytes: usize,
    ) -> Result<PersistedEventPage> {
        let g = self.lock()?;
        let page = queries::replay_events_through_bounded(
            g.state_db.projection(),
            chain_root_id,
            thread_id,
            after_seq,
            through_seq,
            limit,
            max_serialized_bytes,
        )?;
        drop(g);
        let events = page
            .rows
            .into_iter()
            .map(persisted_event_from_projection_row)
            .collect::<Result<Vec<_>>>()?;
        Ok(PersistedEventPage {
            events,
            has_more: page.has_more,
        })
    }

    /// Locate a recent bounded thread page for an execution Field. The rows
    /// remain projection candidates until the caller authenticates them from
    /// the fixed signed thread history. This deliberately takes only the store
    /// lock and reads SQL rows: it never touches CAS, so it holds no CAS guard.
    pub fn recent_thread_events(
        &self,
        chain_root_id: &str,
        thread_id: &str,
        through_seq: Option<i64>,
        limit: usize,
        max_serialized_bytes: usize,
    ) -> Result<PersistedEventPage> {
        let g = self.lock()?;
        let page = queries::recent_thread_events_bounded(
            g.state_db.projection(),
            chain_root_id,
            thread_id,
            through_seq,
            limit,
            max_serialized_bytes,
        )?;
        drop(g);
        let events = page
            .rows
            .into_iter()
            .map(persisted_event_from_projection_row)
            .collect::<Result<Vec<_>>>()?;
        Ok(PersistedEventPage {
            events,
            has_more: page.has_more,
        })
    }

    /// Capture one opaque, signed-head-derived endpoint for bounded history
    /// reads. The chain lock and CAS guard are held only during this capture.
    pub fn capture_thread_event_read_anchor(
        &self,
        chain_root_id: &str,
        thread_id: &str,
    ) -> Result<Option<ryeos_state::AuthoritativeThreadEventReadAnchor>> {
        let guard = self.state_authority.acquire_shared_guard()?;
        let g = self.lock()?;
        g.state_db
            .capture_thread_event_read_anchor(chain_root_id, thread_id, &guard)
            .context("capture authoritative thread event read anchor")
    }

    /// Read one newest-first bounded slice from a fixed authenticated endpoint.
    /// `progress: None` starts at the anchor tip; a returned `Some` token means
    /// the authenticated interval is incomplete, never that an event is absent.
    pub fn read_thread_event_page(
        &self,
        anchor: &ryeos_state::AuthoritativeThreadEventReadAnchor,
        progress: Option<&ryeos_state::AuthoritativeThreadEventReadProgress>,
        max_events: u64,
        max_bytes: u64,
    ) -> Result<ryeos_state::AuthoritativeThreadEventReadPage> {
        let guard = self.state_authority.acquire_shared_guard()?;
        let g = self.lock()?;
        g.state_db
            .read_thread_event_page(anchor, progress, max_events, max_bytes, &guard)
            .context("read bounded authoritative thread event page")
    }

    /// Authenticate one bounded projection-located execution page against
    /// fixed signed thread histories. Projection fields are only search
    /// coordinates: every returned record is rebuilt from CAS, and a
    /// contradictory projected field refuses the whole page. The per-thread
    /// history walk advances in small pages without a guessed seat/history
    /// lifetime ceiling or a lock held across the complete walk.
    pub fn authenticate_execution_event_candidates(
        &self,
        chain_root_id: &str,
        candidates: &[PersistedEventRecord],
    ) -> Result<Vec<PersistedEventRecord>> {
        const MAX_CANDIDATES: usize = 2_000;
        const MAX_CANDIDATE_BYTES: usize = 4 * 1024 * 1024;
        const PAGE_EVENTS: u64 = 64;
        const PAGE_BYTES: u64 = 2 * 1024 * 1024;

        if candidates.len() > MAX_CANDIDATES {
            bail!("execution event candidate page exceeds its item bound");
        }
        let mut encoded_bytes = 0usize;
        let mut by_thread = BTreeMap::<&str, BTreeMap<&str, usize>>::new();
        for (index, candidate) in candidates.iter().enumerate() {
            encoded_bytes = encoded_bytes
                .checked_add(serde_json::to_vec(candidate)?.len())
                .ok_or_else(|| anyhow!("execution event candidate byte count overflow"))?;
            if encoded_bytes > MAX_CANDIDATE_BYTES {
                bail!("execution event candidate page exceeds its byte bound");
            }
            let Some(hash) = candidate.event_hash.as_deref() else {
                bail!("execution event candidate has no CAS hash");
            };
            if candidate.chain_root_id != chain_root_id
                || candidate.thread_id.is_empty()
                || candidate.chain_seq < 1
                || candidate.thread_seq < 1
                || !lillux::valid_hash(hash)
                || hash.bytes().any(|byte| byte.is_ascii_uppercase())
                || by_thread
                    .entry(&candidate.thread_id)
                    .or_default()
                    .insert(hash, index)
                    .is_some()
            {
                bail!("execution event candidate has contradictory coordinates");
            }
        }
        let mut authenticated = vec![None; candidates.len()];
        for (thread_id, pending) in &mut by_thread {
            let anchor = self
                .capture_thread_event_read_anchor(chain_root_id, thread_id)?
                .ok_or_else(|| anyhow!("execution event candidate thread is absent"))?;
            let mut progress = None;
            loop {
                let page = self.read_thread_event_page(
                    &anchor,
                    progress.as_ref(),
                    PAGE_EVENTS,
                    PAGE_BYTES,
                )?;
                for (hash, event) in page.events {
                    if let Some(index) = pending.remove(hash.as_str()) {
                        let signed = persisted_event_from_authoritative(hash, event)?;
                        if !execution_event_candidate_matches(&candidates[index], &signed) {
                            bail!("execution event projection contradicts signed content");
                        }
                        authenticated[index] = Some(signed);
                    }
                }
                progress = page.progress;
                if progress.is_none() || pending.is_empty() {
                    break;
                }
            }
            if !pending.is_empty() {
                bail!("execution event candidate is absent from signed thread history");
            }
        }
        authenticated
            .into_iter()
            .map(|record| record.ok_or_else(|| anyhow!("execution event proof is incomplete")))
            .collect()
    }

    /// Authenticate the small current/previous/next/live-head navigation set
    /// against each exact thread's fixed signed endpoint. The projected refs
    /// are locators only; returned refs are rebuilt from CAS events.
    pub fn authenticate_execution_event_refs(
        &self,
        chain_root_id: &str,
        candidates: &[PersistedEventRef],
    ) -> Result<Vec<PersistedEventRef>> {
        const MAX_REFS: usize = 4;
        const PAGE_EVENTS: u64 = 64;
        const PAGE_BYTES: u64 = 2 * 1024 * 1024;
        if candidates.len() > MAX_REFS {
            bail!("execution event navigation exceeds its reference bound");
        }
        let mut by_thread = BTreeMap::<&str, BTreeMap<&str, Vec<usize>>>::new();
        for (index, candidate) in candidates.iter().enumerate() {
            if candidate.chain_root_id != chain_root_id
                || candidate.chain_seq < 1
                || candidate.thread_id.is_empty()
                || !lillux::valid_hash(&candidate.event_hash)
                || candidate
                    .event_hash
                    .bytes()
                    .any(|byte| byte.is_ascii_uppercase())
            {
                bail!("execution event navigation has contradictory coordinates");
            }
            let positions = by_thread
                .entry(&candidate.thread_id)
                .or_default()
                .entry(&candidate.event_hash)
                .or_default();
            if positions
                .first()
                .is_some_and(|first| &candidates[*first] != candidate)
            {
                bail!("execution event navigation repeats a hash with different coordinates");
            }
            positions.push(index);
        }
        let mut authenticated = vec![None; candidates.len()];
        for (thread_id, pending) in &mut by_thread {
            let anchor = self
                .capture_thread_event_read_anchor(chain_root_id, thread_id)?
                .ok_or_else(|| anyhow!("execution event navigation thread is absent"))?;
            let mut progress = None;
            loop {
                let page = self.read_thread_event_page(
                    &anchor,
                    progress.as_ref(),
                    PAGE_EVENTS,
                    PAGE_BYTES,
                )?;
                for (hash, event) in page.events {
                    if let Some(positions) = pending.remove(hash.as_str()) {
                        let candidate = &candidates[positions[0]];
                        let chain_seq = i64::try_from(event.chain_seq)
                            .context("signed navigation chain sequence exceeds storage range")?;
                        if event.chain_root_id != candidate.chain_root_id
                            || event.thread_id != candidate.thread_id
                            || chain_seq != candidate.chain_seq
                        {
                            bail!("execution event navigation contradicts signed content");
                        }
                        let signed = PersistedEventRef {
                            chain_root_id: event.chain_root_id,
                            chain_seq,
                            event_hash: hash,
                            thread_id: event.thread_id,
                        };
                        for index in positions {
                            authenticated[index] = Some(signed.clone());
                        }
                    }
                }
                progress = page.progress;
                if progress.is_none() || pending.is_empty() {
                    break;
                }
            }
            if !pending.is_empty() {
                bail!("execution event navigation is absent from signed thread history");
            }
        }
        authenticated
            .into_iter()
            .map(|record| {
                record.ok_or_else(|| anyhow!("execution event navigation proof is incomplete"))
            })
            .collect()
    }

    /// Prove every projection-located execution-tree edge from signed child
    /// identity and a signed parent event. A child snapshot's upstream link
    /// alone does not prove the source decision; a projected edge/reason alone
    /// is never authority. Walk parent histories through bounded CAS pages and
    /// release each page guard before continuing. No total-history ceiling is
    /// inferred from the UI node limit.
    pub fn verify_execution_tree_edges(&self, edges: &[ExecutionTreeEdgeCandidate]) -> Result<()> {
        const MAX_EDGES: usize = 1_000;
        const PAGE_EVENTS: u64 = 64;
        const PAGE_BYTES: u64 = 2 * 1024 * 1024;
        if edges.len() > MAX_EDGES {
            bail!("execution tree edge proof exceeds its node bound");
        }
        let mut by_parent =
            BTreeMap::<(&str, &str), BTreeMap<&str, Vec<&ExecutionTreeEdgeCandidate>>>::new();
        for edge in edges {
            if edge.parent_thread_id.is_empty()
                || edge.child_thread_id.is_empty()
                || edge.parent_thread_id == edge.child_thread_id
                || edge.parent_chain_root_id.is_empty()
                || edge.child_chain_root_id.is_empty()
                || !matches!(edge.relation.as_str(), "continued" | "spawned" | "follow")
            {
                bail!("execution tree edge candidate is malformed");
            }
            let (parent, child) = {
                let g = self.lock()?;
                let parent = g
                    .state_db
                    .read_authoritative_thread_snapshot(
                        &edge.parent_chain_root_id,
                        &edge.parent_thread_id,
                    )?
                    .ok_or_else(|| anyhow!("execution tree parent is absent from signed state"))?;
                let child = g
                    .state_db
                    .read_authoritative_thread_snapshot(
                        &edge.child_chain_root_id,
                        &edge.child_thread_id,
                    )?
                    .ok_or_else(|| anyhow!("execution tree child is absent from signed state"))?;
                (parent, child)
            };
            if parent.thread_id != edge.parent_thread_id
                || parent.chain_root_id != edge.parent_chain_root_id
                || child.thread_id != edge.child_thread_id
                || child.chain_root_id != edge.child_chain_root_id
            {
                bail!("execution tree edge identity contradicts signed snapshots");
            }
            let continued = child.upstream_thread_id.as_deref()
                == Some(edge.parent_thread_id.as_str())
                && edge.parent_chain_root_id == edge.child_chain_root_id;
            if (edge.relation == "continued") != continued {
                bail!("execution tree edge relation contradicts signed child lineage");
            }
            let matching = by_parent
                .entry((
                    edge.parent_chain_root_id.as_str(),
                    edge.parent_thread_id.as_str(),
                ))
                .or_default()
                .entry(edge.child_thread_id.as_str())
                .or_default();
            if matching.first().is_some_and(|first| {
                first.relation != edge.relation
                    || first.child_chain_root_id != edge.child_chain_root_id
            }) {
                bail!("execution tree repeats an edge with contradictory identity");
            }
            matching.push(edge);
        }
        // One authenticated traversal serves every candidate child of this
        // parent. A wide fan-out must not rescan the parent's whole history
        // once per child or retain unbounded event pages in memory.
        for ((parent_chain_root_id, parent_thread_id), mut pending) in by_parent {
            let anchor = self
                .capture_thread_event_read_anchor(parent_chain_root_id, parent_thread_id)?
                .ok_or_else(|| anyhow!("execution tree parent has no signed event anchor"))?;
            let mut progress = None;
            loop {
                let page = self.read_thread_event_page(
                    &anchor,
                    progress.as_ref(),
                    PAGE_EVENTS,
                    PAGE_BYTES,
                )?;
                for (_, event) in page.events {
                    let key = match event.event_type.as_str() {
                        ryeos_state::event_types::THREAD_CONTINUED => "successor_thread_id",
                        ryeos_state::event_types::CHILD_THREAD_SPAWNED => "child_thread_id",
                        _ => continue,
                    };
                    let Some(child_id) = event.payload.get(key).and_then(Value::as_str) else {
                        continue;
                    };
                    let Some(matching) = pending.get(child_id) else {
                        continue;
                    };
                    let signed_relation =
                        if event.event_type == ryeos_state::event_types::THREAD_CONTINUED {
                            "continued"
                        } else if event.payload.get("spawn_reason").and_then(Value::as_str)
                            == Some("follow")
                        {
                            "follow"
                        } else {
                            "spawned"
                        };
                    if matching.iter().any(|edge| edge.relation != signed_relation) {
                        bail!("execution tree relation contradicts signed parent event");
                    }
                    pending.remove(child_id);
                }
                progress = page.progress;
                if pending.is_empty() || progress.is_none() {
                    break;
                }
            }
            if !pending.is_empty() {
                bail!("execution tree edge has no signed parent decision");
            }
        }
        Ok(())
    }

    pub fn chain_event_navigation(
        &self,
        chain_root_id: &str,
        chain_seq: i64,
    ) -> Result<PersistedEventNavigation> {
        fn convert(row: ryeos_state::queries::ChainEventRefRow) -> PersistedEventRef {
            PersistedEventRef {
                chain_root_id: row.chain_root_id,
                chain_seq: row.chain_seq,
                event_hash: row.event_hash,
                thread_id: row.thread_id,
            }
        }

        let g = self.lock()?;
        let navigation =
            queries::chain_event_navigation(g.state_db.projection(), chain_root_id, chain_seq)?;
        Ok(PersistedEventNavigation {
            previous: navigation.previous.map(convert),
            current: navigation.current.map(convert),
            next: navigation.next.map(convert),
            live_head: navigation.live_head.map(convert),
        })
    }

    /// Latest durable events across every thread on the node — the feed
    /// behind the node activity lens. `exclude_types` is caller-declared
    /// (content decides what counts as noise), never a baked-in vocabulary.
    pub fn latest_node_events(
        &self,
        limit: usize,
        exclude_types: &[String],
    ) -> Result<Vec<PersistedEventRecord>> {
        let g = self.lock()?;
        let event_rows =
            queries::latest_node_events(g.state_db.projection(), limit, exclude_types)?;
        event_rows
            .into_iter()
            .map(persisted_event_from_projection_row)
            .collect::<Result<Vec<_>>>()
    }

    pub fn latest_thread_events(
        &self,
        thread_id: &str,
        limit: usize,
    ) -> Result<Vec<PersistedEventRecord>> {
        let g = self.lock()?;
        let event_rows = queries::latest_thread_events(g.state_db.projection(), thread_id, limit)?;
        event_rows
            .into_iter()
            .map(persisted_event_from_projection_row)
            .collect::<Result<Vec<_>>>()
    }

    pub fn latest_thread_event_by_type(
        &self,
        thread_id: &str,
        event_type: &str,
    ) -> Result<Option<PersistedEventRecord>> {
        let g = self.lock()?;
        let row =
            queries::latest_thread_event_by_type(g.state_db.projection(), thread_id, event_type)?;
        row.map(persisted_event_from_projection_row).transpose()
    }

    pub fn latest_thread_event_by_type_prefix(
        &self,
        thread_id: &str,
        event_type_prefix: &str,
    ) -> Result<Option<PersistedEventRecord>> {
        let g = self.lock()?;
        let row = queries::latest_thread_event_by_type_prefix(
            g.state_db.projection(),
            thread_id,
            event_type_prefix,
        )?;
        row.map(persisted_event_from_projection_row).transpose()
    }

    pub fn thread_events_by_type_operation_id(
        &self,
        thread_id: &str,
        event_type: &str,
        operation_id: &str,
    ) -> Result<Vec<PersistedEventRecord>> {
        let g = self.lock()?;
        queries::thread_events_by_type_operation_id(
            g.state_db.projection(),
            thread_id,
            event_type,
            operation_id,
        )?
        .into_iter()
        .map(persisted_event_from_projection_row)
        .collect()
    }

    pub fn append_bundle_event(
        &self,
        request: ryeos_state::BundleEventAppendRequest,
    ) -> Result<ryeos_state::BundleEventAppendResult> {
        let permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        g.state_db
            .append_bundle_event_admitted(request, g.signer.as_ref(), permit.cas_guard())
    }

    /// Append a daemon-owned event at the authoritative head selected while
    /// holding the state-store lock.
    ///
    /// This is intentionally crate-private: capability-facing bundle-event
    /// callers must continue to supply an explicit expected head. Internal
    /// evidence streams whose only writer is this daemon need a single atomic
    /// read-head/append operation so concurrent launches cannot create
    /// avoidable evidence gaps.
    pub(crate) fn append_daemon_bundle_event_at_current_head(
        &self,
        mut request: ryeos_state::BundleEventAppendRequest,
    ) -> Result<ryeos_state::BundleEventAppendResult> {
        let permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let bundle_id = request
            .bundle_id
            .as_deref()
            .unwrap_or(&request.effective_bundle_id);
        let head = g.state_db.read_bundle_event_chain_page(
            bundle_id,
            &request.event_kind,
            &request.chain_id,
            None,
            1,
            ryeos_state::objects::MAX_BUNDLE_EVENT_SERIALIZED_BYTES,
            g.signer.as_ref(),
        )?;
        request.expected_chain_head_hash =
            head.records.first().map(|record| record.event_hash.clone());
        g.state_db
            .append_bundle_event_admitted(request, g.signer.as_ref(), permit.cas_guard())
    }

    pub fn append_bundle_event_with_attachments(
        &self,
        mut request: ryeos_state::BundleEventAppendRequest,
        attachments: Vec<NewBundleEventAttachment>,
    ) -> Result<ryeos_state::BundleEventAppendResult> {
        let permit = self.acquire_write_permit()?;
        self.state_authority.ensure_guard(permit.cas_guard())?;
        let cas = self.state_authority.cas_store()?;
        request.attachments = attachments
            .into_iter()
            .map(|attachment| {
                let size_bytes = u64::try_from(attachment.bytes.len())
                    .map_err(|_| anyhow!("bundle event attachment size does not fit u64"))?;
                let stored = cas.put_blob(&attachment.bytes).with_context(|| {
                    format!("store bundle event attachment '{}' in CAS", attachment.name)
                })?;
                Ok(ryeos_state::BundleEventAttachment {
                    name: attachment.name,
                    blob_hash: stored.hash,
                    size_bytes,
                    media_type: attachment.media_type,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let g = self.lock()?;
        g.state_db
            .append_bundle_event_admitted(request, g.signer.as_ref(), permit.cas_guard())
    }

    pub fn read_bundle_event_attachment(
        &self,
        event_hash: &str,
        expected_bundle_id: &str,
        expected_event_kind: &str,
        attachment_name: &str,
    ) -> Result<(
        ryeos_state::BundleEventRecord,
        ryeos_state::BundleEventAttachment,
        Vec<u8>,
    )> {
        let record = {
            let g = self.lock()?;
            g.state_db.read_bundle_event_by_hash(event_hash)?
        };
        if record.event.bundle_id != expected_bundle_id
            || record.event.event_kind != expected_event_kind
        {
            bail!(
                "bundle event attachment identity mismatch: expected {expected_bundle_id}/{expected_event_kind}, event belongs to {}/{}",
                record.event.bundle_id,
                record.event.event_kind
            );
        }
        let attachment = record
            .event
            .attachments
            .iter()
            .find(|attachment| attachment.name == attachment_name)
            .cloned()
            .ok_or_else(|| {
                anyhow!("bundle event {event_hash} has no attachment named {attachment_name:?}")
            })?;
        let cas = self.state_authority.cas_store()?;
        let bytes = cas
            .get_blob(&attachment.blob_hash)?
            .ok_or_else(|| anyhow!("bundle event attachment blob is missing"))?;
        if bytes.len() as u64 != attachment.size_bytes {
            bail!(
                "bundle event attachment '{}' size mismatch: event says {}, CAS contains {}",
                attachment.name,
                attachment.size_bytes,
                bytes.len()
            );
        }
        Ok((record, attachment, bytes))
    }

    pub fn read_bundle_event_chain_page(
        &self,
        bundle_id: &str,
        event_kind: &str,
        chain_id: &str,
        cursor: Option<&ryeos_state::BundleEventCursor>,
        limit: usize,
        max_serialized_bytes: usize,
    ) -> Result<ryeos_state::BundleEventChainPage> {
        let g = self.lock()?;
        g.state_db.read_bundle_event_chain_page(
            bundle_id,
            event_kind,
            chain_id,
            cursor,
            limit,
            max_serialized_bytes,
            g.signer.as_ref(),
        )
    }

    pub fn scan_bundle_events_page(
        &self,
        bundle_id: &str,
        event_kind: &str,
        cursor: Option<&ryeos_state::BundleEventCursor>,
        limit: usize,
        max_serialized_bytes: usize,
    ) -> Result<ryeos_state::BundleEventScanPage> {
        let g = self.lock()?;
        g.state_db.scan_bundle_events_page(
            bundle_id,
            event_kind,
            cursor,
            limit,
            max_serialized_bytes,
            g.signer.as_ref(),
        )
    }
}
