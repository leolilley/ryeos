//! Worker handoff jobs, receipts and sync payload staging.
use super::*;

pub(super) fn active_source_worker_handoff_for_placement_in_db(
    state_db: &StateDb,
    placement_thread_id: &str,
) -> Result<
    Option<(
        ryeos_state::SyncJobRecord,
        crate::worker_handoff::WorkerSessionHandoffJobOperation,
    )>,
> {
    let mut found = None;
    let mut after: Option<(String, String)> = None;
    loop {
        let jobs = state_db.list_active_sync_jobs_by_operation_type_after(
            crate::worker_handoff::WORKER_SESSION_HANDOFF_OPERATION,
            after
                .as_ref()
                .map(|(created_at, job_id)| (created_at.as_str(), job_id.as_str())),
            ACTIVE_WORKER_HANDOFF_SCAN_PAGE,
        )?;
        let Some(last) = jobs.last() else {
            return Ok(found);
        };
        let next = (last.created_at.clone(), last.job_id.clone());
        for job in jobs {
            let operation = crate::worker_handoff::WorkerSessionHandoffJobOperation::from_value(
                job.operation.clone(),
            )?;
            if operation.role != crate::worker_handoff::WorkerHandoffJobRole::Source
                || operation.source_placement_thread_id != placement_thread_id
            {
                continue;
            }
            if found.is_some() {
                bail!("multiple active source handoffs claim placement {placement_thread_id}");
            }
            found = Some((job, operation));
        }
        after = Some(next);
    }
}

pub(super) fn validate_target_preparation_job_transition(
    job: &ryeos_state::SyncJobRecord,
    transfer_manifest_hash: &str,
    requested_progress: &crate::worker_handoff::WorkerSessionHandoffProgress,
) -> Result<Option<crate::worker_handoff::WorkerSessionHandoffProgress>> {
    if job.operation_type != crate::worker_handoff::WORKER_SESSION_HANDOFF_OPERATION
        || !job.roots.iter().any(|hash| hash == transfer_manifest_hash)
    {
        bail!("target worker handoff job is not rooted in its transfer manifest");
    }
    if matches!(
        job.state,
        ryeos_state::SyncJobState::Completed
            | ryeos_state::SyncJobState::Failed
            | ryeos_state::SyncJobState::Cancelled
    ) || job.phase == crate::worker_handoff::WORKER_HANDOFF_TARGET_ABORT_CLAIM_PHASE
    {
        bail!("terminal or abort-claimed target handoff cannot republish preparation");
    }
    let operation =
        crate::worker_handoff::WorkerSessionHandoffJobOperation::from_value(job.operation.clone())?;
    if operation.role != crate::worker_handoff::WorkerHandoffJobRole::Target
        || operation.operation_id != requested_progress.operation_id
        || requested_progress.phase != crate::worker_handoff::WorkerHandoffPhase::TargetPrepared
    {
        bail!("target preparation progress belongs to another authority lane");
    }
    let current_progress = job
        .result
        .clone()
        .map(crate::worker_handoff::WorkerSessionHandoffProgress::from_value)
        .transpose()?;
    if current_progress.as_ref().is_some_and(|current| {
        !matches!(
            current.phase,
            crate::worker_handoff::WorkerHandoffPhase::Planned
                | crate::worker_handoff::WorkerHandoffPhase::TargetPrepared
        )
    }) {
        bail!("target handoff already advanced beyond preparation");
    }
    Ok(current_progress)
}

