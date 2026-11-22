//! Remote adoption publication, runtime seeds and recovery.
use super::*;

/// Complete in-memory operands for the source node's one atomic remote
/// continuation cut. Durable authority is carried by the target placement
/// attestation, source writer grant, canonical continuation event, target
/// capsule, checkpoint, and resulting signed chain head—not by this carrier.
#[derive(Debug, Clone)]
pub struct RemoteAdoptionContinuationAuthority {
    pub placement_attestation_hash: String,
    pub placement: crate::worker_handoff::WorkerPlacementAdmissionEvidence,
    /// Irreversible, externally anchored source-ledger export. Required iff
    /// the admitted launch carries financial accounting authority.
    pub source_accounting_transfer: Option<ryeos_state::objects::AccountingAllowanceTransfer>,
    /// Exact target key pinned by the configured remote used for this
    /// placement. It is an in-memory verification operand, not chain authority
    /// and never widens the node's local head trust store.
    pub target_node_verifying_key: lillux::crypto::VerifyingKey,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteAdoptionPublication {
    pub successor_thread_id: String,
    pub writer_grant_hash: String,
    pub target_launch_capsule_hash: String,
    pub target_runtime_seed_hash: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RetainedRemoteFollowTerminalJobResult {
    Pending,
    Completed,
    RepairMissingRequest,
}

pub(super) fn classify_retained_remote_follow_terminal_job_result(
    job: &ryeos_state::SyncJobRecord,
    request: &crate::federated_follow::RemoteFollowTerminalDeliveryRequest,
) -> Result<RetainedRemoteFollowTerminalJobResult> {
    request.validate()?;
    if let Some(result) = &job.result {
        return match job.state {
            ryeos_state::SyncJobState::Planned
            | ryeos_state::SyncJobState::Running
            | ryeos_state::SyncJobState::Retryable => {
                let retained_request: crate::federated_follow::RemoteFollowTerminalDeliveryRequest =
                    serde_json::from_value(result.clone())?;
                retained_request.validate()?;
                if retained_request != *request {
                    bail!("remote follow terminal job retained another delivery request");
                }
                Ok(RetainedRemoteFollowTerminalJobResult::Pending)
            }
            ryeos_state::SyncJobState::Completed => {
                let response: crate::federated_follow::RemoteFollowTerminalDeliveryResponse =
                    serde_json::from_value(result.clone())?;
                response.validate_against(request)?;
                Ok(RetainedRemoteFollowTerminalJobResult::Completed)
            }
            ryeos_state::SyncJobState::Failed | ryeos_state::SyncJobState::Cancelled => {
                bail!("remote follow terminal delivery ended without settlement")
            }
        };
    }

    // `create_sync_job` and the request-retaining update are two SQLite
    // transactions. A process death between them leaves exactly this pristine
    // planned record. Any attempt or other progress without a request is
    // contradictory rather than grounds for guessed recovery.
    if job.state != ryeos_state::SyncJobState::Planned
        || job.phase != "planned"
        || job.attempt_count != 0
        || !job.uploaded_hashes.is_empty()
        || !job.fetched_hashes.is_empty()
        || job.last_error.is_some()
        || job.finished_at.is_some()
    {
        bail!("remote follow terminal job made progress without its delivery request");
    }
    Ok(RetainedRemoteFollowTerminalJobResult::RepairMissingRequest)
}

pub(super) fn retained_remote_follow_reservation(
    state_authority: &ryeos_state::PinnedStateAuthority,
    inner: &Inner,
    chain_root_id: &str,
) -> Result<
    Option<(
        String,
        ryeos_state::objects::Attestation,
        crate::federated_follow::RemoteFollowReservationEvidence,
    )>,
> {
    let cas = state_authority.cas_store()?;
    let mut cursor = chain_root_id.to_owned();
    let mut visited = HashSet::new();
    let mut retained = None;
    for _ in 0..1024 {
        if !visited.insert(cursor.clone()) {
            bail!("remote follow reservation lineage contains a cycle");
        }
        let Some(readback) = inner
            .state_db
            .read_authoritative_thread_snapshot_with_last_event(chain_root_id, &cursor)?
        else {
            if cursor == chain_root_id {
                return Ok(None);
            }
            bail!("remote follow reservation lineage successor disappeared");
        };
        if let Some((_event_hash, event)) = readback.last_event
            && event.event_type == ryeos_state::event_types::THREAD_CONTINUED
            && let Some(value) = event.payload.get("remote_adoption")
        {
            let authority: ryeos_state::objects::RemoteContinuationAuthority =
                serde_json::from_value(value.clone())
                    .context("decode retained remote continuation authority")?;
            authority.validate()?;
            if let Some(hash) = authority.follow_delivery_reservation_attestation_hash {
                if let Some((existing, _, _)) = &retained
                    && existing != &hash
                {
                    bail!("remote continuation lineage changed its parent follow reservation");
                }
                let value = cas
                    .get_object(&hash)?
                    .ok_or_else(|| anyhow!("retained remote follow reservation is absent"))?;
                if ryeos_state::objects::canonical_value_digest(&value)? != hash {
                    bail!("retained remote follow reservation changed digest");
                }
                let attestation = ryeos_state::objects::Attestation::from_value(&value)?;
                let evidence =
                    crate::federated_follow::RemoteFollowReservationEvidence::from_attestation(
                        &attestation,
                    )?;
                if evidence.child_chain_root_id != chain_root_id {
                    bail!("retained remote follow reservation belongs to another child chain");
                }
                retained = Some((hash, attestation, evidence));
            }
        }
        let Some(successor) =
            queries::continuation_successor(inner.state_db.projection(), &cursor)?
        else {
            return Ok(retained);
        };
        cursor = successor;
    }
    bail!("remote follow reservation lineage exceeds its bounded depth")
}

pub(super) fn validate_remote_adoption_runtime_seed(
    state_authority: &ryeos_state::PinnedStateAuthority,
    inner: &Inner,
    transition: &ryeos_state::sync::AdmittedChainWriterTransition,
    require_adopted_head: bool,
) -> Result<(
    crate::launch_metadata::RuntimeLaunchMetadata,
    Option<(String, ryeos_state::objects::AccountingAllowanceTransfer)>,
)> {
    transition.validate()?;
    let evidence = &transition.evidence;
    if evidence.target_node_signer_fingerprint != inner.signer.fingerprint() {
        bail!("remote adoption runtime seed is not addressed to this node signer");
    }

    let cas = state_authority.cas_store()?;
    let placement_value = cas
        .get_object(&evidence.placement_attestation_hash)?
        .ok_or_else(|| anyhow!("remote adoption placement attestation is absent"))?;
    let placement_attestation = ryeos_state::objects::Attestation::from_value(&placement_value)?;
    placement_attestation.verify_with_key(&transition.target_node_verifying_key)?;
    // Expiry is the pre-commit admission lease. Once the exact target-signed
    // head exists, replay and crash recovery must not reinterpret that durable
    // authority as expired. All immutable placement/writer/capsule facts are
    // still reverified below, as is the exact adopted head when requested.
    if !require_adopted_head && placement_attestation.is_expired_at(&lillux::time::iso8601_now())? {
        bail!("remote adoption placement attestation is expired");
    }
    let placement = crate::worker_handoff::WorkerPlacementAdmissionEvidence::from_attestation(
        &placement_attestation,
    )?;
    let preflight_value = cas
        .get_object(&placement.preflight_attestation_hash)?
        .ok_or_else(|| anyhow!("remote adoption preflight receipt is absent"))?;
    let preflight_attestation = ryeos_state::objects::Attestation::from_value(&preflight_value)?;
    preflight_attestation.verify_with_key(&transition.target_node_verifying_key)?;
    let preflight = crate::worker_handoff::WorkerPlacementPreflightEvidence::from_attestation(
        &preflight_attestation,
    )?;
    crate::worker_handoff::validate_target_realization_after_preflight(
        &cas,
        &preflight.target_execution_realization_hash,
        &placement.target_execution_realization_hash,
    )?;
    if let Some(hash) = &preflight.follow_delivery_reservation_attestation_hash {
        let reservation_value = cas
            .get_object(hash)?
            .ok_or_else(|| anyhow!("remote follow delivery reservation is absent"))?;
        if ryeos_state::objects::canonical_value_digest(&reservation_value)? != *hash {
            bail!("remote follow delivery reservation changed digest");
        }
        let reservation = ryeos_state::objects::Attestation::from_value(&reservation_value)?;
        let evidence = crate::federated_follow::RemoteFollowReservationEvidence::from_attestation(
            &reservation,
        )?;
        if evidence.owner_principal != preflight.owner_principal
            || evidence.child_chain_root_id != preflight.chain_root_id
            || evidence.child_initial_thread_id != preflight.chain_root_id
        {
            bail!("remote follow delivery reservation contradicts placement authority");
        }
    }
    if placement.preflight_id != preflight.preflight_id
        || placement.follow_delivery_reservation_attestation_hash
            != preflight.follow_delivery_reservation_attestation_hash
        || placement.owner_principal != preflight.owner_principal
        || placement.chain_root_id != preflight.chain_root_id
        || placement.origin_site_id != preflight.origin_site_id
        || placement.source_site_id != preflight.source_site_id
        || placement.target_site_id != preflight.target_site_id
        || placement.source_placement_thread_id != preflight.source_placement_thread_id
        || placement.successor_placement_thread_id != preflight.successor_placement_thread_id
        || placement.outer_exact_program_hash != preflight.outer_exact_program_hash
        || placement.persistent_dependency_programs != preflight.persistent_dependency_programs
        || placement.target_persistent_session_capsules
            != preflight.target_persistent_session_capsules
        || placement.credential_reservation.profile_id != preflight.target_credential_profile_id
        || placement.credential_reservation.generation != preflight.target_credential_generation
        || placement.credential_reservation.upstream_session_id != preflight.upstream_session_id
        || placement.credential_reservation.subject_contract_digest
            != preflight.credential_subject_contract_digest
        || placement.credential_reservation.subject_digest != preflight.credential_subject_digest
        || placement.project_rebind.route_digest != preflight.project_route_digest
        || placement
            .project_rebind
            .target_expected_head_hash
            .as_deref()
            != Some(preflight.target_project_head_hash.as_str())
    {
        bail!("remote adoption final placement contradicts its target preflight receipt");
    }
    match &placement.project_rebind.target_authority {
        ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
            display_path: Some(path),
            ..
        } if path.to_str() == Some(preflight.target_project_path.as_str()) => {}
        _ => bail!("remote adoption target path differs from its preflight receipt"),
    }
    let expected_writer = crate::worker_handoff::chain_writer_transition_from_placement(
        &placement,
        evidence.placement_attestation_hash.clone(),
        evidence.source_accounting_transfer_hash.clone(),
        evidence.source_node_signer_fingerprint.clone(),
        evidence.target_node_signer_fingerprint.clone(),
    );
    if expected_writer != *evidence {
        bail!("remote adoption placement and writer grant describe different transitions");
    }
    let writer_value = cas
        .get_object(&transition.writer_grant_hash)?
        .ok_or_else(|| anyhow!("remote adoption writer grant is absent"))?;
    let writer = ryeos_state::objects::Attestation::from_value(&writer_value)?;
    writer.verify_with_key(&transition.source_node_verifying_key)?;
    if ryeos_state::objects::ChainWriterTransitionEvidence::from_attestation(&writer)? != *evidence
    {
        bail!("remote adoption writer grant differs from its admitted transition");
    }
    let accounting_transfer = match (
        placement.accounting.source_scope.as_ref(),
        evidence.source_accounting_transfer_hash.as_ref(),
    ) {
        (None, None) if placement.accounting.target_scope.is_none() => None,
        (Some(source_scope), Some(hash)) => {
            let value = cas
                .get_object(hash)?
                .ok_or_else(|| anyhow!("remote adoption accounting transfer is absent"))?;
            let transfer: ryeos_state::objects::AccountingAllowanceTransfer =
                serde_json::from_value(value)?;
            if transfer.content_hash()? != *hash
                || &transfer.source_scope != source_scope
                || placement.accounting.target_scope.as_ref() != Some(&transfer.target_scope)
                || transfer.operation_id != placement.operation_id
                || transfer.source_chain_root_id != placement.chain_root_id
                || transfer.source_placement_thread_id != placement.source_placement_thread_id
                || transfer.source_financial_high_water_before
                    != placement.accounting.source_financial_high_water
                || transfer.source_charged_usd_nanos
                    != placement.accounting.source_charged_usd_nanos
                || transfer.target_cap_usd_nanos != placement.accounting.target_cap_usd_nanos
                || transfer.target_directive_cap_usd_nanos
                    != placement.accounting.target_directive_cap_usd_nanos
            {
                bail!("remote adoption accounting transfer contradicts placement authority");
            }
            Some((hash.clone(), transfer))
        }
        _ => bail!("remote adoption accounting transfer presence contradicts its placement"),
    };

    let seed_value = cas
        .get_object(&placement.target_runtime_seed_hash)?
        .ok_or_else(|| anyhow!("remote adoption placement runtime seed is absent"))?;
    let seed = ryeos_state::objects::PlacementRuntimeSeed::from_current_value(seed_value)?;
    if seed.content_hash()? != placement.target_runtime_seed_hash
        || seed.operation_id != placement.operation_id
        || seed.chain_root_id != evidence.chain_root_id
        || seed.source_placement_thread_id != evidence.source_placement_thread_id
        || seed.successor_placement_thread_id != evidence.successor_placement_thread_id
        || seed.target_site_id != evidence.target_site_id
        || seed.owner_principal != evidence.owner_principal
        || seed.target_launch_capsule_hash != evidence.transition_subject_hash
    {
        bail!("remote adoption placement runtime seed contradicts signed placement authority");
    }
    let (mut metadata_file, metadata_size) = cas
        .open_blob(&seed.launch_metadata_blob_hash)?
        .ok_or_else(|| anyhow!("remote adoption placement runtime metadata is absent"))?;
    if metadata_size != seed.launch_metadata_size_bytes
        || metadata_size > ryeos_state::objects::MAX_PLACEMENT_RUNTIME_METADATA_BYTES
    {
        bail!("remote adoption placement runtime metadata size changed");
    }
    let mut metadata_bytes = Vec::with_capacity(
        usize::try_from(metadata_size).context("placement runtime metadata size overflow")?,
    );
    metadata_file
        .by_ref()
        .take(ryeos_state::objects::MAX_PLACEMENT_RUNTIME_METADATA_BYTES.saturating_add(1))
        .read_to_end(&mut metadata_bytes)
        .context("read bounded placement runtime metadata")?;
    if u64::try_from(metadata_bytes.len())? != metadata_size
        || lillux::sha256_hex(&metadata_bytes) != seed.launch_metadata_blob_hash
    {
        bail!("remote adoption placement runtime metadata digest changed");
    }
    let metadata_value: Value = serde_json::from_slice(&metadata_bytes)
        .context("decode placement runtime metadata JSON")?;
    if lillux::canonical_json(&metadata_value)?.as_bytes() != metadata_bytes {
        bail!("remote adoption placement runtime metadata is not canonical JSON");
    }
    let target_launch_metadata: crate::launch_metadata::RuntimeLaunchMetadata =
        serde_json::from_value(metadata_value).context("decode placement runtime metadata")?;
    target_launch_metadata.validate()?;
    let target_isolation_digest =
        ryeos_state::objects::canonical_value_digest(&serde_json::to_value(
            target_launch_metadata
                .isolation
                .as_ref()
                .ok_or_else(|| anyhow!("remote adoption runtime seed has no isolation"))?,
        )?)?;
    if target_isolation_digest != preflight.target_isolation_digest {
        bail!("remote adoption isolation differs from its target preflight");
    }
    if target_launch_metadata
        .continuation_source_thread_id
        .as_deref()
        != Some(evidence.source_placement_thread_id.as_str())
    {
        bail!("remote adoption runtime seed names another continuation source");
    }
    let target_capsule = target_launch_metadata
        .admitted_launch_capsule()?
        .ok_or_else(|| anyhow!("remote adoption runtime seed has no admitted launch capsule"))?;
    if target_capsule.content_hash()? != evidence.transition_subject_hash
        || target_capsule.content_hash()? != placement.target_launch_capsule_hash
    {
        bail!("remote adoption runtime seed does not reproduce the admitted target capsule");
    }
    target_capsule
        .verify_retained_execution_realization(
            &cas,
            &state_authority.large_object_store()?,
            state_authority.trust_store(),
        )
        .context("verify remote adoption target execution realization")?;
    let target_resume = target_launch_metadata
        .resume_context
        .as_ref()
        .ok_or_else(|| anyhow!("remote adoption runtime seed has no ResumeContext"))?;
    if target_resume.current_site_id != evidence.target_site_id
        || target_resume.origin_site_id != evidence.origin_site_id
        || target_resume.principal_identifier() != evidence.owner_principal
    {
        bail!("remote adoption runtime seed contradicts owner or site authority");
    }
    if target_resume
        .parameters
        .get("credential_profile_id")
        .and_then(Value::as_str)
        != Some(placement.credential_reservation.profile_id.as_str())
    {
        bail!("remote adoption runtime seed selects another credential profile");
    }

    let checkpoint_value = cas
        .get_object(&placement.checkpoint_manifest_hash)?
        .ok_or_else(|| anyhow!("remote adoption checkpoint manifest is absent"))?;
    let checkpoint = StateManifest::from_current_value(checkpoint_value)?;
    if checkpoint.contract != ryeos_state::objects::WORKER_SESSION_RESTORE_CONTRACT
        || checkpoint.publisher_chain_root_id != evidence.chain_root_id
        || checkpoint.publisher_thread_id != evidence.source_placement_thread_id
    {
        bail!("remote adoption checkpoint manifest contradicts the writer transition");
    }
    let restore_bytes = cas
        .get_blob(&checkpoint.restore.blob_hash)?
        .ok_or_else(|| anyhow!("remote adoption restore document is absent"))?;
    if lillux::sha256_hex(&restore_bytes) != checkpoint.restore.blob_hash {
        bail!("remote adoption restore document digest changed");
    }
    let restore_value: Value = serde_json::from_slice(&restore_bytes)?;
    if lillux::canonical_json(&restore_value)?.as_bytes() != restore_bytes {
        bail!("remote adoption restore document is not canonical JSON");
    }
    let restore = ryeos_state::objects::WorkerSessionRestore::from_current_value(restore_value)?;
    let restored_dependencies = restore
        .persistent_dependencies
        .iter()
        .map(|(name, dependency)| (name.clone(), dependency.exact_program_hash.clone()))
        .collect::<BTreeMap<_, _>>();
    if restore.source_position.chain_root_id != evidence.chain_root_id
        || restore.source_position.placement_thread_id != evidence.source_placement_thread_id
        || restore.project_candidate_snapshot_hash.as_deref()
            != Some(
                placement
                    .project_rebind
                    .source_candidate_snapshot_hash
                    .as_str(),
            )
        || restore.outer_exact_program_hash != placement.outer_exact_program_hash
        || restored_dependencies != placement.persistent_dependency_programs
        || restore.upstream_session_id != placement.credential_reservation.upstream_session_id
        || restore.credential_subject_contract_digest
            != placement.credential_reservation.subject_contract_digest
        || restore.credential_subject_digest != placement.credential_reservation.subject_digest
        || restore.source_site_id != placement.source_site_id
        || restore.source_launch_capsule_hash != preflight.source_launch_capsule_hash
    {
        bail!("remote adoption checkpoint restore contradicts placement authority");
    }

    let reservation = inner
        .runtime_db
        .credential_profile_reservation_for_successor(&evidence.successor_placement_thread_id)?
        .ok_or_else(|| anyhow!("remote adoption credential reservation is absent"))?;
    let admitted_reservation = &placement.credential_reservation;
    if reservation.reservation_id != admitted_reservation.reservation_id
        || reservation.operation_id != placement.operation_id
        || reservation.successor_thread_id != evidence.successor_placement_thread_id
        || reservation.profile_id != admitted_reservation.profile_id
        || reservation.owner_principal != admitted_reservation.owner_principal
        || reservation.credential_generation != admitted_reservation.generation
        || reservation.subject_contract_digest != admitted_reservation.subject_contract_digest
        || reservation.subject_digest != admitted_reservation.subject_digest
        || reservation.checkpoint_manifest_hash != placement.checkpoint_manifest_hash
        || reservation.upstream_session_id != admitted_reservation.upstream_session_id
        || reservation.state != "reserved"
    {
        bail!("remote adoption credential reservation differs from signed placement authority");
    }

    if require_adopted_head {
        let head = inner
            .state_db
            .read_generic_head_ref("chains", &evidence.chain_root_id)?
            .ok_or_else(|| anyhow!("adopted remote chain head is absent"))?;
        if head.target_hash != transition.target_chain_head_hash
            || head.signer != evidence.target_node_signer_fingerprint
        {
            bail!("remote adoption target head is not the exact locally signed successor");
        }
        let successor = inner
            .state_db
            .read_authoritative_thread_snapshot(
                &evidence.chain_root_id,
                &evidence.successor_placement_thread_id,
            )?
            .ok_or_else(|| anyhow!("adopted remote successor snapshot is absent"))?;
        if successor.current_site_id != evidence.target_site_id
            || successor.origin_site_id != evidence.origin_site_id
            || successor.requested_by.as_deref() != Some(evidence.owner_principal.as_str())
            || successor.admitted_launch_capsule_hash.as_deref()
                != Some(evidence.transition_subject_hash.as_str())
        {
            bail!("adopted remote successor snapshot contradicts its runtime seed");
        }
        let source_readback = inner
            .state_db
            .read_authoritative_thread_snapshot_with_last_event(
                &evidence.chain_root_id,
                &evidence.source_placement_thread_id,
            )?
            .ok_or_else(|| anyhow!("adopted remote source snapshot is absent"))?;
        let source = source_readback.snapshot;
        let (_continuation_hash, continuation_event) = source_readback
            .last_event
            .ok_or_else(|| anyhow!("adopted remote source has no continuation event"))?;
        let remote: ryeos_state::objects::RemoteContinuationAuthority = serde_json::from_value(
            continuation_event
                .payload
                .get("remote_adoption")
                .cloned()
                .ok_or_else(|| anyhow!("adopted remote source has no remote authority"))?,
        )?;
        remote.validate()?;
        if remote.operation_id != placement.operation_id
            || remote.preflight_id != placement.preflight_id
            || remote.preflight_attestation_hash != placement.preflight_attestation_hash
            || remote.follow_delivery_reservation_attestation_hash
                != placement.follow_delivery_reservation_attestation_hash
            || remote.source_chain_head_hash != placement.source_chain_head_hash
            || remote.source_last_event_hash != placement.source_last_event_hash
            || remote.checkpoint_manifest_hash != placement.checkpoint_manifest_hash
            || remote.target_placement_attestation_hash != evidence.placement_attestation_hash
            || remote.chain_writer_grant_hash != transition.writer_grant_hash
            || remote.target_launch_capsule_hash != placement.target_launch_capsule_hash
            || remote.target_runtime_seed_hash != placement.target_runtime_seed_hash
            || remote.source_site_id != placement.source_site_id
            || remote.target_site_id != placement.target_site_id
            || remote.target_node_signer_fingerprint != evidence.target_node_signer_fingerprint
            || remote.successor_thread_id != placement.successor_placement_thread_id
        {
            bail!("adopted remote continuation contradicts signed placement authority");
        }
        if source.status != ThreadStatus::Continued
            || source.result_project_snapshot_hash.as_deref()
                != Some(
                    placement
                        .project_rebind
                        .source_candidate_snapshot_hash
                        .as_str(),
                )
            || source.admitted_launch_capsule_hash.as_deref()
                != Some(restore.source_launch_capsule_hash.as_str())
        {
            bail!("adopted remote source settlement contradicts its checkpoint restore");
        }
    }
    Ok((target_launch_metadata, accounting_transfer))
}

impl StateStore {
    /// Atomically make one prepared target placement fetchable and record its
    /// durable job phase. The capsule and recovery seed become CAS roots before
    /// the signed placement admission/head is exposed; the returned hash is
    /// therefore never a promise about collectable or absent bytes.
    pub fn publish_worker_placement_preparation(
        &self,
        job_id: &str,
        transfer_manifest_hash: &str,
        target_capsule: &ryeos_state::objects::AdmittedLaunchCapsule,
        prepared_seed: &crate::worker_handoff::PreparedPlacementRuntimeSeed,
        attestation: &ryeos_state::objects::Attestation,
        progress: &crate::worker_handoff::WorkerSessionHandoffProgress,
        admission_limits: ryeos_state::object_closure::ObjectClosureLimits,
    ) -> Result<ryeos_state::admission::AdmissionResult> {
        target_capsule.validate()?;
        prepared_seed.object.validate()?;
        progress.validate()?;
        let capsule_hash = target_capsule.content_hash()?;
        let seed_hash = prepared_seed.object_hash()?;
        if attestation.subject_hash != capsule_hash
            || progress.placement_attestation_hash.is_none()
            || progress.target_runtime_seed_hash.as_deref() != Some(seed_hash.as_str())
        {
            bail!("target placement publication operands contradict one another");
        }
        let permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let job = g
            .state_db
            .get_sync_job(job_id)?
            .ok_or_else(|| anyhow!("target worker handoff job does not exist"))?;
        let current_progress =
            validate_target_preparation_job_transition(&job, transfer_manifest_hash, progress)?;
        self.state_authority.ensure_guard(permit.cas_guard())?;
        let cas = self.state_authority.cas_store()?;
        let target_capsule_bytes = target_capsule.to_canonical_bytes()?;
        if cas.store_canonical_object_bytes_bounded(
            &target_capsule_bytes,
            ryeos_state::objects::MAX_ADMITTED_LAUNCH_CAPSULE_BYTES,
        )? != capsule_hash
        {
            bail!("stored target placement capsule digest changed");
        }
        if cas.put_blob(&prepared_seed.launch_metadata_bytes)?.hash
            != prepared_seed.object.launch_metadata_blob_hash
        {
            bail!("stored target placement runtime metadata digest changed");
        }
        if cas.put_object(&prepared_seed.object.to_value()?)?.hash != seed_hash {
            bail!("stored target placement runtime seed digest changed");
        }
        let admission = ryeos_state::admission::publish_admission_attestation(
            &g.state_db,
            attestation,
            admission_limits,
            g.signer.as_ref(),
            permit.cas_guard(),
        )?;
        let mut stored_progress = progress.clone();
        stored_progress.placement_attestation_hash = Some(admission.attestation_hash.clone());
        stored_progress.validate()?;
        let mut retained_roots = job.roots.clone();
        retained_roots.extend([
            transfer_manifest_hash.to_owned(),
            capsule_hash,
            seed_hash,
            admission.attestation_hash.clone(),
        ]);
        retained_roots.sort();
        retained_roots.dedup();
        if let Some(current) = current_progress
            && current.phase == crate::worker_handoff::WorkerHandoffPhase::TargetPrepared
        {
            if current != stored_progress || retained_roots != job.roots || job.heads.is_empty() {
                bail!("replayed target preparation differs from its durable publication");
            }
            return Ok(admission);
        }
        g.state_db.update_sync_job(
            job_id,
            &ryeos_state::SyncJobUpdate {
                state: ryeos_state::SyncJobState::Running,
                phase: progress.phase.as_str().to_owned(),
                roots: Some(retained_roots),
                heads: None,
                uploaded_hashes: Vec::new(),
                fetched_hashes: job.roots,
                last_error: None,
                result: Some(stored_progress.to_value()?),
            },
        )?;
        Ok(admission)
    }

