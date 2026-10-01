//! Immutable CAS root for one structurally complete local Q Worker transcript.
//!
//! This row retains evidence after the local process dies. It is not a
//! semantic verdict or a qualification permit. In particular, pushed batch
//! receipt cannot prove that a batch was emitted after the framed Request.

use super::*;
use anyhow::ensure;
use ryeos_external_execution_contract::ProviderConfigurationLaunchInput;
use ryeos_state::objects::QualificationWorkerTranscriptManifest;

use super::qualification_batch::{QualificationWorkerObservationBatchRecord, read_batches};
use super::qualification_command::{
    QualificationWorkerCommandPhase, QualificationWorkerCommandRecord, read_command, require_owner,
};
use super::qualification_process::require_terminal_qualification_worker_process;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct QualificationWorkerTranscriptSource {
    pub owner_principal: String,
    pub provider_configuration: ProviderConfigurationLaunchInput,
    pub command: QualificationWorkerCommandRecord,
    pub batches: Vec<QualificationWorkerObservationBatchRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QualificationWorkerTranscriptRecord {
    pub placement_thread_id: String,
    pub owner_principal: String,
    pub manifest_hash: String,
    pub retained_at_ms: i64,
}

fn source(conn: &Connection, placement: &str) -> Result<QualificationWorkerTranscriptSource> {
    let allocation = read(conn, placement)?.context("Q transcript lost its allocation")?;
    let ExternalAllocationOwner::QualificationCandidate(owner) = &allocation.reservation.owner
    else {
        bail!("Q transcript has no retained qualification owner");
    };
    let command = read_command(conn, placement)?.context("Q transcript has no retained command")?;
    require_owner(conn, &command)?;
    ensure!(
        command.phase == QualificationWorkerCommandPhase::TurnCompleted
            && command.owner_send_settled_at_ms.is_some()
            && command.setup_owner_send_settled_at_ms.is_some()
            && command.setup_final_response.is_some()
            && command.final_response.is_some(),
        "Q transcript has no successful owner send and completed turn"
    );
    let process = require_terminal_qualification_worker_process(conn, &allocation.reservation)?;
    ensure!(
        process.process_identity.owner_incarnation_digest()? == command.process_incarnation_digest
            && process.channel_binding_digest == command.channel_binding_digest,
        "Q transcript changed its death-settled process"
    );
    let batches = read_batches(conn, placement)?;
    Ok(QualificationWorkerTranscriptSource {
        owner_principal: owner.operator_principal.clone(),
        provider_configuration: process.provider_configuration.clone(),
        command,
        batches,
    })
}

fn require_manifest_source(
    manifest: &QualificationWorkerTranscriptManifest,
    source: &QualificationWorkerTranscriptSource,
) -> Result<()> {
    manifest.validate()?;
    let command = &source.command;
    let batches = &source.batches;
    let final_body = command
        .final_response
        .as_ref()
        .context("Q transcript lost Final")?;
    let final_bytes = lillux::canonical_json(final_body)?;
    let delta_bytes = command
        .delta_response
        .as_ref()
        .map(lillux::canonical_json)
        .transpose()?;
    let mut batch_bytes = 0_u64;
    for batch in batches {
        batch_bytes = batch_bytes
            .checked_add(u64::try_from(lillux::canonical_json(&batch.batch)?.len())?)
            .context("Q transcript batch byte count overflow")?;
    }
    ensure!(
        manifest.owner_principal == source.owner_principal
            && manifest.provider_configuration == source.provider_configuration
            && manifest.placement_thread_id == command.placement_thread_id
            && manifest.accepted_root_id == command.accepted_root_id
            && manifest.accepted_capsule_hash == command.accepted_capsule_hash
            && manifest.qualification_operation_id == command.qualification_operation_id
            && manifest.qualification_occurrence_id == command.qualification_occurrence_id
            && manifest.channel_binding_digest == command.channel_binding_digest
            && manifest.process_incarnation_digest == command.process_incarnation_digest
            && manifest.setup_request_digest == command.setup_request_digest
            && Some(manifest.setup_final_response_digest.as_str())
                == command.setup_final_response_digest.as_deref()
            && Some(manifest.setup_owner_send_settled_at_ms)
                == command.setup_owner_send_settled_at_ms
            && Some(manifest.wire_max_bytes) == command.setup_final_wire_max_bytes
            && manifest.request_digest == command.request_digest
            && Some(manifest.owner_send_settled_at_ms) == command.owner_send_settled_at_ms
            && Some(manifest.wire_max_bytes) == command.final_wire_max_bytes
            && Some(manifest.turn_id.as_str()) == command.turn_id.as_deref()
            && Some(manifest.turn_start_digest.as_str()) == command.turn_start_digest.as_deref()
            && Some(manifest.turn_completion_digest.as_str())
                == command.turn_completion_digest.as_deref()
            && manifest.delta_digest == command.delta_response_digest
            && manifest.delta_response_bytes
                == delta_bytes
                    .as_ref()
                    .map(|bytes| u64::try_from(bytes.len()))
                    .transpose()?
            && manifest.delta_wire_max_bytes == command.delta_wire_max_bytes
            && Some(manifest.final_response_digest.as_str())
                == command.final_response_digest.as_deref()
            && manifest.final_response_bytes == u64::try_from(final_bytes.len())?
            && manifest.batch_count == u64::try_from(batches.len())?
            && manifest.through_sequence
                == batches.last().map_or(0, |batch| batch.through_sequence)
            && manifest.last_batch_digest == batches.last().map(|batch| batch.batch_digest.clone())
            && manifest.batch_canonical_bytes == batch_bytes,
        "Q transcript manifest differs from its retained owner or source"
    );
    Ok(())
}

pub(super) fn read_record(
    conn: &Connection,
    placement: &str,
) -> Result<Option<QualificationWorkerTranscriptRecord>> {
    conn.query_row(
        "SELECT owner_principal,manifest_hash,manifest_json,retained_at_ms
         FROM external_qualification_worker_transcript WHERE placement_thread_id=?1",
        [placement],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ))
        },
    )
    .optional()?
    .map(|(owner_principal, manifest_hash, json, retained_at_ms)| {
        ensure!(
            retained_at_ms > 0,
            "Q transcript has invalid retention time"
        );
        validate_sha256("Q transcript manifest", &manifest_hash)?;
        let value: serde_json::Value = serde_json::from_str(&json)?;
        ensure!(
            json == lillux::canonical_json(&value)?
                && ryeos_state::objects::canonical_value_digest(&value)? == manifest_hash,
            "Q transcript row changed its canonical CAS manifest"
        );
        let manifest = QualificationWorkerTranscriptManifest::from_current_value(&value)?;
        ensure!(
            manifest.placement_thread_id == placement
                && manifest.owner_principal == owner_principal,
            "Q transcript row changed its owner"
        );
        let current = source(conn, placement)?;
        require_manifest_source(&manifest, &current)?;
        Ok(QualificationWorkerTranscriptRecord {
            placement_thread_id: placement.to_owned(),
            owner_principal,
            manifest_hash,
            retained_at_ms,
        })
    })
    .transpose()
}