impl StateStore {
    /// Win the target's pre-cut prepare/abort branch before any source
    /// contact. The exact abort head is retained in the existing sync-job head
    /// coordinate, so a crash cannot reopen prepare/adopt or let a retry name
    /// a different source successor.
    pub fn claim_worker_handoff_target_abort(
        &self,
        requested_job: &ryeos_state::NewSyncJob,
        abort_chain_head_hash: &str,
    ) -> Result<ryeos_state::SyncJobRecord> {
        ryeos_state::objects::thread_snapshot::validate_canonical_hash(
            "target abort claim head",
            abort_chain_head_hash,
        )?;
        let requested_operation =
            crate::worker_handoff::WorkerSessionHandoffJobOperation::from_value(
                requested_job.operation.clone(),
            )?;
        if requested_job.operation_type != crate::worker_handoff::WORKER_SESSION_HANDOFF_OPERATION
            || requested_operation.role != crate::worker_handoff::WorkerHandoffJobRole::Target
            || !requested_job.roots.is_empty()
        {
            bail!("target abort claim is not an exact target handoff job");
        }
        let g = self.lock()?;
        if let Some(existing) = g.state_db.get_sync_job(&requested_job.job_id)? {
            if existing.operation_type != requested_job.operation_type
                || existing.operation != requested_job.operation
                || existing.peer != requested_job.peer
                || existing.max_attempts != requested_job.max_attempts
            {
                bail!("target abort claim conflicts with another durable operation");
            }
            if existing.state == ryeos_state::SyncJobState::Cancelled {
                if existing.heads != [abort_chain_head_hash] {
                    bail!("target abort retry changed its terminal source head");
                }
                return Ok(existing);
            }
            if matches!(
                existing.state,
                ryeos_state::SyncJobState::Completed | ryeos_state::SyncJobState::Failed
            ) {
                bail!("terminal target handoff cannot enter the abort branch");
            }
            let progress = existing
                .result
                .clone()
                .map(crate::worker_handoff::WorkerSessionHandoffProgress::from_value)
                .transpose()?;
            if existing.phase == crate::worker_handoff::WORKER_HANDOFF_TARGET_ABORT_CLAIM_PHASE
                || progress.as_ref().is_some_and(|progress| {
                    progress.phase == crate::worker_handoff::WorkerHandoffPhase::AbortAuthorized
                })
            {
                if existing.heads != [abort_chain_head_hash] {
                    bail!("target abort retry changed its durably claimed source head");
                }
                return Ok(existing);
            }
            if progress.as_ref().is_some_and(|progress| {
                !matches!(
                    progress.phase,
                    crate::worker_handoff::WorkerHandoffPhase::Planned
                        | crate::worker_handoff::WorkerHandoffPhase::TargetPrepared
                )
            }) {
                bail!("target handoff already crossed the abortable pre-cut branch");
            }
            g.state_db.update_sync_job(
                &requested_job.job_id,
                &ryeos_state::SyncJobUpdate {
                    state: ryeos_state::SyncJobState::Running,
                    phase: crate::worker_handoff::WORKER_HANDOFF_TARGET_ABORT_CLAIM_PHASE
                        .to_owned(),
                    roots: None,
                    heads: Some(vec![abort_chain_head_hash.to_owned()]),
                    uploaded_hashes: existing.uploaded_hashes,
                    fetched_hashes: existing.fetched_hashes,
                    last_error: None,
                    result: existing.result,
                },
            )?;
            return g
                .state_db
                .get_sync_job(&requested_job.job_id)?
                .ok_or_else(|| anyhow!("target abort claim disappeared after publication"));
        }
        let progress = crate::worker_handoff::WorkerSessionHandoffProgress::planned(
            requested_operation.operation_id,
        )?;
        let mut job = requested_job.clone();
        job.heads = vec![abort_chain_head_hash.to_owned()];
        g.state_db.create_sync_job_with_initial_progress(
            &job,
            ryeos_state::SyncJobState::Running,
            crate::worker_handoff::WORKER_HANDOFF_TARGET_ABORT_CLAIM_PHASE,
            Some(&progress.to_value()?),
        )
    }

    /// Resolve the exact target-node-owned terminal-branch object for one
    /// operation. Callers still have to parse the closed adoption/abort
    /// evidence; this helper only prevents service responses from naming a
    /// mutable job row or an unowned/missing CAS object.
    pub fn worker_handoff_target_branch_hash(&self, operation_id: &str) -> Result<Option<String>> {
        ryeos_state::objects::thread_snapshot::validate_canonical_hash(
            "worker handoff target-branch operation",
            operation_id,
        )?;
        let signer = self.lock()?.signer.clone();
        let Some(head) = self.with_state_db(|db| {
            db.read_generic_head_ref(
                crate::worker_handoff::WORKER_HANDOFF_TARGET_BRANCH_HEAD_NAMESPACE,
                operation_id,
            )
        })?
        else {
            return Ok(None);
        };
        if head.signer != signer.fingerprint() {
            bail!("worker handoff target branch is not owned by this node");
        }
        let authority = self.pinned_state_authority()?;
        let guard = authority.acquire_shared_guard()?;
        authority.ensure_guard(&guard)?;
        if authority
            .cas_store()?
            .get_object(&head.target_hash)?
            .is_none()
        {
            bail!("worker handoff target-branch object is absent");
        }
        Ok(Some(head.target_hash))
    }

