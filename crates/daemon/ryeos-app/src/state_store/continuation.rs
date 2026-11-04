//! Continuation and follow-resume successors.
use super::*;

/// Which kind of running-source continuation successor to create. Both kinds
/// cut a still-running source, seed the successor's resume context, and settle
/// the source `continued`; they differ only in the edge `reason` recorded and
/// in whether the autonomous chain-depth cap applies. The `GraphFollowResume`
/// marker is daemon-trusted (selectable only via the dedicated method, never a
/// caller-supplied reason).
pub(super) enum RunningContinuationKind<'a> {
    Machine {
        sanitized_reason: Option<&'a str>,
    },
    GraphFollowResume,
    RemoteAdoption {
        authority: &'a RemoteAdoptionContinuationAuthority,
    },
}

impl StateStore {
    /// Raw continuation fixture for state-layer tests. Absent in production.
    #[cfg(any(test, feature = "test-support"))]
    pub fn create_continuation_for_test(
        &self,
        successor: &NewThreadRecord,
        source_thread_id: &str,
        chain_root_id: &str,
        reason: Option<&str>,
    ) -> Result<Vec<PersistedEventRecord>> {
        self.create_continuation_admitted(
            successor,
            source_thread_id,
            chain_root_id,
            reason,
            Vec::new(),
            None,
        )
        .map(|publication| publication.persisted)
    }

    /// Machine continuation handoff (limit cut-off) — the autonomous path.
    ///
    /// Unlike the operator-follow-up continuation path, which
    /// accepts a terminal source and leaves it as-is), this enforces the machine
    /// invariants atomically under the write permit + lock:
    ///
    /// - the source must be **exactly `running`** — a cut-off live run. Re-checked
    ///   here, not just by the caller, so a source that goes terminal between the
    ///   caller's check and this commit cannot mint a successor.
    /// - the source must carry a captured `ResumeContext` (spawn-time launch
    ///   identity) — a successor we cannot launch is worse than none.
    /// - the successor's launch metadata is **seeded before the source is settled
    ///   `continued`**, and the runtime-db writes (which can fail independently of
    ///   the source's terminal snapshot) happen first — so any failure aborts the
    ///   handoff with the source still `running` (the runner then fails terminal),
    ///   never `continued` behind an unlaunchable successor.
    pub fn create_machine_continuation(
        &self,
        successor: &NewThreadRecord,
        source_thread_id: &str,
        chain_root_id: &str,
        reason: Option<&str>,
    ) -> Result<Vec<PersistedEventRecord>> {
        let sanitized_reason =
            reason.filter(|r| !queries::ContinuationReasonMarker::is_reserved_str(r));
        self.create_running_continuation_successor(
            successor,
            source_thread_id,
            chain_root_id,
            RunningContinuationKind::Machine { sanitized_reason },
            None,
            None,
            None,
            Vec::new(),
        )
        .map(|publication| publication.persisted)
    }

    // Source lineage, resume proof, launch metadata, and initial durable events
    // stay explicit because each is validated under the same write permit.
    pub fn create_machine_continuation_with_events(
        &self,
        successor: &NewThreadRecord,
        source_thread_id: &str,
        chain_root_id: &str,
        reason: Option<&str>,
        expected_resume_context: &crate::launch_metadata::ResumeContext,
        successor_launch_metadata: &crate::launch_metadata::RuntimeLaunchMetadata,
        initial_events: Vec<NewEventRecord>,
    ) -> Result<(Vec<PersistedEventRecord>, ThreadDetail)> {
        // The machine handoff carries a free-form runtime LOG reason. Scrub ALL
        // daemon-reserved markers so a runtime cannot mint an edge the chain-depth
        // walk would treat as an operator reset or a depth-exempt follow.
        let sanitized_reason =
            reason.filter(|r| !queries::ContinuationReasonMarker::is_reserved_str(r));
        self.create_running_continuation_successor(
            successor,
            source_thread_id,
            chain_root_id,
            RunningContinuationKind::Machine { sanitized_reason },
            Some(expected_resume_context),
            Some(successor_launch_metadata),
            None,
            initial_events,
        )
        .map(|publication| (publication.persisted, publication.successor))
    }

    /// Daemon-owned machine continuation that advances an exact frozen pinned
    /// CoW generation.  The ordinary runtime callback cannot select this
    /// value; checkpoint/handoff code supplies the CAS-proved candidate and
    /// the shared continuation transaction verifies the typed authority
    /// transition before publishing either side.
    pub fn create_machine_continuation_with_project_generation(
        &self,
        successor: &NewThreadRecord,
        source_thread_id: &str,
        chain_root_id: &str,
        reason: Option<&str>,
        expected_resume_context: &crate::launch_metadata::ResumeContext,
        successor_launch_metadata: &crate::launch_metadata::RuntimeLaunchMetadata,
        source_result_snapshot_hash: Option<&str>,
        initial_events: Vec<NewEventRecord>,
    ) -> Result<(Vec<PersistedEventRecord>, ThreadDetail)> {
        let sanitized_reason =
            reason.filter(|reason| !queries::ContinuationReasonMarker::is_reserved_str(reason));
        // This source-only entry point serves retained worker candidates, which
        // have no output partition. The shared typed transition refuses an
        // output-bearing source rather than dropping its capture authority.
        let generation = source_result_snapshot_hash.map(|snapshot_hash| {
            ryeos_state::objects::WorkspaceGenerationPair {
                snapshot_hash: snapshot_hash.to_owned(),
                output_capture_hash: None,
            }
        });
        self.create_running_continuation_successor(
            successor,
            source_thread_id,
            chain_root_id,
            RunningContinuationKind::Machine { sanitized_reason },
            Some(expected_resume_context),
            Some(successor_launch_metadata),
            generation.as_ref(),
            initial_events,
        )
        .map(|publication| (publication.persisted, publication.successor))
    }

    /// Atomically terminalize the exact quiesced source placement and author
    /// one target-site successor without creating a runnable source-node
    /// runtime row. The ordinary running-continuation transaction remains the
    /// sole single-successor serialization boundary.
    pub fn create_remote_adoption_successor(
        &self,
        successor: &NewThreadRecord,
        source_thread_id: &str,
        chain_root_id: &str,
        authority: &RemoteAdoptionContinuationAuthority,
    ) -> Result<RemoteAdoptionPublication> {
        let publication = self.create_running_continuation_successor(
            successor,
            source_thread_id,
            chain_root_id,
            RunningContinuationKind::RemoteAdoption { authority },
            None,
            None,
            Some(&ryeos_state::objects::WorkspaceGenerationPair {
                snapshot_hash: authority
                    .placement
                    .project_rebind
                    .source_candidate_snapshot_hash
                    .clone(),
                output_capture_hash: None,
            }),
            Vec::new(),
        )?;
        Ok(RemoteAdoptionPublication {
            successor_thread_id: successor.thread_id.clone(),
            writer_grant_hash: publication.remote_writer_grant_hash.ok_or_else(|| {
                anyhow!("remote adoption committed without its chain-writer grant identity")
            })?,
            target_launch_capsule_hash: authority.placement.target_launch_capsule_hash.clone(),
            target_runtime_seed_hash: authority.placement.target_runtime_seed_hash.clone(),
        })
    }

