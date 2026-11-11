//! External connector, relay, supervisor and protocol output.
use super::*;

impl StateStore {
    pub(crate) fn cancel_uncontacted_external_allocation(&self, placement: &str) -> Result<()> {
        self.lock()?
            .runtime_db
            .cancel_external_allocation(placement)
    }

    pub(crate) fn bind_external_allocation(
        &self,
        placement: &str,
        occurrence: &runtime_db::external_execution::ExternalAllocationOccurrence,
        timing: runtime_db::external_execution::ExternalObservationTiming,
    ) -> Result<()> {
        self.lock()?
            .runtime_db
            .bind_external_allocation(placement, occurrence, timing)
    }

    /// Hold first activation's guest owner report for this daemon process
    /// only. It is diagnostic testimony for startup errors, never lifecycle
    /// authority, so it is deliberately not durable.
    pub(crate) fn note_external_guest_owner_report(
        &self,
        placement: &str,
        report: ryeos_external_execution_contract::guest_owner_report::GuestOwnerActivationReport,
    ) {
        if let Ok(mut reports) = self.external_guest_owner_reports.lock() {
            reports.insert(placement.to_owned(), report);
        }
    }

    pub(crate) fn external_guest_owner_report(
        &self,
        placement: &str,
    ) -> Option<ryeos_external_execution_contract::guest_owner_report::GuestOwnerActivationReport>
    {
        self.external_guest_owner_reports
            .lock()
            .ok()?
            .get(placement)
            .cloned()
    }

    pub(crate) fn observe_external_lifecycle_pending(
        &self,
        placement: &str,
        timing: runtime_db::external_execution::ExternalObservationTiming,
    ) -> Result<()> {
        self.lock()?
            .runtime_db
            .observe_external_lifecycle_pending(placement, timing)
    }

    pub(crate) fn register_external_execution_channel(
        &self,
        binding: &ryeos_state::external_execution::ExecutionChannelBinding,
    ) -> Result<()> {
        self.lock()?
            .runtime_db
            .register_external_execution_channel(binding)
    }

    pub(crate) fn admit_external_ready_and_author_release(
        &self,
        placement: &str,
        signing_key: &lillux::crypto::SigningKey,
        live_deadline: lillux::time::MonotonicDeadline,
    ) -> Result<Option<ryeos_state::external_execution::AuthenticatedExecutionFrame>> {
        self.lock()?
            .runtime_db
            .admit_external_ready_and_author_release(placement, signing_key, live_deadline)
    }

    pub(crate) fn admit_external_qualification_ready_and_author_release(
        &self,
        state: &crate::state::AppState,
        placement: &str,
        signing_key: &lillux::crypto::SigningKey,
        live_deadline: lillux::time::MonotonicDeadline,
        admission: crate::operator_external_content::product_qualification::AuthenticatedConsumerRoot,
    ) -> Result<Option<ryeos_state::external_execution::AuthenticatedExecutionFrame>> {
        let _permit = self.acquire_write_permit()?;
        admission.with_live_callback(|| {
            let g = self.lock()?;
            let proof = self.verify_external_qualification_owner_for_placement(
                state, &g, placement, &admission,
            )?;
            g.runtime_db
                .admit_external_qualification_ready_and_author_release(
                    placement,
                    signing_key,
                    live_deadline,
                    &proof,
                )
        })
    }

    pub(crate) fn claim_next_external_protocol_output(
        &self,
        placement: &str,
    ) -> Result<runtime_db::external_execution::ExternalProtocolOutputClaim> {
        self.lock()?
            .runtime_db
            .claim_next_external_protocol_output(placement)
    }

    /// Complete authenticated direct-command data, not a finalization permit.
    /// The ordinary terminal writer still owns current authority and cleanup.
    pub fn collect_external_direct_output(
        &self,
        placement: &str,
    ) -> Result<Option<runtime_db::external_execution::ExternalDirectOutput>> {
        self.lock()?
            .runtime_db
            .collect_external_direct_output(placement)
    }