    /// Decode the exact mutually exclusive target terminal branch. The
    /// permanent generic head, rather than an operational job row, selects
    /// adoption, source abort, proved pre-attachment completion, or failure.
    pub fn worker_handoff_target_terminal(
        &self,
        operation_id: &str,
    ) -> Result<Option<crate::worker_handoff::WorkerHandoffTargetTerminalEvidence>> {
        ryeos_state::objects::thread_snapshot::validate_canonical_hash(
            "worker handoff target-terminal operation",
            operation_id,
        )?;
        let signer = self.lock()?.signer.clone();
        let Some(head) = self.with_state_db(|db| {
            db.read_generic_head_ref(
                crate::worker_handoff::WORKER_HANDOFF_TARGET_BRANCH_HEAD_NAMESPACE,
                operation_id,
            )
        })?
        else {
            return Ok(None);
        };
        if head.signer != signer.fingerprint() {
            bail!("worker handoff target terminal is not owned by this node");
        }
        let authority = self.pinned_state_authority()?;
        let guard = authority.acquire_shared_guard()?;
        authority.ensure_guard(&guard)?;
        let value = authority
            .cas_store()?
            .get_object(&head.target_hash)?
            .ok_or_else(|| anyhow!("worker handoff target-terminal object is absent"))?;
        let attestation = ryeos_state::objects::Attestation::from_value(&value)?;
        let terminal =
            crate::worker_handoff::WorkerHandoffTargetTerminalEvidence::from_attestation(
                &attestation,
                &signer.verifying_key(),
            )?;
        let retained_operation_id = match &terminal {
            crate::worker_handoff::WorkerHandoffTargetTerminalEvidence::Adoption(evidence) => {
                &evidence.target_operation.operation_id
            }
            crate::worker_handoff::WorkerHandoffTargetTerminalEvidence::Abort(evidence) => {
                &evidence.target_operation.operation_id
            }
            crate::worker_handoff::WorkerHandoffTargetTerminalEvidence::Completion(evidence) => {
                &evidence.target_operation.operation_id
            }
            crate::worker_handoff::WorkerHandoffTargetTerminalEvidence::Failure(evidence) => {
                &evidence.target_operation.operation_id
            }
        };
        if retained_operation_id != operation_id {
            bail!("worker handoff target-terminal head names another operation");
        }
        Ok(Some(terminal))
    }

    /// Read the permanent target-node-signed abort fence for one operation.
    /// The generic head, not a terminal sync-job retention window, owns the
    /// refusal authority against a delayed or replayed target prepare.
    pub fn worker_handoff_abort_fence(
        &self,
        operation_id: &str,
    ) -> Result<Option<crate::worker_handoff::WorkerHandoffAbortFenceEvidence>> {
        match self.worker_handoff_target_terminal(operation_id)? {
            None => Ok(None),
            Some(crate::worker_handoff::WorkerHandoffTargetTerminalEvidence::Abort(evidence)) => {
                Ok(Some(evidence))
            }
            Some(_) => bail!("worker handoff abort conflicts with another target branch"),
        }
    }

    /// Read the target-node-signed terminal adoption receipt for one exact
    /// handoff operation. Unlike the operational sync-job, this authority is
    /// retained by a signed generic head and remains available for exact
    /// response replay after ordinary terminal-job collection.
    pub fn worker_handoff_adoption_receipt(
        &self,
        operation_id: &str,
    ) -> Result<Option<crate::worker_handoff::WorkerHandoffAdoptionReceiptEvidence>> {
        match self.worker_handoff_target_terminal(operation_id)? {
            None => Ok(None),
            Some(crate::worker_handoff::WorkerHandoffTargetTerminalEvidence::Adoption(
                evidence,
            )) => Ok(Some(evidence)),
            Some(_) => bail!("worker handoff adoption conflicts with another target branch"),
        }
    }

    pub fn worker_handoff_terminal_failure(
        &self,
        operation_id: &str,
    ) -> Result<Option<crate::worker_handoff::WorkerHandoffTerminalFailureEvidence>> {
        match self.worker_handoff_target_terminal(operation_id)? {
            None => Ok(None),
            Some(crate::worker_handoff::WorkerHandoffTargetTerminalEvidence::Failure(evidence)) => {
                Ok(Some(evidence))
            }
            Some(_) => bail!("worker handoff failure conflicts with another target branch"),
        }
    }

    pub fn worker_handoff_terminal_completion(
        &self,
        operation_id: &str,
    ) -> Result<Option<crate::worker_handoff::WorkerHandoffTerminalCompletionEvidence>> {
        match self.worker_handoff_target_terminal(operation_id)? {
            None => Ok(None),
            Some(crate::worker_handoff::WorkerHandoffTargetTerminalEvidence::Completion(
                evidence,
            )) => Ok(Some(evidence)),
            Some(_) => bail!("worker handoff completion conflicts with another target branch"),
        }
    }

