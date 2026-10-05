//! Original sealed file-relay bootstrap commitment. This row contains no
//! capability plaintext or guest descriptor authority. It binds the protected
//! artifact to the one activation journal transaction before provider contact.

use super::*;

pub(in crate::runtime_db) const JOURNAL_SQL: &str = r#"
CREATE TABLE external_execution_relay_bootstrap (
    placement_thread_id TEXT PRIMARY KEY REFERENCES external_execution_allocation(placement_thread_id),
    commitment_json TEXT NOT NULL,
    CHECK (length(CAST(commitment_json AS BLOB)) BETWEEN 1 AND 4096)
);
CREATE TRIGGER external_execution_relay_bootstrap_insert_guard
BEFORE INSERT ON external_execution_relay_bootstrap
WHEN NOT EXISTS(SELECT 1 FROM external_execution_allocation a
    JOIN external_execution_supervisor_activation_intent i
      ON i.placement_thread_id=a.placement_thread_id
    WHERE a.placement_thread_id=NEW.placement_thread_id
      AND a.phase='bound' AND a.occurrence_json IS NOT NULL)
BEGIN SELECT RAISE(ABORT, 'relay bootstrap has no original activation'); END;
CREATE TRIGGER external_execution_relay_bootstrap_immutable
BEFORE UPDATE ON external_execution_relay_bootstrap
BEGIN SELECT RAISE(ABORT, 'relay bootstrap commitment is immutable'); END;
CREATE TRIGGER external_execution_relay_bootstrap_no_delete
BEFORE DELETE ON external_execution_relay_bootstrap
BEGIN SELECT RAISE(ABORT, 'relay bootstrap commitment is retained'); END;
"#;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExternalRelayBootstrapCommitment {
    pub schema: u32,
    pub placement_thread_id: String,
    pub allocation_request_digest: String,
    pub occurrence_id: String,
    pub activation_request_digest: String,
    pub guest_input_identity: String,
    pub bootstrap_sha256: String,
    pub signed_import_sha256: String,
}

impl ExternalRelayBootstrapCommitment {
    pub(crate) fn validate_for(
        &self,
        reservation: &ExternalAllocationReservation,
        occurrence: &ExternalAllocationOccurrence,
        activation: &ExternalSupervisorActivationIntent,
        contract: &crate::node_config::sections::external_execution::ExternalPlacementBackendContract,
    ) -> Result<()> {
        contract.controller_transport.relay()?;
        let owner_matches_workload = matches!(
            (&reservation.owner, &contract.workload),
            (
                ExternalAllocationOwner::QualificationCandidate(_),
                crate::node_config::sections::external_execution::ExternalWorkloadBinding::QualificationStructuredSession(_)
            ) | (
                ExternalAllocationOwner::DedicatedSession(_),
                crate::node_config::sections::external_execution::ExternalWorkloadBinding::StructuredSession(_)
            )
        );
        ensure!(
            owner_matches_workload
                && self.schema == 1
                && self.placement_thread_id == reservation.placement_thread_id
                && self.allocation_request_digest == reservation.request_digest
                && self.occurrence_id == occurrence.occurrence_id
                && self.activation_request_digest == activation.activation_request_digest
                && self.guest_input_identity == activation.guest_input_identity,
            "relay bootstrap commitment changed its original workload activation"
        );
        validate_sha256("original relay bootstrap", &self.bootstrap_sha256)?;
        validate_sha256("original signed guest import", &self.signed_import_sha256)
    }
}

pub(super) fn read_commitment(
    conn: &Connection,
    reservation: &ExternalAllocationReservation,
    occurrence: &ExternalAllocationOccurrence,
    activation: &ExternalSupervisorActivationIntent,
    contract: &crate::node_config::sections::external_execution::ExternalPlacementBackendContract,
) -> Result<Option<ExternalRelayBootstrapCommitment>> {
    let commitment: Option<ExternalRelayBootstrapCommitment> = read_canonical_evidence(
        conn,
        "external_execution_relay_bootstrap",
        "commitment_json",
        &reservation.placement_thread_id,
    )?;
    match (&contract.controller_transport, &commitment) {
        (
            ryeos_state::external_execution::transport::ExternalControllerTransportSelection::ControllerFileRelay(_),
            Some(commitment),
        ) => commitment.validate_for(reservation, occurrence, activation, contract)?,
        (
            ryeos_state::external_execution::transport::ExternalControllerTransportSelection::HttpJson(_),
            None,
        ) => {}
        _ => bail!("external activation changed its original relay bootstrap custody"),
    }
    Ok(commitment)
}

pub(super) fn validate_no_orphans(conn: &Connection) -> Result<()> {
    let orphan: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM external_execution_relay_bootstrap r
         LEFT JOIN external_execution_allocation a
           ON a.placement_thread_id=r.placement_thread_id
         LEFT JOIN external_execution_supervisor_activation_intent i
           ON i.placement_thread_id=r.placement_thread_id
         WHERE a.placement_thread_id IS NULL OR i.placement_thread_id IS NULL)",
        [],
        |row| row.get(0),
    )?;
    ensure!(!orphan, "original relay bootstrap commitment is orphaned");
    Ok(())
}