    /// Complete applied target data with the peer's exact applied Release.
    /// Readiness data only; termination and finalization writers revalidate.
    pub fn external_direct_applied_output(
        &self,
        placement: &str,
    ) -> Result<Option<runtime_db::external_execution::ExternalDirectOutput>> {
        self.lock()?
            .runtime_db
            .external_direct_applied_output(placement)
    }

    /// Historical normal-settlement data; grants no fresh command contact.
    /// Explicit cancellation remains a separate operation and invalidates this
    /// eligibility even when the original target had already exited.
    pub fn external_direct_normal_settlement_output(
        &self,
        placement: &str,
    ) -> Result<Option<runtime_db::external_execution::ExternalDirectOutput>> {
        self.lock()?
            .runtime_db
            .external_direct_normal_settlement_output(placement)
    }

    pub(crate) fn external_connector_capability_generation(
        &self,
        placement: &str,
    ) -> Result<String> {
        self.lock()?
            .runtime_db
            .external_connector_capability_generation(placement)
    }

    pub(crate) fn prepare_external_connector(
        &self,
        placement: &str,
        capability_generation: &str,
        capability_hash: &str,
    ) -> Result<runtime_db::external_execution::connector::ExternalConnectorPreparation> {
        self.lock()?.runtime_db.prepare_external_connector(
            placement,
            capability_generation,
            capability_hash,
        )
    }

    pub(crate) fn connect_external_connector(
        &self,
        placement: &str,
        capability_generation: &str,
        capability_hash: &str,
        peer: &lillux::ExactProcessIdentity,
    ) -> Result<bool> {
        self.lock()?.runtime_db.connect_external_connector(
            placement,
            capability_generation,
            capability_hash,
            peer,
        )
    }

    pub(crate) fn connect_external_connector_contained(
        &self,
        placement: &str,
        capability_generation: &str,
        capability_hash: &str,
        peer: &crate::external_connector::ContainedQualificationPeer,
    ) -> Result<bool> {
        self.lock()?
            .runtime_db
            .connect_external_connector_contained(
                placement,
                capability_generation,
                capability_hash,
                peer,
            )
    }

    pub(crate) fn close_external_connector(
        &self,
        placement: &str,
        reason: &str,
    ) -> Result<runtime_db::external_execution::connector::ExternalConnectorRecord> {
        self.lock()?
            .runtime_db
            .close_external_connector(placement, reason)
    }

    /// Whether the one connector is authenticated and still open. A closed
    /// connector cannot serve the workload, whether or not it ever connected,
    /// so it is refused rather than reported as not yet.
    pub(crate) fn external_connector_connected(&self, placement: &str) -> Result<bool> {
        use runtime_db::external_execution::connector::ExternalConnectorPhase as Phase;
        let record = self
            .lock()?
            .runtime_db
            .external_connector(placement)?
            .context("external connector is absent")?;
        match record.phase {
            Phase::Prepared => Ok(false),
            Phase::Connected => Ok(true),
            Phase::Closed => anyhow::bail!(
                "external connector is closed: {}",
                record
                    .close_reason
                    .as_deref()
                    .unwrap_or("no recorded reason")
            ),
        }
    }

    /// Connector rows still `prepared` or `connected`. Relay owners are
    /// threads of the daemon that prepared them, so during start recovery
    /// every row listed here was left open by an ended daemon generation.
    pub(crate) fn open_external_connectors(
        &self,
    ) -> Result<Vec<runtime_db::external_execution::connector::ExternalConnectorRecord>> {
        self.lock()?.runtime_db.open_external_connectors()
    }

    pub(crate) fn external_connector_retired(&self, placement: &str) -> Result<bool> {
        Ok(self
            .lock()?
            .runtime_db
            .external_connector(placement)?
            .is_none_or(|record| {
                record.phase
                    == runtime_db::external_execution::connector::ExternalConnectorPhase::Closed
            }))
    }