    /// Publish the immutable target-side adoption receipt after the exact
    /// successor attachment has been observed. The receipt is deliberately
    /// separate from both the worker event chain and terminal sync-job: a
    /// worker may exit immediately after attachment, and operational jobs
    /// retain only bounded history.
    pub fn publish_worker_handoff_adoption_receipt(
        &self,
        job_id: &str,
        target_operation: &crate::worker_handoff::WorkerSessionHandoffJobOperation,
        request: &crate::worker_handoff::WorkerPlacementAdoptRequest,
        response: &crate::worker_handoff::WorkerPlacementAdoptResponse,
    ) -> Result<String> {
        let job = self
            .with_state_db(|db| db.get_sync_job(job_id))?
            .context("worker handoff adoption receipt lost its operational job")?;
        let progress = crate::worker_handoff::WorkerSessionHandoffProgress::from_value(
            job.result
                .clone()
                .context("worker handoff adoption receipt job has no progress")?,
        )?;
        crate::worker_handoff::validate_projected_target_adoption(
            &job,
            target_operation,
            &progress,
            request,
        )?;
        let evidence = crate::worker_handoff::WorkerHandoffAdoptionReceiptEvidence::new(
            target_operation.clone(),
            request.clone(),
            response.clone(),
        )?;
        if let Some(existing) =
            self.worker_handoff_adoption_receipt(&target_operation.operation_id)?
        {
            if existing != evidence {
                bail!("worker handoff adoption receipt contradicts its existing operation");
            }
            return self
                .with_state_db(|db| {
                    db.read_generic_head_ref(
                        crate::worker_handoff::WORKER_HANDOFF_TARGET_BRANCH_HEAD_NAMESPACE,
                        &target_operation.operation_id,
                    )
                })?
                .map(|head| head.target_hash)
                .ok_or_else(|| anyhow!("worker handoff adoption receipt disappeared"));
        }

        let permit = self.acquire_write_permit()?;
        let previous = self.with_state_db(|db| {
            db.read_generic_head_ref(
                crate::worker_handoff::WORKER_HANDOFF_TARGET_BRANCH_HEAD_NAMESPACE,
                &target_operation.operation_id,
            )
        })?;
        if previous.is_some() {
            drop(permit);
            let existing = self
                .worker_handoff_adoption_receipt(&target_operation.operation_id)?
                .ok_or_else(|| anyhow!("worker handoff adoption receipt disappeared"))?;
            if existing != evidence {
                bail!("worker handoff adoption receipt contradicts its existing operation");
            }
            return self
                .with_state_db(|db| {
                    db.read_generic_head_ref(
                        crate::worker_handoff::WORKER_HANDOFF_TARGET_BRANCH_HEAD_NAMESPACE,
                        &target_operation.operation_id,
                    )
                })?
                .map(|head| head.target_hash)
                .ok_or_else(|| anyhow!("worker handoff adoption receipt disappeared"));
        }

        let signer = self.lock()?.signer.clone();
        let attestation = evidence.sign_attestation(signer.as_ref())?;
        let authority = self.pinned_state_authority()?;
        authority.ensure_guard(permit.cas_guard())?;
        let cas = authority.cas_store()?;
        let mut stage = authority
            .require_recovery()?
            .begin_staged_cas_roots_admitted(
                permit.cas_guard(),
                "worker-handoff-adoption-receipt",
            )?;
        let attestation_hash =
            stage.store_object_admitted(permit.cas_guard(), &cas, &attestation.to_value())?;
        stage.protect_object_hash_admitted(permit.cas_guard(), &attestation_hash)?;
        self.with_state_db(|db| {
            db.advance_generic_head_ref(
                crate::worker_handoff::WORKER_HANDOFF_TARGET_BRANCH_HEAD_NAMESPACE,
                &target_operation.operation_id,
                &attestation_hash,
                None,
                signer.as_ref(),
                permit.cas_guard(),
            )
        })?;
        if let Err(error) = stage.finish_admitted(permit.cas_guard()) {
            tracing::warn!(
                %error,
                operation_id = %target_operation.operation_id,
                "worker handoff adoption receipt published while temporary roots remained recoverable"
            );
        }
        Ok(attestation_hash)
    }