pub(super) fn validate_transcripts(conn: &Connection) -> Result<()> {
    let mut statement = conn.prepare(
        "SELECT placement_thread_id FROM external_qualification_worker_transcript ORDER BY placement_thread_id"
    )?;
    let placements = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for placement in placements {
        read_record(conn, &placement)?.context("Q transcript disappeared")?;
    }
    Ok(())
}

impl RuntimeDb {
    pub(crate) fn qualification_worker_transcript_source(
        &self,
        placement: &str,
    ) -> Result<QualificationWorkerTranscriptSource> {
        source(&self.conn, placement)
    }

    /// CAS chunks and manifest must already be staged under the same StateStore
    /// write permit. This transaction rechecks the entire frozen source.
    pub(crate) fn bind_qualification_worker_transcript(
        &self,
        expected: &QualificationWorkerTranscriptSource,
        manifest: &QualificationWorkerTranscriptManifest,
        manifest_hash: &str,
    ) -> Result<QualificationWorkerTranscriptRecord> {
        let value = manifest.to_value()?;
        let json = lillux::canonical_json(&value)?;
        ensure!(
            ryeos_state::objects::canonical_value_digest(&value)? == manifest_hash,
            "Q transcript staged manifest hash differs"
        );
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let current = source(&tx, &expected.command.placement_thread_id)?;
        ensure!(
            current == *expected,
            "Q transcript source changed during CAS staging"
        );
        require_manifest_source(manifest, &current)?;
        if let Some(existing) = read_record(&tx, &expected.command.placement_thread_id)? {
            ensure!(
                existing.manifest_hash == manifest_hash,
                "Q transcript replay changed its immutable manifest"
            );
            tx.commit()?;
            return Ok(existing);
        }
        let now = i64::try_from(lillux::time::timestamp_millis())?;
        ensure!(
            tx.execute(
                "INSERT INTO external_qualification_worker_transcript
                 (placement_thread_id,owner_principal,manifest_hash,manifest_json,retained_at_ms)
                 VALUES(?1,?2,?3,?4,?5)",
                params![
                    &expected.command.placement_thread_id,
                    &expected.owner_principal,
                    manifest_hash,
                    json,
                    now
                ],
            )? == 1,
            "Q transcript did not bind its immutable CAS root"
        );
        let record = read_record(&tx, &expected.command.placement_thread_id)?
            .context("Q transcript bind disappeared")?;
        tx.commit()?;
        Ok(record)
    }
}