    /// How the placement's one local connector stands: absent, still open, or
    /// closed with the relay's recorded reason. `completed` means the guest's
    /// protocol EOF was delivered to the live peer; every other reason is an
    /// abnormal end.
    pub fn external_connector_settlement(
        &self,
        placement: &str,
    ) -> Result<ExternalConnectorSettlement> {
        Ok(
            match self.lock()?.runtime_db.external_connector(placement)? {
                None => ExternalConnectorSettlement::Absent,
                Some(record)
                    if record.phase
                        == runtime_db::external_execution::connector::ExternalConnectorPhase::Closed =>
                {
                    ExternalConnectorSettlement::Closed {
                        reason: record.close_reason,
                    }
                }
                Some(_) => ExternalConnectorSettlement::Open,
            },
        )
    }

    pub(crate) fn finish_external_protocol_output(
        &self,
        placement: &str,
        sequence: u64,
        digest: &str,
    ) -> Result<()> {
        self.lock()?.runtime_db.finish_external_frame_application(
            placement,
            ryeos_state::external_execution::ChannelDirection::SupervisorToOwner,
            sequence,
            digest,
        )
    }

    pub fn external_execution_channel(
        &self,
        placement: &str,
    ) -> Result<ryeos_state::external_execution::ExecutionChannelBinding> {
        self.lock()?
            .runtime_db
            .external_execution_channel(placement)
    }

    pub(crate) fn optional_external_execution_channel(
        &self,
        placement: &str,
    ) -> Result<Option<ryeos_state::external_execution::ExecutionChannelBinding>> {
        self.lock()?
            .runtime_db
            .optional_external_execution_channel(placement)
    }