    /// Publish immutable target-signed failure testimony only after the exact
    /// successor terminal and cleanup disposition have been proved by the
    /// target lifecycle owner. The shared target-branch namespace prevents a
    /// failure from coexisting with adoption or source abort.
    pub fn publish_worker_handoff_terminal_failure(
        &self,
        target_operation: &crate::worker_handoff::WorkerSessionHandoffJobOperation,
        request: &crate::worker_handoff::WorkerPlacementAdoptRequest,
        failure: &crate::worker_handoff::WorkerPlacementFailureResponse,
    ) -> Result<String> {
        let evidence = crate::worker_handoff::WorkerHandoffTerminalFailureEvidence::new(
            target_operation.clone(),
            request.clone(),
            failure.clone(),
        )?;
        if let Some(existing) =
            self.worker_handoff_target_terminal(&target_operation.operation_id)?
        {
            match existing {
                crate::worker_handoff::WorkerHandoffTargetTerminalEvidence::Failure(existing)
                    if existing == evidence =>
                {
                    return self
                        .worker_handoff_target_branch_hash(&target_operation.operation_id)?
                        .ok_or_else(|| anyhow!("worker handoff terminal failure disappeared"));
                }
                _ => bail!("worker handoff terminal failure conflicts with another target branch"),
            }
        }

        let permit = self.acquire_write_permit()?;
        let previous = self.with_state_db(|db| {
            db.read_generic_head_ref(
                crate::worker_handoff::WORKER_HANDOFF_TARGET_BRANCH_HEAD_NAMESPACE,
                &target_operation.operation_id,
            )
        })?;
        if previous.is_some() {
            drop(permit);
            let existing = self
                .worker_handoff_terminal_failure(&target_operation.operation_id)?
                .ok_or_else(|| anyhow!("worker handoff terminal failure disappeared"))?;
            if existing != evidence {
                bail!("worker handoff terminal failure changed its existing operation");
            }
            return self
                .worker_handoff_target_branch_hash(&target_operation.operation_id)?
                .ok_or_else(|| anyhow!("worker handoff terminal failure disappeared"));
        }

        let signer = self.lock()?.signer.clone();
        let attestation = evidence.sign_attestation(signer.as_ref())?;
        let authority = self.pinned_state_authority()?;
        authority.ensure_guard(permit.cas_guard())?;
        let cas = authority.cas_store()?;
        let mut stage = authority
            .require_recovery()?
            .begin_staged_cas_roots_admitted(
                permit.cas_guard(),
                "worker-handoff-terminal-failure",
            )?;
        let attestation_hash =
            stage.store_object_admitted(permit.cas_guard(), &cas, &attestation.to_value())?;
        stage.protect_object_hash_admitted(permit.cas_guard(), &attestation_hash)?;
        self.with_state_db(|db| {
            db.advance_generic_head_ref(
                crate::worker_handoff::WORKER_HANDOFF_TARGET_BRANCH_HEAD_NAMESPACE,
                &target_operation.operation_id,
                &attestation_hash,
                None,
                signer.as_ref(),
                permit.cas_guard(),
            )
        })?;
        if let Err(error) = stage.finish_admitted(permit.cas_guard()) {
            tracing::warn!(
                %error,
                operation_id = %target_operation.operation_id,
                "worker handoff terminal failure published while temporary roots remained recoverable"
            );
        }
        Ok(attestation_hash)
    }

    /// Publish immutable target-signed successful terminal testimony when the
    /// successor completes after writer transfer but before the attachment
    /// projection. The same one-operation branch head makes this mutually
    /// exclusive with attachment, source abort, and terminal failure.
    pub fn publish_worker_handoff_terminal_completion(
        &self,
        target_operation: &crate::worker_handoff::WorkerSessionHandoffJobOperation,
        request: &crate::worker_handoff::WorkerPlacementAdoptRequest,
        completion: &crate::worker_handoff::WorkerPlacementCompletionResponse,
    ) -> Result<String> {
        let evidence = crate::worker_handoff::WorkerHandoffTerminalCompletionEvidence::new(
            target_operation.clone(),
            request.clone(),
            completion.clone(),
        )?;
        if let Some(existing) =
            self.worker_handoff_target_terminal(&target_operation.operation_id)?
        {
            match existing {
                crate::worker_handoff::WorkerHandoffTargetTerminalEvidence::Completion(
                    existing,
                ) if existing == evidence => {
                    return self
                        .worker_handoff_target_branch_hash(&target_operation.operation_id)?
                        .ok_or_else(|| anyhow!("worker handoff terminal completion disappeared"));
                }
                _ => {
                    bail!("worker handoff terminal completion conflicts with another target branch")
                }
            }
        }

        let permit = self.acquire_write_permit()?;
        let previous = self.with_state_db(|db| {
            db.read_generic_head_ref(
                crate::worker_handoff::WORKER_HANDOFF_TARGET_BRANCH_HEAD_NAMESPACE,
                &target_operation.operation_id,
            )
        })?;
        if previous.is_some() {
            drop(permit);
            let existing = self
                .worker_handoff_terminal_completion(&target_operation.operation_id)?
                .ok_or_else(|| anyhow!("worker handoff terminal completion disappeared"))?;
            if existing != evidence {
                bail!("worker handoff terminal completion changed its existing operation");
            }
            return self
                .worker_handoff_target_branch_hash(&target_operation.operation_id)?
                .ok_or_else(|| anyhow!("worker handoff terminal completion disappeared"));
        }

        let signer = self.lock()?.signer.clone();
        let attestation = evidence.sign_attestation(signer.as_ref())?;
        let authority = self.pinned_state_authority()?;
        authority.ensure_guard(permit.cas_guard())?;
        let cas = authority.cas_store()?;
        let mut stage = authority
            .require_recovery()?
            .begin_staged_cas_roots_admitted(
                permit.cas_guard(),
                "worker-handoff-terminal-completion",
            )?;
        let attestation_hash =
            stage.store_object_admitted(permit.cas_guard(), &cas, &attestation.to_value())?;
        stage.protect_object_hash_admitted(permit.cas_guard(), &attestation_hash)?;
        self.with_state_db(|db| {
            db.advance_generic_head_ref(
                crate::worker_handoff::WORKER_HANDOFF_TARGET_BRANCH_HEAD_NAMESPACE,
                &target_operation.operation_id,
                &attestation_hash,
                None,
                signer.as_ref(),
                permit.cas_guard(),
            )
        })?;
        if let Err(error) = stage.finish_admitted(permit.cas_guard()) {
            tracing::warn!(
                %error,
                operation_id = %target_operation.operation_id,
                "worker handoff terminal completion published while temporary roots remained recoverable"
            );
        }
        Ok(attestation_hash)
    }

