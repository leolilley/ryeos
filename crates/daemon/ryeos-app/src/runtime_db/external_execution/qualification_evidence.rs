//! Historical, no-permit Q transport evidence for a later semantic verifier.
//!
//! This joins immutable local and provider rows in one read transaction. It
//! does not interpret Worker output, authenticate CAS bytes, re-admit current
//! signed Bundle source, or issue a consumer qualification claim.

use super::*;
use ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeVerificationCoordinate;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QualificationHistoricalTransportEvidence {
    pub placement_thread_id: String,
    pub owner: ExternalQualificationCandidateOwner,
    pub channel_binding_digest: String,
    pub transcript_manifest_hash: String,
    pub request_digest: String,
    pub final_response_digest: String,
    pub turn_id: String,
    pub turn_start_digest: String,
    pub turn_completion_digest: String,
}

impl RuntimeDb {
    pub(crate) fn qualification_historical_transport_evidence(
        &self,
        coordinate: &ConsumerRuntimeVerificationCoordinate,
        qualification_operation_id: &str,
        owner_principal: &str,
    ) -> Result<QualificationHistoricalTransportEvidence> {
        let placement = ryeos_state::external_execution::qualification_session::qualification_candidate_placement_key(
            coordinate,
            qualification_operation_id,
        )?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        let allocation =
            read(&tx, &placement)?.context("Q historical transport has no retained allocation")?;
        let ExternalAllocationOwner::QualificationCandidate(owner) = &allocation.reservation.owner
        else {
            bail!("Q historical transport changed its qualification owner");
        };
        ensure!(
            allocation.phase == ExternalAllocationPhase::Terminated
                && allocation.reservation.placement_thread_id == placement
                && allocation.reservation.admitted_capsule_hash == coordinate.accepted_capsule_hash
                && owner.coordinate == *coordinate
                && owner.qualification_operation_id == qualification_operation_id
                && owner.operator_principal == owner_principal,
            "Q historical transport changed its exact accepted owner"
        );
        let qualification =
            super::super::runtime_snapshot_qualification::read(&tx, qualification_operation_id)?
                .context("Q historical transport lost its restored qualification")?;
        ensure!(
            qualification.phase
                == super::super::runtime_snapshot_qualification::SnapshotQualificationPhase::OccurrenceBound
                && qualification.intent.owner_principal == owner.operator_principal
                && qualification.occurrence.as_ref() == Some(&owner.qualification_occurrence),
            "Q historical transport changed its restored occurrence"
        );
        super::super::restored_verifier_attempt::read_retained_consumer_prerequisite(
            &tx,
            qualification_operation_id,
            owner_principal,
            coordinate,
        )?;
        // This reuses the normal terminal reader, including the signed native
        // whole-scope settlement, original local cleanup, immutable transcript,
        // and exact independent provider terminal observation.
        qualification_guest_scratch::require_completed_transport_tx(&tx, &allocation.reservation)?;
        let transcript = qualification_transcript::read_record(&tx, &placement)?
            .context("Q historical transport lost its source-validated transcript")?;
        let command = qualification_command::read_command(&tx, &placement)?
            .context("Q historical transport lost its one-use command")?;
        ensure!(
            transcript.owner_principal == owner.operator_principal
                && command.accepted_root_id == coordinate.accepted_root_id
                && command.accepted_capsule_hash == coordinate.accepted_capsule_hash
                && command.qualification_operation_id == qualification_operation_id
                && command.qualification_occurrence_id
                    == owner.qualification_occurrence.occurrence_id,
            "Q historical transport changed its transcript owner or occurrence"
        );
        let evidence = QualificationHistoricalTransportEvidence {
            placement_thread_id: placement,
            owner: owner.clone(),
            channel_binding_digest: command.channel_binding_digest,
            transcript_manifest_hash: transcript.manifest_hash,
            request_digest: command.request_digest,
            final_response_digest: command
                .final_response_digest
                .context("Q historical transport has no retained Final digest")?,
            turn_id: command
                .turn_id
                .context("Q historical transport has no turn identity")?,
            turn_start_digest: command
                .turn_start_digest
                .context("Q historical transport has no turn start digest")?,
            turn_completion_digest: command
                .turn_completion_digest
                .context("Q historical transport has no turn completion digest")?,
        };
        tx.commit()?;
        Ok(evidence)
    }
}