    pub(crate) fn exchange_external_supervisor_frame(
        &self,
        placement: &str,
        wire: &[u8],
        signing_key: &lillux::crypto::SigningKey,
        frame_limit: usize,
        byte_limit: usize,
    ) -> Result<runtime_db::external_execution::ExternalSupervisorExchange> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?.runtime_db.exchange_external_supervisor_frame(
            placement,
            wire,
            signing_key,
            frame_limit,
            byte_limit,
        )
    }

    pub(crate) fn author_external_owner_frame(
        &self,
        placement: &str,
        signing_key: &lillux::crypto::SigningKey,
        payload: ryeos_state::external_execution::ExecutionChannelPayload,
    ) -> Result<ryeos_state::external_execution::AuthenticatedExecutionFrame> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .author_external_owner_frame(placement, signing_key, payload)
    }

    pub(crate) fn author_external_owner_revocation(
        &self,
        placement: &str,
        signing_key: &lillux::crypto::SigningKey,
    ) -> Result<ryeos_state::external_execution::AuthenticatedExecutionFrame> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .author_external_owner_revocation(placement, signing_key)
    }

    /// The owner's one protocol input closure for a structured-session
    /// channel, authored once and returned on every later call.
    pub(crate) fn ensure_external_protocol_input_closed(
        &self,
        placement: &str,
        signing_key: &lillux::crypto::SigningKey,
    ) -> Result<ryeos_state::external_execution::AuthenticatedExecutionFrame> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .ensure_external_protocol_input_closed(placement, signing_key)
    }

    pub(crate) fn ensure_external_candidate_quiesce(
        &self,
        placement: &str,
        completion_request_digest: &str,
        signing_key: &lillux::crypto::SigningKey,
    ) -> Result<ryeos_state::external_execution::AuthenticatedExecutionFrame> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?.runtime_db.ensure_external_candidate_quiesce(
            placement,
            completion_request_digest,
            signing_key,
        )
    }

    /// Q-only stop request after exact local Worker and connector settlement.
    pub(crate) fn ensure_external_qualification_complete(
        &self,
        placement: &str,
        signing_key: &lillux::crypto::SigningKey,
    ) -> Result<ryeos_state::external_execution::AuthenticatedExecutionFrame> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .ensure_external_qualification_complete(placement, signing_key)
    }

    pub(crate) fn retained_external_candidate_import(
        &self,
        placement: &str,
    ) -> Result<Option<runtime_db::external_execution::RetainedExternalCandidateImport>> {
        self.lock()?
            .runtime_db
            .retained_external_candidate_import(placement)
    }

    pub(crate) fn settle_external_no_occurrence(
        &self,
        placement: &str,
        evidence: &runtime_db::external_execution::ExternalNoOccurrenceEvidence,
    ) -> Result<()> {
        self.lock()?
            .runtime_db
            .settle_external_no_occurrence(placement, evidence)
    }

    /// An operator's statement that one unknown allocation outcome left no
    /// occurrence. Returns the settled phase in its stored form.
    pub fn settle_unknown_external_allocation(
        &self,
        placement: &str,
        principal: &str,
        reason: &str,
        evidence: &str,
    ) -> Result<String> {
        let phase = self.lock()?.runtime_db.settle_unknown_external_allocation(
            placement,
            runtime_db::external_execution::ExternalOperatorStatement {
                principal: principal.to_owned(),
                reason: reason.to_owned(),
                evidence: evidence.to_owned(),
            },
        )?;
        Ok(format!("{phase:?}"))
    }

    pub(crate) fn begin_external_supervisor_activation(
        &self,
        placement: &str,
        intent: &runtime_db::external_execution::ExternalSupervisorActivationIntent,
    ) -> Result<bool> {
        self.lock()?
            .runtime_db
            .begin_external_supervisor_activation(placement, intent)
    }

    pub(crate) fn begin_external_relay_supervisor_activation(
        &self,
        placement: &str,
        intent: &runtime_db::external_execution::ExternalSupervisorActivationIntent,
        relay_bootstrap: &runtime_db::external_execution::ExternalRelayBootstrapCommitment,
    ) -> Result<bool> {
        self.lock()?
            .runtime_db
            .begin_external_relay_supervisor_activation(placement, intent, relay_bootstrap)
    }

    /// Fresh Q supervisor activation requires the original accepted-root lane.
    /// Retained activation reconciliation uses the generic journal entry point.
    pub(crate) fn begin_external_qualification_supervisor_activation(
        &self,
        state: &crate::state::AppState,
        placement: &str,
        intent: &runtime_db::external_execution::ExternalSupervisorActivationIntent,
        relay_bootstrap: Option<&runtime_db::external_execution::ExternalRelayBootstrapCommitment>,
        admission: crate::operator_external_content::product_qualification::AuthenticatedConsumerRoot,
    ) -> Result<bool> {
        let _permit = self.acquire_write_permit()?;
        admission.with_live_callback(|| {
            let g = self.lock()?;
            let record = g
                .runtime_db
                .external_allocation(placement)?
                .context("Q supervisor activation lost its allocation")?;
            let binding = g
                .runtime_db
                .retained_external_binding(&record.reservation.binding_hash)?
                .context("Q supervisor activation lost its retained signed binding")?;
            let proof = self.verify_external_qualification_owner(
                state,
                &g,
                &record.reservation,
                &binding,
                &admission,
            )?;
            g.runtime_db
                .begin_external_qualification_supervisor_activation_with_verified_owner(
                    placement,
                    intent,
                    relay_bootstrap,
                    &proof,
                )
        })
    }

    pub(crate) fn reserve_external_relay_transfer(
        &self,
        bootstrap: &ryeos_state::external_execution::transport::ExternalSupervisorBootstrap,
        intent: &runtime_db::external_execution::RelayTransferIntent,
        outbound: Option<&[u8]>,
    ) -> Result<runtime_db::external_execution::RelayTransferRecord> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .reserve_external_relay_transfer(bootstrap, intent, outbound)
    }

    pub(crate) fn accepted_external_relay_request(
        &self,
        intent: &runtime_db::external_execution::RelayTransferIntent,
    ) -> Result<runtime_db::external_execution::RelayTransferRecord> {
        self.lock()?
            .runtime_db
            .accepted_external_relay_request(intent)
    }

    pub(crate) fn begin_external_relay_application(
        &self,
        bootstrap: &ryeos_state::external_execution::transport::ExternalSupervisorBootstrap,
        kind: runtime_db::external_execution::RelayKind,
        sequence: u32,
    ) -> Result<runtime_db::external_execution::RelayApplicationDecision> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .begin_external_relay_application(bootstrap, kind, sequence)
    }

    pub(crate) fn commit_external_relay_application(
        &self,
        bootstrap: &ryeos_state::external_execution::transport::ExternalSupervisorBootstrap,
        kind: runtime_db::external_execution::RelayKind,
        sequence: u32,
        response: &ryeos_state::external_execution::transport::ExternalRelayResponseEnvelope,
    ) -> Result<Vec<u8>> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .commit_external_relay_application(bootstrap, kind, sequence, response)
    }

    pub(crate) fn retained_external_relay_outbound(
        &self,
        bootstrap: &ryeos_state::external_execution::transport::ExternalSupervisorBootstrap,
        kind: runtime_db::external_execution::RelayKind,
        sequence: u32,
    ) -> Result<Vec<u8>> {
        self.lock()?
            .runtime_db
            .retained_external_relay_outbound(bootstrap, kind, sequence)
    }

    pub(crate) fn claim_external_relay_read_contact(
        &self,
        intent: &runtime_db::external_execution::RelayTransferIntent,
    ) -> Result<runtime_db::external_execution::ClaimedRelayFileContact> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .claim_external_relay_read_contact(intent)
    }

    pub(crate) fn claim_external_relay_upload_contact(
        &self,
        intent: &runtime_db::external_execution::RelayTransferIntent,
    ) -> Result<runtime_db::external_execution::ClaimedRelayFileContact> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .claim_external_relay_upload_contact(intent)
    }

    pub(crate) fn accept_external_relay_readback(
        &self,
        intent: &runtime_db::external_execution::RelayTransferIntent,
        bytes: &[u8],
    ) -> Result<runtime_db::external_execution::RelayTransferRecord> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .accept_external_relay_readback(intent, bytes)
    }

    /// Author only the original bound occurrence's guest assignment with the
    /// node identity, naming the node's public key inside the signed document.
    /// Signing here does not make a provider snapshot qualified or start it.
    pub(crate) fn author_external_guest_assignment(
        &self,
        reservation: &runtime_db::external_execution::ExternalAllocationReservation,
        occurrence: &runtime_db::external_execution::ExternalAllocationOccurrence,
        intent: &runtime_db::external_execution::ExternalSupervisorActivationIntent,
        contract: &crate::node_config::sections::external_execution::ExternalPlacementBackendContract,
    ) -> Result<ryeos_external_execution_contract::guest_import_authorization::SignedGuestOccurrenceAssignment>{
        use ryeos_external_execution_contract::guest_import_authorization::{
            GUEST_OCCURRENCE_ASSIGNMENT_SCHEMA, GuestOccurrenceAssignmentDocument,
            SignedGuestOccurrenceAssignment,
        };
        let g = self.lock()?;
        let retained = g
            .runtime_db
            .external_allocation(&reservation.placement_thread_id)?
            .context("guest assignment lost its retained allocation")?;
        anyhow::ensure!(
            retained.phase == runtime_db::external_execution::ExternalAllocationPhase::Bound
                && retained.reservation == *reservation
                && retained.occurrence.as_ref() == Some(occurrence)
                && g.runtime_db
                    .external_supervisor_activation(&reservation.placement_thread_id)?
                    .is_none(),
            "guest assignment requires the exact original bound occurrence before activation claim"
        );
        let binding = g
            .runtime_db
            .retained_external_binding(&reservation.binding_hash)?
            .context("guest assignment lost its signed placement binding")?;
        anyhow::ensure!(
            binding.backend_contract() == *contract,
            "guest assignment changed the retained signed placement contract"
        );
        intent.validate_contract(reservation, occurrence, contract)?;
        ryeos_state::external_execution::validate_channel_public_key(
            &reservation.channel_owner_public_key,
        )?;
        let owner_bytes: [u8; 32] = base64::engine::general_purpose::STANDARD
            .decode(&reservation.channel_owner_public_key)?
            .try_into()
            .map_err(|_| anyhow!("guest assignment owner key changed length"))?;
        let assignment = GuestOccurrenceAssignmentDocument {
            schema: GUEST_OCCURRENCE_ASSIGNMENT_SCHEMA,
            placement_thread_id: reservation.placement_thread_id.clone(),
            admitted_capsule_hash: reservation.admitted_capsule_hash.clone(),
            base_snapshot_hash: reservation.base_snapshot_hash.clone(),
            execution_binding_hash: reservation.binding_hash.clone(),
            allocation_request_digest: reservation.request_digest.clone(),
            occurrence_id: occurrence.occurrence_id.clone(),
            activation_request_digest: intent.activation_request_digest.clone(),
            supervisor_runtime_hash: intent.supervisor_runtime_hash.clone(),
            guest_runtime_manifest_hash: contract.guest_runtime_manifest_hash.clone(),
            owner_public_key_hex: hex::encode(owner_bytes),
            controller_public_key_hex: hex::encode(g.signer.verifying_key().to_bytes()),
            attachment_deadline_ms: intent.attachment_deadline_ms,
        };
        let signature = g.signer.sign(&assignment.signing_bytes()?);
        anyhow::ensure!(
            signature.len() == 64,
            "node signer returned non-Ed25519 assignment signature"
        );
        let signed = SignedGuestOccurrenceAssignment {
            assignment,
            signature_hex: hex::encode(signature),
        };
        signed.validate_shape()?;
        Ok(signed)
    }

    pub(crate) fn external_supervisor_activation(
        &self,
        placement: &str,
    ) -> Result<Option<runtime_db::external_execution::ExternalSupervisorActivationRecord>> {
        self.lock()?
            .runtime_db
            .external_supervisor_activation(placement)
    }

    pub(crate) fn settle_external_supervisor_activation(
        &self,
        placement: &str,
        observation: &runtime_db::external_execution::ExternalSupervisorActivationObservation,
        timing: runtime_db::external_execution::ExternalObservationTiming,
    ) -> Result<()> {
        self.lock()?
            .runtime_db
            .settle_external_supervisor_activation(placement, observation, timing)
    }

    pub(crate) fn begin_external_termination(
        &self,
        placement: &str,
        intent: &runtime_db::external_execution::ExternalTerminationIntent,
    ) -> Result<bool> {
        let guard = self.state_authority.acquire_shared_guard()?;
        self.verify_external_q_fixture_custody_admitted(&guard, placement)?;
        self.lock()?
            .runtime_db
            .begin_external_termination(placement, intent)
    }

    pub(crate) fn settle_external_terminal(
        &self,
        placement: &str,
        observation: &runtime_db::external_execution::ExternalTerminalObservation,
    ) -> Result<()> {
        let guard = self.state_authority.acquire_shared_guard()?;
        self.verify_external_q_fixture_custody_admitted(&guard, placement)?;
        self.lock()?
            .runtime_db
            .settle_external_terminal(placement, observation)
    }

    pub(crate) fn ensure_external_qualification_normal_terminal(
        &self,
        placement: &str,
    ) -> Result<()> {
        let guard = self.state_authority.acquire_shared_guard()?;
        self.verify_retained_external_q_fixture_custody_admitted(&guard, placement)?;
        self.lock()?
            .runtime_db
            .ensure_external_qualification_normal_terminal(placement)
    }

    pub(crate) fn has_external_qualification_normal_termination_intent(
        &self,
        placement: &str,
    ) -> Result<bool> {
        // This is also the pre-revocation cleanup probe. An absent normal
        // intent must not require a fixture that was never captured.
        if !self
            .lock()?
            .runtime_db
            .has_external_qualification_normal_termination_intent(placement)?
        {
            return Ok(false);
        }
        let guard = self.state_authority.acquire_shared_guard()?;
        self.verify_retained_external_q_fixture_custody_admitted(&guard, placement)?;
        self.lock()?
            .runtime_db
            .has_external_qualification_normal_termination_intent(placement)
    }
}