    /// Publish the permanent target-side abort fence through the existing CAS
    /// staging and node-signed generic-head authorities. Replays must reproduce
    /// the exact source operation and abort head.
    pub fn publish_worker_handoff_abort_fence(
        &self,
        target_operation: &crate::worker_handoff::WorkerSessionHandoffJobOperation,
        abort_chain_head_hash: &str,
    ) -> Result<String> {
        self.publish_worker_handoff_abort_fence_evidence(
            target_operation,
            abort_chain_head_hash,
            None,
        )
    }

    /// Advance an already-authorized abort fence to its terminal cleanup
    /// receipt. This signed generic head remains replay authority after the
    /// operational sync-job reaches its ordinary retention horizon.
    pub fn publish_worker_handoff_abort_terminal_receipt(
        &self,
        target_operation: &crate::worker_handoff::WorkerSessionHandoffJobOperation,
        abort_chain_head_hash: &str,
        disposition: &str,
    ) -> Result<String> {
        self.publish_worker_handoff_abort_fence_evidence(
            target_operation,
            abort_chain_head_hash,
            Some(disposition.to_owned()),
        )
    }

    pub(super) fn publish_worker_handoff_abort_fence_evidence(
        &self,
        target_operation: &crate::worker_handoff::WorkerSessionHandoffJobOperation,
        abort_chain_head_hash: &str,
        terminal_disposition: Option<String>,
    ) -> Result<String> {
        let evidence = crate::worker_handoff::WorkerHandoffAbortFenceEvidence::new(
            target_operation.clone(),
            abort_chain_head_hash.to_owned(),
            terminal_disposition,
        )?;
        let permit = self.acquire_write_permit()?;
        let previous = self.with_state_db(|db| {
            db.read_generic_head_ref(
                crate::worker_handoff::WORKER_HANDOFF_TARGET_BRANCH_HEAD_NAMESPACE,
                &target_operation.operation_id,
            )
        })?;
        let signer = self.lock()?.signer.clone();
        let attestation = evidence.sign_attestation(signer.as_ref())?;
        let authority = self.pinned_state_authority()?;
        authority.ensure_guard(permit.cas_guard())?;
        let cas = authority.cas_store()?;
        if let Some(previous) = &previous {
            if previous.signer != signer.fingerprint() {
                bail!("worker handoff target branch is not owned by this node");
            }
            let value = cas
                .get_object(&previous.target_hash)?
                .ok_or_else(|| anyhow!("worker handoff target-branch object is absent"))?;
            let attestation = ryeos_state::objects::Attestation::from_value(&value)?;
            let existing =
                crate::worker_handoff::WorkerHandoffAbortFenceEvidence::from_attestation(
                    &attestation,
                    &signer.verifying_key(),
                )
                .context("worker handoff abort cannot replace the competing target branch")?;
            if existing.target_operation != evidence.target_operation
                || existing.abort_chain_head_hash != evidence.abort_chain_head_hash
            {
                bail!("worker handoff abort fence contradicts its existing operation");
            }
            if evidence.terminal_disposition.is_none()
                || existing.terminal_disposition == evidence.terminal_disposition
            {
                return Ok(previous.target_hash.clone());
            }
            if existing.terminal_disposition.is_some() {
                bail!("worker handoff abort receipt changed its terminal disposition");
            }
        } else if evidence.terminal_disposition.is_some() {
            bail!("worker handoff abort terminal receipt has no provisional branch fence");
        }
        let mut stage = authority
            .require_recovery()?
            .begin_staged_cas_roots_admitted(permit.cas_guard(), "worker-handoff-abort-fence")?;
        let attestation_hash =
            stage.store_object_admitted(permit.cas_guard(), &cas, &attestation.to_value())?;
        stage.protect_object_hash_admitted(permit.cas_guard(), &attestation_hash)?;
        self.with_state_db(|db| {
            db.advance_generic_head_ref(
                crate::worker_handoff::WORKER_HANDOFF_TARGET_BRANCH_HEAD_NAMESPACE,
                &target_operation.operation_id,
                &attestation_hash,
                previous.as_ref().map(|head| head.target_hash.as_str()),
                signer.as_ref(),
                permit.cas_guard(),
            )
        })?;
        if let Err(error) = stage.finish_admitted(permit.cas_guard()) {
            tracing::warn!(
                %error,
                operation_id = %target_operation.operation_id,
                "worker handoff abort fence published while temporary roots remained recoverable"
            );
        }
        Ok(attestation_hash)
    }