    /// Adopt an exact cross-site continuation head through the ordinary staged
    /// import path, then idempotently materialize its target-local runtime seed.
    /// The head is authoritative; SQLite installation is recoverable and never
    /// precedes writer-transfer verification.
    pub fn finalize_remote_adoption_import(
        &self,
        staged: ryeos_state::sync::StagedChainImport,
        transition: &ryeos_state::sync::AdmittedChainWriterTransition,
        admit_accounting: impl FnOnce(
            Option<&(String, ryeos_state::objects::AccountingAllowanceTransfer)>,
        ) -> Result<()>,
    ) -> Result<ryeos_state::sync::ImportResult> {
        let permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        validate_remote_adoption_runtime_seed(&self.state_authority, &g, transition, false)?;
        let result = ryeos_state::sync::finalize_transferred_import(
            &g.state_db,
            staged,
            transition,
            g.signer.as_ref(),
            permit.cas_guard(),
        )?;
        let (target_launch_metadata, accounting_transfer) =
            validate_remote_adoption_runtime_seed(&self.state_authority, &g, transition, true)?;
        admit_accounting(accounting_transfer.as_ref())?;
        g.runtime_db.install_imported_thread_runtime(
            &transition.evidence.successor_placement_thread_id,
            &transition.evidence.chain_root_id,
            &target_launch_metadata,
        )?;
        Ok(result)
    }