    pub fn create_follow_resume_successor_with_launch_metadata(
        &self,
        successor: &NewThreadRecord,
        source_thread_id: &str,
        chain_root_id: &str,
        successor_launch_metadata: &crate::launch_metadata::RuntimeLaunchMetadata,
        result_generation: Option<&ryeos_state::objects::WorkspaceGenerationPair>,
    ) -> Result<Vec<PersistedEventRecord>> {
        if let Some(generation) = result_generation {
            generation.validate()?;
            if successor
                .project_authority
                .operational_snapshot_projection()
                != Some(generation.snapshot_hash.as_str())
                || successor
                    .project_authority
                    .workspace_outputs()
                    .and_then(|outputs| outputs.capture_hash.as_ref())
                    != generation.output_capture_hash.as_ref()
            {
                bail!(
                    "follow successor authority contradicts the frozen source/output generation pair"
                );
            }
        }
        self.create_running_continuation_successor(
            successor,
            source_thread_id,
            chain_root_id,
            RunningContinuationKind::GraphFollowResume,
            successor_launch_metadata.resume_context.as_ref(),
            Some(successor_launch_metadata),
            result_generation,
            Vec::new(),
        )
        .map(|publication| publication.persisted)
    }

    /// Shared core for both running-source continuations (machine handoff and
    /// follow-resume). One atomic op under the write permit + lock: re-verify the
    /// source is running, enforce the single-successor invariant, require the
    /// source's captured ResumeContext, seed the successor (runtime-db writes
    /// first), then settle the source `continued`. A race or seed failure aborts
    /// with the source still running — never `continued` behind an unlaunchable
    /// successor.
    pub(super) fn create_running_continuation_successor(
        &self,
        successor: &NewThreadRecord,
        source_thread_id: &str,
        chain_root_id: &str,
        kind: RunningContinuationKind<'_>,
        expected_resume_context: Option<&crate::launch_metadata::ResumeContext>,
        successor_launch_metadata: Option<&crate::launch_metadata::RuntimeLaunchMetadata>,
        source_result_generation: Option<&ryeos_state::objects::WorkspaceGenerationPair>,
        initial_events: Vec<NewEventRecord>,
    ) -> Result<CreatedThreadPublication> {
        let permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        validate_facet_event_admission(&g, &successor.thread_id, &initial_events)?;
        let source_row = g
            .state_db
            .get_thread(source_thread_id)?
            .ok_or_else(|| anyhow!("source thread not found: {source_thread_id}"))?;

        if !self
            .process_attachment_admission_open
            .load(Ordering::Acquire)
        {
            bail!("continuation authoring is closed for daemon shutdown");
        }
        let source_runtime = g
            .runtime_db
            .get_runtime_info(source_thread_id)?
            .ok_or_else(|| anyhow!("source runtime row missing: {source_thread_id}"))?;
        if let Some(intent) = source_runtime.stop_intent.as_ref() {
            bail!(
                "cannot continue stop-requested thread {source_thread_id} ({})",
                intent.as_str()
            );
        }

        // A running-source continuation cuts a still-running source. Re-checked
        // under the lock to close the caller's check-then-commit race; a terminal
        // source is the operator follow-up path, not this one.
        if source_row.status != ThreadStatus::Running.as_str() {
            bail!(
                "running continuation requires a running source; \
                 thread {source_thread_id} is '{}'",
                source_row.status
            );
        }

        // Never braid a successor into the wrong chain.
        if chain_root_id != source_row.chain_root_id {
            bail!(
                "chain_root_id mismatch: requested {chain_root_id}, source \
                 {source_thread_id} is in chain {}",
                source_row.chain_root_id
            );
        }

        // Single-successor invariant (atomic under the held permit + lock).
        if let Some(existing) =
            queries::continuation_successor(g.state_db.projection(), source_thread_id)?
        {
            bail!("thread {source_thread_id} already continued as {existing}");
        }

        // Chain-level ceiling: bound the length of an AUTONOMOUS continuation run.
        // MACHINE handoffs only — a follow-resume edge is structural progress and
        // must be allowed even when the parent chain is already at the cap.
        if let RunningContinuationKind::Machine { .. } = &kind {
            let machine_depth = queries::consecutive_machine_continuation_depth(
                g.state_db.projection(),
                source_thread_id,
                crate::thread_lifecycle::MAX_CONTINUATION_CHAIN_DEPTH,
            )?;
            if machine_depth >= crate::thread_lifecycle::MAX_CONTINUATION_CHAIN_DEPTH {
                bail!(
                    "continuation depth limit reached ({machine_depth}/{}); the autonomous \
                     chain will not continue",
                    crate::thread_lifecycle::MAX_CONTINUATION_CHAIN_DEPTH
                );
            }
        }

        // Same-node continuations inherit operational launch metadata. A
        // remote adoption deliberately does not: its portable source
        // authority is the admitted capsule, while its successor authority is
        // the target-attested capsule and runtime seed.
        let source_launch_metadata = source_runtime.launch_metadata;
        let source_resume_context =
            if matches!(&kind, RunningContinuationKind::RemoteAdoption { .. }) {
                None
            } else {
                Some(
                    source_launch_metadata
                        .as_ref()
                        .and_then(|metadata| metadata.resume_context.as_ref())
                        .cloned()
                        .ok_or_else(|| {
                            anyhow!(
                                "source thread {source_thread_id} has no captured ResumeContext; \
                             cannot create a launchable continuation successor"
                            )
                        })?,
                )
            };
        let remote_frontier = if let RunningContinuationKind::RemoteAdoption { authority } = &kind {
            authority.placement.validate()?;
            let placement = &authority.placement;
            let source_capsule_hash = source_row
                .admitted_launch_capsule_hash
                .as_deref()
                .ok_or_else(|| anyhow!("remote adoption source has no admitted launch capsule"))?;
            let source_capsule = load_admitted_launch_capsule(
                &self.state_authority,
                &self.bundle_admission_trust,
                source_capsule_hash,
            )?;
            source_capsule.validate_durable_handoff_eligibility()?;
            let source_sealed =
                crate::thread_lifecycle::SealedRootExecutionRequest::decode_from_admitted_capsule(
                    &source_capsule,
                )?;
            match (
                source_capsule.accounting_scope.as_ref(),
                authority.source_accounting_transfer.as_ref(),
            ) {
                (None, None) if placement.accounting.source_scope.is_none() => {}
                (Some(scope), Some(transfer))
                    if scope == &transfer.source_scope
                        && placement.accounting.source_scope.as_ref() == Some(scope)
                        && placement.accounting.source_financial_high_water
                            == transfer.source_financial_high_water_before
                        && placement.accounting.source_charged_usd_nanos
                            == transfer.source_charged_usd_nanos
                        && placement.accounting.source_remaining_cap_usd_nanos
                            == transfer.source_execution_available_before_usd_nanos
                        && placement
                            .accounting
                            .source_remaining_directive_cap_usd_nanos
                            == transfer.source_directive_available_before_usd_nanos
                        && placement.accounting.target_scope.as_ref()
                            == Some(&transfer.target_scope)
                        && placement.accounting.allocation_mode.as_deref()
                            == Some(transfer.allocation_mode.as_str())
                        && placement.accounting.target_cap_usd_nanos
                            == transfer.target_cap_usd_nanos
                        && placement.accounting.target_directive_cap_usd_nanos
                            == transfer.target_directive_cap_usd_nanos
                        && transfer.operation_id == placement.operation_id
                        && transfer.source_chain_root_id == placement.chain_root_id
                        && transfer.source_placement_thread_id
                            == placement.source_placement_thread_id => {}
                _ => bail!(
                    "remote adoption accounting evidence is not the exact settled source frontier"
                ),
            }
            if placement.chain_root_id != chain_root_id
                || placement.source_placement_thread_id != source_thread_id
                || placement.successor_placement_thread_id != successor.thread_id
                || placement.owner_principal != source_sealed.handoff_principal_identifier()?
                || placement.origin_site_id != source_sealed.handoff_origin_site_id()
                || placement.source_site_id != source_sealed.handoff_current_site_id()
            {
                bail!(
                    "remote adoption placement does not name the exact source, successor, owner, and sites"
                );
            }
            if successor.current_site_id != placement.target_site_id
                || successor.origin_site_id != placement.origin_site_id
                || successor.requested_by.as_deref() != Some(placement.owner_principal.as_str())
            {
                bail!(
                    "remote adoption successor snapshot identity contradicts placement authority"
                );
            }

            let source_head = g
                .state_db
                .read_generic_head_ref("chains", chain_root_id)?
                .ok_or_else(|| anyhow!("remote adoption source chain has no signed head"))?;
            if source_head.target_hash != placement.source_chain_head_hash
                || source_head.signer != g.signer.fingerprint()
            {
                bail!("remote adoption source head is no longer the exact locally owned frontier");
            }
            let anchor = g
                .state_db
                .read_authoritative_thread_append_anchor(chain_root_id, source_thread_id)?
                .ok_or_else(|| anyhow!("remote adoption source thread has no append anchor"))?;
            if anchor.thread_last_event_hash.as_deref()
                != Some(placement.source_last_event_hash.as_str())
            {
                bail!("remote adoption source event is no longer the exact thread frontier");
            }

            let session = g
                .runtime_db
                .dedicated_session(source_thread_id)?
                .ok_or_else(|| anyhow!("remote adoption source has no dedicated worker session"))?;
            if session.chain_root_id != chain_root_id
                || session.owner_principal != placement.owner_principal
                || session.state != "frozen"
                || session.current_turn_id.is_some()
                || session.send_boundary != "settled"
                || session.candidate_snapshot_hash.as_deref()
                    != Some(
                        placement
                            .project_rebind
                            .source_candidate_snapshot_hash
                            .as_str(),
                    )
                || session.credential_profile_id != source_sealed.worker_credential_profile_id()?
                || session.remote_thread_id.as_deref()
                    != Some(
                        placement
                            .credential_reservation
                            .upstream_session_id
                            .as_str(),
                    )
            {
                bail!("remote adoption source session is not the exact frozen settled candidate");
            }
            let worker_instance_id = session.worker_instance_id.as_deref().ok_or_else(|| {
                anyhow!("remote adoption source session has no retained worker identity")
            })?;
            let worker = g
                .runtime_db
                .worker_process(worker_instance_id)?
                .ok_or_else(|| anyhow!("remote adoption source worker projection is missing"))?;
            if worker.placement_thread_id != source_thread_id
                || session.worker_boot_epoch != Some(worker.boot_epoch)
                || worker.state != runtime_db::WorkerProcessState::Dead
                || worker.cleanup_state != "reaped"
            {
                bail!("remote adoption source worker death is not proved reaped");
            }

            let checkpoint_value = self
                .state_authority
                .cas_store()?
                .get_object(&placement.checkpoint_manifest_hash)?
                .ok_or_else(|| anyhow!("remote adoption checkpoint manifest is absent"))?;
            let checkpoint = StateManifest::from_current_value(checkpoint_value)?;
            if checkpoint.publisher_chain_root_id != chain_root_id
                || checkpoint.publisher_thread_id != source_thread_id
                || checkpoint.contract != ryeos_state::objects::WORKER_SESSION_RESTORE_CONTRACT
            {
                bail!("remote adoption checkpoint was not published by the exact source placement");
            }
            let restore_bytes = self
                .state_authority
                .cas_store()?
                .get_blob(&checkpoint.restore.blob_hash)?
                .ok_or_else(|| anyhow!("remote adoption restore document is absent"))?;
            if lillux::sha256_hex(&restore_bytes) != checkpoint.restore.blob_hash {
                bail!("remote adoption restore document digest changed");
            }
            let restore_value: Value = serde_json::from_slice(&restore_bytes)
                .context("decode remote adoption restore document")?;
            if lillux::canonical_json(&restore_value)?.as_bytes() != restore_bytes {
                bail!("remote adoption restore document is not canonical JSON");
            }
            let restore =
                ryeos_state::objects::WorkerSessionRestore::from_current_value(restore_value)?;
            let source_capsule_hash = source_row
                .admitted_launch_capsule_hash
                .as_deref()
                .ok_or_else(|| anyhow!("remote adoption source has no launch capsule"))?;
            let restored_dependencies = restore
                .persistent_dependencies
                .iter()
                .map(|(name, dependency)| (name.clone(), dependency.exact_program_hash.clone()))
                .collect::<BTreeMap<_, _>>();
            if restore.source_position.chain_root_id != chain_root_id
                || restore.source_position.placement_thread_id != source_thread_id
                || restore.project_candidate_snapshot_hash.as_deref()
                    != Some(
                        placement
                            .project_rebind
                            .source_candidate_snapshot_hash
                            .as_str(),
                    )
                || restore.outer_exact_program_hash != placement.outer_exact_program_hash
                || restored_dependencies != placement.persistent_dependency_programs
                || restore.upstream_session_id
                    != placement.credential_reservation.upstream_session_id
                || restore.credential_subject_contract_digest
                    != placement.credential_reservation.subject_contract_digest
                || restore.credential_subject_digest
                    != placement.credential_reservation.subject_digest
                || restore.source_site_id != placement.source_site_id
                || restore.source_launch_capsule_hash != source_capsule_hash
            {
                bail!("remote adoption placement contradicts its authoritative checkpoint restore");
            }
            let latest_anchor =
                queries::latest_state_anchor_event(g.state_db.projection(), source_thread_id)?
                    .ok_or_else(|| {
                        anyhow!("remote adoption source has no authoritative state anchor")
                    })?;
            let anchor = ryeos_state::objects::StateAnchorMilestone::from_value(
                serde_json::from_slice(&latest_anchor.payload)
                    .context("decode latest source state-anchor payload")?,
            )?;
            if anchor.payload.manifest_ref != format!("cas:{}", placement.checkpoint_manifest_hash)
            {
                bail!("remote adoption checkpoint is not the source's latest state anchor");
            }
            match &anchor.subject {
                ryeos_state::objects::StateAnchorSubject::Execution {
                    chain_root_id: anchor_chain,
                    placement_thread_id,
                    launch_capsule_hash,
                    source_chain_seq,
                    source_event_hash,
                    ..
                } if anchor_chain == chain_root_id
                    && placement_thread_id == source_thread_id
                    && source_row.admitted_launch_capsule_hash.as_deref()
                        == Some(launch_capsule_hash.as_str())
                    && restore.source_position.chain_seq == *source_chain_seq
                    && restore.source_position.event_hash == *source_event_hash => {}
                _ => bail!("remote adoption checkpoint anchor names another execution authority"),
            }

            let placement_value = self
                .state_authority
                .cas_store()?
                .get_object(&authority.placement_attestation_hash)?
                .ok_or_else(|| anyhow!("remote adoption placement attestation is absent"))?;
            let placement_attestation =
                ryeos_state::objects::Attestation::from_value(&placement_value)?;
            placement_attestation.verify_with_key(&authority.target_node_verifying_key)?;
            if placement_attestation.is_expired_at(&lillux::time::iso8601_now())? {
                bail!("remote adoption placement attestation is expired");
            }
            let observed =
                crate::worker_handoff::WorkerPlacementAdmissionEvidence::from_attestation(
                    &placement_attestation,
                )?;
            if observed != *placement {
                bail!("remote adoption placement attestation differs from its supplied evidence");
            }
            let target_signer_fingerprint = placement_attestation.issuer_fingerprint()?.to_string();
            Some((
                source_head.target_hash,
                placement.source_last_event_hash.clone(),
                target_signer_fingerprint,
            ))
        } else {
            None
        };
        if matches!(&kind, RunningContinuationKind::GraphFollowResume)
            && source_launch_metadata
                .as_ref()
                .expect("non-remote continuation source metadata was required above")
                .native_resume
                .is_none()
        {
            bail!("follow-resume source thread {source_thread_id} does not declare native_resume");
        }
        // Successor preconditions BEFORE any write: it must belong to the source's
        // chain and, if it names an upstream, name THIS source — never braid a
        // successor into the wrong chain or contradict the edge being created (a
        // later StateDb reject would leave an orphan runtime row behind).
        if successor.chain_root_id != source_row.chain_root_id {
            bail!(
                "successor {} chain_root_id {} does not match source chain {}",
                successor.thread_id,
                successor.chain_root_id,
                source_row.chain_root_id
            );
        }
        match successor.upstream_thread_id.as_deref() {
            None => {}
            Some(id) if id == source_thread_id => {}
            Some(other) => bail!(
                "successor {} declares upstream {other}, not the continuation source {source_thread_id}",
                successor.thread_id
            ),
        }
        let mut successor_with_upstream = successor.clone();
        successor_with_upstream.upstream_thread_id = Some(source_thread_id.to_string());

        if successor_launch_metadata.is_some()
            && matches!(&kind, RunningContinuationKind::RemoteAdoption { .. })
        {
            bail!("remote adoption derives launch metadata only from its attested target capsule");
        }
        let mut successor_meta = match &kind {
            RunningContinuationKind::RemoteAdoption { authority } => {
                let cas = self.state_authority.cas_store()?;
                let target_capsule_hash = &authority.placement.target_launch_capsule_hash;
                let target_capsule_value = cas
                    .get_object_bounded(
                        target_capsule_hash,
                        ryeos_state::objects::MAX_ADMITTED_LAUNCH_CAPSULE_BYTES as u64,
                    )?
                    .ok_or_else(|| anyhow!("remote adoption target launch capsule is absent"))?;
                let target_capsule =
                    ryeos_state::objects::AdmittedLaunchCapsule::from_current_value(
                        target_capsule_value,
                    )?;
                if target_capsule.content_hash()? != *target_capsule_hash {
                    bail!("remote adoption target launch capsule changed content hash");
                }
                verify_rooted_bundle_member_source_proof(
                    &target_capsule,
                    &self.bundle_admission_trust,
                )
                .context("revalidate transferred rooted Bundle member-source signatures")?;
                target_capsule
                    .verify_retained_execution_realization_with_key(
                        &cas,
                        &self.state_authority.large_object_store()?,
                        &authority.target_node_verifying_key,
                    )
                    .context("verify remote target execution realization")?;
                let source_capsule_hash = source_row
                    .admitted_launch_capsule_hash
                    .as_deref()
                    .ok_or_else(|| {
                        anyhow!("remote adoption source has no admitted launch capsule")
                    })?;
                let source_capsule = load_admitted_launch_capsule(
                    &self.state_authority,
                    &self.bundle_admission_trust,
                    source_capsule_hash,
                )?;
                let target_sealed =
                    crate::thread_lifecycle::SealedRootExecutionRequest::decode_from_admitted_capsule(
                        &target_capsule,
                    )?;
                let cas = self.state_authority.cas_store()?;
                let seed_value = cas
                    .get_object(&authority.placement.target_runtime_seed_hash)?
                    .ok_or_else(|| anyhow!("remote adoption target runtime seed is absent"))?;
                let seed =
                    ryeos_state::objects::PlacementRuntimeSeed::from_current_value(seed_value)?;
                if seed.content_hash()? != authority.placement.target_runtime_seed_hash
                    || seed.operation_id != authority.placement.operation_id
                    || seed.chain_root_id != chain_root_id
                    || seed.source_placement_thread_id != source_thread_id
                    || seed.successor_placement_thread_id != successor.thread_id
                    || seed.target_site_id != authority.placement.target_site_id
                    || seed.owner_principal != authority.placement.owner_principal
                    || seed.target_launch_capsule_hash
                        != authority.placement.target_launch_capsule_hash
                {
                    bail!("remote adoption runtime seed contradicts placement authority");
                }
                let metadata_bytes = cas
                    .get_blob(&seed.launch_metadata_blob_hash)?
                    .ok_or_else(|| anyhow!("remote adoption runtime metadata is absent"))?;
                if u64::try_from(metadata_bytes.len())? != seed.launch_metadata_size_bytes
                    || lillux::sha256_hex(&metadata_bytes) != seed.launch_metadata_blob_hash
                {
                    bail!("remote adoption runtime metadata size or digest changed");
                }
                let metadata_value: Value = serde_json::from_slice(&metadata_bytes)
                    .context("decode target-authored runtime seed")?;
                if lillux::canonical_json(&metadata_value)?.as_bytes() != metadata_bytes {
                    bail!("remote adoption runtime metadata is not canonical JSON");
                }
                let metadata: crate::launch_metadata::RuntimeLaunchMetadata =
                    serde_json::from_value(metadata_value)
                        .context("decode target-authored launch metadata")?;
                metadata
                    .validate()
                    .context("validate remote adoption target launch metadata")?;
                let seed_sealed_matches = metadata
                    .sealed_root_request
                    .as_ref()
                    .map(serde_json::to_value)
                    .transpose()?
                    == Some(serde_json::to_value(&target_sealed)?);
                let target_resume = metadata.resume_context.as_ref().ok_or_else(|| {
                    anyhow!("target-authored runtime seed has no resume authority")
                })?;
                if metadata.continuation_source_thread_id.as_deref() != Some(source_thread_id)
                    || !seed_sealed_matches
                {
                    bail!("target-authored runtime seed changed its resume or source authority");
                }
                crate::worker_handoff::validate_cross_site_capsule_transition(
                    &source_capsule,
                    &target_capsule,
                    target_resume,
                    &authority.placement,
                    &cas,
                )?;
                if metadata.admitted_launch_capsule()?.as_ref() != Some(&target_capsule) {
                    bail!(
                        "remote adoption target metadata does not reproduce its admitted capsule"
                    );
                }
                metadata
            }
            _ => {
                let source_launch_metadata = source_launch_metadata
                    .as_ref()
                    .expect("non-remote continuation source metadata was required above");
                let source_resume_context = source_resume_context
                    .as_ref()
                    .expect("non-remote continuation source resume was required above");
                let prepared = successor_launch_metadata.cloned();
                let mut metadata = prepared.clone().unwrap_or_else(|| {
                    source_launch_metadata
                        .continuation_successor_seed(source_resume_context.clone())
                });
                if prepared.is_none() && matches!(&kind, RunningContinuationKind::Machine { .. }) {
                    metadata.continuation_runtime_bootstrap = Some(
                        crate::launch_metadata::ContinuationRuntimeBootstrap::PredecessorNativeCheckpoint,
                    );
                }
                metadata
            }
        };
        let successor_resume_context =
            successor_meta
                .resume_context
                .as_ref()
                .cloned()
                .ok_or_else(|| {
                    anyhow!(
                        "successor {} has no captured ResumeContext",
                        successor.thread_id
                    )
                })?;
        if expected_resume_context.is_some_and(|expected| expected != &successor_resume_context) {
            bail!(
                "successor {} ResumeContext changed during authoritative preparation",
                successor.thread_id
            );
        }
        match &kind {
            RunningContinuationKind::RemoteAdoption { .. } => {}
            _ => successor_resume_context.validate_continuation_transition_from(
                source_resume_context
                    .as_ref()
                    .expect("non-remote continuation source resume was required above"),
                source_result_generation,
                crate::launch_metadata::ContinuationAuthorityTransitionKind::Inherit,
            )?,
        }
        if successor_meta
            .continuation_source_thread_id
            .as_deref()
            .is_some_and(|source| source != source_thread_id)
        {
            bail!("prepared successor names a different continuation source");
        }
        successor_meta.continuation_source_thread_id = Some(source_thread_id.to_string());
        match &kind {
            RunningContinuationKind::Machine { .. } => {
                if successor_meta.continuation_runtime_bootstrap.is_none() {
                    bail!("machine successor has no durable runtime-state bootstrap");
                }
            }
            RunningContinuationKind::RemoteAdoption { .. } => {
                if successor_meta.continuation_runtime_bootstrap
                    != Some(
                        crate::launch_metadata::ContinuationRuntimeBootstrap::ExternallyRestoredState,
                    )
                {
                    bail!("remote adoption must cold-start from externally restored state");
                }
            }
            RunningContinuationKind::GraphFollowResume => {
                if successor_meta.continuation_runtime_bootstrap.is_some() {
                    bail!("follow-resume successor carries machine runtime-state bootstrap");
                }
            }
        }
        let successor_runtime = RuntimeInfo {
            launch_metadata: Some(successor_meta.clone()),
            ..RuntimeInfo::default()
        };
        let (successor_snapshot, source_snapshot_before) =
            if let RunningContinuationKind::RemoteAdoption { authority } = &kind {
                let mut snapshot = build_continuation_snapshot(
                    &successor_with_upstream,
                    &successor_resume_context,
                )?;
                snapshot.admitted_launch_capsule_hash =
                    Some(authority.placement.target_launch_capsule_hash.clone());
                (
                    snapshot,
                    authoritative_snapshot_for_transition(&g, chain_root_id, source_thread_id)?,
                )
            } else {
                attach_continuation_launch_capsule(
                    &self.state_authority,
                    &self.bundle_admission_trust,
                    permit.cas_guard(),
                    &g,
                    build_continuation_snapshot(
                        &successor_with_upstream,
                        &successor_resume_context,
                    )?,
                    ContinuationCapsuleTransition {
                        chain_root_id,
                        source_thread_id,
                        launch_metadata: Some(&successor_meta),
                        expected_result_generation: source_result_generation,
                        kind: crate::launch_metadata::ContinuationAuthorityTransitionKind::Inherit,
                    },
                )?
            };

        // Same-node successors become locally runnable, so their runtime seed
        // is written before the authoritative edge. A remote successor is
        // deliberately absent from this node's runtime database; the target
        // may create its operational row only after it adopts the transferred
        // signed head.
        let local_runtime_seeded = !matches!(&kind, RunningContinuationKind::RemoteAdoption { .. });
        if local_runtime_seeded {
            if let Some(prepared) = successor_launch_metadata
                && (prepared.native_resume
                    != source_launch_metadata
                        .as_ref()
                        .expect("local continuation source metadata was required above")
                        .native_resume
                    || prepared.cancellation_mode
                        != source_launch_metadata
                            .as_ref()
                            .expect("local continuation source metadata was required above")
                            .cancellation_mode
                    || prepared.launch_driver
                        != source_launch_metadata
                            .as_ref()
                            .expect("local continuation source metadata was required above")
                            .launch_driver)
            {
                bail!(
                    "prepared successor execution policy differs from its source launch metadata"
                );
            }
            let _admission = g.state_db.authorize_runtime_pin(chain_root_id)?;
            g.runtime_db
                .insert_thread_runtime(&successor.thread_id, chain_root_id)?;
            let metadata_result = g
                .runtime_db
                .set_launch_metadata(&successor.thread_id, &successor_meta);
            if let Err(error) = metadata_result {
                let _ = self.delete_thread_runtime_locked(&g, &successor.thread_id);
                return Err(error);
            }
        }

        let remote_authority = match (&kind, remote_frontier.as_ref()) {
            (
                RunningContinuationKind::RemoteAdoption { authority },
                Some((source_chain_head_hash, source_last_event_hash, target_signer_fingerprint)),
            ) => {
                let placement = &authority.placement;
                if source_snapshot_before.current_site_id != placement.source_site_id
                    || source_snapshot_before.origin_site_id != placement.origin_site_id
                    || source_snapshot_before.requested_by.as_deref()
                        != Some(placement.owner_principal.as_str())
                    || source_snapshot_before
                        .admitted_launch_capsule_hash
                        .is_none()
                {
                    bail!("remote adoption source snapshot contradicts placement authority");
                }
                let accounting_transfer_hash = authority
                    .source_accounting_transfer
                    .as_ref()
                    .map(|transfer| -> Result<String> {
                        transfer.validate()?;
                        if transfer.operation_id != placement.operation_id {
                            bail!("accounting export belongs to another handoff operation");
                        }
                        self.state_authority.ensure_guard(permit.cas_guard())?;
                        self.state_authority
                            .cas_store()?
                            .store_object(&serde_json::to_value(transfer)?)
                    })
                    .transpose()?;
                let grant_evidence = crate::worker_handoff::chain_writer_transition_from_placement(
                    placement,
                    authority.placement_attestation_hash.clone(),
                    accounting_transfer_hash.clone(),
                    g.signer.fingerprint().to_string(),
                    target_signer_fingerprint.clone(),
                );
                let grant = grant_evidence.sign_attestation(g.signer.as_ref())?;
                self.state_authority.ensure_guard(permit.cas_guard())?;
                let cas = self.state_authority.cas_store()?;
                let seed_value = cas
                    .get_object(&placement.target_runtime_seed_hash)?
                    .ok_or_else(|| anyhow!("target placement runtime seed is absent at cut"))?;
                let seed =
                    ryeos_state::objects::PlacementRuntimeSeed::from_current_value(seed_value)?;
                if seed.content_hash()? != placement.target_runtime_seed_hash
                    || seed.target_launch_capsule_hash != placement.target_launch_capsule_hash
                {
                    bail!("target placement runtime seed changed before source cut");
                }
                let writer_grant_hash = cas
                    .store_object(&grant.to_value())
                    .context("store one-successor remote chain-writer grant")?;
                Some(ryeos_state::objects::RemoteContinuationAuthority {
                    schema: ryeos_state::objects::REMOTE_CONTINUATION_AUTHORITY_SCHEMA,
                    operation_id: placement.operation_id.clone(),
                    preflight_id: placement.preflight_id.clone(),
                    preflight_attestation_hash: placement.preflight_attestation_hash.clone(),
                    follow_delivery_reservation_attestation_hash: placement
                        .follow_delivery_reservation_attestation_hash
                        .clone(),
                    source_chain_head_hash: source_chain_head_hash.clone(),
                    source_last_event_hash: source_last_event_hash.clone(),
                    checkpoint_manifest_hash: placement.checkpoint_manifest_hash.clone(),
                    target_placement_attestation_hash: authority.placement_attestation_hash.clone(),
                    chain_writer_grant_hash: writer_grant_hash,
                    target_launch_capsule_hash: placement.target_launch_capsule_hash.clone(),
                    target_runtime_seed_hash: placement.target_runtime_seed_hash.clone(),
                    source_accounting_transfer_hash: accounting_transfer_hash,
                    source_site_id: placement.source_site_id.clone(),
                    target_site_id: placement.target_site_id.clone(),
                    target_node_signer_fingerprint: target_signer_fingerprint.clone(),
                    successor_thread_id: successor.thread_id.clone(),
                })
            }
            (RunningContinuationKind::RemoteAdoption { .. }, None) => {
                bail!("remote adoption lost its verified source frontier")
            }
            (_, Some(_)) => bail!("local continuation unexpectedly carries a remote frontier"),
            (_, None) => None,
        };

        // The successor becomes observable with its creation record and complete
        // authoritative launch audit in the same signed chain head. ResumeContext
        // was seeded above, so every visible successor is immediately relaunchable.
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
        // Settle the source to `continued` in the same signed head that creates
        // the successor and records its authoritative birth events.
        let now = lillux::time::iso8601_now();
        let mut source_snapshot_after =
            continued_snapshot_from_authoritative(source_snapshot_before.clone(), &now);
        source_snapshot_after.result_project_snapshot_hash =
            source_result_generation.map(|generation| generation.snapshot_hash.clone());
        if let Some(result_hash) =
            source_result_generation.map(|generation| generation.snapshot_hash.as_str())
            && successor_with_upstream
                .base_project_snapshot_hash
                .as_deref()
                != Some(result_hash)
        {
            bail!(
                "continuation successor {} base snapshot does not match frozen source result {}",
                successor.thread_id,
                result_hash
            );
        }
        source_snapshot_after.result_workspace_output_capture_hash =
            source_result_generation.and_then(|generation| generation.output_capture_hash.clone());
        source_snapshot_after
            .verify_result_workspace_output_capture(&self.state_authority.cas_store()?)?;
        let edge_reason: Option<&str> = match &kind {
            RunningContinuationKind::Machine { sanitized_reason } => *sanitized_reason,
            RunningContinuationKind::GraphFollowResume => {
                Some(queries::ContinuationReasonMarker::GraphFollowResume.as_str())
            }
            RunningContinuationKind::RemoteAdoption { .. } => Some("remote_adoption"),
        };
        let mut source_payload = json!({
            "successor_thread_id": &successor.thread_id,
            "reason": edge_reason,
            "result_project_snapshot_hash": source_snapshot_after.result_project_snapshot_hash,
            "result_workspace_output_capture_hash": source_snapshot_after.result_workspace_output_capture_hash,
        });
        if let Some(remote) = &remote_authority {
            source_payload
                .as_object_mut()
                .expect("continuation payload is an object")
                .insert(
                    "remote_adoption".to_string(),
                    serde_json::to_value(remote).context("encode remote continuation authority")?,
                );
        }
        let source_event = NewEventRecord {
            event_type: "thread_continued".to_string(),
            storage_class: "indexed".to_string(),
            payload: source_payload,
        };
        let mut ste = convert_events(
            std::slice::from_ref(&source_event),
            chain_root_id,
            source_thread_id,
        );
        if let Some(remote) = &remote_authority {
            ste.first_mut()
                .expect("running continuation source event batch is non-empty")
                .prev_thread_event_hash = Some(remote.source_last_event_hash.clone());
        }
        let expected_source_event = ste
            .first()
            .cloned()
            .expect("running continuation source event batch is non-empty");
        let expected_successor_snapshot = successor_snapshot.clone();
        let successor_commit = g.state_db.add_thread_with_events_and_append_admitted(
            chain_root_id,
            successor_snapshot,
            successor_thread_events,
            source_thread_id,
            ste,
            vec![SnapshotUpdate {
                thread_id: source_thread_id.to_string(),
                new_snapshot: source_snapshot_after.clone(),
            }],
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
                        source_snapshot_updated: true,
                        source_event: &expected_source_event,
                        successor_snapshot: &expected_successor_snapshot,
                        successor_events: &expected_successor_events,
                    },
                ) {
                    Ok(ContinuationCommitReadback::Exact(snapshot)) => {
                        tracing::error!(
                            thread_id = %successor.thread_id,
                            source_thread_id,
                            error = %error,
                            "running continuation returned an error after publishing authoritative state; continuing from exact committed pair"
                        );
                        return Ok(CreatedThreadPublication {
                            persisted: Vec::new(),
                            successor: thread_detail_from_committed_snapshot(
                                *snapshot,
                                successor_runtime,
                            ),
                            remote_writer_grant_hash: remote_authority
                                .as_ref()
                                .map(|remote| remote.chain_writer_grant_hash.clone()),
                        });
                    }
                    Err(authority_error) => {
                        return Err(anyhow!(
                            "running continuation failed and authoritative pair readback was unavailable; preserving runtime ownership for reconciliation: write error: {error:#}; readback error: {authority_error:#}"
                        ));
                    }
                    Ok(ContinuationCommitReadback::Ambiguous(reason)) => {
                        return Err(anyhow!(
                            "running continuation failed with ambiguous authoritative readback; preserving runtime ownership for reconciliation: {reason}; write error: {error:#}"
                        ));
                    }
                    Ok(ContinuationCommitReadback::ProvenAbsent) => {}
                }
                if local_runtime_seeded
                    && let Err(cleanup_error) =
                        self.delete_thread_runtime_locked(&g, &successor.thread_id)
                {
                    tracing::error!(
                        thread_id = %successor.thread_id,
                        error = %cleanup_error,
                        "failed to remove runtime row after atomic running-continuation birth failed"
                    );
                }
                return Err(error);
            }
        };
        let mut all_input_events = successor_events;
        all_input_events.push(source_event);
        let persisted = match persisted_from_add_thread_with_events(
            &successor_result,
            &all_input_events,
        ) {
            Ok(persisted) => persisted,
            Err(error) => {
                tracing::error!(
                    thread_id = %successor.thread_id,
                    source_thread_id,
                    chain_root_id,
                    error = %error,
                    "authoritative running continuation committed but live event reconstruction failed"
                );
                Vec::new()
            }
        };
        Ok(CreatedThreadPublication {
            persisted,
            successor: thread_detail_from_committed_snapshot(
                successor_result.snapshot,
                successor_runtime,
            ),
            remote_writer_grant_hash: remote_authority.map(|remote| remote.chain_writer_grant_hash),
        })
    }

    /// Operator follow-up continuation, made idempotent by a request fingerprint.
    ///
    /// Unlike [`Self::create_continuation`] (whose single-successor guard FAILS on
    /// a second follow-up), this dedups: under the write permit + lock, if the
    /// source already has a successor it compares the recorded
    /// `successor_request_fingerprint` to this request's — a match returns the
    /// existing successor (`Existing`, a double-submit), a mismatch is a
    /// `Conflict`. Otherwise it creates the successor and persists the fingerprint
    /// on the `thread_continued` edge in the SAME critical section, so dedup works
    /// even if the daemon crashes before the runtime emits anything. A terminal
    /// (completed/failed) source keeps its status; a running source is settled
    /// `continued` (same as `create_continuation`).
    // Idempotency identity, source lineage, launch metadata, and initial events
    // remain explicit at this atomic admission boundary.
    pub(crate) fn create_or_get_continuation_admitted(
        &self,
        successor: &NewThreadRecord,
        source_thread_id: &str,
        chain_root_id: &str,
        reason: Option<&str>,
        request_fingerprint: &str,
        launch_metadata: Option<&crate::launch_metadata::RuntimeLaunchMetadata>,
        initial_events: Vec<NewEventRecord>,
    ) -> Result<ContinuationOutcome> {
        let permit = self.acquire_write_permit()?;
        let g = self.lock()?;
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

        // The caller-supplied chain root must match the source's — a continuation
        // stays in the predecessor's chain. Fail loud on a mismatch rather than
        // braiding a successor into the wrong chain.
        if chain_root_id != source_row.chain_root_id {
            bail!(
                "chain_root_id mismatch: caller passed '{chain_root_id}' but source \
                 '{source_thread_id}' belongs to chain '{}'",
                source_row.chain_root_id
            );
        }

        if is_terminal_status(&source_row.status)
            && source_row.status != ThreadStatus::Failed.as_str()
            && source_row.status != ThreadStatus::Completed.as_str()
        {
            bail!(
                "cannot continue thread in terminal status '{}'",
                source_row.status
            );
        }

        // Idempotent get-or-conflict: a source already continued is deduped by
        // fingerprint instead of failing the single-successor guard.
        if let Some(existing) =
            queries::continuation_successor(g.state_db.projection(), source_thread_id)?
        {
            let existing_fp =
                queries::continuation_fingerprint(g.state_db.projection(), source_thread_id)?;
            return Ok(if existing_fp.as_deref() == Some(request_fingerprint) {
                ContinuationOutcome::Existing {
                    successor_thread_id: existing,
                }
            } else {
                ContinuationOutcome::Conflict {
                    successor_thread_id: existing,
                }
            });
        }

        let mut successor_with_upstream = successor.clone();
        if successor_with_upstream.upstream_thread_id.is_none() {
            successor_with_upstream.upstream_thread_id = Some(source_thread_id.to_string());
        }
        let effective_launch_metadata = launch_metadata
            .map(|metadata| {
                let mut metadata = metadata.clone();
                if metadata
                    .continuation_source_thread_id
                    .as_deref()
                    .is_some_and(|source| source != source_thread_id)
                {
                    bail!("prepared successor names a different continuation source");
                }
                metadata.continuation_source_thread_id = Some(source_thread_id.to_string());
                Ok(metadata)
            })
            .transpose()?;
        let base_successor_snapshot = match effective_launch_metadata
            .as_ref()
            .and_then(|metadata| metadata.resume_context.as_ref())
        {
            Some(resume) => build_continuation_snapshot(&successor_with_upstream, resume)?,
            None => build_snapshot(&successor_with_upstream),
        };
        let (successor_snapshot, source_snapshot_before) = attach_continuation_launch_capsule(
            &self.state_authority,
            &self.bundle_admission_trust,
            permit.cas_guard(),
            &g,
            base_successor_snapshot,
            ContinuationCapsuleTransition {
                chain_root_id,
                source_thread_id,
                launch_metadata: effective_launch_metadata.as_ref(),
                expected_result_generation: None,
                kind: crate::launch_metadata::ContinuationAuthorityTransitionKind::OperatorFollowUp,
            },
        )?;
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
            launch_metadata: effective_launch_metadata.clone(),
            ..RuntimeInfo::default()
        };
        // Seed runtime state before the atomic signed-head transition. Failure
        // leaves at most an auxiliary runtime row, which is removed below; the
        // successor snapshot and source edge are indivisible.
        {
            let _admission = g.state_db.authorize_runtime_pin(chain_root_id)?;
            g.runtime_db
                .insert_thread_runtime(&successor.thread_id, chain_root_id)?;

            // Seed the operator launch context before the successor is visible.
            if let Some(meta) = effective_launch_metadata.as_ref()
                && let Err(error) = g.runtime_db.set_launch_metadata(&successor.thread_id, meta)
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
                "successor_request_fingerprint": request_fingerprint,
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
            .expect("operator continuation source event batch is non-empty");
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
                        tracing::error!(
                            thread_id = %successor.thread_id,
                            source_thread_id,
                            error = %error,
                            "operator continuation returned an error after publishing authoritative state; continuing from exact committed pair"
                        );
                        return Ok(ContinuationOutcome::Created {
                            persisted: Vec::new(),
                            successor: Box::new(thread_detail_from_committed_snapshot(
                                *snapshot,
                                successor_runtime,
                            )),
                        });
                    }
                    Err(authority_error) => {
                        return Err(anyhow!(
                            "operator continuation failed and authoritative pair readback was unavailable; preserving runtime ownership for reconciliation: write error: {error:#}; readback error: {authority_error:#}"
                        ));
                    }
                    Ok(ContinuationCommitReadback::Ambiguous(reason)) => {
                        return Err(anyhow!(
                            "operator continuation failed with ambiguous authoritative readback; preserving runtime ownership for reconciliation: {reason}; write error: {error:#}"
                        ));
                    }
                    Ok(ContinuationCommitReadback::ProvenAbsent) => {}
                }
                if let Err(cleanup_error) =
                    self.delete_thread_runtime_locked(&g, &successor.thread_id)
                {
                    tracing::error!(
                        thread_id = %successor.thread_id,
                        error = %cleanup_error,
                        "failed to remove runtime row after atomic operator continuation birth failed"
                    );
                }
                return Err(error);
            }
        };
        let mut all_input_events = successor_events;
        all_input_events.push(source_event);
        let persisted = match persisted_from_add_thread_with_events(
            &successor_result,
            &all_input_events,
        ) {
            Ok(persisted) => persisted,
            Err(error) => {
                tracing::error!(
                    thread_id = %successor.thread_id,
                    source_thread_id,
                    chain_root_id,
                    error = %error,
                    "authoritative operator continuation committed but live event reconstruction failed"
                );
                Vec::new()
            }
        };
        Ok(ContinuationOutcome::Created {
            persisted,
            successor: Box::new(thread_detail_from_committed_snapshot(
                successor_result.snapshot,
                successor_runtime,
            )),
        })
    }

    /// Raw idempotent continuation fixture for state-layer tests. Absent in
    /// production.
    #[cfg(any(test, feature = "test-support"))]
    pub fn create_or_get_continuation_for_test(
        &self,
        successor: &NewThreadRecord,
        source_thread_id: &str,
        chain_root_id: &str,
        reason: Option<&str>,
        request_fingerprint: &str,
        resume_context: Option<&crate::launch_metadata::ResumeContext>,
    ) -> Result<ContinuationOutcome> {
        let launch_metadata = resume_context.cloned().map(|resume_context| {
            crate::launch_metadata::RuntimeLaunchMetadata::default()
                .with_launch_driver(ryeos_state::objects::ExecutionLaunchDriver::ManagedRuntime)
                .with_resume_context(resume_context)
        });
        self.create_or_get_continuation_admitted(
            successor,
            source_thread_id,
            chain_root_id,
            reason,
            request_fingerprint,
            launch_metadata.as_ref(),
            Vec::new(),
        )
    }

    /// The `successor_request_fingerprint` recorded on a source's
    /// `thread_continued` edge, if any — used to dedup operator double-submits.
    pub fn get_continuation_fingerprint(&self, thread_id: &str) -> Result<Option<String>> {
        let g = self.lock()?;
        queries::continuation_fingerprint(g.state_db.projection(), thread_id)
    }

    /// Whether `source_thread_id`'s continuation edge is a follow-resume edge
    /// pointing at `successor_thread_id`. Such a successor has the same shape as a
    /// stranded machine continuation but must NOT be auto-launched — it waits for
    /// the followed child's result. Target-aware: another created row that merely
    /// names the same upstream is NOT matched.
    pub fn is_follow_resume_successor(
        &self,
        source_thread_id: &str,
        successor_thread_id: &str,
    ) -> Result<bool> {
        let g = self.lock()?;
        Ok(matches!(
            queries::continuation_edge(g.state_db.projection(), source_thread_id)?,
            Some((succ, Some(reason), _))
                if succ == successor_thread_id
                    && reason == queries::ContinuationReasonMarker::GraphFollowResume.as_str()
        ))
    }

    /// Reserve a durable, owner-bound planning handle before any launch task is
    /// spawned. The reserved thread id remains internal until authoritative
    /// thread publication binds it.
    pub fn reserve_launch_planning(
        &self,
        reserved_thread_id: &str,
        requested_by: &str,
    ) -> std::result::Result<String, LaunchPlanningReservationError> {
        let launch_id = format!("L-{}", uuid::Uuid::new_v4().simple());
        self.reserve_launch_planning_with_id(&launch_id, reserved_thread_id, requested_by)?;
        Ok(launch_id)
    }
}