    /// Complete a non-authoritative worker-placement preflight. The signed
    /// receipt is rooted by the existing sync job, but no chain head,
    /// credential reservation, or runnable placement is created here.
    pub fn complete_worker_handoff_preflight(
        &self,
        job_id: &str,
        attempt_id: &str,
        attestation: &ryeos_state::objects::Attestation,
        result: Value,
    ) -> Result<String> {
        attestation.validate()?;
        if attestation.policy != crate::worker_handoff::WORKER_PLACEMENT_PREFLIGHT_POLICY
            || attestation.claim != crate::worker_handoff::WORKER_PLACEMENT_PREFLIGHT_CLAIM
        {
            bail!("worker handoff preflight receipt has another policy");
        }
        let permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let job = g
            .state_db
            .get_sync_job(job_id)?
            .ok_or_else(|| anyhow!("worker handoff preflight job does not exist"))?;
        if job.operation_type != crate::worker_handoff::WORKER_SESSION_HANDOFF_PREFLIGHT_OPERATION {
            bail!("sync job is not a worker handoff preflight");
        }
        self.state_authority.ensure_guard(permit.cas_guard())?;
        let value = attestation.to_value();
        let expected = ryeos_state::objects::canonical_value_digest(&value)?;
        let stored = self.state_authority.cas_store()?.put_object(&value)?;
        if stored.hash != expected {
            bail!("stored worker handoff preflight receipt digest changed");
        }
        g.state_db
            .record_cas_entry(&ryeos_state::NewCasEntryAttribution {
                hash: expected.clone(),
                entry_kind: ryeos_state::CasEntryKind::Object,
                bytes: u64::try_from(lillux::canonical_json(&value)?.len())?,
                source_principal: Some(format!("fp:{}", g.signer.fingerprint())),
                source_peer: None,
                job_id: Some(job_id.to_owned()),
                state: ryeos_state::CasEntryState::Local,
            })?;
        let mut roots = job.roots;
        roots.push(expected.clone());
        roots.sort();
        roots.dedup();
        g.state_db.finish_sync_job_attempt_and_update_job(
            attempt_id,
            &ryeos_state::FinishSyncJobAttempt {
                state: ryeos_state::SyncJobAttemptState::Completed,
                phase: "preflight_complete".to_owned(),
                error: None,
                result: Some(result.clone()),
            },
            job_id,
            &ryeos_state::SyncJobUpdate {
                state: ryeos_state::SyncJobState::Completed,
                phase: "preflight_complete".to_owned(),
                roots: Some(roots),
                heads: None,
                uploaded_hashes: job.uploaded_hashes,
                fetched_hashes: job.fetched_hashes,
                last_error: None,
                result: Some(result),
            },
        )?;
        Ok(expected)
    }

    /// Settle process-local sync attempts before daemon recovery creates any
    /// replacement attempt. Jobs remain durable and retryable.
    pub fn reconcile_interrupted_sync_job_attempts(&self) -> Result<usize> {
        let g = self.lock()?;
        g.state_db.reconcile_interrupted_sync_job_attempts()
    }

