//! Thread lists, details, results, usage and execution trees.
use super::*;

pub(super) fn thread_detail_from_committed_snapshot(
    snapshot: ThreadSnapshot,
    runtime: RuntimeInfo,
) -> ThreadDetail {
    let lifecycle_authority = runtime.launch_metadata.as_ref().and_then(|metadata| {
        if metadata.launch_driver
            == Some(ryeos_state::objects::ExecutionLaunchDriver::InProcessHandler)
        {
            metadata.in_process_lifecycle_authority
        } else {
            metadata
                .resume_context
                .as_ref()
                .map(|resume| resume.lifecycle_authority)
        }
    });
    ThreadDetail {
        thread_id: snapshot.thread_id,
        chain_root_id: snapshot.chain_root_id,
        kind: snapshot.kind_name,
        status: snapshot.status.as_str().to_string(),
        item_ref: snapshot.item_ref,
        executor_ref: snapshot.executor_ref,
        launch_mode: snapshot.launch_mode,
        current_site_id: snapshot.current_site_id,
        origin_site_id: snapshot.origin_site_id,
        upstream_thread_id: snapshot.upstream_thread_id,
        successor_thread_id: None,
        requested_by: snapshot.requested_by,
        project_root: snapshot
            .project_root
            .map(|path| path.to_string_lossy().into_owned()),
        project_authority: Some(snapshot.project_authority),
        result_project_snapshot_hash: snapshot.result_project_snapshot_hash,
        result_workspace_output_capture_hash: snapshot.result_workspace_output_capture_hash,
        lifecycle_authority,
        admitted_launch_capsule_hash: snapshot.admitted_launch_capsule_hash,
        created_at: snapshot.created_at,
        updated_at: snapshot.updated_at,
        started_at: snapshot.started_at,
        finished_at: snapshot.finished_at,
        runtime,
    }
}

