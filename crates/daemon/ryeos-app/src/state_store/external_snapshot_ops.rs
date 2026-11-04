//! External snapshot bootstrap, termination and verifier operations.
use super::*;

impl StateStore {
    pub(crate) fn snapshot_bootstrap_operation(
        &self,
        operation_id: &str,
    ) -> Result<Option<runtime_db::runtime_snapshot_bootstrap::SnapshotBootstrapRecord>> {
        self.lock()?
            .runtime_db
            .snapshot_bootstrap_operation(operation_id)
    }

    pub(crate) fn reserve_snapshot_bootstrap(
        &self,
        intent: &ryeos_external_execution_contract::runtime_snapshot_bootstrap::RuntimeSnapshotBootstrapIntent,
        binding: &crate::node_config::sections::runtime_snapshot_production::RetainedRuntimeSnapshotProductionBinding,
    ) -> Result<runtime_db::runtime_snapshot_bootstrap::SnapshotBootstrapRecord> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .reserve_snapshot_bootstrap_with_binding(intent, binding)
    }

    /// Link a completed protected lifecycle escrow before the one-shot
    /// provider claim. This is not a publication path: its caller must have
    /// finished the durable CAS upload for this exact root first.
    pub(crate) fn attach_snapshot_bootstrap_artifacts(
        &self,
        operation_id: &str,
        published: &crate::external_artifacts::PublishedLifecycleArtifacts,
    ) -> Result<runtime_db::runtime_snapshot_bootstrap::SnapshotBootstrapRecord> {
        let root_hash = published.root_hash();
        let _permit = self.acquire_write_permit()?;
        let authority = self.pinned_state_authority()?;
        let guard = authority.acquire_shared_guard()?;
        let cas = authority.cas_store()?;
        let source = crate::external_artifacts::verify_retained_lifecycle_closure(&cas, root_hash)?;
        authority.ensure_guard(&guard)?;
        let g = self.lock()?;
        let record = g
            .runtime_db
            .snapshot_bootstrap_operation(operation_id)?
            .context("bootstrap artifact link has no reserved operation")?;
        anyhow::ensure!(
            source.declaration_id == record.intent.provider_id
                && source.executables[0].payload_blob_hash == record.intent.adapter_artifact_hash
                && source.specs[0].blob_hash == record.intent.provider_spec_digest,
            "retained lifecycle closure differs from reserved bootstrap identity"
        );
        g.runtime_db
            .retained_snapshot_bootstrap_binding(operation_id)?
            .context("bootstrap artifact link lost retained signed producer binding")?;
        authority.ensure_guard(&guard)?;
        g.runtime_db
            .attach_snapshot_bootstrap_artifacts(operation_id, root_hash)
    }

    pub(crate) fn retained_snapshot_bootstrap_binding(
        &self,
        operation_id: &str,
    ) -> Result<Option<crate::node_config::sections::runtime_snapshot_production::RetainedRuntimeSnapshotProductionBinding>>{
        self.lock()?
            .runtime_db
            .retained_snapshot_bootstrap_binding(operation_id)
    }

    pub(crate) fn claim_snapshot_bootstrap_attempt(
        &self,
        operation_id: &str,
    ) -> Result<runtime_db::runtime_snapshot_bootstrap::SnapshotBootstrapAttemptClaim> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .claim_snapshot_bootstrap_attempt(operation_id)
    }

    pub(crate) fn bind_snapshot_bootstrap_occurrence(
        &self,
        occurrence: &ryeos_external_execution_contract::runtime_snapshot_bootstrap::RuntimeSnapshotBootstrapOccurrence,
    ) -> Result<runtime_db::runtime_snapshot_bootstrap::SnapshotBootstrapRecord> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .bind_snapshot_bootstrap_occurrence(occurrence)
    }

    pub(crate) fn bind_snapshot_bootstrap_readiness(
        &self,
        observation: &ryeos_external_execution_contract::runtime_snapshot_bootstrap::RuntimeSnapshotBootstrapReadinessObservation,
    ) -> Result<runtime_db::runtime_snapshot_bootstrap::SnapshotBootstrapRecord> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .bind_snapshot_bootstrap_readiness(observation)
    }

    pub(crate) fn quarantine_snapshot_bootstrap_attempt(
        &self,
        operation_id: &str,
    ) -> Result<runtime_db::runtime_snapshot_bootstrap::SnapshotBootstrapRecord> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .quarantine_snapshot_bootstrap_attempt(operation_id)
    }

    pub(crate) fn bootstrap_termination_operation(
        &self,
        operation_id: &str,
    ) -> Result<
        Option<runtime_db::runtime_snapshot_bootstrap_termination::BootstrapTerminationRecord>,
    > {
        self.lock()?
            .runtime_db
            .bootstrap_termination_operation(operation_id)
    }

    pub(crate) fn bootstrap_has_unsettled_snapshot(
        &self,
        bootstrap_operation_id: &str,
    ) -> Result<bool> {
        self.lock()?
            .runtime_db
            .bootstrap_has_unsettled_snapshot(bootstrap_operation_id)
    }

    pub(crate) fn recoverable_bootstrap_sources(&self, now_ms: i64) -> Result<Vec<String>> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .recoverable_bootstrap_sources(now_ms)
    }

    pub(crate) fn reserve_bootstrap_termination(
        &self,
        intent: &ryeos_external_execution_contract::runtime_snapshot_bootstrap::RuntimeSnapshotBootstrapTerminationIntent,
    ) -> Result<runtime_db::runtime_snapshot_bootstrap_termination::BootstrapTerminationRecord>
    {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .reserve_bootstrap_termination(intent)
    }

    pub(crate) fn claim_bootstrap_termination_attempt(
        &self,
        operation_id: &str,
    ) -> Result<runtime_db::runtime_snapshot_bootstrap_termination::BootstrapTerminationClaim> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .claim_bootstrap_termination_attempt(operation_id)
    }

    pub(crate) fn quarantine_bootstrap_termination_attempt(
        &self,
        operation_id: &str,
    ) -> Result<runtime_db::runtime_snapshot_bootstrap_termination::BootstrapTerminationRecord>
    {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .quarantine_bootstrap_termination_attempt(operation_id)
    }

    pub(crate) fn bind_bootstrap_terminal_observation(
        &self,
        observation: &ryeos_external_execution_contract::runtime_snapshot_bootstrap::RuntimeSnapshotBootstrapTerminalObservation,
    ) -> Result<runtime_db::runtime_snapshot_bootstrap_termination::BootstrapTerminationRecord>
    {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .bind_bootstrap_terminal_observation(observation)
    }

    /// The snapshot producer retains its one-attempt journal independently of
    /// any later execution placement. No state lock is held across contact.
    pub(crate) fn reserve_runtime_snapshot(
        &self,
        intent: &ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotIntent,
    ) -> Result<runtime_db::runtime_snapshot::RuntimeSnapshotRecord> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?.runtime_db.reserve_runtime_snapshot(intent)
    }

    pub(crate) fn claim_runtime_snapshot_attempt(
        &self,
        operation_id: &str,
        intent_digest: &str,
    ) -> Result<runtime_db::runtime_snapshot::RuntimeSnapshotAttemptClaim> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .claim_runtime_snapshot_attempt(operation_id, intent_digest)
    }

    pub(crate) fn runtime_snapshot_stage(
        &self,
        operation_id: &str,
    ) -> Result<Option<runtime_db::runtime_snapshot_stage::RuntimeSnapshotStageRecord>> {
        self.lock()?.runtime_db.runtime_snapshot_stage(operation_id)
    }

    pub(crate) fn reserve_runtime_snapshot_upload_stage(
        &self,
        intent: &ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotStageIntent,
    ) -> Result<runtime_db::runtime_snapshot_stage::RuntimeSnapshotStageRecord> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .reserve_runtime_snapshot_upload_stage(intent)
    }

    pub(crate) fn claim_runtime_snapshot_upload_stage(
        &self,
        operation_id: &str,
        intent_digest: &str,
    ) -> Result<runtime_db::runtime_snapshot_stage::RuntimeSnapshotStageClaim> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .claim_runtime_snapshot_upload_stage(operation_id, intent_digest)
    }

    pub(crate) fn bind_runtime_snapshot_upload_receipt(
        &self,
        receipt: &ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotUploadReceipt,
        deadline_exceeded: bool,
    ) -> Result<runtime_db::runtime_snapshot_stage::RuntimeSnapshotStageRecord> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .bind_runtime_snapshot_upload_receipt(receipt, deadline_exceeded)
    }

    pub(crate) fn quarantine_runtime_snapshot_upload_stage(
        &self,
        operation_id: &str,
        intent_digest: &str,
    ) -> Result<runtime_db::runtime_snapshot_stage::RuntimeSnapshotStageRecord> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .quarantine_runtime_snapshot_upload_stage(operation_id, intent_digest)
    }

    pub(crate) fn reserve_runtime_snapshot_create_stage(
        &self,
        intent: &ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotStageIntent,
    ) -> Result<runtime_db::runtime_snapshot_stage::RuntimeSnapshotStageRecord> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .reserve_runtime_snapshot_create_stage(intent)
    }

    pub(crate) fn claim_runtime_snapshot_create_stage(
        &self,
        operation_id: &str,
        intent_digest: &str,
    ) -> Result<runtime_db::runtime_snapshot_stage::RuntimeSnapshotStageClaim> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .claim_runtime_snapshot_create_stage(operation_id, intent_digest)
    }

    pub(crate) fn bind_runtime_snapshot_create_locator(
        &self,
        result: &ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotCreateResult,
        deadline_exceeded: bool,
    ) -> Result<runtime_db::runtime_snapshot_stage::RuntimeSnapshotStageRecord> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .bind_runtime_snapshot_create_locator(result, deadline_exceeded)
    }

    pub(crate) fn quarantine_runtime_snapshot_create_stage(
        &self,
        operation_id: &str,
        intent_digest: &str,
    ) -> Result<runtime_db::runtime_snapshot_stage::RuntimeSnapshotStageRecord> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .quarantine_runtime_snapshot_create_stage(operation_id, intent_digest)
    }

    pub(crate) fn bind_runtime_snapshot_locator(
        &self,
        locator: &ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotLocator,
        deadline_exceeded: bool,
    ) -> Result<runtime_db::runtime_snapshot::RuntimeSnapshotRecord> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .bind_runtime_snapshot_locator(locator, deadline_exceeded)
    }

    pub(crate) fn bind_runtime_snapshot_readiness(
        &self,
        observation: &ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotReadinessObservation,
    ) -> Result<runtime_db::runtime_snapshot::RuntimeSnapshotRecord> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .bind_runtime_snapshot_readiness(observation)
    }

    pub(crate) fn quarantine_runtime_snapshot_attempt(
        &self,
        operation_id: &str,
        intent_digest: &str,
    ) -> Result<runtime_db::runtime_snapshot::RuntimeSnapshotRecord> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .quarantine_runtime_snapshot_attempt(operation_id, intent_digest)
    }

    pub(crate) fn runtime_snapshot_operation(
        &self,
        operation_id: &str,
    ) -> Result<Option<runtime_db::runtime_snapshot::RuntimeSnapshotRecord>> {
        self.lock()?
            .runtime_db
            .runtime_snapshot_operation(operation_id)
    }

    pub(crate) fn snapshot_qualification_operation(
        &self,
        operation_id: &str,
    ) -> Result<Option<runtime_db::runtime_snapshot_qualification::SnapshotQualificationRecord>>
    {
        self.lock()?
            .runtime_db
            .snapshot_qualification_operation(operation_id)
    }

    pub(crate) fn qualification_termination_operation(
        &self,
        operation_id: &str,
    ) -> Result<
        Option<
            runtime_db::runtime_snapshot_qualification_termination::QualificationTerminationRecord,
        >,
    > {
        self.lock()?
            .runtime_db
            .qualification_termination_operation(operation_id)
    }

    pub(crate) fn reserve_qualification_termination(
        &self,
        intent: &ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotQualificationTerminationIntent,
    ) -> Result<
        runtime_db::runtime_snapshot_qualification_termination::QualificationTerminationRecord,
    > {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .reserve_qualification_termination(intent)
    }

    pub(crate) fn claim_qualification_termination_attempt(
        &self,
        operation_id: &str,
    ) -> Result<runtime_db::runtime_snapshot_qualification_termination::QualificationTerminationClaim>
    {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .claim_qualification_termination_attempt(operation_id)
    }

    pub(crate) fn bind_qualification_terminal_observation(
        &self,
        observation: &ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotQualificationTerminalObservation,
    ) -> Result<
        runtime_db::runtime_snapshot_qualification_termination::QualificationTerminationRecord,
    > {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .bind_qualification_terminal_observation(observation)
    }

    /// Consumer cleanup uses the existing journal, with revocation serialized
    /// through durable reservation. Independent operator recovery is unchanged.
    pub(crate) fn reserve_consumer_qualification_termination(
        &self,
        intent: &ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotQualificationTerminationIntent,
        admission: crate::operator_external_content::product_qualification::AuthenticatedConsumerRoot,
    ) -> Result<
        runtime_db::runtime_snapshot_qualification_termination::QualificationTerminationRecord,
    > {
        let _permit = self.acquire_write_permit()?;
        admission.with_live_callback(|| {
            let g = self.lock()?;
            let root = g
                .state_db
                .read_authoritative_thread_snapshot(
                    &admission.coordinate().accepted_root_id,
                    &admission.coordinate().accepted_root_id,
                )?
                .ok_or_else(|| anyhow!("consumer cleanup root disappeared"))?;
            if root.status != ryeos_state::objects::ThreadStatus::Running {
                bail!("consumer cleanup root is not running at reservation");
            }
            g.runtime_db
                .reserve_qualification_termination_with_admission(intent, Some(&admission))
        })
    }

    pub(crate) fn claim_consumer_qualification_termination(
        &self,
        operation_id: &str,
        admission: crate::operator_external_content::product_qualification::AuthenticatedConsumerRoot,
    ) -> Result<runtime_db::runtime_snapshot_qualification_termination::QualificationTerminationClaim>
    {
        let _permit = self.acquire_write_permit()?;
        admission.with_live_callback(|| {
            let g = self.lock()?;
            let root = g
                .state_db
                .read_authoritative_thread_snapshot(
                    &admission.coordinate().accepted_root_id,
                    &admission.coordinate().accepted_root_id,
                )?
                .ok_or_else(|| anyhow!("consumer cleanup root disappeared"))?;
            if root.status != ryeos_state::objects::ThreadStatus::Running {
                bail!("consumer cleanup root is not running at claim");
            }
            g.runtime_db
                .claim_qualification_termination_with_admission(operation_id, Some(&admission))
        })
    }

    pub(crate) fn quarantine_qualification_termination_attempt(
        &self,
        operation_id: &str,
    ) -> Result<
        runtime_db::runtime_snapshot_qualification_termination::QualificationTerminationRecord,
    > {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .quarantine_qualification_termination_attempt(operation_id)
    }

    pub(crate) fn reserve_snapshot_qualification(
        &self,
        intent: &ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotQualificationIntent,
    ) -> Result<runtime_db::runtime_snapshot_qualification::SnapshotQualificationRecord> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .reserve_snapshot_qualification(intent)
    }

    pub(crate) fn claim_snapshot_qualification_attempt(
        &self,
        operation_id: &str,
    ) -> Result<runtime_db::runtime_snapshot_qualification::SnapshotQualificationAttemptClaim> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .claim_snapshot_qualification_attempt(operation_id)
    }

    pub(crate) fn bind_snapshot_qualification_occurrence(
        &self,
        occurrence: &ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotQualificationOccurrence,
    ) -> Result<runtime_db::runtime_snapshot_qualification::SnapshotQualificationRecord> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .bind_snapshot_qualification_occurrence(occurrence)
    }

    pub(crate) fn quarantine_snapshot_qualification_attempt(
        &self,
        operation_id: &str,
    ) -> Result<runtime_db::runtime_snapshot_qualification::SnapshotQualificationRecord> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .quarantine_snapshot_qualification_attempt(operation_id)
    }

    pub(crate) fn observed_consumer_invocation_prerequisite(
        &self,
        owner_principal: &str,
        selection: &ryeos_external_execution_contract::restored_runtime_measurement::ConsumerVerificationInputSelection,
    ) -> Result<runtime_db::runtime_snapshot_qualification::SnapshotQualificationRecord> {
        self.lock()?
            .runtime_db
            .observed_consumer_invocation_prerequisite(owner_principal, selection)
    }

    pub(crate) fn restored_verifier_attempt(
        &self,
        operation_id: &str,
    ) -> Result<Option<runtime_db::restored_verifier_attempt::RestoredVerifierAttemptRecord>> {
        self.lock()?
            .runtime_db
            .restored_verifier_attempt(operation_id)
    }

    pub(crate) fn reserve_restored_verifier_attempt(
        &self,
        intent: &ryeos_external_execution_contract::restored_runtime_measurement::RestoredVerifierAttemptIntent,
    ) -> Result<runtime_db::restored_verifier_attempt::RestoredVerifierAttemptRecord> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .reserve_restored_verifier_attempt(intent)
    }

    pub(crate) fn claim_restored_verifier_attempt(
        &self,
        operation_id: &str,
    ) -> Result<runtime_db::restored_verifier_attempt::RestoredVerifierAttemptClaim> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .claim_restored_verifier_attempt(operation_id)
    }

    /// The caller must hold the qualification occurrence contact gate and
    /// authenticate a fresh accepted root under the current protected profile.
    /// This reservation does not itself authorize provider contact.
    pub(crate) fn reserve_consumer_verifier_attempt(
        &self,
        intent: &ryeos_external_execution_contract::restored_runtime_measurement::RestoredVerifierAttemptIntent,
        input: crate::qualification_consumer_input::ConsumerInputRef<'_>,
        admission: crate::operator_external_content::product_qualification::AuthenticatedConsumerRoot,
    ) -> Result<runtime_db::restored_verifier_attempt::RestoredVerifierAttemptRecord> {
        input.validate_attempt(intent)?;
        anyhow::ensure!(
            input.purpose() == admission.purpose()
                && input.coordinate() == admission.coordinate()
                && input.selection() == admission.selection(),
            "consumer input differs from authenticated accepted root"
        );
        let bytes = input.canonical_bytes()?;
        let permit = self.acquire_write_permit()?;
        let authority = self.pinned_state_authority()?;
        authority.ensure_guard(permit.cas_guard())?;
        let cas = authority.cas_store()?;
        let mut stage = authority
            .require_recovery()?
            .begin_staged_cas_roots_admitted(permit.cas_guard(), "consumer-verifier-input")?;
        let hash = stage.store_blob_admitted(permit.cas_guard(), &cas, &bytes)?;
        let record = admission.with_live_callback(|| {
            let g = self.lock()?;
            if admission.callback_launch_owner().is_some() {
                let root = g
                    .state_db
                    .read_authoritative_thread_snapshot(
                        &admission.coordinate().accepted_root_id,
                        &admission.coordinate().accepted_root_id,
                    )?
                    .ok_or_else(|| anyhow!("consumer callback root disappeared"))?;
                if root.status != ryeos_state::objects::ThreadStatus::Running {
                    bail!("consumer callback root is not running at reservation");
                }
            }
            g.runtime_db
                .reserve_consumer_verifier_attempt(intent, &hash, &admission)
        })?;
        if let Err(error) = stage.finish_admitted(permit.cas_guard()) {
            tracing::warn!(%error, operation_id = %intent.operation_id,
                "consumer input retained while temporary roots remain recoverable");
        }
        Ok(record)
    }

    /// Consumes a separate root authentication; ambiguous claims reconcile
    /// the retained attempt instead of opening another contact.
    pub(crate) fn claim_consumer_verifier_attempt(
        &self,
        operation_id: &str,
        admission: crate::operator_external_content::product_qualification::AuthenticatedConsumerRoot,
    ) -> Result<runtime_db::restored_verifier_attempt::RestoredVerifierAttemptClaim> {
        let _permit = self.acquire_write_permit()?;
        admission.with_live_callback(|| {
            let g = self.lock()?;
            if admission.callback_launch_owner().is_some() {
                let root = g
                    .state_db
                    .read_authoritative_thread_snapshot(
                        &admission.coordinate().accepted_root_id,
                        &admission.coordinate().accepted_root_id,
                    )?
                    .ok_or_else(|| anyhow!("consumer callback root disappeared"))?;
                if root.status != ryeos_state::objects::ThreadStatus::Running {
                    bail!("consumer callback root is not running at claim");
                }
            }
            g.runtime_db
                .claim_consumer_verifier_attempt(operation_id, &admission)
        })
    }

    pub(crate) fn bind_restored_verifier_observation(
        &self,
        observation: &ryeos_external_execution_contract::restored_runtime_measurement::RestoredVerifierAdapterObservation,
    ) -> Result<runtime_db::restored_verifier_attempt::RestoredVerifierAttemptRecord> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .bind_restored_verifier_observation(observation)
    }

    /// Evidence publication and its journal root share the current mutation
    /// permit. Recovery staging protects the Blob across a crash before bind;
    /// the immutable attempt owns it after bind, before staging is released.
    pub(crate) fn bind_consumer_verifier_observation(
        &self,
        observation: &ryeos_external_execution_contract::restored_runtime_measurement::ConsumerVerifierAdapterObservation,
        evidence: &[u8],
    ) -> Result<runtime_db::restored_verifier_attempt::RestoredVerifierAttemptRecord> {
        observation.verify_evidence_bytes(evidence)?;
        let permit = self.acquire_write_permit()?;
        let authority = self.pinned_state_authority()?;
        authority.ensure_guard(permit.cas_guard())?;
        let cas = authority.cas_store()?;
        let mut stage = authority
            .require_recovery()?
            .begin_staged_cas_roots_admitted(permit.cas_guard(), "consumer-verifier-evidence")?;
        let hash = stage.store_blob_admitted(permit.cas_guard(), &cas, evidence)?;
        if hash != observation.evidence_sha256 {
            bail!("consumer evidence publication changed its observed digest");
        }
        let record = self
            .lock()?
            .runtime_db
            .bind_consumer_verifier_observation(observation, evidence)?;
        if let Err(error) = stage.finish_admitted(permit.cas_guard()) {
            tracing::warn!(%error, operation_id = %observation.operation_id,
                "consumer observation retained while temporary evidence roots remain recoverable");
        }
        Ok(record)
    }

    pub(crate) fn quarantine_restored_verifier_attempt(
        &self,
        operation_id: &str,
    ) -> Result<runtime_db::restored_verifier_attempt::RestoredVerifierAttemptRecord> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .quarantine_restored_verifier_attempt(operation_id)
    }
}