    /// Import one digest-verified sync payload as staged CAS data and create
    /// the job that roots it before releasing the shared mutation guard. This
    /// is the generic crash-safe boundary used by remote workflows that must
    /// inspect a closure before deciding whether it may become authority.
    pub fn stage_sync_payload_and_create_job(
        &self,
        payload: &ryeos_state::sync::ExportPayload,
        attribution: &ryeos_state::sync::ImportAttribution,
        job: &ryeos_state::NewSyncJob,
    ) -> Result<(ryeos_state::sync::ImportResult, ryeos_state::SyncJobRecord)> {
        if !job
            .roots
            .iter()
            .all(|root| payload.entries.iter().any(|entry| &entry.hash == root))
        {
            bail!("sync job roots are not all present in its staged transfer payload");
        }
        let permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        if let Some(existing) = g.state_db.get_sync_job(&job.job_id)? {
            if existing.operation_type != job.operation_type
                || existing.operation != job.operation
                || existing.peer != job.peer
                || existing.max_attempts != job.max_attempts
                || !job
                    .roots
                    .iter()
                    .all(|root| existing.roots.iter().any(|existing| existing == root))
            {
                bail!("staged sync job identity is already bound to another operation");
            }
            // `roots` may grow as signed receipts are published and `heads`
            // may advance as the durable workflow progresses. The immutable
            // operation and initial roots above identify an idempotent re-drive;
            // comparing the mutable frontier would make every completed job
            // unreplayable.
            return Ok((ryeos_state::sync::ImportResult::default(), existing));
        }
        let import = ryeos_state::sync::import_objects_staged(
            &g.state_db,
            payload,
            attribution,
            permit.cas_guard(),
        )?;
        if import.hash_mismatches != 0 {
            bail!(
                "staged sync payload contained {} digest mismatch(es)",
                import.hash_mismatches
            );
        }
        let job = g.state_db.create_sync_job(job)?;
        Ok((import, job))
    }

    pub fn stage_sync_payload_for_existing_job(
        &self,
        payload: &ryeos_state::sync::ExportPayload,
        attribution: &ryeos_state::sync::ImportAttribution,
        job_id: &str,
        phase: &str,
        additional_roots: &[String],
        progress: Option<Value>,
    ) -> Result<ryeos_state::sync::ImportResult> {
        self.stage_verified_sync_payload_for_existing_job(
            payload,
            attribution,
            job_id,
            phase,
            additional_roots,
            progress,
            |_| Ok(()),
        )
    }

    /// Import a remote payload, prove its caller-owned semantic contract, and
    /// only then make any of its hashes durable job roots. The verifier runs
    /// while the exact CAS mutation authority and StateStore lock remain held,
    /// so rejected bytes cannot race GC or become retry/recovery authority.
    /// This boundary is deliberately schema-blind: the caller supplies the
    /// proof over the pinned CAS view.
    pub fn stage_verified_sync_payload_for_existing_job<F>(
        &self,
        payload: &ryeos_state::sync::ExportPayload,
        attribution: &ryeos_state::sync::ImportAttribution,
        job_id: &str,
        phase: &str,
        additional_roots: &[String],
        progress: Option<Value>,
        verify: F,
    ) -> Result<ryeos_state::sync::ImportResult>
    where
        F: FnOnce(&lillux::CasStore) -> Result<()>,
    {
        let permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let job = g
            .state_db
            .get_sync_job(job_id)?
            .ok_or_else(|| anyhow!("sync job does not exist"))?;
        if !additional_roots.iter().all(|root| {
            job.roots.iter().any(|existing| existing == root)
                || payload.entries.iter().any(|entry| &entry.hash == root)
        }) {
            bail!(
                "existing sync job additional roots are absent from its durable roots and payload"
            );
        }
        let import = ryeos_state::sync::import_objects_staged(
            &g.state_db,
            payload,
            attribution,
            permit.cas_guard(),
        )?;
        if import.hash_mismatches != 0 {
            bail!("existing sync job import contained digest mismatches");
        }
        let authority = g.state_db.pinned_authority()?;
        authority.ensure_guard(permit.cas_guard())?;
        let cas = authority.cas_store()?;
        verify(&cas)?;
        let mut roots = job.roots;
        roots.extend(additional_roots.iter().cloned());
        roots.sort();
        roots.dedup();
        let mut fetched_hashes = job.fetched_hashes;
        fetched_hashes.extend(
            additional_roots
                .iter()
                .filter(|root| payload.entries.iter().any(|entry| &entry.hash == *root))
                .cloned(),
        );
        fetched_hashes.sort();
        fetched_hashes.dedup();
        g.state_db.update_sync_job(
            job_id,
            &ryeos_state::SyncJobUpdate {
                state: ryeos_state::SyncJobState::Running,
                phase: phase.to_owned(),
                roots: Some(roots),
                heads: None,
                uploaded_hashes: job.uploaded_hashes,
                fetched_hashes,
                last_error: None,
                result: progress,
            },
        )?;
        Ok(import)
    }

    /// Publish a prebuilt node-signed admission object through the generic
    /// admission index while holding the StateStore mutation hierarchy. The
    /// state layer remains blind to the evidence schema.
    pub fn publish_admission_attestation(
        &self,
        attestation: &ryeos_state::objects::Attestation,
        limits: ryeos_state::object_closure::ObjectClosureLimits,
    ) -> Result<ryeos_state::admission::AdmissionResult> {
        let permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        ryeos_state::admission::publish_admission_attestation(
            &g.state_db,
            attestation,
            limits,
            g.signer.as_ref(),
            permit.cas_guard(),
        )
    }
}