    /// Repair the only allowed crash gap after a transferred head was adopted
    /// but before its operational runtime seed was installed. No staged bytes
    /// or remote phase claims are consulted; the current signed head and the
    /// complete writer/placement/capsule authority are reverified.
    pub fn recover_remote_adoption_runtime(
        &self,
        transition: &ryeos_state::sync::AdmittedChainWriterTransition,
        admit_accounting: impl FnOnce(
            Option<&(String, ryeos_state::objects::AccountingAllowanceTransfer)>,
        ) -> Result<()>,
    ) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let (target_launch_metadata, accounting_transfer) =
            validate_remote_adoption_runtime_seed(&self.state_authority, &g, transition, true)?;
        admit_accounting(accounting_transfer.as_ref())?;
        g.runtime_db.install_imported_thread_runtime(
            &transition.evidence.successor_placement_thread_id,
            &transition.evidence.chain_root_id,
            &target_launch_metadata,
        )
    }

    #[track_caller]
    pub(super) fn lock(&self) -> Result<StateStoreGuard<'_>> {
        let started = lillux::time::MonotonicTimer::start();
        let caller = std::panic::Location::caller();
        let inner = cooperate_while_blocking(|| {
            self.inner
                .lock()
                .map_err(|e| anyhow!("StateStore lock poisoned: {e}"))
        })?;
        let waited = started.elapsed();
        if waited >= std::time::Duration::from_millis(100) {
            record_store_contention(StoreContentionKind::Wait, waited, caller);
        }
        Ok(StateStoreGuard {
            store_mutex: &self.inner,
            inner,
            acquired_at: lillux::time::MonotonicTimer::start(),
            caller,
        })
    }

    /// Create the parent's follow-resume successor: a running-source continuation
    /// marked `graph_follow_resume`. Created and seeded only — NOT launched (the
    /// resume path launches it later, once the child's result is available) and
    /// NOT subject to the autonomous chain-depth cap (a follow is structural
    /// progress, not an autonomous run). Daemon-only: the trusted marker cannot be
    /// reached through a runtime-supplied reason. The running source must declare
    /// native resume, and that exact policy is projected into the successor so a
    /// resumed segment remains eligible to suspend at another follow node.
    pub fn create_follow_resume_successor(
        &self,
        successor: &NewThreadRecord,
        source_thread_id: &str,
        chain_root_id: &str,
    ) -> Result<Vec<PersistedEventRecord>> {
        self.create_running_continuation_successor(
            successor,
            source_thread_id,
            chain_root_id,
            RunningContinuationKind::GraphFollowResume,
            None,
            None,
            None,
            Vec::new(),
        )
        .map(|publication| publication.persisted)
    }
}