impl StateStore {
    pub fn get_thread(&self, thread_id: &str) -> Result<Option<ThreadDetail>> {
        let g = self.lock()?;
        let thread_row = match g.state_db.get_thread(thread_id)? {
            Some(row) => row,
            None => return Ok(None),
        };

        let runtime = g
            .runtime_db
            .get_runtime_info(thread_id)?
            .unwrap_or_default();
        let lifecycle_authority = runtime
            .launch_metadata
            .as_ref()
            .map(|metadata| metadata.lifecycle_authority())
            .transpose()?
            .flatten();
        // The projection locates this exact thread. Read both project
        // coordinates from the same verified immutable snapshot, rather than
        // treating its SQLite result-hash column or a newer chain tip as proof.
        let snapshot = g
            .state_db
            .read_authoritative_thread_snapshot(&thread_row.chain_root_id, thread_id)?;
        let result_project_snapshot_hash = snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.result_project_snapshot_hash.clone());
        let result_workspace_output_capture_hash = snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.result_workspace_output_capture_hash.clone());
        let project_authority = snapshot.map(|snapshot| snapshot.project_authority);

        let successor_thread_id = if is_terminal_status(&thread_row.status) {
            queries::continuation_successor(g.state_db.projection(), thread_id)?
        } else {
            None
        };

        Ok(Some(ThreadDetail {
            thread_id: thread_row.thread_id,
            chain_root_id: thread_row.chain_root_id,
            kind: thread_row.kind,
            status: thread_row.status,
            item_ref: thread_row.item_ref,
            executor_ref: thread_row.executor_ref,
            launch_mode: thread_row.launch_mode,
            current_site_id: thread_row.current_site_id,
            origin_site_id: thread_row.origin_site_id,
            upstream_thread_id: thread_row.upstream_thread_id,
            successor_thread_id,
            requested_by: thread_row.requested_by,
            project_root: thread_row.project_root,
            project_authority: project_authority.clone(),
            result_project_snapshot_hash,
            result_workspace_output_capture_hash,
            lifecycle_authority,
            admitted_launch_capsule_hash: thread_row.admitted_launch_capsule_hash,
            created_at: thread_row.created_at,
            updated_at: thread_row.updated_at,
            started_at: thread_row.started_at,
            finished_at: thread_row.finished_at,
            runtime,
        }))
    }

    /// Resolve the authoritative continuation tip for one stable execution
    /// chain. Hosted-worker callers address the chain root; operational state
    /// is always fenced to the exact placement thread at this verified tip.
    pub fn current_chain_placement_thread_id(&self, chain_root_id: &str) -> Result<Option<String>> {
        let mut cursor = chain_root_id.to_owned();
        let mut visited = std::collections::HashSet::new();
        for _ in 0..1024 {
            if !visited.insert(cursor.clone()) {
                bail!("execution continuation lineage contains a cycle");
            }
            let Some(thread) = self.get_thread(&cursor)? else {
                return if cursor == chain_root_id {
                    Ok(None)
                } else {
                    bail!("execution continuation successor `{cursor}` disappeared")
                };
            };
            if thread.chain_root_id != chain_root_id {
                bail!("execution continuation escaped its stable chain root");
            }
            match thread.successor_thread_id {
                Some(successor_thread_id) => {
                    let successor = self
                        .get_thread(&successor_thread_id)?
                        .ok_or_else(|| anyhow!("execution continuation successor disappeared"))?;
                    if successor.chain_root_id != chain_root_id
                        || successor.upstream_thread_id.as_deref() != Some(cursor.as_str())
                    {
                        bail!("execution continuation successor has contradictory lineage");
                    }
                    cursor = successor_thread_id;
                }
                None => return Ok(Some(cursor)),
            }
        }
        bail!("execution continuation lineage exceeds its bounded depth")
    }

    /// Read the immutable generation pinned by authoritative signed history.
    /// Runtime metadata and projection paths are never consulted.
    pub fn authoritative_project_generation(
        &self,
        thread_id: &str,
    ) -> Result<Option<(Option<String>, Option<String>)>> {
        let g = self.lock()?;
        let Some(row) = g.state_db.get_thread(thread_id)? else {
            return Ok(None);
        };
        let snapshot =
            authoritative_snapshot_for_transition(&g, &row.chain_root_id, &row.thread_id)?;
        Ok(Some((
            snapshot.base_project_snapshot_hash,
            snapshot.result_project_snapshot_hash,
        )))
    }

    /// Exact terminal source/output pair from immutable thread authority. This
    /// does not follow a continuation or treat the launch generation as output.
    pub fn authoritative_result_generation(
        &self,
        thread_id: &str,
    ) -> Result<Option<ryeos_state::objects::WorkspaceGenerationPair>> {
        let g = self.lock()?;
        let row = g
            .state_db
            .get_thread(thread_id)?
            .ok_or_else(|| anyhow!("terminal result thread does not exist: {thread_id}"))?;
        let snapshot = authoritative_snapshot_for_transition(&g, &row.chain_root_id, thread_id)?;
        snapshot.verify_result_workspace_output_capture(&self.state_authority.cas_store()?)?;
        if snapshot.result_project_snapshot_hash.is_some() && !snapshot.status.is_terminal() {
            bail!("retained result generation requires an authoritative terminal thread");
        }
        Ok(snapshot.result_project_snapshot_hash.map(|snapshot_hash| {
            ryeos_state::objects::WorkspaceGenerationPair {
                snapshot_hash,
                output_capture_hash: snapshot.result_workspace_output_capture_hash,
            }
        }))
    }

    pub fn get_thread_result(&self, thread_id: &str) -> Result<Option<ThreadResultRecord>> {
        let result_row = {
            let g = self.lock()?;
            queries::get_thread_result(g.state_db.projection(), thread_id)?
        };
        let Some(row) = result_row else {
            return Ok(None);
        };
        // JSON parsing and exact response serialization happen after releasing
        // the global store mutex. The query has already bounded both source
        // columns before allocating them.
        let result = match row.result {
            Some(bytes) => Some(serde_json::from_slice::<Value>(&bytes).with_context(|| {
                format!(
                    "malformed JSON in thread_results.result for thread_id {}",
                    thread_id
                )
            })?),
            None => None,
        };
        let record = ThreadResultRecord {
            outcome_code: row.outcome_code,
            result,
            error: row
                .error
                .map(|error| {
                    serde_json::from_str::<Value>(&error).with_context(|| {
                        format!(
                            "malformed JSON in thread_results.error for thread_id {}",
                            thread_id
                        )
                    })
                })
                .transpose()?,
            metadata: None,
        };
        let response_bytes = serde_json::to_vec(&record)?.len();
        if response_bytes > MAX_THREAD_RESULT_RESPONSE_BYTES {
            bail!(
                "thread {thread_id} result response is {response_bytes} bytes; maximum is {MAX_THREAD_RESULT_RESPONSE_BYTES}"
            );
        }
        Ok(Some(record))
    }

    pub fn get_thread_terminal_authority(
        &self,
        thread_id: &str,
    ) -> Result<Option<ThreadTerminalAuthority>> {
        let g = self.lock()?;
        let Some(thread) = g.state_db.get_thread(thread_id)? else {
            return Ok(None);
        };
        let Some(status) = ThreadStatus::from_str_lossy(&thread.status) else {
            bail!("thread {thread_id} has unknown status `{}`", thread.status);
        };
        if !status.is_terminal() {
            return Ok(None);
        }

        let snapshot = g
            .state_db
            .read_authoritative_thread_snapshot(&thread.chain_root_id, thread_id)?
            .ok_or_else(|| anyhow!("terminal thread {thread_id} is missing its CAS snapshot"))?;
        let ThreadSnapshot {
            status: snapshot_status,
            result,
            error,
            budget,
            managed_runtime_terminal,
            facets,
            ..
        } = snapshot;
        if snapshot_status != status {
            bail!(
                "terminal thread {thread_id} projection status `{status}` contradicts CAS status `{}`",
                snapshot_status
            );
        }

        let final_cost = budget
            .map(|usage| {
                let metadata = facets
                    .get("cost.metadata_json")
                    .map(|raw| serde_json::from_str(raw))
                    .transpose()
                    .context("decode authoritative final cost metadata")?;
                Ok::<_, anyhow::Error>(ryeos_engine::contracts::FinalCost {
                    turns: usage.completed_turns,
                    input_tokens: usage.input_tokens,
                    output_tokens: usage.output_tokens,
                    spend: usage.spend_usd,
                    provider: facets.get("cost.provider").cloned(),
                    basis: facets.get("cost.basis").cloned(),
                    metadata,
                })
            })
            .transpose()?;
        let managed_envelope = managed_runtime_terminal
            .map(|supplement| {
                managed_terminal_envelope_from_supplement(
                    thread_id,
                    status,
                    result.as_ref(),
                    error.as_ref(),
                    final_cost.as_ref(),
                    supplement,
                )
            })
            .transpose()?;

        Ok(Some(ThreadTerminalAuthority {
            status,
            result,
            error,
            final_cost,
            managed_envelope,
        }))
    }

    pub fn list_thread_artifacts(&self, thread_id: &str) -> Result<Vec<ThreadArtifactRecord>> {
        let artifact_rows = {
            let g = self.lock()?;
            // Aggregate/count-only preflight happens before the guarded SELECT,
            // so no collection BLOB is copied until the whole collection fits.
            ensure_artifact_projection_capacity(&g, thread_id, 0, 0, 0)?;
            queries::list_thread_artifacts_bounded(
                g.state_db.projection(),
                thread_id,
                MAX_THREAD_ARTIFACT_ITEMS,
                MAX_THREAD_ARTIFACT_TYPE_BYTES,
                MAX_THREAD_ARTIFACT_METADATA_BYTES,
                MAX_THREAD_ARTIFACT_METADATA_TOTAL_BYTES,
            )?
        };
        let mut records = Vec::with_capacity(artifact_rows.len());
        let mut response_bytes = b"[]".len();
        for (idx, row) in artifact_rows.into_iter().enumerate() {
            let metadata = match row.metadata {
                Some(bytes) => {
                    Some(serde_json::from_slice::<Value>(&bytes).with_context(|| {
                        format!(
                            "malformed JSON in thread_artifacts.metadata \
                             for artifact at index {idx} of thread_id {}",
                            thread_id
                        )
                    })?)
                }
                None => None,
            };
            let record = ThreadArtifactRecord {
                artifact_id: idx as i64 + 1,
                artifact_type: row.kind,
                uri: String::new(),
                content_hash: None,
                metadata,
            };
            let encoded = serde_json::to_vec(&record)?;
            response_bytes = response_bytes
                .checked_add(encoded.len())
                .and_then(|bytes| bytes.checked_add(usize::from(!records.is_empty())))
                .ok_or_else(|| anyhow!("thread artifact response size overflow"))?;
            if response_bytes > MAX_THREAD_ARTIFACT_RESPONSE_BYTES {
                bail!(
                    "thread {thread_id} artifact response exceeds the {MAX_THREAD_ARTIFACT_RESPONSE_BYTES}-byte maximum"
                );
            }
            records.push(record);
        }
        Ok(records)
    }

    #[tracing::instrument(
        name = "state:publish_state_anchor",
        skip(self, params),
        fields(thread_id = %params.thread_id, contract = %params.contract)
    )]
    pub fn publish_state_anchor(
        &self,
        params: &StateAnchorPublishParams,
    ) -> Result<StateAnchorPublication> {
        validate_state_anchor_request(params)?;

        let restore_bytes = lillux::canonical_json(&params.restore)
            .context("canonicalize state restore contract")?
            .into_bytes();
        if restore_bytes.len() > MAX_STATE_RESTORE_BYTES {
            bail!(
                "state restore contract is {} bytes; maximum is {MAX_STATE_RESTORE_BYTES}",
                restore_bytes.len()
            );
        }
        let restore_hash = lillux::sha256_hex(&restore_bytes);
        require_prefixed_digest("restore_digest", &params.restore_digest, &restore_hash)?;

        let mut decoded_objects = Vec::with_capacity(params.objects.len());
        let mut total_input_bytes = 0usize;
        for input in &params.objects {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(&input.content_base64)
                .with_context(|| format!("decode state manifest object {:?}", input.name))?;
            if bytes.len() > MAX_STATE_MANIFEST_INPUT_BYTES {
                bail!(
                    "state manifest object {:?} is {} bytes; maximum is {MAX_STATE_MANIFEST_INPUT_BYTES}",
                    input.name,
                    bytes.len()
                );
            }
            total_input_bytes = total_input_bytes
                .checked_add(bytes.len())
                .ok_or_else(|| anyhow!("state manifest input byte total overflow"))?;
            if total_input_bytes > MAX_STATE_MANIFEST_TOTAL_INPUT_BYTES {
                bail!(
                    "state manifest inputs total {total_input_bytes} bytes; maximum is {MAX_STATE_MANIFEST_TOTAL_INPUT_BYTES}"
                );
            }
            let hash = lillux::sha256_hex(&bytes);
            require_prefixed_digest("state manifest object digest", &input.digest, &hash)?;
            decoded_objects.push((input, bytes, hash));
        }

        let permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let thread = g
            .state_db
            .get_thread(&params.thread_id)?
            .ok_or_else(|| anyhow!("state-anchor thread not found: {}", params.thread_id))?;
        if thread.status != ThreadStatus::Running.as_str() {
            bail!(
                "state-anchor publication requires a running thread; {} is '{}'",
                params.thread_id,
                thread.status
            );
        }
        let runtime = g
            .runtime_db
            .get_runtime_info(&params.thread_id)?
            .ok_or_else(|| anyhow!("runtime row missing while publishing state anchor"))?;
        if runtime.stop_intent.is_some()
            || !self
                .process_attachment_admission_open
                .load(Ordering::Acquire)
        {
            bail!(
                "state-anchor publication is closed for thread {}",
                params.thread_id
            );
        }
        match &params.subject {
            ryeos_state::objects::StateAnchorSubject::Graph { .. } => {}
            ryeos_state::objects::StateAnchorSubject::Execution {
                chain_root_id,
                placement_thread_id,
                item_ref,
                exact_program_hash,
                launch_capsule_hash,
                source_chain_seq,
                source_event_hash,
            } => {
                if chain_root_id != &thread.chain_root_id
                    || placement_thread_id != &params.thread_id
                    || item_ref != &thread.item_ref
                    || thread.admitted_launch_capsule_hash.as_deref()
                        != Some(launch_capsule_hash.as_str())
                {
                    bail!("state-anchor execution subject contradicts its authoritative thread");
                }
                let capsule = load_admitted_launch_capsule(
                    &self.state_authority,
                    &self.bundle_admission_trust,
                    launch_capsule_hash,
                )?;
                if &capsule.exact_program_hash != exact_program_hash {
                    bail!("state-anchor execution subject contradicts its admitted program");
                }
                let authoritative = g
                    .state_db
                    .read_authoritative_thread_snapshot_with_last_event(
                        &thread.chain_root_id,
                        &params.thread_id,
                    )?
                    .ok_or_else(|| anyhow!("state-anchor authoritative thread is missing"))?;
                let (observed_hash, observed_event) = authoritative
                    .last_event
                    .ok_or_else(|| anyhow!("execution state anchor requires a source event"))?;
                if observed_event.thread_id != params.thread_id
                    || observed_event.chain_seq != *source_chain_seq
                    || observed_hash != *source_event_hash
                {
                    bail!("state-anchor execution checkpoint position is no longer current");
                }
            }
        }
        if !self.projection_health.is_current() {
            bail!("state-anchor admission requires a current thread projection");
        }

        self.state_authority.ensure_guard(permit.cas_guard())?;
        let cas = self.state_authority.cas_store()?;
        let stored_restore = cas.put_blob(&restore_bytes)?;
        if stored_restore.hash != restore_hash {
            bail!(
                "state restore CAS hash mismatch: expected {restore_hash}, stored {}",
                stored_restore.hash
            );
        }
        let restore = StateManifestBlob {
            name: "restore".to_string(),
            media_type: "application/json".to_string(),
            blob_hash: restore_hash,
            size_bytes: u64::try_from(restore_bytes.len())
                .context("state restore byte length exceeds u64")?,
        };
        let mut objects = Vec::with_capacity(decoded_objects.len());
        for (input, bytes, expected_hash) in decoded_objects {
            let stored = cas.put_blob(&bytes)?;
            if stored.hash != expected_hash {
                bail!(
                    "state manifest object {:?} CAS hash mismatch: expected {expected_hash}, stored {}",
                    input.name,
                    stored.hash
                );
            }
            objects.push(StateManifestBlob {
                name: input.name.clone(),
                media_type: input.media_type.clone(),
                blob_hash: expected_hash,
                size_bytes: u64::try_from(bytes.len())
                    .context("state manifest object byte length exceeds u64")?,
            });
        }
        let manifest = StateManifest::new(
            params.contract.clone(),
            thread.chain_root_id.clone(),
            params.thread_id.clone(),
            restore,
            objects,
        )?;
        let manifest_hash = cas
            .store_object(&manifest.to_value())
            .context("store canonical state manifest")?;
        let state_digest = format!("sha256:{manifest_hash}");
        let manifest_ref = format!("cas:{manifest_hash}");
        let event_payload = ryeos_state::objects::StateAnchorMilestone {
            kind: "state_anchor".to_string(),
            payload: ryeos_state::objects::StateAnchorPayload {
                schema_version: ryeos_state::objects::STATE_ANCHOR_SCHEMA_VERSION,
                label: params.anchor.label.clone(),
                state_digest: state_digest.clone(),
                manifest_ref: manifest_ref.clone(),
                runtime: params.anchor.runtime.clone(),
                metadata: params.anchor.metadata.clone(),
            },
            subject: params.subject.clone(),
        }
        .to_value()
        .context("encode current state-anchor milestone")?;
        let event = NewEventRecord {
            event_type: ryeos_state::event_types::MILESTONE.to_string(),
            storage_class: "indexed".to_string(),
            payload: event_payload,
        };
        let persisted = append_events_locked(
            &g,
            Some(permit.cas_guard()),
            &thread.chain_root_id,
            &params.thread_id,
            std::slice::from_ref(&event),
        )?;
        let event = persisted
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("state-anchor milestone was not persisted"))?;
        Ok(StateAnchorPublication {
            state_digest,
            manifest_ref,
            event,
        })
    }

    pub fn list_threads(&self, limit: usize) -> Result<Vec<ThreadListItem>> {
        let (thread_rows, successor_payloads) = {
            let g = self.lock()?;
            let rows = queries::list_threads(g.state_db.projection(), limit)?;
            let payloads = Self::continuation_payloads_for_rows(&g, &rows)?;
            (rows, payloads)
        };
        Self::rows_to_list_items(thread_rows, successor_payloads)
    }

    /// List threads with optional principal filtering.
    ///
    /// When `filter_principal` is `Some(fp)`, only threads with
    /// `requested_by = fp` are returned. `None` returns all threads
    /// (used by internal callers that intentionally request an unfiltered view).
    pub fn list_threads_filtered(
        &self,
        limit: usize,
        filter_principal: Option<&str>,
    ) -> Result<Vec<ThreadListItem>> {
        let (thread_rows, successor_payloads) = {
            let g = self.lock()?;
            let rows =
                queries::list_threads_filtered(g.state_db.projection(), limit, filter_principal)?;
            let payloads = Self::continuation_payloads_for_rows(&g, &rows)?;
            (rows, payloads)
        };
        Self::rows_to_list_items(thread_rows, successor_payloads)
    }

    /// As [`Self::list_threads_filtered`] but with an explicit
    /// [`queries::ThreadSort`] — `Watch` orders active-before-terminal then
    /// newest for the operator dashboard, without changing the default order.
    pub fn list_threads_sorted(
        &self,
        limit: usize,
        filter_principal: Option<&str>,
        sort: queries::ThreadSort,
    ) -> Result<Vec<ThreadListItem>> {
        let (thread_rows, successor_payloads) = {
            let g = self.lock()?;
            let rows = queries::list_threads_sorted(
                g.state_db.projection(),
                limit,
                filter_principal,
                sort,
            )?;
            let payloads = Self::continuation_payloads_for_rows(&g, &rows)?;
            (rows, payloads)
        };
        Self::rows_to_list_items(thread_rows, successor_payloads)
    }

    /// Chain-wide execution usage totals (tokens, cost, turns, thread count)
    /// for a `chain_root_id` — the deep-watch summary of an execution and its
    /// continuations.
    pub fn chain_usage_totals(&self, chain_root_id: &str) -> Result<queries::ThreadUsageTotals> {
        let g = self.lock()?;
        queries::sum_thread_usage_latest_by_chain(g.state_db.projection(), chain_root_id)
    }

    /// Node-wide usage settled since `since_iso` (inclusive), continuation-
    /// deduped — what cognition on this node cost inside the window.
    pub fn node_usage_totals_since(&self, since_iso: &str) -> Result<queries::ThreadUsageTotals> {
        let g = self.lock()?;
        queries::sum_thread_usage_latest_since(g.state_db.projection(), since_iso)
    }

    /// Per-status thread counts for the node pulse: non-terminal statuses
    /// count unconditionally, terminal ones only inside the window.
    pub fn thread_status_counts(&self, since_iso: &str) -> Result<Vec<(String, i64)>> {
        let g = self.lock()?;
        queries::thread_status_counts(g.state_db.projection(), since_iso)
    }

    /// As [`Self::list_threads_sorted`] but with the full optional filter set
    /// (status / kind / requested_by) the operator dashboard narrows by.
    pub fn list_threads_query(
        &self,
        limit: usize,
        filter: &queries::ThreadListFilter,
        sort: queries::ThreadSort,
    ) -> Result<Vec<ThreadListItem>> {
        let (thread_rows, successor_payloads) = {
            let g = self.lock()?;
            let rows = queries::list_threads_query(g.state_db.projection(), limit, filter, sort)?;
            let payloads = Self::continuation_payloads_for_rows(&g, &rows)?;
            (rows, payloads)
        };
        Self::rows_to_list_items(thread_rows, successor_payloads)
    }

    /// Bounded projection candidates for a signed-first listing. Unlike the
    /// general dashboard list this does not enrich rows with projected
    /// continuation facts; the caller must check every candidate against its
    /// signed subject before displaying it or advancing to a new page.
    pub fn list_threads_candidate_page(
        &self,
        limit: usize,
        filter: &queries::ThreadListFilter,
        sort: queries::ThreadSort,
        after: Option<&queries::ThreadListCursor>,
    ) -> Result<Vec<ThreadListItem>> {
        let rows = {
            let g = self.lock()?;
            queries::list_threads_candidate_page(
                g.state_db.projection(),
                limit,
                filter,
                sort,
                after,
            )?
        };
        Ok(rows
            .into_iter()
            .map(|row| ThreadListItem {
                thread_id: row.thread_id,
                chain_root_id: row.chain_root_id,
                kind: row.kind,
                status: row.status,
                item_ref: row.item_ref,
                launch_mode: row.launch_mode,
                current_site_id: row.current_site_id,
                origin_site_id: row.origin_site_id,
                upstream_thread_id: row.upstream_thread_id,
                successor_thread_id: None,
                requested_by: row.requested_by,
                project_root: row.project_root,
                project_authority: Some(row.project_authority),
                lifecycle_authority: None,
                admitted_launch_capsule_hash: row.admitted_launch_capsule_hash,
                created_at: row.created_at,
                updated_at: row.updated_at,
            })
            .collect())
    }

    /// Bounded execution closure containing continuation and cross-chain spawn
    /// edges. The query resolves an arbitrary selected thread to its oldest
    /// reachable ancestor, so opening the tree from a child still shows its
    /// surrounding execution rather than an orphaned subtree.
    pub fn execution_tree(
        &self,
        selected_thread_id: &str,
        max_depth: usize,
        max_nodes: usize,
    ) -> Result<ExecutionTreePage> {
        let (mut tree_rows, successor_payloads) = {
            let g = self.lock()?;
            let rows = queries::execution_tree(
                g.state_db.projection(),
                selected_thread_id,
                max_depth,
                max_nodes.saturating_add(1),
            )?;
            let thread_rows = rows
                .iter()
                .take(max_nodes)
                .map(|row| row.thread.clone())
                .collect::<Vec<_>>();
            let payloads = Self::continuation_payloads_for_rows(&g, &thread_rows)?;
            (rows, payloads)
        };
        let node_truncated = tree_rows.len() > max_nodes;
        tree_rows.truncate(max_nodes);
        let depth_truncated = tree_rows
            .iter()
            .any(|row| row.depth >= max_depth && row.has_children);
        let thread_rows = tree_rows
            .iter()
            .map(|row| row.thread.clone())
            .collect::<Vec<_>>();
        let items = Self::rows_to_list_items(thread_rows, successor_payloads)?;
        let items = tree_rows
            .into_iter()
            .zip(items)
            .map(|(tree, item)| ExecutionTreeItem {
                item,
                tree_parent_thread_id: tree.tree_parent_thread_id,
                relation: tree.relation,
                depth: tree.depth,
                has_children: tree.has_children,
            })
            .collect();
        Ok(ExecutionTreePage {
            items,
            truncated: node_truncated || depth_truncated,
        })
    }

    /// Project thread rows into `ThreadListItem`s, resolving each terminal
    /// thread's continuation successor so the client can identify chain heads
    /// (a head has no successor). Shared by the filtered and unfiltered list
    /// paths.
    pub(super) fn continuation_payloads_for_rows(
        g: &Inner,
        rows: &[queries::ThreadRow],
    ) -> Result<HashMap<String, Vec<u8>>> {
        let terminal_thread_ids = rows
            .iter()
            .filter(|row| is_terminal_status(&row.status))
            .map(|row| row.thread_id.clone())
            .collect::<Vec<_>>();
        queries::continuation_successor_payloads(
            g.state_db.projection(),
            &terminal_thread_ids,
            MAX_THREAD_LIST_ENRICHMENT_THREADS,
            MAX_THREAD_LIST_EVENT_PAYLOAD_BYTES,
            MAX_THREAD_LIST_EVENT_PAYLOAD_TOTAL_BYTES,
        )
    }

    pub(super) fn rows_to_list_items(
        rows: Vec<queries::ThreadRow>,
        successor_payloads: HashMap<String, Vec<u8>>,
    ) -> Result<Vec<ThreadListItem>> {
        let mut successors = HashMap::new();
        for (thread_id, payload) in successor_payloads {
            let value: serde_json::Value = serde_json::from_slice(&payload)
                .context("parse thread_continued payload for thread-list enrichment")?;
            if let Some(successor) = value
                .get("successor_thread_id")
                .and_then(serde_json::Value::as_str)
            {
                successors.insert(thread_id, successor.to_string());
            }
        }
        let mut items = Vec::with_capacity(rows.len());
        for row in rows {
            let successor_thread_id = successors.remove(&row.thread_id);
            items.push(ThreadListItem {
                thread_id: row.thread_id,
                chain_root_id: row.chain_root_id,
                kind: row.kind,
                status: row.status,
                item_ref: row.item_ref,
                launch_mode: row.launch_mode,
                current_site_id: row.current_site_id,
                origin_site_id: row.origin_site_id,
                upstream_thread_id: row.upstream_thread_id,
                successor_thread_id,
                requested_by: row.requested_by,
                project_root: row.project_root,
                project_authority: Some(row.project_authority),
                // The high-frequency list remains projection-only. Lifecycle
                // authority is operational metadata; project authority is now
                // a signed-snapshot-derived projection column.
                lifecycle_authority: None,
                admitted_launch_capsule_hash: row.admitted_launch_capsule_hash,
                created_at: row.created_at,
                updated_at: row.updated_at,
            });
        }
        Ok(items)
    }

    /// Load the non-thread-row facts used to decorate one list page. Facets,
    /// current graph nodes, and live follow waiters share one outer store lock
    /// instead of relocking the store for every row.
    pub fn thread_list_enrichment(&self, thread_ids: &[String]) -> Result<ThreadListEnrichment> {
        let (facet_rows, graph_node_payloads, follow_waiters, terminal_error_previews) = {
            let g = self.lock()?;

            (
                load_bounded_facets_many(&g, thread_ids)?,
                queries::current_graph_node_payloads(
                    g.state_db.projection(),
                    thread_ids,
                    MAX_THREAD_LIST_ENRICHMENT_THREADS,
                    MAX_THREAD_LIST_EVENT_PAYLOAD_BYTES,
                    MAX_THREAD_LIST_EVENT_PAYLOAD_TOTAL_BYTES,
                )?,
                g.runtime_db.follow_waiter_summaries_for_threads(
                    thread_ids,
                    MAX_THREAD_LIST_ENRICHMENT_THREADS,
                )?,
                queries::thread_result_error_previews(
                    g.state_db.projection(),
                    thread_ids,
                    MAX_THREAD_LIST_ENRICHMENT_THREADS,
                    MAX_THREAD_LIST_ERROR_PREVIEW_BYTES,
                )?,
            )
        };
        Self::assemble_thread_list_enrichment(
            facet_rows,
            graph_node_payloads,
            follow_waiters,
            terminal_error_previews,
        )
    }

    pub fn thread_list_enrichment_with_waiters(
        &self,
        thread_ids: &[String],
        follow_waiters: Vec<runtime_db::FollowWaiterSummary>,
    ) -> Result<ThreadListEnrichment> {
        let (facet_rows, graph_node_payloads, terminal_error_previews) = {
            let g = self.lock()?;

            (
                load_bounded_facets_many(&g, thread_ids)?,
                queries::current_graph_node_payloads(
                    g.state_db.projection(),
                    thread_ids,
                    MAX_THREAD_LIST_ENRICHMENT_THREADS,
                    MAX_THREAD_LIST_EVENT_PAYLOAD_BYTES,
                    MAX_THREAD_LIST_EVENT_PAYLOAD_TOTAL_BYTES,
                )?,
                queries::thread_result_error_previews(
                    g.state_db.projection(),
                    thread_ids,
                    MAX_THREAD_LIST_ENRICHMENT_THREADS,
                    MAX_THREAD_LIST_ERROR_PREVIEW_BYTES,
                )?,
            )
        };
        Self::assemble_thread_list_enrichment(
            facet_rows,
            graph_node_payloads,
            follow_waiters,
            terminal_error_previews,
        )
    }

    pub(super) fn assemble_thread_list_enrichment(
        facet_rows: Vec<queries::FacetRow>,
        graph_node_payloads: HashMap<String, Vec<u8>>,
        follow_waiters: Vec<runtime_db::FollowWaiterSummary>,
        terminal_error_previews: HashMap<String, String>,
    ) -> Result<ThreadListEnrichment> {
        let mut facets: HashMap<String, Vec<(String, String)>> = HashMap::new();
        for row in facet_rows {
            let value =
                String::from_utf8(row.value).context("thread-list facet value is not UTF-8")?;
            facets
                .entry(row.thread_id)
                .or_default()
                .push((row.key, value));
        }
        let mut current_graph_nodes = HashMap::new();
        for (thread_id, payload) in graph_node_payloads {
            let payload: serde_json::Value = match serde_json::from_slice(&payload) {
                Ok(payload) => payload,
                Err(error) => {
                    tracing::warn!(
                        %thread_id,
                        %error,
                        "ignoring malformed graph_step_started payload in thread-list enrichment"
                    );
                    continue;
                }
            };
            let Some(node) = payload.get("node").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let step = payload
                .get("step")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0) as u32;
            current_graph_nodes.insert(thread_id, (node.to_string(), step));
        }
        Ok(ThreadListEnrichment {
            follow_waiters,
            facets,
            current_graph_nodes,
            terminal_error_previews,
        })
    }

    /// One consistent runtime waiter snapshot plus its projected suspended
    /// parents. Parent rows are fetched in bounded batches under the same store
    /// lock, avoiding one lock/query cycle per waiter.
    pub fn follow_parent_list_snapshot(&self) -> Result<FollowParentListSnapshot> {
        let (waiters, rows, successor_payloads) = {
            let g = self.lock()?;
            let waiters = g
                .runtime_db
                .follow_waiter_summaries_bounded(MAX_THREAD_LIST_ENRICHMENT_THREADS)?;
            let mut parent_ids = waiters
                .iter()
                .map(|waiter| waiter.parent_thread_id.clone())
                .collect::<Vec<_>>();
            parent_ids.sort();
            parent_ids.dedup();
            let rows = queries::get_threads_many(g.state_db.projection(), &parent_ids)?;
            let payloads = Self::continuation_payloads_for_rows(&g, &rows)?;
            (waiters, rows, payloads)
        };
        let parents = Self::rows_to_list_items(rows, successor_payloads)?;
        Ok(FollowParentListSnapshot { waiters, parents })
    }

    pub fn summarize_usage_by_subject(
        &self,
        filter: queries::UsageSummaryFilter<'_>,
    ) -> Result<Vec<queries::UsageSummaryRow>> {
        let g = self.lock()?;
        queries::summarize_usage_by_subject(g.state_db.projection(), filter)
    }

    pub fn get_provider_attempt_budget_latest(
        &self,
        attempt_id: &str,
    ) -> Result<Option<queries::ProviderAttemptBudgetRow>> {
        let g = self.lock()?;
        queries::get_provider_attempt_budget_latest(g.state_db.projection(), attempt_id)
    }

    pub fn get_provider_attempt_budget_transition_identity(
        &self,
        transition_id: &str,
    ) -> Result<Option<queries::ProviderAttemptBudgetTransitionIdentity>> {
        let g = self.lock()?;
        queries::get_provider_attempt_budget_transition_identity(
            g.state_db.projection(),
            transition_id,
        )
    }

    pub fn get_resource_budget_transition_identity(
        &self,
        transition_id: &str,
    ) -> Result<Option<queries::ResourceBudgetTransitionIdentity>> {
        let g = self.lock()?;
        queries::get_resource_budget_transition_identity(g.state_db.projection(), transition_id)
    }

    pub fn summarize_provider_attempt_budget(
        &self,
        filter: &queries::AccountingSummaryFilter<'_>,
    ) -> Result<queries::AccountingSummaryTotals> {
        let g = self.lock()?;
        queries::summarize_provider_attempt_budget(g.state_db.projection(), filter)
    }

    pub fn provider_attempt_budget_projection_bounds(
        &self,
    ) -> Result<queries::AccountingProjectionBounds> {
        let g = self.lock()?;
        queries::provider_attempt_budget_projection_bounds(g.state_db.projection())
    }

    pub fn summarize_direct_resource_cost(
        &self,
        thread_id: &str,
    ) -> Result<queries::ResourceCostSummary> {
        let g = self.lock()?;
        queries::summarize_direct_resource_cost(g.state_db.projection(), thread_id)
    }

    pub fn thread_projection_health(
        &self,
    ) -> crate::projection_health::ThreadProjectionHealthSnapshot {
        self.projection_health.snapshot()
    }

    pub fn list_provider_attempt_budget(
        &self,
        filter: &queries::AccountingSummaryFilter<'_>,
        limit: u32,
        after_attempt_id: Option<&str>,
    ) -> Result<Vec<queries::ProviderAttemptBudgetRow>> {
        let g = self.lock()?;
        queries::list_provider_attempt_budget(
            g.state_db.projection(),
            filter,
            limit,
            after_attempt_id,
        )
    }

    pub fn list_thread_children(&self, thread_id: &str) -> Result<Vec<ThreadDetail>> {
        let g = self.lock()?;
        let child_rows = queries::list_thread_children(g.state_db.projection(), thread_id)?;
        let mut children = Vec::new();
        for row in child_rows {
            let runtime = g
                .runtime_db
                .get_runtime_info(&row.thread_id)?
                .unwrap_or_default();
            let successor_thread_id = if is_terminal_status(&row.status) {
                queries::continuation_successor(g.state_db.projection(), &row.thread_id)?
            } else {
                None
            };
            children.push(ThreadDetail {
                // Display projection, like the other list fields. Exact get
                // and retained-result import perform immutable verification.
                result_project_snapshot_hash: row.result_project_snapshot_hash,
                result_workspace_output_capture_hash: row.result_workspace_output_capture_hash,
                thread_id: row.thread_id,
                chain_root_id: row.chain_root_id,
                kind: row.kind,
                status: row.status,
                item_ref: row.item_ref,
                executor_ref: row.executor_ref,
                launch_mode: row.launch_mode,
                current_site_id: row.current_site_id,
                origin_site_id: row.origin_site_id,
                upstream_thread_id: row.upstream_thread_id,
                successor_thread_id,
                requested_by: row.requested_by,
                project_root: row.project_root,
                project_authority: Some(row.project_authority),
                lifecycle_authority: runtime
                    .launch_metadata
                    .as_ref()
                    .map(|metadata| metadata.lifecycle_authority())
                    .transpose()?
                    .flatten(),
                admitted_launch_capsule_hash: row.admitted_launch_capsule_hash,
                created_at: row.created_at,
                updated_at: row.updated_at,
                started_at: row.started_at,
                finished_at: row.finished_at,

                runtime,
            });
        }
        Ok(children)
    }

    pub fn list_chain_threads(&self, chain_root_id: &str) -> Result<Vec<ThreadDetail>> {
        let g = self.lock()?;
        let thread_rows = queries::list_threads_by_chain(g.state_db.projection(), chain_root_id)?;
        let mut threads = Vec::new();
        for row in thread_rows {
            let runtime = g
                .runtime_db
                .get_runtime_info(&row.thread_id)?
                .unwrap_or_default();
            let successor_thread_id = if is_terminal_status(&row.status) {
                queries::continuation_successor(g.state_db.projection(), &row.thread_id)?
            } else {
                None
            };
            threads.push(ThreadDetail {
                // Do not reread/revalidate the complete chain head per row.
                // List data is not retained-result import authority.
                result_project_snapshot_hash: row.result_project_snapshot_hash,
                result_workspace_output_capture_hash: row.result_workspace_output_capture_hash,
                thread_id: row.thread_id,
                chain_root_id: row.chain_root_id,
                kind: row.kind,
                status: row.status,
                item_ref: row.item_ref,
                executor_ref: row.executor_ref,
                launch_mode: row.launch_mode,
                current_site_id: row.current_site_id,
                origin_site_id: row.origin_site_id,
                upstream_thread_id: row.upstream_thread_id,
                successor_thread_id,
                requested_by: row.requested_by,
                project_root: row.project_root,
                project_authority: Some(row.project_authority),
                lifecycle_authority: runtime
                    .launch_metadata
                    .as_ref()
                    .map(|metadata| metadata.lifecycle_authority())
                    .transpose()?
                    .flatten(),
                admitted_launch_capsule_hash: row.admitted_launch_capsule_hash,
                created_at: row.created_at,
                updated_at: row.updated_at,
                started_at: row.started_at,
                finished_at: row.finished_at,

                runtime,
            });
        }
        Ok(threads)
    }

    pub fn list_chain_edges(&self, chain_root_id: &str) -> Result<Vec<ThreadEdgeRecord>> {
        let g = self.lock()?;
        let edge_rows = queries::list_thread_edges(g.state_db.projection(), chain_root_id)?;
        Ok(edge_rows
            .into_iter()
            .enumerate()
            .map(|(idx, row)| ThreadEdgeRecord {
                edge_id: idx as i64 + 1,
                chain_root_id: row.chain_root_id,
                source_thread_id: row.parent_thread_id,
                target_thread_id: row.child_thread_id,
                edge_type: "spawned".to_string(),
                created_at: String::new(),
                metadata: row.spawn_reason.map(|r| json!(r)),
            })
            .collect())
    }

    pub fn list_threads_by_status(&self, statuses: &[&str]) -> Result<Vec<ThreadDetail>> {
        let g = self.lock()?;
        let thread_rows = queries::list_threads_by_status(g.state_db.projection(), statuses)?;
        let mut details = Vec::new();
        for row in thread_rows {
            let runtime = g
                .runtime_db
                .get_runtime_info(&row.thread_id)?
                .unwrap_or_default();
            let successor_thread_id = if is_terminal_status(&row.status) {
                queries::continuation_successor(g.state_db.projection(), &row.thread_id)?
            } else {
                None
            };
            details.push(ThreadDetail {
                // Reconciliation enumerates projected rows; exact authority
                // remains with the operation that consumes the coordinate.
                result_project_snapshot_hash: row.result_project_snapshot_hash,
                result_workspace_output_capture_hash: row.result_workspace_output_capture_hash,
                thread_id: row.thread_id,
                chain_root_id: row.chain_root_id,
                kind: row.kind,
                status: row.status,
                item_ref: row.item_ref,
                executor_ref: row.executor_ref,
                launch_mode: row.launch_mode,
                current_site_id: row.current_site_id,
                origin_site_id: row.origin_site_id,
                upstream_thread_id: row.upstream_thread_id,
                successor_thread_id,
                requested_by: row.requested_by,
                project_root: row.project_root,
                project_authority: Some(row.project_authority),
                lifecycle_authority: runtime
                    .launch_metadata
                    .as_ref()
                    .map(|metadata| metadata.lifecycle_authority())
                    .transpose()?
                    .flatten(),
                admitted_launch_capsule_hash: row.admitted_launch_capsule_hash,
                created_at: row.created_at,
                updated_at: row.updated_at,
                started_at: row.started_at,
                finished_at: row.finished_at,

                runtime,
            });
        }
        Ok(details)
    }

    /// Ids of the live (`created` or `running`) threads whose covering
    /// authority is the authorized-key grant `grant_digest`, whoever requested
    /// them. Threads are not indexed by grant digest: each live thread's
    /// launch metadata is read, which is cheap while live threads are few.
    pub fn live_thread_ids_under_grant(&self, grant_digest: &str) -> Result<Vec<String>> {
        Ok(self
            .list_threads_by_status(&["created", "running"])?
            .into_iter()
            .filter(|thread| {
                thread
                    .runtime
                    .launch_metadata
                    .as_ref()
                    .and_then(|metadata| metadata.covering_authority.as_ref())
                    .and_then(|authority| authority.grant_digest.as_deref())
                    == Some(grant_digest)
            })
            .map(|thread| thread.thread_id)
            .collect())
    }

    pub fn active_thread_count(&self) -> Result<i64> {
        let g = self.lock()?;
        queries::active_thread_count(g.state_db.projection())
    }

    pub fn wait_for_project_authority(
        &self,
        thread_id: &str,
        reason: &str,
        detail: &str,
        now_ms: i64,
        deadline_at_ms: i64,
    ) -> Result<runtime_db::RecoveryWaitDisposition> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let result = g.runtime_db.wait_for_project_authority(
            thread_id,
            reason,
            detail,
            now_ms,
            deadline_at_ms,
        );
        drop(g);
        result
    }

    pub fn clear_recovery_wait(&self, thread_id: &str) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let result = g.runtime_db.clear_recovery_wait(thread_id);
        drop(g);
        result
    }

    pub fn authoritative_result_project_snapshot(&self, thread_id: &str) -> Result<Option<String>> {
        let g = self.lock()?;
        Ok(g.state_db
            .get_thread(thread_id)?
            .and_then(|snapshot| snapshot.result_project_snapshot_hash))
    }

    /// Prove that an authoritative project snapshot still has its complete,
    /// canonical CAS closure under this store's pinned state generation.
    ///
    /// Startup repair uses this before reconnecting an operational session row
    /// to a snapshot already testified by the root thread chain. A hash-shaped
    /// SQLite value alone is never sufficient publication authority.
    pub fn verify_project_snapshot_closure(&self, snapshot_hash: &str) -> Result<()> {
        if !lillux::valid_hash(snapshot_hash) {
            bail!("project snapshot hash is not canonical");
        }
        let cas = self.state_authority.cas_store()?;
        ryeos_state::project_materialization::VerifiedProjectSnapshotClosure::load(
            &cas,
            snapshot_hash,
        )?;
        Ok(())
    }

    pub fn get_facets(&self, thread_id: &str) -> Result<Vec<(String, String)>> {
        let facet_rows = {
            let g = self.lock()?;
            let (items, content_bytes) =
                queries::thread_facet_stats(g.state_db.projection(), thread_id)?;
            ensure_facet_collection_bounds(thread_id, items, content_bytes)?;
            queries::get_facets_bounded(
                g.state_db.projection(),
                thread_id,
                MAX_THREAD_FACET_ITEMS,
                MAX_THREAD_FACET_KEY_BYTES,
                MAX_THREAD_FACET_VALUE_BYTES,
                MAX_THREAD_FACET_CONTENT_BYTES,
            )?
        };
        let mut facets = Vec::with_capacity(facet_rows.len());
        let mut response_bytes = b"{}".len();
        for row in facet_rows {
            let value = String::from_utf8(row.value)
                .with_context(|| format!("thread {thread_id} facet value is not UTF-8"))?;
            let key_bytes = serde_json::to_vec(&row.key)?.len();
            let value_bytes = serde_json::to_vec(&value)?.len();
            response_bytes = response_bytes
                .checked_add(key_bytes)
                .and_then(|bytes| bytes.checked_add(value_bytes))
                .and_then(|bytes| bytes.checked_add(1)) // colon
                .and_then(|bytes| bytes.checked_add(usize::from(!facets.is_empty())))
                .ok_or_else(|| anyhow!("thread facet response size overflow"))?;
            if response_bytes > MAX_THREAD_FACET_RESPONSE_BYTES {
                bail!(
                    "thread {thread_id} facet response exceeds the {MAX_THREAD_FACET_RESPONSE_BYTES}-byte maximum"
                );
            }
            facets.push((row.key, value));
        }
        Ok(facets)
    }

    /// A graph thread's current `(node, step)` from its latest
    /// `graph_step_started` — the cheap live "where is it now". See
    /// [`queries::current_graph_node`].
    pub fn current_graph_node(&self, thread_id: &str) -> Result<Option<(String, u32)>> {
        let g = self.lock()?;
        queries::current_graph_node(g.state_db.projection(), thread_id)
    }
}
